// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine ↔ `molt-net` glue (transport concept §2).
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
//! The loopback demo mesh that replaced the reply simulator (test seam
//! only) lives in [`demo_mesh`].

use molt_core::{
    Command, EventEnvelope, GroupConfig, MemberId, MessageId, MoltError,
    Reply, SessionScope, SessionView, WorkspaceEvent, WorkspaceId,
};
use std::sync::{Arc, Mutex};

use molt_net::supervisor::{self, EngineSink, MemLog, MemStateStore, MlsChannel, NetConfig, PeerLink};
use molt_net::{LoopbackHub, NetError, SupervisorHandle, Transport};
use tokio::sync::{mpsc, oneshot, watch};

use crate::chat::PendingRef;
use crate::{Envelope, State};

mod delivery;
#[cfg(test)]
pub(crate) use delivery::ORDERED_PARK_GIVEUP_SECS;
use delivery::ACK_DEBOUNCE_SECS;
mod demo_mesh;
mod ingest;
#[cfg(test)]
pub(crate) use ingest::{CHAIN_SERVE_DEBOUNCE_SECS, PARKED_READS_PER_FRAME};

#[cfg(test)]
mod demo_mesh_tests;

/// Minimum seconds between accepted mesh (re-)announces per member — each
/// costs every peer a supervisor teardown+rebuild+fsync (see
/// `State::spawn_mesh_extension`).
const MESH_EXTENSION_COOLDOWN_SECS: u64 = 60;

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

    async fn poked(&self, member: &MemberId, to: &MemberId) {
        let _ = self
            .execute(Command::NetPoked {
                from: member.clone(),
                to: to.clone(),
                generation: self.generation,
            })
            .await;
    }

    async fn rekeyed(&self, member: &MemberId) {
        let _ = self
            .execute(Command::NetPeerRekeyed {
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

    async fn link_up(&self, member: &MemberId) {
        let _ = self
            .execute(Command::NetLinkUp {
                member: member.clone(),
                generation: self.generation,
            })
            .await;
    }

    async fn link_down(&self, member: &MemberId, reason: &str) {
        let _ = self
            .execute(Command::NetLinkDown {
                member: member.clone(),
                reason: reason.to_string(),
                generation: self.generation,
            })
            .await;
    }

    async fn send_ok(&self, member: &MemberId) {
        let _ = self
            .execute(Command::NetSendOk {
                member: member.clone(),
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

    // load → apply → save under the handle's gate: the three stores a
    // workspace's runtime builds (outbox, inbox, file plane) all clone one
    // handle, so they serialize against each other
    async fn update<F>(&self, f: F)
    where
        F: FnOnce(&mut molt_core::TransportState) -> bool + Send,
    {
        let gate = self.handle.transport_gate();
        let _held = gate.lock().await;
        let mut state = self.handle.load_transport_state().await;
        if f(&mut state) {
            self.handle.save_transport_state(state);
        }
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
            | WorkspaceEvent::ChatRead { .. }
            | WorkspaceEvent::FileRemoved { .. }
            | WorkspaceEvent::Proposed { .. }
            | WorkspaceEvent::Approved { .. }
            // a decline is a vote too: without it on the wire the decliner's
            // replicas would show Rejected while everyone else keeps the
            // proposal pending forever — votes must converge like approvals
            | WorkspaceEvent::Declined { .. }
            | WorkspaceEvent::Withdrawn { .. }
            | WorkspaceEvent::Committed(_)
            | WorkspaceEvent::ChainRequest { .. }
            | WorkspaceEvent::MembershipProposed { .. }
            | WorkspaceEvent::CheckpointProposed { .. }
            | WorkspaceEvent::CheckpointServed { .. }
            | WorkspaceEvent::MlsCommit { .. }
            | WorkspaceEvent::MeshAnnounced { .. }
            | WorkspaceEvent::FileRequested { .. }
            | WorkspaceEvent::FileWanted { .. }
            | WorkspaceEvent::FileServed { .. }
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
type RealCrypto = (molt_net::LoopbackTransport, Arc<Mutex<molt_net::MlsMember>>);

/// The MLS re-key a coordinator produces on recovery: `(commit, welcome)` or a
/// failure reason (`None` from the caller means there was no runtime group).
type MlsRekey = Result<(Vec<u8>, Vec<u8>), String>;

/// What a clean close persists: `(MLS snapshot, transport queue-credential bytes)`.
type CloseCrypto = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The relay file plane's working set: `(channel, secrets-to-OPEN,
/// current-secret-to-SEAL)` — the seal half is `None` when the current
/// epoch's exporter is unavailable (a serve must then refuse, never fall
/// back to a stale ring secret).
type FilePlaneContext = (
    molt_net::ritual_net::GroupChannel,
    Vec<[u8; 32]>,
    Option<[u8; 32]>,
);

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
    /// both create the queue and later subscribe to it (a queue's receive
    /// credential lives in the creating transport's state — a fresh transport
    /// could send but never receive). `None` for the demo mesh (no real transport).
    pub(crate) fn runtime_transport(&self) -> Option<molt_net::LoopbackTransport> {
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
        // NO_CARRIER_STAMP on BOTH sides: the loopback mesh carries no
        // per-event timestamp, and a locally-read clock here against a 0 on
        // the receive side would make our own commit always lose the
        // tiebreak (review finding 2026-07-31). N4's Nostr carrier supplies
        // the real `created_at` to both ends.
        Some(
            group
                .restore_member(member, key_package, molt_net::mls::NO_CARRIER_STAMP)
                .map_err(|e| e.to_string()),
        )
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
        if let Some(task) = self.recovery.seat_inbox.take() {
            task.abort();
        }
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
        // flush a dirty accept window FIRST: the writer processes messages in
        // order, and the merge below SEALS `transport.state` against later
        // saves — enqueued before it, this one still lands (§4.7)
        if self.delivery.accepted_dirty {
            if let Some(active) = self.active.as_ref() {
                active.handle.save_accepted(self.delivery.accepted.clone());
                self.delivery.accepted_dirty = false;
            }
        }
        // the group runtime's ratchet, on the same terms: stop it, then merge
        // durably. Without this the reopen restores the founding blob and the
        // next publish reuses sender generations — replay-rejected and
        // silently lost at every peer.
        if let Some(group) = self.group_net.take() {
            let snap = group.mls.lock().ok().and_then(|g| g.snapshot().ok());
            // signals stop and aborts the inbox; the outbox finishes its
            // in-flight publish and exits on its own
            drop(group);
            if let (Some(active), Some(snap)) = (self.active.as_ref(), snap) {
                if !active.handle.persist_crypto_blocking(Some(snap), None) {
                    tracing::error!("the group ratchet did not reach the disk on close");
                }
            }
        }
        let Some(net) = self.net.take() else {
            return;
        };
        let crypto = net.crypto_for_close();
        drop(net); // stop the supervisor before the durable merge
        if let (Some(active), Some((mls, creds))) = (self.active.as_ref(), crypto) {
            if !active.handle.persist_crypto_blocking(mls, creds) {
                tracing::error!("the group ratchet did not reach the disk on close");
            }
        }
    }

    /// Build the **real** T2 runtime for the open workspace from its persisted
    /// `transport.state`: restore the MLS group, rebuild the full-mesh
    /// [`PeerLink`]s, and spawn a supervisor whose outbox is the encrypted
    /// workspace log ([`StorageLog`]) and whose cursors live in the state file
    /// ([`FileStateStore`]). `transport` must reach the mesh queues — the
    /// still-alive ritual transport (right after founding, and the only option
    /// on the loopback hub, whose queues can't be reconstructed) or the reopen
    /// seam's re-adopted transport. Returns `None` when there is no mesh/group
    /// to run (nothing to build) or the group can't be restored.
    pub(crate) fn build_real_net(
        &mut self,
        transport: molt_net::LoopbackTransport,
        mesh: &[molt_core::MeshLink],
        mls_blob: &[u8],
    ) -> Option<NetRuntime> {
        let mls = molt_net::MlsMember::restore(mls_blob).ok()?;
        // share the group between the supervisor (advances the ratchet) and the
        // engine (snapshots it on a clean close, so a reopen resumes it)
        self.build_real_net_shared(transport, mesh, Arc::new(Mutex::new(mls)))
    }

    /// The relays this node may actually dial for the OPEN republic: what the
    /// group ratified, intersected with this node's own confirmed pool.
    ///
    /// The two are different questions, and publishing to a relay nobody else
    /// reads is the partition §10.15 is about. Empty = this node shares no
    /// relay with its own republic, which every caller must treat as a named
    /// failure rather than as silence.
    pub(crate) fn dialable_group_relays(&self) -> Vec<String> {
        let Some(nostr) = self.nostr.as_ref() else {
            return Vec::new();
        };
        molt_core::relay::diagnose_invite_relays(
            &nostr.relays,
            &self.session.settings.relays,
            self.clearnet_session,
        )
        .iter()
        .filter(|v| v.blocked.is_none())
        .map(|v| v.url.clone())
        .collect()
    }

    /// Bring up the kind-445 GROUP runtime of a Nostr workspace (N5.2).
    ///
    /// The Nostr twin of [`Self::build_real_net`], and it reuses that one's
    /// engine seam verbatim — the same `StorageLog` / `FileStateStore` /
    /// `CmdSink` triple. What differs is everything below the seam: one
    /// broadcast channel instead of n queues, and no `NetRuntime` at all.
    ///
    /// `None` when anything the runtime needs is absent — a Nostr workspace
    /// that cannot dial one of its own relays, or whose MLS group did not
    /// restore, must stay honestly silent rather than half-run.
    pub(crate) fn build_group_net(&mut self, mls_blob: &[u8]) -> Option<crate::GroupNet> {
        let mls = molt_net::MlsMember::restore(mls_blob).ok()?;
        self.build_group_net_shared(std::sync::Arc::new(std::sync::Mutex::new(mls)))
    }

    /// [`Self::build_group_net`] over an **already-live** shared group — the
    /// R6 pool-change rebuild hands the running `Arc` straight through (the
    /// `build_real_net_shared` twin): a late encrypt by the dying outbox
    /// advances the SAME ratchet the new runtime continues from.
    /// Hand the runtime's MLS group the chain's identity table as the ONE
    /// authority on which signature key a leaf for each member may carry:
    /// a re-key commit adding a leaf under any other key is dropped before
    /// the merge (review 2026-08-25). A workspace without a chain sets no
    /// authority — its adds stay unchecked, as before.
    fn arm_mls_roster_authority(&self, mls: &std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>) {
        let Some(head) = self.chain.head.as_ref() else {
            return;
        };
        let keys: std::collections::BTreeMap<String, Vec<u8>> = head
            .identities
            .iter()
            .filter_map(|i| hex::decode(&i.identity_pk).ok().map(|k| (i.member.clone(), k)))
            .collect();
        if keys.len() != head.identities.len() {
            tracing::warn!(armed = false, "an anchored identity key does not decode");
            return;
        }
        if let Ok(mut m) = mls.lock() {
            m.set_roster_keys(keys);
        }
    }

    pub(crate) fn build_group_net_shared(
        &mut self,
        mls_arc: std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>,
    ) -> Option<crate::GroupNet> {
        let relays = self.dialable_group_relays();
        self.arm_mls_roster_authority(&mls_arc);
        let active = self.active.as_ref()?;
        let nostr = self.nostr.as_ref()?;
        let dialer = self.dialer_for().ok()?;
        if relays.is_empty() {
            tracing::warn!("no dialable relay for this republic - the group runtime stays down");
            return None;
        }
        let channel = molt_net::ritual_net::GroupChannel::new(
            dialer,
            relays,
            nostr.rotation_seed,
        );
        let owner = self.member();
        let others: Vec<MemberId> = self
            .roster()
            .into_iter()
            .filter(|m| *m != owner)
            .collect();
        let feed = StorageLog::new(active.handle.clone());
        let store = FileStateStore::new(active.handle.clone());
        let (wakeup, wakeup_rx) = watch::channel(0u64);
        let (health_tx, health) = watch::channel(molt_net::group_runtime::GroupHealth::default());
        let handle = molt_net::group_runtime::spawn_group(
            channel,
            molt_net::supervisor::MlsChannel::from_shared(mls_arc.clone()),
            molt_net::group_runtime::GroupNetConfig::new(owner, others),
            feed,
            store,
            // `generation: None`: the generation gate requires `self.net` to be
            // Some, and a Nostr workspace builds no NetRuntime at all
            CmdSink { tx: self.cmd_tx.clone(), generation: None },
            wakeup_rx,
            health_tx,
        );
        Some(crate::GroupNet { handle, mls: mls_arc, wakeup, health })
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
        transport: molt_net::LoopbackTransport,
        mesh: &[molt_core::MeshLink],
        mls_arc: Arc<Mutex<molt_net::MlsMember>>,
    ) -> Option<NetRuntime> {
        self.arm_mls_roster_authority(&mls_arc);
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
        // honest open: every mesh leg starts "connecting" (amber) until its
        // watchdog confirms the subscription with a link_up — from here on,
        // `Ok` means every INBOUND leg is live (outbound trouble surfaces on
        // the first send attempt via NetSendFailed; the transport has no
        // passive outbound probe). A stuck-send flag survives a same-workspace mesh
        // REBUILD (extension) for members still in the mesh — clearing it
        // would launder a genuinely dead outbound leg back to green; only a
        // successful send (NetSendOk) clears it.
        self.delivery.link_down.clear();
        self.delivery.send_stuck.retain(|m, _| peer_names.contains(m));
        for p in &peer_names {
            self.delivery.link_down.insert(p.clone(), "connecting".to_string());
        }
        self.recompute_net_health();
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
    pub(crate) fn net_generation_current(&self, generation: Option<u64>) -> bool {
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

    /// The operative per-file cap (`file_transfer_nostr.md` §5.1):
    /// `None` = file sharing is OFF (`file_cap_bytes = 0`, FP4 2026-08-16),
    /// otherwise the configured value (an absent config key serde-defaults
    /// to 4 MiB before it ever reaches here).
    pub(crate) fn effective_file_cap(&self) -> Option<u64> {
        let cap = self.session.settings.file_cap_bytes;
        (cap != 0).then_some(cap)
    }

    /// The RELAY file plane's channel + exporter material, if this
    /// workspace can carry one (`file_transfer_nostr.md`): the same dial
    /// list and rotation seed the group runtime uses, plus the MLS
    /// exporter ring (open) and its head (seal).
    fn nostr_file_context(&self) -> Option<FilePlaneContext> {
        let nostr = self.nostr.as_ref()?;
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return None;
        }
        let dialer = self.dialer_for().ok()?;
        let channel =
            molt_net::ritual_net::GroupChannel::new(dialer, relays, nostr.rotation_seed);
        // the CURRENT epoch's secret leads (it seals new series), the ring
        // follows (it opens series sealed before a re-key) — a fresh seat's
        // ring is empty until the first rotation, so the current secret is
        // what makes the plane work at all. The current secret travels
        // SEPARATELY too: a serve must refuse when it is unavailable rather
        // than seal a fresh series under a stale ring epoch nobody past the
        // ring horizon could open (review 2026-08-10).
        let (ring, current) = {
            let g = self.group_net.as_ref()?;
            let m = g.mls.lock().ok()?;
            (m.exporter_ring().to_vec(), m.exporter_secret().ok())
        };
        let mut secrets: Vec<[u8; 32]> = Vec::with_capacity(ring.len() + 1);
        if let Some(c) = current {
            secrets.push(c);
        }
        for s in ring {
            if !secrets.contains(&s) {
                secrets.push(s);
            }
        }
        if secrets.is_empty() {
            return None;
        }
        Some((channel, secrets, current))
    }

    /// Download a peer's share over the relay plane: fetch when the
    /// series' publish stamp is known, else park the download and ask the
    /// sharer to publish (lazy) — the `FileServed` announcement resumes it.
    pub(crate) fn nostr_download(
        &mut self,
        id: MessageId,
        target: crate::transfer::FetchTarget,
        dest: crate::transfer::DestSpec,
    ) {
        if let Some(at) = self.files.series.get(&id).copied() {
            self.spawn_nostr_fetch(id, at, target, dest);
        } else {
            self.files.pending.insert(id, (target, dest));
            let me = self.member();
            let env = self.make_env(me, WorkspaceEvent::FileWanted { id });
            self.record(env);
            // a parked download must not wait forever: if no FileServed
            // drains it within the window, it fails honestly and the
            // operator can retry (review 2026-08-10 — the park had no
            // timeout and the phase guard blocked every retry)
            if let Some(cmd_tx) = self.cmd_tx.upgrade() {
                crate::transfer::spawn_want_timeout(id, self.net_scope, cmd_tx);
            }
        }
    }

    /// The parked download's watchdog fired: if the `FileServed` answer
    /// never came (the id still parks), fail the download honestly — a
    /// drained park means the fetch is running and the watchdog is stale.
    pub(crate) fn cmd_net_file_wanted_timeout(
        &mut self,
        id: MessageId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        if self.files.pending.remove(&id).is_some() {
            self.set_download_phase(
                id,
                molt_core::TransferPhase::Failed {
                    reason: "the sharer did not answer".to_string(),
                },
            );
        }
        Ok(Reply::Ack)
    }

    /// Spawn the off-actor series fetch (reports back over the same
    /// `NetFileDone`/`NetFileFailed` path the queue-plane download uses).
    fn spawn_nostr_fetch(
        &mut self,
        id: MessageId,
        at: u64,
        target: crate::transfer::FetchTarget,
        dest: crate::transfer::DestSpec,
    ) {
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        let Some((channel, ring, _)) = self.nostr_file_context() else {
            crate::transfer::spawn_file_verdict(
                id,
                Err("no dialable relay or group ring for the file plane".to_string()),
                self.net_scope,
                cmd_tx,
            );
            return;
        };
        let fetch = crate::transfer::spawn_nostr_fetch(
            channel,
            ring,
            id,
            at,
            target,
            dest,
            // sharing-off still allows PULLING a peer's share; the default
            // bounds what a hostile series claim may allocate
            self.effective_file_cap()
                .unwrap_or_else(molt_core::default_file_cap_bytes),
            self.net_scope,
            cmd_tx,
        );
        self.files.fetches.retain(|h| !h.is_finished());
        self.files.fetches.push(fetch);
    }

    /// A `FileWanted` broadcast landed: ONLY the sharer answers (every
    /// member receives it), by lazily publishing the chunk series — or by
    /// re-announcing a fresh enough stamp, so a burst of requests does not
    /// publish the series N times.
    fn serve_file_wanted(&mut self, id: MessageId) {
        let me = self.member();
        let (is_mine, ts, size) = match self.chat_by_id(&id) {
            Ok((_, msg)) => (
                msg.from == me && msg.file.as_ref().is_some_and(|f| f.available),
                msg.ts,
                msg.file.as_ref().map_or(0, |f| f.size),
            ),
            Err(_) => return,
        };
        if !is_mine || self.chat_ts_aged_out(ts) || self.files.serving.contains(&id) {
            return;
        }
        // the size is known here — an over-cap share must not cost a full
        // disk read per request only to be refused inside the publish
        // (review 2026-08-10; the share-time gate makes this an edge)
        let Some(cap) = self.effective_file_cap() else {
            tracing::warn!(%id, "not serving: file sharing off (file_cap_bytes=0)");
            return;
        };
        if size > cap {
            tracing::warn!(%id, size, "not serving a share beyond the file cap");
            return;
        }
        let now = crate::now_secs();
        if let Some(at) = self.files.series.get(&id).copied() {
            // a standing series re-announces instead of re-publishing (one
            // stored copy serves everyone within relay retention) — UNLESS
            // this requester evidently just saw the stamp and still asks
            // again: then the series is unfetchable for it (pruned, or
            // sealed under an epoch it cannot open) and only a FRESH
            // publish under the current secret converges (review 2026-08-10)
            let recently_announced = self
                .files.announced
                .get(&id)
                .is_some_and(|t| now.saturating_sub(*t) < 300);
            if now.saturating_sub(at) < 86_400 && !recently_announced {
                self.files.announced.insert(id, now);
                let env = self.make_env(me, WorkspaceEvent::FileServed { id, at });
                self.record(env);
                return;
            }
        }
        let Some(path) = self.files.share_paths.get(&id).cloned() else {
            return;
        };
        let Some((channel, _, current)) = self.nostr_file_context() else {
            return;
        };
        // a fresh series seals under the CURRENT epoch only — publishing
        // under a stale ring secret would hand out a series fresh seats
        // and post-re-key members can never open
        let Some(exporter) = current else {
            tracing::warn!(%id, "no current exporter secret - not publishing the series");
            return;
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        // §5.4: the publish is metered on the SAME persisted hourly budget
        // the resend rounds draw from — the store is how the consumption
        // lands in transport.state
        let Some(store) = self
            .active
            .as_ref()
            .map(|a| crate::net::FileStateStore::new(a.handle.clone()))
        else {
            return;
        };
        self.files.serving.insert(id);
        crate::transfer::spawn_series_publish(
            channel,
            exporter,
            id,
            path,
            cap,
            store,
            self.net_scope,
            cmd_tx,
        );
    }

    /// The off-actor series publish reported back: clear the in-flight
    /// mark and, on success, announce the stamp to the group (that
    /// announcement is what resumes the requesters' parked fetches).
    pub(crate) fn cmd_net_file_series_published(
        &mut self,
        id: MessageId,
        at: u64,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        self.files.serving.remove(&id);
        if at == 0 {
            return Ok(Reply::Ack); // the publish failed — honest silence, the
                                   // requester's park runs into its watchdog
        }
        self.files.series.insert(id, at);
        self.files.announced.insert(id, crate::now_secs());
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::FileServed { id, at });
        self.record(env);
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
            refuse("the sharer removed the file - no longer available");
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
        let Some(path) = self.files.share_paths.get(&id).cloned() else {
            refuse("this node no longer knows the shared file's local path");
            return;
        };
        crate::transfer::spawn_file_serve(
            transport,
            path,
            size,
            req.id,
            req.reply,
            self.files.serve_slots.clone(),
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
    ///
    /// Nostr third anchor: this IS the choke point. `RecoverRequest` carries
    /// the rejoiner's NEW anchor (N4b step 1), and it is canonicalized,
    /// checked for cross-seat collision and proven-possessed HERE — before
    /// the ticket is spent and before it can reach a `Restored` block —
    /// exactly like `cmd_net_join_requested`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_net_recover_requested(
        &mut self,
        member: MemberId,
        identity_pk: String,
        key_package: String,
        ticket: String,
        seat_proof: String,
        new_nostr_pk: String,
        relays: Vec<String>,
        consent: String,
        reply: String,
        sender_npub: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        // TWO lanes (`detached_reattach.md` §2.2). Ticketed: the ticket must
        // be a live one this node minted via a recovery link. Self-service:
        // an unknown ticket is a restored seat announcing itself — accepted
        // only on an open chain-governed group, only WITH a consent (it is
        // the authorization), and every failure past here stays a SILENT
        // drop (no refusal frame — an unauthenticated prober gets no oracle).
        // a ticket is bound to the seat it was minted for (review R8): a
        // member holding its own phrase must not spend ANOTHER seat's link
        let ticketed = self
            .recovery.tickets
            .get(&ticket)
            .is_some_and(|minted_for| *minted_for == member);
        if !ticketed {
            if !self.is_chain_governed() || self.group_net.is_none() {
                tracing::debug!(%member, "unsolicited recovery request without an open group - dropped");
                return Ok(Reply::Ack);
            }
            if consent.is_empty() {
                tracing::warn!(%member, "unsolicited recovery request without a consent - dropped");
                return Ok(Reply::Ack);
            }
        }
        // one re-admission at a time, on BOTH lanes (review R3): a pending
        // Restored proposal for this member means another receiver (or an
        // earlier frame of this broadcast) already coordinates — a second,
        // ticketed request would re-key with its KeyPackage while the first
        // block's Welcome goes to a dead anchor, stranding the seat
        if self.chain.proposal_changes.values().any(|c| {
            matches!(c, molt_core::ChainChange::Membership {
                op: molt_core::MembershipOp::Restored,
                member: m,
                ..
            } if m == &member)
        }) {
            tracing::warn!(%member, ticketed, "recovery request while a re-admission is pending - dropped");
            return Ok(Reply::Ack);
        }
        // NB: on a verified request, verify_and_propose_restore registers the
        // pending recovery BEFORE proposing (a lone coordinator commits the
        // block synchronously inside the propose, which consumes that entry)
        // NORMALIZE-OR-REJECT at the choke point, exactly like the founding
        // ingest: an anchor that is not a canonical curve point must never
        // reach a chain block, and it must not spend the ticket either. Empty
        // stays empty (the loopback path has no transport anchor).
        let canonical = if new_nostr_pk.is_empty() {
            String::new()
        } else {
            match molt_net::canonical_nostr_pk(&new_nostr_pk) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(%member, error = %e, "recovery request with a malformed transport anchor - dropped");
                    return Ok(Reply::Ack);
                }
            }
        };
        // …and it must not collide with a seat that already holds it
        if !canonical.is_empty() {
            // the complete register (review C8): founding anchors, every
            // Restored block's anchor and the blob's working anchors
            let taken = self.anchor_seen_in_chain(&canonical);
            if taken {
                tracing::warn!(%member, "recovery request reuses an anchored transport key - dropped");
                return Ok(Reply::Ack);
            }
        }
        // PROOF-OF-POSSESSION (§2.1, the founding twin at `cmd_net_join_requested`):
        // the anchor claimed here must BE the key that sealed the gift wrap.
        // The seat proof already binds it under the identity key, so this does
        // not gate authenticity — it gates DELIVERABILITY: without it a
        // relay-level attacker could re-address the coordinator's Welcome to a
        // key nobody holds and strand the seat.
        //
        // Gated on THIS node's transport kind, which no remote can influence —
        // not on the field being non-empty, which would let a missing proof
        // read as "loopback, nothing to check".
        if self.transport_kind == Some(molt_core::TransportKind::Nostr)
            && (sender_npub.is_empty() || canonical != sender_npub)
        {
            tracing::warn!(
                %member,
                "recovery request claims a transport key it did not sign with - refused (possible impersonation)"
            );
            return Ok(Reply::Ack);
        }
        // self-service cooldown: relays replay 1059 wraps on every
        // resubscribe, and the accept window does not cover them — an
        // accepted (member, anchor) pair is served once per window
        const UNSOLICITED_COOLDOWN_SECS: u64 = 1_800;
        if !ticketed {
            // a request whose "new" anchor ALREADY is the member's working
            // anchor is the LIVE incarnation, replayed by a relay after the
            // cooldown — there is nothing to restore, and re-keying a live
            // seat is pure epoch churn
            if !canonical.is_empty() && self.working_nostr_pk(&member) == canonical {
                tracing::debug!(%member, "unsolicited recovery request for the live anchor - dropped");
                return Ok(Reply::Ack);
            }
            // THE CHAIN IS THE REPLAY REGISTER (field storm 2026-08-24):
            // relays replay every stored request wrap on each resubscribe,
            // and each once-ACCEPTED old request re-keyed the seat onto a
            // DEAD incarnation's anchor — kicking the live one out, forever,
            // in a loop. An anchor that was EVER anchored in this chain
            // (genesis, any Restored block, the checkpoint's summary) can
            // only be a replay: a genuine reattach mints a fresh salt.
            if self.anchor_seen_in_chain(&canonical) {
                tracing::debug!(%member, "unsolicited recovery request replays a chain-known anchor - dropped");
                return Ok(Reply::Ack);
            }
            let now = crate::now_secs();
            let key = (member.to_string(), canonical.clone());
            if self
                .recovery.unsolicited_cooldown
                .get(&key)
                .is_some_and(|t| now.saturating_sub(*t) < UNSOLICITED_COOLDOWN_SECS)
            {
                tracing::debug!(%member, "unsolicited recovery request within the cooldown - dropped");
                return Ok(Reply::Ack);
            }
        }
        match self.verify_and_propose_restore(
            ticketed,
            &member,
            &identity_pk,
            &key_package,
            &ticket,
            &seat_proof,
            &canonical,
            &relays,
            &consent,
            &reply,
        ) {
            Ok(id) => {
                // spend the ticket only on a verified request, so a legitimate
                // member whose first attempt failed (e.g. a truncated proof) can
                // retry on the still-live queue
                if ticketed {
                    self.recovery.tickets.remove(&ticket);
                }
                let now = crate::now_secs();
                self.recovery.unsolicited_cooldown
                    .retain(|_, t| now.saturating_sub(*t) < UNSOLICITED_COOLDOWN_SECS);
                self.recovery.unsolicited_cooldown
                    .insert((member.to_string(), canonical.clone()), now);
                tracing::info!(%member, ticketed, "recovery seat proof verified - proposing re-admission");
                // the first checklist frame: the rejoiner learns the roster,
                // the threshold and the voices already counted
                self.push_recover_progress(id);
            }
            Err(e) if ticketed => {
                // the operator must SEE the refusal (relay-pool mismatch is
                // the common honest cause — R5 names the relay to add); a
                // tracing-only drop left the coordinator staring at a silent
                // screen while the rejoiner waited out its timeout
                self.session.notice = format!("recover-refused:{member}:{e}");
                self.emit_session(SessionScope::Full);
                tracing::warn!(%member, error = %e, "dropping an invalid recovery request");
                // …and so must the REJOINER (WP6, field log 2026-08-23): a
                // wrong phrase looked like a dead coordinator for 15 minutes.
                // Answered only here — behind the ticket + PoP gates — so an
                // unknown ticket stays a silent drop, and the ticket is NOT
                // spent (the same link with the right phrase still works).
                if !sender_npub.is_empty() {
                    self.send_recover_frame(
                        sender_npub.clone(),
                        molt_net::invite::RitualMsg::RecoverRefused {
                            member: member.to_string(),
                            reason: e,
                        },
                    );
                }
            }
            Err(e) => {
                // self-service lane: silent toward the wire (no oracle), one
                // structured line for the operator's log
                tracing::warn!(%member, error = %e, "dropping an invalid unsolicited recovery request");
            }
        }
        Ok(Reply::Ack)
    }

    /// Report a coordinated recovery's vote state to its waiting rejoiner
    /// (`recovery_auto_approval.md` §4): gift-wrap a `RecoverProgress` frame
    /// to the seat's NEW transport anchor. A no-op unless `id` is a pending
    /// recovery this node coordinates on a Nostr republic (the loopback test
    /// transport carries no progress frames). Best-effort display data —
    /// a lost frame costs a stale checklist, never the recovery.
    pub(crate) fn push_recover_progress(&mut self, id: u64) {
        let Some(report) = self.recover_progress_for(id) else {
            return;
        };
        self.send_recover_progress_frame(report);
    }

    /// The send tail shared by [`Self::push_recover_progress`] (live vote
    /// updates) and the sealed block's completion report
    /// (`after_block_applied`).
    pub(crate) fn send_recover_progress_frame(&mut self, report: crate::chain::RecoverProgressReport) {
        let Some(to) = report.to.clone().filter(|t| !t.is_empty()) else {
            return;
        };
        let msg = molt_net::invite::RitualMsg::RecoverProgress {
            member: report.member,
            need: report.need,
            roster: report.roster,
            approved: report.approved,
        };
        self.send_recover_frame(to, msg);
    }

    /// Gift-wrap one recovery-side ritual frame to `to` over the group's
    /// dialable relays — the shared tail of the progress report and the
    /// refusal answer. Nostr only (the loopback test transport carries no
    /// recovery side-channel); best-effort, off the actor.
    fn send_recover_frame(&mut self, to: String, msg: molt_net::invite::RitualMsg) {
        if self.transport_kind != Some(molt_core::TransportKind::Nostr) {
            return;
        }
        let Some(nostr) = self.nostr.as_ref() else {
            return;
        };
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return;
        }
        let Ok(dialer) = self.dialer_for() else {
            return;
        };
        let Ok(net) = molt_net::ritual_net::RitualNet::new(dialer, relays, &nostr.sk) else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = net.send_ritual(&to, &msg).await {
                tracing::debug!(error = %e, "recovery frame did not publish");
            }
        });
    }

    /// Stand the STANDING seat inbox up for the open Nostr workspace
    /// (`detached_reattach.md` §2.1): subscribe this seat's own 1059 anchor
    /// so a restored seat can announce itself without a minted link. Called
    /// wherever the group runtime comes up (open, materialize); replaces a
    /// previous incarnation. A refusal to spawn (no relays, no key) is
    /// quiet — the ticketed link path is unaffected.
    pub(crate) fn spawn_seat_inbox_if_nostr(&mut self) {
        if let Some(task) = self.recovery.seat_inbox.take() {
            task.abort();
        }
        if self.transport_kind != Some(molt_core::TransportKind::Nostr)
            || !self.is_chain_governed()
        {
            return;
        }
        let Some(nostr) = self.nostr.as_ref() else {
            return;
        };
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return;
        }
        let Ok(dialer) = self.dialer_for() else {
            return;
        };
        let Ok(net) = molt_net::ritual_net::RitualNet::new(dialer, relays, &nostr.sk) else {
            return;
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        self.recovery.seat_inbox = Some(crate::nostr_ritual::spawn_seat_inbox(
            net,
            self.net_scope,
            cmd_tx.downgrade(),
        ));
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
        // pool-settled gate (the founding/join twin): the minted link names
        // this node's relay pool — minting while a confirmation probe is in
        // flight hands out a link naming a pool about to change
        if !self.pending_relay_confirms.is_empty() {
            return Err(MoltError::Recover(
                crate::relay_msg::pool_verifying_reason().to_string(),
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
        // N4b step 5: a Nostr republic has no mesh and needs none — the mint
        // wants only a dialer, this seat's transport secret and the group's
        // relays. The discriminator is read FIRST, so a Nostr workspace is
        // never pushed down the queue-shaped path (whose absence of creds is
        // by design, not damage) and never refused with "mesh-not-running".
        if self.transport_kind == Some(molt_core::TransportKind::Nostr) {
            return self.mint_recovery_link_over_relays(member, republic, republic_id);
        }
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
        self.recovery.tickets.insert(ticket.clone(), member.clone());
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
            self.recovery.material_sink.clone(),
        );
        Ok(Reply::Ack)
    }

    /// The Nostr half of [`Self::cmd_recover_invite_start`] (N4b §8.8 step 5).
    ///
    /// The relay set is the group's list INTERSECTED with what this node may
    /// actually dial. Advertising a relay this coordinator cannot reach would
    /// hand the returning member an address nobody is listening on — and
    /// relays do not federate (`relay_pool.md` §2.6), so "the group uses it"
    /// is not the same question as "I am reachable there". Capped like every
    /// other advertised list.
    ///
    /// Every refusal is an operational state of THIS node, not a caller error:
    /// it rides the recovery notice, never a command error.
    fn mint_recovery_link_over_relays(
        &mut self,
        member: MemberId,
        republic: String,
        republic_id: String,
    ) -> Result<Reply, MoltError> {
        let Some(nostr) = self.nostr.as_ref() else {
            // the kind says Nostr but the material did not load — its own
            // fault, not "no mesh"
            return self.cmd_net_recover_link_failed(
                member,
                "no transport key for this seat".to_string(),
                String::new(),
                None,
            );
        };
        let group_relays = nostr.relays.clone();
        let sk = nostr.sk.clone();
        // `dialer_for`, NOT `resolve_dialer`: the latter writes
        // `session.net_health = Ok` on success, and a Nostr workspace sits at
        // `Down { NOSTR_RUNTIME_PENDING }` on purpose until N5 exists.
        // Minting a link would have turned the pill green for the rest of the
        // session — promising a runtime that is not there. Same choice the
        // founding and join paths already make.
        let dialer = match self.dialer_for() {
            Ok(d) => d,
            Err(e) => {
                return self.cmd_net_recover_link_failed(
                    member,
                    format!("transport: {e}"),
                    String::new(),
                    None,
                )
            }
        };
        let verdicts = molt_core::relay::diagnose_invite_relays(
            &group_relays,
            &self.session.settings.relays,
            self.clearnet_session,
        );
        let relays: Vec<String> = verdicts
            .iter()
            .filter(|v| v.blocked.is_none())
            .map(|v| v.url.clone())
            .take(molt_net::welcome::MAX_PAYLOAD_RELAYS)
            .collect();
        if relays.is_empty() {
            // classified from THESE relays' verdicts — "my pool is empty" and
            // "my pool shares nothing with this republic" are different
            // faults with different fixes, and the whole-pool verdict cannot
            // tell them apart
            let reason = crate::relay_msg::republic_relay_reason(&verdicts);
            return self.cmd_net_recover_link_failed(member, reason, String::new(), None);
        }
        let net = match molt_net::ritual_net::RitualNet::new(dialer, relays, &sk) {
            Ok(n) => n,
            Err(e) => {
                return self.cmd_net_recover_link_failed(
                    member,
                    format!("transport keys: {e}"),
                    String::new(),
                    None,
                )
            }
        };
        // the sender is taken BEFORE the ticket is minted: every lane that
        // registers a ticket must go on to either use it or unregister it,
        // and this one cannot do either
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Recover("engine stopped".to_string()));
        };
        let ticket =
            molt_net::invite::mint_ticket().map_err(|e| MoltError::Recover(e.to_string()))?;
        // register BEFORE the inbox can carry a request, so the spend-once
        // guard is armed the moment the returning member answers
        self.recovery.tickets.insert(ticket.clone(), member.clone());
        // ONE inbox per open workspace. Every mint subscribes the same filter
        // on the same anchor (kind 1059, #p = this seat), so a second
        // subscription would duplicate every delivery and add another set of
        // forever-redialing relay supervisors. The actor validates by TICKET,
        // not by which task delivered the request, so one inbox serves every
        // outstanding link.
        for old in self.recovery.inboxes.drain(..) {
            old.abort();
        }
        // the seat's anchored identity pk rides the link (WP7): the rejoiner
        // needs it to resolve the founder-vs-joiner derivation convention
        let anchored_pk = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone())
            .unwrap_or_default();
        let task = crate::nostr_ritual::spawn_recovery_inbox(
            net,
            member,
            ticket,
            republic,
            republic_id,
            anchored_pk,
            // recovery loops are scoped to the open WORKSPACE
            self.net_scope,
            cmd_tx.downgrade(),
        );
        // parked so the close path can abort it — a relay subscription does
        // not end on its own the way a dead queue does
        self.recovery.inboxes.push(task);
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
    /// provisioning task (`Command::NetRecoverLinkFailed`, e.g. the queue
    /// mint failed). Surface the calm `recovery-link-failed:` notice on the
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
            self.recovery.tickets.remove(&ticket);
        }
        tracing::warn!(%member, %reason, "recovery link mint failed");
        self.session.notice = format!("recovery-link-failed:{reason}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A rejoiner's **mesh announce** arrived on the recovery queue (dynamic
    /// mesh membership, `docs_archive/transport/dynamic_mesh.md` ❷): authenticate the
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
            tracing::warn!("a recovery-queue mesh announce did not decrypt - dropped");
            return Ok(Reply::Ack);
        };
        // parse BEFORE spending the one-shot window: a malformed (but
        // authentic) announce must degrade to a dropped frame, not burn the
        // rejoiner's only chance to re-mesh (version skew / client bug)
        let Ok(announce) = serde_json::from_slice::<molt_net::mesh::MeshAnnounce>(&plain) else {
            tracing::warn!(%announcer, "mesh announce is malformed - dropped (window kept)");
            return Ok(Reply::Ack);
        };
        // only the member whose re-key JUST completed may (re)announce here —
        // the recovery queue can never re-point another member's links
        if !self.recovery.mesh_window.remove(&announcer) {
            tracing::warn!(%announcer, "mesh announce outside a recovery window - dropped");
            return Ok(Reply::Ack);
        }
        // E7 review finding 1: the rejoiner's NEW incarnation restarts its
        // log seq space (materialize_workspace), while our accept window for
        // it still holds the OLD device's marks — every fresh envelope would
        // read as already-accepted (set bit or aged) and be silently
        // swallowed AND falsely acked. This authenticated, one-shot recovery
        // announce IS the incarnation boundary: forget the old window.
        self.reset_peer_accept_window(&announcer);
        // relay VERBATIM: each survivor decrypts (and thereby authenticates)
        // the announcer itself, exactly like the founding star relay. A
        // recovery re-announce is single-hop over the live mesh (nonce-less —
        // nonce'd announces are the retired rotate relay and are ignored).
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::MeshAnnounced { ct, nonce: None });
        self.record(env);
        // a recovery re-announce is targeted at every survivor — no queue
        // for us here is a real anomaly, so spawn_mesh_extension warns
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
        let me = self.member();
        // send side FIRST — before the cooldown: an announce that carries no
        // queue for this node must not burn the announcer's cooldown slot, or
        // the follow-up announce that IS for us would bounce off "inside the
        // cooldown" for a full window (delivery_guarantee.md V1 — the live
        // 3-node deaf-leg loop). The cooldown guards the expensive path
        // (queue mint + rebuild), which a no-queue-for-us announce never
        // reaches.
        let (snds, wrap_out) = match molt_net::mesh::send_targets(announce, &me) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%member, reason = %e, "mesh announce carries no usable queue for this node");
                return;
            }
        };
        // per-member cooldown: an extension costs a full supervisor
        // teardown+rebuild+fsync on THIS node, so a member re-announcing
        // within the window is ignored (its first announce always passes —
        // recovery and honest rotation are one-shot; only rapid repeats are
        // capped, bounding the churn a misbehaving member can inflict)
        let now = self.presence_now();
        if let Some(last) = self.recovery.mesh_extension_at.get(&member) {
            if now.saturating_sub(*last) < MESH_EXTENSION_COOLDOWN_SECS {
                tracing::warn!(%member, "mesh announce inside the cooldown - ignored");
                return;
            }
        }
        self.recovery.mesh_extension_at.insert(member.clone(), now);
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
            // one fresh per-pair inbound queue for the new leg
            let pair = match transport.create_queue().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%member, error = %e, "mesh-extension queue creation failed");
                    return;
                }
            };
            let Ok(wrap_in) = molt_net::WrapKey::fresh() else {
                let _ = transport.delete_queue(&pair.rcv).await;
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
            // the reply goes to the announcer's queue (per-queue FIFO puts it
            // ahead of runtime traffic)
            if let Err(e) = supervisor::send_framed(
                &transport,
                &snds[0],
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
                snds,
                wrap_out,
                rcvs: vec![pair.rcv],
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
        // subscriber on the same queues would supersede the first)
        let member = link.member.clone();
        if PeerLink::from_mesh(&link).is_none() {
            tracing::warn!(%member, "mesh extension link is malformed - keeping the old mesh");
            return Ok(Reply::Ack);
        }
        let mut mesh = net.mesh().to_vec();
        // V8 (delivery_guarantee.md §4.9): the replaced leg's OWN inbound
        // queues die here — collect them for a best-effort server-side delete
        // AFTER the rebuild (they are ours; their undelivered content is
        // covered by the acked-floor rewind, so deleting loses nothing)
        let replaced_rcvs: Vec<molt_net::RcvQueue> = mesh
            .iter()
            .filter(|l| l.member == link.member)
            .filter_map(PeerLink::from_mesh)
            .flat_map(|p| p.rcvs)
            .collect();
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
                if !active.handle.persist_mesh_crypto_blocking(mls, creds, mesh) {
                    tracing::error!("the grown mesh did not reach the disk");
                }
            }
            self.session.notice = format!("mesh-extended:{member}");
            self.emit_session(SessionScope::Full);
            tracing::info!(%member, "mesh extended");
            // V8 queue hygiene: the replaced leg's queues never carried a
            // delete before — every rotate leaked N queues on their servers
            // until idle expiry. Best-effort, off the actor, only after the
            // rebuild committed to the new leg.
            if !replaced_rcvs.is_empty() {
                let transport = self
                    .net
                    .as_ref()
                    .and_then(|n| n.runtime_transport());
                if let Some(transport) = transport {
                    tokio::spawn(async move {
                        for rcv in replaced_rcvs {
                            if let Err(e) = transport.delete_queue(&rcv).await {
                                tracing::debug!(error = %e, "deleting a replaced mesh queue failed (best-effort)");
                            }
                        }
                    });
                }
            }
        } else {
            tracing::warn!(%member, "mesh extension rebuild failed");
        }
        Ok(Reply::Ack)
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
            self.delivery.unreachable.remove(&member);
            let now = self.presence_now();
            self.stamp_member_pill(&member, now);
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// A merged re-key commit re-admitted `member` as a new incarnation
    /// (fresh log seq space): forget its accept window. Arrives over the
    /// transport's ORDERED inbound path, so the reset lands before any of
    /// the new incarnation's envelopes — the race the announce-/block-side
    /// resets could lose on a bystander catching up from a backlog (live
    /// incident 2026-08-09 §2, field rerun 2026-08-17). A member never in
    /// the roster carries no window, so no roster check is needed; the own
    /// seat never rides an add-proposal this node merges about itself
    /// mid-session.
    pub(crate) fn cmd_net_peer_rekeyed(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            tracing::info!(%member, "re-key commit merged - forgetting the seat's old accept window");
            self.reset_peer_accept_window(&member);
        }
        Ok(Reply::Ack)
    }

    /// Transport trouble: pin the member's pill unreachable AND flag the
    /// outbound leg stuck (Stage B: the endless-backoff outbox — e.g. the
    /// 2026-07-19 `SKEY ERR AUTH` loop — becomes a visible `Degraded`, not
    /// one stderr line). The last-seen stamp stays untouched — it records
    /// real sightings only; the presence pin lifts on the next sighting,
    /// the stuck flag only on a successful send (`NetSendOk`).
    pub(crate) fn cmd_net_send_failed(
        &mut self,
        member: MemberId,
        reason: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            return Ok(Reply::Ack);
        }
        tracing::warn!(%member, %reason, "sends to a member keep failing - outbox is backing off");
        self.delivery.send_stuck.insert(member.clone(), reason);
        // the group runtime names the OWN seat for its broadcast outbox: a
        // presence pin on this node could never lift (nothing sights itself)
        if member != self.member() {
            self.delivery.unreachable.insert(member);
        }
        self.refresh_member_pills();
        self.recompute_net_health();
        Ok(Reply::Ack)
    }

    /// The watchdog confirmed a member's inbound leg (subscription live):
    /// clear its degraded state (Stage B).
    pub(crate) fn cmd_net_link_up(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.link_down.remove(&member);
            // delivery guarantee §4.3: a (re)established leg gets an ACK right
            // away (the next presence tick flushes it), so a peer resuming or
            // rewinding trims its resend range to what this node still misses
            if self.delivery.accepted.contains_key(&member) {
                self.delivery.ack_due.insert(member.clone(), self.presence_now());
            }
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// A member's inbound leg died (subscription ended/failed); the
    /// watchdog is re-subscribing — surface it honestly (Stage B).
    pub(crate) fn cmd_net_link_down(
        &mut self,
        member: MemberId,
        reason: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.link_down.insert(member, reason);
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// A previously backing-off send went through: clear the stuck flag
    /// (Stage B).
    pub(crate) fn cmd_net_send_ok(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.send_stuck.remove(&member);
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// This member's REAL last-seen stamp in the active workspace, or
    /// [`molt_core::MemberInfo::NEVER`] (= 0) if we've never heard from it
    /// (or it isn't in the active roster). Used by the self-heal liveness
    /// cross-check — a stamp older than a leg's mesh-up means nothing has
    /// been delivered on that leg since it came live.
    pub(crate) fn member_last_seen(&self, member: &MemberId) -> u64 {
        self.session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .and_then(|w| w.members.iter().find(|m| &m.name == member))
            .map_or(molt_core::MemberInfo::NEVER, |m| m.last_seen)
    }

    /// Re-derive `session.net_health` (Track A — honest per-peer status). `Down`
    /// is the open/config path's fail-closed verdict and is NEVER overridden
    /// here. Otherwise a `Degraded` names only the REAL troubles: an inbound leg
    /// the watchdog reported down, or an outbox whose sends keep failing. Only
    /// an honest all-clear is `Ok`. Emits only on an actual change.
    pub(crate) fn recompute_net_health(&mut self) {
        // a Nostr workspace: the group channel's verdict, nothing else —
        // link/send maps are per-peer mesh concepts it never feeds
        if let Some(h) = self.group_net.as_ref().map(|g| g.health.borrow().clone()) {
            self.apply_group_health(h);
            return;
        }
        if matches!(self.session.net_health, molt_core::NetHealth::Down { .. }) {
            return;
        }
        let health = if self.delivery.link_down.is_empty() && self.delivery.send_stuck.is_empty() {
            molt_core::NetHealth::Ok
        } else {
            let parts: Vec<String> = self
                .delivery.link_down
                .iter()
                .map(|(m, r)| format!("link to {m}: {r}"))
                .chain(self.delivery.send_stuck.iter().map(|(m, r)| format!("sends to {m}: {r}")))
                .collect();
            molt_core::NetHealth::Degraded {
                reason: parts.join("; "),
            }
        };
        if self.session.net_health != health {
            self.session.net_health = health;
            self.emit_session(SessionScope::Full);
        }
    }

    /// N5.4/N5.5: fold the GROUP CHANNEL's health into `session.net_health`
    /// — on a relay transport the verdict is about relays, not members
    /// (there are no per-peer legs to be deaf; §6.5).
    ///
    /// Unlike the mesh fold this RECOMPUTES fully, `Down` included: a live
    /// group runtime is itself the proof that the open path's fail-closed
    /// config verdict passed (a refused dialer never builds one), and the
    /// one `Down` this fold owns — a dead subscription — really is terminal
    /// (the inbox loop returned; nothing re-subscribes until reopen).
    pub(crate) fn apply_group_health(&mut self, h: molt_net::group_runtime::GroupHealth) {
        let health = if !h.subscribed {
            molt_core::NetHealth::Down {
                reason: h.deaf.unwrap_or_else(|| "no 445 subscription".to_string()),
            }
        } else {
            let mut parts: Vec<String> = Vec::new();
            if let Some(why) = h.deaf {
                parts.push(format!("relays: {why}"));
            }
            if h.opaque_frames > 0 {
                // G4 (N5.4): older than the exporter ring is unreadable BY
                // CONSTRUCTION — a permanent, named loss, never silence
                parts.push(format!("{} frames past the key ring", h.opaque_frames));
            }
            // a stuck broadcast outbox names no peer — the channel is the
            // trouble, so its reason joins the channel verdict
            parts.extend(self.delivery.send_stuck.values().cloned());
            // SELF-HEAL (detached_reattach.md §2.4): the deaf-node signature
            // — the OWN outbox stalls (nobody acks) while frames arrive that
            // no held key opens. A healthy rejoiner counting a laggard's
            // stale frames never stalls, so it never triggers.
            if !self.delivery.send_stuck.is_empty() && h.opaque_frames > 0 {
                self.maybe_self_heal_reattach();
            }
            if parts.is_empty() {
                molt_core::NetHealth::Ok
            } else {
                molt_core::NetHealth::Degraded { reason: parts.join("; ") }
            }
        };
        if self.session.net_health != health {
            self.session.net_health = health;
            self.emit_session(SessionScope::Full);
        }
    }

    /// The presence ticker (spawned with the actor, period
    /// [`crate::PRESENCE_TICK_MS`]): re-age every pill from its stamp so
    /// a silent member drifts online → stale → offline. The stamps only
    /// ever move on real traffic; reads additionally re-derive live, so
    /// the tick exists for the PUSHED session pills. It also re-evaluates
    /// `net_health` on the same periodic beat.
    pub(crate) fn cmd_net_presence_tick(&mut self) -> Result<Reply, MoltError> {
        self.refresh_member_pills();
        self.recompute_net_health();
        // WP4a: the DAILY compaction beat rides this tick (F8) — expired chat
        // stops existing on this device, it does not merely leave the read
        // filter. Gated to one round a day; the work itself is off-actor.
        self.maybe_compact(self.presence_now());
        Ok(Reply::Ack)
    }

    /// Record a real sighting on the active workspace entry's pill. The
    /// stamp is always advanced (aging + the activity trio read it), and a
    /// full session push fires when the pill STATE changes OR when the
    /// advanced stamp crosses a label-minute boundary. A peer already online
    /// re-stamping every second within the same displayed minute renders an
    /// identical "N min ago" label and is not re-broadcast, but once the
    /// label would change the fresh stamp IS pushed — otherwise the pushed
    /// stamp freezes and the displayed age drifts upward against a still-green
    /// pill (mirrors render the age from the pushed stamp against their own
    /// clock, the `last_sync_min` pattern).
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
        // the "N min ago" label renders at minute granularity: a re-stamp that
        // lands in a new minute bucket is what a mirror would draw differently
        let label_advanced = m.last_seen / 60 != now / 60;
        m.state = state;
        m.last_seen = now;
        if state_changed || label_advanced {
            // the same gate keeps the DISK write down to one per displayed
            // minute per member: presence is local knowledge, and without
            // it on disk every restart claims it never saw anyone
            self.remember_seen(vec![(member.clone(), now)]);
            self.emit_session(SessionScope::Full);
        }
    }

    /// Re-derive every pill state — of EVERY known workspace entry, not just
    /// the active one — from each member's stamp, so a switched-away workspace
    /// ages instead of freezing its pills at whatever they were on close.
    /// Self-online and send-failure pins are scoped to the ACTIVE workspace
    /// (the node runs exactly one mesh); a non-active entry ages purely from
    /// stamps. Emits only when a state actually changed.
    fn refresh_member_pills(&mut self) {
        let now = self.presence_now();
        let me = self.member();
        let active = self.session.active_workspace.clone();
        let unreachable = &self.delivery.unreachable;
        // §6.5 (N5.5): the open workspace's transport decides how silence
        // ages — see `presence_of`, the shared derivation this mirrors
        let coarse = self.nostr.is_some();
        let mut changed = false;
        for entry in &mut self.session.workspaces {
            let is_active = entry.id == active;
            for m in &mut entry.members {
                let state = if is_active && m.name == me {
                    0
                } else if is_active && unreachable.contains(&m.name) {
                    2
                } else {
                    let s = molt_core::presence_state(now, m.last_seen);
                    if s == 2
                        && is_active
                        && coarse
                        && m.last_seen != molt_core::MemberInfo::NEVER
                        && now.saturating_sub(m.last_seen)
                            <= molt_core::MemberInfo::COARSE_SECS
                    {
                        1
                    } else {
                        s
                    }
                };
                if m.state != state {
                    m.state = state;
                    changed = true;
                }
            }
        }
        if changed {
            self.emit_session(SessionScope::Full);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::chat::{ParkedRefs, PendingRef, PARKED_TARGET_CAP};
    use molt_core::{ChatMessage, EventEnvelope, MessageId, WorkspaceEvent};

    /// The provisioning task's failure report lands as the calm
    /// `recovery-link-failed:` session notice (the same channel the minted
    /// link rides), and the dead mint's ticket is unregistered — nothing of
    /// the failed attempt stays armed.
    #[test]
    fn a_recover_link_failure_report_sets_the_notice_and_kills_the_ticket() {
        let mut st = crate::tests::plain_state();
        st.recovery.tickets.insert("t-1".to_string(), "bob".to_string());
        st.cmd_net_recover_link_failed(
            "bob".to_string(),
            "boom".to_string(),
            "t-1".to_string(),
            None,
        )
        .expect("the report acks");
        assert_eq!(st.session.notice, "recovery-link-failed:boom");
        assert!(
            st.recovery.tickets.is_empty(),
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
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 101,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(id, "peer-1", "soon gone", 101)),
        });
        st.apply(&EventEnvelope { prev_seq: 0,
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
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 100,
            by: "me".to_string(),
            body: WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: molt_core::Surface::Memory,
                payload: serde_json::json!({ "op": "add_note", "title": "t" }),
            },
        });
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 2,
            ts: 101,
            by: "peer-2".to_string(),
            body: WorkspaceEvent::Declined {
                id: molt_core::ProposalId(1),
                by: "peer-2".to_string(),
                hash: String::new(),
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
            EventEnvelope { prev_seq: 0,
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

    /// **A hostile stamp on a wire chat never panics a read.** `ts` is the
    /// peer's claim; `uploads_view` added the retention to it, and with
    /// release overflow checks on, `u64::MAX` took the whole actor down —
    /// persisted, so every reopen died on the Uploads tab (review
    /// 2026-08-25). The wire clamps the stamp to a plausible window and
    /// the read saturates.
    #[test]
    fn a_wire_chat_with_a_hostile_stamp_never_panics_the_uploads_view() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        let mut msg = ChatMessage::text(id(8), "peer-1", "share", u64::MAX);
        msg.file = Some(molt_core::FileMeta {
            name: "a.bin".to_string(),
            size: 1,
            kind: "bin".to_string(),
            modified: u64::MAX,
            available: true,
            checksum: String::new(),
        });
        st.cmd_net_delivered(
            "peer-1".to_string(),
            EventEnvelope { prev_seq: 0,
                seq: 1,
                ts: u64::MAX,
                by: "peer-1".to_string(),
                body: WorkspaceEvent::Chat(msg),
            },
            None,
        )
        .expect("a wire delivery never errors");
        assert_eq!(st.chat.len(), 1);
        assert!(
            st.chat[0].ts <= crate::now_secs().saturating_add(900),
            "the stamp is clamped to the FileServed plausibility window"
        );
        let uploads = st.uploads_view();
        assert_eq!(uploads.len(), 1, "the share is listed, the read did not panic");
    }

    /// A wire chat is a FRESH message: the log original carries no
    /// reactions, receipts or tombstone — those travel as their own
    /// link-authenticated events. Carrying them inside the body attributed
    /// forged stances to OTHER members (review 2026-08-25).
    #[test]
    fn a_wire_chat_carries_no_foreign_reactions_receipts_or_tombstone() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        let mut msg = ChatMessage::text(id(9), "peer-1", "forged", 0);
        msg.reactions
            .insert("👍".to_string(), vec!["peer-2".to_string()]);
        msg.read_by.insert("peer-2".to_string());
        msg.deleted_by = Some("peer-2".to_string());
        st.cmd_net_delivered(
            "peer-1".to_string(),
            EventEnvelope { prev_seq: 0,
                seq: 1,
                ts: 0,
                by: "peer-1".to_string(),
                body: WorkspaceEvent::Chat(msg),
            },
            None,
        )
        .expect("a wire delivery never errors");
        let m = &st.chat[0];
        assert!(m.reactions.is_empty(), "no forged reactions");
        assert!(m.read_by.is_empty(), "no forged receipts");
        assert_eq!(m.deleted_by, None, "no forged tombstone");
        assert_ne!(m.ts, 0, "an unknown age is the arrival time, not 'forever'");
    }

    /// E6: one `ChatRead` of random ids parks a bounded number of
    /// targets — never the whole P6 buffer.
    #[test]
    fn a_read_receipt_frame_parks_a_bounded_number_of_targets() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        let ids: Vec<MessageId> = (100..160).map(id).collect();
        st.cmd_net_delivered(
            "peer-1".to_string(),
            EventEnvelope { prev_seq: 0,
                seq: 1,
                ts: 100,
                by: "peer-1".to_string(),
                body: WorkspaceEvent::ChatRead { ids: ids.clone(), by: "peer-1".to_string() },
            },
            None,
        )
        .expect("a wire delivery never errors");
        let parked = ids.iter().filter(|i| st.parked.holds(i)).count();
        assert_eq!(parked, super::PARKED_READS_PER_FRAME, "the per-frame cap holds");
    }

    fn id(n: usize) -> MessageId {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(u64::try_from(n).expect("small")).to_le_bytes());
        MessageId(b)
    }

    /// RELAY file plane trust gates (review 2026-08-10): a `FileServed`
    /// counts only from the SHARER's own mouth, only with a plausible
    /// stamp, and an old redelivery never regresses a newer one — one
    /// crafted frame from any member must not poison the group's stamp
    /// cache (a future stamp names an h-window that holds nothing, so a
    /// poisoned cache bricks the share's downloads for good).
    #[test]
    fn a_file_served_counts_only_from_the_sharer() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        st.nostr = Some(crate::NostrTransport {
            sk: zeroize::Zeroizing::new(vec![7u8; 32]),
            relays: vec!["ws://relay.example".to_string()],
            rotation_seed: [0u8; 32],
        });
        // peer-1's share message lands over the wire
        let sid = id(9);
        let mut msg = ChatMessage::text(sid, "peer-1", "der bericht", 100);
        msg.file = Some(molt_core::FileMeta {
            name: "bericht.bin".to_string(),
            size: 4,
            kind: "File".to_string(),
            modified: 100,
            available: true,
            checksum: "aa".repeat(32),
        });
        st.cmd_net_delivered(
            "peer-1".to_string(),
            EventEnvelope {
                prev_seq: 0,
                seq: 1,
                ts: 100,
                by: "peer-1".to_string(),
                body: WorkspaceEvent::Chat(msg),
            },
            None,
        )
        .expect("share lands");
        let served = |from: &str, seq: u64, at: u64| EventEnvelope {
            prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: from.to_string(),
            body: WorkspaceEvent::FileServed { id: sid, at },
        };
        // another member announcing the sharer's series: dropped
        st.cmd_net_delivered("peer-2".to_string(), served("peer-2", 1, 1_000), None)
            .expect("ack");
        assert!(st.files.series.is_empty(), "a non-sharer's announcement must not count");
        // the sharer with an absurd future stamp: dropped
        let future = crate::now_secs() + 1_000_000;
        st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 2, future), None)
            .expect("ack");
        assert!(st.files.series.is_empty(), "a far-future stamp must not count");
        // the sharer's plausible stamp lands…
        st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 3, 5_000), None)
            .expect("ack");
        assert_eq!(st.files.series.get(&sid), Some(&5_000));
        // …and an older redelivery does not regress it
        st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 4, 4_000), None)
            .expect("ack");
        assert_eq!(st.files.series.get(&sid), Some(&5_000), "at-least-once must not rewind");
    }

    // --- real presence: numeric stamps, aging, the activity trio -----------

    use molt_core::MemberInfo;

    /// A base instant for the presence tests, far from the thresholds.
    const T: u64 = 1_750_000_000;

    /// A state with an active workspace entry and a real 2-of-3 roster —
    /// ada is the local member; nobody has been seen yet.
    fn presence_fixture() -> crate::State {
        let mut st = crate::tests::plain_state();
        st.presence.clock_override = Some(T);
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
            restored: false,
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
        st.presence.clock_override = Some(T + 7_200);
        let s = st.status();
        assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 2, 2));
        // eight days of silence: bob leaves every window
        st.presence.clock_override = Some(T + 8 * 86_400);
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
        st.presence.clock_override = Some(T + 30 * 86_400);
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

    /// Stage B honest health: the supervisor's link/send signals drive
    /// `session.net_health` Ok → Degraded (reason naming every troubled
    /// peer) → Ok, and only when BOTH legs are clear again.
    #[test]
    fn link_and_send_signals_drive_ok_degraded_ok() {
        let mut st = presence_fixture();
        assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
        st.cmd_net_link_down("bob".to_string(), "subscription ended".to_string(), None)
            .expect("ack");
        match &st.session.net_health {
            molt_core::NetHealth::Degraded { reason } => {
                assert!(reason.contains("bob"), "names the peer: {reason}");
                assert!(reason.contains("subscription ended"), "carries the cause: {reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        // a stuck outbox on ANOTHER peer joins the reason
        st.cmd_net_send_failed("cid".to_string(), "SKEY rejected: ERR AUTH".to_string(), None)
            .expect("ack");
        match &st.session.net_health {
            molt_core::NetHealth::Degraded { reason } => {
                assert!(reason.contains("bob") && reason.contains("cid"), "{reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        // heal one leg: bob's subscription is back AND it delivers a frame (so
        // the leg is verified, not merely live-but-unverified) — still degraded
        // because the OTHER peer's outbox is stuck
        st.cmd_net_link_up("bob".to_string(), None).expect("ack");
        st.cmd_net_peer_seen("bob".to_string(), None).expect("bob delivers - leg verified");
        assert!(
            matches!(st.session.net_health, molt_core::NetHealth::Degraded { .. }),
            "cid's outbox is still stuck"
        );
        // heal the second: honest Ok again
        st.cmd_net_send_ok("cid".to_string(), None).expect("ack");
        assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
    }

    /// §6.5 (N5.5): presence over relays is traffic-derived and COARSE —
    /// silence is not absence. On a Nostr workspace a stamped member ages
    /// to stale and STAYS there; only never-heard shows dark. The mesh
    /// keeps its keepalive-backed aging (a silent mesh member really is
    /// unreachable — its keepalives stopped).
    #[test]
    fn a_quiet_nostr_republic_shows_last_seen_not_offline() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("stamp bob");
        // a quiet weekend later, on a MESH workspace: bob is honestly offline
        st.presence.clock_override = Some(T + 3 * 86_400);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 2, "mesh aging is unchanged");
        // the same silence on a NOSTR workspace: coarse, not dark
        st.nostr = Some(crate::NostrTransport {
            sk: zeroize::Zeroizing::new(vec![7u8; 32]),
            relays: vec!["ws://relay.example".to_string()],
            rotation_seed: [0u8; 32],
        });
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 1, "a stamped member is stale, never dark");
        assert_eq!(
            st.presence_of("bob", T, st.presence_now()),
            1,
            "the shared derivation agrees (co-equality)"
        );
        // …but a member NEVER heard from is honestly dark
        assert_eq!(pill(&st, "cid").state, 2, "never-heard stays dark");
    }

    /// The coarse-Nostr lift covers a QUIET republic, not an absent seat.
    /// Since the founding date became a real stamp (nobody reads back as
    /// never-seen), "stamped" alone would paint a member gone for months
    /// the same yellow as one heard from this morning - so the lift ends
    /// with [`MemberInfo::COARSE_SECS`] and the dot goes dark again.
    #[test]
    fn a_seat_silent_past_the_coarse_window_goes_dark_again() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("stamp bob");
        st.nostr = Some(crate::NostrTransport {
            sk: zeroize::Zeroizing::new(vec![7u8; 32]),
            relays: vec!["ws://relay.example".to_string()],
            rotation_seed: [0u8; 32],
        });
        // inside the window: coarse, not dark (the quiet-republic case)
        st.presence.clock_override = Some(T + MemberInfo::COARSE_SECS - 60);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 1, "a quiet week is still stale");
        // past it: this is not silence any more, it is absence
        st.presence.clock_override = Some(T + MemberInfo::COARSE_SECS + 60);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 2, "months of silence must read dark");
        assert_eq!(
            st.presence_of("bob", T, st.presence_now()),
            2,
            "the shared derivation agrees (co-equality)"
        );
    }

    /// N5.4 (G4 epoch-ring honesty) + N5.5: on a Nostr workspace the health
    /// verdict is the GROUP CHANNEL's — relays, not members. A deaf channel
    /// degrades with the relay reason; frames past the exporter ring are a
    /// PERMANENT, named loss; a dead subscription is Down; a healthy
    /// channel is an honest Ok again.
    #[test]
    fn group_channel_health_names_relays_and_ring_losses() {
        let mut st = presence_fixture();
        let h = |subscribed: bool, deaf: Option<&str>, opaque: u64| {
            molt_net::group_runtime::GroupHealth {
                subscribed,
                deaf: deaf.map(|s| s.to_string()),
                opaque_frames: opaque,
            }
        };
        st.apply_group_health(h(true, Some("relay ws://r refused the sub"), 0));
        match &st.session.net_health {
            molt_core::NetHealth::Degraded { reason } => {
                assert!(reason.contains("relay"), "names the relay trouble: {reason}");
            }
            other => panic!("deaf must degrade, got {other:?}"),
        }
        // the deafness heals — honest Ok again
        st.apply_group_health(h(true, None, 0));
        assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
        // G4: a frame older than the exporter ring is unreadable BY
        // CONSTRUCTION — a named permanent loss, never silence
        st.apply_group_health(h(true, None, 3));
        match &st.session.net_health {
            molt_core::NetHealth::Degraded { reason } => {
                assert!(reason.contains('3') && reason.contains("key ring"), "{reason}");
            }
            other => panic!("ring losses must be loud, got {other:?}"),
        }
        // a dead subscription cannot heal itself — Down, not Degraded
        st.apply_group_health(h(false, Some("subscribe: connection refused"), 0));
        assert!(
            matches!(st.session.net_health, molt_core::NetHealth::Down { .. }),
            "a dead inbox is Down: {:?}",
            st.session.net_health
        );
    }

    /// The group verdict also carries a stuck outbox (send_failed on
    /// broadcast names no peer — the trouble is the channel).
    #[test]
    fn a_stuck_group_outbox_joins_the_channel_verdict() {
        let mut st = presence_fixture();
        st.delivery.send_stuck
            .insert("ada".to_string(), "no relay accepted the frame".to_string());
        st.apply_group_health(molt_net::group_runtime::GroupHealth {
            subscribed: true,
            deaf: None,
            opaque_frames: 0,
        });
        match &st.session.net_health {
            molt_core::NetHealth::Degraded { reason } => {
                assert!(reason.contains("no relay accepted"), "{reason}");
            }
            other => panic!("a stuck outbox must surface, got {other:?}"),
        }
    }

    /// `Down` is the open/config path's verdict (fail-closed dialer,
    /// detached reopen) — runtime link signals must never lift it.
    #[test]
    fn a_down_verdict_is_never_lifted_by_link_signals() {
        let mut st = presence_fixture();
        st.session.net_health = molt_core::NetHealth::Down {
            reason: "resume failed - workspace opened detached".to_string(),
        };
        st.cmd_net_link_down("bob".to_string(), "x".to_string(), None).expect("ack");
        assert!(matches!(st.session.net_health, molt_core::NetHealth::Down { .. }));
        st.cmd_net_link_up("bob".to_string(), None).expect("ack");
        st.cmd_net_send_ok("bob".to_string(), None).expect("ack");
        assert!(
            matches!(st.session.net_health, molt_core::NetHealth::Down { .. }),
            "link signals must never lift a Down verdict"
        );
    }

    /// FP3: a relay-plane fetch task holds a PRIVATE subscription (its own
    /// relay runtime, which no net teardown reaches) — the close/switch
    /// boundary must end the task instead of letting it live out its fetch
    /// budget against a closed workspace.
    #[test]
    fn a_workspace_reset_aborts_the_running_file_fetches() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _hold = tx;
            std::future::pending::<()>().await;
        });
        st.files.fetches.push(task.abort_handle());
        st.reset_workspace_state();
        rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("the fetch task must be aborted at the workspace boundary")
                .expect_err("the sender drops with the aborted future, unused");
        });
        assert!(st.files.fetches.is_empty(), "the handle list is cleared");
    }

    /// Link/send-stuck state is scoped to the workspace: the close/switch
    /// boundary clears it (like the send-failure presence pins), so the
    /// next workspace never inherits a Degraded pill.
    #[test]
    fn link_state_does_not_leak_past_a_workspace_reset() {
        let mut st = presence_fixture();
        st.cmd_net_link_down("bob".to_string(), "gone".to_string(), None).expect("ack");
        st.cmd_net_send_failed("cid".to_string(), "gone".to_string(), None).expect("ack");
        assert!(!st.delivery.link_down.is_empty() && !st.delivery.send_stuck.is_empty());
        st.reset_workspace_state();
        assert!(
            st.delivery.link_down.is_empty() && st.delivery.send_stuck.is_empty(),
            "the close/switch boundary clears the link state"
        );
    }

    /// §4.3: the ACK flush takes only DUE deadlines (future ones stay armed),
    /// and a (re)established leg arms an immediate ack only when there is a
    /// window to report.
    #[test]
    fn the_ack_flush_takes_only_due_deadlines_and_link_up_arms_one() {
        let mut st = presence_fixture();
        let win = {
            let mut w = molt_core::AcceptedWindow::default();
            assert!(w.accept(3));
            w
        };
        st.delivery.accepted.insert("bob".to_string(), win);
        st.delivery.ack_due.insert("bob".to_string(), T);
        st.delivery.ack_due.insert("cid".to_string(), T + 100);
        st.flush_due_acks(T + 1);
        assert!(
            !st.delivery.ack_due.contains_key("bob"),
            "the due deadline is consumed (no mesh here: dropped - resends re-arm)"
        );
        assert!(st.delivery.ack_due.contains_key("cid"), "a future deadline stays armed");

        // link-up arms an immediate ack — but only with a window to report
        st.delivery.ack_due.clear();
        st.cmd_net_link_up("cid".to_string(), None).expect("ack");
        assert!(st.delivery.ack_due.is_empty(), "no window for cid - nothing to report");
        st.cmd_net_link_up("bob".to_string(), None).expect("ack");
        assert_eq!(st.delivery.ack_due.get("bob"), Some(&T), "bob's window arms a due-now ack");
    }

    /// V1 (delivery_guarantee.md): an announce that carries no queue for THIS
    /// node must not burn the announcer's extension cooldown — the follow-up
    /// announce that IS for us (moments later) must still adopt. A repeated
    /// VALID announce inside the window stays capped as before.
    #[test]
    fn an_announce_without_our_queue_does_not_burn_the_cooldown() {
        let mut st = presence_fixture();
        st.presence.clock_override = Some(T);
        let handover = |queue: &str| molt_net::mesh::QueueHandover {
            server: String::new(),
            queue: queue.to_string(),
            wrap: hex::encode([7u8; 32]),
        };
        // bob's announce reaches ada without any queue for ada
        let mut queues = std::collections::BTreeMap::new();
        queues.insert("cid".to_string(), handover("aa"));
        let for_cid = molt_net::mesh::MeshAnnounce { queues };
        st.spawn_mesh_extension("bob".to_string(), &for_cid);
        assert!(
            !st.recovery.mesh_extension_at.contains_key("bob"),
            "an announce carrying nothing for us must not stamp the cooldown"
        );
        // moments later bob's announce FOR ada arrives — it must pass the gate
        st.presence.clock_override = Some(T + 5);
        let mut queues = std::collections::BTreeMap::new();
        queues.insert("ada".to_string(), handover("bb"));
        let for_ada = molt_net::mesh::MeshAnnounce { queues };
        st.spawn_mesh_extension("bob".to_string(), &for_ada);
        assert_eq!(
            st.recovery.mesh_extension_at.get("bob"),
            Some(&(T + 5)),
            "the announce for us passes the cooldown gate and stamps it"
        );
        // a REPEATED valid announce inside the window is still ignored (churn cap)
        st.presence.clock_override = Some(T + 10);
        st.spawn_mesh_extension("bob".to_string(), &for_ada);
        assert_eq!(
            st.recovery.mesh_extension_at.get("bob"),
            Some(&(T + 5)),
            "a rapid repeat stays capped - the stamp is not refreshed"
        );
    }

    /// A send-failure pin is scoped to the workspace: closing/resetting the
    /// workspace drops it, so a same-named member in the next workspace is
    /// not falsely shown unreachable.
    #[test]
    fn a_send_failure_pin_does_not_leak_past_a_workspace_reset() {
        let mut st = presence_fixture();
        st.cmd_net_send_failed("bob".to_string(), "gone".to_string(), None)
            .expect("ack");
        assert!(st.delivery.unreachable.contains("bob"));
        st.reset_workspace_state();
        assert!(
            st.delivery.unreachable.is_empty(),
            "the close/switch boundary clears the pins"
        );
    }

    /// A stuck BROADCAST outbox (the group runtime names the own seat)
    /// flags the channel, never the operator's own presence: this node is
    /// running, and no sighting could ever lift a pin on itself.
    #[test]
    fn a_stuck_broadcast_outbox_never_pins_the_own_seat_offline() {
        let mut st = presence_fixture();
        st.cmd_net_send_failed(
            "ada".to_string(),
            "no relay accepted the frame".to_string(),
            None,
        )
        .expect("ack");
        assert!(
            st.delivery.send_stuck.contains_key("ada"),
            "the channel trouble is recorded"
        );
        assert!(
            !st.delivery.unreachable.contains("ada"),
            "the own seat is never pinned unreachable"
        );
    }

    /// The presence ticker ages a silent member's pill: online → stale
    /// after `ONLINE_SECS`, stale → offline after `STALE_SECS` — the stamp
    /// itself never moves without real traffic.
    #[test]
    fn the_ticker_ages_a_silent_pill_stale_then_offline() {
        let mut st = presence_fixture();
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        st.presence.clock_override = Some(T + MemberInfo::ONLINE_SECS + 1);
        st.cmd_net_presence_tick().expect("tick");
        assert_eq!(pill(&st, "bob").state, 1, "silence past ONLINE_SECS is stale");
        st.presence.clock_override = Some(T + MemberInfo::STALE_SECS + 1);
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

    /// A silent workspace entry the operator has switched AWAY from must age
    /// its pills too — the presence ticker cannot freeze a closed workspace's
    /// members at "online" forever. Self-online and send-failure pins are
    /// scoped to the ACTIVE workspace; a switched-away one ages purely from
    /// each member's real stamp.
    #[test]
    fn a_switched_away_workspace_ages_out_instead_of_freezing_online() {
        let mut st = presence_fixture(); // active "w-presence" (ada/bob/cid)
        st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
        // a second workspace we last looked at when everyone was online
        // (fresh stamps), then switched away from and never touched again
        let closed_roster = vec!["ada".to_string(), "bob".to_string()];
        st.session.workspaces.push(molt_core::WorkspaceInfo {
            id: "w-closed".to_string(),
            name: "Closed".to_string(),
            detail: "1-of-2".to_string(),
            synced: false,
            state: 2,
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
            members: molt_core::roster_members(&closed_roster, T, |_| T),
            agenda: String::new(),
            restored: false,
        });
        // 31 minutes of total silence pass everywhere
        st.presence.clock_override = Some(T + MemberInfo::STALE_SECS + 1);
        st.cmd_net_presence_tick().expect("tick");
        // the ACTIVE entry ages honestly (bob offline, ada self-online)
        assert_eq!(pill(&st, "bob").state, 2, "the active workspace's silent peer ages offline");
        assert_eq!(pill(&st, "ada").state, 0, "the active workspace keeps self online");
        // the CLOSED entry must age from its stamps, not freeze at online
        let closed = st
            .session
            .workspaces
            .iter()
            .find(|w| w.id == "w-closed")
            .expect("closed entry");
        let closed_pill = |name: &str| {
            closed.members.iter().find(|m| m.name == name).expect("closed pill").state
        };
        assert_eq!(closed_pill("bob"), 2, "a switched-away peer ages offline, not frozen online");
        assert_eq!(
            closed_pill("ada"),
            2,
            "self-online applies only to the ACTIVE workspace; a closed one ages self too"
        );
    }

    /// The pushed presence stamp must not freeze between state changes. A
    /// re-stamp that renders an identical "N min ago" label (same displayed
    /// minute) is not re-broadcast, but one that crosses a label-minute
    /// boundary IS — otherwise a continuously-seen peer's pushed age drifts
    /// upward against a still-green pill.
    #[test]
    fn a_restamp_crossing_a_label_minute_re_pushes_the_fresh_stamp() {
        let mut st = presence_fixture();
        // align to a label-minute boundary so the buckets are obvious
        let base = (T / 60) * 60;
        st.presence.clock_override = Some(base);
        // first sighting flips NEVER -> online: a state change, so it pushes
        st.cmd_net_peer_seen("bob".to_string(), None).expect("first sighting");
        // observe only pushes from here on
        let mut ev = st.subscribe_events();
        // a re-stamp still inside the same displayed minute renders identically
        st.presence.clock_override = Some(base + 59);
        st.cmd_net_peer_seen("bob".to_string(), None).expect("re-stamp, same minute");
        assert!(
            ev.try_recv().is_err(),
            "a re-stamp inside the same label-minute must not re-broadcast the session"
        );
        // a re-stamp crossing into the next displayed minute changes the label
        st.presence.clock_override = Some(base + 60);
        st.cmd_net_peer_seen("bob".to_string(), None).expect("re-stamp, next minute");
        assert!(
            matches!(ev.try_recv(), Ok(crate::Event::SessionChanged { .. })),
            "crossing a label-minute must push the refreshed stamp"
        );
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
        st.presence.clock_override = Some(T + 10);
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
        st.apply(&EventEnvelope { prev_seq: 0,
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
