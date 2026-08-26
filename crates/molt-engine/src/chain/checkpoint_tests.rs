// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::checkpoint`]: the self-proposing cut,
//! verify-before-sign, the local drop and the re-anchor on a served blob.

use super::test_support::*;
use super::*;
use super::checkpoint::AUTO_CHECKPOINT_MIN_LEN;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::identity_sign;
use serde_json::json;

/// **Known-debt refinement (2026-08-16 list): only a buffered block
/// ADJACENT to head pins the auto-checkpoint.** The buffer accepts
/// claims up to head+4096, so an insider posting one plausible
/// near-future height used to freeze compaction until a drain or
/// re-serve cleared it. A gap block cannot apply next — only head+1
/// says the head is about to move and the cut would be stale.
#[test]
fn a_far_future_buffered_block_does_not_pin_the_auto_checkpoint() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    for i in 1..=32 {
        b.commit_applied(i, &["petra", "walter"]);
    }
    let head_h = b.blocks.last().expect("blocks").height;
    let mut dummy = b.blocks[1].clone();

    // a far-future claim in the buffer must NOT hold the compaction
    let mut peer = chain_peer("petra", &b, b.blocks.clone());
    dummy.height = head_h + 2;
    peer.pending_blocks.insert(head_h + 2, dummy.clone());
    peer.maybe_auto_checkpoint();
    assert!(
        peer.proposal_changes
            .values()
            .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
        "a gap block cannot apply next - the cut must still be proposed"
    );

    // …but a block adjacent to head still pins it: the head is about
    // to move and the cut would be stale on arrival
    let mut peer = chain_peer("petra", &b, b.blocks.clone());
    dummy.height = head_h + 1;
    peer.pending_blocks.insert(head_h + 1, dummy);
    peer.maybe_auto_checkpoint();
    assert!(
        !peer
            .proposal_changes
            .values()
            .any(|c| matches!(c, ChainChange::Checkpoint { .. })),
        "an adjacent buffered block keeps pinning the auto-checkpoint"
    );
}

/// L3: ONE cut per head is registered and co-signed — the identical
/// (upto, state_hash) under fresh ids minted one registry entry plus
/// one signed Approved per frame (a 1:1 outbound amplifier).
#[test]
fn only_one_cut_per_head_registers() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let ours = checkpoint_state_hash(
        &walter.own_checkpoint_state(0).expect("own projection"),
    );
    for id in 50..60u64 {
        walter.receive_checkpoint_proposal(id, 0, &ours);
    }
    let cuts = walter
        .proposal_changes
        .values()
        .filter(|c| matches!(c, ChainChange::Checkpoint { .. }))
        .count();
    assert_eq!(cuts, 1, "the first id IS the cut for this head");
}

/// WP4b stage 2, full holders: a committed checkpoint block verifies
/// only when its `state_hash` matches THIS chain's own recomputed
/// projection — a forged or drifted summary is hard-rejected with the
/// whole chain (all-or-nothing, like every other violation).
#[test]
fn a_checkpoint_block_verifies_against_the_own_projection() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["walter", "dora"]);
    let state = checkpoint_state(&b.blocks, 2).expect("state@2");
    let good = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&state),
        },
        &["petra", "walter"],
    );
    let mut chain = b.blocks.clone();
    chain.push(good.clone());
    let head = verify_chain(&chain).expect("a truthful checkpoint verifies");
    assert_eq!(head.height, 3);
    // a forged state hash is rejected with the whole chain
    let forged = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: molt_storage::content_hash(b"not the projection"),
        },
        &["petra", "walter"],
    );
    let mut bad = b.blocks.clone();
    bad.push(forged);
    assert!(verify_chain(&bad).is_err(), "a forged checkpoint kills the chain");
    // upto must precede the block height
    let self_ref = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 3,
            state_hash: checkpoint_state_hash(&state),
        },
        &["petra", "walter"],
    );
    let mut bad = b.blocks.clone();
    bad.push(self_ref);
    assert!(verify_chain(&bad).is_err(), "upto >= height is structural nonsense");
}

/// WP4b stage 2, suffix holders: a chain that BEGINS with a checkpoint
/// verifies from the blob as trust anchor — blob hash, founding
/// recomputation (forgery check without the genesis), current-roster
/// threshold on the anchor block, double-apply seeded across the cut.
#[test]
fn a_suffix_chain_bootstraps_from_a_checkpoint() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["walter", "dora"]);
    let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
    let anchor = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&blob),
        },
        &["petra", "walter"],
    );
    b.push(anchor.clone());
    // one more applied block on top — the suffix a dropped-history
    // holder keeps
    b.commit_applied(7, &["petra", "dora"]);
    let suffix: Vec<ChainBlock> = b.blocks[3..].to_vec();
    assert_eq!(suffix.len(), 2, "anchor + one applied block");

    let head = verify_suffix_chain(&blob, &suffix, &b.republic_id)
        .expect("the suffix verifies from the checkpoint anchor");
    assert_eq!(head.height, 4);
    assert_eq!(head.identities.len(), 3, "roster comes from the blob");

    // a doctored blob (foreign roster key) fails the hash check
    let mut forged = blob.clone();
    forged.roster[0].identity_pk = "00".repeat(32);
    assert!(
        verify_suffix_chain(&forged, &suffix, &b.republic_id).is_err(),
        "a doctored roster no longer hashes to the signed state"
    );
    // …and its nostr_pk twin: under checkpoint-v2 the third anchor is
    // inside the hashed bytes, so a swapped roster transport anchor is
    // caught exactly like a swapped identity key (under v1 it was NOT
    // hashed — a served blob's roster anchor was silently mutable)
    let mut forged_npk = blob.clone();
    forged_npk.roster[0].nostr_pk = "ee".repeat(32);
    assert!(
        verify_suffix_chain(&forged_npk, &suffix, &b.republic_id).is_err(),
        "a doctored roster nostr anchor no longer hashes to the signed state"
    );
    // a wholly self-consistent forged blob still fails the founding
    // recomputation against the expected republic id
    let mut alien = blob.clone();
    alien.founding_name = "Fake Club".to_string();
    let alien_anchor_hash = checkpoint_state_hash(&alien);
    let mut alien_suffix = suffix.clone();
    if let ChainChange::Checkpoint { state_hash, .. } = &mut alien_suffix[0].change {
        *state_hash = alien_anchor_hash;
    }
    assert!(
        verify_suffix_chain(&alien, &alien_suffix, &b.republic_id).is_err(),
        "a forged founding does not recompute to the real republic id"
    );
    // double-apply across the cut: proposal 1 was consumed below upto
    let mut replay = b.clone();
    replay.commit_applied(1, &["petra", "walter"]);
    let replay_suffix: Vec<ChainBlock> = replay.blocks[3..].to_vec();
    assert!(
        verify_suffix_chain(&blob, &replay_suffix, &b.republic_id).is_err(),
        "an id consumed below the cut cannot re-apply in the suffix"
    );
    // below-threshold anchor signatures are refused
    let weak_anchor = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&blob),
        },
        &["petra"],
    );
    assert!(
        verify_suffix_chain(&blob, &[weak_anchor], &b.republic_id).is_err(),
        "one signature is not a threshold"
    );
}

/// WP4b stage 3: the propose flow end to end at the state level.
/// Petra proposes the cut (self-cosign = 1 of 2); Walter receives the
/// gossip, RECOMPUTES the hash from his own chain, auto-co-signs on
/// the match, and the checkpoint block seals at 2-of-2 — on both
/// nodes, byte-identically. A mismatched hash is never signed; a
/// stale cut dies on re-base instead of sealing an invalid block.
#[test]
fn a_checkpoint_proposal_seals_via_verify_before_sign() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let mut walter = chain_signer("walter", &b, b.blocks.clone());

    let id = match petra.cmd_propose_checkpoint().expect("propose") {
        molt_core::Reply::Proposed { id } => id.0,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(
        petra.pending_sigs.get(&id).map(|p| p.sigs.len()),
        Some(1),
        "the proposer co-signed its own cut"
    );
    let (upto, state_hash) = match petra.proposal_changes.get(&id) {
        Some(ChainChange::Checkpoint { upto, state_hash }) => {
            (*upto, state_hash.clone())
        }
        other => panic!("unexpected change: {other:?}"),
    };
    assert_eq!(upto, 1, "the cut is the current head (B-F1)");

    // a WRONG hash is refused: nothing registered, nothing signed
    walter.receive_checkpoint_proposal(id, upto, "00");
    assert!(!walter.proposal_changes.contains_key(&id));
    assert!(!walter.pending_sigs.contains_key(&id));

    // the truthful gossip: walter recomputes, matches, auto-co-signs
    walter.receive_checkpoint_proposal(id, upto, &state_hash);
    let petra_sig = petra
        .pending_sigs
        .get(&id)
        .expect("petra's set")
        .sigs
        .first()
        .expect("petra signed")
        .sig
        .clone();
    walter.receive_approval(id, "petra", 2, &petra_sig);
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        2,
        "the checkpoint sealed at 2-of-2 on walter"
    );
    assert!(matches!(
        walter.chain.last().expect("block").change,
        ChainChange::Checkpoint { .. }
    ));
    // petra converges from walter's signature the same way
    let walter_sig = walter
        .chain
        .last()
        .expect("block")
        .sigs
        .iter()
        .find(|a| a.member == "walter")
        .expect("walter signed")
        .sig
        .clone();
    petra.receive_approval(id, "walter", 2, &walter_sig);
    assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
    assert_eq!(
        block_hash(&b.republic_id, petra.chain.last().expect("b")),
        block_hash(&b.republic_id, walter.chain.last().expect("b")),
        "both nodes sealed the byte-identical checkpoint block"
    );
    // the sealed proposal's bookkeeping is gone on both
    assert!(!petra.proposal_changes.contains_key(&id));
    assert!(!walter.proposal_changes.contains_key(&id));
}

/// WP4b stage 4: sealing a checkpoint DROPS the summarized history
/// locally (B-F2), the blob becomes the trust anchor, pre-cut applied
/// entries stay readable, and the pruned holder keeps verifying and
/// extending its suffix chain — including a reopen-style re-adopt.
#[test]
fn a_sealed_checkpoint_drops_history_and_the_holder_keeps_governing() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // the propose flow seals the cut at 2-of-2 (stage-3 mechanics)
    let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
    walter.receive_checkpoint_proposal(40, 1, &hash);
    let change = ChainChange::Checkpoint { upto: 1, state_hash: hash };
    let bytes = approval_bytes(&b.republic_id, 2, &change);
    let petra_sig = identity_sign(b.key("petra"), &bytes);
    walter.receive_approval(40, "petra", 2, &petra_sig);
    // sealed AND pruned: only the anchor remains, the blob anchors
    assert_eq!(walter.chain_head.as_ref().expect("head").height, 2);
    assert_eq!(walter.chain.len(), 1, "history below the cut is dropped");
    assert!(matches!(
        walter.chain.first().expect("anchor").change,
        ChainChange::Checkpoint { .. }
    ));
    let blob = walter.checkpoint_blob.clone().expect("blob anchors the holder");
    assert_eq!(blob.upto, 1);
    // pre-cut applied entries survive in the read projection
    let mem = walter.chain_applied.get(&Surface::Memory).expect("projection");
    assert_eq!(mem.len(), 1, "the pre-cut applied entry stays readable");
    // the pruned holder keeps governing: a fresh applied change seals
    // on top of the suffix (verify runs the suffix rules)
    let payload = json!({"op": "add_note", "title": "post-cut"});
    walter.receive_proposed(41, Surface::Memory, payload.clone(), "peer");
    let post = ChainChange::Applied {
        proposal_id: 41,
        surface: Surface::Memory,
        payload,
    };
    let bytes = approval_bytes(&b.republic_id, 3, &post);
    let petra_sig = identity_sign(b.key("petra"), &bytes);
    walter.receive_approval(41, "petra", 3, &petra_sig);
    walter.chain_sign_and_gossip_approval(41);
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        3,
        "the pruned holder extends its suffix"
    );
    assert_eq!(
        walter.chain_applied.get(&Surface::Memory).map(|v| v.len()),
        Some(2),
        "pre- and post-cut entries read together"
    );
    // reopen-style: a fresh holder re-anchors on blob + suffix
    let mut reopened = chain_peer("walter", &b, b.blocks.clone());
    reopened.checkpoint_blob = Some(blob);
    reopened.adopt_chain(walter.chain.clone());
    assert_eq!(
        reopened.chain_head.as_ref().expect("head").height,
        3,
        "a pruned chain re-adopts from the persisted blob"
    );
    assert_eq!(
        reopened.chain_applied.get(&Surface::Memory).map(|v| v.len()),
        Some(2)
    );
    // …and the Accepted cards match an unpruned holder's (review
    // 2026-08-16): the pre-cut card materializes from the blob's
    // summarized payloads — voter pills open, the sigs went with the
    // cut — the post-cut card from its live block, voters proven
    let snap = reopened.snapshot(Surface::Memory, None, None);
    let card = |pid: u64| {
        snap.accepted
            .iter()
            .find(|c| c.id.0 == pid)
            .unwrap_or_else(|| panic!("card {pid}"))
    };
    assert_eq!(card(1).approvals, 0, "pre-cut: only chain-provable votes show");
    assert!(
        card(41)
            .votes
            .iter()
            .any(|v| v.vote == molt_core::VoteState::Approved),
        "post-cut: the live block's signers show"
    );
}

/// Drive one gated proposal through `s` to a sealed block: peer
/// approval first, then the local co-sign seals at 2-of-2.
fn seal_one(s: &mut crate::State, b: &Builder, peer: &str, id: u64) {
    let target = s.chain_head.as_ref().expect("head").height + 1;
    let payload = json!({"op": "add_note", "id": id});
    s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
    let change = ChainChange::Applied {
        proposal_id: id,
        surface: Surface::Memory,
        payload,
    };
    let bytes = approval_bytes(&b.republic_id, target, &change);
    let sig = identity_sign(b.key(peer), &bytes);
    s.receive_approval(id, peer, target, &sig);
    s.chain_sign_and_gossip_approval(id);
    assert_eq!(
        s.chain_head.as_ref().expect("head").height,
        target,
        "the driven proposal seals"
    );
}

/// The pending checkpoint cut registered in `s`, if any.
fn pending_cut(s: &crate::State) -> Option<u64> {
    s.proposal_changes.values().find_map(|c| match c {
        ChainChange::Checkpoint { upto, .. } => Some(*upto),
        _ => None,
    })
}

/// WP4b automation: once the chain reaches the trigger length, the
/// alphabetically LOWEST-named roster member auto-proposes the
/// compaction cut right after a block commit (every co-signer is at
/// the same head then) — and co-signs it like a manual propose. A
/// non-lowest member never auto-proposes: one deterministic
/// proposer, no node-local id collisions.
#[test]
fn the_lowest_member_auto_proposes_a_checkpoint_at_the_trigger_length() {
    let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 2);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    // one below the trigger: a commit runs the hook, but no cut yet —
    // pins the length lower bound THROUGH the hook, not just at init
    seal_one(&mut petra, &b, "walter", 90);
    assert_eq!(pending_cut(&petra), None, "below the trigger: no cut");
    seal_one(&mut petra, &b, "walter", 300);
    let head = petra.chain_head.as_ref().expect("head").height;
    assert_eq!(
        pending_cut(&petra),
        Some(head),
        "the lowest member proposes the cut at the fresh head"
    );

    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    seal_one(&mut walter, &b, "petra", 90);
    seal_one(&mut walter, &b, "petra", 300);
    assert_eq!(
        pending_cut(&walter),
        None,
        "a non-lowest member never auto-proposes"
    );
}

/// The trigger is bound to SEALING at the live head: a passively
/// applied block (catch-up serve, another sealer's broadcast) never
/// auto-proposes — a catching-up node would cut at a stale
/// intermediate head, and a lockstep-catching-up quorum could even
/// co-sign that cut and fork a holder after it dropped history.
#[test]
fn a_passively_applied_block_never_auto_proposes() {
    let mut b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    // the trigger length arrives via the PASSIVE path — no cut
    b.commit_applied(400, &["petra", "walter"]);
    petra.receive_block(b.blocks.last().expect("built block").clone());
    assert_eq!(petra.chain.len(), AUTO_CHECKPOINT_MIN_LEN, "passively at length");
    assert_eq!(
        pending_cut(&petra),
        None,
        "a passively applied block never triggers the cut"
    );
    // the next SELF-sealed block fires it
    seal_one(&mut petra, &b, "walter", 90);
    let head = petra.chain_head.as_ref().expect("head").height;
    assert_eq!(
        pending_cut(&petra),
        Some(head),
        "the node's own seal at the live head triggers the cut"
    );
}

/// The automation never cuts while a vote is open: an interfering
/// seal would only stale the cut. The trigger re-fires on the commit
/// that resolves the last open vote.
#[test]
fn no_auto_checkpoint_while_a_vote_is_open() {
    let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    // a second, still-open vote holds the cut back
    petra.receive_proposed(91, Surface::Memory, json!({"op": "add_note", "id": 91}), "peer");
    seal_one(&mut petra, &b, "walter", 90);
    assert_eq!(
        pending_cut(&petra),
        None,
        "no cut while another vote is open"
    );
    // resolving the open vote triggers the cut on ITS commit
    let target = petra.chain_head.as_ref().expect("head").height + 1;
    let change = ChainChange::Applied {
        proposal_id: 91,
        surface: Surface::Memory,
        payload: json!({"op": "add_note", "id": 91}),
    };
    let bytes = approval_bytes(&b.republic_id, target, &change);
    let sig = identity_sign(b.key("walter"), &bytes);
    petra.receive_approval(91, "walter", target, &sig);
    petra.chain_sign_and_gossip_approval(91);
    assert_eq!(
        pending_cut(&petra),
        Some(target),
        "the commit that clears the last open vote fires the cut"
    );
}

/// A staled cut needs no timer: the very block that staled it re-runs
/// the trigger and re-proposes at the new head.
#[test]
fn a_staled_auto_cut_re_proposes_on_the_next_commit() {
    let b = grown_chain(AUTO_CHECKPOINT_MIN_LEN - 1);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    seal_one(&mut petra, &b, "walter", 90);
    let first_cut = pending_cut(&petra).expect("auto cut pending");
    // an interfering surface vote seals first — the cut goes stale
    // (id 300: well clear of the auto-cut's freshly minted next_id)
    seal_one(&mut petra, &b, "walter", 300);
    let head = petra.chain_head.as_ref().expect("head").height;
    assert_eq!(
        pending_cut(&petra),
        Some(head),
        "the staled cut is re-proposed at the new head"
    );
    assert!(first_cut < head, "the old cut was swept, not resurrected");
}

/// WP4b stage 4b: a holder that is BEHIND a served cut re-anchors on
/// blob + anchor + suffix (hard-verified), and a forged blob is
/// dropped at the cheap rid check.
#[test]
fn a_lagging_holder_re_anchors_on_a_served_blob() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
    let anchor = b.seal(
        2,
        ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&blob),
        },
        &["petra", "walter"],
    );
    b.push(anchor.clone());
    b.commit_applied(7, &["petra", "walter"]);
    let suffix_tail = b.blocks.last().expect("tail").clone();

    // the laggard holds only the genesis
    let mut lag = chain_peer("walter", &b, b.blocks[..1].to_vec());
    assert_eq!(lag.chain_head.as_ref().expect("head").height, 0);
    // a forged blob (wrong founding) dies at the rid check
    let mut forged = blob.clone();
    forged.founding_name = "Fake".to_string();
    lag.receive_checkpoint_blob(forged);
    assert!(lag.pending_served_blob.is_none());
    // the served pieces arrive in any order: blob, tail, anchor
    lag.receive_checkpoint_blob(blob.clone());
    lag.receive_block(suffix_tail);
    assert_eq!(lag.chain_head.as_ref().expect("head").height, 0, "waits for the anchor");
    lag.receive_block(anchor);
    assert_eq!(
        lag.chain_head.as_ref().expect("head").height,
        3,
        "re-anchored on blob + anchor + suffix"
    );
    assert!(lag.checkpoint_blob.is_some());
    assert_eq!(
        lag.chain_applied.get(&Surface::Memory).map(|v| v.len()),
        Some(2),
        "pre-cut and post-cut entries both readable"
    );
}

/// Review pins: an id collision must never turn the auto-cosign into
/// an unattended approval of a DIFFERENT change, and the gossip frame
/// crosses the wire.
#[test]
fn a_checkpoint_proposal_never_signs_a_colliding_id() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
    // id already names a pending MEMBERSHIP change → refused, unsigned
    walter.receive_membership_proposal(5, MembershipOp::Restored, "petra", &b.pk("petra"), None, Vec::new(), None);
    walter.receive_checkpoint_proposal(5, 1, &hash);
    assert!(
        !walter.pending_sigs.contains_key(&5),
        "an occupied id must never be auto-signed"
    );
    assert!(matches!(
        walter.proposal_changes.get(&5),
        Some(ChainChange::Membership { .. })
    ));
    // id already names a SURFACE proposal → refused too
    walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
    walter.receive_checkpoint_proposal(6, 1, &hash);
    assert!(!walter.pending_sigs.contains_key(&6));
    // a replayed valid frame does not amplify into more signatures
    walter.receive_checkpoint_proposal(9, 1, &hash);
    let sigs = walter.pending_sigs.get(&9).map(|p| p.sigs.len());
    walter.receive_checkpoint_proposal(9, 1, &hash);
    assert_eq!(walter.pending_sigs.get(&9).map(|p| p.sigs.len()), sigs);
    // the gossip frame is wire-scoped
    assert!(crate::net::crosses_wire(&WorkspaceEvent::CheckpointProposed {
        id: ProposalId(1),
        upto: 1,
        state_hash: hash,
    }));
}

/// A checkpoint cut pinned at the old head dies when another block
/// commits first — dropped on re-base (re-cut needed), never re-signed
/// into an invalid block.
#[test]
fn a_stale_checkpoint_proposal_dies_on_rebase() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let id = match petra.cmd_propose_checkpoint().expect("propose") {
        molt_core::Reply::Proposed { id } => id.0,
        other => panic!("unexpected: {other:?}"),
    };
    // another applied block races the checkpoint to height 2
    b.commit_applied(7, &["petra", "walter"]);
    petra.receive_block(b.blocks.last().expect("block").clone());
    assert_eq!(petra.chain_head.as_ref().expect("head").height, 2);
    assert!(
        !petra.proposal_changes.contains_key(&id)
            && !petra.pending_sigs.contains_key(&id),
        "the stale cut is dropped, not re-signed"
    );
}

/// shared_memory_real.md WP-B keystone: memory's applied entries are
/// ACCUMULATING at a checkpoint cut (`applied_lww_slot` = None), so
/// the fold over the summarized state is byte-identical to the fold
/// over the full chain — a cut can never fork the wiki.
#[test]
fn a_checkpoint_cut_keeps_the_wiki_fold_identical() {
    const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const EDIT_A: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    for (h, id, patch) in [(1u64, 10u64, ADD_A), (2, 11, EDIT_A)] {
        let block = b.seal(
            h,
            ChainChange::Applied {
                proposal_id: id,
                surface: Surface::Memory,
                payload: serde_json::json!({ "op": "wiki_patch", "value": patch }),
            },
            &["petra", "walter"],
        );
        b.push(block);
    }
    let full = chain_signer("walter", &b, b.blocks.clone());
    let full_tree = full.wiki_tree();
    assert_eq!(
        full_tree.get("a.md").map(String::as_str),
        Some("hallo\nworld\n")
    );
    let state = checkpoint_state(&b.blocks, 2).expect("summary");
    let mem: Vec<serde_json::Value> = state
        .applied
        .iter()
        .find(|(s, _)| *s == Surface::Memory)
        .map(|(_, entries)| entries.iter().map(|(_, p)| p.clone()).collect())
        .expect("memory summary");
    assert_eq!(
        molt_core::wiki_fold::wiki_fold(&mem),
        full_tree,
        "a cut keeps the fold byte-identical"
    );
}

/// Two racing enables both survive a compaction cut: `set_features`
/// entries ACCUMULATE in the checkpoint summary (deliberately no
/// `applied_lww_slot` — an LWW summary would keep only the later value
/// and silently lose the other vote's addition across the cut).
#[test]
fn racing_feature_enables_both_survive_a_checkpoint_cut() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    // two independently-proposed enables, each a superset of the
    // baseline but not of each other (the race)
    for (h, value) in [(1, "memory quests"), (2, "memory vault")] {
        let block = b.seal(
            h,
            ChainChange::Applied {
                proposal_id: h,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_features", "value": value }),
            },
            &["petra", "walter"],
        );
        b.push(block);
    }
    let state = checkpoint_state(&b.blocks, 2).expect("summary");
    let org = state
        .applied
        .iter()
        .find(|(s, _)| *s == Surface::Organization)
        .map(|(_, entries)| entries)
        .expect("organization summary");
    let kept: Vec<&str> = org
        .iter()
        .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        kept,
        vec!["memory quests", "memory vault"],
        "both racing enables survive the summary"
    );
    // …and the union over the summarized entries is the full set
    let walter = chain_signer("walter", &b, b.blocks.clone());
    assert_eq!(
        walter.effective_features(),
        vec!["memory".to_string(), "quests".to_string(), "vault".to_string()],
    );
}

/// **A cut must not carry every superseded avatar forever**
/// (`member_profiles_plan.md` §3): the profile ops hold per-member LWW
/// slots, so the summary keeps the LATEST picture and description per
/// seat — one seat's edit never drops another's — and the fold over the
/// summarized entries equals the fold over the full chain.
#[test]
fn a_checkpoint_cut_keeps_only_the_latest_avatar_per_member() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let entries = [
        ("set_member_image", "petra", "old.png", "b2xk"),
        ("set_member_desc", "petra", "typo", ""),
        ("set_member_image", "walter", "walter.png", "d2FsdGVy"),
        ("set_member_image", "petra", "new.png", "bmV3"),
        ("set_member_desc", "petra", "keeps the bees", ""),
    ];
    for (h, (op, member, value, bytes)) in entries.iter().enumerate() {
        let height = u64::try_from(h + 1).expect("small height");
        let mut payload = serde_json::json!({ "op": op, "member": member, "value": value });
        if !bytes.is_empty() {
            payload["bytes_b64"] = serde_json::Value::String((*bytes).to_string());
        }
        let block = b.seal(
            height,
            ChainChange::Applied {
                proposal_id: height,
                surface: Surface::Organization,
                payload,
            },
            &["petra", "walter"],
        );
        b.push(block);
    }
    let state = checkpoint_state(&b.blocks, 5).expect("summary");
    let org: Vec<(Option<u64>, serde_json::Value)> = state
        .applied
        .iter()
        .find(|(s, _)| *s == Surface::Organization)
        .map(|(_, e)| e.iter().map(|(id, p)| (Some(*id), p.clone())).collect())
        .expect("organization summary");
    let kept: Vec<&str> = org
        .iter()
        .filter_map(|(_, p)| p.get("value").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        kept,
        vec!["walter.png", "new.png", "keeps the bees"],
        "the cut must keep exactly the latest entry per member and field"
    );

    // the post-cut fold is the live fold
    let full = chain_signer("walter", &b, b.blocks.clone());
    let live: Vec<(String, String, String)> = full
        .member_profiles()
        .iter()
        .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
        .collect();
    let mut cut = chain_signer("walter", &b, vec![b.blocks[0].clone()]);
    cut.chain_applied.insert(Surface::Organization, org);
    let after: Vec<(String, String, String)> = cut
        .member_profiles()
        .iter()
        .map(|(m, p)| ((*m).to_string(), p.image.clone(), p.desc.to_string()))
        .collect();
    assert_eq!(after, live, "a cut must not change what the profiles fold to");
    assert_eq!(
        live,
        vec![
            ("petra".to_string(), "new.png".to_string(), "keeps the bees".to_string()),
            ("walter".to_string(), "walter.png".to_string(), String::new()),
        ]
    );
}
