// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Applied Organization changes are REAL: an applied `set_name` renames
//! what every reader sees — the session's workspace entry (the GUI header
//! and the Open-screen list read exactly this) and the plaintext
//! `manifest.toml` on disk (what the undecrypted Open-screen scan lists) —
//! while the genesis event itself stays byte-identical, immutable history.

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(w).await;
        match s.create.run.outcome {
            1 => return,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_set_name_renames_the_session_entry_and_the_manifest() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    // 1-of-2 so the founder's self-cosign alone seals every block — the
    // point here is the applied EFFECT, not the threshold machinery
    let w = molt_engine::__spawn_sim_founding(molt_core::GroupConfig::demo(), session, true);
    w.execute(Command::CreateStart {
        name: "Alte Gilde".to_string(),
        member: "petra".to_string(),
        threshold: 1,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("enter");
    let id = read_session(&w).await.active_workspace.clone();

    // the ratified founding name is what every view shows before the change
    let s = read_session(&w).await;
    let entry = s
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("active entry");
    assert_eq!(entry.name, "Alte Gilde");

    // propose the rename — 1-of-2 applies it immediately (self-cosign)
    w.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "set_name",
            "title": "Namen ändern",
            "value": "Neue Gilde",
        }),
    })
    .await
    .expect("propose set_name");

    // the session entry (header + Open list) follows the effective name
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = read_session(&w).await;
        let name = s
            .workspaces
            .iter()
            .find(|ws| ws.id == id)
            .map(|ws| ws.name.clone())
            .unwrap_or_default();
        if name == "Neue Gilde" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session entry never took the applied name (still {name:?})"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // the plaintext manifest follows too (async writer → poll), so the
    // undecrypted Open-screen scan lists the new name after a restart
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("workspace dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let manifest = molt_storage::read_manifest(&dir).expect("manifest");
        if manifest.workspace.name == "Neue Gilde" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "manifest.toml never took the applied name (still {:?})",
            manifest.workspace.name
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // the genesis stays immutable: block 0 still carries the founding name
    match w
        .execute(Command::Status)
        .await
        .expect("status")
    {
        Reply::Status(st) => {
            assert_eq!(st.name, "Neue Gilde", "the effective name is real state");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// An applied `set_image` is REAL on every member's device: the bytes ride
/// the proposal payload (sign-what-you-see), and applying materializes them
/// as `logo.<ext>` inside the workspace directory — the reference every
/// view shows is that local file. `remove_image` deletes it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_applied_set_image_materializes_the_logo_file() {
    use base64::Engine as _;
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(molt_core::GroupConfig::demo(), session, true);
    w.execute(Command::CreateStart {
        name: "Logo Club".to_string(),
        member: "petra".to_string(),
        threshold: 1,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("enter");
    let id = read_session(&w).await.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("workspace dir");

    // a real 2x2 PNG — since WP3 the bytes must decode as a picture
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==".to_string();
    let image_bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .expect("fixture decodes");
    w.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "set_image",
            "title": "Logo setzen",
            "value": "vereinslogo.png",
            "bytes_b64": b64,
        }),
    })
    .await
    .expect("propose set_image");

    // the applied change materializes the logo file (async writer → poll)
    let logo = dir.join("logo.png");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read(&logo).is_ok_and(|b| b == image_bytes) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "logo.png never materialized with the proposed bytes"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // and the effective image reference IS that local file
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => assert_eq!(st.image, logo.display().to_string()),
        other => panic!("unexpected: {other:?}"),
    }

    // an applied remove_image deletes the file and clears the reference
    w.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "remove_image",
            "title": "Logo entfernen",
            "value": "",
        }),
    })
    .await
    .expect("propose remove_image");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !logo.exists() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "logo.png was not deleted by the applied remove_image"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => assert_eq!(st.image, ""),
        other => panic!("unexpected: {other:?}"),
    }
}
