// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The kanban basket (`docs/kanban/kanban_workflows.md` §8) survives a
//! close and reopen, and is sealed at rest like the other state files.

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn load(w: &WalletHandle) -> String {
    match w.execute(Command::KanbanDraftLoad).await.expect("load") {
        Reply::KanbanDraft { draft } => draft,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_basket_is_sealed_and_survives_a_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let session = SessionView {
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(molt_core::GroupConfig::demo(), session, true);
    w.execute(Command::CreateStart {
        name: "Basket Club".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let seed = read_session(&w).await.create.seed.clone();
    w.execute(Command::ConfirmSeedBackup { phrase: seed })
        .await
        .expect("backup confirm");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while read_session(&w).await.create.run.outcome != 1 {
        assert!(tokio::time::Instant::now() < deadline, "founding did not seal");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.execute(Command::CreateFinish).await.expect("enter");
    let id = read_session(&w).await.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");

    let draft = r#"{"acts":[{"op":"add","title":"plaintext-marker"}]}"#.to_string();
    w.execute(Command::KanbanDraftSave { draft: draft.clone() })
        .await
        .expect("save");
    assert_eq!(load(&w).await, draft);
    w.execute(Command::CloseWorkspace).await.expect("close");

    let raw = std::fs::read(dir.join("kanban_draft.json")).expect("persisted");
    assert!(
        !raw.windows(b"plaintext-marker".len()).any(|x| x == b"plaintext-marker"),
        "sealed at rest"
    );

    w.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen");
    assert_eq!(load(&w).await, draft, "the basket comes back");

    w.execute(Command::KanbanDraftSave { draft: String::new() })
        .await
        .expect("clear");
    assert_eq!(load(&w).await, "");
    w.execute(Command::CloseWorkspace).await.expect("close");
    assert!(!dir.join("kanban_draft.json").exists(), "empty removes the file");
}
