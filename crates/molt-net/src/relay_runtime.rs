// SPDX-License-Identifier: GPL-3.0-or-later

//! N2 (`docs_archive/transport/nostr_n2_plan.md` §2): the pool runtime over
//! [`RelayWs`] — publish with ≥1-OK semantics today; subscriptions, cursors,
//! dedup, the EOSE gate and connection supervision land with the later plan
//! steps. The relay list ALWAYS arrives from `molt_core::relay::dialable`
//! (ADR-0004) — this module never reads the pool or decides dial policy.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use nostr::{ClientMessage, Event, EventId, Filter, JsonUtil, RelayMessage, SubscriptionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

use crate::dial::Dialer;
use crate::relay_ws::{dial_maybe_tls, RecvFail, RelayWs};
use crate::NetError;

/// Per-relay deadline for one publish attempt (dial + upgrade + OK).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// A subscription reader's INBOUND idle bound: with a keepalive Ping every
/// [`KEEPALIVE`] a healthy relay ponges, so a relay from which not one frame
/// arrived for this long is dead even if our writes still "succeed" (dropped
/// Tor circuit behind a live SOCKS proxy) — cut and reconnect. Measured on
/// [`RelayWs::idle_for`] (received frames only): armed by anything else the
/// bound can never fire, and an inbound-dead node stays deaf for good.
const SUB_IDLE_TIMEOUT: Duration = Duration::from_secs(150);

/// Idle interval after which the reader pings the relay, so a silently
/// dropped flow surfaces in seconds rather than at the idle bound.
const KEEPALIVE: Duration = Duration::from_secs(45);

/// A (re)connect must complete inside this: dial and TLS carry their own
/// deadlines, but the WS upgrade itself would otherwise wait forever on a
/// relay that accepts bytes and never answers (review finding, HIGH).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a session must live for the reconnect backoff to reset.
const HEALTHY_SESSION: Duration = Duration::from_secs(30);

/// Dedup ring capacity: enough for the WP4a-horizon backlog of a busy
/// republic, bounded so a hostile relay cannot balloon memory.
const DEDUP_CAP: usize = 4096;

/// Clock-skew margin (concept §4.4's ±1h): how far past local now a peer's
/// `created_at` may plausibly sit. It is deliberately NOT applied to the
/// cursor — the cursor clamps to local now, so a future-dated event cannot
/// eat into the resubscribe overlap. The envelope layer (N3) applies it when
/// judging an event's freshness.
pub const CURSOR_SKEW: u64 = 3_600;

/// Re-subscribe overlap: `since = cursor − OVERLAP`, the full NIP-59
/// timestamp-tweak width — without it, offline gift-wraps are permanently
/// skipped (MDK port #2, `mdk_evaluation.md` §2.2).
const CURSOR_OVERLAP: u64 = 172_800;

/// The publish budget applied while no NIP-11 cap is known: the smallest
/// `max_message_length` measured on public relays (nos.lol, N0 2026-07-30).
/// Conservative — refuse rather than let one relay accept what another
/// silently drops (the §7 wire-size cliff).
///
/// **Public because it bounds what may ENTER the chain**, not merely what
/// may leave it: a payload that cannot become a publishable 445 wedges the
/// outbox at the envelope carrying it (the cursor holds, deliberately, so a
/// transient relay failure loses nothing — and a permanent one then never
/// clears). The honest place to refuse it is where the user can still act,
/// which is the propose path in molt-engine, and that needs this number.
pub const DEFAULT_SIZE_BUDGET: u64 = 128 * 1024;

/// Ceiling a relay-advertised frame cap may raise the budget to: this
/// client's own hard WebSocket read limit — a budget above it would let
/// the node publish frames it could never read back on subscribe.
#[allow(clippy::as_conversions)] // usize → u64 is lossless
pub const MAX_SANE_RELAY_CAP: u64 = crate::relay_ws::MAX_WS_MESSAGE as u64;

/// Sanity-bound a relay-advertised NIP-11 `max_message_length` BEFORE it
/// may drive any budget (relay_topology_plan §7, carried from N2): a
/// lying relay must never zero or shrink the publish budget. `None` =
/// not usable (below the floor the pool already requires for admission —
/// a probed cap may only ever RAISE the budget); above the ceiling it
/// clamps (a generous relay is not misbehaving). The probe itself stays
/// an honest reporter — sanitize at the consumption site, so refusal
/// messages can still quote the raw number.
pub fn sane_relay_cap(cap: u64) -> Option<u64> {
    if cap < DEFAULT_SIZE_BUDGET {
        return None;
    }
    Some(cap.min(MAX_SANE_RELAY_CAP))
}

/// Bound on a NIP-11 response we are willing to read.
const NIP11_MAX_RESPONSE: usize = 64 * 1024;

/// How one publish went, per relay — ≥1 accepted relay makes the publish a
/// success, but the failures are always REPORTED, never hidden (concept
/// §11 N2: no silent partial).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublishReport {
    /// Relays that accepted the event (or already had it — see
    /// [`counts_as_published`]).
    pub accepted: Vec<String>,
    /// Relays that did not, with the honest reason.
    pub failed: Vec<(String, String)>,
}

/// NIP-01 outcome → did this event land on this relay? An `OK:false` whose
/// message starts with `duplicate:` means the relay ALREADY HOLDS the event —
/// for the delivery guarantee that IS "published", and treating it as an
/// error would make every rewind-resend look like a failure (MDK port #3,
/// `mdk_evaluation.md` §2.2).
fn counts_as_published(status: bool, message: &str) -> bool {
    status || message.starts_with("duplicate:")
}

/// The relay-pool runtime: everything the engine-facing transport will drive
/// (publish now; subscribe/cursor/health in later N2 steps).
#[derive(Clone)]
pub struct RelayRuntime {
    dialer: Dialer,
    /// Normalized relay URLs, in priority order — the output of
    /// `molt_core::relay::dialable(...)`, never a raw pool.
    urls: Vec<String>,
    /// Per-relay resume cursor: the max CLAMPED `created_at` delivered by
    /// that relay. What a reopen persists and reseeds ([`Self::with_cursors`]).
    cursors: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    /// The pool-wide publish size budget (bytes of the serialized event):
    /// the SMALLEST configured relay's NIP-11 `max_message_length`, or the
    /// conservative floor when unknown. `None` = not probed yet — publishes
    /// then apply [`DEFAULT_SIZE_BUDGET`].
    size_budget: Option<u64>,
    /// Per-relay connection state, written by the subscription supervisors.
    health: Arc<Mutex<std::collections::HashMap<String, RelayHealth>>>,
    /// Stored-event bound per REQ (see [`MAX_STORED_EVENTS_PER_REQ`]).
    history_bound: usize,
    /// Reconnect backoff (initial, cap) — overridable for tests.
    backoff: (Duration, Duration),
    /// Subscription (keepalive interval, idle bound) — overridable for
    /// tests; defaults [`KEEPALIVE`] / [`SUB_IDLE_TIMEOUT`].
    sub_timing: (Duration, Duration),
    /// NIP-42 identity for the SUBSCRIBE connections (the per-republic
    /// transport anchor). Publishing never authenticates — an authenticated
    /// publish channel would link every ephemeral-key event to the member
    /// (`mdk_evaluation.md` §5, concept §7.5).
    auth_keys: Option<nostr::Keys>,
}

// manual: the auth keys hold a SECRET key — never in Debug output
impl std::fmt::Debug for RelayRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRuntime")
            .field("urls", &self.urls)
            .field("size_budget", &self.size_budget)
            .field("backoff", &self.backoff)
            .field("auth", &self.auth_keys.is_some())
            .finish_non_exhaustive()
    }
}

/// One relay's connection state as the supervisors see it — the seed of the
/// N5 `net_health` relay model ("relays, not members").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayHealth {
    /// Connected, subscription live.
    Up,
    /// Between sessions — a (re)connect attempt is running.
    Connecting,
    /// The last attempt failed; the supervisor is backing off.
    Down,
}

/// How many STORED events one relay may replay for a single REQ before we
/// stop reading from it.
///
/// Counted pre-EOSE only; live traffic is unbounded, which is what buzz's
/// "500 historical events per filter" actually means. Ours is an order of
/// magnitude higher because one of our REQs legitimately spans several past
/// h-windows (`GroupChannel::subscribe_since`) — a bound tight enough to catch
/// a chat flood would silently truncate a real catch-up, and a member quietly
/// missing history it believes it has is worse than a flood that announces
/// itself. At this size it is evidence of a hostile or broken relay, never a
/// tuning knob.
pub const MAX_STORED_EVENTS_PER_REQ: usize = 5_000;

impl RelayRuntime {
    /// A runtime over the CURRENTLY dialable relays. An empty list is legal
    /// (a fresh install) — every operation then fails typed, and connects to
    /// nothing, silently (ADR-0004).
    pub fn new(dialer: Dialer, urls: Vec<String>) -> Self {
        // The dial PLAN, once, before anything is attempted. Without it an
        // operator cannot tell "Tor is broken" from "this node was never
        // going to dial anything" — and an empty pool connects to nothing by
        // design, which looks identical to a failure from the outside.
        if urls.is_empty() {
            tracing::warn!(via = %dialer.route(), "no dialable relay — nothing will be contacted");
        } else {
            tracing::info!(relays = urls.len(), via = %dialer.route(), targets = %urls.join(" "), "relay runtime");
        }
        Self {
            dialer,
            urls,
            cursors: Arc::new(Mutex::new(std::collections::HashMap::new())),
            size_budget: None,
            health: Arc::new(Mutex::new(std::collections::HashMap::new())),
            backoff: (Duration::from_secs(1), Duration::from_secs(60)),
            auth_keys: None,
            history_bound: MAX_STORED_EVENTS_PER_REQ,
            sub_timing: (KEEPALIVE, SUB_IDLE_TIMEOUT),
        }
    }

    /// Provide the NIP-42 identity for subscribe connections (the transport
    /// anchor). Without it, an auth-required relay simply never syncs —
    /// honestly visible via [`Subscription::synced`] and `health()`.
    #[must_use]
    pub fn with_auth_keys(self, keys: Option<nostr::Keys>) -> Self {
        Self { auth_keys: keys, ..self }
    }

    /// Override the reconnect backoff (tests use milliseconds).
    #[must_use]
    pub fn with_backoff(self, initial: Duration, cap: Duration) -> Self {
        Self { backoff: (initial, cap), ..self }
    }

    /// Override the subscription keepalive interval and idle bound
    /// ([`KEEPALIVE`], [`SUB_IDLE_TIMEOUT`] — tests use milliseconds).
    #[must_use]
    pub fn with_sub_timing(self, keepalive: Duration, idle: Duration) -> Self {
        Self { sub_timing: (keepalive, idle), ..self }
    }

    /// Override [`MAX_STORED_EVENTS_PER_REQ`] (tests use a small number — a
    /// test that had to publish five thousand events to reach the real bound
    /// would be measuring the relay, not the bound).
    #[must_use]
    pub fn with_history_bound(self, bound: usize) -> Self {
        Self { history_bound: bound, ..self }
    }

    /// The supervisors' per-relay connection state — the honest input for
    /// "relay status" surfaces (N5). Empty until a subscription runs.
    pub async fn health(&self) -> std::collections::HashMap<String, RelayHealth> {
        self.health.lock().await.clone()
    }

    /// Override the publish size budget (tests, or the engine after probing
    /// every relay's NIP-11 via [`probe_nip11_max_message`] and taking the
    /// minimum). `None` keeps the conservative [`DEFAULT_SIZE_BUDGET`].
    #[must_use]
    pub fn with_size_budget(self, budget: Option<u64>) -> Self {
        Self { size_budget: budget, ..self }
    }

    /// Reseed the per-relay cursors (the reopen path — cursors come from the
    /// persisted transport state).
    #[must_use]
    pub fn with_cursors(self, cursors: std::collections::HashMap<String, u64>) -> Self {
        Self { cursors: Arc::new(Mutex::new(cursors)), ..self }
    }

    /// The current per-relay cursors — what a close persists.
    pub async fn cursors(&self) -> std::collections::HashMap<String, u64> {
        self.cursors.lock().await.clone()
    }

    /// Publish one signed event to EVERY relay concurrently. Success iff at
    /// least one relay accepted (or already held) it; the per-relay outcomes
    /// ride in the report either way.
    pub async fn publish(&self, event: &Event) -> Result<PublishReport, NetError> {
        if self.urls.is_empty() {
            return Err(NetError::Unreachable(
                "no dialable relay — the pool is empty or gated".into(),
            ));
        }
        // the size budget gates BEFORE any relay sees the event: one relay
        // accepting what another drops is the §7 wire-size cliff (a cursor
        // advances past an event a smaller relay never stored)
        let budget = self.size_budget.unwrap_or(DEFAULT_SIZE_BUDGET);
        // NIP-11's cap bounds the WEBSOCKET MESSAGE, not the event JSON —
        // measure what actually goes on the wire, `["EVENT",{…}]` framing
        // included, or an event exactly at the cap ships a frame over it
        // (review finding)
        let size = u64::try_from(ClientMessage::event(event.clone()).as_json().len())
            .unwrap_or(u64::MAX);
        if size > budget {
            return Err(NetError::Framing(format!(
                "event of {size} bytes exceeds the smallest relay cap ({budget} bytes) — refused before publish"
            )));
        }
        let attempts = self.urls.iter().map(|url| {
            let dialer = self.dialer.clone();
            let event = event.clone();
            async move {
                let outcome =
                    tokio::time::timeout(PUBLISH_TIMEOUT, publish_one(&dialer, url, &event))
                        .await
                        .unwrap_or_else(|_| {
                            Err(NetError::Unreachable("publish timed out".into()))
                        });
                (url.clone(), outcome)
            }
        });
        let mut report = PublishReport::default();
        for (url, outcome) in futures_util::future::join_all(attempts).await {
            match outcome {
                Ok(()) => report.accepted.push(url),
                Err(e) => report.failed.push((url, e.to_string())),
            }
        }
        // the shared ≥1-OK reduction (the persistent pool uses it too)
        finish_publish_report(report)
    }
}

/// The bounded first-seen event-id ring behind the subscription fan-in:
/// relay-count copies collapse to ONE delivery.
struct DedupRing {
    seen: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl DedupRing {
    fn new() -> Self {
        Self { seen: HashSet::new(), order: VecDeque::new() }
    }

    /// `true` iff this id was not seen before (and is now recorded).
    fn fresh(&mut self, id: EventId) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > DEDUP_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

/// A pooled subscription: one filter, every relay, ONE deduplicated stream
/// out.
///
/// **Dedup horizon.** The ring holds the last [`DEDUP_CAP`] event ids, so
/// relay-count copies collapse within that window; a republic with more
/// events than that inside the 48 h resubscribe overlap can see an evicted
/// id re-delivered after a reconnect. That is within the at-least-once
/// contract (`docs_archive/transport/delivery_guarantee.md`) — but the envelope
/// layer above MUST stay idempotent and must not read this stream as
/// exactly-once.
///
/// **Teardown.** Dropping aborts the per-relay supervisors. The only
/// outbound frames they own are subscription-scoped (the REQ and a NIP-42
/// AUTH), so losing them on teardown is lossless by construction — the
/// drain rule that governs the delivery path does not apply here.
pub struct Subscription {
    rx: mpsc::Receiver<Event>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    /// How many relays actually accepted the REQ — the EOSE gate's
    /// denominator (a dead relay never joined, so it cannot wedge the gate).
    connected: usize,
    /// How many of them finished their stored-events replay.
    eose: tokio::sync::watch::Receiver<usize>,
    /// Shared health map + this subscription's relays, so teardown can clear
    /// the entries instead of leaving a stale `Up` behind (review finding).
    health: Arc<Mutex<std::collections::HashMap<String, RelayHealth>>>,
    urls: Vec<String>,
}

impl Subscription {
    /// The next deduplicated event, or `None` if nothing arrived within
    /// `timeout` (also `None` once every reader has ended and the buffer is
    /// drained).
    pub async fn recv(&mut self, timeout: Duration) -> Option<Event> {
        tokio::time::timeout(timeout, self.rx.recv()).await.ok().flatten()
    }

    /// Wait until EVERY connected relay sent EOSE (MDK port #6) — only then
    /// is the stored backlog complete. `false` on timeout.
    pub async fn synced(&mut self, timeout: Duration) -> bool {
        self.sync_state(timeout).await.full()
    }

    /// `Some(reason)` while NO relay connection is up — the subscription is
    /// deaf right now (the supervisors keep reconnecting, so this is
    /// advisory, never terminal). `None` while at least one relay is `Up`.
    /// The channel itself never closes (reconnect supervisors hold the
    /// senders), so this health read is the only honest deafness signal.
    pub async fn deaf(&self) -> Option<String> {
        let health = self.health.lock().await;
        let up = self
            .urls
            .iter()
            .filter(|u| matches!(health.get(*u), Some(RelayHealth::Up)))
            .count();
        (up == 0).then(|| {
            format!("no live relay connection (0 of {} up, reconnecting)", self.urls.len())
        })
    }

    /// How many relays actually replayed, out of how many accepted the REQ.
    ///
    /// `synced()` collapses "none replayed" and "some replayed" into one
    /// `false`, which is the difference between a provisioning FAILURE and a
    /// warning: a single lagging relay in a healthy pool must not kill a
    /// founding, but a pool where NOTHING is readable must not be proceeded
    /// through blind.
    pub async fn sync_state(&mut self, timeout: Duration) -> SyncState {
        let deadline = tokio::time::Instant::now() + timeout;
        while *self.eose.borrow() < self.connected {
            match tokio::time::timeout_at(deadline, self.eose.changed()).await {
                Ok(Ok(())) => {}
                // timeout, or every sender gone without reaching the count
                _ => break,
            }
        }
        SyncState { synced: *self.eose.borrow(), connected: self.connected }
    }
}

/// How much of a subscription is proven readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncState {
    /// Relays that replayed their stored events (sent EOSE).
    pub synced: usize,
    /// Relays that accepted the REQ at all.
    pub connected: usize,
}

impl SyncState {
    /// At least one relay is proven readable — the ≥1 rule, mirroring the
    /// pool's ≥1-OK publish semantics. Below this, nothing may proceed.
    pub fn any(&self) -> bool {
        self.synced > 0
    }

    /// Every relay that accepted the REQ replayed.
    pub fn full(&self) -> bool {
        self.synced >= self.connected
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
        // …and drop the health entries with them: a torn-down subscription
        // has no connections, so reporting `Up` would be a stale lie to the
        // N5 relay-status surface. `try_lock` keeps Drop non-blocking; the
        // supervisors are already aborted either way.
        if let Ok(mut health) = self.health.try_lock() {
            for url in &self.urls {
                health.remove(url);
            }
        }
    }
}

/// Everything a per-relay supervisor needs — shared across relays and
/// reconnect sessions.
struct SubShared {
    dialer: Dialer,
    filter: Filter,
    tx: mpsc::Sender<Event>,
    dedup: Arc<Mutex<DedupRing>>,
    cursors: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    eose_tx: Arc<tokio::sync::watch::Sender<usize>>,
    health: Arc<Mutex<std::collections::HashMap<String, RelayHealth>>>,
    backoff: (Duration, Duration),
    auth_keys: Option<nostr::Keys>,
    history_bound: usize,
    /// (keepalive interval, idle bound) — see [`KEEPALIVE`] / [`SUB_IDLE_TIMEOUT`].
    sub_timing: (Duration, Duration),
}

impl RelayRuntime {
    /// Subscribe the SAME filter on every relay and fan the deliveries into
    /// one deduplicated stream. Each relay gets a SUPERVISOR: on session
    /// loss it goes `Down` in `health()`, reconnects with backoff, and
    /// RE-SUBSCRIBES from the current cursor (minus the overlap) — the
    /// dedup ring absorbs the overlap redeliveries. Succeeds if at least
    /// one relay accepted the initial REQ; a relay that only comes alive
    /// later is picked up by its supervisor but stays outside the EOSE
    /// sync-gate denominator.
    pub async fn subscribe(&self, filter: Filter) -> Result<Subscription, NetError> {
        if self.urls.is_empty() {
            return Err(NetError::Unreachable(
                "no dialable relay — the pool is empty or gated".into(),
            ));
        }
        // The channel must hold the WHOLE bounded stored replay without a
        // consumer: the ritual legs gate on `sync_state` BEFORE their first
        // recv, and a replay larger than the channel would park the reader
        // on a full `send` with the EOSE still unread behind it — the gate
        // then times out against a healthy relay, and forever, because the
        // backlog only grows (found 2026-08-17, recovery rerun). The bound
        // is honest: the reader drops a relay that replays past
        // `history_bound`, so this can never buffer more than that (+ some
        // live headroom).
        let (tx, rx) = mpsc::channel(self.history_bound + 64);
        let (eose_tx, eose_rx) = tokio::sync::watch::channel(0usize);
        let shared = Arc::new(SubShared {
            dialer: self.dialer.clone(),
            filter,
            tx,
            dedup: Arc::new(Mutex::new(DedupRing::new())),
            cursors: self.cursors.clone(),
            eose_tx: Arc::new(eose_tx),
            health: self.health.clone(),
            backoff: self.backoff,
            auth_keys: self.auth_keys.clone(),
            history_bound: self.history_bound,
            sub_timing: self.sub_timing,
        });
        // the FIRST connects run CONCURRENTLY and bounded: sequentially, one
        // relay that accepts TCP but never answers the WS upgrade would wedge
        // subscribe() — and with it the engine task driving it (review
        // finding, HIGH)
        let firsts = futures_util::future::join_all(self.urls.iter().map(|url| {
            let shared = shared.clone();
            let via = crate::relay_ws::dialer_for(&self.dialer, url).route();
            async move {
                let began = tokio::time::Instant::now();
                // This is the connect an operator actually watches — the one
                // at startup. It used to end in `.ok().and_then(Result::ok)`,
                // throwing away BOTH the timeout and the reason, so a node
                // that reached no relay said nothing about why. Report every
                // outcome; the Option is what the caller still needs.
                match tokio::time::timeout(CONNECT_TIMEOUT, connect_and_req(&shared, url)).await {
                    Ok(Ok(ws)) => {
                        tracing::info!(
                            relay = %url, via = %via,
                            ms = began.elapsed().as_millis(),
                            "relay connected"
                        );
                        Some(ws)
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(relay = %url, via = %via, error = %e, "relay connect failed");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            relay = %url, via = %via,
                            after_s = CONNECT_TIMEOUT.as_secs(),
                            "relay connect timed out"
                        );
                        None
                    }
                }
            }
        }))
        .await;

        let mut supervisors = Vec::new();
        // the EOSE gate's denominator is exactly the set of relays whose
        // FIRST connect succeeded — and only those relays may count toward
        // it, or a late-arriving relay's EOSE would satisfy a gate it was
        // never part of and synced() would lie (review finding, HIGH)
        let mut gated: Vec<String> = Vec::new();
        for (url, first) in self.urls.iter().zip(firsts) {
            let up = first.is_some();
            if up {
                gated.push(url.clone());
            }
            shared
                .health
                .lock()
                .await
                .insert(url.clone(), if up { RelayHealth::Up } else { RelayHealth::Down });
            let shared = shared.clone();
            let url = url.clone();
            let counts = up;
            supervisors.push(tokio::spawn(supervise(shared, url, first, counts)));
        }
        if gated.is_empty() {
            // abort before returning: a dropped JoinHandle DETACHES, and a
            // detached supervisor would redial a dead relay forever and keep
            // writing health for a subscription that does not exist (review
            // finding, HIGH)
            for s in supervisors {
                s.abort();
            }
            // the per-relay reasons are already on WARN above; this line ties
            // them together so the operator sees the verdict, not just N
            // individual complaints
            tracing::warn!(
                tried = self.urls.len(),
                via = %self.dialer.route(),
                "no relay accepted the subscription"
            );
            return Err(NetError::Unreachable(
                "no relay accepted the subscription".into(),
            ));
        }
        Ok(Subscription {
            rx,
            readers: supervisors,
            connected: gated.len(),
            eose: eose_rx,
            health: self.health.clone(),
            urls: self.urls.clone(),
        })
    }
}

/// Connect one relay and place the REQ, resuming from the CURRENT cursor
/// widened by the overlap — the write-side clamp keeps that honest against
/// future-dated events (concept §4.3).
async fn connect_and_req(shared: &SubShared, url: &str) -> Result<RelayWs, NetError> {
    let mut ws = RelayWs::connect(&shared.dialer, url).await?;
    // ONE subscription id per session: the post-AUTH re-placement reuses it
    // (NIP-01 overwrites a REQ by id), so the pre-auth subscription is not
    // left open delivering every event a second time (review finding)
    let sub_id = SubscriptionId::generate();
    place_req(shared, url, &mut ws, &sub_id).await?;
    ws.set_subscription(sub_id);
    Ok(ws)
}

/// Place (or re-place, after a NIP-42 handshake) the subscription REQ.
async fn place_req(
    shared: &SubShared,
    url: &str,
    ws: &mut RelayWs,
    sub_id: &SubscriptionId,
) -> Result<(), NetError> {
    let mut relay_filter = shared.filter.clone();
    if let Some(cursor) = shared.cursors.lock().await.get(url) {
        relay_filter = relay_filter.since(nostr::Timestamp::from_secs(
            cursor.saturating_sub(CURSOR_OVERLAP),
        ));
    }
    tracing::debug!(relay = %url, filter = %nostr::JsonUtil::as_json(&relay_filter), "placing REQ");
    ws.send(ClientMessage::req(sub_id.clone(), vec![relay_filter]))
        .await
}

/// One relay's supervisor: read the live session until it dies, then
/// backoff-reconnect-resubscribe forever (the Subscription's drop aborts
/// us — pure inbound, the sanctioned abort). The EOSE gate is counted at
/// most ONCE per relay across all sessions, so a reconnect cannot inflate
/// the denominator's numerator.
async fn supervise(
    shared: Arc<SubShared>,
    url: String,
    first: Option<RelayWs>,
    counts_toward_eose: bool,
) {
    let (initial, cap) = shared.backoff;
    let mut backoff = initial;
    let mut session = first;
    // relays that were down at subscribe time are NOT in the gate's
    // denominator, so they must never increment its numerator either
    let mut eose_counted = !counts_toward_eose;
    // the route this relay actually takes (a Local relay is dialed direct
    // even under Tor, §10.14) — the single most useful field when an
    // operator says "Tor runs but the client will not connect"
    let via = crate::relay_ws::dialer_for(&shared.dialer, &url).route();
    let mut attempt: u32 = 0;
    // the last failure reported at WARN — an identical repeat stays on
    // DEBUG so a permanently dead relay does not bury everything else
    let mut last_reason: Option<String> = None;
    loop {
        let ws = match session.take() {
            Some(ws) => ws,
            None => {
                attempt += 1;
                shared.health.lock().await.insert(url.clone(), RelayHealth::Connecting);
                let began = tokio::time::Instant::now();
                let outcome =
                    tokio::time::timeout(CONNECT_TIMEOUT, connect_and_req(&shared, &url)).await;
                // NEVER discard the reason. This arm used to be a bare `_`,
                // so every connect failure — proxy refused, TLS rejected, WS
                // upgrade 4xx, auth required — vanished and the loop retried
                // forever in silence. That silence IS the bug an operator
                // reports as "it just says it cannot connect".
                match outcome {
                    Ok(Ok(ws)) => {
                        tracing::info!(
                            relay = %url, via = %via, attempt,
                            ms = began.elapsed().as_millis(),
                            "relay connected"
                        );
                        // a fresh failure after a good session is loud again
                        last_reason = None;
                        ws
                    }
                    Ok(Err(e)) => {
                        shared.health.lock().await.insert(url.clone(), RelayHealth::Down);
                        let reason = e.to_string();
                        let repeat = last_reason.as_deref() == Some(reason.as_str());
                        if repeat {
                            tracing::debug!(
                                relay = %url, via = %via, attempt,
                                retry_in_s = backoff.as_secs(), error = %reason,
                                "relay connect failed again"
                            );
                        } else {
                            tracing::warn!(
                                relay = %url, via = %via, attempt,
                                retry_in_s = backoff.as_secs(), error = %reason,
                                "relay connect failed"
                            );
                            last_reason = Some(reason);
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(cap);
                        continue;
                    }
                    Err(_) => {
                        shared.health.lock().await.insert(url.clone(), RelayHealth::Down);
                        // the SAME repeat suppression as the error arm. A
                        // firewalled or blackholed relay (or a dead SOCKS
                        // port) is the most common PERMANENT failure, and it
                        // lands here — unsuppressed it warned every
                        // CONNECT_TIMEOUT + backoff forever, burying the
                        // diagnostics this reporting exists to surface.
                        const TIMED_OUT: &str = "connect timed out";
                        let repeat = last_reason.as_deref() == Some(TIMED_OUT);
                        if repeat {
                            tracing::debug!(
                                relay = %url, via = %via, attempt,
                                after_s = CONNECT_TIMEOUT.as_secs(),
                                retry_in_s = backoff.as_secs(),
                                "relay connect timed out again"
                            );
                        } else {
                            tracing::warn!(
                                relay = %url, via = %via, attempt,
                                after_s = CONNECT_TIMEOUT.as_secs(),
                                retry_in_s = backoff.as_secs(),
                                "relay connect timed out"
                            );
                            last_reason = Some(TIMED_OUT.to_string());
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(cap);
                        continue;
                    }
                }
            }
        };
        shared.health.lock().await.insert(url.clone(), RelayHealth::Up);
        let started = tokio::time::Instant::now();
        let outcome = read_session(&shared, &url, ws, &mut eose_counted).await;
        if outcome.is_break() {
            return; // consumer gone — nothing left to supervise
        }
        // reset the backoff only after a session that LIVED: a relay that
        // accepts and drops immediately (dead backend, ban) would otherwise
        // pin us at the initial delay forever, hammering it (review finding)
        if started.elapsed() >= HEALTHY_SESSION {
            backoff = initial;
        }
        shared.health.lock().await.insert(url.clone(), RelayHealth::Down);
        // a session that dies instantly, over and over, is the signature of a
        // relay that accepts then bans; say so instead of only going Down
        tracing::warn!(
            relay = %url, via = %via,
            lived_s = started.elapsed().as_secs(),
            retry_in_s = backoff.as_secs(),
            "relay session ended"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(cap);
    }
}

/// Drain one live session. `Break` = the consumer dropped the stream (stop
/// supervising); `Continue` = the SESSION died (reconnect).
async fn read_session(
    shared: &SubShared,
    url: &str,
    mut ws: RelayWs,
    eose_counted: &mut bool,
) -> std::ops::ControlFlow<()> {
    let (keepalive, idle_bound) = shared.sub_timing;
    let mut last_ping = tokio::time::Instant::now();
    let mut pending_auth: Option<nostr::EventId> = None;
    // stored events this relay has replayed for this REQ. Counted only while
    // the replay is running: after EOSE the stream is live group traffic,
    // which is bounded by the group itself, and counting it would eventually
    // cut off a perfectly healthy relay.
    let mut stored_seen: usize = 0;
    loop {
        // keepalive: a flow silently dropped by a NAT/middlebox otherwise
        // reads as a healthy-but-quiet connection until the idle bound
        if last_ping.elapsed() >= keepalive {
            if ws.ping().await.is_err() {
                return std::ops::ControlFlow::Continue(());
            }
            last_ping = tokio::time::Instant::now();
        }
        match ws.recv(keepalive.min(idle_bound)).await {
            Ok(RelayMessage::Event { event, .. }) => {
                // a relay that keeps replaying past the bound is hostile or
                // broken; either way we stop reading from it rather than
                // spending a signature verification and a dedup slot per
                // event forever (buzz B3)
                if !*eose_counted {
                    stored_seen += 1;
                    if stored_seen > shared.history_bound {
                        tracing::warn!(
                            relay = %url,
                            bound = shared.history_bound,
                            got = stored_seen,
                            "relay replayed more stored events than the bound — dropped"
                        );
                        return std::ops::ControlFlow::Break(());
                    }
                }
                // VERIFY BEFORE TRUSTING (review 2026-07-31, HIGH): the id
                // and signature arrive verbatim from an untrusted relay
                // (`RelayMessage::from_json` verifies NOTHING). Without this
                // check a single pool relay could SUPPRESS any event it can
                // name: send garbage carrying that event's id, the dedup ring
                // records the id, and the honest relay's real copy is then
                // dropped as a duplicate — multi-relay redundancy defeated.
                if event.verify().is_err() {
                    continue;
                }
                // this relay DELIVERED the event, so its cursor advances
                // (duplicates included) — but never past LOCAL NOW: the skew
                // margin bounds what we ACCEPT, never the resume point, or a
                // +1h event would eat an hour of the NIP-59 overlap
                let now = nostr::Timestamp::now().as_secs();
                let stamp = event.created_at.as_secs().min(now);
                {
                    let mut c = shared.cursors.lock().await;
                    let entry = c.entry(url.to_string()).or_insert(0);
                    *entry = (*entry).max(stamp);
                }
                let fresh = shared.dedup.lock().await.fresh(event.id);
                if fresh && shared.tx.send(event.into_owned()).await.is_err() {
                    return std::ops::ControlFlow::Break(());
                }
            }
            Ok(RelayMessage::EndOfStoredEvents(_)) => {
                tracing::debug!(relay = %url, counted = !*eose_counted, "relay sent EOSE");
                if !*eose_counted {
                    *eose_counted = true;
                    shared.eose_tx.send_modify(|n| *n += 1);
                }
            }
            Ok(RelayMessage::Auth { challenge }) => {
                // NIP-42 on the subscribe connection: answer with the
                // transport anchor and RE-PLACE the REQ under the SAME
                // subscription id (the relay refused the pre-auth one).
                // Without keys we stay connected and honestly unsynced —
                // the caller sees it via synced().
                let Some(keys) = &shared.auth_keys else { continue };
                let auth_event = nostr::RelayUrl::parse(url).ok().and_then(|relay_url| {
                    nostr::EventBuilder::auth(challenge.as_ref(), relay_url)
                        .sign_with_keys(keys)
                        .ok()
                });
                let Some(auth_event) = auth_event else { continue };
                let auth_id = auth_event.id;
                if ws.send(ClientMessage::auth(auth_event)).await.is_err() {
                    return std::ops::ControlFlow::Continue(());
                }
                pending_auth = Some(auth_id);
                let sub_id = ws.subscription().cloned().unwrap_or_else(SubscriptionId::generate);
                if place_req(shared, url, &mut ws, &sub_id).await.is_err() {
                    return std::ops::ControlFlow::Continue(());
                }
            }
            // the relay's verdict on OUR auth event: a rejection would
            // otherwise leave a connected-but-never-syncing session with no
            // reason anywhere (review finding)
            Ok(RelayMessage::Ok { event_id, status, message })
                if pending_auth == Some(event_id) =>
            {
                pending_auth = None;
                if !status {
                    tracing::warn!(relay = %url, reason = %message, "NIP-42 auth rejected");
                    return std::ops::ControlFlow::Continue(());
                }
            }
            // the relay ENDED our subscription. `auth-required:` is the
            // NIP-42 handshake asking us to authenticate — the AUTH
            // challenge follows on the same connection and the REQ is
            // re-placed there, so the session lives. Any OTHER reason
            // (rate limit, bad filter, subscription quota) leaves a live
            // but useless connection: treat it as session death so the
            // supervisor backs off and retries instead of parking until
            // the idle bound (review finding).
            Ok(RelayMessage::Closed { message, .. }) => {
                if !message.starts_with("auth-required:") {
                    tracing::warn!(relay = %url, reason = %message, "relay closed the subscription");
                    return std::ops::ControlFlow::Continue(());
                }
            }
            // OK for other events / NOTICE etc. — not subscription traffic
            Ok(other) => {
                tracing::debug!(relay = %url, msg = ?other, "relay frame (non-subscription)");
            }
            // a frame that is not NIP-01 says nothing about the connection —
            // skip it, and do NOT re-arm the keepalive clock with it (a relay
            // dribbling junk would otherwise hold the idle bound open forever)
            Err(RecvFail::Framing(e)) => {
                tracing::debug!(relay = %url, error = %e, "relay frame skipped");
            }
            // the connection is GONE: reading it again can only fail again,
            // and a ping on a dead stream still returns Ok (the bytes go into
            // the OS buffer), so pinging here is a hot loop that never
            // reconnects. End the session and let the supervisor redial.
            Err(RecvFail::Dead(e)) => {
                tracing::warn!(relay = %url, error = %e, "relay connection died");
                return std::ops::ControlFlow::Continue(());
            }
            // a timeout is just the keepalive window elapsing — ping and
            // keep reading, unless the RELAY has been silent past the idle
            // bound. The verdict must come from `idle_for` (time since the
            // last RECEIVED frame): a ping SEND "succeeds" into the OS
            // buffer even on a half-dead flow (dropped Tor circuit behind a
            // live SOCKS proxy), so a clock re-armed by our own pings never
            // expires and the node stays inbound-deaf for good (live
            // incident 2026-08-09 §2, field rerun 2026-08-17).
            Err(RecvFail::TimedOut) => {
                if ws.idle_for() >= idle_bound {
                    tracing::warn!(
                        relay = %url,
                        idle_s = ws.idle_for().as_secs(),
                        "relay sent nothing past the idle bound — reconnecting"
                    );
                    return std::ops::ControlFlow::Continue(());
                }
                if ws.ping().await.is_err() {
                    return std::ops::ControlFlow::Continue(());
                }
                last_ping = tokio::time::Instant::now();
            }
        }
    }
}

/// One probe phase's bound — a probe that hangs is worse than one that
/// fails (B4).
const PROBE_PHASE_TIMEOUT: Duration = Duration::from_secs(10);

/// The NIP-11 phase's own, SHORTER bound: it is best-effort metadata (an
/// unanswered GET keeps the conservative default budget), and a relay that
/// speaks only WS never answers the HTTP probe at all — every second here
/// is paid on every single confirm.
const PROBE_NIP11_TIMEOUT: Duration = Duration::from_secs(3);

/// The relay probe's outcome (B4). `Unreachable` and `Unusable` are
/// DIFFERENT verdicts on purpose: a relay that answered and is wrong (no
/// kind 445, no retention, tiny frame cap) can never serve the group and is
/// refused for good; one we cannot reach right now (down, or onion while
/// Tor is off) simply cannot be JUDGED — the caller decides what an
/// unverified relay may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Answered every question correctly.
    Usable,
    /// Could not be reached at all — no judgement, with the dial reason.
    Unreachable(String),
    /// Answered, and disqualified itself — with the ONE reason.
    Unusable(String),
}

/// B4 — the relay probe (`docs_archive/reviews/buzz_followups.md`): the four
/// questions a relay must answer before it may be trusted, ONE verdict,
/// one reason. Every phase is bounded.
///
/// 0. **Reachability**: one bare WS connect. Failure ends the probe as
///    [`ProbeVerdict::Unreachable`] — everything after this point is a
///    judgement about a relay that provably answered.
/// 1. **Frame cap** (best-effort NIP-11): a cap below
///    [`DEFAULT_SIZE_BUDGET`] would truncate a checkpoint serve mid-ritual —
///    refused by name. A relay without a NIP-11 answer keeps the
///    conservative default, exactly like the production publish path.
/// 2. **Accepts kind 445** — the group's entire traffic — probed with one
///    tiny throwaway under an ephemeral key. The same key answers a NIP-42
///    READ challenge on the fetch-back; publishing stays unauthenticated by
///    design (mdk_eval §5), so a relay demanding WRITE auth is refused with
///    its own reason.
/// 3. **Retention**: the event must be fetchable back moments later — a
///    relay that drops immediately could never carry a join ritual.
pub async fn probe_relay(dialer: &Dialer, url: &str) -> ProbeVerdict {
    match probe_relay_inner(dialer, url).await {
        Ok(()) => ProbeVerdict::Usable,
        Err(v) => v,
    }
}

async fn probe_relay_inner(dialer: &Dialer, url: &str) -> Result<(), ProbeVerdict> {
    // phase 0 — reachability: everything after this is a judgement
    match tokio::time::timeout(PROBE_PHASE_TIMEOUT, crate::relay_ws::RelayWs::connect(dialer, url))
        .await
    {
        Err(_) => {
            return Err(ProbeVerdict::Unreachable("the connect timed out".to_string()));
        }
        Ok(Err(e)) => return Err(ProbeVerdict::Unreachable(e.to_string())),
        Ok(Ok(_ws)) => {}
    }
    // phase 1 — the frame cap, best-effort (an Err keeps the default budget)
    if let Ok(Ok(Some(cap))) =
        tokio::time::timeout(PROBE_NIP11_TIMEOUT, probe_nip11_max_message(dialer, url)).await
    {
        if cap < DEFAULT_SIZE_BUDGET {
            return Err(ProbeVerdict::Unusable(format!(
                "frame cap {cap} B is below the {DEFAULT_SIZE_BUDGET} B the group needs"
            )));
        }
    }
    // phase 2 — one throwaway kind-445 under an ephemeral key
    let keys = nostr::Keys::generate();
    let h = format!("{:x}", md5ish(&keys));
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(crate::kinds::KIND_GROUP), "cHJvYmU")
        .tag(nostr::Tag::parse(["h", h.as_str()]).map_err(|e| {
            ProbeVerdict::Unusable(format!("building the probe tag: {e}"))
        })?)
        .sign_with_keys(&keys)
        .map_err(|e| ProbeVerdict::Unusable(format!("signing the probe event: {e}")))?;
    let runtime = RelayRuntime::new(dialer.clone(), vec![url.to_string()])
        .with_auth_keys(Some(keys))
        .with_backoff(Duration::from_millis(200), Duration::from_secs(1));
    // phase 0 PROVED reachability, so a publish that comes back refused —
    // whether as a report or as a total-failure error — is the relay's own
    // JUDGEMENT about kind 445, not an outage
    let report = tokio::time::timeout(PROBE_PHASE_TIMEOUT, runtime.publish(&event))
        .await
        .map_err(|_| ProbeVerdict::Unreachable("the publish probe timed out".to_string()))
        .and_then(|r| {
            r.map_err(|e| ProbeVerdict::Unusable(format!("does not accept kind 445: {e}")))
        })?;
    if report.accepted.is_empty() {
        let reason = report
            .failed
            .first()
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| "refused the probe event".to_string());
        return Err(ProbeVerdict::Unusable(format!("does not accept kind 445: {reason}")));
    }
    // phase 3 — retention: the event must come back
    let mut sub = runtime
        .subscribe(Filter::new().id(event.id))
        .await
        .map_err(|e| ProbeVerdict::Unreachable(format!("subscribe: {e}")))?;
    let got = tokio::time::timeout(PROBE_PHASE_TIMEOUT, async {
        loop {
            match sub.recv(Duration::from_millis(500)).await {
                Some(ev) if ev.id == event.id => break true,
                Some(_) => continue,
                None if sub.synced(Duration::from_millis(1)).await => {
                    // EOSE without our event: ask once more after a beat —
                    // some relays index asynchronously
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                None => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    if !got {
        return Err(ProbeVerdict::Unusable(
            "does not retain events - a join ritual could never complete on it".to_string(),
        ));
    }
    Ok(())
}

/// A cheap deterministic-per-key probe tag (NOT a hash function — just 32
/// hex chars derived from the ephemeral pubkey so concurrent probes never
/// share a tag).
fn md5ish(keys: &nostr::Keys) -> u128 {
    let pk = keys.public_key().to_bytes();
    let mut v = [0u8; 16];
    v.copy_from_slice(&pk[..16]);
    u128::from_le_bytes(v)
}

/// Probe a relay's NIP-11 information document — one HTTP/1.1 GET with the
/// `application/nostr+json` Accept header over the SAME fail-closed dial
/// path the WS connection uses (never a second HTTP client stack; the
/// response head is parsed with `httparse`, already in the tree via
/// tungstenite). Returns the advertised `max_message_length`; `Ok(None)` =
/// the relay answered but names no cap. On `Err` the caller keeps the
/// conservative [`DEFAULT_SIZE_BUDGET`]. A `wss://` probe rides the same
/// rustls-rustcrypto TLS over the same dialed stream as the WS connection.
pub async fn probe_nip11_max_message(
    dialer: &Dialer,
    ws_url: &str,
) -> Result<Option<u64>, NetError> {
    let parsed = url::Url::parse(ws_url)
        .map_err(|e| NetError::Framing(format!("relay url {ws_url}: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| NetError::Framing(format!("relay url {ws_url}: no host")))?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| NetError::Framing(format!("relay url {ws_url}: no port")))?;
    let mut stream = dial_maybe_tls(
        &crate::relay_ws::dialer_for(dialer, ws_url),
        &host,
        port,
        parsed.scheme() == "wss",
    )
    .await?;
    let request = format!(
        "GET / HTTP/1.1\r\nhost: {host}\r\naccept: application/nostr+json\r\nconnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| NetError::Unreachable(format!("nip11 {ws_url}: {e}")))?;
    // read to EOF, bounded
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| NetError::Unreachable(format!("nip11 {ws_url}: timed out")))?
            .map_err(|e| NetError::Unreachable(format!("nip11 {ws_url}: {e}")))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > NIP11_MAX_RESPONSE {
            return Err(NetError::Framing(format!("nip11 {ws_url}: response too large")));
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let body_start = match response
        .parse(&raw)
        .map_err(|e| NetError::Framing(format!("nip11 {ws_url}: bad http: {e}")))?
    {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => {
            return Err(NetError::Framing(format!("nip11 {ws_url}: truncated response")))
        }
    };
    if response.code != Some(200) {
        return Err(NetError::Unreachable(format!(
            "nip11 {ws_url}: http {:?}",
            response.code
        )));
    }
    let doc: serde_json::Value = serde_json::from_slice(&raw[body_start..])
        .map_err(|e| NetError::Framing(format!("nip11 {ws_url}: bad json: {e}")))?;
    Ok(doc
        .pointer("/limitation/max_message_length")
        .and_then(serde_json::Value::as_u64))
}

/// A PERSISTENT, UNAUTHENTICATED publish channel (live incident
/// 2026-08-09 §3): one long-lived connection per relay, reused across
/// publishes, redialed only when it broke — the per-frame fresh dial
/// (Tor circuit + WS + TLS ≈ 2 s) is what let resend rounds starve fresh
/// sends. Deliberately NOT the NIP-42-authenticated subscribe session:
/// an authenticated publish channel would link every ephemeral-key event
/// to the member (§7.5). A relay can already group the frames by their
/// `h` tag; what this channel must never leak is WHO.
///
/// Concurrency: publishers (outbox, ack task, file plane) serialize PER
/// RELAY — the slot lock is held across one send→OK round-trip so
/// interleaved OKs can never be attributed to the wrong event; distinct
/// relays still publish in parallel.
#[derive(Clone)]
pub struct PublishPool {
    dialer: Dialer,
    /// (url, its connection slot) — urls are fixed at construction; a pool
    /// change builds a new channel and thereby a new pool.
    conns: Arc<Vec<(String, tokio::sync::Mutex<Option<RelayWs>>)>>,
    size_budget: Option<u64>,
}

// manual: RelayWs holds no secrets, but Debug on a live socket is noise
impl std::fmt::Debug for PublishPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishPool")
            .field("relays", &self.conns.len())
            .finish_non_exhaustive()
    }
}

impl PublishPool {
    /// A pool over `urls`. Connections are dialed lazily on the first
    /// publish and kept until they break.
    pub fn new(dialer: Dialer, urls: Vec<String>) -> Self {
        if !urls.is_empty() {
            tracing::info!(
                relays = urls.len(),
                via = %dialer.route(),
                "persistent publish channel"
            );
        }
        Self {
            dialer,
            conns: Arc::new(
                urls.into_iter()
                    .map(|u| (u, tokio::sync::Mutex::new(None)))
                    .collect(),
            ),
            size_budget: None,
        }
    }

    /// Publish one signed event to EVERY relay concurrently over the kept
    /// connections — the same ≥1-OK semantics and size gate as
    /// [`RelayRuntime::publish`].
    pub async fn publish(&self, event: &Event) -> Result<PublishReport, NetError> {
        if self.conns.is_empty() {
            return Err(NetError::Unreachable(
                "no dialable relay — the pool is empty or gated".into(),
            ));
        }
        let budget = self.size_budget.unwrap_or(DEFAULT_SIZE_BUDGET);
        let size = u64::try_from(ClientMessage::event(event.clone()).as_json().len())
            .unwrap_or(u64::MAX);
        if size > budget {
            return Err(NetError::Framing(format!(
                "event of {size} bytes exceeds the smallest relay cap ({budget} bytes) — refused before publish"
            )));
        }
        let attempts = self.conns.iter().map(|(url, slot)| {
            let dialer = self.dialer.clone();
            let event = event.clone();
            async move {
                let outcome = Self::publish_pooled(&dialer, url, slot, &event).await;
                (url.clone(), outcome)
            }
        });
        let mut report = PublishReport::default();
        for (url, outcome) in futures_util::future::join_all(attempts).await {
            match outcome {
                Ok(()) => report.accepted.push(url),
                Err(e) => report.failed.push((url, e.to_string())),
            }
        }
        finish_publish_report(report)
    }

    /// One relay: try the kept connection; if the TRANSPORT broke (send or
    /// recv failed — an idle-closed socket looks like this), drop it and
    /// dial fresh exactly once. A relay's VERDICT (refusal, auth demand)
    /// is a real answer over a live socket — it never redials.
    async fn publish_pooled(
        dialer: &Dialer,
        url: &str,
        slot: &tokio::sync::Mutex<Option<RelayWs>>,
        event: &Event,
    ) -> Result<(), NetError> {
        let mut guard = slot.lock().await;
        if let Some(ws) = guard.as_mut() {
            match tokio::time::timeout(PUBLISH_TIMEOUT, send_and_await_ok(ws, event)).await {
                Ok(Ok(verdict)) => return verdict,
                // transport broke or the OK never came — the socket is not
                // trustworthy any more either way
                Ok(Err(_)) | Err(_) => {
                    *guard = None;
                }
            }
        }
        let attempt = async {
            let mut ws = RelayWs::connect(dialer, url).await?;
            let verdict = send_and_await_ok(&mut ws, event)
                .await
                .map_err(RecvFail::into_error)?;
            Ok::<(RelayWs, Result<(), NetError>), NetError>((ws, verdict))
        };
        let (ws, verdict) = tokio::time::timeout(PUBLISH_TIMEOUT, attempt)
            .await
            .unwrap_or_else(|_| Err(NetError::Unreachable("publish timed out".into())))?;
        // keep the live socket — a refusal verdict still proves it works
        *guard = Some(ws);
        verdict
    }
}

/// Send EVENT and await the OK for THIS event id on an already-connected
/// socket. `Err` = the transport broke (caller drops the socket); `Ok`
/// carries the relay's verdict (accepted / refused — a live answer).
async fn send_and_await_ok(
    ws: &mut RelayWs,
    event: &Event,
) -> Result<Result<(), NetError>, RecvFail> {
    if let Err(e) = ws.send(ClientMessage::event(event.clone())).await {
        return Err(RecvFail::Dead(e));
    }
    loop {
        match ws.recv(PUBLISH_TIMEOUT).await? {
            RelayMessage::Ok { event_id, status, message } if event_id == event.id => {
                return Ok(if counts_as_published(status, &message) {
                    Ok(())
                } else if message.starts_with("auth-required:") {
                    // deliberate: the publish path NEVER authenticates —
                    // an authed publish channel would link every
                    // ephemeral-key event to the member (§7.5)
                    Err(NetError::Unreachable(format!(
                        "relay requires AUTH to publish — refused to link the publish key: {message}"
                    )))
                } else {
                    Err(NetError::Unreachable(format!("relay refused: {message}")))
                });
            }
            // frames for other events / notices are not our OK
            _ => {}
        }
    }
}

/// The shared ≥1-OK reduction: log per-relay failures, fail typed when
/// NOTHING accepted, warn on a partial landing.
fn finish_publish_report(report: PublishReport) -> Result<PublishReport, NetError> {
    for (url, e) in &report.failed {
        tracing::warn!(relay = %url, error = %e, "publish rejected");
    }
    if report.accepted.is_empty() {
        let mut reasons: Vec<&str> = Vec::new();
        for (_, e) in &report.failed {
            let r = e.as_str();
            if !reasons.contains(&r) {
                reasons.push(r);
            }
        }
        return Err(NetError::Unreachable(format!(
            "no relay accepted the event ({} tried): {}",
            report.failed.len(),
            reasons.join("; ")
        )));
    }
    if !report.failed.is_empty() {
        tracing::warn!(
            accepted = report.accepted.len(),
            failed = report.failed.len(),
            "publish landed on part of the pool"
        );
    }
    Ok(report)
}

/// One relay, one publish: connect, EVENT, await the OK for THIS event id.
async fn publish_one(dialer: &Dialer, url: &str, event: &Event) -> Result<(), NetError> {
    let mut ws = RelayWs::connect(dialer, url).await?;
    ws.send(ClientMessage::event(event.clone())).await?;
    let verdict = loop {
        match ws.recv(PUBLISH_TIMEOUT).await.map_err(RecvFail::into_error)? {
            RelayMessage::Ok { event_id, status, message } if event_id == event.id => {
                break if counts_as_published(status, &message) {
                    Ok(())
                } else if message.starts_with("auth-required:") {
                    // deliberate: the publish path NEVER authenticates —
                    // an authed publish channel would link every
                    // ephemeral-key event to the member (§7.5). Loud, so
                    // the operator can pick a different relay.
                    Err(NetError::Unreachable(format!(
                        "relay requires AUTH to publish — refused to link the publish key: {message}"
                    )))
                } else {
                    Err(NetError::Unreachable(format!("relay refused: {message}")))
                };
            }
            // frames for other events / notices are not our OK
            _ => {}
        }
    };
    ws.close().await;
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `duplicate:` rule pinned as a pure function, independent of any
    /// relay implementation's actual duplicate behavior.
    #[test]
    fn duplicate_ok_false_counts_as_published() {
        assert!(counts_as_published(true, ""));
        assert!(counts_as_published(false, "duplicate: already have this event"));
        assert!(!counts_as_published(false, "blocked: spam"));
        assert!(!counts_as_published(false, "invalid: bad sig"));
        // the prefix is exact — a message merely CONTAINING the word is a refusal
        assert!(!counts_as_published(false, "error: duplicate elsewhere"));
    }
}
