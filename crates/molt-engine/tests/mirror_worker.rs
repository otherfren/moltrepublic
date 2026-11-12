// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M4 keystones - the mirror worker over a real 2-of-2 republic**
//! (`docs_archive/files/mirroring.md` §3.3): a persisted share is complete in the
//! second seat's mirror folder without anyone downloading it, and the
//! sharer reads that seat as a holder; a quota too small stops the worker
//! with ONE notice and no fetch.

mod common;
mod file_support;

use common::{read_session, wait_for};
use file_support::{found_pair, pattern, persist, share};
use std::time::Duration;

use molt_core::{Command, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

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
    // the quota counts the folder: four sealed blocks (two data, one
    // manifest chunk, the top record)
    assert_eq!(mine.used, 4 * molt_net::trickle::PIECE_WIRE_BYTES, "{mine:?}");
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
