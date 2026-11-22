// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **B7b - the sealing lock over a real relay**
//! (`docs/chain/chain_lock_design.md`): three seats of a 2-of-3 republic,
//! every one running the lock, end on one head with no fork decision;
//! a stopped leader, a restarted seat, a cut, a consented restore, an
//! older-build seat and a capability that flips mid-run.

mod common;

use common::{adopt_relay, hard_kill, read_session, wait_for};

/// A node that runs the lock whatever the build default.
fn engine(dir: &std::path::Path) -> WalletHandle {
    let w = common::engine(dir);
    w.__lock_cap_off(false);
    w
}
use std::time::Duration;

use molt_core::{Command, ProposalState, Reply, StatusView};
use molt_engine::WalletHandle;

fn add_page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

/// A storing relay without the notes-per-minute limit.
async fn relay() -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::builder::{RateLimit, RelayBuilder};
    use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = nostr_relay_builder::LocalRelay::new(
        RelayBuilder::default().database(db).rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 }),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// Approve every open foreign card this seat sees.
async fn approve_open(w: &WalletHandle) -> usize {
    let Reply::Proposals { proposals, .. } = w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") else {
        panic!("list_proposals answers Reply::Proposals");
    };
    let mut n = 0;
    for p in proposals {
        if p.state == ProposalState::Proposed
            && !p.approved_by_me
            && w.execute(Command::Approve { proposal: p.id, note: None }).await.is_ok()
        {
            n += 1;
        }
    }
    n
}

async fn chain(w: &WalletHandle) -> Vec<(u64, u64, Vec<String>)> {
    let Reply::Chain { blocks, .. } = w.execute(Command::ReadChain).await.expect("read chain") else {
        panic!("read_chain answers Reply::Chain");
    };
    blocks.iter().map(|b| (b.height, b.proposal_id, b.signers.clone())).collect()
}

async fn status(w: &WalletHandle) -> StatusView {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// A real 2-of-3 republic over `url`: mara founds, walter and dora join.
/// The sealing line by name: dora, mara, walter. Returns the seats and
/// dora's phrase.
async fn found_trio(root: &std::path::Path, url: &str) -> ([WalletHandle; 3], String) {
    found_trio_with(root, url, &[]).await
}

/// [`found_trio`] where the seats named in `old` run an older build.
async fn found_trio_with(root: &std::path::Path, url: &str, old: &[&str]) -> ([WalletHandle; 3], String) {
    let a = engine(&root.join("mara"));
    a.__lock_cap_off(old.contains(&"mara"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Drei".to_string(),
        member: "mara".to_string(),
        members: 3,
        threshold: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "two seat links", |s| {
        s.create.seats.len() >= 2
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["walter", "dora"].iter().zip(links) {
        let w = engine(&root.join(name));
        w.__lock_cap_off(old.contains(name));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    let dora_phrase = wait_for(&joiners[1], "dora's phrase", |s| !s.join.seed.is_empty()).await.join.seed.clone();
    wait_for(&a, "both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Drei".to_string(), agenda: String::new(), features: vec!["memory".to_string()] })
        .await
        .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the joiner to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    }
    let mut it = joiners.into_iter();
    ([a, it.next().expect("walter"), it.next().expect("dora")], dora_phrase)
}

/// Wait until every seat runs the lock at its head+1.
async fn lock_on(seats: &[&WalletHandle], on: bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    for w in seats {
        loop {
            if status(w).await.chain_sync.lock_active == on {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the lock never turned {on}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Propose `pages` per seat, approve everything, until every seat holds
/// `want` applied blocks on one head. Returns the summed fork decisions.
async fn burst(seats: &[&WalletHandle], names: &[&str], tag: &str, pages: usize, want: usize, secs: u64) -> u64 {
    for i in 0..pages {
        for (w, name) in seats.iter().zip(names) {
            w.execute(Command::Propose {
                surface: molt_core::Surface::Memory,
                payload: add_page(&format!("{name}/{tag}{i}.md"), name),
            })
            .await
            .expect("proposes");
        }
    }
    converge(seats, want, secs).await
}

async fn converge(seats: &[&WalletHandle], want: usize, secs: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        for w in seats {
            approve_open(w).await;
        }
        let mut chains = Vec::new();
        let mut heads = Vec::new();
        for w in seats {
            chains.push(chain(w).await);
            heads.push(status(w).await.chain_sync.head_hash);
        }
        let applied = |c: &Vec<(u64, u64, Vec<String>)>| c.iter().filter(|(_, pid, _)| *pid != 0).count();
        if chains.iter().all(|c| applied(c) >= want) && heads.iter().all(|h| *h == heads[0]) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no common head: {chains:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut forks = 0;
    for w in seats {
        forks += status(w).await.chain_sync.fork_decisions;
    }
    forks
}

/// The keystone: three capable seats, bursts, every tenth inbound frame
/// lost. One head hash, ZERO fork decisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_seats_under_the_lock_with_bursts_and_lost_frames_never_fork() {
    trace();
    let started = std::time::Instant::now();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (seats, _) = found_trio(tmp.path(), &url).await;
    let all: Vec<&WalletHandle> = seats.iter().collect();
    lock_on(&all, true).await;
    for w in &seats {
        w.__drop_inbound_every(10);
        w.__lock_timing(0, 5);
    }
    let names = ["mara", "walter", "dora"];
    let mut forks = 0;
    for b in 0..2 {
        forks = burst(&all, &names, &format!("b{b}p"), 3, (b + 1) * 9, 420).await;
    }
    let dropped: u64 = seats.iter().map(WalletHandle::__dropped_inbound).sum();
    eprintln!("lock fork_decisions={forks} dropped={dropped} secs={}", started.elapsed().as_secs());
    assert!(dropped > 0, "the network was lossy");
    assert_eq!(forks, 0, "no fork under the lock");
}

/// The leader (dora) stops right after its `Accept` left; the others
/// accepted. The SAME proposal seals without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_leader_is_completed_with_the_same_change() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], _) = found_trio(tmp.path(), &url).await;
    lock_on(&[&mara, &walter, &dora], true).await;
    for w in [&mara, &walter, &dora] {
        w.__lock_timing(5, 3);
    }
    dora.__lock_stop_after_accept();
    let Reply::Proposed { id, .. } = mara
        .execute(Command::Propose { surface: molt_core::Surface::Memory, payload: add_page(&format!("{}.md", "one"), "one") })
        .await
        .expect("proposes")
    else {
        panic!("proposed");
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !dora.__lock_stopped() {
        approve_open(&walter).await;
        assert!(tokio::time::Instant::now() < deadline, "dora never led the slot");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(dora);
    let mut sealed = None;
    while sealed.is_none() {
        let c = chain(&walter).await;
        sealed = c.iter().find(|(_, pid, _)| *pid != 0).map(|(h, pid, _)| (*h, *pid));
        assert!(tokio::time::Instant::now() < deadline, "the slot never sealed without its leader");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(sealed, Some((1, id.0)), "the same change at the same height");
    let forks = converge(&[&mara, &walter], 1, 60).await;
    assert_eq!(forks, 0);
}

/// A seat hard-killed mid-height keeps its promise across the restart
/// (the file holds it) and the republic goes on on one head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_seat_keeps_its_promise() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], _) = found_trio(tmp.path(), &url).await;
    lock_on(&[&mara, &walter, &dora], true).await;
    let names = ["mara", "walter", "dora"];
    burst(&[&mara, &walter, &dora], &names, "x", 1, 3, 180).await;
    // dora opened the next slot at once; walter has promised there
    let next = status(&walter).await.chain_sync.lock_ballot;
    assert!(next.ends_with(":dora"), "dora leads head+1: {next:?}");
    let ws = read_session(&walter).await.active_workspace.clone();
    let dir = hard_kill(walter, &tmp.path().join("walter"), &ws).await;
    let (opened, _) = molt_storage::open_workspace(&dir).expect("open");
    let lock = opened.read_chain_lock().expect("read").expect("the lock file exists");
    let lock = String::from_utf8(lock).expect("json");
    assert!(lock.contains(r#""leader":"dora""#), "the promise is on disk: {lock}");
    drop(opened);
    let walter = engine(&tmp.path().join("walter"));
    adopt_relay(&walter, &url).await;
    walter.execute(Command::OpenWorkspace { id: ws }).await.expect("walter reopens");
    let forks = burst(&[&mara, &walter, &dora], &names, "y", 1, 6, 240).await;
    assert_eq!(forks, 0, "no fork across the restart");
}

/// A cut seals n-of-n under the lock and every seat prunes to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_seals_under_the_lock() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], _) = found_trio(tmp.path(), &url).await;
    lock_on(&[&mara, &walter, &dora], true).await;
    let names = ["mara", "walter", "dora"];
    burst(&[&mara, &walter, &dora], &names, "c", 1, 3, 180).await;
    dora.execute(Command::ProposeCheckpoint).await.expect("cut");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    for w in [&mara, &walter, &dora] {
        loop {
            let c = chain(w).await;
            if c.first().is_some_and(|(h, _, signers)| *h == 4 && signers.len() == 3) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the cut never sealed: {c:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    let forks = burst(&[&mara, &walter, &dora], &names, "d", 1, 3, 180).await;
    assert_eq!(forks, 0);
}

/// One seat on an older build (no capability, lock frames dropped): the
/// others see it and run B6 - every seat still ends on one head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_older_seat_means_b6_and_still_one_head() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], _) = found_trio_with(tmp.path(), &url, &["walter"]).await;
    lock_on(&[&mara, &walter, &dora], false).await;
    let names = ["mara", "walter", "dora"];
    let forks = burst(&[&mara, &walter, &dora], &names, "m", 2, 6, 300).await;
    eprintln!("mixed fork_decisions={forks}");
    assert!(forks <= 3, "B6 keeps forks rare: {forks}");
}

/// The capability goes away mid-run (a seat restored onto an older
/// build), then comes back: no height is left half-run, nothing stalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capability_that_flips_mid_run_neither_stalls_nor_forks() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], _) = found_trio(tmp.path(), &url).await;
    let all = [&mara, &walter, &dora];
    lock_on(&all, true).await;
    let names = ["mara", "walter", "dora"];
    burst(&all, &names, "f", 1, 3, 180).await;
    // proposals in flight while the old build appears
    for (w, name) in all.iter().zip(names) {
        w.execute(Command::Propose { surface: molt_core::Surface::Memory, payload: add_page(&format!("{name}/g.md"), name) })
            .await
            .expect("proposes");
    }
    walter.__lock_cap_off(true);
    let mut forks = converge(&all, 6, 300).await;
    lock_on(&[&walter], false).await;
    walter.__lock_cap_off(false);
    lock_on(&all, true).await;
    forks += burst(&all, &names, "h", 1, 9, 300).await;
    eprintln!("flip fork_decisions={forks}");
    assert!(forks <= 3, "the flip forks no more than B6: {forks}");
}

/// §4.2 under the lock, 2-of-3: dora is lost and restored with a link.
/// With walter away, mara alone does not seal and says why; once walter
/// is back, one survivor signature plus the consent seals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consented_restore_under_the_lock_needs_both_survivors() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let ([mara, walter, dora], dora_phrase) = found_trio(tmp.path(), &url).await;
    lock_on(&[&mara, &walter, &dora], true).await;
    for w in [&mara, &walter] {
        w.__seat_active_secs(0);
        w.__lock_timing(5, 3);
    }
    drop(dora);
    let ws = read_session(&walter).await.active_workspace.clone();
    let walter_dir = hard_kill(walter, &tmp.path().join("walter"), &ws).await;
    mara.execute(Command::RecoverInviteStart { member: "dora".to_string() }).await.expect("mint");
    let s = wait_for(&mara, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s.notice.strip_prefix("recovery-link:").unwrap_or_else(|| panic!("mint: {:?}", s.notice)).to_string();
    let rejoiner = engine(&tmp.path().join("dora-new"));
    adopt_relay(&rejoiner, &url).await;
    rejoiner
        .execute(Command::RecoverStart { link, phrase: dora_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    // mara alone: the restore is registered and waits for a second promise
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let s = status(&mara).await;
        if s.chain_sync.lock_wait == "promises 1/2" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no visible wait: {:?}", s.chain_sync);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(chain(&mara).await.iter().all(|(h, _, _)| *h == 0), "nothing sealed with one survivor");
    let _ = walter_dir;
    let walter = engine(&tmp.path().join("walter"));
    adopt_relay(&walter, &url).await;
    walter.__seat_active_secs(0);
    walter.__lock_timing(5, 3);
    walter.execute(Command::OpenWorkspace { id: ws }).await.expect("walter reopens");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let c = chain(&mara).await;
        if c.iter().any(|(h, _, signers)| *h >= 1 && signers.len() == 1) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the restore never sealed: {c:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let s = wait_for(&rejoiner, "dora is back", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()) || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "{:?}", s.notice);
    assert_eq!(status(&mara).await.chain_sync.fork_decisions, 0);
}
