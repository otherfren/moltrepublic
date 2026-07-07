// SPDX-License-Identifier: GPL-3.0-or-later

//! `SmpTransport`: the [`Transport`] trait over real SMP servers.
//!
//! Maps the transport abstraction onto the SMP command layer
//! ([`SmpConn`]): `create_queue` → `NEW`, `send` → `SKEY`(once) + signed
//! `SEND`, `subscribe` → `SUB` + a `recv_next` loop, `delete_queue` →
//! `DEL`. So the engine and the founding ritual run over real SMP exactly
//! as they run over the loopback hub — same trait, same code.
//!
//! Connection model (deliberately simple for the ritual's low volume): a
//! fresh connection per send; one long-lived connection per subscription.
//! The recipient keys of queues we created, and the sender key each queue
//! was secured with, are remembered so any later send/subscribe works from
//! a fresh connection (SMP securing is server-side state, not per
//! connection). Pooling is a later optimisation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use tokio::sync::mpsc;

use crate::block::PADDED_BLOCK_LEN;
use crate::smp::conn::{NewQueue, SmpConn};
use crate::smp::server::SmpServer;
use crate::{
    AckToken, Delivery, NetError, PaddedBlock, QueueId, QueuePair, RcvQueue, SndQueueAddr,
    Transport,
};

/// Per-node SMP transport bound to one server (multi-server routing by the
/// queue's own server address is a later step).
#[derive(Clone)]
pub struct SmpTransport {
    server: SmpServer,
    state: Arc<Mutex<SmpState>>,
}

#[derive(Default)]
struct SmpState {
    /// Queues we created (recipient side), by recipient id.
    recv: HashMap<Vec<u8>, NewQueue>,
    /// The sender key each queue we send to was secured with, by sender id.
    send_keys: HashMap<Vec<u8>, SigningKey>,
}

impl SmpTransport {
    /// A transport that creates its queues on, and sends through, `server`.
    pub fn new(server: SmpServer) -> SmpTransport {
        SmpTransport {
            server,
            state: Arc::new(Mutex::new(SmpState::default())),
        }
    }

    fn recv_queue(&self, id: &[u8]) -> Option<NewQueue> {
        self.state.lock().ok()?.recv.get(id).cloned()
    }
}

/// The serializable form of one created queue's recipient credential.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedQueue {
    recipient_id: Vec<u8>,
    sender_id: Vec<u8>,
    auth_sk: [u8; 32],
    dh_secret: [u8; 32],
    server_dh: [u8; 32],
}

/// The serializable form of a transport's whole credential set (`SmpState`):
/// the queues we can receive on, and the sender keys we send peer queues with.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedCreds {
    recv: Vec<PersistedQueue>,
    send_keys: Vec<(Vec<u8>, [u8; 32])>,
}

impl SmpTransport {
    /// Snapshot the credential set for `transport.state` (reopen re-adopts it).
    fn creds_bytes(&self) -> Option<Vec<u8>> {
        let s = self.state.lock().ok()?;
        let recv = s
            .recv
            .values()
            .map(|q| PersistedQueue {
                recipient_id: q.recipient_id.clone(),
                sender_id: q.sender_id.clone(),
                auth_sk: q.auth_sk.to_bytes(),
                dh_secret: q.dh_secret,
                server_dh: q.server_dh,
            })
            .collect();
        let send_keys = s
            .send_keys
            .iter()
            .map(|(id, k)| (id.clone(), k.to_bytes()))
            .collect();
        bincode::serialize(&PersistedCreds { recv, send_keys }).ok()
    }

    /// Re-adopt a persisted credential set into this (fresh) transport.
    fn adopt_creds(&self, bytes: &[u8]) {
        let Ok(creds) = bincode::deserialize::<PersistedCreds>(bytes) else {
            return;
        };
        let Ok(mut s) = self.state.lock() else {
            return;
        };
        for q in creds.recv {
            s.recv.insert(
                q.recipient_id.clone(),
                NewQueue {
                    recipient_id: q.recipient_id,
                    sender_id: q.sender_id,
                    auth_sk: SigningKey::from_bytes(&q.auth_sk),
                    dh_secret: q.dh_secret,
                    server_dh: q.server_dh,
                },
            );
        }
        for (id, k) in creds.send_keys {
            s.send_keys.insert(id, SigningKey::from_bytes(&k));
        }
    }
}

impl Transport for SmpTransport {
    async fn create_queue(&self) -> Result<QueuePair, NetError> {
        let mut conn = SmpConn::connect(&self.server).await?;
        let q = conn.new_queue(false).await?;
        let rcv = RcvQueue {
            id: QueueId::from_bytes(q.recipient_id.clone()),
        };
        let snd = SndQueueAddr {
            server: self.server.render(),
            id: QueueId::from_bytes(q.sender_id.clone()),
        };
        if let Ok(mut s) = self.state.lock() {
            s.recv.insert(q.recipient_id.clone(), q);
        }
        Ok(QueuePair { rcv, snd })
    }

    async fn send(&self, addr: &SndQueueAddr, block: PaddedBlock) -> Result<(), NetError> {
        let sender_id = &addr.id.0;
        let existing = self.state.lock().ok().and_then(|s| s.send_keys.get(sender_id).cloned());
        let mut conn = SmpConn::connect(&self.server).await?;
        let key = match existing {
            Some(k) => k,
            None => {
                // secure the queue as sender the first time (server-side,
                // so later sends from fresh connections reuse the key)
                let k = conn.secure_as_sender(sender_id).await?;
                if let Ok(mut s) = self.state.lock() {
                    s.send_keys.insert(sender_id.clone(), k.clone());
                }
                k
            }
        };
        conn.send_to(sender_id, &key, block.as_slice()).await
    }

    async fn subscribe(
        &self,
        q: &RcvQueue,
    ) -> Result<mpsc::Receiver<Delivery>, NetError> {
        let queue = self
            .recv_queue(&q.id.0)
            .ok_or_else(|| NetError::Framing("subscribe to a queue this node did not create".into()))?;
        let mut conn = SmpConn::connect(&self.server).await?;
        conn.sub(&queue.recipient_id, &queue.auth_sk).await?;
        let (tx, rx) = mpsc::channel::<Delivery>(64);
        tokio::spawn(async move {
            loop {
                match conn.recv_next(&queue).await {
                    Ok(body) => {
                        // the delivered body is our fixed-size block plus
                        // the server's row padding — take exactly one block
                        let Some(slice) = body.get(..PADDED_BLOCK_LEN) else {
                            tracing::warn!("SMP message shorter than one block — dropped");
                            continue;
                        };
                        let Ok(block) = PaddedBlock::from_bytes(slice.to_vec()) else {
                            continue;
                        };
                        // recv_next acks lazily on the next call, so the
                        // Delivery's own ack is a no-op (at-least-once; the
                        // reassembler + cursors absorb any redelivery)
                        if tx.send(Delivery { block, ack: AckToken::noop() }).await.is_err() {
                            return; // subscriber gone
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "SMP subscription ended");
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn delete_queue(&self, q: &RcvQueue) -> Result<(), NetError> {
        let Some(queue) = self.recv_queue(&q.id.0) else {
            return Ok(());
        };
        let mut conn = SmpConn::connect(&self.server).await?;
        conn.delete(&queue.recipient_id, &queue.auth_sk).await?;
        if let Ok(mut s) = self.state.lock() {
            s.recv.remove(&q.id.0);
        }
        Ok(())
    }

    fn export_creds(&self) -> Option<Vec<u8>> {
        self.creds_bytes()
    }

    fn import_creds(&self, creds: &[u8]) {
        self.adopt_creds(creds);
    }
}
