// SPDX-License-Identifier: GPL-3.0-or-later

//! **N5.2 — the kind-445 group runtime.**
//!
//! The Nostr twin of [`crate::supervisor::spawn`], and a NEW function rather
//! than a parameterization of it: that one is bound to `T: Transport` and
//! `Vec<PeerLink>`, and a relay group has neither. What it DOES reuse is the
//! engine seam verbatim — the same `OutboxLog` / `StateStore` / `EngineSink`
//! triple and the same wakeup watch, so the engine wires a Nostr workspace the
//! way it already wires a mesh one.
//!
//! Two tasks, and their shutdown is deliberately asymmetric:
//!
//! - the **outbox** reads the log from the broadcast cursor, MLS-frames each
//!   own envelope and publishes it as one 445 that reaches every member. It is
//!   DRAINED on shutdown, never aborted — aborting between seal and relay-OK
//!   drops the frame silently, which is the failure the delivery guarantee
//!   exists to prevent;
//! - the **inbox** is pure inbound and IS aborted, the sanctioned abort.
//!
//! What N5.2 does not do: acks, rewind, resend. The cursor and the floor
//! helpers are shaped so N5.3 lights them up by SENDING rather than by
//! reshaping anything.

use std::time::Duration;

use molt_core::{MemberId, TransportState};
use tokio::sync::watch;

use crate::ritual_net::{GroupChannel, GroupRecv};
use crate::supervisor::{EngineSink, MlsChannel, MlsDecode, OutboxLog, StateStore};

/// How long the inbox waits per receive slice — short enough that a stopped
/// runtime dies promptly, long enough to stay quiet.
const RECV_SLICE: Duration = Duration::from_secs(5);

/// Publish attempts before the outbox gives up on an envelope for this round
/// and holds the cursor where it is.
const PUBLISH_ATTEMPTS: u32 = 3;

/// First stall backoff: how long an unacked tail waits before it is re-offered.
const RESEND_AFTER_SECS: u64 = 10;

/// Ceiling of the doubling stall backoff.
const RESEND_MAX_BACKOFF_SECS: u64 = 600;

/// Fruitless rewinds before the stall is reported loudly. Not a stop — the
/// resends keep going at the cap.
const RESEND_GIVEUP_ROUNDS: u32 = 8;

/// Resend rounds allowed per hour.
///
/// A broadcast resend costs one publish per relay and every member re-reads
/// it, so an unbounded stall loop is an amplifier pointed at the whole
/// republic. PERSISTED with the cursor, unlike the in-memory backoff: a crash
/// loop must not buy itself a fresh budget on every start.
const RESEND_ROUNDS_PER_HOUR: u32 = 12;

/// One node's group-runtime configuration.
#[derive(Debug, Clone)]
pub struct GroupNetConfig {
    /// This node's own handle — the outbox publishes exactly its own events.
    pub member: MemberId,
    /// The roster minus self: whose floors the guarantee tracks (N5.3).
    pub members: Vec<MemberId>,
    /// First retry delay after a failed publish.
    pub retry_base: Duration,
    /// Retry ceiling.
    pub retry_cap: Duration,
}

impl GroupNetConfig {
    /// Production timings.
    #[must_use]
    pub fn new(member: MemberId, members: Vec<MemberId>) -> Self {
        Self {
            member,
            members,
            retry_base: Duration::from_secs(1),
            retry_cap: Duration::from_secs(120),
        }
    }

    /// Millisecond timings for tests.
    #[must_use]
    pub fn fast(member: MemberId, members: Vec<MemberId>) -> Self {
        Self {
            member,
            members,
            retry_base: Duration::from_millis(20),
            retry_cap: Duration::from_millis(200),
        }
    }
}

/// What the runtime is doing, for the operator surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupHealth {
    /// The 445 subscription is placed and readable.
    pub subscribed: bool,
    /// Why the channel cannot be heard, while it cannot be.
    pub deaf: Option<String>,
    /// Frames no held exporter secret could open — older than the ring, gone
    /// for good. Counted rather than folded into a plain skip: past the ring
    /// the loss is permanent, and silence about it is the dishonesty §10.4
    /// (G4) names.
    pub opaque_frames: u64,
}

/// A running group runtime.
pub struct GroupHandle {
    /// A `watch`, NOT a `Notify`: the stop must LATCH. `notify_waiters` wakes
    /// only tasks parked at that instant, so a handle dropped in the same
    /// synchronous stretch that built it (the recovery path did) signalled
    /// into the void and left an orphaned outbox publishing every frame twice.
    stop: watch::Sender<bool>,
    /// The latest claim sheet awaiting publication. FULL STATE, so a newer
    /// sheet supersedes an unsent one by construction — no queue to drain and
    /// no coalescing logic to get wrong.
    acks: watch::Sender<Option<crate::group_ack::GroupAck>>,
    outbox: Option<tokio::task::JoinHandle<()>>,
    inbox: Option<tokio::task::JoinHandle<()>>,
    ack: Option<tokio::task::JoinHandle<()>>,
}

impl GroupHandle {
    /// Signal both tasks, then AWAIT the outbox so an in-flight publish
    /// finishes. Drain-don't-abort applies to the outbox only; the inbox is
    /// pure inbound and is aborted.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(inbox) = self.inbox.take() {
            inbox.abort();
        }
        if let Some(outbox) = self.outbox.take() {
            let _ = outbox.await;
        }
        // the ack task is outbound too — drained, not aborted
        if let Some(ack) = self.ack.take() {
            let _ = ack.await;
        }
    }

    /// Queue the latest claim sheet for publication.
    ///
    /// Synchronous and non-blocking: the engine actor never awaits, and the
    /// sheet is full state, so dropping an unsent one in favour of a newer one
    /// loses nothing.
    pub fn publish_ack(&self, ack: crate::group_ack::GroupAck) {
        let _ = self.acks.send(Some(ack));
    }
}

impl Drop for GroupHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(inbox) = self.inbox.take() {
            inbox.abort();
        }
    }
}

/// `Some(min acked_floor)` over the configured members that have ever acked;
/// `None` when nobody has.
///
/// This is §4.1's "resends publish ONCE at the min acked_floor across
/// members" — one publish, and every receiver's own `AcceptedWindow` dedups
/// what it already holds. In N5.2 nobody acks, so it is `None` and the cursor
/// keeps exactly plain-cursor behaviour.
#[must_use]
pub(crate) fn group_floor(state: &TransportState, cfg: &GroupNetConfig) -> Option<u64> {
    cfg.members
        .iter()
        .filter_map(|m| state.outbound.get(m))
        .filter(|c| c.ack_seen)
        .map(|c| c.acked_floor)
        .min()
}

/// Rewind the broadcast cursor to the proven floor.
///
/// The twin of `supervisor::rewind_unacked`, which walks `outbound` with
/// PER-PEER semantics and must never run here: on a broadcast channel there is
/// one publish position, and rewinding it per peer would republish the tail
/// once per member.
pub(crate) fn rewind_group(state: &mut TransportState, cfg: &GroupNetConfig) {
    let Some(floor) = group_floor(state, cfg) else {
        return; // nobody has ever acked — nothing is proven, so nothing rewinds
    };
    let mut cur = state.group.unwrap_or_default();
    if floor < cur.log_seq {
        cur.log_seq = floor;
        state.group = Some(cur);
    }
}

/// Start a node's kind-445 group runtime.
#[allow(clippy::too_many_arguments)]
pub fn spawn_group<L, S, K>(
    channel: GroupChannel,
    mls: MlsChannel,
    cfg: GroupNetConfig,
    log: L,
    store: S,
    sink: K,
    wakeup: watch::Receiver<u64>,
    health: watch::Sender<GroupHealth>,
) -> GroupHandle
where
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let (stop, stop_rx) = watch::channel(false);
    let (log2, store2) = (log.clone(), store.clone());
    let (acks, acks_rx) = watch::channel(None);
    // the ack-wake (§3.1a): an applied claim sheet can create the outbox's
    // own-ackable tail out of thin air — a rejoiner's FIRST sheet latches
    // evidence at floor 0 — and the log-append wakeup by definition never
    // fires for it. `Notify` over a channel because it has no closed state
    // (a deaf inbox must not stop the outbox), and `notify_one` over
    // `notify_waiters` because it LATCHES: a sheet applied while the outbox
    // is mid-pass must not be missed.
    let ack_wake = std::sync::Arc::new(tokio::sync::Notify::new());
    // a THIRD task: an ack must not queue behind `publish_with_backoff`, whose
    // chain runs three attempts up to the retry cap. Head-of-line blocking the
    // guarantee's own feedback behind the traffic it is meant to prove is how
    // a stall becomes permanent.
    let ack = tokio::spawn(ack_loop(
        channel.clone(),
        mls.clone(),
        acks_rx,
        stop_rx.clone(),
    ));
    let outbox = tokio::spawn(outbox_loop(
        channel.clone(),
        mls.clone(),
        cfg.clone(),
        log,
        store,
        sink.clone(),
        wakeup,
        ack_wake.clone(),
        stop_rx.clone(),
    ));
    let inbox = tokio::spawn(inbox_loop(
        channel,
        mls,
        log2,
        store2,
        cfg.member.clone(),
        sink,
        health,
        ack_wake,
        stop_rx,
    ));
    GroupHandle {
        stop,
        acks,
        outbox: Some(outbox),
        inbox: Some(inbox),
        ack: Some(ack),
    }
}

/// Apply one peer's claim sheet to our own outbound bookkeeping.
///
/// Two rules, and both exist because getting them wrong turns a small frame
/// into a republic-wide republish:
///
/// 1. a sheet that says nothing ABOUT US proves nothing — it must not latch
///    `ack_seen`, because a "proven" floor of zero would then make the
///    outbox rewind to the start of the log on a peer's mere silence. The
///    test of that rule is `window_for(me)`, and nothing else: a sheet that
///    DOES speak about us is evidence the peer is listening, whatever floor
///    it implies. (Requiring a non-zero floor as well is what stranded a
///    REJOINER, whose honest floor is 0 — it entered the broadcast
///    mid-stream and never saw the early events — so no rewind ever
///    republished the frames its catch-up was waiting on: §3.1a.)
/// 2. the window we receive from `from` is in OUR seq space, because it
///    describes OUR events. The window we SEND about `from` is in THEIRS.
///    The two directions are mirror images and the inversion compiles.
async fn apply_group_ack<L: OutboxLog, S: StateStore>(
    log: &L,
    store: &S,
    me: &MemberId,
    from: &MemberId,
    ack: &crate::group_ack::GroupAck,
) {
    let Some(window) = ack.window_for(me) else {
        return; // silence, not a claim of zero
    };
    let mut state = store.load().await;
    let old = state
        .outbound
        .get(from)
        .map_or(0, |c| c.acked_floor);
    let envs = log.read_from(old.saturating_add(1)).await;
    let floor = crate::supervisor::advance_acked_floor(me, &envs, window, old);
    tracing::debug!(
        me = %me,
        %from,
        old,
        floor,
        window_high = window.high,
        "group ack applied"
    );
    // a peer that spoke about our events AND still trails our publish
    // cursor is a proven lagging LISTENER: latch the heal evidence — it
    // buys the outbox one budget-free round when the hourly allowance is
    // already burned (a recovered incarnation must not wait out the
    // refill: live incident 2026-08-09 §2). A caught-up peer's sheet
    // proves nothing to heal, and saving is conditional — most sheets in
    // steady state change nothing worth a state write.
    let mut changed = false;
    let mut group = state.group.unwrap_or_default();
    if floor < group.log_seq && !group.heal_evidence {
        group.heal_evidence = true;
        changed = true;
    }
    state.group = Some(group);
    if floor > old {
        let cursor = state.outbound.entry(from.clone()).or_default();
        cursor.acked_floor = floor;
        cursor.ack_seen = true;
        changed = true;
    } else {
        // no progress — but the peer is listening. Latch without moving the
        // floor; the floor stays honest (0 = nothing proven) and the rewind
        // can now reach back to what this peer is actually missing.
        let cursor = state.outbound.entry(from.clone()).or_default();
        if !cursor.ack_seen {
            cursor.ack_seen = true;
            changed = true;
        }
    }
    if changed {
        store.save(state).await;
    }
}

/// Publish claim sheets as they are handed over.
async fn ack_loop(
    channel: GroupChannel,
    mls: MlsChannel,
    mut rx: watch::Receiver<Option<crate::group_ack::GroupAck>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            r = rx.changed() => {
                if r.is_err() {
                    return;
                }
            }
            _ = stop.changed() => return,
        }
        let Some(ack) = rx.borrow_and_update().clone() else {
            continue;
        };
        let Some(frame) = mls.group_control_frame(&ack.to_frame()) else {
            tracing::error!("framing a group ack failed — skipped");
            continue;
        };
        // ONE publish, no retry chain: the sheet is full state, so the next
        // one supersedes this one anyway and a retry would only duplicate
        // what the next beat already carries.
        if let Err(e) = channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
            tracing::warn!(error = %e, "publishing a group ack failed");
        }
    }
}

/// Why a frame did not go out.
#[derive(Debug)]
enum PublishStall {
    /// Every attempt failed, but the cause can clear by itself — a relay is
    /// down, a dial timed out, the pool is momentarily empty. Retrying is
    /// the right answer, and the outbox holds its cursor meanwhile.
    Transient,
    /// The frame can **never** be published as it stands: it was refused
    /// locally, before any relay was contacted, by a check whose verdict does
    /// not depend on anything outside this process. Retrying is futile by
    /// construction, so the honest move is to report the real reason at once.
    Permanent(String),
    /// The runtime was told to stop.
    Stopped,
}

/// Is this refusal one that retrying cannot change?
///
/// [`NetError::Framing`] is documented as "always a local bug, never a remote
/// condition" — the over-budget publish gate raises it, and so does a
/// malformed `h` tag. [`NetError::Crypto`] (sealing, signing) is local for
/// the same reason. Everything else describes the world outside this process
/// and may well look different in a second.
fn permanent_refusal(e: &crate::NetError) -> Option<String> {
    match e {
        crate::NetError::Framing(_) | crate::NetError::Crypto(_) => Some(e.to_string()),
        _ => None,
    }
}

/// Publish one framed envelope, retrying a relay-side refusal on a backoff.
async fn publish_with_backoff(
    channel: &GroupChannel,
    frame: &crate::supervisor::GroupFrame,
    cfg: &GroupNetConfig,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), PublishStall> {
    let mut delay = cfg.retry_base;
    for _ in 0..PUBLISH_ATTEMPTS {
        // the SAME bytes across relay retries inside one attempt chain: a relay
        // NAK is not a peer rejection, and re-encrypting would burn a ratchet
        // generation for nothing. A REWIND re-frames instead (N5.3).
        match channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
            Ok((_stamp, report)) if !report.accepted.is_empty() => return Ok(()),
            Ok((_stamp, _report)) => {
                tracing::warn!("no relay accepted a group frame — retrying");
            }
            Err(e) => {
                if let Some(why) = permanent_refusal(&e) {
                    // no backoff: this answer is the same in a second and in
                    // an hour, and the delay only postpones the diagnosis
                    return Err(PublishStall::Permanent(why));
                }
                tracing::warn!(error = %e, "publishing a group frame failed");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.changed() => return Err(PublishStall::Stopped),
        }
        delay = (delay * 2).min(cfg.retry_cap);
    }
    Err(PublishStall::Transient)
}

/// Read the log from the broadcast cursor and publish this node's own events.
#[allow(clippy::too_many_arguments)]
async fn outbox_loop<L, S, K>(
    channel: GroupChannel,
    mls: MlsChannel,
    cfg: GroupNetConfig,
    log: L,
    store: S,
    sink: K,
    mut wakeup: watch::Receiver<u64>,
    ack_wake: std::sync::Arc<tokio::sync::Notify>,
    mut stop: watch::Receiver<bool>,
) where
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    // the stall clock: when the proven floor last moved, and how hard we are
    // currently backing off
    let mut last_floor: Option<u64> = None;
    let mut backoff_secs = RESEND_AFTER_SECS;
    let mut rounds_without_progress: u32 = 0;
    let mut stall_reported = false;
    let mut stalled_since: Option<tokio::time::Instant> = None;
    loop {
        // NO rewind here. It sat at the loop top through N5.2, where it was
        // inert because nothing ever acked — N5.3's acks made it live, and a
        // rewind on every wakeup re-publishes the whole unacked tail on every
        // appended envelope, with the hourly budget gating only the persisted
        // write downstream. The budget must ration the PUBLISH, so the rewind
        // happens exactly once per granted round, below.
        let state = store.load().await;
        let start = state.group.unwrap_or_default().log_seq;
        let batch = log.read_from(start + 1).await;
        let mut published_through = start;
        for env in batch {
            // AUTHOR, not `crosses_wire` — the same filter the queue outbox
            // uses. It matters more than it looks: the genesis `Founded` is
            // self-authored on every node, so seq 1 must ride the wire or the
            // receiver never accepts it and every later own event parks on
            // `prev_seq = 1` until the give-up timer.
            if env.by != cfg.member {
                published_through = env.seq;
                continue;
            }
            let Some(frame) = mls.group_frame(&env) else {
                // a LOCAL framing error is not resendable — skipping it is the
                // only way forward, and it is loud
                tracing::error!(seq = env.seq, "MLS-framing a group frame failed — skipped");
                published_through = env.seq;
                continue;
            };
            match publish_with_backoff(&channel, &frame, &cfg, &mut stop).await {
                Ok(()) => {
                    tracing::debug!(me = %cfg.member, seq = env.seq, "group frame published");
                    published_through = env.seq;
                }
                // hold the cursor exactly here in EVERY failure case: nothing
                // recovers a skipped envelope, so advancing past an
                // unpublished one would lose it permanently. What differs is
                // only what the operator is told.
                Err(stall) => {
                    // a stop is a shutdown, not an outage — crying send_failed
                    // here painted a red pill onto every clean close that
                    // happened to catch a publish mid-backoff
                    if matches!(stall, PublishStall::Stopped) {
                        return;
                    }
                    let reason = match stall {
                        // a wedge, not an outage: the node writes nothing more
                        // until this envelope can go out, across restarts.
                        // Loud, and naming the real cause — the propose-time
                        // size gate (`molt-engine/proposals.rs`) is what keeps
                        // this unreachable in the first place
                        PublishStall::Permanent(why) => {
                            tracing::error!(
                                seq = env.seq,
                                reason = %why,
                                "a group frame can never be published — the outbox holds here"
                            );
                            why
                        }
                        _ => "no relay accepted the frame".to_string(),
                    };
                    sink.send_failed(&cfg.member, &reason).await;
                    stalled_since = None;
                    break;
                }
            }
        }
        if published_through > start {
            let mut state = store.load().await;
            let mut cur = state.group.unwrap_or_default();
            if published_through > cur.log_seq {
                cur.log_seq = published_through;
                state.group = Some(cur);
                store.save(state).await;
            }
        }
        // --- the stall clock -------------------------------------------------
        let state = store.load().await;
        let floor = group_floor(&state, &cfg);
        let cursor = state.group.unwrap_or_default().log_seq;
        // `None < Some(_)` for Options, which is what is wanted here: the
        // first real floor de-escalates from "nothing proven", and a floor
        // that REGRESSES (a peer whose window persistence lost a beat) does
        // not — the guarantee only ever counts forward progress as progress.
        if floor > last_floor {
            // the republic confirmed progress — de-escalate entirely
            last_floor = floor;
            backoff_secs = RESEND_AFTER_SECS;
            rounds_without_progress = 0;
            if stall_reported {
                stall_reported = false;
                // clear it on the members it was raised against — naming this
                // node would have the surface blame the operator's own seat
                // for a peer that stopped acking
                for m in &cfg.members {
                    sink.send_ok(m).await;
                }
            }
        }
        // resend ONLY when somebody has proven something AND we have an own
        // ackable envelope above that floor.
        //
        // "cursor > floor" alone is not that: the broadcast cursor walks over
        // FOREIGN envelopes too, and a member that mostly listens would sit
        // permanently above a frozen floor — reporting "deliveries keep going
        // unacknowledged" on a perfectly healthy republic. The mesh learned
        // this as its own-ackable span guard; this is the same rule.
        let tail = match floor {
            Some(f) if cursor > f => log
                .read_from(f + 1)
                .await
                .iter()
                .any(|e| crate::supervisor::own_ackable(&cfg.member, e)),
            _ => false,
        };
        if !tail {
            tracing::debug!(
                me = %cfg.member,
                ?floor,
                cursor,
                "outbox idle — no own-ackable tail; sleeping on wakeup"
            );
            stalled_since = None;
            tokio::select! {
                r = wakeup.changed() => {
                    if r.is_err() {
                        return;
                    }
                }
                // an applied claim sheet can flip `tail` without any append —
                // re-evaluate (§3.1a: the rejoiner's first sheet at floor 0)
                () = ack_wake.notified() => {}
                _ = stop.changed() => return,
            }
            continue;
        }
        tracing::debug!(
            me = %cfg.member,
            ?floor,
            cursor,
            backoff_secs,
            armed = stalled_since.is_some(),
            "outbox tail unacked — stall clock running"
        );
        // ANCHORED, not recomputed per iteration: traffic must not starve the
        // timer. The mesh calls this `stalled_since` and the distinction is
        // the whole point — a chatty republic would otherwise never resend.
        let since = *stalled_since.get_or_insert_with(tokio::time::Instant::now);
        let deadline = since + Duration::from_secs(backoff_secs);
        let fired = tokio::select! {
            r = wakeup.changed() => {
                if r.is_err() {
                    return;
                }
                false // new work first; the stall clock keeps running
            }
            // a floor advance de-escalates at the loop top; the anchored
            // clock is kept by `get_or_insert`, so a sheet can never starve it
            () = ack_wake.notified() => false,
            _ = stop.changed() => return,
            () = tokio::time::sleep_until(deadline) => true,
        };
        if !fired {
            continue;
        }
        let mut state = store.load().await;
        // the floor may have moved during a sleep of up to ten minutes;
        // rewinding to a stale snapshot would re-offer what has since been
        // confirmed
        let Some(f) = group_floor(&state, &cfg) else {
            stalled_since = None;
            continue;
        };
        let mut cur = state.group.unwrap_or_default();
        if !consume_resend_round(&mut cur, wall_secs()) {
            // spent — but fresh evidence of a lagging listener (a claim
            // sheet since the last round) buys the one heal round
            if consume_heal_round(&mut cur, wall_secs()) {
                tracing::info!(
                    floor = f,
                    "budget spent, but a listener proved itself lagging — heal round"
                );
            } else {
                tracing::warn!(
                    floor = f,
                    "the resend budget for this hour is spent — holding the tail"
                );
                backoff_secs = backoff_secs.saturating_mul(2).min(RESEND_MAX_BACKOFF_SECS);
                continue;
            }
        }
        // rewind to the proven floor and re-offer the tail. Every group frame
        // is a FRESH encryption (`group_frame` bypasses the fan-out cache), so
        // there is no stale-ciphertext eviction to do and no resend epoch to
        // bump — a relay-level duplicate dedups at each receiver's
        // AcceptedWindow.
        //
        // Through `rewind_group`, not by assignment: one definition of "pull
        // the broadcast cursor back to what is proven", and it is the one the
        // unit tests pin.
        state.group = Some(cur); // the consumed budget
        rewind_group(&mut state, &cfg);
        store.save(state).await;
        rounds_without_progress = rounds_without_progress.saturating_add(1);
        tracing::warn!(
            floor = f,
            cursor,
            attempt = rounds_without_progress,
            "unacknowledged deliveries — re-offering the tail"
        );
        if rounds_without_progress >= RESEND_GIVEUP_ROUNDS && !stall_reported {
            // loud, honest, and NOT a stop: the surface names it while the
            // resends keep trying at the cap
            stall_reported = true;
            for m in &cfg.members {
                sink.send_failed(m, "not acknowledging deliveries — still resending")
                    .await;
            }
        }
        backoff_secs = backoff_secs.saturating_mul(2).min(RESEND_MAX_BACKOFF_SECS);
        stalled_since = Some(tokio::time::Instant::now());
    }
}

/// Wall-clock seconds.
///
/// Deliberately NOT `ritual_net::now_secs`, which follows the h-window test
/// shift: this budget rations real traffic at real relays, and a test that
/// moves the window clock must not thereby buy itself resend rounds.
fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Take one resend round from this hour's budget. `false` = spent.
///
/// Persisted with the cursor on purpose: an in-memory budget hands a crash
/// loop a fresh allowance on every start, and the thing being rationed is a
/// publish that every member of the republic then re-reads.
pub(crate) fn consume_resend_round(cur: &mut molt_core::GroupCursor, now: u64) -> bool {
    roll_resend_window(cur, now);
    if cur.resend_rounds >= RESEND_ROUNDS_PER_HOUR {
        return false;
    }
    cur.resend_rounds = cur.resend_rounds.saturating_add(1);
    // the round republishes the whole tail — whatever evidence prompted it
    // is served by it
    cur.heal_evidence = false;
    true
}

/// Take the ONE evidence-driven round this hour grants past a spent budget
/// (live incident 2026-08-09 §2): a claim sheet that spoke about our events
/// proved a listening, still-lagging peer — typically a recovered
/// incarnation — and holding its heal behind the hourly refill read as
/// permanent deafness in the field. Bounded: the sheet latch buys at most
/// one extra round per window, a blind stall loop buys nothing.
pub(crate) fn consume_heal_round(cur: &mut molt_core::GroupCursor, now: u64) -> bool {
    roll_resend_window(cur, now);
    if !cur.heal_evidence || cur.heal_rounds > 0 {
        return false;
    }
    cur.heal_evidence = false;
    cur.heal_rounds = 1;
    true
}

fn roll_resend_window(cur: &mut molt_core::GroupCursor, now: u64) {
    if now.saturating_sub(cur.resend_window_start) >= 3_600 {
        cur.resend_window_start = now;
        cur.resend_rounds = 0;
        cur.heal_rounds = 0;
    }
}

/// What one inbound frame turned into.
#[derive(Debug)]
enum Ingest {
    Delivered,
    /// No held exporter secret opened the outer layer: the frame predates this
    /// node's ring. Permanent, and counted.
    Opaque,
    /// A peer's claim sheet — its floor over OUR events, already applied.
    Acked,
    /// Decoded but carried nothing to deliver (a replay, a proposal).
    Nothing,
    /// Encrypted at an epoch this node has not reached — its commit is still
    /// in flight. **Held, never dropped**: on a broadcast channel the commit
    /// and the frames encrypted after it race, and the loser is ordinary.
    FutureEpoch,
    /// A commit merged and the epoch advanced — whatever is being held may
    /// decode now.
    EpochAdvanced,
    /// The engine is gone.
    EngineGone,
}

/// Frames held awaiting the commit that unlocks them.
///
/// Bounded for the same reason the mesh's reorder buffer is: a peer that
/// never sends the commit would otherwise pin memory for the session. Excess
/// is dropped back onto the delivery guarantee's re-offer, which is where an
/// unbounded hold's contents would have to come from anyway.
const EPOCH_HOLD_MAX: usize = 512;

/// A bounded ring of recently CONSUMED ciphertext hashes (live-incident
/// 2026-08-15): relays re-deliver frames across subscription overlaps and
/// resend rounds, the envelope seq sits INSIDE the ciphertext, and a
/// second MLS decrypt of a consumed frame can only ever end in openmls's
/// SecretReuseError storm — so exact duplicates turn around BEFORE the
/// ratchet is asked. Only consumed outcomes enter the ring: a held
/// FutureEpoch/Opaque frame must stay retryable.
pub(crate) struct SeenCiphertexts {
    order: std::collections::VecDeque<[u8; 32]>,
    set: std::collections::HashSet<[u8; 32]>,
}

impl SeenCiphertexts {
    const CAP: usize = 4096;

    pub(crate) fn new() -> Self {
        SeenCiphertexts {
            order: std::collections::VecDeque::new(),
            set: std::collections::HashSet::new(),
        }
    }

    fn hash(content: &str) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(content.as_bytes()).into()
    }

    pub(crate) fn seen(&self, content: &str) -> bool {
        self.set.contains(&Self::hash(content))
    }

    pub(crate) fn note(&mut self, content: &str) {
        let h = Self::hash(content);
        if self.set.insert(h) {
            self.order.push_back(h);
            if self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
}

// (content, created_at) travel as one pair: they are the FRAME, everything
// else is the channel the frame lands in
async fn ingest_one<L: OutboxLog, S: StateStore, K: EngineSink>(
    mls: &MlsChannel,
    log: &L,
    store: &S,
    sink: &K,
    me: &MemberId,
    seen: &mut SeenCiphertexts,
    frame: (&str, u64),
) -> Ingest {
    let (content, created_at) = frame;
    // an exact re-delivery of a frame this node already consumed: turn
    // around before the ratchet is asked (a second decrypt is at best a
    // SecretReuseError logged at ERROR by openmls, at worst wasted work)
    if seen.seen(content) {
        tracing::debug!(me = %me, "dropping an exact re-delivery of a consumed frame");
        return Ingest::Nothing;
    }
    let secrets = mls.exporter_secrets();
    let Ok(wire) = crate::envelope::open_outer(&secrets, content) else {
        return Ingest::Opaque;
    };
    // the CARRIER stamp, never NO_CARRIER_STAMP: 445 is the first transport
    // that carries one, and it is half of the CommitKey that breaks a
    // concurrent same-epoch commit race
    let outcome = mls.decode_at(&wire, created_at);
    // consumed outcomes enter the dedup ring — their re-delivery can never
    // decode again. Held ones (FutureEpoch/Opaque) must stay retryable and
    // do NOT. Commit outcomes (EpochAdvanced) stay out DELIBERATELY even
    // though their decrypt is spent: the concurrent-commit tiebreak (N3 §1)
    // may rewind one epoch and must be able to re-see the winning commit —
    // a ring hit there would swallow it.
    if matches!(
        outcome,
        MlsDecode::Deliver(..) | MlsDecode::Ack(..) | MlsDecode::GroupAck(..) | MlsDecode::Discard
    ) {
        seen.note(content);
    }
    match outcome {
        MlsDecode::Deliver(from, env) => {
            sink.peer_seen(&from).await;
            tracing::debug!(me = %me, %from, seq = env.seq, "group frame delivered to engine");
            if sink.deliver(&from, *env).await.is_err() {
                return Ingest::EngineGone;
            }
            Ingest::Delivered
        }
        // a broadcast ack rides the same channel as everything else and is
        // recognised by its own tag inside the MLS plaintext
        // a MESH ack on a broadcast channel: authenticated, but subject-less,
        // so there is nothing here that can be applied to anyone in
        // particular. Presence only.
        MlsDecode::Ack(from, _win) => {
            sink.peer_seen(&from).await;
            Ingest::Nothing
        }
        MlsDecode::GroupAck(from, ack) => {
            sink.peer_seen(&from).await;
            // `by` is SELF-DESCRIPTION; the MLS credential is the
            // authentication. A mismatch is a routing bug or a forgery
            // attempt, and either way the sheet is not actionable.
            if ack.by != from {
                tracing::warn!(%from, claimed = %ack.by, "a group ack disowns its sender — dropped");
                return Ingest::Nothing;
            }
            apply_group_ack(log, store, me, &from, &ack).await;
            Ingest::Acked
        }
        // the two arms the mesh supervisor implements and this loop did not:
        // both used to fall into a catch-all `Nothing`, which held nothing and
        // retried nothing, so a frame arriving ahead of its commit was DROPPED
        MlsDecode::FutureEpoch => Ingest::FutureEpoch,
        MlsDecode::EpochAdvanced => Ingest::EpochAdvanced,
        MlsDecode::Discard => Ingest::Nothing,
    }
}

/// Re-offer every held frame after an epoch advance, **in hold order**, and
/// repeatedly while progress is made — a held commit can unlock further held
/// frames, so one pass is not enough. Returns how many stayed permanently
/// unreadable, for the operator counter.
///
/// Each frame is retried at its ORIGINAL `created_at`: that stamp is half of
/// the `CommitKey` both ends must agree on, and re-stamping it here with the
/// retry time would make this node compute a different key from everyone
/// else's for the same commit.
///
/// **A frame still opaque after an advance is opaque for good.** That is the
/// eviction rule, and it is what keeps the hold from growing without bound:
/// if the frame had been from an epoch AHEAD of us, the advance we just made
/// is exactly what would have opened it.
async fn retry_epoch_hold<L: OutboxLog, S: StateStore, K: EngineSink>(
    mls: &MlsChannel,
    log: &L,
    store: &S,
    sink: &K,
    me: &MemberId,
    seen: &mut SeenCiphertexts,
    hold: &mut Vec<(String, u64)>,
) -> Result<u64, ()> {
    let mut lost = 0u64;
    loop {
        let mut progress = false;
        let mut still = Vec::new();
        for (content, at) in std::mem::take(hold) {
            match ingest_one(mls, log, store, sink, me, seen, (&content, at)).await {
                Ingest::EngineGone => return Err(()),
                Ingest::FutureEpoch => still.push((content, at)),
                Ingest::Opaque => lost += 1,
                _ => progress = true,
            }
        }
        *hold = still;
        if !progress || hold.is_empty() {
            return Ok(lost);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn inbox_loop<L: OutboxLog, S: StateStore, K: EngineSink>(
    channel: GroupChannel,
    mls: MlsChannel,
    log: L,
    store: S,
    me: MemberId,
    sink: K,
    health: watch::Sender<GroupHealth>,
    ack_wake: std::sync::Arc<tokio::sync::Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut sub = match channel.subscribe().await {
        Ok(s) => {
            tracing::debug!(me = %me, "group inbox subscribed");
            s
        }
        Err(e) => {
            tracing::warn!(me = %me, error = %e, "group inbox subscribe failed - the runtime is deaf");
            let _ = health.send(GroupHealth {
                subscribed: false,
                deaf: Some(e.to_string()),
                opaque_frames: 0,
            });
            return;
        }
    };
    let mut state = GroupHealth {
        subscribed: true,
        deaf: None,
        opaque_frames: 0,
    };
    let _ = health.send(state.clone());
    // frames whose commit has not arrived yet — see `retry_epoch_hold`
    let mut hold: Vec<(String, u64)> = Vec::new();
    // exact-duplicate turnaround (relay re-deliveries) — runtime-only
    let mut seen = SeenCiphertexts::new();
    loop {
        let recv = tokio::select! {
            r = sub.recv(RECV_SLICE) => r,
            _ = stop.changed() => return,
        };
        match recv {
            GroupRecv::Frame { content, created_at } => {
                if state.deaf.take().is_some() {
                    let _ = health.send(state.clone());
                }
                match ingest_one(&mls, &log, &store, &sink, &me, &mut seen, (&content, created_at)).await {
                    Ingest::EngineGone => return,
                    // NOT counted yet, and not dropped. On 445 the epoch shows
                    // up at the OUTER layer, not at the MLS decode: a frame
                    // sealed under an exporter we have not derived yet is
                    // simply unopenable, so "newer than us" and "older than
                    // the ring" arrive as the SAME answer. Holding it costs
                    // one retry after the next commit merges, and that retry
                    // is what tells the two apart.
                    Ingest::Opaque | Ingest::FutureEpoch => {
                        if hold.len() < EPOCH_HOLD_MAX {
                            hold.push((content, created_at));
                        } else {
                            // loud: the guarantee's re-offer is the repair
                            // path, and silence here would read as delivery
                            state.opaque_frames += 1;
                            let _ = health.send(state.clone());
                            tracing::warn!(
                                held = hold.len(),
                                "epoch hold is full — dropping an unopenable frame"
                            );
                        }
                    }
                    Ingest::EpochAdvanced => {
                        match retry_epoch_hold(&mls, &log, &store, &sink, &me, &mut seen, &mut hold).await {
                            Err(()) => return,
                            Ok(0) => {}
                            Ok(lost) => {
                                state.opaque_frames += lost;
                                let _ = health.send(state.clone());
                            }
                        }
                        // a drained hold may have applied a sheet
                        ack_wake.notify_one();
                    }
                    Ingest::Acked => ack_wake.notify_one(),
                    Ingest::Delivered | Ingest::Nothing => {}
                }
            }
            GroupRecv::Idle => {}
            GroupRecv::Deaf(why) => {
                if state.deaf.as_deref() != Some(why.as_str()) {
                    tracing::warn!(me = %me, why = %why, "group inbox went deaf");
                    state.deaf = Some(why);
                    let _ = health.send(state.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::OutboundCursor;

    fn cfg() -> GroupNetConfig {
        GroupNetConfig::fast("walter".into(), vec!["petra".into(), "zoe".into()])
    }

    /// The exact-duplicate turnaround (live-incident 2026-08-15): a
    /// CONSUMED frame's re-delivery is seen, an un-noted frame is not,
    /// and the ring evicts oldest-first at its cap — so a re-delivery
    /// storm can never reach the ratchet, while held (never-noted)
    /// frames stay retryable by construction.
    #[test]
    fn seen_ciphertexts_dedup_consumed_frames_and_evict_oldest() {
        let mut seen = SeenCiphertexts::new();
        assert!(!seen.seen("frame-1"));
        seen.note("frame-1");
        assert!(seen.seen("frame-1"), "a consumed frame turns around");
        assert!(!seen.seen("frame-2"), "an unseen frame passes");
        seen.note("frame-1");
        // fill past the cap: the oldest leaves, the newest stays
        for i in 0..SeenCiphertexts::CAP {
            seen.note(&format!("bulk-{i}"));
        }
        assert!(!seen.seen("frame-1"), "the oldest evicts at the cap");
        assert!(seen.seen(&format!("bulk-{}", SeenCiphertexts::CAP - 1)));
    }

    /// **A publish that can never succeed is not retried, and says so.**
    ///
    /// `RelayRuntime::publish` refuses an over-budget event LOCALLY, before
    /// any relay is contacted — the verdict is deterministic, so the outbox's
    /// three attempts and their backoff buy nothing but delay before the same
    /// answer. The reason mattered more than the delay: a permanent local
    /// refusal reported as "no relay accepted the frame" sends an operator
    /// hunting for a relay outage that does not exist.
    ///
    /// The relay URL here is never dialed — the size gate fires first — which
    /// is exactly the property under test.
    ///
    /// Time is PAUSED, so the elapsed reading counts backoff sleeps and
    /// nothing else. Wall-clock would measure the real CPU cost of sealing a
    /// 256 KiB frame in a debug build (~90 ms) and drown the signal.
    #[tokio::test(start_paused = true)]
    async fn a_locally_refused_frame_is_not_retried() {
        let channel = crate::ritual_net::GroupChannel::new(
            crate::dial::Dialer::Direct,
            vec!["ws://127.0.0.1:1".to_string()],
            [7u8; 32],
        );
        let cfg = cfg();
        // comfortably past DEFAULT_SIZE_BUDGET once base64 has had its way
        let frame = crate::supervisor::GroupFrame {
            ciphertext: vec![0x5au8; 256 * 1024],
            exporter: [3u8; 32],
        };
        let (_stop_tx, mut stop) = watch::channel(false);

        let started = tokio::time::Instant::now();
        let outcome = publish_with_backoff(&channel, &frame, &cfg, &mut stop).await;
        let elapsed = started.elapsed();

        let Err(PublishStall::Permanent(why)) = outcome else {
            panic!("an over-budget frame must be a PERMANENT refusal, got {outcome:?}");
        };
        assert!(
            why.contains("exceeds"),
            "the reason must name the actual cause, not a relay condition: {why}"
        );
        assert!(
            elapsed < cfg.retry_base,
            "the refusal took {elapsed:?} — it was retried on a backoff that cannot \
             change a locally deterministic verdict"
        );
    }

    /// A relay that is merely unreachable is the opposite case and must keep
    /// its retries: that verdict CAN change on its own.
    #[tokio::test]
    async fn an_empty_pool_stays_a_transient_stall() {
        let channel = crate::ritual_net::GroupChannel::new(
            crate::dial::Dialer::Direct,
            Vec::new(),
            [7u8; 32],
        );
        let frame = crate::supervisor::GroupFrame {
            ciphertext: vec![0u8; 32],
            exporter: [3u8; 32],
        };
        let (_stop_tx, mut stop) = watch::channel(false);
        let outcome = publish_with_backoff(&channel, &frame, &cfg(), &mut stop).await;
        assert!(
            matches!(outcome, Err(PublishStall::Transient)),
            "an empty pool can refill — it must not be classed permanent: {outcome:?}"
        );
    }

    /// **A frame that arrives ahead of its commit is held, not dropped.**
    ///
    /// A recovery re-key is the first thing in this product that puts an MLS
    /// commit on the group channel, and on a broadcast channel the commit and
    /// the frames encrypted after it race. `ingest_one` used to route both
    /// `FutureEpoch` and `EpochAdvanced` into a catch-all `Nothing`: nothing
    /// was held, nothing retried, so the loser of that race was lost.
    ///
    /// The order here is the whole test — the application frame is published
    /// BEFORE the commit that makes it decodable. Publish them the other way
    /// round and it passes with the hold absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_ahead_of_its_commit_is_held_until_the_commit_lands() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<molt_core::EventEnvelope>>>);
        impl EngineSink for Sink {
            async fn deliver(
                &self,
                _from: &MemberId,
                env: molt_core::EventEnvelope,
            ) -> Result<(), crate::NetError> {
                self.0.lock().expect("sink").push(env);
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(1), "alice").expect("alice");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        alice.create_group().expect("group");
        let welcome = alice
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("a welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");
        let cara2 = MlsMember::new(&key(4), "cara").expect("cara's fresh device");

        // alice re-keys cara's seat: this is the RECOVERY shape, and it is the
        // only API that hands the commit out for someone else to merge
        let (commit, _w) = alice
            .restore_member("cara", &cara2.key_package().expect("kp"), 1_760_000_000)
            .expect("re-key");
        drop(cara);

        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [7u8; 32];
        let alice_chan = crate::ritual_net::GroupChannel::new(
            crate::dial::Dialer::Direct,
            vec![url.clone()],
            seed,
        );
        let alice_mls = MlsChannel::new(alice);

        let env = |seq: u64, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope {
            prev_seq: 0,
            seq,
            ts: 1_751_000_000,
            by: "alice".to_string(),
            body,
        };
        let msg = env(
            1,
            molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage {
                id: molt_core::MessageId([9u8; 16]),
                from: "alice".to_string(),
                body: "spoken past the commit".to_string(),
                ts: 1_751_000_000,
                quote: None,
                quote_id: None,
                channel: molt_core::ChannelRef::Group,
                kind: molt_core::ChatKind::User,
                reactions: std::collections::BTreeMap::new(),
                deleted_by: None,
                file: None,
                read_by: std::collections::BTreeSet::new(),
            }),
        );
        // encrypted at the NEW epoch — alice merged her own commit already
        let after = alice_mls.group_frame(&msg).expect("frame after the commit");
        let commit_frame = alice_mls
            .group_frame(&env(2, molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&commit) }))
            .expect("commit frame");

        let sink = Sink::default();
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        let bob_chan =
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], seed);
        let handle = spawn_group(
            bob_chan,
            MlsChannel::new(bob),
            GroupNetConfig::fast("bob".into(), vec!["alice".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            sink.clone(),
            wake_rx,
            health_tx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // THE ORDER IS THE TEST: the message first, its commit second
        alice_chan
            .publish_frame(&after.exporter, &after.ciphertext)
            .await
            .expect("publish the future-epoch message");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            sink.0.lock().expect("sink").is_empty(),
            "it must NOT be deliverable before its commit — otherwise this \
             test proves nothing about holding it"
        );

        alice_chan
            .publish_frame(&commit_frame.exporter, &commit_frame.ciphertext)
            .await
            .expect("publish the commit");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if !sink.0.lock().expect("sink").is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the held frame was never re-offered after the commit merged"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.shutdown().await;
    }

    /// **G4 (N5.4): a frame still opaque after an epoch advance is COUNTED,
    /// not silently dropped.** "Older than the exporter ring" and "newer
    /// than us" arrive as the same answer on 445 — an unopenable outer
    /// layer — and the retry after the next commit merges is what tells
    /// them apart: what the advance did not open is opaque for good. That
    /// permanent loss must reach the health surface (`opaque_frames`),
    /// because the engine's net_health fold is downstream of exactly this
    /// counter and silence here would read as delivery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_still_opaque_after_the_advance_is_counted_as_lost() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<molt_core::EventEnvelope>>>);
        impl EngineSink for Sink {
            async fn deliver(
                &self,
                _from: &MemberId,
                env: molt_core::EventEnvelope,
            ) -> Result<(), crate::NetError> {
                self.0.lock().expect("sink").push(env);
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(1), "alice").expect("alice");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        let cara = MlsMember::new(&key(3), "cara").expect("cara");
        alice.create_group().expect("group");
        let welcome = alice
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("a welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");
        let cara2 = MlsMember::new(&key(4), "cara").expect("cara's fresh device");
        let (commit, _w) = alice
            .restore_member("cara", &cara2.key_package().expect("kp"), 1_760_000_000)
            .expect("re-key");
        drop(cara);

        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [7u8; 32];
        let alice_chan = crate::ritual_net::GroupChannel::new(
            crate::dial::Dialer::Direct,
            vec![url.clone()],
            seed,
        );
        let alice_mls = MlsChannel::new(alice);
        let commit_frame = alice_mls
            .group_frame(&molt_core::EventEnvelope {
                prev_seq: 0,
                seq: 1,
                ts: 1_751_000_000,
                by: "alice".to_string(),
                body: molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&commit) },
            })
            .expect("commit frame");

        let sink = Sink::default();
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, mut health_rx) = watch::channel(GroupHealth::default());
        let bob_chan =
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], seed);
        let handle = spawn_group(
            bob_chan,
            MlsChannel::new(bob),
            GroupNetConfig::fast("bob".into(), vec!["alice".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            sink,
            wake_rx,
            health_tx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // a frame under an exporter NO group epoch ever derives: unopenable
        // now, unopenable after any advance — the laggard-past-the-ring shape
        alice_chan
            .publish_frame(&[9u8; 32], &[0x5au8; 64])
            .await
            .expect("publish the alien frame");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        // …then the commit that advances bob's epoch and triggers the retry
        alice_chan
            .publish_frame(&commit_frame.exporter, &commit_frame.ciphertext)
            .await
            .expect("publish the commit");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if health_rx.borrow_and_update().opaque_frames >= 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the permanently opaque frame was never counted as lost"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.shutdown().await;
    }

    /// **A stop sent before the tasks first poll must still stop them.**
    ///
    /// The stop rode a `Notify`, and `notify_waiters` wakes only tasks parked
    /// on it AT THAT MOMENT — it does not latch. An engine that builds a
    /// runtime and replaces or shuts it down within the same synchronous
    /// stretch (the recovery path did exactly that) signalled into the void:
    /// the loops had never polled yet, the wake was lost, and the ORPHANED
    /// outbox kept publishing next to its replacement — every group frame of
    /// the rejoiner went out twice.
    ///
    /// On a current-thread runtime the race is deterministic: nothing spawned
    /// has run before `shutdown()` is called, so a signal that does not latch
    /// is GUARANTEED lost — and `shutdown()`, which awaits the outbox, never
    /// returns.
    #[tokio::test(start_paused = true)]
    async fn a_stop_sent_before_the_tasks_first_poll_still_stops_them() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;

        #[derive(Clone)]
        struct NullSink;
        impl EngineSink for NullSink {
            async fn deliver(
                &self,
                _from: &MemberId,
                _env: molt_core::EventEnvelope,
            ) -> Result<(), crate::NetError> {
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
        }

        let mls = MlsMember::new(&SigningKey::from_bytes(&[9u8; 32]), "walter").expect("mls");
        let channel = crate::ritual_net::GroupChannel::new(
            crate::dial::Dialer::Direct,
            vec!["ws://127.0.0.1:1".to_string()],
            [7u8; 32],
        );
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        let handle = spawn_group(
            channel,
            MlsChannel::from_shared(std::sync::Arc::new(std::sync::Mutex::new(mls))),
            cfg(),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            NullSink,
            wake_rx,
            health_tx,
        );
        // nothing spawned has polled yet — this IS the lost-wakeup window
        let done =
            tokio::time::timeout(std::time::Duration::from_secs(30), handle.shutdown()).await;
        assert!(
            done.is_ok(),
            "the stop was sent before the loops first polled and was lost — \
             the outbox never exits"
        );
    }

    /// **A fresh incarnation's claim sheet is EVIDENCE, even at floor 0.**
    ///
    /// A rejoiner enters the broadcast mid-stream: it never saw the
    /// sender's early events, so its claim sheet computes a floor of 0 —
    /// truthfully, nothing is proven delivered. `apply_group_ack` then hit
    /// its `floor == old && old > 0` arm and latched NOTHING, so
    /// `group_floor` stayed `None`, the rewind never ran, and the frames
    /// the rejoiner is missing were never republished. It parked its
    /// catch-up on a predecessor that could not arrive and sat there until
    /// the 900 s pathology valve — the §3.1a race, measured 1 run in 4.
    ///
    /// The rule the guard exists for is "a sheet that says NOTHING about us
    /// proves nothing" — and that case is the `window_for(me).is_none()`
    /// early return, not this one. A sheet that DOES speak about us is
    /// evidence the peer is listening, whatever floor it implies.
    #[tokio::test]
    async fn a_fresh_incarnations_sheet_counts_as_evidence_at_floor_zero() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        // three own events; the peer accepted only the LAST one (it joined
        // the broadcast after 1 and 2 were published)
        for seq in 1..=3u64 {
            log.push(molt_core::EventEnvelope {
                prev_seq: seq.saturating_sub(1),
                seq,
                ts: 1_751_000_000 + seq,
                by: "walter".to_string(),
                body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    molt_core::MessageId([u8::try_from(seq).unwrap_or(0); 16]),
                    "walter",
                    "x",
                    1_751_000_000,
                )),
            });
        }
        let mut window = molt_core::AcceptedWindow::default();
        assert!(window.accept(3), "the rejoiner accepted only the newest");
        let ack = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([("walter".to_string(), window)]),
        );

        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &ack).await;

        let state = <crate::MemStateStore as StateStore>::load(&store).await;
        let cursor = state.outbound.get("petra").expect("the sheet spoke about us");
        assert!(
            cursor.ack_seen,
            "a sheet that speaks about us is evidence - without the latch the \
             rewind never runs and the rejoiner's missing frames are never resent"
        );
        assert_eq!(cursor.acked_floor, 0, "…and the floor stays honest: nothing is proven");
        // …which is what makes the rewind reach back and republish the span
        let cfg = GroupNetConfig::fast("walter".into(), vec!["petra".into()]);
        assert_eq!(group_floor(&state, &cfg), Some(0));
    }

    /// **…and the evidence must WAKE the outbox (§3.1a, the losing run).**
    ///
    /// The latch above is inert if nobody re-reads it: the outbox evaluates
    /// its own-ackable tail and — at `floor=None` — goes to sleep on the
    /// log-append wakeup. A rejoiner's FIRST claim sheet arrives moments
    /// later, creates the tail evidence out of thin air, and by definition
    /// never appends to the log. In a quiet republic nothing else does
    /// either, so the stall clock never arms, no rewind republishes the span
    /// the rejoiner's catch-up parked on, and the park sits until its 900 s
    /// valve. This is the capstone's 1-in-4 losing interleaving, made
    /// deterministic: the sheet is sent only after the outbox is provably
    /// idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_claim_sheet_wakes_the_idle_outbox() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<molt_core::EventEnvelope>>>);
        impl EngineSink for Sink {
            async fn deliver(
                &self,
                _from: &MemberId,
                env: molt_core::EventEnvelope,
            ) -> Result<(), crate::NetError> {
                self.0.lock().expect("sink").push(env);
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut walter = MlsMember::new(&key(1), "walter").expect("walter");
        let petra = MlsMember::new(&key(2), "petra").expect("petra");
        walter.create_group().expect("group");
        let welcome = walter
            .add_members(&[petra.key_package().expect("kp")])
            .expect("add")
            .expect("a welcome");
        let mut petra = petra;
        petra.join_from_welcome(&welcome).expect("petra joins");

        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [7u8; 32];
        let chan = |u: &str| {
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![u.into()], seed)
        };

        // walter: two own events already in the log, nobody has ever acked
        let log = crate::MemLog::default();
        for seq in 1..=2u64 {
            log.push(molt_core::EventEnvelope {
                prev_seq: seq.saturating_sub(1),
                seq,
                ts: 1_751_000_000 + seq,
                by: "walter".to_string(),
                body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    molt_core::MessageId([u8::try_from(seq).unwrap_or(0); 16]),
                    "walter",
                    "x",
                    1_751_000_000,
                )),
            });
        }
        let (_wake_w, wake_w_rx) = watch::channel(0u64);
        let (health_w, _hw) = watch::channel(GroupHealth::default());
        let walter_handle = spawn_group(
            chan(&url),
            MlsChannel::new(walter),
            GroupNetConfig::fast("walter".into(), vec!["petra".into()]),
            log,
            crate::MemStateStore::default(),
            Sink::default(),
            wake_w_rx,
            health_w,
        );

        // petra: a receiving runtime (no engine, so nothing acks by itself);
        // keep an MlsChannel clone to frame her claim sheet by hand
        let petra_mls = MlsChannel::new(petra);
        let sink = Sink::default();
        let (_wake_p, wake_p_rx) = watch::channel(0u64);
        let (health_p, _hp) = watch::channel(GroupHealth::default());
        let petra_handle = spawn_group(
            chan(&url),
            petra_mls.clone(),
            GroupNetConfig::fast("petra".into(), vec!["walter".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            sink.clone(),
            wake_p_rx,
            health_p,
        );

        // both published events land on petra…
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while sink.0.lock().expect("sink").len() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "walter's initial publishes never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // …and walter's outbox finishes its pass and goes idle (floor=None)
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // the rejoiner-shaped sheet: it speaks about walter, but proves only
        // the NEWEST event — seq 1 stays own-ackable above floor 0
        let mut window = molt_core::AcceptedWindow::default();
        assert!(window.accept(2), "petra accepted only the newest");
        let ack = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([("walter".to_string(), window)]),
        );
        let frame = petra_mls
            .group_control_frame(&ack.to_frame())
            .expect("ack frame");
        chan(&url)
            .publish_frame(&frame.exporter, &frame.ciphertext)
            .await
            .expect("publish the sheet");

        // the sheet must wake the outbox, arm the stall clock, and re-offer
        // the span within one RESEND_AFTER_SECS round — without any append
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(RESEND_AFTER_SECS + 8);
        while sink.0.lock().expect("sink").len() < 4 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the claim sheet latched evidence but nothing woke the outbox — \
                 the rewind never republished the span the rejoiner is missing"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        walter_handle.shutdown().await;
        petra_handle.shutdown().await;
    }

    /// The counter-case the guard exists for, unchanged: a sheet that says
    /// NOTHING about us proves nothing and must not latch — else a silent
    /// peer's floor of zero rewinds the whole log for everyone.
    #[tokio::test]
    async fn a_sheet_that_is_silent_about_us_still_proves_nothing() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        let ack = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([(
                "zoe".to_string(),
                molt_core::AcceptedWindow::default(),
            )]),
        );
        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &ack).await;
        let state = <crate::MemStateStore as StateStore>::load(&store).await;
        assert!(
            state.outbound.get("petra").is_none_or(|c| !c.ack_seen),
            "silence about us is not evidence about us"
        );
    }

    /// With nobody acking, the floor is unknown and the cursor does not move.
    ///
    /// This is the N5.2 shape: the rewind exists and is inert, so N5.3 lights
    /// it up by SENDING acks rather than by reshaping the outbox.
    #[test]
    fn an_unacked_group_never_rewinds() {
        let mut st = TransportState {
            group: Some(molt_core::GroupCursor { log_seq: 9, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(group_floor(&st, &cfg()), None);
        rewind_group(&mut st, &cfg());
        assert_eq!(st.group.expect("cursor").log_seq, 9, "nothing proven, nothing rewound");
    }

    /// The floor is the MINIMUM over acking members — one lagging member holds
    /// the whole broadcast back, which is the point: one publish serves all.
    #[test]
    fn the_group_floor_is_the_slowest_acking_member() {
        let mut st = TransportState {
            group: Some(molt_core::GroupCursor { log_seq: 9, ..Default::default() }),
            ..Default::default()
        };
        st.outbound.insert(
            "petra".into(),
            OutboundCursor { acked_floor: 7, ack_seen: true, ..Default::default() },
        );
        st.outbound.insert(
            "zoe".into(),
            OutboundCursor { acked_floor: 4, ack_seen: true, ..Default::default() },
        );
        // …and a member who never acked is NOT counted as a floor of zero:
        // absence of evidence is not evidence of loss
        st.outbound.insert(
            "quentin".into(),
            OutboundCursor { acked_floor: 0, ack_seen: false, ..Default::default() },
        );
        assert_eq!(group_floor(&st, &cfg()), Some(4));
        rewind_group(&mut st, &cfg());
        assert_eq!(st.group.expect("cursor").log_seq, 4);
    }

    /// The hourly budget is spent, then refilled by the clock — not by a
    /// restart.
    ///
    /// A broadcast resend is one publish that every member re-reads, so an
    /// unbounded stall loop is an amplifier pointed at the whole republic.
    /// The counter lives WITH the cursor for exactly that reason: an
    /// in-memory budget would hand a crash loop a fresh allowance on every
    /// start, which is the shape a stall loop already has.
    #[test]
    fn the_resend_budget_is_spent_by_rounds_and_refilled_by_the_clock() {
        let mut cur = molt_core::GroupCursor::default();
        let t0 = 1_700_000_000;
        for i in 0..RESEND_ROUNDS_PER_HOUR {
            assert!(consume_resend_round(&mut cur, t0), "round {i} must be allowed");
        }
        assert!(!consume_resend_round(&mut cur, t0), "the budget is spent");
        // …still spent a minute later — the window is an hour, not a nap
        assert!(!consume_resend_round(&mut cur, t0 + 60));
        // …and a RESTART does not refill it: the counter is on the persisted
        // cursor, so this is the same state a reopened workspace loads
        let mut reloaded = cur;
        assert!(!consume_resend_round(&mut reloaded, t0 + 60));
        // the clock does
        assert!(consume_resend_round(&mut reloaded, t0 + 3_600));
        assert_eq!(reloaded.resend_rounds, 1, "a fresh window starts at one");
    }

    /// **Evidence of a lagging listener grants ONE heal round past a spent
    /// budget** (live incident 2026-08-09 §2, the budget audit).
    ///
    /// The field sequence: a dead peer burns all 12 rounds; the peer then
    /// RECOVERS and its fresh incarnation sends claim sheets that prove it
    /// is alive and still missing the tail — but the budget is spent, so
    /// the healing resend waits out the hour and the user reads the node
    /// as permanently deaf. A sheet is EVIDENCE, not a blind retry; it
    /// buys exactly one extra round per hour window.
    #[test]
    fn a_claim_sheet_grants_one_heal_round_past_a_spent_budget() {
        let mut cur = molt_core::GroupCursor::default();
        let t0 = 1_700_000_000;
        for _ in 0..RESEND_ROUNDS_PER_HOUR {
            assert!(consume_resend_round(&mut cur, t0));
        }
        assert!(!consume_resend_round(&mut cur, t0), "the budget is spent");
        // no evidence -> no heal round
        assert!(!consume_heal_round(&mut cur, t0));
        // a claim sheet arrived (apply_group_ack latches this)
        cur.heal_evidence = true;
        assert!(consume_heal_round(&mut cur, t0), "evidence buys ONE round");
        // …and only one: fresh evidence inside the same window is held
        cur.heal_evidence = true;
        assert!(!consume_heal_round(&mut cur, t0));
        // the clock refills everything, heal allowance included: burn the
        // fresh window, then a NEW sheet arrives (a normal round clears the
        // latch on purpose - it just served the evidence)
        for _ in 0..RESEND_ROUNDS_PER_HOUR {
            assert!(consume_resend_round(&mut cur, t0 + 3_600));
        }
        cur.heal_evidence = true;
        assert!(consume_heal_round(&mut cur, t0 + 3_600), "a fresh window heals again");
    }

    /// A round the normal budget still covers SERVES the evidence — the
    /// heal allowance stays in reserve for the spent case only.
    #[test]
    fn a_normal_resend_round_clears_the_heal_evidence() {
        let mut cur =
            molt_core::GroupCursor { heal_evidence: true, ..Default::default() };
        assert!(consume_resend_round(&mut cur, 1_700_000_000));
        assert!(!cur.heal_evidence, "the round just republished the tail");
    }

    /// `apply_group_ack` latches the heal evidence only for a sheet whose
    /// peer still TRAILS our publish cursor — that is what a re-recovered
    /// incarnation's sheets look like (their floor cannot advance past
    /// what the old incarnation already proved). A caught-up peer's sheet
    /// and silence about us both latch nothing (review 2026-08-15: an
    /// over-eager latch let a healthy peer's ack re-arm the heal round
    /// against a peer that provably is not listening).
    #[tokio::test]
    async fn only_a_lagging_peers_sheet_latches_heal_evidence() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=3u64 {
            log.push(molt_core::EventEnvelope {
                prev_seq: seq.saturating_sub(1),
                seq,
                ts: 1_751_000_000 + seq,
                by: "walter".to_string(),
                body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    molt_core::MessageId([u8::try_from(seq).unwrap_or(0); 16]),
                    "walter",
                    "x",
                    1_751_000_000,
                )),
            });
        }
        // the publish cursor stands at 3; petra's window proves only seq 1
        {
            let mut st = <crate::MemStateStore as StateStore>::load(&store).await;
            st.group = Some(molt_core::GroupCursor { log_seq: 3, ..Default::default() });
            <crate::MemStateStore as StateStore>::save(&store, st).await;
        }
        let mut window = molt_core::AcceptedWindow::default();
        assert!(window.accept(1));
        let ack = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([("walter".to_string(), window)]),
        );
        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &ack).await;
        let state = <crate::MemStateStore as StateStore>::load(&store).await;
        assert!(
            state.group.unwrap_or_default().heal_evidence,
            "a lagging listener's sheet is heal evidence"
        );

        // a CAUGHT-UP peer's sheet proves nothing to heal
        let store2 = crate::MemStateStore::default();
        {
            let mut st = <crate::MemStateStore as StateStore>::load(&store2).await;
            st.group = Some(molt_core::GroupCursor { log_seq: 3, ..Default::default() });
            <crate::MemStateStore as StateStore>::save(&store2, st).await;
        }
        let mut caught_up = molt_core::AcceptedWindow::default();
        for seq in 1..=3u64 {
            assert!(caught_up.accept(seq));
        }
        let ack2 = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([("walter".to_string(), caught_up)]),
        );
        apply_group_ack(&log, &store2, &"walter".to_string(), &"petra".to_string(), &ack2).await;
        let state2 = <crate::MemStateStore as StateStore>::load(&store2).await;
        assert!(
            !state2.group.unwrap_or_default().heal_evidence,
            "a caught-up peer buys no heal round"
        );

        // silence (a sheet about OTHER members only) latches nothing
        let store3 = crate::MemStateStore::default();
        let ack3 = crate::group_ack::GroupAck::new(
            "petra".to_string(),
            std::collections::BTreeMap::from([(
                "zoe".to_string(),
                molt_core::AcceptedWindow::default(),
            )]),
        );
        apply_group_ack(&log, &store3, &"walter".to_string(), &"petra".to_string(), &ack3).await;
        let state3 = <crate::MemStateStore as StateStore>::load(&store3).await;
        assert!(
            !state3.group.unwrap_or_default().heal_evidence,
            "silence about us proves nothing and buys nothing"
        );
    }

    /// A floor at or above the cursor never pushes it forward.
    #[test]
    fn a_rewind_only_ever_moves_backwards() {
        let mut st = TransportState {
            group: Some(molt_core::GroupCursor { log_seq: 3, ..Default::default() }),
            ..Default::default()
        };
        st.outbound.insert(
            "petra".into(),
            OutboundCursor { acked_floor: 11, ack_seen: true, ..Default::default() },
        );
        st.outbound.insert(
            "zoe".into(),
            OutboundCursor { acked_floor: 11, ack_seen: true, ..Default::default() },
        );
        rewind_group(&mut st, &cfg());
        assert_eq!(
            st.group.expect("cursor").log_seq,
            3,
            "a rewind is a rewind — it must never advance the send position"
        );
    }
}
