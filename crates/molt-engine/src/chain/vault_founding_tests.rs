// SPDX-License-Identifier: GPL-3.0-or-later

//! The chain side of a vault founding (vault build plan S2): the roster-v6
//! rules at the genesis, the v5 mock left alone, and the walk's own
//! vault context (plan 1.3.8, 1.3.12, 1.3.13).

use super::test_support::*;
use super::*;
use serde_json::json;

/// A live v5 republic carrying the MOCK vault key keeps verifying, even
/// outside the vault bounds: the converse rule is for new foundings only.
#[test]
fn a_v5_genesis_with_the_mock_vault_feature_still_verifies() {
    for (members, m) in [(&["a", "b", "c", "d"][..], 2), (&["a", "b", "c"][..], 2)] {
        let b = Builder::new_with_features(members, m, &["vault"], false);
        let walk = walk_chain(&b.blocks).expect("the v5 mock verifies");
        assert!(walk.vault_ctx().is_none(), "a mock is not a vault");
    }
}

/// Historic `set_features vault` blocks keep verifying (D11 is a door
/// rule, never a block rule).
#[test]
fn a_chain_with_an_applied_mock_vault_set_features_still_verifies() {
    let mut b = Builder::new_with_features(&["a", "b", "c"], 2, &["memory"], false);
    b.commit_org(7, "set_features", "memory vault", &["a", "b"]);
    verify_chain(&b.blocks).expect("a historic set_features vault verifies");
}

/// What an older build sees: it drops the unknown `vault_pk`, recomputes
/// v5 bytes, and the founding signatures fail - the lockout rests on it.
#[test]
fn a_v6_genesis_does_not_verify_with_vault_pk_stripped() {
    let mut b = Builder::vault();
    verify_chain(&b.blocks).expect("the honest v6 genesis verifies");
    if let ChainChange::Genesis { identities, .. } = &mut b.blocks[0].change {
        for id in identities {
            id.vault_pk.clear();
        }
    }
    assert!(verify_chain(&b.blocks).is_err());
}

/// Every seat of a v6 genesis is keyed, even when every member signed.
#[test]
fn verify_genesis_rejects_a_v6_genesis_missing_a_key() {
    let mut b = Builder::vault();
    let ChainChange::Genesis { mut identities, .. } = b.blocks[0].change.clone() else {
        panic!("genesis");
    };
    identities[2].vault_pk.clear();
    b.reseal_genesis(identities);
    let err = verify_chain(&b.blocks).expect_err("a keyless seat in a v6 genesis");
    assert!(err.contains("vault key"), "{err}");
}

/// The other one-directional rules at the genesis: bounds when the vault
/// is chosen, canonical and unique keys - each over a fully re-signed table.
#[test]
fn verify_genesis_applies_the_v6_roster_rules() {
    let base = Builder::vault();
    let ChainChange::Genesis { identities, .. } = base.blocks[0].change.clone() else {
        panic!("genesis");
    };
    let mut dup = identities.clone();
    dup[1].vault_pk = dup[0].vault_pk.clone();
    let mut bad = identities;
    bad[1].vault_pk = "ff".repeat(32);
    for table in [dup, bad] {
        let mut b = base.clone();
        b.reseal_genesis(table);
        assert!(verify_chain(&b.blocks).is_err());
    }
    // E2: keys without the feature are the prepared vault, in bounds or not
    for (m, seats) in [(2, &["a", "b", "c", "d"][..]), (2, &["a", "b", "c"][..])] {
        let b = Builder::new_with_features(seats, m, &["memory"], true);
        verify_chain(&b.blocks).expect("a prepared vault verifies");
    }
    for m in [3, 1] {
        let b = Builder::new_with_features(&["a", "b", "c", "d"], m, &["vault"], true);
        let err = verify_chain(&b.blocks).expect_err("outside the bounds");
        assert!(err.contains("needs 2 <= m <= n-2"), "{err}");
    }
}

/// Plan 1.3.13: a Restored block re-keys the transport, never the vault
/// key - the walked roster keeps every founding `vault_pk`.
#[test]
fn a_membership_change_cannot_move_a_vault_pk() {
    let mut b = Builder::vault();
    let founding = verify_chain(&b.blocks).expect("genesis").identities;
    b.commit_restored("c", &molt_net::nostr_identity(&[9u8; 32], "r").1, &["a", "b"]);
    // a frame naming another key for the seat carries it nowhere
    let mut v = serde_json::to_value(&b.blocks[1].change).expect("json");
    v["vault_pk"] = json!("01".repeat(32));
    let decoded: ChainChange = serde_json::from_value(v).expect("decodes");
    assert_eq!(decoded, b.blocks[1].change, "no field carries a vault key");
    let walk = walk_chain(&b.blocks).expect("the restore verifies");
    assert_eq!(walk.head.identities, founding, "every vault key stays the founding one");
    assert_eq!(walk.vault_ctx(), crate::vault::ctx_from_founding(2, &founding).as_ref());
}

/// Plan 1.3.8: a walk takes the vault context of the chain it walks, never
/// the node's - a v5 holder walking a v6 candidate sees the v6 context,
/// and a v6 holder walking a v5 candidate sees none.
#[test]
fn the_walk_takes_the_vault_ctx_from_the_candidate_chain() {
    let v6 = Builder::vault();
    let v5 = Builder::new_with_features(&["a", "b", "c", "d"], 2, &["vault"], false);

    let holder5 = chain_peer("a", &v5, v5.blocks.clone());
    assert!(holder5.chain.head.is_some(), "the v5 holder adopted its chain");
    assert!(!holder5.is_vault_republic());
    let walk = holder5.walk_own(&v6.blocks).expect("the v6 candidate verifies");
    let ctx = walk.vault_ctx().expect("the candidate's own context").clone();
    assert_eq!(ctx.m, 2);
    assert_eq!(
        ctx.holders_in_genesis_order.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c", "d"]
    );
    assert!(ctx.holders_in_genesis_order.iter().all(|h| !h.2.is_empty()));

    let holder6 = chain_peer("a", &v6, v6.blocks.clone());
    assert!(holder6.is_vault_republic());
    assert_eq!(holder6.vault_ctx(), Some(ctx));
    let walk = holder6.walk_own(&v5.blocks).expect("the v5 candidate verifies");
    assert!(walk.vault_ctx().is_none());
}

/// A v6 republic and its legacy cut at height 2 (`upto` 1), honestly signed.
fn v6_with_a_legacy_cut() -> (Builder, molt_core::CheckpointState, ChainBlock) {
    let mut b = Builder::vault();
    b.commit_applied(1, &["a", "b"]);
    let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
    let cut = b.seal(
        2,
        ChainChange::Checkpoint { upto: 1, state_hash: checkpoint_state_hash(&blob) },
        &["a", "b"],
    );
    (b, blob, cut)
}

/// Only `CheckpointVault` binds `vault_pk` (checkpoint-v10), so a legacy cut
/// cannot carry the keys past the genesis: a v6 chain refuses both variants.
#[test]
fn a_v6_chain_refuses_a_legacy_checkpoint() {
    let (b, _, cut) = v6_with_a_legacy_cut();
    let mut chain = b.blocks.clone();
    chain.push(cut);
    let err = verify_chain(&chain).expect_err("a legacy cut in a v6 chain");
    assert!(err.contains("vault"), "{err}");

    let folded = ChainChange::CheckpointFolded { upto: 1, state_hash: "00".repeat(32) };
    let mut chain = b.blocks.clone();
    chain.push(b.seal(2, folded, &["a", "b"]));
    let err = verify_chain(&chain).expect_err("a folded legacy cut in a v6 chain");
    assert!(err.contains("vault"), "{err}");
}

/// No v8 anchor authenticates a `vault_pk`, so a blob carrying one never
/// bootstraps a suffix holder.
#[test]
fn a_suffix_walk_refuses_a_blob_with_vault_keys() {
    let (b, blob, cut) = v6_with_a_legacy_cut();
    let err = verify_suffix_chain(&blob, &[cut], &b.republic_id, None)
        .expect_err("a keyed blob under a v8 anchor");
    assert!(err.contains("vault"), "{err}");
}

/// The doors (S5): a vault republic proposes and co-signs only the vault
/// cut, whatever the wiki choice on the wire.
#[test]
fn a_vault_republic_proposes_and_signs_only_the_vault_cut() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::vault();
    b.commit_applied(1, &["a", "b"]);
    let mut a = chain_signer("a", &b, b.blocks.clone());
    assert!(a.is_vault_republic());
    a.cmd_propose_checkpoint().expect("the vault cut");
    assert!(a
        .chain
        .proposal_changes
        .values()
        .all(|c| matches!(c, ChainChange::CheckpointVault { wiki_folded: false, .. })));

    let mut c = chain_signer("c", &b, b.blocks.clone());
    // the legacy v8 hash is not what a vault seat attests
    let legacy = checkpoint_state_hash(&checkpoint_state(&b.blocks, 1).expect("state"));
    c.receive_checkpoint_proposal(50, 1, &legacy, false);
    assert!(c.chain.pending_sigs.is_empty(), "no co-signature on a legacy hash");
    let ours = c.own_cut_hash(1, c.cut_kind(false)).expect("own projection");
    c.receive_checkpoint_proposal(52, 1, &ours, false);
    assert!(matches!(c.chain.proposal_changes.get(&52), Some(ChainChange::CheckpointVault { .. })));
    assert!(c.chain.pending_sigs.contains_key(&52), "co-signed");
}

/// Review HIGH-1: v6 bytes of `features: None` equal those of `Some([])`,
/// so a keyed table without a feature set would let a shipper swap the
/// two under every signature. Every door refuses it.
#[test]
fn a_keyed_genesis_without_a_feature_set_is_refused() {
    let b = Builder::keyed_without_features(&["a", "b", "c", "d"], 2);
    let err = verify_chain(&b.blocks).expect_err("keys without a feature set");
    assert!(err.contains("feature set"), "{err}");
}
