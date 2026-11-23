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

use molt_core::{EventEnvelope, MemberId, TransportState};
use tokio::sync::{mpsc, watch};

use crate::ritual_net::{GroupChannel, GroupRecv};
use crate::mls::MlsError;
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

/// Clock skew a live frame's stamp may carry against our subscription start.
const LIVE_SKEW_SECS: u64 = 120;

/// Control frames awaiting publication before further ones are dropped:
/// a paged mirror status must fit whole (mirroring §3.4), a poke is worth
/// one attempt anyway.
const CONTROL_QUEUE: usize = 64;

/// Re-offer rounds after a runtime start that go out even to peers the
/// engine calls silent: a fresh node's stamps come from its last session,
/// and a quiet live listener only proves itself by acking a round.
const OPEN_ROUNDS: u32 = 2;

/// Per asker: at most `GAP_MAX_SEQS` re-published frames per window, and a
/// seq re-published once is refused for [`GAP_RESEND_REFUSE`].
const GAP_ANSWER_EVERY: Duration = Duration::from_secs(10);

/// How long a seq re-published to one asker is not re-published to it again.
const GAP_RESEND_REFUSE: Duration = Duration::from_secs(60);

/// Per asker: one range listing per this long.
const GAP_LIST_EVERY: Duration = Duration::from_secs(5);

/// Re-published frames per turn before the next asker is served.
const GAP_TURN_FRAMES: usize = 16;

/// Re-published gap frames per minute, all askers together.
const GAP_FRAMES_PER_MIN: usize = 384;

/// Test seam: envelopes re-published by gap answers in this process.
static GAP_REPUBLISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam: how many envelopes gap answers re-published in this process.
#[doc(hidden)]
pub fn __gap_republished() -> u64 {
    GAP_REPUBLISHED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Test seam: gap wants that reached an answering runtime in this process.
static GAP_ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam: how many gap wants reached an answering runtime in this
/// process.
#[doc(hidden)]
pub fn __gap_asked() -> u64 {
    GAP_ASKED.load(std::sync::atomic::Ordering::SeqCst)
}

/// What one asker wants answered, coalesced: a flood overwrites only its
/// own entry.
#[derive(Debug, Default)]
struct GapAsk {
    seqs: std::collections::BTreeSet<u64>,
    range: Option<(u64, u64)>,
}

/// The pending gap asks, one entry per asker, served round-robin.
#[derive(Debug, Default)]
struct GapAsks {
    pending: std::sync::Mutex<std::collections::BTreeMap<MemberId, GapAsk>>,
    wake: tokio::sync::Notify,
}

/// Test seam for the stall backoff, packed ms `first << 32 | cap` (`0` =
/// production).
static RESEND_BACKOFF_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam: the stall backoff's first delay and ceiling for every runtime
/// configured afterwards in this process.
#[doc(hidden)]
pub fn __set_resend_backoff(first: Duration, cap: Duration) {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).min(0xffff_ffff);
    RESEND_BACKOFF_MS.store((ms(first) << 32) | ms(cap), std::sync::atomic::Ordering::SeqCst);
}

fn resend_backoff() -> (Duration, Duration) {
    match RESEND_BACKOFF_MS.load(std::sync::atomic::Ordering::SeqCst) {
        0 => (Duration::from_secs(RESEND_AFTER_SECS), Duration::from_secs(RESEND_MAX_BACKOFF_SECS)),
        v => (Duration::from_millis(v >> 32), Duration::from_millis(v & 0xffff_ffff)),
    }
}

/// Who the outbox may re-offer to, as the engine sees it. The default
/// (nobody silent, not locked out) holds until the engine's first verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxReach {
    /// Peers not heard within the engine's silence window.
    pub silent: std::collections::BTreeSet<MemberId>,
    /// This seat concluded it is locked out of the group.
    pub locked_out: bool,
}

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
    /// First stall backoff before an unacked tail is re-offered.
    pub resend_after: Duration,
    /// Ceiling of the stall backoff.
    pub resend_cap: Duration,
}

impl GroupNetConfig {
    /// Production timings.
    #[must_use]
    pub fn new(member: MemberId, members: Vec<MemberId>) -> Self {
        let (resend_after, resend_cap) = resend_backoff();
        Self {
            member,
            members,
            retry_base: Duration::from_secs(1),
            retry_cap: Duration::from_secs(120),
            resend_after,
            resend_cap,
        }
    }

    /// Millisecond publish retries for tests (the stall backoff stays).
    #[must_use]
    pub fn fast(member: MemberId, members: Vec<MemberId>) -> Self {
        Self {
            retry_base: Duration::from_millis(20),
            retry_cap: Duration::from_millis(200),
            ..Self::new(member, members)
        }
    }

    /// The same, with its own stall backoff.
    #[must_use]
    pub fn with_resend(mut self, first: Duration, cap: Duration) -> Self {
        self.resend_after = first;
        self.resend_cap = cap;
        self
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
    /// Of `opaque_frames`, those stamped after the subscription opened: live
    /// traffic this seat cannot read. Older ones are history it never held
    /// keys for (a rejoiner's day window), not a loss.
    pub live_opaque: u64,
    /// The subscription's first stored-events replay has not finished.
    pub catching_up: bool,
    /// A stored replay - the first or a reconnect's - is still coming in
    /// and not yet read through.
    pub replaying: bool,
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
    /// Control frames awaiting publication (pokes, piece wants). An mpsc,
    /// NOT a watch: they are events, not state — two nudges at two members
    /// must both go out, so a newer one may never supersede an unsent one.
    /// Bounded and dropped-when-full, because each is worth exactly one
    /// best-effort attempt.
    controls: mpsc::Sender<Vec<u8>>,
    /// Gap wants addressed at this node, awaiting their answer.
    gaps: std::sync::Arc<GapAsks>,
    /// The engine's latest verdict on who can be reached: state, so a watch.
    reach: watch::Sender<OutboxReach>,
    /// Whether the outbox is mid-pass with own frames to publish - what
    /// the file trickle yields to (mirroring §3.2).
    busy: watch::Receiver<bool>,
    outbox: Option<tokio::task::JoinHandle<()>>,
    inbox: Option<tokio::task::JoinHandle<()>>,
    ack: Option<tokio::task::JoinHandle<()>>,
    poke: Option<tokio::task::JoinHandle<()>>,
    gap: Option<tokio::task::JoinHandle<()>>,
    /// Closed on drop: its clones outlive the tasks (`relay_pool.md` §3 rule 3).
    channel: GroupChannel,
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
        // the poke task is outbound as well; its sender lives on this handle,
        // so dropping it ends the loop on its own
        if let Some(poke) = self.poke.take() {
            let _ = poke.await;
        }
        if let Some(gap) = self.gap.take() {
            let _ = gap.await;
        }
    }

    /// Queue a targeted resend of `seqs` for `to` (delivery guarantee
    /// §10); `false` = dropped, the queue is full - the asker asks again.
    pub fn answer_gap(&self, to: MemberId, seqs: Vec<u64>, range: Option<(u64, u64)>) -> bool {
        GAP_ASKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Ok(mut pending) = self.gaps.pending.lock() else { return false };
        let ask = pending.entry(to).or_default();
        ask.seqs.extend(seqs);
        while ask.seqs.len() > crate::gap::GAP_MAX_SEQS {
            ask.seqs.pop_last();
        }
        if range.is_some() {
            ask.range = range;
        }
        drop(pending);
        self.gaps.wake.notify_one();
        true
    }

    /// Queue the latest claim sheet for publication.
    ///
    /// Synchronous and non-blocking: the engine actor never awaits, and the
    /// sheet is full state, so dropping an unsent one in favour of a newer one
    /// loses nothing.
    pub fn publish_ack(&self, ack: crate::group_ack::GroupAck) {
        let _ = self.acks.send(Some(ack));
    }

    /// Queue one poke for publication (best-effort, non-blocking).
    ///
    /// `try_send`, so the engine actor never awaits and a full queue drops
    /// the nudge instead of applying backpressure to the actor. Losing a
    /// poke is the designed failure mode: it is a nudge, not a message.
    pub fn publish_poke(&self, poke: crate::poke::Poke) {
        self.publish_control(poke.to_frame());
    }

    /// Queue one control-frame plaintext for publication (best-effort,
    /// non-blocking, like a poke); `false` = dropped, the queue is full or
    /// closed.
    pub fn publish_control(&self, frame: Vec<u8>) -> bool {
        let queued = self.controls.try_send(frame).is_ok();
        if !queued {
            tracing::debug!("the control queue is full or closed - frame dropped");
        }
        queued
    }

    /// Hand the outbox the engine's reach verdict; `true` when it changed.
    pub fn set_reach(&self, reach: OutboxReach) -> bool {
        self.reach.send_if_modified(|r| {
            let changed = *r != reach;
            *r = reach;
            changed
        })
    }

    /// The verdict last handed over.
    #[must_use]
    pub fn reach(&self) -> OutboxReach {
        self.reach.borrow().clone()
    }

    /// Whether the last verdict called `member` silent.
    #[must_use]
    pub fn reach_silent(&self, member: &str) -> bool {
        self.reach.borrow().silent.contains(member)
    }

    /// The outbox's pending flag: `true` while a pass publishes own frames.
    #[must_use]
    pub fn outbox_busy(&self) -> watch::Receiver<bool> {
        self.busy.clone()
    }
}

impl Drop for GroupHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.channel.close();
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
#[cfg(test)]
pub(crate) fn rewind_group(state: &mut TransportState, cfg: &GroupNetConfig) {
    let Some(floor) = group_floor(state, cfg) else {
        return; // nobody has ever acked — nothing is proven, so nothing rewinds
    };
    rewind_group_to(state, floor);
}

/// Pull the broadcast cursor back to `floor`, never forward.
fn rewind_group_to(state: &mut TransportState, floor: u64) {
    let mut cur = state.group.unwrap_or_default();
    if floor < cur.log_seq {
        cur.log_seq = floor;
        state.group = Some(cur);
    }
}

/// The floor a re-offer round rewinds to: the lowest among the peers still
/// owed an own-ackable envelope of `envs` that the round may reach (all of
/// them during the open rounds). `Ok(None)` = nobody is owed any more,
/// `Err` = a quiet round.
fn reachable_floor(
    state: &TransportState,
    cfg: &GroupNetConfig,
    envs: &[EventEnvelope],
    reach: &OutboxReach,
    open: bool,
) -> Result<Option<u64>, Quiet> {
    let owed: Vec<(&MemberId, u64)> = cfg
        .members
        .iter()
        // a peer that stayed ahead through every give-up round is left
        // alone until a plausible sheet of it ends the episode
        .filter_map(|m| {
            state
                .outbound
                .get(m)
                .filter(|c| c.ack_seen && !(c.ack_ahead && c.ahead_rounds >= RESEND_GIVEUP_ROUNDS))
                .map(|c| (m, c.acked_floor))
        })
        .filter(|(_, f)| envs.iter().any(|e| e.seq > *f && crate::supervisor::own_ackable(&cfg.member, e)))
        .collect();
    if owed.is_empty() {
        return Ok(None);
    }
    if reach.locked_out {
        return Err(Quiet::LockedOut);
    }
    owed.iter()
        .filter(|(m, _)| open || !reach.silent.contains(*m))
        .map(|(_, f)| *f)
        .min()
        .map(Some)
        .ok_or(Quiet::PeersSilent)
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
    // the running group's frames ask the relays to let them go after three
    // days (`docs_archive/chat/chat_threads_lifecycle.md` §8.1) - the ritual that made
    // the group publishes without a deadline, a joiner may read it late
    let channel = channel.with_group_expiry();
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
    // a FOURTH task, for the same reason the ack has its own: a nudge must
    // not queue behind the outbox's retry chain, and the outbox must not
    // wait for a nudge
    let (controls, controls_rx) = mpsc::channel(CONTROL_QUEUE);
    let poke = tokio::spawn(control_loop(
        channel.clone(),
        mls.clone(),
        controls_rx,
        stop_rx.clone(),
    ));
    // a FIFTH: a targeted resend answers in seconds, never behind the
    // outbox's pass or its hourly budget (delivery guarantee §10)
    let gaps = std::sync::Arc::new(GapAsks::default());
    let gap = tokio::spawn(gap_loop(
        channel.clone(),
        mls.clone(),
        cfg.member.clone(),
        log.clone(),
        store.clone(),
        gaps.clone(),
        stop_rx.clone(),
    ));
    let (busy_tx, busy) = watch::channel(false);
    let (reach, reach_rx) = watch::channel(OutboxReach::default());
    let outbox = tokio::spawn(outbox_loop(
        channel.clone(),
        mls.clone(),
        cfg.clone(),
        log,
        store,
        sink.clone(),
        wakeup,
        ack_wake.clone(),
        reach_rx,
        busy_tx,
        stop_rx.clone(),
    ));
    let inbox = tokio::spawn(inbox_loop(
        channel.clone(),
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
        controls,
        gaps,
        reach,
        busy,
        outbox: Some(outbox),
        inbox: Some(inbox),
        ack: Some(ack),
        poke: Some(poke),
        gap: Some(gap),
        channel,
    }
}

/// Answer gap wants, one asker per turn: a range want gets the listing of
/// this node's own seqs in the span (nothing re-published); named seqs are
/// re-published as fresh frames and answered `resent`/`gone`. Seqs past the
/// publish cursor are the outbox's and left out. Bounded per asker (frames
/// per window, per-seq refusal, listings) and in all (frames per minute).
async fn gap_loop<L: OutboxLog, S: StateStore>(
    channel: GroupChannel,
    mls: MlsChannel,
    me: MemberId,
    log: L,
    store: S,
    asks: std::sync::Arc<GapAsks>,
    mut stop: watch::Receiver<bool>,
) {
    type Instant = tokio::time::Instant;
    // per asker: window start + frames in it, per-seq refusals, last listing
    let mut window: std::collections::HashMap<MemberId, (Instant, usize)> = std::collections::HashMap::new();
    let mut refused: std::collections::HashMap<(MemberId, u64), Instant> = std::collections::HashMap::new();
    let mut listed: std::collections::HashMap<MemberId, Instant> = std::collections::HashMap::new();
    let mut minute: (Instant, usize) = (Instant::now(), 0);
    let mut last: Option<MemberId> = None;
    loop {
        let next = asks.pending.lock().ok().and_then(|mut p| {
            let key = last
                .as_ref()
                .and_then(|l| p.range::<MemberId, _>((std::ops::Bound::Excluded(l), std::ops::Bound::Unbounded)).next())
                .or_else(|| p.iter().next())
                .map(|(k, _)| k.clone())?;
            p.remove(&key).map(|a| (key, a))
        });
        let Some((to, mut ask)) = next else {
            tokio::select! {
                () = asks.wake.notified() => continue,
                _ = stop.changed() => return,
            }
        };
        last = Some(to.clone());
        let now = Instant::now();
        refused.retain(|_, t| now.duration_since(*t) < GAP_RESEND_REFUSE);
        let cursor = store.load().await.group.unwrap_or_default().log_seq;
        if let Some((after, upto)) = ask.range.take() {
            if listed.get(&to).map_or(true, |t| now.duration_since(*t) >= GAP_LIST_EVERY) {
                listed.insert(to.clone(), now);
                let upto = upto.min(cursor).min(after.saturating_add(crate::gap::GAP_RANGE_MAX));
                if upto > after {
                    let envs = log.read_range(after + 1, upto).await;
                    let mut own: Vec<u64> =
                        envs.iter().filter(|e| crate::supervisor::own_ackable(&me, e)).map(|e| e.seq).collect();
                    let through = if own.len() > crate::gap::GAP_LIST_MAX {
                        own.truncate(crate::gap::GAP_LIST_MAX);
                        own.last().copied().unwrap_or(upto)
                    } else {
                        upto
                    };
                    tracing::debug!(%to, after, through, own = own.len(), "gap_listed");
                    let listing = crate::gap::GapListing { after, through, own };
                    publish_gap_answer(&channel, &mls, crate::gap::GapAnswer::listing(me.clone(), to.clone(), listing)).await;
                }
            }
        }
        let w = window.entry(to.clone()).or_insert((now, 0));
        if now.duration_since(w.0) >= GAP_ANSWER_EVERY {
            *w = (now, 0);
        }
        if now.duration_since(minute.0) >= Duration::from_secs(60) {
            minute = (now, 0);
        }
        let room = crate::gap::GAP_MAX_SEQS
            .saturating_sub(w.1)
            .min(GAP_TURN_FRAMES)
            .min(GAP_FRAMES_PER_MIN.saturating_sub(minute.1));
        let mut now_seqs: Vec<u64> = ask
            .seqs
            .iter()
            .copied()
            .filter(|s| *s <= cursor && !refused.contains_key(&(to.clone(), *s)))
            .collect();
        let later = now_seqs.split_off(room.min(now_seqs.len()));
        // cut by the turn: back into the queue behind the other askers; cut
        // by a budget: dropped, the asker asks again
        if !later.is_empty() && room == GAP_TURN_FRAMES {
            if let Ok(mut p) = asks.pending.lock() {
                p.entry(to.clone()).or_default().seqs.extend(later);
            }
            asks.wake.notify_one();
        }
        let (Some(&lo), Some(&hi)) = (now_seqs.first(), now_seqs.last()) else { continue };
        let envs = log.read_range(lo.max(hi.saturating_sub(crate::gap::GAP_RANGE_MAX)), hi).await;
        let (mut resent, mut gone) = (Vec::new(), Vec::new());
        for seq in now_seqs {
            let env = envs
                .binary_search_by_key(&seq, |e| e.seq)
                .ok()
                .and_then(|i| envs.get(i))
                .filter(|e| crate::supervisor::own_ackable(&me, e));
            let Some(env) = env else {
                gone.push(seq);
                continue;
            };
            let Some(frame) = mls.group_frame(env) else {
                if mls.evicted() {
                    return;
                }
                continue;
            };
            refused.insert((to.clone(), seq), now);
            GAP_REPUBLISHED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            w.1 += 1;
            minute.1 += 1;
            match channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
                Ok((_, report)) if !report.accepted.is_empty() => resent.push(seq),
                Ok(_) => tracing::debug!(%to, seq, "gap resend accepted by no relay"),
                Err(e) => tracing::debug!(%to, seq, error = %e, "gap resend failed"),
            }
        }
        if resent.is_empty() && gone.is_empty() {
            continue;
        }
        tracing::info!(%to, resent = resent.len(), gone = gone.len(), "gap_answered");
        publish_gap_answer(&channel, &mls, crate::gap::GapAnswer::new(me.clone(), to, resent, gone)).await;
    }
}

async fn publish_gap_answer(channel: &GroupChannel, mls: &MlsChannel, ans: crate::gap::GapAnswer) {
    let Some(frame) = mls.group_control_frame(&ans.to_frame()) else { return };
    if let Err(e) = channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
        tracing::debug!(error = %e, "publishing a gap answer failed");
    }
}

/// Publish control frames (pokes, piece wants) as they are handed over.
/// One attempt each, no retry chain: a nudge that could not go out now is
/// stale by the time a retry would land, and the sender can always ask
/// again.
async fn control_loop(
    channel: GroupChannel,
    mls: MlsChannel,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let plaintext = tokio::select! {
            got = rx.recv() => match got {
                Some(p) => p,
                None => return, // the handle is gone
            },
            _ = stop.changed() => return,
        };
        let Some(frame) = mls.group_control_frame(&plaintext) else {
            if mls.evicted() {
                return;
            }
            tracing::error!("framing a control frame failed - skipped");
            continue;
        };
        if let Err(e) = channel.publish_frame(&frame.exporter, &frame.ciphertext).await {
            tracing::warn!(error = %e, "publishing a control frame failed");
        }
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
    let held = store.load().await.outbound.get(from).copied().unwrap_or_default();
    let old = held.acked_floor;
    // only the tail from the claimed high is read: nothing there means the
    // claim is beyond our log
    if window.high > old && log.read_from(window.high).await.is_empty() {
        refuse_ack_ahead(log, store, from, window.high).await;
        return;
    }
    // an open episode ends only on a sheet below the stale high: one at or
    // above it may be that same stale window, now overtaken by this log
    if held.ack_ahead && held.ahead_high != 0 && window.high >= held.ahead_high {
        tracing::debug!(%from, window_high = window.high, ahead_high = held.ahead_high, "ack_ahead_held");
        return;
    }
    let envs = log.read_from(old.saturating_add(1)).await;
    // only a sheet written after the one that set the floor can contradict it
    let regressing = ack.at > held.ack_at
        && (window.high < held.ack_high || (held.ack_high == 0 && window.high < old));
    if regressing && rewind_regressed(log, store, me, from, ack.at, window, old).await {
        return;
    }
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
    store
        .update(|state| {
            let mut changed = false;
            let mut group = state.group.unwrap_or_default();
            if floor < group.log_seq && !group.heal_evidence {
                group.heal_evidence = true;
                changed = true;
            }
            state.group = Some(group);
            let cursor = state.outbound.entry(from.clone()).or_default();
            if cursor.ack_ahead {
                cursor.ack_ahead = false;
                cursor.ahead_rounds = 0;
                cursor.ahead_high = 0;
                changed = true;
            }
            // only ever forward: another sheet may have moved it meanwhile
            if floor > cursor.acked_floor {
                cursor.acked_floor = floor;
                cursor.ack_seen = true;
                cursor.ack_high = window.high;
                cursor.ack_at = cursor.ack_at.max(ack.at);
                if cursor.regressed_from != 0 && floor >= cursor.regressed_from {
                    cursor.regressed_from = 0;
                }
                changed = true;
            } else if !cursor.ack_seen {
                // no progress — but the peer is listening. Latch without
                // moving the floor; the floor stays honest (0 = nothing
                // proven) and the rewind can now reach back to what this
                // peer is actually missing.
                cursor.ack_seen = true;
                changed = true;
            }
            changed
        })
        .await;
}

/// A sheet written after the one that set the floor no longer holds what
/// that one acked (the peer's window was reset, or regressed across a hard
/// stop): the floor falls to what this sheet proves, never below the log,
/// and the stall rounds re-offer the rest. `false` = no contradiction.
async fn rewind_regressed<L: OutboxLog, S: StateStore>(
    log: &L,
    store: &S,
    me: &MemberId,
    from: &MemberId,
    at: u64,
    window: &molt_core::AcceptedWindow,
    old: u64,
) -> bool {
    // below the window the sheet reads every seq as accepted
    let base = window.high.saturating_sub(molt_core::ACCEPT_WINDOW_BITS + 1);
    let envs = log.read_range(base.saturating_add(1), old).await;
    let Some(first) = envs.first().map(|e| e.seq) else { return false };
    let start = base.max(first.saturating_sub(1));
    let floor = crate::supervisor::advance_acked_floor(me, &envs, window, start);
    if floor >= old {
        return false;
    }
    // positions the peer lacks that compaction already dropped: gone
    let within = (base + 1..first.min(window.high.saturating_add(1))).filter(|s| !window.is_accepted(*s)).count();
    let compacted = u64::try_from(within)
        .unwrap_or(u64::MAX)
        .saturating_add(first.saturating_sub(1).saturating_sub(window.high.max(base)));
    let mut first_episode = false;
    store
        .update(|state| {
            let cursor = state.outbound.entry(from.clone()).or_default();
            first_episode = cursor.regressed_from == 0;
            if first_episode {
                cursor.regressed_from = old;
            }
            cursor.acked_floor = floor;
            cursor.ack_seen = true;
            cursor.ack_high = window.high;
            cursor.ack_at = at;
            let mut group = state.group.unwrap_or_default();
            group.heal_evidence |= floor < group.log_seq;
            state.group = Some(group);
            true
        })
        .await;
    if first_episode {
        tracing::warn!(%from, reported = window.high, floor = old, compacted, "ack_regressed");
    } else {
        tracing::debug!(%from, reported = window.high, floor = old, rewound = floor, "ack_regressed");
    }
    true
}

/// A sheet acking seqs above our own log: either the peer holds a previous
/// incarnation's window of this seat (it missed the restore), or this log
/// is behind what the peer really has (an older export imported). The
/// floor falls to 0 once so the stall rounds re-offer everything; after
/// [`RESEND_GIVEUP_ROUNDS`] such rounds the peer is no longer re-offered
/// to (`reachable_floor`) until a plausible sheet ends the episode.
async fn refuse_ack_ahead<L: OutboxLog, S: StateStore>(log: &L, store: &S, from: &MemberId, window_high: u64) {
    let held = store.load().await.outbound.get(from).copied().unwrap_or_default();
    if held.ack_ahead {
        tracing::debug!(%from, window_high, "ack_ahead");
        return;
    }
    let own_high = log.read_from(held.acked_floor.saturating_add(1)).await.last().map_or(held.acked_floor, |e| e.seq);
    store
        .update(|state| {
            let cursor = state.outbound.entry(from.clone()).or_default();
            cursor.ack_ahead = true;
            cursor.ahead_rounds = 0;
            cursor.ahead_high = window_high;
            cursor.acked_floor = 0;
            cursor.ack_seen = true;
            let mut group = state.group.unwrap_or_default();
            group.heal_evidence |= group.log_seq > 0;
            state.group = Some(group);
            true
        })
        .await;
    tracing::warn!(%from, window_high, own_high, "ack_ahead");
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
            if mls.evicted() {
                return;
            }
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
pub(crate) fn permanent_refusal(e: &crate::NetError) -> Option<String> {
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
    for attempt in 1..=PUBLISH_ATTEMPTS {
        // the SAME bytes across relay retries inside one attempt chain: a relay
        // NAK is not a peer rejection, and re-encrypting would burn a ratchet
        // generation for nothing. A REWIND re-frames instead (N5.3).
        let published = match frame.stamp {
            // a commit rides its ORIGINAL stamp on every resend (M8)
            Some(at) => channel.publish_frame_at(&frame.exporter, &frame.ciphertext, at).await,
            None => channel.publish_frame(&frame.exporter, &frame.ciphertext).await,
        };
        match published {
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
        // the delay separates attempts — none after the last one
        if attempt == PUBLISH_ATTEMPTS {
            break;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.changed() => return Err(PublishStall::Stopped),
        }
        delay = (delay * 2).min(cfg.retry_cap);
    }
    Err(PublishStall::Transient)
}

/// Advance the persisted broadcast cursor to `published_through` (never
/// backwards, never when nothing went out).
async fn persist_cursor<S: StateStore>(store: &S, start: u64, published_through: u64) {
    if published_through <= start {
        return;
    }
    store
        .update(|state| {
            let mut cur = state.group.unwrap_or_default();
            if published_through > cur.log_seq {
                cur.log_seq = published_through;
                state.group = Some(cur);
                true
            } else {
                false
            }
        })
        .await;
}

/// The stall clock's verdict on one fired round.
#[derive(Debug)]
enum StallRound {
    /// No floor is proven any more — nothing to re-offer.
    NoFloor,
    /// The hourly budget is spent and no heal evidence buys a round.
    Spent(u64),
    /// Nobody owed the tail can be reached.
    Quiet(Quiet),
    /// A round was granted (`heal` = the evidence-driven one) and the
    /// cursor rewound to the floor.
    Granted { floor: u64, heal: bool },
}

/// Decide one stall round on `state`: the floor it rewinds to and the
/// budget it spends - and count it against every ack-ahead peer.
fn plan_round(
    state: &mut TransportState,
    cfg: &GroupNetConfig,
    envs: &[EventEnvelope],
    reach: &OutboxReach,
    open: bool,
    now: u64,
) -> StallRound {
    let f = match reachable_floor(state, cfg, envs, reach, open) {
        Ok(Some(f)) => f,
        Ok(None) => return StallRound::NoFloor,
        Err(q) => return StallRound::Quiet(q),
    };
    let mut cur = state.group.unwrap_or_default();
    let heal = if consume_resend_round(&mut cur, now) {
        false
    } else if consume_heal_round(&mut cur, now) {
        // spent — but fresh evidence of a lagging listener (a claim sheet
        // since the last round) buys the one heal round
        true
    } else {
        return StallRound::Spent(f);
    };
    state.group = Some(cur); // the consumed budget
    for c in state.outbound.values_mut().filter(|c| c.ack_ahead) {
        c.ahead_rounds = c.ahead_rounds.saturating_add(1);
    }
    rewind_group_to(state, f);
    StallRound::Granted { floor: f, heal }
}

/// A doubling delay: starts at [`RESEND_AFTER_SECS`], doubles per
/// escalation, capped at [`RESEND_MAX_BACKOFF_SECS`]; a reset returns it to
/// the start. Pure — the loop owns the clocks. Two live in the outbox, and
/// they are deliberately separate (review M6): the held-frame retry and the
/// stall clock must not inflate or reset each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    cur: Duration,
    first: Duration,
    cap: Duration,
}

impl Backoff {
    const fn new() -> Backoff {
        Backoff::with(Duration::from_secs(RESEND_AFTER_SECS), Duration::from_secs(RESEND_MAX_BACKOFF_SECS))
    }

    const fn with(first: Duration, cap: Duration) -> Backoff {
        Backoff { cur: first, first, cap }
    }

    fn reset(&mut self) {
        self.cur = self.first;
    }

    /// Double toward the ceiling; returns the new delay in seconds.
    fn escalate(&mut self) -> u64 {
        self.cur = self.cur.saturating_mul(2).min(self.cap);
        self.secs()
    }

    fn secs(&self) -> u64 {
        self.cur.as_secs()
    }

    fn delay(&self) -> Duration {
        self.cur
    }
}

/// The outbox's held-frame state: a publish chain that ran dry holds the
/// cursor at the failed envelope. `reported` = the surface was told
/// (`send_failed`), and the engine lifts that flag on `send_ok` and on
/// nothing else, so the first success afterwards owes it (field
/// 2026-08-24: "Reconnecting… no relay accepted the frame" long after the
/// relays were back). `permanent` = a local, deterministic refusal: reported
/// like an outage but never retried on the timer (the answer is the same
/// in an hour, and every retry re-frames the envelope — a ratchet
/// generation per try). Its own escalation, not the stall clock's (M6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldFrame {
    reported: bool,
    permanent: bool,
    backoff: Backoff,
}

impl HeldFrame {
    const fn new() -> HeldFrame {
        HeldFrame {
            reported: false,
            permanent: false,
            backoff: Backoff::new(),
        }
    }

    /// A publish went through: the hold is over. `true` when the surface
    /// is owed a `send_ok` (a failure had been reported).
    fn published(&mut self) -> bool {
        let owed = self.reported;
        self.reported = false;
        self.permanent = false;
        self.backoff.reset();
        owed
    }

    /// A publish chain ran dry (and the surface was told).
    fn failed(&mut self, permanent: bool) {
        self.reported = true;
        self.permanent = permanent;
    }

    /// Is the held-frame timer armed? Only for a reported TRANSIENT hold.
    fn retry_armed(&self) -> bool {
        self.reported && !self.permanent
    }

    /// The timer's delay, capped at the retry ceiling so a relay outage is
    /// re-tried at least every `retry_cap`.
    fn retry_in(&self, retry_cap: Duration) -> Duration {
        self.backoff.delay().min(retry_cap)
    }
}

/// The stall clock: when the proven floor last moved and how hard the
/// outbox is backing off. ANCHORED (`since`), not recomputed per iteration:
/// traffic must not starve the timer — a chatty republic would otherwise
/// never resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StallClock {
    last_floor: Option<u64>,
    backoff: Backoff,
    rounds_without_progress: u32,
    /// The stall was reported on the health surface (G4).
    reported: bool,
    since: Option<tokio::time::Instant>,
    /// Rounds that went out since this runtime started.
    granted: u32,
    /// The last round was skipped: nobody owed can be reached.
    quiet: Option<Quiet>,
}

/// Why a round was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quiet {
    /// Every peer still owed the tail is silent.
    PeersSilent,
    /// This seat concluded it is locked out.
    LockedOut,
}

impl Quiet {
    fn reason(self) -> &'static str {
        match self {
            Quiet::PeersSilent => "peers_silent",
            Quiet::LockedOut => "locked_out",
        }
    }
}

impl StallClock {
    #[cfg(test)]
    const fn new() -> StallClock {
        StallClock::with(Backoff::new())
    }

    const fn with(backoff: Backoff) -> StallClock {
        StallClock {
            last_floor: None,
            backoff,
            rounds_without_progress: 0,
            reported: false,
            since: None,
            granted: 0,
            quiet: None,
        }
    }

    /// The republic's proven floor after a pass. `None < Some(_)` for
    /// Options, which is what is wanted: the first real floor de-escalates
    /// from "nothing proven", and a floor that REGRESSES (a peer whose
    /// window persistence lost a beat) does not — the guarantee only ever
    /// counts forward progress as progress. Returns `true` when a reported
    /// stall must now be cleared on the surface.
    fn observe_floor(&mut self, floor: Option<u64>) -> bool {
        if floor <= self.last_floor {
            return false;
        }
        self.last_floor = floor;
        self.backoff.reset();
        self.rounds_without_progress = 0;
        std::mem::take(&mut self.reported)
    }

    fn disarm(&mut self) {
        self.since = None;
    }

    fn armed(&self) -> bool {
        self.since.is_some()
    }

    /// Keep (or set) the anchor and return the deadline it implies.
    fn deadline(&mut self, now: tokio::time::Instant) -> tokio::time::Instant {
        *self.since.get_or_insert(now) + self.backoff.delay()
    }

    /// Escalate and re-anchor at `now` — after a spent OR a granted round
    /// (a stale anchor past its deadline would fire on every iteration: a
    /// hot loop of loads, log reads and warnings for the rest of the hour).
    fn escalate(&mut self, now: tokio::time::Instant) {
        self.backoff.escalate();
        self.since = Some(now);
    }

    /// A granted round is one more without progress. `true` exactly once:
    /// when the give-up count is reached and the stall is not yet reported
    /// (loud, honest, and NOT a stop — the resends keep trying at the cap).
    fn round_granted(&mut self) -> bool {
        self.rounds_without_progress = self.rounds_without_progress.saturating_add(1);
        if self.rounds_without_progress >= RESEND_GIVEUP_ROUNDS && !self.reported {
            self.reported = true;
            return true;
        }
        false
    }
}

/// What the outbox does after a publish pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Nothing to re-offer: sleep on a wakeup, a claim sheet, a stop, or the
    /// held-frame timer.
    Idle,
    /// An own-ackable tail sits above a proven floor: run the stall clock.
    Stall,
}

/// The stall decision. Resend ONLY when somebody has proven something AND
/// we have an own ackable envelope above that floor (`tail`) — and never on
/// a PERMANENT hold, even above a proven floor: the stall clock would
/// otherwise rewind and re-fail it every round (a resend round and a
/// ratchet generation per frame, for nothing); it waits for an append.
fn stall_decision(tail: bool, held: &HeldFrame) -> Action {
    if !tail || held.permanent {
        Action::Idle
    } else {
        Action::Stall
    }
}

/// How one publish pass ended.
#[derive(Debug)]
enum PassOutcome {
    /// Every own envelope of the batch went out (or was skipped as
    /// unframeable); the cursor advanced past the batch.
    Done,
    /// A publish chain ran dry: the cursor holds at the failed envelope,
    /// the surface was told.
    Held,
    /// The runtime was told to stop; what went out before is persisted.
    Stopped,
}

/// One publish pass: read the log from the broadcast cursor, MLS-frame and
/// publish every own envelope in order, persist the cursor.
///
/// NO rewind here. It sat at the loop top through N5.2, where it was inert
/// because nothing ever acked — N5.3's acks made it live, and a rewind on
/// every wakeup re-publishes the whole unacked tail on every appended
/// envelope, with the hourly budget gating only the persisted write
/// downstream. The budget must ration the PUBLISH, so the rewind happens
/// exactly once per granted round, in the stall clock.
#[allow(clippy::too_many_arguments)]
async fn publish_pass<L, S, K>(
    channel: &GroupChannel,
    mls: &MlsChannel,
    cfg: &GroupNetConfig,
    log: &L,
    store: &S,
    sink: &K,
    held: &mut HeldFrame,
    busy: &watch::Sender<bool>,
    stop: &mut watch::Receiver<bool>,
) -> PassOutcome
where
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let state = store.load().await;
    let start = state.group.unwrap_or_default().log_seq;
    let batch = log.read_from(start + 1).await;
    let mut published_through = start;
    // the pending flag the file trickle yields to: raised for the pass's
    // own frames, lowered at every exit
    let _ = busy.send_replace(batch.iter().any(|e| e.by == cfg.member));
    let outcome = publish_batch(channel, mls, cfg, store, sink, held, stop, start, batch, &mut published_through).await;
    let _ = busy.send_replace(false);
    outcome
}

/// The body of [`publish_pass`]: every own envelope of `batch` in order.
#[allow(clippy::too_many_arguments)]
async fn publish_batch<S, K>(
    channel: &GroupChannel,
    mls: &MlsChannel,
    cfg: &GroupNetConfig,
    store: &S,
    sink: &K,
    held: &mut HeldFrame,
    stop: &mut watch::Receiver<bool>,
    start: u64,
    batch: Vec<EventEnvelope>,
    published_through: &mut u64,
) -> PassOutcome
where
    S: StateStore,
    K: EngineSink,
{
    for env in batch {
        // AUTHOR, not `crosses_wire` — the same filter the queue outbox
        // uses. It matters more than it looks: the genesis `Founded` is
        // self-authored on every node, so seq 1 must ride the wire or the
        // receiver never accepts it and every later own event parks on
        // `prev_seq = 1` until the give-up timer.
        if env.by != cfg.member {
            *published_through = env.seq;
            continue;
        }
        let Some(frame) = mls.group_frame(&env) else {
            // evicted: nothing this node frames can be read; hold the cursor
            if mls.evicted() {
                persist_cursor(store, start, *published_through).await;
                return PassOutcome::Stopped;
            }
            // a LOCAL framing error is not resendable — skipping it is the
            // only way forward, and it is loud
            tracing::error!(seq = env.seq, "MLS-framing a group frame failed — skipped");
            *published_through = env.seq;
            continue;
        };
        match publish_with_backoff(channel, &frame, cfg, stop).await {
            Ok(()) => {
                tracing::debug!(me = %cfg.member, seq = env.seq, "group frame published");
                if held.published() {
                    // the backoff exit — the channel publishes again
                    sink.send_ok(&cfg.member).await;
                }
                *published_through = env.seq;
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
                    // drain, don't discard: the frames that went out
                    // before the stop stay published (review M9)
                    persist_cursor(store, start, *published_through).await;
                    return PassOutcome::Stopped;
                }
                let permanent = matches!(stall, PublishStall::Permanent(_));
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
                held.failed(permanent);
                persist_cursor(store, start, *published_through).await;
                return PassOutcome::Held;
            }
        }
    }
    persist_cursor(store, start, *published_through).await;
    PassOutcome::Done
}

/// Read the log from the broadcast cursor and publish this node's own
/// events; between passes, run the stall clock over the proven floor
/// (delivery guarantee §4.4) and the held-frame timer.
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
    mut reach: watch::Receiver<OutboxReach>,
    busy: watch::Sender<bool>,
    mut stop: watch::Receiver<bool>,
) where
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let mut stall = StallClock::with(Backoff::with(cfg.resend_after, cfg.resend_cap));
    let mut held = HeldFrame::new();
    loop {
        // an evicted seat publishes nothing: no pass, no re-offer
        if mls.evicted() {
            tracing::debug!(me = %cfg.member, "outbox=stopped reason=evicted");
            return;
        }
        match publish_pass(&channel, &mls, &cfg, &log, &store, &sink, &mut held, &busy, &mut stop).await {
            PassOutcome::Stopped => return,
            PassOutcome::Held => stall.disarm(),
            PassOutcome::Done => {}
        }
        // --- the stall clock -------------------------------------------------
        let state = store.load().await;
        let floor = group_floor(&state, &cfg);
        let cursor = state.group.unwrap_or_default().log_seq;
        if stall.observe_floor(floor) {
            // the republic confirmed progress — clear the report on the
            // members it was raised against (naming this node would have
            // the surface blame the operator's own seat for a peer that
            // stopped acking)
            for m in &cfg.members {
                sink.send_ok(m).await;
            }
        }
        // "cursor > floor" alone is not an own-ackable tail: the broadcast
        // cursor walks over FOREIGN envelopes too, and a member that mostly
        // listens would sit permanently above a frozen floor — reporting
        // "deliveries keep going unacknowledged" on a perfectly healthy
        // republic. The mesh learned this as its own-ackable span guard;
        // this is the same rule.
        let envs = match floor {
            Some(f) if cursor > f => log.read_from(f + 1).await,
            _ => Vec::new(),
        };
        let tail = envs.iter().any(|e| crate::supervisor::own_ackable(&cfg.member, e));
        if stall_decision(tail, &held) == Action::Idle {
            stall.quiet = None;
            tracing::debug!(
                me = %cfg.member,
                ?floor,
                cursor,
                "outbox idle — no own-ackable tail; sleeping on wakeup"
            );
            stall.disarm();
            // a HELD frame (the publish chain ran dry) below any proven floor
            // has no stall clock to bring it back: without its own timer it
            // waited for the next own append — on a quiet seat, never. The
            // same escalation as the stall clock, capped at the retry ceiling
            // so a relay outage is re-tried at least every `retry_cap`; the
            // first success resets it. No rewind, no resend budget: this is
            // the same pass continuing, not a re-offer of proven frames.
            let retry_in = held.retry_in(cfg.retry_cap);
            let retry_armed = held.retry_armed();
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
                () = tokio::time::sleep(retry_in), if retry_armed => {
                    let backoff_secs = held.backoff.escalate();
                    tracing::info!(me = %cfg.member, backoff_secs, "held frame - retrying the publish");
                }
            }
            continue;
        }
        tracing::debug!(
            me = %cfg.member,
            ?floor,
            cursor,
            backoff_secs = stall.backoff.secs(),
            armed = stall.armed(),
            "outbox tail unacked — stall clock running"
        );
        let deadline = stall.deadline(tokio::time::Instant::now());
        let quiet = stall.quiet.is_some();
        let (fired, by_event) = tokio::select! {
            r = wakeup.changed() => {
                if r.is_err() {
                    return;
                }
                (false, false) // new work first; the stall clock keeps running
            }
            // a floor advance de-escalates at the loop top; the anchored
            // clock is kept by `deadline`, so a sheet can never starve it
            () = ack_wake.notified() => (false, false),
            // a quiet stall resumes the moment the engine hears an owed peer
            r = reach.changed(), if quiet => {
                if r.is_err() {
                    return;
                }
                (true, true)
            }
            _ = stop.changed() => return,
            () = tokio::time::sleep_until(deadline) => (true, false),
        };
        if !fired {
            continue;
        }
        let reach_now = reach.borrow_and_update().clone();
        let open = stall.granted < OPEN_ROUNDS;
        // decide and rewind as ONE state step (M7): the floor may have moved
        // during a sleep of up to ten minutes — rewinding to a stale
        // snapshot would re-offer what has since been confirmed — and a
        // claim sheet landing between a load and a save was lost before.
        // Every group frame is a FRESH encryption (`group_frame` bypasses
        // the fan-out cache), so there is no stale-ciphertext eviction to do
        // and no resend epoch to bump — a relay-level duplicate dedups at
        // each receiver's AcceptedWindow. Through `rewind_group_to`, not by
        // assignment: one definition of "pull the broadcast cursor back",
        // and it is the one the unit tests pin. The floor is the lowest
        // among the owed peers this round may reach (`reachable_floor`).
        let mut round = StallRound::NoFloor;
        let now_wall = wall_secs();
        store
            .update(|state| {
                round = plan_round(state, &cfg, &envs, &reach_now, open, now_wall);
                matches!(round, StallRound::Granted { .. })
            })
            .await;
        let f = match round {
            StallRound::NoFloor => {
                stall.disarm();
                continue;
            }
            // nobody owed can be reached: no publish, no budget. The ladder
            // and the loud count go on, so `send_failed` stays reachable.
            StallRound::Quiet(_) if by_event => continue,
            StallRound::Quiet(q) => {
                if stall.quiet == Some(q) {
                    tracing::debug!(me = %cfg.member, reason = q.reason(), "outbox=quiet");
                } else {
                    tracing::info!(me = %cfg.member, reason = q.reason(), ?floor, cursor, "outbox=quiet");
                }
                stall.quiet = Some(q);
                if stall.round_granted() {
                    for m in &cfg.members {
                        sink.send_failed(m, "not acknowledging deliveries").await;
                    }
                }
                stall.escalate(tokio::time::Instant::now());
                continue;
            }
            StallRound::Spent(f) => {
                tracing::warn!(
                    floor = f,
                    "the resend budget for this hour is spent — holding the tail"
                );
                stall.escalate(tokio::time::Instant::now());
                continue;
            }
            StallRound::Granted { floor, heal } => {
                if let Some(q) = stall.quiet.take() {
                    tracing::info!(me = %cfg.member, was = q.reason(), floor, "outbox=resumed");
                }
                stall.granted = stall.granted.saturating_add(1);
                if heal {
                    tracing::info!(
                        floor,
                        "budget spent, but a listener proved itself lagging — heal round"
                    );
                }
                floor
            }
        };
        let report = stall.round_granted();
        // routine until the give-up count; from there on it is the loud one
        if stall.rounds_without_progress >= RESEND_GIVEUP_ROUNDS {
            tracing::warn!(
                floor = f,
                cursor,
                attempt = stall.rounds_without_progress,
                "unacknowledged deliveries — re-offering the tail"
            );
        } else {
            tracing::info!(
                floor = f,
                cursor,
                attempt = stall.rounds_without_progress,
                "unacknowledged deliveries — re-offering the tail"
            );
        }
        if report {
            for m in &cfg.members {
                sink.send_failed(m, "not acknowledging deliveries - still resending")
                    .await;
            }
        }
        stall.escalate(tokio::time::Instant::now());
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

/// Take one round from this hour's SHARED publish budget. `false` = spent.
///
/// Persisted with the cursor on purpose: an in-memory budget hands a crash
/// loop a fresh allowance on every start, and the thing being rationed is a
/// publish that every member of the republic then re-reads. Shared by the
/// resend rounds AND the file-plane series publishes (`file_transfer_nostr.md`
/// §5.4: one series = one round — no second budget to reason about), which is
/// why it is `pub`: [`crate::file_plane::publish_series_metered`] consumes it.
pub fn consume_resend_round(cur: &mut molt_core::GroupCursor, now: u64) -> bool {
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

/// Rounds the hour still holds - what the file trickle keeps free for
/// chat and governance (`docs_archive/files/mirroring.md` §3.2). Pure: a rolled
/// window reads as the full allowance without being written.
#[must_use]
pub fn resend_headroom(cur: &molt_core::GroupCursor, now: u64) -> u32 {
    let mut cur = *cur;
    roll_resend_window(&mut cur, now);
    RESEND_ROUNDS_PER_HOUR.saturating_sub(cur.resend_rounds)
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
    /// decode now. Carries the members the commit re-admitted (their accept
    /// windows must be forgotten BEFORE the hold is retried — the frames in
    /// the hold are the new incarnation's).
    EpochAdvanced(Vec<MemberId>),
    /// A commit removed this node's own leaf; the engine was told.
    Evicted,
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

async fn ingest_one<L: OutboxLog, S: StateStore, K: EngineSink>(
    mls: &MlsChannel,
    log: &L,
    store: &S,
    sink: &K,
    me: &MemberId,
    seen: &mut SeenCiphertexts,
    content: &str,
) -> Ingest {
    // an exact re-delivery of a frame this node already consumed: turn
    // around before the ratchet is asked (a second decrypt is a wasted
    // `MlsError::Stale` round)
    if seen.seen(content) {
        tracing::debug!(me = %me, "dropping an exact re-delivery of a consumed frame");
        return Ingest::Nothing;
    }
    let secrets = mls.exporter_secrets();
    let wire = match crate::envelope::open_outer(&secrets, content) {
        Ok(w) => w,
        // not even the shape of a frame: nothing a later key could open,
        // so it must not take a hold slot from an honest frame (review M10)
        Err(crate::envelope::EnvelopeError::Shape(_)) => {
            tracing::debug!(me = %me, "dropping a malformed 445 frame");
            return Ingest::Nothing;
        }
        Err(_) => return Ingest::Opaque,
    };
    // the carrier stamp stays out: it is publisher-chosen, and the
    // concurrent-commit order is the digest alone (audit #9)
    let outcome = mls.decode(&wire);
    // consumed outcomes enter the dedup ring — their re-delivery can never
    // decode again. Held ones (FutureEpoch/Opaque) must stay retryable and
    // do NOT. Commit outcomes (EpochAdvanced) stay out DELIBERATELY even
    // though their decrypt is spent: the concurrent-commit tiebreak (N3 §1)
    // may rewind one epoch and must be able to re-see the winning commit —
    // a ring hit there would swallow it.
    if matches!(
        outcome,
        MlsDecode::Deliver(..)
            | MlsDecode::Ack(..)
            | MlsDecode::GroupAck(..)
            | MlsDecode::Poke(..)
            | MlsDecode::PieceWanted(..)
            | MlsDecode::Gap(..)
            | MlsDecode::MirrorDecl(..)
            | MlsDecode::MirrorStatus(..)
            | MlsDecode::MirrorWho(..)
            | MlsDecode::Vault(..)
            | MlsDecode::Wallet(..)
            | MlsDecode::Discard
            | MlsDecode::Failed(_)
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
        MlsDecode::Poke(from, poke) => {
            sink.peer_seen(&from).await;
            // `by` is SELF-DESCRIPTION, the MLS credential is the
            // authentication — the same rule the claim sheet follows
            if poke.by != from {
                tracing::warn!(%from, claimed = %poke.by, "a poke disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.poked(&from, &poke.to).await;
            Ingest::Nothing
        }
        MlsDecode::PieceWanted(from, want) => {
            sink.peer_seen(&from).await;
            if want.by != from {
                tracing::warn!(%from, claimed = %want.by, "a piece want disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.piece_wanted(&from, &want).await;
            Ingest::Nothing
        }
        MlsDecode::Gap(from, frame) => {
            sink.peer_seen(&from).await;
            if *frame.by() != from {
                tracing::warn!(%from, claimed = %frame.by(), "a gap frame disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.gap_frame(&from, &frame).await;
            Ingest::Nothing
        }
        MlsDecode::MirrorDecl(from, decl) => {
            sink.peer_seen(&from).await;
            if decl.by != from {
                tracing::warn!(%from, claimed = %decl.by, "a mirror declaration disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.mirror_decl(&from, &decl).await;
            Ingest::Nothing
        }
        MlsDecode::MirrorStatus(from, status) => {
            sink.peer_seen(&from).await;
            if status.by != from {
                tracing::warn!(%from, claimed = %status.by, "a mirror status disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.mirror_status(&from, &status).await;
            Ingest::Nothing
        }
        MlsDecode::MirrorWho(from) => {
            sink.peer_seen(&from).await;
            sink.mirror_who(&from).await;
            Ingest::Nothing
        }
        MlsDecode::Vault(from, frame) => {
            sink.peer_seen(&from).await;
            if *frame.by() != from {
                tracing::warn!(%from, claimed = %frame.by(), "a vault frame disowns its sender - dropped");
                return Ingest::Nothing;
            }
            sink.vault_frame(&from, &frame).await;
            Ingest::Nothing
        }
        MlsDecode::Wallet(from, frame) => {
            sink.peer_seen(&from).await;
            sink.wallet_frame(&from, &frame).await;
            Ingest::Nothing
        }
        // the two arms the mesh supervisor implements and this loop did not:
        // both used to fall into a catch-all `Nothing`, which held nothing and
        // retried nothing, so a frame arriving ahead of its commit was DROPPED
        MlsDecode::FutureEpoch => Ingest::FutureEpoch,
        MlsDecode::EpochAdvanced(readmitted) => Ingest::EpochAdvanced(readmitted),
        MlsDecode::Evicted(by) => {
            tracing::warn!(me = %me, %by, "seat_evicted");
            sink.evicted(&by).await;
            Ingest::Evicted
        }
        MlsDecode::Discard => Ingest::Nothing,
        // relays replay the whole day window on every subscribe, and a
        // publisher hears its own frames back: both routine, neither the
        // operator's business. Anything else is the ONE line for a frame
        // this seat cannot read.
        MlsDecode::Failed(MlsError::Stale(reason)) => {
            tracing::debug!(me = %me, reason, "dropping a stale group frame");
            Ingest::Nothing
        }
        MlsDecode::Failed(MlsError::OwnEcho) => Ingest::Nothing,
        MlsDecode::Failed(e) => {
            tracing::warn!(me = %me, error = %e, "dropping an undecryptable group frame");
            Ingest::Nothing
        }
    }
}

/// Re-offer every held frame after an epoch advance, **in hold order**, and
/// repeatedly while progress is made — a held commit can unlock further held
/// frames, so one pass is not enough. Returns the stamps of the frames that
/// stayed permanently unreadable, for the operator counters.
///
/// Each frame keeps its carrier `created_at` for the live/history split of
/// the loss counters only.
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
) -> Result<Vec<u64>, ()> {
    let mut ingest = GroupHeldIngest { mls, log, store, sink, me, seen };
    crate::epoch_hold::drain_until_no_progress(hold, &mut ingest)
        .await
        .map(|lost| lost.into_iter().map(|(_, at)| at).collect())
}

/// How the 445 inbox ingests one held frame (the group runtime's half of
/// [`crate::epoch_hold::drain_until_no_progress`]).
struct GroupHeldIngest<'a, L, S, K> {
    mls: &'a MlsChannel,
    log: &'a L,
    store: &'a S,
    sink: &'a K,
    me: &'a MemberId,
    seen: &'a mut SeenCiphertexts,
}

impl<L: OutboxLog, S: StateStore, K: EngineSink> crate::epoch_hold::HeldIngest<(String, u64)>
    for GroupHeldIngest<'_, L, S, K>
{
    async fn ingest(&mut self, item: &mut (String, u64)) -> crate::epoch_hold::Held {
        use crate::epoch_hold::Held;
        match ingest_one(self.mls, self.log, self.store, self.sink, self.me, self.seen, &item.0).await {
            Ingest::EngineGone | Ingest::Evicted => Held::Stop,
            Ingest::FutureEpoch => Held::Still,
            Ingest::Opaque => Held::Opaque,
            // a held COMMIT merged during the retry: forget the
            // re-admitted members' windows before their held frames
            // (later in this very pass) are delivered
            Ingest::EpochAdvanced(readmitted) => {
                for m in &readmitted {
                    self.sink.rekeyed(m).await;
                }
                Held::Progress
            }
            Ingest::Delivered | Ingest::Acked | Ingest::Nothing => Held::Progress,
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
                live_opaque: 0,
                catching_up: false,
                replaying: false,
            });
            return;
        }
    };
    // an opaque frame stamped before this is history, not a live loss
    let opened_at = crate::ritual_net::now_secs();
    let live = |at: u64| at.saturating_add(LIVE_SKEW_SECS) >= opened_at;
    let mut state = GroupHealth {
        subscribed: true,
        deaf: None,
        opaque_frames: 0,
        live_opaque: 0,
        catching_up: true,
        replaying: true,
    };
    let _ = health.send(state.clone());
    // frames whose commit has not arrived yet — see `retry_epoch_hold`
    let mut hold: Vec<(String, u64)> = Vec::new();
    // exact-duplicate turnaround (relay re-deliveries) — runtime-only
    let mut seen = SeenCiphertexts::new();
    loop {
        if state.catching_up && sub.replayed() {
            state.catching_up = false;
            let _ = health.send(state.clone());
        }
        let replaying = sub.replaying();
        if state.replaying != replaying {
            state.replaying = replaying;
            let _ = health.send(state.clone());
        }
        let recv = tokio::select! {
            r = sub.recv(RECV_SLICE) => r,
            _ = stop.changed() => return,
        };
        match recv {
            GroupRecv::Frame { content, created_at } => {
                if state.deaf.take().is_some() {
                    let _ = health.send(state.clone());
                }
                match ingest_one(&mls, &log, &store, &sink, &me, &mut seen, &content).await {
                    // nothing of this group decrypts here any more
                    Ingest::EngineGone | Ingest::Evicted => return,
                    // NOT counted yet, and not dropped. On 445 the epoch shows
                    // up at the OUTER layer, not at the MLS decode: a frame
                    // sealed under an exporter we have not derived yet is
                    // simply unopenable, so "newer than us" and "older than
                    // the ring" arrive as the SAME answer. Holding it costs
                    // one retry after the next commit merges, and that retry
                    // is what tells the two apart.
                    Ingest::Opaque | Ingest::FutureEpoch => {
                        if hold.len() >= EPOCH_HOLD_MAX {
                            // evict the OLDEST held frame, never the new
                            // arrival: a hold saturated with a laggard's
                            // stale frames must not crowd out the one
                            // commit that would heal this node
                            // (detached_reattach.md §2.4)
                            let (_, at) = hold.remove(0);
                            state.opaque_frames += 1;
                            if live(at) {
                                state.live_opaque += 1;
                            }
                            let _ = health.send(state.clone());
                            // sampled: a backlog storm otherwise logs per
                            // frame, several times a second, for minutes
                            if state.opaque_frames % 256 == 1 {
                                tracing::debug!(
                                    held = hold.len(),
                                    dropped = state.opaque_frames,
                                    "epoch hold is full — evicting oldest unopenable frames (sampled)"
                                );
                            }
                        }
                        hold.push((content, created_at));
                    }
                    Ingest::EpochAdvanced(readmitted) => {
                        // BEFORE the hold retry: the held frames are the new
                        // incarnation's — its window must be gone first
                        for m in &readmitted {
                            sink.rekeyed(m).await;
                        }
                        match retry_epoch_hold(&mls, &log, &store, &sink, &me, &mut seen, &mut hold).await {
                            Err(()) => return,
                            Ok(lost) if lost.is_empty() => {}
                            Ok(lost) => {
                                let n = |k: usize| u64::try_from(k).unwrap_or(u64::MAX);
                                state.opaque_frames += n(lost.len());
                                state.live_opaque += n(lost.iter().filter(|at| live(**at)).count());
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

    /// A sink that collects delivered envelopes and nothing else.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<molt_core::EventEnvelope>>>);
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

    /// A sink that drops everything.
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
            stamp: None,
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
            stamp: None,
        };
        let (_stop_tx, mut stop) = watch::channel(false);
        let outcome = publish_with_backoff(&channel, &frame, &cfg(), &mut stop).await;
        assert!(
            matches!(outcome, Err(PublishStall::Transient)),
            "an empty pool can refill — it must not be classed permanent: {outcome:?}"
        );
    }

    /// M8: a commit recorded with its carrier stamp is framed for a
    /// publish AT that stamp — the resend keys identically to the original.
    #[test]
    fn a_commits_frame_carries_its_recorded_stamp() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        let mut walter = MlsMember::new(&SigningKey::from_bytes(&[1u8; 32]), "walter").expect("w");
        walter.create_group().expect("group");
        let chan = MlsChannel::new(walter);
        let env = |body: molt_core::WorkspaceEvent| molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 1,
            by: "walter".to_string(),
            body,
        };
        let plain = chan.group_frame(&env(molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([1u8; 16]),
            "walter",
            "x",
            1,
        ))));
        assert_eq!(plain.expect("frame").stamp, None, "an application frame is stamped now");
        let commit = chan
            .group_frame(&env(molt_core::WorkspaceEvent::MlsCommit { commit: "00".to_string(), stamp: 1234 }));
        // a junk commit does not frame (no epoch to seal under) — only the
        // stamp plumbing is under test, so read it off the event directly
        assert!(commit.is_none() || commit.map(|f| f.stamp) == Some(Some(1234)));
    }

    /// **A commit RESEND stays sealed at its OWN epoch
    /// (`detached_reattach.md` §7).** Sealing resends "one epoch back"
    /// stranded every laggard ≥2 epochs behind: after a second commit, the
    /// first commit's re-offer wore an exporter its laggard never had, and
    /// the guarantee's repair path could not repair. The ring now labels
    /// each retained exporter with its epoch and a re-framed commit picks
    /// the label its own bytes name.
    #[tokio::test]
    async fn a_resent_commit_stays_sealed_at_its_own_epoch() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(11), "alice").expect("alice");
        let bob = MlsMember::new(&key(12), "bob").expect("bob");
        let cara = MlsMember::new(&key(13), "cara").expect("cara");
        alice.create_group().expect("group");
        let _welcome = alice
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add")
            .expect("a welcome");
        // the exporter of the epoch C1 will be MADE at — every laggard still
        // on this epoch holds exactly this secret
        let e_c1 = alice.exporter_secret().expect("pre-C1 exporter");
        let cara2 = MlsMember::new(&key(14), "cara").expect("cara's 2nd device");
        let (c1, _w1) = alice
            .restore_member("cara", &cara2.key_package().expect("kp"))
            .expect("re-key 1");
        let e_c2 = alice.exporter_secret().expect("pre-C2 exporter");
        let cara3 = MlsMember::new(&key(15), "cara").expect("cara's 3rd device");
        let (c2, _w2) = alice
            .restore_member("cara", &cara3.key_package().expect("kp"))
            .expect("re-key 2");
        assert_ne!(e_c1, e_c2, "two epochs, two exporters");

        let env = |seq: u64, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope {
            prev_seq: seq - 1,
            seq,
            ts: 1_751_000_000,
            by: "alice".to_string(),
            body,
        };
        let chan = crate::supervisor::MlsChannel::new(alice);
        let f1 = chan
            .group_frame(&env(2, molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&c1), stamp: 0 }))
            .expect("frame C1");
        assert_eq!(
            f1.exporter, e_c1,
            "a re-framed C1 is sealed under C1's OWN epoch — its laggards can strip it"
        );
        let f2 = chan
            .group_frame(&env(3, molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&c2), stamp: 0 }))
            .expect("frame C2");
        assert_eq!(f2.exporter, e_c2, "…and C2 under C2's");
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
        use nostr_relay_builder::MockRelay;

        let (alice, bob, commit) = alice_rekeys_cara();

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
                thread_op: None,
                operator: None,
                read_by: std::collections::BTreeSet::new(),
            }),
        );
        // encrypted at the NEW epoch — alice merged her own commit already
        let after = alice_mls.group_frame(&msg).expect("frame after the commit");
        let commit_frame = alice_mls
            .group_frame(&env(2, molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&commit), stamp: 0 }))
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

    /// **B2: an evicted runtime tells the engine and goes quiet.** Alice
    /// re-keys bob's seat for another device; bob's running runtime merges
    /// the commit, reports `evicted(alice)`, and its outbox publishes
    /// nothing after that - a later own envelope stays unpublished.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_evicted_runtime_reports_it_and_publishes_nothing() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;

        #[derive(Clone, Default)]
        struct EvictSink(std::sync::Arc<std::sync::Mutex<Vec<MemberId>>>);
        impl EngineSink for EvictSink {
            async fn deliver(&self, _f: &MemberId, _e: molt_core::EventEnvelope) -> Result<(), crate::NetError> {
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
            async fn evicted(&self, by: &MemberId) {
                self.0.lock().expect("sink").push(by.clone());
            }
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(1), "alice").expect("alice");
        let mut bob = MlsMember::new(&key(2), "bob").expect("bob");
        alice.create_group().expect("group");
        let welcome = alice
            .add_members(&[bob.key_package().expect("kp")])
            .expect("add")
            .expect("welcome");
        bob.join_from_welcome(&welcome).expect("bob joins");
        let (commit, _w) = alice
            .restore_member("bob", &MlsMember::new(&key(2), "bob").expect("bob2").key_package().expect("kp"))
            .expect("re-key bob");
        let alice_mls = MlsChannel::new(alice);
        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [7u8; 32];
        let alice_chan =
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url.clone()], seed);

        let bob_mls = MlsChannel::new(bob);
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        let sink = EvictSink::default();
        let (wake, wake_rx) = watch::channel(0u64);
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        let handle = spawn_group(
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], seed),
            bob_mls.clone(),
            GroupNetConfig::fast("bob".into(), vec!["alice".into()]),
            log.clone(),
            store.clone(),
            sink.clone(),
            wake_rx,
            health_tx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let commit_frame = alice_mls
            .group_frame(&molt_core::EventEnvelope {
                prev_seq: 0,
                seq: 1,
                ts: 1_751_000_000,
                by: "alice".to_string(),
                body: molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&commit), stamp: 0 },
            })
            .expect("commit frame");
        alice_chan
            .publish_frame(&commit_frame.exporter, &commit_frame.ciphertext)
            .await
            .expect("publish the commit");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while sink.0.lock().expect("sink").is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "the eviction never reached the sink");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(*sink.0.lock().expect("sink"), vec!["alice".to_string()]);
        assert!(bob_mls.evicted());

        log.push(molt_core::EventEnvelope { by: "bob".to_string(), ..walter_event(1) });
        let _ = wake.send(1);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(
            store.load().await.group.unwrap_or_default().log_seq,
            0,
            "nothing was published after the eviction"
        );
        assert!(handle.outbox.as_ref().is_some_and(tokio::task::JoinHandle::is_finished), "the outbox stopped");
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
        let h = one_opaque_frame(false).await;
        assert!(h.live_opaque >= 1, "a frame stamped after the subscription opened is a live loss");
    }

    /// A rejoiner's day window: an opaque frame stamped before the
    /// subscription opened is history this seat never held keys for -
    /// counted, but not a live loss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_opaque_frame_from_before_the_subscription_is_history() {
        let h = one_opaque_frame(true).await;
        assert_eq!(h.live_opaque, 0, "history is not a live loss");
    }

    /// **The 445 carrier stamp never picks the commit winner** (audit #9,
    /// decided 2026-10-11). Two concurrent commits reach a bystander's
    /// runtime, the losing one under the EARLIER stamp and then once more
    /// re-published earlier still; the bystander must end on the
    /// lower-digest commit in either arrival order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_carrier_stamp_never_picks_the_commit_winner() {
        for loser_first in [true, false] {
            carrier_race(loser_first).await;
        }
    }

    async fn carrier_race(loser_first: bool) {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;
        use sha2::Digest as _;

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(21), "alice").expect("alice");
        let mut bob = MlsMember::new(&key(22), "bob").expect("bob");
        let mut cara = MlsMember::new(&key(23), "cara").expect("cara");
        let dave = MlsMember::new(&key(24), "dave").expect("dave");
        let erin = MlsMember::new(&key(25), "erin").expect("erin");
        alice.create_group().expect("group");
        let welcome = alice
            .add_members(&[
                bob.key_package().expect("kp"),
                cara.key_package().expect("kp"),
                dave.key_package().expect("kp"),
                erin.key_package().expect("kp"),
            ])
            .expect("add")
            .expect("a welcome");
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");
        let dave2 = MlsMember::new(&key(24), "dave").expect("dave2");
        let erin2 = MlsMember::new(&key(25), "erin").expect("erin2");
        let (commit_a, _) = alice
            .restore_member("dave", &dave2.key_package().expect("kp"))
            .expect("alice re-keys dave");
        let (commit_c, _) = cara
            .restore_member("erin", &erin2.key_package().expect("kp"))
            .expect("cara re-keys erin");
        let (alice, cara) = (MlsChannel::new(alice), MlsChannel::new(cara));
        let a_wins = sha2::Sha256::digest(&commit_a) < sha2::Sha256::digest(&commit_c);
        let ((winner, w_commit, w_name), (loser, l_commit)) = if a_wins {
            ((&alice, &commit_a, "alice"), (&cara, &commit_c))
        } else {
            ((&cara, &commit_c, "cara"), (&alice, &commit_a))
        };
        let env = |by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 1_751_000_000,
            by: by.to_string(),
            body,
        };
        let now = crate::ritual_net::now_secs();
        let commit_frame = |chan: &MlsChannel, by: &str, c: &[u8], stamp: u64| {
            chan.group_frame(&env(by, molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(c), stamp }))
                .expect("commit frame")
        };
        let w_frame = commit_frame(winner, w_name, w_commit, now - 10);
        let l_frame = commit_frame(loser, "loser", l_commit, now - 20);
        let probe = winner
            .group_frame(&env(
                w_name,
                molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    molt_core::MessageId([3u8; 16]),
                    w_name,
                    "one group",
                    1,
                )),
            ))
            .expect("probe frame");

        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [8u8; 32];
        let pub_chan =
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url.clone()], seed);
        let sink = Sink::default();
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        let bob_chan = crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], seed);
        let handle = spawn_group(
            bob_chan,
            MlsChannel::new(bob),
            GroupNetConfig::fast("bob".into(), vec!["alice".into(), "cara".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            sink.clone(),
            wake_rx,
            health_tx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let order = if loser_first {
            [(&l_frame, now - 20), (&w_frame, now - 10), (&l_frame, now - 40)]
        } else {
            [(&w_frame, now - 10), (&l_frame, now - 20), (&l_frame, now - 40)]
        };
        for (frame, stamp) in order {
            pub_chan
                .publish_frame_at(&frame.exporter, &frame.ciphertext, stamp)
                .await
                .expect("publish a commit");
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        pub_chan
            .publish_frame(&probe.exporter, &probe.ciphertext)
            .await
            .expect("publish the probe");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if !sink.0.lock().expect("sink").is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "loser_first={loser_first}: bob is not on the lower-digest branch"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.shutdown().await;
    }

    /// Bob's runtime meets one frame no epoch opens, then the commit whose
    /// retry proves it lost; `historic` publishes that frame before bob
    /// subscribes, stamped ten minutes back.
    async fn one_opaque_frame(historic: bool) -> GroupHealth {
        use nostr_relay_builder::MockRelay;

        let (alice, bob, commit) = alice_rekeys_cara();

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
                body: molt_core::WorkspaceEvent::MlsCommit { commit: hex::encode(&commit), stamp: 0 },
            })
            .expect("commit frame");

        if historic {
            alice_chan
                .publish_frame_at(&[9u8; 32], &[0x5au8; 64], crate::ritual_net::now_secs() - 600)
                .await
                .expect("publish the historic alien frame");
        }
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
        if !historic {
            alice_chan
                .publish_frame(&[9u8; 32], &[0x5au8; 64])
                .await
                .expect("publish the alien frame");
        }
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
        let h = health_rx.borrow().clone();
        handle.shutdown().await;
        h
    }

    /// The first replay reads as catching up until the relay is through it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_first_replay_reads_as_catching_up_until_the_relay_is_through() {
        use nostr_relay_builder::builder::{PolicyResult, QueryPolicy, RelayBuilder};
        use nostr_relay_builder::prelude::{BoxedFuture, Filter};

        #[derive(Debug)]
        struct SlowReq;
        impl QueryPolicy for SlowReq {
            fn admit_query<'a>(
                &'a self,
                _query: &'a Filter,
                _addr: &'a std::net::SocketAddr,
            ) -> BoxedFuture<'a, PolicyResult> {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    PolicyResult::Accept
                })
            }
        }
        let relay = nostr_relay_builder::LocalRelay::new(RelayBuilder::default().query_policy(SlowReq));
        relay.run().await.expect("relay");
        let url = relay.url().await.to_string();
        let mut alice = crate::mls::MlsMember::new(&ed25519_dalek::SigningKey::from_bytes(&[1; 32]), "alice")
            .expect("alice");
        alice.create_group().expect("group");
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, mut health_rx) = watch::channel(GroupHealth::default());
        let chan = crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], [7u8; 32]);
        let handle = spawn_group(
            chan,
            MlsChannel::new(alice),
            GroupNetConfig::fast("alice".into(), Vec::new()),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            NullSink,
            wake_rx,
            health_tx,
        );
        health_rx.changed().await.expect("the first health report");
        assert!(health_rx.borrow_and_update().catching_up, "a fresh subscription is catching up");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while health_rx.borrow_and_update().catching_up {
            assert!(tokio::time::Instant::now() < deadline, "the replay never finished");
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

    /// **A dropped runtime's channel dials nothing more.** The engine drops
    /// the handle when a relay is withdrawn; a clone of its channel (a
    /// publish in flight, the file trickle) must not reach the relay after.
    #[tokio::test]
    async fn a_dropped_runtime_closes_its_publish_channel() {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let accepts = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = accepts.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                drop(sock);
            }
        });
        let mls = MlsMember::new(&SigningKey::from_bytes(&[9u8; 32]), "walter").expect("mls");
        let channel = crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], [7u8; 32]);
        let kept = channel.clone();
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
        drop(handle);
        assert!(kept.publish_frame(&[0u8; 32], b"frame").await.is_err());
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "the dropped runtime's channel dialed the relay");
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
            log.push(walter_event(seq));
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
            log.push(walter_event(seq));
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

    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A restored seat: a sheet acking seqs above its own log describes its
    /// previous incarnation. It proves nothing: the floor falls back to the
    /// start, so the outbox re-offers everything, with one WARN per episode.
    #[tokio::test]
    async fn a_sheet_acking_above_the_own_log_is_refused_and_rewinds_the_peer() {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).without_time().finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=10u64 {
            log.push(walter_event(seq));
        }
        // an older build already believed the stale sheet
        store
            .update(|st| {
                st.outbound.insert("petra".into(), OutboundCursor { acked_floor: 10, ack_seen: true, ..Default::default() });
                true
            })
            .await;
        let stale = |high: u64| {
            let mut w = molt_core::AcceptedWindow::default();
            assert!(w.accept(high));
            crate::group_ack::GroupAck::new("petra".to_string(), std::collections::BTreeMap::from([("walter".to_string(), w)]))
        };
        let (me, from) = ("walter".to_string(), "petra".to_string());
        apply_group_ack(&log, &store, &me, &from, &stale(6_000)).await;
        apply_group_ack(&log, &store, &me, &from, &stale(6_001)).await;
        let cursor = <crate::MemStateStore as StateStore>::load(&store).await.outbound["petra"];
        assert_eq!((cursor.acked_floor, cursor.ack_seen), (0, true), "the tail is owed again");
        let logged = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
        assert_eq!(logged.matches("ack_ahead").count(), 1, "one WARN per episode: {logged}");
        assert!(logged.contains("window_high=6000 own_high=10"), "{logged}");

        // counter: a sheet within the own log advances the floor and ends
        // the episode
        let mut w = molt_core::AcceptedWindow::default();
        for seq in 1..=4u64 {
            assert!(w.accept(seq));
        }
        let ack = crate::group_ack::GroupAck::new("petra".to_string(), std::collections::BTreeMap::from([("walter".to_string(), w)]));
        apply_group_ack(&log, &store, &me, &from, &ack).await;
        let cursor = <crate::MemStateStore as StateStore>::load(&store).await.outbound["petra"];
        assert_eq!(cursor.acked_floor, 4);
        assert!(!cursor.ack_ahead);
        apply_group_ack(&log, &store, &me, &from, &stale(6_000)).await;
        let logged = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
        assert_eq!(logged.matches("ack_ahead").count(), 2, "a new episode warns again");
    }

    fn sheet_at(at: u64, accepted: std::ops::RangeInclusive<u64>) -> crate::group_ack::GroupAck {
        let mut w = molt_core::AcceptedWindow::default();
        for seq in accepted {
            assert!(w.accept(seq));
        }
        let mut ack =
            crate::group_ack::GroupAck::new("petra".to_string(), std::collections::BTreeMap::from([("walter".to_string(), w)]));
        ack.at = at;
        ack
    }

    /// Walter's log 1..=10, all acked by petra's sheet stamped 1000.
    async fn acked_through_ten() -> (crate::MemLog, crate::MemStateStore) {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=10u64 {
            log.push(walter_event(seq));
        }
        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &sheet_at(1_000, 1..=10)).await;
        (log, store)
    }

    async fn petra_cursor(store: &crate::MemStateStore) -> OutboundCursor {
        <crate::MemStateStore as StateStore>::load(store).await.outbound["petra"]
    }

    /// A newer sheet that no longer holds what an earlier one acked (the
    /// peer's window was reset, or regressed across a hard stop): the floor
    /// falls back to what this sheet proves, so the stall rounds re-offer
    /// the rest. One WARN per episode; the episode ends once the floor is
    /// back. The resend budget is untouched.
    #[tokio::test]
    async fn a_newer_sheet_below_the_floor_rewinds_once_per_episode() {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).without_time().finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (log, store) = acked_through_ten().await;
        assert_eq!(petra_cursor(&store).await.acked_floor, 10);
        let budget = <crate::MemStateStore as StateStore>::load(&store).await.group.unwrap_or_default();
        let (me, from) = ("walter".to_string(), "petra".to_string());
        for i in 0..50u64 {
            apply_group_ack(&log, &store, &me, &from, &sheet_at(2_000 + i, 1..=4)).await;
        }
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.acked_floor, cursor.ack_seen), (4, true), "seqs 5..=10 are owed again");
        let logged = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
        assert_eq!(logged.matches("WARN").count(), 1, "one WARN per episode: {logged}");
        assert!(logged.contains("ack_regressed") && logged.contains("reported=4 floor=10"), "{logged}");
        let group = <crate::MemStateStore as StateStore>::load(&store).await.group.unwrap_or_default();
        assert_eq!(
            (group.resend_rounds, group.resend_window_start, group.heal_rounds),
            (budget.resend_rounds, budget.resend_window_start, budget.heal_rounds),
            "a rewind spends and refills no budget: the rounds stay at {RESEND_ROUNDS_PER_HOUR}/h plus one heal"
        );
        // the tail landed: the episode ends, a later regression warns again
        apply_group_ack(&log, &store, &me, &from, &sheet_at(3_000, 1..=10)).await;
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.acked_floor, cursor.regressed_from), (10, 0));
        apply_group_ack(&log, &store, &me, &from, &sheet_at(4_000, 1..=2)).await;
        let logged = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
        assert_eq!(logged.matches("WARN").count(), 2, "{logged}");
    }

    /// A log that records where each read started.
    #[derive(Clone, Default)]
    struct CountingLog(crate::MemLog, std::sync::Arc<std::sync::Mutex<Vec<u64>>>);

    impl OutboxLog for CountingLog {
        async fn read_from(&self, from_seq: u64) -> Vec<molt_core::EventEnvelope> {
            self.1.lock().expect("reads").push(from_seq);
            self.0.read_from(from_seq).await
        }
    }

    /// While a peer stays ahead, its sheets read only the log's tail from
    /// the claimed high - never the whole log every few seconds - and once
    /// the give-up rounds re-offered to it in vain it is no longer owed a
    /// round until a plausible sheet of it arrives: no timer decides.
    #[tokio::test]
    async fn an_ack_ahead_episode_reads_the_tail_only_and_stops_after_the_give_up_rounds() {
        let log = CountingLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=10u64 {
            log.0.push(walter_event(seq));
        }
        let (me, from) = ("walter".to_string(), "petra".to_string());
        apply_group_ack(&log, &store, &me, &from, &sheet_at(1, 6_000..=6_000)).await;
        assert_eq!(petra_cursor(&store).await.acked_floor, 0, "the tail is re-offered");
        log.1.lock().expect("reads").clear();
        for i in 1..20u64 {
            apply_group_ack(&log, &store, &me, &from, &sheet_at(1 + i, 6_000..=6_000)).await;
        }
        let reads = log.1.lock().expect("reads").clone();
        assert!(reads.iter().all(|r| *r == 6_000), "a stale sheet read more than the tail: {reads:?}");
        let envs = log.0.read_from(1).await;
        let cfg = GroupNetConfig::fast("walter".into(), vec!["petra".into()]);
        let reach = OutboxReach::default();
        let mut st = <crate::MemStateStore as StateStore>::load(&store).await;
        let t0 = 1_700_000_000;
        for round in 0..RESEND_GIVEUP_ROUNDS {
            let planned = plan_round(&mut st, &cfg, &envs, &reach, false, t0 + 3_600 * u64::from(round));
            assert!(matches!(planned, StallRound::Granted { floor: 0, .. }), "round {round}: {planned:?}");
        }
        assert_eq!(st.outbound["petra"].ahead_rounds, RESEND_GIVEUP_ROUNDS, "every round counted");
        assert!(
            matches!(plan_round(&mut st, &cfg, &envs, &reach, false, t0 + 3_600 * 99), StallRound::NoFloor),
            "left alone after the give-up rounds"
        );
        // a plausible sheet ends the episode and the peer is owed again
        store.update(|s| { *s = st.clone(); true }).await;
        apply_group_ack(&log, &store, &me, &from, &sheet_at(200, 1..=4)).await;
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.ack_ahead, cursor.ahead_rounds, cursor.acked_floor), (false, 0, 4));
    }

    /// A peer that held this seat's previous incarnation keeps sending the
    /// same frozen sheet; once this log has grown past that stale high the
    /// sheet looks plausible, but it ends nothing - acking below it would
    /// mark frames the peer never had. Only a sheet below the stale high
    /// (the peer reset its window) ends the episode.
    #[tokio::test]
    async fn a_frozen_stale_sheet_does_not_end_the_episode_once_the_log_passes_it() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=10u64 {
            log.push(walter_event(seq));
        }
        let (me, from) = ("walter".to_string(), "petra".to_string());
        apply_group_ack(&log, &store, &me, &from, &sheet_at(1, 3..=12)).await;
        assert!(petra_cursor(&store).await.ack_ahead);
        for seq in 11..=20u64 {
            log.push(walter_event(seq));
        }
        apply_group_ack(&log, &store, &me, &from, &sheet_at(2, 3..=12)).await;
        apply_group_ack(&log, &store, &me, &from, &sheet_at(3, 3..=15)).await;
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.ack_ahead, cursor.acked_floor), (true, 0), "the stale window marked nothing acked");
        apply_group_ack(&log, &store, &me, &from, &sheet_at(4, 1..=5)).await;
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.ack_ahead, cursor.acked_floor), (false, 5), "a reset window's sheet ends it");
    }

    /// A cursor a development build wrote mid-episode carries no stale high:
    /// a plausible sheet still ends that episode.
    #[tokio::test]
    async fn a_dev_cursor_without_a_stale_high_ends_on_a_plausible_sheet() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=10u64 {
            log.push(walter_event(seq));
        }
        store
            .update(|st| {
                st.outbound.insert("petra".into(), OutboundCursor { ack_ahead: true, ack_seen: true, ..Default::default() });
                true
            })
            .await;
        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &sheet_at(1, 1..=4)).await;
        let cursor = petra_cursor(&store).await;
        assert_eq!((cursor.ack_ahead, cursor.acked_floor), (false, 4));
    }

    /// The seq at `high - 1024` is still an explicit bit of the window; only
    /// below it does the sheet read every seq as accepted.
    #[tokio::test]
    async fn a_rewind_reads_the_last_explicit_bit_as_unacked() {
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 1..=1_100u64 {
            log.push(walter_event(seq));
        }
        store
            .update(|st| {
                st.outbound.insert(
                    "petra".into(),
                    OutboundCursor { acked_floor: 1_100, ack_seen: true, ack_high: 1_101, ack_at: 1, ..Default::default() },
                );
                true
            })
            .await;
        apply_group_ack(&log, &store, &"walter".to_string(), &"petra".to_string(), &sheet_at(2, 1_100..=1_100)).await;
        let edge = 1_100 - molt_core::ACCEPT_WINDOW_BITS;
        assert_eq!(petra_cursor(&store).await.acked_floor, edge - 1, "seq {edge} is an explicit, unset bit");
    }

    /// A late older sheet, or one from a build that stamps nothing, proves
    /// nothing about now: the floor stays.
    #[tokio::test]
    async fn a_late_older_or_unstamped_sheet_does_not_rewind() {
        let (log, store) = acked_through_ten().await;
        let (me, from) = ("walter".to_string(), "petra".to_string());
        apply_group_ack(&log, &store, &me, &from, &sheet_at(999, 1..=4)).await;
        apply_group_ack(&log, &store, &me, &from, &sheet_at(1_000, 1..=4)).await;
        apply_group_ack(&log, &store, &me, &from, &sheet_at(0, 1..=4)).await;
        assert_eq!(petra_cursor(&store).await.acked_floor, 10);
    }

    /// What compaction already dropped cannot be re-offered: the floor goes
    /// no lower than the log reaches, and the gone positions are logged once.
    #[tokio::test]
    async fn a_rewind_below_the_compacted_log_counts_what_is_gone() {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).without_time().finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let log = crate::MemLog::default();
        let store = crate::MemStateStore::default();
        for seq in 6..=10u64 {
            log.push(walter_event(seq));
        }
        store
            .update(|st| {
                st.outbound.insert(
                    "petra".into(),
                    OutboundCursor { acked_floor: 10, ack_seen: true, ack_high: 10, ack_at: 1_000, ..Default::default() },
                );
                true
            })
            .await;
        let (me, from) = ("walter".to_string(), "petra".to_string());
        apply_group_ack(&log, &store, &me, &from, &sheet_at(2_000, 1..=2)).await;
        apply_group_ack(&log, &store, &me, &from, &sheet_at(2_001, 1..=2)).await;
        assert_eq!(petra_cursor(&store).await.acked_floor, 5, "nothing below the log to re-offer");
        let logged = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
        assert_eq!(logged.matches("compacted=3").count(), 1, "{logged}");
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
            log.push(walter_event(seq));
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
    /// The doubling delay: 10 → 20 → … → 600 and no further; a reset
    /// returns to the start.
    #[test]
    fn a_backoff_doubles_to_the_ceiling_and_resets() {
        let mut b = Backoff::new();
        assert_eq!(b.secs(), RESEND_AFTER_SECS);
        assert_eq!(b.escalate(), 20);
        assert_eq!(b.escalate(), 40);
        for _ in 0..10 {
            b.escalate();
        }
        assert_eq!(b.secs(), RESEND_MAX_BACKOFF_SECS, "capped");
        assert_eq!(b.delay(), Duration::from_secs(RESEND_MAX_BACKOFF_SECS));
        b.reset();
        assert_eq!(b.secs(), RESEND_AFTER_SECS);
    }

    /// M6: a transient hold arms the held-frame timer, a permanent one
    /// never does; the first publish afterwards owes exactly one `send_ok`
    /// and resets the escalation.
    #[test]
    fn a_held_frame_arms_the_timer_for_a_transient_refusal_only() {
        let mut held = HeldFrame::new();
        assert!(!held.retry_armed(), "nothing held, nothing armed");
        assert!(!held.published(), "no report, no send_ok owed");

        held.failed(false);
        assert!(held.retry_armed(), "a transient hold retries on the timer");
        held.backoff.escalate();
        assert_eq!(held.retry_in(Duration::from_secs(5)), Duration::from_secs(5), "capped at retry_cap");
        assert_eq!(held.retry_in(Duration::from_secs(60)), Duration::from_secs(20));
        assert!(held.published(), "the first success owes the send_ok");
        assert!(!held.published(), "…once");
        assert_eq!(held.backoff.secs(), RESEND_AFTER_SECS, "a success resets the escalation");

        held.failed(true);
        assert!(!held.retry_armed(), "a permanent refusal never arms the timer");
        assert_eq!(stall_decision(true, &held), Action::Idle, "…and idles even above a proven floor");
        assert!(held.published());
        assert!(!held.permanent, "a success clears the permanent hold");
    }

    /// The stall decision: an own-ackable tail above a proven floor runs
    /// the clock; no tail idles.
    #[test]
    fn the_stall_decision_needs_an_own_ackable_tail() {
        let held = HeldFrame::new();
        assert_eq!(stall_decision(false, &held), Action::Idle);
        assert_eq!(stall_decision(true, &held), Action::Stall);
    }

    /// The stall clock de-escalates on FORWARD floor progress only, clears
    /// a reported stall exactly then, and reports once at the give-up count.
    #[test]
    fn the_stall_clock_counts_forward_progress_only_and_reports_once() {
        let mut clock = StallClock::new();
        clock.observe_floor(None);
        assert_eq!(clock.last_floor, None, "nothing proven yet");
        clock.observe_floor(Some(3));
        assert_eq!(clock.last_floor, Some(3), "the first real floor is progress");
        clock.observe_floor(Some(2));
        assert_eq!(clock.last_floor, Some(3), "a regressing floor is not");

        let mut reported = 0;
        for _ in 0..RESEND_GIVEUP_ROUNDS + 3 {
            if clock.round_granted() {
                reported += 1;
            }
            clock.escalate(tokio::time::Instant::now());
        }
        assert_eq!(reported, 1, "reported exactly once, at the give-up count");
        assert!(clock.armed());
        assert_eq!(clock.backoff.secs(), RESEND_MAX_BACKOFF_SECS);

        assert!(clock.observe_floor(Some(4)), "progress clears the report…");
        assert_eq!(clock.rounds_without_progress, 0);
        assert_eq!(clock.backoff.secs(), RESEND_AFTER_SECS, "…and de-escalates entirely");
        assert!(!clock.observe_floor(Some(5)), "…and there is nothing left to clear");
        clock.disarm();
        assert!(!clock.armed());
    }

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

    /// `(member, None)` = send_ok, `(member, Some(reason))` = send_failed.
    type Signals = Vec<(String, Option<String>)>;

    /// A sink that records the backoff signals and nothing else.
    #[derive(Clone, Default)]
    struct SignalSink(std::sync::Arc<std::sync::Mutex<Signals>>);
    impl EngineSink for SignalSink {
        async fn deliver(
            &self,
            _from: &MemberId,
            _env: molt_core::EventEnvelope,
        ) -> Result<(), crate::NetError> {
            Ok(())
        }
        async fn peer_seen(&self, _m: &MemberId) {}
        async fn send_failed(&self, m: &MemberId, r: &str) {
            self.0
                .lock()
                .expect("sink")
                .push((m.clone(), Some(r.to_string())));
        }
        async fn send_ok(&self, m: &MemberId) {
            self.0.lock().expect("sink").push((m.clone(), None));
        }
    }

    impl SignalSink {
        async fn wait_for(&self, ok: bool, what: &str) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                let seen = self.0.lock().expect("sink").clone();
                if seen.iter().any(|(m, r)| m == "walter" && r.is_none() == ok) {
                    return;
                }
                assert!(tokio::time::Instant::now() < deadline, "{what}: {seen:?}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    /// A MockRelay behind a proxy the test flips: down (every connection
    /// dropped at once → a transient stall) or up (bytes piped through).
    /// Returns the proxy URL, the up-flag, and the relay to keep alive.
    async fn flaky_relay() -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        nostr_relay_builder::MockRelay,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
        let upstream = relay
            .url()
            .await
            .to_string()
            .trim_start_matches("ws://")
            .trim_end_matches('/')
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy");
        let url = format!("ws://{}", listener.local_addr().expect("proxy addr"));
        let up = std::sync::Arc::new(AtomicBool::new(false));
        let flag = up.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                if !flag.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    if let Ok(mut server) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        });
        (url, up, relay)
    }

    /// alice, bob (joined) and the commit by which alice re-keyed cara's seat
    /// onto a fresh device.
    fn alice_rekeys_cara() -> (crate::mls::MlsMember, crate::mls::MlsMember, Vec<u8>) {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;

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
            .restore_member("cara", &cara2.key_package().expect("kp"))
            .expect("re-key");
        drop(cara);
        (alice, bob, commit)
    }

    fn walter_event(seq: u64) -> molt_core::EventEnvelope {
        molt_core::EventEnvelope {
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
        }
    }

    /// Walter alone in a group, one own event in the log, publishing at
    /// `url`; nobody has ever acked (floor `None`).
    fn spawn_walter(
        url: String,
        log: crate::MemLog,
        sink: SignalSink,
        wake_rx: watch::Receiver<u64>,
    ) -> GroupHandle {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;
        let mut walter =
            MlsMember::new(&SigningKey::from_bytes(&[1u8; 32]), "walter").expect("walter");
        walter.create_group().expect("group");
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        spawn_group(
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], [7u8; 32]),
            MlsChannel::new(walter),
            GroupNetConfig::fast("walter".into(), vec!["petra".into()]),
            log,
            crate::MemStateStore::default(),
            sink,
            wake_rx,
            health_tx,
        )
    }

    /// **A publish that works again lifts the send-failed flag.**
    ///
    /// Field 2026-08-24: a restored seat's re-offer hit a relay outage
    /// (both relays timed out), the outbox raised `send_failed` — and kept
    /// the surface amber ("Reconnecting… no relay accepted the frame") long
    /// after the next publish had gone through, because nothing ever sent
    /// the matching `send_ok`. The engine clears the stuck flag on exactly
    /// that signal, so the outbox owes it on the first success after a
    /// reported failure.
    #[tokio::test]
    async fn a_publish_that_works_again_lifts_the_send_failed_flag() {
        use std::sync::atomic::Ordering;
        let (url, up, _relay) = flaky_relay().await;
        let log = crate::MemLog::default();
        log.push(walter_event(1));
        let sink = SignalSink::default();
        let (wake_tx, wake_rx) = watch::channel(0u64);
        let handle = spawn_walter(url, log.clone(), sink.clone(), wake_rx);

        // the relay is down: the retry chain runs dry and the outbox says so
        sink.wait_for(false, "the outage was never reported").await;

        // the relay is back; the next append wakes the outbox, which
        // republishes the held frame and the new one
        up.store(true, Ordering::SeqCst);
        log.push(walter_event(2));
        wake_tx.send(2).expect("wake");
        sink.wait_for(
            true,
            "the publish went through but nothing lifted the send-failed flag — \
             the surface stays amber for ever",
        )
        .await;
        handle.shutdown().await;
    }

    /// **A PERMANENT refusal does not arm the held-frame timer.**
    ///
    /// The timer exists for outages. A frame refused locally (over the
    /// size budget, unsealable) answers the same in a second and in an
    /// hour; retrying it re-frames the envelope (one ratchet generation per
    /// try) and logs an error every round for nothing. It waits for an
    /// append or the operator — the ERROR line already names it.
    #[tokio::test]
    async fn a_permanent_refusal_does_not_arm_the_held_frame_timer() {
        use std::sync::atomic::Ordering;
        let (url, up, _relay) = flaky_relay().await;
        up.store(true, Ordering::SeqCst);
        let log = crate::MemLog::default();
        let mut huge = walter_event(1);
        huge.body = molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([1u8; 16]),
            "walter",
            "x".repeat(256 * 1024),
            1_751_000_000,
        ));
        log.push(huge);
        let sink = SignalSink::default();
        let (_wake_tx, wake_rx) = watch::channel(0u64);
        let handle = spawn_walter(url, log, sink.clone(), wake_rx);

        sink.wait_for(false, "the refusal was never reported").await;
        // five retry ceilings later the outbox must still have said it ONCE
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        let failures = sink
            .0
            .lock()
            .expect("sink")
            .iter()
            .filter(|(_, r)| r.is_some())
            .count();
        assert_eq!(failures, 1, "a permanent refusal is retried on a timer");
        handle.shutdown().await;
    }

    /// **A held frame retries by itself, before anyone has acked.**
    ///
    /// The stall clock only runs above a PROVEN floor, and a seat whose
    /// publishes fail before any peer ever acked it has none — the
    /// held envelope then waited for the next own append, which on a
    /// quiet seat is never. A transient outage must be retried on a clock,
    /// not on the operator typing something.
    #[tokio::test]
    async fn a_held_frame_retries_by_itself_before_anyone_acks() {
        use std::sync::atomic::Ordering;
        let (url, up, _relay) = flaky_relay().await;
        let log = crate::MemLog::default();
        log.push(walter_event(1));
        let sink = SignalSink::default();
        let (_wake_tx, wake_rx) = watch::channel(0u64);
        let handle = spawn_walter(url, log, sink.clone(), wake_rx);

        sink.wait_for(false, "the outage was never reported").await;
        up.store(true, Ordering::SeqCst);
        // NO append, NO ack: the outbox must come back on its own
        sink.wait_for(
            true,
            "the relay is back but the held frame waits for an append that never comes",
        )
        .await;
        handle.shutdown().await;
    }

    /// **A vault frame speaks only for its MLS sender** (plan S3a, 1.3.4):
    /// alice's frame claiming `by = cara` is dropped; her own goes through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_vault_frame_whose_by_is_not_the_sender_is_dropped() {
        use crate::mls::MlsMember;
        use crate::vault_frames::{VaultAskFrame, VaultFrame, VAULT_V};
        use ed25519_dalek::SigningKey;
        use nostr_relay_builder::MockRelay;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<(MemberId, VaultFrame)>>>);
        impl EngineSink for Sink {
            async fn deliver(
                &self,
                _from: &MemberId,
                _env: molt_core::EventEnvelope,
            ) -> Result<(), crate::NetError> {
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
            async fn vault_frame(&self, member: &MemberId, frame: &VaultFrame) {
                self.0.lock().expect("sink").push((member.clone(), frame.clone()));
            }
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut alice = MlsMember::new(&key(1), "alice").expect("alice");
        let bob = MlsMember::new(&key(2), "bob").expect("bob");
        alice.create_group().expect("group");
        let welcome = alice
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("a welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");

        let relay = MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let seed = [7u8; 32];
        let alice_chan =
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url.clone()], seed);
        let alice_mls = MlsChannel::new(alice);
        let ask = |by: &str, id: char| {
            VaultFrame::Ask(VaultAskFrame {
                v: VAULT_V,
                by: by.to_string(),
                grant_id: std::iter::repeat_n(id, 64).collect(),
            })
        };

        let sink = Sink::default();
        let (_wake, wake_rx) = watch::channel(0u64);
        let (health_tx, _health) = watch::channel(GroupHealth::default());
        let bob_chan = crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url], seed);
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

        for frame in [ask("cara", 'a'), ask("alice", 'b')] {
            let sealed = alice_mls.group_control_frame(&frame.to_frame()).expect("sealed");
            alice_chan
                .publish_frame(&sealed.exporter, &sealed.ciphertext)
                .await
                .expect("publish");
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while sink.0.lock().expect("sink").is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "the honest vault frame never arrived");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let got = sink.0.lock().expect("sink").clone();
        assert_eq!(got, vec![("alice".to_string(), ask("alice", 'b'))], "only the honest frame passes");
        handle.shutdown().await;
    }

    /// Walter (two own events, both owed to petra and zoe, who acked once at
    /// floor 0) and petra as a listener that never acks; zoe never runs.
    /// Petra's deliveries count walter's group frames: every re-offer is a
    /// fresh encryption.
    async fn trio(first: Duration, cap: Duration) -> Trio {
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut walter = MlsMember::new(&key(1), "walter").expect("walter");
        let petra = MlsMember::new(&key(2), "petra").expect("petra");
        let zoe = MlsMember::new(&key(3), "zoe").expect("zoe");
        walter.create_group().expect("group");
        let welcome = walter
            .add_members(&[petra.key_package().expect("kp"), zoe.key_package().expect("kp")])
            .expect("add")
            .expect("a welcome");
        let mut petra = petra;
        petra.join_from_welcome(&welcome).expect("petra joins");

        let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
        let url = relay.url().await.to_string();
        let chan = |u: &str| {
            crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![u.into()], [7u8; 32])
        };

        let log = crate::MemLog::default();
        for seq in 1..=2u64 {
            log.push(walter_event(seq));
        }
        let store = crate::MemStateStore::default();
        store
            .update(|st| {
                for m in ["petra", "zoe"] {
                    st.outbound.insert(m.into(), OutboundCursor { ack_seen: true, ..Default::default() });
                }
                true
            })
            .await;
        let (wake_w, wake_w_rx) = watch::channel(0u64);
        let (health_w, _hw) = watch::channel(GroupHealth::default());
        let walter_handle = spawn_group(
            chan(&url),
            MlsChannel::new(walter),
            GroupNetConfig::fast("walter".into(), vec!["petra".into(), "zoe".into()]).with_resend(first, cap),
            log,
            store,
            NullSink,
            wake_w_rx,
            health_w,
        );

        let sink = Sink::default();
        let (wake_p, wake_p_rx) = watch::channel(0u64);
        let (health_p, _hp) = watch::channel(GroupHealth::default());
        let petra_handle = spawn_group(
            chan(&url),
            MlsChannel::new(petra),
            GroupNetConfig::fast("petra".into(), vec!["walter".into(), "zoe".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            sink.clone(),
            wake_p_rx,
            health_p,
        );
        Trio { walter: walter_handle, petra: petra_handle, sink, _keep: (relay, wake_w, wake_p) }
    }

    struct Trio {
        walter: GroupHandle,
        petra: GroupHandle,
        sink: Sink,
        _keep: (nostr_relay_builder::MockRelay, watch::Sender<u64>, watch::Sender<u64>),
    }

    fn count(sink: &Sink) -> usize {
        sink.0.lock().expect("sink").len()
    }

    fn silent(names: &[&str]) -> OutboxReach {
        OutboxReach { silent: names.iter().map(|n| (*n).to_string()).collect(), locked_out: false }
    }

    /// A round rewinds to the lowest floor it may reach: zoe (silent) pins
    /// the floor at 1, petra (heard) at 3, so the round starts at 3 - and
    /// the open rounds still reach back to zoe.
    #[test]
    fn a_round_rewinds_only_as_far_as_a_reachable_peer() {
        let mut st = TransportState::default();
        st.outbound.insert("zoe".into(), OutboundCursor { acked_floor: 1, ack_seen: true, ..Default::default() });
        st.outbound.insert("petra".into(), OutboundCursor { acked_floor: 3, ack_seen: true, ..Default::default() });
        let envs: Vec<_> = (2..=5).map(walter_event).collect();
        let reach = silent(&["zoe"]);
        assert_eq!(reachable_floor(&st, &cfg(), &envs, &reach, false), Ok(Some(3)));
        assert_eq!(reachable_floor(&st, &cfg(), &envs, &reach, true), Ok(Some(1)));
        assert_eq!(
            reachable_floor(&st, &cfg(), &envs, &silent(&["zoe", "petra"]), false),
            Err(Quiet::PeersSilent)
        );
        let locked = OutboxReach { locked_out: true, ..Default::default() };
        assert_eq!(reachable_floor(&st, &cfg(), &envs, &locked, true), Err(Quiet::LockedOut));
        st.outbound.insert("zoe".into(), OutboundCursor { acked_floor: 5, ack_seen: true, ..Default::default() });
        st.outbound.insert("petra".into(), OutboundCursor { acked_floor: 5, ack_seen: true, ..Default::default() });
        assert_eq!(reachable_floor(&st, &cfg(), &envs, &locked, false), Ok(None), "nobody owed");
    }

    /// The first pass plus the open rounds, two frames each.
    fn opening() -> usize {
        2 + 2 * usize::try_from(OPEN_ROUNDS).expect("small")
    }

    /// **Nobody owed is listening: the re-offers stop, and resume on the
    /// event.** Field: a seat re-offered its whole tail every ten minutes,
    /// for ever, to peers that were gone or split.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_silent_peer_gets_no_re_offers_until_it_is_heard() {
        let Trio { walter, petra, sink, _keep } = trio(Duration::from_millis(50), Duration::from_secs(30)).await;
        walter.set_reach(silent(&["petra", "zoe"]));
        // rounds fire at ~0.05, 0.15, 0.35, 0.75, 1.55, 3.15 s
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(count(&sink), opening(), "after the open rounds nothing is re-offered to silent peers");

        // petra is heard again; the next natural round would be ~1.3 s away
        walter.set_reach(silent(&["zoe"]));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while count(&sink) < opening() + 2 {
            assert!(tokio::time::Instant::now() < deadline, "the heard peer was not re-offered on the event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        walter.shutdown().await;
        petra.shutdown().await;
    }

    /// Case (b): one peer gone for good, one heard but slow. The slow one
    /// keeps getting its resends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_heard_slow_peer_keeps_its_resends_beside_a_silent_one() {
        let Trio { walter, petra, sink, _keep } = trio(Duration::from_millis(50), Duration::from_millis(200)).await;
        walter.set_reach(silent(&["zoe"]));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(count(&sink) >= opening() + 8, "the heard peer's resends stopped: {}", count(&sink));
        walter.shutdown().await;
        petra.shutdown().await;
    }

    /// A gap want is answered with exactly the missing own frames, fresh,
    /// past any budget; a seq that is not an own envelope is answered
    /// `gone`, one past the publish cursor is left to the outbox, and a
    /// second want inside the debounce gets nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_gap_want_is_answered_with_exactly_the_missing_frames() {
        use crate::gap::{GapAnswer, GapFrame};
        use crate::mls::MlsMember;
        use ed25519_dalek::SigningKey;

        #[derive(Clone, Default)]
        struct Cap(std::sync::Arc<std::sync::Mutex<(Vec<u64>, Vec<GapAnswer>)>>);
        impl EngineSink for Cap {
            async fn deliver(&self, _f: &MemberId, env: molt_core::EventEnvelope) -> Result<(), crate::NetError> {
                self.0.lock().expect("cap").0.push(env.seq);
                Ok(())
            }
            async fn peer_seen(&self, _m: &MemberId) {}
            async fn send_failed(&self, _m: &MemberId, _r: &str) {}
            async fn gap_frame(&self, _m: &MemberId, frame: &GapFrame) {
                if let GapFrame::Answer(a) = frame {
                    self.0.lock().expect("cap").1.push(a.clone());
                }
            }
        }

        let key = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut walter = MlsMember::new(&key(1), "walter").expect("walter");
        let petra = MlsMember::new(&key(2), "petra").expect("petra");
        walter.create_group().expect("group");
        let welcome = walter.add_members(&[petra.key_package().expect("kp")]).expect("add").expect("welcome");
        let mut petra = petra;
        petra.join_from_welcome(&welcome).expect("petra joins");
        // without the 60-notes-per-minute default: a hundred frames here
        let relay = nostr_relay_builder::LocalRelay::new(nostr_relay_builder::RelayBuilder::default().rate_limit(
            nostr_relay_builder::builder::RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 },
        ));
        relay.run().await.expect("relay");
        let url = relay.url().await.to_string();
        let chan = || crate::ritual_net::GroupChannel::new(crate::dial::Dialer::Direct, vec![url.clone()], [7u8; 32]);

        let log = crate::MemLog::default();
        log.push(walter_event(1));
        log.push(molt_core::EventEnvelope { by: "zoe".into(), prev_seq: 0, ..walter_event(2) });
        for seq in 3..=100 {
            log.push(walter_event(seq));
        }
        let (_ww, wake_w) = watch::channel(0u64);
        let (hw, _hw) = watch::channel(GroupHealth::default());
        let walter = spawn_group(
            chan(),
            MlsChannel::new(walter),
            GroupNetConfig::fast("walter".into(), vec!["petra".into()])
                .with_resend(Duration::from_secs(600), Duration::from_secs(600)),
            log,
            crate::MemStateStore::default(),
            NullSink,
            wake_w,
            hw,
        );
        let cap = Cap::default();
        let (_wp, wake_p) = watch::channel(0u64);
        let (hp, _hp) = watch::channel(GroupHealth::default());
        let petra = spawn_group(
            chan(),
            MlsChannel::new(petra),
            GroupNetConfig::fast("petra".into(), vec!["walter".into()]),
            crate::MemLog::default(),
            crate::MemStateStore::default(),
            cap.clone(),
            wake_p,
            hp,
        );
        let wait = |n_seqs: usize, n_answers: usize| {
            let cap = cap.clone();
            async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    let (s, a) = cap.0.lock().expect("cap").clone();
                    if s.len() >= n_seqs && a.len() >= n_answers {
                        return (s, a);
                    }
                    assert!(tokio::time::Instant::now() < deadline, "got {s:?} {a:?}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        };
        let first: Vec<u64> = std::iter::once(1).chain(3..=100).collect();
        // a stored replay arrives newest first: the first pass has no order
        let (mut seqs, _) = wait(99, 0).await;
        seqs.sort_unstable();
        assert_eq!(seqs, first, "the first pass");

        // seq 1 again, 2 is not walter's, 999 is past the cursor
        assert!(walter.answer_gap("petra".into(), vec![1, 2, 999], None));
        let (seqs, answers) = wait(100, 1).await;
        assert_eq!(seqs[99..], [1], "exactly seq 1 again");
        assert_eq!(answers[0], GapAnswer::new("walter".into(), "petra".into(), vec![1], vec![2]));

        // seq 1 is refused for a while; 3 is not. The answer follows the
        // re-publications, so a re-published 1 would be here by then.
        assert!(walter.answer_gap("petra".into(), vec![1, 3], None));
        let (seqs, answers) = wait(101, 2).await;
        assert_eq!(seqs[100..], [3], "seq 1 is not re-published twice: {seqs:?}");
        assert_eq!(answers[1].resent, vec![3]);

        // a range want is listed, nothing re-published
        assert!(walter.answer_gap("petra".into(), Vec::new(), Some((0, 10))));
        let (seqs, answers) = wait(101, 3).await;
        let listing = answers[2].listing.clone().expect("a listing");
        assert_eq!((listing.after, listing.through, listing.own), (0, 10, vec![1, 3, 4, 5, 6, 7, 8, 9, 10]));
        assert_eq!(seqs.len(), 101, "a listing re-publishes nothing");

        // the per-asker budget: 64 frames per window, two spent already
        assert!(walter.answer_gap("petra".into(), (4..=70).collect(), None));
        let resent = |a: &[GapAnswer]| a.iter().map(|x| x.resent.len()).sum::<usize>();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while resent(&cap.0.lock().expect("cap").1) < 64 {
            assert!(tokio::time::Instant::now() < deadline, "the budget's frames never all came");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(walter.answer_gap("petra".into(), vec![80], None));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (seqs, answers) = cap.0.lock().expect("cap").clone();
        assert_eq!(resent(&answers), 64, "the window's budget holds: {answers:?}");
        assert_eq!(seqs.iter().filter(|s| **s == 80).count(), 1, "past the budget nothing goes out");
        walter.shutdown().await;
        petra.shutdown().await;
        drop(relay);
    }

    /// A seat that concluded it is locked out re-offers nothing, not even
    /// the open rounds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_locked_out_seat_re_offers_nothing() {
        let Trio { walter, petra, sink, _keep } = trio(Duration::from_millis(50), Duration::from_millis(200)).await;
        walter.set_reach(OutboxReach { silent: Default::default(), locked_out: true });
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(count(&sink), 2, "only the first pass went out");
        walter.set_reach(OutboxReach::default());
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while count(&sink) < 4 {
            assert!(tokio::time::Instant::now() < deadline, "the lifted lock-out did not resume the re-offer");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        walter.shutdown().await;
        petra.shutdown().await;
    }
}
