// SPDX-License-Identifier: GPL-3.0-or-later

//! The founding ritual (transport concept §3.3): the republic is
//! constituted *before* any workspace touches the disk.
//!
//! The founder mints one single-use invite per future member and provisions a
//! transport queue per seat. Each member — a loopback node in the dev seams
//! today, a real remote node once the Nostr transport (N4) lands — derives its
//! own identity key from its own recovery phrase, activates the link
//! (`JoinRequest`, MAC-bound to the ticket), and later signs the final
//! canonical roster table (`SealSigned`). Only when every seat is sealed does
//! the founder write the `Founded` genesis — carrying the complete identity
//! table and all n attestations — and distribute the sealed roster so every
//! member writes its own workspace. The roster is salted by a neutral,
//! content-derived [`molt_storage::republic_id`], so no member's seed
//! privileges the founder.
//!
//! Every leg lands in the wizard's live log as a real event.

use molt_core::{Command, MemberId, MemberIdentity, RosterAttestation};
use molt_net::supervisor;
use molt_net::{
    invite, msg_id, Delivery, LoopbackHub, LoopbackTransport,
    RcvQueue, SndQueueAddr, Transport, WrapKey,
};
use molt_storage::SigningKey;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::ritual_member::{run_member_ladder, MemberSeat, Phase, RitualLeg};
use crate::{Envelope, State};

/// One seat's transport material, held by the founder for the ritual's
/// lifetime: the ticket it verifies against, the reply queue the member
/// advertised (learned from its JoinRequest — the member owns the queue it
/// receives on), and (once collected) the member's anchored identity.
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
    /// Whether this seat's seed-backup attestation was already accepted
    /// (`seed_backup_confirmation.md` ❻½) — idempotent like `sealed`, and
    /// only ever set AFTER `sealed` (strict ratify-then-confirm).
    backup_confirmed: bool,
    /// An attestation that ARRIVED before this seat's seal signature —
    /// the transports do not order separate messages (the loopback hub
    /// reorders under load, relays reorder 445s), so the member's honest
    /// Signed→BackupConfirmed sequence can invert on the wire. Parked
    /// like an outrun decline and verified when the seat seals; a seat
    /// that never ratifies never applies it (one bounded slot).
    parked_backup: Option<String>,
}

/// The founder-side ritual runtime: the transport, the founder's own
/// identity, the seats, and the keepalives for the simulated joiners.
pub(crate) struct RitualRuntime {
    // for loopback the transport holds the hub's Arc, so keeping it alive
    // keeps every ritual queue alive; dropping the runtime tears it down.
    // `maybe_seal` sends the canonical table over this.
    transport: LoopbackTransport,
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
    /// The proposed feature set (roster-v5, `charter_features.md`), set with
    /// the charter proposal. `None` until proposed (and on the pre-v5 seams),
    /// which keeps the canonical bytes v4-shaped.
    features: Option<Vec<String>>,
    rule_m: u8,
    rule_n: u8,
    founder: MemberIdentity,
    founder_sk: SigningKey,
    /// The founder's own Nostr transport secret (32-byte secp256k1 scalar),
    /// derived at ritual start from a random ephemeral self-ticket — salt
    /// only, minted and dropped inside `start_ritual` (consistency with the
    /// members' ticket-salted derivation, no cross-republic correlation).
    /// Lives in memory until the seal persists it beside `identity_sk`;
    /// `Zeroizing`, so a cancelled/torn-down ritual wipes it on drop (the
    /// ritual's no-trace promise extends to freed memory).
    founder_nostr_sk: zeroize::Zeroizing<Vec<u8>>,
    seats: Vec<SeatRuntime>,
    generation: u64,
    /// Keepalives for the simulated members of the offline **test seam**
    /// ([`crate::__spawn_sim_founding`]); dropping the runtime stops them.
    /// Empty for a manual founding.
    _sim: Vec<mpsc::Sender<()>>,
    /// The founder's own recv tasks live on the transport; kept alive by it.
    seq: std::sync::atomic::AtomicU64,
    /// The Nostr founding runtime (N4a) — `Some` on the production path,
    /// `None` for the loopback manual/sim seams. When set, every send leg
    /// branches HERE FIRST; the queue `transport` above is an inert
    /// placeholder that must never carry a frame.
    nostr: Option<NostrRitual>,
}

/// The founder's Nostr-side ritual state (N4a): its transport endpoint over
/// the invite relays, and — from the all-joined group birth on — the live
/// MLS group, the h-tag seed, and the 445 channel.
pub(crate) struct NostrRitual {
    /// The founder's ritual endpoint (keys + invite relays): gift-wrap sends
    /// and the 1059 inbox ride it.
    pub(crate) net: molt_net::ritual_net::RitualNet,
    /// The fail-closed dialer the whole ritual dials through.
    pub(crate) dialer: molt_net::dial::Dialer,
    /// The group relay list (= the invite relays at founding).
    pub(crate) relays: Vec<String>,
    /// Minted at group birth; delivered only inside the Welcomes.
    pub(crate) rotation_seed: Option<[u8; 32]>,
    /// The founder's live MLS group, born at all-joined (shared with the
    /// spawned 445 publish/recv tasks).
    pub(crate) group: Option<std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>>,
    /// The 445 group channel (set with the group).
    pub(crate) chan: Option<molt_net::ritual_net::GroupChannel>,
    /// INBOUND task handles (the 1059 inbox loop, the 445 recv loop) —
    /// aborted when the ritual ends. Outbound legs are fire-and-forget tasks
    /// that finish on their own (drain, don't abort).
    pub(crate) tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for NostrRitual {
    fn drop(&mut self) {
        // inbound-only tasks — the sanctioned abort (they hold sockets that
        // must not outlive the ritual; a cancelled ritual leaves no listener)
        for t in &self.tasks {
            t.abort();
        }
    }
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
        molt_core::roster_canonical_bytes(
            &rid,
            self.rule_m,
            self.rule_n,
            identities,
            &self.agenda,
            &self.group_relays(),
            self.features.as_deref(),
        )
    }

    /// The proposed feature set the members ratified (`None` on the pre-v5
    /// seams and until the founder proposes the charter).
    pub(crate) fn features(&self) -> Option<Vec<String>> {
        self.features.clone()
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

    /// The founder's Nostr transport secret (the third anchor's private
    /// half) — persisted beside `identity_sk` when the founding seals.
    /// The group's relay pool for this founding — the founder's pick, which
    /// every member ratifies by signing it into the roster bytes (R3). Empty
    /// on the loopback path, which has no relays.
    pub(crate) fn group_relays(&self) -> Vec<String> {
        self.nostr.as_ref().map(|n| n.relays.clone()).unwrap_or_default()
    }

    pub(crate) fn founder_nostr_sk(&self) -> &[u8] {
        &self.founder_nostr_sk
    }

    /// A clone of the ritual transport — keeping it alive keeps the founding
    /// star (and its queues) up for the post-founding mesh bootstrap.
    pub(crate) fn transport(&self) -> LoopbackTransport {
        self.transport.clone()
    }

    /// The live Nostr-born MLS group (`None` on the loopback seams, and
    /// before all-joined) — finalize snapshots THIS group instead of
    /// building a second one.
    pub(crate) fn nostr_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>> {
        self.nostr.as_ref().and_then(|n| n.group.clone())
    }

    /// The 445 group channel (set with the group at birth).
    pub(crate) fn nostr_chan(&self) -> Option<molt_net::ritual_net::GroupChannel> {
        self.nostr.as_ref().and_then(|n| n.chan.clone())
    }

    /// What the founder's materialize seals into `transport.state`: the
    /// Nostr shape (kind + relays + rotation seed) on the production path,
    /// the legacy default on the loopback seams.
    pub(crate) fn transport_shape(&self) -> crate::lifecycles::TransportShape {
        match &self.nostr {
            Some(n) => match n.rotation_seed {
                Some(seed) => crate::lifecycles::TransportShape::nostr(n.relays.clone(), seed),
                None => crate::lifecycles::TransportShape::default(),
            },
            None => crate::lifecycles::TransportShape::default(),
        }
    }

    /// This ritual's incarnation (the bootstrap's late results are bound to it).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The nostr anchors of every seat that has activated its link — the
    /// members an abort must reach before the group is born.
    pub(crate) fn anchored_nostr_pks(&self) -> Vec<String> {
        self.seats
            .iter()
            .filter_map(|s| s.identity.as_ref().map(|i| i.nostr_pk.clone()))
            .collect()
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

/// What a started ritual hands back: the seat links, plus run-log notes the
/// caller must apply itself (`cmd_create_start` replaces `session.create`
/// wholesale, so a line pushed inside `start_ritual` would be wiped).
pub(crate) struct RitualStart {
    pub(crate) links: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl State {
    /// Begin the founding ritual: derive the founder's identity, mint the
    /// invites, open the per-seat transport, and start the simulated
    /// members. Returns the invite preview links (for the seat rows) plus
    /// any run-log notes the caller must apply.
    pub(crate) fn start_ritual(
        &mut self,
        name: &str,
        founder_name: &str,
        rule_m: u8,
        rule_n: u8,
        seed_phrase: &str,
        // the founder's deliberate relay pick; empty = this node's whole
        // dialable pool (the pre-pick behaviour)
        picked_relays: &[String],
    ) -> Result<RitualStart, String> {
        // notes the CALLER must apply: `cmd_create_start` replaces
        // `session.create` wholesale after this returns, so a run-log line
        // pushed from in here would be discarded
        let mut notes: Vec<String> = Vec::new();
        let entropy = molt_storage::seed_entropy(seed_phrase).map_err(|e| e.to_string())?;
        let ws_id = molt_storage::derive_workspace_id(&entropy, founder_name);
        let (founder_sk, founder_pk) = molt_storage::derive_identity_key(&entropy, &ws_id);
        // the founder's own third anchor: same ticket-salted derivation as
        // every member's, salted with a random SELF-ticket. The ticket is
        // salt only — minted here, dropped at the end of this scope (the
        // ritual is ephemeral pre-seal); the derived secret rides the
        // runtime until the seal persists it beside identity_sk.
        let self_ticket = invite::mint_ticket().map_err(|e| e.to_string())?;
        let (mut founder_nostr_raw, founder_nostr_pk) =
            molt_net::nostr_identity(&entropy, &self_ticket);
        // one long-lived carrier for the scalar (wiped on ritual teardown);
        // the stack copy is wiped here — the derivation's reject-path hygiene
        // (nostr.rs) extends to the accepted secret's hops
        let founder_nostr_sk = zeroize::Zeroizing::new(founder_nostr_raw.to_vec());
        zeroize::Zeroize::zeroize(&mut founder_nostr_raw);
        let founder = MemberIdentity {
            member: founder_name.to_string(),
            identity_pk: founder_pk,
            nostr_pk: founder_nostr_pk,
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
        // the link's secret, minted without any I/O.
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
                backup_confirmed: false,
                parked_backup: None,
            });
        }

        // A production founding (no test seam) runs over Nostr (N4a): the
        // fail-closed dialer + the operator's confirmed relay pool are the
        // prerequisites, and both failures are honest, named refusals.
        let mut nostr = None;
        let mut sim = Vec::new();
        // the loopback transport of the test seams; on the Nostr path it is
        // an inert placeholder (no queue is ever created on it)
        let transport;
        if !manual && !self.ritual_sim {
            let dialer = self
                .dialer_for()
                .map_err(|e| format!("transport: {e}"))?;
            // …and when there is nothing to dial, say WHICH of the three
            // reasons it is: an operator whose relay reads `confirmed = true`
            // in their own config was told to confirm it again, while the
            // switch that actually blocked them went unnamed (2026-08-01
            // report). `pool_gap` is Some exactly when `dialable` is empty.
            if let Some(gap) = molt_core::relay::pool_gap(
                &self.session.settings.relays,
                self.clearnet_session,
            ) {
                return Err(format!("cannot found: {}", crate::relay_msg::pool_gap_reason(gap)));
            }
            // An invite link and a Welcome payload are UNTRUSTED INPUT at the
            // far end, and both cap the relay list at MAX_PAYLOAD_RELAYS. That
            // cap was being applied to the founder's OWN pool, so an operator
            // who confirmed nine relays could not render a link at all and the
            // founding aborted. Cap what goes IN instead — in the pool's own
            // priority order (relay_pool.md: the order IS the priority).
            //
            // ONCE, here, upstream of every consumer: the joiner requires the
            // invite's relay set and the Welcome's to be byte-identical
            // (`nostr_ritual.rs` member_join), so capping in two places would
            // break every join over a >8 pool.
            let dialable = molt_core::relay::dialable(
                &self.session.settings.relays,
                self.clearnet_session,
            );
            // The founder's PICK, when there is one. It is the republic's
            // relay set — constitutional once R3 signs it into the genesis —
            // so it must be a deliberate choice, not whatever this settings
            // page happened to hold. A picked relay this node cannot dial is
            // REFUSED, never dropped: a republic founded on a relay its own
            // founder cannot reach is a republic nobody can join, and
            // silently shrinking the set would hide that.
            let chosen: Vec<String> = if picked_relays.is_empty() {
                dialable.clone()
            } else {
                if let Some(bad) = picked_relays.iter().find(|r| !dialable.contains(r)) {
                    return Err(format!("cannot found: {bad} is not dialable here"));
                }
                picked_relays.to_vec()
            };
            let over = chosen.len().saturating_sub(molt_net::welcome::MAX_PAYLOAD_RELAYS);
            let relays: Vec<String> = chosen
                .into_iter()
                .take(molt_net::welcome::MAX_PAYLOAD_RELAYS)
                .collect();
            if over > 0 {
                // never truncate silently: the operator must be able to tell
                // "using my whole pool" from "using the first eight of it"
                notes.push(format!(
                    "→ this node has {} dialable relays; the invite and the Welcome carry the first {} (pool order = priority - reorder in Settings)",
                    relays.len() + over,
                    molt_net::welcome::MAX_PAYLOAD_RELAYS
                ));
            }
            let net = molt_net::ritual_net::RitualNet::new(
                dialer.clone(),
                relays.clone(),
                &founder_nostr_sk,
            )
            .map_err(|e| format!("transport keys: {e}"))?;
            // the inbox task: subscribe the founder's 1059 inbox, THEN
            // surface the v2 links (subscribe-before-advertise), then feed
            // every gift-wrapped JoinRequest into the actor's ladder
            let seat_invites = seat_setup
                .iter()
                .map(|(seat, ticket, _)| crate::nostr_ritual::SeatInvite {
                    seat: *seat,
                    ticket: ticket.clone(),
                    info: molt_core::InviteInfo {
                        republic: name.to_string(),
                        threshold: rule_m,
                        members: rule_n,
                        inviter: founder_name.to_string(),
                        ticket: ticket[..10].to_string(),
                    },
                })
                .collect();
            let inbox_task = crate::nostr_ritual::spawn_founder_inbox(
                net.clone(),
                seat_invites,
                generation,
                cmd_tx.downgrade(),
            );
            nostr = Some(NostrRitual {
                net,
                dialer,
                relays,
                rotation_seed: None,
                group: None,
                chan: None,
                tasks: vec![inbox_task],
            });
            transport = LoopbackHub::calm().transport();
        } else {
            // loopback dev seams: the hub creates queues synchronously. The
            // manual seam hands the per-seat material to the waiting
            // instance(s); the sim seam spawns simulated members.
            let hub = LoopbackHub::calm();
            transport = hub.transport();
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
        }

        self.net_ritual = Some(RitualRuntime {
            transport,
            name: name.to_string(),
            agenda: String::new(),
            // the automated sim seam has no human founder to deliberate, so it
            // pre-proposes and seals on all-joined (its name, empty agenda);
            // every real founding waits for the founder's explicit charter
            charter_proposed: self.ritual_sim,
            features: None,
            rule_m,
            rule_n,
            founder,
            founder_sk,
            founder_nostr_sk,
            seats,
            generation,
            _sim: sim,
            seq: std::sync::atomic::AtomicU64::new(0),
            nostr,
        });
        Ok(RitualStart { links, notes })
    }

    /// Tear the ritual down (cancel or completion): drops the hub, its
    /// queues and the simulated members. Also reaps any in-flight founder mesh
    /// bootstrap — dropping its `ct_tx` closes the task's inbound channel, which
    /// cascades the whole bootstrap to shut down and release the founding star
    /// (an abandoned founding must not leave a task blocked forever).
    /// End the ritual AND tell the members, then tear down.
    ///
    /// `teardown_ritual` alone is silent: it drops the founder's state and
    /// leaves every member sitting in an unbounded wait forever, unable to
    /// tell a dead founding from a slow one. The abort travels on BOTH paths
    /// because a member listens on exactly one of them depending on how far
    /// the ritual got:
    /// - before group birth the member is on its 1059 gift-wrap inbox, so the
    ///   abort goes per anchored seat as a wrap;
    /// - after birth it is on the 445 group channel, so it goes as a group
    ///   frame.
    ///
    /// Fire-and-forget: the outbound tasks own their clones, and
    /// `NostrRitual`'s Drop aborts only INBOUND tasks, so the sends survive
    /// the teardown. A no-op on loopback and on a ritual that never started.
    pub(crate) fn abandon_ritual(&mut self, reason: &str) {
        if let Some(ritual) = &self.net_ritual {
            let gen = ritual.generation();
            if let Some(nostr) = &ritual.nostr {
                let msg = invite::RitualMsg::Aborted { reason: reason.to_string() };
                // pre-birth: every seat that anchored an identity is waiting
                // on its own gift-wrap inbox
                for seat in ritual.anchored_nostr_pks() {
                    let net = nostr.net.clone();
                    let msg = msg.clone();
                    tokio::spawn(async move {
                        if let Err(e) = net.send_ritual(&seat, &msg).await {
                            tracing::warn!(error = %e, "abort wrap did not publish");
                        }
                    });
                }
                // post-birth: they moved to the group channel
                if let (Some(group), Some(chan), Some(tx)) =
                    (nostr.group.clone(), nostr.chan.clone(), self.cmd_tx.upgrade())
                {
                    crate::nostr_ritual::spawn_publish_frame(
                        chan,
                        crate::nostr_ritual::FramePayload::Encrypt(group, Box::new(msg)),
                        "abort",
                        crate::nostr_ritual::RetryPolicy::PRE_SEAL,
                        tx.downgrade(),
                        // its OWN generation, never None: `None` means
                        // "current in every incarnation", so a failed
                        // farewell would fail whatever founding is running
                        // ~30 s later
                        Some(gen),
                        String::new(),
                    );
                }
            }
        }
        self.teardown_ritual();
    }

    pub(crate) fn teardown_ritual(&mut self) {
        self.net_ritual = None;
        self.founder_mesh_in = None;
        self.runtime_transport = None;
        // the once-guard belongs to the ritual that is going away
        self.seal_published = false;
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

/// One delivery on a ritual queue: unwrap under `wrap`, feed the
/// reassembler, ACK (every delivery is acked — a block that does not unwrap
/// is noise, a partial message is held by `reasm`), and hand back the
/// complete payload when this delivery finished one.
pub(crate) fn take_delivery(
    wrap: &WrapKey,
    reasm: &mut molt_net::Reassembler,
    delivery: Delivery,
) -> Option<Vec<u8>> {
    let Ok(plain) = molt_net::wrap::unwrap_block(wrap, &delivery.block) else {
        delivery.ack.ack();
        return None;
    };
    let outcome = reasm.push(&plain);
    delivery.ack.ack();
    match outcome {
        Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) => Some(bytes),
        _ => None,
    }
}

/// The next complete framed payload on a subscribed ritual queue — the
/// receive twin of [`supervisor::send_framed`]; `None` once the queue
/// closed. The caller parses (what a queue carries differs per loop).
pub(crate) async fn next_framed_msg(
    rx: &mut mpsc::Receiver<Delivery>,
    wrap: &WrapKey,
    reasm: &mut molt_net::Reassembler,
) -> Option<Vec<u8>> {
    while let Some(delivery) = rx.recv().await {
        if let Some(bytes) = take_delivery(wrap, reasm, delivery) {
            return Some(bytes);
        }
    }
    None
}

/// The founder's recv loop on one invite queue: unwrap, reassemble, parse
/// a [`invite::RitualMsg`], and issue the matching internal command. The
/// loopback twin of `nostr_ritual::spawn_founder_inbox`.
fn spawn_founder_recv(
    transport: LoopbackTransport,
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
        while let Some(bytes) = next_framed_msg(&mut rx, &wrap, &mut reasm).await {
            let Ok(msg) = serde_json::from_slice::<invite::RitualMsg>(&bytes) else {
                continue;
            };
            let cmd = match msg {
                invite::RitualMsg::Join(j) => Command::NetJoinRequested {
                    seat,
                    member: j.name,
                    identity_pk: j.identity_pk,
                    nostr_pk: j.nostr_pk,
                    proof: j.mac,
                    // the member's reply-queue handover, opaque to core
                    reply: j
                        .reply
                        .as_ref()
                        .and_then(|r| serde_json::to_string(r).ok())
                        .unwrap_or_default(),
                    // loopback carries no gift wrap, so nothing is proven
                    sender_npub: String::new(),
                    key_package: j.key_package,
                    relays: j.relays,
                    generation: Some(generation),
                },
                invite::RitualMsg::Signed(s) => Command::NetSealSigned {
                    seat,
                    sig: s.sig,
                    // the private reply queue authenticated this
                    from: String::new(),
                    generation: Some(generation),
                },
                invite::RitualMsg::BackupConfirmed { sig, .. } => Command::NetBackupConfirmed {
                    seat,
                    sig,
                    // the private reply queue authenticated this
                    from: String::new(),
                    generation: Some(generation),
                },
                invite::RitualMsg::Declined { .. } => Command::NetJoinDeclined {
                    seat,
                    from: String::new(),
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
                | invite::RitualMsg::Aborted { .. }
                | invite::RitualMsg::Recover(_)
                | invite::RitualMsg::RecoverProgress { .. }
                | invite::RitualMsg::RecoverRefused { .. }
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

/// The founder-side pool-deviation line (2026-08-08): a joiner's declared
/// dial set vs the invite pool — `None` when every pool relay is reachable
/// or nothing was declared. One line, the count and the FIRST missing relay
/// (the remedy is the same for all of them: bridge the pool).
fn join_relay_deviation(member: &str, pool: &[String], declared: &[String]) -> Option<String> {
    if declared.is_empty() || pool.is_empty() {
        return None;
    }
    let unreachable: Vec<&String> = pool.iter().filter(|u| !declared.contains(u)).collect();
    let first = unreachable.first()?;
    Some(format!(
        "✗ {member} does not reach {} of {} pool relays - {first}",
        unreachable.len(),
        pool.len(),
    ))
}

/// Map a returning member's [`invite::RecoverRequest`] to the internal
/// [`Command::NetRecoverRequested`] — the coordinator recv loop's one decode.
/// The reply-queue handover is re-serialized to the opaque string core carries.
#[cfg_attr(not(test), allow(dead_code))] // wired by the recovery link-mint increment
/// `sender_npub` is the wrap's PROVEN author on the Nostr path, and empty on
/// the loopback one (a queue delivery has no wrap author to prove).
pub(crate) fn recover_command(
    r: invite::RecoverRequest,
    sender_npub: String,
    generation: u64,
) -> Command {
    Command::NetRecoverRequested {
        member: r.member,
        identity_pk: r.identity_pk,
        key_package: r.key_package,
        ticket: r.ticket,
        seat_proof: r.seat_proof,
        new_nostr_pk: r.new_nostr_pk,
        relays: r.relays,
        consent: r.consent,
        reply: r
            .reply
            .as_ref()
            .and_then(|h| serde_json::to_string(h).ok())
            .unwrap_or_default(),
        sender_npub,
        generation: Some(generation),
    }
}

/// The recovery coordinator's recv loop on its recovery queue — the twin of
/// [`spawn_founder_recv`]. It accepts a returning member's
/// [`invite::RitualMsg::Recover`] and issues [`Command::NetRecoverRequested`]
/// (the engine verifies the seat proof + proposes re-admission); any other
/// message on this queue is ignored.
pub(crate) fn spawn_coordinator_recv(
    transport: LoopbackTransport,
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
        while let Some(bytes) = next_framed_msg(&mut rx, &wrap, &mut reasm).await {
            // a recovery request, or — after the re-key — the rejoiner's mesh
            // announce (dynamic mesh membership); anything else is dropped
            let cmd = match serde_json::from_slice::<invite::RitualMsg>(&bytes) {
                Ok(invite::RitualMsg::Recover(r)) => recover_command(r, String::new(), generation),
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

/// One founding invite's full transport handover — everything a member's
/// node needs to activate and seal (transport concept §3.3: the payload
/// the `molt://invite/…` link will carry in-band once T3 encodes it).
/// Exposed for the two-instance dev test, where a *second* engine runs the
/// member side against the founder's hub.
#[doc(hidden)]
#[derive(Clone)]
pub struct InviteMaterial<T: molt_net::Transport = LoopbackTransport> {
    /// The seat this invite fills (0-based).
    pub seat: u32,
    /// The transport the founder reached the member over — the loopback hub
    /// today, a real relay once N4 lands. A genuinely separate instance uses
    /// its *own* transport and only reads the address / wrap / ticket below;
    /// `run_ritual_member` is generic over `T`.
    pub transport: T,
    /// member → founder queue (JoinRequest, then SealSigned).
    pub invite_snd: SndQueueAddr,
    pub invite_wrap: WrapKey,
    /// The single-use ticket.
    pub ticket: String,
}

/// A full founding-invite link: the [`molt_core::InviteInfo`] display preview
/// plus the **v2 handover** ([`molt_net::invite::InviteHandoverV2`] — founder
/// npub, invite relays, the FULL ticket) a *separate node* (a second moltd,
/// the GUI join flow) needs to join a founding over Nostr.
///
/// The URL is **neutral** (2026-08-08): `molt://invite/<segment>`, hex over a
/// versioned envelope carrying preview and handover — the URL itself names
/// neither the republic nor the inviter. (Hex is encoding, not encryption:
/// the link's holder can decode it — but the holder is the invitee, who must
/// learn those names anyway.) Pre-neutral path-shaped links still parse.
/// The pre-N4 queue-shaped handover is REJECTED with an honest message —
/// nothing real could ever join from it (it carried only a ticket prefix).
#[doc(hidden)]
#[derive(Debug)]
pub struct FoundingInvite {
    /// The display preview (republic, m/n, inviter, ticket prefix).
    pub info: molt_core::InviteInfo,
    /// The Nostr transport handover (seat, full ticket, founder npub,
    /// invite relays).
    pub handover: molt_net::invite::InviteHandoverV2,
}

/// The neutral link's envelope: the display preview plus the v2 handover's
/// wire JSON, hexed once as the link's single segment.
#[derive(serde::Serialize, serde::Deserialize)]
struct NeutralInviteWire {
    v: u8,
    republic: String,
    m: u8,
    n: u8,
    inviter: String,
    ticket: String,
    h2: String,
}

/// The neutral-envelope version — the third link generation (path+queue,
/// path+v2, neutral). Distinct from the inner handover's version (v2).
const INVITE_LINK_VERSION: u8 = 3;

impl FoundingInvite {
    /// Render the full joinable link as its one neutral hex segment.
    pub fn render(&self) -> Result<String, String> {
        let blob = self.handover.encode().map_err(|e| e.to_string())?;
        let wire = NeutralInviteWire {
            v: INVITE_LINK_VERSION,
            republic: self.info.republic.clone(),
            m: self.info.threshold,
            n: self.info.members,
            inviter: self.info.inviter.clone(),
            ticket: self.info.ticket.clone(),
            // the inner WIRE JSON, not its hex — hexing once at the end
            // keeps the link half the length
            h2: String::from_utf8(hex::decode(&blob).unwrap_or_default()).unwrap_or_default(),
        };
        let json = serde_json::to_string(&wire).map_err(|e| e.to_string())?;
        Ok(format!("molt://invite/{}", hex::encode(json)))
    }

    /// Parse a neutral single-segment link — same sanity gates the path
    /// shape enforced, then the handover decode's own fail-closed ladder.
    fn parse_neutral(segment: &str) -> Result<FoundingInvite, String> {
        let bytes =
            hex::decode(segment).map_err(|_| "not an invite link".to_string())?;
        let text =
            String::from_utf8(bytes).map_err(|_| "not an invite link".to_string())?;
        let wire: NeutralInviteWire =
            serde_json::from_str(&text).map_err(|_| "not an invite link".to_string())?;
        if wire.v != INVITE_LINK_VERSION {
            return Err(format!(
                "unsupported invite link version {} - this build reads v{INVITE_LINK_VERSION}",
                wire.v
            ));
        }
        let info = molt_core::InviteInfo {
            republic: wire.republic,
            threshold: wire.m,
            members: wire.n,
            inviter: wire.inviter,
            ticket: wire.ticket,
        };
        if info.republic.trim().is_empty()
            || info.inviter.is_empty()
            || info.ticket.len() < 4
            || info.threshold == 0
            || info.members < 2
            || info.threshold > info.members
        {
            return Err("not an invite link".to_string());
        }
        let handover = molt_net::invite::InviteHandoverV2::decode(&hex::encode(wire.h2.as_bytes()))
            .map_err(|e| e.to_string())?;
        Ok(FoundingInvite { info, handover })
    }

    /// Parse a full founding link — the error is surfaced to the joiner, so
    /// it distinguishes "no handover at all" (a bare preview link) from a
    /// malformed/older handover.
    pub fn parse(link: &str) -> Result<FoundingInvite, String> {
        let trimmed = link.trim();
        let rest = trimmed
            .strip_prefix("molt://invite/")
            .ok_or_else(|| "not an invite link".to_string())?;
        if !rest.contains('/') {
            return Self::parse_neutral(rest);
        }
        // the pre-neutral path shape
        let info = molt_core::InviteInfo::parse(trimmed)
            .ok_or_else(|| "not an invite link".to_string())?;
        let (head, blob) = trimmed
            .rsplit_once('/')
            .ok_or_else(|| "not an invite link".to_string())?;
        // a bare preview link's last segment is the ticket prefix itself —
        // the preview parse of `head` then fails, which tells them apart
        // without guessing on the blob's shape
        if molt_core::InviteInfo::parse(head).is_none() {
            return Err("not a joinable invite link - it carries no transport details".into());
        }
        let handover =
            molt_net::invite::InviteHandoverV2::decode(blob).map_err(|e| e.to_string())?;
        Ok(FoundingInvite { info, handover })
    }
}

/// Every seat's third anchor must be a valid, canonical, roster-unique
/// nostr key — the check EVERY member can (and must) run for EVERY seat.
/// A member can only compare its OWN seat's value against its derivation;
/// format and uniqueness are the verifiable properties of the others'
/// anchors, and threshold-signing a malformed, second-byte-form, or
/// duplicated anchor would seal it into the signed roster bytes and the
/// republic-id-v2 preimage forever. Empty is rejected too: the one
/// founding path always fills the anchor, so an empty value on a founding
/// seat is an attacker aliasing the legacy marker — the legitimately empty
/// anchors of chain-derived later-`Joined` seats never pass through the
/// two callers below (both verify freshly-sealed FOUNDING rosters only;
/// `sealed_roster_from_blob` documents it must not be routed here).
fn check_roster_anchors(identities: &[molt_core::MemberIdentity]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    // HANDLES must be unique too, not just transport anchors. The chain layer
    // ALREADY assumes it — `valid_signers` resolves a signature by
    // `identities.find(|i| i.member == att.member)` and counts a
    // `BTreeSet<String>` of NAMES — so a republic with two seats called
    // `walter` can never count both as distinct signers and is permanently
    // ungovernable once m exceeds the number of distinct handles. It is also
    // what every "is this the founder?" comparison rests on.
    let mut names = std::collections::BTreeSet::new();
    for id in identities {
        if !names.insert(id.member.as_str()) {
            return Err(format!(
                "two seats share the handle {} - every seat must be distinguishable",
                id.member
            ));
        }
    }
    for id in identities {
        let canonical = molt_net::canonical_nostr_pk(&id.nostr_pk)
            .map_err(|e| format!("seat {} carries an invalid nostr anchor: {e}", id.member))?;
        if canonical != id.nostr_pk {
            return Err(format!(
                "seat {} carries a non-canonical nostr anchor (one key, one signed-byte form)",
                id.member
            ));
        }
        if !seen.insert(id.nostr_pk.as_str()) {
            return Err(format!(
                "seat {} shares its nostr anchor with another seat",
                id.member
            ));
        }
    }
    Ok(())
}

/// The `roster` field must be exactly the identities' members, in order.
///
/// `SealedRoster.roster` is CONSTITUTIONAL — it becomes the `Founded` event's
/// member list and thus `State::roster()` — but it is covered by NO signature:
/// it is absent from `roster_canonical_bytes` (every version) and from
/// `republic_id` (molt-republic-id-v2). Without this check every attestation
/// can verify over an honest identity table while the member list a member
/// actually reads names a different set.
///
/// Checked rather than bound into the signed bytes (user decision
/// 2026-08-01): the field is fully DERIVABLE from `identities`, so equality
/// closes the hole with no byte-layout bump and no recompute-site ripple.
/// Recovery already derives it (`recovery.rs::sealed_roster_from_blob`).
/// The constitutional numbers must describe the table they ride with:
/// `n` seats in the identity table and a threshold inside `1..=n` — the
/// same shape `verify_genesis` enforces, checked here before a member
/// ratifies and before a joiner writes anything (review 2026-08-25).
/// A member handle as it may be anchored: non-empty, at most
/// [`MAX_HANDLE_CHARS`] characters, one line, no control characters — it
/// becomes forever-bytes in the roster and one line of every run log.
pub(crate) fn check_handle(handle: &str) -> Result<(), String> {
    let handle = handle.trim();
    if handle.is_empty() {
        return Err("the handle must not be empty".to_string());
    }
    if handle.chars().count() > MAX_HANDLE_CHARS {
        return Err(format!("the handle is too long (max {MAX_HANDLE_CHARS} characters)"));
    }
    if handle.chars().any(char::is_control) {
        return Err("the handle must be one line without control characters".to_string());
    }
    Ok(())
}

/// The longest handle a seat may carry.
pub(crate) const MAX_HANDLE_CHARS: usize = 64;

fn check_rule_shape(rule_m: u8, rule_n: u8, seats: usize) -> Result<(), String> {
    if usize::from(rule_n) != seats {
        return Err(format!("rule n={rule_n} does not match {seats} seats"));
    }
    if rule_m == 0 || rule_m > rule_n {
        return Err(format!("rule m={rule_m} is outside 1..={rule_n}"));
    }
    Ok(())
}

fn check_roster_matches_identities(
    roster: &[String],
    identities: &[molt_core::MemberIdentity],
) -> Result<(), String> {
    if roster.len() != identities.len() {
        return Err(format!(
            "the roster names {} member(s) but the signed identity table has {}",
            roster.len(),
            identities.len()
        ));
    }
    for (seat, id) in roster.iter().zip(identities) {
        if seat != &id.member {
            return Err(format!(
                "the roster names {seat} where the signed identity table has {}",
                id.member
            ));
        }
    }
    Ok(())
}

/// Verify a distributed sealed roster before trusting it: the republic id
/// must be the neutral content-derived value (v2 — committing to every
/// member's identity/nostr anchor PAIR), every seat's nostr anchor must be
/// valid, canonical and unique ([`check_roster_anchors`]), every
/// attestation must verify against its member's anchored key over the
/// canonical table (v3 — the nostr anchors are inside the signed bytes),
/// and every member must have signed (n identities, n attestations).
pub(crate) fn verify_sealed_roster(s: &molt_core::SealedRoster) -> Result<(), String> {
    let rid = molt_storage::republic_id(&s.name, s.rule_m, s.rule_n, &s.identities);
    if rid != s.republic_id {
        return Err("republic id does not match the roster content".to_string());
    }
    check_roster_anchors(&s.identities)?;
    // the unsigned constitutional field: it must be exactly what the signed
    // identity table says, or the member list diverges from what was ratified
    check_roster_matches_identities(&s.roster, &s.identities)?;
    check_rule_shape(s.rule_m, s.rule_n, s.identities.len())?;
    // one attestation per DISTINCT member: `[A, A, B]` over three seats is
    // not "fully signed" (the same rule `verify_genesis` enforces — here it
    // runs BEFORE anything reaches disk)
    let signers: std::collections::BTreeSet<&str> =
        s.attestations.iter().map(|a| a.member.as_str()).collect();
    if s.attestations.len() != s.identities.len() || signers.len() != s.identities.len() {
        return Err("roster is not fully signed by every member".to_string());
    }
    // one set, one byte form, no key this build cannot render — the same
    // rule the ratifying member enforces (review 2026-08-12: an unrendered
    // key silently signed into forever-bytes is the sign-what-you-see hole)
    if let Some(features) = &s.features {
        molt_core::verify_canonical_features(features)?;
    }
    // recompute over the sealed charter too: if the founder put a different
    // name/agenda in the genesis than the members ratified, their signatures
    // (made over the Seal's table) fail here — the charter is tamper-evident
    let table =
        molt_core::roster_canonical_bytes(
            &s.republic_id,
            s.rule_m,
            s.rule_n,
            &s.identities,
            &s.agenda,
            &s.relays,
            s.features.as_deref(),
        );
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
pub(crate) fn seat_proof_bytes(
    ticket: &str,
    key_package_hex: &str,
    republic_id: &str,
    new_nostr_pk: &str,
    relays: &[String],
) -> Vec<u8> {
    // v2 (N4b): the NEW transport anchor is inside the signed bytes. Without
    // it a captured proof could be replayed with somebody else's transport
    // key substituted, re-anchoring the seat's traffic to them while every
    // signature still verified.
    //
    // Length-prefixed, not separator-joined: a member supplies these fields,
    // and a separator-only preimage lets one field's content impersonate the
    // boundary (the N1 CRITICAL — see `hash-length-prefix-not-separators`).
    let mut m = Vec::with_capacity(
        20 + ticket.len() + key_package_hex.len() + republic_id.len() + new_nostr_pk.len() + 16,
    );
    m.extend_from_slice(b"molt-seat-proof-v2\0");
    for field in [ticket, key_package_hex, republic_id, new_nostr_pk] {
        let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
        m.extend_from_slice(&len.to_le_bytes());
        m.extend_from_slice(field.as_bytes());
    }
    // R5: the relay declaration, bound CONDITIONALLY — an empty one signs
    // the exact v2 bytes (every pre-R5 proof keeps verifying); a non-empty
    // one extends with a marker + counted run, so "no declaration" and a
    // stripped one can never verify against each other.
    if !relays.is_empty() {
        m.push(1);
        m.extend_from_slice(&u32::try_from(relays.len()).unwrap_or(u32::MAX).to_le_bytes());
        for r in relays {
            let len = u32::try_from(r.len()).unwrap_or(u32::MAX);
            m.extend_from_slice(&len.to_le_bytes());
            m.extend_from_slice(r.as_bytes());
        }
    }
    m
}

/// The **rejoiner** builds its seat proof: sign the canonical bytes with the
/// identity key re-derived from its recovery phrase. Returns the signature (hex).
pub fn make_seat_proof(
    identity_sk: &molt_storage::SigningKey,
    ticket: &str,
    key_package_hex: &str,
    republic_id: &str,
    new_nostr_pk: &str,
    relays: &[String],
) -> String {
    molt_storage::identity_sign(
        identity_sk,
        &seat_proof_bytes(ticket, key_package_hex, republic_id, new_nostr_pk, relays),
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
    new_nostr_pk: &str,
    relays: &[String],
    sig_hex: &str,
) -> bool {
    molt_storage::identity_verify(
        anchored_pk,
        &seat_proof_bytes(ticket, key_package_hex, republic_id, new_nostr_pk, relays),
        sig_hex,
    )
}

/// Verify a `Seal` proposal before ratifying it, and return the exact canonical
/// bytes to sign. The republic id must be the content-derived value (no forged
/// salt), and our own `(name, key)` must be in the roster — otherwise a founder
/// could have us ratify a constitution we are not part of. Sign-what-you-see
/// extends to the THIRD anchor: our seat's `nostr_pk` must be exactly the key
/// WE derived (`nostr_pk`), or a malicious founder could anchor an
/// attacker-controlled transport key for us — MLS still binds Ed25519, but our
/// future gift-wrapped material (Welcomes, recovery) would flow to the
/// attacker. Recomputing the table here (rather than trusting an opaque blob)
/// is what makes the signature a ratification of exactly the name + agenda +
/// roster the member is shown.
pub(crate) fn verify_seal_proposal(
    proposal: &molt_core::SealedRoster,
    name: &str,
    pk: &str,
    nostr_pk: &str,
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
    // format + uniqueness for EVERY seat, not just ours: we can only compare
    // our own anchor's VALUE, but we must never ratify a table that seals a
    // malformed or duplicated anchor for a peer
    check_roster_anchors(&proposal.identities)?;
    // sign-what-you-see extends to the member list itself: a member must not
    // ratify a table whose roster names a set its identities do not back
    check_roster_matches_identities(&proposal.roster, &proposal.identities)?;
    check_rule_shape(proposal.rule_m, proposal.rule_n, proposal.identities.len())?;
    let Some(our_seat) = proposal
        .identities
        .iter()
        .find(|i| i.member == name && i.identity_pk == pk)
    else {
        return Err("the proposed roster does not anchor our own (name, key)".to_string());
    };
    if our_seat.nostr_pk != nostr_pk {
        return Err(
            "the proposed roster anchors a nostr transport key for us that we did not derive"
                .to_string(),
        );
    }
    // one set, one byte form — and NO key this build cannot render: the
    // ratify card shows exactly the known vocabulary, so a foreign key
    // would be signed sight-unseen into forever-bytes (review 2026-08-12).
    // A newer-build founder against an older member fails honestly here,
    // like the m-of-n mismatch gate.
    if let Some(features) = &proposal.features {
        molt_core::verify_canonical_features(features)?;
    }
    Ok(molt_core::roster_canonical_bytes(
        &proposal.republic_id,
        proposal.rule_m,
        proposal.rule_n,
        &proposal.identities,
        &proposal.agenda,
        &proposal.relays,
        proposal.features.as_deref(),
    ))
}

pub use crate::ritual_member::{JoinOutcome, Ratifier};

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
        let Some(bytes) = take_delivery(wrap, reasm, delivery) else {
            continue;
        };
        if let Ok(msg) = serde_json::from_slice::<invite::RitualMsg>(&bytes) {
            return Ok(msg);
        }
    }
}

/// The member's per-workspace identity keypair, derived deterministically from
/// its own recovery phrase — the ONE derivation both the ritual (which anchors
/// the public key in the roster) and the join finish (which needs the private
/// key to sign chain governance) must agree on. Returns `(signing key, pk hex)`.
///
/// Public so a test can build a request the way a real seat does: forking
/// the salt convention into a test would defeat the "ONE derivation" the
/// paragraph above is about.
pub fn member_identity(
    phrase: &str,
) -> Result<(molt_storage::SigningKey, String), String> {
    let entropy = molt_storage::seed_entropy(phrase).map_err(|e| e.to_string())?;
    Ok(member_identity_from_entropy(&entropy))
}

/// Resolve the identity keypair a seat's RECOVERY presents
/// (`recovery_auto_approval.md` WP7, field bug 2026-08-23): the ritual
/// anchored the FOUNDER's key salted with a name-derived workspace id
/// ([`State::start_ritual`]) but every joiner's with the fixed "member" tag —
/// and the phrase alone cannot say which kind of seat it re-derives.
/// `anchored` (the seat's identity pk, from the recovery link or a verified
/// chain head) picks the matching convention; an EMPTY hint keeps the legacy
/// member-convention behavior (old links). A phrase that derives NEITHER is
/// refused here — locally, before any network round. The restore path's
/// twin lives at `apply_restore_staged` (both-derivations-verified-head).
pub(crate) fn seat_identity(
    phrase: &str,
    member: &str,
    anchored: &str,
) -> Result<(molt_storage::SigningKey, String), String> {
    let entropy = molt_storage::seed_entropy(phrase).map_err(|e| e.to_string())?;
    let member_kp = member_identity_from_entropy(&entropy);
    if anchored.is_empty() || member_kp.1 == anchored {
        return Ok(member_kp);
    }
    // the founder convention: start_ritual salts with the name-derived
    // workspace id
    let ws_id = molt_storage::derive_workspace_id(&entropy, member);
    let founder_kp = molt_storage::derive_identity_key(&entropy, &ws_id);
    if founder_kp.1 == anchored {
        return Ok(founder_kp);
    }
    Err("the phrase does not derive this seat's identity key".to_string())
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

/// The loopback leg of the member ladder ([`crate::ritual_member`]): the
/// member's reply queue on the founder's hub (subscribed BEFORE it is
/// advertised, so the founder's table can never race ahead of the
/// subscription — each party owns the queue it receives on) and the
/// founder's invite queue for everything outbound. The private reply queue
/// is the authenticator: only the founder holds its address and wrap key.
struct LoopbackLeg<T: molt_net::Transport> {
    transport: T,
    invite_snd: SndQueueAddr,
    invite_wrap: WrapKey,
    reply_snd: SndQueueAddr,
    reply_wrap: WrapKey,
    /// The reply-queue reader; taken by the mesh bootstrap at the end.
    rx: Option<mpsc::Receiver<Delivery>>,
    reasm: molt_net::Reassembler,
    cancel: Option<mpsc::Receiver<()>>,
    name: String,
    /// Outbound frame counter (the msg ids only need to be distinct).
    seq: u64,
    /// Run the post-founding mesh bootstrap after the genesis.
    bootstrap: bool,
}

impl<T: molt_net::Transport> RitualLeg for LoopbackLeg<T> {
    async fn next_msg(
        &mut self,
        _phase: Phase,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<invite::RitualMsg, String> {
        let Some(rx) = self.rx.as_mut() else {
            return Err("queue closed".to_string());
        };
        match deadline {
            None => next_ritual_msg(rx, &mut self.cancel, &self.reply_wrap, &mut self.reasm).await,
            Some(d) => match tokio::time::timeout_at(
                d,
                next_ritual_msg(rx, &mut self.cancel, &self.reply_wrap, &mut self.reasm),
            )
            .await
            {
                Ok(msg) => msg,
                Err(_) => Err("the inviter did not answer - ask the founder for a fresh link".to_string()),
            },
        }
    }

    async fn send(&mut self, msg: &invite::RitualMsg) -> Result<(), String> {
        let payload = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        self.seq += 1;
        supervisor::send_framed(
            &self.transport,
            &self.invite_snd,
            &self.invite_wrap,
            msg_id(&self.name, "founder", self.seq),
            &payload,
        )
        .await
        .map_err(|e| e.to_string())
    }

    fn reply_handover(&self) -> Option<invite::ReplyHandover> {
        Some(invite::ReplyHandover {
            server: self.reply_snd.server.clone(),
            queue_id: hex::encode(&self.reply_snd.id.0),
            wrap: hex::encode(self.reply_wrap.to_bytes()),
        })
    }

    fn declared_relays(&self) -> Vec<String> {
        // the loopback path has no relays to declare
        Vec::new()
    }

    fn in_group(&self) -> bool {
        // the Welcome rides the genesis; the ladder joins from it
        false
    }

    async fn finish(
        &mut self,
        name: &str,
        sealed: &molt_core::SealedRoster,
        group: &Arc<Mutex<molt_net::MlsMember>>,
        early: Vec<Vec<u8>>,
    ) -> Option<Vec<molt_core::MeshLink>> {
        if !self.bootstrap {
            return None;
        }
        let rx = self.rx.take()?;
        let reasm = std::mem::replace(&mut self.reasm, molt_net::Reassembler::new());
        let peers: Vec<MemberId> = sealed.roster.iter().filter(|r| *r != name).cloned().collect();
        // best-effort: a bootstrap that times out or errors still lets us
        // enter, just without a direct mesh (the group is already in hand;
        // the mesh can be re-established later)
        match crate::loopback_mesh::member_bootstrap(
            name,
            peers,
            &self.transport,
            self.invite_snd.clone(),
            self.invite_wrap.clone(),
            self.reply_wrap.clone(),
            rx,
            reasm,
            early,
            group.clone(),
        )
        .await
        {
            Ok(mesh) => Some(mesh),
            Err(e) => {
                tracing::warn!(error = %e, "mesh bootstrap did not complete; entering without a direct mesh");
                None
            }
        }
    }
}

/// Run the **member side** of the founding ritual over the loopback
/// transport — the ONE ladder ([`crate::ritual_member::run_member_ladder`])
/// on a [`LoopbackLeg`]: derive the member's own identity, build its MLS
/// `KeyPackage`, activate the seat (ticket MAC), ratify the charter behind
/// `ratify` (`None` = sign as soon as the table verifies), and — when
/// `collect_genesis` is set — receive and verify the sealed roster + Welcome
/// (`bootstrap` = then assemble the post-founding mesh over the star).
/// `cancel` (if any) ends any wait early (ritual teardown). Returns the
/// member's [`JoinOutcome`]. The founder's simulated members and a genuinely
/// separate test instance both run exactly this.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn run_ritual_member<T: molt_net::Transport>(
    m: InviteMaterial<T>,
    name: String,
    phrase: String,
    collect_genesis: bool,
    bootstrap: bool,
    ratify: Option<Ratifier>,
    cancel: Option<mpsc::Receiver<()>>,
) -> Result<JoinOutcome, String> {
    let seat = MemberSeat::derive(m.seat, &m.ticket, &phrase)?;
    // subscribe BEFORE the JoinRequest advertises the queue
    let reply_q = m.transport.create_queue().await.map_err(|e| e.to_string())?;
    let reply_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
    let rx = m
        .transport
        .subscribe(&reply_q.rcv)
        .await
        .map_err(|e| e.to_string())?;
    let mut leg = LoopbackLeg {
        transport: m.transport,
        invite_snd: m.invite_snd,
        invite_wrap: m.invite_wrap,
        reply_snd: reply_q.snd,
        reply_wrap,
        rx: Some(rx),
        reasm: molt_net::Reassembler::new(),
        cancel,
        name: name.clone(),
        seq: 0,
        bootstrap,
    };
    run_member_ladder(&mut leg, &name, seat, ratify, collect_genesis).await
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

/// What the single-use ticket says about an activation of an already
/// anchored seat (`State::spent_seat`).
enum SpentSeat {
    /// The seat is not anchored — run the ladder.
    Open,
    /// The anchored member's own request, redelivered — drop it silently.
    Silent,
    /// Told and logged — drop it.
    Refused,
    /// The same person re-activating before the group is born: run the
    /// ladder; on success the anchor `displaced` is told it was replaced.
    ReAnchor { displaced: String },
}

/// The ritual command handlers (`cmd_net_join_requested`,
/// `cmd_net_seal_signed`), split out so the transport plumbing above stays
/// readable. They are inherent `State` methods — no re-export needed.
mod ritual_ops {
    use super::*;

    impl State {
        /// A founding seat's real invite link became available (its invite
        /// queue is now provisioned — dormant until N4's Nostr provisioning
        /// re-emits it). Replace the seat's preview link with the joinable
        /// one, so the founder's GUI shows a link a separate node can use.
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

        /// The founder's off-actor queue provisioning failed (dormant until
        /// N4's Nostr provisioning re-emits it): fail the create run and tear
        /// the ritual down, so the wizard shows the error instead of waiting
        /// for links that will never come.
        /// The ritual task reports a NON-FATAL transport condition.
        ///
        /// Never sets `outcome = 2`: a one-shot `CreatePropose` must not be
        /// destroyed by a relay blip (it would lose every collected signature
        /// and force a re-mint). Deduped against the last line, because a
        /// deaf channel repeats its note on every poll.
        pub(crate) fn cmd_net_ritual_note(
            &mut self,
            note: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation)
                || self.session.create.run.outcome != 0
            {
                return Ok(molt_core::Reply::Ack);
            }
            if self.session.create.run.log.last() == Some(&note) {
                return Ok(molt_core::Reply::Ack);
            }
            self.session.create.run.log.push(note);
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// A publish task reported its REAL per-relay outcome.
        ///
        /// Four outcomes, three of which used to be invisible:
        /// - nothing accepted, pre-seal leg → the founding FAILS (this is the
        ///   hang that made the cluster: the founder sat on "charter proposed"
        ///   while every member waited for a frame no relay ever took);
        /// - nothing accepted, genesis → the founder HAS materialized, so the
        ///   run is not failed; the members were simply never told, and that
        ///   is surfaced as a notice the GUI toasts;
        /// - accepted by some, refused by others → a ⚠ line naming who
        ///   refused. Landing on 1 of 5 relays is not a failure, but it is
        ///   not the success the operator would otherwise assume;
        /// - clean → debug only.
        pub(crate) fn cmd_net_ritual_published(
            &mut self,
            what: &str,
            accepted: &[String],
            failed: &[String],
            generation: Option<u64>,
            workspace: &str,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            let detail = failed.join(" · ");
            if accepted.is_empty() {
                // The genesis is published AFTER maybe_finalize took the
                // ritual, so it is deliberately NOT generation-gated —
                // gating it would drop the report and recreate the exact
                // inertness this cluster exists to remove.
                if what == "genesis" {
                    tracing::error!(%workspace, %detail, "the genesis frame reached no relay");
                    // The genesis retries for ~45 s, so this can land after
                    // the operator started a DIFFERENT founding. It is
                    // therefore attributed by workspace id, never to whatever
                    // run happens to be on screen.
                    let ours = self.active.as_ref().is_some_and(|a| a.id == workspace);
                    if !ours {
                        tracing::error!(
                            %workspace,
                            "…and its workspace is no longer open - reopen it to see the notice"
                        );
                        return Ok(molt_core::Reply::Ack);
                    }
                    self.session.notice = format!("genesis-undelivered:{detail}");
                    // …and a DURABLE line inside the republic itself: the
                    // toast is edge-triggered and one-shot, so without this
                    // the only message saying "your republic exists here but
                    // nobody else was told" flashes once and is gone.
                    if let Err(e) = self.post_message_with_kind(
                        String::new(),
                        format!(
                            "⚠ genesis reached no relay ({detail}) - nobody can join until it is published"
                        ),
                        None,
                        molt_core::ChannelRef::Group,
                        molt_core::ChatKind::System,
                    ) {
                        tracing::warn!(error = %e, "could not post the undelivered-genesis notice");
                    }
                    self.emit_session(molt_core::SessionScope::Full);
                    return Ok(molt_core::Reply::Ack);
                }
                // `None` means "belongs to no ritual" — it must never be
                // read as "belongs to the current one" and fail a run that
                // has nothing to do with it (that is what the abort leg did).
                let Some(g) = generation else {
                    tracing::error!(what, %detail, "an ungenerationed ritual leg reached no relay");
                    return Ok(molt_core::Reply::Ack);
                };
                if !self.ritual_generation_current(Some(g)) {
                    return Ok(molt_core::Reply::Ack);
                }
                return self.cmd_net_ritual_failed(
                    format!("{what} did not publish: {detail}"),
                    Some(g),
                );
            }
            if !failed.is_empty() {
                if what != "genesis" && !self.ritual_generation_current(generation) {
                    return Ok(molt_core::Reply::Ack);
                }
                self.session.create.run.log.push(format!(
                    "⚠ {what} landed on {} of {} relays - {detail}",
                    accepted.len(),
                    accepted.len() + failed.len()
                ));
                self.emit_session(molt_core::SessionScope::Full);
                return Ok(molt_core::Reply::Ack);
            }
            tracing::debug!(what, relays = accepted.len(), "ritual frame published");
            Ok(molt_core::Reply::Ack)
        }

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
            self.session.create.run.headline = crate::relay_msg::headline_for(&error);
            self.session
                .create
                .run
                .log
                .push(format!("✗ founding failed: {error}"));
            // the founding died on our side — say so, do not just vanish
            self.abandon_ritual(&error);
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// N4a §4.2: **the group is born at all-joined** on the Nostr path.
        /// Build the founder's MLS group (one commit adds every seat), mint
        /// the h-tag rotation seed, gift-wrap one Welcome (payload v2) per
        /// seat, and stand the 445 recv up — all BEFORE any seal proposal,
        /// because deliberation/ratification run as 445s inside the group.
        /// Idempotent; a no-op on the loopback seams.
        pub(crate) fn nostr_group_birth(&mut self) {
            let Some(ritual) = &self.net_ritual else {
                return;
            };
            let Some(nostr) = &ritual.nostr else {
                return;
            };
            if nostr.group.is_some() {
                return;
            }
            let generation = ritual.generation;
            let fail = |state: &mut Self, e: String| {
                let _ = state.cmd_net_ritual_failed(format!("group birth: {e}"), Some(generation));
            };
            let (mls, welcome_hex) = match ritual.build_founder_mls() {
                Ok(x) => x,
                Err(e) => return fail(self, e),
            };
            let welcome_bytes = match hex::decode(&welcome_hex) {
                Ok(b) if !b.is_empty() => b,
                _ => return fail(self, "the group produced no Welcome".to_string()),
            };
            let seed = match molt_net::ritual_net::mint_rotation_seed() {
                Ok(s) => s,
                Err(e) => return fail(self, e.to_string()),
            };
            let Some(cmd_tx) = self.cmd_tx.upgrade() else {
                return;
            };
            let weak = cmd_tx.downgrade();
            let group = std::sync::Arc::new(std::sync::Mutex::new(mls));
            let (net, dialer, relays, targets) = {
                let Some(ritual) = &self.net_ritual else {
                    return;
                };
                let Some(n) = &ritual.nostr else { return };
                (
                    n.net.clone(),
                    n.dialer.clone(),
                    n.relays.clone(),
                    ritual
                        .seats
                        .iter()
                        .filter_map(|s| s.identity.as_ref().map(|i| i.nostr_pk.clone()))
                        .collect::<Vec<_>>(),
                )
            };
            let chan = molt_net::ritual_net::GroupChannel::new(dialer, relays.clone(), seed);
            // Welcome fan-out: outbound fire-and-forget tasks (they drain on
            // their own — never aborted); a failed publish is fatal for the
            // founding (that member could never deliberate) and reports
            // through the designed NetRitualFailed seam
            for npub in targets {
                let net = net.clone();
                let payload = molt_net::welcome::WelcomePayload {
                    welcome: welcome_bytes.clone(),
                    rotation_seed: seed,
                    relays: relays.clone(),
                };
                let weak = weak.clone();
                tokio::spawn(async move {
                    if let Err(e) = net.send_welcome(&npub, &payload).await {
                        tracing::error!(error = %e, "welcome did not publish");
                        let (reply, _rx) = tokio::sync::oneshot::channel();
                        if let Some(tx) = weak.upgrade() {
                            let _ = tx
                                .send(crate::Envelope {
                                    cmd: molt_core::Command::NetRitualFailed {
                                        error: format!("welcome did not publish: {e}"),
                                        generation: Some(generation),
                                    },
                                    reply,
                                })
                                .await;
                        }
                    }
                });
            }
            let recv_task = crate::nostr_ritual::spawn_founder_group_recv(
                chan.clone(),
                group.clone(),
                generation,
                weak,
            );
            if let Some(nostr) = self.net_ritual.as_mut().and_then(|r| r.nostr.as_mut()) {
                nostr.rotation_seed = Some(seed);
                nostr.group = Some(group);
                nostr.chan = Some(chan);
                nostr.tasks.push(recv_task);
            }
            self.session
                .create
                .run
                .log
                .push("→ the group is born · welcomes sent to every member".to_string());
        }

        /// Refuse an activation, visibly: one `✗ invite N: …` line in the
        /// founding log (a silently ignored activation is indistinguishable
        /// from "the invitee never tried", which an operator cannot debug),
        /// the session pushed, the frame acked — a refusal is never an
        /// error on the actor.
        fn refuse_join(
            &mut self,
            idx: usize,
            why: String,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            self.session
                .create
                .run
                .log
                .push(format!("✗ invite {}: {why}", idx + 1));
            self.emit_session(molt_core::SessionScope::Create);
            Ok(molt_core::Reply::Ack)
        }

        /// The single-use ticket, decided BEFORE the ladder: is this seat
        /// already anchored, and by whom?
        ///
        /// The SAME member re-announcing itself is at-least-once delivery (a
        /// redelivered JoinRequest) — silent, the seat is already theirs. A
        /// DIFFERENT identity with a valid MAC is either the same person
        /// re-activating after a transport hiccup (`cmd_join_start` mints a
        /// FRESH phrase on every start, so a retry always derives a different
        /// identity_pk — resumable only BEFORE the group is born and BEFORE
        /// the charter is proposed: the Welcome is bound to the first
        /// KeyPackage, and the collected signatures cover the identity
        /// table, review R2) or a second person on one link — that one is
        /// told on its own claimed address and logged, the anchored seat
        /// untouched. Nothing is cleared here: the write at the end of the
        /// ladder overwrites identity, key package and reply handover
        /// wholesale, so a re-activation that then FAILS a check leaves the
        /// honest anchor in place (an early clear evicted it).
        #[allow(clippy::too_many_arguments)] // one wire request's fields, not a bag
        fn spent_seat(
            &mut self,
            idx: usize,
            seat: u32,
            member: &str,
            identity_pk: &str,
            nostr_pk: &str,
            proof: &str,
            reply: &str,
        ) -> SpentSeat {
            let Some((anchored_member, anchored_pk, anchored_npk, ticket, seat_sealed)) = self
                .net_ritual
                .as_ref()
                .and_then(|r| r.seats.get(idx))
                .and_then(|s| {
                    s.identity.as_ref().map(|a| {
                        (
                            a.member.clone(),
                            a.identity_pk.clone(),
                            a.nostr_pk.clone(),
                            s.ticket.clone(),
                            s.sealed,
                        )
                    })
                })
            else {
                return SpentSeat::Open;
            };
            let same = anchored_member == member && anchored_pk == identity_pk;
            if same {
                return SpentSeat::Silent;
            }
            let mac_ok = invite::verify_join_mac(&ticket, member, identity_pk, nostr_pk, proof);
            if !mac_ok {
                // previously silent: an unverifiable re-activation looked
                // exactly like "the invitee never tried"
                self.session.create.run.log.push(format!(
                    "✗ invite {}: a second activation by {member} did not verify - ignored",
                    idx + 1
                ));
                return SpentSeat::Refused;
            }
            let group_born = self
                .net_ritual
                .as_ref()
                .and_then(|r| r.nostr.as_ref())
                .is_some_and(|n| n.group.is_some());
            let charter_proposed = self
                .net_ritual
                .as_ref()
                .is_some_and(|r| r.charter_proposed);
            let same_person = anchored_member == member;
            if same_person && !seat_sealed && !group_born && !charter_proposed {
                // STAGE, do not destroy — the displaced anchor is told only
                // once the replacement has passed every check
                return SpentSeat::ReAnchor {
                    displaced: anchored_npk,
                };
            }
            // WHY it is refused travels with the frame: "ask for your own
            // link" is right for a second PERSON and wrong for the same
            // person retrying after the group already formed (a fresh link
            // cannot help them — the founding must be re-minted)
            let why = if same_person {
                "this founding has already formed its group around your first \
                 attempt - the founder must cancel and re-mint it"
                    .to_string()
            } else {
                "that link was already used by someone else - ask the founder for \
                 your own, unused link"
                    .to_string()
            };
            // tell the second activator its link is spent — over its OWN
            // claimed transport address: the gift-wrap anchor on Nostr
            // (canonicalized; an invalid one gets no reply), the advertised
            // reply queue on loopback
            if let Some(nostr) = self.net_ritual.as_ref().and_then(|r| r.nostr.as_ref()) {
                if let Ok(target) = molt_net::canonical_nostr_pk(nostr_pk) {
                    let net = nostr.net.clone();
                    tokio::spawn(async move {
                        if let Err(e) = net
                            .send_ritual(&target, &invite::RitualMsg::LinkSpent { seat, reason: why })
                            .await
                        {
                            tracing::warn!(error = %e, "link-spent notice did not publish");
                        }
                    });
                }
            } else if let (Some((snd, wrap)), Some(ritual)) =
                (parse_reply_handover(reply), &self.net_ritual)
            {
                if let Ok(payload) =
                    serde_json::to_vec(&invite::RitualMsg::LinkSpent { seat, reason: why })
                {
                    let transport = ritual.transport.clone();
                    let id = ritual.next_msg_id(&format!("spent-{idx}-{member}"));
                    tokio::spawn(async move {
                        let _ = supervisor::send_framed(&transport, &snd, &wrap, id, &payload).await;
                    });
                }
            }
            let line = if same_person {
                format!(
                    "✗ invite {}: the group already formed around the first activation - \
                     cancel and re-mint to let {member} back in",
                    idx + 1
                )
            } else {
                format!(
                    "✗ invite {} was activated a second time (by {member}) - that \
                     link is spent, they need an unused one",
                    idx + 1
                )
            };
            self.session.create.run.log.push(line);
            SpentSeat::Refused
        }

        /// A member activated their link. Verify the ticket MAC (v2 — it
        /// binds the nostr transport anchor to the ticket holder), anchor
        /// their identity, and — once every seat's key is in — send the
        /// canonical table to all members to sign. Verification failures
        /// are logged and dropped (a bad request must not wedge anything).
        #[allow(clippy::too_many_arguments)]
        #[allow(clippy::too_many_arguments)] // one wire request's fields, not a bag
        pub(crate) fn cmd_net_join_requested(
            &mut self,
            seat: u32,
            member: MemberId,
            identity_pk: String,
            nostr_pk: String,
            proof: String,
            reply: String,
            sender_npub: String,
            key_package: String,
            relays: Vec<String>,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            // a handle is forever-bytes and one line of the run log: bound it
            // BEFORE it is logged or anchored (review R6) — the request is
            // unauthenticated at this point, so the drop is silent
            if let Err(e) = check_handle(&member) {
                tracing::warn!(seat, error = %e, "join request with an invalid handle - dropped");
                return Ok(molt_core::Reply::Ack);
            }
            // R4's founding twin (2026-08-08): a joiner that declares its
            // dialable relays lets the founder SEE a pool deviation while
            // everyone is still in the ritual — one log line naming the
            // relay, not two sides staring at a partial mesh later. Empty =
            // no declaration (loopback, older builds); display-grade only.
            if !relays.is_empty() {
                let pool = self
                    .net_ritual
                    .as_ref()
                    .map(|r| r.group_relays())
                    .unwrap_or_default();
                if let Some(line) = join_relay_deviation(&member, &pool, &relays) {
                    self.session.create.run.log.push(line);
                    self.emit_session(molt_core::SessionScope::Create);
                }
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            // the ticket is single-use: a spent seat is decided FIRST
            let displaced = match self.spent_seat(idx, seat, &member, &identity_pk, &nostr_pk, &proof, &reply) {
                SpentSeat::Open => None,
                SpentSeat::Silent => return Ok(molt_core::Reply::Ack),
                SpentSeat::Refused => {
                    self.emit_session(molt_core::SessionScope::Create);
                    return Ok(molt_core::Reply::Ack);
                }
                SpentSeat::ReAnchor { displaced } => Some(displaced),
            };
            let re_anchoring = displaced.is_some();
            let Some(ritual) = &self.net_ritual else {
                return Ok(molt_core::Reply::Ack);
            };
            let Some(s) = ritual.seats.get(idx) else {
                return Ok(molt_core::Reply::Ack);
            };
            // Every refusal below is now VISIBLE in the founding log, not
            // only in a tracing::warn nobody sees: a silently ignored
            // activation is indistinguishable from "the invitee never tried",
            // which is exactly the state an operator cannot debug.
            let is_nostr = ritual.nostr.is_some();
            self.session.create.run.log.push(format!(
                "· invite {} activated by {member} - checking",
                idx + 1
            ));
            // PROOF OF POSSESSION (Nostr only): the request arrived inside a
            // gift wrap whose seal NIP-59 verified, so `sender_npub` is a key
            // the sender demonstrably holds. Requiring it to equal the
            // claimed anchor is what upgrades the third anchor from "chosen"
            // to "possessed". Checked BEFORE the ticket can be spent.
            if is_nostr {
                let claimed = molt_net::canonical_nostr_pk(&nostr_pk).ok();
                if claimed.is_none() || claimed.as_deref() != Some(sender_npub.as_str()) {
                    tracing::warn!(seat, %member, "founding join rejected: anchor is not the wrap's proven sealer");
                    return self.refuse_join(
                        idx,
                        "the request claims a transport key it did not sign with - refused".to_string(),
                    );
                }
            }
            if !invite::verify_join_mac(&s.ticket, &member, &identity_pk, &nostr_pk, &proof) {
                tracing::warn!(seat, %member, "founding join rejected: bad ticket MAC");
                return self.refuse_join(
                    idx,
                    "the ticket code does not match - refused (wrong, edited or foreign link)"
                        .to_string(),
                );
            }
            // normalize-or-reject the wire anchor (concept §3, "normalize at
            // ingest"): the MAC only proves the TICKET HOLDER chose these
            // bytes, and the value becomes threshold-signed forever-bytes in
            // the roster/genesis/republic-id — only the one canonical form of
            // a real x-only key may be anchored. Rejecting here does NOT
            // spend the ticket (the seat's identity stays empty), so the
            // holder can re-activate with a well-formed anchor.
            let nostr_pk = match molt_net::canonical_nostr_pk(&nostr_pk) {
                Ok(canonical) => canonical,
                Err(e) => {
                    tracing::warn!(seat, %member, error = %e, "founding join rejected: invalid nostr transport anchor");
                    return self.refuse_join(
                        idx,
                        format!("malformed transport key ({e}) - refused, the ticket stays usable"),
                    );
                }
            };
            // cross-seat uniqueness: two seats sharing a transport anchor is
            // either a broken client or a correlation attack (and would make
            // the future npk→member mapping non-injective) — reject it. The
            // founder's own anchor counts too.
            // …and the HANDLE, for the same reason plus a sharper one: every
            // "is this the founder?" check on the 445 channel
            // (`frame_is_from_founder`, `check_proposal_provenance`) is a
            // string compare against a handle printed in the invite link. A
            // joiner who simply TYPES the founder's handle would otherwise be
            // welcomed into the group with a leaf credential that satisfies
            // every one of those gates — able to abort every other seat's
            // join, or propose a charter as the founder. OpenMLS does not
            // help: it enforces uniqueness of signature/encryption/init keys,
            // never of credential identities.
            let handle_taken = ritual.founder.member == member
                || ritual
                    .seats
                    .iter()
                    .enumerate()
                    .any(|(i, other)| {
                        i != idx && other.identity.as_ref().is_some_and(|x| x.member == member)
                    });
            if handle_taken {
                tracing::warn!(seat, %member, "founding join rejected: handle already taken");
                return self.refuse_join(
                    idx,
                    format!("the name {member} is already taken in this founding - refused"),
                );
            }
            let duplicate = ritual.founder.nostr_pk == nostr_pk
                || ritual.seats.iter().enumerate().any(|(i, other)| {
                    // a re-activation must not collide with the very anchor
                    // it is replacing
                    !(re_anchoring && i == idx)
                        && other.identity.as_ref().is_some_and(|x| x.nostr_pk == nostr_pk)
                });
            if duplicate {
                tracing::warn!(seat, %member, "founding join rejected: nostr transport anchor already anchored by another seat");
                return self.refuse_join(
                    idx,
                    "that transport key is already used by another seat - refused".to_string(),
                );
            }
            // the reply address: on Nostr it IS the MAC-bound nostr anchor
            // (no queue handover travels); on loopback the member advertised
            // a reply queue, without which the seat can never be sealed
            let reply_queue = if is_nostr {
                None
            } else {
                match parse_reply_handover(&reply) {
                    Some(rq) => Some(rq),
                    None => {
                        tracing::warn!(seat, %member, "founding join rejected: missing/invalid reply queue");
                        return self.refuse_join(
                            idx,
                            "no usable reply address in the request - refused".to_string(),
                        );
                    }
                }
            };
            // the member's MLS KeyPackage is required AND must be bound to the
            // anchored identity: its credential must name this member and its
            // signature key must be the MAC-bound identity key (the Ed25519
            // anchor is MLS-bound; the nostr anchor is NOT — its bindings are
            // the MAC, the canonical-form gate above, and the member's own
            // sign-what-you-see re-check). Otherwise a joiner could pass the
            // ticket MAC for one handle yet authenticate inside the group as
            // another.
            let key_package_binds = hex::decode(&key_package)
                .ok()
                .and_then(|b| molt_net::mls::key_package_binding(&b).ok())
                .is_some_and(|(id, sig)| id == member.as_bytes() && hex::encode(sig) == identity_pk);
            if !key_package_binds {
                tracing::warn!(seat, %member, "founding join rejected: MLS key package does not match the anchored identity");
                return self.refuse_join(
                    idx,
                    "the key package does not match the identity in the request - refused"
                        .to_string(),
                );
            }
            // keep a copy of the reply handover to ack the joiner below
            let ack_queue = reply_queue.clone();
            // all checks passed — re-borrow mutably and anchor all three:
            // the MAC bound the (now canonicalized) nostr transport key to
            // the ticket holder alongside the identity key
            let ack_npub = nostr_pk.clone();
            let Some(s) = self.net_ritual.as_mut().and_then(|r| r.seats.get_mut(idx)) else {
                return Ok(molt_core::Reply::Ack);
            };
            s.identity = Some(MemberIdentity {
                member: member.clone(),
                identity_pk,
                nostr_pk,
            });
            if let Some((reply_snd, reply_wrap)) = reply_queue {
                s.reply_snd = Some(reply_snd);
                s.reply_wrap = Some(reply_wrap);
            }
            s.key_package = Some(key_package);
            // reflect into the session seat + log
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.member = member.clone();
                view.state = 1;
            }
            // the replacement is COMMITTED — only now is the displaced anchor
            // told, and only now is the re-activation announced
            if let Some(old_npk) = displaced {
                if let Some(nostr) = self.net_ritual.as_ref().and_then(|r| r.nostr.as_ref()) {
                    if let Ok(target) = molt_net::canonical_nostr_pk(&old_npk) {
                        let net = nostr.net.clone();
                        tokio::spawn(async move {
                            if let Err(e) = net
                                .send_ritual(
                                    &target,
                                    &invite::RitualMsg::LinkSpent {
                                        seat,
                                        reason: "you re-activated this invite from another \
                                                 device or attempt; that newer attempt now \
                                                 holds the seat"
                                            .to_string(),
                                    },
                                )
                                .await
                            {
                                tracing::warn!(error = %e, "displaced-anchor notice did not publish");
                            }
                        });
                    }
                }
                self.session.create.run.log.push(format!(
                    "· invite {} re-activated by {member} - the earlier attempt is replaced",
                    idx + 1
                ));
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
                if let Some(nostr) = &ritual.nostr {
                    let net = nostr.net.clone();
                    tokio::spawn(async move {
                        if let Err(e) = net
                            .send_ritual(&ack_npub, &invite::RitualMsg::JoinAccepted { seat })
                            .await
                        {
                            tracing::warn!(error = %e, "join-accepted ack did not publish");
                        }
                    });
                } else if let (Some((ack_addr, ack_wrap)), Ok(payload)) = (
                    ack_queue,
                    serde_json::to_vec(&invite::RitualMsg::JoinAccepted { seat }),
                ) {
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
            // Nostr (N4a): all-joined is the GROUP BIRTH — build the MLS
            // group, mint the rotation seed, gift-wrap every Welcome, and
            // stand the 445 recv up, BEFORE any seal can be proposed (§4.2:
            // deliberation runs inside the freshly born group)
            if all_joined {
                self.nostr_group_birth();
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
            features: Vec<String>,
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
            // the feature selection: the ONE shared rule — known optional
            // keys, sorted + deduped, so every member verifies and signs the
            // identical v5 table
            let features =
                molt_core::canonical_features(&features).map_err(molt_core::MoltError::Create)?;
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
                    "the charter was already proposed - cancel the founding to change it".to_string(),
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
            ritual.features = Some(features.clone());
            ritual.charter_proposed = true;
            self.session.create.name = name;
            self.session.create.agenda = agenda;
            self.session.create.features = features;
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
            from: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            // A decline carries no signature, so on Nostr the MLS author is
            // its only authentication — a member may decline its OWN seat and
            // no other, or any invitee could abort the founding and frame a
            // peer for it. Empty `from` = the loopback path, where the seat's
            // private reply queue already authenticated the sender.
            if !from.is_empty() {
                let owner = self
                    .net_ritual
                    .as_ref()
                    .and_then(|r| r.seats.get(idx))
                    .and_then(|s| s.identity.as_ref())
                    .map(|i| i.member.clone());
                if owner.as_deref() != Some(from.as_str()) {
                    tracing::warn!(seat, %from, "decline refused: not that seat's member");
                    self.session.create.run.log.push(format!(
                        "✗ a decline for invite {} came from {from}, who does not hold \
                         that seat - ignored",
                        idx + 1
                    ));
                    self.emit_session(molt_core::SessionScope::Create);
                    return Ok(molt_core::Reply::Ack);
                }
            }
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
            // posture instead of idling on a ritual that cannot complete —
            // and tell EVERY member (2026-08-08): a co-member that already
            // ratified sits in its waiting modal, and without the abort
            // broadcast it would hang there until its timeout while the
            // founder already knows the founding is dead. abandon_ritual
            // sends the Aborted frame both pre-birth (per-seat wraps) and
            // over the born group, then tears the ritual down; outcome 2
            // already blocks maybe_finalize.
            if self.session.create.run.outcome == 0 {
                self.session.create.run.outcome = 2;
                self.session.create.run.log.push(
                    "✗ the ritual is over - this republic must be founded anew (close and re-mint)"
                        .to_string(),
                );
                self.abandon_ritual(&format!("{who} declined the charter - the founding is over"));
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
                // the pool is part of what the members RATIFY, not something
                // the founder adds afterwards — the genesis byte comparison
                // closes on exactly these bytes
                relays: ritual.group_relays(),
                name: ritual.name.clone(),
                republic_id: ritual.republic_id(&identities),
                rule_m: ritual.rule_m,
                rule_n: ritual.rule_n,
                roster: identities.iter().map(|i| i.member.clone()).collect(),
                identities: identities.clone(),
                attestations: Vec::new(),
                agenda: ritual.agenda.clone(),
                features: ritual.features.clone(),
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
            // Nostr (N4a): ONE 445 group event carries the proposal to every
            // member (they joined the group at all-joined, before this). The
            // handles are lifted out first so the ritual borrow ends before
            // the once-guard is written.
            let nostr_leg = self.net_ritual.as_ref().and_then(|r| {
                r.nostr
                    .as_ref()
                    .map(|n| (n.group.clone(), n.chan.clone(), r.generation()))
            });
            if let Some((group, chan, generation)) = nostr_leg {
                if let (Some(group), Some(chan)) = (group, chan) {
                    // ONCE per ritual: maybe_seal is reachable from two call
                    // sites (a redelivered JoinRequest re-enters it), and a
                    // second Seal would double-report AND advance the ratchet
                    // past the snapshot finalize_founding takes
                    if self.seal_published {
                        return;
                    }
                    self.seal_published = true;
                    let Some(tx) = self.cmd_tx.upgrade() else {
                        return;
                    };
                    crate::nostr_ritual::spawn_publish_frame(
                        chan,
                        crate::nostr_ritual::FramePayload::Encrypt(group, Box::new(msg)),
                        "seal",
                        crate::nostr_ritual::RetryPolicy::PRE_SEAL,
                        tx.downgrade(),
                        Some(generation),
                        String::new(),
                    );
                } else {
                    // group birth failed or has not happened — never silent
                    tracing::error!("seal proposed but the Nostr group is not born");
                }
                return;
            }
            let Some(ritual) = &self.net_ritual else {
                return;
            };
            let payload = match serde_json::to_vec(&msg) {
                Ok(p) => p,
                Err(_) => return,
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
            from: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            // defence in depth: the signature below is verified against the
            // seat's ANCHORED key (so it cannot be forged), but a signature
            // attributed to a seat its author does not hold is refused here
            // rather than silently attributed. Empty `from` = loopback.
            if !from.is_empty() {
                let owner = self
                    .net_ritual
                    .as_ref()
                    .and_then(|r| r.seats.get(idx))
                    .and_then(|s| s.identity.as_ref())
                    .map(|i| i.member.clone());
                if owner.as_deref() != Some(from.as_str()) {
                    tracing::warn!(seat, %from, "seal signature refused: not that seat's member");
                    return Ok(molt_core::Reply::Ack);
                }
            }
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
                // the TABLE must be frozen (review R2): before the charter is
                // proposed `canonical()` is the provisional one, and a
                // signature over it would mark the seat sealed — its real
                // signature over the proposed table then drops as a duplicate
                if !ritual.charter_proposed {
                    tracing::warn!(seat, "seal signature before the charter proposal - ignored");
                    return Ok(molt_core::Reply::Ack);
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

            // an attestation that outran this seal signature on the wire
            // was parked — it stands now (❻½ reorder tolerance)
            let parked = self
                .net_ritual
                .as_mut()
                .and_then(|r| r.seats.get_mut(idx))
                .and_then(|s| s.parked_backup.take());
            if let Some(sig) = parked {
                self.apply_backup_attestation(idx, &sig);
            }
            self.maybe_finalize();
            // maybe_finalize may have sealed the workspace (active id + entry
            // list change), so mirror the FULL session, not just the create
            // sub-state
            self.emit_session(molt_core::SessionScope::Full);
            Ok(molt_core::Reply::Ack)
        }

        /// A member's seed-backup attestation (`seed_backup_confirmation.md`
        /// ❻½): verified against the seat's ANCHORED key over the
        /// attestation bytes of the ratified table. Strict order — a
        /// confirmation from a seat that has not ratified is ignored.
        /// Idempotent per seat, like the seal handler.
        pub(crate) fn cmd_net_backup_confirmed(
            &mut self,
            seat: u32,
            sig: String,
            from: String,
            generation: Option<u64>,
        ) -> Result<molt_core::Reply, molt_core::MoltError> {
            if !self.ritual_generation_current(generation) {
                return Ok(molt_core::Reply::Ack);
            }
            let idx = usize::try_from(seat).unwrap_or(usize::MAX);
            // defence in depth, the seal handler's twin: refuse an
            // attestation attributed to a seat its author does not hold.
            // Empty `from` = loopback.
            if !from.is_empty() {
                let owner = self
                    .net_ritual
                    .as_ref()
                    .and_then(|r| r.seats.get(idx))
                    .and_then(|s| s.identity.as_ref())
                    .map(|i| i.member.clone());
                if owner.as_deref() != Some(from.as_str()) {
                    tracing::warn!(seat, %from, "backup confirmation refused: not that seat's member");
                    return Ok(molt_core::Reply::Ack);
                }
            }
            {
                let Some(ritual) = &mut self.net_ritual else {
                    return Ok(molt_core::Reply::Ack);
                };
                let Some(s) = ritual.seats.get_mut(idx) else {
                    return Ok(molt_core::Reply::Ack);
                };
                if s.backup_confirmed {
                    return Ok(molt_core::Reply::Ack); // idempotent
                }
                // strict ratify-then-confirm holds SEMANTICALLY, but the
                // wire does not order separate messages (loopback reorders
                // under load, relays reorder 445s) — an attestation that
                // outran its seat's seal signature PARKS and is applied
                // when the seat seals (the parked-decline idiom). A seat
                // that never ratifies never applies it.
                if !s.sealed {
                    s.parked_backup = Some(sig);
                    return Ok(molt_core::Reply::Ack);
                }
            }
            self.apply_backup_attestation(idx, &sig);
            self.emit_session(molt_core::SessionScope::Full);
            Ok(molt_core::Reply::Ack)
        }

        /// Verify + apply one SEALED seat's backup attestation against the
        /// anchored key over the ratified table's attestation bytes; on
        /// success the seat advances to state 3 and the ritual may
        /// finalize. Shared by the live ingest and the parked drain.
        fn apply_backup_attestation(&mut self, idx: usize, sig: &str) {
            let (ok, member) = {
                let Some(ritual) = &self.net_ritual else {
                    return;
                };
                let Some(identities) = ritual.full_identities() else {
                    return;
                };
                let Some(s) = ritual.seats.get(idx) else {
                    return;
                };
                if !ritual.charter_proposed || !s.sealed || s.backup_confirmed {
                    return;
                }
                let Some(who) = &s.identity else {
                    return;
                };
                let table = ritual.canonical(&identities);
                let att = molt_storage::backup_confirm_bytes(&table);
                (
                    molt_storage::identity_verify(&who.identity_pk, &att, sig),
                    who.member.clone(),
                )
            };
            if !ok {
                tracing::warn!(seat = idx, "backup confirmation refused: signature invalid");
                return;
            }
            if let Some(ritual) = &mut self.net_ritual {
                if let Some(s) = ritual.seats.get_mut(idx) {
                    s.backup_confirmed = true;
                }
            }
            if let Some(view) = self.session.create.seats.get_mut(idx) {
                view.state = 4;
            }
            self.session
                .create
                .run
                .log
                .push(format!("✓ {member} secured their key"));
            self.maybe_finalize();
        }
    }
}

#[cfg(test)]
mod tests {
    /// The recovery identity resolver (WP7, field bug 2026-08-23): the
    /// anchored pk picks the ritual convention — the fixed "member" salt for
    /// a joiner, the name-derived workspace-id salt for the FOUNDER — and a
    /// phrase deriving neither is refused locally. An empty hint keeps the
    /// legacy joiner behavior.
    #[test]
    fn seat_identity_resolves_both_ritual_conventions() {
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
        let (_, joiner_pk) = super::member_identity_from_entropy(&entropy);
        let founder_ws = molt_storage::derive_workspace_id(&entropy, "walter");
        let (_, founder_pk) = molt_storage::derive_identity_key(&entropy, &founder_ws);
        assert_ne!(joiner_pk, founder_pk, "the two ritual salts genuinely differ");

        let (_, pk) = super::seat_identity(&phrase, "walter", &joiner_pk).expect("joiner");
        assert_eq!(pk, joiner_pk);
        let (_, pk) = super::seat_identity(&phrase, "walter", &founder_pk).expect("founder");
        assert_eq!(pk, founder_pk, "the founder convention resolves against its anchor");
        let (_, pk) = super::seat_identity(&phrase, "walter", "").expect("legacy hint");
        assert_eq!(pk, joiner_pk, "no hint keeps the legacy joiner derivation");
        assert!(
            super::seat_identity(&phrase, "walter", &"ab".repeat(32)).is_err(),
            "a phrase deriving neither convention is refused locally"
        );
    }

    use super::*;
    use molt_core::{MemberIdentity, RosterAttestation, SealedRoster};

    /// The founder's pool-deviation line: silent when the joiner reaches the
    /// whole pool (or declared nothing), one factual line naming the first
    /// missing relay otherwise.
    #[test]
    fn a_joiners_partial_pool_declaration_yields_one_deviation_line() {
        let pool = vec!["wss://a.example".to_string(), "wss://b.example".to_string()];
        assert_eq!(join_relay_deviation("petra", &pool, &pool), None);
        assert_eq!(join_relay_deviation("petra", &pool, &[]), None);
        assert_eq!(join_relay_deviation("petra", &[], &pool), None);
        let line = join_relay_deviation("petra", &pool, &pool[..1])
            .expect("a missing pool relay yields the line");
        assert!(
            line.contains("petra") && line.contains("1 of 2") && line.contains("wss://b.example"),
            "the line names the member, the count and the relay: {line}"
        );
    }

    #[test]
    fn recover_command_maps_the_request_and_encodes_the_reply() {
        let r = invite::RecoverRequest {
            member: "walter".to_string(),
            identity_pk: "aa".to_string(),
            key_package: "bb".to_string(),
            ticket: "cc".to_string(),
            seat_proof: "dd".to_string(),
            new_nostr_pk: String::new(),
            relays: Vec::new(),
            consent: String::new(),
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
        } = recover_command(r, "npub-of-the-wrap-author".to_string(), 7)
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
            new_nostr_pk: String::new(),
            relays: Vec::new(),
            reply: None,
            consent: String::new(),
        };
        let Command::NetRecoverRequested { reply, .. } = recover_command(bare, String::new(), 1) else {
            panic!("expected NetRecoverRequested");
        };
        assert_eq!(reply, "");
    }

    #[test]
    fn seat_proof_binds_ticket_key_package_and_republic() {
        let (sk, pk) = molt_storage::derive_identity_key(&[7u8; 32], "ws");
        let sig = make_seat_proof(&sk, "ticket-abc", "aabbcc", "rep-id-1", "", &[]);
        // the genuine proof verifies against the anchored key
        assert!(verify_seat_proof(&pk, "ticket-abc", "aabbcc", "rep-id-1", "", &[], &sig));
        // tampering ANY of the three bound fields breaks it
        assert!(!verify_seat_proof(&pk, "other", "aabbcc", "rep-id-1", "", &[], &sig));
        assert!(!verify_seat_proof(&pk, "ticket-abc", "ffff", "rep-id-1", "", &[], &sig));
        assert!(!verify_seat_proof(&pk, "ticket-abc", "aabbcc", "rep-id-2", "", &[], &sig));
        // a different identity key (a leaked link without the phrase) can't forge it
        let (_, pk2) = molt_storage::derive_identity_key(&[8u8; 32], "ws");
        assert!(!verify_seat_proof(&pk2, "ticket-abc", "aabbcc", "rep-id-1", "", &[], &sig));
    }

    /// A real, canonical nostr anchor for the founder-seat fixtures.
    fn npk_founder() -> String {
        molt_net::nostr_identity(b"founder-entropy", "ticket-a").1
    }

    /// A real, canonical nostr anchor for the member-seat fixtures.
    fn npk_member() -> String {
        molt_net::nostr_identity(b"member-entropy", "ticket-b").1
    }

    /// A fully-signed 2-member sealed roster with real keys and the GIVEN
    /// nostr anchors — the attestations are honest signatures over exactly
    /// these identities, so a rejection isolates the anchor checks (it can
    /// never hide behind a signature failure).
    fn signed_roster_with(npk_a: &str, npk_b: &str) -> SealedRoster {
        let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
        let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
        let identities = vec![
            MemberIdentity {
                member: "founder".into(),
                identity_pk: pk_a,
                nostr_pk: npk_a.to_string(),
            },
            MemberIdentity {
                member: "member".into(),
                identity_pk: pk_b,
                nostr_pk: npk_b.to_string(),
            },
        ];
        let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
        let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "charter", &[], None);
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
            relays: Vec::new(),
            features: None,
        }
    }

    /// A fully-signed 2-member sealed roster with real keys.
    fn valid_roster() -> SealedRoster {
        signed_roster_with(&npk_founder(), &npk_member())
    }

    /// N4b step 1 — the seat proof binds the NEW transport anchor.
    ///
    /// A recovered seat's working key rides the threshold-signed `Restored`
    /// block, attested by the re-derived IDENTITY key. If the proof did not
    /// cover `new_nostr_pk`, anyone able to replay a captured proof could
    /// substitute their OWN transport key into the re-anchoring — the seat's
    /// traffic would then be addressed to them while every signature checked
    /// out. So the anchor is inside the signed bytes, and the tag is bumped
    /// (`molt-seat-proof-v2`) rather than silently re-used: an unbumped
    /// layout change breaks signatures without anyone noticing.
    #[test]
    fn a_seat_proof_binds_the_new_transport_anchor() {
        let (sk, pk) = molt_storage::derive_identity_key(&[11u8; 32], "dora");
        let npk = molt_net::nostr_identity(b"dora-entropy", "recovery-ticket").1;
        let other = molt_net::nostr_identity(b"attacker-entropy", "recovery-ticket").1;
        let (ticket, kp, rid) = ("ab".repeat(8), "cc".repeat(20), "f00d".to_string());

        let proof = make_seat_proof(&sk, &ticket, &kp, &rid, &npk, &[]);
        assert!(
            verify_seat_proof(&pk, &ticket, &kp, &rid, &npk, &[], &proof),
            "the honest proof verifies"
        );
        // the whole point: the anchor cannot be swapped after signing
        assert!(
            !verify_seat_proof(&pk, &ticket, &kp, &rid, &other, &[], &proof),
            "a proof must NOT verify against a different transport anchor"
        );
        // …and every other bound field still binds
        assert!(!verify_seat_proof(&pk, "ff".repeat(8).as_str(), &kp, &rid, &npk, &[], &proof));
        assert!(!verify_seat_proof(&pk, &ticket, "dd".repeat(20).as_str(), &rid, &npk, &[], &proof));
        assert!(!verify_seat_proof(&pk, &ticket, &kp, "beef", &npk, &[], &proof));
    }

    /// R5: the relay declaration is inside the signed seat proof — a
    /// relay-level rewrite of the declared pool must fail the proof, not
    /// silently re-route the ledger. CONDITIONALLY: an empty declaration
    /// signs the exact v2 bytes, so every pre-R5 proof keeps verifying.
    #[test]
    fn a_seat_proof_binds_the_relay_declaration() {
        let (sk, pk) = molt_storage::derive_identity_key(&[13u8; 32], "dora");
        let npk = molt_net::nostr_identity(b"dora-entropy", "t").1;
        let (ticket, kp, rid) = ("ab".repeat(8), "cc".repeat(20), "f00d".to_string());

        // an empty declaration IS the v2 preimage, byte for byte
        assert_eq!(
            seat_proof_bytes(&ticket, &kp, &rid, &npk, &[]),
            {
                let mut v2 = Vec::new();
                v2.extend_from_slice(b"molt-seat-proof-v2\0");
                for f in [ticket.as_str(), kp.as_str(), rid.as_str(), npk.as_str()] {
                    v2.extend_from_slice(
                        &u32::try_from(f.len()).expect("small").to_le_bytes(),
                    );
                    v2.extend_from_slice(f.as_bytes());
                }
                v2
            },
            "an empty declaration must not move a signed byte"
        );

        let declared = vec!["wss://relay.two.example".to_string()];
        let proof = make_seat_proof(&sk, &ticket, &kp, &rid, &npk, &declared);
        assert!(verify_seat_proof(&pk, &ticket, &kp, &rid, &npk, &declared, &proof));
        assert!(
            !verify_seat_proof(
                &pk,
                &ticket,
                &kp,
                &rid,
                &npk,
                &["wss://evil.example".to_string()],
                &proof
            ),
            "a swapped declaration must fail the proof"
        );
        assert!(
            !verify_seat_proof(&pk, &ticket, &kp, &rid, &npk, &[], &proof),
            "a STRIPPED declaration must fail it too"
        );
    }

    /// A v1 proof must not verify against a v2 seat — the tag bump is what
    /// makes that true, and nothing shipped a v1 recovery over Nostr, so
    /// there is deliberately no back-compat path.
    #[test]
    fn a_v1_seat_proof_does_not_verify_against_v2() {
        let (sk, pk) = molt_storage::derive_identity_key(&[12u8; 32], "dora");
        let npk = molt_net::nostr_identity(b"dora-entropy", "t").1;
        let (ticket, kp, rid) = ("ab".repeat(8), "cc".repeat(20), "f00d".to_string());
        // the v1 preimage, byte for byte as it shipped
        let mut v1 = Vec::new();
        v1.extend_from_slice(b"molt-seat-proof-v1\0");
        v1.extend_from_slice(ticket.as_bytes());
        v1.push(0);
        v1.extend_from_slice(kp.as_bytes());
        v1.push(0);
        v1.extend_from_slice(rid.as_bytes());
        let v1_sig = molt_storage::identity_sign(&sk, &v1);
        assert!(
            !verify_seat_proof(&pk, &ticket, &kp, &rid, &npk, &[], &v1_sig),
            "a v1 signature must be refused by the v2 verifier"
        );
    }

    #[test]
    fn verify_sealed_roster_accepts_a_valid_roster() {
        assert!(verify_sealed_roster(&valid_roster()).is_ok());
    }

    /// The member-side verifiers were weaker than `verify_genesis`: a table
    /// signed `[A, A, B]`, or one whose `rule_n` disagrees with its seats,
    /// passed and was written to disk before the strong check ran.
    #[test]
    fn verify_sealed_roster_refuses_duplicate_signers_and_a_lying_n() {
        let mut dup = valid_roster();
        assert!(dup.attestations.len() >= 2, "fixture has several signers");
        dup.attestations[1] = dup.attestations[0].clone();
        assert!(
            verify_sealed_roster(&dup).is_err(),
            "[A, A, ...] is not signed by every member"
        );
        let mut lying = valid_roster();
        lying.rule_n = lying.rule_n.saturating_add(1);
        lying.republic_id =
            molt_storage::republic_id(&lying.name, lying.rule_m, lying.rule_n, &lying.identities);
        assert!(verify_sealed_roster(&lying).is_err(), "n must equal the seat count");
        let (name, pk, npk) = {
            let seat = &lying.identities[0];
            (seat.member.clone(), seat.identity_pk.clone(), seat.nostr_pk.clone())
        };
        assert!(
            verify_seal_proposal(&lying, &name, &pk, &npk).is_err(),
            "the ratifying member refuses the same table"
        );
    }

    /// SECURITY — `SealedRoster.roster` is a CONSTITUTIONAL field that no
    /// signature covers: it is absent from `roster_canonical_bytes`
    /// (every version) and from `republic_id` (molt-republic-id-v2), yet
    /// `into_genesis` copies it off the wire into the `Founded` event, where
    /// it becomes `State::roster()` — the republic's member list.
    ///
    /// So every attestation can verify, the republic id can be the honest
    /// content-derived value, and the member list can still be a different
    /// set than the one everybody signed. Not a chain-authorization hole
    /// (`verify_chain` authorizes over `identities`, and MLS binds credentials
    /// separately) — but a sign-what-you-see hole in the one table a member
    /// reads to know who they are governed with.
    ///
    /// Closed the cheap, additive way (user decision 2026-08-01): the field
    /// must be exactly the identities' members, in order. No byte layout
    /// moves, so no `molt-roster-v4` and no recompute-site ripple.
    #[test]
    fn verify_sealed_roster_rejects_a_roster_the_identities_do_not_back() {
        // an extra name nobody signed for
        let mut s = valid_roster();
        s.roster.push("mallory".into());
        assert!(
            verify_sealed_roster(&s).is_err(),
            "a member list longer than the signed identity table must be refused"
        );

        // a substituted name — same length, same signatures, different republic
        let mut s = valid_roster();
        s.roster = vec!["founder".into(), "mallory".into()];
        assert!(
            verify_sealed_roster(&s).is_err(),
            "a seat renamed on the wire must be refused"
        );

        // reordered: the roster's order is what surfaces to the member, and
        // it must match the table that was signed
        let mut s = valid_roster();
        s.roster = vec!["member".into(), "founder".into()];
        assert!(
            verify_sealed_roster(&s).is_err(),
            "the order must match the signed identity table"
        );

        // dropped seat — a 2-of-2 republic silently presented as smaller
        let mut s = valid_roster();
        s.roster = vec!["founder".into()];
        assert!(verify_sealed_roster(&s).is_err(), "a dropped seat must be refused");
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

    /// N1 PIN — a member must never trust a sealed roster carrying a
    /// malformed, non-canonical, or duplicated third anchor on ANY seat: the
    /// value is threshold-signed forever-bytes (the roster layout, republic id v2),
    /// and honest attestations over garbage would seal the garbage. Every
    /// forged roster here is SELF-CONSISTENT (honest signatures over exactly
    /// the identities shown), so only the anchor check can reject it.
    #[test]
    fn verify_sealed_roster_rejects_malformed_or_duplicate_anchors() {
        let good = npk_member();
        // the empty legacy marker must never appear on a founding seat
        assert!(verify_sealed_roster(&signed_roster_with("", &good)).is_err());
        // 64 hex chars whose x is not on the curve
        assert!(verify_sealed_roster(&signed_roster_with(&"ff".repeat(32), &good)).is_err());
        // not hex at all
        assert!(verify_sealed_roster(&signed_roster_with(&"zz".repeat(32), &good)).is_err());
        // a REAL key in a second byte form (uppercase) — one key, one signed form
        assert!(
            verify_sealed_roster(&signed_roster_with(&npk_founder().to_uppercase(), &good))
                .is_err()
        );
        // two seats sharing one transport anchor (founder claiming the
        // member's npub — the seat nobody else verifies)
        assert!(verify_sealed_roster(&signed_roster_with(&good, &good)).is_err());
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

    #[test]
    fn verify_sealed_roster_rejects_a_tampered_feature_list() {
        // roster-v5: the feature set sits inside the signed bytes exactly
        // like the agenda — a genesis carrying features nobody ratified
        // (here: signatures over None, sealed with Some) fails every
        // attestation. Adding, dropping and swapping are all tampers.
        let mut s = valid_roster();
        s.features = Some(vec!["wallet".to_string()]);
        assert!(verify_sealed_roster(&s).is_err());
    }

    #[test]
    fn verify_sealed_roster_rejects_a_non_canonical_feature_set() {
        // one set, one byte form — an unsorted or duplicated list is refused
        // outright, before any signature math
        let mut s = valid_roster();
        s.features = Some(vec!["wallet".to_string(), "memory".to_string()]);
        assert!(verify_sealed_roster(&s)
            .is_err_and(|e| e.contains("not canonical")));
        s.features = Some(vec!["memory".to_string(), "memory".to_string()]);
        assert!(verify_sealed_roster(&s)
            .is_err_and(|e| e.contains("not canonical")));
    }

    // --- the joiner's pre-signature verification (sign-what-you-see) ---------

    #[test]
    fn verify_seal_proposal_accepts_and_recomputes_the_table() {
        let p = valid_roster(); // acts as a proposal; attestations are ignored
        let pk = &p.identities[1].identity_pk; // "member"
        let npk = &p.identities[1].nostr_pk;
        let table = verify_seal_proposal(&p, "member", pk, npk).expect("a member ratifies");
        // the returned bytes are exactly the canonical table over the charter,
        // so a signature over them ratifies precisely this name + agenda + roster
        let expect =
            molt_core::roster_canonical_bytes(&p.republic_id, p.rule_m, p.rule_n, &p.identities, &p.agenda, &[], None);
        assert_eq!(table, expect);
    }

    #[test]
    fn verify_seal_proposal_rejects_a_forged_republic_id() {
        let mut p = valid_roster();
        p.republic_id = "deadbeef".to_string();
        let pk = p.identities[1].identity_pk.clone();
        let npk = p.identities[1].nostr_pk.clone();
        assert!(verify_seal_proposal(&p, "member", &pk, &npk).is_err());
    }

    #[test]
    fn verify_seal_proposal_rejects_when_our_key_is_absent() {
        let p = valid_roster();
        let npk = p.identities[1].nostr_pk.clone();
        // right name, wrong key → not us
        assert!(verify_seal_proposal(&p, "member", &"00".repeat(32), &npk).is_err());
        // our key, but under a name not in the roster → not us
        let pk = p.identities[1].identity_pk.clone();
        assert!(verify_seal_proposal(&p, "impostor", &pk, &npk).is_err());
    }

    /// N1 PIN — sign-what-you-see extends to the THIRD anchor: a proposal
    /// that anchors our (name, identity_pk) correctly but a nostr_pk we did
    /// NOT derive must be rejected before we sign. Otherwise a malicious
    /// founder anchors an attacker-controlled transport key for us — MLS
    /// still binds Ed25519, but our future gift-wrapped material (Welcomes,
    /// recovery) would flow to the attacker: denial-of-recovery plus a
    /// shadow transport identity the relays see as us.
    #[test]
    fn verify_seal_proposal_rejects_a_split_nostr_anchor() {
        let p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let ours = p.identities[1].nostr_pk.clone();
        // the honest proposal passes the 3-anchor self-check
        assert!(verify_seal_proposal(&p, "member", &pk, &ours).is_ok());
        // same roster, but our seat carries a nostr anchor we never derived
        let mut split = p.clone();
        split.identities[1].nostr_pk = "ee".repeat(32);
        // (the founder recomputes the id over the swapped roster, as a real
        // attacker controlling the proposal would)
        split.republic_id = molt_storage::republic_id(
            &split.name,
            split.rule_m,
            split.rule_n,
            &split.identities,
        );
        assert!(
            verify_seal_proposal(&split, "member", &pk, &ours).is_err(),
            "a split nostr anchor must be rejected before signing"
        );
    }

    /// N1 PIN — the ratification self-check covers OTHER seats' anchor
    /// format too: a member whose own seat is intact must still refuse to
    /// sign a proposal that anchors a malformed or duplicated third anchor
    /// for a peer (each member can only self-check its own VALUE, so format
    /// + uniqueness are what everyone can and must verify for everyone).
    #[test]
    fn verify_seal_proposal_rejects_a_malformed_or_duplicate_foreign_anchor() {
        let with_founder_anchor = |npk: &str| {
            let mut p = valid_roster();
            p.identities[0].nostr_pk = npk.to_string();
            // the attacker controls the proposal, so it recomputes the id
            p.republic_id =
                molt_storage::republic_id(&p.name, p.rule_m, p.rule_n, &p.identities);
            p
        };
        let pk = valid_roster().identities[1].identity_pk.clone();
        let ours = npk_member();
        for bad in [
            String::new(),                  // empty legacy marker
            "ff".repeat(32),                // 64 hex chars, not on the curve
            "zz".repeat(32),                // not hex
            npk_founder().to_uppercase(),   // a second byte form of a real key
            ours.clone(),                   // duplicates OUR anchor
        ] {
            assert!(
                verify_seal_proposal(&with_founder_anchor(&bad), "member", &pk, &ours).is_err(),
                "a foreign anchor {:?}… must be rejected before we sign",
                &bad[..bad.len().min(12)]
            );
        }
    }

    #[test]
    fn verify_seal_proposal_binds_the_agenda() {
        let mut p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let npk = p.identities[1].nostr_pk.clone();
        let before = verify_seal_proposal(&p, "member", &pk, &npk).expect("ok");
        p.agenda = "a different charter".to_string();
        let after = verify_seal_proposal(&p, "member", &pk, &npk).expect("ok");
        assert_ne!(before, after, "a changed agenda changes the bytes we sign");
    }

    #[test]
    fn verify_seal_proposal_binds_the_features() {
        // sign-what-you-see covers the feature set: what the member ratifies
        // is exactly the selection it was shown — None, Some([]) and every
        // concrete set produce pairwise different bytes
        let mut p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let npk = p.identities[1].nostr_pk.clone();
        let legacy = verify_seal_proposal(&p, "member", &pk, &npk).expect("ok");
        p.features = Some(Vec::new());
        let none_picked = verify_seal_proposal(&p, "member", &pk, &npk).expect("ok");
        p.features = Some(vec!["memory".to_string(), "wallet".to_string()]);
        let picked = verify_seal_proposal(&p, "member", &pk, &npk).expect("ok");
        assert_ne!(legacy, none_picked, "Some([]) is not the legacy absence");
        assert_ne!(none_picked, picked, "the selection changes the bytes we sign");
    }

    #[test]
    fn verify_seal_proposal_rejects_a_non_canonical_feature_set() {
        // the member must never ratify a second byte encoding of one set
        let mut p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let npk = p.identities[1].nostr_pk.clone();
        p.features = Some(vec!["wallet".to_string(), "memory".to_string()]);
        assert!(verify_seal_proposal(&p, "member", &pk, &npk)
            .is_err_and(|e| e.contains("not canonical")));
    }

    /// Review 2026-08-12 (sign-what-you-see): the ratify card renders
    /// exactly the known vocabulary, so a key this build cannot render must
    /// never be signed - the member refuses instead of ratifying it
    /// sight-unseen into forever-bytes. A core key is diagnosed as what it
    /// is, not as unknown.
    #[test]
    fn verify_seal_proposal_rejects_a_feature_key_this_build_cannot_render() {
        let mut p = valid_roster();
        let pk = p.identities[1].identity_pk.clone();
        let npk = p.identities[1].nostr_pk.clone();
        p.features = Some(vec!["memory".to_string(), "zzz".to_string()]);
        assert!(verify_seal_proposal(&p, "member", &pk, &npk)
            .is_err_and(|e| e.contains("unknown feature: zzz")));
        p.features = Some(vec!["chat".to_string(), "memory".to_string()]);
        assert!(verify_seal_proposal(&p, "member", &pk, &npk)
            .is_err_and(|e| e.contains("chat is always on")));
    }

    /// A bare, unactivated seat holding `ticket`.
    fn bare_seat(ticket: &str) -> SeatRuntime {
        SeatRuntime {
            ticket: ticket.to_string(),
            reply_snd: None,
            reply_wrap: None,
            identity: None,
            key_package: None,
            sealed: false,
            backup_confirmed: false,
            parked_backup: None,
        }
    }

    /// N1 PIN — the ONE ingest choke point normalizes-or-rejects the wire
    /// anchor (concept §3 "normalize at ingest"): a ticket holder MACs
    /// whatever bytes it chooses, so the founder must parse-validate before
    /// anchoring — the value becomes threshold-signed forever-bytes. A
    /// rejected activation must NOT spend the ticket (the seat stays open
    /// for the honest re-activation), and no two seats may share a
    /// transport anchor (founder's included — a shared anchor is a bug or a
    /// correlation attack).
    #[test]
    fn cmd_net_join_requested_rejects_invalid_or_duplicate_nostr_anchors() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut st = crate::tests::plain_state();
        let (founder_sk, founder_pk) = molt_storage::derive_identity_key(&[9u8; 32], "f");
        let hub = LoopbackHub::calm();
        st.net_ritual = Some(RitualRuntime {
            transport: hub.transport(),
            name: "R".to_string(),
            agenda: String::new(),
            charter_proposed: false,
            features: None,
            rule_m: 3,
            rule_n: 3,
            founder: MemberIdentity {
                member: "founder".to_string(),
                identity_pk: founder_pk,
                nostr_pk: npk_founder(),
            },
            founder_sk,
            founder_nostr_sk: zeroize::Zeroizing::new(vec![7u8; 32]),
            seats: vec![bare_seat("t0"), bare_seat("t1")],
            generation: 0,
            _sim: Vec::new(),
            seq: std::sync::atomic::AtomicU64::new(0),
            nostr: None,
        });

        let (bob_sk, bob_pk) = molt_storage::derive_identity_key(&[3u8; 32], "bob");
        let bob_kp = hex::encode(
            molt_net::MlsMember::new(&bob_sk, "bob")
                .expect("mls")
                .key_package()
                .expect("kp"),
        );
        let (carol_sk, carol_pk) = molt_storage::derive_identity_key(&[4u8; 32], "carol");
        let carol_kp = hex::encode(
            molt_net::MlsMember::new(&carol_sk, "carol")
                .expect("mls")
                .key_package()
                .expect("kp"),
        );
        let reply = serde_json::to_string(&invite::ReplyHandover {
            server: String::new(),
            queue_id: "aa".to_string(),
            wrap: "ef".repeat(32),
        })
        .expect("handover json");
        let join = |st: &mut State, seat: u32, member: &str, pk: &str, npk: &str, ticket: &str, kp: &str| {
            st.cmd_net_join_requested(
                seat,
                member.to_string(),
                pk.to_string(),
                npk.to_string(),
                invite::join_mac(ticket, member, pk, npk),
                reply.clone(),
                // loopback ritual in this fixture: nothing is wrap-proven
                String::new(),
                kp.to_string(),
                Vec::new(),
                None,
            )
            .expect("handler never errors");
        };
        let anchored = |st: &State, seat: usize| {
            st.net_ritual
                .as_ref()
                .expect("ritual")
                .seats[seat]
                .identity
                .clone()
        };

        // every malformed wire anchor is rejected WITHOUT spending the ticket
        for bad in [
            String::new(),
            "ff".repeat(32),
            "zz".repeat(32),
            format!("{}\0{}", "bb".repeat(32), "33".repeat(16)),
            "dd".repeat(31),
        ] {
            join(&mut st, 0, "bob", &bob_pk, &bad, "t0", &bob_kp);
            assert!(
                anchored(&st, 0).is_none(),
                "a malformed anchor {:?}… must not be anchored",
                &bad[..bad.len().min(12)]
            );
        }
        // …so the honest re-activation on the SAME ticket still succeeds
        let bob_npk = molt_net::nostr_identity(b"bob-entropy", "t0").1;
        join(&mut st, 0, "bob", &bob_pk, &bob_npk, "t0", &bob_kp);
        let bob_anchor = anchored(&st, 0).expect("the honest activation anchors");
        assert_eq!(bob_anchor.nostr_pk, bob_npk);

        // a second seat presenting an ALREADY-ANCHORED anchor is rejected —
        // bob's, and the founder's own (the seat no member verifies)
        join(&mut st, 1, "carol", &carol_pk, &bob_npk, "t1", &carol_kp);
        assert!(anchored(&st, 1).is_none(), "duplicate of bob's anchor rejected");
        join(&mut st, 1, "carol", &carol_pk, &npk_founder(), "t1", &carol_kp);
        assert!(anchored(&st, 1).is_none(), "duplicate of the founder's anchor rejected");

        // an uppercase presentation of a REAL key is normalized at ingest:
        // the roster only ever carries the one canonical byte form
        let carol_npk = molt_net::nostr_identity(b"carol-entropy", "t1").1;
        join(&mut st, 1, "carol", &carol_pk, &carol_npk.to_uppercase(), "t1", &carol_kp);
        assert_eq!(
            anchored(&st, 1).expect("normalized activation anchors").nostr_pk,
            carol_npk,
            "the CANONICAL lowercase form is anchored"
        );
    }

    /// N1 PIN — sign-what-you-see closes at the GENESIS: the roster a member
    /// MATERIALIZES must be byte-identically the table it RATIFIED. The
    /// scripted founder here runs the ritual honestly through ratification
    /// (bob's `verify_seal_proposal` passes over proposal P with his true
    /// anchors), then distributes a sealed roster with bob's seat replaced
    /// by attacker keys (identity + nostr) and ALL attestations self-signed
    /// over the swapped table — a fully self-consistent forgery that passes
    /// `verify_sealed_roster` (the test asserts exactly that). Only the
    /// member's own ratified-bytes comparison can reject it; without it bob
    /// would enter a founder-controlled shadow republic whose "bob" seat he
    /// does not own.
    #[test]
    fn a_sealed_roster_differing_from_the_ratified_proposal_is_rejected() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let hub = LoopbackHub::calm();
            let transport = hub.transport();
            let invite_q = transport.create_queue().await.expect("invite queue");
            let invite_wrap = WrapKey::fresh().expect("invite wrap");
            let ticket = invite::mint_ticket().expect("ticket");
            // the founder listens BEFORE the member is spawned (queue order)
            let mut founder_rx = transport.subscribe(&invite_q.rcv).await.expect("subscribe");
            let mut reasm = molt_net::Reassembler::new();
            let mut no_cancel: Option<mpsc::Receiver<()>> = None;

            let material = InviteMaterial {
                seat: 0,
                transport: transport.clone(),
                invite_snd: invite_q.snd.clone(),
                invite_wrap: invite_wrap.clone(),
                ticket: ticket.clone(),
            };
            let phrase = molt_storage::generate_seed_phrase().expect("phrase");
            let member_task = tokio::spawn(run_ritual_member(
                material,
                "bob".to_string(),
                phrase,
                true,  // collect_genesis — the arm under test
                false, // no mesh bootstrap
                None,  // ratify=None: signs as soon as the proposal verifies
                None,
            ));

            // ❶ bob activates — his JoinRequest carries his true anchors
            let join = loop {
                if let invite::RitualMsg::Join(j) =
                    next_ritual_msg(&mut founder_rx, &mut no_cancel, &invite_wrap, &mut reasm)
                        .await
                        .expect("join request")
                {
                    break j;
                }
            };
            let reply = join.reply.clone().expect("bob advertised a reply queue");
            let reply_snd = SndQueueAddr {
                server: reply.server.clone(),
                id: molt_net::QueueId::from_bytes(hex::decode(&reply.queue_id).expect("qid")),
            };
            let reply_wrap = WrapKey::from_bytes(
                hex::decode(&reply.wrap)
                    .expect("wrap hex")
                    .try_into()
                    .expect("32-byte wrap"),
            );
            let send = |msg: invite::RitualMsg, n: u64| {
                let transport = transport.clone();
                let reply_snd = reply_snd.clone();
                let reply_wrap = reply_wrap.clone();
                async move {
                    let payload = serde_json::to_vec(&msg).expect("encode");
                    supervisor::send_framed(
                        &transport,
                        &reply_snd,
                        &reply_wrap,
                        msg_id("founder", "test", n),
                        &payload,
                    )
                    .await
                    .expect("send");
                }
            };

            // ❷ the HONEST proposal P: bob's true seat + the founder's
            let (f_sk, f_pk) = molt_storage::derive_identity_key(&[7u8; 32], "f");
            let identities = vec![
                MemberIdentity {
                    member: "founder".to_string(),
                    identity_pk: f_pk,
                    nostr_pk: npk_founder(),
                },
                MemberIdentity {
                    member: "bob".to_string(),
                    identity_pk: join.identity_pk.clone(),
                    nostr_pk: join.nostr_pk.clone(),
                },
            ];
            let rid = molt_storage::republic_id("R", 2, 2, &identities);
            let proposal = SealedRoster {
                name: "R".to_string(),
                republic_id: rid,
                rule_m: 2,
                rule_n: 2,
                roster: vec!["founder".to_string(), "bob".to_string()],
                identities: identities.clone(),
                attestations: Vec::new(),
                agenda: "the ratified charter".to_string(),
                relays: Vec::new(),
                features: None,
            };
            send(invite::RitualMsg::JoinAccepted { seat: 0 }, 1).await;
            send(
                invite::RitualMsg::Seal {
                    proposal: serde_json::to_string(&proposal).expect("proposal json"),
                },
                2,
            )
            .await;

            // ❸ bob ratifies P (his 3-anchor self-check passes) and signs
            loop {
                if let invite::RitualMsg::Signed(_) =
                    next_ritual_msg(&mut founder_rx, &mut no_cancel, &invite_wrap, &mut reasm)
                        .await
                        .expect("seal signature")
                {
                    break;
                }
            }

            // ❹ the SWAP — a CHARTER swap, chosen deliberately to isolate the
            // byte comparison.
            //
            // This test used to swap bob's identity_pk for an attacker key.
            // That is caught one gate EARLIER, by verify_seal_proposal's
            // "does not anchor our own (name, key)" self-check, so the test
            // stayed green with the byte comparison deleted — it was pinning
            // verify_seal_proposal, not the thing it claims to pin.
            //
            // A different AGENDA passes every check verify_seal_proposal
            // makes: the republic id does not commit to the agenda, bob's
            // three anchors are untouched, and the roster still matches the
            // identities. The ONLY thing that can catch it is that the
            // distributed table's bytes differ from the ones bob ratified —
            // which is exactly the tamper-evident-charter property.
            let evil_sk = f_sk.clone();
            let evil_identities = identities.clone();
            let evil_rid = molt_storage::republic_id("R", 2, 2, &evil_identities);
            let table = molt_core::roster_canonical_bytes(
                &evil_rid,
                2,
                2,
                &evil_identities,
                "a charter nobody ratified",
                &[],
                None,
            );
            let sealed = SealedRoster {
                relays: Vec::new(),
                name: "R".to_string(),
                republic_id: evil_rid,
                rule_m: 2,
                rule_n: 2,
                roster: vec!["founder".to_string(), "bob".to_string()],
                identities: evil_identities,
                attestations: vec![
                    RosterAttestation {
                        member: "founder".to_string(),
                        sig: molt_storage::identity_sign(&f_sk, &table),
                    },
                    RosterAttestation {
                        member: "bob".to_string(),
                        sig: molt_storage::identity_sign(&evil_sk, &table),
                    },
                ],
                agenda: "a charter nobody ratified".to_string(),
                features: None,
            };
            // The forgery must clear every OTHER gate on this path, or the
            // test pins the wrong one — which is exactly what it did before:
            // the old evil-identity swap tripped verify_seal_proposal's
            // "does not anchor our own (name, key)" check first, so the byte
            // comparison could be deleted and this test stayed green.
            //
            // A charter swap clears it: the republic id does not commit to
            // the agenda, bob's three anchors are untouched, and the roster
            // still matches the identities.
            //
            // (`verify_sealed_roster` is deliberately NOT asserted here.
            // `run_ritual_member` never calls it — the actor does, later, as
            // defence in depth — and a founder cannot forge bob's own
            // attestation over the swapped table anyway. On THIS path the
            // ratified-bytes comparison is the only gate that can fire.)
            assert!(
                verify_seal_proposal(&sealed, "bob", &join.identity_pk, &join.nostr_pk).is_ok(),
                "the forgery must clear verify_seal_proposal for the byte pin to be the gate"
            );
            send(
                invite::RitualMsg::Genesis {
                    sealed: serde_json::to_string(&sealed).expect("sealed json"),
                    welcome: String::new(),
                },
                3,
            )
            .await;

            // ❺ the member must reject the join
            let outcome = member_task.await.expect("member task");
            let Err(err) = outcome else {
                panic!("a sealed roster differing from the ratified table must fail the join");
            };
            assert!(
                err.contains("not the table we ratified"),
                "…and specifically at the ratified-bytes comparison, not an earlier \
                 gate that happens to also refuse: {err}"
            );
        });
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
            handover: molt_net::invite::InviteHandoverV2 {
                seat: 0,
                ticket: "ab".repeat(32),
                npub: molt_net::nostr_identity(b"walter-entropy", "self-ticket").1,
                relays: vec!["wss://relay.example".to_string()],
            },
        }
    }

    #[test]
    fn founding_invite_round_trips() {
        let link = sample_invite().render().expect("renders");
        let back = FoundingInvite::parse(&link).expect("parses");
        assert_eq!(back.handover, sample_invite().handover);
        assert_eq!(back.info, sample_invite().info);
    }

    /// The link is NEUTRAL (2026-08-08): one opaque hex segment. Republic,
    /// rule and inviter ride inside it — the URL itself names nothing.
    #[test]
    fn a_founding_invite_link_is_neutral() {
        let link = sample_invite().render().expect("renders");
        let seg = link.strip_prefix("molt://invite/").expect("invite scheme");
        assert!(!seg.contains('/'), "one opaque segment: {link}");
        assert!(
            seg.bytes().all(|b| b.is_ascii_hexdigit()),
            "nothing but hex after the scheme: {link}"
        );
    }

    /// Pre-neutral links (`molt://invite/<republic>/<m>of<n>/<inviter>/
    /// <ticket>/<blob>`) still parse — the preview from the path.
    #[test]
    fn an_old_path_shaped_invite_link_still_parses() {
        let inv = sample_invite();
        let old = format!(
            "{}/{}",
            inv.info.render(),
            inv.handover.encode().expect("encodes")
        );
        let back = FoundingInvite::parse(&old).expect("old link parses");
        assert_eq!(back.info, inv.info);
        assert_eq!(back.handover, inv.handover);
    }

    /// The parse is fail-closed AND honest: a bare preview link, a
    /// non-handover trailing segment, and a pre-N4 queue-shaped handover
    /// each refuse with a message that says what the link is missing.
    #[test]
    fn founding_invite_parse_rejects_malformed_handovers() {
        let preview = sample_invite().info.render();
        // no handover segment at all (a bare preview link)
        let err = FoundingInvite::parse(&preview).expect_err("preview is not joinable");
        assert!(err.contains("no transport details"), "honest preview error: {err}");
        // trailing segment not valid hex
        assert!(FoundingInvite::parse(&format!("{preview}/zzzz")).is_err());
        // a pre-N4 queue-shaped handover — honest "older build" rejection
        let v1 = hex::encode("smp://x@h\ncd\nef\n0");
        let err =
            FoundingInvite::parse(&format!("{preview}/{v1}")).expect_err("v1 must refuse");
        assert!(err.contains("older build"), "honest v1 error: {err}");
    }
}
