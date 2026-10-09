// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Kanban S5 keystone** (`docs_archive/kanban/kanban_workflows.md` §9):
//! two real engines of a 2-of-2 republic run kanban changesets through the
//! real governance path. Both serve the same board and the same derived
//! dates for the same UTC day, a changeset the fold voids is void on both,
//! a checkpoint cut keeps the fold, and a wiki page's `quest:` link reads
//! back as a backlink on both seats.

use std::time::Duration;

use chrono::{Days, NaiveDate};
use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;
use serde_json::{json, Value};

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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

/// A real 2-of-2 republic with the wiki and the board: mara founds,
/// walter joins.
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Board".to_string(),
        member: "mara".to_string(),
        members: 2,
        threshold: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("member"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join start");
    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Board".to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string(), "quests".to_string()],
    })
    .await
    .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed })
        .await
        .expect("founder backup confirm");
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let seed = read_session(&b).await.join.seed.clone();
    b.execute(Command::ConfirmSeedBackup { phrase: seed })
        .await
        .expect("joiner backup confirm");
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// mara proposes; the reply's minted ids.
async fn propose(a: &WalletHandle, surface: Surface, payload: Value) -> (u64, Vec<String>) {
    match a
        .execute(Command::Propose { surface, payload })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, minted, .. } => (id.0, minted),
        other => panic!("unexpected: {other:?}"),
    }
}

/// walter co-signs once the card reached him; the 2-of-2 seals it.
async fn approve(b: &WalletHandle, id: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while b
        .execute(Command::Approve {
            proposal: molt_core::ProposalId(id),
            note: None,
        })
        .await
        .is_err()
    {
        assert!(tokio::time::Instant::now() < deadline, "proposal {id} never reached walter");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn quests(w: &WalletHandle) -> molt_core::SurfaceSnapshot {
    match w
        .execute(Command::ReadState {
            surface: Surface::Quests,
            channel: None,
            view: None,
        })
        .await
        .expect("read quests")
    {
        Reply::State(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The board as every seat must see it: the reader's own keys removed.
async fn shared_board(w: &WalletHandle) -> Value {
    let mut board = quests(w).await.board.expect("the quests read carries the board");
    let o = board.as_object_mut().expect("board object");
    for reader in ["me", "now", "starting"] {
        o.remove(reader);
    }
    board
}

/// Both seats' boards once both reached `rev` and the predicate holds.
async fn boards_at(a: &WalletHandle, b: &WalletHandle, rev: u64, pred: impl Fn(&Value) -> bool) -> (Value, Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (x, y) = (shared_board(a).await, shared_board(b).await);
        if x["rev"] == json!(rev) && y["rev"] == json!(rev) && pred(&x) && pred(&y) {
            return (x, y);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for rev {rev}: mara {} walter {}",
            x["rev"],
            y["rev"]
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn cs(summary: &str, base_rev: u64, ops: Value) -> Value {
    json!({"op": "kanban_ops", "summary": summary, "base_rev": base_rev, "ops": ops})
}

fn day(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

async fn void_of(w: &WalletHandle, id: u64) -> Option<String> {
    quests(w)
        .await
        .accepted
        .iter()
        .find(|p| p.id.0 == id)
        .and_then(|p| p.void.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_seats_share_the_board_its_dates_voids_and_backlinks_across_a_cut() {
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
    let (a, b) = found_pair(&tmp.path().join("workspaces"), &url).await;

    // §5.3 shifted to the real UTC day: E promised after B needs it, F late
    let today = chrono::Utc::now().date_naive();
    let plus = |n: u64| day(today.checked_add_days(Days::new(n)).expect("date"));
    let yesterday = day(today.checked_sub_days(Days::new(1)).expect("date"));
    let adds = cs(
        "plan",
        0,
        json!([
            {"act": "add", "ref": "a", "title": "A", "assignees": ["mara"]},
            {"act": "add", "ref": "e", "title": "E", "assignees": ["walter"], "due": plus(16)},
            {"act": "add", "ref": "b", "title": "B", "assignees": ["walter"], "blocked_by": ["@a", "@e"]},
            {"act": "add", "ref": "c", "title": "C", "assignees": ["walter"], "blocked_by": ["@a"], "due": plus(8)},
            {"act": "add", "ref": "d", "title": "D", "assignees": ["mara"]},
            {"act": "add", "ref": "m", "title": "M1", "assignees": ["mara"], "blocked_by": ["@a", "@b", "@c"], "due": plus(11)},
            {"act": "add", "title": "F", "assignees": ["mara"], "due": yesterday}
        ]),
    );
    let (p1, minted) = propose(&a, Surface::Quests, adds).await;
    let [ta, te, tb, tc, td, tm, tf] = <[String; 7]>::try_from(minted).expect("seven minted ids");
    approve(&b, p1).await;
    boards_at(&a, &b, 1, |_| true).await;
    let (p2, _) = propose(&a, Surface::Quests, cs("A started", 1, json!([{"act": "state", "id": ta, "to": "wip"}]))).await;
    approve(&b, p2).await;

    // the same board AND the same derived dates on both seats
    let (x, y) = boards_at(&a, &b, 2, |_| true).await;
    assert_eq!(x, y, "the shared board differs between the seats");
    assert_eq!(x["today"], json!(day(today)), "both read on the test's UTC day");
    let t = &x["tasks"];
    assert_eq!(t[&ta]["needed_by"], json!(plus(8)), "C's deadline flows back to A");
    assert_eq!(t[&te]["needed_by"], json!(plus(11)), "M1's deadline flows back through B");
    assert_eq!(t[&te]["flags"], json!(["date_conflict"]));
    assert_eq!(t[&tf]["flags"], json!(["overdue"]));
    assert_eq!(t[&tb]["shown"], json!("blocked"));
    assert_eq!(t[&tm]["shown"], json!("blocked"));
    assert_eq!(t[&tc]["needed_by"], json!(plus(8)));
    assert_eq!(x["priority"][0], json!(ta), "wip first");
    assert_eq!(x["next"]["walter"], json!([te]));

    // two cards that both apply now; sealed in order, the second voids
    let (p3, _) = propose(
        &a,
        Surface::Quests,
        cs("drop D", 2, json!([{"act": "state", "id": td, "to": "cancelled", "note": "not needed"}])),
    )
    .await;
    let (p4, _) = propose(&a, Surface::Quests, cs("start D", 2, json!([{"act": "state", "id": td, "to": "wip"}]))).await;
    approve(&b, p3).await;
    boards_at(&a, &b, 3, |_| true).await;
    approve(&b, p4).await;
    let (x, y) = boards_at(&a, &b, 4, |_| true).await;
    assert_eq!(x, y, "the void changed the boards differently");
    assert_eq!(x["tasks"][&td]["state"], json!("cancelled"), "the void left D as it was");
    for w in [&a, &b] {
        let void = void_of(w, p4).await;
        assert!(void.as_deref().is_some_and(|r| r.contains("not allowed")), "{void:?}");
        assert_eq!(void_of(w, p3).await, None);
    }

    // a wiki page links two tasks; each reads it back as a backlink
    let short = &ta[..8];
    let page = format!(
        "diff --git a/plan.md b/plan.md\nnew file mode 100644\n--- /dev/null\n+++ b/plan.md\n@@ -0,0 +1,1 @@\n+Start with [A](quest:{short}), then [M1](quest:{tm}).\n"
    );
    let (p5, _) = propose(&a, Surface::Memory, json!({"op": "wiki_patch", "value": page, "summary": "plan"})).await;
    approve(&b, p5).await;
    let linked = |v: &Value| v["tasks"][&ta]["referenced_by"] == json!(["plan.md"]);
    let (x, y) = boards_at(&a, &b, 4, linked).await;
    assert_eq!(x, y);
    assert_eq!(x["tasks"][&tm]["referenced_by"], json!(["plan.md"]));
    assert_eq!(x["tasks"][&tb].get("referenced_by"), None);
    let before = x;

    // the cut drops the blocks, never the board
    a.execute(Command::ProposeCheckpoint).await.expect("cut proposed");
    for w in [&a, &b] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let cut = match w.execute(Command::ReadChain).await.expect("read chain") {
                Reply::Chain { blocks, .. } => blocks.iter().any(|v| v.kind == "checkpoint"),
                other => panic!("unexpected: {other:?}"),
            };
            if cut {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the cut never sealed");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    let (x, y) = boards_at(&a, &b, 4, linked).await;
    assert_eq!(x, before, "mara's board moved at the cut");
    assert_eq!(y, before, "walter's board moved at the cut");
}
