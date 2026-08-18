// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Poke, end to end over the mesh**: a directed nudge crosses the wire
//! like chat, and the target reacts only behind its own opt-in — an emitted
//! [`Event::Poked`] (what a GUI toasts and sounds) plus the configured wake
//! command (how a sleeping agent harness gets its prompt).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{found_with_mesh, CaptureSink};
use molt_core::{Command, Event, EventEnvelope, WorkspaceEvent};
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, PeerLink};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poke_crosses_the_mesh_and_wakes_the_opted_in_target() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, _id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter");

    // member-b's runtime supervisor on the shared hub; the test drives its
    // feed and reads its sink in place of a second engine
    let link = member_mesh
        .iter()
        .filter_map(PeerLink::from_mesh)
        .next()
        .expect("the member's link to the founder");
    let member_group =
        Arc::new(Mutex::new(MlsMember::restore(&member_mls).expect("restore member MLS")));
    let member_sink = CaptureSink::default();
    let member_feed = MemLog::new();
    let (member_wake, wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![link.clone()], 33),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        wake_rx,
        Some(MlsChannel::from_shared(member_group.clone())),
    );

    // outbound leg: poking is an explicit opt-in, then the poke crosses
    let refused = a
        .execute(Command::Poke {
            member: "member-b".to_string(),
        })
        .await;
    assert!(refused.is_err(), "poking without the opt-in must refuse");

    let marker = tmp.path().join("wake-marker");
    a.execute(Command::PatchSettings {
        patch: serde_json::json!({
            "poke_enabled": true,
            "poke_wake_command":
                format!("echo \"$MOLT_WAKE_REASON $MOLT_WAKE_BY\" > '{}'", marker.display()),
        }),
    })
    .await
    .expect("enable poking + arm the wake hook");
    a.execute(Command::Poke {
        member: "member-b".to_string(),
    })
    .await
    .expect("poke member-b");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !member_sink.messages().iter().any(|(from, env)| {
        from == "founder-a"
            && matches!(&env.body, WorkspaceEvent::Poked { to } if to == "member-b")
    }) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the poke never crossed the mesh"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // inbound leg: member-b pokes the founder — the founder's engine emits
    // Event::Poked (toast + sound in a GUI) and runs the wake command
    let mut ev = a.subscribe();
    member_feed.push(EventEnvelope {
        prev_seq: 0,
        seq: 9,
        ts: 9,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Poked {
            to: "founder-a".to_string(),
        },
    });
    let _ = member_wake.send(9);

    let by = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Event::Poked { by, to }) = ev.recv().await {
                if to == "founder-a" {
                    return by;
                }
            }
        }
    })
    .await
    .expect("the founder never reacted to the poke");
    assert_eq!(by, "member-b", "the toastable event names the poker");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let content = loop {
        if let Ok(c) = std::fs::read_to_string(&marker) {
            if !c.is_empty() {
                break c;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wake command never ran"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        content.trim(),
        "poked member-b",
        "the wake command carries its context env vars"
    );
}
