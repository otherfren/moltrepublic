// SPDX-License-Identifier: GPL-3.0-or-later

//! Block identity v2 (`docs/chain/block_identity_plan.md`): the layout
//! switch by activation height, its boundary, and the holder's duplicate
//! rule.

use super::test_support::*;
use super::*;
use molt_core::chain::{block_id_bytes, BLOCK_V2_FEATURE};

const SEATS3: [&str; 3] = ["petra", "walter", "dora"];

/// A 2-of-3 chain whose block 1 enables v2 (active from height 2), grown
/// to `len` blocks, every block sealed by petra and walter.
fn enabled(len: usize) -> Builder {
    let mut b = Builder::new(&SEATS3, 2);
    b.commit_org(1, "set_features", &format!("memory {BLOCK_V2_FEATURE}"), &["petra", "walter"]);
    while b.blocks.len() < len {
        let id = u64::try_from(b.blocks.len()).expect("small") + 10;
        b.commit_applied(id, &["petra", "walter"]);
    }
    b
}

/// The same chain without the enabling block.
fn plain(len: usize) -> Builder {
    let mut b = Builder::new(&SEATS3, 2);
    while b.blocks.len() < len {
        let id = u64::try_from(b.blocks.len()).expect("small") + 10;
        b.commit_applied(id, &["petra", "walter"]);
    }
    b
}

/// `blocks[h]` re-attested by `signers`.
fn reattested(b: &Builder, h: usize, signers: &[&str]) -> ChainBlock {
    let mut v = b.blocks[h].clone();
    let height = v.height;
    v.sigs = b.seal(height, v.change.clone(), signers).sigs;
    v
}

fn v2_id(b: &Builder, block: &ChainBlock) -> String {
    molt_storage::content_hash(&block_id_bytes(&b.republic_id, block))
}

struct OlderBuild;

impl OlderBuild {
    fn on() -> Self {
        OLDER_BUILD.with(|c| c.set(true));
        OlderBuild
    }
}

impl Drop for OlderBuild {
    fn drop(&mut self) {
        OLDER_BUILD.with(|c| c.set(false));
    }
}

#[test]
fn the_boundary_links_v1_below_and_v2_from_the_activation() {
    let b = enabled(5);
    let head = verify_chain(&b.blocks).expect("verifies");
    assert_eq!(head.block_v2_from, Some(2));
    assert_eq!(b.blocks[2].prev, block_hash(&b.republic_id, &b.blocks[1]), "H links to the v1 hash of H-1");
    assert_eq!(b.blocks[3].prev, v2_id(&b, &b.blocks[2]), "H+1 links to the v2 identity of H");
    assert_eq!(head.hash, v2_id(&b, &b.blocks[4]));
    assert_eq!(verify_chain(&plain(5).blocks).expect("verifies").block_v2_from, None);
}

#[test]
fn another_attestation_keeps_the_identity_from_the_activation() {
    let mut b = enabled(5);
    let variant = reattested(&b, 3, &["walter", "dora"]);
    assert_eq!(v2_id(&b, &variant), v2_id(&b, &b.blocks[3]));
    b.blocks[3] = variant;
    verify_chain(&b.blocks).expect("any m-subset attests the same block");
    // counter: before the activation the signer set is in the link
    let mut p = plain(5);
    p.blocks[3] = reattested(&p, 3, &["walter", "dora"]);
    let err = verify_chain(&p.blocks).expect_err("a v1 variant moves the link");
    assert!(err.contains("does not link"), "{err}");
}

#[test]
fn a_variant_below_the_activation_still_moves_the_link() {
    let mut b = enabled(4);
    b.blocks[1] = reattested(&b, 1, &["walter", "dora"]);
    let err = verify_chain(&b.blocks).expect_err("the enabling block is v1");
    assert!(err.contains("block 2 does not link"), "{err}");
}

#[test]
fn the_identity_does_not_vouch_for_the_attestation() {
    let mut b = enabled(4);
    let mut forged = b.blocks[3].clone();
    forged.sigs.retain(|a| a.member == "petra");
    forged.sigs.push(RosterAttestation { member: "dora".to_string(), sig: "00".repeat(64) });
    assert_eq!(v2_id(&b, &forged), v2_id(&b, &b.blocks[3]));
    b.blocks[3] = forged;
    let err = verify_chain(&b.blocks).expect_err("m valid signers stay required");
    assert!(err.contains("valid approvals"), "{err}");
}

#[test]
fn an_older_build_applies_the_enabling_block_and_stops_after_it() {
    let b = enabled(4);
    let _older = OlderBuild::on();
    verify_chain(&b.blocks[..3]).expect("the enabling block and H still link for it");
    let err = verify_chain(&b.blocks).expect_err("H+1 links to a v2 identity");
    assert_eq!(refusal_rule(&err), "link", "{err}");
}

#[test]
fn a_founding_that_ratifies_it_is_v2_from_height_one() {
    let mut b = Builder::new_with_features(&SEATS3, 2, &[BLOCK_V2_FEATURE], false);
    b.commit_applied(10, &["petra", "walter"]);
    b.commit_applied(11, &["petra", "dora"]);
    let head = verify_chain(&b.blocks).expect("verifies");
    assert_eq!(head.block_v2_from, Some(1));
    assert_eq!(b.blocks[1].prev, block_hash(&b.republic_id, &b.blocks[0]), "the genesis keeps its v1 hash");
    assert_eq!(b.blocks[2].prev, v2_id(&b, &b.blocks[1]));
}

/// A cut below, at and above H: the suffix walk reaches the full walk's
/// head identity.
#[test]
fn a_cut_on_either_side_of_the_activation_walks_alike() {
    for (enable_at, cut_at) in [(3u64, 2u64), (1, 2), (1, 4)] {
        let mut b = Builder::new(&SEATS3, 2);
        let mut blob = None;
        for h in 1..=6u64 {
            if h == enable_at {
                b.commit_org(h, "set_features", &format!("memory {BLOCK_V2_FEATURE}"), &["petra", "walter"]);
            } else if h == cut_at {
                let state = checkpoint_state(&b.blocks, h - 1).expect("state");
                let cut = b.seal(h, ChainChange::Checkpoint { upto: h - 1, state_hash: checkpoint_state_hash(&state) }, &SEATS3);
                b.push(cut);
                blob = Some(state);
            } else {
                b.commit_applied(h + 20, &["petra", "walter"]);
            }
        }
        let full = verify_chain(&b.blocks).expect("full walk");
        let blob = blob.expect("a cut");
        let at = usize::try_from(cut_at).expect("small");
        let suffix = verify_suffix_chain(&blob, &b.blocks[at..], &b.republic_id, None)
            .unwrap_or_else(|e| panic!("enable {enable_at} cut {cut_at}: {e}"));
        assert_eq!(suffix.hash, full.hash, "enable {enable_at} cut {cut_at}");
        assert_eq!(suffix.block_v2_from.is_some(), full.block_v2_from.is_some());
    }
}

/// The holder keeps the first attestation of a v2 block: another one is a
/// duplicate, neither a fork nor a divergence.
#[test]
fn a_second_attestation_is_a_duplicate_not_a_fork() {
    let b = enabled(4);
    let mut walter = chain_peer_3("walter", &b);
    let held = walter.chain.blocks[3].sigs.clone();
    for (seq, h) in [(10u64, 3usize), (11, 2)] {
        let variant = reattested(&b, h, &["petra", "dora"]);
        wire(&mut walter, "dora", seq, WorkspaceEvent::Committed(variant));
    }
    assert_eq!(walter.chain.fork_decisions, 0);
    assert!(walter.chain.diverged.is_empty() && walter.chain.fork_candidates.is_empty());
    assert_eq!(walter.chain.blocks[3].sigs, held, "the first attestation stays");
    assert_eq!(walter.chain.head.as_ref().expect("head").hash, v2_id(&b, &b.blocks[3]));
}

/// Counter-test: before the activation another attestation of the tip is
/// a fork decision (a relink).
#[test]
fn before_the_activation_a_second_attestation_is_a_fork() {
    let mut b = plain(2);
    let change = ChainChange::Applied {
        proposal_id: 50,
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "id": 50 }),
    };
    let mut variants: Vec<ChainBlock> = [["petra", "walter"], ["petra", "dora"], ["walter", "dora"]]
        .iter()
        .map(|s| b.seal(2, change.clone(), s))
        .collect();
    variants.sort_by_key(|v| block_hash(&b.republic_id, v));
    let low = variants.remove(0);
    b.push(variants.pop().expect("the highest"));
    let mut walter = chain_peer_3("walter", &b);
    wire(&mut walter, "dora", 10, WorkspaceEvent::Committed(low));
    assert_eq!(walter.chain.fork_decisions, 1, "a v1 variant is decided");
}

// --- S2: activation -------------------------------------------------------

fn enable_payload() -> serde_json::Value {
    serde_json::json!({ "op": "set_features", "value": format!("memory {BLOCK_V2_FEATURE}") })
}

/// Proposing and signing the enabling vote waits for every seat's
/// capability; another feature vote does not.
#[test]
fn enabling_needs_every_seat_capability() {
    let b = plain(1);
    let mut petra = genesis_seat("petra", &b, b.blocks.clone());
    petra.chain.block_v2_caps_override = Some(false);
    let err = petra
        .cmd_propose(Surface::Organization, enable_payload())
        .expect_err("a seat cannot follow");
    assert!(err.to_string().contains("older build"), "{err}");
    let change = ChainChange::Applied { proposal_id: 7, surface: Surface::Organization, payload: enable_payload() };
    assert!(petra.block_v2_enable_blocker(&change).is_some());
    assert_eq!(petra.chain_sync().block_v2_wait, "a seat runs an older build");
    let other = ChainChange::Applied {
        proposal_id: 8,
        surface: Surface::Organization,
        payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
    };
    assert!(petra.block_v2_enable_blocker(&other).is_none(), "another feature is not held");
    petra.chain.block_v2_caps_override = Some(true);
    assert!(petra.block_v2_enable_blocker(&change).is_none());
    petra.cmd_propose(Surface::Organization, enable_payload()).expect("every seat can follow");
}

/// Without an announcement a seat counts as not capable.
#[test]
fn an_unheard_seat_keeps_it_off() {
    let b = plain(1);
    let petra = genesis_seat("petra", &b, b.blocks.clone());
    assert!(petra.block_v2_not_capable().is_some_and(|w| w.ends_with("has not announced")));
}

/// A holder follows the chain block by block across the activation and
/// reports it.
#[test]
fn a_holder_follows_the_chain_across_the_activation() {
    let b = enabled(6);
    let mut walter = chain_peer_3("walter", &plain(1));
    for blk in &b.blocks[1..] {
        walter.receive_block(blk.clone());
    }
    let head = walter.chain.head.as_ref().expect("head");
    assert_eq!((head.height, head.hash.as_str()), (5, b.head_hash.as_str()));
    assert_eq!(walter.chain_sync().block_v2_since, 2);
    assert_eq!(walter.chain_sync().head_hash, v2_id(&b, &b.blocks[5]));
}

/// A rejoiner verifies a served v2 chain, full or anchored on a cut above
/// the activation, to the same head identity.
#[test]
fn a_rejoiner_verifies_a_served_chain_after_the_activation() {
    let mut b = enabled(4);
    let state = checkpoint_state(&b.blocks, 3).expect("state");
    let cut = b.seal(4, ChainChange::Checkpoint { upto: 3, state_hash: checkpoint_state_hash(&state) }, &SEATS3);
    b.push(cut);
    b.commit_applied(40, &["walter", "dora"]);
    let (full, _) = verify_served(None, &b.blocks, Some(&b.republic_id)).expect("full");
    let (anchored, _) = verify_served(Some(&state), &b.blocks[4..], Some(&b.republic_id)).expect("anchored");
    assert_eq!(full.hash, anchored.hash);
    assert_eq!(full.hash, v2_id(&b, &b.blocks[5]));
}
