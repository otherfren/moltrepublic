// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The demo mesh end to end — behind its **test seam** (`__spawn_demo_mesh`,
//! default OFF like `ritual_sim`): a demo reply travels peer engine → peer
//! outbox → loopback hub → this engine's supervisor → `NetDelivered` → the
//! chat log. Deterministic: the brains draw from per-name seeds. The
//! production spawners never build this mesh — the negative tests below pin
//! that no context of a production engine grows fake peers.

mod common;

use std::time::Duration;

use common::{await_founding, read_chat};
use molt_core::{Command, Event, GroupConfig, Reply, SessionView};
use molt_engine::WalletHandle;

/// Post `n` chats as `own` and require SILENCE: no chat event from anyone
/// else within a window comfortably past the old brains' max reply delay
/// (1.5–6.5 s), and a log holding exactly the own messages afterwards.
async fn expect_no_peer_reply(w: &WalletHandle, own: &str, n: usize) {
    let mut ev = w.subscribe();
    for i in 0..n {
        w.execute(Command::Chat {
            body: format!("anyone there {i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
    }
    let reply = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(Event::Chat { from, .. }) = ev.recv().await {
                if from != own {
                    return from;
                }
            }
        }
    })
    .await;
    assert!(
        reply.is_err(),
        "no fake peer may answer in production, got a reply from {reply:?}"
    );
    let chat = read_chat(w).await;
    assert_eq!(chat.len(), n, "the log holds exactly the operator's own messages");
    assert!(chat.iter().all(|m| m["from"] == serde_json::json!(own)));
}

/// Post a few messages and wait for a loopback peer to answer through the
/// real transport path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_peers_answer_over_the_loopback_mesh() {
    let w = molt_engine::__spawn_demo_mesh(GroupConfig::demo(), SessionView::default());
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
    let w = molt_engine::__spawn_demo_mesh(GroupConfig::demo(), SessionView::default());
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
    let w = molt_engine::__spawn_demo_mesh(GroupConfig::demo(), SessionView::default());
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

/// PRODUCTION spawns no fake peers: the demo mesh lives behind the
/// `__spawn_demo_mesh` test seam (default OFF, like `ritual_sim`). A
/// public `spawn` — the same engine `moltd` embeds — that chats without an
/// open workspace gets a local-only scratch log: nobody answers, and the
/// log never grows a message the operator did not write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_spawn_runs_no_demo_mesh() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    expect_no_peer_reply(&w, "me", 6).await;
}

/// `prefs.simulated_members` is INERT for the mesh: a persisted workspace
/// flagged as simulated (a sim-seam founding persists the flag) no longer
/// stands fake peers up on a production engine — the flag stays parsed,
/// the mesh stays down, chat stays local.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_members_flag_spawns_no_fake_peers() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: molt_core::SessionSettings {
            workspace_dir: tmp.path().join("workspaces").display().to_string(),
            ..molt_core::SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(GroupConfig::demo(), session, true);
    w.execute(Command::CreateStart {
        name: "Inert Flag".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
    })
    .await
    .expect("create start");
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("finish");
    expect_no_peer_reply(&w, "petra", 6).await;
}

/// The streaming `Proposed` event names its proposer: a frontend must be
/// able to tell an own proposal (quiet feedback) from a peer's (alert
/// sound) — the GUI only rings for votes somebody ELSE initiated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposed_event_carries_the_proposer() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let mut ev = w.subscribe();
    w.execute(Command::Propose {
        surface: molt_core::Surface::Organization,
        payload: serde_json::json!({
            "op": "set_name",
            "title": "Namen ändern",
            "value": "Umbenannt",
        }),
    })
    .await
    .expect("propose");
    let by = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Event::Proposed { by, .. }) = ev.recv().await {
                return by;
            }
        }
    })
    .await
    .expect("the Proposed event arrives");
    assert_eq!(by, "me", "a local proposal is attributed to the local member");
}
