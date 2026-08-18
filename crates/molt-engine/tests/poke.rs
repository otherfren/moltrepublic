// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Poke, end to end over the mesh**: the nudge crosses the wire as a
//! CONTROL FRAME (never a log event — that is what keeps an older build able
//! to open the workspace), and the target reacts only behind its own opt-in:
//! an emitted [`Event::Poked`] (what a GUI toasts and sounds) plus the
//! configured wake command (how a sleeping agent harness gets its prompt).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{found_with_mesh, CaptureSink};
use molt_core::{Command, Event, WorkspaceEvent};
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, PeerLink};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poke_crosses_the_mesh_and_wakes_the_opted_in_target() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
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
    let wake = format!(
        "echo \"$MOLT_WAKE_REASON $MOLT_WAKE_BY\" > '{}'",
        marker.display()
    );
    // the wake command has its own door: the wholesale settings paths refuse
    // it, so an MCP client can never plant a shell command
    assert!(
        a.execute(Command::PatchSettings {
            patch: serde_json::json!({ "poke_wake_command": wake.clone() }),
        })
        .await
        .is_err(),
        "patch_settings must not be able to set a shell command"
    );
    a.execute(Command::PatchSettings {
        patch: serde_json::json!({ "poke_enabled": true }),
    })
    .await
    .expect("enable poking");
    a.execute(Command::SetWakeCommand {
        command: wake.clone(),
    })
    .await
    .expect("arm the wake hook");

    a.execute(Command::Poke {
        member: "member-b".to_string(),
    })
    .await
    .expect("poke member-b");

    // the poke leaves as a CONTROL FRAME: nothing lands in the log, and the
    // member's sink (which only ever sees application envelopes) stays empty
    // of it. What proves the wire crossing is the inbound leg below.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !member_sink
            .messages()
            .iter()
            .any(|(_, env)| !matches!(&env.body, WorkspaceEvent::Chat(_))),
        "a poke must not appear as an application envelope"
    );

    // inbound leg: member-b pokes the founder over the SAME control-frame
    // path the engine uses — the founder's engine emits Event::Poked (toast
    // + sound in a GUI) and runs the wake command
    let mut ev = a.subscribe();
    let frame = molt_net::poke::Poke::new("member-b".to_string(), "founder-a".to_string())
        .to_frame();
    let ct = member_group
        .lock()
        .expect("mls lock")
        .encrypt(&frame)
        .expect("encrypt the poke");
    let founder_link = molt_net::PeerLink::from_mesh(
        member_mesh.first().expect("the member's mesh link"),
    )
    .expect("peer link");
    molt_net::supervisor::send_framed(
        &hub,
        founder_link.snd0(),
        &founder_link.wrap_out,
        molt_net::MsgId([9u8; 16]),
        &ct,
    )
    .await
    .expect("send the poke frame");
    let _ = (&member_feed, &member_wake);

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
