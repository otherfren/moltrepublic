// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M3 keystone - the mirror gossip over a real 2-of-2 republic**
//! (`docs_archive/files/mirroring.md` §3.4): a seat's declaration reaches the
//! peer, a shared file shows its sharer as a whole holder on the peer,
//! and both survive the peer's close and reopen.

mod common;
mod file_support;

use common::read_session;
use file_support::{found_pair, pattern, share};
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

const THREE_GIB: u64 = 3 * 1024 * 1024 * 1024;

/// Walter declares (off, 3 GiB): petra reads it. Petra shares a file:
/// walter reads petra as its whole holder, in `read_mirror` and in the
/// uploads row. Walter closes and reopens: both facts persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declaration_and_a_whole_hold_reach_the_peer_and_persist() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;

    b.execute(Command::SetMirror { on: false, quota_bytes: THREE_GIB })
        .await
        .expect("set mirror");
    let mine = read_mirror(&b).await;
    assert!(!mine.on && mine.quota == THREE_GIB, "{mine:?}");
    wait_mirror(&a, "walter's declaration", |v| {
        v.members
            .iter()
            .any(|m| m.member == "walter" && m.known && !m.on && m.quota == THREE_GIB)
    })
    .await;
    let seen = read_mirror(&a).await;
    let petra = seen.members.iter().find(|m| m.member == "petra").expect("own row");
    assert!(petra.known && petra.on && petra.quota == molt_core::MIRROR_QUOTA_DEFAULT, "{petra:?}");

    let bytes = pattern(molt_net::file_plane::PIECE_PAYLOAD_LEN + 5);
    let src = tmp.path().join("zwei.bin");
    std::fs::write(&src, &bytes).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    wait_mirror(&b, "petra as the holder", |v| {
        v.files.iter().any(|f| f.id == id && f.holders == vec!["petra".to_string()] && f.of == 2)
    })
    .await;
    let rows = match b.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    };
    let row = rows.iter().find(|u| u.id == id).expect("the share row");
    assert_eq!((row.mirrors, row.mirror_held, row.mirror_of), (1, 0, 2), "{row:?}");
    let own = match a.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    };
    let own_row = own.iter().find(|u| u.id == id).expect("the sharer's row");
    assert_eq!((own_row.mirrors, own_row.mirror_held, own_row.mirror_of), (1, 2, 2), "{own_row:?}");

    let ws_b = read_session(&b).await.active_workspace.clone();
    b.execute(Command::CloseWorkspace).await.expect("close b");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    let again = read_mirror(&b).await;
    assert!(!again.on && again.quota == THREE_GIB, "own declaration persisted: {again:?}");
    assert!(
        again.files.iter().any(|f| f.id == id && f.holders == vec!["petra".to_string()]),
        "petra's holding persisted: {again:?}"
    );
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
