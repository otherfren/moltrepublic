// SPDX-License-Identifier: GPL-3.0-or-later

//! The branch choice (`docs/chain/chain_rescue_plan.md`): the block's
//! verification, the evidence, the vote, its application.

use super::test_support::Builder;
use super::*;
use molt_core::chain::DroppedRun;

/// A 2-of-3 fork at height 1 of a current founding (block identity v2
/// from height 1): `x` (a + b) holds two blocks above it, `y` (b + c) one.
pub(super) fn fork() -> (Builder, Builder) {
    fork_from(Builder::new_with_features(&["a", "b", "c"], 2, &[molt_core::chain::BLOCK_V2_FEATURE], false))
}

/// [`fork`] of an older founding (v1 identities throughout).
pub(super) fn fork_v1() -> (Builder, Builder) {
    fork_from(Builder::new(&["a", "b", "c"], 2))
}

pub(super) fn fork_from(mut base: Builder) -> (Builder, Builder) {
    base.commit_applied(100, &["a", "b"]);
    let mut x = base.clone();
    x.commit_applied(200, &["a", "b"]);
    x.commit_applied(201, &["a", "b"]);
    let mut y = base;
    y.commit_applied(300, &["b", "c"]);
    (x, y)
}

fn hash(b: &Builder, h: usize) -> String {
    block_id(&b.republic_id, &b.blocks[h], b.v2_from)
}

/// `x` kept up to its head, `y` dropped from the fork.
pub(super) fn keep_x(x: &Builder, y: &Builder) -> ChainChange {
    ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(x, 1),
        tip_height: 3,
        tip_hash: hash(x, 3),
        chosen_root: hash(x, 2),
        dropped: vec![DroppedRun { from_height: 2, first_hash: hash(y, 2), head_height: 2, head_hash: hash(y, 2) }],
    }
}

fn with(x: &Builder, change: ChainChange, signers: &[&str]) -> Vec<ChainBlock> {
    let mut chain = x.blocks.clone();
    chain.push(x.seal(4, change, signers));
    chain
}

/// `x`'s first `len` blocks plus `change` sealed on top of them.
pub(super) fn on_prefix(x: &Builder, len: usize, change: ChainChange, signers: &[&str]) -> Vec<ChainBlock> {
    let mut chain = x.blocks[..len].to_vec();
    chain.push(x.seal_on(u64::try_from(len).expect("small"), &hash(x, len - 1), change, signers));
    chain
}

fn edit(change: &ChainChange, f: impl FnOnce(&mut ChainChange)) -> ChainChange {
    let mut c = change.clone();
    f(&mut c);
    c
}

#[test]
fn a_branch_choice_seals_on_its_tip() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    for (x, y) in [fork(), fork_v1()] {
        let head = verify_chain(&with(&x, keep_x(&x, &y), &["a", "b"])).expect("verifies");
        assert_eq!(head.height, 4);
    }
}

#[test]
fn a_branch_choice_needs_the_threshold() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, y) = fork();
    let e = verify_chain(&with(&x, keep_x(&x, &y), &["a"])).expect_err("one signer");
    assert_eq!(super::refusal_rule(&e), "threshold");
}

/// Signatures do not cover `prev`: the tip hash is what binds the vote to
/// one branch, so the same signed block refuses on another one.
#[test]
fn a_choice_replayed_on_another_branch_is_refused() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, mut y) = fork();
    y.commit_applied(301, &["b", "c"]);
    let block = x.seal(4, keep_x(&x, &y), &["a", "b"]);
    let mut replay = block.clone();
    replay.prev = y.head_hash.clone();
    let mut chain = y.blocks.clone();
    chain.push(replay);
    let e = verify_chain(&chain).expect_err("the tip is x's");
    assert_eq!(super::refusal_rule(&e), "branch", "{e}");
}

#[test]
fn a_choice_moved_to_another_base_does_not_verify() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, y) = fork();
    // re-signed at base 0 with block 1's hash: every block-local rule holds,
    // only the walk sees that block 0 is something else
    let moved = edit(&keep_x(&x, &y), |c| {
        if let ChainChange::BranchChosen { base_height, chosen_root, dropped, .. } = c {
            *base_height = 0;
            *chosen_root = hash(&x, 1);
            dropped[0].from_height = 1;
            dropped[0].first_hash = hash(&y, 2);
        }
    });
    let e = verify_chain(&with(&x, moved, &["a", "b"])).expect_err("re-based");
    assert!(e.contains("base 0 is not on this chain"), "{e}");
}

#[test]
fn the_named_blocks_must_be_on_the_chain() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, y) = fork();
    let good = keep_x(&x, &y);
    let cases = [
        (edit(&good, |c| if let ChainChange::BranchChosen { base_hash, .. } = c { *base_hash = "ab".repeat(32) }), "base 1 is not on this chain"),
        (edit(&good, |c| if let ChainChange::BranchChosen { chosen_root, .. } = c { *chosen_root = "ab".repeat(32) }), "root at 2 is not on this chain"),
        (edit(&good, |c| if let ChainChange::BranchChosen { tip_hash, .. } = c { *tip_hash = "ab".repeat(32) }), "not its predecessor"),
        (edit(&good, |c| if let ChainChange::BranchChosen { tip_height, .. } = c { *tip_height = 2 }), "minus one"),
        (edit(&good, |c| if let ChainChange::BranchChosen { base_height, .. } = c { *base_height = 4 }), "lies above its tip"),
    ];
    for (change, want) in cases {
        let e = verify_chain(&with(&x, change, &["a", "b"])).expect_err(want);
        assert!(e.contains(want), "{e} / {want}");
        assert_eq!(super::refusal_rule(&e), "branch", "{e}");
    }
}

/// Q1: a threshold may not reset its own history without a second branch.
#[test]
fn no_choice_without_a_second_branch() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, y) = fork();
    let own_tail = DroppedRun { from_height: 3, first_hash: hash(&x, 3), head_height: 3, head_hash: hash(&x, 3) };
    let tail_only = ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(&x, 1),
        tip_height: 2,
        tip_hash: hash(&x, 2),
        chosen_root: hash(&x, 2),
        dropped: vec![own_tail.clone()],
    };
    let chain = on_prefix(&x, 3, tail_only.clone(), &["a", "b"]);
    let e = verify_chain(&chain).expect_err("no other branch");
    assert!(e.contains("no second branch"), "{e}");
    // the same choice with y's root dropped too verifies
    let mut both = tail_only;
    if let ChainChange::BranchChosen { dropped, .. } = &mut both {
        dropped.push(DroppedRun { from_height: 2, first_hash: hash(&y, 2), head_height: 2, head_hash: hash(&y, 2) });
    }
    verify_chain(&on_prefix(&x, 3, both, &["a", "b"])).expect("a second branch is named");
    // its own root is not a second branch
    let own_root = edit(&keep_x(&x, &y), |c| {
        if let ChainChange::BranchChosen { dropped, .. } = c {
            dropped[0].first_hash = hash(&x, 2);
        }
    });
    assert!(verify_chain(&with(&x, own_root, &["a", "b"])).expect_err("own root").contains("its own root"));
}

/// The common block itself may be the tip: both branches drop.
#[test]
fn the_base_itself_may_be_chosen() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (x, y) = fork();
    let at_base = ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(&x, 1),
        tip_height: 1,
        tip_hash: hash(&x, 1),
        chosen_root: String::new(),
        dropped: vec![
            DroppedRun { from_height: 2, first_hash: hash(&x, 2), head_height: 3, head_hash: hash(&x, 3) },
            DroppedRun { from_height: 2, first_hash: hash(&y, 2), head_height: 2, head_hash: hash(&y, 2) },
        ],
    };
    assert_eq!(verify_chain(&on_prefix(&x, 2, at_base.clone(), &["b", "c"])).expect("verifies").height, 2);
    let one_root = edit(&at_base, |c| {
        if let ChainChange::BranchChosen { dropped, .. } = c {
            dropped.pop();
        }
    });
    assert!(verify_chain(&on_prefix(&x, 2, one_root, &["b", "c"])).expect_err("one root").contains("no second branch"));
}

/// §3: never below a cut - a suffix holder cannot verify the base.
#[test]
fn a_choice_below_the_anchor_is_refused() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
    let (mut x, y) = fork();
    let blob = checkpoint_state(&x.blocks, 3).expect("state@3");
    let anchor = x.seal(4, ChainChange::Checkpoint { upto: 3, state_hash: checkpoint_state_hash(&blob) }, &["a", "b"]);
    x.push(anchor);
    let below = ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(&x, 1),
        tip_height: 4,
        tip_hash: hash(&x, 4),
        chosen_root: hash(&x, 2),
        dropped: vec![DroppedRun { from_height: 2, first_hash: hash(&y, 2), head_height: 2, head_hash: hash(&y, 2) }],
    };
    let block = x.seal(5, below, &["a", "b"]);
    let suffix = vec![x.blocks[4].clone(), block.clone()];
    let e = verify_suffix_chain(&blob, &suffix, &x.republic_id, None).expect_err("below the cut");
    assert_eq!(super::refusal_rule(&e), "below_cut", "{e}");
    // a full holder refuses it alike: the rule is the last cut's
    let mut full = x.blocks.clone();
    full.push(block);
    assert_eq!(super::refusal_rule(&verify_chain(&full).expect_err("below the cut")), "below_cut");
}
