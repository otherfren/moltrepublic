// SPDX-License-Identifier: GPL-3.0-or-later

//! The founding ritual (transport concept §3.3): the republic is
//! constituted *before* any workspace touches the disk.
//!
//! The founder mints one single-use invite per future member and provisions a
//! transport queue per seat. Each member — a real remote node over SMP, or a
//! simulated loopback node in the offline test seam — derives its own identity
//! key from its own recovery phrase, activates the link (`JoinRequest`,
//! MAC-bound to the ticket), and later signs the final canonical roster table
//! (`SealSigned`). Only when every seat is sealed does the founder write the
//! `Founded` genesis — carrying the complete identity table and all n
//! attestations — and distribute the sealed roster so every member writes its
//! own workspace. The roster is salted by a neutral, content-derived
//! [`molt_storage::republic_id`], so no member's seed privileges the founder.
//!
//! Every leg lands in the wizard's live log as a real event.

use molt_core::{Command, MemberId, MemberIdentity, RosterAttestation};
use molt_net::smp::tls::Dialer;
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::supervisor;
use molt_net::{
    invite, msg_id, Delivery, LoopbackHub, LoopbackTransport, NetError, PaddedBlock, QueueId,
    QueuePair, RcvQueue, SndQueueAddr, Transport, WrapKey,
};
use molt_storage::SigningKey;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::{Envelope, State};

/// How long a node waits for its peers' mesh announcements before giving up and
/// entering without a direct mesh (best-effort bootstrap — see the join gating
/// decision). Generous: at founding time every peer is present, so the exchange
/// normally completes in well under a second; this only bounds a failed peer.
pub(crate) const MESH_BOOTSTRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Build the runtime SMP transport from settings (Track B Stage 2 redundancy):
/// spread inbound queues across the configured server list. `prepend` is a server
/// that MUST be reachable first (a joiner/recovery invite server the ritual talks
/// to); it is placed first and de-duplicated against the config list. The result
/// is capped at [`molt_net::MESH_REDUNDANCY_CAP`] servers.
///
/// The list is this node's OWN redundancy — not a requirement on the others:
/// `SmpTransport::route` dials a queue's own (pinned) server even when it is not
/// in this list, so every member picks its inbound servers independently and a
/// leg keeps both copies either way.
pub(crate) fn build_smp_transport(
    settings: &molt_core::SessionSettings,
    dialer: Dialer,
    prepend: Option<SmpServer>,
) -> Result<SmpTransport, String> {
    let mut servers: Vec<SmpServer> = Vec::new();
    if let Some(s) = prepend {
        servers.push(s);
    }
    for url in settings.smp_server_list(&molt_config::default_public_smp()) {
        if let Ok(s) = SmpServer::parse(url.trim()) {
            if !servers.iter().any(|e| e.render() == s.render()) {
                servers.push(s);
            }
        }
    }
    if servers.is_empty() {
        return Err("no SMP server configured".to_string());
    }
    servers.truncate(molt_net::MESH_REDUNDANCY_CAP.max(1));
    Ok(SmpTransport::with_dialer_multi(servers, dialer))
}

/// A fresh SMP transport for **resuming** a persisted mesh on reopen: build it
/// for the mesh's server(s) and re-adopt the persisted queue credentials (recv
/// keys so it can subscribe to our inbound queues, secured sender keys so it can
/// keep sending without a rejected re-SKEY). `None` for a loopback mesh (empty
/// server — its in-memory queues cannot outlive the process) or bad creds. Track
/// B Stage 2: gathers ALL distinct servers the persisted mesh uses (primary +
/// extra, both send and receive sides) so a resumed multi-server mesh
/// re-subscribes on every one, not a single collapsed server. The list is
/// truncated to the redundancy cap because it seeds `create_queue`'s spread; a
/// leg on a server beyond the cut is NOT lost — `SmpTransport::route` dials it
/// as a pinned dynamic server.
pub(crate) fn reopen_transport(
    mesh: &[molt_core::MeshLink],
    creds: &[u8],
    dialer: Dialer,
) -> Option<RitualTransport> {
    let mut servers: Vec<SmpServer> = Vec::new();
    let mut push = |raw: &str| {
        let raw = raw.trim();
        if !raw.is_empty() {
            if let Ok(s) = SmpServer::parse(raw) {
                if !servers.iter().any(|e| e.render() == s.render()) {
                    servers.push(s);
                }
            }
        }
    };
    for l in mesh {
        push(&l.snd_server);
        push(&l.rcv_server);
        for x in &l.snd_extra {
            push(&x.server);
        }
        for x in &l.rcv_extra {
            push(&x.server);
        }
    }
    if servers.is_empty() {
        return None; // loopback mesh (empty servers) — nothing to resume
    }
    servers.truncate(molt_net::MESH_REDUNDANCY_CAP.max(1));
    let t = SmpTransport::with_dialer_multi(servers, dialer);
    t.import_creds(creds);
    Some(RitualTransport::Smp(t))
}

/// The transport a founding ritual runs over. The in-app demo founds over
/// the in-process loopback hub (with simulated members); a real founding
/// runs over the configured SMP server. One enum so the founder side — the
/// recv loops, `maybe_seal`, teardown — is written once and dispatches at
/// runtime, and so [`InviteMaterial`] and [`RitualRuntime`] have a single
/// concrete transport type.
#[doc(hidden)]
#[derive(Clone)]
pub enum RitualTransport {
    /// In-process hub (the demo's simulated members). The held transport
    /// keeps the hub's Arc alive, so the ritual's queues live with it.
    Loopback(LoopbackTransport),
    /// A real SMP server.
    Smp(SmpTransport),
}

impl Transport for RitualTransport {
    async fn create_queue(&self) -> Result<QueuePair, NetError> {
        match self {
            RitualTransport::Loopback(t) => t.create_queue().await,
            RitualTransport::Smp(t) => t.create_queue().await,
        }
    }

    async fn send(&self, addr: &SndQueueAddr, block: PaddedBlock) -> Result<(), NetError> {
        match self {
            RitualTransport::Loopback(t) => t.send(addr, block).await,
            RitualTransport::Smp(t) => t.send(addr, block).await,
        }
    }

    async fn subscribe(&self, q: &RcvQueue) -> Result<mpsc::Receiver<Delivery>, NetError> {
        match self {
            RitualTransport::Loopback(t) => t.subscribe(q).await,
            RitualTransport::Smp(t) => t.subscribe(q).await,
        }
    }

    async fn delete_queue(&self, q: &RcvQueue) -> Result<(), NetError> {
        match self {
            RitualTransport::Loopback(t) => t.delete_queue(q).await,
            RitualTransport::Smp(t) => t.delete_queue(q).await,
        }
    }

    fn export_creds(&self) -> Option<Vec<u8>> {
        match self {
            RitualTransport::Loopback(t) => t.export_creds(),
            RitualTransport::Smp(t) => t.export_creds(),
        }
    }

    fn import_creds(&self, creds: &[u8]) {
        match self {
            RitualTransport::Loopback(t) => t.import_creds(creds),
            RitualTransport::Smp(t) => t.import_creds(creds),
        }
    }

    fn redundancy(&self) -> usize {
        // delegate to the inner transport (an SmpTransport spread across N
        // servers returns N) — WITHOUT this, RitualTransport would use the
        // trait default (1) and every rotate/bootstrap would mint a single
        // queue, silently stripping Track B redundancy.
        match self {
            RitualTransport::Loopback(t) => t.redundancy(),
            RitualTransport::Smp(t) => t.redundancy(),
        }
    }
}

/// One seat's transport material, held by the founder for the ritual's
/// lifetime: the ticket it verifies against, the reply queue the member
/// advertised (learned from its JoinRequest — in SMP the member owns the
/// queue it receives on), and (once collected) the member's anchored
/// identity.
struct SeatRuntime {
    ticket: String,
    /// founder → member: where the canonical table goes. Learned from the
    /// member's JoinRequest (`None` until the member activates the link).
    reply_snd: Option<SndQueueAddr>,
    reply_wrap: Option<WrapKey>,
    /// The member's identity, once their JoinRequest verified.
    identity: Option<MemberIdentity>,
    /// The member's MLS KeyPackage (hex of the wire bytes), delivered with the
    /// JoinRequest — the founder adds every seat's to the group at sealing.
    key_package: Option<String>,
    /// Whether this seat's seal signature was already accepted — a second
    /// (distinct) `SealSigned` must not push a duplicate attestation.
    sealed: bool,
}

/// The founder-side ritual runtime: the transport, the founder's own
/// identity, the seats, and the keepalives for the simulated joiners.
pub(crate) struct RitualRuntime {
    // for loopback the transport holds the hub's Arc, so keeping it alive
    // keeps every ritual queue alive; dropping the runtime tears it down.
    // `maybe_seal` sends the canonical table over this.
    transport: RitualTransport,
    /// The republic's **final** display name — the founder's provisional name
    /// until the deliberation step, then the ratified one. An input to the
    /// neutral [`molt_storage::republic_id`] (the roster salt).
    name: String,
    /// The deliberated free-text charter/agenda, set when the founder proposes
    /// it; empty until then. Bound into every member's seal signature.
    agenda: String,
    /// Whether the founder has proposed the charter (final name + agenda). The
    /// roster seals only once this is set AND every seat has joined — so the
    /// members ratify a concrete charter, never an empty placeholder. The pure
    /// sim seam pre-proposes (its founder does not deliberate).
    charter_proposed: bool,
    rule_m: u8,
    rule_n: u8,
    founder: MemberIdentity,
    founder_sk: SigningKey,
    seats: Vec<SeatRuntime>,
    generation: u64,
    /// Keepalives for the simulated members of the offline **test seam**
    /// ([`crate::__spawn_sim_founding`]); dropping the runtime stops them.
    /// Empty for a real (SMP) founding.
    _sim: Vec<mpsc::Sender<()>>,
    /// The founder's own recv tasks live on the transport; kept alive by it.
    seq: std::sync::atomic::AtomicU64,
}

impl RitualRuntime {
    /// The final identity table in ritual order: founder first, then seat
    /// order. `None` until every seat's key is collected.
    fn full_identities(&self) -> Option<Vec<MemberIdentity>> {
        let mut out = Vec::with_capacity(self.seats.len() + 1);
        out.push(self.founder.clone());
        for s in &self.seats {
            out.push(s.identity.clone()?);
        }
        Some(out)
    }

    /// The republic's neutral, content-derived id — the roster salt every
    /// member computes identically once all keys are in.
    pub(crate) fn republic_id(&self, identities: &[MemberIdentity]) -> String {
        molt_storage::republic_id(&self.name, self.rule_m, self.rule_n, identities)
    }

    /// The canonical bytes every member signs once the table is complete —
    /// binding the roster AND the deliberated charter (name via the republic id,
    /// agenda directly), so a signature is a ratification of exactly this
    /// constitution.
    fn canonical(&self, identities: &[MemberIdentity]) -> Vec<u8> {
        let rid = self.republic_id(identities);
        molt_core::roster_canonical_bytes(&rid, self.rule_m, self.rule_n, identities, &self.agenda)
    }

    fn next_msg_id(&self, tag: &str) -> molt_net::MsgId {
        let n = self
            .seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        msg_id("founder", tag, n)
    }

    /// Build the founder's MLS group at sealing: create the group with the
    /// founder as sole leaf, then add every seat from its advertised KeyPackage
    /// in one commit. Returns the founder's live [`MlsMember`] (to snapshot into
    /// its own `transport.state`) and the single Welcome (hex) that covers all
    /// added members — distributed with the genesis so each finishes the ritual
    /// already inside the group (concept §3.3). Every joined seat has a
    /// KeyPackage by sealing (the join is rejected without one).
    pub(crate) fn build_founder_mls(&self) -> Result<(molt_net::MlsMember, String), String> {
        let mut founder = molt_net::MlsMember::new(&self.founder_sk, &self.founder.member)
            .map_err(|e| e.to_string())?;
        founder.create_group().map_err(|e| e.to_string())?;
        let mut kps = Vec::with_capacity(self.seats.len());
        for (idx, s) in self.seats.iter().enumerate() {
            let hex = s
                .key_package
                .as_ref()
                .ok_or_else(|| format!("seat {} has no MLS key package", idx + 1))?;
            kps.push(hex::decode(hex).map_err(|e| e.to_string())?);
        }
        let welcome = founder.add_members(&kps).map_err(|e| e.to_string())?;
        Ok((founder, welcome.map(hex::encode).unwrap_or_default()))
    }

    /// Send the complete sealed roster + the MLS Welcome to every member's reply
    /// queue, so each writes its own genesis and joins the group. Fire-and-forget
    /// (a member already gone just misses it); every seat has a reply queue by
    /// the time this is called.
    pub(crate) fn distribute_genesis(&self, sealed_json: String, welcome: String) {
        let msg = invite::RitualMsg::Genesis {
            sealed: sealed_json,
            welcome,
        };
        let Ok(payload) = serde_json::to_vec(&msg) else {
            return;
        };
        for (idx, s) in self.seats.iter().enumerate() {
            let (Some(addr), Some(wrap)) = (s.reply_snd.clone(), s.reply_wrap.clone()) else {
                continue;
            };
            let transport = self.transport.clone();
            let id = self.next_msg_id(&format!("genesis-{idx}"));
            let payload = payload.clone();
            tokio::spawn(async move {
                let _ = supervisor::send_framed(&transport, &addr, &wrap, id, &payload).await;
            });
        }
    }

    /// The final identity table (founder first); only valid once every
    /// seat is sealed, which the caller has already checked.
    pub(crate) fn sealed_identities(&self) -> Vec<MemberIdentity> {
        self.full_identities().unwrap_or_else(|| vec![self.founder.clone()])
    }

    /// The founder's signing key (for the founder's own attestation).
    pub(crate) fn founder_sk(&self) -> &SigningKey {
        &self.founder_sk
    }

    /// A clone of the ritual transport — keeping it alive keeps the founding
    /// star (and its queues) up for the post-founding mesh bootstrap.
    pub(crate) fn transport(&self) -> RitualTransport {
        self.transport.clone()
    }

    /// This ritual's incarnation (the bootstrap's late results are bound to it).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Each joined seat's reply queue `(seat index, send address, wrap key)` —
    /// where the founder sends its own + relayed mesh announcements. A seat with
    /// no reply queue (never joined) is skipped.
    pub(crate) fn seat_replies(&self) -> Vec<(u32, SndQueueAddr, WrapKey)> {
        self.seats
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let seat = u32::try_from(i).unwrap_or(u32::MAX);
                Some((seat, s.reply_snd.clone()?, s.reply_wrap.clone()?))
            })
            .collect()
    }
}

impl State {
    /// Begin the founding ritual: derive the founder's identity, mint the
    /// invites, open the per-seat transport, and start the simulated
    /// members. Returns the invite preview links (for the seat rows).
    pub(crate) fn start_ritual(
        &mut self,
        name: &str,
        founder_name: &str,
        rule_m: u8,
        rule_n: u8,
        seed_phrase: &str,
    ) -> Result<Vec<String>, String> {
        let entropy = molt_storage::seed_entropy(seed_phrase).map_err(|e| e.to_string())?;
        let ws_id = molt_storage::derive_workspace_id(&entropy, founder_name);
        let (founder_sk, founder_pk) = molt_storage::derive_identity_key(&entropy, &ws_id);
        let founder = MemberIdentity {
            member: founder_name.to_string(),
            identity_pk: founder_pk,
        };
        self.net_generation += 1;
        let generation = self.net_generation;
        let seat_count = usize::from(rule_n).saturating_sub(1);

        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err("engine stopped".to_string());
        };
        // manual mode (the two-instance dev test / a real founding): don't
        // spawn simulated members — hand the invite material out so a second
        // engine runs the member side itself
        let manual = self.ritual_material_sink.is_some();

        // tickets, links and seats are set up synchronously — the ticket is
        // the link's secret, minted without any I/O. Queue creation is the
        // only async part (real on SMP), handled per transport below.
        let mut seats = Vec::with_capacity(seat_count);
        let mut links = Vec::with_capacity(seat_count);
        let mut seat_setup = Vec::with_capacity(seat_count);
        for seat in 0..seat_count {
            let seat_u32 = u32::try_from(seat).unwrap_or(u32::MAX);
            let ticket = invite::mint_ticket().map_err(|e| e.to_string())?;
            let invite_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
            links.push(
                molt_core::InviteInfo {
                    republic: name.to_string(),
                    threshold: rule_m,
                    members: rule_n,
                    inviter: founder_name.to_string(),
                    ticket: ticket[..10].to_string(),
                }
                .render(),
            );
            seat_setup.push((seat_u32, ticket.clone(), invite_wrap));
            seats.push(SeatRuntime {
                ticket,
                // the member advertises its reply queue in the JoinRequest
                reply_snd: None,
                reply_wrap: None,
                identity: None,
                key_package: None,
                sealed: false,
            });
        }

        // A real founding runs over the configured SMP server: the in-app
        // founding (no test sink, no sim flag) always does, and so does the
        // manual-over-SMP dev seam. Real remote members join over it. Only the
        // offline dev seams — loopback simulated members, or loopback manual —
        // stay on the in-process hub.
        let use_smp = self.ritual_over_smp || (!manual && !self.ritual_sim);
        let mut sim = Vec::new();
        let transport = if use_smp {
            // fail-closed: resolve the dialer from settings; a TorMisconfigured
            // aborts the founding with the reason and sets the health pill.
            let dialer = match self.resolve_dialer() {
                Ok(dialer) => dialer,
                Err(reason) => {
                    self.emit_session(molt_core::SessionScope::Full);
                    return Err(reason);
                }
            };
            // Track B Stage 2: the founder's runtime transport spans the
            // configured server list, so the founding mesh mints its inbound
            // queues across N servers (N=1 for a single-server config, unchanged).
            let transport = RitualTransport::Smp(build_smp_transport(
                &self.session.settings,
                dialer,
                None,
            )?);
            // SMP queue creation is async: provision off the actor, wire each
            // seat's recv loop, then hand the material out
            spawn_smp_provisioning(
                transport.clone(),
                seat_setup,
                generation,
                cmd_tx.clone(),
                self.ritual_material_sink.clone(),
                name.to_string(),
                founder_name.to_string(),
                rule_m,
                rule_n,
            );
            transport
        } else {
            // loopback dev seams: the hub creates queues synchronously. The
            // manual seam hands the per-seat material to the waiting
            // instance(s); the sim seam spawns simulated members.
            let hub = LoopbackHub::calm();
            let transport = RitualTransport::Loopback(hub.transport());
            let mut materials = Vec::with_capacity(seat_count);
            for (seat_u32, ticket, invite_wrap) in &seat_setup {
                let invite_q = hub.create_queue_blocking().map_err(|e| e.to_string())?;
                spawn_founder_recv(
                    transport.clone(),
                    invite_q.rcv.clone(),
                    invite_wrap.clone(),
                    *seat_u32,
                    generation,
                    cmd_tx.downgrade(),
                );
                let material = InviteMaterial {
                    seat: *seat_u32,
                    transport: transport.clone(),
                    invite_snd: invite_q.snd.clone(),
                    invite_wrap: invite_wrap.clone(),
                    ticket: ticket.clone(),
                };
                if self.ritual_sim {
                    sim.push(spawn_sim_member(material)?);
                } else {
                    materials.push(material);
                }
            }
            if let Some(sink) = &self.ritual_material_sink {
                let _ = sink.send(materials);
            }
            // `hub` drops here; `transport` (and its task clones) hold the
            // shared Arc, so every ritual queue stays alive until the runtime
            // is dropped
            transport
        };

        self.net_ritual = Some(RitualRuntime {
            transport,
            name: name.to_string(),
            agenda: String::new(),
            // the automated sim seam has no human founder to deliberate, so it
            // pre-proposes and seals on all-joined (its name, empty agenda);
            // every real founding waits for the founder's explicit charter
            charter_proposed: self.ritual_sim,
            rule_m,
            rule_n,
            founder,
            founder_sk,
            seats,
            generation,
            _sim: sim,
            seq: std::sync::atomic::AtomicU64::new(0),
        });
        Ok(links)
    }

    /// Tear the ritual down (cancel or completion): drops the hub, its
    /// queues and the simulated members. Also reaps any in-flight founder mesh
    /// bootstrap — dropping its `ct_tx` closes the task's inbound channel, which
    /// cascades the whole bootstrap to shut down and release the founding star
    /// (an abandoned founding must not leave a task blocked forever).
    pub(crate) fn teardown_ritual(&mut self) {
        self.net_ritual = None;
        self.founder_mesh_in = None;
        self.runtime_transport = None;
    }

    /// Whether a ritual command's incarnation is still current: the ritual
    /// must still be installed AND still be the live incarnation. Binding to
    /// `net_generation` (bumped by a new founding, or by opening a workspace /
    /// starting the mesh) means an abandoned founding's late seals are dropped
    /// even on paths that switch context without an explicit teardown.
    fn ritual_generation_current(&self, generation: Option<u64>) -> bool {
        match generation {
            None => true,
            Some(g) => {
                g == self.net_generation
                    && self.net_ritual.as_ref().is_some_and(|r| r.generation == g)
            }
        }
    }
}

/// The founder's recv loop on one invite queue: unwrap, reassemble, parse
/// a [`invite::RitualMsg`], and issue the matching internal command. Runs
/// over whichever [`RitualTransport`] the ritual chose.
fn spawn_founder_recv(
    transport: RitualTransport,
    rcv: RcvQueue,
    wrap: WrapKey,
    seat: u32,
    generation: u64,
    cmd_tx: mpsc::WeakSender<Envelope>,
) {
    // WEAK sender, upgraded per message (the ticker rule): this recv loop
    // outlives the ritual — it blocks on the star queue for as long as the
    // transport lives, so a strong sender would keep a dropped engine's
    // actor (and its writer thread + workspace flock) alive forever. The
    // hard-kill tests drop the handle and wait for exactly that release.
    tokio::spawn(async move {
        let Ok(mut rx) = transport.subscribe(&rcv).await else {
            return;
        };
        let mut reasm = molt_net::Reassembler::new();
        while let Some(delivery) = rx.recv().await {
            let Ok(plain) = molt_net::wrap::unwrap_block(&wrap, &delivery.block) else {
                delivery.ack.ack();
                continue;
            };
            let outcome = reasm.push(&plain);
            delivery.ack.ack();
            let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
                continue;
            };
            let Ok(msg) = serde_json::from_slice::<invite::RitualMsg>(&bytes) else {
                continue;
            };
            let cmd = match msg {
                invite::RitualMsg::Join(j) => Command::NetJoinRequested {
                    seat,
                    member: j.name,
                    identity_pk: j.identity_pk,
                    proof: j.mac,
                    // the member's reply-queue handover, opaque to core
                    reply: j
                        .reply
                        .as_ref()
                        .and_then(|r| serde_json::to_string(r).ok())
                        .unwrap_or_default(),
                    key_package: j.key_package,
                    generation: Some(generation),
                },
                invite::RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat,
                    sig: s.sig,
                    generation: Some(generation),
                },
                invite::RitualMsg::Declined { .. } => Command::NetJoinDeclined {
                    seat,
                    generation: Some(generation),
                },
                // a member's post-founding mesh handover — hand it to the
                // founder's running bootstrap (the handler forwards + relays)
                invite::RitualMsg::MeshAnnounce { ct } => Command::NetMeshAnnounced {
                    seat,
                    ct,
                    generation: Some(generation),
                },
                // founder→member only, or not a founding-queue message (Recover
                // and Welcome belong on the recovery queue / rejoiner reply queue):
                invite::RitualMsg::JoinAccepted { .. }
                | invite::RitualMsg::Seal { .. }
                | invite::RitualMsg::Genesis { .. }
                | invite::RitualMsg::LinkSpent { .. }
                | invite::RitualMsg::Recover(_)
                | invite::RitualMsg::Welcome { .. } => continue,
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let Some(tx) = cmd_tx.upgrade() else {
                return; // engine stopped — so do we
            };
            if tx.send(Envelope { cmd, reply }).await.is_err() {
                return;
            }
        }
    });
}

/// Map a returning member's [`invite::RecoverRequest`] to the internal
/// [`Command::NetRecoverRequested`] — the coordinator recv loop's one decode.
/// The reply-queue handover is re-serialized to the opaque string core carries.
#[cfg_attr(not(test), allow(dead_code))] // wired by the recovery link-mint increment
pub(crate) fn recover_command(r: invite::RecoverRequest, generation: u64) -> Command {
    Command::NetRecoverRequested {
        member: r.member,
        identity_pk: r.identity_pk,
        key_package: r.key_package,
        ticket: r.ticket,
        seat_proof: r.seat_proof,
        reply: r
            .reply
            .as_ref()
            .and_then(|h| serde_json::to_string(h).ok())
            .unwrap_or_default(),
        generation: Some(generation),
    }
}

/// The recovery coordinator's recv loop on its recovery queue — the twin of
/// [`spawn_founder_recv`]. It accepts a returning member's
/// [`invite::RitualMsg::Recover`] and issues [`Command::NetRecoverRequested`]
/// (the engine verifies the seat proof + proposes re-admission); any other
/// message on this queue is ignored.
pub(crate) fn spawn_coordinator_recv(
    transport: RitualTransport,
    rcv: RcvQueue,
    wrap: WrapKey,
    generation: u64,
    cmd_tx: mpsc::WeakSender<Envelope>,
) {
    // WEAK sender, upgraded per message — same rule as spawn_founder_recv:
    // this loop lives as long as the transport, and must never keep a
    // dropped engine's actor (writer thread, workspace flock) alive.
    tokio::spawn(async move {
        let Ok(mut rx) = transport.subscribe(&rcv).await else {
            return;
        };
        let mut reasm = molt_net::Reassembler::new();
        while let Some(delivery) = rx.recv().await {
            let Ok(plain) = molt_net::wrap::unwrap_block(&wrap, &delivery.block) else {
                delivery.ack.ack();
                continue;
            };
            let outcome = reasm.push(&plain);
            delivery.ack.ack();
            let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
                continue;
            };
            // a recovery request, or — after the re-key — the rejoiner's mesh
            // announce (dynamic mesh membership); anything else is dropped
            let cmd = match serde_json::from_slice::<invite::RitualMsg>(&bytes) {
                Ok(invite::RitualMsg::Recover(r)) => recover_command(r, generation),
                Ok(invite::RitualMsg::MeshAnnounce { ct }) => Command::NetRecoverAnnounced {
                    ct,
                    generation: Some(generation),
                },
                _ => continue,
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let Some(tx) = cmd_tx.upgrade() else {
                return; // engine stopped — so do we
            };
            if tx.send(Envelope { cmd, reply }).await.is_err() {
                return;
            }
        }
    });
}

/// Provision the founder's per-seat invite queues over SMP **off the actor**
/// — SMP `create_queue` is a live NEW round-trip, which the synchronous
/// command handler must not block on. Each seat gets its recv loop wired,
/// and the full per-seat material is handed to the waiting instance once
/// every queue is up. Manual mode only: a real founding invites real remote
/// members, so there are no simulated joiners here.
#[allow(clippy::too_many_arguments)]
fn spawn_smp_provisioning(
    transport: RitualTransport,
    seat_setup: Vec<(u32, String, WrapKey)>,
    generation: u64,
    cmd_tx: mpsc::Sender<Envelope>,
    sink: Option<std::sync::mpsc::Sender<Vec<InviteMaterial>>>,
    republic: String,
    inviter: String,
    rule_m: u8,
    rule_n: u8,
) {
    tokio::spawn(async move {
        tracing::debug!(seats = seat_setup.len(), "SMP invite-queue provisioning started");
        let mut materials = Vec::with_capacity(seat_setup.len());
        for (seat, ticket, invite_wrap) in seat_setup {
            let invite_q = match transport.create_queue().await {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!(seat, error = %e, "SMP invite-queue provisioning failed");
                    // tell the founder the founding cannot proceed instead of
                    // leaving the create wizard stuck with no links, forever
                    let (reply, _rx) = tokio::sync::oneshot::channel();
                    let _ = cmd_tx
                        .send(Envelope {
                            cmd: Command::NetRitualFailed {
                                error: format!("could not reach the SMP server: {e}"),
                                generation: Some(generation),
                            },
                            reply,
                        })
                        .await;
                    return;
                }
            };
            spawn_founder_recv(
                transport.clone(),
                invite_q.rcv.clone(),
                invite_wrap.clone(),
                seat,
                generation,
                cmd_tx.downgrade(),
            );
            // the real, joinable link: now that the queue exists, it carries
            // the full transport handover. Report it so the founder's session
            // (its GUI) shows a link a separate node can actually use.
            let link = FoundingInvite {
                info: molt_core::InviteInfo {
                    republic: republic.clone(),
                    threshold: rule_m,
                    members: rule_n,
                    inviter: inviter.clone(),
                    ticket: ticket.clone(),
                },
                server: invite_q.snd.server.clone(),
                queue_id: hex::encode(&invite_q.snd.id.0),
                wrap: hex::encode(invite_wrap.to_bytes()),
                seat,
            }
            .render();
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx
                .send(Envelope {
                    cmd: Command::NetRitualLinkReady {
                        seat,
                        link,
                        generation: Some(generation),
                    },
                    reply,
                })
                .await;
            materials.push(InviteMaterial {
                seat,
                transport: transport.clone(),
                invite_snd: invite_q.snd,
                invite_wrap,
                ticket,
            });
        }
        if let Some(sink) = sink {
            let _ = sink.send(materials);
        }
    });
}

/// One founding invite's full transport handover — everything a member's
/// node needs to activate and seal (transport concept §3.3: the payload
/// the `molt://invite/…` link will carry in-band once T3 encodes it).
/// Exposed for the two-instance dev test, where a *second* engine runs the
/// member side against the founder's hub.
#[doc(hidden)]
#[derive(Clone)]
pub struct InviteMaterial<T: molt_net::Transport = RitualTransport> {
    /// The seat this invite fills (0-based).
    pub seat: u32,
    /// The transport the founder reached the member over — the loopback hub
    /// for the demo mesh, the SMP server for a real founding. A genuinely
    /// separate instance uses its *own* transport and only reads the address
    /// / wrap / ticket below; `run_ritual_member` is generic over `T`.
    pub transport: T,
    /// member → founder queue (JoinRequest, then SealSigned).
    pub invite_snd: SndQueueAddr,
    pub invite_wrap: WrapKey,
    /// The single-use ticket.
    pub ticket: String,
}

/// A full founding-invite link: the [`molt_core::InviteInfo`] display preview
/// plus the transport handover a *separate node* (a second moltd, the GUI
/// join flow) needs to join a founding over SMP. Rendered as the preview link
/// with one extra hex-wrapped segment, so `InviteInfo::parse` still reads the
/// preview and a joining node reads the whole thing here.
#[doc(hidden)]
pub struct FoundingInvite {
    /// The display preview (republic, m/n, inviter, ticket).
    pub info: molt_core::InviteInfo,
    /// The founder's SMP server (`smp://fingerprint@host`).
    pub server: String,
    /// The invite queue's send-side id (where the member sends its join), hex.
    pub queue_id: String,
    /// The invite queue's wrap key, hex.
    pub wrap: String,
    /// The seat this invite fills.
    pub seat: u32,
}

impl FoundingInvite {
    /// Render the full joinable link: the preview link plus one extra path
    /// segment, `hex(server\nqueue_id\nwrap\nseat)`. Hex keeps the handover a
    /// single URL-safe segment (the server url's `//`/`@`/`=` don't leak).
    pub fn render(&self) -> String {
        let payload = format!(
            "{}\n{}\n{}\n{}",
            self.server, self.queue_id, self.wrap, self.seat
        );
        format!("{}/{}", self.info.render(), hex::encode(payload))
    }

    /// Parse a full founding link; `None` if it lacks a valid handover.
    pub fn parse(link: &str) -> Option<FoundingInvite> {
        let info = molt_core::InviteInfo::parse(link)?;
        let (_, blob) = link.trim().rsplit_once('/')?;
        let payload = String::from_utf8(hex::decode(blob).ok()?).ok()?;
        let mut fields = payload.split('\n');
        let server = fields.next()?.to_string();
        let queue_id = fields.next()?.to_string();
        let wrap = fields.next()?.to_string();
        let seat: u32 = fields.next()?.parse().ok()?;
        Some(FoundingInvite {
            info,
            server,
            queue_id,
            wrap,
            seat,
        })
    }
}

/// Verify a distributed sealed roster before trusting it: the republic id
/// must be the neutral content-derived value, every attestation must verify
/// against its member's anchored key over the canonical table, and every
/// member must have signed (n identities, n attestations).
pub(crate) fn verify_sealed_roster(s: &molt_core::SealedRoster) -> Result<(), String> {
    let rid = molt_storage::republic_id(&s.name, s.rule_m, s.rule_n, &s.identities);
    if rid != s.republic_id {
        return Err("republic id does not match the roster content".to_string());
    }
    if s.attestations.len() != s.identities.len() {
        return Err("roster is not fully signed by every member".to_string());
    }
    // recompute over the sealed charter too: if the founder put a different
    // name/agenda in the genesis than the members ratified, their signatures
    // (made over the Seal's table) fail here — the charter is tamper-evident
    let table =
        molt_core::roster_canonical_bytes(&s.republic_id, s.rule_m, s.rule_n, &s.identities, &s.agenda);
    for att in &s.attestations {
        let id = s
            .identities
            .iter()
            .find(|i| i.member == att.member)
            .ok_or_else(|| format!("attestation for unknown member {}", att.member))?;
        if !molt_storage::identity_verify(&id.identity_pk, &table, &att.sig) {
            return Err(format!("attestation for {} does not verify", att.member));
        }
    }
    Ok(())
}

/// The canonical bytes a **recovery seat proof** signs (concept §3.3):
/// domain-separated `ticket ‖ key_package ‖ republic_id`. The rejoiner signs it
/// with the identity key it re-derived from its recovery phrase; the approver
/// verifies against the seat's *anchored* public key (from the genesis identity
/// table). So a leaked recovery link alone — the transport path + the ticket —
/// cannot answer the challenge: only the phrase re-derives the signing key, and
/// the ticket is spent on first use (replay is dead). Binding the KeyPackage
/// ties the proof to exactly the credential being re-added to the group, and the
/// republic id to exactly this workspace.
pub(crate) fn seat_proof_bytes(ticket: &str, key_package_hex: &str, republic_id: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(
        20 + ticket.len() + key_package_hex.len() + republic_id.len() + 2,
    );
    m.extend_from_slice(b"molt-seat-proof-v1\0");
    m.extend_from_slice(ticket.as_bytes());
    m.push(0);
    m.extend_from_slice(key_package_hex.as_bytes());
    m.push(0);
    m.extend_from_slice(republic_id.as_bytes());
    m
}

/// The **rejoiner** builds its seat proof: sign the canonical bytes with the
/// identity key re-derived from its recovery phrase. Returns the signature (hex).
pub fn make_seat_proof(
    identity_sk: &molt_storage::SigningKey,
    ticket: &str,
    key_package_hex: &str,
    republic_id: &str,
) -> String {
    molt_storage::identity_sign(
        identity_sk,
        &seat_proof_bytes(ticket, key_package_hex, republic_id),
    )
}

/// The **approver** verifies a seat proof against the seat's *anchored* public
/// key (from the genesis identity table). A leaked recovery link (transport +
/// ticket) without the phrase cannot produce a signature that verifies here, and
/// a request that fails this check never reaches the approval prompt (concept
/// §3.3).
pub fn verify_seat_proof(
    anchored_pk: &str,
    ticket: &str,
    key_package_hex: &str,
    republic_id: &str,
    sig_hex: &str,
) -> bool {
    molt_storage::identity_verify(
        anchored_pk,
        &seat_proof_bytes(ticket, key_package_hex, republic_id),
        sig_hex,
    )
}

/// Verify a `Seal` proposal before ratifying it, and return the exact canonical
/// bytes to sign. The republic id must be the content-derived value (no forged
/// salt), and our own `(name, key)` must be in the roster — otherwise a founder
/// could have us ratify a constitution we are not part of. Recomputing the table
/// here (rather than trusting an opaque blob) is what makes the signature a
/// ratification of exactly the name + agenda + roster the member is shown.
pub(crate) fn verify_seal_proposal(
    proposal: &molt_core::SealedRoster,
    name: &str,
    pk: &str,
) -> Result<Vec<u8>, String> {
    let rid = molt_storage::republic_id(
        &proposal.name,
        proposal.rule_m,
        proposal.rule_n,
        &proposal.identities,
    );
    if rid != proposal.republic_id {
        return Err("proposed republic id does not match its roster".to_string());
    }
    if !proposal
        .identities
        .iter()
        .any(|i| i.member == name && i.identity_pk == pk)
    {
        return Err("the proposed roster does not anchor our own (name, key)".to_string());
    }
    Ok(molt_core::roster_canonical_bytes(
        &proposal.republic_id,
        proposal.rule_m,
        proposal.rule_n,
        &proposal.identities,
        &proposal.agenda,
    ))
}

/// The verified outcome of joining a founding: the sealed roster to write, and
/// the joiner's own MLS group snapshot to seal into its `transport.state`.
#[doc(hidden)]
pub struct JoinResult {
    /// The verified sealed roster the founder distributed.
    pub sealed: molt_core::SealedRoster,
    /// The joiner's MLS group snapshot (after processing the Welcome, and — if
    /// the bootstrap ran — after the announcement exchange), or `None` on a
    /// pre-MLS founding.
    pub mls_snapshot: Option<Vec<u8>>,
    /// The joiner's assembled direct-mesh handovers, when the (best-effort)
    /// post-founding bootstrap completed; empty otherwise.
    pub mesh: Vec<molt_core::MeshLink>,
    /// The transport the joiner ran the ritual over, to REUSE for the runtime
    /// supervisor. It shares the created queues' **receive credentials** (over
    /// SMP the recipient keys live in a per-instance `Arc`, unreconstructable
    /// from the mesh handover) — a fresh transport to the same server could send
    /// but never subscribe to the joiner's own inbound queues.
    pub transport: RitualTransport,
}

/// Run the member side of a founding **over SMP** from its real invite link,
/// and return the verified sealed roster + own MLS snapshot: parse the handover,
/// build our *own* [`SmpTransport`] for the founder's server, run the ritual
/// (`cancel` ends the wait early), ratify the proposed charter (via `ratify`,
/// or automatically when `None`), wait for the founder to distribute the sealed
/// roster, and verify it. Writing the workspace is the caller's job (the engine
/// materialises it into its state; [`join_founding_over_smp`] writes it
/// standalone).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn ritual_join_over_smp(
    link: &str,
    name: String,
    phrase: String,
    bootstrap: bool,
    ratify: Option<Ratifier>,
    cancel: Option<mpsc::Receiver<()>>,
    dialer: Dialer,
    extra_server_urls: Vec<String>,
) -> Result<JoinResult, String> {
    let inv = FoundingInvite::parse(link).ok_or("not a joinable founding link")?;
    let server = SmpServer::parse(inv.server.trim()).map_err(|e| e.to_string())?;
    let wrap_bytes: [u8; 32] = hex::decode(&inv.wrap)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "bad wrap key length".to_string())?;
    let queue_id = hex::decode(&inv.queue_id).map_err(|e| e.to_string())?;
    // our OWN transport to the founder's server — not the founder's, routed
    // through the resolved `dialer` (Tor when configured). Keep a clone (SMP
    // clones share the recipient-key store) to hand back for the runtime
    // supervisor: it must reuse THIS instance to subscribe to the inbound
    // queues the bootstrap created.
    //
    // Track B Stage 2: the invite server is the primary (the ritual reaches the
    // founder there); this node's own configured redundancy servers
    // (`extra_server_urls` = settings.smp_urls) are added so the joiner mints its
    // inbound queues across N servers too. Capped at MESH_REDUNDANCY_CAP; an
    // empty list is the former single-server joiner exactly.
    let mut servers = vec![server.clone()];
    for url in &extra_server_urls {
        if let Ok(s) = SmpServer::parse(url.trim()) {
            if !servers.iter().any(|e| e.render() == s.render()) {
                servers.push(s);
            }
        }
    }
    servers.truncate(molt_net::MESH_REDUNDANCY_CAP.max(1));
    let transport = RitualTransport::Smp(SmpTransport::with_dialer_multi(servers, dialer));
    let material = InviteMaterial {
        seat: inv.seat,
        transport: transport.clone(),
        invite_snd: SndQueueAddr {
            server: server.render(),
            id: QueueId::from_bytes(queue_id),
        },
        invite_wrap: WrapKey::from_bytes(wrap_bytes),
        ticket: inv.info.ticket.clone(),
    };
    // `bootstrap` runs the (best-effort) post-founding mesh bootstrap after the
    // group is joined; the caller enables it for the real product flow.
    let outcome =
        run_ritual_member(material, name.clone(), phrase, true, bootstrap, ratify, cancel).await?;
    let sealed = outcome
        .sealed
        .ok_or_else(|| "founder never distributed the sealed roster".to_string())?;
    verify_sealed_roster(&sealed)?;
    // refuse a roster that does not anchor our own (name, key) pair: a founder
    // that excluded us — or anchored our key under a different name — must not
    // leave us holding a workspace whose own acting member is not in its roster
    if !sealed
        .identities
        .iter()
        .any(|i| i.member == name && i.identity_pk == outcome.pk)
    {
        return Err("the sealed roster does not anchor our own (name, key)".to_string());
    }
    Ok(JoinResult {
        sealed,
        mls_snapshot: outcome.mls_snapshot,
        mesh: outcome.mesh.unwrap_or_default(),
        transport,
    })
}

/// Join a founding from its real link over SMP and write our **own** workspace
/// under `root` from our **own** seed (own local id + keys; the shared
/// republic id rides in the genesis). Returns the local workspace id. The
/// standalone entry (a second moltd, tests); the GUI join uses
/// [`ritual_join_over_smp`] and materialises into engine state instead.
#[doc(hidden)]
pub async fn join_founding_over_smp(
    link: &str,
    name: String,
    phrase: String,
    root: &std::path::Path,
    dialer: Dialer,
) -> Result<molt_core::WorkspaceId, String> {
    // bootstrap=false: this standalone one-shot writes a workspace and returns
    // (no running engine to host a runtime supervisor); the live product join is
    // cmd_join_start, which bootstraps. A future CLI that keeps a node running
    // would pass true and persist the mesh (the plumbing below already handles it).
    let result =
        // standalone one-shot: single-server (no running node / config list here)
        ritual_join_over_smp(link, name.clone(), phrase.clone(), false, None, None, dialer, Vec::new())
            .await?;
    let entropy = molt_storage::seed_entropy(&phrase).map_err(|e| e.to_string())?;
    let genesis = result.sealed.into_genesis(&name, molt_storage::now_secs());
    let opened = molt_storage::create_workspace(root, &entropy, &genesis).map_err(|e| e.to_string())?;
    let id = opened.manifest.workspace.id.clone();
    // seal our own MLS group state + assembled mesh into transport.state
    if result.mls_snapshot.is_some() || !result.mesh.is_empty() {
        let mut ts = opened.read_transport_state();
        if let Some(blob) = result.mls_snapshot {
            ts.mls = Some(blob);
        }
        ts.mesh = result.mesh;
        opened.write_transport_state(&ts).map_err(|e| e.to_string())?;
    }
    Ok(id)
}

/// What the member side produced: its anchored identity pk, and — when it
/// waited for it (`collect_genesis`) — the sealed roster the founder
/// distributed at the end, from which the member writes its **own** workspace.
#[doc(hidden)]
pub struct JoinOutcome {
    /// The member's identity public key (what the founder anchored).
    pub pk: String,
    /// The complete sealed roster, present only when `collect_genesis` was
    /// set and the founder finished distributing it.
    pub sealed: Option<molt_core::SealedRoster>,
    /// The member's own MLS group snapshot after processing the Welcome (and,
    /// if `bootstrap` ran, advancing the ratchet through its announcements) —
    /// present only when `collect_genesis` was set and a Welcome arrived. The
    /// caller seals it into the member's `transport.state`.
    pub mls_snapshot: Option<Vec<u8>>,
    /// The member's assembled runtime full-mesh handovers — present only when
    /// `bootstrap` ran to completion. The caller seals them into
    /// `transport.state.mesh` and builds the runtime supervisor.
    pub mesh: Option<Vec<molt_core::MeshLink>>,
}

/// Receive the next complete [`invite::RitualMsg`] on the member's reply
/// queue (unwrap, reassemble); `cancel` ends the wait early.
async fn next_ritual_msg(
    rx: &mut mpsc::Receiver<Delivery>,
    cancel: &mut Option<mpsc::Receiver<()>>,
    wrap: &WrapKey,
    reasm: &mut molt_net::Reassembler,
) -> Result<invite::RitualMsg, String> {
    loop {
        let delivery = match cancel {
            Some(c) => tokio::select! {
                _ = c.recv() => return Err("ritual cancelled".to_string()),
                d = rx.recv() => match d { Some(d) => d, None => return Err("queue closed".into()) },
            },
            None => match rx.recv().await {
                Some(d) => d,
                None => return Err("queue closed".to_string()),
            },
        };
        let Ok(plain) = molt_net::wrap::unwrap_block(wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
            continue;
        };
        if let Ok(msg) = serde_json::from_slice::<invite::RitualMsg>(&bytes) {
            return Ok(msg);
        }
    }
}

/// The member side of the founding ritual, as a standalone unit both the
/// founder's simulated members and a real second instance run: derive the
/// identity from `phrase`, activate the invite (`JoinRequest`, MAC-bound to
/// the ticket), await the canonical table, sign it, and — when
/// `collect_genesis` — wait for the founder to distribute the complete sealed
/// roster (so the caller can write the member's own workspace). Simulated
/// members pass `false`; a real joining node passes `true`.
///
/// `cancel` (if any) ends the wait early (ritual teardown). This is the exact
/// code path a remote member's node runs over SMP.
/// The joiner's human **ratification gate**: `run_ritual_member` surfaces the
/// founder's proposed charter (final name, agenda) on `proposal` and blocks on
/// `confirm` before signing — signing *is* the ratification (concept §3.3).
/// `None` on the non-interactive paths (the founder's sim members, the
/// standalone CLI join), which sign as soon as the table verifies.
#[doc(hidden)]
pub struct Ratifier {
    /// Fires once when the founder acknowledges the join (`JoinAccepted`) — the
    /// joiner's wizard shows "you're in, waiting for the deliberation" instead of
    /// a silent wait. Best-effort (capacity 1; a resend is dropped).
    pub accepted: mpsc::Sender<()>,
    /// The proposed `(final name, agenda)` surfaced for the human to review.
    pub proposal: mpsc::Sender<(String, String)>,
    /// The human's decision: `true` ratifies (sign); `false` or a closed
    /// channel declines and aborts the join.
    pub confirm: mpsc::Receiver<bool>,
}

/// Run the member side of the post-founding **mesh bootstrap** over the star:
/// carry [`molt_net::mesh::MeshAnnounce`]s as MLS ciphertext — outbound as
/// `RitualMsg::MeshAnnounce` on the founder's invite queue, inbound on our reply
/// queue — and return the assembled full-mesh handovers. Consumes `rx`/`reasm`
/// (the reply-queue reader after the genesis message).
#[allow(clippy::too_many_arguments)]
async fn member_bootstrap<T: molt_net::Transport>(
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
        let mut never: Option<mpsc::Receiver<()>> = None;
        loop {
            match next_ritual_msg(&mut rx, &mut never, &reply_wrap, &mut reasm).await {
                Ok(invite::RitualMsg::MeshAnnounce { ct }) => {
                    if let Ok(bytes) = hex::decode(&ct) {
                        if in_tx.send(bytes).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
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
    transport: RitualTransport,
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

/// The member's per-workspace identity keypair, derived deterministically from
/// its own recovery phrase — the ONE derivation both the ritual (which anchors
/// the public key in the roster) and the join finish (which needs the private
/// key to sign chain governance) must agree on. Returns `(signing key, pk hex)`.
pub(crate) fn member_identity(
    phrase: &str,
) -> Result<(molt_storage::SigningKey, String), String> {
    let entropy = molt_storage::seed_entropy(phrase).map_err(|e| e.to_string())?;
    Ok(member_identity_from_entropy(&entropy))
}

/// The entropy-level core of [`member_identity`] — shared with the restore
/// path, which holds raw seed entropy (from the blob meta) instead of a
/// typed phrase. ONE salt convention: changing it here changes it for the
/// ritual, the join finish, and the restored-identity check together.
pub(crate) fn member_identity_from_entropy(
    entropy: &[u8],
) -> (molt_storage::SigningKey, String) {
    let member_id = molt_storage::derive_workspace_id(entropy, "member");
    molt_storage::derive_identity_key(entropy, &member_id)
}

/// Run the **member side** of the founding ritual against the founder's
/// transport: derive the member's own identity, build its MLS `KeyPackage`,
/// activate the seat (ticket MAC), ratify the charter, and — when
/// `collect_genesis` is set — receive and verify the sealed roster + Welcome
/// (optionally bootstrapping the post-founding mesh). Returns the member's
/// [`JoinOutcome`]. The code path a genuinely separate node runs.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn run_ritual_member<T: molt_net::Transport>(
    m: InviteMaterial<T>,
    name: String,
    phrase: String,
    collect_genesis: bool,
    bootstrap: bool,
    ratify: Option<Ratifier>,
    mut cancel: Option<mpsc::Receiver<()>>,
) -> Result<JoinOutcome, String> {
    // per-workspace identity, deterministic from the member's own phrase —
    // a real, verifiable key the founder anchors on activation. The SAME
    // derivation must be reproducible when the join finish materializes the
    // workspace (so the chain signing key matches the anchored roster key) —
    // hence the shared [`member_identity`] helper.
    let (sk, pk) = member_identity(&phrase)?;

    // the MLS member, built from the *same* identity key (concept §3.3: one
    // identity anchors both the genesis table and the MLS credential). Its
    // KeyPackage rides the JoinRequest; its provider must live until the
    // Welcome is processed, then is snapshotted into transport.state.
    let mut mls = molt_net::MlsMember::new(&sk, &name).map_err(|e| e.to_string())?;
    let key_package = mls.key_package().map_err(|e| e.to_string())?;

    // create the reply queue we (the member) receive the canonical table
    // on, and subscribe *before* announcing it — so the founder's table can
    // never race ahead of our subscription. In SMP each party owns the
    // queue it receives on; this is exactly that queue.
    let reply_q = m.transport.create_queue().await.map_err(|e| e.to_string())?;
    let reply_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
    let mut rx = m
        .transport
        .subscribe(&reply_q.rcv)
        .await
        .map_err(|e| e.to_string())?;

    // activate: JoinRequest, MAC-bound to the ticket, advertising our reply
    // queue so the founder knows where to send the table
    let join = invite::RitualMsg::Join(invite::JoinRequest {
        seat: m.seat,
        name: name.clone(),
        identity_pk: pk.clone(),
        mac: invite::join_mac(&m.ticket, &name, &pk),
        reply: Some(invite::ReplyHandover {
            server: reply_q.snd.server.clone(),
            queue_id: hex::encode(&reply_q.snd.id.0),
            wrap: hex::encode(reply_wrap.to_bytes()),
        }),
        key_package: hex::encode(&key_package),
    });
    let payload = serde_json::to_vec(&join).map_err(|e| e.to_string())?;
    supervisor::send_framed(
        &m.transport,
        &m.invite_snd,
        &m.invite_wrap,
        msg_id(&name, "founder", 1),
        &payload,
    )
    .await
    .map_err(|e| e.to_string())?;

    // await the proposed constitution on our reply queue; the founder's
    // JoinAccepted ack arrives first and gives the wizard early feedback.
    // UNTIL that ack arrives, the wait has a hard deadline: a spent link
    // used against a FINISHED/cancelled ritual is dropped silently on the
    // founder side (stale generation), and an offline founder answers
    // nothing — without the deadline the joiner hangs in "Contacting the
    // inviter…" forever. AFTER the ack the wait is unbounded again: the
    // charter deliberation is a human step and may take as long as it
    // takes (and the wizard's × can cancel any time).
    const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
    let accept_deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
    let mut accepted = false;
    let mut reasm = molt_net::Reassembler::new();
    let proposal_json = loop {
        let msg = if accepted {
            next_ritual_msg(&mut rx, &mut cancel, &reply_wrap, &mut reasm).await?
        } else {
            match tokio::time::timeout_at(
                accept_deadline,
                next_ritual_msg(&mut rx, &mut cancel, &reply_wrap, &mut reasm),
            )
            .await
            {
                Ok(msg) => msg?,
                Err(_) => {
                    return Err(
                        "the inviter did not answer — the link may already be used \
                         up, the founding may be over, or the founder is offline; \
                         ask the founder for a fresh link and try again"
                            .to_string(),
                    );
                }
            }
        };
        match msg {
            invite::RitualMsg::JoinAccepted { .. } => {
                accepted = true;
                if let Some(r) = ratify.as_ref() {
                    let _ = r.accepted.try_send(());
                }
            }
            // the founder rejected this activation: the single-use ticket was
            // already spent by another member (the same link went to two
            // people). Fail fast with the reason — the joiner needs their
            // own, unused link.
            invite::RitualMsg::LinkSpent { .. } => {
                return Err(
                    "this invite link was already used by another member — every \
                     member needs their own link; ask the founder for a fresh one"
                        .to_string(),
                );
            }
            invite::RitualMsg::Seal { proposal } => break proposal,
            _ => {}
        }
    };
    let proposal: molt_core::SealedRoster =
        serde_json::from_str(&proposal_json).map_err(|e| e.to_string())?;
    // verify what we are about to ratify BEFORE we sign, and recompute the exact
    // bytes to sign from the shown proposal — so what we sign provably equals
    // the name + agenda + roster we ratify
    let table = verify_seal_proposal(&proposal, &name, &pk)?;
    // human ratification gate: surface the charter and wait for the confirm
    // before signing. The non-interactive paths (sim members, CLI) pass None
    // and ratify once the proposal verified.
    if let Some(mut r) = ratify {
        let _ = r.proposal.send((proposal.name.clone(), proposal.agenda.clone())).await;
        match r.confirm.recv().await {
            Some(true) => {}
            Some(false) => {
                // explicit decline: tell the founder so its seat shows declined
                // (a silent abandon — None below — the founder just sees stale)
                let declined = invite::RitualMsg::Declined { seat: m.seat };
                if let Ok(payload) = serde_json::to_vec(&declined) {
                    let _ = supervisor::send_framed(
                        &m.transport,
                        &m.invite_snd,
                        &m.invite_wrap,
                        msg_id(&name, "founder", 3),
                        &payload,
                    )
                    .await;
                }
                return Err("the charter was declined".to_string());
            }
            None => return Err("the ritual was cancelled".to_string()),
        }
    }
    let sig = molt_storage::identity_sign(&sk, &table);
    let signed = invite::RitualMsg::Signed(invite::SealSigned { seat: m.seat, sig });
    let out = serde_json::to_vec(&signed).map_err(|e| e.to_string())?;
    supervisor::send_framed(
        &m.transport,
        &m.invite_snd,
        &m.invite_wrap,
        msg_id(&name, "founder", 2),
        &out,
    )
    .await
    .map_err(|e| e.to_string())?;

    if !collect_genesis {
        // sim members stop at their seal signature; their KeyPackage still
        // joined the founder's group, they just never process the Welcome
        return Ok(JoinOutcome {
            pk,
            sealed: None,
            mls_snapshot: None,
            mesh: None,
        });
    }

    // wait for the founder to distribute the complete sealed roster + the MLS
    // Welcome once every seat has signed — this is what lets us write our own
    // workspace and enter the group. A `MeshAnnounce` that races ahead of the
    // genesis (the founder starts its bootstrap right after distributing) is
    // *buffered* here, not dropped, so the member's own bootstrap still sees it.
    let mut early_mesh: Vec<Vec<u8>> = Vec::new();
    loop {
        match next_ritual_msg(&mut rx, &mut cancel, &reply_wrap, &mut reasm).await? {
            invite::RitualMsg::MeshAnnounce { ct } => {
                if let Ok(b) = hex::decode(&ct) {
                    early_mesh.push(b);
                }
            }
            invite::RitualMsg::Genesis { sealed, welcome } => {
                let sealed: molt_core::SealedRoster =
                    serde_json::from_str(&sealed).map_err(|e| e.to_string())?;
                // a founding without a Welcome (pre-MLS peer) leaves us groupless
                if welcome.is_empty() {
                    return Ok(JoinOutcome {
                        pk,
                        sealed: Some(sealed),
                        mls_snapshot: None,
                        mesh: None,
                    });
                }
                let bytes = hex::decode(&welcome).map_err(|e| e.to_string())?;
                mls.join_from_welcome(&bytes).map_err(|e| e.to_string())?;
                // opt-in: bootstrap the runtime mesh over the star, then snapshot
                // the group AFTER (its ratchet advanced through the announcements)
                if bootstrap {
                    let peers: Vec<MemberId> =
                        sealed.roster.iter().filter(|r| **r != name).cloned().collect();
                    let mls_arc = Arc::new(Mutex::new(mls));
                    // best-effort: a bootstrap that times out or errors still lets
                    // us enter, just without a direct mesh (the group is already
                    // in hand; the mesh can be re-established later)
                    let mesh = match member_bootstrap(
                        &name,
                        peers,
                        &m.transport,
                        m.invite_snd.clone(),
                        m.invite_wrap.clone(),
                        reply_wrap.clone(),
                        rx,
                        reasm,
                        early_mesh,
                        mls_arc.clone(),
                    )
                    .await
                    {
                        Ok(mesh) => Some(mesh),
                        Err(e) => {
                            tracing::warn!(error = %e, "mesh bootstrap did not complete; entering without a direct mesh");
                            None
                        }
                    };
                    let snap = mls_arc
                        .lock()
                        .map_err(|_| "mls lock poisoned".to_string())?
                        .snapshot()
                        .map_err(|e| e.to_string())?;
                    return Ok(JoinOutcome {
                        pk,
                        sealed: Some(sealed),
                        mls_snapshot: Some(snap),
                        mesh,
                    });
                }
                let snap = mls.snapshot().map_err(|e| e.to_string())?;
                return Ok(JoinOutcome {
                    pk,
                    sealed: Some(sealed),
                    mls_snapshot: Some(snap),
                    mesh: None,
                });
            }
            _ => {}
        }
    }
}

/// A simulated member (offline **test seam** only): a real
/// [`run_ritual_member`] with a canned name, its own fresh phrase, and a
/// small human-like delay. The keepalive channel is its stop signal —
/// dropping it (ritual teardown) ends the member.
fn spawn_sim_member(material: InviteMaterial) -> Result<mpsc::Sender<()>, String> {
    let phrase = molt_storage::generate_seed_phrase().map_err(|e| e.to_string())?;
    let name = SIM_NAMES
        .get(usize::try_from(material.seat).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or("member")
        .to_string();
    let (keep_tx, keep_rx) = mpsc::channel::<()>(1);
    let delay = 200 + 150 * (u64::from(material.seat) % 5);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        // a simulated member does not write a workspace, so it stops at its
        // seal signature (collect_genesis = false) and ratifies automatically
        // (ratify = None) — the sim seam has no human to confirm
        if let Err(e) =
            run_ritual_member(material, name, phrase, false, false, None, Some(keep_rx)).await
        {
            tracing::debug!(error = %e, "simulated founding member ended");
        }
    });
    Ok(keep_tx)
}

/// Display names for the simulated founding members (indexed by seat).
const SIM_NAMES: [&str; 12] = [
    "mira", "juno", "bassa", "tarek", "noor", "eli", "vega", "sol", "rune", "ada", "kai", "wren",
];

/// Parse a member's reply-queue handover (JSON of [`invite::ReplyHandover`])
/// into the founder's send address + wrap key. `None` if absent or
/// malformed — the founder then rejects the join, since the seat could
/// never be sealed without a reply queue.
fn parse_reply_handover(reply: &str) -> Option<(SndQueueAddr, WrapKey)> {
    let r: invite::ReplyHandover = serde_json::from_str(reply).ok()?;
    let id = hex::decode(&r.queue_id).ok()?;
    let wrap_bytes: [u8; 32] = hex::decode(&r.wrap).ok()?.try_into().ok()?;
    Some((
        SndQueueAddr {
            server: r.server,
            id: molt_net::QueueId::from_bytes(id),
        },
        WrapKey::from_bytes(wrap_bytes),
    ))
}

/// The ritual command handlers (`cmd_net_join_requested`,
/// `cmd_net_seal_signed`), split out so the transport plumbing above stays
/// readable. They are inherent `State` methods — no re-export needed.
mod ritual_ops {
    use super::*;

    impl State {
        /// A founding seat's real invite link became available (its SMP
        /// queue is now provisioned). Replace the seat's preview link with
        /// the joinable one, so the founder's GUI shows a link a separate
        /// node can use.
        pub(crate) fn cmd_net_ritual_link_ready(
            &mut self,
            seat: u32,
            link: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.link = link;
            }
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// The founder's SMP provisioning failed (e.g. server unreachable):
        /// fail the create run and tear the ritual down, so the wizard shows
        /// the error instead of waiting for links that will never come.
        pub(crate) fn cmd_net_ritual_failed(
            &mut self,
            error: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation)
                || self.session.create.run.outcome != 0
            {
                return Ok(molt_core::Reply::Ack);
            }
            self.session.create.run.outcome = 2;
            self.session
                .create
                .run
                .log
                .push(format!("✗ founding failed: {error}"));
            self.teardown_ritual();
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

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
                active.handle.persist_mesh_crypto_blocking(
                    Some(mls_snapshot.clone()),
                    creds,
                    mesh.clone(),
                );
            }
            // stand the runtime supervisor up over the direct mesh, reusing the
            // ritual transport (the loopback hub / the founder's SMP server), so
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

        /// A member activated their link. Verify the ticket MAC, anchor
        /// their identity, and — once every seat's key is in — send the
        /// canonical table to all members to sign. Verification failures
        /// are logged and dropped (a bad request must not wedge anything).
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn cmd_net_join_requested(
            &mut self,
            seat: u32,
            member: MemberId,
            identity_pk: String,
            proof: String,
            reply: String,
            key_package: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            // the ticket is single-use — handle a spent seat FIRST, on an
            // immutable borrow (the log/transport access below must not fight
            // the mutable seat borrow). The SAME member re-announcing itself
            // is at-least-once delivery (a redelivered JoinRequest) — stay
            // silent, the seat is already theirs. A DIFFERENT member with a
            // valid MAC means the founder sent one link to two people: reject
            // the second activation on its reply queue so it fails fast
            // instead of waiting forever, and say so in the ritual log — the
            // anchored seat and the ritual stay untouched.
            let spent = self
                .net_ritual
                .as_ref()
                .and_then(|r| r.seats.get(idx))
                .and_then(|s| {
                    s.identity
                        .as_ref()
                        .map(|a| (a.member.clone(), a.identity_pk.clone(), s.ticket.clone()))
                });
            if let Some((anchored_member, anchored_pk, ticket)) = spent {
                let same = anchored_member == member && anchored_pk == identity_pk;
                if !same && invite::verify_join_mac(&ticket, &member, &identity_pk, &proof) {
                    if let (Some((snd, wrap)), Some(ritual)) =
                        (parse_reply_handover(&reply), &self.net_ritual)
                    {
                        if let Ok(payload) =
                            serde_json::to_vec(&invite::RitualMsg::LinkSpent { seat })
                        {
                            let transport = ritual.transport.clone();
                            let id = ritual.next_msg_id(&format!("spent-{idx}-{member}"));
                            tokio::spawn(async move {
                                let _ = supervisor::send_framed(
                                    &transport, &snd, &wrap, id, &payload,
                                )
                                .await;
                            });
                        }
                    }
                    self.session.create.run.log.push(format!(
                        "✗ invite {} was activated a second time (by {member}) — that \
                         link is spent; they need their own, unused link",
                        idx + 1
                    ));
                    self.emit_session(molt_core::SessionScope::Create);
                }
                return Ok(molt_core::Reply::Ack);
            }
            let Some(ritual) = &mut self.net_ritual else {
                return Ok(molt_core::Reply::Ack);
            };
            let Some(s) = ritual.seats.get_mut(idx) else {
                return Ok(molt_core::Reply::Ack);
            };
            if !invite::verify_join_mac(&s.ticket, &member, &identity_pk, &proof) {
                tracing::warn!(seat, %member, "founding join rejected: bad ticket MAC");
                return Ok(molt_core::Reply::Ack);
            }
            // the member advertised the reply queue for its table; without a
            // usable one the seat can never be sealed, so reject the join
            let Some((reply_snd, reply_wrap)) = parse_reply_handover(&reply) else {
                tracing::warn!(seat, %member, "founding join rejected: missing/invalid reply queue");
                return Ok(molt_core::Reply::Ack);
            };
            // the member's MLS KeyPackage is required AND must be bound to the
            // anchored identity: its credential must name this member and its
            // signature key must be the MAC-bound identity key (one identity,
            // two anchors). Otherwise a joiner could pass the ticket MAC for one
            // handle yet authenticate inside the group as another.
            let key_package_binds = hex::decode(&key_package)
                .ok()
                .and_then(|b| molt_net::mls::key_package_binding(&b).ok())
                .is_some_and(|(id, sig)| id == member.as_bytes() && hex::encode(sig) == identity_pk);
            if !key_package_binds {
                tracing::warn!(seat, %member, "founding join rejected: MLS key package does not match the anchored identity");
                return Ok(molt_core::Reply::Ack);
            }
            // keep a copy of the reply handover to ack the joiner below
            let (ack_addr, ack_wrap) = (reply_snd.clone(), reply_wrap.clone());
            s.identity = Some(MemberIdentity {
                member: member.clone(),
                identity_pk,
            });
            s.reply_snd = Some(reply_snd);
            s.reply_wrap = Some(reply_wrap);
            s.key_package = Some(key_package);
            // reflect into the session seat + log
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.member = member.clone();
                view.state = 1;
            }
            self.session
                .create
                .run
                .log
                .push(format!("→ {member} activated invite {} · key received", idx + 1));

            // tell the joiner we accepted, so it gets immediate feedback instead
            // of a silent wait until the charter (advisory — the joiner still
            // verifies the eventual Seal/Genesis)
            if let Some(ritual) = &self.net_ritual {
                if let Ok(payload) = serde_json::to_vec(&invite::RitualMsg::JoinAccepted { seat }) {
                    let transport = ritual.transport.clone();
                    let id = ritual.next_msg_id(&format!("accepted-{idx}"));
                    tokio::spawn(async move {
                        let _ =
                            supervisor::send_framed(&transport, &ack_addr, &ack_wrap, id, &payload)
                                .await;
                    });
                }
            }

            // once every seat has joined, unlock the deliberation step: the
            // founder proposes the final name + agenda, and only then does the
            // roster seal for ratification (concept §3.3)
            let all_joined = self
                .net_ritual
                .as_ref()
                .is_some_and(|r| r.seats.iter().all(|s| s.identity.is_some()));
            if all_joined && !self.session.create.can_propose {
                self.session.create.can_propose = true;
                self.session
                    .create
                    .run
                    .log
                    .push("→ every member has joined · propose the charter to seal".to_string());
            }
            // seal now if the charter was already proposed (the sim seam
            // pre-proposes; a founder may also propose before the last join)
            self.maybe_seal();
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// The founder proposes the deliberated charter (final name + agenda).
        /// Requires every seat joined; sets the final name/agenda on the ritual
        /// and the session, then seals the roster for ratification. Co-equal —
        /// an operator or the GUI issues it.
        pub(crate) fn cmd_create_propose(
            &mut self,
            name: String,
            agenda: String,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(molt_core::MoltError::Create(
                    "the republic needs a name".to_string(),
                ));
            }
            // bound the constitution: it is signed by everyone and stored in
            // every member's genesis forever, so cap both fields (an empty
            // agenda is allowed — a republic may found without a written charter)
            const NAME_MAX: usize = 120;
            const AGENDA_MAX: usize = 4096;
            if name.chars().count() > NAME_MAX {
                return Err(molt_core::MoltError::Create(format!(
                    "the name is too long (max {NAME_MAX} characters)"
                )));
            }
            if agenda.chars().count() > AGENDA_MAX {
                return Err(molt_core::MoltError::Create(format!(
                    "the agenda is too long (max {AGENDA_MAX} characters)"
                )));
            }
            let Some(ritual) = &mut self.net_ritual else {
                return Err(molt_core::MoltError::Create(
                    "no founding is in progress".to_string(),
                ));
            };
            // one-shot: once the charter is proposed the members are ratifying a
            // fixed table, and a second proposal with a different name/agenda
            // would silently invalidate the signatures already collected (their
            // seat stays green but genesis verification fails). To change the
            // charter, cancel and re-mint the founding.
            if ritual.charter_proposed {
                return Err(molt_core::MoltError::Create(
                    "the charter was already proposed — cancel the founding to change it".to_string(),
                ));
            }
            if ritual.seats.iter().any(|s| s.identity.is_none()) {
                return Err(molt_core::MoltError::Create(
                    "every member must join before you propose the charter".to_string(),
                ));
            }
            // the final, ratified name feeds the republic id + canonical bytes;
            // keep the ritual and the session in lock-step so finalize (which
            // reads the session's create state) signs exactly what was proposed
            ritual.name = name.clone();
            ritual.agenda = agenda.clone();
            ritual.charter_proposed = true;
            self.session.create.name = name;
            self.session.create.agenda = agenda;
            self.session
                .create
                .run
                .log
                .push("→ charter proposed · awaiting every member's ratification".to_string());
            self.maybe_seal();
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// A member explicitly declined the proposed charter. Mark its seat as
        /// declined (state 3) and log it — the founding can no longer seal (a
        /// declined seat is never state 2), so the path forward is cancel +
        /// re-mint. A stale/late decline is dropped.
        pub(crate) fn cmd_net_join_declined(
            &mut self,
            seat: u32,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            let who = self
                .net_ritual
                .as_ref()
                .and_then(|r| r.seats.get(idx))
                .and_then(|s| s.identity.as_ref())
                .map(|i| i.member.clone())
                .unwrap_or_else(|| format!("member {}", idx + 1));
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.state = 3; // declined
            }
            self.session.create.run.log.push(format!(
                "✗ {who} declined the charter · cancel and re-mint to change it"
            ));
            // a declined seat can never turn sealed, so this founding is over
            // for good: mark the run FAILED so the GUI leaves the waiting
            // posture (abort re-arms, the lobby says so) instead of idling
            // on a ritual that cannot complete. The ritual itself is kept —
            // other members may still be mid-flight — until cancel tears it
            // down; outcome 2 already blocks maybe_finalize.
            if self.session.create.run.outcome == 0 {
                self.session.create.run.outcome = 2;
                self.session.create.run.log.push(
                    "✗ the ritual is over — this republic must be founded anew (close and re-mint)"
                        .to_string(),
                );
            }
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// If every seat's key is collected AND the founder proposed the charter
        /// (final name + agenda), freeze the canonical table and send it to each
        /// member to ratify (idempotent: only fires once — a resend past the
        /// first is harmless, the members' signatures are idempotent too).
        fn maybe_seal(&mut self) {
            let Some(ritual) = &self.net_ritual else {
                return;
            };
            if !ritual.charter_proposed {
                return; // members ratify a concrete charter, not a placeholder
            }
            let Some(identities) = ritual.full_identities() else {
                return; // still waiting on keys
            };
            // the pre-attestation proposal: every field the member needs to
            // recompute the canonical table itself and check its own membership,
            // so it ratifies exactly what it verifies (not an opaque blob)
            let proposal = molt_core::SealedRoster {
                name: ritual.name.clone(),
                republic_id: ritual.republic_id(&identities),
                rule_m: ritual.rule_m,
                rule_n: ritual.rule_n,
                roster: identities.iter().map(|i| i.member.clone()).collect(),
                identities: identities.clone(),
                attestations: Vec::new(),
                agenda: ritual.agenda.clone(),
            };
            let proposal_json = match serde_json::to_string(&proposal) {
                Ok(j) => j,
                Err(_) => return,
            };
            self.session
                .create
                .run
                .log
                .push("→ charter proposed · sealing the roster for ratification".to_string());
            // send RitualMsg::Seal (the charter to ratify) to each seat
            let msg = invite::RitualMsg::Seal {
                proposal: proposal_json,
            };
            let payload = match serde_json::to_vec(&msg) {
                Ok(p) => p,
                Err(_) => return,
            };
            let Some(ritual) = &self.net_ritual else {
                return;
            };
            for (idx, s) in ritual.seats.iter().enumerate() {
                // every joined seat has a reply queue (set on join); skip
                // any that somehow doesn't rather than panic
                let (Some(addr), Some(wrap)) = (s.reply_snd.clone(), s.reply_wrap.clone()) else {
                    continue;
                };
                let transport = ritual.transport.clone();
                let id = ritual.next_msg_id(&format!("seal-{idx}"));
                let payload = payload.clone();
                tokio::spawn(async move {
                    let _ = supervisor::send_framed(&transport, &addr, &wrap, id, &payload).await;
                });
            }
        }

        /// A member returned its seal signature. Verify it against the
        /// anchored key; when every seat is sealed, write the genesis and
        /// the workspace comes into being.
        pub(crate) fn cmd_net_seal_signed(
            &mut self,
            seat: u32,
            sig: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            let (ok, member) = {
                let Some(ritual) = &self.net_ritual else {
                    return Ok(molt_core::Reply::Ack);
                };
                let Some(identities) = ritual.full_identities() else {
                    return Ok(molt_core::Reply::Ack);
                };
                let Some(s) = ritual.seats.get(idx) else {
                    return Ok(molt_core::Reply::Ack);
                };
                if s.sealed {
                    return Ok(molt_core::Reply::Ack); // this seat already sealed
                }
                let Some(who) = &s.identity else {
                    return Ok(molt_core::Reply::Ack);
                };
                let table = ritual.canonical(&identities);
                (
                    molt_storage::identity_verify(&who.identity_pk, &table, &sig),
                    who.member.clone(),
                )
            };
            if !ok {
                tracing::warn!(seat, "founding seal rejected: bad signature");
                return Ok(molt_core::Reply::Ack);
            }
            // spend the seat so a second, distinct SealSigned cannot push a
            // duplicate attestation (which would bloat the roster and make
            // every honest joiner's verification fail)
            if let Some(ritual) = &mut self.net_ritual {
                if let Some(s) = ritual.seats.get_mut(idx) {
                    s.sealed = true;
                }
            }
            // record the attestation (the seat's identity was anchored on join)
            self.ritual_attestations
                .push(RosterAttestation { member: member.clone(), sig });
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.state = 2;
            }
            self.session
                .create
                .run
                .log
                .push(format!("✓ {member} signed the roster · seat sealed"));

            self.maybe_finalize();
            // maybe_finalize may have auto-entered the workspace (screen change),
            // so mirror the FULL session, not just the create sub-state
            self.emit_session(molt_core::SessionScope::Full);
            Ok(molt_core::Reply::Ack)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{MemberIdentity, RosterAttestation, SealedRoster};

    #[test]
    fn recover_command_maps_the_request_and_encodes_the_reply() {
        let r = invite::RecoverRequest {
            member: "walter".to_string(),
            identity_pk: "aa".to_string(),
            key_package: "bb".to_string(),
            ticket: "cc".to_string(),
            seat_proof: "dd".to_string(),
            reply: Some(invite::ReplyHandover {
                server: "smp://f@h".to_string(),
                queue_id: "ee".to_string(),
                wrap: "ff".to_string(),
            }),
        };
        let Command::NetRecoverRequested {
            member,
            key_package,
            ticket,
            seat_proof,
            reply,
            generation,
            ..
        } = recover_command(r, 7)
        else {
            panic!("expected NetRecoverRequested");
        };
        assert_eq!(member, "walter");
        assert_eq!(key_package, "bb");
        assert_eq!(ticket, "cc");
        assert_eq!(seat_proof, "dd");
        assert_eq!(generation, Some(7));
        assert!(reply.contains("smp://f@h"), "the reply handover is encoded: {reply}");

        // no reply queue → empty handover string
        let bare = invite::RecoverRequest {
            member: "x".to_string(),
            identity_pk: String::new(),
            key_package: String::new(),
            ticket: String::new(),
            seat_proof: String::new(),
            reply: None,
        };
        let Command::NetRecoverRequested { reply, .. } = recover_command(bare, 1) else {
            panic!("expected NetRecoverRequested");
        };
        assert_eq!(reply, "");
    }

    #[test]
    fn seat_proof_binds_ticket_key_package_and_republic() {
        let (sk, pk) = molt_storage::derive_identity_key(&[7u8; 32], "ws");
        let sig = make_seat_proof(&sk, "ticket-abc", "aabbcc", "rep-id-1");
        // the genuine proof verifies against the anchored key
        assert!(verify_seat_proof(&pk, "ticket-abc", "aabbcc", "rep-id-1", &sig));
        // tampering ANY of the three bound fields breaks it
        assert!(!verify_seat_proof(&pk, "other", "aabbcc", "rep-id-1", &sig));
        assert!(!verify_seat_proof(&pk, "ticket-abc", "ffff", "rep-id-1", &sig));
        assert!(!verify_seat_proof(&pk, "ticket-abc", "aabbcc", "rep-id-2", &sig));
        // a different identity key (a leaked link without the phrase) can't forge it
        let (_, pk2) = molt_storage::derive_identity_key(&[8u8; 32], "ws");
        assert!(!verify_seat_proof(&pk2, "ticket-abc", "aabbcc", "rep-id-1", &sig));
    }

    /// A fully-signed 2-member sealed roster with real keys.
    fn valid_roster() -> SealedRoster {
        let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
        let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
        let identities = vec![
            MemberIdentity { member: "founder".into(), identity_pk: pk_a },
            MemberIdentity { member: "member".into(), identity_pk: pk_b },
        ];
        let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
        let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "charter");
        let attestations = vec![
            RosterAttestation { member: "founder".into(), sig: molt_storage::identity_sign(&sk_a, &table) },
            RosterAttestation { member: "member".into(), sig: molt_storage::identity_sign(&sk_b, &table) },
        ];
        SealedRoster {
            name: "R".into(),
            republic_id,
            rule_m: 2,
            rule_n: 2,
            roster: vec!["founder".into(), "member".into()],
            identities,
            attestations,
            agenda: "charter".into(),
        }
    }

    #[test]
    fn verify_sealed_roster_accepts_a_valid_roster() {
        assert!(verify_sealed_roster(&valid_roster()).is_ok());
    }

    #[test]
    fn verify_sealed_roster_rejects_a_forged_republic_id() {
        let mut s = valid_roster();
        s.republic_id = "deadbeef".into();
        assert!(verify_sealed_roster(&s).is_err());
    }

    #[test]
    fn verify_sealed_roster_rejects_a_missing_signature() {
        let mut s = valid_roster();
        s.attestations.pop();
        assert!(verify_sealed_roster(&s).is_err(), "n identities need n attestations");
    }

    #[test]
    fn verify_sealed_roster_rejects_an_attestation_for_an_unknown_member() {
        let mut s = valid_roster();
        s.attestations[1].member = "impostor".into();
        assert!(verify_sealed_roster(&s).is_err());
    }

    #[test]
    fn verify_sealed_roster_rejects_a_bad_signature() {
        let mut s = valid_roster();
        // flip the leading hex nibble of one signature
        let sig = &mut s.attestations[0].sig;
        let first = if sig.starts_with('a') { 'b' } else { 'a' };
        sig.replace_range(0..1, &first.to_string());
        assert!(verify_sealed_roster(&s).is_err());
    }

    #[test]
    fn verify_sealed_roster_rejects_a_tampered_agenda() {
        // the signatures were made over the ratified charter; swapping the
        // agenda in the genesis makes the recomputed table diverge and every
        // attestation fails — the charter is tamper-evident
        let mut s = valid_roster();
        s.agenda = "a charter nobody ratified".to_string();
        assert!(verify_sealed_roster(&s).is_err());
    }

    // --- the joiner's pre-signature verification (sign-what-you-see) ---------

    #[test]
    fn verify_seal_proposal_accepts_and_recomputes_the_table() {
        let p = valid_roster(); // acts as a proposal; attestations are ignored
        let pk = &p.identities[1].identity_pk; // "member"
        let table = verify_seal_proposal(&p, "member", pk).expect("a member ratifies");
        // the returned bytes are exactly the canonical table over the charter,
        // so a signature over them ratifies precisely this name + agenda + roster
        let expect =
            molt_core::roster_canonical_bytes(&p.republic_id, p.rule_m, p.rule_n, &p.identities, &p.agenda);
        assert_eq!(table, expect);
    }

    #[test]
    fn verify_seal_proposal_rejects_a_forged_republic_id() {
        let mut p = valid_roster();
        p.republic_id = "deadbeef".to_string();
        let pk = p.identities[1].identity_pk.clone();
        assert!(verify_seal_proposal(&p, "member", &pk).is_err());
    }

    #[test]
    fn verify_seal_proposal_rejects_when_our_key_is_absent() {
        let p = valid_roster();
        // right name, wrong key → not us
        assert!(verify_seal_proposal(&p, "member", &"00".repeat(32)).is_err());
        // our key, but under a name not in the roster → not us
        let pk = p.identities[1].identity_pk.clone();
        assert!(verify_seal_proposal(&p, "impostor", &pk).is_err());
    }

    #[test]
    fn verify_seal_proposal_binds_the_agenda() {
        let mut p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let before = verify_seal_proposal(&p, "member", &pk).expect("ok");
        p.agenda = "a different charter".to_string();
        let after = verify_seal_proposal(&p, "member", &pk).expect("ok");
        assert_ne!(before, after, "a changed agenda changes the bytes we sign");
    }

    fn sample_invite() -> FoundingInvite {
        FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "Chess Club".into(),
                threshold: 2,
                members: 2,
                inviter: "walter".into(),
                ticket: "ab".repeat(32),
            },
            server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.example.org".into(),
            queue_id: "cd".repeat(12),
            wrap: "ef".repeat(32),
            seat: 0,
        }
    }

    #[test]
    fn founding_invite_round_trips() {
        let link = sample_invite().render();
        let back = FoundingInvite::parse(&link).expect("parses");
        assert_eq!(back.server, sample_invite().server);
        assert_eq!(back.queue_id, sample_invite().queue_id);
        assert_eq!(back.wrap, sample_invite().wrap);
        assert_eq!(back.seat, 0);
    }

    #[test]
    fn founding_invite_parse_rejects_malformed_handovers() {
        let preview = sample_invite().info.render();
        // no handover segment at all (a bare preview link)
        assert!(FoundingInvite::parse(&preview).is_none());
        // trailing segment not valid hex
        assert!(FoundingInvite::parse(&format!("{preview}/zzzz")).is_none());
        // valid hex, but fewer than four newline-separated fields
        let short = hex::encode("only\ntwo\nfields");
        assert!(FoundingInvite::parse(&format!("{preview}/{short}")).is_none());
        // valid hex + four fields, but a non-numeric seat
        let bad_seat = hex::encode("smp://x@h\ncd\nef\nnotanumber");
        assert!(FoundingInvite::parse(&format!("{preview}/{bad_seat}")).is_none());
    }
}
