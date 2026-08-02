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
    outbox: Option<tokio::task::JoinHandle<()>>,
    inbox: Option<tokio::task::JoinHandle<()>>,
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
    let inbox = tokio::spawn(inbox_loop(channel, mls, sink, health, stop.clone()));
    GroupHandle {
        stop,
        outbox: Some(outbox),
        inbox: Some(inbox),
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
    loop {
        let mut state = store.load().await;
        rewind_group(&mut state, &cfg);
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
                sink.send_ok(&cfg.member).await;
            } else {
                // hold the cursor exactly here: nothing in N5.2 recovers a
                // skipped envelope, so advancing past an unpublished one would
                // lose it permanently
                sink.send_failed(&cfg.member, "no relay accepted the frame").await;
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
        tokio::select! {
            r = wakeup.changed() => {
                if r.is_err() {
                    return; // the engine dropped its side
                }
            }
            () = stop.notified() => return,
        }
    }
}

/// What one inbound frame turned into.
enum Ingest {
    Delivered,
    /// No held exporter secret opened the outer layer: the frame predates this
    /// node's ring. Permanent, and counted.
    Opaque,
    /// Decoded but carried nothing to deliver (a commit, an ack, a replay).
    Nothing,
    /// The engine is gone.
    EngineGone,
}

async fn ingest_one<K: EngineSink>(
    mls: &MlsChannel,
    sink: &K,
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
        _ => Ingest::Nothing,
    }
}

async fn inbox_loop<K: EngineSink>(
    channel: GroupChannel,
    mls: MlsChannel,
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
                match ingest_one(&mls, &sink, &content, created_at).await {
                    Ingest::EngineGone => return,
                    Ingest::Opaque => {
                        state.opaque_frames += 1;
                        let _ = health.send(state.clone());
                    }
                    Ingest::Delivered | Ingest::Nothing => {}
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
