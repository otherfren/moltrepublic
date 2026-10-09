// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §14 step 6, end to end over a relay: the init vote is the
//! purse's only door; each seat judges the birthday against its own
//! daemon (an in-process stub), one decline kills the card, and a seat
//! without a daemon abstains until it has one.

use std::time::Duration;

use molt_core::wallet::{WalletPhase, WalletRefusal, WalletView};
use molt_core::{Command, MoltError, ProposalId, ProposalState, Reply, Surface};
use molt_engine::WalletHandle;
use molt_net::monero_rpc::stub::{StubConfig, StubDaemon};
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::found_n_at;

async fn set_daemon(w: &WalletHandle, d: &StubDaemon) {
    let patch = serde_json::json!({ "wallet_daemon_url": d.url, "wallet_daemon_confirmed": true });
    w.execute(Command::PatchSettings { patch }).await.expect("daemon set");
}

async fn wallet(w: &WalletHandle) -> (WalletView, Vec<molt_core::ProposalView>) {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(s)) => (*s.wallet.expect("wallet view"), s.pending.clone()),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wallet(w: &WalletHandle, what: &str, pred: impl Fn(&WalletView, &[molt_core::ProposalView]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let (v, pending) = wallet(w).await;
        if pred(&v, &pending) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn card_state(w: &WalletHandle, id: ProposalId) -> Option<ProposalState> {
    match w.execute(Command::LIST_ALL_PROPOSALS).await {
        Ok(Reply::Proposals { proposals, .. }) => proposals.iter().find(|p| p.id == id).map(|p| p.state),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_card(w: &WalletHandle, id: ProposalId, want: ProposalState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while card_state(w, id).await != Some(want) {
        assert!(tokio::time::Instant::now() < deadline, "card {} never reached {want:?}", id.0);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn init(w: &WalletHandle) -> ProposalId {
    match w.execute(Command::WalletInit).await {
        Ok(Reply::Proposed { id, .. }) => id,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn features(w: &WalletHandle) -> Vec<String> {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s.features,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn three(url: &str, tmp: &std::path::Path) -> Vec<WalletHandle> {
    found_n_at(tmp, url, 3, 2, &["memory"]).await
}

/// Plan §10.23 + §10.25b: the card is reachable while `wallet` is off, and
/// its block turns the feature on for every seat, the daemonless one too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallet_init_turns_the_feature_on() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let all = three(&relay.url().await.to_string(), tmp.path()).await;
    let (da, db) = (StubDaemon::start(5000, StubConfig::default()).await, StubDaemon::start(4995, StubConfig::default()).await);
    set_daemon(&all[0], &da).await;
    set_daemon(&all[1], &db).await;
    assert!(!features(&all[1]).await.iter().any(|f| f == "wallet"));
    let id = init(&all[0]).await;
    wait_wallet(&all[1], "the init card", |_, p| p.iter().any(|c| c.id == id)).await;
    assert!(features(&all[1]).await.iter().any(|f| f == "wallet"), "the nav shows the open card");
    all[1].execute(Command::Approve { proposal: id, note: None }).await.expect("approved while off");
    for w in &all {
        wait_wallet(w, "the init to apply", |v, _| v.phase == WalletPhase::Init).await;
        assert!(features(w).await.iter().any(|f| f == "wallet"));
    }
    match all[1].execute(Command::WalletInit).await {
        Err(MoltError::Wallet(WalletRefusal::InitExists)) => {}
        other => panic!("unexpected: {other:?}"),
    }
}

/// Plan §10.24: one decline kills the card on every seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_decline_kills_the_init_card() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let all = three(&relay.url().await.to_string(), tmp.path()).await;
    let d = StubDaemon::start(5000, StubConfig::default()).await;
    set_daemon(&all[0], &d).await;
    let id = init(&all[0]).await;
    wait_wallet(&all[1], "the init card", |_, p| p.iter().any(|c| c.id == id)).await;
    all[1].execute(Command::Decline { proposal: id, note: None }).await.expect("declined");
    for w in &all {
        wait_card(w, id, ProposalState::Rejected).await;
    }
    assert_eq!(wallet(&all[0]).await.0.phase, WalletPhase::NoPurse);
}

/// Plan §10.25: a birthday above an approver's daemon is declined there
/// and dies; a lagging daemon within the slack approves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_future_birthday_is_declined_but_a_lagging_daemon_is_not() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let all = three(&relay.url().await.to_string(), tmp.path()).await;
    let (da, db) = (StubDaemon::start(9000, StubConfig::default()).await, StubDaemon::start(5000, StubConfig::default()).await);
    set_daemon(&all[0], &da).await;
    set_daemon(&all[1], &db).await;
    let id = init(&all[0]).await;
    wait_card(&all[0], id, ProposalState::Rejected).await;

    da.set_height(5020);
    let id = init(&all[0]).await;
    wait_wallet(&all[1], "the second card", |_, p| p.iter().any(|c| c.id == id)).await;
    all[1].execute(Command::Approve { proposal: id, note: None }).await.expect("a lagging daemon approves");
    for w in &all {
        wait_wallet(w, "the init to apply", |v, _| v.phase == WalletPhase::Init).await;
    }
}

/// Plan §10.25a: no daemon - neither approve nor decline; once one is
/// set, the standing consent signs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_without_a_daemon_abstains() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let all = three(&relay.url().await.to_string(), tmp.path()).await;
    let (da, db) = (StubDaemon::start(5000, StubConfig::default()).await, StubDaemon::start(5001, StubConfig::default()).await);
    set_daemon(&all[0], &da).await;
    match all[1].execute(Command::WalletInit).await {
        Err(e @ MoltError::Wallet(WalletRefusal::NoDaemon)) => assert_eq!(e.to_string(), "purse: no daemon"),
        other => panic!("unexpected: {other:?}"),
    }
    let id = init(&all[0]).await;
    wait_wallet(&all[1], "the init card", |_, p| p.iter().any(|c| c.id == id)).await;
    match all[1].execute(Command::Approve { proposal: id, note: None }).await {
        Err(MoltError::Wallet(WalletRefusal::NoDaemon)) => {}
        other => panic!("unexpected: {other:?}"),
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(card_state(&all[0], id).await, Some(ProposalState::Proposed), "abstained, not declined");
    set_daemon(&all[1], &db).await;
    for w in &all {
        wait_wallet(w, "the consent to sign once a daemon answers", |v, _| v.phase == WalletPhase::Init).await;
    }
}
