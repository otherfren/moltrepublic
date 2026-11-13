// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Temporary threads over the wire** (`docs/chat_threads_lifecycle.md`):
//! two real engines of a 2-of-2 republic. A close made on one seat reaches
//! the other and refuses its speech there too, a reopen lifts it on both,
//! and a forget stays on the seat that forgot.

mod common;

use common::{adopt_relay, engine, read_session, wait_for};
use std::time::Duration;

use molt_core::{ChannelInfo, ChannelRef, Command, MoltError, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Threads".to_string(),
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
    a.execute(Command::CreatePropose { name: "Threads".to_string(), agenda: String::new(), features: Vec::new() })
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

fn topic() -> ChannelRef {
    ChannelRef::Topic { name: "plan".into() }
}

async fn thread(w: &WalletHandle) -> Option<ChannelInfo> {
    match w
        .execute(Command::ReadState { surface: Surface::Chat, channel: None, view: None })
        .await
        .expect("read")
    {
        Reply::State(s) => s.channels.into_iter().find(|c| c.channel == topic()),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn until(w: &WalletHandle, what: &str, pred: impl Fn(&Option<ChannelInfo>) -> bool) {
    for _ in 0..600 {
        if pred(&thread(w).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}: {:?}", thread(w).await);
}

async fn say(w: &WalletHandle, body: &str) -> Result<Reply, MoltError> {
    w.execute(Command::Chat { body: body.into(), quote: None, channel: topic() }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_reaches_the_other_seat_and_a_forget_stays_home() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let root = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(root.path(), &url).await;

    say(&a, "let us plan").await.expect("speech");
    until(&b, "the thread on walter's seat", |t| t.as_ref().is_some_and(|t| t.count == 1)).await;

    b.execute(Command::ThreadClose { name: "plan".into() }).await.expect("close");
    until(&a, "the close on mara's seat", |t| {
        t.as_ref().is_some_and(|t| t.closed && t.closed_by == "walter")
    })
    .await;
    let refused = say(&a, "one more").await;
    assert!(matches!(refused, Err(MoltError::ThreadClosed(_))), "{refused:?}");

    a.execute(Command::ThreadReopen { name: "plan".into() }).await.expect("reopen");
    until(&b, "the reopen on walter's seat", |t| t.as_ref().is_some_and(|t| !t.closed)).await;

    b.execute(Command::ThreadForget { name: "plan".into() }).await.expect("forget");
    assert!(thread(&b).await.is_none(), "forgotten on walter's seat");
    assert!(thread(&a).await.is_some(), "mara keeps the thread");

    say(&a, "back again").await.expect("speech");
    until(&b, "the thread back on walter's seat", |t| t.as_ref().is_some_and(|t| t.count == 1)).await;
}
