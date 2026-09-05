// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **K6 keystone - a folded cut over a real 2-of-2 republic**
//! (`docs/memory/knowledge_base_scale.md` §4.9): the cut collapses the
//! wiki's ratified patches into one commitment and drops them, the wiki
//! stays readable on both seats, and a holder that LOSES its copy of the
//! folded tree fetches it back over the file plane instead of reporting
//! an empty knowledge base.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
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

/// A node whose trickle publishes one piece per second.
fn engine(root: &std::path::Path, download_dir: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            download_dir: download_dir.display().to_string(),
            mirror_publish_interval_secs: 1,
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

/// Found a real 2-of-2 republic over the relay: petra (founder, the
/// sharer) and walter (the requester).
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"), &root.join("dl-a"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Datei Gilde".to_string(),
        member: "petra".to_string(),
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

    let b = engine(&root.join("member"), &root.join("dl-b"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join start");
    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Datei Gilde".to_string(),
        agenda: String::new(),
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
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
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


const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
const ADD_B: &str = "diff --git a/notes/b.md b/notes/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/notes/b.md\n@@ -0,0 +1,1 @@\n+second\n";

/// Ratify one wiki patch on the chain: petra proposes, walter approves,
/// the 2-of-2 threshold seals it.
async fn ratify_wiki(a: &WalletHandle, b: &WalletHandle, patch: &str) {
    let id = match a
        .execute(Command::Propose {
            surface: molt_core::Surface::Memory,
            payload: serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "x" }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if b.execute(Command::Approve { proposal: id }).await.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the proposal never reached walter"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The wiki as this seat serves it: the document count, or the pending
/// state when the folded base is not here.
async fn wiki_state(w: &WalletHandle) -> (u64, Option<molt_core::WikiBaseProgress>) {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read memory")
    {
        Reply::State(s) => (s.wiki_docs, s.wiki_base_pending),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wiki(w: &WalletHandle, what: &str, secs: u64, pred: impl Fn(u64, Option<molt_core::WikiBaseProgress>) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let (docs, pending) = wiki_state(w).await;
        if pred(docs, pending) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what} (docs {docs}, pending {pending:?})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The whole K6 round: two ratified patches, a folded cut that drops
/// them, and a seat that lost its copy of the tree getting it back off
/// the plane. Nothing here is a test double - a real relay, a real
/// threshold, the real trickle sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folded_cut_survives_a_lost_base_over_the_relay() {
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
    let (a, b) = found_pair(&root, &url).await;

    ratify_wiki(&a, &b, ADD_A).await;
    ratify_wiki(&a, &b, ADD_B).await;
    wait_wiki(&a, "petra's two documents", 30, |docs, _| docs == 2).await;
    wait_wiki(&b, "walter's two documents", 30, |docs, _| docs == 2).await;

    // the cut: petra proposes, walter co-signs it as correct
    a.execute(Command::ProposeCheckpoint).await.expect("cut proposed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let folded = match a.execute(Command::ReadChain).await.expect("read chain") {
            Reply::Chain { blocks } => blocks.iter().any(|v| v.kind == "checkpoint"),
            other => panic!("unexpected: {other:?}"),
        };
        if folded {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the cut never sealed");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // the patches are gone from the chain; the documents are not
    wait_wiki(&a, "the wiki after the cut", 30, |docs, p| docs == 2 && p.is_none()).await;
    wait_wiki(&b, "walter's wiki after the cut", 30, |docs, p| docs == 2 && p.is_none()).await;

    // walter loses his copy of the folded tree (§4.9.9: a damaged cache,
    // not a refused workspace) and reopens
    let ws = match b.execute(Command::ReadSession).await.expect("session") {
        Reply::Session(s) => s.workspaces.first().expect("a workspace").id.clone(),
        other => panic!("unexpected: {other:?}"),
    };
    b.execute(Command::CloseWorkspace).await.expect("close");
    let dir = std::fs::read_dir(root.join("member"))
        .expect("workspace root")
        .filter_map(|e| Some(e.ok()?.path()))
        .find(|p| p.is_dir())
        .expect("the joined workspace");
    let base = dir.join("wiki_base.bin");
    if !base.exists() {
        let listing: Vec<String> = std::fs::read_dir(&dir)
            .expect("workspace dir")
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().to_string()))
            .collect();
        panic!("no folded tree at {} - the workspace holds {listing:?}", base.display());
    }
    std::fs::remove_file(&base).expect("lose the base");
    b.execute(Command::OpenWorkspace { id: ws.clone() })
        .await
        .expect("reopen");

    // it is honest about it…
    wait_wiki(&b, "the base-pending state", 30, |docs, p| docs == 0 && p.is_some()).await;
    // …and gets it back off the plane, from the seat that still holds it
    wait_wiki(&b, "the base to arrive over the relay", 120, |docs, p| docs == 2 && p.is_none()).await;
    assert!(base.exists(), "the fetched base is kept for the next open");
}
