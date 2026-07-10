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
//! **The demo mesh.** On a session-only context (no persisted workspace
//! open) the roster's other members run as real loopback peers: each has
//! its own engine instance and transport endpoint, plus a small "brain"
//! that answers the local member's chat with a canned line — through its
//! own engine and outbox, so a demo reply exercises the same code path a
//! real member's message will. Persisted workspaces get no demo peers:
//! their seats are real (and empty until the T2 join flow fills them) —
//! a fake member recorded in a real log would replay forever.

use molt_core::{
    fnv1a64, mockrand, Command, EventEnvelope, GroupConfig, MemberId, MoltError, Reply,
    SessionScope, SessionView, WorkspaceEvent, WorkspaceId,
};
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
            | WorkspaceEvent::MlsCommit { .. }
            | WorkspaceEvent::MeshAnnounced { .. }
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

    /// Make sure the demo mesh matches the current context. It runs for a
    /// session-only context (boot demo group) AND for a persisted
    /// workspace whose members are simulations (founded before the real
    /// network exists — `prefs.simulated_members`). A persisted workspace
    /// with real members gets no fakes.
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

    /// Whether this context should run simulated peer members: nothing
    /// open (boot demo), or an open workspace explicitly flagged as
    /// simulated in its prefs.
    fn wants_demo_mesh(&self) -> bool {
        match &self.active {
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
    fn net_scope_current(&self, scope: Option<u64>) -> bool {
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
                // invariant.
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
                if self.chat_pos.contains_key(&msg.id) {
                    tracing::debug!(%from, id = %msg.id, "dropping a wire chat message with a duplicate id");
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
                // P6 seam: parked reactions/deletes/file-removes targeting
                // `id` would drain here once the parking buffer exists (it
                // needs a State field in lib.rs, which B1 does not own —
                // see the B1 report; until then an early-arriving ref is
                // dropped in its own arm below)
            }
            // chat-bus B1, the P5 receive-side matrix: the id-addressed chat
            // verbs. Defense in depth mirrors the Chat arm — the acting
            // member (`by`) is ALWAYS the authenticated link identity, the
            // target resolves by stable id only (a sender-local index never
            // transfers, same posture as the legacy quote), and the recorded
            // event writes the LOCAL position into the legacy `index` field
            // for older readers.
            WorkspaceEvent::ChatReacted { id, emoji, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire reaction without a message id");
                    return Ok(Reply::Ack);
                };
                // the local-send sanity check (cmd_react_chat's twin)
                let emoji = emoji.trim().to_string();
                if emoji.is_empty() || emoji.chars().count() > 4 {
                    tracing::warn!(%from, "dropping a wire reaction with a malformed emoji");
                    return Ok(Reply::Ack);
                }
                let Some(index) = self
                    .chat_pos
                    .get(&id)
                    .and_then(|p| u64::try_from(*p).ok())
                else {
                    // unknown target: P6 says PARK until the message
                    // arrives — parking is not wired yet (needs a State
                    // field; see the B1 report), so an early reaction is
                    // dropped for now
                    tracing::warn!(%from, %id, "a wire reaction arrived before its message — dropped (parking pending)");
                    return Ok(Reply::Ack);
                };
                let env = self.make_env(
                    from.clone(),
                    WorkspaceEvent::ChatReacted {
                        index,
                        id: Some(id),
                        emoji: emoji.clone(),
                        by: from.clone(),
                    },
                );
                self.record(env);
                self.emit(molt_core::Event::Reacted { id, emoji, by: from });
            }
            WorkspaceEvent::ChatDeleted { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire delete without a message id");
                    return Ok(Reply::Ack);
                };
                let target = self.chat_pos.get(&id).and_then(|p| self.chat.get(*p));
                let Some((index, author)) = self
                    .chat_pos
                    .get(&id)
                    .and_then(|p| u64::try_from(*p).ok())
                    .zip(target.map(|m| m.from.clone()))
                else {
                    // unknown target: P6 parking pending (see the Chat arm)
                    tracing::warn!(%from, %id, "a wire delete arrived before its message — dropped (parking pending)");
                    return Ok(Reply::Ack);
                };
                // no moderation concept: only the author wipes its own
                // message — and the author is what OUR log says, never a
                // claim in the event
                if author != from {
                    tracing::warn!(%from, %id, "dropping a wire delete from a non-author");
                    return Ok(Reply::Ack);
                }
                let env = self.make_env(
                    from.clone(),
                    WorkspaceEvent::ChatDeleted {
                        index,
                        id: Some(id),
                        by: from.clone(),
                    },
                );
                self.record(env);
                self.emit(molt_core::Event::Deleted { id, by: from });
            }
            WorkspaceEvent::FileRemoved { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire file-removal without a message id");
                    return Ok(Reply::Ack);
                };
                let target = self.chat_pos.get(&id).and_then(|p| self.chat.get(*p));
                let Some((index, msg)) = self
                    .chat_pos
                    .get(&id)
                    .and_then(|p| u64::try_from(*p).ok())
                    .zip(target)
                else {
                    // unknown target: P6 parking pending (see the Chat arm)
                    tracing::warn!(%from, %id, "a wire file-removal arrived before its message — dropped (parking pending)");
                    return Ok(Reply::Ack);
                };
                // only the sharer (the share message's author in OUR log)
                // may flip its own share to unavailable
                if msg.from != from || msg.file.is_none() {
                    tracing::warn!(%from, %id, "dropping a wire file-removal from a non-sharer");
                    return Ok(Reply::Ack);
                }
                let env = self.make_env(
                    from.clone(),
                    WorkspaceEvent::FileRemoved {
                        index,
                        id: Some(id),
                        by: from.clone(),
                    },
                );
                self.record(env);
                self.emit(molt_core::Event::FileRemoved { id, by: from });
            }
            // chain governance gossip + block broadcast — only a chain-governed
            // workspace acts on it (the transport carries it; the chain decides)
            WorkspaceEvent::Proposed { id, surface, payload } if self.is_chain_governed() => {
                self.receive_proposed(id.0, surface, payload);
                self.emit(molt_core::Event::Proposed { id, surface });
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
            }
            WorkspaceEvent::MembershipProposed {
                id,
                op,
                member,
                identity_pk,
            } if self.is_chain_governed() => {
                self.receive_membership_proposal(id.0, op, &member, &identity_pk);
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
            other => {
                tracing::debug!(%from, kind = ?std::mem::discriminant(&other), "event over the wire not acted on here");
            }
        }
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
    /// link. Rejections are surfaced to the operator; nothing is torn down.
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
        // the recovery queue is minted on the RUNTIME transport (a clone shares
        // its Arc, so this node can both create the queue and subscribe to it)
        let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) else {
            return Err(MoltError::Recover(
                "the republic's mesh is not running — cannot mint a recovery queue".to_string(),
            ));
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
        tracing::info!(%member, %link, "recovery link ready");
        self.session.notice = format!("recovery-link:{link}");
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

    /// Passive presence: mark the member's pill live.
    pub(crate) fn cmd_net_peer_seen(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.update_member_pill(&member, 0, "just now");
        }
        Ok(Reply::Ack)
    }

    /// Transport trouble: mark the member's pill unreachable.
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
        self.update_member_pill(&member, 2, "unreachable");
        Ok(Reply::Ack)
    }

    /// Update one member's presence pill in the active workspace entry;
    /// emits only when something actually changed.
    ///
    /// Honest limitation (T5 closes it): `MemberInfo.last` is a prose
    /// label, so "just now" cannot age while a member stays silent —
    /// proper staleness needs a numeric last-seen field rendered UI-side
    /// (the `last_sync_min` pattern), which lands with the real transport
    /// health wiring.
    fn update_member_pill(&mut self, member: &MemberId, state: u8, label: &str) {
        let active = self.session.active_workspace.clone();
        let Some(entry) = self.session.workspaces.iter_mut().find(|w| w.id == active) else {
            return;
        };
        let Some(m) = entry.members.iter_mut().find(|m| m.name == *member) else {
            return;
        };
        if m.state == state && m.last == label {
            return;
        }
        m.state = state;
        m.last = label.to_string();
        self.emit_session(SessionScope::Full);
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
