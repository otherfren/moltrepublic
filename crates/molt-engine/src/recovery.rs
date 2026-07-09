// SPDX-License-Identifier: GPL-3.0-or-later

//! The **recovery ritual** transport (see `documents/recovery_ritual.md`): the
//! total-loss twin of the founding ritual. The coordinator/crypto half already
//! lives elsewhere (`Command::NetRecoverRequested`, `cmd_net_recover_requested`,
//! `verify_and_propose_restore`, `coordinator_rekey`); this module builds the
//! transport twin of the founding invite machinery — the recovery link, the
//! `RitualMsg::Recover` wire request, the coordinator recv loop, and the
//! rejoiner activation — mirroring `founding.rs`.
//!
//! Built stepwise, test-first. Today: the recovery **link** type.

use crate::founding::RitualTransport;
use crate::Envelope;
use molt_core::Command;
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::{invite, msg_id, supervisor, QueueId, SndQueueAddr, Transport, WrapKey};
use tokio::sync::mpsc;

/// A recovery link — `molt://recover/<republic>/<member>/<ticket>/<handover>` —
/// mirroring [`crate::FoundingInvite`], but for an *existing* seat. It carries a
/// transport handover (the coordinator's recovery queue) and a single-use ticket
/// the seat proof binds. The `<handover>` segment is
/// `hex(server ‖ '\n' ‖ queue_id ‖ '\n' ‖ wrap)` so the smp URL's `//@=` cannot
/// leak into the path. A link without a handover parses as a preview only and is
/// not actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryInvite {
    /// The republic's display name (spaces travel as dashes).
    pub republic: String,
    /// The returning member's seat handle.
    pub member: String,
    /// The single-use recovery ticket (lowercase hex).
    pub ticket: String,
    /// The coordinator's recovery-queue server (`smp://fingerprint@host`).
    pub server: String,
    /// The coordinator's recovery-queue send-side id (lowercase hex).
    pub queue_id: String,
    /// The per-queue wrap key (lowercase hex, 32 bytes).
    pub wrap: String,
    /// The republic's content-derived id. A total-loss rejoiner cannot derive
    /// it (it has no roster), yet the seat proof must bind it — so the
    /// coordinator carries it in the link. The rejoiner re-verifies the real id
    /// from the genesis once it catches up, and the coordinator checks the proof
    /// against its OWN id, so a doctored link's id simply fails to verify.
    pub republic_id: String,
}

impl RecoveryInvite {
    /// Render the link (preview + hex transport handover).
    pub fn render(&self) -> String {
        let handover = format!(
            "{}\n{}\n{}\n{}",
            self.server, self.queue_id, self.wrap, self.republic_id
        );
        format!(
            "molt://recover/{}/{}/{}/{}",
            self.republic.replace(' ', "-"),
            self.member,
            self.ticket,
            hex::encode(handover),
        )
    }

    /// Parse a `molt://recover/…` link; `None` if it is not a well-formed,
    /// actionable recovery link (a missing/damaged handover is rejected).
    pub fn parse(link: &str) -> Option<RecoveryInvite> {
        let rest = link.trim().strip_prefix("molt://recover/")?;
        let mut parts = rest.split('/');
        let republic = parts.next()?.replace('-', " ");
        let member = parts.next()?.to_string();
        let ticket = parts.next()?.to_string();
        let handover_hex = parts.next()?;
        if parts.next().is_some() {
            return None; // trailing junk
        }
        if republic.trim().is_empty() || member.is_empty() || ticket.len() < 4 {
            return None;
        }
        let text = String::from_utf8(hex::decode(handover_hex).ok()?).ok()?;
        let mut fields = text.split('\n');
        let server = fields.next()?.to_string();
        let queue_id = fields.next()?.to_string();
        let wrap = fields.next()?.to_string();
        let republic_id = fields.next()?.to_string();
        if fields.next().is_some()
            || server.is_empty()
            || queue_id.is_empty()
            || wrap.is_empty()
            || republic_id.is_empty()
        {
            return None;
        }
        Some(RecoveryInvite {
            republic,
            member,
            ticket,
            server,
            queue_id,
            wrap,
            republic_id,
        })
    }
}

/// Reconstruct the sealed founding roster from the genesis chain block — what a
/// rejoiner does after catching up: block 0's `Genesis` change carries the whole
/// constitution and its `sigs` are the founding attestations, so the rejoiner
/// materializes its local workspace from the verified chain (no live founder).
/// `None` if the block is not a genesis.
pub(crate) fn sealed_roster_from_genesis(
    block: &molt_core::ChainBlock,
) -> Option<molt_core::SealedRoster> {
    let molt_core::ChainChange::Genesis {
        name,
        republic_id,
        rule_m,
        rule_n,
        identities,
        agenda,
    } = &block.change
    else {
        return None;
    };
    Some(molt_core::SealedRoster {
        name: name.clone(),
        republic_id: republic_id.clone(),
        rule_m: *rule_m,
        rule_n: *rule_n,
        roster: identities.iter().map(|i| i.member.clone()).collect(),
        identities: identities.clone(),
        attestations: block.sigs.clone(),
        agenda: agenda.clone(),
    })
}

/// One minted recovery link's transport handover — the recovery twin of
/// [`crate::founding::InviteMaterial`]. A real mint reports the rendered link to
/// the operator; the two-instance recovery dev test reads this off the recovery
/// material sink so a *separate* engine can drive the returning-member side
/// against the coordinator's freshly-minted queue.
#[doc(hidden)]
#[derive(Clone)]
pub struct RecoveryMaterial<T: Transport = RitualTransport> {
    /// The returning member the link re-admits.
    pub member: String,
    /// The transport the coordinator minted the recovery queue on (a clone that
    /// shares its `Arc` — a genuinely separate node uses its own transport and
    /// only reads the address / wrap / ticket below).
    pub transport: T,
    /// returning member → coordinator queue (the `RitualMsg::Recover` request).
    pub recover_snd: SndQueueAddr,
    /// The per-queue wrap key.
    pub recover_wrap: WrapKey,
    /// The single-use recovery ticket.
    pub ticket: String,
    /// The republic's content-derived id (carried in the link; the seat proof
    /// binds it).
    pub republic_id: String,
    /// The fully-rendered `molt://recover/…` link.
    pub link: String,
}

/// Provision the coordinator's dedicated **recovery queue** off the actor —
/// `create_queue` is a live round-trip the synchronous command handler must not
/// block on (mirrors [`crate::founding::spawn_smp_provisioning`]). Once the
/// queue is up, wire the coordinator recv loop, render the recovery link, report
/// it to the operator ([`Command::NetRecoverLinkReady`]) and hand the transport
/// handover to the dev-seam sink if one is installed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_recovery_provisioning(
    transport: RitualTransport,
    member: String,
    republic: String,
    republic_id: String,
    ticket: String,
    wrap: WrapKey,
    generation: u64,
    cmd_tx: mpsc::Sender<Envelope>,
    sink: Option<std::sync::mpsc::Sender<RecoveryMaterial>>,
) {
    tokio::spawn(async move {
        let q = match transport.create_queue().await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(%member, error = %e, "recovery-queue provisioning failed");
                return;
            }
        };
        // listen for the returning member's request on the fresh queue: a
        // `RitualMsg::Recover` becomes `Command::NetRecoverRequested`
        crate::founding::spawn_coordinator_recv(
            transport.clone(),
            q.rcv.clone(),
            wrap.clone(),
            generation,
            cmd_tx.clone(),
        );
        let link = RecoveryInvite {
            republic,
            member: member.clone(),
            ticket: ticket.clone(),
            server: q.snd.server.clone(),
            queue_id: hex::encode(&q.snd.id.0),
            wrap: hex::encode(wrap.to_bytes()),
            republic_id: republic_id.clone(),
        }
        .render();
        // report the shareable link to the operator (GUI/MCP read it back)
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let _ = cmd_tx
            .send(Envelope {
                cmd: Command::NetRecoverLinkReady {
                    member: member.clone(),
                    link: link.clone(),
                    generation: Some(generation),
                },
                reply,
            })
            .await;
        // dev seam: hand the handover to a waiting second engine (test only)
        if let Some(sink) = sink {
            let _ = sink.send(RecoveryMaterial {
                member,
                transport,
                recover_snd: q.snd,
                recover_wrap: wrap,
                ticket,
                republic_id,
                link,
            });
        }
    });
}

/// Send the MLS **Welcome** to a returning member's reply queue off the actor —
/// the coordinator's half of `recovery_ritual.md` §4 ❺–❽: once the `Restored`
/// block commits and `restore_member` produced `(commit, welcome)`, the commit
/// is broadcast to the survivors (a recorded `MlsCommit`) and this delivers the
/// welcome that brings the rejoiner back into the group — bundled with the full
/// `chain` (JSON of `Vec<ChainBlock>`) so the rejoiner catches its state up over
/// this same channel (option A). `reply_json` is the opaque `ReplyHandover` the
/// rejoiner advertised in its `RecoverRequest`.
pub(crate) fn spawn_welcome_send(
    transport: RitualTransport,
    reply_json: String,
    welcome: Vec<u8>,
    chain_json: String,
) {
    tokio::spawn(async move {
        let Ok(handover) = serde_json::from_str::<invite::ReplyHandover>(&reply_json) else {
            tracing::warn!("recovery reply handover is not valid JSON — cannot send the welcome");
            return;
        };
        let (Ok(qid), Ok(wrap_bytes)) =
            (hex::decode(&handover.queue_id), hex::decode(&handover.wrap))
        else {
            tracing::warn!("recovery reply handover has malformed hex — cannot send the welcome");
            return;
        };
        let Ok(wrap_arr): Result<[u8; 32], _> = wrap_bytes.try_into() else {
            tracing::warn!("recovery reply wrap key is not 32 bytes");
            return;
        };
        let snd = SndQueueAddr {
            server: handover.server,
            id: QueueId::from_bytes(qid),
        };
        let wrap = WrapKey::from_bytes(wrap_arr);
        let msg = invite::RitualMsg::Welcome {
            welcome: hex::encode(&welcome),
            chain: chain_json,
        };
        let Ok(payload) = serde_json::to_vec(&msg) else {
            return;
        };
        if let Err(e) = supervisor::send_framed(
            &transport,
            &snd,
            &wrap,
            msg_id("coordinator", "rejoiner", 1),
            &payload,
        )
        .await
        {
            tracing::warn!(error = %e, "sending the recovery welcome failed");
        }
    });
}

/// The rejoiner's finished state after the recovery ritual's re-key: it is back
/// inside the encrypted group, holding its re-derived identity, the fresh group
/// snapshot, and the **verified** persistent chain caught up over the recovery
/// channel. What remains (option A's follow-on) is for the engine to materialize
/// the local workspace from `sealed` + `chain` (`recovery_ritual.md` §4 ❼–❽).
#[derive(Debug, Clone)]
pub struct RejoinOutcome {
    /// The recovered seat's member handle (from the link).
    pub member: String,
    /// The re-derived identity pk — equals the anchored roster key (the
    /// coordinator verified the seat proof against it), hex.
    pub pk: String,
    /// The republic's content-derived id — re-verified against the genesis
    /// (not just the link), hex.
    pub republic_id: String,
    /// The MLS group snapshot after processing the Welcome — the rejoiner can
    /// decrypt live group traffic again.
    pub mls_snapshot: Vec<u8>,
    /// The full persistent chain, **verified from block 0** (signatures, links,
    /// threshold). Empty on a chain-less republic.
    pub chain: Vec<molt_core::ChainBlock>,
    /// The founding roster reconstructed from the genesis block — what the
    /// engine materializes the local workspace from. `None` on a chain-less
    /// republic (no genesis to rebuild it).
    pub sealed: Option<molt_core::SealedRoster>,
}

/// Drive the **returning member's side** of the recovery ritual over `transport`
/// (`recovery_ritual.md` §4 ❶,❷,❻), the twin of [`crate::run_ritual_member`]:
///
/// 1. re-derive the seat's identity from the phrase (same `pk` as always) and
///    build a *fresh* MLS `KeyPackage` (a new leaf, same credential handle);
/// 2. open a reply queue and send a `RecoverRequest` — the seat proof binds the
///    link's single-use ticket, the fresh KeyPackage, and the republic id — to
///    the coordinator's recovery queue;
/// 3. await the coordinator's `Welcome` on the reply queue and rejoin the group.
///
/// The generic core a genuinely separate node runs; [`rejoin_over_smp`] wraps it
/// with the transport parsed from a `molt://recover/…` link. It does **not**
/// catch up or materialize — that is the caller's next step, over the mesh.
pub async fn run_rejoin<T: Transport>(
    transport: T,
    inv: RecoveryInvite,
    phrase: &str,
) -> Result<RejoinOutcome, String> {
    // per-seat identity, deterministic from the phrase — the SAME key the
    // genesis roster anchors, so the coordinator's seat-proof check passes and
    // the re-keyed leaf keeps the seat's identity (recovery re-keys the leaf,
    // never the roster identity).
    let (sk, pk) = crate::founding::member_identity(phrase)?;
    // a fresh MLS member from that identity; its credential is the seat handle,
    // so `restore_member` finds and re-keys THIS leaf.
    let mut mls = molt_net::MlsMember::new(&sk, &inv.member).map_err(|e| e.to_string())?;
    let key_package = mls.key_package().map_err(|e| e.to_string())?;
    let kp_hex = hex::encode(&key_package);

    // the reply queue we receive the Welcome on — subscribe before advertising
    // it, so the coordinator's Welcome cannot race ahead of our subscription
    let reply_q = transport.create_queue().await.map_err(|e| e.to_string())?;
    let reply_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
    let mut rx = transport.subscribe(&reply_q.rcv).await.map_err(|e| e.to_string())?;

    // the coordinator's recovery queue + wrap, from the link
    let queue_id = hex::decode(&inv.queue_id).map_err(|e| e.to_string())?;
    let recover_snd = SndQueueAddr {
        server: inv.server.clone(),
        id: QueueId::from_bytes(queue_id),
    };
    let recover_wrap_bytes: [u8; 32] = hex::decode(&inv.wrap)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "bad recovery wrap key length".to_string())?;
    let recover_wrap = WrapKey::from_bytes(recover_wrap_bytes);

    // the seat proof: only the phrase-holder can sign it, and it binds this exact
    // fresh KeyPackage + the republic id carried in the link
    let seat_proof = crate::founding::make_seat_proof(&sk, &inv.ticket, &kp_hex, &inv.republic_id);
    let request = invite::RitualMsg::Recover(invite::RecoverRequest {
        member: inv.member.clone(),
        identity_pk: pk.clone(),
        key_package: kp_hex,
        ticket: inv.ticket.clone(),
        seat_proof,
        reply: Some(invite::ReplyHandover {
            server: reply_q.snd.server.clone(),
            queue_id: hex::encode(&reply_q.snd.id.0),
            wrap: hex::encode(reply_wrap.to_bytes()),
        }),
    });
    let payload = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    supervisor::send_framed(
        &transport,
        &recover_snd,
        &recover_wrap,
        msg_id(&inv.member, "coordinator", 1),
        &payload,
    )
    .await
    .map_err(|e| e.to_string())?;

    // await the coordinator's Welcome, then rejoin the group. Anything else on
    // the reply queue is ignored (only the Welcome finishes the rejoin).
    let mut reasm = molt_net::Reassembler::new();
    loop {
        let Some(delivery) = rx.recv().await else {
            return Err("recovery reply queue closed before the welcome arrived".to_string());
        };
        let Ok(plain) = molt_net::wrap::unwrap_block(&reply_wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
            continue;
        };
        if let Ok(invite::RitualMsg::Welcome { welcome, chain }) =
            serde_json::from_slice::<invite::RitualMsg>(&bytes)
        {
            // ❻ rejoin the encrypted group
            let welcome_bytes = hex::decode(&welcome).map_err(|e| e.to_string())?;
            mls.join_from_welcome(&welcome_bytes).map_err(|e| e.to_string())?;
            let snap = mls.snapshot().map_err(|e| e.to_string())?;
            // ❼–❽ catch up: VERIFY the served chain from block 0 (an untrusted
            // deliverer is safe — the signatures + links + threshold are checked
            // here, not trusted), then reconstruct the founding roster from the
            // genesis for the engine to materialize from.
            let (verified_chain, sealed) = verify_served_chain(&chain, &inv, &pk)?;
            return Ok(RejoinOutcome {
                member: inv.member,
                pk,
                republic_id: inv.republic_id,
                mls_snapshot: snap,
                chain: verified_chain,
                sealed,
            });
        }
    }
}

/// Verify a coordinator-served chain from block 0 and reconstruct the founding
/// roster from its genesis — the rejoiner's catch-up check (`recovery_ritual.md`
/// §4 ❽, §5.2). Hard-rejects a chain whose signatures/links/threshold do not
/// verify, whose genesis id differs from the seat-proof-bound `republic_id` (so
/// a coordinator cannot swap in a different republic's chain), or that does not
/// anchor the rejoiner's own `(member, pk)`. An empty chain (a chain-less/demo
/// republic) verifies trivially to no roster.
fn verify_served_chain(
    chain_json: &str,
    inv: &RecoveryInvite,
    pk: &str,
) -> Result<(Vec<molt_core::ChainBlock>, Option<molt_core::SealedRoster>), String> {
    if chain_json.is_empty() {
        return Ok((Vec::new(), None));
    }
    let blocks: Vec<molt_core::ChainBlock> =
        serde_json::from_str(chain_json).map_err(|e| format!("decoding the served chain: {e}"))?;
    // the WHOLE chain, verified from block 0 (signatures, prev-links, threshold)
    crate::chain::verify_chain(&blocks)?;
    let genesis = blocks.first().ok_or("the served chain is empty")?;
    let sealed = sealed_roster_from_genesis(genesis)
        .ok_or("the served chain does not root on a genesis block")?;
    if sealed.republic_id != inv.republic_id {
        return Err("the served chain's republic id does not match the recovery link".to_string());
    }
    if !sealed
        .identities
        .iter()
        .any(|i| i.member == inv.member && i.identity_pk == pk)
    {
        return Err("the served chain's roster does not anchor our own (name, key)".to_string());
    }
    Ok((blocks, Some(sealed)))
}

/// Rejoin a republic from a `molt://recover/…` link over the real SMP server the
/// link points at — the total-loss member's entry point (a fresh device with
/// only the recovery phrase). Builds this node's OWN transport to the
/// coordinator's server (SMP clones share the recipient-key store, so the caller
/// can reuse it for the follow-on catch-up mesh) and drives [`run_rejoin`].
pub async fn rejoin_over_smp(link: &str, phrase: &str) -> Result<RejoinOutcome, String> {
    let inv = RecoveryInvite::parse(link).ok_or("not an actionable recovery link")?;
    let server = SmpServer::parse(inv.server.trim()).map_err(|e| e.to_string())?;
    let transport = RitualTransport::Smp(SmpTransport::new(server));
    run_rejoin(transport, inv, phrase).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{
        ChainBlock, ChainChange, MemberIdentity, RosterAttestation, GENESIS_PREV,
    };

    fn sample() -> RecoveryInvite {
        RecoveryInvite {
            republic: "Chess Club".to_string(),
            member: "walter".to_string(),
            ticket: "k9x2m4q7aa".to_string(),
            server: "smp://fingerprint@host".to_string(),
            queue_id: "deadbeef".to_string(),
            wrap: "00112233".to_string(),
            republic_id: "f00dbabe".to_string(),
        }
    }

    #[test]
    fn a_recovery_link_round_trips() {
        let inv = sample();
        let link = inv.render();
        assert!(link.starts_with("molt://recover/"), "the scheme names recovery");
        assert!(
            link.contains("Chess-Club"),
            "spaces in the republic travel as dashes"
        );
        assert_eq!(RecoveryInvite::parse(&link).as_ref(), Some(&inv));
    }

    /// The coordinator's welcome-send half: given the rejoiner's advertised reply
    /// handover, `spawn_welcome_send` delivers the MLS Welcome to that queue as a
    /// `RitualMsg::Welcome`, intact.
    #[tokio::test]
    async fn the_welcome_reaches_the_rejoiners_reply_queue() {
        use molt_net::LoopbackHub;

        let hub = LoopbackHub::calm();
        let transport = RitualTransport::Loopback(hub.transport());
        let reply_q = transport.create_queue().await.expect("reply queue");
        let reply_wrap = WrapKey::fresh().expect("wrap");
        let handover = invite::ReplyHandover {
            server: "loopback".to_string(),
            queue_id: hex::encode(&reply_q.snd.id.0),
            wrap: hex::encode(reply_wrap.to_bytes()),
        };
        let handover_json = serde_json::to_string(&handover).expect("handover json");
        let welcome = vec![0xADu8, 0xBE, 0xEF, 0x01];

        spawn_welcome_send(transport.clone(), handover_json, welcome.clone(), String::new());

        // the rejoiner receives the Welcome on its reply queue, intact
        let mut rx = transport.subscribe(&reply_q.rcv).await.expect("subscribe");
        let mut reasm = molt_net::Reassembler::new();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let delivery = rx.recv().await.expect("reply queue open");
                let Ok(plain) = molt_net::wrap::unwrap_block(&reply_wrap, &delivery.block) else {
                    delivery.ack.ack();
                    continue;
                };
                let outcome = reasm.push(&plain);
                delivery.ack.ack();
                if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome {
                    if let Ok(invite::RitualMsg::Welcome { welcome, .. }) =
                        serde_json::from_slice::<invite::RitualMsg>(&bytes)
                    {
                        break welcome;
                    }
                }
            }
        })
        .await
        .expect("the welcome arrives in time");
        assert_eq!(got, hex::encode(&welcome));
    }

    #[test]
    fn a_link_without_a_handover_is_not_actionable() {
        // preview only — no hex handover segment
        assert!(RecoveryInvite::parse("molt://recover/Chess-Club/walter/k9x2m4q7aa").is_none());
    }

    #[test]
    fn a_malformed_link_is_rejected() {
        assert!(RecoveryInvite::parse("molt://invite/Chess-Club/2of3/walter/tick").is_none());
        assert!(RecoveryInvite::parse("not a link").is_none());
    }

    #[test]
    fn a_sealed_roster_reconstructs_from_the_genesis_block() {
        let ids = vec![
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: "aa".to_string(),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: "bb".to_string(),
            },
        ];
        let genesis = ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Genesis {
                name: "Chess Club".to_string(),
                republic_id: "f00".to_string(),
                rule_m: 2,
                rule_n: 2,
                identities: ids.clone(),
                agenda: "play chess".to_string(),
            },
            sigs: vec![
                RosterAttestation {
                    member: "petra".to_string(),
                    sig: "11".to_string(),
                },
                RosterAttestation {
                    member: "walter".to_string(),
                    sig: "22".to_string(),
                },
            ],
        };
        let sealed = sealed_roster_from_genesis(&genesis).expect("genesis reconstructs");
        assert_eq!(sealed.name, "Chess Club");
        assert_eq!(sealed.republic_id, "f00");
        assert_eq!((sealed.rule_m, sealed.rule_n), (2, 2));
        assert_eq!(sealed.roster, vec!["petra", "walter"]);
        assert_eq!(sealed.identities, ids);
        assert_eq!(sealed.attestations.len(), 2, "the block's sigs ARE the attestations");
        assert_eq!(sealed.agenda, "play chess");

        // a non-genesis block has no roster to reconstruct
        let applied = ChainBlock {
            height: 1,
            prev: "ab".to_string(),
            change: ChainChange::Applied {
                proposal_id: 1,
                surface: molt_core::Surface::Memory,
                payload: serde_json::json!({}),
            },
            sigs: Vec::new(),
        };
        assert!(sealed_roster_from_genesis(&applied).is_none());
    }

    /// A real, n-of-n-signed single-block genesis chain for `members`
    /// (name, signing key, pk) and its content-derived republic id.
    fn signed_genesis(
        members: &[(&str, &molt_storage::SigningKey, &str)],
        rule_m: u8,
    ) -> (Vec<ChainBlock>, String) {
        let identities: Vec<MemberIdentity> = members
            .iter()
            .map(|(m, _, pk)| MemberIdentity {
                member: (*m).to_string(),
                identity_pk: (*pk).to_string(),
            })
            .collect();
        let rule_n = u8::try_from(members.len()).expect("small roster");
        let republic_id = molt_storage::republic_id("Guild", rule_m, rule_n, &identities);
        let change = ChainChange::Genesis {
            name: "Guild".to_string(),
            republic_id: republic_id.clone(),
            rule_m,
            rule_n,
            identities,
            agenda: "found it".to_string(),
        };
        let bytes = molt_core::approval_bytes(&republic_id, 0, &change);
        let sigs = members
            .iter()
            .map(|(m, sk, _)| RosterAttestation {
                member: (*m).to_string(),
                sig: molt_storage::identity_sign(sk, &bytes),
            })
            .collect();
        let genesis = ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change,
            sigs,
        };
        (vec![genesis], republic_id)
    }

    fn recovery_inv(member: &str, republic_id: &str) -> RecoveryInvite {
        RecoveryInvite {
            republic: "Guild".to_string(),
            member: member.to_string(),
            ticket: "0011223344".to_string(),
            server: "loopback".to_string(),
            queue_id: "aa".to_string(),
            wrap: "bb".to_string(),
            republic_id: republic_id.to_string(),
        }
    }

    /// The rejoiner's catch-up check: a served chain is verified from block 0,
    /// its genesis id must match the seat-proof-bound link id, and it must anchor
    /// the rejoiner's own key — every other case is hard-rejected.
    #[test]
    fn a_served_chain_verifies_from_genesis_and_hard_rejects_tampering() {
        let (coord_sk, coord_pk) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, bob_pk) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let (chain, republic_id) = signed_genesis(
            &[("coordinator", &coord_sk, &coord_pk), ("bob", &bob_sk, &bob_pk)],
            2,
        );
        let chain_json = serde_json::to_string(&chain).expect("chain json");
        let inv = recovery_inv("bob", &republic_id);

        // valid: verifies + reconstructs the roster that anchors bob
        let (blocks, sealed) =
            verify_served_chain(&chain_json, &inv, &bob_pk).expect("a valid served chain");
        assert_eq!(blocks.len(), 1);
        let sealed = sealed.expect("a genesis roster");
        assert!(sealed.identities.iter().any(|i| i.member == "bob" && i.identity_pk == bob_pk));

        // an empty chain (chain-less republic) verifies trivially to no roster
        assert_eq!(verify_served_chain("", &inv, &bob_pk).expect("empty ok").0.len(), 0);

        // a doctored link id (≠ the genesis id) is rejected
        let bad_id = recovery_inv("bob", "an-attackers-substituted-id");
        assert!(verify_served_chain(&chain_json, &bad_id, &bob_pk).is_err());

        // a rejoiner whose key the roster does not anchor is rejected
        assert!(verify_served_chain(&chain_json, &inv, &coord_pk).is_err());
        assert!(verify_served_chain(&chain_json, &recovery_inv("mallory", &republic_id), &bob_pk).is_err());

        // a tampered chain (a flipped signature) is hard-rejected by verify_chain
        let mut tampered = chain.clone();
        tampered[0].sigs[0].sig = "0".repeat(128);
        let tampered_json = serde_json::to_string(&tampered).expect("json");
        assert!(verify_served_chain(&tampered_json, &inv, &bob_pk).is_err());
    }
}
