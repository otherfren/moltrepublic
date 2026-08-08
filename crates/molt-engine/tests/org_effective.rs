// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Applied Organization changes are REAL: an applied `set_name` renames
//! what every reader sees — the session's workspace entry (the GUI header
//! and the Open-screen list read exactly this) and the plaintext
//! `manifest.toml` on disk (what the undecrypted Open-screen scan lists) —
//! while the genesis event itself stays byte-identical, immutable history.
//!
//! Since the m ≥ 2 gate (2026-08-08) a republic cannot be founded 1-of-2,
//! so these run on a REAL 2-of-2 pair over an in-process relay: the founder
//! proposes, the member approves through the public command surface, and
//! only then is the change applied — the applied EFFECT under the real
//! threshold machinery.

use std::time::Duration;

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe to confirm the relay", |s| {
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

/// Found a real 2-of-2 republic over the relay; both engines end up entered.
async fn found_pair(
    root: &std::path::Path,
    url: &str,
    name: &str,
) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: name.to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link to become joinable", |s| {
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
    .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: name.to_string(),
        agenda: String::new(),
    })
    .await
    .expect("charter proposed");
    wait_for(&b, "walter to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    // entering is gated on the phrase-backup step now (2026-08-08) — both ends
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty())
        .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// The second voice: wait until `w` sees the open proposal with this `op`,
/// then approve it through the public command surface.
async fn approve_op(w: &WalletHandle, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals } =
            w.execute(Command::ListProposals).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(|v| v.as_str()) == Some(op)
            }) {
                w.execute(Command::Approve { proposal: p.id })
                    .await
                    .expect("approve");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {op} proposal never reached the second voice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_set_name_renames_the_session_entry_and_the_manifest() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let founder_root = root.join("founder");

    let (a, b) = found_pair(&root, &url, "Alte Gilde").await;
    let id = read_session(&a).await.active_workspace.clone();

    // the ratified founding name is what every view shows before the change
    let s = read_session(&a).await;
    let entry = s
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("active entry");
    assert_eq!(entry.name, "Alte Gilde");

    // propose the rename; the member's approval reaches 2-of-2 and applies it
    a.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "set_name",
            "title": "Namen ändern",
            "value": "Neue Gilde",
        }),
    })
    .await
    .expect("propose set_name");
    approve_op(&b, "set_name").await;

    // the session entry (header + Open list) follows the effective name
    wait_for(&a, "the session entry to take the applied name", |s| {
        s.workspaces
            .iter()
            .any(|ws| ws.id == id && ws.name == "Neue Gilde")
    })
    .await;

    // the plaintext manifest follows too (async writer → poll), so the
    // undecrypted Open-screen scan lists the new name after a restart
    let dir = molt_storage::find_workspace_dir(&founder_root, &id).expect("workspace dir");
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

    // the genesis stays immutable: the effective name is projection state
    match a.execute(Command::Status).await.expect("status") {
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_set_image_materializes_the_logo_file() {
    use base64::Engine as _;
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let founder_root = root.join("founder");

    let (a, b) = found_pair(&root, &url, "Logo Club").await;
    let id = read_session(&a).await.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&founder_root, &id).expect("workspace dir");

    // a real 2x2 PNG — since WP3 the bytes must decode as a picture
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==".to_string();
    let image_bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .expect("fixture decodes");
    a.execute(Command::Propose {
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
    approve_op(&b, "set_image").await;

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
    match a.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => assert_eq!(st.image, logo.display().to_string()),
        other => panic!("unexpected: {other:?}"),
    }

    // an applied remove_image deletes the file and clears the reference
    a.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "remove_image",
            "title": "Logo entfernen",
            "value": "",
        }),
    })
    .await
    .expect("propose remove_image");
    approve_op(&b, "remove_image").await;
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
    match a.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => assert_eq!(st.image, ""),
        other => panic!("unexpected: {other:?}"),
    }
}
