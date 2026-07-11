// SPDX-License-Identifier: GPL-3.0-or-later

//! `SmpTransport`: the [`Transport`] trait over real SMP servers.
//!
//! Maps the transport abstraction onto the SMP command layer
//! ([`SmpConn`]): `create_queue` → `NEW`, `send` → `SKEY`(once) + signed
//! `SEND`, `subscribe` → `SUB` + a `recv_next` loop, `delete_queue` →
//! `DEL`. So the engine and the founding ritual run over real SMP exactly
//! as they run over the loopback hub — same trait, same code.
//!
//! Connection model: a pooled, persistent connection per server, reused
//! across `create_queue`/`send`/`delete_queue` (a fresh dial per op is a
//! whole Tor circuit + TLS handshake — pathological over Tor; T4 §P4). The
//! `subscribe` path keeps its OWN long-lived connection (its dedicated recv
//! loop) and never goes through the pool. The recipient keys of queues we
//! created, and the sender key each queue was secured with, are remembered
//! so a send/subscribe survives a reconnect (SMP securing is server-side
//! state, not per connection).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

use crate::block::PADDED_BLOCK_LEN;
use crate::smp::conn::{NewQueue, SmpConn};
use crate::smp::server::SmpServer;
use crate::smp::tls::Dialer;
use crate::{
    AckToken, Delivery, NetError, PaddedBlock, QueueId, QueuePair, RcvQueue, SndQueueAddr,
    Transport,
};

/// Per-node SMP transport bound to one server (multi-server routing by the
/// queue's own server address is a later step).
#[derive(Clone)]
pub struct SmpTransport {
    server: SmpServer,
    dialer: Dialer,
    state: Arc<Mutex<SmpState>>,
    /// The reused connection for `create_queue`/`send`/`delete_queue` (one per
    /// server; `subscribe` keeps its own). Clones of a transport share it.
    pool: ConnPool<SmpConn>,
}

#[derive(Default)]
struct SmpState {
    /// Queues we created (recipient side), by recipient id.
    recv: HashMap<Vec<u8>, NewQueue>,
    /// The sender key each queue we send to was secured with, by sender id.
    send_keys: HashMap<Vec<u8>, SigningKey>,
}

impl SmpTransport {
    /// A transport that creates its queues on, and sends through, `server`,
    /// dialing directly (clearnet/loopback).
    pub fn new(server: SmpServer) -> SmpTransport {
        SmpTransport::with_dialer(server, Dialer::Direct)
    }

    /// Like [`new`](SmpTransport::new) but routes every connection through
    /// `dialer` — e.g. a SOCKS5h Tor proxy (concept §4).
    pub fn with_dialer(server: SmpServer, dialer: Dialer) -> SmpTransport {
        SmpTransport {
            server,
            dialer,
            state: Arc::new(Mutex::new(SmpState::default())),
            pool: ConnPool::new(),
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

/// A connection the [`ConnPool`] can (re)dial. [`SmpConn`] in production; a
/// stub in the pool's own tests. Only the *dial* is abstracted — each pooled
/// operation is passed to [`ConnPool::with_conn`] as a closure, so the pool
/// stays agnostic to the SMP command layer.
trait PooledConn: Sized + Send + 'static {
    /// Open (dial + handshake) one fresh connection to `server` via `dialer`.
    fn dial(dialer: Dialer, server: SmpServer)
        -> impl Future<Output = Result<Self, NetError>> + Send;
}

impl PooledConn for SmpConn {
    async fn dial(dialer: Dialer, server: SmpServer) -> Result<SmpConn, NetError> {
        SmpConn::connect(&dialer, &server).await
    }
}

/// A boxed pooled-operation future borrowing the connection for `'a`.
type ConnFut<'a, R> = Pin<Box<dyn Future<Output = Result<R, NetError>> + Send + 'a>>;

/// One persistent connection per server, reused across the request/response SMP
/// operations (`create_queue`/`send`/`delete_queue`) instead of a fresh dial
/// per op — over Tor a fresh dial is a whole new circuit + TLS handshake, which
/// is pathological (T4 §P4). The `subscribe` path keeps its own long-lived
/// connection (its dedicated recv loop) and does NOT go through this pool.
///
/// Concurrency tradeoff: the async mutex serialises operations on the one
/// connection. SMP is request/response with a per-connection correlation
/// counter and session id, so interleaving two operations on one connection
/// would corrupt correlation — serialising is *correct*, not merely convenient.
/// A concurrent op waits, but the wait is bounded (every read/write carries the
/// 30 s `BLOCK_IO_TIMEOUT` landed in Stage A), so a wedged connection can never
/// deadlock the transport: the op times out, the broken connection is dropped,
/// and the next turn re-dials.
///
/// The pool holds only the shared slot + dial counter (two `Arc`s); the
/// `dialer`/`server` needed to (re)dial are passed in by [`SmpTransport`], which
/// already owns them — no duplication, so wrapping this in the transport keeps
/// the transport small (it rides inside `RitualTransport`, whose variant sizes
/// clippy watches).
struct ConnPool<C> {
    /// The one live connection (async mutex: held across an op's I/O).
    slot: Arc<AsyncMutex<Option<C>>>,
    /// Total dials (circuit builds) — instrumentation the pool tests assert on.
    dials: Arc<AtomicU64>,
}

// Manual `Clone`: transport clones share the SAME connection slot (one
// connection per server for the whole node), independent of whether `C: Clone`
// (`SmpConn` is not).
impl<C> Clone for ConnPool<C> {
    fn clone(&self) -> Self {
        ConnPool {
            slot: self.slot.clone(),
            dials: self.dials.clone(),
        }
    }
}

impl<C: PooledConn> ConnPool<C> {
    fn new() -> ConnPool<C> {
        ConnPool {
            slot: Arc::new(AsyncMutex::new(None)),
            dials: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Dial one fresh connection and count it.
    async fn open(&self, dialer: &Dialer, server: &SmpServer) -> Result<C, NetError> {
        let c = C::dial(dialer.clone(), server.clone()).await?;
        self.dials.fetch_add(1, Ordering::Relaxed);
        Ok(c)
    }

    /// Run one pooled operation, reusing the live connection (opening one
    /// lazily via `dialer`/`server`). On an error the broken connection is
    /// dropped; whether `op` is retried once on a fresh dial depends on
    /// `idempotent`:
    ///
    /// - `idempotent = true` (e.g. `send`): retry on any failure. Safe because
    ///   the transport is at-least-once (the peer dedups a re-sent block by its
    ///   message id) and the op re-reads the securing-key state, so a reconnect
    ///   never double-`SKEY`s.
    /// - `idempotent = false` (`NEW`/`DEL` — a queue create/delete the server
    ///   does NOT dedup): retry ONLY when the failed connection was *reused*
    ///   from the pool, i.e. a stale connection whose write fails fast before
    ///   the request reaches the server — healing it is safe. A failure on a
    ///   *freshly dialed* connection is returned as-is, so a lost response after
    ///   the server already applied the op is never retried into a second queue
    ///   (the double-allocation this used to cause). Residual: a reused
    ///   connection whose write reached the server but whose response was lost
    ///   can still re-issue — a narrow window, benign to node state.
    ///
    /// A *dial* failure is returned straight away (the outbox backs off).
    async fn with_conn<R, F>(
        &self,
        dialer: &Dialer,
        server: &SmpServer,
        idempotent: bool,
        mut op: F,
    ) -> Result<R, NetError>
    where
        F: for<'a> FnMut(&'a mut C) -> ConnFut<'a, R>,
    {
        let mut slot = self.slot.lock().await;
        let mut last: Option<NetError> = None;
        for _ in 0..2 {
            let reused = slot.is_some();
            if slot.is_none() {
                *slot = Some(self.open(dialer, server).await?);
            }
            let result = {
                let conn = slot.as_mut().expect("just ensured a connection");
                op(conn).await
            };
            match result {
                Ok(r) => return Ok(r),
                Err(e) => {
                    *slot = None; // drop the broken connection
                    last = Some(e);
                    // a non-idempotent op only retries to heal a *stale reused*
                    // connection (write fails before the server sees it); after
                    // a fresh dial the request may have been applied — do not
                    // re-issue it.
                    if !idempotent && !reused {
                        break;
                    }
                }
            }
        }
        Err(last.unwrap_or(NetError::Closed))
    }

    /// Total dials so far — the pool tests assert reuse (one dial) vs reconnect
    /// (two).
    #[cfg(test)]
    fn dials(&self) -> u64 {
        self.dials.load(Ordering::Relaxed)
    }
}

impl Transport for SmpTransport {
    async fn create_queue(&self) -> Result<QueuePair, NetError> {
        let q = self
            .pool
            .with_conn(&self.dialer, &self.server, false, |c: &mut SmpConn| {
                Box::pin(async move { c.new_queue(false).await })
            })
            .await?;
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
        let sender_id = addr.id.0.clone();
        let state = self.state.clone();
        self.pool
            .with_conn(&self.dialer, &self.server, true, |c: &mut SmpConn| {
                let sender_id = sender_id.clone();
                let state = state.clone();
                let block = block.clone();
                Box::pin(async move {
                    // re-read the securing key inside the op so a reconnect
                    // retry (after a broken connection) never re-`SKEY`s a queue
                    // already secured on an earlier attempt.
                    let cached = state
                        .lock()
                        .ok()
                        .and_then(|s| s.send_keys.get(&sender_id).cloned());
                    let key = match cached {
                        Some(k) => k,
                        None => {
                            // secure the queue as sender the first time
                            // (server-side, so later sends reuse the key even
                            // from a reconnected connection)
                            let k = c.secure_as_sender(&sender_id).await?;
                            if let Ok(mut s) = state.lock() {
                                s.send_keys.insert(sender_id.clone(), k.clone());
                            }
                            k
                        }
                    };
                    c.send_to(&sender_id, &key, block.as_slice()).await
                })
            })
            .await
    }

    async fn subscribe(
        &self,
        q: &RcvQueue,
    ) -> Result<mpsc::Receiver<Delivery>, NetError> {
        let queue = self
            .recv_queue(&q.id.0)
            .ok_or_else(|| NetError::Framing("subscribe to a queue this node did not create".into()))?;
        let mut conn = SmpConn::connect(&self.dialer, &self.server).await?;
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
        let recipient_id = queue.recipient_id.clone();
        let auth_sk = queue.auth_sk.clone();
        self.pool
            .with_conn(&self.dialer, &self.server, false, |c: &mut SmpConn| {
                let recipient_id = recipient_id.clone();
                let auth_sk = auth_sk.clone();
                Box::pin(async move { c.delete(&recipient_id, &auth_sk).await })
            })
            .await?;
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

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

    const FP: &str = "f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=";

    fn dummy_server() -> SmpServer {
        SmpServer::parse(&format!("smp://{FP}@example.invalid")).expect("server")
    }

    /// A stub connection: never touches a socket, so the pool's reuse/reconnect
    /// logic is testable without a live SMP server. Op failures are simulated in
    /// the op closures the tests pass to [`ConnPool::with_conn`].
    struct TestConn;
    impl PooledConn for TestConn {
        async fn dial(_dialer: Dialer, _server: SmpServer) -> Result<TestConn, NetError> {
            Ok(TestConn)
        }
    }

    /// P4: a second operation reuses the first connection — two sequential ops on
    /// one pool open exactly ONE connection, not two.
    #[tokio::test]
    async fn a_second_send_reuses_the_first_connection() {
        let pool: ConnPool<TestConn> = ConnPool::new();
        let (dialer, server) = (Dialer::Direct, dummy_server());
        let ops = Arc::new(AtomicU64::new(0));
        for _ in 0..2 {
            let ops = ops.clone();
            pool.with_conn(&dialer, &server, true, move |_c: &mut TestConn| {
                let ops = ops.clone();
                Box::pin(async move {
                    ops.fetch_add(1, SeqCst);
                    Ok::<(), NetError>(())
                })
            })
            .await
            .expect("op ok");
        }
        assert_eq!(ops.load(SeqCst), 2, "both operations ran");
        assert_eq!(
            pool.dials(),
            1,
            "two operations must share one dialed connection"
        );
    }

    /// P4: a broken pooled connection reconnects transparently — an op that fails
    /// once (the connection dropped under it) succeeds on the freshly re-dialed
    /// connection instead of erroring out to the caller.
    #[tokio::test]
    async fn a_broken_pooled_connection_reconnects_transparently() {
        let pool: ConnPool<TestConn> = ConnPool::new();
        let (dialer, server) = (Dialer::Direct, dummy_server());
        pool.with_conn(&dialer, &server, true, |_c: &mut TestConn| {
            Box::pin(async { Ok::<(), NetError>(()) })
        })
        .await
        .expect("first op opens a connection");
        assert_eq!(pool.dials(), 1);

        let fail_once = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicU64::new(0));
        let res = pool
            .with_conn(&dialer, &server, true, |_c: &mut TestConn| {
                let fail_once = fail_once.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, SeqCst);
                    if fail_once.swap(false, SeqCst) {
                        Err(NetError::Unreachable("broken pooled connection".into()))
                    } else {
                        Ok::<(), NetError>(())
                    }
                })
            })
            .await;
        assert!(res.is_ok(), "reconnects rather than erroring out: {res:?}");
        assert_eq!(calls.load(SeqCst), 2, "op retried once after the break");
        assert_eq!(pool.dials(), 2, "reconnected on a fresh dial");
    }

    /// Review fix: a NON-idempotent op (queue create/delete) that fails on a
    /// *freshly dialed* connection must NOT be retried — a lost response after
    /// the server already applied it would otherwise allocate a second queue.
    #[tokio::test]
    async fn a_fresh_dial_failure_is_not_retried_for_a_non_idempotent_op() {
        let pool: ConnPool<TestConn> = ConnPool::new();
        let (dialer, server) = (Dialer::Direct, dummy_server());
        let calls = Arc::new(AtomicU64::new(0));
        let res = pool
            .with_conn(&dialer, &server, false, |_c: &mut TestConn| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, SeqCst);
                    Err::<(), NetError>(NetError::TorUnavailable("response lost".into()))
                })
            })
            .await;
        assert!(res.is_err(), "the failure is surfaced, not retried away");
        assert_eq!(calls.load(SeqCst), 1, "a non-idempotent op must run only once on a fresh dial");
        assert_eq!(pool.dials(), 1, "no second dial / no second queue allocation");
    }

    /// ...but a non-idempotent op still heals a *stale reused* connection: the
    /// first (reused) attempt failing fast re-dials once, so a create/delete
    /// after the pool went stale doesn't spuriously fail.
    #[tokio::test]
    async fn a_non_idempotent_op_heals_a_stale_reused_connection() {
        let pool: ConnPool<TestConn> = ConnPool::new();
        let (dialer, server) = (Dialer::Direct, dummy_server());
        // warm the pool so the next op reuses the connection
        pool.with_conn(&dialer, &server, true, |_c: &mut TestConn| {
            Box::pin(async { Ok::<(), NetError>(()) })
        })
        .await
        .expect("warm");
        assert_eq!(pool.dials(), 1);

        let fail_once = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicU64::new(0));
        let res = pool
            .with_conn(&dialer, &server, false, |_c: &mut TestConn| {
                let fail_once = fail_once.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, SeqCst);
                    if fail_once.swap(false, SeqCst) {
                        Err(NetError::Unreachable("stale reused connection".into()))
                    } else {
                        Ok::<(), NetError>(())
                    }
                })
            })
            .await;
        assert!(res.is_ok(), "a stale reused connection is healed: {res:?}");
        assert_eq!(calls.load(SeqCst), 2, "retried once on the reused-then-broken connection");
        assert_eq!(pool.dials(), 2);
    }
}
