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
use molt_net::{SndQueueAddr, Transport, WrapKey};
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
#[cfg_attr(not(test), allow(dead_code))] // wired by the rejoiner materialize increment
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
}
