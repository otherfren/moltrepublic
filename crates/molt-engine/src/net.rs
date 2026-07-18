// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine ↔ `molt-net` glue (transport concept §2), plus the demo
//! mesh that replaced the reply simulator.
//!
//! Wiring rules, all load-bearing:
//!
//! * The engine **never awaits the transport**. [`crate::State::record`]
//!   publishes the envelope to the net feed and bumps a coalescing watch;
//!   the supervisor's outbox reads the log, the wakeup carries no data.
//! * Inbound runs through the engine-internal `Net*` commands — the same
//!   pattern as the run tickers. They are on the documented INTERNAL list
//!   of the co-equality test: a network peer must not be impersonatable
//!   through the MCP surface.
//! * Presence is passive: `NetPeerSeen` derives from authenticated inbound
//!   traffic that happens anyway; `NetSendFailed` marks transport trouble.
//!   No beacons, ever.
//!
//! **The demo mesh — test seam only** ([`crate::State::demo_mesh`],
//! default OFF; only `__spawn_demo_mesh` sets it). On a session-only
//! context the roster's other members run as real loopback peers: each
//! has its own engine instance and transport endpoint, plus a small
//! "brain" that answers the local member's chat with a canned line —
//! through its own engine and outbox, so a demo reply exercises the same
//! code path a real member's message will. **Production spawns no fake
//! peers, ever**: without the seam a session-only context runs no
//! transport at all (chat is an honest local-only scratch log), and a
//! persisted workspace's `prefs.simulated_members` flag is inert — a
//! fake member recorded in a real log would replay forever.

use molt_core::{
    fnv1a64, mockrand, Command, EventEnvelope, GroupConfig, MemberId, MessageId, MoltError,
    Reply, SessionScope, SessionView, WorkspaceEvent, WorkspaceId,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use molt_net::supervisor::{self, EngineSink, MemLog, MemStateStore, MlsChannel, NetConfig, PeerLink};
use molt_net::{LoopbackHub, NetError, SupervisorHandle, Transport};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{Envelope, State};

/// Minimum seconds between accepted mesh (re-)announces per member — each
/// costs every peer a supervisor teardown+rebuild+fsync (see
/// `State::spawn_mesh_extension`).
const MESH_EXTENSION_COOLDOWN_SECS: u64 = 60;

/// Demo fan-out jitter (ms): enough to be honest about asynchrony, small
/// enough to feel live. Real deployments keep the concept's 2 s default.
const DEMO_JITTER_MS: u64 = 300;
/// Demo brain: answer roughly one in this many owner messages.
const BRAIN_REPLY_ONE_IN: u64 = 3;
/// Demo brain reply delay: base + up to span (ms) — the old simulator's
/// 1.5–6.5 s feel.
const BRAIN_DELAY_BASE_MS: u64 = 1_500;
const BRAIN_DELAY_SPAN_MS: u64 = 5_000;

/// The canned demo lines (moved here from the retired reply simulator).
const LINES: [&str; 16] = [
    "sounds good to me",
    "can someone double-check the numbers?",
    "+1",
    "i'll take that quest tomorrow",
    "did anyone hear back from the notary?",
    "lol",
    "agreed, let's move on",
    "wait — which invite was that?",
    "backing this",
    "brb, checking the vault",
    "nice, ship it",
    "hmm, not sure about that",
    "we should propose it properly",
    "who's online later tonight?",
    "good morning everyone",
    "that fence isn't going to fix itself 🙂",
];

// ---------------------------------------------------------------------------
// P6: the parking buffer for out-of-order wire references
// ---------------------------------------------------------------------------

/// Cap on distinct target message ids the parking buffer holds at once;
/// when full, the OLDEST parked target (insertion order) is evicted whole.
const PARKED_TARGET_CAP: usize = 256;
/// Cap on refs parked under ONE target id (a flood of reactions to a single
/// unknown id must not grow without bound); the oldest ref is shed first.
const PARKED_REFS_PER_TARGET: usize = 64;

/// One wire reference (reaction / delete / file-removal) whose target
/// message has not arrived yet. `by` is ALWAYS the authenticated link
/// identity it arrived on (forced at park time, exactly like a live wire
/// event), so the P5 enforcement matrix re-evaluates at drain time against
/// trusted data only — never against a claim inside the parked event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingRef {
    /// A reaction; the emoji passed the wire sanity check at park time.
    React {
        /// The reacting member (the link identity).
        by: MemberId,
        /// The sanitized emoji.
        emoji: String,
        /// The sender's explicit direction (`None` = legacy toggle).
        op: Option<molt_core::ReactOp>,
    },
    /// A message deletion — honored at drain only if `by` turns out to be
    /// the target's author (no moderation concept).
    Delete {
        /// The deleting member (the link identity).
        by: MemberId,
    },
    /// A file-share removal — honored at drain only if `by` turns out to be
    /// the sharer.
    FileRemove {
        /// The removing member (the link identity).
        by: MemberId,
    },
}

/// The P6 parking buffer: cross-sender ordering is not guaranteed (per-sender
/// in-order only, and the MLS path bypasses the wire reorder buffer), so a
/// reaction/delete/file-removal can arrive BEFORE the message it targets.
/// Such refs are parked here, keyed by the unknown target id, and drained
/// when the `Chat` lands. Bounded (FIFO eviction of the oldest target) and
/// strictly runtime-only: never persisted — a restart loses parked refs,
/// which is fine, the chat bus is ephemeral by design.
pub(crate) struct ParkedRefs {
    /// Parked refs per unknown target id, in arrival order.
    refs: BTreeMap<MessageId, Vec<PendingRef>>,
    /// Target ids in insertion order — the FIFO eviction ledger.
    order: VecDeque<MessageId>,
}

impl ParkedRefs {
    /// An empty buffer.
    pub(crate) fn new() -> Self {
        ParkedRefs {
            refs: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Park one ref under its (unknown) target id. A new target beyond the
    /// cap evicts the OLDEST parked target wholesale; within one target the
    /// oldest ref is shed once the per-target cap is hit.
    pub(crate) fn park(&mut self, target: MessageId, r: PendingRef) {
        if let Some(list) = self.refs.get_mut(&target) {
            if list.len() >= PARKED_REFS_PER_TARGET {
                list.remove(0);
            }
            list.push(r);
            return;
        }
        if self.order.len() >= PARKED_TARGET_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.refs.remove(&oldest);
                tracing::warn!(target = %oldest, "parking buffer full — evicting the oldest parked target");
            }
        }
        self.refs.insert(target, vec![r]);
        self.order.push_back(target);
    }

    /// Remove and return everything parked for `target` (its message just
    /// arrived), freeing the target's slot in the eviction ledger.
    pub(crate) fn drain(&mut self, target: &MessageId) -> Vec<PendingRef> {
        let parked = self.refs.remove(target).unwrap_or_default();
        if !parked.is_empty() {
            self.order.retain(|t| t != target);
        }
        parked
    }

    /// Drop everything (workspace close/switch).
    pub(crate) fn clear(&mut self) {
        self.refs.clear();
        self.order.clear();
    }

    /// Number of distinct parked targets (tests).
    #[cfg(test)]
    fn targets(&self) -> usize {
        debug_assert_eq!(self.refs.len(), self.order.len());
        self.refs.len()
    }

    /// Whether a target has parked refs (tests).
    #[cfg(test)]
    fn holds(&self, target: &MessageId) -> bool {
        self.refs.contains_key(target)
    }
}

// ---------------------------------------------------------------------------
// EngineSink / OutboxLog / StateStore implementations
// ---------------------------------------------------------------------------

/// The supervisor's way into an engine: every inbound event and health
/// signal becomes an internal command on the actor queue.
///
/// It holds only a **weak** sender: supervisor tasks live inside the very
/// engine they feed (State owns the `SupervisorHandle`), so a strong
/// sender would be a reference cycle — the actor could never stop, and a
/// torn-down demo peer would leak forever. `generation` tags which mesh
/// incarnation is speaking (`None` = engine-lifetime transport).
#[derive(Clone)]
pub struct CmdSink {
    tx: mpsc::WeakSender<Envelope>,
    generation: Option<u64>,
}

impl crate::WalletHandle {
    /// A transport sink into this engine: the supervisor of a real (T2+)
    /// or test mesh drives the engine-internal `Net*` commands through it.
    /// It cannot issue anything else — peers stay peers, not operators.
    pub fn net_sink(&self) -> CmdSink {
        CmdSink {
            tx: self.cmd_tx.downgrade(),
            generation: None,
        }
    }
}

impl CmdSink {
    async fn execute(&self, cmd: Command) -> Result<(), NetError> {
        let Some(tx) = self.tx.upgrade() else {
            return Err(NetError::Closed); // engine stopped — so do we
        };
        let (reply, rx) = oneshot::channel();
        tx.send(Envelope { cmd, reply })
            .await
            .map_err(|_| NetError::Closed)?;
        // the engine's answer content does not matter here: validation
        // failures are ack-and-skip (a poison event must not wedge the
        // queue); only a vanished engine stops the supervisor
        rx.await.map(|_| ()).map_err(|_| NetError::Closed)
    }
}

impl EngineSink for CmdSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.execute(Command::NetDelivered {
            from: from.clone(),
            envelope: env,
            generation: self.generation,
        })
        .await
    }

    async fn peer_seen(&self, member: &MemberId) {
        let _ = self
            .execute(Command::NetPeerSeen {
                member: member.clone(),
                generation: self.generation,
            })
            .await;
    }

    async fn send_failed(&self, member: &MemberId, reason: &str) {
        let _ = self
            .execute(Command::NetSendFailed {
                member: member.clone(),
                reason: reason.to_string(),
                generation: self.generation,
            })
            .await;
    }
}

/// The log-backed outbox source of a persisted workspace: reads pending
/// envelopes through the storage writer (same channel as appends, so a
/// read enqueued after an append always sees it). Consumed by the T2 join
/// flow; exercised today by the persisted-mesh tests.
#[derive(Clone)]
pub struct StorageLog {
    handle: molt_storage::StorageHandle,
}

impl StorageLog {
    /// Wrap a workspace's writer handle.
    pub fn new(handle: molt_storage::StorageHandle) -> StorageLog {
        StorageLog { handle }
    }
}

impl supervisor::OutboxLog for StorageLog {
    async fn read_from(&self, from_seq: u64) -> Vec<EventEnvelope> {
        self.handle.read_log_from(from_seq).await
    }
}

/// Delivery cursors in the workspace's encrypted `transport.state`.
#[derive(Clone)]
pub struct FileStateStore {
    handle: molt_storage::StorageHandle,
}

impl FileStateStore {
    /// Wrap a workspace's writer handle.
    pub fn new(handle: molt_storage::StorageHandle) -> FileStateStore {
        FileStateStore { handle }
    }
}

impl supervisor::StateStore for FileStateStore {
    async fn load(&self) -> molt_core::TransportState {
        self.handle.load_transport_state().await
    }

    async fn save(&self, state: molt_core::TransportState) {
        self.handle.save_transport_state(state);
    }
}

// ---------------------------------------------------------------------------
// The engine-side net runtime
// ---------------------------------------------------------------------------

/// The wire scope, in ONE place: which workspace events cross the transport.
/// Chat and its id-addressed verbs (reactions, deletions, file removals —
/// chat bus B1: they carry a stable [`molt_core::MessageId`], so they are
/// global refs, not sender-local indices) and the chain-governance traffic —
/// `Proposed`/`Approved` gossip and a broadcast `Committed` block. Everything
/// else (presence, membership frames) stays node-local. Consulted by the
/// outbox feed gate; the receiving side's match in `cmd_net_delivered` is the
/// defense-in-depth twin (a persisted log's outbox has no feed gate), and it
/// drops the governance variants for a non-chain workspace.
pub(crate) fn crosses_wire(event: &WorkspaceEvent) -> bool {
    matches!(
        event,
        WorkspaceEvent::Chat(_)
            | WorkspaceEvent::ChatReacted { .. }
            | WorkspaceEvent::ChatDeleted { .. }
            | WorkspaceEvent::FileRemoved { .. }
            | WorkspaceEvent::Proposed { .. }
            | WorkspaceEvent::Approved { .. }
            | WorkspaceEvent::Committed(_)
            | WorkspaceEvent::ChainRequest { .. }
            | WorkspaceEvent::MembershipProposed { .. }
            | WorkspaceEvent::CheckpointProposed { .. }
            | WorkspaceEvent::CheckpointServed { .. }
            | WorkspaceEvent::MlsCommit { .. }
            | WorkspaceEvent::MeshAnnounced { .. }
            | WorkspaceEvent::FileRequested { .. }
    )
}

/// Where a running mesh's outbox reads from. The **demo** mesh has no storage,
/// so the local member's own events are mirrored into an in-memory [`MemLog`]
/// its peers read; a **real** (T2) workspace's outbox IS the encrypted workspace
/// log ([`StorageLog`], wired at build), so publishing only has to wake the
/// supervisor — after the storage append has been enqueued (see [`State::record`]).
enum NetFeed {
    Demo(MemLog),
    Real,
}

/// A real mesh's persistable crypto: the runtime transport (whose `Arc` owns the
/// queue credentials) + the shared MLS group (whose ratchet the supervisor
/// advances). Snapshotted into `transport.state` on a clean close.
type RealCrypto = (crate::founding::RitualTransport, Arc<Mutex<molt_net::MlsMember>>);

/// The MLS re-key a coordinator produces on recovery: `(commit, welcome)` or a
/// failure reason (`None` from the caller means there was no runtime group).
type MlsRekey = Result<(Vec<u8>, Vec<u8>), String>;

/// What a clean close persists: `(MLS snapshot, transport queue-credential bytes)`.
type CloseCrypto = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The engine's transport runtime: the outbox feed + wakeup on the engine
/// side, the supervisor and (for the demo mesh) the peer nodes — each
/// kept alive by holding its engine's command sender (the peer actor
/// stops once every sender is gone; its supervisor holds only a weak one).
pub(crate) struct NetRuntime {
    feed: NetFeed,
    wakeup: watch::Sender<u64>,
    _supervisor: SupervisorHandle,
    _peer_keepalives: Vec<mpsc::Sender<Envelope>>,
    /// The mesh this runtime was built for; a different context rebuilds.
    /// On peer nodes this is (own name, "") — which matches the peer's own
    /// never-changing session context, so a peer engine's `ensure_demo_net`
    /// can never tear down its externally wired mesh. Deliberately NOT
    /// keyed on member presence states: a presence flap must not rebuild
    /// the mesh (rebuild-on-NetSendFailed would be a feedback loop).
    context: (MemberId, WorkspaceId),
    /// Whose events the engine accepts over this mesh (snapshot at build;
    /// T1 rosters are static within a workspace context).
    peer_names: Vec<MemberId>,
    /// This mesh incarnation; stale `Net*` commands are dropped by it.
    generation: u64,
    /// A **real** mesh's persistable crypto: a clone of the runtime transport
    /// (whose `Arc` owns the queues' receive/sender credentials) and the shared
    /// MLS group (whose ratchet the supervisor advances). On a clean close the
    /// engine snapshots both into `transport.state` so a reopen resumes the mesh.
    /// `None` for the demo mesh (nothing to persist / resume).
    real_crypto: Option<RealCrypto>,
    /// The mesh links this runtime was built over — what a dynamic mesh
    /// extension grows (rebuild with `mesh + new link`) and what the grown
    /// persist writes. Empty for the demo mesh.
    mesh: Vec<molt_core::MeshLink>,
}

impl NetRuntime {
    /// Whether this envelope should cross the wire on this mesh: only
    /// self-authored events inside the wire scope — relayed peer events must not
    /// echo, and node-local kinds must not burn blocks only to be dropped at the
    /// far end.
    fn wants(&self, env: &EventEnvelope) -> bool {
        env.by == self.context.0 && crosses_wire(&env.body)
    }

    /// A real (storage-backed) mesh — its outbox is the workspace log, so it
    /// wakes *after* the append; a demo mesh mirrors into its own feed and wakes
    /// immediately in [`Self::publish`].
    pub(crate) fn is_real(&self) -> bool {
        matches!(self.feed, NetFeed::Real)
    }

    /// The persistable crypto of a real mesh — its current MLS snapshot and the
    /// transport's queue credentials — for a clean-close write into
    /// `transport.state` (so a reopen resumes). `None` for the demo mesh.
    pub(crate) fn crypto_for_close(&self) -> Option<CloseCrypto> {
        let (transport, mls) = self.real_crypto.as_ref()?;
        let snapshot = mls.lock().ok().and_then(|m| m.snapshot().ok());
        Some((snapshot, transport.export_creds()))
    }

    /// A clone of the runtime transport, for minting a **dedicated recovery
    /// queue** off the actor. The clone shares the transport's `Arc`, so it can
    /// both create the queue and later subscribe to it (an SMP queue's receive
    /// credential lives in the creating transport's state — a fresh transport
    /// could send but never receive). `None` for the demo mesh (no real transport).
    pub(crate) fn runtime_transport(&self) -> Option<crate::founding::RitualTransport> {
        self.real_crypto.as_ref().map(|(transport, _)| transport.clone())
    }

    /// Decrypt one MLS **application** message with the runtime group,
    /// returning the group-authenticated sender and the plaintext — how a
    /// relayed mesh announce is authenticated. `None` when this runtime has no
    /// real group, or on any decrypt failure (incl. replays — MLS rejects them).
    pub(crate) fn decrypt_group_message(&self, ct: &[u8]) -> Option<(MemberId, Vec<u8>)> {
        let (_transport, mls) = self.real_crypto.as_ref()?;
        let mut group = mls.lock().ok()?;
        match group.decrypt(ct) {
            Ok(molt_net::MlsIncoming::Application { from, plaintext }) => Some((from, plaintext)),
            _ => None,
        }
    }

    /// The shared runtime MLS group, for an off-actor task that must encrypt
    /// in-sequence with the supervisor (same `Arc`, same ratchet). `None` for
    /// the demo mesh.
    pub(crate) fn group_arc(&self) -> Option<Arc<Mutex<molt_net::MlsMember>>> {
        self.real_crypto.as_ref().map(|(_t, mls)| mls.clone())
    }

    /// The mesh links this runtime runs over (empty for the demo mesh).
    pub(crate) fn mesh(&self) -> &[molt_core::MeshLink] {
        &self.mesh
    }

    /// Coordinator side of recovery: run `restore_member` on the runtime MLS
    /// group — remove the returning member's stale leaf + add its fresh
    /// KeyPackage in one commit → `(commit, welcome)`. `None` when this runtime
    /// has no real MLS group (a demo/state-only node).
    pub(crate) fn restore_member_on_group(
        &self,
        member: &str,
        key_package: &[u8],
    ) -> Option<MlsRekey> {
        let (_transport, mls) = self.real_crypto.as_ref()?;
        let mut group = mls.lock().ok()?;
        Some(group.restore_member(member, key_package).map_err(|e| e.to_string()))
    }

    /// Publish one recorded envelope. The demo mesh mirrors it into its in-memory
    /// feed and wakes the supervisor now; a real mesh does nothing here — its
    /// outbox already read the storage log, so [`State::record`] wakes it after
    /// the append (never blocks — the watch coalesces).
    pub(crate) fn publish(&self, env: &EventEnvelope) {
        if !self.wants(env) {
            return;
        }
        if let NetFeed::Demo(feed) = &self.feed {
            feed.push(env.clone());
            let _ = self.wakeup.send(env.seq);
        }
    }

    /// Wake a real mesh's supervisor for a just-appended envelope (called after
    /// the storage append so the log-backed outbox read sees it).
    pub(crate) fn wake_appended(&self, env: &EventEnvelope) {
        if self.wants(env) {
            let _ = self.wakeup.send(env.seq);
        }
    }
}

impl State {
    /// Drop the transport runtime (workspace switch, persisted open). The
    /// next session-only chat lazily rebuilds it for the current roster.
    pub(crate) fn teardown_net(&mut self) {
        self.net = None;
    }

    /// Clean-close persist: snapshot a running REAL mesh's crypto (advanced MLS
    /// ratchet + the transport's queue credentials) into the active workspace's
    /// `transport.state`, durably, so a reopen resumes the mesh. No-op for the
    /// demo mesh or a session-only context. Takes over `teardown_net`'s job for a
    /// real net: capture the crypto, drop the supervisor (signal its tasks to
    /// stop), then the blocking merge. A supervisor task winding down could still
    /// enqueue one stale per-drain `save` AFTER the merge; the storage writer
    /// **seals `transport.state` on the merge** and ignores any later save, so
    /// the merge is authoritative regardless of that race.
    pub(crate) fn persist_net_crypto_on_close(&mut self) {
        let Some(net) = self.net.take() else {
            return;
        };
        let crypto = net.crypto_for_close();
        drop(net); // stop the supervisor before the durable merge
        if let (Some(active), Some((mls, creds))) = (self.active.as_ref(), crypto) {
            active.handle.persist_crypto_blocking(mls, creds);
        }
    }

    /// Make sure the demo mesh matches the current context. **No-op in
    /// production** (the [`crate::State::demo_mesh`] seam is off — nothing
    /// to stand up, nothing to tear down). On the seam it runs for a
    /// session-only context AND for a persisted workspace whose members
    /// are simulations (`prefs.simulated_members`). A persisted workspace
    /// with real members gets no fakes even on the seam.
    pub(crate) fn ensure_demo_net(&mut self) {
        // a real (T2) mesh is managed by the founding/join/open paths, not here —
        // never tear it down to stand up (or clear) the demo mesh
        if self.net.as_ref().is_some_and(NetRuntime::is_real) {
            return;
        }
        if !self.wants_demo_mesh() {
            self.net = None;
            return;
        }
        let owner = self.member();
        let context = (owner.clone(), self.session.active_workspace.clone());
        if self.net.as_ref().is_some_and(|n| n.context == context) {
            return;
        }
        self.net = None; // old mesh (if any) tears down first
        self.net_generation += 1; // stale Net* commands die at this line
        let peers = self.demo_peer_names(&owner);
        if peers.is_empty() {
            return;
        }
        match self.build_demo_net(owner, context, peers) {
            Ok(net) => self.net = Some(net),
            Err(e) => tracing::warn!(error = %e, "building the demo mesh failed — chat stays local"),
        }
    }

    /// Whether this context should run simulated peer members: only on
    /// the [`crate::State::demo_mesh`] test seam, and there only for a
    /// session-only context or an open workspace explicitly flagged as
    /// simulated in its prefs. With the seam off — every production
    /// engine — the answer is always no: `prefs.simulated_members` stays
    /// parsed but inert.
    fn wants_demo_mesh(&self) -> bool {
        self.demo_mesh
            && match &self.active {
                None => true,
                Some(a) => a.prefs.simulated_members,
            }
    }

    /// The demo peers: for a persisted simulated workspace, the replayed
    /// genesis roster; for the session-only context, the active entry's
    /// non-offline members, else the boot group — always minus the local
    /// member.
    fn demo_peer_names(&self, owner: &MemberId) -> Vec<MemberId> {
        let mut names: Vec<MemberId> = if self.active.is_some() {
            self.roster()
        } else {
            self.session
                .workspaces
                .iter()
                .find(|w| w.id == self.session.active_workspace)
                .filter(|w| !w.members.is_empty())
                .map(|w| {
                    w.members
                        .iter()
                        .filter(|m| m.state != 2) // offline members stay silent
                        .map(|m| m.name.clone())
                        .collect()
                })
                .unwrap_or_else(|| self.roster())
        };
        names.retain(|n| n != owner);
        names.dedup();
        names
    }

    /// Build the full-mesh loopback network: one queue + wrap key per
    /// directed pair (wired by [`LoopbackHub::full_mesh`] — the T2 invite
    /// payload carries this handover in-band), a supervisor for this
    /// engine, and one peer node (engine + supervisor + brain) per other
    /// member.
    fn build_demo_net(
        &self,
        owner: MemberId,
        context: (MemberId, WorkspaceId),
        peers: Vec<MemberId>,
    ) -> Result<NetRuntime, NetError> {
        let hub = LoopbackHub::calm();
        let all: Vec<MemberId> = std::iter::once(owner.clone()).chain(peers.iter().cloned()).collect();
        let mut mesh = hub.full_mesh(&all)?;
        let mut links_for =
            |me: &MemberId| mesh.remove(me).ok_or(NetError::UnknownQueue);

        // this engine's side
        let feed = MemLog::new();
        let (wakeup, wakeup_rx) = watch::channel(0u64);
        let supervisor = supervisor::spawn(
            hub.transport(),
            demo_config(owner.clone(), links_for(&owner)?),
            feed.clone(),
            MemStateStore::new(),
            CmdSink {
                tx: self.cmd_tx.clone(),
                generation: Some(self.net_generation),
            },
            wakeup_rx,
            None, // the demo mesh's peers share no MLS group (plaintext path)
        );

        // the peer nodes
        let threshold = u8::try_from(self.threshold()).unwrap_or(u8::MAX);
        let peer_keepalives = peers
            .iter()
            .map(|name| {
                links_for(name)
                    .map(|links| spawn_demo_peer(name, &all, threshold, &hub, links, &owner))
            })
            .collect::<Result<_, _>>()?;

        Ok(NetRuntime {
            feed: NetFeed::Demo(feed),
            wakeup,
            _supervisor: supervisor,
            _peer_keepalives: peer_keepalives,
            context,
            peer_names: peers,
            generation: self.net_generation,
            real_crypto: None,
            mesh: Vec::new(),
        })
    }

    /// Build the **real** T2 runtime for the open workspace from its persisted
    /// `transport.state`: restore the MLS group, rebuild the full-mesh
    /// [`PeerLink`]s, and spawn a supervisor whose outbox is the encrypted
    /// workspace log ([`StorageLog`]) and whose cursors live in the state file
    /// ([`FileStateStore`]). `transport` must reach the mesh queues — a fresh
    /// [`SmpTransport`] to their server (reopen path), or the still-alive ritual
    /// transport (right after founding, and the only option on the loopback hub,
    /// whose queues can't be reconstructed). Returns `None` when there is no
    /// mesh/group to run (nothing to build) or the group can't be restored.
    pub(crate) fn build_real_net(
        &mut self,
        transport: crate::founding::RitualTransport,
        mesh: &[molt_core::MeshLink],
        mls_blob: &[u8],
    ) -> Option<NetRuntime> {
        let mls = molt_net::MlsMember::restore(mls_blob).ok()?;
        // share the group between the supervisor (advances the ratchet) and the
        // engine (snapshots it on a clean close, so a reopen resumes it)
        self.build_real_net_shared(transport, mesh, Arc::new(Mutex::new(mls)))
    }

    /// [`Self::build_real_net`] over an **already-live** shared group — the
    /// mesh-extension rebuild hands the running `Arc` straight through instead
    /// of a snapshot→restore round-trip: a late encrypt by the dying outbox
    /// advances the SAME ratchet the new supervisor continues from, so sender
    /// generations are never reused (a reused generation is replay-rejected
    /// and silently lost at every peer); resends of already-sent envelopes
    /// dedup by msg id at the receiver.
    pub(crate) fn build_real_net_shared(
        &mut self,
        transport: crate::founding::RitualTransport,
        mesh: &[molt_core::MeshLink],
        mls_arc: Arc<Mutex<molt_net::MlsMember>>,
    ) -> Option<NetRuntime> {
        let active = self.active.as_ref()?;
        let links: Vec<PeerLink> = mesh.iter().filter_map(PeerLink::from_mesh).collect();
        if links.is_empty() {
            return None;
        }
        let owner = self.member();
        let peer_names: Vec<MemberId> = links.iter().map(|l| l.member.clone()).collect();
        let feed = StorageLog::new(active.handle.clone());
        let store = FileStateStore::new(active.handle.clone());
        // a fresh incarnation: any stale demo delivery queued behind the switch
        // dies at this bump (net_generation_current)
        self.net_generation += 1;
        let generation = self.net_generation;
        let (wakeup, wakeup_rx) = watch::channel(0u64);
        let mut seed = [0u8; 8];
        let _ = getrandom::getrandom(&mut seed);
        // keep a transport clone (shares the credential Arc) to export on close
        let transport_for_persist = transport.clone();
        // the concept's defaults: ~2 s per-send fan-out jitter (traffic-analysis
        // resistance) + 1 s→2 min retry backoff. Privacy over snappiness — the
        // runtime is not a low-latency chat, it is an unlinkable mesh.
        let supervisor = supervisor::spawn(
            transport,
            NetConfig::new(owner.clone(), links, u64::from_le_bytes(seed)),
            feed,
            store,
            CmdSink {
                tx: self.cmd_tx.clone(),
                generation: Some(generation),
            },
            wakeup_rx,
            Some(MlsChannel::from_shared(mls_arc.clone())),
        );
        Some(NetRuntime {
            feed: NetFeed::Real,
            wakeup,
            _supervisor: supervisor,
            _peer_keepalives: Vec::new(),
            context: (owner, self.session.active_workspace.clone()),
            peer_names,
            generation,
            real_crypto: Some((transport_for_persist, mls_arc)),
            mesh: mesh.to_vec(),
        })
    }

    // ---- the Net* command handlers ---------------------------------------

    /// Whether a `Net*` command's mesh incarnation is still the current
    /// one. `None` is the engine-lifetime transport (T2+, tests) and is
    /// always current; a tagged command must match the live mesh — a
    /// delivery from a torn-down demo mesh, already queued behind the
    /// workspace switch, must never reach the new context's (possibly
    /// persisted!) log. This is the transport twin of the old simulator's
    /// "session-only workspaces only" guard.
    fn net_generation_current(&self, generation: Option<u64>) -> bool {
        match generation {
            None => true,
            Some(g) => self.net.as_ref().is_some_and(|n| n.generation == g),
        }
    }

    /// Whether a command's **workspace scope** is still the open workspace.
    /// The recovery recv loops and mesh-extension tasks live as long as the
    /// workspace stays open — a mesh REBUILD (extension) does not invalidate
    /// them; only a workspace switch/close does (`reset_workspace_state`).
    pub(crate) fn net_scope_current(&self, scope: Option<u64>) -> bool {
        match scope {
            None => true,
            Some(s) => s == self.net_scope,
        }
    }

    /// An authenticated peer event arrived. Validation failures are
    /// ack-and-skip (returning an error would wedge the supervisor on a
    /// poison event); T1's wire scope is [`crosses_wire`] — everything
    /// else is logged and ignored until MLS lands (T2).
    pub(crate) fn cmd_net_delivered(
        &mut self,
        from: MemberId,
        envelope: EventEnvelope,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            tracing::debug!(%from, "dropping a delivery from a torn-down mesh");
            return Ok(Reply::Ack);
        }
        let known = match &self.net {
            Some(net) => net.peer_names.contains(&from),
            None => self.roster().contains(&from),
        };
        if !known || from == self.member() {
            tracing::warn!(%from, "dropping a delivery from an unknown or impersonated member");
            return Ok(Reply::Ack);
        }
        if envelope.by != from {
            tracing::warn!(%from, claimed = %envelope.by, "dropping a delivery whose author does not match its link");
            return Ok(Reply::Ack);
        }
        match envelope.body {
            WorkspaceEvent::Chat(mut msg) => {
                msg.from = from.clone(); // defense in depth: the link decides
                msg.quote = None; // sender-local LEGACY index — does not transfer
                                  // (`quote_id`/`channel` are global refs and stay)
                // The channel tag is a CLAIM, not a fact (display routing,
                // never a boundary — nothing engine-side trusts it): run it
                // through the same normalization a local send gets, and
                // COERCE an unnormalizable claim (empty/oversized topic
                // name) to the all-hands `Group` channel instead of
                // dropping the message — a peer's mangled tag must not
                // suppress content anyone was meant to see, and the log
                // keeps its "every stored topic name is normalized"
                // invariant. Same posture for CLOSED discussions: the
                // local send guard (`ensure_channel_writable`) is NOT
                // applied here — a peer's message that was in flight while
                // the vote decided must still land identically on every
                // member (convergence over enforcement).
                msg.channel = msg
                    .channel
                    .normalized()
                    .unwrap_or(molt_core::ChannelRef::Group);
                // P5: the wire admits each message exactly once, by stable
                // id — a nil id (pre-chat-bus sender) or an already-known
                // id (duplicate / replay / mesh-rebuild resend) is dropped
                if msg.id.is_nil() {
                    tracing::warn!(%from, "dropping a wire chat message without a stable id");
                    return Ok(Reply::Ack);
                }
                if let Some(pos) = self.chat_pos.get(&msg.id) {
                    match self.chat.get(*pos).map(|stored| stored.from.clone()) {
                        // documented v1 limitation ("id squatting"): whoever
                        // lands an id first keeps it — but a cross-AUTHOR
                        // collision is either a bug or an attempt to occupy
                        // a foreign id, so leave an audit trail at WARN
                        Some(stored_author) if stored_author != from => tracing::warn!(
                            %from,
                            %stored_author,
                            id = %msg.id,
                            "dropping a wire chat message whose duplicate id belongs to another author"
                        ),
                        _ => tracing::debug!(%from, id = %msg.id, "dropping a wire chat message with a duplicate id"),
                    }
                    return Ok(Reply::Ack);
                }
                let id = msg.id;
                let channel = msg.channel.clone();
                let body = msg.body.clone();
                let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
                self.record(env);
                self.emit(molt_core::Event::Chat {
                    id,
                    from,
                    body,
                    channel,
                });
                // P6 drain: refs that outran this message were parked under
                // its id — the appliers re-evaluate the P5 rules NOW,
                // against the just-landed link-authenticated message
                // (delete only from its author, file-removal only from its
                // sharer, a reaction always — `by` was forced to the link
                // at park time), through the very same record/emit path a
                // live arrival takes.
                for r in self.parked.drain(&id) {
                    match r {
                        PendingRef::React { by, emoji, op } => self.wire_react(id, by, emoji, op),
                        PendingRef::Delete { by } => self.wire_delete(id, by),
                        PendingRef::FileRemove { by } => self.wire_file_remove(id, by),
                    }
                }
            }
            // chat-bus B1, the P5 receive-side matrix: the id-addressed chat
            // verbs. Defense in depth mirrors the Chat arm — the acting
            // member (`by`) is ALWAYS the authenticated link identity, the
            // target resolves by stable id only (a sender-local index never
            // transfers, same posture as the legacy quote), and the recorded
            // event writes the LOCAL position into the legacy `index` field
            // for older readers. An unknown target parks (P6) and re-applies
            // when its message lands — see the Chat arm's drain.
            WorkspaceEvent::ChatReacted { id, emoji, op, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire reaction without a message id");
                    return Ok(Reply::Ack);
                };
                // the local-send sanity check (cmd_react_chat's twin)
                let Some(emoji) = crate::chat::sanitize_emoji(&emoji) else {
                    tracing::warn!(%from, "dropping a wire reaction with a malformed emoji");
                    return Ok(Reply::Ack);
                };
                self.wire_react(id, from, emoji, op);
            }
            WorkspaceEvent::ChatDeleted { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire delete without a message id");
                    return Ok(Reply::Ack);
                };
                self.wire_delete(id, from);
            }
            WorkspaceEvent::FileRemoved { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire file-removal without a message id");
                    return Ok(Reply::Ack);
                };
                self.wire_file_remove(id, from);
            }
            // chain governance gossip + block broadcast — only a chain-governed
            // workspace acts on it (the transport carries it; the chain decides)
            WorkspaceEvent::Proposed { id, surface, payload } if self.is_chain_governed() => {
                // defense in depth: a peer's set_image must respect the same
                // byte cap AND decodability sniff the propose validation
                // enforces locally (WP3) — an oversized or undecodable
                // payload is dropped, never recorded (convergence before
                // enforcement, like every wire guard)
                if surface == molt_core::Surface::Organization
                    && payload.get("op").and_then(serde_json::Value::as_str) == Some("set_image")
                    && !crate::proposals::image_bytes(&payload).is_some_and(|b| {
                        b.len() <= crate::proposals::ORG_IMAGE_MAX_BYTES
                            && crate::proposals::image_decodable(&b).is_ok()
                    })
                {
                    tracing::warn!(from = %from, "dropping a set_image proposal without valid, decodable bytes within the cap");
                    return Ok(Reply::Ack);
                }
                // announce only a genuinely NEW proposal: a WP2 re-serve or
                // an id-collision refusal must not (re-)ring frontends
                if self.receive_proposed(id.0, surface, payload) {
                    self.emit(molt_core::Event::Proposed { id, surface, by: from });
                }
            }
            WorkspaceEvent::Approved { id, by, height, sig } if self.is_chain_governed() => {
                self.receive_approval(id.0, &by, height, &sig);
                self.emit(molt_core::Event::Approved {
                    id,
                    have: self.chain_approval_count(id.0),
                    need: self.threshold(),
                });
            }
            WorkspaceEvent::Committed(block) if self.is_chain_governed() => {
                self.receive_block(block);
            }
            WorkspaceEvent::ChainRequest { from_height } if self.is_chain_governed() => {
                self.serve_chain_from(from_height);
                // WP2: the requester is (re)joining the conversation — beyond
                // the committed suffix it also lost the ephemeral open
                // governance state with its RAM, so re-serve that too
                self.serve_open_governance();
            }
            WorkspaceEvent::MembershipProposed {
                id,
                op,
                member,
                identity_pk,
            } if self.is_chain_governed() => {
                self.receive_membership_proposal(id.0, op, &member, &identity_pk);
            }
            // WP4b: a peer proposed a compaction cut — recompute the state
            // hash from OUR chain and auto-co-sign only on a match
            // (verify-before-sign; correctness attestation, not a product
            // decision, so no human round-trip)
            WorkspaceEvent::CheckpointProposed { id, upto, state_hash }
                if self.is_chain_governed() =>
            {
                self.receive_checkpoint_proposal(id.0, upto, &state_hash);
            }
            // WP4b: a pruned peer served its blob ahead of the anchor —
            // stash it; the adopt happens hard-verified once the anchor
            // block (and its suffix) arrive as Committed frames
            WorkspaceEvent::CheckpointServed { blob } if self.is_chain_governed() => {
                self.receive_checkpoint_blob(blob);
            }
            // dynamic mesh membership ❸: a relayed mesh announce — authenticate
            // the ANNOUNCER by MLS decryption (the event author is only the
            // relay) and extend this node's own mesh toward it
            WorkspaceEvent::MeshAnnounced { ct } if self.is_chain_governed() => {
                let me = self.member();
                if let Ok(raw) = hex::decode(&ct) {
                    if let Some((announcer, plain)) =
                        self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
                    {
                        if announcer != me && self.roster().contains(&announcer) {
                            if let Ok(a) =
                                serde_json::from_slice::<molt_net::mesh::MeshAnnounce>(&plain)
                            {
                                self.spawn_mesh_extension(announcer, &a);
                            }
                        }
                    }
                }
            }
            // a member wants a shared file's bytes: authenticate the
            // REQUESTER by MLS decryption (like a mesh announce), and only
            // the SHARER acts — everyone else in the group decrypts the
            // broadcast and drops it silently. The bytes then flow over the
            // advertised dedicated queue, never through this log.
            WorkspaceEvent::FileRequested { ct } => {
                let me = self.member();
                if let Ok(raw) = hex::decode(&ct) {
                    if let Some((requester, plain)) =
                        self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
                    {
                        if requester != me && self.roster().contains(&requester) {
                            if let Ok(req) =
                                serde_json::from_slice::<molt_net::transfer::FetchRequest>(&plain)
                            {
                                self.answer_file_request(req);
                            }
                        }
                    }
                }
            }
            other => {
                tracing::debug!(%from, kind = ?std::mem::discriminant(&other), "event over the wire not acted on here");
            }
        }
        Ok(Reply::Ack)
    }

    /// A group-authenticated fetch request landed. The broadcast reaches
    /// EVERY member, so **only the sharer answers** — a member that does
    /// not (yet) hold the share, or whose share it isn't, stays completely
    /// silent: a `Refused` from a non-sharer would abort the requester's
    /// fetch of the REAL sharer's bytes (a laggard's refusal racing the
    /// sharer's manifest). Once this node is established as the sharer,
    /// honest refusals (unavailable, path lost) are correct.
    fn answer_file_request(&mut self, req: molt_net::transfer::FetchRequest) {
        let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) else {
            return; // no real mesh → nothing to serve on
        };
        if req.expires < crate::now_secs() {
            tracing::debug!(share = %req.id, "dropping an expired file request");
            return; // the requester is long gone — nobody listens for a refusal
        }
        let Ok(id) = req.id.parse::<MessageId>() else {
            return;
        };
        let me = self.member();
        // silent unless this node is the sharer — never refuse a share we
        // simply don't have; the actual sharer answers
        let is_my_share = matches!(self.chat_by_id(&id), Ok((_, msg)) if msg.from == me);
        if !is_my_share {
            return;
        }
        let refuse = |reason: &str| {
            let frame = molt_net::transfer::TransferFrame::Refused {
                id: req.id.clone(),
                reason: reason.to_string(),
            };
            crate::transfer::spawn_send_refusal(transport.clone(), req.reply.clone(), frame);
        };
        let (_, msg) = self.chat_by_id(&id).expect("just checked it is our share");
        let Some(file) = msg.file.as_ref() else {
            refuse("the message carries no file");
            return;
        };
        if !file.available {
            refuse("the sharer removed the file — no longer available");
            return;
        }
        // uploads are ephemeral like chat: once the share aged out of the
        // sharer's own read contract it is not served any more, even to a
        // requester whose engine skipped its local check (near the boundary
        // this is an honest refusal, not a hang)
        if self.chat_ts_aged_out(msg.ts) {
            refuse("the share aged out of the chat retention window");
            return;
        }
        let size = file.size;
        let Some(path) = self.share_paths.get(&id).cloned() else {
            refuse("this node no longer knows the shared file's local path");
            return;
        };
        crate::transfer::spawn_file_serve(
            transport,
            path,
            size,
            req.id,
            req.reply,
            self.file_serve_slots.clone(),
        );
    }

    /// The fetch task's request is ready: record the `FileRequested` event
    /// (the outbox ships it to every peer; the sharer answers).
    pub(crate) fn cmd_net_file_request_ready(
        &mut self,
        id: MessageId,
        ct: String,
    ) -> Result<Reply, MoltError> {
        // the share must still exist and be available — the honest guard
        // before broadcasting a request every member will decrypt
        let (_, msg) = self.chat_by_id(&id)?;
        if !msg.file.as_ref().is_some_and(|f| f.available) {
            return Err(MoltError::FileUnavailable(id));
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::FileRequested { ct });
        self.record(env);
        Ok(Reply::Ack)
    }

    /// A returning member's recovery request reached this coordinator (recovery
    /// step ❸): verify the seat proof against the anchored roster identity and
    /// propose the threshold re-admission, remembering the fresh KeyPackage +
    /// reply queue for the MLS re-key once the `Restored` block commits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_net_recover_requested(
        &mut self,
        member: MemberId,
        identity_pk: String,
        key_package: String,
        ticket: String,
        seat_proof: String,
        reply: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        // Spend-once guard: the ticket must be a live one this node minted (via
        // a recovery link). An unknown or already-spent ticket — a replay of a
        // captured request, or a bare-queue probe — is dropped without a trace.
        if !self.recovery_tickets.contains(&ticket) {
            tracing::warn!(%member, "recovery request with an unknown or spent ticket — dropped");
            return Ok(Reply::Ack);
        }
        // NB: on a verified request, verify_and_propose_restore registers the
        // pending recovery BEFORE proposing (a lone coordinator commits the
        // block synchronously inside the propose, which consumes that entry)
        match self.verify_and_propose_restore(
            &member,
            &identity_pk,
            &key_package,
            &ticket,
            &seat_proof,
            &reply,
        ) {
            Ok(_id) => {
                // spend the ticket only on a verified request, so a legitimate
                // member whose first attempt failed (e.g. a truncated proof) can
                // retry on the still-live queue
                self.recovery_tickets.remove(&ticket);
                tracing::info!(%member, "recovery seat proof verified — proposing re-admission");
            }
            Err(e) => {
                tracing::warn!(%member, error = %e, "dropping an invalid recovery request");
            }
        }
        Ok(Reply::Ack)
    }

    /// A surviving coordinator mints a recovery link for a member who lost its
    /// device (`recovery_ritual.md` §3) — a manually-granted re-admission for an
    /// existing seat. Validate the request against the open chain-governed
    /// workspace, mint a single-use ticket (the spend-once guard registers it),
    /// then provision the dedicated recovery queue off the actor and report the
    /// link. Caller errors (no republic, unknown seat, no chain) reject hard;
    /// OPERATIONAL states report on the recovery notice channel instead — the
    /// mint's real outcome (the link, or a failure) always arrives there, and
    /// the RETURNING member's presence is never involved: the link exists
    /// precisely because that member is unreachable.
    pub(crate) fn cmd_recover_invite_start(
        &mut self,
        member: MemberId,
    ) -> Result<Reply, MoltError> {
        // recovery only exists for a chain-governed republic (the returning
        // member re-verifies the handed-over chain from genesis)
        if !self.is_chain_governed() {
            return Err(MoltError::Recover(
                "recovery needs an open, chain-governed republic".to_string(),
            ));
        }
        let Some(replica) = self.replica.as_ref() else {
            return Err(MoltError::Recover("no republic is open".to_string()));
        };
        // the returning member must be an anchored seat (the seat proof will be
        // checked against this key when the request arrives)
        if !replica.identities.iter().any(|i| i.member == member) {
            return Err(MoltError::Recover(format!(
                "{member} is not a member of this republic"
            )));
        }
        let republic = replica.name.clone();
        let republic_id = replica.republic_id.clone();
        // announce the attempt on the recovery notice channel: the frontends
        // render a calm pending state until the real outcome (`recovery-link:`
        // or `recovery-link-failed:`) replaces it — and because pending and
        // outcome always differ, a REPEATED identical outcome still
        // edge-triggers on every attempt
        self.session.notice = format!("recovery-link-pending:{member}");
        self.emit_session(molt_core::SessionScope::Full);
        // the recovery queue is minted on the RUNTIME transport (a clone shares
        // its Arc, so this node can both create the queue and subscribe to it).
        // No runtime mesh (e.g. the workspace was reopened without a resumable
        // transport) is an operational state of THIS node, not a caller error:
        // ack the decision and report the calm outcome on the notice channel.
        let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) else {
            return self.cmd_net_recover_link_failed(
                member,
                "mesh-not-running".to_string(),
                String::new(),
                None,
            );
        };
        let ticket = molt_net::invite::mint_ticket().map_err(|e| MoltError::Recover(e.to_string()))?;
        let wrap = molt_net::wrap::WrapKey::fresh().map_err(|e| MoltError::Recover(e.to_string()))?;
        // register the ticket BEFORE the queue can carry a request, so the
        // spend-once guard is armed the moment the returning member answers
        self.recovery_tickets.insert(ticket.clone());
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Recover("engine stopped".to_string()));
        };
        crate::recovery::spawn_recovery_provisioning(
            transport,
            member,
            republic,
            republic_id,
            ticket,
            wrap,
            // recovery loops are scoped to the open WORKSPACE (a mesh rebuild
            // mid-recovery must not orphan the minted link)
            self.net_scope,
            cmd_tx,
            self.recovery_material_sink.clone(),
        );
        Ok(Reply::Ack)
    }

    /// A minted recovery link became available (from the off-actor provisioning
    /// task). Surface it to the operator so it can be shared off-band with the
    /// returning member.
    pub(crate) fn cmd_net_recover_link_ready(
        &mut self,
        member: MemberId,
        link: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        // the link itself is single-use secret material — it goes to the
        // operator surface only, never into the log
        tracing::info!(%member, "recovery link ready");
        self.session.notice = format!("recovery-link:{link}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A recovery-link mint failed — either synchronously (no runtime mesh,
    /// from [`Self::cmd_recover_invite_start`] itself) or from the off-actor
    /// provisioning task (`Command::NetRecoverLinkFailed`, e.g. the SMP server
    /// was unreachable). Surface the calm `recovery-link-failed:` notice on the
    /// same channel the minted link rides — the operator asked for a link, so
    /// silence would leave it waiting forever — and unregister the dead mint's
    /// ticket (it never left this node; nothing of the attempt stays armed).
    pub(crate) fn cmd_net_recover_link_failed(
        &mut self,
        member: MemberId,
        reason: String,
        ticket: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        if !ticket.is_empty() {
            self.recovery_tickets.remove(&ticket);
        }
        tracing::warn!(%member, %reason, "recovery link mint failed");
        self.session.notice = format!("recovery-link-failed:{reason}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A rejoiner's **mesh announce** arrived on the recovery queue (dynamic
    /// mesh membership, `documents/dynamic_mesh.md` ❷): authenticate the
    /// announcer by MLS decryption and check it is the member whose re-key
    /// just completed, then relay the ciphertext **verbatim** over the runtime
    /// mesh (every survivor authenticates + extends itself) and extend this
    /// node's own mesh toward the rejoiner.
    pub(crate) fn cmd_net_recover_announced(
        &mut self,
        ct: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let Ok(raw) = hex::decode(&ct) else {
            return Ok(Reply::Ack);
        };
        let Some((announcer, plain)) =
            self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
        else {
            tracing::warn!("a recovery-queue mesh announce did not decrypt — dropped");
            return Ok(Reply::Ack);
        };
        // parse BEFORE spending the one-shot window: a malformed (but
        // authentic) announce must degrade to a dropped frame, not burn the
        // rejoiner's only chance to re-mesh (version skew / client bug)
        let Ok(announce) = serde_json::from_slice::<molt_net::mesh::MeshAnnounce>(&plain) else {
            tracing::warn!(%announcer, "mesh announce is malformed — dropped (window kept)");
            return Ok(Reply::Ack);
        };
        // only the member whose re-key JUST completed may (re)announce here —
        // the recovery queue can never re-point another member's links
        if !self.recovery_mesh_window.remove(&announcer) {
            tracing::warn!(%announcer, "mesh announce outside a recovery window — dropped");
            return Ok(Reply::Ack);
        }
        // relay VERBATIM: each survivor decrypts (and thereby authenticates)
        // the announcer itself, exactly like the founding star relay
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::MeshAnnounced { ct });
        self.record(env);
        self.spawn_mesh_extension(announcer, &announce);
        Ok(Reply::Ack)
    }

    /// Extend this node's running mesh toward `member` (dynamic mesh
    /// membership ❹): create a fresh per-pair inbound queue, reply with our
    /// own MLS-encrypted announce **directly onto the queue `member` announced
    /// for us** (per-queue FIFO puts it ahead of any runtime traffic), and
    /// report the assembled link back as [`Command::NetMeshExtended`]. Off the
    /// actor — queue creation is a live round-trip.
    pub(crate) fn spawn_mesh_extension(
        &mut self,
        member: MemberId,
        announce: &molt_net::mesh::MeshAnnounce,
    ) {
        // per-member cooldown: an extension costs a full supervisor
        // teardown+rebuild+fsync on THIS node, so a member re-announcing
        // within the window is ignored (its first announce always passes —
        // recovery and honest rotation are one-shot; only rapid repeats are
        // capped, bounding the churn a misbehaving member can inflict)
        let now = crate::now_secs();
        if let Some(last) = self.mesh_extension_at.get(&member) {
            if now.saturating_sub(*last) < MESH_EXTENSION_COOLDOWN_SECS {
                tracing::warn!(%member, "mesh announce inside the cooldown — ignored");
                return;
            }
        }
        self.mesh_extension_at.insert(member.clone(), now);
        let me = self.member();
        let Some(target) = announce.queues.get(&me) else {
            tracing::warn!(%member, "mesh announce carries no queue for this node");
            return;
        };
        let (Some(snd), Some(wrap_out)) = (target.addr(), target.wrap_key()) else {
            tracing::warn!(%member, "mesh announce handover is malformed");
            return;
        };
        let Some(net) = self.net.as_ref() else {
            return;
        };
        let (Some(transport), Some(group)) = (net.runtime_transport(), net.group_arc()) else {
            tracing::warn!(%member, "no real runtime mesh to extend");
            return;
        };
        // workspace scope, not mesh generation: a CONCURRENT extension's
        // rebuild must not drop this one's result (both fold into the live net)
        let generation = self.net_scope;
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let pair = match transport.create_queue().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%member, error = %e, "mesh-extension queue creation failed");
                    return;
                }
            };
            let Ok(wrap_in) = molt_net::WrapKey::fresh() else {
                return;
            };
            let mut queues = std::collections::BTreeMap::new();
            queues.insert(
                member.clone(),
                molt_net::mesh::QueueHandover::of(&pair.snd, &wrap_in),
            );
            let reply = molt_net::mesh::MeshAnnounce { queues };
            let Ok(bytes) = serde_json::to_vec(&reply) else {
                return;
            };
            // encrypt with the SHARED runtime group (same Arc as the
            // supervisor — one ratchet, used in sequence)
            let Some(ct) = group.lock().ok().and_then(|mut g| g.encrypt(&bytes).ok()) else {
                tracing::warn!(%member, "encrypting the mesh reply failed");
                return;
            };
            let msg = molt_net::invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            let Ok(payload) = serde_json::to_vec(&msg) else {
                return;
            };
            if let Err(e) = supervisor::send_framed(
                &transport,
                &snd,
                &wrap_out,
                molt_net::msg_id(&me, &member, 3),
                &payload,
            )
            .await
            {
                tracing::warn!(%member, error = %e, "sending the mesh reply failed");
                return;
            }
            let link = PeerLink {
                member: member.clone(),
                snd,
                wrap_out,
                rcv: pair.rcv,
                wrap_in,
            }
            .to_mesh();
            let (reply_tx, _rx) = oneshot::channel();
            let _ = cmd_tx
                .send(Envelope {
                    cmd: Command::NetMeshExtended {
                        link,
                        generation: Some(generation),
                    },
                    reply: reply_tx,
                })
                .await;
        });
    }

    /// Fold a freshly assembled per-pair link into the **running** mesh
    /// (dynamic mesh membership ❺): rebuild the supervisor over
    /// `old mesh + link` — replacing any stale link to the same member (a
    /// recovered seat's old queues are dead) — and persist the grown mesh +
    /// crypto so a reopen resumes it. The rebuild IS the reopen path: per-peer
    /// cursors live in `transport.state` and survive it.
    pub(crate) fn cmd_net_mesh_extended(
        &mut self,
        link: molt_core::MeshLink,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let Some(net) = self.net.as_ref() else {
            return Ok(Reply::Ack);
        };
        if !net.is_real() {
            return Ok(Reply::Ack);
        }
        // everything fallible is hoisted BEFORE the teardown: a failed
        // precondition must leave the old, working mesh standing (the rebuild
        // itself cannot start until the old supervisor is down — a second
        // subscriber on the same SMP queues would supersede the first
        // server-side)
        let member = link.member.clone();
        if PeerLink::from_mesh(&link).is_none() {
            tracing::warn!(%member, "mesh extension link is malformed — keeping the old mesh");
            return Ok(Reply::Ack);
        }
        let mut mesh = net.mesh().to_vec();
        mesh.retain(|l| l.member != link.member);
        mesh.push(link);
        let (Some(transport), Some(group)) = (net.runtime_transport(), net.group_arc()) else {
            return Ok(Reply::Ack);
        };
        if self.active.is_none() {
            return Ok(Reply::Ack);
        }
        // stop the old supervisor, then rebuild over the grown mesh SHARING
        // the live group Arc — no snapshot→restore: a late encrypt by a dying
        // outbox task advances the same ratchet the new supervisor continues
        // from, so sender generations are never rewound/reused (the snapshot
        // variant silently lost one message per peer in that race)
        self.teardown_net();
        if let Some(new_net) = self.build_real_net_shared(transport, &mesh, group.clone()) {
            // the grown mesh must survive a reopen — a LIVE merge (the rebuilt
            // supervisor keeps saving its cursors afterwards, so no seal),
            // snapshotted AFTER the rebuild from the shared group
            let crypto = new_net.crypto_for_close();
            self.net = Some(new_net);
            if let (Some(active), Some((mls, creds))) = (self.active.as_ref(), crypto) {
                active.handle.persist_mesh_crypto_blocking(mls, creds, mesh);
            }
            self.session.notice = format!("mesh-extended:{member}");
            self.emit_session(SessionScope::Full);
            tracing::info!(%member, "mesh extended");
        } else {
            tracing::warn!(%member, "mesh extension rebuild failed");
        }
        Ok(Reply::Ack)
    }

    // ---- the P5 appliers (live wire arrivals AND P6 drains) --------------
    //
    // Each resolves the target by stable id; an unknown target parks the
    // ref (P6) instead of dropping it. `from` is ALWAYS the authenticated
    // link identity (a live arm passes the link; a drain passes the `by`
    // that was forced to the link at park time), so the authorization
    // checks below never trust event-claimed data. A drain runs right
    // after the target's `Chat` was inserted, so it cannot re-park.

    /// Apply (or park) a link-authenticated wire reaction. The sender's
    /// explicit `op` passes through unchanged (`None` only from a legacy
    /// peer — that records the old toggle semantics, accepted Q3-style
    /// degradation while versions are mixed).
    fn wire_react(
        &mut self,
        id: MessageId,
        from: MemberId,
        emoji: String,
        op: Option<molt_core::ReactOp>,
    ) {
        let Ok((index, msg)) = self.chat_by_id(&id) else {
            tracing::debug!(%from, %id, "a wire reaction arrived before its message — parked (P6)");
            self.parked.park(id, PendingRef::React { by: from, emoji, op });
            return;
        };
        // a KNOWN but tombstoned target: skip entirely — recording would
        // put a dead event in the log (the applier ignores reactions on
        // tombstones so that react/delete commute)
        if msg.deleted_by.is_some() {
            tracing::debug!(%from, %id, "skipping a wire reaction on a tombstoned message");
            return;
        }
        self.record_react(index, id, from, emoji, op);
    }

    /// Apply (or park) a link-authenticated wire delete. Honored only if
    /// `from` is the target's author in OUR log (no moderation concept).
    fn wire_delete(&mut self, id: MessageId, from: MemberId) {
        let Ok((index, msg)) = self.chat_by_id(&id) else {
            tracing::debug!(%from, %id, "a wire delete arrived before its message — parked (P6)");
            self.parked.park(id, PendingRef::Delete { by: from });
            return;
        };
        // no moderation concept: only the author wipes its own message —
        // and the author is what OUR log says, never a claim in the event
        if msg.from != from {
            tracing::warn!(%from, %id, "dropping a wire delete from a non-author");
            return;
        }
        self.record_delete(index, id, from);
    }

    /// Apply (or park) a link-authenticated wire file-removal. Honored only
    /// if `from` is the sharer (the share message's author in OUR log).
    fn wire_file_remove(&mut self, id: MessageId, from: MemberId) {
        let Ok((index, msg)) = self.chat_by_id(&id) else {
            tracing::debug!(%from, %id, "a wire file-removal arrived before its message — parked (P6)");
            self.parked.park(id, PendingRef::FileRemove { by: from });
            return;
        };
        // only the sharer (the share message's author in OUR log) may flip
        // its own share to unavailable
        if msg.from != from || msg.file.is_none() {
            tracing::warn!(%from, %id, "dropping a wire file-removal from a non-sharer");
            return;
        }
        self.record_file_remove(index, id, from);
    }

    /// Passive presence: stamp the member with the engine clock's real
    /// unix time (authenticated inbound traffic is the ONLY thing that
    /// moves a stamp) and lift a send-failure pin.
    pub(crate) fn cmd_net_peer_seen(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.net_unreachable.remove(&member);
            let now = self.presence_now();
            self.stamp_member_pill(&member, now);
        }
        Ok(Reply::Ack)
    }

    /// Transport trouble: pin the member's pill unreachable. The last-seen
    /// stamp stays untouched — it records real sightings only; the pin
    /// holds through aging ticks until the next sighting clears it.
    pub(crate) fn cmd_net_send_failed(
        &mut self,
        member: MemberId,
        reason: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            return Ok(Reply::Ack);
        }
        tracing::warn!(%member, %reason, "sends to a member keep failing — outbox is backing off");
        self.net_unreachable.insert(member);
        self.refresh_member_pills();
        Ok(Reply::Ack)
    }

    /// The presence ticker (spawned with the actor, period
    /// [`crate::PRESENCE_TICK_MS`]): re-age every pill from its stamp so
    /// a silent member drifts online → stale → offline. The stamps only
    /// ever move on real traffic; reads additionally re-derive live, so
    /// the tick exists for the PUSHED session pills.
    pub(crate) fn cmd_net_presence_tick(&mut self) -> Result<Reply, MoltError> {
        self.refresh_member_pills();
        Ok(Reply::Ack)
    }

    /// Record a real sighting on the active workspace entry's pill. The
    /// stamp is always advanced (aging + the activity trio read it), but a
    /// full session push fires ONLY when the pill STATE changes — a peer
    /// already online re-stamping every second must not re-broadcast the
    /// whole session for a label that renders identically (the 30 s ticker
    /// and the next state-change push carry the refreshed stamp).
    fn stamp_member_pill(&mut self, member: &MemberId, now: u64) {
        let active = self.session.active_workspace.clone();
        let Some(entry) = self.session.workspaces.iter_mut().find(|w| w.id == active) else {
            return;
        };
        let Some(m) = entry.members.iter_mut().find(|m| m.name == *member) else {
            return;
        };
        let state = molt_core::presence_state(now, now);
        let state_changed = m.state != state;
        m.state = state;
        m.last_seen = now;
        if state_changed {
            self.emit_session(SessionScope::Full);
        }
    }

    /// Re-derive every pill state of the active entry from its stamp
    /// (self always online, send-failure pins win); emits only when a
    /// state actually changed.
    fn refresh_member_pills(&mut self) {
        let now = self.presence_now();
        let me = self.member();
        let active = self.session.active_workspace.clone();
        let unreachable = &self.net_unreachable;
        let Some(entry) = self.session.workspaces.iter_mut().find(|w| w.id == active) else {
            return;
        };
        let mut changed = false;
        for m in &mut entry.members {
            let state = if m.name == me {
                0
            } else if unreachable.contains(&m.name) {
                2
            } else {
                molt_core::presence_state(now, m.last_seen)
            };
            if m.state != state {
                m.state = state;
                changed = true;
            }
        }
        if changed {
            self.emit_session(SessionScope::Full);
        }
    }
}

/// The demo mesh's supervisor tuning: short jitter, standard backoff.
fn demo_config(member: MemberId, peers: Vec<PeerLink>) -> NetConfig {
    let mut seed = [0u8; 8];
    let _ = getrandom::getrandom(&mut seed);
    NetConfig {
        jitter_max_ms: DEMO_JITTER_MS,
        ..NetConfig::new(member, peers, u64::from_le_bytes(seed))
    }
}

/// A stable per-name seed: deterministic brains make the demo — and its
/// tests — reproducible. (`| 1`: xorshift must not start at 0.)
fn name_seed(name: &str) -> u64 {
    fnv1a64(name) | 1
}

/// Spawn one demo peer: an engine of its own, its transport supervisor,
/// and the brain that answers the owner.
fn spawn_demo_peer(
    name: &MemberId,
    all: &[MemberId],
    threshold: u8,
    hub: &LoopbackHub,
    links: Vec<PeerLink>,
    owner: &MemberId,
) -> mpsc::Sender<Envelope> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(crate::CMD_QUEUE);
    let feed = MemLog::new();
    let (wakeup, wakeup_rx) = watch::channel(0u64);
    let supervisor = supervisor::spawn(
        hub.transport(),
        demo_config(name.clone(), links),
        feed.clone(),
        MemStateStore::new(),
        CmdSink {
            tx: cmd_tx.downgrade(),
            // the peer's one and only mesh: State::new starts the
            // generation counter at 0 and nothing on a peer bumps it
            generation: Some(0),
        },
        wakeup_rx,
        None, // demo peer: plaintext path (no MLS group)
    );
    let net = NetRuntime {
        feed: NetFeed::Demo(feed),
        wakeup,
        _supervisor: supervisor,
        _peer_keepalives: Vec::new(),
        context: (name.clone(), String::new()),
        peer_names: all.iter().filter(|m| *m != name).cloned().collect(),
        generation: 0,
        real_crypto: None,
        mesh: Vec::new(),
    };
    let config = GroupConfig {
        member: name.clone(),
        members: all.to_vec(),
        threshold: usize::from(threshold),
        self_cosign: true,
    };
    let handle = crate::spawn_actor(
        config,
        SessionView::default(),
        cmd_tx.clone(),
        cmd_rx,
        None,
        false,
        Some(net),
        None,
        false,
        false,
        false,
        None,
        // a peer node lives on the demo seam by definition: its own
        // `ensure_demo_net` must keep (not tear down) the injected mesh
        true,
    );
    spawn_brain(handle.subscribe(), cmd_tx.downgrade(), owner.clone(), name_seed(name));
    // the returned sender is the peer's sole keepalive: mesh teardown
    // drops it, the actor exits, its State (and with it the supervisor
    // handle) drops, and every transport task aborts
    cmd_tx
}

/// The peer's brain: answer roughly a third of the owner's messages with
/// a canned line, after a natural delay — via its own engine, so the
/// reply travels the full record → outbox → hub → delivery path. Holding
/// only a weak sender, it never keeps a torn-down peer engine alive.
fn spawn_brain(
    mut events: tokio::sync::broadcast::Receiver<molt_core::Event>,
    weak_tx: mpsc::WeakSender<Envelope>,
    owner: MemberId,
    seed: u64,
) {
    tokio::spawn(async move {
        let mut rng = seed;
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let molt_core::Event::Chat { from, body, .. } = ev else {
                continue;
            };
            if from != owner {
                continue; // answer the human only — no peer-to-peer chatter loops
            }
            if body.is_empty() {
                continue; // file shares travel as empty-bodied messages —
                          // the old demo never answered those either
            }
            if mockrand::xorshift(&mut rng) % BRAIN_REPLY_ONE_IN != 0 {
                continue;
            }
            let line = LINES[usize::try_from(mockrand::xorshift(&mut rng)).unwrap_or_default()
                % LINES.len()];
            let delay = BRAIN_DELAY_BASE_MS + mockrand::xorshift(&mut rng) % BRAIN_DELAY_SPAN_MS;
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            let Some(tx) = weak_tx.upgrade() else { break };
            let (reply, _rx) = oneshot::channel();
            if tx
                .send(Envelope {
                    cmd: Command::Chat {
                        body: line.to_string(),
                        quote: None,
                        channel: molt_core::ChannelRef::default(),
                    },
                    reply,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{ParkedRefs, PendingRef, PARKED_TARGET_CAP};
    use molt_core::{ChatMessage, EventEnvelope, MessageId, WorkspaceEvent};

    /// The demo mesh is a **default-off test seam**: a freshly built state
    /// (what every production spawner creates) wants no fake peers in the
    /// session-only boot context; only the seam flag re-enables them.
    #[test]
    fn the_demo_mesh_seam_is_default_off() {
        let mut st = crate::tests::plain_state();
        assert!(
            !st.demo_mesh,
            "production default: the demo-mesh seam starts OFF"
        );
        assert!(
            !st.wants_demo_mesh(),
            "the boot context must not want fake peers without the seam"
        );
        st.demo_mesh = true;
        assert!(
            st.wants_demo_mesh(),
            "the test seam re-enables the session-only demo mesh"
        );
    }

    /// The provisioning task's failure report lands as the calm
    /// `recovery-link-failed:` session notice (the same channel the minted
    /// link rides), and the dead mint's ticket is unregistered — nothing of
    /// the failed attempt stays armed.
    #[test]
    fn a_recover_link_failure_report_sets_the_notice_and_kills_the_ticket() {
        let mut st = crate::tests::plain_state();
        st.recovery_tickets.insert("t-1".to_string());
        st.cmd_net_recover_link_failed(
            "bob".to_string(),
            "boom".to_string(),
            "t-1".to_string(),
            None,
        )
        .expect("the report acks");
        assert_eq!(st.session.notice, "recovery-link-failed:boom");
        assert!(
            st.recovery_tickets.is_empty(),
            "the failed mint's ticket must not stay armed"
        );
    }

    /// A wire reaction whose known target is already a tombstone is skipped
    /// ENTIRELY — no event recorded (the log gets no dead entry), nothing
    /// parked, no reaction on the tombstone. The commuting twin of the
    /// applier-side guard: react/delete converge independent of order.
    #[test]
    fn a_wire_reaction_on_a_tombstone_records_no_event() {
        let mut st = crate::tests::plain_state();
        let id = MessageId([0x2au8; 16]);
        st.apply(&EventEnvelope {
            seq: 1,
            ts: 101,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(id, "peer-1", "soon gone", 101)),
        });
        st.apply(&EventEnvelope {
            seq: 2,
            ts: 102,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::ChatDeleted {
                index: 0,
                id: Some(id),
                by: "peer-1".to_string(),
            },
        });
        let seq_before = st.next_seq;
        st.wire_react(
            id,
            "peer-2".to_string(),
            "🔥".to_string(),
            Some(molt_core::ReactOp::Add),
        );
        assert_eq!(st.next_seq, seq_before, "no event was recorded");
        assert!(st.chat[0].reactions.is_empty(), "no reaction on the tombstone");
        assert!(!st.parked.holds(&id), "a KNOWN tombstoned target parks nothing");
    }

    /// A wire chat message into a DECIDED vote's discussion still lands in
    /// the log: closed discussions are enforced on the local send paths
    /// only (`cmd_chat` / `cmd_share_file`) — the receive path stays
    /// permissive so every member's log converges even when a peer's
    /// message was in flight while the vote decided (convergence over
    /// enforcement, same posture as the channel-claim coercion above).
    #[test]
    fn a_wire_chat_into_a_closed_discussion_still_lands() {
        // a runtime context: the delivery path may publish to a transport
        // feed / bump watch channels (spawned tasks)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        // a proposal that is proposed, then declined — replayed as events,
        // exactly like the log rebuild does it
        st.apply(&EventEnvelope {
            seq: 1,
            ts: 100,
            by: "me".to_string(),
            body: WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: molt_core::Surface::Memory,
                payload: serde_json::json!({ "op": "add_note", "title": "t" }),
            },
        });
        st.apply(&EventEnvelope {
            seq: 2,
            ts: 101,
            by: "peer-2".to_string(),
            body: WorkspaceEvent::Declined {
                id: molt_core::ProposalId(1),
                by: "peer-2".to_string(),
            },
        });
        // the local send path refuses…
        assert!(matches!(
            st.cmd_chat(
                "too late".to_string(),
                None,
                molt_core::ChannelRef::Patch {
                    id: molt_core::ProposalId(1)
                },
            ),
            Err(molt_core::MoltError::DiscussionClosed(
                molt_core::ProposalId(1),
                molt_core::ProposalState::Rejected,
            ))
        ));
        // …but the same message arriving over the wire lands in the log
        let msg = ChatMessage::text(id(7), "peer-1", "was in flight", 102).with_channel(
            molt_core::ChannelRef::Patch {
                id: molt_core::ProposalId(1),
            },
        );
        st.cmd_net_delivered(
            "peer-1".to_string(),
            EventEnvelope {
                seq: 1,
                ts: 102,
                by: "peer-1".to_string(),
                body: WorkspaceEvent::Chat(msg),
            },
            None,
        )
        .expect("a wire delivery never errors");
        assert_eq!(st.chat.len(), 1, "the wire message landed");
        assert_eq!(st.chat[0].body, "was in flight");
    }

    fn id(n: usize) -> MessageId {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(u64::try_from(n).expect("small")).to_le_bytes());
        MessageId(b)
    }

    // --- real presence: numeric stamps, aging, the activity trio -----------

    use molt_core::MemberInfo;

    /// A base instant for the presence tests, far from the thresholds.
    const T: u64 = 1_750_000_000;

    /// A state with an active workspace entry and a real 2-of-3 roster —
    /// ada is the local member; nobody has been seen yet.
    fn presence_fixture() -> crate::State {
        let mut st = crate::tests::plain_state();
        st.clock_override = Some(T);
        let roster: Vec<String> =
            vec!["ada".to_string(), "bob".to_string(), "cid".to_string()];
        st.replica = Some(crate::ReplicaState {
            member: "ada".to_string(),
            roster: roster.clone(),
            rule_m: 2,
            ..Default::default()
        });
        let id = "w-presence".to_string();
        st.session.active_workspace = id.clone();
        st.session.workspaces.push(molt_core::WorkspaceInfo {
            id,
            name: "Presence".to_string(),
            detail: "2-of-3".to_string(),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3: false,
            size_kib: 0,
            last_backup_min: molt_core::WorkspaceInfo::NEVER,
            backup_copies: 0,
            backup_error: String::new(),
            seed: String::new(),
            net: "none".to_string(),
            encrypted: false,
            members: molt_core::roster_members(&roster, T, |_| MemberInfo::NEVER),
            agenda: String::new(),
        });
        st
    }

    fn pill(st: &crate::State, name: &str) -> MemberInfo {
        st.session
            .workspaces
            .iter()
            .find(|w| w.id == st.session.active_workspace)
            .expect("active entry")
            .members
            .iter()
            .find(|m| m.name == name)
            .expect("member pill")
            .clone()
    }

    /// A peer sighting stamps the member with the engine clock's REAL unix
    /// time, and the activity trio counts it in every window it falls into
    /// (ada, the local member, always counts — it is the one reading).
    #[test]
    fn a_peer_sighting_stamps_the_real_clock_and_feeds_the_trio() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        let bob = pill(&st, "bob");
        assert_eq!(bob.last_seen, T, "the stamp is the engine clock's time");
        assert_eq!(bob.state, 0, "a fresh sighting is online");
        let s = st.status();
        assert_eq!((s.active_1h, s.active_24h, s.active_7d), (2, 2, 2));
        // two hours of silence: bob leaves the 1h window by pure clock
        // advance — no event needed, the trio reads the stamps
        st.clock_override = Some(T + 7_200);
        let s = st.status();
        assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 2, 2));
        // eight days of silence: bob leaves every window
        st.clock_override = Some(T + 8 * 86_400);
        let s = st.status();
        assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 1, 1));
    }

    /// This node never hears itself on the wire, so its own stamp would
    /// age out — but it is the one running the app: self stays online
    /// through every aging pass and read, and always counts in the trio.
    #[test]
    fn the_local_member_stays_online_through_aging() {
        let mut st = presence_fixture(); // ada is the local member
        // long after every threshold, with no traffic at all
        st.clock_override = Some(T + 30 * 86_400);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "ada").state, 0, "self never ages offline");
        let ada = st
            .members_view()
            .into_iter()
            .find(|m| m.member == "ada")
            .expect("ada row");
        assert_eq!(ada.presence, 0, "the Members table shows self online");
        let s = st.status();
        assert_eq!(s.active_1h, 1, "self always counts active");
    }

    /// A send-failure pin is scoped to the workspace: closing/resetting the
    /// workspace drops it, so a same-named member in the next workspace is
    /// not falsely shown unreachable.
    #[test]
    fn a_send_failure_pin_does_not_leak_past_a_workspace_reset() {
        let mut st = presence_fixture();
        st.cmd_net_send_failed("bob".to_string(), "gone".to_string(), None)
            .expect("ack");
        assert!(st.net_unreachable.contains("bob"));
        st.reset_workspace_state();
        assert!(
            st.net_unreachable.is_empty(),
            "the close/switch boundary clears the pins"
        );
    }

    /// The presence ticker ages a silent member's pill: online → stale
    /// after `ONLINE_SECS`, stale → offline after `STALE_SECS` — the stamp
    /// itself never moves without real traffic.
    #[test]
    fn the_ticker_ages_a_silent_pill_stale_then_offline() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        st.clock_override = Some(T + MemberInfo::ONLINE_SECS + 1);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 1, "silence past ONLINE_SECS is stale");
        st.clock_override = Some(T + MemberInfo::STALE_SECS + 1);
        st.cmd_net_presence_tick().expect("tick");
        let bob = pill(&st, "bob");
        assert_eq!(bob.state, 2, "silence past STALE_SECS is offline");
        assert_eq!(bob.last_seen, T, "aging never invents a sighting");
    }

    /// A member the transport never heard from stays honestly never-seen:
    /// sentinel stamp, offline pill, counted in NO activity window — and
    /// the ticker does not invent presence for it.
    #[test]
    fn a_member_without_traffic_stays_never_seen_and_counts_nowhere() {
        let mut st = presence_fixture();
        st.cmd_net_presence_tick().expect("tick");
        let cid = pill(&st, "cid");
        assert_eq!(cid.last_seen, MemberInfo::NEVER);
        assert_eq!(cid.state, 2);
        let view = st
            .members_view()
            .into_iter()
            .find(|m| m.member == "cid")
            .expect("cid row");
        assert_eq!(view.last_seen, MemberInfo::NEVER);
        assert_eq!(view.presence, 2);
        let s = st.status();
        // only ada (the local member) is active anywhere
        assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 1, 1));
    }

    /// A send-failure pins the member unreachable (state 2) WITHOUT
    /// touching its last-seen stamp — the stamp records real sightings
    /// only — and the pin outlives the ticker until the next sighting.
    #[test]
    fn a_send_failure_pins_unreachable_until_the_next_sighting() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        st.cmd_net_send_failed("bob".to_string(), "queue gone".to_string(), None)
            .expect("ack");
        let bob = pill(&st, "bob");
        assert_eq!(bob.state, 2, "failing sends mark the member unreachable");
        assert_eq!(bob.last_seen, T, "a failure is not a sighting");
        // the ticker must not lift the pin while the stamp is still fresh
        st.clock_override = Some(T + 10);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 2, "unreachable is sticky");
        assert_eq!(
            st.members_view()
                .into_iter()
                .find(|m| m.member == "bob")
                .expect("bob row")
                .presence,
            2,
            "reads see the pin too"
        );
        // real inbound traffic lifts it
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        let bob = pill(&st, "bob");
        assert_eq!(bob.state, 0);
        assert_eq!(bob.last_seen, T + 10);
    }

    /// Upload availability ("sharer online?") derives from the same real
    /// stamps: a never-seen sharer is offline, a sighting flips it.
    #[test]
    fn upload_availability_follows_the_real_stamps() {
        fn cid_online(st: &crate::State) -> bool {
            st.uploads_view()
                .into_iter()
                .find(|u| u.member == "cid")
                .expect("cid share")
                .online
        }
        let mut st = presence_fixture();
        // the share's ts must sit inside the retention window, which is
        // measured on the REAL clock (chat visibility is not presence)
        let ts = crate::now_secs();
        let mut msg = ChatMessage::text(id(9), "cid", "", ts);
        msg.file = Some(molt_core::FileMeta {
            name: "notes.pdf".to_string(),
            size: 10,
            kind: "PDF".to_string(),
            modified: ts,
            available: true,
            checksum: String::new(),
        });
        st.apply(&EventEnvelope {
            seq: 1,
            ts,
            by: "cid".to_string(),
            body: WorkspaceEvent::Chat(msg),
        });
        assert!(!cid_online(&st), "a never-seen sharer is honestly offline");
        st.cmd_net_peer_seen("cid".to_string(), None).expect("ack");
        assert!(cid_online(&st), "a sighting makes the sharer reachable");
    }

    fn react(by: &str) -> PendingRef {
        PendingRef::React {
            by: by.to_string(),
            emoji: "🎉".to_string(),
            op: Some(molt_core::ReactOp::Add),
        }
    }

    /// Cap overflow evicts the OLDEST parked target (FIFO), and a drained
    /// target frees its slot so the next new target evicts nothing.
    #[test]
    fn park_eviction_is_fifo_and_a_drain_frees_the_slot() {
        let mut p = ParkedRefs::new();
        for n in 0..PARKED_TARGET_CAP {
            p.park(id(n), react("ada"));
        }
        assert_eq!(p.targets(), PARKED_TARGET_CAP);
        assert!(p.holds(&id(0)));

        // one over the cap: the oldest target (0) goes, the newest stays
        p.park(id(PARKED_TARGET_CAP), react("ben"));
        assert_eq!(p.targets(), PARKED_TARGET_CAP);
        assert!(!p.holds(&id(0)), "the OLDEST target is evicted first");
        assert!(p.holds(&id(1)));
        assert!(p.holds(&id(PARKED_TARGET_CAP)));

        // draining a target frees its slot: the next new target fits
        // without evicting anything
        assert_eq!(p.drain(&id(5)), vec![react("ada")]);
        assert!(!p.holds(&id(5)), "drained refs are gone");
        assert!(p.drain(&id(5)).is_empty(), "a second drain finds nothing");
        p.park(id(PARKED_TARGET_CAP + 1), react("chi"));
        assert_eq!(p.targets(), PARKED_TARGET_CAP);
        assert!(p.holds(&id(1)), "no eviction after a drain freed a slot");

        // several refs under one target keep their arrival order
        let mut q = ParkedRefs::new();
        q.park(id(7), react("ada"));
        q.park(id(7), PendingRef::Delete { by: "ben".to_string() });
        assert_eq!(
            q.drain(&id(7)),
            vec![react("ada"), PendingRef::Delete { by: "ben".to_string() }]
        );
    }
}
