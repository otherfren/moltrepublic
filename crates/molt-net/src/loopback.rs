// SPDX-License-Identifier: GPL-3.0-or-later

//! The in-process transport: an SMP-shaped hub of unidirectional queues.
//!
//! [`LoopbackHub`] plays the server side — store-and-forward queues,
//! at-least-once delivery with redelivery of unacked blocks, and an
//! injectable [`ChaosPolicy`] (delay, reorder via random delay, duplicate,
//! drop-first-attempt, partition). This is what replaces the old reply
//! simulator: simulated members become loopback peers driving the real
//! send → wrap → chunk → deliver → ack code paths, and the default
//! `cargo test` tier runs the whole stack on it without a socket.
//!
//! Determinism note: the chaos decisions are seeded and reproducible; the
//! *interleaving* still depends on the tokio scheduler, which is exactly
//! the nondeterminism the convergence tests are meant to survive.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{mockrand, MemberId};
use tokio::sync::mpsc;

use crate::supervisor::PeerLink;
use crate::wrap::WrapKey;
use crate::{AckToken, Delivery, NetError, PaddedBlock, QueueId, QueuePair, RcvQueue, SndQueueAddr, Transport};

/// Capacity of one queue's subscriber channel.
const SUB_CHANNEL: usize = 64;
/// Redelivery attempts per block before the hub stops re-arming (a
/// backstop against runaway tasks: ~5 min at the default interval). The
/// block stays pending — a fresh `subscribe` re-schedules it — and the
/// expiry is logged loudly: a receiver that cannot ack for this long is a
/// bug, not weather.
const MAX_REDELIVERIES: u32 = 600;

/// The injectable chaos policy (concept §7, loopback tier). All decisions
/// draw from one seeded RNG; `Default` is calm (no chaos, instant
/// delivery).
#[derive(Debug, Clone)]
pub struct ChaosPolicy {
    /// RNG seed for every chaos decision.
    pub seed: u64,
    /// Uniform per-delivery delay range in milliseconds (reordering falls
    /// out of unequal delays).
    pub delay_ms: (u64, u64),
    /// Percent chance to *not* deliver a block's first attempt (the
    /// redelivery timer picks it up — at-least-once, like an SMP server
    /// redelivering unacked messages).
    pub drop_pct: u8,
    /// Percent chance to deliver a block twice.
    pub duplicate_pct: u8,
    /// How long the hub waits for an ack before redelivering.
    pub redeliver_after_ms: u64,
}

impl Default for ChaosPolicy {
    fn default() -> Self {
        ChaosPolicy {
            seed: 1,
            delay_ms: (0, 0),
            drop_pct: 0,
            duplicate_pct: 0,
            redeliver_after_ms: 500,
        }
    }
}

/// One queue on the hub.
struct Queue {
    sub: Option<mpsc::Sender<Delivery>>,
    /// Unacked blocks by delivery id.
    pending: HashMap<u64, PaddedBlock>,
    /// Test seam for the SMP idle-expiry / "silently deaf" failure
    /// (`documents/mesh_selfheal.md` §6): when set, `SUB` and `SEND` still
    /// return `Ok` but every delivery to this queue is silently dropped —
    /// exactly the server-side state the client cannot detect. Loopback is
    /// otherwise permissive, so this is the only way to reproduce a
    /// live-but-non-delivering (born-dead) leg.
    expired: bool,
}

struct Hub {
    queues: HashMap<QueueId, Queue>,
    chaos: ChaosPolicy,
    rng: u64,
    partitioned: bool,
    next_delivery: u64,
}

/// The in-process "server": create it once per test/demo mesh, hand every
/// node a [`LoopbackHub::transport`].
#[derive(Clone)]
pub struct LoopbackHub {
    inner: Arc<Mutex<Hub>>,
}

impl LoopbackHub {
    /// A hub with the given chaos policy.
    pub fn new(chaos: ChaosPolicy) -> LoopbackHub {
        let rng = chaos.seed | 1; // xorshift must not start at 0
        LoopbackHub {
            inner: Arc::new(Mutex::new(Hub {
                queues: HashMap::new(),
                chaos,
                rng,
                partitioned: false,
                next_delivery: 1,
            })),
        }
    }

    /// A calm hub (no chaos) — the demo/default configuration.
    pub fn calm() -> LoopbackHub {
        LoopbackHub::new(ChaosPolicy::default())
    }

    /// Partition the whole hub: every `send` fails with
    /// [`NetError::Unreachable`] until healed. (Blocks already accepted
    /// stay queued and deliver normally — the partition models the *dial*,
    /// not the server's storage.)
    pub fn set_partitioned(&self, partitioned: bool) {
        if let Ok(mut hub) = self.inner.lock() {
            hub.partitioned = partitioned;
        }
    }

    /// Sever every queue's live subscription (test seam for the resubscribe
    /// watchdog): each subscriber's channel sender is dropped, so its
    /// receiver ends — exactly how a died SMP recv loop looks to the
    /// supervisor. Unacked blocks stay pending and redeliver to the NEXT
    /// subscriber, like a real server's store-and-forward.
    pub fn sever_subscriptions(&self) {
        if let Ok(mut hub) = self.inner.lock() {
            for q in hub.queues.values_mut() {
                q.sub = None;
            }
        }
    }

    /// Mark a queue **idle-expired / silently deaf** (test seam,
    /// `documents/mesh_selfheal.md` §6): from now on `SUB`/`SEND` still return
    /// `Ok` but every delivery to it is silently dropped — the exact
    /// server-side state that produces a born-dead / live-but-deaf leg. The
    /// `id` is a queue's `QueueId` (a `PeerLink`'s `rcv.id`, which equals the
    /// sender-face `snd.id` on the hub). Returns whether the queue existed.
    pub fn expire_queue(&self, id: &QueueId) -> bool {
        if let Ok(mut hub) = self.inner.lock() {
            if let Some(q) = hub.queues.get_mut(id) {
                q.expired = true;
                return true;
            }
        }
        false
    }

    /// A transport endpoint on this hub.
    pub fn transport(&self) -> LoopbackTransport {
        LoopbackTransport {
            hub: self.clone(),
            created: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Create a queue without an async context (mesh builders run inside
    /// the engine actor); the trait's `create_queue` wraps this.
    pub fn create_queue_blocking(&self) -> Result<QueuePair, NetError> {
        let id = QueueId::fresh()?;
        let mut hub = self.inner.lock().map_err(|_| NetError::Closed)?;
        hub.queues.insert(
            id.clone(),
            Queue {
                sub: None,
                pending: HashMap::new(),
                expired: false,
            },
        );
        Ok(QueuePair {
            rcv: RcvQueue {
                server: "loopback".to_string(),
                id: id.clone(),
            },
            snd: SndQueueAddr {
                server: "loopback".to_string(),
                id,
            },
        })
    }

    /// Schedule the delivery of one pending block: chaos delay first, then
    /// hand it to the subscriber (if any) and start the redelivery watch.
    fn schedule(&self, queue: QueueId, delivery: u64, attempt: u32, delay: Duration) {
        let hub = self.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let (sub, block, redeliver_after) = {
                let Ok(mut hub) = hub.inner.lock() else { return };
                // a silently-deaf (idle-expired) queue drops every delivery and
                // forgets it — SUB/SEND already returned Ok, so the sender never
                // learns; the block is removed so it does not redeliver forever
                if hub.queues.get(&queue).is_some_and(|q| q.expired) {
                    if let Some(q) = hub.queues.get_mut(&queue) {
                        q.pending.remove(&delivery);
                    }
                    return;
                }
                let Some(q) = hub.queues.get(&queue) else { return };
                let Some(block) = q.pending.get(&delivery) else {
                    return; // acked in the meantime
                };
                (
                    q.sub.clone(),
                    block.clone(),
                    Duration::from_millis(hub.chaos.redeliver_after_ms),
                )
            };
            if let Some(sub) = sub {
                let ack_hub = hub.clone();
                let ack_queue = queue.clone();
                let ack = AckToken::new(move || {
                    if let Ok(mut h) = ack_hub.inner.lock() {
                        if let Some(q) = h.queues.get_mut(&ack_queue) {
                            q.pending.remove(&delivery);
                        }
                    }
                });
                // a full/closed subscriber drops the delivery — the
                // redelivery below retries it
                let _ = sub.send(Delivery { block, ack }).await;
            }
            // redeliver while unacked (at-least-once)
            tokio::time::sleep(redeliver_after).await;
            let still_pending = hub
                .inner
                .lock()
                .ok()
                .map(|h| {
                    h.queues
                        .get(&queue)
                        .is_some_and(|q| q.pending.contains_key(&delivery))
                })
                .unwrap_or(false);
            if still_pending {
                if attempt < MAX_REDELIVERIES {
                    hub.schedule(queue, delivery, attempt + 1, Duration::ZERO);
                } else {
                    tracing::error!(
                        queue = %queue,
                        "block unacked after {MAX_REDELIVERIES} redeliveries — parked until a re-subscribe"
                    );
                }
            }
        });
    }

    /// Wire a full mesh over this hub: one queue plus a fresh wrap key per
    /// directed pair, returned as each member's [`PeerLink`] list. This is
    /// the out-of-band key/address handover that the T2 invite payload
    /// will carry in-band — and the ONE place that owns the subtle
    /// direction convention (`wrap_out` is the *peer's* inbound queue key,
    /// `wrap_in` our own). Sync on purpose: mesh builders run inside the
    /// engine actor.
    pub fn full_mesh(
        &self,
        members: &[MemberId],
    ) -> Result<BTreeMap<MemberId, Vec<PeerLink>>, NetError> {
        // inbound queue of (recipient, sender), with its wrap key
        let mut queues: BTreeMap<(MemberId, MemberId), (QueuePair, WrapKey)> = BTreeMap::new();
        for recipient in members {
            for sender in members {
                if sender == recipient {
                    continue;
                }
                let pair = self.create_queue_blocking()?;
                let key = WrapKey::fresh()?;
                queues.insert((recipient.clone(), sender.clone()), (pair, key));
            }
        }
        let mut mesh = BTreeMap::new();
        for me in members {
            let links = members
                .iter()
                .filter(|p| *p != me)
                .map(|peer| {
                    let (out_pair, out_key) = &queues[&(peer.clone(), me.clone())];
                    let (in_pair, in_key) = &queues[&(me.clone(), peer.clone())];
                    PeerLink {
                        member: peer.clone(),
                        snd: out_pair.snd.clone(),
                        wrap_out: out_key.clone(),
                        rcv: in_pair.rcv.clone(),
                        wrap_in: in_key.clone(),
                    }
                })
                .collect();
            mesh.insert(me.clone(), links);
        }
        Ok(mesh)
    }
}

/// One node's endpoint on a [`LoopbackHub`].
#[derive(Clone)]
pub struct LoopbackTransport {
    hub: LoopbackHub,
    /// Queue ids this endpoint created (= receives on): the loopback
    /// analogue of SMP's receive credentials. Shared across clones (like
    /// `SmpTransport`'s state Arc) so ritual/runtime clones export ONE
    /// credential set. Only meaningful while the hub lives — a new PROCESS
    /// cannot resume a loopback mesh; a fresh engine on the same hub
    /// (the tests' reopen seam) can.
    created: Arc<Mutex<BTreeSet<Vec<u8>>>>,
}

impl Transport for LoopbackTransport {
    async fn create_queue(&self) -> Result<QueuePair, NetError> {
        let pair = self.hub.create_queue_blocking()?;
        if let Ok(mut created) = self.created.lock() {
            created.insert(pair.rcv.id.0.clone());
        }
        Ok(pair)
    }

    fn export_creds(&self) -> Option<Vec<u8>> {
        let ids: Vec<String> = self.created.lock().ok()?.iter().map(hex::encode).collect();
        if ids.is_empty() {
            return None;
        }
        serde_json::to_vec(&ids).ok()
    }

    fn import_creds(&self, creds: &[u8]) {
        // bookkeeping only — the hub is permissive (any endpoint may
        // subscribe), so adopting the ids keeps a re-export after reopen
        // faithful
        let Ok(ids) = serde_json::from_slice::<Vec<String>>(creds) else {
            return;
        };
        if let Ok(mut c) = self.created.lock() {
            for id in ids {
                if let Ok(b) = hex::decode(id) {
                    c.insert(b);
                }
            }
        }
    }

    async fn send(&self, addr: &SndQueueAddr, block: PaddedBlock) -> Result<(), NetError> {
        let scheduled = {
            let mut hub = self.hub.inner.lock().map_err(|_| NetError::Closed)?;
            if hub.partitioned {
                return Err(NetError::Unreachable("partitioned".to_string()));
            }
            if !hub.queues.contains_key(&addr.id) {
                return Err(NetError::UnknownQueue);
            }
            // chaos decisions under the lock (one shared RNG)
            let chaos = hub.chaos.clone();
            let mut rng = hub.rng;
            let pct = |rng: &mut u64| mockrand::xorshift(rng) % 100;
            let copies: usize = if u64::from(chaos.duplicate_pct) > pct(&mut rng) {
                2
            } else {
                1
            };
            let mut scheduled = Vec::with_capacity(copies);
            for _ in 0..copies {
                let id = hub.next_delivery;
                hub.next_delivery += 1;
                let (lo, hi) = chaos.delay_ms;
                let delay = if hi > lo {
                    lo + mockrand::xorshift(&mut rng) % (hi - lo)
                } else {
                    lo
                };
                let dropped = u64::from(chaos.drop_pct) > pct(&mut rng);
                if dropped {
                    // first attempt lost: only the redelivery watch picks
                    // it up, after the ack timeout
                    scheduled.push((id, 1u32, Duration::from_millis(chaos.redeliver_after_ms)));
                } else {
                    scheduled.push((id, 0u32, Duration::from_millis(delay)));
                }
                if let Some(q) = hub.queues.get_mut(&addr.id) {
                    q.pending.insert(id, block.clone());
                }
            }
            hub.rng = rng;
            scheduled
        };
        for (id, attempt, delay) in scheduled {
            self.hub.schedule(addr.id.clone(), id, attempt, delay);
        }
        Ok(())
    }

    async fn subscribe(&self, q: &RcvQueue) -> Result<mpsc::Receiver<Delivery>, NetError> {
        let (tx, rx) = mpsc::channel(SUB_CHANNEL);
        let pending: Vec<u64> = {
            let mut hub = self.hub.inner.lock().map_err(|_| NetError::Closed)?;
            let queue = hub.queues.get_mut(&q.id).ok_or(NetError::UnknownQueue)?;
            queue.sub = Some(tx);
            queue.pending.keys().copied().collect()
        };
        // store-and-forward: everything unacked flows to the new subscriber
        for id in pending {
            self.hub.schedule(q.id.clone(), id, 0, Duration::ZERO);
        }
        Ok(rx)
    }

    async fn delete_queue(&self, q: &RcvQueue) -> Result<(), NetError> {
        let mut hub = self.hub.inner.lock().map_err(|_| NetError::Closed)?;
        hub.queues.remove(&q.id).ok_or(NetError::UnknownQueue)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(fill: u8) -> PaddedBlock {
        PaddedBlock::from_bytes(vec![fill; crate::PADDED_BLOCK_LEN]).expect("size")
    }

    #[tokio::test]
    async fn send_deliver_ack_no_redelivery() {
        let hub = LoopbackHub::calm();
        let t = hub.transport();
        let pair = t.create_queue().await.expect("queue");
        let mut rx = t.subscribe(&pair.rcv).await.expect("sub");
        t.send(&pair.snd, block(7)).await.expect("send");
        let d = rx.recv().await.expect("delivery");
        assert_eq!(d.block, block(7));
        d.ack.ack();
        // acked: nothing redelivers
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unacked_blocks_redeliver_and_survive_resubscribe() {
        let hub = LoopbackHub::new(ChaosPolicy {
            redeliver_after_ms: 50,
            ..ChaosPolicy::default()
        });
        let t = hub.transport();
        let pair = t.create_queue().await.expect("queue");
        // no subscriber yet: store-and-forward
        t.send(&pair.snd, block(1)).await.expect("send");
        let mut rx = t.subscribe(&pair.rcv).await.expect("sub");
        let d = rx.recv().await.expect("first");
        drop(d.ack); // deliberately never ack (drop without arming)
        let d2 = rx.recv().await.expect("redelivered");
        assert_eq!(d2.block, block(1));
        d2.ack.ack();
    }

    #[tokio::test]
    async fn partition_fails_sends_until_healed() {
        let hub = LoopbackHub::calm();
        let t = hub.transport();
        let pair = t.create_queue().await.expect("queue");
        hub.set_partitioned(true);
        assert!(matches!(
            t.send(&pair.snd, block(2)).await,
            Err(NetError::Unreachable(_))
        ));
        hub.set_partitioned(false);
        t.send(&pair.snd, block(2)).await.expect("healed");
    }

    #[tokio::test]
    async fn unknown_queue_is_an_error() {
        let hub = LoopbackHub::calm();
        let t = hub.transport();
        let ghost = SndQueueAddr {
            server: "loopback".to_string(),
            id: QueueId::fresh().expect("rng"),
        };
        assert!(matches!(
            t.send(&ghost, block(3)).await,
            Err(NetError::UnknownQueue)
        ));
    }

    /// The idle-expiry / "silently deaf" test seam (mesh self-heal §6): once a
    /// queue is expired, `SUB` and `SEND` still succeed but deliveries vanish —
    /// exactly the server-side state the client cannot detect and the only way
    /// to reproduce a born-dead leg on the otherwise-permissive hub.
    #[tokio::test]
    async fn an_expired_queue_accepts_sub_send_but_never_delivers() {
        let hub = LoopbackHub::calm();
        let t = hub.transport();
        let pair = t.create_queue().await.expect("queue");
        let mut rx = t.subscribe(&pair.rcv).await.expect("sub"); // SUB ok
        // a normal send delivers
        t.send(&pair.snd, block(1)).await.expect("send");
        assert_eq!(rx.recv().await.expect("delivery").block, block(1));
        // the queue idle-expires: SUB/SEND stay Ok, but deliveries silently drop
        assert!(hub.expire_queue(&pair.rcv.id), "the queue exists");
        t.send(&pair.snd, block(2))
            .await
            .expect("SEND still returns Ok on an expired queue");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            rx.try_recv().is_err(),
            "an expired queue delivers nothing — the silent-deafness the client cannot see"
        );
    }
}
