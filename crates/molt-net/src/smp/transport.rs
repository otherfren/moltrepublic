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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// Per-node SMP transport: queues are created on the configured server(s), and
/// every send/subscribe is routed to the server the queue itself names — a
/// configured one, or (pinned, bounded) one only the peer configured.
#[derive(Clone)]
pub struct SmpTransport {
    /// The server(s) this transport creates queues on and reaches. N=2
    /// redundancy spreads a leg's queues across them; `subscribe`/`send`/
    /// `delete_queue` route by the queue's OWN server
    /// (`RcvQueue.server`/`SndQueueAddr.server`). A single-element list is the
    /// former single-server transport, byte-for-byte in behaviour.
    servers: Vec<SmpServer>,
    dialer: Dialer,
    state: Arc<Mutex<SmpState>>,
    /// One reused connection pool per server (parallel to `servers`) for
    /// `create_queue`/`send`/`delete_queue`; `subscribe` keeps its own
    /// connection. Clones of a transport share them.
    pools: Vec<ConnPool<SmpConn>>,
    /// Pools for servers this transport did **not** configure but a queue names
    /// — a peer's inbound queue on its own server, or (after a reopen) a leg of
    /// our persisted mesh beyond the truncated server list. Keyed by the
    /// server's rendered address, created on first route, shared by clones, and
    /// bounded by [`MAX_ROUTED_SERVERS`].
    routed: Arc<Mutex<RoutedPools>>,
    /// Round-robin cursor spreading `create_queue` across `servers` (shared by
    /// clones), so a peer's N redundant inbound queues land on different servers.
    next: Arc<AtomicUsize>,
    /// Monotonic use stamp for the `routed` table's LRU eviction.
    routed_clock: Arc<AtomicU64>,
}

#[derive(Default)]
struct SmpState {
    /// Queues we created (recipient side), by recipient id.
    recv: HashMap<Vec<u8>, NewQueue>,
    /// The sender key each queue we send to was secured with, by sender id.
    send_keys: HashMap<Vec<u8>, SigningKey>,
    /// The seed every per-queue sender key is derived from
    /// ([`derive_sender_key`]). Minted at transport creation, exported with
    /// the creds, adopted on import — so a reopened transport re-derives the
    /// SAME key a queue was secured with, regardless of when the creds were
    /// persisted (the 2026-07-19 restart fix). `None` only when the RNG
    /// failed at creation: sends then fail honestly instead of falling back
    /// to a predictable (attacker-pre-SKEYable) constant seed.
    sender_seed: Option<[u8; 32]>,
}

/// Lazily opened pools for servers only a queue names, keyed by rendered
/// address. The `u64` is a use stamp: when the bound is reached the
/// least-recently-used entry is evicted, so a member that keeps announcing
/// ever-new hosts cannot permanently fill the table and push the servers of
/// HONEST peers onto the primary (where their queues do not exist).
type RoutedPools = HashMap<String, (SmpServer, ConnPool<SmpConn>, u64)>;

/// How many **unconfigured** servers one transport will dial and keep a pooled
/// connection for (queues a peer hosts on its own servers, plus a resumed
/// mesh's servers beyond the configured list). A generous bound — a republic's
/// legs name at most two servers per peer — that exists so a misbehaving member
/// cannot make us hold connections to arbitrarily many hosts by announcing
/// ever-new ones. Beyond it the least-recently-used entry is evicted (its
/// pooled connection closes with it) — never a fallback to the primary, which
/// would mis-route an honest peer's queue to a server it does not live on.
const MAX_ROUTED_SERVERS: usize = 64;

/// Derive the deterministic sender key for one queue:
/// `Ed25519(HMAC-SHA256(key = seed, msg = "molt-smp-sender-v1" ‖ sender_id))`.
fn derive_sender_key(seed: &[u8; 32], sender_id: &[u8]) -> SigningKey {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(seed)
        .expect("hmac accepts any key length");
    mac.update(b"molt-smp-sender-v1");
    mac.update(sender_id);
    let out: [u8; 32] = mac.finalize().into_bytes().into();
    SigningKey::from_bytes(&out)
}

impl SmpTransport {
    /// A transport that creates its queues on, and sends through, `server`,
    /// dialing directly (clearnet/loopback).
    pub fn new(server: SmpServer) -> SmpTransport {
        SmpTransport::with_dialer(server, Dialer::Direct)
    }

    /// A transport spread across `servers` (N=2 redundancy), dialing directly.
    pub fn new_multi(servers: Vec<SmpServer>) -> SmpTransport {
        SmpTransport::with_dialer_multi(servers, Dialer::Direct)
    }

    /// Like [`new`](SmpTransport::new) but routes every connection through
    /// `dialer` — e.g. a SOCKS5h Tor proxy (concept §4).
    pub fn with_dialer(server: SmpServer, dialer: Dialer) -> SmpTransport {
        SmpTransport::with_dialer_multi(vec![server], dialer)
    }

    /// The general constructor: N servers + a dialer. A single-element `servers`
    /// is the former single-server transport, identical in behaviour.
    ///
    /// An EMPTY `servers` would divide-by-zero in `create_queue`'s round-robin
    /// and index-panic in `route` (Stage-2 audit finding #3) — and the deferred
    /// config server-list path could feed one in. Guard it: an empty list is
    /// treated as a single loopback-style placeholder server (`smp://…@invalid`)
    /// so the transport is always non-empty; every dial against it simply fails
    /// (fail-closed) instead of panicking.
    pub fn with_dialer_multi(mut servers: Vec<SmpServer>, dialer: Dialer) -> SmpTransport {
        if servers.is_empty() {
            tracing::error!("SmpTransport built with no servers — using a fail-closed placeholder");
            if let Ok(placeholder) = SmpServer::parse(
                "smp://AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=@no-server.invalid",
            ) {
                servers.push(placeholder);
            }
        }
        // mint the sender seed once per transport incarnation; on an RNG
        // failure it stays None (first send fails with `NetError::Crypto`) —
        // NEVER a constant fallback, which would let anyone pre-`SKEY` the
        // queues this node is about to secure
        let mut seed = [0u8; 32];
        let sender_seed = getrandom::getrandom(&mut seed).ok().map(|()| seed);
        let pools = servers.iter().map(|_| ConnPool::new()).collect();
        SmpTransport {
            servers,
            dialer,
            state: Arc::new(Mutex::new(SmpState {
                sender_seed,
                ..SmpState::default()
            })),
            pools,
            routed: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicUsize::new(0)),
            routed_clock: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Route to the `(server, pool)` for a queue's own server string.
    ///
    /// A configured server is matched by rendered address. A server we did NOT
    /// configure but whose address parses — which means it carries a valid
    /// 32-byte server pin — gets its own lazily created, shared connection pool
    /// and is dialed **there** ([`MAX_ROUTED_SERVERS`] of them at most). That is
    /// what lets a peer host its inbound queues on servers of its own choosing:
    /// before it, an unconfigured server silently collapsed to our primary, so
    /// N=2 redundancy demanded a shared server set across all members and a
    /// resumed mesh spread wider than [`crate::MESH_REDUNDANCY_CAP`] servers
    /// mis-subscribed on the truncated list.
    ///
    /// SECURITY: the dialed address always comes with the pin the announcer
    /// named, so TLS verifies against exactly that certificate (no third party
    /// can interpose) and every dial still goes through this transport's
    /// `dialer` (Tor stays Tor). What it does NOT constrain is *which host* a
    /// peer points us at — inherent to contact-hosted queues (the same holds in
    /// SimpleX), which is why the map is bounded and an address without a valid
    /// pin is never dialed at all: it degrades to the primary.
    fn route(&self, server: &str) -> (SmpServer, ConnPool<SmpConn>) {
        let primary = || (self.servers[0].clone(), self.pools[0].clone());
        let raw = server.trim();
        if raw.is_empty() {
            return primary();
        }
        if let Some(i) = self.servers.iter().position(|s| s.render() == raw) {
            return (self.servers[i].clone(), self.pools[i].clone());
        }
        // not a literal match — parse (which enforces the pin) and try again on
        // the normalized address before opening a dynamic pool for it
        let Ok(parsed) = SmpServer::parse(raw) else {
            tracing::warn!(server = %raw, "queue names an unusable server — falling back to the primary");
            return primary();
        };
        let key = parsed.render();
        if let Some(i) = self.servers.iter().position(|s| s.render() == key) {
            return (self.servers[i].clone(), self.pools[i].clone());
        }
        let Ok(mut routed) = self.routed.lock() else {
            return primary();
        };
        let stamp = self.routed_clock.fetch_add(1, Ordering::Relaxed);
        if let Some(hit) = routed.get_mut(&key) {
            hit.2 = stamp;
            return (hit.0.clone(), hit.1.clone());
        }
        if routed.len() >= MAX_ROUTED_SERVERS {
            // evict the least recently used rather than degrading THIS route:
            // a member announcing ever-new hosts would otherwise fill the
            // table once and permanently mis-route every honest peer that
            // later moves to a new server
            if let Some(victim) = routed
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(k, _)| k.clone())
            {
                tracing::warn!(
                    evicted = %victim,
                    bound = MAX_ROUTED_SERVERS,
                    "dynamic server table full — evicting the least recently used"
                );
                routed.remove(&victim);
            }
        }
        let pool = ConnPool::new();
        routed.insert(key, (parsed.clone(), pool.clone(), stamp));
        (parsed, pool)
    }

    fn recv_queue(&self, id: &[u8]) -> Option<NewQueue> {
        self.state.lock().ok()?.recv.get(id).cloned()
    }

    /// The transport's sender seed (tests pin the export/import round-trip).
    #[cfg(test)]
    fn sender_seed(&self) -> Option<[u8; 32]> {
        self.state.lock().ok().and_then(|s| s.sender_seed)
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
///
/// **Additive V2**: `sender_seed` sits at the END so a pre-seed (V1) reader —
/// bincode v1 `deserialize` tolerates trailing bytes, pinned by
/// `a_v2_export_stays_readable_for_a_v1_reader` — still reads the blob, and
/// [`SmpTransport::adopt_creds`] falls back to the V1 layout for old blobs.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedCreds {
    recv: Vec<PersistedQueue>,
    send_keys: Vec<(Vec<u8>, [u8; 32])>,
    sender_seed: Option<[u8; 32]>,
}

/// The pre-seed (V1) creds layout, kept as a decode fallback so a
/// `transport.state` written before the sender-seed fix still opens.
#[derive(serde::Deserialize)]
struct PersistedCredsV1 {
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
        bincode::serialize(&PersistedCreds {
            recv,
            send_keys,
            sender_seed: s.sender_seed,
        })
        .ok()
    }

    /// Re-adopt a persisted credential set into this (fresh) transport.
    /// V2 blobs also carry the sender seed (adopted, replacing the fresh
    /// one); a V1 blob (no seed — its V2 decode fails at EOF) adopts
    /// recv + send_keys and keeps whatever seed the transport already has.
    fn adopt_creds(&self, bytes: &[u8]) {
        let (recv, send_keys, seed) = match bincode::deserialize::<PersistedCreds>(bytes) {
            Ok(c) => (c.recv, c.send_keys, c.sender_seed),
            Err(_) => match bincode::deserialize::<PersistedCredsV1>(bytes) {
                Ok(c) => (c.recv, c.send_keys, None),
                Err(_) => return,
            },
        };
        let Ok(mut s) = self.state.lock() else {
            return;
        };
        if let Some(seed) = seed {
            s.sender_seed = Some(seed);
        }
        for q in recv {
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
        for (id, k) in send_keys {
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
        // round-robin the new queue across the configured servers, so a leg's N
        // redundant inbound queues land on DIFFERENT servers (N=2 redundancy);
        // a single-server transport always picks index 0.
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.servers.len();
        let server = &self.servers[idx];
        let q = self.pools[idx]
            .with_conn(&self.dialer, server, false, |c: &mut SmpConn| {
                Box::pin(async move { c.new_queue(false).await })
            })
            .await?;
        let rcv = RcvQueue {
            server: server.render(),
            id: QueueId::from_bytes(q.recipient_id.clone()),
        };
        let snd = SndQueueAddr {
            server: server.render(),
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
        // route to the server this queue lives on (N=2: the redundant copies go
        // to each of the peer's inbound servers)
        let (server, pool) = self.route(&addr.server);
        pool.with_conn(&self.dialer, &server, true, |c: &mut SmpConn| {
                let sender_id = sender_id.clone();
                let state = state.clone();
                let block = block.clone();
                Box::pin(async move {
                    // ONE lock: re-read the securing key (so a reconnect
                    // retry after a broken connection never re-`SKEY`s a
                    // queue secured on an earlier attempt) and the seed.
                    let (cached, seed) = match state.lock() {
                        Ok(s) => (s.send_keys.get(&sender_id).cloned(), s.sender_seed),
                        Err(_) => (None, None),
                    };
                    // whether the derived key must still be proven by a
                    // successful SEND before it may be cached (D3 fallback)
                    let mut cache_on_success = false;
                    let key = match cached {
                        Some(k) => k,
                        None => {
                            // no cached key: derive this queue's sender key
                            // from the persisted seed (deterministic — a
                            // reopened transport re-derives the SAME key it
                            // secured the queue with) and (re-)SKEY with it
                            let seed = seed.ok_or_else(|| {
                                NetError::Crypto(
                                    "no sender seed (rng failed at transport creation)".into(),
                                )
                            })?;
                            let k = derive_sender_key(&seed, &sender_id);
                            match c.secure_as_sender(&sender_id, &k).await {
                                Ok(()) => {
                                    if let Ok(mut s) = state.lock() {
                                        s.send_keys.insert(sender_id.clone(), k.clone());
                                    }
                                }
                                Err(e) => {
                                    // the server may already hold exactly this
                                    // key (a re-SKEY after reopen, which some
                                    // servers reject) — the SEND verdict below
                                    // is authoritative, so try the signed send
                                    // anyway; a genuinely foreign key then
                                    // fails the send honestly (backoff →
                                    // degraded), and the key is cached ONLY
                                    // after a successful send
                                    tracing::warn!(
                                        error = %e,
                                        "SKEY rejected — attempting the signed SEND \
                                         with the derived sender key"
                                    );
                                    cache_on_success = true;
                                }
                            }
                            k
                        }
                    };
                    let sent = c.send_to(&sender_id, &key, block.as_slice()).await;
                    if sent.is_ok() && cache_on_success {
                        if let Ok(mut s) = state.lock() {
                            s.send_keys.insert(sender_id.clone(), key.clone());
                        }
                    }
                    sent
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
        // subscribe on the queue's OWN server (Stage 1 multi-server routing): a
        // resumed/redundant leg reaches the server it was created on, not a
        // single collapsed one.
        let (server, _) = self.route(&q.server);
        let mut conn = SmpConn::connect(&self.dialer, &server).await?;
        conn.sub(&queue.recipient_id, &queue.auth_sk).await?;
        let (tx, rx) = mpsc::channel::<Delivery>(64);
        let rcv_tag = crate::supervisor::queue_tag(&queue.recipient_id);
        tokio::spawn(async move {
            loop {
                match conn.recv_next(&queue).await {
                    Ok(body) => {
                        tracing::debug!(queue = %rcv_tag, len = body.len(), "SMP message received");
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
                        tracing::debug!(queue = %rcv_tag, error = %e, "SMP subscription ended");
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
        let (server, pool) = self.route(&q.server);
        pool.with_conn(&self.dialer, &server, false, |c: &mut SmpConn| {
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

    fn redundancy(&self) -> usize {
        // one inbound queue per configured server, capped (Track B Stage 2); a
        // single-server transport is N=1 (unchanged). `create_queue`'s
        // round-robin then spreads a leg's N queues one-per-server.
        self.servers.len().clamp(1, crate::MESH_REDUNDANCY_CAP)
    }
}

#[cfg(test)]
mod creds_tests {
    use super::*;

    const FP: &str = "f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=";

    fn transport() -> SmpTransport {
        SmpTransport::new(SmpServer::parse(&format!("smp://{FP}@example.invalid")).expect("server"))
    }

    /// Stage-2 audit finding #3: an EMPTY server list must not panic
    /// (divide-by-zero in `create_queue`, index panic in `route`). The
    /// constructor substitutes a fail-closed placeholder, so the transport is
    /// always non-empty and `route`/`redundancy` are safe.
    #[test]
    fn an_empty_server_list_does_not_panic() {
        let t = SmpTransport::new_multi(Vec::new());
        assert_eq!(t.redundancy(), 1, "empty → a single (placeholder) server");
        // route on any string must not panic (index 0 exists)
        let _ = t.route("");
        let _ = t.route("smp://whatever@host");
    }

    /// Stage 2 redundancy: the per-leg inbound-queue count tracks the configured
    /// server count, capped at [`crate::MESH_REDUNDANCY_CAP`]. Single-server is
    /// N=1 (unchanged); the mint sites read this via `Transport::redundancy`.
    #[test]
    fn redundancy_tracks_the_server_count_capped() {
        const FP2: &str = "0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=";
        let s1 = SmpServer::parse(&format!("smp://{FP}@host-one.invalid")).expect("s1");
        let s2 = SmpServer::parse(&format!("smp://{FP2}@host-two.invalid")).expect("s2");
        assert_eq!(SmpTransport::new(s1.clone()).redundancy(), 1, "single server → N=1");
        assert_eq!(
            SmpTransport::new_multi(vec![s1.clone(), s2.clone()]).redundancy(),
            2,
            "two servers → N=2"
        );
        let many = vec![s1.clone(), s2.clone(), s1.clone(), s2];
        assert_eq!(
            SmpTransport::new_multi(many).redundancy(),
            crate::MESH_REDUNDANCY_CAP,
            "more servers than the cap → capped"
        );
    }

    /// Stage 1 multi-server routing: a queue names which of the transport's
    /// servers it lives on, and `route` dispatches there instead of collapsing
    /// every leg to one. An empty server (loopback / pre-Stage-0 link) is the
    /// primary.
    #[test]
    fn route_dispatches_by_the_queue_s_own_server() {
        const FP2: &str = "0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=";
        let s1 = SmpServer::parse(&format!("smp://{FP}@host-one.invalid")).expect("s1");
        let s2 = SmpServer::parse(&format!("smp://{FP2}@host-two.invalid")).expect("s2");
        let t = SmpTransport::new_multi(vec![s1.clone(), s2.clone()]);
        assert_eq!(t.route("").0.render(), s1.render(), "empty → first");
        assert_eq!(t.route(&s2.render()).0.render(), s2.render(), "names s2 → routes to s2");
        assert_eq!(t.route(&s1.render()).0.render(), s1.render(), "names s1 → routes to s1");
        // whitespace / an equivalent spelling still matches the CONFIGURED server
        // (not a second, dynamically routed copy of it)
        assert_eq!(
            t.route(&format!("  {}  ", s2.render())).0.render(),
            s2.render(),
            "a padded spelling matches the configured server"
        );
    }

    /// **A queue on a server we did not configure is dialed at ITS server.**
    /// Falling back to the primary (the Stage-1 behaviour) mis-routed every such
    /// queue: it made N=2 require a shared server set across all members, and it
    /// broke a resumed mesh whose servers exceed `MESH_REDUNDANCY_CAP` (the
    /// reopen list is truncated). The address carries the server's 32-byte pin,
    /// so the dial is verified against exactly what the peer named.
    #[test]
    fn route_dials_an_unconfigured_but_pinned_server() {
        const FP2: &str = "0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=";
        let s1 = SmpServer::parse(&format!("smp://{FP}@host-one.invalid")).expect("s1");
        let peer = SmpServer::parse(&format!("smp://{FP2}@a-peers-server.invalid")).expect("peer");
        let t = SmpTransport::new(s1.clone());
        assert_eq!(
            t.route(&peer.render()).0.render(),
            peer.render(),
            "a pinned server we never configured is routed to ITSELF, not the primary"
        );
        // one pooled connection per dynamic server, shared by every route() and
        // by transport clones — not a fresh dial per send
        let a = t.route(&peer.render()).1;
        let b = t.clone().route(&peer.render()).1;
        assert!(
            Arc::ptr_eq(&a.slot, &b.slot),
            "the dynamic server keeps ONE shared connection pool"
        );
        // an unparseable / unpinned address is not dialed at all — it falls back
        // to the primary exactly as before (fail-soft, never an unpinned dial)
        assert_eq!(t.route("not-a-server").0.render(), s1.render(), "garbage → primary");
        assert_eq!(
            t.route("smp://short@host.invalid").0.render(),
            s1.render(),
            "a fingerprint that is not a 32-byte SHA-256 → primary, never dialed"
        );
    }

    /// The dynamic pool table is bounded AND self-cleaning: a member cannot
    /// make us hold an unbounded number of server connections by naming
    /// ever-new hosts, and — the security half — filling it must not push an
    /// honest peer's server onto our primary, where its queue does not exist.
    /// The least recently used entry is evicted instead.
    #[test]
    fn dynamic_routing_is_bounded_and_evicts_the_least_recently_used() {
        let s1 = SmpServer::parse(&format!("smp://{FP}@host-one.invalid")).expect("s1");
        let t = SmpTransport::new(s1.clone());
        let flood: Vec<String> = (0..MAX_ROUTED_SERVERS)
            .map(|i| format!("smp://{FP}@host-{i}.invalid"))
            .collect();
        for (i, s) in flood.iter().enumerate() {
            assert_eq!(&t.route(s).0.render(), s, "server {i} is within the bound");
        }
        // keep the FIRST one hot, then overflow the table
        let _ = t.route(&flood[0]);
        let honest = format!("smp://{FP}@an-honest-peer.invalid");
        assert_eq!(
            t.route(&honest).0.render(),
            honest,
            "a new server is still routed to ITSELF when the table is full"
        );
        assert_eq!(
            t.route(&flood[0]).0.render(),
            flood[0],
            "the recently used entry survived the eviction"
        );
        assert!(
            t.routed.lock().expect("lock").len() <= MAX_ROUTED_SERVERS,
            "the table stays bounded"
        );
    }

    /// D2: the sender key is a pure function of (seed, queue id) — the same
    /// seed re-derives the same key after a restart; a different queue or a
    /// different seed derives a different key.
    #[test]
    fn sender_key_derivation_is_deterministic_and_queue_bound() {
        let seed = [7u8; 32];
        let a = derive_sender_key(&seed, b"queue-a");
        let again = derive_sender_key(&seed, b"queue-a");
        let other_queue = derive_sender_key(&seed, b"queue-b");
        let other_seed = derive_sender_key(&[8u8; 32], b"queue-a");
        assert_eq!(a.to_bytes(), again.to_bytes(), "same seed + id → same key");
        assert_ne!(a.to_bytes(), other_queue.to_bytes(), "queue-bound");
        assert_ne!(a.to_bytes(), other_seed.to_bytes(), "seed-bound");
    }

    /// D4/D5: export → import carries the sender seed, so a reopened
    /// transport derives the SAME sender key for the same queue — even when
    /// the export happened BEFORE any send (the mesh-up persist moment that
    /// broke the 2026-07-19 incident nodes).
    #[test]
    fn creds_v2_round_trips_the_sender_seed() {
        let t1 = transport();
        let bytes = t1.export_creds().expect("export");
        let t2 = transport();
        assert_ne!(
            t1.sender_seed().expect("t1 seed"),
            t2.sender_seed().expect("t2 fresh seed"),
            "fresh transports mint distinct seeds"
        );
        t2.import_creds(&bytes);
        let s1 = t1.sender_seed().expect("t1 seed");
        let s2 = t2.sender_seed().expect("t2 adopted seed");
        assert_eq!(s1, s2, "the import adopts the exported seed");
        assert_eq!(
            derive_sender_key(&s1, b"some-queue").to_bytes(),
            derive_sender_key(&s2, b"some-queue").to_bytes(),
            "both incarnations derive the same per-queue sender key"
        );
    }

    /// A byte-exact mirror of the PRE-seed creds layout (recv + send_keys
    /// only) — what every deployed reader before this fix wrote and read.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct V1Creds {
        recv: Vec<V1Queue>,
        send_keys: Vec<(Vec<u8>, [u8; 32])>,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    struct V1Queue {
        recipient_id: Vec<u8>,
        sender_id: Vec<u8>,
        auth_sk: [u8; 32],
        dh_secret: [u8; 32],
        server_dh: [u8; 32],
    }

    fn v1_blob() -> Vec<u8> {
        bincode::serialize(&V1Creds {
            recv: vec![V1Queue {
                recipient_id: vec![1, 2],
                sender_id: vec![3, 4],
                auth_sk: [5u8; 32],
                dh_secret: [6u8; 32],
                server_dh: [7u8; 32],
            }],
            send_keys: vec![(vec![3, 4], [9u8; 32])],
        })
        .expect("v1 blob")
    }

    /// D4: a pre-fix `transport.state` (V1, no seed) still imports — recv +
    /// send_keys are adopted and the transport KEEPS its own fresh seed.
    #[test]
    fn import_falls_back_to_the_v1_creds_format() {
        let t = transport();
        let own = t.sender_seed().expect("fresh seed");
        t.import_creds(&v1_blob());
        assert_eq!(
            t.sender_seed().expect("seed kept"),
            own,
            "a V1 import must never discard the transport's seed"
        );
        assert!(t.recv_queue(&[1, 2]).is_some(), "V1 recv cred adopted");
        let re = t.export_creds().expect("re-export");
        let creds: PersistedCreds = bincode::deserialize(&re).expect("decode own export");
        assert_eq!(creds.send_keys, vec![(vec![3, 4], [9u8; 32])], "V1 send key adopted");
    }

    /// D4's load-bearing assumption, pinned: bincode v1 `deserialize`
    /// tolerates trailing bytes, so an OLD (V1) reader still reads a V2
    /// export — recv + send_keys land, the trailing seed is ignored.
    #[test]
    fn a_v2_export_stays_readable_for_a_v1_reader() {
        let t = transport();
        t.import_creds(&v1_blob()); // give it a queue + a send key to carry
        let v2 = t.export_creds().expect("export");
        let v1: V1Creds =
            bincode::deserialize(&v2).expect("a V1 reader must still decode a V2 blob");
        assert_eq!(v1.recv.len(), 1);
        assert_eq!(v1.recv[0].recipient_id, vec![1, 2]);
        assert_eq!(v1.send_keys, vec![(vec![3, 4], [9u8; 32])]);
    }

    /// D5: importing a V2 blob that carries a seed OVERWRITES the fresh one
    /// (the reopen path must re-derive the previous incarnation's keys), and
    /// a later V1 import cannot roll that adoption back.
    #[test]
    fn a_v1_import_after_a_v2_import_keeps_the_adopted_seed() {
        let t1 = transport();
        let t2 = transport();
        t2.import_creds(&t1.export_creds().expect("v2 export"));
        let adopted = t2.sender_seed().expect("adopted");
        assert_eq!(adopted, t1.sender_seed().expect("t1 seed"));
        t2.import_creds(&v1_blob());
        assert_eq!(
            t2.sender_seed().expect("still adopted"),
            adopted,
            "a V1 import (no seed) must not clobber an adopted seed"
        );
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
