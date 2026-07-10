// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The demo mesh end to end: the old reply simulator is gone — a demo
//! reply now travels peer engine → peer outbox → loopback hub → this
//! engine's supervisor → `NetDelivered` → the chat log. Deterministic:
//! the brains draw from per-name seeds.

mod common;

use std::time::Duration;

use common::read_chat;
use molt_core::{Command, Event, GroupConfig, Reply, SessionView};

/// Post a few messages and wait for a loopback peer to answer through the
/// real transport path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_peers_answer_over_the_loopback_mesh() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let mut ev = w.subscribe();
    for i in 0..4 {
        w.execute(Command::Chat {
            body: format!("hello {i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
    }
    // await the first peer reply event (brains answer ~1/3 of messages,
    // 1.5–6.5 s later, from deterministic per-name seeds)
    let reply_from = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(Event::Chat { from, .. }) = ev.recv().await {
                if from != "me" {
                    return from;
                }
            }
        }
    })
    .await
    .expect("a peer reply arrives");
    assert!(
        ["peer-1", "peer-2"].contains(&reply_from.as_str()),
        "reply came from a roster peer, got `{reply_from}`"
    );

    // the reply is a real chat log entry, recorded via NetDelivered
    let chat = read_chat(&w).await;
    assert!(chat.len() >= 5, "own messages plus at least one reply");
    assert!(
        chat.iter().any(|m| m["from"] == serde_json::json!(reply_from)),
        "the reply landed in the shared chat log"
    );
}

/// Dropping the last operator handle stops the engine — even with a live
/// mesh. Regression test for the reference cycle where supervisor tasks
/// held strong senders into their own engine's queue: the actor could
/// never exit, and every torn-down demo peer leaked its engine,
/// supervisor and brain forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_and_mesh_shut_down_when_the_last_handle_drops() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let mut ev = w.subscribe();
    w.execute(Command::Chat {
        body: "build the mesh".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat");
    drop(w);
    // the event channel closes only when the actor's State drops — which
    // requires the actor loop to exit despite the running supervisor
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ev.recv().await {
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => continue,
            }
        }
    })
    .await
    .expect("the engine actor stops once the last handle is gone");
}

/// On a session-only demo workspace the mesh is built from the workspace
/// roster, and a peer's reply flips its presence pill to live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_workspace_mesh_updates_presence_on_reply() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    w.execute(Command::OpenWorkspace {
        id: molt_core::demo_workspace_id("Family Office"),
    })
    .await
    .expect("open");
    let mut ev = w.subscribe();
    for i in 0..4 {
        w.execute(Command::Chat {
            body: format!("family checkin {i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
    }
    let reply_from = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(Event::Chat { from, .. }) = ev.recv().await {
                if from != "me" {
                    return from;
                }
            }
        }
    })
    .await
    .expect("a workspace peer replies");

    match w.execute(Command::ReadSession).await.expect("session") {
        Reply::Session(s) => {
            let ws = s
                .workspaces
                .iter()
                .find(|ws| ws.name == "Family Office")
                .expect("entry");
            let member = ws
                .members
                .iter()
                .find(|m| m.name == reply_from)
                .expect("the replier is a roster member");
            assert_eq!(member.state, 0, "presence pill is live");
            assert_eq!(member.last, "just now");
            // offline members never joined the mesh, so they never spoke
            let offline = ws.members.iter().find(|m| m.name == "notary").expect("notary");
            assert_eq!(offline.state, 2, "offline members stay offline");
        }
        other => panic!("unexpected: {other:?}"),
    }
}
