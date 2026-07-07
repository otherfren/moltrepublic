// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-node transport runtime (concept §2/§5).
//!
//! One supervisor per node owns the transport endpoint and runs, per peer:
//!
//! * an **outbox task** — the log *is* the outbox: it reads pending
//!   envelopes (`by == self`, `seq > cursor(peer)`) from an [`OutboxLog`],
//!   assigns per-link wire seqs, chunks, wraps and sends them with an
//!   independent uniform **fan-out jitter** (a server hosting several
//!   member queues must not correlate a group message by simultaneous
//!   arrival) and retries failures with jittered exponential backoff.
//!   The engine never awaits any of this: it appends to its log and bumps
//!   a coalescing `watch` — the wakeup carries no data, the log does.
//! * a **recv task** — one long-lived subscription per inbound queue:
//!   unwrap (per-queue key), reassemble, then per-sender **in-order,
//!   exactly-once** delivery into the engine sink: wire seqs ≤ the cursor
//!   are duplicates (acked and dropped), gaps are buffered until the
//!   missing piece redelivers. A block is acked only after the engine
//!   accepted the message (tightening this to *after fsync* is T3 work,
//!   noted in the concept's status).
//!
//! Delivery cursors live in a [`StateStore`] (`transport.state` for
//! persisted workspaces, memory for the loopback demo). Losing them costs
//! resends, never history — the peers' dedup absorbs it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{mockrand, EventEnvelope, MemberId, TransportState};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;

use crate::chunk::{chunk_message, msg_id, PushOutcome, Reassembler};
use crate::mls::{MlsIncoming, MlsMember};
use crate::wrap::{unwrap_block, wrap, WrapKey};
use crate::{AckToken, NetError, RcvQueue, SndQueueAddr, Transport};

/// The node's MLS group at runtime (T2): the confidentiality layer whose
/// ciphertext is the SMP payload. Shared by every per-peer outbox/recv task of a
/// node. When present, a workspace event is **encrypted once** per log seq
/// (`create_message` advances the ratchet exactly once) and the *same*
/// ciphertext is fanned out to every peer — each per-queue-wrapped distinctly,
/// so the copies stay byte-distinct (concept §3.2). Absent (`None`) keeps the
/// plaintext-JSON-in-wrap path (the demo mesh, whose peers share no group).
#[derive(Clone)]
pub struct MlsChannel {
    /// The node's group state; `encrypt`/`decrypt` mutate the ratchet, so all
    /// tasks serialize on this one lock.
    member: Arc<Mutex<MlsMember>>,
    /// Ciphertext produced once per outbound log seq, reused by the n−1 fan-out
    /// sends. In-memory for now: it grows with in-flight messages until the
    /// persistent (crash-safe) encrypted outbox lands with the runtime mesh —
    /// the current sole caller is a bounded test, so it never leaks in practice.
    cache: Arc<Mutex<BTreeMap<u64, Vec<u8>>>>,
}

impl std::fmt::Debug for MlsChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MlsChannel")
    }
}

impl MlsChannel {
    /// Wrap a node's live MLS group for the supervisor.
    pub fn new(member: MlsMember) -> MlsChannel {
        MlsChannel::from_shared(Arc::new(Mutex::new(member)))
    }

    /// Wrap an already-shared group — so the bootstrap (which uses the same
    /// `MlsMember` to encrypt/decrypt its announcements) and the runtime
    /// supervisor share one ratchet, in sequence.
    pub fn from_shared(member: Arc<Mutex<MlsMember>>) -> MlsChannel {
        MlsChannel {
            member,
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The MLS ciphertext for one outbound envelope, encrypting exactly once per
    /// `seq` (subsequent fan-out copies reuse the cached bytes — re-encrypting
    /// would double-advance the ratchet). `None` on a local encode/crypto error.
    fn ciphertext_for(&self, seq: u64, env: &EventEnvelope) -> Option<Vec<u8>> {
        if let Some(c) = self.cache.lock().ok()?.get(&seq) {
            return Some(c.clone());
        }
        let plaintext = serde_json::to_vec(env).ok()?;
        let mut m = self.member.lock().ok()?;
        let c = m.encrypt(&plaintext).ok()?;
        self.cache.lock().ok()?.insert(seq, c.clone());
        Some(c)
    }

    /// Decrypt one inbound MLS message into (authenticated sender, envelope).
    /// Duplicates and non-application messages (commits/proposals) return `None`
    /// — MLS itself rejects replays, so there is no separate dedup window here.
    fn decode(&self, wire: &[u8]) -> Option<(MemberId, EventEnvelope)> {
        let mut m = self.member.lock().ok()?;
        match m.decrypt(wire) {
            Ok(MlsIncoming::Application { from, plaintext }) => {
                let env: EventEnvelope = serde_json::from_slice(&plaintext).ok()?;
                Some((from, env))
            }
            _ => None,
        }
    }
}

/// Out-of-order inbound messages buffered per peer before the incoming
/// excess is dropped back onto the transport's redelivery.
const REORDER_BUFFER_MAX: usize = 512;

/// What one wire message carries: the per-link wire seq (the receiver's
/// order/dedup key) and the sender's original envelope. From T2 on this
/// rides inside MLS ciphertext; today it is the plaintext of the per-queue
/// wrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFrame {
    /// Wire-format version.
    pub v: u32,
    /// Per-link, strictly monotonic from 1.
    pub seq: u64,
    /// The sender's envelope, original stamps intact.
    pub env: EventEnvelope,
}

/// Where the outbox reads pending envelopes: the workspace log (persisted
/// nodes) or a [`MemLog`] (loopback demo, tests). The log is the outbox
/// source of truth — there is no separate send queue to recover after a
/// crash.
pub trait OutboxLog: Send + Sync + Clone + 'static {
    /// Every envelope with `seq >= from_seq`, in seq order.
    fn read_from(
        &self,
        from_seq: u64,
    ) -> impl std::future::Future<Output = Vec<EventEnvelope>> + Send;
}

/// Where delivery cursors persist (`transport.state`, or memory for the
/// demo).
pub trait StateStore: Send + Sync + Clone + 'static {
    /// Load the persisted state (defaults when absent).
    fn load(&self) -> impl std::future::Future<Output = TransportState> + Send;
    /// Persist a snapshot (atomic rewrite; implementations may queue).
    fn save(&self, state: TransportState) -> impl std::future::Future<Output = ()> + Send;
}

/// How the supervisor talks to the engine: deliveries and transport
/// health. Implementations send the engine-internal `Net*` commands.
pub trait EngineSink: Send + Sync + Clone + 'static {
    /// Hand one in-order peer envelope to the engine. `Err` means the
    /// engine is gone — the supervisor stops.
    fn deliver(
        &self,
        from: &MemberId,
        env: EventEnvelope,
    ) -> impl std::future::Future<Output = Result<(), NetError>> + Send;
    /// Passive presence: authenticated traffic from `member` arrived.
    fn peer_seen(&self, member: &MemberId) -> impl std::future::Future<Output = ()> + Send;
    /// Sends to `member` keep failing; the outbox is backing off.
    fn send_failed(
        &self,
        member: &MemberId,
        reason: &str,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// One fully wired peer connection: their inbound queue's send address and
/// wrap key (we send), our queue and its wrap key (we receive).
#[derive(Debug, Clone)]
pub struct PeerLink {
    /// The peer this link reaches.
    pub member: MemberId,
    /// Send side of the peer's inbound queue.
    pub snd: SndQueueAddr,
    /// Wrap key of the peer's inbound queue.
    pub wrap_out: WrapKey,
    /// Our inbound queue from this peer.
    pub rcv: RcvQueue,
    /// Wrap key of our inbound queue.
    pub wrap_in: WrapKey,
}

impl PeerLink {
    /// Persist this link as a [`molt_core::MeshLink`] (hex-encoded) for
    /// `transport.state`.
    pub fn to_mesh(&self) -> molt_core::MeshLink {
        molt_core::MeshLink {
            member: self.member.clone(),
            snd_server: self.snd.server.clone(),
            snd_queue: hex::encode(&self.snd.id.0),
            snd_wrap: hex::encode(self.wrap_out.to_bytes()),
            rcv_queue: hex::encode(&self.rcv.id.0),
            rcv_wrap: hex::encode(self.wrap_in.to_bytes()),
        }
    }

    /// Rebuild a link from a persisted [`molt_core::MeshLink`]. `None` on any
    /// malformed hex — a corrupt mesh entry drops that peer, never panics.
    pub fn from_mesh(m: &molt_core::MeshLink) -> Option<PeerLink> {
        let snd_wrap: [u8; 32] = hex::decode(&m.snd_wrap).ok()?.try_into().ok()?;
        let rcv_wrap: [u8; 32] = hex::decode(&m.rcv_wrap).ok()?.try_into().ok()?;
        Some(PeerLink {
            member: m.member.clone(),
            snd: SndQueueAddr {
                server: m.snd_server.clone(),
                id: crate::QueueId::from_bytes(hex::decode(&m.snd_queue).ok()?),
            },
            wrap_out: WrapKey::from_bytes(snd_wrap),
            rcv: RcvQueue {
                id: crate::QueueId::from_bytes(hex::decode(&m.rcv_queue).ok()?),
            },
            wrap_in: WrapKey::from_bytes(rcv_wrap),
        })
    }
}

/// Supervisor configuration for one node.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// This node's member handle (outbox sends only envelopes it authored).
    pub member: MemberId,
    /// The full-mesh links to every other member.
    pub peers: Vec<PeerLink>,
    /// Upper bound of the uniform per-send fan-out jitter (0 in tests;
    /// the concept's default is 2 s).
    pub jitter_max_ms: u64,
    /// First retry backoff.
    pub retry_base_ms: u64,
    /// Backoff cap (the concept's 2 min).
    pub retry_cap_ms: u64,
    /// Seed for jitter/backoff randomness (never key material).
    pub seed: u64,
}

impl NetConfig {
    /// The concept's defaults: 2 s jitter, 1 s → 2 min backoff.
    pub fn new(member: MemberId, peers: Vec<PeerLink>, seed: u64) -> NetConfig {
        NetConfig {
            member,
            peers,
            jitter_max_ms: 2_000,
            retry_base_ms: 1_000,
            retry_cap_ms: 120_000,
            seed,
        }
    }

    /// Test-tier tuning: no jitter, fast retries — the one definition the
    /// loopback/chaos tests share.
    pub fn fast(member: MemberId, peers: Vec<PeerLink>, seed: u64) -> NetConfig {
        NetConfig {
            member,
            peers,
            jitter_max_ms: 0,
            retry_base_ms: 20,
            retry_cap_ms: 100,
            seed,
        }
    }
}

/// The running supervisor. [`SupervisorHandle::shutdown`] (or drop) aborts
/// every child task.
pub struct SupervisorHandle {
    stop: Arc<Notify>,
}

impl SupervisorHandle {
    /// Stop all transport tasks of this node.
    pub fn shutdown(&self) {
        self.stop.notify_one();
    }
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

/// Start a node's transport runtime. `wakeup` is the engine-held watch of
/// the newest appended log seq — bumping it wakes every outbox task
/// (coalescing: intermediate values may be skipped, the log has the data).
pub fn spawn<T, L, S, K>(
    transport: T,
    cfg: NetConfig,
    log: L,
    store: S,
    sink: K,
    wakeup: watch::Receiver<u64>,
    mls: Option<MlsChannel>,
) -> SupervisorHandle
where
    T: Transport,
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let stop = Arc::new(Notify::new());
    let stopped = stop.clone();
    tokio::spawn(async move {
        let state = Arc::new(Mutex::new(store.load().await));
        let mut children = JoinSet::new();
        for (i, peer) in cfg.peers.iter().enumerate() {
            let seed = cfg.seed
                .wrapping_add(1 + u64::try_from(i).unwrap_or_default())
                | 1;
            children.spawn(outbox_task(
                transport.clone(),
                cfg.clone(),
                peer.clone(),
                log.clone(),
                store.clone(),
                sink.clone(),
                state.clone(),
                wakeup.clone(),
                seed,
                mls.clone(),
            ));
            match transport.subscribe(&peer.rcv).await {
                Ok(rx) => {
                    children.spawn(recv_task(
                        peer.clone(),
                        rx,
                        store.clone(),
                        sink.clone(),
                        state.clone(),
                        mls.clone(),
                    ));
                }
                Err(e) => {
                    tracing::error!(peer = %peer.member, error = %e, "subscribing inbound queue failed");
                }
            }
        }
        stopped.notified().await;
        // dropping the JoinSet aborts every child task
        drop(children);
        tracing::debug!("net supervisor stopped");
    });
    SupervisorHandle { stop }
}

/// The per-peer outbox drainer. Never blocks the engine: it waits on the
/// wakeup watch and reads pending envelopes straight from the log.
#[allow(clippy::too_many_arguments)]
async fn outbox_task<T, L, S, K>(
    transport: T,
    cfg: NetConfig,
    peer: PeerLink,
    log: L,
    store: S,
    sink: K,
    state: Arc<Mutex<TransportState>>,
    mut wakeup: watch::Receiver<u64>,
    seed: u64,
    mls: Option<MlsChannel>,
) where
    T: Transport,
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let mut rng = seed;
    loop {
        // drain everything pending for this peer
        loop {
            let cursor = state
                .lock()
                .ok()
                .and_then(|s| s.outbound.get(&peer.member).copied())
                .unwrap_or_default();
            let batch = log.read_from(cursor.log_seq + 1).await;
            let Some(last_seq) = batch.last().map(|e| e.seq) else {
                break;
            };
            let mut wire_seq = cursor.wire_seq;
            for env in batch {
                // wire seqs are the receiver's in-order contract: a seq is
                // consumed ONLY by a fully accepted send — a locally
                // skipped envelope (encode/wrap failure) must not leave an
                // unfillable hole that wedges the receiver's cursor
                if env.by == cfg.member
                    && send_one(&transport, &cfg, &peer, &sink, env, wire_seq + 1, &mut rng, mls.as_ref())
                        .await
                        .is_ok()
                {
                    wire_seq += 1;
                }
            }
            // one snapshot per drained batch: a cursor that is stale by a
            // crash only costs resends, which the peer's dedup absorbs
            advance_outbound(&state, &store, &peer.member, last_seq, wire_seq).await;
            if last_seq <= cursor.log_seq {
                break; // defensive: a log that stopped growing
            }
        }
        if wakeup.changed().await.is_err() {
            return; // engine gone
        }
    }
}

/// Send one envelope to one peer: jitter, chunk, wrap, then every block
/// with jittered exponential backoff until the transport accepts it.
/// `Err` means the envelope was skipped for a *local* reason (encode,
/// chunk or wrap failure) — the caller must then not consume the wire
/// seq. Transport failures never skip: they retry until accepted.
#[allow(clippy::too_many_arguments)]
async fn send_one<T, K>(
    transport: &T,
    cfg: &NetConfig,
    peer: &PeerLink,
    sink: &K,
    env: EventEnvelope,
    wire_seq: u64,
    rng: &mut u64,
    mls: Option<&MlsChannel>,
) -> Result<(), ()>
where
    T: Transport,
    K: EngineSink,
{
    // MLS path: the payload is the group ciphertext, computed once per log seq
    // and reused for every fan-out copy; the per-queue wrap below still makes
    // the copies byte-distinct. Plaintext path: the versioned WireFrame, the
    // receiver's per-link order/dedup key.
    let (payload, id) = match mls {
        Some(ch) => {
            let Some(ct) = ch.ciphertext_for(env.seq, &env) else {
                tracing::error!("MLS-encrypting an envelope failed — skipping it");
                return Err(());
            };
            (ct, msg_id(&cfg.member, &peer.member, env.seq))
        }
        None => {
            let frame = WireFrame { v: 1, seq: wire_seq, env };
            let Ok(payload) = serde_json::to_vec(&frame) else {
                tracing::error!("encoding a wire frame failed — skipping the envelope");
                return Err(());
            };
            (payload, msg_id(&cfg.member, &peer.member, wire_seq))
        }
    };
    let chunks = match chunk_message(id, &payload) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "chunking failed — skipping the envelope");
            return Err(());
        }
    };
    // independent uniform jitter before this send's dispatch (§5); it
    // delays, it never reorders within one member (this task is the
    // per-member sub-queue)
    if cfg.jitter_max_ms > 0 {
        let jitter = mockrand::xorshift(rng) % (cfg.jitter_max_ms + 1);
        tokio::time::sleep(Duration::from_millis(jitter)).await;
    }
    for chunk in chunks {
        let block = match wrap(&peer.wrap_out, &chunk) {
            Ok(b) => b,
            Err(e) => {
                // abort the whole message: already-sent blocks are junk
                // the receiver's partial eviction cleans up; the wire seq
                // stays unconsumed
                tracing::error!(error = %e, "wrapping failed — skipping the envelope");
                return Err(());
            }
        };
        let mut attempt: u32 = 0;
        loop {
            match transport.send(&peer.snd, block.clone()).await {
                Ok(()) => break,
                Err(e) => {
                    if attempt == 0 {
                        sink.send_failed(&peer.member, &e.to_string()).await;
                    }
                    let backoff = backoff_ms(cfg, attempt, rng);
                    tracing::debug!(peer = %peer.member, error = %e, backoff_ms = backoff, "send failed — backing off");
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
    Ok(())
}

/// Jittered exponential backoff: `base * 2^attempt`, capped, uniformly
/// jittered in `[½x, 1½x)`.
fn backoff_ms(cfg: &NetConfig, attempt: u32, rng: &mut u64) -> u64 {
    let raw = cfg
        .retry_base_ms
        .saturating_mul(1u64 << attempt.min(20))
        .min(cfg.retry_cap_ms)
        .max(1);
    raw / 2 + mockrand::xorshift(rng) % raw
}

/// Record outbound progress and persist a snapshot.
async fn advance_outbound<S: StateStore>(
    state: &Arc<Mutex<TransportState>>,
    store: &S,
    member: &MemberId,
    log_seq: u64,
    wire_seq: u64,
) {
    let snapshot = {
        let Ok(mut s) = state.lock() else { return };
        let cur = s.outbound.entry(member.clone()).or_default();
        cur.log_seq = log_seq;
        cur.wire_seq = wire_seq;
        s.clone()
    };
    store.save(snapshot).await;
}

/// Partially received or gap-buffered messages tracked per peer; beyond
/// this the ack side sheds load onto the transport's redelivery (aligned
/// with the reassembler's own partial cap).
const CHUNK_ACK_MAX: usize = 64;

/// The per-peer receive loop: unwrap → reassemble → parse → per-sender
/// in-order delivery, ack after the engine accepted.
///
/// Ack discipline (load-bearing): a redelivered copy carries the SAME
/// transport pending entry as its original — acking a copy of a message
/// the engine has *not* yet accepted would erase the only durable copy of
/// an in-memory-buffered message (lost on restart). So duplicates are
/// *attached* to wherever the original's acks are held and fire together
/// on acceptance; only copies of already-accepted messages ack
/// immediately.
#[allow(clippy::too_many_arguments)]
async fn recv_task<S, K>(
    peer: PeerLink,
    mut rx: tokio::sync::mpsc::Receiver<crate::Delivery>,
    store: S,
    sink: K,
    state: Arc<Mutex<TransportState>>,
    mls: Option<MlsChannel>,
) where
    S: StateStore,
    K: EngineSink,
{
    let mut reasm = Reassembler::new();
    // acks of buffered chunks, keyed by message id
    let mut chunk_acks: HashMap<[u8; 16], Vec<AckToken>> = HashMap::new();
    // out-of-order complete messages: wire seq → (envelope, its acks)
    let mut reorder: BTreeMap<u64, (EventEnvelope, Vec<AckToken>)> = BTreeMap::new();
    // message id → wire seq for messages sitting in `reorder`
    let mut buffered_ids: HashMap<[u8; 16], u64> = HashMap::new();
    // this task is the sole writer of inbound[peer]; the shared state is
    // only the persistence snapshot
    let mut cursor = state
        .lock()
        .ok()
        .and_then(|s| s.inbound.get(&peer.member).copied())
        .unwrap_or(0);
    while let Some(delivery) = rx.recv().await {
        let plain = match unwrap_block(&peer.wrap_in, &delivery.block) {
            Ok(p) => p,
            Err(e) => {
                // undecryptable: redelivery cannot help — ack it away
                tracing::warn!(peer = %peer.member, error = %e, "dropping an undecryptable block");
                delivery.ack.ack();
                continue;
            }
        };
        let (id, complete) = match reasm.push(&plain) {
            Ok(PushOutcome::Duplicate(id)) => {
                if let Some(held) = chunk_acks.get_mut(&id.0) {
                    held.push(delivery.ack); // message still partial
                } else if let Some(held) = buffered_ids
                    .get(&id.0)
                    .and_then(|seq| reorder.get_mut(seq))
                {
                    held.1.push(delivery.ack); // message buffered behind a gap
                } else {
                    delivery.ack.ack(); // message was accepted — safe
                }
                continue;
            }
            Ok(PushOutcome::Buffered(id)) => {
                chunk_acks.entry(id.0).or_default().push(delivery.ack);
                if chunk_acks.len() > CHUNK_ACK_MAX {
                    // shed an arbitrary partial's acks (unacked → the
                    // transport redelivers, reassembly restarts)
                    if let Some(k) = chunk_acks.keys().next().copied() {
                        chunk_acks.remove(&k);
                        tracing::warn!(peer = %peer.member, "ack buffer full — shedding a partial onto redelivery");
                    }
                }
                continue;
            }
            Ok(PushOutcome::Complete(id, bytes)) => (id, bytes),
            Err(e) => {
                tracing::warn!(peer = %peer.member, error = %e, "dropping a malformed chunk");
                delivery.ack.ack();
                continue;
            }
        };
        let mut acks = chunk_acks.remove(&id.0).unwrap_or_default();
        acks.push(delivery.ack);

        // MLS path: the reassembled bytes are group ciphertext. Decrypt to the
        // authenticated sender + envelope; MLS itself rejects replays, so a
        // duplicate/undecryptable message is just acked away. Ordering is MLS's
        // job, so the per-link reorder buffer below does not apply.
        if let Some(ch) = &mls {
            match ch.decode(&complete) {
                Some((from, env)) => {
                    sink.peer_seen(&peer.member).await;
                    if sink.deliver(&from, env).await.is_err() {
                        tracing::debug!(peer = %peer.member, "engine gone — recv task stops");
                        return;
                    }
                    ack_all(acks);
                }
                None => ack_all(acks), // replay / commit / undecryptable
            }
            continue;
        }

        let frame: WireFrame = match serde_json::from_slice(&complete) {
            Ok(f) => f,
            Err(e) => {
                // a wire frame we cannot parse cannot advance the cursor;
                // ack it away and degrade loudly (concept: explicit
                // degradation, never silent drops)
                tracing::error!(peer = %peer.member, error = %e, "unparseable wire frame — acked and dropped");
                ack_all(acks);
                continue;
            }
        };
        if frame.env.by != peer.member {
            tracing::warn!(peer = %peer.member, claimed = %frame.env.by, "envelope author does not match the link — dropped");
            ack_all(acks);
            continue;
        }
        // any authenticated traffic is passive presence
        sink.peer_seen(&peer.member).await;

        if frame.seq <= cursor {
            ack_all(acks); // replay of an accepted message
            continue;
        }
        if let Some(held) = reorder.get_mut(&frame.seq) {
            held.1.append(&mut acks); // replay of a message we still hold
            continue;
        }
        if reorder.len() >= REORDER_BUFFER_MAX && frame.seq != cursor + 1 {
            // bounded memory: un-remember the message so its redelivered
            // chunks reassemble again, and leave the blocks unacked
            reasm.forget(id);
            tracing::warn!(peer = %peer.member, seq = frame.seq, "reorder buffer full — deferring to redelivery");
            continue;
        }
        reorder.insert(frame.seq, (frame.env, acks));
        buffered_ids.insert(id.0, frame.seq);

        // drain the contiguous run
        while let Some((env, acks)) = reorder.remove(&(cursor + 1)) {
            if sink.deliver(&peer.member, env).await.is_err() {
                tracing::debug!(peer = %peer.member, "engine gone — recv task stops");
                return;
            }
            cursor += 1;
            let snapshot = {
                let Ok(mut s) = state.lock() else { return };
                s.inbound.insert(peer.member.clone(), cursor);
                s.clone()
            };
            store.save(snapshot).await;
            ack_all(acks);
        }
        buffered_ids.retain(|_, seq| *seq > cursor);
    }
}

fn ack_all(acks: Vec<AckToken>) {
    for a in acks {
        a.ack();
    }
}

/// Send one whole message over a queue: chunk → wrap → send every block,
/// retrying the transport until it accepts (short, bounded — the founding
/// ritual's messages are tiny and the loopback hub rarely refuses). The
/// one-shot counterpart to the outbox's fan-out, used by the founding
/// ritual (transport concept §3.3) where there is no per-member cursor,
/// just a handful of request/table/signature exchanges.
pub async fn send_framed<T: Transport>(
    transport: &T,
    addr: &crate::SndQueueAddr,
    wrap_key: &WrapKey,
    id: crate::MsgId,
    payload: &[u8],
) -> Result<(), NetError> {
    for chunk in chunk_message(id, payload)? {
        let block = wrap(wrap_key, &chunk)?;
        let mut tries = 0u32;
        loop {
            match transport.send(addr, block.clone()).await {
                Ok(()) => break,
                Err(e) if tries < 100 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// In-memory implementations (loopback demo, tests)
// ---------------------------------------------------------------------------

/// An in-memory [`OutboxLog`]: the demo's stand-in for the workspace log
/// (session-only workspaces persist nothing). The engine pushes every
/// recorded envelope here and bumps the wakeup.
#[derive(Clone, Default)]
pub struct MemLog {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl MemLog {
    /// An empty log.
    pub fn new() -> MemLog {
        MemLog::default()
    }

    /// Append one envelope (seqs must arrive in order — the engine's
    /// monotonic counter guarantees it).
    pub fn push(&self, env: EventEnvelope) {
        if let Ok(mut e) = self.events.lock() {
            e.push(env);
        }
    }
}

impl OutboxLog for MemLog {
    async fn read_from(&self, from_seq: u64) -> Vec<EventEnvelope> {
        self.events
            .lock()
            .map(|e| {
                // seqs are appended in order: binary-search the start
                // instead of scanning the whole session
                let start = e.partition_point(|x| x.seq < from_seq);
                e[start..].to_vec()
            })
            .unwrap_or_default()
    }
}

/// An in-memory [`StateStore`]. Keep it outside the supervisor to carry
/// cursors across a simulated restart.
#[derive(Clone, Default)]
pub struct MemStateStore {
    state: Arc<Mutex<TransportState>>,
}

impl MemStateStore {
    /// A fresh store (default cursors).
    pub fn new() -> MemStateStore {
        MemStateStore::default()
    }
}

impl StateStore for MemStateStore {
    async fn load(&self) -> TransportState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    async fn save(&self, state: TransportState) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_link_round_trips_through_a_mesh_handover() {
        let link = PeerLink {
            member: "bob".to_string(),
            snd: SndQueueAddr {
                server: "smp://fp@host".to_string(),
                id: crate::QueueId::from_bytes(vec![1, 2, 3, 4]),
            },
            wrap_out: WrapKey::from_bytes([7u8; 32]),
            rcv: crate::RcvQueue {
                id: crate::QueueId::from_bytes(vec![9, 8, 7]),
            },
            wrap_in: WrapKey::from_bytes([3u8; 32]),
        };
        let mesh = link.to_mesh();
        assert_eq!(mesh.member, "bob");
        assert_eq!(mesh.snd_server, "smp://fp@host");
        let back = PeerLink::from_mesh(&mesh).expect("round trips");
        assert_eq!(back.member, link.member);
        assert_eq!(back.snd.server, link.snd.server);
        assert_eq!(back.snd.id.0, link.snd.id.0);
        assert_eq!(back.wrap_out.to_bytes(), link.wrap_out.to_bytes());
        assert_eq!(back.rcv.id.0, link.rcv.id.0);
        assert_eq!(back.wrap_in.to_bytes(), link.wrap_in.to_bytes());
    }

    #[test]
    fn a_corrupt_mesh_entry_is_dropped_not_panicked() {
        let bad = molt_core::MeshLink {
            member: "bob".to_string(),
            snd_server: String::new(),
            snd_queue: "nothex".to_string(),
            snd_wrap: "zz".to_string(),
            rcv_queue: String::new(),
            rcv_wrap: String::new(),
        };
        assert!(PeerLink::from_mesh(&bad).is_none());
    }
}
