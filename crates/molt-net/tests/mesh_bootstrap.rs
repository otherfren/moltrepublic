// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! T2 runtime (2b): the post-founding mesh bootstrap end to end. Two nodes that
//! already share an MLS group open their per-pair queues, exchange
//! `MeshAnnounce`s over the group channel (here a direct relay stands in for the
//! founder-relayed MLS-over-star seed), assemble their full-mesh `PeerLink`s,
//! and then chat over MLS through real supervisors. Proves bootstrap → mesh →
//! MLS traffic composes over loopback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use molt_core::{ChatMessage, EventEnvelope, MemberId, WorkspaceEvent};
use molt_net::mesh::{bootstrap_mesh, MeshAnnounce};
use molt_net::mls::MlsMember;
use molt_net::{
    EngineSink, LoopbackHub, MemLog, MemStateStore, MlsChannel, NetConfig, NetError, PeerLink,
};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Default)]
struct TestSink {
    delivered: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
}
impl TestSink {
    fn delivered(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.delivered.lock().expect("lock").clone()
    }
}
impl EngineSink for TestSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.delivered.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, _m: &MemberId) {}
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

fn key(s: u8) -> SigningKey {
    SigningKey::from_bytes(&[s; 32])
}

fn chat_env(seq: u64, by: &str, body: &str) -> EventEnvelope {
    EventEnvelope {
        seq,
        ts: 1_751_000_000,
        by: by.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage {
            from: by.to_string(),
            body: body.to_string(),
            ts: 1_751_000_000,
            quote: None,
            reactions: std::collections::BTreeMap::new(),
            deleted_by: None,
            file: None,
        }),
    }
}

/// Relay every announcement a node broadcasts to the *other* node, tagged with
/// the sender's handle — standing in for the founder-relayed MLS-over-star seed.
fn relay(from: &str, mut out: mpsc::Receiver<MeshAnnounce>, to: mpsc::Sender<(MemberId, MeshAnnounce)>) {
    let from = from.to_string();
    tokio::spawn(async move {
        while let Some(a) = out.recv().await {
            if to.send((from.clone(), a)).await.is_err() {
                break;
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_bootstrap_a_mesh_and_chat_over_mls() {
    // --- a shared MLS group (alice founds + adds bob) ------------------------
    let mut alice_mls = MlsMember::new(&key(1), "alice").expect("alice");
    let bob = MlsMember::new(&key(2), "bob").expect("bob");
    alice_mls.create_group().expect("group");
    let welcome = alice_mls
        .add_members(&[bob.key_package().expect("kp")])
        .expect("add")
        .expect("welcome");
    let mut bob_mls = bob;
    bob_mls.join_from_welcome(&welcome).expect("bob joins");

    let hub = LoopbackHub::calm();

    // --- announcement channels + the stand-in relay -------------------------
    let (a_out, a_out_rx) = mpsc::channel::<MeshAnnounce>(4);
    let (b_out, b_out_rx) = mpsc::channel::<MeshAnnounce>(4);
    let (a_in_tx, a_in_rx) = mpsc::channel::<(MemberId, MeshAnnounce)>(4);
    let (b_in_tx, b_in_rx) = mpsc::channel::<(MemberId, MeshAnnounce)>(4);
    relay("alice", a_out_rx, b_in_tx); // alice's announce reaches bob
    relay("bob", b_out_rx, a_in_tx); // bob's reaches alice

    // --- both nodes bootstrap their mesh concurrently -----------------------
    let ta = hub.transport();
    let tb = hub.transport();
    let alice_boot = tokio::spawn(async move {
        bootstrap_mesh("alice", &["bob".to_string()], &ta, a_out, a_in_rx).await
    });
    let bob_boot = tokio::spawn(async move {
        bootstrap_mesh("bob", &["alice".to_string()], &tb, b_out, b_in_rx).await
    });
    let alice_links: Vec<PeerLink> = alice_boot.await.expect("join").expect("alice mesh");
    let bob_links: Vec<PeerLink> = bob_boot.await.expect("join").expect("bob mesh");
    assert_eq!(alice_links.len(), 1);
    assert_eq!(bob_links.len(), 1);

    // --- spawn a runtime supervisor per node from the assembled mesh --------
    let alice_feed = MemLog::new();
    let (alice_wake, alice_wake_rx) = watch::channel(0u64);
    let _alice_sup = molt_net::supervisor::spawn(
        hub.transport(),
        NetConfig::fast("alice".to_string(), alice_links, 1),
        alice_feed.clone(),
        MemStateStore::new(),
        TestSink::default(),
        alice_wake_rx,
        Some(MlsChannel::new(alice_mls)),
    );
    let bob_sink = TestSink::default();
    let (_bob_wake, bob_wake_rx) = watch::channel(0u64);
    let _bob_sup = molt_net::supervisor::spawn(
        hub.transport(),
        NetConfig::fast("bob".to_string(), bob_links, 2),
        MemLog::new(),
        MemStateStore::new(),
        bob_sink.clone(),
        bob_wake_rx,
        Some(MlsChannel::new(bob_mls)),
    );

    // --- alice chats; bob receives it decrypted over the bootstrapped mesh --
    alice_feed.push(chat_env(2, "alice", "the mesh is up"));
    let _ = alice_wake.send(2);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = bob_sink.delivered();
        if let Some((from, env)) = got.first() {
            assert_eq!(from, "alice", "MLS-authenticated sender");
            let WorkspaceEvent::Chat(msg) = &env.body else {
                panic!("not a chat");
            };
            assert_eq!(msg.body, "the mesh is up");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat never reached bob over the bootstrapped mesh"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
