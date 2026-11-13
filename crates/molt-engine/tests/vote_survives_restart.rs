// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **An own vote survives a restart** (field report v0.0.11,
//! `docs/reviews/known_debt.md`): an `approve` on an own open wiki
//! proposal read `approved_by_me: true`; after a moltd restart the same
//! proposal read open again. Two real engines of a 2-of-2 republic, so the
//! vote stays open (one signature of two).

mod common;

use common::{adopt_relay, engine, read_session, wait_for};

use molt_core::{AllowWarnings, Command, Reply, WikiEdit};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Votes".to_string(),
        member: "mara".to_string(),
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
    let b = engine(&root.join("member"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart { invite: link, member: "walter".to_string(), relays: Vec::new() })
        .await
        .expect("join start");
    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Votes".to_string(), agenda: String::new(), features: vec!["memory".to_string()] })
        .await
        .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let seed = read_session(&b).await.join.seed.clone();
    b.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    (a, b)
}


async fn mine(w: &WalletHandle, id: u64) -> (bool, usize) {
    match w.execute(Command::ReadProposal { id }).await.expect("read proposal") {
        Reply::Proposals { proposals, .. } => {
            let p = proposals.first().expect("the proposal");
            (p.approved_by_me, p.approvals)
        }
        other => panic!("unexpected: {other:?}"),
    }
}

async fn propose_and_approve(a: &WalletHandle) -> u64 {
    let reply = a
        .execute(Command::WikiEdit {
            edits: vec![WikiEdit::Create { path: concat!("a", ".md").into(), content: "one\n".into() }],
            dry_run: false,
            allow_warnings: AllowWarnings::All(true),
            supersedes: None,
            repair_links: true,
        })
        .await
        .expect("wiki edit");
    let Reply::Proposed { id, .. } = reply else { panic!("unexpected: {reply:?}") };
    a.execute(Command::Approve { proposal: id, note: None }).await.expect("approve");
    id.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_vote_survives_a_close_and_reopen() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let root = tempfile::tempdir().expect("tmp");
    let (a, _b) = found_pair(root.path(), &url).await;
    let id = propose_and_approve(&a).await;
    assert_eq!(mine(&a, id).await, (true, 1), "the vote counts before the restart");

    let ws = read_session(&a).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");
    let after = mine(&a, id).await;
    assert_eq!(after, (true, 1), "the own vote survives a close and reopen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_vote_survives_a_process_restart() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let root = tempfile::tempdir().expect("tmp");
    let (a, _b) = found_pair(root.path(), &url).await;
    let id = propose_and_approve(&a).await;
    assert_eq!(mine(&a, id).await, (true, 1));

    let ws = read_session(&a).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("clean close, like SIGTERM");
    drop(a);
    let a = engine(&root.path().join("founder"));
    a.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen in a fresh engine");
    let after = mine(&a, id).await;
    assert_eq!(after, (true, 1), "the own vote survives a process restart");
}
