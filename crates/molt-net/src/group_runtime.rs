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

use std::sync::Arc;
use std::time::Duration;

use molt_core::{MemberId, TransportState};
use tokio::sync::{watch, Notify};

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
    stop: Arc<Notify>,
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
        self.stop.notify_waiters();
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
        self.stop.notify_waiters();
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
    let stop = Arc::new(Notify::new());
    let (log2, store2) = (log.clone(), store.clone());
    let (acks, acks_rx) = watch::channel(None);
    // a THIRD task: an ack must not queue behind `publish_with_backoff`, whose
    // chain runs three attempts up to the retry cap. Head-of-line blocking the
    // guarantee's own feedback behind the traffic it is meant to prove is how
    // a stall becomes permanent.
    let ack = tokio::spawn(ack_loop(
        channel.clone(),
        mls.clone(),
        acks_rx,
        stop.clone(),
    ));
    let outbox = tokio::spawn(outbox_loop(
        channel.clone(),
        mls.clone(),
        cfg.clone(),
        log,
        store,
        sink.clone(),
        wakeup,
        stop.clone(),
    ));
    let inbox = tokio::spawn(inbox_loop(
        channel,
        mls,
        log2,
        store2,
        cfg.member.clone(),
        sink,
        health,
        stop.clone(),
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
/// 1. a sheet that says nothing about us proves nothing — it must NOT latch
///    `ack_seen`, because a "proven" floor of zero makes the outbox rewind to
///    the start of the log;
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
    if floor > old {
        let cursor = state.outbound.entry(from.clone()).or_default();
        cursor.acked_floor = floor;
        cursor.ack_seen = true;
        store.save(state).await;
    } else if floor == old && old > 0 {
        // no progress, but the peer HAS spoken about us before — keep the
        // evidence flag without moving anything
        let cursor = state.outbound.entry(from.clone()).or_default();
        if !cursor.ack_seen {
            cursor.ack_seen = true;
            store.save(state).await;
        }
    }
}

/// Publish claim sheets as they are handed over.
async fn ack_loop(
    channel: GroupChannel,
    mls: MlsChannel,
    mut rx: watch::Receiver<Option<crate::group_ack::GroupAck>>,
    stop: Arc<Notify>,
) {
    loop {
        tokio::select! {
            r = rx.changed() => {
                if r.is_err() {
                    return;
                }
            }
            () = stop.notified() => return,
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

/// Publish one framed envelope, retrying a relay-side refusal on a backoff.
/// `None` = the runtime was told to stop, or every attempt failed.
async fn publish_with_backoff(
    channel: &GroupChannel,
    frame: &crate::supervisor::GroupFrame,
    cfg: &GroupNetConfig,
    stop: &Notify,
) -> Option<()> {
    let mut delay = cfg.retry_base;
    for _ in 0..PUBLISH_ATTEMPTS {
        // the SAME bytes across relay retries inside one attempt chain: a relay
        // NAK is not a peer rejection, and re-encrypting would burn a ratchet
        // generation for nothing. A REWIND re-frames instead (N5.3).
        match channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
            Ok((_stamp, report)) if !report.accepted.is_empty() => return Some(()),
            Ok((_stamp, _report)) => {
                tracing::warn!("no relay accepted a group frame — retrying");
            }
            Err(e) => tracing::warn!(error = %e, "publishing a group frame failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = stop.notified() => return None,
        }
        delay = (delay * 2).min(cfg.retry_cap);
    }
    None
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
    stop: Arc<Notify>,
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
            if publish_with_backoff(&channel, &frame, &cfg, &stop).await.is_some() {
                published_through = env.seq;
            } else {
                // hold the cursor exactly here: nothing in N5.2 recovers a
                // skipped envelope, so advancing past an unpublished one would
                // lose it permanently
                sink.send_failed(&cfg.member, "no relay accepted the frame").await;
                stalled_since = None;
                break;
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
            stalled_since = None;
            tokio::select! {
                r = wakeup.changed() => {
                    if r.is_err() {
                        return;
                    }
                }
                () = stop.notified() => return,
            }
            continue;
        }
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
            () = stop.notified() => return,
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
            tracing::warn!(
                floor = f,
                "the resend budget for this hour is spent — holding the tail"
            );
            backoff_secs = backoff_secs.saturating_mul(2).min(RESEND_MAX_BACKOFF_SECS);
            continue;
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
    if now.saturating_sub(cur.resend_window_start) >= 3_600 {
        cur.resend_window_start = now;
        cur.resend_rounds = 0;
    }
    if cur.resend_rounds >= RESEND_ROUNDS_PER_HOUR {
        return false;
    }
    cur.resend_rounds = cur.resend_rounds.saturating_add(1);
    true
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
    /// Decoded but carried nothing to deliver (a commit, a replay).
    Nothing,
    /// The engine is gone.
    EngineGone,
}

async fn ingest_one<L: OutboxLog, S: StateStore, K: EngineSink>(
    mls: &MlsChannel,
    log: &L,
    store: &S,
    sink: &K,
    me: &MemberId,
    content: &str,
    created_at: u64,
) -> Ingest {
    let secrets = mls.exporter_secrets();
    let Ok(wire) = crate::envelope::open_outer(&secrets, content) else {
        return Ingest::Opaque;
    };
    // the CARRIER stamp, never NO_CARRIER_STAMP: 445 is the first transport
    // that carries one, and it is half of the CommitKey that breaks a
    // concurrent same-epoch commit race
    match mls.decode_at(&wire, created_at) {
        MlsDecode::Deliver(from, env) => {
            sink.peer_seen(&from).await;
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
        _ => Ingest::Nothing,
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
    stop: Arc<Notify>,
) {
    let mut sub = match channel.subscribe().await {
        Ok(s) => s,
        Err(e) => {
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
    loop {
        let recv = tokio::select! {
            r = sub.recv(RECV_SLICE) => r,
            () = stop.notified() => return,
        };
        match recv {
            GroupRecv::Frame { content, created_at } => {
                if state.deaf.take().is_some() {
                    let _ = health.send(state.clone());
                }
                match ingest_one(&mls, &log, &store, &sink, &me, &content, created_at).await {
                    Ingest::EngineGone => return,
                    Ingest::Opaque => {
                        state.opaque_frames += 1;
                        let _ = health.send(state.clone());
                    }
                    Ingest::Delivered | Ingest::Acked | Ingest::Nothing => {}
                }
            }
            GroupRecv::Idle => {}
            GroupRecv::Deaf(why) => {
                if state.deaf.as_deref() != Some(why.as_str()) {
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
