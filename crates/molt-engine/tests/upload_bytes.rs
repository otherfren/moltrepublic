// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Stage-2 keystone - `ReadUploadBytes` over a real 2-of-2 republic**
//! (`docs_archive/ui/wiki_files_and_images.md` §3.3/§4): the second seat reads a
//! persisted file's bytes out of its MIRROR without ever downloading it,
//! a cap below the size refuses before any read, and a source file
//! swapped behind the share answers a mismatch - never bytes.

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
/// sharer) and walter (the mirroring seat).
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"), &root.join("dl-a"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Bildarchiv".to_string(),
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
        name: "Bildarchiv".to_string(),
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

/// Share `src` from petra and return the share's id as walter sees it.
async fn share(a: &WalletHandle, b: &WalletHandle, src: &std::path::Path) -> molt_core::MessageId {
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
                msg.file.as_ref().map(|_| msg.id)
            });
            if let Some(found) = found {
                return found;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "the share never reached walter");
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
                w.execute(Command::Approve { proposal: p.id, note: None }).await.expect("approve");
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

async fn uploads(w: &WalletHandle) -> Vec<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("read uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn read_bytes(w: &WalletHandle, checksum: &str, cap: u64) -> Result<Vec<u8>, molt_core::MoltError> {
    match w
        .execute(Command::ReadUploadBytes { checksum: checksum.to_string(), cap })
        .await?
    {
        Reply::UploadBytes { bytes } => Ok(bytes),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Petra shares a two-piece file and the republic persists it. Walter
/// never downloads it - his mirror completes, and `ReadUploadBytes`
/// assembles the sealed pieces back into the exact plaintext. A cap
/// below the size refuses before the read, and once petra's source file
/// is swapped her own read answers a mismatch instead of bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirrored_file_reads_its_bytes_without_a_download() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("zwei.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let id = share(&a, &b, &src).await;
    persist(&a, &b, id).await;

    // walter's mirror completes on its own - no download anywhere
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let checksum = loop {
        let row = uploads(&b).await.into_iter().find(|u| u.id == id);
        if let Some(row) = row {
            if row.mirror_of > 0 && row.mirror_held == row.mirror_of {
                assert!(row.download.is_none(), "the bytes arrived by mirroring: {row:?}");
                break row.checksum;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "walter's mirror never completed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(checksum.len(), 64);

    // the job is only a byte source once its manifest pieces are stored
    // too, a beat after the last data piece
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let got = loop {
        match read_bytes(&b, &checksum, 32 << 20).await {
            Ok(got) => break got,
            Err(e) => {
                assert!(tokio::time::Instant::now() < deadline, "the mirror never answered: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    assert_eq!(got, bytes, "the assembled pieces are the file");
    // a 12-hex prefix names the same file
    let got = read_bytes(&b, &checksum[..12], 32 << 20).await.expect("a prefix resolves first");
    assert_eq!(got, bytes);
    assert!(uploads(&b).await.iter().all(|u| u.download.is_none()), "still no download");

    // the cap refuses before any read
    let err = read_bytes(&b, &checksum, 10).await.expect_err("the cap refuses");
    assert!(err.to_string().contains("cap"), "{err}");

    // petra holds the source: her own read answers the same bytes…
    let got = read_bytes(&a, &checksum, 32 << 20).await.expect("the own share answers");
    assert_eq!(got, bytes);
    // …until the file behind the share is swapped. A swap that keeps SIZE
    // and mtime slips past the immutability stamp (D1) - the re-hash is
    // what still refuses it.
    let was = std::fs::metadata(&src).expect("stat").modified().expect("mtime");
    let other: Vec<u8> = bytes.iter().map(|b| b ^ 0x5a).collect();
    std::fs::write(&src, &other).expect("swap the source");
    std::fs::File::options()
        .write(true)
        .open(&src)
        .expect("reopen")
        .set_times(std::fs::FileTimes::new().set_modified(was))
        .expect("restore the mtime");
    let err = read_bytes(&a, &checksum, 32 << 20).await.expect_err("the re-hash refuses");
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
    // a swap the stamp CAN see stops the read one step earlier
    std::fs::write(&src, pattern(1_000)).expect("shrink the source");
    let err = read_bytes(&a, &checksum, 32 << 20).await.expect_err("the stamp refuses");
    assert!(err.to_string().contains("not on this device"), "{err}");
    // …and walter, who has the honest bytes, still answers them
    let got = read_bytes(&b, &checksum, 32 << 20).await.expect("the mirror is unaffected");
    assert_eq!(got, bytes);

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}


/// **Round 3, D2 keystone**: bytes this seat already mirrors are not
/// fetched again. Walter's mirror completes, the relay is stopped, and
/// `download_file` still lands the exact file in his exchange folder -
/// while `read_uploads` says where the bytes are (`local`) and reads
/// `mirrored` now that a seat beside the sharer holds the series. The
/// same node's `wiki_health` names the chain head it measured on (R10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirrored_file_downloads_with_the_relay_stopped() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("zwei.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let id = share(&a, &b, &src).await;
    persist(&a, &b, id).await;

    // walter mirrors the whole series without ever downloading
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let checksum = loop {
        let row = uploads(&b).await.into_iter().find(|u| u.id == id);
        if let Some(row) = row {
            if row.local == "mirrored" {
                assert!(row.download.is_none(), "no download was involved: {row:?}");
                assert_eq!(row.availability, "mirrored", "a seat beside the sharer holds it");
                break row.checksum;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "walter's mirror never completed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // the pieces are a readable source a beat after the last one arrives
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match read_bytes(&b, &checksum, 32 << 20).await {
            Ok(_) => break,
            Err(e) => {
                assert!(tokio::time::Instant::now() < deadline, "the mirror never answered: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    relay.shutdown();

    // …and the download is served off those pieces, with no network at all
    b.execute(Command::DownloadFile { id, dest: None })
        .await
        .expect("the mirrored download is admitted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let landed = loop {
        let row = uploads(&b).await.into_iter().find(|u| u.id == id).expect("row");
        if let Some(d) = &row.download {
            assert_ne!(d.phase, "failed", "the mirror assembly failed: {d:?}");
            if d.phase == "done" {
                break d.path.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mirrored download never finished"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(std::fs::read(&landed).expect("the landed file"), bytes);
    assert!(
        landed.starts_with(&root.join("..").display().to_string())
            || std::path::Path::new(&landed).starts_with(tmp.path()),
        "it landed in walter's exchange folder: {landed}"
    );
    let row = uploads(&b).await.into_iter().find(|u| u.id == id).expect("row");
    assert_eq!(row.local, "downloaded", "the exchange copy is the nearest source now");

    // R10: the hygiene read names the chain it was measured on
    match b.execute(Command::WikiHealth { limit: 0 }).await {
        Ok(Reply::WikiHealth { head, .. }) => assert!(head >= 1, "the head is a real height"),
        Err(molt_core::MoltError::IndexBuilding { .. }) => {}
        other => panic!("unexpected: {other:?}"),
    }
    // D4: the chain rows carry the moment this node's log took the block
    for (who, w) in [("petra", &a), ("walter", &b)] {
        match w.execute(Command::ReadChain).await.expect("read chain") {
            Reply::Chain { blocks, .. } => assert!(
                blocks.iter().any(|bl| bl.ts > 0),
                "{who} has no stamped block: {blocks:?}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
