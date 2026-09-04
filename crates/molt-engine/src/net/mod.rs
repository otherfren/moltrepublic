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
pub(crate) mod files;
mod ingest;
mod presence;
pub(crate) use presence::pill_state;
mod recovery;
#[cfg(test)]
pub(crate) use ingest::{CHAIN_SERVE_DEBOUNCE_SECS, PARKED_READS_PER_FRAME};


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
mod test_support;
#[cfg(test)]
mod delivery_tests;
#[cfg(test)]
mod demo_mesh_tests;
#[cfg(test)]
mod files_tests;
#[cfg(test)]
mod ingest_tests;
#[cfg(test)]
mod presence_tests;
#[cfg(test)]
mod recovery_tests;
