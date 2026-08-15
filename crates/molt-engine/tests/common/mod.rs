// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs, dead_code)]

//! Shared helpers of the engine integration tests.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{
    ChatMessage, Command, EventEnvelope, MemberId, MeshLink, Reply, SessionSettings, SessionView,
    Surface, WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::{EngineSink, NetError};

/// A deterministic non-nil message id for hand-built test envelopes (the
/// engine mints real random ids; this stands in for a peer's minting).
pub fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

/// One hand-built peer chat envelope carrying `test_msg_id(seq)` — what a
/// sender's outbox would hold for a plain text message. Stamped "now" so
/// the messages sit inside the chat-retention read window.
pub fn chat_env(seq: u64, from: &str, body: &str) -> EventEnvelope {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + seq;
    EventEnvelope { prev_seq: 0,
        seq,
        ts,
        by: from.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(test_msg_id(seq), from, body, ts)),
    }
}

/// Read a surface's applied log.
pub async fn read_applied(w: &WalletHandle, surface: Surface) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface,
            channel: None,
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Read the chat surface's applied log.
pub async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll the session until a running founding seals (`create.run.outcome
/// == 1`; panic on failure or after 20 s) — the sim ritual runs its
/// members asynchronously.
pub async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    // ❻½: the founder's own phrase-backup confirmation gates the seal
    // (the sim members attest automatically) — played once, like a human
    let mut confirmed = false;
    loop {
        match w.execute(Command::ReadSession).await.expect("read session") {
            Reply::Session(s) => match s.create.run.outcome {
                1 => return,
                2 => panic!("founding failed: {:?}", s.create.run.log),
                _ => {
                    if !confirmed && !s.create.seed.is_empty() {
                        w.execute(Command::ConfirmSeedBackup {
                            phrase: s.create.seed.clone(),
                        })
                        .await
                        .expect("founder backup confirm");
                        confirmed = true;
                    }
                }
            },
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll until the chat log holds at least `want` messages (or panic after
/// `secs`).
pub async fn await_chat_len(w: &WalletHandle, want: usize, secs: u64) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let chat = read_chat(w).await;
        if chat.len() >= want {
            return chat;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {want} chat messages, have {}",
            chat.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Read the full session view.
pub async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// An [`EngineSink`] that captures what a member-side supervisor delivers and
/// every passive-presence (`peer_seen`) sighting — the observation point for
/// mesh-resume and keepalive tests.
#[derive(Clone, Default)]
pub struct CaptureSink {
    delivered: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
    seen: Arc<Mutex<Vec<MemberId>>>,
}
impl CaptureSink {
    pub fn messages(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.delivered.lock().expect("lock").clone()
    }
    /// Every member `peer_seen` fired for, in order (duplicates kept).
    pub fn seen(&self) -> Vec<MemberId> {
        self.seen.lock().expect("lock").clone()
    }
}
impl EngineSink for CaptureSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.delivered.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, m: &MemberId) {
        self.seen.lock().expect("lock").push(m.clone());
    }
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

/// Run a real 2-of-2 founding + mesh bootstrap over the loopback hub: founder
/// engine on `root_a`, genuine member via `run_ritual_member`. Returns the
/// founder handle, the shared hub transport, the member's assembled mesh +
/// post-bootstrap MLS snapshot, and the workspace id. The founder is left one
/// `CreateFinish` short of entered — callers that need the workspace open must
/// execute it.
pub async fn found_with_mesh(
    root_a: &Path,
) -> (
    WalletHandle,
    molt_engine::RitualTransport,
    Vec<MeshLink>,
    Vec<u8>,
    String,
) {
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_bootstrap(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Phoenix".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out the invite material")
    })
    .await
    .expect("join blocking");
    let seat = materials.into_iter().next().expect("seat material");
    let hub = seat.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Phoenix".to_string(),
        agenda: "keep the mesh alive".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("founder proposes the charter");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate; the
    // None-ratifier member attests automatically)
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
        if s.create.run.outcome == 1
            && s.create.run.log.iter().any(|l| l.contains("direct mesh established"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never bootstrapped its mesh; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b_outcome = b_task.await.expect("B task");
    let member_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    let member_mls = b_outcome.mls_snapshot.expect("member post-bootstrap snapshot");
    let id = read_session(&a).await.active_workspace.clone();
    assert!(!id.is_empty(), "the founded workspace is active");
    (a, hub, member_mesh, member_mls, id)
}
