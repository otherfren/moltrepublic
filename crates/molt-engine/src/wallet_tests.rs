// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse's door (plan §7.1, §7.2, §7.7): gates, the closed op set at
//! every door, the decline veto and the projection of the init.

use super::*;
use crate::chain::test_support::{genesis_seat, wire, Builder};
use molt_core::{ChainChange, ProposalState, WorkspaceEvent};

const ABC: [&str; 3] = ["a", "b", "c"];

fn init(birthday: u64, network: &str) -> Value {
    json!({ "op": WALLET_INIT, "birthday_height": birthday, "network": network })
}

fn commit_wallet(b: &mut Builder, id: u64, payload: Value) {
    b.commit(ChainChange::Applied { proposal_id: id, surface: Surface::Wallet, payload }, &["a", "b"]);
}

/// A seat whose daemon answered `height` just now.
fn with_daemon(st: &mut State, height: u64) {
    st.session.settings.wallet_daemon_url = "http://127.0.0.1:18081".to_string();
    st.purse.height = Some((height, crate::now_secs()));
}

fn refused<T: std::fmt::Debug>(r: Result<T, MoltError>, want: &WalletRefusal) {
    match r {
        Err(MoltError::Wallet(got)) if &got == want => {}
        other => panic!("expected `{want}`, got {other:?}"),
    }
}

/// A peer's init card lands on `st` as id `id`.
fn card_from(st: &mut State, by: &str, id: u64, payload: Value) {
    wire(
        st,
        by,
        id,
        WorkspaceEvent::Proposed { id: ProposalId(id), surface: Surface::Wallet, payload },
    );
}

/// Plan §10.20: W1 from the genesis, chain-governed, one init.
#[test]
fn wallet_needs_two_to_n_minus_one() {
    refused(crate::tests::plain_state().cmd_wallet_init(), &WalletRefusal::NotChain);
    for m in [1, 3] {
        let b = Builder::new(&ABC, m);
        let mut st = genesis_seat("a", &b, b.blocks.clone());
        refused(st.cmd_wallet_init(), &WalletRefusal::Bounds);
        assert_eq!(st.wallet_view().phase, WalletPhase::Bounds);
        with_daemon(&mut st, 3000);
        refused(st.wallet_approve_check(&init(2990, "mainnet")).map_err(MoltError::Wallet), &WalletRefusal::Bounds);
    }
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    refused(st.cmd_wallet_init(), &WalletRefusal::NoDaemon);
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    refused(st.cmd_wallet_init(), &WalletRefusal::InitPending);

    let mut b = Builder::new(&ABC, 2);
    commit_wallet(&mut b, 5, init(2990, "mainnet"));
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    refused(st.cmd_wallet_init(), &WalletRefusal::InitExists);
}

/// Plan §10.21: propose, the wire and approve refuse adding `wallet`; a
/// ride-along of an effective `wallet` passes.
#[test]
fn set_features_cannot_add_wallet() {
    let add = json!({ "op": "set_features", "value": "memory wallet" });
    let b = Builder::new_with_features(&ABC, 2, &["memory"], false);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    match st.cmd_propose(Surface::Organization, add.clone()) {
        Err(e @ MoltError::BadPayload(_)) => assert_eq!(e.to_string(), "bad payload: wallet: set up the purse"),
        other => panic!("unexpected: {other:?}"),
    }
    wire(&mut st, "b", 1, WorkspaceEvent::Proposed { id: ProposalId(7), surface: Surface::Organization, payload: add.clone() });
    assert!(!st.proposals.contains_key(&7), "the wire twin drops it");
    st.proposals.insert(
        8,
        molt_core::ProposalRecord {
            surface: Surface::Organization,
            payload: add.clone(),
            approvals: 0,
            state: ProposalState::Proposed,
            applied_at: 0,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: "b".to_string(),
            superseded: false,
            superseded_kind: None,
            withdrawn: false,
            wiki_rev: None,
        },
    );
    assert!(matches!(st.cmd_approve(ProposalId(8), None), Err(MoltError::BadPayload(_))));

    let mut b = Builder::new_with_features(&ABC, 2, &["memory"], false);
    commit_wallet(&mut b, 5, init(2990, "mainnet"));
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    st.cmd_propose(Surface::Organization, json!({ "op": "set_features", "value": "memory quests wallet" }))
        .expect("wallet rides along once effective");
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    st.cmd_propose(Surface::Organization, json!({ "op": "set_features", "value": "memory quests" }))
        .expect("and need not ride: the union keeps it");
    assert!(st.effective_features().iter().any(|f| f == "wallet"));
}

/// Plan §10.22: a legacy `wallet` feature is just a feature (W4, I14).
#[test]
fn a_memory_vote_in_a_legacy_wallet_republic_creates_no_purse() {
    let mut b = Builder::new_with_features(&ABC, 2, &["memory", "wallet"], false);
    b.commit_org(4, "set_features", "memory quests wallet", &["a", "b"]);
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert!(st.effective_features().iter().any(|f| f == "wallet"));
    assert_eq!(st.wallet_init_applied(), None);
    assert_eq!(st.wallet_view().phase, WalletPhase::NoPurse);
}

/// Plan §10.23: the first well-formed applied init turns `wallet` on;
/// later or malformed ones are ignored.
#[test]
fn wallet_init_turns_the_feature_on() {
    let mut b = Builder::new_with_features(&ABC, 2, &["memory"], false);
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert!(!st.feature_on(Surface::Wallet));
    commit_wallet(&mut b, 4, init(7, "moonnet"));
    commit_wallet(&mut b, 5, init(2990, "stagenet"));
    commit_wallet(&mut b, 6, init(10, "mainnet"));
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert!(st.effective_features().iter().any(|f| f == "wallet"));
    assert!(st.feature_on(Surface::Wallet));
    assert_eq!(
        st.wallet_init_applied(),
        Some(AppliedInit { id: Some(5), birthday: 2990, network: "stagenet".to_string() })
    );
    assert_eq!(st.wallet_view().phase, WalletPhase::Init);
}

/// Plan §10.24: in a 2-of-5 one decline is not the n-m+1 the others need;
/// on an init card it kills.
#[test]
fn one_decline_kills_the_init_card() {
    let five = ["a", "b", "c", "d", "e"];
    let b = Builder::new(&five, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    wire(&mut st, "c", 2, WorkspaceEvent::Declined { id: ProposalId(5), by: "c".to_string(), hash: String::new() });
    assert_eq!(st.proposals.get(&5).map(|p| p.state), Some(ProposalState::Rejected));
}

/// Plan §10.25: a birthday above the slack is declined (the card dies), a
/// lagging daemon within it is approved.
#[test]
fn a_future_birthday_is_declined_but_a_lagging_daemon_is_not() {
    assert!(birthday_ok(3030, 3000) && !birthday_ok(3031, 3000));
    assert!(birthday_ok(3000 - BIRTHDAY_WINDOW, 3000) && !birthday_ok(3000 - BIRTHDAY_WINDOW - 1, 3000));
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    card_from(&mut st, "b", 5, init(3100, "mainnet"));
    let p = st.proposals.get(&5).expect("card");
    assert_eq!(p.state, ProposalState::Rejected, "the seat declined on arrival");
    assert_eq!(p.decliners, vec!["a".to_string()]);

    card_from(&mut st, "c", 6, init(3020, "mainnet"));
    assert_eq!(st.proposals.get(&6).map(|p| p.state), Some(ProposalState::Proposed));
    st.cmd_approve(ProposalId(6), None).expect("a lagging daemon still approves");
    assert!(st.chain.own_approvals.contains(&6));

    card_from(&mut st, "c", 7, init(3000, "stagenet"));
    assert_eq!(st.proposals.get(&7).map(|p| p.state), Some(ProposalState::Rejected), "another network");
}

/// Plan §10.25a: no daemon - neither approve nor decline; the consent
/// signs once a height is known.
#[test]
fn a_seat_without_a_daemon_abstains() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(3100, "mainnet"));
    refused(st.cmd_approve(ProposalId(5), None), &WalletRefusal::Held(Box::new(WalletRefusal::NoDaemon)));
    let p = st.proposals.get(&5).expect("card");
    assert_eq!(p.state, ProposalState::Proposed);
    assert!(p.decliners.is_empty() && !st.chain.own_approvals.contains(&5));
    with_daemon(&mut st, 3090);
    st.wallet_review();
    assert!(st.chain.own_approvals.contains(&5), "the standing consent signs");
}

/// Plan §10.25b: the init card is approvable while `wallet` is off, and
/// the nav shows the purse while it is open.
#[test]
fn the_init_card_is_approvable_while_wallet_is_off() {
    let b = Builder::new_with_features(&ABC, 2, &["memory"], false);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    assert!(!st.feature_on(Surface::Wallet));
    with_daemon(&mut st, 3000);
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    assert!(st.feature_on(Surface::Wallet), "the open card shows the purse");
    assert!(!st.effective_features().iter().any(|f| f == "wallet"), "but enables nothing");
    st.cmd_approve(ProposalId(5), None).expect("the feature gate exempts the init");
    assert!(st.chain.own_approvals.contains(&5));

    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    st.propose_payload(Surface::Wallet, init(2990, "mainnet")).expect("the door opens while off");
}

/// Plan §10.36 (§3.6, I13): a legacy mock `transfer` block verifies and
/// folds as nothing; no door takes another Wallet op.
#[test]
fn a_mock_transfer_block_does_not_break_the_chain() {
    let mut b = Builder::new(&ABC, 2);
    commit_wallet(&mut b, 4, json!({ "op": "transfer", "title": "t" }));
    crate::chain::verify_chain(&b.blocks).expect("the mock block verifies");
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    assert_eq!(st.wallet_view().phase, WalletPhase::NoPurse);
    refused(st.cmd_propose(Surface::Wallet, json!({ "op": "transfer" })), &WalletRefusal::UseInit);
    refused(st.propose_payload(Surface::Wallet, json!({ "op": "transfer" })), &WalletRefusal::UnknownOp);
    card_from(&mut st, "b", 9, json!({ "op": "transfer" }));
    assert!(!st.proposals.contains_key(&9), "the wire drops a non-purse op");
    refused(st.wallet_approve_check(&json!({ "op": "transfer" })).map_err(MoltError::Wallet), &WalletRefusal::UnknownOp);
    refused(
        st.wallet_approve_check(&json!({ "op": WALLET_CREATED })).map_err(MoltError::Wallet),
        &WalletRefusal::UnknownOp,
    );

    commit_wallet(&mut b, 5, init(2990, "mainnet"));
    commit_wallet(&mut b, 6, json!({ "op": "transfer" }));
    crate::chain::verify_chain(&b.blocks).expect("still verifies");
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert_eq!(st.wallet_view().phase, WalletPhase::Init);
}

/// The signing path checks too: a re-sign never signs a card the approve
/// would refuse.
#[test]
fn the_signing_path_runs_the_wallet_check() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    st.chain_sign_and_gossip_approval(5);
    assert!(!st.chain.own_approvals.contains(&5), "no daemon, no signature");
}

/// A re-sign refused on a stale height is made once the daemon answers.
#[test]
fn a_stale_height_re_signs_once_the_daemon_answers() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    st.cmd_approve(ProposalId(5), None).expect("approved");
    st.purse.height = Some((3000, 0));
    st.chain.pending_sigs.remove(&5);
    st.chain_sign_and_gossip_approval(5);
    assert!(!st.own_signature_stands(5), "a stale height signs nothing");
    with_daemon(&mut st, 3000);
    st.wallet_review();
    assert!(st.own_signature_stands(5));
}

/// No daemon abstains on any network: the default network never declines.
#[test]
fn a_seat_without_a_daemon_abstains_on_another_network() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(3100, "stagenet"));
    let p = st.proposals.get(&5).expect("card");
    assert_eq!(p.state, ProposalState::Proposed);
    assert!(p.decliners.is_empty(), "abstained, not declined");
    refused(st.cmd_approve(ProposalId(5), None), &WalletRefusal::Held(Box::new(WalletRefusal::NoDaemon)));
}

/// An approve kept as consent says so.
#[test]
fn a_held_approve_says_it_is_held() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(3100, "mainnet"));
    match st.cmd_approve(ProposalId(5), None) {
        Err(e) => assert_eq!(e.to_string(), "purse: no daemon - approval held"),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(st.purse.consent.contains(&5));
}

/// A malformed init never lands: it would lock the door and nothing declines it.
#[test]
fn a_malformed_init_is_dropped_at_the_wire() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    card_from(&mut st, "b", 5, init(3100, "regtest"));
    card_from(&mut st, "b", 6, json!({ "op": WALLET_INIT, "network": "mainnet" }));
    assert!(!st.proposals.contains_key(&5) && !st.proposals.contains_key(&6));
    assert!(!st.wallet_init_pending());
}

/// An approved card whose birthday fell out of the window is declined, not left open.
#[test]
fn an_aged_out_init_is_declined_even_after_approving() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    card_from(&mut st, "b", 5, init(2990, "mainnet"));
    st.cmd_approve(ProposalId(5), None).expect("approved");
    with_daemon(&mut st, 2990 + BIRTHDAY_WINDOW + 1);
    st.wallet_review();
    assert_eq!(st.proposals.get(&5).map(|p| p.state), Some(ProposalState::Rejected));
    assert!(!st.wallet_init_pending(), "the door opens again");
}

/// A daemon change while the init probe runs cancels it: the old height never proposes.
#[test]
fn a_daemon_change_cancels_a_waiting_init() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    st.purse.probe_gen = 1;
    st.purse.init_gen = Some(1);
    st.wallet_daemon_changed();
    refused(st.cmd_net_wallet_probe(Some(3000), String::new(), Some(1)), &WalletRefusal::Cancelled);
    assert!(!st.wallet_init_pending(), "nothing proposed");
    assert!(st.purse.init_gen.is_none());
}

/// Closing the workspace drops the waiting init and the held consents.
#[test]
fn a_close_drops_the_waiting_init_and_consents() {
    let b = Builder::new(&ABC, 2);
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut st, 3000);
    st.purse.probe_gen = 1;
    st.purse.init_gen = Some(1);
    st.purse.probing = true;
    st.purse.consent.insert(5);
    st.reset_workspace_state();
    assert!(st.purse.consent.is_empty() && st.purse.init_gen.is_none());

    let mut other = genesis_seat("a", &b, b.blocks.clone());
    with_daemon(&mut other, 3000);
    other.purse = std::mem::take(&mut st.purse);
    refused(other.cmd_net_wallet_probe(Some(3000), String::new(), Some(1)), &WalletRefusal::Cancelled);
    assert!(!other.wallet_init_pending(), "nothing proposed into the next republic");
    assert!(!other.purse.probing && other.purse.height.is_some(), "the height still lands");
}
