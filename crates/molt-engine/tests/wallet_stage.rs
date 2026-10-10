// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §14 step 7b, end to end over a relay: the purse stage that
//! follows a founding with `wallet` in its charter (auto-init and
//! auto-approve), a later enable, and the purse after a recovery or a
//! backup restore (status frames, the view key's ask and answer).

use std::time::Duration;

use molt_core::wallet::{RunStage, ShareStatus, WalletPhase, WalletView};
use molt_core::{Command, Event, Reply, Surface, VoteState};
use molt_engine::WalletHandle;
use molt_net::monero_rpc::stub::{StubConfig, StubDaemon};
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{adopt_relay, engine, found_n_prepared, read_session, wait_for, NAMES};

const DEADLINE: u64 = 30;

fn daemon_patch(d: &StubDaemon) -> serde_json::Value {
    serde_json::json!({ "wallet_daemon_url": d.url, "wallet_daemon_confirmed": true, "wallet_network": "mainnet" })
}

async fn set_daemon(w: &WalletHandle, d: &StubDaemon) {
    w.execute(Command::PatchSettings { patch: daemon_patch(d) }).await.expect("daemon set");
}

async fn wallet(w: &WalletHandle) -> WalletView {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(s)) => *s.wallet.expect("wallet view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wallet(w: &WalletHandle, what: &str, pred: impl Fn(&WalletView) -> bool) -> WalletView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
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

fn status_of(v: &WalletView, seat: &str) -> ShareStatus {
    v.shareholders.iter().find(|(m, _)| m == seat).map_or(ShareStatus::Unknown, |(_, s)| *s)
}

async fn wallet_cards(w: &WalletHandle) -> Vec<molt_core::ProposalView> {
    match w.execute(Command::LIST_ALL_PROPOSALS).await.expect("proposals") {
        Reply::Proposals { proposals, .. } => proposals.into_iter().filter(|p| p.surface == Surface::Wallet).collect(),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Three seats, 2-of-3, `features` in the charter; seat `NAMES[i]` gets a
/// daemon before the founding where `daemons[i]`. The handles come back
/// with their names, in `NAMES` order.
async fn founded(
    url: &str,
    tmp: &std::path::Path,
    features: &[&str],
    daemons: [bool; 3],
) -> (Vec<WalletHandle>, Vec<StubDaemon>) {
    let mut ds = Vec::new();
    let mut patches = Vec::new();
    for with in daemons {
        if with {
            let d = StubDaemon::start(5000, StubConfig::default()).await;
            patches.push(daemon_patch(&d));
            ds.push(d);
        } else {
            patches.push(serde_json::Value::Null);
        }
    }
    let all = found_n_prepared(tmp, url, 3, 2, features, &patches).await;
    for w in &all {
        w.__wallet_deadline(DEADLINE);
    }
    (all, ds)
}

async fn reveal_seed(w: &WalletHandle) -> String {
    let id = read_session(w).await.active_workspace.clone();
    match w.execute(Command::RevealSeed { id }).await.expect("reveal") {
        Reply::Seed { seed } => seed,
        other => panic!("unexpected: {other:?}"),
    }
}

/// `all[2]` ("c") loses its device and comes back from its phrase alone on
/// a fresh one; the old handle is dropped.
async fn recover_c(all: &mut Vec<WalletHandle>, url: &str, tmp: &std::path::Path) -> WalletHandle {
    let phrase = reveal_seed(&all[2]).await;
    let c = all.pop().expect("c");
    c.execute(Command::CloseWorkspace).await.expect("c goes away");
    drop(c);
    all[0].execute(Command::RecoverInviteStart { member: "c".to_string() }).await.expect("mint");
    let s = wait_for(&all[0], "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s.notice.strip_prefix("recovery-link:").unwrap_or_else(|| panic!("mint: {:?}", s.notice)).to_string();
    let c = engine(&tmp.join("c-new"));
    adopt_relay(&c, url).await;
    c.__wallet_deadline(DEADLINE);
    c.execute(Command::RecoverStart { link, phrase }).await.expect("recover start");
    let s = wait_for(&c, "the recovery to open", |s| {
        (s.screen == molt_core::Screen::Main && s.notice.starts_with("recovered:")) || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery: {:?}", s.notice);
    c
}

/// Plan §10.29 (W9): wallet in the charter, a daemon on every seat: the
/// purse comes up with no command at all - the lowest position proposes,
/// every seat that ratified approves and counts as consenting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founding_with_wallet_runs_the_purse_stage() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, _ds) = founded(&url, tmp.path(), &["memory", "wallet"], [true; 3]).await;
    let mut events = all[2].subscribe();
    let mut address = String::new();
    for w in &all {
        let v = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await;
        assert!(v.founding, "the wizard ends with the purse stage");
        assert!(v.can_watch);
        assert_eq!(stage(&v), Some(RunStage::Done));
        if address.is_empty() {
            address.clone_from(&v.address);
        }
        assert_eq!(v.address, address, "one purse");
    }
    let cards = wallet_cards(&all[0]).await;
    let inits: Vec<_> = cards.iter().filter(|p| p.payload["op"] == "wallet_init").collect();
    assert_eq!(inits.len(), 1, "one init, from one position");
    let mut progress = false;
    while let Ok(ev) = events.try_recv() {
        progress |= matches!(ev, Event::WalletRunProgress { .. });
    }
    assert!(progress, "the stage reaches the frontends");
}

/// Plan §7.3: the lowest position with a daemon proposes; position 1
/// without one leaves it to position 2, one step after the stage armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_position_proposes_when_the_first_has_no_daemon() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let armed = tokio::time::Instant::now();
    let (all, _ds) = founded(&url, tmp.path(), &["memory", "wallet"], [false, true, true]).await;
    let step = Duration::from_secs(DEADLINE / 4);
    let early = armed.elapsed();
    if early + Duration::from_secs(1) < step {
        assert!(wallet_cards(&all[0]).await.is_empty(), "nothing before position 2's wait");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let card = loop {
        if let Some(p) = wallet_cards(&all[0]).await.into_iter().find(|p| p.payload["op"] == "wallet_init") {
            break p;
        }
        assert!(tokio::time::Instant::now() < deadline, "no init from position 2");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(card.by, "b", "position 2 proposes");
    assert!(armed.elapsed() >= step, "after its wait");
    tokio::time::sleep(step).await;
    let inits = wallet_cards(&all[0]).await.into_iter().filter(|p| p.payload["op"] == "wallet_init").count();
    assert_eq!(inits, 1, "position 3 sees it and stays quiet");
}

/// Plan §10.30 (I15): a seat without a daemon stops the stage; the
/// republic stays founded and the purse comes once it has one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_founding_stage_leaves_the_republic_founded() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, _ds) = founded(&url, tmp.path(), &["memory", "wallet"], [true, true, false]).await;
    for w in &all[..2] {
        let v = wait_wallet(w, "the abort", |v| stage(v) == Some(RunStage::Aborted)).await;
        let run = v.run.expect("run");
        assert_eq!(run.reason.as_deref(), Some("not ready"));
        assert_eq!(run.missing, vec!["c".to_string()], "the daemonless seat is named");
        assert_eq!(v.phase, WalletPhase::Init);
    }
    for w in &all {
        let s = read_session(w).await;
        assert_eq!(s.screen, molt_core::Screen::Main, "founded and entered");
        assert!(!s.active_workspace.is_empty());
    }
    let d = StubDaemon::start(5000, StubConfig::default()).await;
    set_daemon(&all[2], &d).await;
    all[0].execute(Command::WalletRetry).await.expect("try again");
    for w in &all {
        wait_wallet(w, "the purse after the retry", |v| v.phase == WalletPhase::Ready).await;
    }
}

/// Plan §10.31 (W10): no wallet in the charter, no purse by itself; a
/// later init runs the stage on every seat, the seat that never voted is
/// asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_enable_runs_the_stage_on_every_seat() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, _ds) = founded(&url, tmp.path(), &["memory"], [true; 3]).await;
    let mut events: Vec<_> = all.iter().map(WalletHandle::subscribe).collect();
    tokio::time::sleep(Duration::from_secs(DEADLINE / 4 + 2)).await;
    for w in &all {
        let v = wallet(w).await;
        assert!(!v.founding);
        assert_eq!(v.phase, WalletPhase::NoPurse, "nothing without the wallet in the charter");
    }
    assert!(wallet_cards(&all[0]).await.is_empty());
    let id = match all[0].execute(Command::WalletInit).await {
        Ok(Reply::Proposed { id, .. }) => id,
        other => panic!("unexpected: {other:?}"),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while all[1].execute(Command::Approve { proposal: id, note: None }).await.is_err() {
        assert!(tokio::time::Instant::now() < deadline, "the init never became approvable");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for w in &all {
        wait_wallet(w, "the stage", |v| stage(v) == Some(RunStage::Ready)).await;
    }
    let v = wait_wallet(&all[2], "the prompt", |v| v.run.as_ref().is_some_and(|r| r.needs_consent)).await;
    assert!(!v.founding);
    all[2].execute(Command::WalletConsent { accept: true }).await.expect("consent");
    for (w, ev) in all.iter().zip(events.iter_mut()) {
        wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await;
        let mut progress = false;
        while let Ok(e) = ev.try_recv() {
            progress |= matches!(e, Event::WalletRunProgress { .. });
        }
        assert!(progress, "every seat sees the stage");
    }
}

/// Plan §10.38b: a recovered seat never proposes the init and never
/// approves it for the charter it ratified on its lost device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_never_auto_proposes_an_init() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (mut all, _) = founded(&url, tmp.path(), &["memory", "wallet"], [false; 3]).await;
    let c = recover_c(&mut all, &url, tmp.path()).await;
    let dc = StubDaemon::start(5000, StubConfig::default()).await;
    set_daemon(&c, &dc).await;
    tokio::time::sleep(Duration::from_secs(3 * (DEADLINE / 4) + 2)).await;
    assert!(wallet_cards(&c).await.is_empty(), "no init from a recovered seat");
    assert!(!wallet(&c).await.founding);
    let da = StubDaemon::start(5000, StubConfig::default()).await;
    set_daemon(&all[0], &da).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let id = loop {
        if let Some(p) = wallet_cards(&c).await.into_iter().find(|p| p.payload["op"] == "wallet_init") {
            break p.id;
        }
        assert!(tokio::time::Instant::now() < deadline, "a's init never reached c");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    let card = wallet_cards(&c).await.into_iter().find(|p| p.id == id).expect("card");
    assert!(!card.approved_by_me, "a recovered seat consents only by its own act");
    let votes: Vec<(String, VoteState)> = card.votes.iter().map(|v| (v.member.clone(), v.vote)).collect();
    assert!(votes.contains(&("c".to_string(), VoteState::Open)), "{votes:?}");
}

/// Plan §10.40 (design §5): a phrase-only recovery holds no key part; it
/// asks for the view key, checks it against the address and keeps it;
/// every seat shows it as view only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phrase_only_recovery_is_watch_only_and_gets_the_view_key() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (mut all, _ds) = founded(&url, tmp.path(), &["memory", "wallet"], [true; 3]).await;
    let mut address = String::new();
    for w in &all {
        address = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await.address;
    }
    for w in &all {
        wait_wallet(w, "every key part seen", |v| NAMES[..3].iter().all(|n| status_of(v, n) == ShareStatus::Held)).await;
    }
    let c = recover_c(&mut all, &url, tmp.path()).await;
    let v = wait_wallet(&c, "the view key", |v| v.can_watch).await;
    assert_eq!(status_of(&v, "c"), ShareStatus::WatchOnly, "no key part here");
    assert_eq!(v.phase, WalletPhase::Ready);
    assert_eq!(v.address, address, "the same purse");
    assert!(!v.founding, "a recovery is no founding stage");
    for w in &all {
        let v = wait_wallet(w, "c view only", |v| status_of(v, "c") == ShareStatus::WatchOnly).await;
        assert_eq!(status_of(&v, "a"), ShareStatus::Held);
        assert_eq!(status_of(&v, "b"), ShareStatus::Held);
    }
    let ws = read_session(&c).await.active_workspace.clone();
    c.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&tmp.path().join("c-new"), &ws).expect("dir");
    let (opened, _) = molt_storage::open_workspace(&dir).expect("open");
    assert!(opened.read_wallet_keys().expect("keys").is_empty(), "no key part here");
    assert!(opened.read_transport_state().wallet_view.is_some(), "the view key is kept");
    drop(opened);
    for w in &all {
        w.execute(Command::CloseWorkspace).await.expect("nobody left to answer");
    }
    c.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");
    let v = wallet(&c).await;
    assert!(v.can_watch, "the view key is kept across a reopen");
    assert_eq!(status_of(&v, "c"), ShareStatus::WatchOnly);
}

/// Plan §10.41 (design §6): a backup restored on a fresh device carries
/// the keys file; its record matches the purse, so the seat holds its part.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backup_restore_keeps_the_share() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, _ds) = founded(&url, tmp.path(), &["memory", "wallet"], [true; 3]).await;
    let mut address = String::new();
    for w in &all {
        address = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await.address;
    }
    let ws = read_session(&all[1]).await.active_workspace.clone();
    let blob = tmp.path().join("b.molt.enc");
    let pass = "correct horse battery";
    all[1]
        .execute(Command::ExportWorkspace { id: ws, dest: blob.display().to_string(), passphrase: pass.to_string() })
        .await
        .expect("export kickoff");
    let s = wait_for(&all[1], "the export", |s| !s.export.running && !s.export.result.is_empty()).await;
    assert_eq!(s.export.result, "ok");
    all[1].execute(Command::CloseWorkspace).await.expect("close");
    let fresh = engine(&tmp.path().join("b-fresh"));
    fresh
        .execute(Command::RestoreStart {
            way: "file".to_string(),
            target: blob.display().to_string(),
            secret: pass.to_string(),
            replace: false,
        })
        .await
        .expect("restore start");
    let s = wait_for(&fresh, "the restore", |s| s.restore.run.outcome != 0).await;
    assert_eq!(s.restore.run.outcome, 1, "{:?}", s.restore.run.log);
    fresh.execute(Command::RestoreFinish).await.expect("restore finish");
    let v = wait_wallet(&fresh, "the restored purse", |v| v.phase == WalletPhase::Ready).await;
    assert_eq!(v.address, address);
    assert_eq!(status_of(&v, "b"), ShareStatus::Held, "the backup keeps the key part");
    assert!(v.can_watch);
    assert!(!v.founding);
    assert!(!read_session(&fresh).await.notice.contains("view only"));
}
