// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Rejoin and the next cut** (investigation for the rejoin completion
//! plan): a 2-of-3 republic with a wiki, one seat restored on a fresh
//! device over the recovery ritual. Does the restored seat get the wiki
//! back, and does a later cut seal? Two shapes: the restore AFTER a
//! folded cut (anchored at it, needs the base), and the restore with no
//! cut at all (anchored at the genesis, catches up every patch).

use std::time::Duration;

use molt_core::{Command, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod common;

use common::{adopt_relay, engine, read_session, wait_for};

fn page(path: &str, body: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{body}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

/// walter (founder), petra and vera; 2-of-3, wiki on. Returns petra's phrase.
async fn found_three(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle, WalletHandle, String) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart { name: "Club".to_string(), member: "walter".to_string(), threshold: 2, members: 3, relays: Vec::new() })
        .await
        .expect("create start");
    let s = wait_for(&a, "two seat links", |s| {
        s.create.seats.len() == 2 && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["petra", "vera"].iter().zip(links) {
        let w = engine(&root.join(name));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() }).await.expect("join");
        joiners.push(w);
    }
    let petra_phrase = wait_for(&joiners[0], "petra's phrase", |s| !s.join.seed.is_empty()).await.join.seed.clone();
    wait_for(&a, "both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Club".to_string(), agenda: String::new(), features: vec!["memory".to_string()] })
        .await
        .expect("charter");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup");
    for w in &joiners {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("finish");
    for w in &joiners {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "entered", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    }
    for w in [&a, &joiners[0], &joiners[1]] {
        w.__seat_active_secs(0);
    }
    let mut it = joiners.into_iter();
    (a, it.next().expect("petra"), it.next().expect("vera"), petra_phrase)
}

/// `proposer` proposes a page, `approver` approves until it seals.
async fn ratify(proposer: &WalletHandle, approver: &WalletHandle, path: &str) {
    let Reply::Proposed { id, .. } = proposer
        .execute(Command::Propose { surface: molt_core::Surface::Memory, payload: page(path, path) })
        .await
        .expect("propose")
    else {
        panic!("proposed");
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while let Err(e) = approver.execute(Command::Approve { proposal: id, note: None }).await {
        assert!(tokio::time::Instant::now() < deadline, "the card never reached the approver: {e}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // sealed on both before the next card: one decision per height (a
    // seat lost a moment ago still holds the sealing line until it is
    // silent: the fallback grace)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while !(applied(proposer, id.0).await && applied(approver, id.0).await) {
        assert!(tokio::time::Instant::now() < deadline, "{path} never sealed on both");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn applied(w: &WalletHandle, id: u64) -> bool {
    match w.execute(Command::LIST_ALL_PROPOSALS).await {
        Ok(Reply::Proposals { proposals, .. }) => proposals.iter().any(|p| p.id.0 == id && p.state == molt_core::ProposalState::Applied),
        _ => false,
    }
}

async fn wiki(w: &WalletHandle) -> (u64, bool) {
    match w
        .execute(Command::ReadState { surface: molt_core::Surface::Memory, channel: None, view: None })
        .await
        .expect("read memory")
    {
        Reply::State(s) => (s.wiki_docs, s.wiki_base_pending.is_some()),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn cuts(w: &WalletHandle) -> usize {
    match w.execute(Command::ReadChain).await.expect("chain") {
        Reply::Chain { blocks, .. } => blocks.iter().filter(|v| v.kind == "checkpoint").count(),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn head(w: &WalletHandle) -> u64 {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s.seats.iter().find(|x| x.head_height.is_some() && x.build.is_some()).and_then(|x| x.head_height).unwrap_or(0),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll `pred` for up to `secs`; whether it held.
async fn within<F, Fut>(secs: u64, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if pred().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Restore petra on a fresh device by walter's link.
async fn restore_petra(a: &WalletHandle, root: &std::path::Path, url: &str, phrase: String) -> WalletHandle {
    a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await.expect("mint");
    let s = wait_for(a, "the link", |s| s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")).await;
    let link = s.notice.strip_prefix("recovery-link:").unwrap_or_else(|| panic!("{:?}", s.notice)).to_string();
    let c = engine(&root.join("petra-new"));
    adopt_relay(&c, url).await;
    c.execute(Command::RecoverStart { link, phrase, relays: Vec::new() }).await.expect("recover start");
    let s = common::wait_for_secs(&c, "the restored republic", 120, |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()) || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "{:?}", s.notice);
    c
}

fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

/// Q3: restored AFTER a folded cut - anchored at it, the wiki behind a
/// base commitment. Does it get the base, and does the next cut seal?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_restored_after_a_folded_cut_signs_the_next_cut() {
    trace();
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, v, phrase) = found_three(tmp.path(), &url).await;
    ratify(&a, &v, &format!("{}.md", "a")).await;
    ratify(&a, &v, &format!("{}.md", "b")).await;
    assert!(within(60, || async { wiki(&a).await.0 == 2 && wiki(&b).await.0 == 2 && wiki(&v).await.0 == 2 }).await, "every seat holds both pages");
    a.execute(Command::ProposeCheckpoint).await.expect("cut");
    assert!(within(90, || async { cuts(&a).await == 1 && cuts(&b).await == 1 && cuts(&v).await == 1 }).await, "the first cut seals");
    drop(b);
    let c = restore_petra(&a, tmp.path(), &url, phrase).await;
    let base = within(180, || async { wiki(&c).await == (2, false) }).await;
    eprintln!("probe restored_after_cut base_arrived={base} wiki={:?}", wiki(&c).await);
    ratify(&a, &v, &format!("{}.md", "c")).await;
    // every seat at the same head before the cut (a stale proposer's cut is ignored)
    assert!(within(60, || async { wiki(&a).await.0 == 3 && wiki(&v).await.0 == 3 && wiki(&c).await.0 == 3 }).await, "c.md everywhere");
    a.execute(Command::ProposeCheckpoint).await.expect("second cut");
    let sealed = within(120, || async { cuts(&a).await == 1 && head(&a).await > 0 && {
        // the second cut prunes to itself: one checkpoint row, a higher anchor
        matches!(a.execute(Command::ReadChain).await, Ok(Reply::Chain { blocks, .. }) if blocks.iter().filter(|x| x.kind == "checkpoint").map(|x| x.height).max().unwrap_or(0) > 4)
    } })
    .await;
    eprintln!("probe restored_after_cut second_cut_sealed={sealed} wiki_c={:?}", wiki(&c).await);
    assert!(base, "the restored seat never got the shared memory base");
    assert!(sealed, "the cut after the restore never sealed");
}

/// Q1 with no earlier cut: the restored seat anchors at the genesis and
/// catches up every patch. Does an automatic-size cut seal?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_restored_without_a_cut_signs_the_first_cut() {
    trace();
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, v, phrase) = found_three(tmp.path(), &url).await;
    ratify(&a, &v, &format!("{}.md", "a")).await;
    ratify(&a, &v, &format!("{}.md", "b")).await;
    assert!(within(60, || async { wiki(&b).await.0 == 2 }).await, "petra holds both pages");
    drop(b);
    let c = restore_petra(&a, tmp.path(), &url, phrase).await;
    let caught_up = within(180, || async { wiki(&c).await.0 == 2 }).await;
    eprintln!("probe restored_no_cut wiki_c={:?}", wiki(&c).await);
    a.execute(Command::ProposeCheckpoint).await.expect("cut");
    let sealed = within(120, || async { cuts(&a).await == 1 && cuts(&c).await == 1 && cuts(&v).await == 1 }).await;
    eprintln!("probe restored_no_cut cut_sealed={sealed}");
    assert!(caught_up, "the restored seat never caught up the wiki");
    assert!(sealed, "the first cut after the restore never sealed");
}

/// The field shape: the only cut far below the head (40 blocks of
/// activity after it), the restored seat anchored there with a catch-up
/// to drain while it fetches the base. Does the base arrive, and does
/// the next cut seal?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_restored_far_above_its_anchor_gets_the_base_and_cuts() {
    trace();
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, v, phrase) = found_three(tmp.path(), &url).await;
    ratify(&a, &v, &format!("{}.md", "a")).await;
    ratify(&a, &v, &format!("{}.md", "b")).await;
    assert!(within(60, || async { wiki(&a).await.0 == 2 && wiki(&b).await.0 == 2 && wiki(&v).await.0 == 2 }).await, "every seat holds both pages");
    a.execute(Command::ProposeCheckpoint).await.expect("cut");
    assert!(within(90, || async { cuts(&a).await == 1 && cuts(&b).await == 1 && cuts(&v).await == 1 }).await, "the first cut seals");
    drop(b);
    for i in 0..40 {
        ratify(&a, &v, &format!("p{i}.md")).await;
    }
    assert!(within(240, || async { wiki(&a).await.0 == 42 && wiki(&v).await.0 == 42 }).await, "40 more pages");
    let started = tokio::time::Instant::now();
    let c = restore_petra(&a, tmp.path(), &url, phrase).await;
    let caught = within(600, || async { wiki(&c).await == (42, false) }).await;
    eprintln!("probe far_anchor caught_up={caught} after_secs={} wiki_c={:?}", started.elapsed().as_secs(), wiki(&c).await);
    a.execute(Command::ProposeCheckpoint).await.expect("second cut");
    let sealed = within(120, || async {
        matches!(c.execute(Command::ReadChain).await, Ok(Reply::Chain { blocks, .. }) if blocks.iter().filter(|x| x.kind == "checkpoint").map(|x| x.height).max().unwrap_or(0) > 10)
    })
    .await;
    eprintln!("probe far_anchor second_cut_sealed={sealed}");
    assert!(caught, "the restored seat never got its wiki back");
    assert!(sealed, "the cut after the restore never sealed");
}
