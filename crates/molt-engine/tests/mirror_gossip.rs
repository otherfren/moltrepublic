// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M3 keystone - the mirror gossip over a real 2-of-2 republic**
//! (`docs_archive/files/mirroring.md` §3.4): a seat's declaration reaches the
//! peer, a shared file shows its sharer as a whole holder on the peer,
//! and both survive the peer's close and reopen.

mod common;
mod file_support;

use common::read_session;
use file_support::{found_pair, pattern, read_mirror, share, uploads, wait_mirror};

use molt_core::Command;
use nostr_relay_builder::MockRelay;

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
    let rows = uploads(&b).await;
    let row = rows.iter().find(|u| u.id == id).expect("the share row");
    assert_eq!((row.mirrors, row.mirror_held, row.mirror_of), (1, 0, 2), "{row:?}");
    let own = uploads(&a).await;
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

/// Gap 24, end to end: a sharer that reopens holding nothing (its file
/// replaced, as on a restored seat's new device) says so, and the peer
/// stops listing it as the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_that_reopens_holding_nothing_clears_its_old_claim() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;

    let src = tmp.path().join("eins.bin");
    std::fs::write(&src, pattern(1000)).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    wait_mirror(&b, "petra as the holder", |v| {
        v.files.iter().any(|f| f.id == id && f.holders == vec!["petra".to_string()])
    })
    .await;

    let ws_a = read_session(&a).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("close a");
    std::fs::write(&src, pattern(999)).expect("replace source");
    a.execute(Command::OpenWorkspace { id: ws_a }).await.expect("reopen a");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let v = read_mirror(&b).await;
        if v.files.iter().all(|f| f.id != id || f.holders.is_empty()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "petra's old claim stayed: {v:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
