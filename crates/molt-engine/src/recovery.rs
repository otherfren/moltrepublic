// SPDX-License-Identifier: GPL-3.0-or-later

//! The **recovery ritual** transport (see `docs/ritual/recovery_ritual.md`): the
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
use molt_net::{invite, msg_id, supervisor, QueueId, SndQueueAddr, Transport, WrapKey};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a rejoiner waits for the coordinator's `Welcome` after sending its
/// `RecoverRequest`. The window spans the survivors' **human** m-of-n approval
/// of the restore proposal, hence generous; on expiry the operator's failover
/// is to mint a fresh recovery link on any survivor and retry (decision
/// 2026-07-11).
pub const RECOVERY_WELCOME_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A recovery link — `molt://recover/<republic>/<member>/<ticket>/<handover>` —
/// mirroring [`crate::FoundingInvite`], but for an *existing* seat. It carries a
/// transport handover (the coordinator's recovery queue) and a single-use ticket
/// the seat proof binds. The `<handover>` segment is
/// `hex(server ‖ '\n' ‖ queue_id ‖ '\n' ‖ wrap)` so a server URL's `//@=`
/// cannot leak into the path. A link without a handover parses as a preview
/// only and is not actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryInvite {
    /// The republic's display name (spaces travel as dashes).
    pub republic: String,
    /// The returning member's seat handle.
    pub member: String,
    /// The single-use recovery ticket (lowercase hex).
    pub ticket: String,
    /// The coordinator's recovery-queue server (opaque transport address;
    /// empty on loopback).
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
/// block on. Once the
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
                // report back instead of dying silently — the operator asked
                // for a link and must see the calm failed state, and the dead
                // mint's ticket must be unregistered on the actor
                tracing::warn!(%member, error = %e, "recovery-queue provisioning failed");
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx
                    .send(Envelope {
                        cmd: Command::NetRecoverLinkFailed {
                            member,
                            reason: e.to_string(),
                            ticket,
                            generation: Some(generation),
                        },
                        reply,
                    })
                    .await;
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
            cmd_tx.downgrade(),
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
    /// The verified persistent chain: from block 0 for a full coordinator,
    /// or the SUFFIX a pruned coordinator serves (then `checkpoint_blob`
    /// carries the verified anchor state). Empty on a chain-less republic.
    pub chain: Vec<molt_core::ChainBlock>,
    /// WP4b 4c: the checkpoint blob the suffix anchors on (`None` = the
    /// chain roots on the genesis).
    pub checkpoint_blob: Option<molt_core::CheckpointState>,
    /// The founding roster reconstructed from the genesis block — what the
    /// engine materializes the local workspace from. `None` on a chain-less
    /// republic (no genesis to rebuild it).
    pub sealed: Option<molt_core::SealedRoster>,
    /// The re-established full-mesh handovers to the survivors (dynamic mesh
    /// membership, `docs_archive/transport/dynamic_mesh.md`) — the engine stands the runtime
    /// supervisor up over them. Empty when the mesh phase was skipped or timed
    /// out (best-effort: the recovered STATE never depends on it).
    pub mesh: Vec<molt_core::MeshLink>,
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
/// The generic core a genuinely separate node runs (N4's Nostr rejoin will
/// wrap it with the transport parsed from a `molt://recover/…` link). With
/// `bootstrap` it also re-establishes the runtime mesh ([`rejoin_mesh`],
/// best-effort) after the chain verified — the engine's production path;
/// tests that only exercise the crypto/ritual core pass `false`.
///
/// The wait for the Welcome is bounded by [`RECOVERY_WELCOME_TIMEOUT`]
/// (see [`run_rejoin_with_timeout`]) — a coordinator that dies after the
/// request surfaces as an error instead of hanging the rejoiner forever.
pub async fn run_rejoin<T: Transport>(
    transport: T,
    inv: RecoveryInvite,
    phrase: &str,
    bootstrap: bool,
) -> Result<RejoinOutcome, String> {
    run_rejoin_with_timeout(transport, inv, phrase, bootstrap, RECOVERY_WELCOME_TIMEOUT).await
}

/// [`run_rejoin`] with an explicit bound on the welcome wait: the ritual fails
/// with a "timed out" error once `welcome_timeout` elapses without the
/// coordinator's `Welcome`. The deadline is **absolute** — computed once when
/// the wait starts — so noise frames on the reply queue (which the loop
/// ignores) cannot extend it. The operator's failover on expiry is minting a
/// fresh recovery link on any survivor.
pub async fn run_rejoin_with_timeout<T: Transport>(
    transport: T,
    inv: RecoveryInvite,
    phrase: &str,
    bootstrap: bool,
    welcome_timeout: Duration,
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
    // the reply queue is ignored (only the Welcome finishes the rejoin). The
    // deadline is ABSOLUTE — computed once, before the loop — so ignored noise
    // frames cannot extend the wait.
    let deadline = tokio::time::Instant::now() + welcome_timeout;
    let mut reasm = molt_net::Reassembler::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(received) = tokio::time::timeout(remaining, rx.recv()).await else {
            return Err(format!(
                "timed out waiting for the coordinator's welcome after {}s — the coordinator \
                 may be gone; mint a fresh recovery link on any survivor and retry",
                welcome_timeout.as_secs()
            ));
        };
        let Some(delivery) = received else {
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
            // ❼–❽ catch up: VERIFY the served chain from block 0 (an untrusted
            // deliverer is safe — the signatures + links + threshold are checked
            // here, not trusted), then reconstruct the founding roster from the
            // genesis for the engine to materialize from.
            let (verified_chain, sealed, live_roster, checkpoint_blob) =
                verify_served_chain(&chain, &inv, &pk)?;
            // re-establish the runtime mesh (best-effort — the recovered STATE
            // is already safe, so a mesh failure degrades to option A, never
            // fails the recovery). Only after the chain verified: the survivor
            // list is the verified HEAD's live roster (membership blocks may
            // have added seats after the genesis), never the link.
            let mesh = match (sealed.is_some(), bootstrap) {
                (true, true) => {
                    let survivors: Vec<String> = live_roster
                        .iter()
                        .map(|i| i.member.clone())
                        .filter(|m| m != &inv.member)
                        .collect();
                    match rejoin_mesh(
                        &inv.member,
                        &survivors,
                        &transport,
                        &mut mls,
                        &recover_snd,
                        &recover_wrap,
                        crate::founding::MESH_BOOTSTRAP_TIMEOUT,
                    )
                    .await
                    {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(error = %e, "mesh re-join failed — recovered without live links");
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };
            // snapshot AFTER the mesh phase — its announces advanced the ratchet
            let snap = mls.snapshot().map_err(|e| e.to_string())?;
            return Ok(RejoinOutcome {
                member: inv.member,
                pk,
                republic_id: inv.republic_id,
                mls_snapshot: snap,
                chain: verified_chain,
                sealed,
                checkpoint_blob,
                mesh,
            });
        }
    }
}

/// What [`verify_served_chain`] hands back: the verified blocks, the genesis
/// constitution (what the workspace materializes from; `None` on a chain-less
/// republic) and the verified head's **live roster** (the survivor set —
/// membership blocks may have evolved it past the genesis).
type ServedChain = (
    Vec<molt_core::ChainBlock>,
    Option<molt_core::SealedRoster>,
    Vec<molt_core::MemberIdentity>,
    Option<molt_core::CheckpointState>,
);

/// Verify a coordinator-served chain from block 0 — the rejoiner's catch-up
/// check (`recovery_ritual.md` §4 ❽, §5.2). Hard-rejects a chain whose
/// signatures/links/threshold do not verify, whose genesis id differs from the
/// seat-proof-bound `republic_id` (so a coordinator cannot swap in a different
/// republic's chain), or whose **verified head** does not anchor the
/// rejoiner's own `(member, pk)` — the head, not the genesis: `Membership`
/// blocks evolve the roster, and a member who joined after the founding must
/// still be able to recover. Returns the blocks, the genesis constitution
/// (what the workspace materializes from) and the head's LIVE roster (the
/// survivor set). An empty chain (chain-less/demo) verifies trivially.
fn verify_served_chain(chain_json: &str, inv: &RecoveryInvite, pk: &str) -> Result<ServedChain, String> {
    if chain_json.is_empty() {
        return Ok((Vec::new(), None, Vec::new(), None));
    }
    let wire: crate::chain::ServedChainWire =
        serde_json::from_str(chain_json).map_err(|e| format!("decoding the served chain: {e}"))?;
    let (blocks, head, sealed, blob) = match wire {
        crate::chain::ServedChainWire::Full(blocks) => {
            // the WHOLE chain, verified from block 0 (signatures, prev-links,
            // threshold) — the head carries the roster after every membership
            let head = crate::chain::verify_chain(&blocks)?;
            let genesis = blocks.first().ok_or("the served chain is empty")?;
            let sealed = sealed_roster_from_genesis(genesis)
                .ok_or("the served chain does not root on a genesis block")?;
            (blocks, head, sealed, None)
        }
        crate::chain::ServedChainWire::Pruned {
            checkpoint_blob,
            blocks,
        } => {
            // WP4b 4c: a pruned coordinator — the blob is the trust anchor,
            // hard-verified by the suffix rules (founding recomputation,
            // founding-bound anchor signatures, double-apply seed)
            let head =
                crate::chain::verify_suffix_chain(&checkpoint_blob, &blocks, &inv.republic_id)?;
            let sealed = sealed_roster_from_blob(&checkpoint_blob);
            (blocks, head, sealed, Some(checkpoint_blob))
        }
    };
    if head.republic_id != inv.republic_id {
        return Err("the served chain's republic id does not match the recovery link".to_string());
    }
    if !head
        .identities
        .iter()
        .any(|i| i.member == inv.member && i.identity_pk == pk)
    {
        return Err(
            "the served chain's live roster does not anchor our own (name, key)".to_string(),
        );
    }
    Ok((blocks, Some(sealed), head.identities, blob))
}

/// The constitution a checkpoint rejoiner materializes from — rebuilt from
/// the blob's rid-bound FOUNDING table. The genesis attestations are gone
/// with block 0 (deliberately dropped history); authority rests on the
/// verified blob + suffix, so the local Founded record carries an empty
/// attestation set — display/bootstrap metadata, never consensus input.
/// CAUTION: never route this roster through `verify_sealed_roster` or
/// `genesis_chain` (both require one attestation per member and would
/// silently degrade); a pruned-recovered workspace must always carry its
/// chain + blob explicitly.
pub(crate) fn sealed_roster_from_blob(blob: &molt_core::CheckpointState) -> molt_core::SealedRoster {
    molt_core::SealedRoster {
        name: blob.founding_name.clone(),
        republic_id: blob.republic_id.clone(),
        rule_m: blob.rule_m,
        rule_n: blob.rule_n,
        roster: blob.roster.iter().map(|i| i.member.clone()).collect(),
        identities: blob.roster.clone(),
        attestations: Vec::new(),
        agenda: blob.agenda.clone(),
    }
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
            while let Some(d) = rx.recv().await {
                let Ok(plain) = molt_net::wrap::unwrap_block(&wrap, &d.block) else {
                    d.ack.ack();
                    continue;
                };
                let outcome = reasm.push(&plain);
                d.ack.ack();
                if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome {
                    if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                        serde_json::from_slice::<invite::RitualMsg>(&bytes)
                    {
                        if let Ok(raw) = hex::decode(&ct) {
                            let _ = tx.send(raw).await;
                        }
                    }
                    return;
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
                                tracing::warn!(%from, "mesh reply carries no usable queue for us — ignored");
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
                    "mesh re-join timed out — assembling the partial mesh"
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

    /// A coordinator that dies after receiving the `RecoverRequest` must not
    /// hang the rejoiner forever: the welcome wait is bounded, and expiry
    /// surfaces as an error telling the operator to mint a fresh link on any
    /// survivor (decision A1, 2026-07-11).
    #[tokio::test]
    async fn a_dead_coordinator_times_the_rejoin_out() {
        use molt_net::LoopbackHub;

        let hub = LoopbackHub::calm();
        let transport = hub.transport();
        // a real queue NOBODY serves — the coordinator "died" after minting it
        let dead_q = transport.create_queue().await.expect("coordinator queue");
        let wrap = WrapKey::fresh().expect("wrap");
        let inv = RecoveryInvite {
            republic: "Guild".to_string(),
            member: "walter".to_string(),
            ticket: "0011223344".to_string(),
            server: dead_q.snd.server.clone(),
            queue_id: hex::encode(&dead_q.snd.id.0),
            wrap: hex::encode(wrap.to_bytes()),
            // any hex string — the ritual times out before the id matters
            republic_id: "f00dbabe".to_string(),
        };
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");

        // outer guard: the bounded rejoin must return WELL before this trips
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_rejoin_with_timeout(transport, inv, &phrase, false, Duration::from_millis(300)),
        )
        .await
        .expect("the bounded rejoin returns before the outer guard trips");
        let err = result.expect_err("a dead coordinator must surface as an error");
        assert!(
            err.contains("timed out waiting for the coordinator's welcome"),
            "the error names the timeout and the failover, got: {err}"
        );
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
                nostr_pk: "cc".to_string(),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: "bb".to_string(),
                nostr_pk: "dd".to_string(),
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
                nostr_pk: "cc".repeat(32),
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

    /// **Dynamic mesh membership, rejoiner side.** Bob (recovered, in the MLS
    /// group) announces fresh per-pair queues over the recovery channel; the
    /// coordinator authenticates + relays the ciphertext to the other survivor;
    /// each survivor replies — MLS-encrypted, directly onto the queue bob
    /// announced for it — and bob assembles correctly wired links to BOTH.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_rejoiner_reestablishes_mesh_links_with_the_survivors() {
        use molt_net::{mesh, supervisor, LoopbackHub, MlsIncoming, MlsMember, Reassembler};
        use molt_net::{msg_id, Transport};

        // one real 3-member MLS group: coordinator + cara (survivors) + bob
        let (coord_sk, _) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, _) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let (cara_sk, _) = molt_storage::derive_identity_key(&[3u8; 32], "cara");
        let mut coord = MlsMember::new(&coord_sk, "coordinator").expect("coord mls");
        let mut bob = MlsMember::new(&bob_sk, "bob").expect("bob mls");
        let mut cara = MlsMember::new(&cara_sk, "cara").expect("cara mls");
        coord.create_group().expect("group");
        let welcome = coord
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add members")
            .expect("welcome");
        bob.join_from_welcome(&welcome).expect("bob joins");
        cara.join_from_welcome(&welcome).expect("cara joins");

        let hub = LoopbackHub::calm();
        let transport = hub.transport();
        // the coordinator's recovery queue (minted at link time in the product)
        let recover_q = transport.create_queue().await.expect("recovery queue");
        let recover_wrap = WrapKey::fresh().expect("wrap");

        // bob drives the mesh re-join
        let bob_transport = hub.transport();
        let recover_snd = recover_q.snd.clone();
        let rw = recover_wrap.clone();
        let bob_task = tokio::spawn(async move {
            let links = rejoin_mesh(
                "bob",
                &["coordinator".to_string(), "cara".to_string()],
                &bob_transport,
                &mut bob,
                &recover_snd,
                &rw,
                std::time::Duration::from_secs(10),
            )
            .await;
            (bob, links)
        });

        // survivor half shared by the coordinator and cara: decrypt bob's
        // announce, create the own inbound queue for bob, reply MLS-encrypted
        // directly onto the queue bob announced — returns the created queue's
        // send address (what bob's link must point at)
        async fn survivor_reply<T: Transport>(
            me: &str,
            mls: &mut MlsMember,
            transport: &T,
            announce_ct: &[u8],
        ) -> molt_net::SndQueueAddr {
            let MlsIncoming::Application { from, plaintext } =
                mls.decrypt(announce_ct).expect("decrypt bob's announce")
            else {
                panic!("bob's announce is an application message");
            };
            assert_eq!(from, "bob", "the announce is MLS-authenticated as bob");
            let a: mesh::MeshAnnounce = serde_json::from_slice(&plaintext).expect("announce");
            let target = a.queues.get(me).expect("bob announced a queue for me");
            let own_q = transport.create_queue().await.expect("own queue for bob");
            let own_wrap = WrapKey::fresh().expect("own wrap");
            let mut queues = std::collections::BTreeMap::new();
            queues.insert("bob".to_string(), mesh::QueueHandover::of(&own_q.snd, &own_wrap));
            let reply = mesh::MeshAnnounce { queues };
            let ct = mls
                .encrypt(&serde_json::to_vec(&reply).expect("encode"))
                .expect("encrypt reply");
            let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            let payload = serde_json::to_vec(&msg).expect("payload");
            supervisor::send_framed(
                transport,
                &target.addr().expect("announced addr"),
                &target.wrap_key().expect("announced wrap"),
                msg_id(me, "bob", 1),
                &payload,
            )
            .await
            .expect("reply reaches bob's announced queue");
            own_q.snd
        }

        // the coordinator: read bob's announce off the recovery queue, relay the
        // ciphertext verbatim to cara (in the product: a MeshAnnounced event over
        // the runtime mesh), and answer for itself
        let coord_transport = hub.transport();
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let coord_task = tokio::spawn(async move {
            let mut rx = coord_transport.subscribe(&recover_q.rcv).await.expect("subscribe");
            let mut reasm = Reassembler::new();
            let ct = loop {
                let d = rx.recv().await.expect("recovery queue open");
                let Ok(plain) = molt_net::wrap::unwrap_block(&recover_wrap, &d.block) else {
                    d.ack.ack();
                    continue;
                };
                let out = reasm.push(&plain);
                d.ack.ack();
                if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = out {
                    if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                        serde_json::from_slice::<invite::RitualMsg>(&bytes)
                    {
                        break hex::decode(&ct).expect("announce hex");
                    }
                }
            };
            relay_tx.send(ct.clone()).await.expect("relay to cara");
            survivor_reply("coordinator", &mut coord, &coord_transport, &ct).await
        });
        let cara_transport = hub.transport();
        let cara_task = tokio::spawn(async move {
            let ct = relay_rx.recv().await.expect("relayed announce");
            survivor_reply("cara", &mut cara, &cara_transport, &ct).await
        });

        // bob finishes only after BOTH survivors replied (or errors fast), so he
        // is awaited FIRST — a red run must fail here, not hang on the fakes
        let (_bob, links) = tokio::time::timeout(std::time::Duration::from_secs(15), bob_task)
            .await
            .expect("bob's mesh re-join finishes in time")
            .expect("bob task");
        let mut links = links.expect("bob assembles his mesh");
        let coord_snd = tokio::time::timeout(std::time::Duration::from_secs(5), coord_task)
            .await
            .expect("the coordinator fake finished")
            .expect("coordinator task");
        let cara_snd = tokio::time::timeout(std::time::Duration::from_secs(5), cara_task)
            .await
            .expect("the cara fake finished")
            .expect("cara task");
        links.sort_by(|a, b| a.member.cmp(&b.member));
        assert_eq!(links.len(), 2, "one link per survivor");
        // each link SENDS to the queue that survivor created for bob …
        assert_eq!(links[0].member, "cara");
        assert_eq!(links[0].snd_queue, hex::encode(&cara_snd.id.0));
        assert_eq!(links[1].member, "coordinator");
        assert_eq!(links[1].snd_queue, hex::encode(&coord_snd.id.0));
        // … and every link parses back into a runnable PeerLink
        for l in &links {
            assert!(molt_net::PeerLink::from_mesh(l).is_some(), "link {l:?} is runnable");
        }
    }

    /// **One silent survivor must not cost the links that DID come back.** The
    /// survivors that replied have already re-pointed and persisted their side
    /// — discarding the whole mesh on a timeout would leave them sending into
    /// queues nobody ever subscribes. The re-join returns the PARTIAL mesh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_partial_mesh_survives_a_silent_survivor() {
        use molt_net::{mesh, supervisor, LoopbackHub, MlsIncoming, MlsMember, Transport};
        use molt_net::{msg_id, Reassembler};

        // coordinator + cara are the survivors; cara stays silent
        let (coord_sk, _) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, _) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let (cara_sk, _) = molt_storage::derive_identity_key(&[3u8; 32], "cara");
        let mut coord = MlsMember::new(&coord_sk, "coordinator").expect("coord mls");
        let mut bob = MlsMember::new(&bob_sk, "bob").expect("bob mls");
        let cara = MlsMember::new(&cara_sk, "cara").expect("cara mls");
        coord.create_group().expect("group");
        let welcome = coord
            .add_members(&[
                bob.key_package().expect("bob kp"),
                cara.key_package().expect("cara kp"),
            ])
            .expect("add members")
            .expect("welcome");
        bob.join_from_welcome(&welcome).expect("bob joins");

        let hub = LoopbackHub::calm();
        let transport = hub.transport();
        let recover_q = transport.create_queue().await.expect("recovery queue");
        let recover_wrap = WrapKey::fresh().expect("wrap");

        let bob_transport = hub.transport();
        let recover_snd = recover_q.snd.clone();
        let rw = recover_wrap.clone();
        let bob_task = tokio::spawn(async move {
            rejoin_mesh(
                "bob",
                &["coordinator".to_string(), "cara".to_string()],
                &bob_transport,
                &mut bob,
                &recover_snd,
                &rw,
                std::time::Duration::from_secs(2),
            )
            .await
        });

        // only the coordinator answers; cara never does
        let coord_transport = hub.transport();
        let coord_task = tokio::spawn(async move {
            let mut rx = coord_transport.subscribe(&recover_q.rcv).await.expect("subscribe");
            let mut reasm = Reassembler::new();
            let ct = loop {
                let d = rx.recv().await.expect("recovery queue open");
                let Ok(plain) = molt_net::wrap::unwrap_block(&recover_wrap, &d.block) else {
                    d.ack.ack();
                    continue;
                };
                let out = reasm.push(&plain);
                d.ack.ack();
                if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = out {
                    if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                        serde_json::from_slice::<invite::RitualMsg>(&bytes)
                    {
                        break hex::decode(&ct).expect("announce hex");
                    }
                }
            };
            let MlsIncoming::Application { plaintext, .. } =
                coord.decrypt(&ct).expect("decrypt bob's announce")
            else {
                panic!("an application message");
            };
            let a: mesh::MeshAnnounce = serde_json::from_slice(&plaintext).expect("announce");
            let target = a.queues.get("coordinator").expect("a queue for the coordinator");
            let own_q = coord_transport.create_queue().await.expect("own queue");
            let own_wrap = WrapKey::fresh().expect("own wrap");
            let mut queues = std::collections::BTreeMap::new();
            queues.insert("bob".to_string(), mesh::QueueHandover::of(&own_q.snd, &own_wrap));
            let reply = mesh::MeshAnnounce { queues };
            let ct = coord
                .encrypt(&serde_json::to_vec(&reply).expect("encode"))
                .expect("encrypt reply");
            let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            let payload = serde_json::to_vec(&msg).expect("payload");
            supervisor::send_framed(
                &coord_transport,
                &target.addr().expect("addr"),
                &target.wrap_key().expect("wrap"),
                msg_id("coordinator", "bob", 1),
                &payload,
            )
            .await
            .expect("reply reaches bob");
            own_q.snd
        });

        let links = tokio::time::timeout(std::time::Duration::from_secs(10), bob_task)
            .await
            .expect("bob finishes in time")
            .expect("bob task")
            .expect("a PARTIAL mesh is still a mesh");
        let coord_snd = coord_task.await.expect("coordinator fake");
        assert_eq!(links.len(), 1, "the answering survivor's link survives the timeout");
        assert_eq!(links[0].member, "coordinator");
        assert_eq!(links[0].snd_queue, hex::encode(&coord_snd.id.0));
    }

    /// A survivor that never replies bounds the wait: the mesh re-join fails
    /// with a timeout (the caller recovers mesh-less, option A) instead of
    /// hanging the rejoin forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_silent_survivor_times_the_mesh_rejoin_out() {
        use molt_net::{LoopbackHub, MlsMember, Transport};
        let (coord_sk, _) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, _) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let mut coord = MlsMember::new(&coord_sk, "coordinator").expect("coord mls");
        let mut bob = MlsMember::new(&bob_sk, "bob").expect("bob mls");
        coord.create_group().expect("group");
        let welcome = coord
            .add_members(&[bob.key_package().expect("bob kp")])
            .expect("add")
            .expect("welcome");
        bob.join_from_welcome(&welcome).expect("bob joins");

        let hub = LoopbackHub::calm();
        let transport = hub.transport();
        let recover_q = transport.create_queue().await.expect("recovery queue");
        let recover_wrap = WrapKey::fresh().expect("wrap");
        // nobody listens on the recovery queue — no reply ever comes
        let err = rejoin_mesh(
            "bob",
            &["coordinator".to_string()],
            &transport,
            &mut bob,
            &recover_q.snd,
            &recover_wrap,
            std::time::Duration::from_millis(300),
        )
        .await
        .expect_err("a silent survivor must time out");
        assert!(err.contains("0/1"), "the timeout names the shortfall: {err}");
    }

    /// **A member who joined AFTER the genesis recovers against the verified
    /// HEAD, not the genesis roster.** `verify_chain` evolves the identities
    /// across `Membership` blocks; the self-anchor check and the survivor set
    /// must use that head — a genesis-only check would refuse a post-genesis
    /// member's recovery outright and mesh-announce to a stale roster.
    #[test]
    fn a_post_genesis_member_recovers_against_the_verified_head() {
        use molt_core::{approval_bytes, block_link_bytes, ChainBlock, ChainChange, MembershipOp, RosterAttestation};

        let (coord_sk, coord_pk) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, bob_pk) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let (_dave_sk, dave_pk) = molt_storage::derive_identity_key(&[4u8; 32], "dave");
        let (mut chain, republic_id) = signed_genesis(
            &[("coordinator", &coord_sk, &coord_pk), ("bob", &bob_sk, &bob_pk)],
            1,
        );
        // dave joins later: a threshold-committed Membership{Joined} block
        let change = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dave".to_string(),
            identity_pk: dave_pk.clone(),
        };
        let bytes = approval_bytes(&republic_id, 1, &change);
        let block1 = ChainBlock {
            height: 1,
            prev: molt_storage::content_hash(&block_link_bytes(&republic_id, &chain[0])),
            sigs: vec![RosterAttestation {
                member: "coordinator".to_string(),
                sig: molt_storage::identity_sign(&coord_sk, &bytes),
            }],
            change,
        };
        chain.push(block1);
        let chain_json = serde_json::to_string(&chain).expect("chain json");

        // dave lost his device — his recovery must verify against the HEAD
        let inv = recovery_inv("dave", &republic_id);
        let (blocks, sealed, roster, _) = verify_served_chain(&chain_json, &inv, &dave_pk)
            .expect("a post-genesis member recovers");
        assert_eq!(blocks.len(), 2);
        // the genesis constitution stays what the workspace materializes from …
        assert_eq!(
            sealed.expect("genesis roster").identities.len(),
            2,
            "the genesis constitution is untouched by later membership"
        );
        // … while the LIVE roster (survivor set, anchor base) is the head's
        let names: Vec<&str> = roster.iter().map(|i| i.member.as_str()).collect();
        assert_eq!(names, vec!["coordinator", "bob", "dave"]);
    }

    /// WP4b 4c: a PRUNED coordinator serves blob + suffix — the rejoiner
    /// verifies via the suffix rules and materializes from the blob's
    /// rid-bound founding table (empty attestations: authority is the
    /// verified blob, not the local Founded record). A forged blob dies.
    #[test]
    fn a_pruned_coordinator_serves_blob_and_suffix_for_recovery() {
        let (coord_sk, coord_pk) = molt_storage::derive_identity_key(&[1u8; 32], "coordinator");
        let (bob_sk, bob_pk) = molt_storage::derive_identity_key(&[2u8; 32], "bob");
        let (chain, republic_id) = signed_genesis(
            &[("coordinator", &coord_sk, &coord_pk), ("bob", &bob_sk, &bob_pk)],
            2,
        );
        // the cut at the genesis head, anchored at height 1
        let blob = crate::chain::checkpoint_state(&chain, 0).expect("state@0");
        let change = molt_core::ChainChange::Checkpoint {
            upto: 0,
            state_hash: crate::chain::checkpoint_state_hash(&blob),
        };
        let bytes = molt_core::approval_bytes(&republic_id, 1, &change);
        let anchor = molt_core::ChainBlock {
            height: 1,
            prev: crate::chain::block_hash(&republic_id, &chain[0]),
            change,
            sigs: vec![
                molt_core::RosterAttestation {
                    member: "coordinator".to_string(),
                    sig: molt_storage::identity_sign(&coord_sk, &bytes),
                },
                molt_core::RosterAttestation {
                    member: "bob".to_string(),
                    sig: molt_storage::identity_sign(&bob_sk, &bytes),
                },
            ],
        };
        let wire = crate::chain::ServedChainWire::Pruned {
            checkpoint_blob: blob.clone(),
            blocks: vec![anchor.clone()],
        };
        let chain_json = serde_json::to_string(&wire).expect("wire json");
        let inv = recovery_inv("bob", &republic_id);
        let (blocks, sealed, roster, served_blob) =
            verify_served_chain(&chain_json, &inv, &bob_pk).expect("pruned serve verifies");
        assert_eq!(blocks.len(), 1);
        let sealed = sealed.expect("blob constitution");
        assert_eq!(sealed.identities.len(), 2);
        assert!(sealed.attestations.is_empty(), "genesis attestations are gone with block 0");
        assert_eq!(roster.len(), 2);
        assert_eq!(served_blob.expect("blob returned").upto, 0);
        // a forged blob (sock-puppet roster) is hard-rejected
        let mut forged = blob.clone();
        forged.roster[1].identity_pk = "00".repeat(32);
        let forged_wire = crate::chain::ServedChainWire::Pruned {
            checkpoint_blob: forged,
            blocks: vec![anchor],
        };
        let forged_json = serde_json::to_string(&forged_wire).expect("wire json");
        assert!(verify_served_chain(&forged_json, &inv, &bob_pk).is_err());
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
        let (blocks, sealed, roster, _) =
            verify_served_chain(&chain_json, &inv, &bob_pk).expect("a valid served chain");
        assert_eq!(blocks.len(), 1);
        let sealed = sealed.expect("a genesis roster");
        assert!(sealed.identities.iter().any(|i| i.member == "bob" && i.identity_pk == bob_pk));
        assert_eq!(roster, sealed.identities, "no membership blocks: head == genesis");

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
