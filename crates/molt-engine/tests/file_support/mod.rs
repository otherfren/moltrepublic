// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code)]

//! Shared harness of the file-plane integration tests: a real 2-of-2
//! republic over one relay, a share and its persist vote.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;

use crate::common::{adopt_relay, approve_op, read_session, wait_for};

/// A node whose trickle publishes one piece per second.
pub fn engine(root: &std::path::Path, download_dir: &std::path::Path) -> WalletHandle {
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

/// Found a real 2-of-2 republic over the relay: petra (founder, the
/// sharer) and walter (the requester).
pub async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
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
        relays: Vec::new(),
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

pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
}

/// Share `bytes` from petra and return the share's id and content key as
/// walter sees them in his chat.
pub async fn share(a: &WalletHandle, b: &WalletHandle, src: &std::path::Path) -> (molt_core::MessageId, [u8; 32]) {
    a.execute(Command::ShareFile {
        path: src.display().to_string(),
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("the share is admitted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
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
                let file = msg.file.as_ref()?;
                use base64::Engine as _;
                let key = base64::engine::general_purpose::STANDARD.decode(&file.key_b64).ok()?;
                Some((msg.id, <[u8; 32]>::try_from(key).ok()?))
            });
            if let Some(found) = found {
                return found;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "the share never reached walter");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Persist `id` by the 2-of-2 vote: petra proposes, walter approves.
pub async fn persist(a: &WalletHandle, b: &WalletHandle, id: molt_core::MessageId) {
    a.execute(Command::Propose {
        surface: molt_core::Surface::Files,
        payload: serde_json::json!({ "op": "persist", "id": id.to_string() }),
    })
    .await
    .expect("propose persist");
    approve_op(b, "persist").await;
}
