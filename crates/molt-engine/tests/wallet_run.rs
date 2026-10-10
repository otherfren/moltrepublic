// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §14 step 7a, end to end over a relay: a run after the init
//! (readiness, consent, both rounds over control frames), persist then
//! attest, the self-proving `wallet_created` sealed at m, and the restart.

use std::time::Duration;

use molt_core::wallet::{RunStage, WalletPhase, WalletRunView, WalletView};
use molt_core::{Command, Event, Reply, Surface};
use molt_engine::WalletHandle;
use molt_net::monero_rpc::stub::{StubConfig, StubDaemon};
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{found_n_at, read_session, NAMES};

/// Run deadlines in these tests: long enough for a loaded box, short
/// enough for an abort test.
const DEADLINE: u64 = 30;

async fn set_daemon(w: &WalletHandle, d: &StubDaemon) {
    let patch = serde_json::json!({ "wallet_daemon_url": d.url, "wallet_daemon_confirmed": true, "wallet_network": "mainnet" });
    w.execute(Command::PatchSettings { patch }).await.expect("daemon set");
}

async fn wallet(w: &WalletHandle) -> WalletView {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(s)) => *s.wallet.expect("wallet view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wallet(w: &WalletHandle, what: &str, pred: impl Fn(&WalletView) -> bool) -> WalletView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let v = wallet(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\nlast view: {v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn stage(v: &WalletView) -> Option<RunStage> {
    v.run.as_ref().map(|r| r.stage)
}

fn run(v: &WalletView) -> WalletRunView {
    v.run.clone().expect("a run")
}

/// The seats' names in founding order (`read_session` lists the roster so).
async fn founding_order(w: &WalletHandle) -> Vec<String> {
    let s = read_session(w).await;
    let entry = s.workspaces.iter().find(|e| e.id == s.active_workspace).expect("entry");
    entry.members.iter().map(|m| m.name.clone()).collect()
}

/// Three seats in founding order (`all[i]` is position i+1, named
/// `names[i]`), the first `daemons` with a daemon, the init applied:
/// position 1 proposes, position 2 approves, position 3 has not consented.
async fn three_with_init(
    url: &str,
    tmp: &std::path::Path,
    daemons: usize,
) -> (Vec<WalletHandle>, Vec<String>, Vec<StubDaemon>) {
    let mut found = found_n_at(tmp, url, 3, 2, &["memory"]).await;
    let names = founding_order(&found[0]).await;
    let all: Vec<WalletHandle> = names
        .iter()
        .map(|n| found[NAMES.iter().position(|x| x == n).expect("seat")].clone())
        .collect();
    found.clear();
    let mut ds = Vec::new();
    for w in all.iter().take(daemons) {
        w.__wallet_deadline(DEADLINE);
        let d = StubDaemon::start(5000, StubConfig::default()).await;
        set_daemon(w, &d).await;
        ds.push(d);
    }
    for w in all.iter().skip(daemons) {
        w.__wallet_deadline(DEADLINE);
    }
    let id = match all[0].execute(Command::WalletInit).await {
        Ok(Reply::Proposed { id, .. }) => id,
        other => panic!("unexpected: {other:?}"),
    };
    approve(&all[1], id).await;
    for w in &all {
        wait_wallet(w, "the init to apply", |v| v.phase != WalletPhase::NoPurse).await;
    }
    (all, names, ds)
}

/// Close every seat and open its workspace from disk.
async fn reopen_all(root: &std::path::Path, all: &[WalletHandle], names: &[String]) -> Vec<molt_storage::OpenedWorkspace> {
    let mut out = Vec::new();
    for (w, name) in all.iter().zip(names) {
        let id = read_session(w).await.active_workspace.clone();
        w.execute(Command::CloseWorkspace).await.expect("close");
        let dir = molt_storage::find_workspace_dir(&root.join(name), &id).expect("dir");
        out.push(molt_storage::open_workspace(&dir).expect("open").0);
    }
    out
}

/// Approve the init once it arrives; a daemon still being asked holds
/// the approval and signs when it answers.
async fn approve(w: &WalletHandle, id: molt_core::ProposalId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        match w.execute(Command::Approve { proposal: id, note: None }).await {
            Ok(_) | Err(molt_core::MoltError::Wallet(molt_core::wallet::WalletRefusal::Held(_))) => return,
            Err(e) => assert!(tokio::time::Instant::now() < deadline, "approve: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Until `w`'s current run is `nonce`.
async fn wait_run(w: &WalletHandle, nonce: [u8; 32]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while w.__wallet_run() != Some(nonce) {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for run {}", hex::encode(&nonce[..4]));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn consent(w: &WalletHandle) {
    wait_wallet(w, "the consent prompt", |v| v.run.as_ref().is_some_and(|r| r.needs_consent)).await;
    w.execute(Command::WalletConsent { accept: true }).await.expect("consent");
}

async fn purse_block(w: &WalletHandle) -> serde_json::Value {
    match w.execute(Command::LIST_ALL_PROPOSALS).await.expect("proposals") {
        Reply::Proposals { proposals, .. } => proposals
            .iter()
            .find(|p| p.surface == Surface::Wallet && p.payload["op"] == "wallet_created" && p.state == molt_core::ProposalState::Applied)
            .map(|p| p.payload.clone())
            .expect("an applied wallet_created"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Plan §10.28: every seat computes the same purse, its keys record is on
/// disk, and the sealed record carries exactly the attestation each seat
/// persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_seats_found_one_purse() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    let mut events = all[2].subscribe();
    consent(&all[2]).await;
    let mut address = String::new();
    for w in &all {
        let v = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await;
        assert!(!v.address.is_empty());
        if address.is_empty() {
            address = v.address.clone();
        }
        assert_eq!(v.address, address, "one purse, one address");
        assert!(v.address.starts_with('4'), "a standard main address");
        assert_eq!(stage(&v), Some(RunStage::Done));
    }
    let mut saw = (false, false);
    while let Ok(ev) = events.try_recv() {
        match ev {
            Event::WalletRunProgress { .. } => saw.0 = true,
            Event::WalletCreated { address: a } => saw.1 = a == address,
            _ => {}
        }
    }
    assert_eq!(saw, (true, true), "progress and the purse reach the frontends");
    let block = purse_block(&all[0]).await;
    for (i, ws) in reopen_all(tmp.path(), &all, &names).await.iter().enumerate() {
        let records = ws.read_wallet_keys().expect("keys file");
        assert_eq!(records.len(), 1, "{}: exactly the purse's record", names[i]);
        let rec = molt_treasury::keys::KeysRecord::decode(&records[0]).expect("record");
        assert_eq!(rec.address, address);
        assert_eq!(block["attestations"][i], hex::encode(rec.attestation), "the attestation that left is the persisted one");
    }
}

/// Plan §10.26: a seat without a daemon never announces readiness, even
/// with consent; the run aborts at the deadline and names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_without_a_daemon_aborts_at_readiness_and_is_named() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 2).await;
    consent(&all[2]).await;
    for w in &all[..2] {
        let v = wait_wallet(w, "the abort", |v| stage(v) == Some(RunStage::Aborted)).await;
        let r = run(&v);
        assert_eq!(r.reason.as_deref(), Some("not ready"));
        assert_eq!(r.missing, vec![names[2].clone()], "the daemonless seat is named");
    }
}

/// Plan §10.27 (W6): a declined consent aborts the run everywhere; a retry
/// is a new run that can complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declined_consent_aborts_the_run_and_retry_starts_a_new_nonce() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    wait_wallet(&all[2], "the consent prompt", |v| v.run.as_ref().is_some_and(|r| r.needs_consent)).await;
    all[2].execute(Command::WalletConsent { accept: false }).await.expect("decline");
    for w in &all {
        let v = wait_wallet(w, "the abort", |v| stage(v) == Some(RunStage::Aborted)).await;
        assert_eq!(run(&v).reason.as_deref(), Some("declined"));
        assert_eq!(run(&v).missing, vec![names[2].clone()]);
    }
    all[1].execute(Command::WalletRetry).await.expect("retry");
    consent(&all[2]).await;
    for w in &all {
        wait_wallet(w, "the purse after the retry", |v| v.phase == WalletPhase::Ready).await;
    }
}

/// Plan §10.37: a reopen restarts nothing; a run that never persisted is
/// aborted by the seat that lost it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_does_not_restart_a_run() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    wait_wallet(&all[0], "the run", |v| stage(v) == Some(RunStage::Ready)).await;
    // its start and readiness reached the others before it closes
    for w in &all[1..] {
        wait_wallet(w, "seat 1 ready", |v| {
            v.run.as_ref().is_some_and(|r| r.stage == RunStage::Ready && !r.missing.contains(&names[0]))
        })
        .await;
    }
    let id = read_session(&all[0]).await.active_workspace.clone();
    all[0].execute(Command::CloseWorkspace).await.expect("close");
    all[0].execute(Command::OpenWorkspace { id }).await.expect("reopen");
    let v = wallet(&all[0]).await;
    assert_eq!(v.phase, WalletPhase::Init);
    assert!(v.run.is_none(), "no run after a reopen");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(wallet(&all[0]).await.run.is_none(), "and none starts by itself");
    // the others go on; their first round frame reaches a seat that lost the run
    consent(&all[2]).await;
    for w in &all[1..] {
        let v = wait_wallet(w, "the lost run's abort", |v| stage(v) == Some(RunStage::Aborted)).await;
        assert_eq!(run(&v).reason.as_deref(), Some("restart"));
    }
    assert!(wallet(&all[0]).await.run.is_none());
}

/// Plan §10.38a: starts racing in readiness converge on the lowest
/// starter position, even against a lower nonce.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_starts_converge_on_one_run() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    // position 1 the highest nonce, position 2 the lowest
    all[0].__wallet_start_with([5; 32]);
    all[1].__wallet_start_with([0; 32]);
    all[2].__wallet_start_with([1; 32]);
    for w in &all {
        wait_run(w, [5; 32]).await;
    }
    consent(&all[2]).await;
    for w in &all {
        let v = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await;
        let shown: Vec<String> = v.shareholders.iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(shown, names, "the founding order");
    }
    let block = purse_block(&all[0]).await;
    assert_eq!(block["run"], hex::encode([5u8; 32]), "position 1 wins over a lower nonce");
}

/// Plan §10.38a: a start reusing an aborted run's nonce is refused, also
/// from a seat that forgot it (reopened).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reused_nonce_is_refused() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    // c declines a's auto-start, then b's run 9, then a's run 8
    for start in [None, Some((1, [9; 32])), Some((0, [8; 32]))] {
        if let Some((k, nonce)) = start {
            all[k].__wallet_start_with(nonce);
            for w in &all {
                wait_run(w, nonce).await;
            }
        }
        wait_wallet(&all[2], "the prompt", |v| v.run.as_ref().is_some_and(|r| r.needs_consent)).await;
        all[2].execute(Command::WalletConsent { accept: false }).await.expect("decline");
        for w in &all {
            wait_wallet(w, "the abort", |v| stage(v) == Some(RunStage::Aborted)).await;
        }
    }
    let id = read_session(&all[0]).await.active_workspace.clone();
    all[0].execute(Command::CloseWorkspace).await.expect("close");
    all[0].execute(Command::OpenWorkspace { id }).await.expect("reopen");
    all[0].__wallet_start_with([9; 32]);
    wait_run(&all[0], [9; 32]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for w in &all[1..] {
        assert_eq!(w.__wallet_run(), Some([8; 32]), "the reused nonce is not joined");
        assert_eq!(stage(&wallet(w).await), Some(RunStage::Aborted));
    }
}

/// Plan §10.38 (I16): a withheld attestation keeps the purse from
/// sealing; the next run runs beside the kept record, and when the
/// withheld attestations arrive, whichever run wins, every seat holds its share.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_attestation_cannot_seal_a_run_without_shares() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, names, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    all[2].__wallet_withhold_attestation(true);
    consent(&all[2]).await;
    for w in &all {
        let v = wait_wallet(w, "every seat persisted", |v| stage(v) == Some(RunStage::Attest)).await;
        assert_eq!(v.phase, WalletPhase::Init);
    }
    tokio::time::sleep(Duration::from_secs(DEADLINE / 2)).await;
    for w in &all {
        assert_eq!(wallet(w).await.phase, WalletPhase::Init, "no purse without n attestations");
    }
    let first = wallet(&all[0]).await;
    assert_eq!(run(&first).done, 2);
    // a second run while the first is stuck: c still withholds
    all[0].execute(Command::WalletRetry).await.expect("retry beside a kept record");
    for w in &all {
        wait_wallet(w, "the second run persisted", |v| stage(v) == Some(RunStage::Attest)).await;
    }
    all[2].__wallet_withhold_attestation(false);
    for w in &all {
        wait_wallet(w, "a purse", |v| v.phase == WalletPhase::Ready).await;
    }
    let block = purse_block(&all[0]).await;
    for ws in &reopen_all(tmp.path(), &all, &names).await {
        let records = ws.read_wallet_keys().expect("keys");
        assert_eq!(records.len(), 1, "the losing run's record is gone");
        let rec = molt_treasury::keys::KeysRecord::decode(&records[0]).expect("record");
        assert_eq!(block["run"], hex::encode(rec.run.run), "each seat keeps the winner's share");
    }
}

/// Plan §10.39: a seat that persisted re-attests and co-signs after a
/// restart; the run is never aborted by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_after_persist_re_attests_and_cosigns() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let (all, _, _ds) = three_with_init(&relay.url().await.to_string(), tmp.path(), 3).await;
    all[2].__wallet_withhold_attestation(true);
    consent(&all[2]).await;
    for w in &all {
        wait_wallet(w, "every seat persisted", |v| stage(v) == Some(RunStage::Attest)).await;
    }
    let id = read_session(&all[2]).await.active_workspace.clone();
    all[2].execute(Command::CloseWorkspace).await.expect("close");
    all[2].__wallet_withhold_attestation(false);
    all[2].execute(Command::OpenWorkspace { id }).await.expect("reopen");
    for w in &all {
        wait_wallet(w, "the purse after the restart", |v| v.phase == WalletPhase::Ready).await;
    }
    let a = wallet(&all[0]).await.address;
    for w in &all[1..] {
        assert_eq!(wallet(w).await.address, a);
    }
}
