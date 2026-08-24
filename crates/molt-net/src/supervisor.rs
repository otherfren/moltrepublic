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

use molt_core::{mockrand, EventEnvelope, MemberId, TransportState, WorkspaceEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinSet;

use crate::chunk::{chunk_message, msg_id, PushOutcome, Reassembler};
use crate::mls::{MlsIncoming, MlsMember};
use crate::wrap::{unwrap_block, wrap, WrapKey};
use crate::{AckToken, NetError, RcvQueue, SndQueueAddr, Transport};

/// The node's MLS group at runtime (T2): the confidentiality layer whose
/// ciphertext is the wire payload. Shared by every per-peer outbox/recv task of a
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
    /// Bumped whenever a commit merges (the epoch advanced). The group is
    /// node-global but each per-peer recv loop holds its own future-epoch
    /// buffer — a commit arriving on ONE link must wake every OTHER link's
    /// loop to retry its held messages, or they sit there for the session.
    epoch_bump: Arc<watch::Sender<u64>>,
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
            epoch_bump: Arc::new(watch::channel(0u64).0),
        }
    }

    /// The MLS ciphertext for `env` AND the exporter secret of the epoch it
    /// was made at, under ONE lock.
    ///
    /// Split into two calls, a commit merging in between seals a NEW-epoch
    /// outer over an OLD-epoch inner: the receiver strips the outer with its
    /// current secret and can then never decrypt the inner (`max_past_epochs`
    /// is 0). Taken together, that state is unrepresentable.
    ///
    /// Deliberately bypasses the fan-out `cache`: a broadcast publishes once
    /// per seq, so there is no second consumer — and without the cache every
    /// rewind-resend is a fresh encryption at the current epoch BY
    /// CONSTRUCTION, which is what the guarantee requires anyway.
    pub(crate) fn group_frame(&self, env: &EventEnvelope) -> Option<GroupFrame> {
        let mut m = self.member.lock().ok()?;
        // a re-key commit is an MLS handshake message: it ships RAW, never
        // wrapped in an application ciphertext (one encrypted at the old epoch
        // could never be processed — the recipient needs it to REACH the new
        // epoch). Same branch `ciphertext_for` keeps for the queue path.
        let (ciphertext, exporter) = if let WorkspaceEvent::MlsCommit { commit } = &env.body {
            // …and on 445 its OUTER layer must be sealed under the epoch its
            // RECIPIENTS are still at — the epoch the commit was MADE at
            // (`detached_reattach.md` §7): a RESEND after further commits
            // must stay openable to exactly the laggards it exists to move
            // forward, and "one epoch back" stopped being that the moment a
            // second commit landed. The commit's own bytes carry its epoch;
            // the ring maps it to the exporter (head-aligned labels, v3).
            // Fallback one-back covers legacy rings restored without labels.
            // (The queue path has no outer layer, which is why this only
            // bites here.)
            let bytes = hex::decode(commit).ok()?;
            let exporter = crate::mls::wire_epoch(&bytes)
                .and_then(|e| m.exporter_for_epoch(e))
                .or_else(|| m.exporter_ring().first().copied())?;
            (bytes, exporter)
        } else {
            let plaintext = serde_json::to_vec(env).ok()?;
            let ct = m.encrypt(&plaintext).ok()?;
            let exporter = m.exporter_secret().ok()?;
            (ct, exporter)
        };
        Some(GroupFrame { ciphertext, exporter })
    }

    /// Ciphertext + the exporter secret of the epoch it was made at, under ONE
    /// lock, for arbitrary CONTROL plaintext (a broadcast ack).
    ///
    /// Same atomicity rule as [`Self::group_frame`], and the same reason: two
    /// calls, and a commit merging between them seals a new-epoch outer over
    /// an old-epoch inner that the receiver can then never open.
    pub(crate) fn group_control_frame(&self, plaintext: &[u8]) -> Option<GroupFrame> {
        let mut m = self.member.lock().ok()?;
        let ciphertext = m.encrypt(plaintext).ok()?;
        let exporter = m.exporter_secret().ok()?;
        Some(GroupFrame { ciphertext, exporter })
    }

    /// The outer-opening ladder: the CURRENT epoch's exporter secret first,
    /// then the retained ring, in one lock scope.
    ///
    /// One definition of a ladder that otherwise gets rebuilt inline at every
    /// call site — and the ring is why a laggard past `EXPORTER_RING_K` epoch
    /// changes cannot strip the outer layer at all, however faithfully the
    /// relay stored the frame.
    pub(crate) fn exporter_secrets(&self) -> Vec<[u8; 32]> {
        let Ok(m) = self.member.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Ok(cur) = m.exporter_secret() {
            out.push(cur);
        }
        out.extend_from_slice(m.exporter_ring());
        out
    }

    /// Subscribe to node-wide epoch advances (one bump per merged commit,
    /// whichever link it arrived on).
    pub(crate) fn epoch_watch(&self) -> watch::Receiver<u64> {
        self.epoch_bump.subscribe()
    }

    /// The MLS ciphertext for one outbound envelope, encrypting exactly once per
    /// `seq` (subsequent fan-out copies reuse the cached bytes — re-encrypting
    /// would double-advance the ratchet). `None` on a local encode/crypto error.
    fn ciphertext_for(&self, seq: u64, env: &EventEnvelope) -> Option<Vec<u8>> {
        if let Some(c) = self.cache.lock().ok()?.get(&seq) {
            return Some(c.clone());
        }
        // A re-key commit is itself an MLS handshake message: it is sent RAW, not
        // wrapped in an application ciphertext (a commit encrypted at the old
        // epoch could never be processed — the recipient needs it to REACH the
        // new epoch). The receiver's `decrypt` recognises it as a commit and
        // merges it. It still rides this per-link stream in order with chat.
        if let WorkspaceEvent::MlsCommit { commit } = &env.body {
            let raw = hex::decode(commit).ok()?;
            self.cache.lock().ok()?.insert(seq, raw.clone());
            return Some(raw);
        }
        let plaintext = serde_json::to_vec(env).ok()?;
        let mut m = self.member.lock().ok()?;
        let c = m.encrypt(&plaintext).ok()?;
        self.cache.lock().ok()?.insert(seq, c.clone());
        Some(c)
    }

    /// Drop cached ciphertexts for every seq ABOVE `floor` (rewind eviction,
    /// delivery guarantee §4.5): a resend must be a FRESH encryption at the
    /// current ratchet position/epoch — resending original bytes the peer
    /// already decrypted would be MLS-replay-rejected forever, and bytes
    /// encrypted at a left-behind epoch can never decrypt again.
    fn evict_above(&self, floor: u64) {
        if let Ok(mut c) = self.cache.lock() {
            let _ = c.split_off(&(floor + 1));
        }
    }

    /// Drop cached ciphertexts at or BELOW `floor` (memory bound, §4.5):
    /// once every acking peer confirmed a seq, its ciphertext can never be
    /// needed again. A non-acking (old) peer that still needs such a seq
    /// just pays one re-encryption — never a correctness cost.
    fn evict_at_or_below(&self, floor: u64) {
        if let Ok(mut c) = self.cache.lock() {
            let keep = c.split_off(&(floor + 1));
            *c = keep;
        }
    }

    /// Decrypt one inbound MLS message, classified for the recv loop's ack
    /// discipline. MLS itself rejects replays, so there is no separate dedup
    /// window here.
    fn decode(&self, wire: &[u8]) -> MlsDecode {
        self.decode_at(wire, crate::mls::NO_CARRIER_STAMP)
    }

    /// [`Self::decode`] with the carrier event's `created_at`.
    ///
    /// kind-445 is the FIRST transport that carries one, and it is half of the
    /// `CommitKey` that breaks a concurrent same-epoch commit race. The queue
    /// path has no per-event stamp, so it keeps passing `NO_CARRIER_STAMP` —
    /// symmetric zero on both sides, which degrades the order to the digest
    /// rather than making our own commit always lose.
    pub(crate) fn decode_at(&self, wire: &[u8], created_at: u64) -> MlsDecode {
        let Ok(mut m) = self.member.lock() else {
            return MlsDecode::Discard;
        };
        match m.decrypt_at(wire, created_at) {
            Ok(MlsIncoming::Application { from, plaintext }) => {
                // a delivery ACK (delivery guarantee §4.3): the peer reports
                // what it has engine-accepted of OUR events. Authenticated by
                // the MLS credential (`from`) — the recv loop additionally
                // pins it to the link's member before applying it. Checked
                // BEFORE the envelope parse; its NUL-prefixed tag can never be
                // valid `EventEnvelope` JSON.
                if let Some(payload) = plaintext.strip_prefix(crate::MESH_ACK_TAG) {
                    return match serde_json::from_slice::<molt_core::AcceptedWindow>(payload) {
                        Ok(win) => MlsDecode::Ack(from, Box::new(win)),
                        Err(_) => MlsDecode::Discard,
                    };
                }
                // the BROADCAST ack (N5.3): its own tag, because one 445
                // reaches everyone and the sheet must say whose acceptance it
                // reports — the leg that answered that on the mesh is gone
                match crate::group_ack::GroupAck::from_frame(&plaintext) {
                    Ok(ack) => return MlsDecode::GroupAck(from, Box::new(ack)),
                    Err(crate::group_ack::GroupAckError::NotAnAck) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "dropping an unusable group ack");
                        return MlsDecode::Discard;
                    }
                }
                // a poke: its own tag for the same reason as the acks — the
                // tag IS the version boundary, and an older build that
                // predates it lands in the NUL discard below
                match crate::poke::Poke::from_frame(&plaintext) {
                    Ok(poke) => return MlsDecode::Poke(from, Box::new(poke)),
                    Err(crate::poke::PokeError::NotAPoke) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "dropping an unusable poke");
                        return MlsDecode::Discard;
                    }
                }
                // the `\x00molt-mesh-*` space is reserved for control frames; a
                // JSON envelope never starts with NUL. An unknown control tag (a
                // newer control frame this build predates) is dropped as a no-op,
                // never mis-parsed as an event.
                if plaintext.first() == Some(&0) {
                    return MlsDecode::Discard;
                }
                match serde_json::from_slice::<EventEnvelope>(&plaintext) {
                    Ok(env) => MlsDecode::Deliver(from, Box::new(env)),
                    Err(_) => MlsDecode::Discard,
                }
            }
            // a merged commit advanced the epoch — held future-epoch messages
            // may decrypt now; wake every recv loop of this node (the buffers
            // are per-link, the group is not)
            Ok(MlsIncoming::Commit { readmitted }) => {
                self.epoch_bump.send_modify(|n| *n = n.wrapping_add(1));
                MlsDecode::EpochAdvanced(readmitted)
            }
            // its commit is still in flight — hold the SAME bytes and retry
            Ok(MlsIncoming::FutureEpoch) => MlsDecode::FutureEpoch,
            // a concurrent same-epoch commit that LOST the tiebreak: our own
            // commit stands and the sender rewinds to it, so nothing changed
            // here — but the epoch did not advance either, so this is not an
            // EpochAdvanced (N3 §1).
            Ok(MlsIncoming::CommitSuperseded) => MlsDecode::Discard,
            // we rewound onto a concurrent commit: the epoch advanced, AND
            // whatever our own commit carried (a recovery re-key) is gone.
            // Loud, because the member it was for may hold a Welcome for a
            // branch nobody is on — N4 re-issues it against the new epoch.
            Ok(MlsIncoming::CommitRewound { readmitted }) => {
                tracing::warn!(
                    "a concurrent commit won the tiebreak — our own commit was rolled back; \
                     any re-key it carried must be re-issued at the new epoch"
                );
                self.epoch_bump.send_modify(|n| *n = n.wrapping_add(1));
                MlsDecode::EpochAdvanced(readmitted)
            }
            // proposals / replays / past-window / garbage: redelivery cannot help
            Ok(MlsIncoming::Proposal) | Err(_) => MlsDecode::Discard,
        }
    }
}

/// One outbound group frame: the MLS ciphertext and the exporter secret of
/// the epoch it was produced at. Always made together — see
/// [`MlsChannel::group_frame`].
pub(crate) struct GroupFrame {
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) exporter: [u8; 32],
}

/// What the recv loop should do with one inbound MLS message.
#[derive(Debug)]
pub(crate) enum MlsDecode {
    /// An authenticated application envelope — deliver and ack.
    Deliver(MemberId, Box<EventEnvelope>),
    /// A delivery ACK (delivery guarantee §4.3): the authenticated sender
    /// reports its accept window over OUR events — advance that peer's
    /// `acked_floor`, stamp presence, deliver nothing.
    Ack(MemberId, Box<molt_core::AcceptedWindow>),
    /// A BROADCAST claim sheet (N5.3): the authenticated sender reports what
    /// it accepted, per subject. The receiver acts only on its OWN entry.
    GroupAck(MemberId, Box<crate::group_ack::GroupAck>),
    /// A poke: an ephemeral, directed nudge. Everyone in the group decrypts
    /// it, only the named target reacts — and only behind that node's own
    /// opt-in. Carries no state, so it is deliberately NOT a log event.
    Poke(MemberId, Box<crate::poke::Poke>),
    /// A commit merged (epoch advanced) — ack it and retry the epoch buffer.
    /// `readmitted` names the members the commit ADDED (a recovery re-key):
    /// the consumer forwards them to the engine BEFORE anything of the new
    /// epoch, so the engine forgets their old incarnation's accept window
    /// in-stream (live incident 2026-08-09 §2).
    EpochAdvanced(Vec<MemberId>),
    /// Encrypted at an epoch this node has not reached — hold it (acks
    /// unfired) and retry after the next commit merges.
    FutureEpoch,
    /// Replay / proposal / undecryptable — ack it away.
    Discard,
}

/// Out-of-order inbound messages buffered per peer before the incoming
/// excess is dropped back onto the transport's redelivery.
const REORDER_BUFFER_MAX: usize = 512;

/// Delivery guarantee §4.4: how long an unacked tail may sit (with a live,
/// proven-acking leg) before the outbox rewinds and re-offers it. 10 s (was
/// 30): the ack latency is ≤ ~4 s since the 1 s delivery tick, and with G7
/// in-order holds a missing predecessor delays its successors' visibility —
/// the first re-offer must come fast (the live evaluation saw a lost A keep
/// B invisible for the better part of a minute).
const RESEND_AFTER_SECS: u64 = 10;

/// Cap for the per-peer resend backoff (doubling from
/// [`RESEND_AFTER_SECS`]); resends never stop inside the horizon, they only
/// slow down to this pace.
const RESEND_MAX_BACKOFF_SECS: u64 = 600;

/// After this many rewinds without any floor progress the stall is reported
/// on the health surface (loud, honest — G4); resending continues.
const RESEND_GIVEUP_REWINDS: u32 = 8;

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
    /// An authenticated poke from `member` addressed at `to`. Default no-op
    /// so existing sinks keep compiling (additive). The engine decides
    /// whether it is for this seat and whether this node reacts at all.
    fn poked(&self, member: &MemberId, to: &MemberId) -> impl std::future::Future<Output = ()> + Send {
        let _ = (member, to);
        async {}
    }
    /// Sends to `member` keep failing; the outbox is backing off.
    fn send_failed(
        &self,
        member: &MemberId,
        reason: &str,
    ) -> impl std::future::Future<Output = ()> + Send;
    /// The inbound leg from `member` is live (subscription established).
    /// Default no-op so existing sinks keep compiling (Stage B, additive).
    fn link_up(&self, member: &MemberId) -> impl std::future::Future<Output = ()> + Send {
        let _ = member;
        async {}
    }
    /// The inbound leg from `member` ended/failed; the resubscribe watchdog
    /// is backing off and retrying. Default no-op (Stage B, additive).
    fn link_down(
        &self,
        member: &MemberId,
        reason: &str,
    ) -> impl std::future::Future<Output = ()> + Send {
        let _ = (member, reason);
        async {}
    }
    /// A previously failing send to `member` went through again — the
    /// backoff exit signal. Default no-op (Stage B, additive).
    fn send_ok(&self, member: &MemberId) -> impl std::future::Future<Output = ()> + Send {
        let _ = member;
        async {}
    }
    /// A merged re-key commit re-admitted `member` as a NEW incarnation
    /// whose log seq space restarts. The engine must forget its accept
    /// window for that member BEFORE any frame of the new epoch reaches
    /// `deliver` — this call rides the same ordered path, which is what
    /// makes the reset race-free (a bystander otherwise swallows the new
    /// incarnation's colliding seqs as duplicates and falsely acks them —
    /// live incident 2026-08-09 §2). Default no-op (additive).
    fn rekeyed(&self, member: &MemberId) -> impl std::future::Future<Output = ()> + Send {
        let _ = member;
        async {}
    }
}

/// One fully wired peer connection: the peer's inbound queue send address and
/// wrap key (we send), our inbound queue and its wrap key (we receive).
/// `snds`/`rcvs` keep their Vec-of-1 shape from the removed multi-queue
/// redundancy: every mint site produces exactly one queue per directed pair
/// and the runtime drives only index 0 (N5's relay runtime reshapes
/// `PeerLink`); a longer persisted list reloads capped and rides along inert.
#[derive(Debug, Clone)]
pub struct PeerLink {
    /// The peer this link reaches.
    pub member: MemberId,
    /// Send side of the peer's inbound queue. Non-empty; index 0 is the one
    /// the outbox sends to.
    pub snds: Vec<SndQueueAddr>,
    /// Wrap key of the peer's inbound direction.
    pub wrap_out: WrapKey,
    /// Our inbound queue from this peer. Non-empty; index 0 is the one the
    /// recv loop subscribes.
    pub rcvs: Vec<RcvQueue>,
    /// Wrap key of our inbound direction.
    pub wrap_in: WrapKey,
}

/// Cap on the per-leg queue count reloaded from a persisted
/// [`molt_core::MeshLink`] (SECURITY, Stage-2 audit finding #1, kept after
/// the redundancy removal): a tampered/inflated `transport.state` must never
/// reload an unbounded queue list that survives reopen. Nothing mints more
/// than one queue per directed pair anymore; old state may still carry one
/// extra.
const MESH_LINK_QUEUE_CAP: usize = 2;

impl PeerLink {
    /// The primary (index 0) send address — the outbox always has ≥1 target.
    pub fn snd0(&self) -> &SndQueueAddr {
        &self.snds[0]
    }

    /// The primary (index 0) receive queue.
    pub fn rcv0(&self) -> &RcvQueue {
        &self.rcvs[0]
    }

    /// Persist this link as a [`molt_core::MeshLink`] (hex-encoded) for
    /// `transport.state`. The primary queue is the scalar `snd_*`/`rcv_*`
    /// fields; queues 1..N ride the additive `snd_extra`/`rcv_extra` vectors.
    pub fn to_mesh(&self) -> molt_core::MeshLink {
        let snd0 = self.snd0();
        let rcv0 = self.rcv0();
        molt_core::MeshLink {
            member: self.member.clone(),
            snd_server: snd0.server.clone(),
            snd_queue: hex::encode(&snd0.id.0),
            snd_wrap: hex::encode(self.wrap_out.to_bytes()),
            rcv_queue: hex::encode(&rcv0.id.0),
            rcv_wrap: hex::encode(self.wrap_in.to_bytes()),
            rcv_server: rcv0.server.clone(),
            snd_extra: self.snds[1..]
                .iter()
                .map(|a| molt_core::QueueRef {
                    server: a.server.clone(),
                    queue: hex::encode(&a.id.0),
                })
                .collect(),
            rcv_extra: self.rcvs[1..]
                .iter()
                .map(|r| molt_core::QueueRef {
                    server: r.server.clone(),
                    queue: hex::encode(&r.id.0),
                })
                .collect(),
        }
    }

    /// Rebuild a link from a persisted [`molt_core::MeshLink`]. `None` on any
    /// malformed hex — a corrupt mesh entry drops that peer, never panics. An
    /// extra queue with malformed hex is skipped (the leg survives on the rest).
    pub fn from_mesh(m: &molt_core::MeshLink) -> Option<PeerLink> {
        let snd_wrap: [u8; 32] = hex::decode(&m.snd_wrap).ok()?.try_into().ok()?;
        let rcv_wrap: [u8; 32] = hex::decode(&m.rcv_wrap).ok()?.try_into().ok()?;
        // SECURITY (Stage-2 audit finding #1): cap the reloaded queue count so
        // a tampered/inflated `transport.state` can never reload an unbounded
        // queue list that survives reopen.
        let cap = MESH_LINK_QUEUE_CAP.max(1);
        let mut snds = vec![SndQueueAddr {
            server: m.snd_server.clone(),
            id: crate::QueueId::from_bytes(hex::decode(&m.snd_queue).ok()?),
        }];
        for x in &m.snd_extra {
            if snds.len() >= cap {
                break;
            }
            if let Ok(id) = hex::decode(&x.queue) {
                snds.push(SndQueueAddr {
                    server: x.server.clone(),
                    id: crate::QueueId::from_bytes(id),
                });
            }
        }
        let mut rcvs = vec![RcvQueue {
            server: m.rcv_server.clone(),
            id: crate::QueueId::from_bytes(hex::decode(&m.rcv_queue).ok()?),
        }];
        for x in &m.rcv_extra {
            if rcvs.len() >= cap {
                break;
            }
            if let Ok(id) = hex::decode(&x.queue) {
                rcvs.push(RcvQueue {
                    server: x.server.clone(),
                    id: crate::QueueId::from_bytes(id),
                });
            }
        }
        Some(PeerLink {
            member: m.member.clone(),
            snds,
            wrap_out: WrapKey::from_bytes(snd_wrap),
            rcvs,
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
        let mut loaded = store.load().await;
        // delivery guarantee §4.4, THE rewind rule: every supervisor build
        // (reopen, rotate adopt, mesh extension, recovery fold) re-offers each
        // proven-acking peer its unacked tail — the outbox then re-reads from
        // the floor and re-encrypts (this build's MLS cache starts empty)
        // under a fresh resend epoch (fresh msg ids, §4.5). Peers that never
        // acked (old nodes) keep exactly the pre-guarantee cursor behavior.
        rewind_unacked(&mut loaded);
        let state = Arc::new(Mutex::new(loaded));
        let mut children = JoinSet::new();
        // Every outbox task first: they park on the wakeup watch and dial
        // nothing until there is something to send, so spawning them up front
        // costs no round-trip.
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
        }
        // Circuit prebuild (concept §5, T4 §P4): open every inbound
        // subscription in parallel, bounded by PREBUILD_PARALLELISM, so n cold
        // Tor circuits build concurrently at workspace-open instead of one after
        // another. Because a node's send + recv connections to one server share
        // a circuit (the dialer's per-server isolation token), warming the recv
        // circuit here also warms the circuit the outbox pool will reuse, so the
        // first send is one round-trip, not a cold circuit build. Drain-don't-
        // abort is untouched: each recv task still lands in the JoinSet and is
        // aborted with the rest on stop; only the initial dials go concurrent.
        let subscribed = {
            let transport = transport.clone();
            let peers = cfg.peers.clone();
            prebuild_circuits(peers.len(), PREBUILD_PARALLELISM, move |i| {
                let transport = transport.clone();
                let rcv = peers[i].rcv0().clone();
                async move { transport.subscribe(&rcv).await }
            })
            .await
        };
        for (i, sub) in subscribed.into_iter().enumerate() {
            let peer = &cfg.peers[i];
            // the prebuild's first subscribe seeds the watchdog; a failed one is
            // NOT fatal — the watchdog redials with capped backoff (Stage B)
            let first = match sub {
                Some(Ok(rx)) => Some(rx),
                Some(Err(e)) => {
                    tracing::error!(peer = %peer.member, queue = %queue_tag(&peer.rcv0().id.0), error = %e, "subscribing an inbound queue failed — the watchdog will retry");
                    None
                }
                None => {
                    tracing::error!(peer = %peer.member, "prebuild subscribe task did not complete");
                    None
                }
            };
            let seed = cfg.seed.wrapping_add(
                0x9e37_79b9_7f4a_7c15u64
                    .wrapping_mul(1 + u64::try_from(i).unwrap_or_default()),
            ) | 1;
            children.spawn(recv_watchdog_task(
                transport.clone(),
                cfg.clone(),
                peer.clone(),
                log.clone(),
                store.clone(),
                sink.clone(),
                state.clone(),
                mls.clone(),
                first,
                seed,
            ));
        }
        stopped.notified().await;
        // dropping the JoinSet aborts every child task
        drop(children);
        tracing::debug!("net supervisor stopped");
    });
    SupervisorHandle { stop }
}

/// Circuit prebuild (concept §5, T4 §P4): run up to `max_in_flight` dials
/// concurrently, returning each result in input order (`None` only if that
/// dial task itself failed to join — a panic). Bounds cold-circuit builds at
/// workspace-open so the first send is one round-trip, not a serial wait on n
/// cold Tor circuits. Generic over the dial so the semaphore bound is testable
/// without a live server; the supervisor passes `Transport::subscribe`.
async fn prebuild_circuits<F, Fut, R>(
    count: usize,
    max_in_flight: usize,
    dial: F,
) -> Vec<Option<R>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let sem = Arc::new(Semaphore::new(max_in_flight.max(1)));
    let mut set = JoinSet::new();
    for i in 0..count {
        // acquire BEFORE spawning so at most `max_in_flight` dials run at once;
        // the permit rides into the task and releases when the dial completes.
        let Ok(permit) = sem.clone().acquire_owned().await else {
            break; // semaphore closed — never happens (we hold the only handle)
        };
        let fut = dial(i);
        set.spawn(async move {
            let _permit = permit;
            (i, fut.await)
        });
    }
    let mut out: Vec<Option<R>> = (0..count).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((i, r)) = joined {
            if let Some(slot) = out.get_mut(i) {
                *slot = Some(r);
            }
        }
    }
    out
}

/// How many circuits the prebuild opens at once (concept §5).
const PREBUILD_PARALLELISM: usize = 4;

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
    // Periodic-resend bookkeeping (delivery guarantee §4.4): while an unacked
    // tail exists toward a proven-acking peer, rewind to the floor on a
    // per-peer backoff (doubling, capped) until the floor moves. Escalation
    // resets on ANY floor progress; the give-up cap only makes the stall LOUD
    // (send_failed → honest Degraded naming the peer) — resends continue at
    // the max backoff, because the guarantee never silently abandons a
    // message inside the horizon (G4).
    let mut last_floor = state
        .lock()
        .ok()
        .and_then(|s| s.outbound.get(&peer.member).map(|c| c.acked_floor))
        .unwrap_or(0);
    let mut backoff_secs = RESEND_AFTER_SECS;
    let mut stalled_since: Option<tokio::time::Instant> = None;
    let mut rewinds_without_progress: u32 = 0;
    let mut stall_reported = false;
    // the (floor, head) span last verified to contain an OWN ackable event —
    // caches the tail check's log read (invalidated by any movement)
    let mut own_span: Option<(u64, u64)> = None;
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
                    && send_one(
                        &transport,
                        &cfg,
                        &peer,
                        &sink,
                        env,
                        wire_seq + 1,
                        &mut rng,
                        mls.as_ref(),
                        cursor.resend_epoch,
                    )
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
        // resend condition: a tail the peer has not confirmed, and the peer
        // has proven it acks at all (§4.8 — never resend-loop at an old node)
        let (floor, ack_seen, head) = state
            .lock()
            .ok()
            .and_then(|s| s.outbound.get(&peer.member).copied())
            .map(|c| (c.acked_floor, c.ack_seen, c.log_seq))
            .unwrap_or((0, false, 0));
        let mut tail = ack_seen && head > floor;
        // …but only an OWN, ackable event makes the tail real. A span of
        // purely foreign/commit entries (the quiet-listener steady state)
        // owes the peer nothing — self-advance the floor over it, WITHOUT
        // acks, so neither the stall clock nor the compaction gate trips
        // (E7 findings 1+2). One log read per (floor, head) span, cached.
        if tail && own_span != Some((floor, head)) {
            let has_own = log
                .read_from(floor + 1)
                .await
                .iter()
                .any(|e| e.seq <= head && own_ackable(&cfg.member, e));
            if has_own {
                own_span = Some((floor, head)); // genuine tail — remember it
            } else {
                record_acked(&state, &store, &peer.member, head).await;
                tail = false;
            }
        }
        if floor > last_floor {
            // the peer confirmed progress — de-escalate entirely
            last_floor = floor;
            backoff_secs = RESEND_AFTER_SECS;
            rewinds_without_progress = 0;
            stalled_since = None;
            if stall_reported {
                stall_reported = false;
                sink.send_ok(&peer.member).await;
            }
        }
        if !(ack_seen && tail) {
            stalled_since = None;
            if wakeup.changed().await.is_err() {
                return; // engine gone
            }
            continue;
        }
        let since = *stalled_since.get_or_insert_with(tokio::time::Instant::now);
        let deadline = since + Duration::from_secs(backoff_secs);
        match tokio::time::timeout_at(deadline, wakeup.changed()).await {
            Ok(Err(_)) => return, // engine gone
            Ok(Ok(())) => continue, // new work first; the stall clock keeps running
            Err(_) => {
                // the backoff elapsed with no floor progress: rewind to the
                // floor under a fresh resend epoch and re-offer the tail as
                // fresh encryptions with fresh msg ids (§4.4/§4.5)
                {
                    let Ok(mut s) = state.lock() else { return };
                    let cur = s.outbound.entry(peer.member.clone()).or_default();
                    cur.log_seq = cur.acked_floor;
                    cur.resend_epoch = cur.resend_epoch.saturating_add(1);
                }
                if let Some(ch) = mls.as_ref() {
                    ch.evict_above(floor);
                }
                rewinds_without_progress = rewinds_without_progress.saturating_add(1);
                tracing::warn!(
                    peer = %peer.member,
                    floor,
                    attempt = rewinds_without_progress,
                    next_backoff_secs = backoff_secs.saturating_mul(2).min(RESEND_MAX_BACKOFF_SECS),
                    "unacknowledged deliveries — resending the tail"
                );
                if rewinds_without_progress >= RESEND_GIVEUP_REWINDS && !stall_reported {
                    // loud, honest, and NOT a stop: the health surface names
                    // the peer while the resends keep trying at the cap
                    stall_reported = true;
                    sink.send_failed(
                        &peer.member,
                        "deliveries keep going unacknowledged — still resending",
                    )
                    .await;
                }
                backoff_secs = backoff_secs.saturating_mul(2).min(RESEND_MAX_BACKOFF_SECS);
                stalled_since = Some(tokio::time::Instant::now());
            }
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
    resend_epoch: u32,
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
                tracing::error!(total_skipped = count_skipped(), "MLS-encrypting an envelope failed — skipping it");
                return Err(());
            };
            // the resend epoch salts the id (§4.5): a rewound tail carries
            // fresh ids, so the receiver's completed ring can never swallow
            // a resend of a message it discarded undecrypted (V4)
            (ct, crate::msg_id_epoch(&cfg.member, &peer.member, env.seq, resend_epoch))
        }
        None => {
            let frame = WireFrame { v: 1, seq: wire_seq, env };
            let Ok(payload) = serde_json::to_vec(&frame) else {
                tracing::error!(total_skipped = count_skipped(), "encoding a wire frame failed — skipping the envelope");
                return Err(());
            };
            (payload, msg_id(&cfg.member, &peer.member, wire_seq))
        }
    };
    let chunks = match chunk_message(id, &payload) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, total_skipped = count_skipped(), "chunking failed — skipping the envelope");
            return Err(());
        }
    };
    // independent uniform jitter before this send's dispatch (§5); it
    // delays, it never reorders within one member (this task is the
    // per-member sub-queue). Drawn from the OS RNG (L12): xorshift64 is
    // invertible from a few observed outputs, so a timing observer could
    // reconstruct the schedule this jitter exists to blur; the xorshift
    // stays only as the never-fails fallback.
    if cfg.jitter_max_ms > 0 {
        let mut b = [0u8; 8];
        let draw = match getrandom::getrandom(&mut b) {
            Ok(()) => u64::from_le_bytes(b),
            Err(_) => mockrand::xorshift(rng),
        };
        tokio::time::sleep(Duration::from_millis(draw % (cfg.jitter_max_ms + 1))).await;
    }
    for chunk in chunks {
        let block = match wrap(&peer.wrap_out, &chunk) {
            Ok(b) => b,
            Err(e) => {
                // abort the whole message: already-sent blocks are junk
                // the receiver's partial eviction cleans up; the wire seq
                // stays unconsumed
                tracing::error!(error = %e, total_skipped = count_skipped(), "wrapping failed — skipping the envelope");
                return Err(());
            }
        };
        // single-target retry: a failed round backs off (jittered exponential)
        // and retries the same block until the transport accepts it — the
        // transport never skips, only local encode/wrap failures do.
        let snd = peer.snd0();
        let mut attempt: u32 = 0;
        loop {
            match transport.send(snd, block.clone()).await {
                Ok(()) => {
                    tracing::debug!(peer = %peer.member, queue = %queue_tag(&snd.id.0), "block sent");
                    if attempt > 0 {
                        // the backoff exit: sends to this member work again
                        sink.send_ok(&peer.member).await;
                    }
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        sink.send_failed(&peer.member, &e.to_string()).await;
                    }
                    let backoff = backoff_ms(cfg, attempt, rng);
                    tracing::debug!(peer = %peer.member, queue = %queue_tag(&snd.id.0), error = %e, backoff_ms = backoff, "block send failed — backing off");
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

/// Delivery guarantee §4.4: the build-time rewind. For every peer that has
/// EVER acked (§4.8 — old nodes keep the plain cursor semantics), pull the
/// outbox read position back to the acked floor and bump the resend epoch,
/// so this incarnation re-offers the whole unacked tail with fresh msg ids.
/// Not persisted by itself: a crash before the next cursor save simply
/// rewinds again.
pub(crate) fn rewind_unacked(state: &mut TransportState) {
    for cur in state.outbound.values_mut() {
        if cur.ack_seen && cur.log_seq > cur.acked_floor {
            cur.log_seq = cur.acked_floor;
            cur.resend_epoch = cur.resend_epoch.saturating_add(1);
        }
    }
}

/// Whether `env` is something the peer's engine can ever acknowledge: an
/// OWN envelope that is not an `MlsCommit` (commits are supervisor-consumed
/// raw — §4.5 — and foreign envelopes are never fanned out at all).
pub(crate) fn own_ackable(me: &MemberId, env: &EventEnvelope) -> bool {
    env.by == *me && !matches!(env.body, WorkspaceEvent::MlsCommit { .. })
}

/// Delivery guarantee §4.4: compute the new acked floor for one peer from
/// its reported accept window. `envs` is this node's log from `old_floor + 1`
/// on, in seq order. The floor is a **log position**: it advances freely over
/// foreign envelopes and `MlsCommit`s (nothing there is owed to the peer)
/// and stops at the first OWN ackable seq the window does not hold. Keeping
/// the floor in log-position space is load-bearing (E7 review findings 1+2):
/// compared against the whole-log outbox cursor, an "own-events-only" floor
/// read as a permanently unacked tail on every quiet listener — a perpetual
/// rewind loop with a false Degraded, and a compaction gate pinned weeks
/// behind on lurkers.
pub(crate) fn advance_acked_floor(
    me: &MemberId,
    envs: &[EventEnvelope],
    win: &molt_core::AcceptedWindow,
    old_floor: u64,
) -> u64 {
    let mut floor = old_floor;
    for env in envs {
        if own_ackable(me, env) && (env.seq > win.high || !win.is_accepted(env.seq)) {
            break;
        }
        floor = env.seq;
    }
    floor
}

/// Record an advanced acked floor (+ the §4.8 `ack_seen` proof) for `member`.
/// The floor is monotonic: a REGRESSED ack (the peer's own window
/// persistence lost a beat) must not pull the floor back and trigger a
/// resend storm of long-confirmed history. Persists a snapshot ONLY when
/// something changed — an identical re-ack must not cost a
/// `transport.state` rewrite (ack-spam hardening, E7 review).
async fn record_acked<S: StateStore>(
    state: &Arc<Mutex<TransportState>>,
    store: &S,
    member: &MemberId,
    floor: u64,
) {
    let snapshot = {
        let Ok(mut s) = state.lock() else { return };
        let cur = s.outbound.entry(member.clone()).or_default();
        if floor <= cur.acked_floor && cur.ack_seen {
            None
        } else {
            cur.acked_floor = cur.acked_floor.max(floor);
            cur.ack_seen = true;
            Some(s.clone())
        }
    };
    if let Some(snap) = snapshot {
        store.save(snap).await;
    }
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

/// Running count of inbound MLS messages that did not decode (node-wide,
/// diagnostics only — never persisted).
static DISCARDED_INBOUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Running count of outbound envelopes skipped for local reasons
/// (encode/chunk/wrap failures; node-wide, diagnostics only).
static SKIPPED_OUTBOUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one locally-skipped outbound envelope, returning the new total.
fn count_skipped() -> u64 {
    SKIPPED_OUTBOUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// Count one discarded (undecodable) inbound MLS message, returning the new
/// total. Process-wide, like [`count_skipped`] — diagnostics only.
fn count_discarded() -> u64 {
    DISCARDED_INBOUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// A short hex tag of a queue id for log correlation (never the full id).
pub(crate) fn queue_tag(id: &[u8]) -> String {
    hex::encode(&id[..id.len().min(4)])
}

/// Await the next node-wide epoch advance; pends forever on the plaintext
/// path (no MLS channel) or once the watch sender is gone.
async fn epoch_changed(rx: &mut Option<watch::Receiver<u64>>) {
    match rx {
        Some(r) => {
            if r.changed().await.is_err() {
                *rx = None; // sender gone — never wake through this again
            }
        }
        None => std::future::pending().await,
    }
}

/// Retry every held future-epoch message after an epoch advance, in hold
/// order (= the sender-ratchet generation order); keep passing while progress
/// is made (a held commit can unlock further messages). Returns `false` when
/// the engine sink is gone and the recv task must stop.
async fn drain_epoch_buffer<K: EngineSink>(
    ch: &MlsChannel,
    sink: &K,
    peer: &PeerLink,
    epoch_buffer: &mut Vec<([u8; 16], Vec<u8>, Vec<AckToken>)>,
) -> bool {
    let mut progressed = true;
    while progressed && !epoch_buffer.is_empty() {
        progressed = false;
        for (id, bytes, held) in std::mem::take(epoch_buffer) {
            match ch.decode(&bytes) {
                MlsDecode::Deliver(from, env) => {
                    progressed = true;
                    sink.peer_seen(&peer.member).await;
                    if sink.deliver(&from, *env).await.is_err() {
                        return false;
                    }
                    ack_all(held);
                }
                // a BROADCAST sheet on a queue leg: the mesh has per-leg acks
                // and no use for one, and acting on it would apply a broadcast
                // claim through pairwise bookkeeping
                MlsDecode::GroupAck(_, _) => {}
                // a nudge held across an epoch advance is stale by the time it
                // lands: stamp presence, drop the nudge (a late poke is noise)
                MlsDecode::Poke(_, _) => {
                    progressed = true;
                    sink.peer_seen(&peer.member).await;
                    ack_all(held);
                }
                MlsDecode::Ack(_, _) => {
                    // an ack held across an epoch advance: stamp presence and
                    // let it go — its window is stale by now, and the peer's
                    // next debounced ack (dup-triggered resends guarantee one)
                    // supersedes it. Floor advance needs log+state, which this
                    // drain deliberately does not carry.
                    progressed = true;
                    sink.peer_seen(&peer.member).await;
                    ack_all(held);
                }
                MlsDecode::EpochAdvanced(readmitted) => {
                    progressed = true;
                    for m in &readmitted {
                        sink.rekeyed(m).await;
                    }
                    ack_all(held);
                }
                MlsDecode::FutureEpoch => {
                    epoch_buffer.push((id, bytes, held)); // still ahead
                }
                MlsDecode::Discard => {
                    tracing::warn!(peer = %peer.member, total = count_discarded(), "held MLS message did not decode after the epoch advance — dropped");
                    ack_all(held);
                }
            }
        }
    }
    true
}

/// Future-epoch messages held per peer awaiting their re-key commit. The
/// claimed epoch is unauthenticated before decryption, so a forgery can claim
/// a future epoch — the buffer is small and shed-tolerant: a shed entry's acks
/// are dropped unfired, so the transport redelivers it later (by which time
/// the commit has usually landed).
const EPOCH_BUFFER_MAX: usize = 64;

/// Why a [`recv_task`] incarnation ended — the watchdog's branch signal.
enum RecvEnd {
    /// The engine sink refused a delivery (actor gone): stop for good.
    EngineGone,
    /// The delivery stream ended (the transport's recv loop died): resubscribe.
    StreamEnded,
}

/// The per-peer inbound **watchdog** (Stage B): subscribe the peer's inbound
/// queue, run [`recv_task`] over its deliveries, and when the stream ends — a
/// died server-side recv loop used to leave the seat deaf for the whole
/// session — redial `subscribe` with capped jittered backoff and run a fresh
/// incarnation. The inbound twin of the outbox's retry-with-backoff. Health
/// is honest: `link_up` when the subscription is live, `link_down` on a
/// stream end or a failed redial. A fresh incarnation loses only in-memory
/// buffers whose acks never fired — the transport redelivers them; the
/// delivery cursor rides the shared `state`. Ends for good only when the
/// engine sink is gone (nobody is listening, so no alarm either).
#[allow(clippy::too_many_arguments)]
async fn recv_watchdog_task<T, L, S, K>(
    transport: T,
    cfg: NetConfig,
    peer: PeerLink,
    log: L,
    store: S,
    sink: K,
    state: Arc<Mutex<TransportState>>,
    mls: Option<MlsChannel>,
    first: Option<tokio::sync::mpsc::Receiver<crate::Delivery>>,
    seed: u64,
) where
    T: Transport,
    L: OutboxLog,
    S: StateStore,
    K: EngineSink,
{
    let mut rng = seed;
    let mut attempt: u32 = 0;
    let mut next = first;
    loop {
        let rx = match next.take() {
            Some(rx) => rx,
            None => match transport.subscribe(peer.rcv0()).await {
                Ok(rx) => rx,
                Err(e) => {
                    tracing::warn!(member = %peer.member, queue = %queue_tag(&peer.rcv0().id.0), error = %e, "inbound subscribe failed — backing off");
                    sink.link_down(&peer.member, &e.to_string()).await;
                    let backoff = backoff_ms(&cfg, attempt, &mut rng);
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            },
        };
        tracing::debug!(member = %peer.member, queue = %queue_tag(&peer.rcv0().id.0), "inbound subscription live");
        sink.link_up(&peer.member).await;
        let lived = tokio::time::Instant::now();
        match recv_task(
            peer.clone(),
            rx,
            cfg.member.clone(),
            log.clone(),
            store.clone(),
            sink.clone(),
            state.clone(),
            mls.clone(),
        )
        .await
        {
            RecvEnd::EngineGone => return,
            RecvEnd::StreamEnded => {
                tracing::warn!(member = %peer.member, queue = %queue_tag(&peer.rcv0().id.0), "inbound subscription ended — resubscribing");
                sink.link_down(&peer.member, "inbound subscription ended — resubscribing")
                    .await;
            }
        }
        // Reset the escalation only after a LONG-LIVED incarnation: a queue
        // whose subscription is accepted but ended immediately (e.g. a server END
        // war on a contended queue) must keep escalating toward retry_cap_ms, or
        // the loop redials at base rate forever and flaps link_up/link_down.
        if lived.elapsed() >= Duration::from_millis(cfg.retry_cap_ms) {
            attempt = 0;
        }
        let backoff = backoff_ms(&cfg, attempt, &mut rng);
        tokio::time::sleep(Duration::from_millis(backoff)).await;
        attempt = attempt.saturating_add(1);
    }
}

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
async fn recv_task<L, S, K>(
    peer: PeerLink,
    mut rx: tokio::sync::mpsc::Receiver<crate::Delivery>,
    me: MemberId,
    log: L,
    store: S,
    sink: K,
    state: Arc<Mutex<TransportState>>,
    mls: Option<MlsChannel>,
) -> RecvEnd
where
    L: OutboxLog,
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
    // MLS path: complete messages encrypted at an epoch ahead of ours, held
    // (acks unfired) until a commit merges — the cross-epoch retry
    let mut epoch_buffer: Vec<([u8; 16], Vec<u8>, Vec<AckToken>)> = Vec::new();
    // ack-spam hardening (E7 review): full ack processing — a log read on
    // the writer thread + a possible state save — at most twice a second
    // per peer; surplus frames still stamp presence and are acked away
    let mut last_ack_processed: Option<tokio::time::Instant> = None;
    // this task is the sole writer of inbound[peer]; the shared state is
    // only the persistence snapshot
    let mut cursor = state
        .lock()
        .ok()
        .and_then(|s| s.inbound.get(&peer.member).copied())
        .unwrap_or(0);
    // node-wide epoch advances: a commit merging on ANOTHER peer's link makes
    // messages held HERE decryptable — without this wake they would sit until
    // this link happened to carry a commit itself (it may never)
    let mut epoch_rx = mls.as_ref().map(MlsChannel::epoch_watch);
    loop {
        let delivery = tokio::select! {
            biased;
            _ = epoch_changed(&mut epoch_rx), if !epoch_buffer.is_empty() => {
                if let Some(ch) = &mls {
                    if !drain_epoch_buffer(ch, &sink, &peer, &mut epoch_buffer).await {
                        return RecvEnd::EngineGone;
                    }
                }
                continue;
            }
            d = rx.recv() => match d {
                Some(d) => d,
                // the delivery stream ended — the transport's recv loop died
                None => return RecvEnd::StreamEnded,
            },
        };
        let plain = match unwrap_block(&peer.wrap_in, &delivery.block) {
            Ok(p) => p,
            Err(e) => {
                // undecryptable: redelivery cannot help — ack it away
                tracing::warn!(peer = %peer.member, error = %e, "dropping an undecryptable block");
                delivery.ack.ack();
                continue;
            }
        };
        tracing::debug!(peer = %peer.member, plain = plain.len(), "MESHRX unwrapped a block → reassembler");
        let (id, complete) = match reasm.push(&plain) {
            Ok(PushOutcome::Duplicate(id)) => {
                tracing::debug!(peer = %peer.member, "MESHRX reassembler=DUPLICATE (redelivery/already-seen) — no decode");
                if let Some(held) = chunk_acks.get_mut(&id.0) {
                    held.push(delivery.ack); // message still partial
                } else if let Some(held) = buffered_ids
                    .get(&id.0)
                    .and_then(|seq| reorder.get_mut(seq))
                {
                    held.1.push(delivery.ack); // message buffered behind a gap
                } else if let Some(held) = epoch_buffer.iter_mut().find(|e| e.0 == id.0) {
                    held.2.push(delivery.ack); // message held for its epoch
                } else {
                    delivery.ack.ack(); // message was accepted — safe
                }
                continue;
            }
            Ok(PushOutcome::Buffered(id)) => {
                tracing::debug!(peer = %peer.member, "MESHRX reassembler=BUFFERED (incomplete message, holding for more chunks) — no decode yet");
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
            Ok(PushOutcome::Complete(id, bytes)) => {
                tracing::debug!(peer = %peer.member, len = bytes.len(), "MESHRX reassembler=COMPLETE → MLS decode");
                (id, bytes)
            }
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
        // job, so the per-link reorder buffer below does not apply — EXCEPT
        // across an epoch boundary: a message encrypted at an epoch we have not
        // reached (its re-key commit still in flight) is held, acks unfired,
        // and retried after each merged commit (the cross-epoch retry;
        // `docs_archive/ritual/recovery_ritual.md` §8). A held message a crash loses is
        // redelivered by the transport (its acks never fired).
        if let Some(ch) = &mls {
            match ch.decode(&complete) {
                MlsDecode::Deliver(from, env) => {
                    tracing::debug!(peer = %peer.member, from = %from, "MESHRX decode=DELIVER (an application message)");
                    sink.peer_seen(&peer.member).await;
                    if sink.deliver(&from, *env).await.is_err() {
                        tracing::debug!(peer = %peer.member, "engine gone — recv task stops");
                        return RecvEnd::EngineGone;
                    }
                    ack_all(acks);
                }
                MlsDecode::GroupAck(..) => {
                    ack_all(acks);
                }
                // a poke on a queue leg: only the LINK's member may speak for
                // itself, exactly like an ack — then hand it to the engine,
                // which owns the opt-in, the target check and the cooldown
                MlsDecode::Poke(from, poke) => {
                    tracing::debug!(peer = %peer.member, %from, to = %poke.to, "MESHRX decode=POKE");
                    sink.peer_seen(&peer.member).await;
                    if from == peer.member && poke.by == from {
                        sink.poked(&from, &poke.to).await;
                    } else {
                        tracing::warn!(peer = %peer.member, claimed = %poke.by, "a poke disowns its link - dropped");
                    }
                    ack_all(acks);
                }
                MlsDecode::Ack(from, win) => {
                    // only the LINK's member may move this link's floor — an
                    // authenticated ack from anyone else is misrouted traffic
                    if from != peer.member {
                        tracing::warn!(peer = %peer.member, claimed = %from, "an ack's author does not match its link — dropped");
                        ack_all(acks);
                        continue;
                    }
                    tracing::debug!(peer = %peer.member, high = win.high, "MESHRX decode=ACK");
                    // authenticated live traffic stamps presence (an acking
                    // peer is a breathing peer)
                    sink.peer_seen(&peer.member).await;
                    if last_ack_processed
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
                    {
                        ack_all(acks);
                        continue;
                    }
                    last_ack_processed = Some(tokio::time::Instant::now());
                    let old_floor = state
                        .lock()
                        .ok()
                        .and_then(|s| s.outbound.get(&peer.member).map(|c| c.acked_floor))
                        .unwrap_or(0);
                    // diff the window against OUR OWN log above the floor —
                    // the tail is short in steady state (the floor tracks the
                    // head), and the first-ever ack's full read happens once
                    let envs = log.read_from(old_floor + 1).await;
                    let floor = advance_acked_floor(&me, &envs, &win, old_floor);
                    record_acked(&state, &store, &peer.member, floor).await;
                    // memory bound (§4.5): ciphertexts every acking peer has
                    // confirmed can never be needed again
                    let min_floor = state.lock().ok().and_then(|s| {
                        s.outbound
                            .values()
                            .filter(|c| c.ack_seen)
                            .map(|c| c.acked_floor)
                            .min()
                    });
                    if let Some(f) = min_floor {
                        ch.evict_at_or_below(f);
                    }
                    ack_all(acks);
                }
                MlsDecode::EpochAdvanced(readmitted) => {
                    tracing::debug!(peer = %peer.member, "MESHRX decode=EPOCH_ADVANCED (a re-key commit merged)");
                    for m in &readmitted {
                        sink.rekeyed(m).await;
                    }
                    ack_all(acks);
                    if !drain_epoch_buffer(ch, &sink, &peer, &mut epoch_buffer).await {
                        return RecvEnd::EngineGone;
                    }
                }
                MlsDecode::FutureEpoch => {
                    if epoch_buffer.len() >= EPOCH_BUFFER_MAX {
                        // bounded: shed the NEWEST (this one), acks unfired —
                        // the transport redelivers it once the commit has
                        // landed. Newest, not oldest: the buffer must stay in
                        // arrival order, which is the sender-ratchet
                        // generation order — a rotated-out OLD message would
                        // redeliver after the drain advanced the ratchet far
                        // past its generation and fail the sender's
                        // out-of-order window, while the shed newest simply
                        // decrypts as the next generation. The reassembler
                        // must FORGET the shed id, or the redelivered copy
                        // would classify as a duplicate of an "accepted"
                        // message and be acked away — erasing the only
                        // durable copy (same discipline as the reorder
                        // buffer's shed path below).
                        reasm.forget(id);
                        drop(acks);
                        tracing::warn!(peer = %peer.member, "epoch buffer full — shedding onto redelivery");
                    } else {
                        tracing::debug!(peer = %peer.member, "holding a future-epoch message for its commit");
                        epoch_buffer.push((id.0, complete, acks));
                    }
                }
                MlsDecode::Discard => {
                    // loud, with a running count: a silently-dropped inbound
                    // message is how a dead leg hides (Stage B §3.3). Replays
                    // of redelivered blocks land here too — routine weather,
                    // but a STREAM of these on a quiet mesh is the signature
                    // of a desynced ratchet.
                    tracing::warn!(peer = %peer.member, total = count_discarded(), "inbound MLS message did not decode (replay/proposal/garbage) — dropped");
                    ack_all(acks);
                }
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
                return RecvEnd::EngineGone;
            }
            cursor += 1;
            let snapshot = {
                let Ok(mut s) = state.lock() else { return RecvEnd::EngineGone };
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
            snds: vec![SndQueueAddr {
                server: "smp://fp@host".to_string(),
                id: crate::QueueId::from_bytes(vec![1, 2, 3, 4]),
            }],
            wrap_out: WrapKey::from_bytes([7u8; 32]),
            rcvs: vec![crate::RcvQueue {
                // Stage 0: our inbound may live on a DIFFERENT server than the
                // peer's inbound — the round-trip must preserve rcv_server.
                server: "smp://rcvfp@host2".to_string(),
                id: crate::QueueId::from_bytes(vec![9, 8, 7]),
            }],
            wrap_in: WrapKey::from_bytes([3u8; 32]),
        };
        let mesh = link.to_mesh();
        assert_eq!(mesh.member, "bob");
        assert_eq!(mesh.snd_server, "smp://fp@host");
        assert_eq!(mesh.rcv_server, "smp://rcvfp@host2");
        assert!(mesh.snd_extra.is_empty(), "N=1 leg persists no extra queues");
        assert!(mesh.rcv_extra.is_empty());
        let back = PeerLink::from_mesh(&mesh).expect("round trips");
        assert_eq!(back.member, link.member);
        assert_eq!(back.snds[0].server, link.snds[0].server);
        assert_eq!(back.snds[0].id.0, link.snds[0].id.0);
        assert_eq!(back.wrap_out.to_bytes(), link.wrap_out.to_bytes());
        assert_eq!(back.rcvs[0].server, link.rcvs[0].server, "rcv_server survives the round-trip");
        assert_eq!(back.rcvs[0].id.0, link.rcvs[0].id.0);
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
            rcv_server: String::new(),
            snd_extra: Vec::new(),
            rcv_extra: Vec::new(),
        };
        assert!(PeerLink::from_mesh(&bad).is_none());
    }

    /// §4.4: the build-time rewind re-offers the unacked tail ONLY toward
    /// peers that ever acked (old nodes keep the plain cursor), and bumps
    /// the resend epoch so the re-offer carries fresh msg ids.
    #[test]
    fn the_build_rewind_touches_only_proven_acking_peers_with_a_tail() {
        let mut ts = TransportState::default();
        ts.outbound.insert(
            "acker".to_string(),
            molt_core::OutboundCursor {
                log_seq: 9,
                wire_seq: 4,
                acked_floor: 6,
                ack_seen: true,
                resend_epoch: 1,
            },
        );
        ts.outbound.insert(
            "old-node".to_string(),
            molt_core::OutboundCursor { log_seq: 9, wire_seq: 4, ..Default::default() },
        );
        ts.outbound.insert(
            "caught-up".to_string(),
            molt_core::OutboundCursor {
                log_seq: 6,
                wire_seq: 3,
                acked_floor: 6,
                ack_seen: true,
                resend_epoch: 2,
            },
        );
        rewind_unacked(&mut ts);
        let acker = ts.outbound["acker"];
        assert_eq!((acker.log_seq, acker.resend_epoch), (6, 2), "tail re-offered, epoch bumped");
        let old = ts.outbound["old-node"];
        assert_eq!((old.log_seq, old.resend_epoch), (9, 0), "an old node keeps plain cursors");
        let done = ts.outbound["caught-up"];
        assert_eq!((done.log_seq, done.resend_epoch), (6, 2), "no tail — nothing to re-offer");
    }

    /// Delivery guarantee §4.3: an ACK frame decodes to `Ack` carrying the
    /// MLS-authenticated sender and its window; a malformed payload after the
    /// tag drops instead of mis-parsing.
    #[test]
    fn an_ack_frame_classifies_with_its_authenticated_sender() {
        use ed25519_dalek::SigningKey;
        let sk = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut founder = MlsMember::new(&sk(1), "founder").expect("founder");
        let bob = MlsMember::new(&sk(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");
        let recv = MlsChannel::new(bob);

        let mut win = molt_core::AcceptedWindow::default();
        assert!(win.accept(4));
        assert!(win.accept(7));
        let mut frame = crate::MESH_ACK_TAG.to_vec();
        frame.extend_from_slice(&serde_json::to_vec(&win).expect("json"));
        let ct = founder.encrypt(&frame).expect("encrypt ack");
        match recv.decode(&ct) {
            MlsDecode::Ack(from, got) => {
                assert_eq!(from, "founder", "authenticated to the MLS credential");
                assert_eq!(got.high, 7);
                assert!(got.is_accepted(4) && !got.is_accepted(5));
            }
            other => panic!("an ack frame must classify as Ack, got {other:?}"),
        }

        // a mangled payload after the tag drops — never a mis-parse
        let mut bad = crate::MESH_ACK_TAG.to_vec();
        bad.extend_from_slice(b"{not json");
        let ct = founder.encrypt(&bad).expect("encrypt bad ack");
        assert!(
            matches!(recv.decode(&ct), MlsDecode::Discard),
            "a malformed ack payload is dropped"
        );
    }

    /// §4.4: the acked floor advances over OWN accepted seqs, skips foreign
    /// envelopes and `MlsCommit` seqs (ack-exempt), stops at the first own
    /// unaccepted seq, and never crosses the reported high.
    #[test]
    fn the_acked_floor_walks_own_events_and_stops_at_the_first_gap() {
        let win = {
            let mut w = molt_core::AcceptedWindow::default();
            for s in [2u64, 3, 8] {
                assert!(w.accept(s));
            }
            w
        };
        let env = |seq: u64, by: &str, body: WorkspaceEvent| EventEnvelope { prev_seq: 0,
            seq,
            ts: seq,
            by: by.to_string(),
            body,
        };
        let chat = |seq: u64, by: &str| {
            env(
                seq,
                by,
                WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    molt_core::MessageId([u8::try_from(seq).unwrap_or(9); 16]),
                    by,
                    "x",
                    seq,
                )),
            )
        };
        // seq 2,3 = own accepted; 4 = foreign (never gates); 5 = own COMMIT
        // (exempt, skipped); 8 = own accepted → floor lands on 8
        let envs = vec![
            chat(2, "me"),
            chat(3, "me"),
            chat(4, "peer"),
            env(5, "me", WorkspaceEvent::MlsCommit { commit: "aa".to_string() }),
            chat(8, "me"),
        ];
        assert_eq!(advance_acked_floor(&"me".to_string(), &envs, &win, 0), 8);

        // an own UNACCEPTED seq stops the walk — later accepted ones wait
        let envs = vec![chat(2, "me"), chat(6, "me"), chat(8, "me")];
        assert_eq!(
            advance_acked_floor(&"me".to_string(), &envs, &win, 0),
            2,
            "seq 6 is not accepted — the floor must not skip over it"
        );

        // nothing OWN above the reported high ever counts
        let envs = vec![chat(8, "me"), chat(9, "me")];
        assert_eq!(advance_acked_floor(&"me".to_string(), &envs, &win, 5), 8);

        // E7 findings 1+2: the floor is a LOG position — a purely foreign /
        // commit remainder advances it freely (even past the peer's high),
        // so a quiet listener's floor tracks the head and neither the stall
        // clock nor the compaction gate ever trips on nothing
        let envs = vec![
            chat(2, "me"),
            chat(4, "peer"),
            chat(9, "peer"),
            env(11, "me", WorkspaceEvent::MlsCommit { commit: "bb".to_string() }),
        ];
        assert_eq!(
            advance_acked_floor(&"me".to_string(), &envs, &win, 0),
            11,
            "foreign + commit tails advance the floor to the head"
        );
        // …but an own unacked seq still stops the walk mid-span
        let envs = vec![chat(4, "peer"), chat(6, "me"), chat(9, "peer")];
        assert_eq!(
            advance_acked_floor(&"me".to_string(), &envs, &win, 0),
            4,
            "the own unaccepted seq 6 pins the floor below the foreign tail"
        );
    }

    /// The `\x00molt-mesh-*` space is reserved for control frames (the ACK
    /// rides in it): an UNKNOWN control frame is dropped as a no-op — a newer
    /// control tag this build predates must never be mis-parsed as an event,
    /// while a real envelope still decodes to `Deliver` authenticated to its
    /// sender.
    #[test]
    fn an_unknown_control_tag_drops_and_an_envelope_still_delivers() {
        use ed25519_dalek::SigningKey;
        let sk = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let mut founder = MlsMember::new(&sk(1), "founder").expect("founder");
        let bob = MlsMember::new(&sk(2), "bob").expect("bob");
        founder.create_group().expect("create group");
        let welcome = founder
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        let mut bob = bob;
        bob.join_from_welcome(&welcome).expect("bob joins");
        let recv = MlsChannel::new(bob);

        // an unknown NUL-prefixed control frame: dropped, never mis-parsed
        let unknown = founder.encrypt(b"\x00molt-mesh-future-v9").expect("encrypt unknown");
        assert!(
            matches!(recv.decode(&unknown), MlsDecode::Discard),
            "an unknown reserved control tag is a dropped no-op"
        );

        // a real envelope still delivers, authenticated to its sender
        let env = EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 1,
            by: "founder".to_string(),
            body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([1u8; 16]),
                "founder",
                "hi",
                1,
            )),
        };
        let ct = founder
            .encrypt(&serde_json::to_vec(&env).expect("json"))
            .expect("encrypt envelope");
        assert!(
            matches!(recv.decode(&ct), MlsDecode::Deliver(from, _) if from == "founder"),
            "a real envelope still classifies as Deliver"
        );
    }

    /// P4: the prebuild hook dials N servers bounded by a semaphore of 4 — never
    /// more than four circuits build at once, every server is dialed, and with
    /// N > 4 the bound is actually saturated (so it is concurrent, not serial).
    #[tokio::test]
    async fn prebuild_opens_connections_under_a_semaphore_of_4() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let n = 12usize;
        let results = {
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            prebuild_circuits(n, PREBUILD_PARALLELISM, move |i| {
                let in_flight = in_flight.clone();
                let max_seen = max_seen.clone();
                async move {
                    let cur = in_flight.fetch_add(1, SeqCst) + 1;
                    max_seen.fetch_max(cur, SeqCst);
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    in_flight.fetch_sub(1, SeqCst);
                    i
                }
            })
            .await
        };
        // every server dialed, results in input order
        assert_eq!(results.len(), n);
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, Some(i), "result {i} present and in order");
        }
        let peak = max_seen.load(SeqCst);
        assert!(
            peak <= PREBUILD_PARALLELISM,
            "peak {peak} in flight exceeded the semaphore bound"
        );
        assert_eq!(
            peak, PREBUILD_PARALLELISM,
            "prebuild should saturate the semaphore of 4 with n > 4"
        );
    }
}
