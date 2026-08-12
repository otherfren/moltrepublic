// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F3 keystone — file sharing over relays**
//! (`docs/transport/file_transfer_nostr.md`): the share is metadata-only,
//! the bytes publish LAZILY on the first download request (kind-447 chunk
//! series), and the downloader ends with the verified file on disk — over
//! a real 2-of-2 republic on an in-process relay.

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
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path, download_dir: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            download_dir: download_dir.display().to_string(),
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

/// Found a real 2-of-2 republic over the relay (the org_effective harness).
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let dl_a = root.join("dl-a");
    let dl_b = root.join("dl-b");
    let a = engine(&root.join("founder"), &dl_a);
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
    let s = wait_for(&a, "the seat link to become joinable", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("member"), &dl_b);
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join start");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Datei Gilde".to_string(),
        agenda: String::new(),
        features: Vec::new(),
    })
    .await
    .expect("charter proposed");
    wait_for(&b, "walter to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// The whole story: petra shares (metadata only — nothing on the relay
/// yet), walter downloads; the request triggers petra's lazy chunk-series
/// publish, walter fetches, verifies and lands the file in his download
/// dir with the shared bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_file_downloads_over_the_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;

    // petra's file: 100 KiB pattern
    let bytes: Vec<u8> = (0..100 * 1024)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let src = tmp.path().join("bericht.bin");
    std::fs::write(&src, &bytes).expect("write source");

    a.execute(Command::ShareFile {
        path: src.display().to_string(),
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("the share is admitted over relays");

    // the share message reaches walter's chat (metadata only)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let share_id = loop {
        if let Reply::State(snap) = b
            .execute(Command::ReadState {
                surface: molt_core::Surface::Chat,
                channel: None,
                view: None,
            })
            .await
            .expect("read chat")
        {
            let found = snap.applied.iter().find_map(|v| {
                let msg = serde_json::from_value::<molt_core::ChatMessage>(v.clone()).ok()?;
                msg.file.is_some().then_some(msg.id)
            });
            if let Some(id) = found {
                break id;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the share message never reached the member"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // walter downloads — lazy publish + fetch under the hood
    b.execute(Command::DownloadFile { id: share_id, dest: None })
        .await
        .expect("download starts");

    let dl_dir = root.join("dl-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let landed = std::fs::read_dir(&dl_dir)
            .ok()
            .and_then(|mut it| {
                it.find_map(|e| {
                    let p = e.ok()?.path();
                    (p.file_name()?.to_string_lossy() == "bericht.bin").then_some(p)
                })
            })
            .and_then(|p| std::fs::read(&p).ok());
        if let Some(got) = landed {
            assert_eq!(got, bytes, "the downloaded bytes are the shared bytes");
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let phase = match b.execute(Command::ReadUploads).await.expect("uploads") {
                Reply::Uploads { uploads } => format!("{uploads:?}"),
                other => format!("{other:?}"),
            };
            panic!("the download never landed; uploads: {phase}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
