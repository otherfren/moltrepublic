// SPDX-License-Identifier: GPL-3.0-or-later

//! N4a (`docs_archive/transport/nostr_n4_plan.md` §5): the engine-facing **Nostr
//! ritual facade** — the one place the founding/join ritual touches relays.
//! The engine stays free of `nostr` crate types: everything on this surface
//! speaks hex anchors, [`RitualMsg`], [`WelcomePayload`] and raw bytes.
//!
//! Two carriers, matching the §2 wire mapping:
//!
//! * [`RitualNet`] — one party's gift-wrap endpoint: kind-446 ritual rumors
//!   and the kind-444 Welcome ride kind-1059 wraps addressed by anchor
//!   (`#p`); [`RitualInbox`] peels them fail-closed and returns the PROVEN
//!   sender (NIP-59 verifies seal author == rumor author — the §2.1
//!   proof-of-possession the roster's third anchor gains at join time).
//! * [`GroupChannel`] — the ritual-time kind-445 group channel: frames
//!   sealed under the epoch's exporter secret ([`envelope::seal_outer`]),
//!   published under the rotating `#h` tag by a FRESH ephemeral key per
//!   event, so a relay sees anonymous ciphertext belonging to a group id.
//!
//! Publish paths build a fresh [`RelayRuntime`] per send (publish opens
//! per-relay connections anyway); the inbox and the group subscription hold
//! a live [`Subscription`]. Delivery is at-least-once end to end (the N2
//! dedup ring is a bounded courtesy, and a window-roll resubscribe replays)
//! — every caller must be idempotent.

use std::time::Duration;

use nostr::{
    Alphabet, EventBuilder, Filter, Keys, Kind, PublicKey, SingleLetterTag, Tag, Timestamp,
};

use crate::dial::Dialer;
use crate::envelope::{self, H_WINDOW};
use crate::invite::RitualMsg;
use crate::relay_runtime::{PublishReport, RelayRuntime, Subscription, SyncState};
use crate::ritual_wrap::{self, RitualWrapError};
use crate::welcome::{self, WelcomeError, WelcomePayload};
use crate::NetError;

// The kinds this module publishes under (gift wrap 1059, group 445) now live
// in `crate::kinds` — one registry, so two work packages cannot allocate the
// same number in parallel.

/// The §4.4 clock-skew margin around a UTC h-window boundary: within this
/// distance of a boundary the subscription also covers the adjacent
/// window's tag, so a peer whose clock is up to an hour off never publishes
/// past our filter.
const SKEW_MARGIN: u64 = 3_600;

/// How long one inner wait on the group subscription may run before the
/// window-roll check gets another look — bounds how late a UTC-day roll is
/// noticed while a caller sits in a long [`GroupSub::recv`] budget.
const ROLL_POLL: Duration = Duration::from_secs(1);

/// Mint a fresh 32-byte h-tag rotation seed from the OS CSPRNG (the group
/// secret behind [`envelope::h_tag`], set once at founding and delivered
/// only inside the authenticated Welcome).
pub fn mint_rotation_seed() -> Result<[u8; 32], NetError> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
    Ok(seed)
}

/// The `h` tags the group subscription must cover at `now_secs`: the
/// CURRENT window's tag first, plus the adjacent window's when within
/// [`SKEW_MARGIN`] of a UTC boundary (the next window's shortly before
/// midnight, the previous one's shortly after). Pure — time is an argument,
/// so the margin logic is testable without a clock.
pub fn window_tags(rotation_seed: &[u8; 32], now_secs: u64) -> Vec<String> {
    let start = now_secs - (now_secs % H_WINDOW);
    let mut tags = vec![envelope::h_tag(rotation_seed, now_secs)];
    // just after a boundary: a peer whose clock still reads yesterday
    // publishes under the PREVIOUS window's tag
    if now_secs - start < SKEW_MARGIN {
        if let Some(prev) = start.checked_sub(H_WINDOW) {
            tags.push(envelope::h_tag(rotation_seed, prev));
        }
    }
    // just before a boundary: a fast peer already stamps into the NEXT
    let next = start.saturating_add(H_WINDOW);
    if next.saturating_sub(now_secs) <= SKEW_MARGIN {
        tags.push(envelope::h_tag(rotation_seed, next));
    }
    tags
}

/// Map a wrap-side ritual error onto the transport error vocabulary: size
/// violations are framing (a local bug or an over-fat founding, named
/// loudly), everything else is crypto.
fn ritual_wrap_err(e: RitualWrapError) -> NetError {
    match e {
        RitualWrapError::TooLarge { .. } | RitualWrapError::Payload(_) => {
            NetError::Framing(e.to_string())
        }
        other => NetError::Crypto(other.to_string()),
    }
}

/// The Welcome twin of [`ritual_wrap_err`].
fn welcome_wrap_err(e: WelcomeError) -> NetError {
    match e {
        WelcomeError::TooLarge { .. } | WelcomeError::Payload(_) => {
            NetError::Framing(e.to_string())
        }
        other => NetError::Crypto(other.to_string()),
    }
}

/// Parse a wire anchor into the recipient key — normalize-or-reject via
/// [`crate::nostr::canonical_nostr_pk`], the same gate every roster ingest
/// runs, so an endpoint can never address a non-canonical form.
fn recipient(to_pk_hex: &str) -> Result<PublicKey, NetError> {
    let canonical = crate::nostr::canonical_nostr_pk(to_pk_hex)?;
    PublicKey::from_hex(&canonical)
        .map_err(|e| NetError::Crypto(format!("recipient anchor: {e}")))
}

/// One party's ritual-side Nostr endpoint: its ticket-salted transport keys
/// plus the invite-relay list. Cloning shares nothing live — publishes open
/// fresh connections, and the inbox is minted per [`RitualNet::inbox`] call.
#[derive(Clone)]
pub struct RitualNet {
    dialer: Dialer,
    relays: Vec<String>,
    keys: Keys,
    pk_hex: String,
}

// manual: the keys hold a SECRET scalar — never in Debug output (the
// relay_runtime precedent)
impl std::fmt::Debug for RitualNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RitualNet")
            .field("relays", &self.relays)
            .field("pk", &self.pk_hex)
            .finish_non_exhaustive()
    }
}

impl RitualNet {
    /// An endpoint over `relays` speaking as the holder of `sk` (the
    /// 32-byte secp256k1 scalar — the ticket-salted transport secret).
    /// Fail-closed: an invalid scalar, or keys whose public half disagrees
    /// with the anchor derivation ([`crate::nostr::nostr_pk_for_sk`]), is
    /// refused here rather than surfacing as undeliverable wraps later.
    pub fn new(dialer: Dialer, relays: Vec<String>, sk: &[u8]) -> Result<Self, NetError> {
        let anchored = crate::nostr::nostr_pk_for_sk(sk)?;
        let secret = nostr::SecretKey::from_slice(sk)
            .map_err(|e| NetError::Crypto(format!("not a valid secp256k1 scalar: {e}")))?;
        let keys = Keys::new(secret);
        let pk_hex = keys.public_key().to_hex();
        if pk_hex != anchored {
            return Err(NetError::Crypto(
                "transport key disagrees with its anchor derivation".into(),
            ));
        }
        // the anchor's own circuits: what this endpoint does under its
        // anchor (the `#p` inbox, its ritual wraps) must not share a
        // circuit with the group's anonymous traffic or another republic
        let dialer = dialer.isolated(&format!("anchor:{pk_hex}"));
        Ok(Self { dialer, relays, keys, pk_hex })
    }

    /// Our canonical x-only hex anchor (the form in the signed roster bytes).
    pub fn pk_hex(&self) -> String {
        self.pk_hex.clone()
    }

    /// The invite-relay list this endpoint publishes to and subscribes on.
    pub fn relays(&self) -> &[String] {
        &self.relays
    }

    /// Publish one signed event with ≥1-OK semantics over a fresh runtime,
    /// returning the PER-RELAY outcome.
    ///
    /// This runtime stays UNAUTHENTICATED on purpose (§7.5): an authenticated
    /// publish channel would link every ephemeral-key event we send to the
    /// member behind it. `publish_one` refuses an `auth-required:` OK loudly
    /// rather than quietly authenticating.
    ///
    /// The report used to be discarded here (`.map(|_report| ())`), which made
    /// "landed on 1 of 5 relays" indistinguishable from full delivery — the
    /// per-relay outcomes N2 built were thrown away one layer above where they
    /// were computed. A ritual leg that lands on a single relay is not a
    /// failure, but it is not a success the operator should be left to guess
    /// at either.
    async fn publish(&self, event: &nostr::Event) -> Result<PublishReport, NetError> {
        RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .publish(event)
            .await
    }

    /// Gift-wrap a [`RitualMsg`] (kind-446 rumor) to `to_pk_hex` and publish
    /// it — success once ≥1 relay accepted the wrap.
    pub async fn send_ritual(
        &self,
        to_pk_hex: &str,
        msg: &RitualMsg,
    ) -> Result<PublishReport, NetError> {
        let to = recipient(to_pk_hex)?;
        let wrap = ritual_wrap::wrap_ritual(&self.keys, &to, msg)
            .await
            .map_err(ritual_wrap_err)?;
        self.publish(&wrap).await
    }

    /// Gift-wrap the kind-444 Welcome payload v2 to `to_pk_hex` and publish
    /// it — same ≥1-OK semantics, same fail-loud size gate as the wrap
    /// helper (a too-big founding refuses with a real error).
    pub async fn send_welcome(
        &self,
        to_pk_hex: &str,
        payload: &WelcomePayload,
    ) -> Result<PublishReport, NetError> {
        let to = recipient(to_pk_hex)?;
        let wrap = welcome::wrap_welcome(&self.keys, &to, payload)
            .await
            .map_err(welcome_wrap_err)?;
        self.publish(&wrap).await
    }

    /// Subscribe our kind-1059 inbox (`#p` = our anchor) across the relays.
    /// Succeeds once ≥1 relay accepted the REQ; [`RitualInbox::live`] gates
    /// on the full replay.
    pub async fn inbox(&self) -> Result<RitualInbox, NetError> {
        let filter = Filter::new()
            .kind(Kind::Custom(crate::kinds::KIND_GIFT_WRAP))
            .pubkey(self.keys.public_key());
        // NIP-42 with OUR ANCHOR here, deliberately: the filter is
        // `#p = our anchor`, so the relay already learns this key from the
        // REQ itself — authenticating with the same key discloses nothing the
        // subscription did not. Without it an auth-required relay keeps a
        // live, silent connection and the whole ritual times out with no
        // error anywhere.
        let sub = RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .with_auth_keys(Some(self.keys.clone()))
            .subscribe(filter)
            .await?;
        Ok(RitualInbox { sub, keys: self.keys.clone() })
    }
}

/// First delay before a failed window-roll re-placement is retried.
const RETRY_INITIAL: Duration = Duration::from_secs(1);
/// Ceiling for that backoff — loud forever, but never a spin.
const RETRY_CAP: Duration = Duration::from_secs(30);

/// TEST SEAM — a process-global shift on the window clock, in seconds.
///
/// Zero in every shipping run. Deliberately NOT feature-gated: molt-net's own
/// integration tests cannot enable a feature on their own crate, and a
/// midnight window roll is otherwise untestable without waiting for midnight.
/// It is process-global, so any test using it needs its OWN test binary.
static WINDOW_CLOCK_SHIFT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Move the window clock for tests. See [`WINDOW_CLOCK_SHIFT`].
#[doc(hidden)]
pub fn shift_window_clock_for_tests(secs: i64) {
    WINDOW_CLOCK_SHIFT.store(secs, std::sync::atomic::Ordering::SeqCst);
}

/// Wall-clock seconds as the h-window logic sees them.
pub(crate) fn now_secs() -> u64 {
    Timestamp::now()
        .as_secs()
        .saturating_add_signed(WINDOW_CLOCK_SHIFT.load(std::sync::atomic::Ordering::SeqCst))
}

/// What [`GroupSub::recv`] observed.
///
/// `Idle` and `Deaf` used to be the same `None`, which every caller read as
/// "nothing arrived" — so a node whose window-roll resubscribe failed went
/// PERMANENTLY DEAF at a UTC boundary while looking perfectly healthy, and
/// spun without backoff doing it. Advisory, never terminal: a `Deaf` and a
/// delivered `Frame` can interleave while the stale subscription still
/// carries legitimate traffic inside the skew margin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRecv {
    /// A valid 445 frame under one of our current tags.
    Frame {
        /// The sealed outer content.
        content: String,
        /// The carrier stamp both ends must agree on.
        created_at: u64,
    },
    /// Nothing arrived within the budget — the honest quiet.
    Idle,
    /// The subscription could not be re-placed for the new window; the
    /// channel is not being heard. Retried on a backoff.
    Deaf(String),
}

/// The live 1059 inbox of one [`RitualNet`] endpoint: peels arriving wraps
/// and yields typed deliveries with their proven senders.
pub struct RitualInbox {
    sub: Subscription,
    keys: Keys,
}

// manual: the keys hold a SECRET scalar — never in Debug output
impl std::fmt::Debug for RitualInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RitualInbox").finish_non_exhaustive()
    }
}

impl RitualInbox {
    /// Best-effort "the REQ replayed everywhere" gate — `true` once every
    /// relay connected at subscribe time sent EOSE, `false` on timeout
    /// (advisory: subscribe-before-advertise leans on it, correctness does
    /// not).
    pub async fn live(&mut self, timeout: Duration) -> bool {
        self.sub.synced(timeout).await
    }

    /// The replay counts behind [`Self::live`] — so a caller can tell "no
    /// relay is readable" (a provisioning failure) from "one of three lagged"
    /// (a warning). Proceeding blind on the first is how a ritual times out
    /// with no error anywhere.
    pub async fn live_state(&mut self, timeout: Duration) -> SyncState {
        self.sub.sync_state(timeout).await
    }

    /// The next peeled delivery, or `None` when nothing arrives within
    /// `timeout`. Wraps that do not open for us, carry the wrong inner
    /// kind, or fail their payload parse are SKIPPED (traced at debug) and
    /// the wait continues within the same overall budget — foreign or
    /// corrupt relay traffic must never kill the ritual loop.
    pub async fn recv(&mut self, timeout: Duration) -> Option<RitualDelivery> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining =
                deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let event = self.sub.recv(remaining).await?;
            match ritual_wrap::peel_ritual(&self.keys, &event).await {
                Ok((msg, sender)) => {
                    return Some(RitualDelivery::Msg(msg, sender.to_hex()));
                }
                // a 444 rumor is the Welcome — peel it with its own
                // fail-closed chain (the second decrypt is one Welcome per
                // seat per founding, not a hot path)
                Err(RitualWrapError::NotARitual { kind })
                    if kind == welcome::KIND_WELCOME =>
                {
                    match welcome::peel_welcome(&self.keys, &event).await {
                        Ok((payload, sender)) => {
                            return Some(RitualDelivery::Welcome(payload, sender.to_hex()));
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "skipping an unreadable welcome wrap");
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "skipping a wrap that is not ours");
                }
            }
        }
    }
}

/// One peeled inbox delivery. The sender string is the canonical x-only hex
/// of the NIP-59-verified seal author — PROVEN possession of that nostr
/// key, never an asserted field.
#[derive(Debug)]
pub enum RitualDelivery {
    /// A kind-446 ritual message and its proven sender.
    Msg(RitualMsg, String),
    /// A kind-444 Welcome payload and its proven sender.
    Welcome(WelcomePayload, String),
}

/// The ritual-time kind-445 group channel: outer-sealed frames under the
/// rotating `#h` tag derived from the group's rotation seed. Cloning is
/// cheap and shares nothing live.
#[derive(Clone)]
pub struct GroupChannel {
    dialer: Dialer,
    relays: Vec<String>,
    rotation_seed: [u8; 32],
    /// The persistent publish connections (incident 2026-08-09 §3): shared
    /// by every clone of this channel — outbox, ack task and file plane
    /// ride the same kept sockets instead of dialing per frame.
    pool: crate::relay_runtime::PublishPool,
}

// manual: the rotation seed is secret-class (plan §6) — never in Debug
impl std::fmt::Debug for GroupChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupChannel")
            .field("relays", &self.relays)
            .finish_non_exhaustive()
    }
}

impl GroupChannel {
    /// A channel over `relays` for the group whose h tags derive from
    /// `rotation_seed` (minted at founding, learned from the Welcome).
    pub fn new(dialer: Dialer, relays: Vec<String>, rotation_seed: [u8; 32]) -> Self {
        // two lanes per republic: the h-tag subscriptions (throwaway auth
        // key) on the group's lane, the ephemeral-key publishes on a lane of
        // their own per channel instance — a relay must not see the
        // subscriber and the publisher on one circuit (`Dialer::isolated`)
        let publish_lane = crate::dial::session_token().unwrap_or_default();
        let pool = crate::relay_runtime::PublishPool::new(
            dialer.isolated(&format!("publish:{publish_lane}")),
            relays.clone(),
        );
        let sub_lane = {
            use sha2::{Digest, Sha256};
            hex::encode(&Sha256::digest(rotation_seed)[..8])
        };
        let dialer = dialer.isolated(&format!("group:{sub_lane}"));
        Self { dialer, relays, rotation_seed, pool }
    }

    /// Seal `mls_ciphertext` under `exporter` and publish it as a kind-445
    /// event: EXACTLY one `h` tag (the current window's) and no other,
    /// authored by a FRESH ephemeral key so a relay can link the event to
    /// nothing but the group id. Returns the event's `created_at` (unix
    /// secs) — the carrier stamp both ends must agree on, so it is stamped
    /// explicitly and read back from the signed event, never re-derived.
    pub async fn publish_frame(
        &self,
        exporter: &[u8; 32],
        mls_ciphertext: &[u8],
    ) -> Result<(u64, PublishReport), NetError> {
        self.publish_frame_at(exporter, mls_ciphertext, now_secs()).await
    }

    /// [`Self::publish_frame`] at a **caller-chosen** `created_at`.
    ///
    /// A commit's sender has to key it at the value every receiver will read
    /// off the wire — `CommitKey(created_at, digest)`, and the rule is that
    /// the stamp comes from the same source on both sides
    /// (`molt-net/CLAUDE.md`). Choosing it afterwards is too late: the MLS
    /// commit is already made. So the coordinator picks the stamp, commits at
    /// it, and publishes at exactly it.
    ///
    /// The supplied value drives the **h tag** as well, for the same reason
    /// the generated one does: a tag and a stamp from different clocks can
    /// straddle a UTC window boundary, and the frame then sits under a tag
    /// that disowns its own timestamp — invisible to everyone asking for the
    /// window it claims to be in.
    pub async fn publish_frame_at(
        &self,
        exporter: &[u8; 32],
        mls_ciphertext: &[u8],
        created_at: u64,
    ) -> Result<(u64, PublishReport), NetError> {
        self.publish_kind_at(crate::kinds::KIND_GROUP, exporter, mls_ciphertext, created_at)
            .await
    }

    /// A FILE CHUNK (kind 447, `file_plane.rs`): sealed and tagged exactly
    /// like a 445 frame, at the series' one shared stamp so every chunk of
    /// a series sits under one window's tag.
    pub async fn publish_file_chunk_at(
        &self,
        exporter: &[u8; 32],
        chunk: &[u8],
        created_at: u64,
    ) -> Result<(u64, PublishReport), NetError> {
        self.publish_kind_at(crate::kinds::KIND_FILE_CHUNK, exporter, chunk, created_at)
            .await
    }

    /// A kind-447 content sealed elsewhere (a holder's stored piece, series
    /// v2): published as is, under `created_at`'s window tag.
    pub async fn publish_file_content_at(
        &self,
        content: &str,
        created_at: u64,
    ) -> Result<(u64, PublishReport), NetError> {
        self.publish_sealed_at(crate::kinds::KIND_FILE_CHUNK, content.to_string(), created_at)
            .await
    }

    async fn publish_kind_at(
        &self,
        kind: u16,
        exporter: &[u8; 32],
        plaintext: &[u8],
        created_at: u64,
    ) -> Result<(u64, PublishReport), NetError> {
        let sealed = envelope::seal_outer(exporter, plaintext)
            .map_err(|e| NetError::Crypto(format!("sealing the {kind} frame: {e}")))?;
        self.publish_sealed_at(kind, sealed, created_at).await
    }

    async fn publish_sealed_at(
        &self,
        kind: u16,
        sealed: String,
        created_at: u64,
    ) -> Result<(u64, PublishReport), NetError> {
        // one value for tag and stamp — see the doc above
        let now = Timestamp::from_secs(created_at);
        let tag = envelope::h_tag(&self.rotation_seed, now.as_secs());
        let h = Tag::parse(["h", tag.as_str()])
            .map_err(|e| NetError::Framing(format!("h tag: {e}")))?;
        let event = EventBuilder::new(Kind::Custom(kind), sealed)
            .tag(h)
            .custom_created_at(now)
            .sign_with_keys(&Keys::generate())
            .map_err(|e| NetError::Crypto(format!("signing the {kind} frame: {e}")))?;
        let stamp = event.created_at.as_secs();
        let report = self.pool.publish(&event).await?;
        Ok((stamp, report))
    }

    /// Subscribe kind-445 under the tags [`window_tags`] names right now —
    /// the current window's, plus the adjacent one's inside the §4.4 skew
    /// margin (ONE filter, both `#h` values).
    pub async fn subscribe(&self) -> Result<GroupSub, NetError> {
        let tags = window_tags(&self.rotation_seed, now_secs());
        let sub = self.subscribe_tags(&tags).await?;
        Ok(GroupSub {
            sub,
            tags,
            channel: self.clone(),
            deaf: None,
            retry_at: tokio::time::Instant::now(),
            retry_backoff: RETRY_INITIAL,
        })
    }

    /// Subscribe the windows a returning member slept through — every `h` tag
    /// from `since_secs` ago through now, oldest first, capped to the newest
    /// `max_windows`.
    ///
    /// This exists because [`Self::subscribe`] names only the CURRENT window's
    /// tag (plus one adjacent inside the skew margin), and an `h` tag is
    /// `SHA256(seed ‖ le64(unix / H_WINDOW))`. A frame from three days ago
    /// therefore sits under a tag the live subscription never asks for — it is
    /// unreachable **because we do not ask**, not because the relay pruned it.
    /// The 445 filter carries no `since`/`until`/`limit`, so naming the right
    /// tags IS the history query.
    ///
    /// Returns its own type, not a [`GroupSub`]: that one re-places the
    /// subscription with exactly the current window's tags whenever they are
    /// not covered, which would discard the whole catch-up range on the first
    /// `recv` — immediately, if placed near a UTC boundary.
    pub async fn subscribe_since(
        &self,
        since_secs: u64,
        max_windows: usize,
    ) -> Result<CatchupSub, NetError> {
        let now = now_secs();
        let tags = envelope::h_tags_for_catchup(
            &self.rotation_seed,
            now.saturating_sub(since_secs),
            now,
            max_windows,
        );
        let sub = self.subscribe_tags(&tags).await?;
        Ok(CatchupSub { sub, tags })
    }

    /// The file chunks of a series published at `at_secs`: a fixed-tag
    /// catch-up subscription (kind 447) over that stamp's window tags —
    /// [`window_tags`] includes the skew-adjacent one, so a series
    /// straddling a UTC boundary stays reachable.
    pub async fn subscribe_files_at(&self, at_secs: u64) -> Result<CatchupSub, NetError> {
        let tags = window_tags(&self.rotation_seed, at_secs);
        let sub = self
            .subscribe_tags_kind(crate::kinds::KIND_FILE_CHUNK, &tags, None)
            .await?;
        Ok(CatchupSub { sub, tags })
    }

    /// The file pieces of a series that STARTED at `start_secs` and may
    /// still be going (series v2, `docs/files/mirroring.md` §3.1): every
    /// window from the start's through the current one (newest
    /// `max_windows` kept) plus the skew-adjacent ones - see
    /// [`file_catchup_tags`]. `history_bound` sizes the relay's stored-event
    /// replay to the series (the default bound cuts a large one short).
    pub async fn subscribe_files_from(
        &self,
        start_secs: u64,
        max_windows: usize,
        history_bound: Option<usize>,
    ) -> Result<CatchupSub, NetError> {
        let tags = file_catchup_tags(&self.rotation_seed, start_secs, now_secs(), max_windows);
        let sub = self
            .subscribe_tags_kind(crate::kinds::KIND_FILE_CHUNK, &tags, history_bound)
            .await?;
        Ok(CatchupSub { sub, tags })
    }

    /// Place one pooled 445 subscription over exactly `tags`.
    async fn subscribe_tags(&self, tags: &[String]) -> Result<Subscription, NetError> {
        self.subscribe_tags_kind(crate::kinds::KIND_GROUP, tags, None).await
    }

    /// [`Self::subscribe_tags`] for a caller-chosen kind (445 group frames,
    /// 447 file chunks — same anonymous h-tag filter shape).
    async fn subscribe_tags_kind(
        &self,
        kind: u16,
        tags: &[String],
        history_bound: Option<usize>,
    ) -> Result<Subscription, NetError> {
        let filter = Filter::new()
            .kind(Kind::Custom(kind))
            .custom_tags(SingleLetterTag::lowercase(Alphabet::H), tags.iter().cloned());
        // A FRESH ephemeral key per placement (and per window-roll
        // re-placement) — NOT the roster anchor. The 445 filter names only an
        // h tag, so it is anonymous; authenticating it with the anchor would
        // hand every relay operator the anchor→group-id link for the life of
        // the republic, and that link would survive into the N5 runtime
        // subscriptions. The cost is a relay that WHITELISTS known pubkeys
        // refusing us — which fails loudly and visibly, unlike a silent,
        // permanent deanonymization.
        let mut runtime = RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .with_auth_keys(Some(Keys::generate()));
        if let Some(bound) = history_bound {
            runtime = runtime.with_history_bound(bound);
        }
        runtime.subscribe(filter).await
    }
}

/// The `h` tags a series fetch subscribes: every window between the
/// series start and now, in EITHER order - a sharer's clock ahead across a
/// UTC day boundary stamps into the fetcher's next window - plus the
/// skew-adjacent window at both ends ([`window_tags`]), like v1's fetch.
/// Newest `max_windows` of the range kept.
pub fn file_catchup_tags(
    rotation_seed: &[u8; 32],
    start_secs: u64,
    now_secs: u64,
    max_windows: usize,
) -> Vec<String> {
    let (lo, hi) = (start_secs.min(now_secs), start_secs.max(now_secs));
    let mut tags = envelope::h_tags_for_catchup(rotation_seed, lo, hi, max_windows);
    for extra in window_tags(rotation_seed, start_secs)
        .into_iter()
        .chain(window_tags(rotation_seed, now_secs))
    {
        if !tags.contains(&extra) {
            tags.push(extra);
        }
    }
    tags
}

/// A catch-up subscription over a FIXED set of past `h` windows.
///
/// Deliberately not a [`GroupSub`]: it must never re-place itself under the
/// current window's tags, because that is precisely what would throw away the
/// range it was opened for. It also holds no channel — there is nothing to
/// re-place, so nothing to hold it for. Drop it when the replay is done
/// ([`Self::live`] is the completion signal the relay gives us).
pub struct CatchupSub {
    sub: Subscription,
    /// The past windows this subscription names; fixed for its lifetime.
    tags: Vec<String>,
}

impl std::fmt::Debug for CatchupSub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatchupSub")
            .field("windows", &self.tags.len())
            .finish_non_exhaustive()
    }
}

impl CatchupSub {
    /// Whether at least one relay has finished replaying (EOSE) — "the
    /// history we asked for has been served", as far as a relay can say it.
    pub async fn live(&mut self, timeout: Duration) -> bool {
        self.sub.synced(timeout).await
    }

    /// The per-relay replay counts behind [`Self::live`], so a caller can tell
    /// "no relay served the range" from "one of three lagged".
    pub async fn live_state(&mut self, timeout: Duration) -> SyncState {
        self.sub.sync_state(timeout).await
    }

    /// The next valid 445 under one of the catch-up windows. Same strict tag
    /// gate as [`GroupSub::recv`] — and no roll check, by design.
    ///
    /// A quiet slice that ends with NO relay connection up returns
    /// [`GroupRecv::Deaf`], never `Idle` (FP2): "nothing stored" sends the
    /// caller to the sharer, "no relay reachable" to the network, and the
    /// two must not conflate. Evaluated only at slice end, so a healed blip
    /// inside the slice never trips it.
    pub async fn recv(&mut self, timeout: Duration) -> GroupRecv {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return match self.sub.deaf().await {
                    Some(why) => GroupRecv::Deaf(why),
                    None => GroupRecv::Idle,
                };
            }
            let Some(event) = self.sub.recv(deadline.saturating_duration_since(now)).await else {
                // slice elapsed (the channel itself never closes — the
                // reconnect supervisors hold the senders)
                continue;
            };
            let tags: Vec<Vec<String>> =
                event.tags.iter().map(|t| t.as_slice().to_vec()).collect();
            match envelope::parse_445_tags(&tags) {
                Ok((h, _expiration)) if self.tags.contains(&h) => {
                    return GroupRecv::Frame {
                        content: event.content,
                        created_at: event.created_at.as_secs(),
                    };
                }
                Ok((h, _)) => tracing::debug!(h = %h, "catch-up: skipping a foreign h tag"),
                Err(e) => tracing::debug!(error = %e, "catch-up: skipping an invalid tag shape"),
            }
        }
    }
}

/// A live group subscription. Owns its [`GroupChannel`] so it can DROP and
/// re-place the subscription when the UTC day window rolls past what the
/// live filter covers (a placed Filter is immutable — N2 limit). A
/// resubscribe replays the relay backlog: at-least-once, callers idempotent.
pub struct GroupSub {
    sub: Subscription,
    /// The tags the LIVE subscription was opened with — the roll detector
    /// compares [`window_tags`] at now against exactly this.
    tags: Vec<String>,
    channel: GroupChannel,
    /// Why the last re-placement failed, while it is still failing. `None`
    /// once a retry succeeds.
    deaf: Option<String>,
    /// Earliest next re-placement attempt — without it a failing roll retries
    /// on every caller iteration, which is a busy-spin, not a retry.
    retry_at: tokio::time::Instant,
    retry_backoff: Duration,
}

// manual: the channel inside holds the secret-class rotation seed
impl std::fmt::Debug for GroupSub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupSub")
            .field("tags", &self.tags)
            .finish_non_exhaustive()
    }
}

impl GroupSub {
    /// Best-effort replay gate, like [`RitualInbox::live`].
    pub async fn live(&mut self, timeout: Duration) -> bool {
        self.sub.synced(timeout).await
    }

    /// The replay counts behind [`Self::live`] — so a caller can tell "no
    /// relay is readable" (a provisioning failure) from "one of three lagged"
    /// (a warning). Proceeding blind on the first is how a ritual times out
    /// with no error anywhere.
    pub async fn live_state(&mut self, timeout: Duration) -> SyncState {
        self.sub.sync_state(timeout).await
    }

    /// The next VALID kind-445 frame as `(sealed content, created_at)`, or
    /// `None` when nothing valid arrives within `timeout`. The gate is
    /// strict [`envelope::parse_445_tags`] AND the `h` must be one of OUR
    /// current tags — malformed tag shapes and foreign-group frames are
    /// skipped at debug, never fatal. The UTC window roll is checked
    /// between waits ([`ROLL_POLL`] slices) and triggers a resubscribe
    /// under the fresh tags before the wait continues.
    pub async fn recv(&mut self, timeout: Duration) -> GroupRecv {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let current = window_tags(&self.channel.rotation_seed, now_secs());
            // A re-placement is only NEEDED when the live subscription does
            // not already cover the wanted tags. `window_tags` NARROWS as the
            // skew margin passes ([W, W-1] -> [W]), so a plain `!=` called a
            // strict superset "stale" — and a failed re-placement then
            // reported deafness while reception was in fact complete.
            let covered = current.iter().all(|t| self.tags.contains(t));
            if !covered && tokio::time::Instant::now() >= self.retry_at {
                match self.channel.subscribe_tags(&current).await {
                    Ok(sub) => {
                        // the old subscription drops here — its supervisors
                        // abort (pure inbound, the sanctioned abort)
                        self.sub = sub;
                        self.tags = current;
                        self.deaf = None;
                        self.retry_backoff = RETRY_INITIAL;
                    }
                    Err(e) => {
                        // Returning `None` here was the bug: every caller
                        // reads it as "idle", so a node went permanently deaf
                        // at a UTC boundary while looking healthy — and,
                        // because the caller loops immediately, span the CPU
                        // doing it. Say Deaf, and gate the next attempt.
                        let why = e.to_string();
                        tracing::warn!(error = %why, "445 window-roll resubscribe failed");
                        self.deaf = Some(why.clone());
                        self.retry_at = tokio::time::Instant::now() + self.retry_backoff;
                        self.retry_backoff = (self.retry_backoff * 2).min(RETRY_CAP);
                        return GroupRecv::Deaf(why);
                    }
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                // while deaf, the budget elapsing is NOT the honest quiet
                return match &self.deaf {
                    Some(why) => GroupRecv::Deaf(why.clone()),
                    None => GroupRecv::Idle,
                };
            }
            let slice = deadline.saturating_duration_since(now).min(ROLL_POLL);
            let Some(event) = self.sub.recv(slice).await else {
                continue; // slice elapsed — re-check the roll and the budget
            };
            let tags: Vec<Vec<String>> =
                event.tags.iter().map(|t| t.as_slice().to_vec()).collect();
            match envelope::parse_445_tags(&tags) {
                Ok((h, _expiration)) if self.tags.contains(&h) => {
                    // a DELIVERED frame is the honest proof of reception:
                    // whatever the roll did, this channel is being heard.
                    // Without this the flag survived the frame and the
                    // caller's "the group channel is back" / "cannot hear it"
                    // pair alternated forever, both false when written.
                    self.deaf = None;
                    return GroupRecv::Frame {
                        content: event.content,
                        created_at: event.created_at.as_secs(),
                    };
                }
                Ok((h, _)) => {
                    tracing::debug!(h = %h, "skipping a 445 under a foreign h tag");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "skipping a 445 with an invalid tag shape");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start stamp ahead of the fetcher's clock across a day boundary
    /// still names the start's window (the pieces sit there), the current
    /// one, and the skew neighbours - never the fetcher's window alone.
    #[test]
    fn the_file_catchup_tags_cover_a_start_ahead_of_the_clock() {
        let seed = [7u8; 32];
        let boundary = 1_800_000_000 / H_WINDOW * H_WINDOW;
        let now = boundary - 20;
        let start = now + 30; // the sharer's clock is 30 s ahead
        let tags = file_catchup_tags(&seed, start, now, 60);
        assert!(tags.contains(&envelope::h_tag(&seed, start)), "the start's window");
        assert!(tags.contains(&envelope::h_tag(&seed, now)), "the fetcher's window");
        assert_eq!(tags.len(), 2, "no duplicates: {tags:?}");
        // the ordinary case: three days back through now
        let three = file_catchup_tags(&seed, now - 3 * H_WINDOW, now, 60);
        assert!(three.len() >= 4);
        assert_eq!(three.first(), Some(&envelope::h_tag(&seed, now - 3 * H_WINDOW)));
    }
}
