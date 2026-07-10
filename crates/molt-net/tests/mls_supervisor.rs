// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! T2 runtime: the supervisor carries **MLS ciphertext**, not plaintext. A
//! workspace event is encrypted **once** (the ratchet advances a single time)
//! and the same ciphertext is fanned out to every peer — each per-queue-wrapped
//! distinctly — and every member decrypts it to the same envelope with an
//! authenticated sender. Proves the encrypt-once-fan-out + decrypt mechanism
//! end-to-end through the real supervisor / wrap / chunk / MLS path over the
//! loopback hub (the mesh bootstrap that establishes the runtime queues is the
//! next increment; here the queues are wired directly).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use molt_core::{ChatMessage, EventEnvelope, MemberId, WorkspaceEvent};
use molt_net::mls::MlsMember;
use molt_net::{
    EngineSink, LoopbackHub, MemLog, MemStateStore, MlsChannel, NetConfig, NetError, PeerLink,
    SupervisorHandle,
};
use tokio::sync::watch;

#[derive(Clone, Default)]
struct TestSink {
    delivered: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
}

impl TestSink {
    fn delivered(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.delivered.lock().expect("sink lock").clone()
    }
}

impl EngineSink for TestSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.delivered.lock().expect("sink lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, _member: &MemberId) {}
    async fn send_failed(&self, _member: &MemberId, _reason: &str) {}
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A deterministic non-nil message id for hand-built test envelopes.
fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

fn chat_env(seq: u64, by: &str, body: &str) -> EventEnvelope {
    EventEnvelope {
        seq,
        ts: 1_751_000_000,
        by: by.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage {
            id: test_msg_id(seq),
            from: by.to_string(),
            body: body.to_string(),
            ts: 1_751_000_000,
            quote: None,
            quote_id: None,
            channel: molt_core::ChannelRef::Group,
            reactions: std::collections::BTreeMap::new(),
            deleted_by: None,
            file: None,
        }),
    }
}

/// One node's runtime pieces, kept alive for the duration of the test.
struct Node {
    feed: MemLog,
    wakeup: watch::Sender<u64>,
    sink: TestSink,
    _supervisor: SupervisorHandle,
}

fn spawn_node(hub: &LoopbackHub, links: Vec<PeerLink>, member: &str, mls: MlsMember) -> Node {
    let feed = MemLog::new();
    let (wakeup, wakeup_rx) = watch::channel(0u64);
    let sink = TestSink::default();
    let supervisor = molt_net::supervisor::spawn(
        hub.transport(),
        NetConfig::fast(member.to_string(), links, 42),
        feed.clone(),
        MemStateStore::new(),
        sink.clone(),
        wakeup_rx,
        Some(MlsChannel::new(mls)),
    );
    Node {
        feed,
        wakeup,
        sink,
        _supervisor: supervisor,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_encrypt_fans_out_to_the_whole_group_and_each_member_decrypts() {
    // --- a real 3-member MLS group (founder alice adds bob + cara) ----------
    let mut alice = MlsMember::new(&key(1), "alice").expect("alice");
    let bob = MlsMember::new(&key(2), "bob").expect("bob");
    let cara = MlsMember::new(&key(3), "cara").expect("cara");
    alice.create_group().expect("create group");
    let welcome = alice
        .add_members(&[
            bob.key_package().expect("bob kp"),
            cara.key_package().expect("cara kp"),
        ])
        .expect("add")
        .expect("a welcome");
    let mut bob = bob;
    let mut cara = cara;
    bob.join_from_welcome(&welcome).expect("bob joins");
    cara.join_from_welcome(&welcome).expect("cara joins");

    // --- wire the full-mesh loopback queues and a supervisor per node -------
    let hub = LoopbackHub::calm();
    let names = vec!["alice".to_string(), "bob".to_string(), "cara".to_string()];
    let mut mesh = hub.full_mesh(&names).expect("mesh wiring");
    let alice_node = spawn_node(&hub, mesh.remove("alice").expect("alice links"), "alice", alice);
    let bob_node = spawn_node(&hub, mesh.remove("bob").expect("bob links"), "bob", bob);
    let cara_node = spawn_node(&hub, mesh.remove("cara").expect("cara links"), "cara", cara);

    // --- alice posts one chat; the outbox encrypts it ONCE and fans it out --
    alice_node.feed.push(chat_env(2, "alice", "the fence is mended"));
    let _ = alice_node.wakeup.send(2);

    // --- bob and cara both decrypt the same group message -------------------
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let b = bob_node.sink.delivered();
        let c = cara_node.sink.delivered();
        if !b.is_empty() && !c.is_empty() {
            for (label, got) in [("bob", &b), ("cara", &c)] {
                assert_eq!(got.len(), 1, "[{label}] exactly one delivery");
                let (from, env) = &got[0];
                assert_eq!(from, "alice", "[{label}] MLS-authenticated sender");
                let WorkspaceEvent::Chat(msg) = &env.body else {
                    panic!("[{label}] not a chat");
                };
                assert_eq!(msg.body, "the fence is mended", "[{label}] plaintext recovered");
            }
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group message never reached both members (bob={}, cara={})",
            b.len(),
            c.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
