// SPDX-License-Identifier: GPL-3.0-or-later

//! The **direct mesh over the loopback star** — the post-founding bootstrap
//! (`member_bootstrap` / `founder_bootstrap` + the founder's `NetMesh*`
//! handlers) and the post-recovery re-join (`rejoin_mesh`). Loopback is THE
//! test transport (`CLAUDE.md`, "Transport"): the production transport is
//! Nostr, whose 445 group runtime needs no per-pair mesh, so nothing here
//! runs on a production path. It is reached only through the test seams —
//! `State::ritual_bootstrap`, `run_ritual_member(bootstrap = true)`,
//! `run_rejoin(bootstrap = true)`.
//!
//! Not `cfg(test)`-gated, deliberately: the integration tests compile the
//! crate WITHOUT `cfg(test)`, and `NetMeshAnnounced` / `NetMeshReady` are
//! `Command` variants on the co-equal surface whose handlers must exist in
//! every build (`co_equality_every_command_is_a_tool_or_documented_internal`).
//! A cargo feature would need a self-dev-dependency to reach the tests and
//! could not be verified against the GUI crates from here; a module boundary
//! gives the readability the split is for.

use std::sync::{Arc, Mutex};

use molt_core::{Command, MemberId};
use molt_net::{invite, msg_id, supervisor, Delivery, LoopbackTransport, SndQueueAddr, Transport, WrapKey};
use tokio::sync::mpsc;

use crate::founding::{next_framed_msg, RitualRuntime};
use crate::{Envelope, State};

/// How long a node waits for its peers' mesh announcements before giving up and
/// entering without a direct mesh (best-effort bootstrap — see the join gating
/// decision). Generous: at founding time every peer is present, so the exchange
/// normally completes in well under a second; this only bounds a failed peer.
pub(crate) const MESH_BOOTSTRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Run the member side of the post-founding **mesh bootstrap** over the star:
/// carry [`molt_net::mesh::MeshAnnounce`]s as MLS ciphertext — outbound as
/// `RitualMsg::MeshAnnounce` on the founder's invite queue, inbound on our reply
/// queue — and return the assembled full-mesh handovers. Consumes `rx`/`reasm`
/// (the reply-queue reader after the genesis message).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn member_bootstrap<T: molt_net::Transport>(
    name: &str,
    peers: Vec<MemberId>,
    transport: &T,
    invite_snd: SndQueueAddr,
    invite_wrap: WrapKey,
    reply_wrap: WrapKey,
    mut rx: mpsc::Receiver<Delivery>,
    mut reasm: molt_net::Reassembler,
    early: Vec<Vec<u8>>,
    mls: Arc<Mutex<molt_net::MlsMember>>,
) -> Result<Vec<molt_core::MeshLink>, String> {
    let cap = peers.len() + 1 + early.len();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(cap);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(cap);
    // any announcement that arrived before the genesis was processed goes in
    // first, ahead of the live reply-queue reader
    for ct in early {
        let _ = in_tx.send(ct).await;
    }
    // outbound: MLS ciphertext → RitualMsg::MeshAnnounce on the invite queue
    let t2 = transport.clone();
    let nm = name.to_string();
    let send_task = tokio::spawn(async move {
        let mut n = 1000u64;
        while let Some(ct) = out_rx.recv().await {
            let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            if let Ok(p) = serde_json::to_vec(&msg) {
                let _ = supervisor::send_framed(&t2, &invite_snd, &invite_wrap, msg_id(&nm, "mesh", n), &p).await;
                n += 1;
            }
        }
    });
    // inbound: read the reply queue for MeshAnnounce → the bootstrap's in channel
    let recv_task = tokio::spawn(async move {
        while let Some(bytes) = next_framed_msg(&mut rx, &reply_wrap, &mut reasm).await {
            if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                serde_json::from_slice::<invite::RitualMsg>(&bytes)
            {
                if let Ok(raw) = hex::decode(&ct) {
                    if in_tx.send(raw).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let links = molt_net::mesh::bootstrap_over_mls(
        name,
        &peers,
        transport,
        mls,
        out_tx,
        in_rx,
        MESH_BOOTSTRAP_TIMEOUT,
    )
    .await;
    // await (don't abort) the send task: bootstrap_over_mls has flushed our
    // announcement into `out_ct` and dropped its sender, so the send task drains
    // that last frame onto the invite queue and then ends — awaiting it ensures
    // the founder actually receives our handover before we return
    let _ = send_task.await;
    recv_task.abort();
    links.map(|ls| ls.iter().map(molt_net::PeerLink::to_mesh).collect())
}

/// Run the **founder** side of the post-founding mesh bootstrap over the star.
/// The founder participates like any node (opens per-pair queues, announces its
/// own, collects the members') AND is the star's temporary **relay**: each
/// member's ciphertext arrives on `ct_in` as `(seat, hex)` (routed there by the
/// founder's recv loop) — the founder forwards it into its own bootstrap and
/// re-sends the *same* MLS ciphertext to every **other** member's reply queue,
/// so members learn each other's queues before any direct link exists (any
/// group member can decrypt it; the sender stays MLS-authenticated end to end).
/// `seat_replies` is each joined seat's reply queue. Returns the founder's
/// assembled full-mesh handovers.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn founder_bootstrap(
    founder_name: String,
    peers: Vec<MemberId>,
    transport: LoopbackTransport,
    seat_replies: Vec<(u32, SndQueueAddr, WrapKey)>,
    mls: Arc<Mutex<molt_net::MlsMember>>,
    mut ct_in: mpsc::UnboundedReceiver<(u32, String)>,
) -> Result<Vec<molt_core::MeshLink>, String> {
    let cap = peers.len() + 1;
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(cap);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(cap);

    // outbound: the founder's own encrypted announcement → every member's reply queue
    let replies = seat_replies.clone();
    let t_out = transport.clone();
    let send_task = tokio::spawn(async move {
        let mut n = 5000u64;
        while let Some(ct) = out_rx.recv().await {
            let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            let Ok(payload) = serde_json::to_vec(&msg) else {
                continue;
            };
            for (seat, addr, wrap) in &replies {
                let id = msg_id("founder", "mesh", n + u64::from(*seat));
                let _ = supervisor::send_framed(&t_out, addr, wrap, id, &payload).await;
            }
            n += 1000;
        }
    });

    // inbound + relay: a member's ciphertext feeds the founder's own bootstrap
    // AND is relayed verbatim to every other member's reply queue
    let replies2 = seat_replies.clone();
    let t_relay = transport.clone();
    let relay_task = tokio::spawn(async move {
        let mut n = 90_000u64;
        while let Some((seat, hexct)) = ct_in.recv().await {
            let Ok(bytes) = hex::decode(&hexct) else {
                continue;
            };
            let msg = invite::RitualMsg::MeshAnnounce { ct: hexct };
            if let Ok(payload) = serde_json::to_vec(&msg) {
                for (s, addr, wrap) in &replies2 {
                    if *s == seat {
                        continue; // don't echo back to the announcer
                    }
                    let id = msg_id("founder", "relay", n);
                    let _ = supervisor::send_framed(&t_relay, addr, wrap, id, &payload).await;
                    n += 1;
                }
            }
            if in_tx.send(bytes).await.is_err() {
                break;
            }
        }
    });

    let links = molt_net::mesh::bootstrap_over_mls(
        &founder_name,
        &peers,
        &transport,
        mls,
        out_tx,
        in_rx,
        MESH_BOOTSTRAP_TIMEOUT,
    )
    .await;
    // await (don't abort) the send task so the founder's own announcement is
    // fully delivered to every member's reply queue before we return; the task
    // ends on its own once bootstrap_over_mls drops the outbound sender
    let _ = send_task.await;
    relay_task.abort();
    links.map(|ls| ls.iter().map(molt_net::PeerLink::to_mesh).collect())
}

/// Re-join the **runtime mesh** after recovery — the rejoiner side of dynamic
/// mesh membership (`docs_archive/transport/dynamic_mesh.md`): open one fresh per-pair
/// inbound queue per survivor, announce them MLS-encrypted over the recovery
/// channel (the coordinator authenticates the sender and relays the ciphertext
/// verbatim over the runtime mesh), then await each survivor's reply announce
/// as the **first frame on the very queue announced for it** (per-queue FIFO:
/// the reply precedes any runtime traffic, so it is read here and acked before
/// the queue is handed to the supervisor), authenticate each reply by MLS
/// decryption, and assemble the full-mesh links.
pub(crate) async fn rejoin_mesh<T: Transport>(
    me: &str,
    survivors: &[String],
    transport: &T,
    mls: &mut molt_net::MlsMember,
    recover_snd: &SndQueueAddr,
    recover_wrap: &WrapKey,
    timeout: std::time::Duration,
) -> Result<Vec<molt_core::MeshLink>, String> {
    use molt_net::mesh;
    use std::collections::BTreeMap;

    // one fresh per-pair inbound queue per survivor (per-pair = unlinkability,
    // same as the founding bootstrap). The reply arrives on that queue, which
    // is subscribed BEFORE the announce so a fast reply cannot race the
    // subscription.
    let mut my_inbound: BTreeMap<String, (Vec<molt_net::RcvQueue>, WrapKey)> = BTreeMap::new();
    let mut queues: BTreeMap<String, mesh::QueueHandover> = BTreeMap::new();
    let (reply_tx, mut reply_rx) = mpsc::channel::<Vec<u8>>(survivors.len().max(1));
    let mut readers = Vec::with_capacity(survivors.len());
    for s in survivors {
        let wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
        let pair = transport.create_queue().await.map_err(|e| e.to_string())?;
        let mut rx = transport.subscribe(&pair.rcv).await.map_err(|e| e.to_string())?;
        queues.insert(s.clone(), mesh::QueueHandover::of(&pair.snd, &wrap));
        my_inbound.insert(s.clone(), (vec![pair.rcv], wrap.clone()));
        // the survivor's reply is the FIRST frame on this queue (it sends the
        // reply before it stands its extended supervisor up, and the queue is
        // fresh) — read exactly one framed message, ack it, and stop, leaving
        // every later (runtime) frame for the supervisor's own subscription
        let tx = reply_tx.clone();
        readers.push(tokio::spawn(async move {
            let mut reasm = molt_net::Reassembler::new();
            // exactly ONE complete frame, whatever it parses as
            if let Some(bytes) = next_framed_msg(&mut rx, &wrap, &mut reasm).await {
                if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                    serde_json::from_slice::<invite::RitualMsg>(&bytes)
                {
                    if let Ok(raw) = hex::decode(&ct) {
                        let _ = tx.send(raw).await;
                    }
                }
            }
        }));
    }
    drop(reply_tx);

    // announce the queues — MLS-encrypted, so every survivor authenticates the
    // sender — over the recovery channel (the coordinator relays to the mesh)
    let announce = mesh::MeshAnnounce { queues };
    let bytes = serde_json::to_vec(&announce).map_err(|e| e.to_string())?;
    let ct = mls.encrypt(&bytes).map_err(|e| e.to_string())?;
    let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
    let payload = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
    supervisor::send_framed(transport, recover_snd, recover_wrap, msg_id(me, "mesh", 2), &payload)
        .await
        .map_err(|e| e.to_string())?;

    // collect + MLS-authenticate every survivor's reply, bounded by `timeout`
    // (best-effort like the founding bootstrap)
    let deadline = tokio::time::Instant::now() + timeout;
    let mut announces: BTreeMap<String, mesh::MeshAnnounce> = BTreeMap::new();
    while announces.len() < survivors.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, reply_rx.recv()).await {
            Ok(Some(raw)) => {
                // decryption authenticates the replier — an announce from anyone
                // but an expected survivor is ignored
                if let Ok(molt_net::MlsIncoming::Application { from, plaintext }) = mls.decrypt(&raw)
                {
                    if survivors.contains(&from) {
                        if let Ok(a) = serde_json::from_slice::<mesh::MeshAnnounce>(&plaintext) {
                            // validate BEFORE counting it: one malformed reply
                            // (no queue for us / bad hex) must degrade to "that
                            // survivor stayed silent", never fail the final
                            // assembly and nuke the honest survivors' links
                            let usable = a
                                .queues
                                .get(me)
                                .is_some_and(|h| h.addr().is_some() && h.wrap_key().is_some());
                            if usable {
                                announces.insert(from, a);
                            } else {
                                tracing::warn!(%from, "mesh reply carries no usable queue for us - ignored");
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                return Err("mesh re-join reply channel closed".to_string());
            }
            Err(_) => {
                for r in &readers {
                    r.abort(); // inbound readers only — safe to abort
                }
                // NOBODY answered: mesh-less recovery (option A) is honest.
                if announces.is_empty() {
                    return Err(format!(
                        "mesh re-join timed out: 0/{} survivors replied",
                        survivors.len()
                    ));
                }
                // SOME answered: keep their links. Those survivors have
                // already re-pointed and persisted their side — discarding
                // the whole mesh would leave them sending into queues nobody
                // ever subscribes (a durable blackhole pairing). The silent
                // rest stays unlinked until a later announce.
                tracing::warn!(
                    got = announces.len(),
                    want = survivors.len(),
                    "mesh re-join timed out - assembling the partial mesh"
                );
                break;
            }
        }
    }
    // assemble over the survivors that actually replied (all of them on the
    // happy path; the answering subset after a timeout)
    let inbound: BTreeMap<String, (Vec<molt_net::RcvQueue>, WrapKey)> = my_inbound
        .into_iter()
        .filter(|(m, _)| announces.contains_key(m))
        .collect();
    let links = mesh::assemble_mesh(me, &inbound, &announces)?;
    Ok(links.iter().map(molt_net::PeerLink::to_mesh).collect())
}

impl State {
    /// Spawn the founder's post-founding **mesh bootstrap** off the actor:
    /// keep the star's transport alive, exchange mesh announcements with the
    /// members (relaying between them), and report the assembled mesh + the
    /// post-bootstrap group back as [`Command::NetMeshReady`] for the actor
    /// to persist. Members' ciphertext is routed in via `founder_mesh_in`.
    pub(crate) fn spawn_founder_bootstrap(
        &mut self,
        ritual: &RitualRuntime,
        mls: molt_net::MlsMember,
        founder_name: String,
        peers: Vec<MemberId>,
    ) {
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        // the just-materialized founded workspace the mesh will persist into
        let Some(ws_id) = self.active.as_ref().map(|a| a.id.clone()) else {
            return;
        };
        let generation = ritual.generation();
        let transport = ritual.transport();
        let seat_replies = ritual.seat_replies();
        let (ct_tx, ct_rx) = mpsc::unbounded_channel::<(u32, String)>();
        // members' NetMeshAnnounced ciphertext flows into this bootstrap
        self.founder_mesh_in = Some((generation, ws_id, ct_tx));
        // keep the transport for the runtime supervisor (built once the mesh
        // is assembled — on loopback its queues can't be rebuilt from state)
        self.runtime_transport = Some(ritual.transport());
        let mls_arc = Arc::new(Mutex::new(mls));
        tokio::spawn(async move {
            match founder_bootstrap(
                founder_name,
                peers,
                transport,
                seat_replies,
                mls_arc.clone(),
                ct_rx,
            )
            .await
            {
                Ok(mesh) => {
                    // snapshot AFTER the announcements advanced the ratchet,
                    // so a reopened supervisor is in sync with the members
                    let snap = mls_arc.lock().ok().and_then(|m| m.snapshot().ok());
                    let Some(mls_snapshot) = snap else {
                        tracing::warn!("founder bootstrap: post-bootstrap snapshot failed");
                        return;
                    };
                    let cmd = Command::NetMeshReady {
                        mesh,
                        mls_snapshot,
                        generation: Some(generation),
                    };
                    let (reply, _rx) = tokio::sync::oneshot::channel();
                    let _ = cmd_tx.send(Envelope { cmd, reply }).await;
                }
                Err(e) => tracing::warn!(error = %e, "founder mesh bootstrap failed"),
            }
        });
    }

    /// A member's post-founding mesh handover reached the founder over the
    /// star. Forward the MLS ciphertext into the running bootstrap (which
    /// relays it to the other members and assembles the founder's own mesh).
    /// Dropped when no bootstrap is running or the incarnation is stale.
    pub(crate) fn cmd_net_mesh_announced(
        &mut self,
        seat: u32,
        ct: String,
        generation: Option<u64>,
    ) -> Result<molt_core::Reply, molt_core::MoltError> {
        if let Some((gen, _id, tx)) = &self.founder_mesh_in {
            if generation.is_none() || generation == Some(*gen) {
                let _ = tx.send((seat, ct));
            }
        }
        Ok(molt_core::Reply::Ack)
    }

    /// The founder's mesh bootstrap finished: persist the assembled direct
    /// mesh + the post-bootstrap group into the founded workspace's transport
    /// state, over the pre-bootstrap snapshot. Dropped if the workspace is no
    /// longer the one we bootstrapped (a later context switch).
    pub(crate) fn cmd_net_mesh_ready(
        &mut self,
        mesh: Vec<molt_core::MeshLink>,
        mls_snapshot: Vec<u8>,
        generation: Option<u64>,
    ) -> Result<molt_core::Reply, molt_core::MoltError> {
        // persist only when this is still the same bootstrap AND its founded
        // workspace is still the active one — so a late bootstrap that
        // finished after a context switch can never clobber another workspace
        let same_ctx = match (&self.founder_mesh_in, &self.active) {
            (Some((g, id, _)), Some(active)) => {
                Some(*g) == generation && *id == active.id
            }
            _ => false,
        };
        if !same_ctx {
            return Ok(molt_core::Reply::Ack);
        }
        self.founder_mesh_in = None;
        let peers = mesh.len();
        // reuse the ritual transport for the runtime supervisor AND export
        // its queue credentials: the receive keys of the star+mesh queues
        // live only in this transport's memory. Persisting them NOW — not
        // only on clean close — is what makes a hard kill after this point
        // survivable (2026-07-19 incident).
        let transport = self.runtime_transport.take();
        if let Some(active) = &self.active {
            let creds = transport.as_ref().and_then(|t| t.export_creds());
            // merge the founder's post-bootstrap MLS + assembled mesh +
            // queue creds into transport.state (a LIVE merge: the writer
            // owns the file, and plain cursor saves carry only the cursor
            // maps)
            if !active.handle.persist_mesh_crypto_blocking(
                Some(mls_snapshot.clone()),
                creds,
                mesh.clone(),
            ) {
                tracing::error!("the group ratchet did not reach the disk");
            }
        }
        // stand the runtime supervisor up over the direct mesh, reusing the
        // ritual transport (it owns the mesh queues' receive credentials), so
        // the founder can chat peer-to-peer the moment the mesh is assembled
        if let Some(transport) = transport {
            if let Some(net) = self.build_real_net(transport, &mesh, &mls_snapshot) {
                self.teardown_net();
                self.net = Some(net);
            }
        }
        // surface it on the founding log (still present until CreateFinish) —
        // the direct mesh is up, the star can be let go
        if self.session.create.run.outcome == 1 {
            self.session
                .create
                .run
                .log
                .push(format!("✓ direct mesh established · {peers} peer(s)"));
            self.emit_session(molt_core::SessionScope::Create);
        }
        Ok(molt_core::Reply::Ack)
    }
}
