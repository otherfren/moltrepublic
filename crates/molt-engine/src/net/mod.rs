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
mod files;
mod ingest;
mod presence;
mod recovery;
#[cfg(test)]
pub(crate) use ingest::{CHAIN_SERVE_DEBOUNCE_SECS, PARKED_READS_PER_FRAME};

#[cfg(test)]
mod demo_mesh_tests;

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
