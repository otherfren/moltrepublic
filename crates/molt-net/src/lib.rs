// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-net`: the transport layer of MoltRepublic — MLS-protected payloads
//! over queue transports (loopback today, Nostr relays in build).
//!
//! Everything above the [`Transport`] trait runs without sockets, everything
//! below runs without the engine. The trait models unidirectional message
//! queues: the *recipient* creates a queue and hands the send-side address to
//! exactly one sender; blocks are uniform-size, at-least-once, acked by the
//! receiver.
//!
//! Layering (the crate sits **beside** the engine, never above it):
//!
//! * [`block`] — the uniform 16 KiB block and the payload-budget math.
//!   Nothing in this crate assumes "payload == 16 KiB"; every size derives
//!   from named constants.
//! * [`wrap`] — mandatory per-queue wrapping (XChaCha20-Poly1305, fresh key
//!   per queue). Its purpose is **copy unlinkability**, not confidentiality:
//!   the n−1 fan-out copies of one group message must be pairwise
//!   byte-distinct, or a server hosting two members' queues links them.
//! * [`chunk`] — chunking larger messages into blocks and reassembling them,
//!   deduplicating by `(message id, chunk index)`.
//! * [`loopback`] — the in-process [`Transport`]: replaces the old reply
//!   simulator (simulated members become loopback peers driving real code
//!   paths) and carries the chaos harness for the test tiers.
//! * [`supervisor`] — the per-node transport runtime: a log-backed outbox
//!   (per-member delivery cursors, jittered fan-out, retry with backoff)
//!   and per-queue receive tasks (unwrap, reassemble, per-sender in-order
//!   delivery into the engine). The engine never awaits the transport: it
//!   appends to its log and *nudges* the supervisor.
//! * [`dial`] — the fail-closed T4 dialer (direct / SOCKS5h-Tor / embedded
//!   arti), shared by the S3 backup client and N2's WebSocket relay
//!   connections.
//! * [`relay_ws`] / [`relay_runtime`] — the N2 Nostr relay runtime: one
//!   typed WebSocket connection, and the pool over it (publish ≥1-OK,
//!   pooled subscription with dedup, per-relay cursors, health).
//! * [`envelope`] — the N3 OUTER layer of a kind-445 event: sealing under
//!   the epoch's exporter secret, opening across the bounded ring.

pub mod block;
pub mod chunk;
pub mod dial;
pub mod envelope;
pub mod invite;
pub mod kinds;
pub mod loopback;
pub mod mesh;
pub mod mls;
pub mod nostr;
pub mod relay_runtime;
pub mod relay_ws;
pub mod ritual_net;
pub mod ritual_wrap;
pub mod s3;
pub mod supervisor;
pub mod tor_probe;
pub mod transfer;
pub mod welcome;
/// Embedded in-process Tor via arti — only built with `--features embedded-tor`
/// (the default build never pulls arti). See the module docs and the
/// `Cargo.toml` `[features]` note.
#[cfg(feature = "embedded-tor")]
pub mod tor_embedded;
pub mod wrap;

use std::future::Future;

pub use block::{PaddedBlock, PADDED_BLOCK_LEN};
pub use chunk::{msg_id, msg_id_epoch, MsgId, Reassembler, CHUNK_PAYLOAD_BUDGET};
pub use invite::{
    join_mac, mint_ticket, verify_join_mac, JoinRequest, ReplyHandover, RitualMsg, SealSigned,
};
pub use mls::{MlsError, MlsIncoming, MlsMember};
pub use self::nostr::{canonical_nostr_pk, nostr_identity, nostr_pk_for_sk};
pub use supervisor::send_framed;
pub use loopback::{ChaosPolicy, LoopbackHub, LoopbackTransport};
pub use supervisor::{
    EngineSink, MemLog, MemStateStore, MlsChannel, NetConfig, OutboxLog, PeerLink, StateStore,
    SupervisorHandle,
};
pub use wrap::{WrapKey, CHUNK_PLAIN_LEN};

/// The transport-level **delivery ACK** tag (delivery guarantee §4.3). A
/// NUL-prefixed control frame carried as an MLS application ciphertext —
/// never a `WorkspaceEvent`, never logged or chained. The frame plaintext is
/// this tag followed by the JSON of a [`molt_core::AcceptedWindow`]: "what I
/// (the sender of this frame) have engine-accepted OF YOURS". The receiving
/// supervisor advances its `acked_floor` for that peer against its OWN log
/// and trims/rewinds resends with it; an ack also stamps the sender's
/// presence (authenticated traffic). The leading NUL keeps it from ever
/// colliding with a JSON-encoded `EventEnvelope`; the whole `\x00molt-mesh-*`
/// space is reserved for control frames, and an unknown one decodes to a
/// dropped no-op. An old node drops it as an unknown control tag — harmless,
/// the guarantee simply stays inactive toward it (§4.8).
pub const MESH_ACK_TAG: &[u8] = b"\x00molt-mesh-ack-v1";

/// Everything that can go wrong between a queue and the engine.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The peer's server cannot be reached right now (retryable — the
    /// outbox backs off and retries until acked).
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The addressed queue does not exist (deleted, rotated away, or never
    /// created on this server).
    #[error("unknown queue")]
    UnknownQueue,
    /// The transport (or its hub) is shut down.
    #[error("transport closed")]
    Closed,
    /// A framing/size invariant was violated (always a local bug, never a
    /// remote condition — sizes derive from named constants).
    #[error("framing: {0}")]
    Framing(String),
    /// A cryptographic operation failed (wrap/unwrap, RNG).
    #[error("crypto: {0}")]
    Crypto(String),
    /// Tor is selected but the circuit could not be reached right now (proxy
    /// down, circuit build/handshake timed out). A transient runtime state —
    /// surfaced as the amber/red transport-health pill, retryable.
    #[error("tor unavailable: {0}")]
    TorUnavailable(String),
    /// Tor is selected but misconfigured so no dial is even attempted
    /// (embedded build missing, unknown mode, `nym` not implemented). A
    /// fail-closed config error, never a silent clearnet fallback.
    #[error("tor misconfigured: {0}")]
    TorMisconfigured(String),
}

/// A queue id: random, meaningless bytes — no accounts, no user identifiers.
/// Sender and recipient of one queue are never linkable to other queues.
/// Variable length: the loopback hub mints 16 bytes; a real server may
/// assign its own length.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueId(pub Vec<u8>);

impl QueueId {
    /// A fresh 16-byte random id from the OS CSPRNG (loopback).
    pub fn fresh() -> Result<QueueId, NetError> {
        let mut b = [0u8; 16];
        getrandom::getrandom(&mut b)
            .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
        Ok(QueueId(b.to_vec()))
    }

    /// Wrap a server-assigned id.
    pub fn from_bytes(b: Vec<u8>) -> QueueId {
        QueueId(b)
    }
}

impl std::fmt::Display for QueueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(&self.0))
    }
}

/// The receive side of a queue (held by its creator, the recipient).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcvQueue {
    /// The server/relay holding the queue (a URL later; empty or `loopback`
    /// for the in-process hub). Persisted so a resumed leg subscribes on the
    /// RIGHT server instead of collapsing to a single one.
    pub server: String,
    /// The queue id on that server.
    pub id: QueueId,
}

/// The send side of a queue (handed to exactly one sender).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SndQueueAddr {
    /// The server/relay holding the queue (a URL later; the loopback hub
    /// ignores it).
    pub server: String,
    /// The queue id on that server.
    pub id: QueueId,
}

/// What [`Transport::create_queue`] returns: both sides of a fresh queue.
#[derive(Debug, Clone)]
pub struct QueuePair {
    /// The receive side (kept by the creator).
    pub rcv: RcvQueue,
    /// The send side (handed to the sender, in-band).
    pub snd: SndQueueAddr,
}

/// The receiver's acknowledgement handle for one delivered block. Ack only
/// once the block's content is safely applied — an unacked block is
/// redelivered (at-least-once; the reassembler and the per-sender cursors
/// absorb the duplicates).
pub struct AckToken(Option<Box<dyn FnOnce() + Send>>);

impl AckToken {
    /// Wrap the transport-specific acknowledgement action.
    pub fn new(f: impl FnOnce() + Send + 'static) -> AckToken {
        AckToken(Some(Box::new(f)))
    }

    /// An ack that does nothing (tests, already-acked redeliveries).
    pub fn noop() -> AckToken {
        AckToken(None)
    }

    /// Acknowledge the block.
    pub fn ack(mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl std::fmt::Debug for AckToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AckToken")
            .field("armed", &self.0.is_some())
            .finish()
    }
}

/// One block arriving on a subscribed queue.
#[derive(Debug)]
pub struct Delivery {
    /// The uniform-size block.
    pub block: PaddedBlock,
    /// Ack handle; unacked deliveries are redelivered.
    pub ack: AckToken,
}

/// The transport abstraction (concept §2). Everything above it — engine,
/// supervisor, UI, E2E tests — runs against this trait; the implementation is
/// [`loopback::LoopbackTransport`] today, with the Nostr relay transport to
/// follow (N-plan).
pub trait Transport: Send + Sync + Clone + 'static {
    /// Create a fresh queue on this transport (recipient side).
    fn create_queue(&self) -> impl Future<Output = Result<QueuePair, NetError>> + Send;

    /// Send one uniform block to a queue's send-side address.
    fn send(
        &self,
        addr: &SndQueueAddr,
        block: PaddedBlock,
    ) -> impl Future<Output = Result<(), NetError>> + Send;

    /// Subscribe to a queue's deliveries (one long-lived SUB per queue).
    /// Pending (unacked) blocks are (re)delivered to a fresh subscriber.
    fn subscribe(
        &self,
        q: &RcvQueue,
    ) -> impl Future<Output = Result<tokio::sync::mpsc::Receiver<Delivery>, NetError>> + Send;

    /// Retire a queue (rotation / teardown).
    fn delete_queue(&self, q: &RcvQueue) -> impl Future<Output = Result<(), NetError>> + Send;

    /// Serialize this transport's queue **credentials** — whatever a reopened
    /// node needs to re-adopt the SAME queues (re-`subscribe` what it created,
    /// keep sending where it already sent) — so a resume never mints a stray
    /// second identity on the server. `None` when this endpoint created no
    /// queues yet. The loopback transport exports its created queue ids (its
    /// analogue of receive credentials) — valid only while the in-process hub
    /// lives, so a NEW process still cannot resume a loopback mesh.
    fn export_creds(&self) -> Option<Vec<u8>> {
        None
    }

    /// Re-adopt credentials produced by [`Self::export_creds`] into a fresh
    /// transport on reopen. A no-op for transports without persistable creds.
    fn import_creds(&self, _creds: &[u8]) {}
}
