// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Loopback-tier tests of the supervisor (concept §7): under an arbitrary
//! chaos seed every node receives every other node's events exactly once
//! and in per-sender order; the outbox drains after a partition heals; a
//! restarted node resumes from its persisted cursors without duplicating
//! deliveries at its peers.
//!
//! Honest scope note: "convergence" here (and in T1 generally) means the
//! delivered event *sets* and per-sender order converge. Identical
//! cross-sender ordering needs the stable-message-id / reconciliation work
//! scheduled with T2 — see the concept's status section.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{ChatMessage, EventEnvelope, MemberId, WorkspaceEvent};
use molt_net::{
    ChaosPolicy, EngineSink, LoopbackHub, MemLog, MemStateStore, NetConfig, NetError, PeerLink,
    SupervisorHandle,
};
use tokio::sync::watch;

/// A sink that just collects what the engine would apply.
#[derive(Clone, Default)]
struct TestSink {
    delivered: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
    send_failures: Arc<Mutex<u32>>,
    /// Stage-B health signals in arrival order: `up:<m>` / `down:<m>:<reason>`.
    link_events: Arc<Mutex<Vec<String>>>,
    send_oks: Arc<Mutex<u32>>,
}

impl TestSink {
    fn delivered(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.delivered.lock().expect("sink lock").clone()
    }

    fn link_events(&self) -> Vec<String> {
        self.link_events.lock().expect("sink lock").clone()
    }
}

impl EngineSink for TestSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.delivered
            .lock()
            .expect("sink lock")
            .push((from.clone(), env));
        Ok(())
    }

    async fn peer_seen(&self, _member: &MemberId) {}

    async fn send_failed(&self, _member: &MemberId, _reason: &str) {
        *self.send_failures.lock().expect("sink lock") += 1;
    }

    async fn link_up(&self, member: &MemberId) {
        self.link_events.lock().expect("sink lock").push(format!("up:{member}"));
    }

    async fn link_down(&self, member: &MemberId, reason: &str) {
        self.link_events
            .lock()
            .expect("sink lock")
            .push(format!("down:{member}:{reason}"));
    }

    async fn send_ok(&self, _member: &MemberId) {
        *self.send_oks.lock().expect("sink lock") += 1;
    }
}

/// One test node: its log, cursor store, wakeup and sink.
struct Node {
    member: MemberId,
    log: MemLog,
    store: MemStateStore,
    wakeup: watch::Sender<u64>,
    sink: TestSink,
    supervisor: SupervisorHandle,
    /// The links, kept to restart the supervisor mid-test.
    links: Vec<PeerLink>,
    hub: LoopbackHub,
    seed: u64,
}

impl Node {
    /// Simulate a process restart: stop the supervisor, start a fresh one
    /// on the same log + cursor store (that is exactly what survives).
    async fn restart(&mut self) {
        self.supervisor.shutdown();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (tx, rx) = watch::channel(0u64);
        self.wakeup = tx;
        self.supervisor = molt_net::supervisor::spawn(
            self.hub.transport(),
            NetConfig::fast(self.member.clone(), self.links.clone(), self.seed),
            self.log.clone(),
            self.store.clone(),
            self.sink.clone(),
            rx,
            None,
        );
    }
}

/// Build a full mesh of nodes over one hub (the queue/wrap-key wiring is
/// `LoopbackHub::full_mesh` — the same helper the engine's demo mesh
/// uses, so the direction convention has one owner).
async fn mesh(hub: &LoopbackHub, members: &[&str], seed: u64) -> Vec<Node> {
    let names: Vec<MemberId> = members.iter().map(|m| (*m).to_string()).collect();
    let mut links_by_member = hub.full_mesh(&names).expect("mesh wiring");
    let mut nodes = Vec::new();
    for (i, me) in names.iter().enumerate() {
        let links: Vec<PeerLink> = links_by_member.remove(me).expect("own links");
        let node_seed = seed.wrapping_add(u64::try_from(i).unwrap_or_default() * 7919) | 1;
        let log = MemLog::new();
        let store = MemStateStore::new();
        let sink = TestSink::default();
        let (tx, rx) = watch::channel(0u64);
        let supervisor = molt_net::supervisor::spawn(
            hub.transport(),
            NetConfig::fast(me.clone(), links.clone(), node_seed),
            log.clone(),
            store.clone(),
            sink.clone(),
            rx,
            None,
        );
        nodes.push(Node {
            member: me.clone(),
            log,
            store,
            wakeup: tx,
            sink,
            supervisor,
            links,
            hub: hub.clone(),
            seed: node_seed,
        });
    }
    nodes
}

/// Like [`mesh`] but with an explicit redundancy factor `n`: each directed
/// pair-leg gets `n` inbound queues (Track B Stage 2). Lets the redundancy
/// tests exercise the N-recv-merge / dedup / send-fan logic over loopback.
async fn mesh_n(hub: &LoopbackHub, members: &[&str], seed: u64, n: usize) -> Vec<Node> {
    let names: Vec<MemberId> = members.iter().map(|m| (*m).to_string()).collect();
    let mut links_by_member = hub.full_mesh_n(&names, n).expect("mesh wiring");
    let mut nodes = Vec::new();
    for (i, me) in names.iter().enumerate() {
        let links: Vec<PeerLink> = links_by_member.remove(me).expect("own links");
        let node_seed = seed.wrapping_add(u64::try_from(i).unwrap_or_default() * 7919) | 1;
        let log = MemLog::new();
        let store = MemStateStore::new();
        let sink = TestSink::default();
        let (tx, rx) = watch::channel(0u64);
        let supervisor = molt_net::supervisor::spawn(
            hub.transport(),
            NetConfig::fast(me.clone(), links.clone(), node_seed),
            log.clone(),
            store.clone(),
            sink.clone(),
            rx,
            None,
        );
        nodes.push(Node {
            member: me.clone(),
            log,
            store,
            wakeup: tx,
            sink,
            supervisor,
            links,
            hub: hub.clone(),
            seed: node_seed,
        });
    }
    nodes
}

/// Every inbound-queue id a node receives on FROM `peer` (its `rcvs` for that
/// peer) — the redundant queues the sender fans copies to.
fn inbound_queue_ids(node: &Node, peer: &str) -> Vec<molt_net::QueueId> {
    node.links
        .iter()
        .find(|l| l.member == peer)
        .expect("a link to the peer")
        .rcvs
        .iter()
        .map(|r| r.id.clone())
        .collect()
}

/// A deterministic non-nil message id for hand-built test envelopes.
fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

fn chat_env(by: &str, seq: u64, body: &str) -> EventEnvelope {
    EventEnvelope { prev_seq: 0,
        seq,
        ts: 1_000 + seq,
        by: by.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(test_msg_id(seq), by, body, 1_000 + seq)),
    }
}

/// Post one own event on a node: append to the log, bump the wakeup —
/// exactly what the engine's record() does.
fn post(node: &Node, seq: u64, body: &str) {
    node.log.push(chat_env(&node.member, seq, body));
    let _ = node.wakeup.send(seq);
}

/// Wait until `sink` holds `expect` deliveries (or time out).
async fn await_deliveries(sink: &TestSink, expect: usize, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while sink.delivered().len() < expect {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: got {} of {expect} deliveries",
            sink.delivered().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Exactly-once and per-sender order over one node's deliveries.
fn assert_exactly_once_in_order(who: &str, delivered: &[(MemberId, EventEnvelope)], senders: &[&str], per_sender: u64) {
    for sender in senders {
        let seqs: Vec<u64> = delivered
            .iter()
            .filter(|(from, _)| from == sender)
            .map(|(_, e)| e.seq)
            .collect();
        let want: Vec<u64> = (1..=per_sender).collect();
        assert_eq!(
            seqs, want,
            "{who}: deliveries from {sender} must be exactly-once and in sender order"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_nodes_converge_under_chaos() {
    for seed in [3u64, 17, 40_961] {
        let hub = LoopbackHub::new(ChaosPolicy {
            seed,
            delay_ms: (0, 30),
            drop_pct: 20,
            duplicate_pct: 20,
            redeliver_after_ms: 60,
        });
        let members = ["ada", "ben", "chi"];
        let nodes = mesh(&hub, &members, seed).await;
        let per_sender = 12u64;
        for node in &nodes {
            for k in 1..=per_sender {
                post(node, k, &format!("{} says {k}", node.member));
            }
        }
        let expect = usize::try_from(per_sender).expect("small") * (members.len() - 1);
        for node in &nodes {
            await_deliveries(&node.sink, expect, 30).await;
        }
        // settle so late duplicates would show up
        tokio::time::sleep(Duration::from_millis(200)).await;
        for node in &nodes {
            let delivered = node.sink.delivered();
            assert_eq!(delivered.len(), expect, "{} (seed {seed})", node.member);
            let others: Vec<&str> = members.iter().filter(|m| **m != node.member).copied().collect();
            assert_exactly_once_in_order(&node.member, &delivered, &others, per_sender);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbox_drains_after_partition_heals() {
    let hub = LoopbackHub::calm();
    let nodes = mesh(&hub, &["ada", "ben"], 5).await;
    hub.set_partitioned(true);
    post(&nodes[0], 1, "into the void");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(nodes[1].sink.delivered().is_empty(), "partitioned: nothing arrives");
    assert!(
        *nodes[0].sink.send_failures.lock().expect("lock") > 0,
        "the outbox reported the failing sends"
    );
    hub.set_partitioned(false);
    await_deliveries(&nodes[1].sink, 1, 10).await;
    assert_eq!(nodes[1].sink.delivered()[0].1.seq, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_resumes_from_cursors_without_duplicates() {
    let hub = LoopbackHub::calm();
    let mut nodes = mesh(&hub, &["ada", "ben"], 9).await;
    post(&nodes[0], 1, "before restart");
    await_deliveries(&nodes[1].sink, 1, 10).await;

    // ada "crashes" and comes back with only log + transport.state
    nodes[0].restart().await;
    post(&nodes[0], 2, "after restart");
    await_deliveries(&nodes[1].sink, 2, 10).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let delivered = nodes[1].sink.delivered();
    assert_eq!(delivered.len(), 2, "no duplicates after the restart");
    assert_exactly_once_in_order("ben", &delivered, &["ada"], 2);
}

/// Stage B: a severed subscription (the loopback analogue of a died SMP
/// recv loop) must resubscribe by ITSELF and resume delivery — and the sink
/// must have seen the leg go down and come back up, in that order (the
/// honest-health signals the engine turns into Degraded/Ok).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_severed_subscription_resubscribes_and_resumes_delivery() {
    let hub = LoopbackHub::calm();
    let nodes = mesh(&hub, &["ada", "ben"], 11).await;
    // baseline: delivery works
    post(&nodes[0], 1, "before the cut");
    await_deliveries(&nodes[1].sink, 1, 10).await;
    // sever every live subscription; unacked blocks stay pending on the hub
    hub.sever_subscriptions();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // the peer keeps sending — the watchdog must resubscribe and deliver
    post(&nodes[0], 2, "after the cut");
    await_deliveries(&nodes[1].sink, 2, 10).await;
    // ben saw ada's leg die and come back, in that order
    let events = nodes[1].sink.link_events();
    let down = events
        .iter()
        .position(|e| e.starts_with("down:ada"))
        .unwrap_or_else(|| panic!("no link_down for ada in {events:?}"));
    let up = events
        .iter()
        .rposition(|e| e == "up:ada")
        .unwrap_or_else(|| panic!("no link_up for ada in {events:?}"));
    assert!(up > down, "link_up must follow the link_down: {events:?}");
}

/// Stage B: a send that finally goes through after backing off fires the
/// sink's `send_ok` — the signal that clears the stuck-send flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healed_send_after_backoff_fires_send_ok() {
    let hub = LoopbackHub::calm();
    let nodes = mesh(&hub, &["ada", "ben"], 7).await;
    hub.set_partitioned(true);
    post(&nodes[0], 1, "queued behind the partition");
    // wait until the outbox reported the failure (it is now backing off)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while *nodes[0].sink.send_failures.lock().expect("lock") == 0 {
        assert!(tokio::time::Instant::now() < deadline, "no send_failed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    hub.set_partitioned(false);
    await_deliveries(&nodes[1].sink, 1, 10).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while *nodes[0].sink.send_oks.lock().expect("lock") == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the healed send never fired send_ok"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_queue_leg_survives_one_expired_queue() {
    // Track B Stage 2 redundancy: ben has TWO inbound queues from ada. One goes
    // silently deaf (idle-expired shape: SUB/SEND still Ok, deliveries dropped),
    // but ada's fan-out still reaches ben on the OTHER queue — the message
    // arrives with zero heal latency, the SimpleX-style redundancy win.
    let hub = LoopbackHub::calm();
    let nodes = mesh_n(&hub, &["ada", "ben"], 5, 2).await;
    let ben_from_ada = inbound_queue_ids(&nodes[1], "ada");
    assert_eq!(ben_from_ada.len(), 2, "an N=2 leg has two inbound queues");
    assert!(hub.expire_queue(&ben_from_ada[0]), "the queue exists to expire");

    post(&nodes[0], 1, "survives a dead queue");
    await_deliveries(&nodes[1].sink, 1, 10).await;
    // settle so a double delivery (if the surviving queue also duplicated) shows
    tokio::time::sleep(Duration::from_millis(150)).await;
    let delivered = nodes[1].sink.delivered();
    assert_eq!(delivered.len(), 1, "delivered exactly once via the surviving queue");
    assert_eq!(delivered[0].1.seq, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leg_stays_up_when_one_of_two_queues_dies() {
    // Stage-2 audit finding #2 (honesty): with N=2, deleting ONE of a leg's two
    // inbound queues (ending that forwarder's stream for good) must NOT make the
    // leg report DOWN — the other queue is still live, so the aggregate `live`
    // count stays ≥1 and no `link_down` fires. The per-peer live lock also keeps
    // the up/down notifications ordered so a healthy leg can't get stuck alarming.
    use molt_net::Transport;
    let hub = LoopbackHub::calm();
    let nodes = mesh_n(&hub, &["ada", "ben"], 11, 2).await;
    // baseline: ben hears ada on the N=2 leg
    post(&nodes[0], 1, "before the queue dies");
    await_deliveries(&nodes[1].sink, 1, 10).await;

    // delete ONE of ben's two inbound queues from ada (ends that forwarder)
    let ben_rcv0 = nodes[1]
        .links
        .iter()
        .find(|l| l.member == "ada")
        .expect("ben's link to ada")
        .rcvs[0]
        .clone();
    hub.transport().delete_queue(&ben_rcv0).await.expect("delete one queue");

    // ada keeps sending; ben must keep hearing it on the surviving queue
    post(&nodes[0], 2, "after one queue died");
    await_deliveries(&nodes[1].sink, 2, 10).await;
    // settle so any spurious link_down churn would surface
    tokio::time::sleep(Duration::from_millis(300)).await;

    let events = nodes[1].sink.link_events();
    assert!(
        !events.iter().any(|e| e.starts_with("down:ada")),
        "a leg with one surviving queue must NEVER report link_down — events: {events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| e.as_str() == "up:ada").count(),
        1,
        "the N=2 leg comes up exactly once (not once per queue) — events: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redundant_copies_dedup_to_one_delivery() {
    // N=2, no failure: ada fans the SAME ciphertext to BOTH of ben's queues, so
    // ben's ONE recv consumer (single shared Reassembler + sole-writer cursor)
    // sees each message twice and MUST dedup to a single delivery.
    let hub = LoopbackHub::calm();
    let nodes = mesh_n(&hub, &["ada", "ben"], 7, 2).await;
    assert_eq!(inbound_queue_ids(&nodes[1], "ada").len(), 2, "two inbound queues");

    for k in 1..=3u64 {
        post(&nodes[0], k, &format!("msg {k}"));
    }
    await_deliveries(&nodes[1].sink, 3, 10).await;
    // settle so the second copy of each (if dedup were broken) would arrive
    tokio::time::sleep(Duration::from_millis(200)).await;
    let delivered = nodes[1].sink.delivered();
    assert_eq!(
        delivered.len(),
        3,
        "three messages, each fanned to two queues, dedup to three deliveries"
    );
    assert_exactly_once_in_order("ben", &delivered, &["ada"], 3);
}
