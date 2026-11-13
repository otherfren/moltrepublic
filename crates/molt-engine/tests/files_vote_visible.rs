// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A persist vote is visible to the other member and votable by it**
//! over a real 2-of-2 republic: walter reads petra's open vote on the
//! uploads row and in the files-only proposal list, approves by that
//! id, and the share turns persistent.

mod common;
mod file_support;

use file_support::{found_pair, pattern, share};
use std::time::Duration;

use molt_core::{Command, ProposalFilter, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn uploads(w: &WalletHandle) -> Vec<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_row(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&molt_core::UploadView) -> bool,
) -> molt_core::UploadView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(row) = uploads(w).await.into_iter().find(|r| pred(r)) {
            return row;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_other_member_sees_a_persist_vote_and_approves_it() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;

    let src = tmp.path().join("plan.bin");
    std::fs::write(&src, pattern(1000)).expect("write source");
    let (id, _) = share(&a, &b, &src).await;
    let row = wait_row(&b, "the share", |r| r.id == id).await;
    assert!(row.vote.is_none(), "no vote yet: {:?}", row.vote);

    let pid = match a
        .execute(Command::Propose {
            surface: Surface::Files,
            payload: serde_json::json!({ "op": "persist", "id": id.to_string() }),
        })
        .await
        .expect("propose persist")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };

    let row = wait_row(&b, "the vote on walter's row", |r| r.id == id && r.vote.is_some()).await;
    let vote = row.vote.expect("vote");
    assert_eq!((vote.op.as_str(), vote.proposal), ("persist", pid));
    assert!(vote.mine_open, "it waits on walter's voice");
    assert!(!row.persistent);

    match b
        .execute(Command::ListProposals {
            filter: ProposalFilter::Open,
            limit: 0,
            cursor: None,
            with_texts: false,
            surface: Some(Surface::Files),
        })
        .await
        .expect("list")
    {
        Reply::Proposals { proposals, total, .. } => {
            assert_eq!(total, 1);
            assert_eq!(proposals[0].id, pid);
            assert_eq!(proposals[0].surface, Surface::Files);
        }
        other => panic!("unexpected: {other:?}"),
    }

    b.execute(Command::Approve { proposal: pid, note: None })
        .await
        .expect("walter approves");
    let done = wait_row(&b, "the persistent row", |r| r.id == id && r.persistent).await;
    assert!(done.vote.is_none(), "a decided vote leaves the row: {:?}", done.vote);
    wait_row(&a, "petra's persistent row", |r| r.id == id && r.persistent).await;
}
