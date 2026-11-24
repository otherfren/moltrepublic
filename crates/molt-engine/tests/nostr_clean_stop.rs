// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A clean close delivers what was recorded before it (delivery guarantee
//! §4.11): messages answered `ok` an instant before `CloseWorkspace` reach
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
