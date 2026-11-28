// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The delivery guarantee, end to end** (`docs_archive/transport/delivery_guarantee.md`).
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

use common::{found_with_mesh, hard_kill, read_chat, CaptureSink};
use molt_core::{Command, SessionSettings, SessionView, WorkspaceEvent};
use molt_net::supervisor::{self, send_framed, MemLog, MemStateStore, NetConfig, StateStore};
use molt_net::{MlsChannel, MlsMember, MsgId, PeerLink};
use tokio::sync::watch;

/// A chat body among the sink's captured envelopes, from the founder?
fn has_chat(sink: &CaptureSink, body: &str) -> bool {
    sink.messages().iter().any(|(from, env)| {
        from == "founder-a"
            && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == body)
    })
}

/// The shared first act of both keystone tests: found over loopback, stand
/// the member's supervisor up, prove the baseline chat crosses, ack it as
/// the member's engine would, then swallow a second chat in a deaf window.
struct DeafWindow {
    hub: molt_net::LoopbackTransport,
    hub_ctl: molt_net::LoopbackHub,
    link: PeerLink,
    member_sink: CaptureSink,
    id: String,
    _member_sup: molt_net::supervisor::SupervisorHandle,
}

async fn deaf_window(root_a: &std::path::Path) -> (DeafWindow, molt_engine::WalletHandle) {
    let (a, hub, member_mesh, member_mls, id) = found_with_mesh(root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter");
    let hub_ctl = hub.hub();

    // the member's runtime supervisor; the test doubles as its "engine":
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
    let member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![link.clone()], 21),
        MemLog::new(),
        MemStateStore::new(),
        member_sink.clone(),
        wake_rx,
        Some(MlsChannel::from_shared(member_group.clone())),
    );

    // baseline: a chat crosses the live mesh
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

    // pad the session with real traffic: every send advances the sender
    // ratchet, so a hard-kill's regression to a STALE snapshot is dozens of
    // generations — without the debounced live persist, the resend loop
    // could not outrun MLS replay rejection inside any sane deadline (that
    // gap is what makes the hard-kill test a genuine pin)
    for i in 0..12 {
        a.execute(Command::Chat {
            body: format!("füller-{i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("filler chat");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while !has_chat(&member_sink, "füller-11") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the filler traffic never finished crossing"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // the member acks everything it has engine-accepted so far (what a real
    // member engine's debounced ACK does): the founder records acked_floor +
    // ack_seen for member-b
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
    // three spaced copies (distinct frames, idempotent floor): the founder
    // must durably record ack_seen before the deaf window — one lost or
    // slow frame must not turn into a hard test failure (E7 review)
    send_framed(&hub, link.snd0(), &link.wrap_out, MsgId([0xA1; 16]), &ct)
        .await
        .expect("send the ack");
    for (i, gap) in [(0xA2u8, 600u64), (0xA3, 600)] {
        tokio::time::sleep(Duration::from_millis(gap)).await;
        let ct = member_group
            .lock()
            .expect("member group lock")
            .encrypt(&frame)
            .expect("encrypt ack copy");
        send_framed(&hub, link.snd0(), &link.wrap_out, MsgId([i; 16]), &ct)
            .await
            .expect("send the ack copy");
    }
    // let the founder's recv loop process + flush the cursor save
    tokio::time::sleep(Duration::from_millis(700)).await;

    // the deaf window: the member's inbound queue goes silently dead
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

    (DeafWindow { hub, hub_ctl, link, member_sink, id, _member_sup: member_sup }, a)
}

/// Revive the member's queue and reopen the workspace with a fresh engine
/// on the same directory (the reopen seam rides the still-running hub).
async fn heal_and_reopen(dw: &DeafWindow, root_a: &std::path::Path) -> molt_engine::WalletHandle {
    for rcv in &dw.link.rcvs {
        assert!(dw.hub_ctl.revive_queue(&rcv.id), "revive the healed leg");
    }
    let session_a2 = SessionView {
        workspaces: molt_storage::scan_workspaces(root_a)
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
        dw.hub.clone(),
    );
    a2.execute(Command::OpenWorkspace { id: dw.id.clone() })
        .await
        .expect("reopen after the deaf window");
    a2
}

/// Send one chat through the founder engine.
async fn dw_chat(a: &molt_engine::WalletHandle, body: &str) {
    a.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat");
}

/// Await a resent chat at the member, or fail with what it saw.
async fn await_resend(member_sink: &CaptureSink, body: &str, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !has_chat(member_sink, body) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deaf window's chat {body:?} was never resent; member saw {:?}",
            member_sink
                .messages()
                .iter()
                .map(|(f, e)| (f.clone(), format!("{:?}", std::mem::discriminant(&e.body))))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    let (dw, a) = deaf_window(&root_a).await;

    // clean close (persists the advanced ratchet + cursors), then heal + reopen
    a.execute(Command::CloseWorkspace).await.expect("clean close");
    hard_kill(a, &root_a, &dw.id).await; // after the clean close this only waits for the lock
    let _a2 = heal_and_reopen(&dw, &root_a).await;

    // the guarantee: the reopened supervisor rewound to member-b's acked
    // floor and re-offers the unacked tail — "zwei" arrives, re-encrypted at
    // the current ratchet under a fresh resend epoch
    await_resend(&dw.member_sink, "zwei", 30).await;
}

/// E6's pin: the same deaf window, but the founder is HARD-killed — the
/// resumed ratchet is whatever the debounced live MLS persist last merged
/// (seconds old), NOT the mesh-up snapshot (a whole session old). The
/// rewound resend therefore re-encrypts just past what the member already
/// consumed and lands, instead of grinding through MLS replay rejection
/// forever. Red without `persist_mls_if_due`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_lost_in_a_deaf_window_survives_a_hard_kill() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (dw, a) = deaf_window(&root_a).await;

    // force a DETERMINISTIC ratchet persist covering "zwei": wait out the
    // 10 s persist debounce, then record one more chat — persist_mls_if_due
    // rides record(), so the snapshot provably lands (covering zwei's
    // generation) BEFORE "drei" is even encrypted; drei joins the unacked
    // tail behind the still-deaf queue. Then HARD kill.
    tokio::time::sleep(Duration::from_secs(11)).await;
    dw_chat(&a, "drei").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    hard_kill(a, &root_a, &dw.id).await;
    let _a2 = heal_and_reopen(&dw, &root_a).await;

    await_resend(&dw.member_sink, "zwei", 45).await;
    // the post-persist chat rides the same rewound tail
    await_resend(&dw.member_sink, "drei", 15).await;
}

/// The 2026-11-27 field deadlock (gap 25), end to end over the loopback
/// hub with the member supervisor as the sender and the founder engine as
/// the receiver. The sender's log fills with ~1,700 entries that never
/// cross the wire (a gossip flood of re-recorded peer traffic), so its
/// next owed envelope arrives far above the receiver's accept cap - with
/// a `prev_seq` that IS an accepted seq. The receiver accepts it (the
/// prev chain proves nothing owed hides between), the sheet's high rises,
/// the sender's floor follows: no re-offer round. Red before the fix: the
/// envelope sat held behind the cap, the floor stuck at the first
/// envelope, and the stall ladder re-offered the tail on its 10 s rung.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_envelope_chained_to_an_accepted_prev_crosses_a_log_flood_without_a_re_offer() {
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
    let link = member_mesh
        .iter()
        .filter_map(PeerLink::from_mesh)
        .next()
        .expect("the member's link to the founder");
    let member_group =
        Arc::new(Mutex::new(MlsMember::restore(&member_mls).expect("restore member MLS")));

    // the sender's log: one owed envelope, the flood of entries that never
    // cross the wire, then the next owed one - its prev_seq on the first
    let first = 1_000_000u64;
    let flood: u64 = 1_700;
    let second = first + flood + 1;
    // stamped "now": an old stamp would fall outside the chat-retention
    // read window and the applied read would hide the message
    let ts: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let own_chat = |seq: u64, prev: u64, body: &str| molt_core::EventEnvelope {
        seq,
        ts,
        prev_seq: prev,
        by: "member-b".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            common::test_msg_id(seq),
            "member-b",
            body,
            ts,
        )),
    };
    let foreign = |seq: u64| molt_core::EventEnvelope {
        seq,
        ts,
        prev_seq: 0,
        by: "member-c".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            common::test_msg_id(seq),
            "member-c",
            "noise",
            ts,
        )),
    };
    let log = MemLog::new();
    log.push(own_chat(first, 0, "eins"));
    for seq in first + 1..second {
        log.push(foreign(seq));
    }
    log.push(own_chat(second, first, "zwei"));

    let sink = CaptureSink::default();
    let store = MemStateStore::new();
    let (_wake, wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![link.clone()], 21),
        log,
        store.clone(),
        sink.clone(),
        wake_rx,
        Some(MlsChannel::from_shared(member_group.clone())),
    );

    // the baseline envelope crosses and is accepted (first contact)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    while !read_chat(&a).await.iter().any(|m| m.get("body").and_then(|v| v.as_str()) == Some("eins")) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the baseline envelope never crossed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // the flooded envelope: accepted, the sheet's high rises, the sender's
    // floor moves past the whole flood
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let member_cursor = || async {
        store.load().await.outbound.get("founder-a").map(|c| c.acked_floor).unwrap_or(0)
    };
    let mut floor = member_cursor().await;
    while floor < second {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sender's floor never passed the flood (stuck at {floor})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        floor = member_cursor().await;
    }
    assert_eq!(floor, second, "the floor sits past the flood");

    // one stall rung (10 s) passes with no re-offer round: the cursor never
    // rewound (resend epoch 0) and the floor stayed at the flood's end
    tokio::time::sleep(Duration::from_secs(12)).await;
    let cur = store.load().await.outbound.get("founder-a").copied().unwrap_or_default();
    assert_eq!(cur.resend_epoch, 0, "a re-offer round ran");
    assert!(cur.acked_floor >= second, "the floor regressed");
    // and the receiver really applied it
    assert!(
        read_chat(&a).await.iter().any(|m| m.get("body").and_then(|v| v.as_str()) == Some("zwei")),
        "the flooded envelope was applied"
    );
}
