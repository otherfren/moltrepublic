// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M2 keystones - the trickle sender and the resumable fetch over a
//! real 2-of-2 republic** (`docs_archive/files/mirroring.md` §3.2): a share
//! downloads end to end with the sharer publishing one piece per second;
//! a requester closed and reopened mid-way resumes its job at the bitmap;
//! a piece the relay lost is asked for (`PieceWanted`) and re-published.

mod common;
mod file_support;

use common::read_session;
use file_support::{found_pair, pattern, share};
use std::time::Duration;

use molt_core::{Command, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

async fn download_view(w: &WalletHandle, id: molt_core::MessageId) -> Option<molt_core::DownloadView> {
    match w.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads.into_iter().find(|u| u.id == id).and_then(|u| u.download),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Wait until `name` lands in `dir` with `bytes`.
async fn wait_landed(w: &WalletHandle, id: molt_core::MessageId, dir: &std::path::Path, name: &str, bytes: &[u8], secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let landed = std::fs::read_dir(dir).ok().and_then(|mut it| {
            it.find_map(|e| {
                let p = e.ok()?.path();
                (p.file_name()?.to_string_lossy() == name).then_some(p)
            })
        });
        if let Some(p) = landed {
            assert_eq!(std::fs::read(&p).expect("read landed"), bytes, "the landed bytes are the shared bytes");
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the download never landed; view: {:?}",
            download_view(w, id).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Petra shares a five-piece file; walter's download makes petra's
/// trickle publish the series at one piece per second, and the file lands
/// with the shared bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_five_piece_download_completes_over_the_trickle() {
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
    let bytes = pattern(4 * molt_net::file_plane::PIECE_PAYLOAD_LEN + 777);
    let src = tmp.path().join("fuenf.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    let started = tokio::time::Instant::now();
    b.execute(Command::DownloadFile { id, dest: None }).await.expect("download starts");
    wait_landed(&b, id, &root.join("dl-b"), "fuenf.bin", &bytes, 60).await;
    // seven pieces at one per second: a burst would land in well under that
    assert!(started.elapsed() >= Duration::from_secs(5), "trickled, not burst: {:?}", started.elapsed());
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// Walter's workspace closes mid-download and reopens: the job resumes at
/// its persisted bitmap (the view shows the earlier progress, never
/// "failed"), and the file lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requester_restarted_mid_way_resumes_its_job() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(7 * molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("acht.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    b.execute(Command::DownloadFile { id, dest: None }).await.expect("download starts");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    let before = loop {
        let view = download_view(&b, id).await;
        if let Some(v) = &view {
            assert_ne!(v.phase, "failed", "{v:?}");
            if v.phase == "transferring" && v.percent >= 25 {
                break v.percent;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no progress: {view:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let ws_b = read_session(&b).await.active_workspace.clone();
    // the bitmap persists at most a second behind the pieces
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    b.execute(Command::CloseWorkspace).await.expect("close b");
    tokio::time::sleep(Duration::from_secs(2)).await;
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    let view = download_view(&b, id).await.expect("the job resumed into the view");
    assert_ne!(view.phase, "failed", "{view:?}");
    assert!(view.percent >= before, "resumed at the bitmap, not restarted: {view:?} before={before}");
    wait_landed(&b, id, &root.join("dl-b"), "acht.bin", &bytes, 60).await;
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// The relay loses one data piece after the series was published. A
/// second download of the same share replays what the relay has, goes
/// quiet, asks the sharer for the missing piece, and completes once the
/// trickle re-publishes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_piece_the_relay_lost_is_recovered_through_piece_wanted() {
    molt_engine::__set_piece_want_after(Duration::from_secs(2));
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = LocalRelay::new(RelayBuilder::default().database(db.clone()));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(2 * molt_net::file_plane::PIECE_PAYLOAD_LEN + 99);
    let src = tmp.path().join("drei.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, key) = share(&a, &b, &src).await;
    let first = tmp.path().join("first");
    std::fs::create_dir_all(&first).expect("first dir");
    b.execute(Command::DownloadFile { id, dest: Some(first.display().to_string()) })
        .await
        .expect("first download");
    wait_landed(&b, id, &first, "drei.bin", &bytes, 60).await;

    // the relay loses data piece 1
    let pieces = db
        .query(Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_FILE_CHUNK)))
        .await
        .expect("query pieces");
    let lost = pieces
        .iter()
        .find(|e| matches!(molt_net::file_plane::open_piece(&key, &e.content), Ok((1, _, _))))
        .expect("piece 1 is on the relay");
    db.delete(Filter::new().id(lost.id)).await.expect("delete piece 1");
    let left = db
        .query(Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_FILE_CHUNK)))
        .await
        .expect("query again")
        .len();
    assert_eq!(left + 1, pieces.len(), "exactly one piece is gone");

    let second = tmp.path().join("second");
    std::fs::create_dir_all(&second).expect("second dir");
    b.execute(Command::DownloadFile { id, dest: Some(second.display().to_string()) })
        .await
        .expect("second download");
    wait_landed(&b, id, &second, "drei.bin", &bytes, 90).await;
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
