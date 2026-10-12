// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The local wiki draft (`shared_memory_real.md` §9.2) survives a close and
//! reopen, is sealed at rest, and a legacy plaintext file reads as none.

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
    match w.execute(Command::WikiDraftLoad).await.expect("load") {
        Reply::WikiDraft { draft } => draft,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn founded(root: &std::path::Path) -> (WalletHandle, String, std::path::PathBuf) {
    let session = SessionView {
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(molt_core::GroupConfig::demo(), session, true);
    w.execute(Command::CreateStart {
        name: "Draft Club".to_string(),
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
    let dir = molt_storage::find_workspace_dir(root, &id).expect("dir");
    (w, id, dir)
}

fn contains(raw: &[u8], needle: &[u8]) -> bool {
    raw.windows(needle.len()).any(|x| x == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wiki_draft_is_sealed_and_survives_a_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (w, id, dir) = founded(&root).await;

    let draft = r#"{"stack":[{"path":"a","text":"plaintext-marker"}]}"#.to_string();
    w.execute(Command::WikiDraftSave { draft: draft.clone() })
        .await
        .expect("save");
    assert_eq!(load(&w).await, draft);
    w.execute(Command::CloseWorkspace).await.expect("close");

    let raw = std::fs::read(dir.join("wiki_draft.json")).expect("persisted");
    assert!(!contains(&raw, b"plaintext-marker"), "sealed at rest");

    w.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen");
    assert_eq!(load(&w).await, draft, "the draft comes back");

    w.execute(Command::WikiDraftSave { draft: String::new() })
        .await
        .expect("clear");
    assert_eq!(load(&w).await, "");
    w.execute(Command::CloseWorkspace).await.expect("close");
    assert!(!dir.join("wiki_draft.json").exists(), "empty removes the file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_plaintext_wiki_draft_reads_as_none() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (w, id, dir) = founded(&root).await;
    w.execute(Command::CloseWorkspace).await.expect("close");

    std::fs::write(dir.join("wiki_draft.json"), br#"{"stack":["legacy-marker"]}"#).expect("plant");
    w.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen");
    assert_eq!(load(&w).await, "", "legacy plaintext is no draft");

    w.execute(Command::WikiDraftSave { draft: "{\"stack\":[]}".to_string() })
        .await
        .expect("save");
    w.execute(Command::CloseWorkspace).await.expect("close");
    let raw = std::fs::read(dir.join("wiki_draft.json")).expect("persisted");
    assert!(!contains(&raw, b"legacy-marker"), "the sealed write replaced it");
    assert!(!contains(&raw, b"stack"), "sealed at rest");
}
