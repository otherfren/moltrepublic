// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The cut keystone (round 3, D9 / A1-A4)**: a real 2-of-3 republic over
//! one in-process relay cuts twice, with a seat that REOPENS in between and
//! a seat that stays silent while a cut is on the table.
//!
//! What round 3 broke here: the verifier's folded base was a snapshot taken
//! when the cached chain walk was built - at a reopen that is BEFORE the
//! stored tree is adopted, so it was the EMPTY tree, and the next folded cut
//! was refused by the very seat that had just co-signed it (that seat sat
//! partitioned at its old height for the rest of the run). The base is a
//! parameter of the fold now, never a cached field.

use std::time::Duration;

mod common;

use common::hard_kill;
use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// A real 2-of-3 republic with the memory feature: walter (founder), petra,
/// vera - the three seats of the round-3 run.
async fn found_three(
    root: &std::path::Path,
    url: &str,
) -> (WalletHandle, WalletHandle, WalletHandle) {
    let a = engine(&root.join("walter"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "both seat links", |s| {
        s.create.seats.len() == 2
            && s.create
                .seats
                .iter()
                .all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();

    let b = engine(&root.join("petra"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: links[0].clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("petra joins");

    let v = engine(&root.join("vera"));
    adopt_relay(&v, url).await;
    v.execute(Command::JoinStart {
        invite: links[1].clone(),
        member: "vera".to_string(),
    })
    .await
    .expect("vera joins");

    wait_for(&a, "both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "keep one shared memory".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    for w in [&b, &v] {
        wait_for(w, "the proposed charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed_ = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in [&b, &v] {
        wait_for(w, "the join to seal", |s| {
            s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
        })
        .await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the member to enter", |s| {
            s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
        })
        .await;
    }
    (a, b, v)
}

fn add(path: &str, body: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{body}\n"
    )
}

async fn propose_patch(w: &WalletHandle, patch: &str) -> molt_core::ProposalId {
    match w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "x" }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Approve on `b` as soon as the proposal's gossip has reached it - the
/// propose returns before the wire does.
async fn approve_once_it_arrived(b: &WalletHandle, id: molt_core::ProposalId, deadline: tokio::time::Instant) {
    loop {
        if b.execute(Command::Approve { proposal: id, note: None }).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the proposal never arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ratify one patch at the 2-of-3 threshold: `a` proposes, `b` approves,
/// and the call returns only once the block has moved `a`'s fold - the
/// patches of this test are meant to seal one after the other.
async fn ratify(a: &WalletHandle, b: &WalletHandle, patch: &str) {
    let before = memory_of(a).await.1;
    let id = propose_patch(a, patch).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    approve_once_it_arrived(b, id, deadline).await;
    loop {
        if memory_of(a).await.1 > before {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "the patch never sealed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What this seat holds of the shared memory: documents, revision, and the
/// folded base it counts from.
async fn memory_of(w: &WalletHandle) -> (u64, u64, Option<String>) {
    let (docs, rev) = match w
        .execute(Command::ReadState {
            surface: Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read memory")
    {
        Reply::State(s) => (s.wiki_docs, s.wiki_rev),
        other => panic!("unexpected: {other:?}"),
    };
    let base = match w
        .execute(Command::WikiChanges {
            since_rev: 0,
            limit: 1,
            cursor: 0,
        })
        .await
        .expect("wiki changes")
    {
        Reply::WikiChanges { base, .. } => base,
        other => panic!("unexpected: {other:?}"),
    };
    (docs, rev, base)
}

/// (head height, the cut this holder is anchored on, diverged peers). A
/// sealed cut PRUNES, so the view carries exactly one checkpoint block -
/// its height is what says which cut landed.
async fn chain_of(w: &WalletHandle) -> (u64, u64, usize) {
    match w.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, diverged, .. } => (
            blocks.iter().map(|b| b.height).max().unwrap_or(0),
            blocks
                .iter()
                .filter(|b| b.kind == "checkpoint")
                .map(|b| b.height)
                .max()
                .unwrap_or(0),
            diverged.len(),
        ),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for_memory(w: &WalletHandle, who: &str, what: &str, pred: impl Fn(u64, u64) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let (docs, rev, _) = memory_of(w).await;
        if pred(docs, rev) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{who} never reached {what} (docs {docs}, rev {rev})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait until this seat is anchored on a cut ABOVE `above`, and answer the
/// anchor height it reached.
async fn wait_cut_above(w: &WalletHandle, above: u64, what: &str, secs: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let (_, anchor, _) = chain_of(w).await;
        if anchor > above {
            return anchor;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn workspace_id(w: &WalletHandle) -> String {
    read_session(w)
        .await
        .workspaces
        .first()
        .expect("a workspace")
        .id
        .clone()
}

/// **The keystone.** Two folded cuts around a reopen and a silent seat: the
/// seat that reopened must still apply the next cut (D9), the cut must wait
/// for every seat (A2), the revision must not restart at it (A3), and an
/// open patch must keep its vote across it (A4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_cut_lands_on_every_seat_after_a_reopen() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b, v) = found_three(&root, &url).await;

    for (i, path) in ["a.md", "b.md", "c.md"].iter().enumerate() {
        ratify(&a, &b, &add(path, &format!("body {i}"))).await;
    }
    for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
        wait_for_memory(w, who, "three documents", |docs, _| docs == 3).await;
    }

    // the first cut - every seat is here, so it seals
    a.execute(Command::ProposeCheckpoint).await.expect("first cut");
    let mut first_cut = 0;
    for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
        let at = wait_cut_above(w, 0, &format!("{who}'s first cut"), 90).await;
        assert!(first_cut == 0 || first_cut == at, "every seat anchors on the same cut");
        first_cut = at;
    }
    let after_first = [memory_of(&a).await, memory_of(&b).await, memory_of(&v).await];
    for m in &after_first {
        assert_eq!(m.0, 3, "every seat holds three documents");
        assert_eq!(m.1, 3, "the revision does not restart at a cut");
        assert_eq!(m.2, after_first[0].2, "every seat holds the same folded base");
        assert!(m.2.is_some(), "a cut leaves a folded base behind");
    }
    // a full holder keeps the cards the cut folded away, stamped with
    // their revisions: the history below the cut is still answered
    for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
        match w
            .execute(Command::WikiChanges { since_rev: 0, limit: 0, cursor: 0 })
            .await
            .expect("wiki changes")
        {
            Reply::WikiChanges { changes, truncated, .. } => {
                assert_eq!(changes.len(), 3, "{who} lists the three documents from the founding");
                assert!(!truncated, "{who}'s cards answer below the cut");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // vera goes silent, the other two keep working
    let vera_ws = workspace_id(&v).await;
    v.execute(Command::CloseWorkspace).await.expect("vera closes");
    for (i, path) in ["d.md", "e.md", "f.md"].iter().enumerate() {
        ratify(&a, &b, &add(path, &format!("more {i}"))).await;
    }
    wait_for_memory(&a, "walter", "six documents", |docs, _| docs == 6).await;

    // an open proposal of petra's, on a path nothing else touches: the cut
    // must not cost it its vote (A4, `rebase`)
    let open = propose_patch(&b, &add("open.md", "still on the table")).await;

    // A2: a cut needs EVERY seat, so this one sits pending while vera sleeps
    a.execute(Command::ProposeCheckpoint).await.expect("second cut");
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(
        chain_of(&a).await.1,
        first_cut,
        "a cut must not seal while a seat has not co-signed it"
    );

    // vera comes back: she catches up and co-signs, and the cut seals
    v.execute(Command::OpenWorkspace { id: vera_ws })
        .await
        .expect("vera reopens");
    for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
        wait_cut_above(w, first_cut, &format!("{who}'s second cut"), 120).await;
    }

    let after_second = [memory_of(&a).await, memory_of(&b).await, memory_of(&v).await];
    for m in &after_second {
        assert_eq!(m.0, 6, "every seat holds six documents");
        assert_eq!(m.1, 6, "six ratified patches, six revisions - a cut is not a reset");
        assert_eq!(m.2, after_second[0].2, "every seat holds the same folded base");
    }
    let heads = [chain_of(&a).await, chain_of(&b).await, chain_of(&v).await];
    for h in &heads {
        assert_eq!(h.0, heads[0].0, "every seat is at the same height");
        assert_eq!(h.2, 0, "nothing diverged");
    }

    // the open proposal survived the cut with its own signature
    let view = match b.execute(Command::LIST_ALL_PROPOSALS).await.expect("read proposals") {
        Reply::Proposals { proposals, .. } => proposals
            .into_iter()
            .find(|p| p.id == open)
            .expect("the open proposal is still listed"),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(
        view.state,
        molt_core::ProposalState::Proposed,
        "a cut re-bases an open patch, it does not kill it"
    );
    assert!(view.approvals >= 1, "the proposer's own signature survives the cut");
    assert_eq!(
        view.superseded_kind,
        Some(molt_core::SupersededKind::Rebase),
        "the cut re-based it - that is not the same news as a conflict"
    );
    assert!(!view.superseded, "a re-based patch is not dead");

    // and the next patch counts on from there on every seat
    ratify(&a, &b, &add("g.md", "after the cut")).await;
    for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
        wait_for_memory(w, who, "revision 7", |_, rev| rev == 7).await;
    }
}

fn edit(path: &str, from: &str, to: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,1 @@\n-{from}\n+{to}\n")
}

/// The reopen twin of the supersede walk: a seat whose log holds two
/// ratified edits and one open edit of its own comes back with the
/// ratified ones applied and clean and the open one still on the table -
/// and the open one still seals. A clean close writes a snapshot (the tail
/// is empty); a hard kill leaves the tail to replay BEFORE the chain is
/// adopted, and the walk must not judge those cards against the empty tree
/// it sees then. With a cut in between, the reopen also has a folded base
/// to adopt (K6) before the walk may judge anything.
async fn reopen_keeps_the_cards(hard_kill_it: bool, with_cut: bool) {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b, v) = found_three(&root, &url).await;

    ratify(&a, &b, &add("a.md", "one")).await;
    ratify(&a, &b, &edit("a.md", "one", "two")).await;
    if with_cut {
        a.execute(Command::ProposeCheckpoint).await.expect("cut");
        for (w, who) in [(&a, "walter"), (&b, "petra"), (&v, "vera")] {
            wait_cut_above(w, 0, &format!("{who}'s cut"), 90).await;
        }
    }
    let open = propose_patch(&a, &edit("a.md", "two", "three")).await;

    let a_ws = workspace_id(&a).await;
    let a = if hard_kill_it {
        hard_kill(a, &root.join("walter"), &a_ws).await;
        // a fresh engine on the same directory: its relay comes from the
        // settings, as every seat's did at the founding
        let a = engine(&root.join("walter"));
        adopt_relay(&a, &url).await;
        a
    } else {
        a.execute(Command::CloseWorkspace).await.expect("walter closes");
        a
    };
    a.execute(Command::OpenWorkspace { id: a_ws })
        .await
        .expect("walter reopens");

    let views = match a.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") {
        Reply::Proposals { proposals, .. } => proposals,
        other => panic!("unexpected: {other:?}"),
    };
    let patches: Vec<_> = views
        .iter()
        .filter(|p| p.surface == Surface::Memory && p.id.0 != 0)
        .collect();
    assert_eq!(patches.len(), 3, "two ratified edits and the open one");
    for p in patches {
        let want = if p.id == open {
            molt_core::ProposalState::Proposed
        } else {
            molt_core::ProposalState::Applied
        };
        assert_eq!(p.state, want, "#{} after the reopen", p.id.0);
        assert!(!p.superseded, "#{} carries no verdict", p.id.0);
        assert_eq!(p.superseded_kind, None, "#{} carries no kind", p.id.0);
    }

    // the open edit is still a vote: the second voice seals it and the
    // reopened seat's fold moves
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    approve_once_it_arrived(&b, open, deadline).await;
    wait_for_memory(&a, "walter", "revision 3", |_, rev| rev == 3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_reopen_keeps_a_ratified_edit_clean_and_an_open_edit_open() {
    reopen_keeps_the_cards(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hard_kill_reopen_keeps_a_ratified_edit_clean_and_an_open_edit_open() {
    reopen_keeps_the_cards(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hard_kill_reopen_under_a_cut_keeps_a_ratified_edit_clean_and_an_open_edit_open() {
    reopen_keeps_the_cards(true, true).await;
}
