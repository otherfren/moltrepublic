// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-net`: the transport layer of MoltRepublic.
//!
//! Implements `documents/concept-transport-simplex-tor.md`, milestone **T1**:
//! everything above the [`Transport`] trait runs without sockets, everything
//! below runs without the engine. The trait models SMP-style unidirectional
//! message queues: the *recipient* creates a queue and hands the send-side
//! address to exactly one sender; blocks are uniform-size, at-least-once,
//! acked by the receiver.
//!
//! Layering (the crate sits **beside** the engine, never above it):
//!
//! * [`block`] — the uniform 16 KiB SMP block and the payload-budget math.
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
//!
//! Later milestones add `SmpTransport` (T3) and Tor dialing (T4/T5) behind
//! the same trait.

pub mod block;
pub mod chunk;
pub mod invite;
pub mod loopback;
pub mod mesh;
pub mod mls;
pub mod s3;
pub mod smp;
pub mod socks5;
pub mod supervisor;
pub mod transfer;
/// Embedded in-process Tor via arti — only built with `--features embedded-tor`
/// (the default build never pulls arti). See the module docs and the
/// `Cargo.toml` `[features]` note.
#[cfg(feature = "embedded-tor")]
pub mod tor_embedded;
pub mod wrap;

use std::future::Future;

pub use block::{PaddedBlock, PADDED_BLOCK_LEN, SMP_BLOCK_LEN, SMP_FRAMING_RESERVE};
pub use chunk::{msg_id, MsgId, Reassembler, CHUNK_PAYLOAD_BUDGET};
pub use invite::{
    join_mac, mint_ticket, verify_join_mac, JoinRequest, ReplyHandover, RitualMsg, SealSigned,
};
pub use mls::{MlsError, MlsIncoming, MlsMember};
pub use supervisor::send_framed;
pub use loopback::{ChaosPolicy, LoopbackHub, LoopbackTransport};
pub use supervisor::{
    EngineSink, MemLog, MemStateStore, MlsChannel, NetConfig, OutboxLog, PeerLink, StateStore,
    SupervisorHandle,
};
pub use wrap::{WrapKey, CHUNK_PLAIN_LEN};

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
/// Variable length: the loopback hub mints 16 bytes, real SMP servers
/// assign 16–24.
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
    /// The queue id on its server.
    pub id: QueueId,
}

/// The send side of a queue (handed to exactly one sender).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SndQueueAddr {
    /// The server holding the queue (`smp://fingerprint@host` later; the
    /// loopback hub ignores it).
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
/// supervisor, UI, E2E tests — runs against this trait; implementations are
/// [`loopback::LoopbackTransport`] today, `SmpTransport` (T3) and
/// `NymTransport` later.
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

    /// Serialize this transport's queue **credentials** — the recipient keys of
    /// queues we created (to re-`subscribe`) and the sender keys we secured peer
    /// queues with (to keep sending without a rejected re-`SKEY`) — so a reopened
    /// node re-adopts the SAME queues. `None` for transports whose queues carry
    /// no persistable credential (the loopback hub, whose queues live only in the
    /// in-memory shared hub and cannot outlive the process).
    fn export_creds(&self) -> Option<Vec<u8>> {
        None
    }

    /// Re-adopt credentials produced by [`Self::export_creds`] into a fresh
    /// transport on reopen. A no-op for transports without persistable creds.
    fn import_creds(&self, _creds: &[u8]) {}
}
