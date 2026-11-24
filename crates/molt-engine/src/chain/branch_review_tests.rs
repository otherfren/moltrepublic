// SPDX-License-Identifier: GPL-3.0-or-later

//! The review findings of 2026-11-24 on the branch choice, one rule per
//! test (`docs/chain/chain_rescue_plan.md`).

use super::branch_run_tests::{bundle_of, chosen, hash, head_hash, seats};
use super::branch_tests::{fork, fork_v1};
use super::test_support::{consent_for, genesis_seat, Builder};
use super::*;
use molt_core::chain::DroppedRun;
use molt_core::{Command, MoltError, ProposalRecord};

fn arm() {
    super::branch::ARMED_HERE.with(|a| a.set(true));
}

fn run(b: &Builder, from: usize, to: usize) -> DroppedRun {
    let h = |i: usize| u64::try_from(i).expect("small");
    DroppedRun { from_height: h(from), first_hash: hash(b, from), head_height: h(to), head_hash: hash(b, to) }
}

fn sig_of(st: &State, id: u64, who: &str) -> String {
    st.chain.branch.file.cards[&id].sigs[who].clone()
}

fn last_kind(st: &State) -> &'static str {
    match st.chain.blocks.last().map(|b| &b.change) {
        Some(ChainChange::BranchChosen { .. }) => "branch",
        Some(ChainChange::Applied { .. }) => "applied",
        _ => "other",
    }
}

/// Finding 1: a block sealed on the kept branch while the card was open is
/// in nobody's digest - the card goes stale, nothing drops it.
#[test]
fn the_seal_drops_only_what_the_card_lists() {
    arm();
    let (mut x, _, mut a, mut b, c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("a holds y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    b.import_bundle(&bundle_of(&a)).expect("b: the card");
    b.cmd_approve_branch(id, &digest, false).expect("b approves");
    // x moves on while the card is open
    let x4 = x.commit(ChainChange::Applied { proposal_id: 210, surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note" }) }, &["a", "b"]);
    a.receive_block_from("b", x4);
    assert_eq!(a.chain.head.as_ref().map(|h| h.height), Some(4));
    let e = a.cmd_approve_branch(id, &digest, false).expect_err("stale");
    assert_eq!(e.to_string(), "branch: stale: the chain moved above the tip");
    a.receive_branch_sig(id, "b", &sig_of(&b, id, "b"));
    assert_eq!(last_kind(&a), "applied", "x's block 4 stays");
    assert!(a.branch_status_view().cards.iter().any(|c| c.id == id && c.stale));
    // a choice block sealed elsewhere does not drop it either
    let mut block = x.seal(4, a.chain.branch.file.cards[&id].change.clone(), &["a", "b"]);
    block.prev = hash(&x, 3);
    a.receive_block_from("c", block);
    assert_eq!(last_kind(&a), "applied");
    assert_eq!(a.chain.head.as_ref().map(|h| h.height), Some(4));
}

/// Finding 2: the record is the whole change - a second card with the
/// same tip but another dropped list is a second choice.
#[test]
fn the_shared_seat_records_the_whole_change() {
    arm();
    let (x, mut y) = fork();
    y.commit_applied(301, &["b", "c"]);
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    a.import_bundle(&bundle_of(&c)).expect("y");
    b.import_bundle(&bundle_of(&c)).expect("y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    // the same tip, y's run cut short: a different change
    let mut short = a.chain.branch.file.cards[&id].change.clone();
    if let ChainChange::BranchChosen { dropped, .. } = &mut short {
        dropped[0] = run(&y, 2, 2);
    }
    b.import_bundle(&bundle_of(&a)).expect("the card");
    b.receive_branch_card("a", 900, short.clone());
    b.cmd_approve_branch(id, &digest, false).expect("the first");
    let other = b.branch_status_view().cards.into_iter().find(|c| c.id == 900).expect("card 900");
    let e = b.cmd_approve_branch(900, &other.digest, false).expect_err("a second change");
    assert_eq!(e.to_string(), "branch: already chose at 1");
}

/// Finding 2: a second valid choice block at the height of one already
/// held never replaces it.
#[test]
fn a_second_choice_block_at_one_height_is_refused() {
    arm();
    let (x, mut y) = fork();
    y.commit_applied(301, &["b", "c"]);
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    a.import_bundle(&bundle_of(&c)).expect("y");
    let (id, _) = chosen(&mut a, 1, &hash(&x, 3));
    let first = a.chain.branch.file.cards[&id].change.clone();
    let mut second = first.clone();
    if let ChainChange::BranchChosen { dropped, .. } = &mut second {
        dropped[0] = run(&y, 2, 2);
    }
    let one = x.seal(4, first, &["a", "b"]);
    a.receive_block_from("b", one);
    let held = head_hash(&a);
    a.receive_block_from("b", x.seal(4, second, &["a", "b"]));
    assert_eq!(head_hash(&a), held, "no ping-pong");
}

/// Finding 3: a dropped run must start at the base. A card naming a base
/// above the real fork hides the other branch's lower blocks.
#[test]
fn a_dropped_run_must_link_to_the_base() {
    arm();
    let (x, mut y) = fork();
    y.commit_applied(301, &["b", "c"]);
    y.commit_applied(302, &["b", "c"]);
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    b.import_bundle(&bundle_of(&c)).expect("y");
    let forged = ChainChange::BranchChosen {
        base_height: 2,
        base_hash: hash(&x, 2),
        tip_height: 3,
        tip_hash: hash(&x, 3),
        chosen_root: hash(&x, 3),
        dropped: vec![run(&y, 3, 4)],
    };
    b.receive_branch_card("a", 900, forged);
    let card = b.branch_status_view().cards.into_iter().find(|c| c.id == 900);
    assert!(card.is_none_or(|c| c.digest.is_empty()), "nothing to sign");
    assert!(b.cmd_approve_branch(900, "00", false).is_err());
}

/// Finding 3: two honest cards at different fork points (the three-branch
/// shape) - the seat that signed one cannot sign the other.
#[test]
fn choices_at_two_fork_points_cannot_contradict() {
    arm();
    let mut base = Builder::new_with_features(&["a", "b", "c"], 2, &[molt_core::chain::BLOCK_V2_FEATURE], false);
    base.commit_applied(100, &["a", "b"]);
    let mut y = base.clone();
    y.commit_applied(300, &["b", "c"]);
    let mut x = base;
    x.commit_applied(200, &["a", "b"]);
    let mut z = x.clone();
    z.commit_applied(400, &["b", "c"]);
    x.commit_applied(201, &["a", "b"]);
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    b.import_bundle(&bundle_of(&genesis_seat("c", &y, y.blocks.clone()))).expect("y");
    b.import_bundle(&bundle_of(&genesis_seat("c", &z, z.blocks.clone()))).expect("z");
    let (keep_x3, d1) = chosen(&mut b, 2, &hash(&x, 3));
    let (keep_y, d2) = chosen(&mut b, 1, &hash(&y, 2));
    b.cmd_approve_branch(keep_x3, &d1, false).expect("x over z at 2");
    let e = b.cmd_approve_branch(keep_y, &d2, false).expect_err("drops what it kept");
    assert_eq!(e.to_string(), "branch: conflicts with the choice at 2");
}

/// Finding 4: an imported workspace counts as a new incarnation.
#[test]
fn an_imported_device_is_amnesiac() {
    arm();
    let (x, _, mut a, mut b, c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    b.load_branch_state(Ok(None), true);
    b.import_bundle(&bundle_of(&a)).expect("the card");
    assert_eq!(b.incarnation_height(), 3);
    let e = b.cmd_approve_branch(id, &digest, false).expect_err("imported");
    assert!(e.to_string().contains("amnesiac"), "{e}");
    b.cmd_approve_branch(id, &digest, true).expect("acknowledged");
}

/// Finding 5a: the majority rule holds at verification for this change.
#[test]
fn a_choice_needs_a_majority_threshold_to_verify() {
    arm();
    let mut base = Builder::new(&["a", "b", "c", "d", "e"], 2);
    base.commit_applied(100, &["a", "b"]);
    let mut y = base.clone();
    y.commit_applied(300, &["c", "d"]);
    let mut x = base;
    x.commit_applied(200, &["a", "b"]);
    let change = ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(&x, 1),
        tip_height: 2,
        tip_hash: hash(&x, 2),
        chosen_root: hash(&x, 2),
        dropped: vec![run(&y, 2, 2)],
    };
    let mut chain = x.blocks.clone();
    chain.push(x.seal(3, change, &["a", "b"]));
    let e = verify_chain(&chain).expect_err("2 of 5");
    assert!(e.contains("is not a majority"), "{e}");
    assert_eq!(refusal_rule(&e), "branch");
}

/// Finding 5b: below the last cut is refused for every holder, full or
/// pruned.
#[test]
fn a_full_holder_refuses_a_choice_below_a_cut() {
    arm();
    let (mut x, y) = fork();
    let blob = checkpoint_state(&x.blocks, 3).expect("state@3");
    let cut = x.seal(4, ChainChange::Checkpoint { upto: 3, state_hash: checkpoint_state_hash(&blob) }, &["a", "b"]);
    x.push(cut);
    let below = ChainChange::BranchChosen {
        base_height: 1,
        base_hash: hash(&x, 1),
        tip_height: 4,
        tip_hash: hash(&x, 4),
        chosen_root: hash(&x, 2),
        dropped: vec![run(&y, 2, 2)],
    };
    let mut full = x.blocks.clone();
    full.push(x.seal(5, below, &["a", "b"]));
    assert_eq!(refusal_rule(&verify_chain(&full).expect_err("below the cut")), "below_cut");
}

/// Finding 6: on a v1 chain a seat holding an attestation variant of the
/// chosen root is on the chosen branch, not left behind.
#[test]
fn an_attestation_variant_of_the_chosen_root_is_not_abandoned() {
    arm();
    let (x, y) = fork_v1();
    let mut variant = x.blocks[..2].to_vec();
    let mut root = x.blocks[2].clone();
    root.sigs = x.seal(2, root.change.clone(), &["b", "c"]).sigs;
    variant.push(root);
    let mut d = genesis_seat("c", &x, variant);
    let change = super::branch_tests::keep_x(&x, &y);
    let mut block = x.seal(4, change, &["a", "b"]);
    block.prev = hash(&x, 3);
    d.receive_block_from("a", block);
    assert!(!d.branch_abandoned(), "a variant is not the dropped branch");
}

/// Finding 7: the exemption ends once the seat shows up on the branch,
/// derived from the chain (it survives a restart), and covers a signer
/// from the dropped branch.
#[test]
fn the_restore_exemption_follows_the_branch_not_the_signature() {
    arm();
    let (mut x, _, mut a, mut b, mut c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("y");
    c.import_bundle(&bundle_of(&a)).expect("x");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    c.import_bundle(&bundle_of(&a)).expect("the card");
    c.cmd_approve_branch(id, &digest, false).expect("c, on y, chooses x");
    a.cmd_approve_branch(id, &digest, false).expect("a");
    a.receive_branch_sig(id, "c", &sig_of(&c, id, "c"));
    assert_eq!(last_kind(&a), "branch");
    let now = a.presence_now();
    a.recovery.frame_heard.insert("c".to_string(), now);
    assert_eq!(a.restore_gate("c"), None, "a signer on the dropped branch rejoins too");
    // c's signature on a later block: it is back, the exemption ends
    let choice = a.chain.blocks.last().cloned().expect("choice");
    x.push(choice);
    let later = x.commit(ChainChange::Applied { proposal_id: 500, surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note" }) }, &["a", "c"]);
    a.receive_block_from("b", later);
    assert!(!a.seat_left_by_choice("c"));
    let _ = &mut b;
}

/// Finding 8: the other branch's own v2 activation above the fork.
#[test]
fn evidence_follows_the_other_branchs_activation() {
    arm();
    let (x, mut y) = fork_v1();
    y.commit_org(310, "set_features", &format!("memory {}", molt_core::chain::BLOCK_V2_FEATURE), &["b", "c"]);
    y.commit_applied(311, &["b", "c"]);
    y.commit_applied(312, &["b", "c"]);
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    assert_eq!(a.import_bundle(&bundle_of(&c)).expect("y with its activation"), (1, 0));
}

/// Finding 9: the own tail's `Restored` of this seat drops with it - the
/// device is out; the dropped branch's decisions are a vote again on the
/// chosen branch's seats.
#[test]
fn dropping_an_own_restore_stops_the_device_and_reopens_the_other_cards() {
    arm();
    let (mut x, y) = fork();
    let npk = "ee".repeat(32);
    x.commit(
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "a".to_string(),
            identity_pk: x.pk("a"),
            nostr_pk: Some(npk.clone()),
            relays: Vec::new(),
            consent: Some(consent_for(&x, "a", &npk)),
        },
        &["b"],
    );
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    let c = genesis_seat("c", &y, y.blocks.clone());
    let mut card = ProposalRecord::new(Surface::Memory, serde_json::json!({ "op": "add_note", "id": 300 }), "c".to_string());
    card.voted = vec!["b".to_string(), "c".to_string()];
    card.decliners = vec!["a".to_string()];
    b.proposals.insert(300, card);
    b.import_bundle(&bundle_of(&c)).expect("y");
    let (id, digest) = chosen(&mut b, 1, &hash(&x, 3));
    a.import_bundle(&bundle_of(&b)).expect("card");
    b.cmd_approve_branch(id, &digest, false).expect("b");
    a.cmd_approve_branch(id, &digest, true).expect("a, restored above the fork");
    b.receive_branch_sig(id, "a", &sig_of(&a, id, "a"));
    assert_eq!(last_kind(&b), "branch");
    let p = &b.proposals[&300];
    assert!(p.voted.is_empty() && p.decliners.is_empty(), "y's decision is a vote again");
    a.receive_block_from("b", b.chain.blocks.last().cloned().expect("choice"));
    assert!(a.branch_abandoned(), "a's restore was dropped");
}

/// Finding 10 and the untested rules: every gate of an abandoned seat, it
/// takes no block, it stays abandoned across a restart, it posts nothing.
#[test]
fn an_abandoned_seat_refuses_everything_and_stays_so() {
    arm();
    let (x, _, mut a, _, mut c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    b.import_bundle(&bundle_of(&a)).expect("card");
    a.cmd_approve_branch(id, &digest, false).expect("a");
    b.cmd_approve_branch(id, &digest, false).expect("b");
    a.receive_branch_sig(id, "b", &sig_of(&b, id, "b"));
    let chat_before = c.chat.len();
    c.import_bundle(&bundle_of(&a)).expect("c stops");
    assert!(c.branch_abandoned());
    assert_eq!(c.chat.len(), chat_before, "no line to the group");
    let cmds = [
        Command::Propose { surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note" }) },
        Command::Approve { proposal: ProposalId(1), note: None },
        Command::Decline { proposal: ProposalId(1), note: None },
        Command::Withdraw { proposal: ProposalId(1), note: None },
        Command::ProposeCheckpoint,
        Command::RecoverInviteStart { member: "a".to_string() },
        Command::ChooseBranch { base_height: 1, tip_hash: hash(&x, 3), amnesia_ack: false },
        Command::ApproveBranch { id: ProposalId(id), dropped_digest: digest.clone(), amnesia_ack: false },
    ];
    for cmd in cmds {
        let e = c.handle(cmd).expect_err("abandoned");
        assert!(matches!(&e, MoltError::Branch(w) if w == "branch abandoned"), "{e}");
    }
    let head = head_hash(&c);
    c.receive_block_from("a", a.chain.blocks.last().cloned().expect("a block"));
    assert_eq!(head_hash(&c), head, "takes no block");
    let disk = c.chain.branch.disk.clone().expect("written");
    let mut again = genesis_seat("c", &x, c.chain.blocks.clone());
    again.load_branch_state(Ok(Some(disk)), false);
    assert!(again.branch_abandoned(), "across a restart");
}

/// The other untested rules: a card whose base is not held is ignored; a
/// signature that does not verify is not counted.
#[test]
fn cards_and_signatures_are_checked_on_arrival() {
    arm();
    let (x, y, mut a, mut b, c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("y");
    let (id, _) = chosen(&mut a, 1, &hash(&x, 3));
    b.import_bundle(&bundle_of(&a)).expect("card");
    b.receive_branch_sig(id, "c", &"00".repeat(64));
    assert!(b.chain.branch.file.cards[&id].sigs.is_empty(), "junk never counts");
    let mut foreign = super::branch_tests::keep_x(&x, &y);
    if let ChainChange::BranchChosen { base_hash, .. } = &mut foreign {
        *base_hash = "ab".repeat(32);
    }
    b.receive_branch_card("c", 901, foreign);
    assert!(!b.chain.branch.file.cards.contains_key(&901), "a base not held here");
}

/// `tie_break` keeps a choice at the tip against a lower-hash plain block.
#[test]
fn a_choice_at_the_tip_survives_a_lower_hash_contender() {
    arm();
    let (x, _, mut a, mut b, c) = seats();
    a.import_bundle(&bundle_of(&c)).expect("y");
    let (id, digest) = chosen(&mut a, 1, &hash(&x, 3));
    b.import_bundle(&bundle_of(&a)).expect("card");
    a.cmd_approve_branch(id, &digest, false).expect("a");
    b.cmd_approve_branch(id, &digest, false).expect("b");
    a.receive_branch_sig(id, "b", &sig_of(&b, id, "b"));
    let held = head_hash(&a);
    let contender = (600..700)
        .map(|pid| {
            let mut blk = x.seal(4, ChainChange::Applied { proposal_id: pid, surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note" }) }, &["a", "b"]);
            blk.prev = hash(&x, 3);
            blk
        })
        .find(|blk| a.block_id_of(blk) < held)
        .expect("a lower hash");
    a.receive_block_from("b", contender);
    assert_eq!(head_hash(&a), held);
}
