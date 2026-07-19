// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Mesh self-heal Stage 2: idle-queue keepalive** (`documents/mesh_selfheal.md`).
//!
//! A founder with a live 2-of-2 mesh; the member side is a real supervisor
//! whose sink records every `peer_seen` sighting and every delivery. Pinned:
//!
//! 1. **An idle mesh emits a keepalive that stamps presence but no event.**
//!    `NetMeshKeepaliveTick` sends a keepalive ping that reaches the member
//!    and fires `peer_seen("founder-a")` — WITHOUT delivering any chat/event.
//!    This is the liveness signal that keeps a quiet-but-alive peer from
//!    tripping the Stage 1 deaf-leg detector, and warms the server queue.
//! 2. **An actively-chatting mesh sends no keepalive.** Right after a chat has
//!    crossed the wire, the same tick is gated off — no extra sighting lands.

mod common;

use std::time::Duration;

use common::{found_with_mesh, read_session, CaptureSink};
use molt_core::{ChannelRef, Command, NetHealth, WorkspaceEvent};
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, PeerLink};
use tokio::sync::watch;

/// Stand up the surviving member's runtime supervisor over the shared hub,
/// returning its capture sink.
fn spawn_member(
    hub: molt_engine::RitualTransport,
    member_mesh: &[molt_core::MeshLink],
    member_mls: &[u8],
) -> (CaptureSink, supervisor::SupervisorHandle) {
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(member_mls).expect("restore member MLS");
    let sink = CaptureSink::default();
    let (_wake, wake_rx) = watch::channel(0u64);
    let handle = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 11),
        MemLog::new(),
        MemStateStore::new(),
        sink.clone(),
        wake_rx,
        Some(MlsChannel::new(member_group)),
    );
    (sink, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_keepalive_stamps_presence_and_an_active_mesh_sends_none() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, _id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter the workspace");

    // the member's runtime supervisor, kept alive for the test
    let (member_sink, _member_sup) = spawn_member(hub, &member_mesh, &member_mls);

    // let the founder's mesh legs confirm (honest Ok) so the outbox can send
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sv = read_session(&a).await;
        if sv.net_health == NetHealth::Ok {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder mesh never confirmed Ok: {:?}",
            sv.net_health
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // (1) IDLE: a keepalive tick sends a ping that stamps the member's
    // presence WITHOUT delivering any event. Nothing has chatted, so
    // `last_mesh_out` is untouched and the ping is due.
    a.execute(Command::NetMeshKeepaliveTick)
        .await
        .expect("keepalive tick");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if member_sink.seen().iter().any(|m| m == "founder-a") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the idle keepalive never stamped the member's presence"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        member_sink.messages().is_empty(),
        "a keepalive carries no event — nothing must be delivered: {:?}",
        member_sink.messages().iter().map(|(f, _)| f).collect::<Vec<_>>()
    );

    // (2) ACTIVE: a chat crosses the wire (stamping `last_mesh_out`), then the
    // same tick is gated off — the chat's own delivery stamps presence once,
    // but the suppressed keepalive adds no further sighting.
    a.execute(Command::Chat {
        body: "still here".to_string(),
        quote: None,
        channel: ChannelRef::default(),
    })
    .await
    .expect("chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if member_sink
            .messages()
            .iter()
            .any(|(from, env)| from == "founder-a"
                && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "still here"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat never reached the member"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sightings_after_chat = member_sink.seen().len();
    a.execute(Command::NetMeshKeepaliveTick)
        .await
        .expect("gated keepalive tick");
    // a keepalive, if it had been sent, would arrive well within this window
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        member_sink.seen().len(),
        sightings_after_chat,
        "an actively-chatting mesh must send no extra keepalive"
    );
}
