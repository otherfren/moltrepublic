// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Download progress keystones** (`docs_archive/files/download_progress.md`)
//! over a real 2-of-2 republic: a download waits for an offline sharer
//! without a deadline, across a restart, and lands when the sharer is
//! back; cancel removes the partial file and the job; the view counts
//! pieces while the sharer's row says it is serving.

mod common;
mod file_support;

use common::read_session;
use file_support::{found_pair, pattern, share};
use std::time::Duration;

use molt_core::{Command, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;

async fn row(w: &WalletHandle, id: molt_core::MessageId) -> Option<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads.into_iter().find(|u| u.id == id),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn view(w: &WalletHandle, id: molt_core::MessageId) -> Option<molt_core::DownloadView> {
    row(w, id).await.and_then(|u| u.download.map(|d| *d))
}

async fn wait_view(
    w: &WalletHandle,
    id: molt_core::MessageId,
    what: &str,
    secs: u64,
    pred: impl Fn(&molt_core::DownloadView) -> bool,
) -> molt_core::DownloadView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let v = view(w, id).await;
        if let Some(v) = v.as_ref().filter(|v| pred(v)) {
            return v.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn landed(dir: &std::path::Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(dir.join(name)).ok()
}

fn parts(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.ends_with(".part"))
                .collect()
        })
        .unwrap_or_default()
}

/// Petra goes offline before walter asks. Walter's download waits - no
/// failure, asking again - survives walter's own restart, and lands
/// once petra is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_waits_for_an_offline_sharer_across_a_restart() {
    molt_engine::__set_want_repeat(Duration::from_secs(1));
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(2 * molt_net::file_plane::PIECE_PAYLOAD_LEN + 11);
    let src = tmp.path().join("warten.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    let ws_a = read_session(&a).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("petra leaves");

    b.execute(Command::DownloadFile { id, dest: None }).await.expect("download starts");
    let first = wait_view(&b, id, "waiting", 10, |v| v.phase == "waiting" && v.asked > 0).await;
    assert!(!first.wait.is_empty(), "it says what it waits on: {first:?}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = view(&b, id).await.expect("still there");
    assert_eq!(later.phase, "waiting", "no deadline: {later:?}");

    let ws_b = read_session(&b).await.active_workspace.clone();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    b.execute(Command::CloseWorkspace).await.expect("close b");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    wait_view(&b, id, "the park resumed", 10, |v| v.phase == "waiting").await;

    a.execute(Command::OpenWorkspace { id: ws_a }).await.expect("petra is back");
    let dir = root.join("dl-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while landed(&dir, "warten.bin").is_none() {
        assert!(tokio::time::Instant::now() < deadline, "never landed: {:?}", view(&b, id).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(landed(&dir, "warten.bin").expect("landed"), bytes);
    let done = wait_view(&b, id, "done", 10, |v| v.phase == "done").await;
    assert_eq!((done.percent, done.held, done.of), (100, 3, 3), "{done:?}");
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// Walter cancels mid-way: the partial file and the job go (a reopen
/// shows nothing), and a fresh download lands. While it ran, petra's row
/// said she was serving and walter's view counted pieces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_removes_the_partial_file_and_a_retry_lands() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;
    let bytes = pattern(7 * molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("abbruch.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;

    b.execute(Command::DownloadFile { id, dest: None }).await.expect("download starts");
    let mid = wait_view(&b, id, "two pieces", 40, |v| v.phase == "transferring" && v.held >= 2).await;
    assert_eq!((mid.of, mid.size), (8, u64::try_from(bytes.len()).expect("len")), "{mid:?}");
    assert!(mid.bytes > 0 && mid.since > 0 && mid.last_piece > 0, "{mid:?}");
    let serving = row(&a, id).await.and_then(|u| u.serving);
    assert!(serving.as_ref().is_some_and(|s| s.published < s.of), "petra serves: {serving:?}");

    b.execute(Command::CancelDownload { id }).await.expect("cancel");
    assert_eq!(view(&b, id).await.expect("view").phase, "cancelled");
    let dir = root.join("dl-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !parts(&dir).is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the .part stayed: {:?}", parts(&dir));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ws_b = read_session(&b).await.active_workspace.clone();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    b.execute(Command::CloseWorkspace).await.expect("close b");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    assert!(view(&b, id).await.is_none(), "the job is gone");

    b.execute(Command::DownloadFile { id, dest: None }).await.expect("retry");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while landed(&dir, "abbruch.bin").is_none() {
        assert!(tokio::time::Instant::now() < deadline, "never landed: {:?}", view(&b, id).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(landed(&dir, "abbruch.bin").expect("landed"), bytes);
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
