// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The delivery guarantee, end to end** (`documents/delivery_guarantee.md`).
//!
//! The user-visible failure this pins (the 2026-07-27 3-node incident): a
//! peer's inbound queue goes silently deaf — the server still answers `OK`
//! to every `SEND`, so the sender's outbox cursor advances past messages
//! nobody will ever read. Before the guarantee, healing the leg (rotate,
//! resubscribe, reopen) did NOT bring those messages back: the cursor said
//! "delivered", and chat / votes / proposals sent during the deaf window
//! were simply gone.
//!
//! Pinned here: the sender-side heal. A message ACCEPTED by the transport
//! into a deaf queue is RE-OFFERED once the leg works again, because the
//! peer never engine-acknowledged it — the reopen's supervisor build rewinds
//! the outbox to the acked floor (`rewind_unacked`) and re-encrypts the
//! unacked tail under a fresh resend epoch.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{found_with_mesh, CaptureSink};
use molt_core::{Command, SessionSettings, SessionView, WorkspaceEvent};
use molt_net::supervisor::{self, send_framed, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, MsgId, PeerLink};
use tokio::sync::watch;

/// A chat body among the sink's captured envelopes, from the founder?
fn has_chat(sink: &CaptureSink, body: &str) -> bool {
    sink.messages().iter().any(|(from, env)| {
        from == "founder-a"
            && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == body)
    })
}

/// The guarantee's keystone: a chat the transport ACCEPTED into a deaf
/// queue (server `OK`, delivery dropped) is resent after the leg heals and
/// the sender reopens — because the member never acked it. Without the
/// rewind (pre-E4) the reopened cursor sat past the lost message and the
/// deaf window's traffic was gone for good.
///
/// The close here is CLEAN (the advanced ratchet persists), so this pins
/// the rewind mechanic in isolation. The hard-kill variant — where the
/// resumed ratchet REGRESSES and the resends must outrun MLS replay
/// rejection — is E6's pin (`a_chat_lost_in_a_deaf_window_survives_a_hard_kill`,
/// needs the debounced MLS persist).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_lost_in_a_deaf_window_is_resent_after_reopen() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter");
    let molt_engine::RitualTransport::Loopback(loopback) = &hub else {
        panic!("found_with_mesh runs on the loopback hub");
    };
    let hub_ctl = loopback.hub();

    // --- the member's runtime supervisor; the test doubles as its "engine":
    // it crafts the delivery ACKs a real engine would send (the shared MLS
    // Arc keeps the ratchet consistent with the supervisor's channel)
    let link = member_mesh
        .iter()
        .filter_map(PeerLink::from_mesh)
        .next()
        .expect("the member's link to the founder");
    let member_group =
        Arc::new(Mutex::new(MlsMember::restore(&member_mls).expect("restore member MLS")));
    let member_sink = CaptureSink::default();
    let (_wake, wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![link.clone()], 21),
        MemLog::new(),
        MemStateStore::new(),
        member_sink.clone(),
        wake_rx,
        Some(MlsChannel::from_shared(member_group.clone())),
    );

    // --- baseline: a chat crosses the live mesh
    a.execute(Command::Chat {
        body: "eins".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat eins");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !has_chat(&member_sink, "eins") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the baseline chat never crossed; got {:?}",
            member_sink.messages().len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // --- the member acks everything it has engine-accepted so far (what a
    // real member engine's debounced ACK does): the founder records
    // acked_floor + ack_seen for member-b
    let mut win = molt_core::AcceptedWindow::default();
    for (from, env) in member_sink.messages() {
        if from == "founder-a" {
            win.accept(env.seq);
        }
    }
    let mut frame = molt_net::MESH_ACK_TAG.to_vec();
    frame.extend_from_slice(&serde_json::to_vec(&win).expect("ack json"));
    let ct = member_group
        .lock()
        .expect("member group lock")
        .encrypt(&frame)
        .expect("encrypt ack");
    send_framed(&hub, link.snd0(), &link.wrap_out, MsgId([0xA1; 16]), &ct)
        .await
        .expect("send the ack");
    // let the founder's recv loop process the ack and flush the cursor save
    tokio::time::sleep(Duration::from_millis(700)).await;

    // --- the deaf window: the member's inbound queue goes silently dead
    // (SEND still returns Ok — the V2 state) and a chat falls into it
    for rcv in &link.rcvs {
        assert!(hub_ctl.expire_queue(&rcv.id), "the member's inbound queue exists");
    }
    a.execute(Command::Chat {
        body: "zwei".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat zwei");
    // the transport ACCEPTS it, the member never sees it — and the sender's
    // cursor moves past it (the pre-guarantee loss, proven deaf here)
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !has_chat(&member_sink, "zwei"),
        "the deaf queue must swallow the chat (that IS the failure mode)"
    );

    // --- clean close (persists the advanced ratchet + cursors), then the
    // leg heals (standing in for rotate/resubscribe) and a fresh engine
    // reopens the workspace on the same directory
    a.execute(Command::CloseWorkspace).await.expect("clean close");
    drop(a);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let dir = molt_storage::find_workspace_dir(&root_a, &id).expect("workspace dir");
    loop {
        match molt_storage::open_workspace(&dir) {
            Ok(_) => break, // writer gone, lock free
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the closed engine never released the workspace lock"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    for rcv in &link.rcvs {
        assert!(hub_ctl.revive_queue(&rcv.id), "revive the healed leg");
    }
    let session_a2 = SessionView {
        workspaces: molt_storage::scan_workspaces(&root_a)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let a2 = molt_engine::__spawn_with_reopen_transport(
        molt_core::GroupConfig::demo(),
        session_a2,
        hub.clone(),
    );
    a2.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen after the deaf window");

    // --- the guarantee: the reopened supervisor rewound to member-b's acked
    // floor and re-offers the unacked tail — "zwei" arrives, re-encrypted at
    // the current ratchet under a fresh resend epoch
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !has_chat(&member_sink, "zwei") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deaf window's chat was never resent; member saw {:?}",
            member_sink
                .messages()
                .iter()
                .map(|(f, e)| (f.clone(), format!("{:?}", std::mem::discriminant(&e.body))))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
