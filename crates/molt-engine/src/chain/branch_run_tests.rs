// SPDX-License-Identifier: GPL-3.0-or-later

//! The branch choice on live seats (`docs/chain/chain_rescue_plan.md`
//! §2-§8): evidence, bundle, vote, seal, the stop, the rejoin gate.

use super::branch_tests::fork;
use super::test_support::{consent_for, genesis_seat, Builder};
use super::*;
use molt_core::{ProposalRecord, Reply};

fn hash(b: &Builder, h: usize) -> String {
    block_id(&b.republic_id, &b.blocks[h], b.v2_from)
}

fn bundle_of(st: &State) -> String {
    serde_json::to_string(&st.branch_bundle()).expect("bundle")
}

/// a and b hold `x`, c holds `y`; b signed on both.
fn seats() -> (Builder, Builder, State, State, State) {
    let (x, y) = fork();
    let a = genesis_seat("a", &x, x.blocks.clone());
    let b = genesis_seat("b", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    (x, y, a, b, c)
}

fn chosen(st: &mut State, base: u64, tip: &str) -> (u64, String) {
    match st.cmd_choose_branch(base, tip, false).expect("choose") {
        Reply::BranchProposed { id, digest, .. } => (id.0, digest),
        other => panic!("unexpected {other:?}"),
    }
}

fn head_hash(st: &State) -> String {
    st.chain.head.as_ref().expect("a head").hash.clone()
}

#[test]
fn an_imported_bundle_proves_the_fork() {
    let (_, _, mut a, _, c) = seats();
    let e = a.cmd_choose_branch(1, &head_hash(&a), false).expect_err("no evidence yet");
    assert!(e.to_string().contains("no second branch"), "{e}");
    assert_eq!(a.import_bundle(&bundle_of(&c)).expect("imports"), (1, 0));
    let view = a.branch_status_view();
    assert!(view.majority);
    assert_eq!(view.forks.len(), 1);
    let fork = &view.forks[0];
    assert_eq!(fork.base_height, 1);
    assert_eq!(fork.branches.len(), 2);
    assert_eq!((fork.branches[0].own, fork.branches[0].blocks.len()), (true, 2));
    assert_eq!((fork.branches[1].own, fork.branches[1].blocks.len()), (false, 1));
    assert_eq!(fork.branches[1].counts.get("applied:memory"), Some(&1));
    assert_eq!(fork.shared, vec!["b".to_string()], "b signed on both");
}

#[test]
fn a_forged_bundle_is_refused_whole() {
    let (_, _, mut a, _, c) = seats();
    let mut bundle = c.branch_bundle();
    let last = bundle.runs[0].last_mut().expect("y's block");
    last.sigs[0].sig = "00".repeat(64);
    let json = serde_json::to_string(&bundle).expect("json");
    assert!(a.import_bundle(&json).expect_err("forged").contains("threshold"));
    assert!(a.chain.branch.file.evidence.is_empty(), "nothing kept");
    let mut foreign = c.branch_bundle();
    foreign.republic_id = "ff".repeat(32);
    assert!(a.import_bundle(&serde_json::to_string(&foreign).expect("json")).is_err());
}

#[test]
fn no_choice_where_m_is_not_a_majority() {
    let mut b = Builder::new(&["a", "b", "c", "d", "e"], 2);
    b.commit_applied(1, &["a", "b"]);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    let e = st.cmd_choose_branch(0, &head_hash(&st), false).expect_err("2 of 5");
    assert_eq!(e.to_string(), "branch: needs a majority of the members: 2 of 5 is not one");
    let mut b3 = Builder::new(&["a", "b", "c", "d", "e"], 3);
    b3.commit_applied(1, &["a", "b", "c"]);
    let mut st = genesis_seat("a", &b3, b3.blocks.clone());
    let e = st.cmd_choose_branch(0, &head_hash(&st), false).expect_err("no fork");
    assert!(e.to_string().contains("no second branch"), "3 of 5 passes the rule: {e}");
}

#[test]
fn a_choice_below_a_cut_is_refused() {
    let (mut x, y) = fork();
    let blob = checkpoint_state(&x.blocks, 3).expect("state@3");
    let cut = x.seal(4, ChainChange::Checkpoint { upto: 3, state_hash: checkpoint_state_hash(&blob) }, &["a", "b"]);
    x.push(cut);
    let mut a = genesis_seat("a", &x, Vec::new());
    a.set_checkpoint_blob(Some(blob));
    a.adopt_chain(vec![x.blocks[4].clone()]);
    assert_eq!(a.chain.head.as_ref().map(|h| h.height), Some(4));
    let e = a.cmd_choose_branch(1, &hash(&y, 2), false).expect_err("below the cut");
    assert_eq!(e.to_string(), "branch: below a cut");
}

/// The keystone at the seat level: a 2-of-3 split, a and b choose x up to
/// its first block, c stops, the dropped card is a vote again, c rejoins.
#[test]
fn a_split_is_repaired_by_two_seats_and_the_third_rejoins() {
    let (x, _y, mut a, mut b, mut c) = seats();
    for st in [&mut a, &mut b] {
        let mut card = ProposalRecord::new(Surface::Memory, serde_json::json!({ "op": "add_note", "id": 201 }), "a".to_string());
        card.settle_applied();
        card.voted = vec!["a".to_string(), "b".to_string()];
        st.proposals.insert(201, card);
    }
    a.import_bundle(&bundle_of(&c)).expect("a holds y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 2));
    // b learns the card and y from a's bundle - nothing is signed by itself
    b.import_bundle(&bundle_of(&a)).expect("b imports");
    assert!(b.chain.branch.file.cards.get(&id).is_some_and(|c| c.sigs.is_empty()));
    assert!(b.chain.branch.file.choices.is_empty(), "no automatism");
    a.cmd_approve_branch(id, &digest, false).expect("a approves");
    assert_eq!(a.chain.head.as_ref().map(|h| h.height), Some(3), "one signature does not seal");
    let e = b.cmd_approve_branch(id, "00", false).expect_err("a digest b never saw");
    assert_eq!(e.to_string(), "branch: dropped list differs");
    b.cmd_approve_branch(id, &digest, false).expect("b approves");
    let sig = b.chain.branch.file.cards[&id].sigs["b"].clone();
    a.receive_branch_sig(id, "b", &sig);
    // a sealed at 3 on x's first block; x's second block is dropped
    let head = a.chain.head.clone().expect("head");
    assert_eq!(head.height, 3);
    assert!(matches!(a.chain.blocks.last().map(|b| &b.change), Some(ChainChange::BranchChosen { .. })));
    let card = &a.proposals[&201];
    assert_eq!(card.state, ProposalState::Proposed, "back to the vote");
    assert!(card.voted.is_empty() && !a.chain.own_approvals.contains(&201), "stances cleared");
    // b adopts the sealed block (it holds the tip) and drops its own tail
    let block = a.chain.blocks.last().cloned().expect("the choice");
    b.receive_block_from("a", block);
    assert_eq!(head_hash(&b), head.hash);
    assert_eq!(b.proposals[&201].state, ProposalState::Proposed);
    // c, on y, stops
    let y_head = head_hash(&c);
    c.import_bundle(&bundle_of(&a)).expect("c imports");
    let stop = c.branch_status_view().abandoned.expect("abandoned");
    assert_eq!((stop.base_height, stop.by.clone()), (1, vec!["a".to_string(), "b".to_string()]));
    assert_eq!(c.chain_sync().abandoned, Some(stop));
    assert_eq!(head_hash(&c), y_head, "it never re-bases by itself");
    assert_eq!(c.sign_blocker(201).as_deref(), Some("branch abandoned"));
    assert!(c.session.notice.starts_with("branch-abandoned:1:"), "{}", c.session.notice);
    // a mints c's link although c is alive and diverged
    let now = a.presence_now();
    a.recovery.frame_heard.insert("c".to_string(), now);
    a.chain.diverged.insert(
        "c".to_string(),
        molt_core::ChainDivergence { peer: "c".to_string(), since_height: 2, seen_ts: a.presence_now() },
    );
    assert_eq!(a.restore_gate("c"), None, "the chain left c behind");
    assert!(a.seat_left_by_choice("c") && !a.seat_left_by_choice("b"));
    // the rejoin: a Restored block for c, then a fresh device of c
    let mut chain = a.chain.blocks.clone();
    let npk = "dd".repeat(32);
    let restore = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "c".to_string(),
        identity_pk: x.pk("c"),
        nostr_pk: Some(npk.clone()),
        relays: Vec::new(),
        consent: Some(consent_for(&x, "c", &npk)),
    };
    let mut block = x.seal(4, restore, &["a"]);
    block.prev = head.hash.clone();
    a.receive_block_from("b", block.clone());
    b.receive_block_from("a", block.clone());
    chain.push(block);
    let c2 = genesis_seat("c", &x, chain);
    assert_eq!(head_hash(&a), head_hash(&b));
    assert_eq!(head_hash(&c2), head_hash(&a), "one head on all three");
    assert!(!c2.branch_abandoned());
}

/// §2: the shared seat signs one choice per fork point - also across a
/// crash between the record and the send.
#[test]
fn the_shared_seat_signs_one_choice_per_fork_point() {
    let (x, y, mut a, mut b, mut c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("a holds y");
    c.import_bundle(&bundle_of(&a)).expect("c holds x");
    let (keep_x, dx) = chosen(&mut a, 1, &hash(&x, 3));
    let (keep_y, dy) = chosen(&mut c, 1, &hash(&y, 2));
    b.import_bundle(&bundle_of(&a)).expect("b: a's card");
    b.import_bundle(&bundle_of(&c)).expect("b: c's card");
    b.cmd_approve_branch(keep_x, &dx, false).expect("b chooses x");
    let e = b.cmd_approve_branch(keep_y, &dy, false).expect_err("a second choice");
    assert_eq!(e.to_string(), "branch: already chose at 1");
    // a kill between the durable record and the frame: the restarted seat
    // still knows its choice
    let disk = b.chain.branch.disk.clone().expect("written");
    let mut again = genesis_seat("b", &x, x.blocks.clone());
    again.load_branch_state(Ok(Some(disk)));
    again.import_bundle(&bundle_of(&c)).expect("c's card again");
    let e = again.cmd_approve_branch(keep_y, &dy, false).expect_err("after the restart");
    assert_eq!(e.to_string(), "branch: already chose at 1");
}

/// The record reaches the disk before the signature leaves: a failed
/// write sends nothing.
#[test]
fn no_signature_leaves_without_the_durable_record() {
    let (x, _, mut a, _, c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("a holds y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    a.chain.branch.fail_writes = true;
    RECORDED_CHAIN.with(|r| r.borrow_mut().clear());
    let e = a.cmd_approve_branch(id, &digest, false).expect_err("not saved");
    assert_eq!(e.to_string(), "branch: choice not saved");
    assert!(RECORDED_CHAIN.with(|r| r.borrow().is_empty()), "nothing recorded");
    assert!(a.chain.branch.file.choices.is_empty() && a.chain.branch.file.cards[&id].sigs.is_empty());
    a.chain.branch.fail_writes = false;
    a.cmd_approve_branch(id, &digest, false).expect("saved now");
    assert!(RECORDED_CHAIN.with(|r| r.borrow().contains(&("branch_approved", 4))));
}

/// The amnesia case (§2): a device restored above the fork point refuses
/// by default; the operator may override.
#[test]
fn a_device_restored_above_the_fork_refuses_by_default() {
    let (mut x, mut y) = fork();
    let npk = "ee".repeat(32);
    let restore = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "b".to_string(),
        identity_pk: x.pk("b"),
        nostr_pk: Some(npk.clone()),
        relays: Vec::new(),
        consent: Some(consent_for(&x, "b", &npk)),
    };
    x.commit(restore, &["a"]);
    y.commit_applied(301, &["b", "c"]);
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    assert_eq!(b.incarnation_height(), 4);
    a.import_bundle(&bundle_of(&c)).expect("a holds y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 4));
    b.import_bundle(&bundle_of(&a)).expect("b: the card");
    let e = b.cmd_approve_branch(id, &digest, false).expect_err("amnesiac");
    assert!(e.to_string().contains("amnesiac"), "{e}");
    b.cmd_approve_branch(id, &digest, true).expect("acknowledged");
}

/// §9: on a seat that holds the choice, a lower hash on the other branch
/// no longer wins the fork.
#[test]
fn the_choice_outranks_the_hash_tie_break() {
    let (x, y) = fork();
    // keep the branch whose block at the fork has the HIGHER hash, so the
    // hash rule alone would re-base onto the other one
    let ((keep, owner), (lose, loser)) =
        if hash(&y, 2) < hash(&x, 2) { ((x, "a"), (y, "c")) } else { ((y, "c"), (x, "a")) };
    let mut own = genesis_seat(owner, &keep, keep.blocks.clone());
    let mut b = genesis_seat("b", &keep, keep.blocks.clone());
    let other = genesis_seat(loser, &lose, lose.blocks.clone());
    own.import_bundle(&bundle_of(&other)).expect("the other branch");
    let tip = keep.blocks.len() - 1;
    let (id, digest) = chosen(&mut own, 1, &hash(&keep, tip));
    b.import_bundle(&bundle_of(&own)).expect("b: the card");
    own.cmd_approve_branch(id, &digest, false).expect("owner");
    b.cmd_approve_branch(id, &digest, false).expect("b");
    let sig = b.chain.branch.file.cards[&id].sigs["b"].clone();
    own.receive_branch_sig(id, "b", &sig);
    let head = head_hash(&own);
    assert_eq!(own.chain.head.as_ref().map(|h| h.height), Some(u64::try_from(tip + 1).expect("small")));
    for block in lose.blocks[2..].iter().cloned() {
        own.receive_block_from(loser, block);
    }
    assert_eq!(head_hash(&own), head, "the lower hash never displaces the choice");
    assert_eq!(own.chain.last_fork.as_ref().map(|f| f.outcome.as_str()), Some("chosen"));
}
