// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The persisted-transport keystone (T1): a node whose outbox is the real
//! encrypted workspace log and whose delivery cursors live in the real
//! `transport.state` file feeds a second, full engine node over the
//! loopback transport. Covers: log-backed outbox reads through the storage
//! writer, cursor persistence across a "process restart" (no duplicate
//! deliveries), the engine's `NetDelivered` path recording peer chat into
//! a persisted workspace (and replaying it after reopen), passive
//! presence, and the ignore path for non-chat wire events (T1 scope).
//!
//! The mesh itself is wired by hand — exactly the material the T2
//! invite/join flow will carry in-band.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{await_chat_len, read_chat};
use molt_core::{
    ChatMessage, Command, EventEnvelope, GroupConfig, MemberId, Reply, SessionSettings,
    SessionView, WorkspaceEvent,
};
use molt_engine::{FileStateStore, StorageLog};
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{EngineSink, LoopbackHub, NetError};
use tokio::sync::watch;

/// The sending side needs a sink only for health signals in this test.
#[derive(Clone)]
struct NullSink;

impl EngineSink for NullSink {
    async fn deliver(&self, _from: &MemberId, _env: EventEnvelope) -> Result<(), NetError> {
        Ok(())
    }
    async fn peer_seen(&self, _member: &MemberId) {}
    async fn send_failed(&self, _member: &MemberId, _reason: &str) {}
}

fn genesis(member: &str) -> EventEnvelope {
    EventEnvelope {
        seq: 1,
        ts: 1_751_000_000,
        by: member.to_string(),
        body: WorkspaceEvent::Founded {
            name: "Mesh Club".to_string(),
            rule_m: 2,
            rule_n: 2,
            member: member.to_string(),
            roster: vec!["ada".to_string(), "ben".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
        },
    }
}

/// A deterministic non-nil message id for hand-built test envelopes (the
/// engine mints real random ids; this stands in for a peer's minting).
fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

fn chat_env(seq: u64, body: &str) -> EventEnvelope {
    EventEnvelope {
        seq,
        ts: 1_751_000_000 + seq,
        by: "ada".to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(
            test_msg_id(seq),
            "ada",
            body,
            1_751_000_000 + seq,
        )),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_outbox_feeds_a_real_engine_and_survives_restart() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("node-a");
    let root_b = tmp.path().join("node-b");

    // --- node B: a full engine on a workspace whose genesis roster
    // includes ada (what a T2 join would have materialized)
    let seed_b = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("gen"))
        .expect("entropy");
    let ws_b = molt_storage::create_workspace(&root_b, &seed_b, &genesis("ben")).expect("create b");
    let id_b = ws_b.manifest.workspace.id.clone();
    drop(ws_b); // release the LOCK for the engine
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(&root_b)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root_b.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w_b = molt_engine::spawn_with_storage(GroupConfig::demo(), session);
    w_b.execute(Command::OpenWorkspace { id: id_b.clone() })
        .await
        .expect("open b");

    // --- the mesh: one queue per direction, wrap keys out of band (the
    // same full_mesh wiring the demo mesh uses)
    let hub = LoopbackHub::calm();
    let members: Vec<MemberId> = vec!["ada".to_string(), "ben".to_string()];
    let mut mesh = hub.full_mesh(&members).expect("mesh wiring");
    let links_ada = mesh.remove("ada").expect("ada links");
    let links_ben = mesh.remove("ben").expect("ben links");

    // --- node B's supervisor drives the engine through its net sink
    let (_wake_b, wake_b_rx) = watch::channel(0u64);
    let _sup_b = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("ben".to_string(), links_ben, 11),
        MemLog::new(), // ben's outbox is idle in this test
        MemStateStore::new(),
        w_b.net_sink(),
        wake_b_rx,
        None,
    );

    // --- node A: raw storage writer + supervisor (no engine needed to
    // prove the log-backed outbox; the engine side is B's job here)
    let seed_a = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("gen"))
        .expect("entropy");
    let ws_a = molt_storage::create_workspace(&root_a, &seed_a, &genesis("ada")).expect("create a");
    let dir_a = ws_a.dir().to_path_buf();
    let handle_a = molt_storage::start_writer(ws_a);
    let (wake_a, wake_a_rx) = watch::channel(0u64);
    let sup_a = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("ada".to_string(), links_ada.clone(), 11),
        StorageLog::new(handle_a.clone()),
        FileStateStore::new(handle_a.clone()),
        NullSink,
        wake_a_rx,
        None,
    );

    // ada writes history: the genesis (a non-chat event B must ignore
    // without wedging) is already in the log; two chat messages follow
    assert!(handle_a.append(chat_env(2, "hi ben")));
    assert!(handle_a.append(chat_env(3, "log-backed outbox speaking")));
    let _ = wake_a.send(3);

    let chat = await_chat_len(&w_b, 2, 20).await;
    assert_eq!(chat[0]["from"], serde_json::json!("ada"));
    assert_eq!(chat[0]["body"], serde_json::json!("hi ben"));
    assert_eq!(chat[1]["body"], serde_json::json!("log-backed outbox speaking"));

    // passive presence: ada's pill on node B is live
    match w_b.execute(Command::ReadSession).await.expect("session") {
        Reply::Session(s) => {
            let ws = s.workspaces.iter().find(|w| w.id == id_b).expect("entry");
            let ada = ws.members.iter().find(|m| m.name == "ada").expect("ada pill");
            assert_eq!((ada.state, ada.last.as_str()), (0, "just now"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    // the delivery cursors persisted beside the log
    assert!(
        Path::new(&dir_a.join("transport.state")).exists(),
        "transport.state exists in the sender's workspace dir"
    );

    // --- "crash" node A: stop the supervisor, close the writer; restart
    // from nothing but the directory (log + transport.state)
    sup_a.shutdown();
    handle_a.close(None);
    let (ws_a2, _loaded) = molt_storage::open_workspace(&dir_a).expect("reopen a");
    assert_eq!(ws_a2.next_seq, 4);
    let handle_a2 = molt_storage::start_writer(ws_a2);
    let (wake_a2, wake_a2_rx) = watch::channel(0u64);
    let _sup_a2 = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("ada".to_string(), links_ada, 11),
        StorageLog::new(handle_a2.clone()),
        FileStateStore::new(handle_a2.clone()),
        NullSink,
        wake_a2_rx,
        None,
    );
    assert!(handle_a2.append(chat_env(4, "back after the crash")));
    let _ = wake_a2.send(4);

    let chat = await_chat_len(&w_b, 3, 20).await;
    // settle, then check for duplicates a cursor loss would have caused
    tokio::time::sleep(Duration::from_millis(300)).await;
    let chat_final = read_chat(&w_b).await;
    assert_eq!(chat_final.len(), chat.len().max(3), "no duplicate deliveries");
    assert_eq!(chat_final.len(), 3);
    assert_eq!(chat_final[2]["body"], serde_json::json!("back after the crash"));

    // --- node B's inbound history is real persisted history: it replays
    w_b.execute(Command::CloseWorkspace).await.expect("close b");
    w_b.execute(Command::OpenWorkspace { id: id_b })
        .await
        .expect("reopen b");
    let replayed = read_chat(&w_b).await;
    assert_eq!(replayed.len(), 3, "peer chat survived close + reopen");
    assert_eq!(replayed[2]["body"], serde_json::json!("back after the crash"));
}
