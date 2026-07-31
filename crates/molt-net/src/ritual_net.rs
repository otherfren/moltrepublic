// SPDX-License-Identifier: GPL-3.0-or-later

//! N4a (`docs/transport/nostr_n4_plan.md` §5): the engine-facing **Nostr
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
use crate::relay_runtime::{RelayRuntime, Subscription};
use crate::ritual_wrap::{self, RitualWrapError};
use crate::welcome::{self, WelcomeError, WelcomePayload};
use crate::NetError;

/// The NIP-59 gift-wrap kind — the OUTER event both ritual rumors (446)
/// and Welcomes (444) arrive under.
const KIND_GIFT_WRAP: u16 = 1_059;

/// The kind-445 group-event kind (NIP-EE group messages).
const KIND_GROUP: u16 = 445;

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

    /// Publish one signed event with ≥1-OK semantics over a fresh runtime.
    async fn publish(&self, event: &nostr::Event) -> Result<(), NetError> {
        RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .publish(event)
            .await
            .map(|_report| ())
    }

    /// Gift-wrap a [`RitualMsg`] (kind-446 rumor) to `to_pk_hex` and publish
    /// it — success once ≥1 relay accepted the wrap.
    pub async fn send_ritual(&self, to_pk_hex: &str, msg: &RitualMsg) -> Result<(), NetError> {
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
    ) -> Result<(), NetError> {
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
            .kind(Kind::Custom(KIND_GIFT_WRAP))
            .pubkey(self.keys.public_key());
        let sub = RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .subscribe(filter)
            .await?;
        Ok(RitualInbox { sub, keys: self.keys.clone() })
    }
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
        Self { dialer, relays, rotation_seed }
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
    ) -> Result<u64, NetError> {
        let sealed = envelope::seal_outer(exporter, mls_ciphertext)
            .map_err(|e| NetError::Crypto(format!("sealing the 445 frame: {e}")))?;
        // one `now` for tag and stamp: deriving them separately could
        // straddle a window boundary and publish a stamp its tag disowns
        let now = Timestamp::now();
        let tag = envelope::h_tag(&self.rotation_seed, now.as_secs());
        let h = Tag::parse(["h", tag.as_str()])
            .map_err(|e| NetError::Framing(format!("h tag: {e}")))?;
        let event = EventBuilder::new(Kind::Custom(KIND_GROUP), sealed)
            .tag(h)
            .custom_created_at(now)
            .sign_with_keys(&Keys::generate())
            .map_err(|e| NetError::Crypto(format!("signing the 445 frame: {e}")))?;
        let stamp = event.created_at.as_secs();
        RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .publish(&event)
            .await?;
        Ok(stamp)
    }

    /// Subscribe kind-445 under the tags [`window_tags`] names right now —
    /// the current window's, plus the adjacent one's inside the §4.4 skew
    /// margin (ONE filter, both `#h` values).
    pub async fn subscribe(&self) -> Result<GroupSub, NetError> {
        let tags = window_tags(&self.rotation_seed, Timestamp::now().as_secs());
        let sub = self.subscribe_tags(&tags).await?;
        Ok(GroupSub { sub, tags, channel: self.clone() })
    }

    /// Place one pooled 445 subscription over exactly `tags`.
    async fn subscribe_tags(&self, tags: &[String]) -> Result<Subscription, NetError> {
        let filter = Filter::new()
            .kind(Kind::Custom(KIND_GROUP))
            .custom_tags(SingleLetterTag::lowercase(Alphabet::H), tags.iter().cloned());
        RelayRuntime::new(self.dialer.clone(), self.relays.clone())
            .subscribe(filter)
            .await
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

    /// The next VALID kind-445 frame as `(sealed content, created_at)`, or
    /// `None` when nothing valid arrives within `timeout`. The gate is
    /// strict [`envelope::parse_445_tags`] AND the `h` must be one of OUR
    /// current tags — malformed tag shapes and foreign-group frames are
    /// skipped at debug, never fatal. The UTC window roll is checked
    /// between waits ([`ROLL_POLL`] slices) and triggers a resubscribe
    /// under the fresh tags before the wait continues.
    pub async fn recv(&mut self, timeout: Duration) -> Option<(String, u64)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let current =
                window_tags(&self.channel.rotation_seed, Timestamp::now().as_secs());
            if current != self.tags {
                match self.channel.subscribe_tags(&current).await {
                    Ok(sub) => {
                        // the old subscription drops here — its supervisors
                        // abort (pure inbound, the sanctioned abort)
                        self.sub = sub;
                        self.tags = current;
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "445 window-roll resubscribe failed");
                        return None;
                    }
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let slice = deadline.saturating_duration_since(now).min(ROLL_POLL);
            let Some(event) = self.sub.recv(slice).await else {
                continue; // slice elapsed — re-check the roll and the budget
            };
            let tags: Vec<Vec<String>> =
                event.tags.iter().map(|t| t.as_slice().to_vec()).collect();
            match envelope::parse_445_tags(&tags) {
                Ok((h, _expiration)) if self.tags.contains(&h) => {
                    return Some((event.content, event.created_at.as_secs()));
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
