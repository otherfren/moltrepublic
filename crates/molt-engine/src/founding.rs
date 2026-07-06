// SPDX-License-Identifier: GPL-3.0-or-later

//! The founding ritual (transport concept §3.3): the republic is
//! constituted *before* any workspace touches the disk.
//!
//! The founder mints one single-use invite per future member and opens a
//! transport pair per seat. Each member — here a simulated loopback node,
//! the real member-side code path once T3 lands — derives its own
//! identity key from its own recovery phrase, activates the link
//! (`JoinRequest`, MAC-bound to the ticket), and later signs the final
//! canonical roster table (`SealSigned`). Only when every seat is sealed
//! does the engine write the `Founded` genesis — carrying the complete
//! identity table and all n attestations, so the member list is signed by
//! everyone from birth.
//!
//! Every leg lands in the wizard's live log as a real event; the fake
//! founding animation is gone.

use molt_core::{Command, MemberId, MemberIdentity, RosterAttestation};
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::supervisor;
use molt_net::{
    invite, msg_id, Delivery, LoopbackHub, LoopbackTransport, NetError, PaddedBlock, QueueId,
    QueuePair, RcvQueue, SndQueueAddr, Transport, WrapKey,
};
use molt_storage::SigningKey;
use tokio::sync::mpsc;

use crate::{Envelope, State};

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
}

/// The founder-side ritual runtime: the transport, the founder's own
/// identity, the seats, and the keepalives for the simulated joiners.
pub(crate) struct RitualRuntime {
    // for loopback the transport holds the hub's Arc, so keeping it alive
    // keeps every ritual queue alive; dropping the runtime tears it down.
    // `maybe_seal` sends the canonical table over this.
    transport: RitualTransport,
    /// The republic's display name — an input to the neutral
    /// [`molt_storage::republic_id`] (the roster salt).
    name: String,
    rule_m: u8,
    rule_n: u8,
    founder: MemberIdentity,
    founder_sk: SigningKey,
    seats: Vec<SeatRuntime>,
    generation: u64,
    /// Simulated joiner tasks self-terminate after sealing; their
    /// keepalive senders are held only so a cancelled ritual drops them.
    _sim: Vec<mpsc::Sender<()>>,
    /// The founder's own recv tasks live on the hub; kept alive by the hub.
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

    /// The canonical bytes every member signs once the table is complete.
    fn canonical(&self, identities: &[MemberIdentity]) -> Vec<u8> {
        let rid = self.republic_id(identities);
        molt_core::roster_canonical_bytes(&rid, self.rule_m, self.rule_n, identities)
    }

    fn next_msg_id(&self, tag: &str) -> molt_net::MsgId {
        let n = self
            .seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        msg_id("founder", tag, n)
    }

    /// Send the complete sealed roster to every member's reply queue, so each
    /// writes its own genesis. Fire-and-forget (a member already gone just
    /// misses it); every seat has a reply queue by the time this is called.
    pub(crate) fn distribute_genesis(&self, sealed_json: String) {
        let msg = invite::RitualMsg::Genesis { sealed: sealed_json };
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
            });
        }

        // A real founding over SMP is opt-in (manual mode + the flag set by
        // __spawn_manual_founding_over_smp): the founder's queues live on the
        // configured server and real remote members join over it. Everything
        // else — the in-app demo — founds over the in-process loopback hub
        // with simulated members.
        let (transport, sim) = if manual && self.ritual_over_smp {
            let url = if self.session.settings.smp_server == "custom" {
                self.session.settings.smp_url.clone()
            } else {
                molt_config::default_public_smp()
            };
            let server = SmpServer::parse(url.trim()).map_err(|e| e.to_string())?;
            let transport = RitualTransport::Smp(SmpTransport::new(server));
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
            (transport, Vec::new())
        } else {
            // loopback: the hub creates queues synchronously right here
            let hub = LoopbackHub::calm();
            let transport = RitualTransport::Loopback(hub.transport());
            let mut sim = Vec::with_capacity(seat_count);
            let mut materials = Vec::with_capacity(seat_count);
            for (seat_u32, ticket, invite_wrap) in &seat_setup {
                let invite_q = hub.create_queue_blocking().map_err(|e| e.to_string())?;
                spawn_founder_recv(
                    transport.clone(),
                    invite_q.rcv.clone(),
                    invite_wrap.clone(),
                    *seat_u32,
                    generation,
                    cmd_tx.clone(),
                );
                let material = InviteMaterial {
                    seat: *seat_u32,
                    transport: transport.clone(),
                    invite_snd: invite_q.snd.clone(),
                    invite_wrap: invite_wrap.clone(),
                    ticket: ticket.clone(),
                };
                if manual {
                    materials.push(material);
                } else {
                    // the simulated member: its own seed → identity, real
                    // JoinRequest + seal signature over real queues
                    sim.push(spawn_sim_member(material)?);
                }
            }
            if let Some(sink) = &self.ritual_material_sink {
                let _ = sink.send(materials);
            }
            // `hub` drops here; `transport` (and its task clones) hold the
            // shared Arc, so every ritual queue stays alive until the runtime
            // is dropped
            (transport, sim)
        };

        self.net_ritual = Some(RitualRuntime {
            transport,
            name: name.to_string(),
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
    /// queues and the simulated members.
    pub(crate) fn teardown_ritual(&mut self) {
        self.net_ritual = None;
    }

    /// Whether a ritual command's incarnation is still current.
    fn ritual_generation_current(&self, generation: Option<u64>) -> bool {
        match generation {
            None => true,
            Some(g) => self.net_ritual.as_ref().is_some_and(|r| r.generation == g),
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
    cmd_tx: mpsc::Sender<Envelope>,
) {
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
                    generation: Some(generation),
                },
                invite::RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat,
                    sig: s.sig,
                    generation: Some(generation),
                },
                // founder→member only:
                invite::RitualMsg::Seal { .. } | invite::RitualMsg::Genesis { .. } => continue,
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            if cmd_tx.send(Envelope { cmd, reply }).await.is_err() {
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
        let mut materials = Vec::with_capacity(seat_setup.len());
        for (seat, ticket, invite_wrap) in seat_setup {
            let invite_q = match transport.create_queue().await {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!(seat, error = %e, "SMP invite-queue provisioning failed");
                    return; // the sink stays silent; the waiting side times out
                }
            };
            spawn_founder_recv(
                transport.clone(),
                invite_q.rcv.clone(),
                invite_wrap.clone(),
                seat,
                generation,
                cmd_tx.clone(),
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
fn verify_sealed_roster(s: &molt_core::SealedRoster) -> Result<(), String> {
    let rid = molt_storage::republic_id(&s.name, s.rule_m, s.rule_n, &s.identities);
    if rid != s.republic_id {
        return Err("republic id does not match the roster content".to_string());
    }
    if s.attestations.len() != s.identities.len() {
        return Err("roster is not fully signed by every member".to_string());
    }
    let table = molt_core::roster_canonical_bytes(&s.republic_id, s.rule_m, s.rule_n, &s.identities);
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

/// Join a founding from its real invite link **over SMP**: parse the
/// handover, build our *own* [`SmpTransport`] for the founder's server, run
/// the member side, verify the sealed roster the founder distributes, and
/// write our **own** workspace under `root` from our **own** seed (own local
/// id + keys; the shared republic id lives in the genesis). Returns the local
/// workspace id. The reusable entry point for a separate node — a second
/// moltd, the GUI join flow — to join over SMP from just the shared link.
#[doc(hidden)]
pub async fn join_founding_over_smp(
    link: &str,
    name: String,
    phrase: String,
    root: &std::path::Path,
) -> Result<molt_core::WorkspaceId, String> {
    let inv = FoundingInvite::parse(link).ok_or("not a founding invite link")?;
    let server = SmpServer::parse(inv.server.trim()).map_err(|e| e.to_string())?;
    let wrap_bytes: [u8; 32] = hex::decode(&inv.wrap)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "bad wrap key length".to_string())?;
    let queue_id = hex::decode(&inv.queue_id).map_err(|e| e.to_string())?;
    let material = InviteMaterial {
        seat: inv.seat,
        // our OWN transport to the founder's server — not the founder's
        transport: RitualTransport::Smp(SmpTransport::new(server.clone())),
        invite_snd: SndQueueAddr {
            server: server.render(),
            id: QueueId::from_bytes(queue_id),
        },
        invite_wrap: WrapKey::from_bytes(wrap_bytes),
        ticket: inv.info.ticket.clone(),
    };
    let outcome = run_ritual_member(material, name.clone(), phrase.clone(), true, None).await?;
    let sealed = outcome
        .sealed
        .ok_or_else(|| "founder never distributed the sealed roster".to_string())?;
    verify_sealed_roster(&sealed)?;

    // our own workspace, from our own seed — the shared roster + republic id
    // ride in the genesis; the local id/keys are ours alone
    let entropy = molt_storage::seed_entropy(&phrase).map_err(|e| e.to_string())?;
    let genesis = molt_core::EventEnvelope {
        seq: 1,
        ts: molt_storage::now_secs(),
        by: name.clone(),
        body: molt_core::WorkspaceEvent::Founded {
            name: sealed.name,
            rule_m: sealed.rule_m,
            rule_n: sealed.rule_n,
            member: name,
            roster: sealed.roster,
            identities: sealed.identities,
            attestations: sealed.attestations,
            republic_id: sealed.republic_id,
        },
    };
    let opened = molt_storage::create_workspace(root, &entropy, &genesis).map_err(|e| e.to_string())?;
    Ok(opened.manifest.workspace.id.clone())
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
#[doc(hidden)]
pub async fn run_ritual_member<T: molt_net::Transport>(
    m: InviteMaterial<T>,
    name: String,
    phrase: String,
    collect_genesis: bool,
    mut cancel: Option<mpsc::Receiver<()>>,
) -> Result<JoinOutcome, String> {
    let entropy = molt_storage::seed_entropy(&phrase).map_err(|e| e.to_string())?;
    // per-workspace identity, deterministic from the member's own phrase —
    // a real, verifiable key the founder anchors on activation
    let member_id = molt_storage::derive_workspace_id(&entropy, "member");
    let (sk, pk) = molt_storage::derive_identity_key(&entropy, &member_id);

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

    // await the canonical table on our reply queue, sign it, send it back
    let mut reasm = molt_net::Reassembler::new();
    let table = loop {
        if let invite::RitualMsg::Seal { table } =
            next_ritual_msg(&mut rx, &mut cancel, &reply_wrap, &mut reasm).await?
        {
            break table;
        }
    };
    let table_bytes = hex::decode(&table).map_err(|e| e.to_string())?;
    let sig = molt_storage::identity_sign(&sk, &table_bytes);
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
        return Ok(JoinOutcome { pk, sealed: None }); // sim members stop here
    }

    // wait for the founder to distribute the complete sealed roster once every
    // seat has signed — this is what lets us write our own workspace
    loop {
        if let invite::RitualMsg::Genesis { sealed } =
            next_ritual_msg(&mut rx, &mut cancel, &reply_wrap, &mut reasm).await?
        {
            let sealed: molt_core::SealedRoster =
                serde_json::from_str(&sealed).map_err(|e| e.to_string())?;
            return Ok(JoinOutcome { pk, sealed: Some(sealed) });
        }
    }
}

/// A simulated member: a real [`run_ritual_member`] with a canned name,
/// its own fresh phrase, and a small human-like delay so the ritual log
/// shows members trickling in. The keepalive channel is its stop signal —
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
        // seal signature (collect_genesis = false)
        if let Err(e) = run_ritual_member(material, name, phrase, false, Some(keep_rx)).await {
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

        /// A member activated their link. Verify the ticket MAC, anchor
        /// their identity, and — once every seat's key is in — send the
        /// canonical table to all members to sign. Verification failures
        /// are logged and dropped (a bad request must not wedge anything).
        pub(crate) fn cmd_net_join_requested(
            &mut self,
            seat: u32,
            member: MemberId,
            identity_pk: String,
            proof: String,
            reply: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let Some(ritual) = &mut self.net_ritual else {
                return Ok(molt_core::Reply::Ack);
            };
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            let Some(s) = ritual.seats.get_mut(idx) else {
                return Ok(molt_core::Reply::Ack);
            };
            if s.identity.is_some() {
                return Ok(molt_core::Reply::Ack); // ticket already spent
            }
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
            s.identity = Some(MemberIdentity {
                member: member.clone(),
                identity_pk,
            });
            s.reply_snd = Some(reply_snd);
            s.reply_wrap = Some(reply_wrap);
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

            // all keys in? send the canonical table to every member to sign
            self.maybe_seal();
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// If every seat's key is collected, freeze the canonical table and
        /// send it to each member for signing (idempotent: only fires once).
        fn maybe_seal(&mut self) {
            let Some(ritual) = &self.net_ritual else {
                return;
            };
            let Some(identities) = ritual.full_identities() else {
                return; // still waiting on keys
            };
            let table = ritual.canonical(&identities);
            let table_hex = hex::encode(&table);
            self.session
                .create
                .run
                .log
                .push("→ all keys collected · sealing the roster".to_string());
            // send RitualMsg::Seal to each seat over its reply queue
            let msg = invite::RitualMsg::Seal { table: table_hex };
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
            // record the attestation on the seat
            if let Some(ritual) = &mut self.net_ritual {
                if let Some(s) = ritual.seats.get_mut(idx) {
                    s.identity = s.identity.take(); // (kept; sig stored below)
                }
            }
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
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }
    }
}
