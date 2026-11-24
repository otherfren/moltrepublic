// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A clean close delivers what was recorded before it (delivery guarantee
//! §12): messages answered `ok` an instant before `CloseWorkspace` reach
//! the other seat without the closed seat starting again.

mod common;

use common::read_chat;
use std::time::Duration;

use molt_core::Command;
use molt_engine::WalletHandle;

async fn landed(w: &WalletHandle, bodies: &[String]) -> usize {
    let chat = read_chat(w).await;
    bodies
        .iter()
        .filter(|b| chat.iter().any(|r| r.get("body").and_then(|v| v.as_str()) == Some(b.as_str())))
        .count()
}

/// Whether every body landed within 30 s.
async fn arrives(w: &WalletHandle, bodies: &[String]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while landed(w, bodies).await < bodies.len() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    true
}

async fn chat(w: &WalletHandle, body: &str) {
    w.execute(Command::Chat { body: body.to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("chat");
}

/// Petra's outbox sleeps through the appends (the seam: no race decides
/// what it saw), she sends five and closes at once. Walter holds all five
/// while petra stays closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_sent_right_before_a_close_reach_the_peer_without_a_restart() {
    let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = common::found_pair(tmp.path(), &[&url], "Chess Club").await;
    chat(&a, "warm").await;
    let warm = vec!["warm".to_string()];
    assert!(arrives(&b, &warm).await, "the live path works");

    molt_net::group_runtime::__hold_outbox("petra", true);
    let bodies: Vec<String> = (0..5).map(|i| format!("last {i}")).collect();
    for body in &bodies {
        chat(&a, body).await;
    }
    a.execute(Command::CloseWorkspace).await.expect("close petra");
    molt_net::group_runtime::__hold_outbox("petra", false);

    arrives(&b, &bodies).await;
    assert_eq!(landed(&b, &bodies).await, bodies.len(), "the close left recorded messages unsent");
    b.execute(Command::CloseWorkspace).await.expect("close walter");
}

/// **Joining a second republic closes the open one like `CloseWorkspace`.**
/// The join's seal materializes the new workspace; the open one's outbox
/// takes its last pass before the storage closes, so petra holds the
/// five messages walter recorded just before (walter, not petra: the
/// other test in this file holds petra's outbox).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joining_another_republic_sends_what_the_open_one_recorded() {
    let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (b, a) = common::found_pair(tmp.path(), &[&url], "Chess Club").await;
    chat(&a, "warm").await;
    assert!(arrives(&b, &["warm".to_string()]).await, "the live path works");

    let c = common::engine(&tmp.path().join("other"));
    common::adopt_relay(&c, &url).await;
    c.execute(Command::CreateStart {
        name: "Go Club".to_string(),
        member: "vera".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("second founding");
    let s = common::wait_for(&c, "the seat link", |s| {
        !s.create.seats.is_empty() && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    molt_net::group_runtime::__hold_outbox("walter", true);
    let bodies: Vec<String> = (0..5).map(|i| format!("before the join {i}")).collect();
    for body in &bodies {
        chat(&a, body).await;
    }
    a.execute(Command::JoinStart { invite: link, member: "wim".to_string(), relays: Vec::new() })
        .await
        .expect("join starts");
    common::wait_for(&c, "the join", |s| s.create.can_propose).await;
    c.execute(Command::CreatePropose {
        name: "Go Club".to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter");
    let seed = common::read_session(&c).await.create.seed.clone();
    c.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    common::wait_for(&a, "the charter", |s| s.join.awaiting_ratify).await;
    a.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let seed = common::read_session(&a).await.join.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    common::wait_for(&c, "the seal", |s| s.create.run.outcome == 1).await;
    c.execute(Command::CreateFinish).await.expect("create finish");
    common::wait_for(&a, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
    a.execute(Command::JoinFinish).await.expect("join finish");
    common::wait_for(&a, "walter in the second republic", |s| {
        s.workspaces.len() == 2 && s.screen == molt_core::Screen::Main
    })
    .await;
    molt_net::group_runtime::__hold_outbox("walter", false);

    arrives(&b, &bodies).await;
    assert_eq!(landed(&b, &bodies).await, bodies.len(), "the switch left recorded messages unsent");
}
