// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M4 keystones - the mirror worker over a real 2-of-2 republic**
//! (`docs/files/mirroring.md` §3.3): a persisted share is complete in the
//! second seat's mirror folder without anyone downloading it, and the
//! sharer reads that seat as a holder; a quota too small stops the worker
//! with ONE notice and no fetch.

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

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
}

/// Share `bytes` from petra and return the share's id and content key as
/// walter sees them in his chat.
async fn share(a: &WalletHandle, b: &WalletHandle, src: &std::path::Path) -> (molt_core::MessageId, [u8; 32]) {
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

async fn read_mirror(w: &WalletHandle) -> Box<molt_core::MirrorView> {
    match w.execute(Command::ReadMirror).await.expect("read mirror") {
        Reply::Mirror(v) => v,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_mirror(w: &WalletHandle, what: &str, pred: impl Fn(&molt_core::MirrorView) -> bool) -> Box<molt_core::MirrorView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = read_mirror(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}; view={v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}


/// The second voice: wait until `w` sees the open proposal with this
/// `op`, then approve it.
async fn approve_op(w: &WalletHandle, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals } = w.execute(Command::ListProposals).await.expect("list") {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(|v| v.as_str()) == Some(op)
            }) {
                w.execute(Command::Approve { proposal: p.id }).await.expect("approve");
                return;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "the {op} proposal never reached the second voice");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Persist `id` by the 2-of-2 vote: petra proposes, walter approves.
async fn persist(a: &WalletHandle, b: &WalletHandle, id: molt_core::MessageId) {
    a.execute(Command::Propose {
        surface: molt_core::Surface::Files,
        payload: serde_json::json!({ "op": "persist", "id": id.to_string() }),
    })
    .await
    .expect("propose persist");
    approve_op(b, "persist").await;
}

/// Petra shares a two-piece file and the republic persists it. Walter
/// never downloads it - his worker mirrors it: the pieces land sealed in
/// his mirror folder, his view says held == of, and petra reads walter
/// as a whole holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_persisted_share_is_mirrored_by_the_second_seat() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("zwei.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, key) = share(&a, &b, &src).await;
    persist(&a, &b, id).await;

    wait_mirror(&a, "walter as a whole holder", |v| {
        v.files
            .iter()
            .any(|f| f.id == id && f.holders == vec!["petra".to_string(), "walter".to_string()])
    })
    .await;
    let mine = read_mirror(&b).await;
    let row = mine.files.iter().find(|f| f.id == id).expect("walter's row");
    assert_eq!((row.held, row.of), (2, 2), "{row:?}");
    assert_eq!(mine.used, u64::try_from(bytes.len()).expect("fits"), "{mine:?}");
    // the folder holds every piece sealed: two data, one chunk, the top
    let ws_b = read_session(&b).await.active_workspace.clone();
    let dir = root.join("member").join("..").join("mirror").join(&ws_b).join(id.to_string());
    for index in 0..4u32 {
        let content = std::fs::read_to_string(dir.join(index.to_string())).expect("a stored piece");
        let (got, count, payload) = molt_net::file_plane::open_piece(&key, &content).expect("opens under the key");
        assert_eq!((got, count), (index, 2));
        if index == 0 {
            assert_eq!(payload, &bytes[..molt_net::file_plane::PIECE_PAYLOAD_LEN]);
        }
    }
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// Walter's quota is one byte: the worker stops before the first piece,
/// says so ONCE on the notice channel, and petra never reads him as a
/// holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quota_too_small_stops_the_worker_with_one_notice() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    b.execute(Command::SetMirror { on: true, quota_bytes: 1 }).await.expect("set mirror");
    let bytes = pattern(100);
    let src = tmp.path().join("klein.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    persist(&a, &b, id).await;
    wait_for(&b, "the quota notice", |s| s.notice == "mirror-quota:0:1").await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let mine = read_mirror(&b).await;
    let row = mine.files.iter().find(|f| f.id == id).expect("walter's row");
    assert_eq!((row.held, mine.used), (0, 0), "{mine:?}");
    let theirs = read_mirror(&a).await;
    assert!(
        theirs.files.iter().any(|f| f.id == id && f.holders == vec!["petra".to_string()]),
        "{theirs:?}"
    );
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
