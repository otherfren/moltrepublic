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
use molt_net::supervisor;
use molt_net::{invite, msg_id, LoopbackHub, RcvQueue, SndQueueAddr, Transport, WrapKey};
use molt_storage::SigningKey;
use tokio::sync::mpsc;

use crate::{Envelope, State};

/// One seat's transport material, held by the founder for the ritual's
/// lifetime: the ticket it verifies against, the queue it sends the table
/// on, and (once collected) the member's anchored identity.
struct SeatRuntime {
    ticket: String,
    /// founder → member: where the canonical table goes.
    reply_snd: SndQueueAddr,
    reply_wrap: WrapKey,
    /// The member's identity, once their JoinRequest verified.
    identity: Option<MemberIdentity>,
}

/// The founder-side ritual runtime: the loopback hub, the founder's own
/// identity, the seats, and the keepalives for the simulated joiners.
pub(crate) struct RitualRuntime {
    // the transport holds the hub's Arc, so keeping it alive keeps every
    // ritual queue alive; dropping the runtime tears the whole hub down
    transport: molt_net::LoopbackTransport,
    ws_id: String,
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

    /// The canonical bytes every member signs once the table is complete.
    fn canonical(&self, identities: &[MemberIdentity]) -> Vec<u8> {
        molt_core::roster_canonical_bytes(&self.ws_id, self.rule_m, self.rule_n, identities)
    }

    fn next_msg_id(&self, tag: &str) -> molt_net::MsgId {
        let n = self
            .seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        msg_id("founder", tag, n)
    }

    /// The final identity table (founder first); only valid once every
    /// seat is sealed, which the caller has already checked.
    pub(crate) fn sealed_identities(&self) -> Vec<MemberIdentity> {
        self.full_identities().unwrap_or_else(|| vec![self.founder.clone()])
    }

    /// The workspace id this ritual derived from the founder's seed.
    pub(crate) fn ws_id(&self) -> &str {
        &self.ws_id
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

        let hub = LoopbackHub::calm();
        let transport = hub.transport();
        let seat_count = usize::from(rule_n).saturating_sub(1);

        // one invite queue (member → founder) and one reply queue
        // (founder → member) per seat
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err("engine stopped".to_string());
        };
        let mut seats = Vec::with_capacity(seat_count);
        let mut links = Vec::with_capacity(seat_count);
        let mut sim = Vec::with_capacity(seat_count);
        let mut materials = Vec::with_capacity(seat_count);
        // manual mode (the two-instance dev test): don't spawn simulated
        // members — hand the invite material out so a second engine runs
        // the member side itself
        let manual = self.ritual_material_sink.is_some();
        for seat in 0..seat_count {
            let seat_u32 = u32::try_from(seat).unwrap_or(u32::MAX);
            let ticket = invite::mint_ticket().map_err(|e| e.to_string())?;
            let invite_q = hub.create_queue_blocking().map_err(|e| e.to_string())?;
            let invite_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
            let reply_q = hub.create_queue_blocking().map_err(|e| e.to_string())?;
            let reply_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;

            // the founder's recv task on the invite queue → internal
            // NetJoinRequested / NetSealSigned commands
            spawn_founder_recv(
                &transport,
                invite_q.rcv.clone(),
                invite_wrap.clone(),
                seat_u32,
                generation,
                cmd_tx.clone(),
            );

            // the visible preview link (the real transport handover is the
            // InviteMaterial below; T3 encodes the full payload into the
            // link)
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

            let material = InviteMaterial {
                seat: seat_u32,
                transport: transport.clone(),
                invite_snd: invite_q.snd.clone(),
                invite_wrap: invite_wrap.clone(),
                reply_rcv: reply_q.rcv.clone(),
                reply_wrap: reply_wrap.clone(),
                ticket: ticket.clone(),
            };
            if manual {
                materials.push(material);
            } else {
                // the simulated member: its own seed → identity, real
                // JoinRequest + seal signature over real queues
                sim.push(spawn_sim_member(material)?);
            }

            seats.push(SeatRuntime {
                ticket,
                reply_snd: reply_q.snd.clone(),
                reply_wrap,
                identity: None,
            });
        }
        // hand the material to the waiting test instance (manual mode)
        if let Some(sink) = &self.ritual_material_sink {
            let _ = sink.send(materials);
        }

        // `hub` drops here; `transport` (and its task clones) hold the
        // shared Arc, so every ritual queue stays alive until the runtime
        // is dropped
        self.net_ritual = Some(RitualRuntime {
            transport,
            ws_id,
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
/// a [`invite::RitualMsg`], and issue the matching internal command.
fn spawn_founder_recv(
    transport: &molt_net::LoopbackTransport,
    rcv: RcvQueue,
    wrap: WrapKey,
    seat: u32,
    generation: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    let transport = transport.clone();
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
                    generation: Some(generation),
                },
                invite::RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat,
                    sig: s.sig,
                    generation: Some(generation),
                },
                invite::RitualMsg::Seal { .. } => continue, // founder→member only
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            if cmd_tx.send(Envelope { cmd, reply }).await.is_err() {
                return;
            }
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
pub struct InviteMaterial {
    /// The seat this invite fills (0-based).
    pub seat: u32,
    /// The shared transport (the founder's loopback hub today; an SMP
    /// transport at T3).
    pub transport: molt_net::LoopbackTransport,
    /// member → founder queue (JoinRequest, then SealSigned).
    pub invite_snd: SndQueueAddr,
    pub invite_wrap: WrapKey,
    /// founder → member queue (the canonical table to sign).
    pub reply_rcv: RcvQueue,
    pub reply_wrap: WrapKey,
    /// The single-use ticket.
    pub ticket: String,
}

/// The member side of the founding ritual, as a standalone unit both the
/// founder's simulated members and a real second instance run: derive the
/// identity from `phrase`, activate the invite (`JoinRequest`, MAC-bound
/// to the ticket), await the canonical table, sign it and return the
/// signature. Returns the member's identity public key on success.
///
/// `cancel` (if any) ends the wait early (ritual teardown). This is the
/// exact code path a remote member's node will run over SMP at T3 — here
/// it runs over the founder's loopback hub.
#[doc(hidden)]
pub async fn run_ritual_member(
    m: InviteMaterial,
    name: String,
    phrase: String,
    mut cancel: Option<mpsc::Receiver<()>>,
) -> Result<String, String> {
    let entropy = molt_storage::seed_entropy(&phrase).map_err(|e| e.to_string())?;
    // per-workspace identity, deterministic from the member's own phrase —
    // a real, verifiable key the founder anchors on activation
    let member_id = molt_storage::derive_workspace_id(&entropy, "member");
    let (sk, pk) = molt_storage::derive_identity_key(&entropy, &member_id);

    // activate: JoinRequest, MAC-bound to the ticket
    let join = invite::RitualMsg::Join(invite::JoinRequest {
        seat: m.seat,
        name: name.clone(),
        identity_pk: pk.clone(),
        mac: invite::join_mac(&m.ticket, &name, &pk),
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

    // await the canonical table on the reply queue, sign it, send back
    let mut rx = m
        .transport
        .subscribe(&m.reply_rcv)
        .await
        .map_err(|e| e.to_string())?;
    let mut reasm = molt_net::Reassembler::new();
    loop {
        let delivery = match &mut cancel {
            Some(c) => tokio::select! {
                _ = c.recv() => return Err("ritual cancelled".to_string()),
                d = rx.recv() => match d { Some(d) => d, None => return Err("queue closed".into()) },
            },
            None => match rx.recv().await {
                Some(d) => d,
                None => return Err("queue closed".to_string()),
            },
        };
        let Ok(plain) = molt_net::wrap::unwrap_block(&m.reply_wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
            continue;
        };
        let Ok(invite::RitualMsg::Seal { table }) = serde_json::from_slice(&bytes) else {
            continue;
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
        return Ok(pk); // the member's ritual work is done
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
        if let Err(e) = run_ritual_member(material, name, phrase, Some(keep_rx)).await {
            tracing::debug!(error = %e, "simulated founding member ended");
        }
    });
    Ok(keep_tx)
}

/// Display names for the simulated founding members (indexed by seat).
const SIM_NAMES: [&str; 12] = [
    "mira", "juno", "bassa", "tarek", "noor", "eli", "vega", "sol", "rune", "ada", "kai", "wren",
];

/// The ritual command handlers (`cmd_net_join_requested`,
/// `cmd_net_seal_signed`), split out so the transport plumbing above stays
/// readable. They are inherent `State` methods — no re-export needed.
mod ritual_ops {
    use super::*;

    impl State {
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
            s.identity = Some(MemberIdentity {
                member: member.clone(),
                identity_pk,
            });
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
                let transport = ritual.transport.clone();
                let addr = s.reply_snd.clone();
                let wrap = s.reply_wrap.clone();
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
