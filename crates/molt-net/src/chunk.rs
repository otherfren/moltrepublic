// SPDX-License-Identifier: GPL-3.0-or-later

//! Chunking and reassembly (concept §3.2/§3.4).
//!
//! Messages larger than one block are split into chunks with a tiny
//! reassembly header *inside* the encrypted payload. The chunker computes
//! its budget from the named constants — the header layout is
//! `msg id (16) | index u16le | count u16le | payload len u16le`, the rest
//! of the chunk plaintext is the payload slice plus zero padding (padding
//! sits inside the wrap, so the server only ever sees uniform blocks).
//!
//! The reassembler is the transport-layer half of the two-layer dedup:
//! retries redeliver individual *chunks*, so it dedups by
//! `(message id, chunk index)`; the per-sender cursors in the supervisor
//! are the second layer (whole-message replays). Server input is
//! untrusted — every header field is bounds-checked.

use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

use crate::wrap::CHUNK_PLAIN_LEN;
use crate::NetError;

/// Message-id length inside the chunk header.
pub const MSG_ID_LEN: usize = 16;
/// The chunk header: msg id + index + count + payload length.
pub const CHUNK_HEADER_LEN: usize = MSG_ID_LEN + 2 + 2 + 2;
/// Usable payload bytes per chunk.
pub const CHUNK_PAYLOAD_BUDGET: usize = CHUNK_PLAIN_LEN - CHUNK_HEADER_LEN;

/// Partially reassembled messages kept at once; beyond this the oldest
/// partial is evicted (its chunks redeliver later — bounded memory beats
/// completeness against a hostile server).
const MAX_PARTIALS: usize = 64;
/// Recently completed message ids remembered so late duplicate chunks are
/// recognized (and acked) instead of reopening a partial forever.
const RECENT_COMPLETED: usize = 256;

/// A message id: 16 bytes, deterministic per logical message so that a
/// retried send dedups against the first attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MsgId(pub [u8; MSG_ID_LEN]);

/// Derive the message id for one wire message on one link:
/// `sha256("molt-msg" ‖ sender ‖ 0 ‖ recipient ‖ wire seq)[..16]`.
/// Deterministic on purpose — a resend after a crash reuses the id, so the
/// receiver's dedup absorbs it.
pub fn msg_id(sender: &str, recipient: &str, wire_seq: u64) -> MsgId {
    msg_id_epoch(sender, recipient, wire_seq, 0)
}

/// [`msg_id`] salted with the link's **resend epoch** (delivery guarantee
/// §4.5): epoch 0 is byte-identical to the unsalted id (compat — first
/// sends and old nodes), while every rewind bumps the epoch so a RESEND
/// carries a fresh id. Without the salt, the receiver's completed-id ring
/// would classify the resend as a duplicate of the original attempt and
/// ack it away UNREAD — even when that original was discarded undecrypted
/// (the V4 loss path). Same-attempt fan-out copies still share one id, so
/// copy dedup keeps working.
pub fn msg_id_epoch(sender: &str, recipient: &str, wire_seq: u64, resend_epoch: u32) -> MsgId {
    let mut h = Sha256::new();
    h.update(b"molt-msg");
    h.update(sender.as_bytes());
    h.update([0u8]);
    h.update(recipient.as_bytes());
    h.update(wire_seq.to_le_bytes());
    if resend_epoch > 0 {
        h.update(b"resend");
        h.update(resend_epoch.to_le_bytes());
    }
    let d = h.finalize();
    let mut id = [0u8; MSG_ID_LEN];
    id.copy_from_slice(&d[..MSG_ID_LEN]);
    MsgId(id)
}

/// Split one message into chunk plaintexts (each exactly
/// [`CHUNK_PLAIN_LEN`], zero-padded after the payload). An empty message
/// still yields one chunk — a message always has at least one block.
pub fn chunk_message(id: MsgId, payload: &[u8]) -> Result<Vec<Vec<u8>>, NetError> {
    chunk_message_sized(id, payload, CHUNK_PLAIN_LEN)
}

/// [`chunk_message`] at a caller-chosen uniform chunk size (the file plane
/// sizes its chunks to the RELAY publish budget, not to the SMP block).
/// Same header layout, same padding-to-size (every chunk of a series is
/// one size — the relay sees uniform blocks), same reassembler.
pub fn chunk_message_sized(
    id: MsgId,
    payload: &[u8],
    plain_len: usize,
) -> Result<Vec<Vec<u8>>, NetError> {
    let Some(budget) = plain_len.checked_sub(CHUNK_HEADER_LEN).filter(|b| *b > 0) else {
        return Err(NetError::Framing(format!(
            "chunk size {plain_len} leaves no payload room (header {CHUNK_HEADER_LEN})"
        )));
    };
    chunk_message_budgeted(id, payload, budget, plain_len)
}

fn chunk_message_budgeted(
    id: MsgId,
    payload: &[u8],
    budget: usize,
    plain_len: usize,
) -> Result<Vec<Vec<u8>>, NetError> {
    let count = payload.len().div_ceil(budget).max(1);
    let count_u16 = u16::try_from(count).map_err(|_| {
        NetError::Framing(format!(
            "message of {} bytes needs {count} chunks (max {})",
            payload.len(),
            u16::MAX
        ))
    })?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        // `count` is `max(1)`, so an empty payload still yields one
        // (empty) chunk — a message always has at least one block
        let start = index * budget;
        let end = payload.len().min(start + budget);
        let part = &payload[start.min(payload.len())..end];
        let index_u16 = u16::try_from(index)
            .map_err(|_| NetError::Framing("chunk index overflow".to_string()))?;
        let len_u16 = u16::try_from(part.len())
            .map_err(|_| NetError::Framing("chunk payload overflow".to_string()))?;
        let mut chunk = Vec::with_capacity(plain_len);
        chunk.extend_from_slice(&id.0);
        chunk.extend_from_slice(&index_u16.to_le_bytes());
        chunk.extend_from_slice(&count_u16.to_le_bytes());
        chunk.extend_from_slice(&len_u16.to_le_bytes());
        chunk.extend_from_slice(part);
        chunk.resize(plain_len, 0);
        out.push(chunk);
    }
    Ok(out)
}

/// What [`Reassembler::push`] concluded about one chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// The chunk completed its message — here it is.
    Complete(MsgId, Vec<u8>),
    /// The chunk was stored; its message is still missing pieces.
    Buffered(MsgId),
    /// The chunk was already seen (or its message already completed) —
    /// safe to ack immediately.
    Duplicate(MsgId),
}

/// One partially received message. `chunks` is a SPARSE map keyed by chunk
/// index — its memory grows with chunks actually received, never with the
/// attacker-controlled `count` in the header (a hostile count=65535 with
/// one real chunk must not pin ~1.5 MB of empty slots).
struct Partial {
    count: u16,
    chunks: std::collections::BTreeMap<u16, Vec<u8>>,
}

/// Reassembles chunks into messages, deduplicating by
/// `(message id, chunk index)`. Bounded: at most [`MAX_PARTIALS`] open
/// messages, the oldest evicted first.
pub struct Reassembler {
    /// The uniform chunk plaintext size this instance accepts (hard bound).
    plain_len: usize,
    partial: HashMap<MsgId, Partial>,
    /// Insertion order of `partial`, for eviction.
    order: VecDeque<MsgId>,
    /// Ring of recently completed ids (with membership set semantics).
    completed: VecDeque<MsgId>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Reassembler::new()
    }
}

impl Reassembler {
    /// An empty reassembler.
    pub fn new() -> Reassembler {
        Reassembler::new_sized(CHUNK_PLAIN_LEN)
    }

    /// A reassembler for a context whose uniform chunk size is not the SMP
    /// block — the file plane sizes chunks to the relay publish budget.
    /// The size stays a hard per-instance bound (hostile input).
    pub fn new_sized(plain_len: usize) -> Reassembler {
        Reassembler {
            plain_len,
            partial: HashMap::new(),
            order: VecDeque::new(),
            completed: VecDeque::new(),
        }
    }

    /// Feed one chunk plaintext (untrusted — header fields are validated).
    pub fn push(&mut self, chunk: &[u8]) -> Result<PushOutcome, NetError> {
        if chunk.len() != self.plain_len {
            return Err(NetError::Framing(format!(
                "chunk must be {} bytes, got {}",
                self.plain_len,
                chunk.len()
            )));
        }
        let mut id = [0u8; MSG_ID_LEN];
        id.copy_from_slice(&chunk[..MSG_ID_LEN]);
        let id = MsgId(id);
        let word = |at: usize| u16::from_le_bytes([chunk[at], chunk[at + 1]]);
        let index = word(MSG_ID_LEN);
        let count = word(MSG_ID_LEN + 2);
        let len = word(MSG_ID_LEN + 4);
        if count == 0
            || index >= count
            || usize::from(len) > self.plain_len.saturating_sub(CHUNK_HEADER_LEN)
        {
            return Err(NetError::Framing(format!(
                "chunk header out of bounds (index {index}, count {count}, len {len})"
            )));
        }
        if self.completed.contains(&id) {
            return Ok(PushOutcome::Duplicate(id));
        }
        let payload = chunk[CHUNK_HEADER_LEN..CHUNK_HEADER_LEN + usize::from(len)].to_vec();

        let partial = match self.partial.get_mut(&id) {
            Some(p) => {
                if p.count != count {
                    return Err(NetError::Framing(format!(
                        "chunk count changed mid-message ({} vs {count})",
                        p.count
                    )));
                }
                p
            }
            None => {
                if self.partial.len() >= MAX_PARTIALS {
                    if let Some(oldest) = self.order.pop_front() {
                        self.partial.remove(&oldest);
                        tracing::warn!(msg = %hex::encode(oldest.0), "reassembler full — evicted the oldest partial (its chunks will redeliver)");
                    }
                }
                self.order.push_back(id);
                self.partial.entry(id).or_insert(Partial {
                    count,
                    chunks: std::collections::BTreeMap::new(),
                })
            }
        };
        if partial.chunks.contains_key(&index) {
            return Ok(PushOutcome::Duplicate(id));
        }
        partial.chunks.insert(index, payload);
        if partial.chunks.len() < usize::from(partial.count) {
            return Ok(PushOutcome::Buffered(id));
        }

        // complete: concatenate in index order
        let Some(done) = self.partial.remove(&id) else {
            // the entry was obtained via get_mut/entry above — a miss here
            // is a broken invariant and must fail loudly, not fabricate an
            // empty message
            return Err(NetError::Framing(
                "reassembler lost a partial mid-completion".to_string(),
            ));
        };
        self.order.retain(|x| *x != id);
        self.completed.push_back(id);
        if self.completed.len() > RECENT_COMPLETED {
            self.completed.pop_front();
        }
        let mut msg = Vec::new();
        for (_, c) in done.chunks {
            msg.extend_from_slice(&c);
        }
        Ok(PushOutcome::Complete(id, msg))
    }

    /// Un-remember a completed message: the receiver dropped it
    /// *unprocessed* (e.g. reorder buffer full), so a redelivery of its
    /// chunks must reassemble it again instead of being acked away as a
    /// duplicate — without this, a dropped message could never be
    /// recovered and its wire seq would wedge the link.
    pub fn forget(&mut self, id: MsgId) {
        self.completed.retain(|x| *x != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(payload: &[u8]) -> Vec<u8> {
        let id = msg_id("a", "b", 1);
        let chunks = chunk_message(id, payload).expect("chunk");
        let mut r = Reassembler::new();
        let mut out = None;
        for c in chunks {
            if let PushOutcome::Complete(_, m) = r.push(&c).expect("push") {
                out = Some(m);
            }
        }
        out.expect("message completes")
    }

    #[test]
    fn chunker_roundtrips_empty_small_and_multiblock() {
        for len in [0usize, 1, 100, CHUNK_PAYLOAD_BUDGET, CHUNK_PAYLOAD_BUDGET + 1, 3 * CHUNK_PAYLOAD_BUDGET + 17] {
            let payload: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            assert_eq!(roundtrip(&payload), payload, "len {len}");
        }
    }

    #[test]
    fn every_chunk_is_exactly_one_plaintext_size() {
        let id = msg_id("a", "b", 2);
        for len in [0usize, 1, 2 * CHUNK_PAYLOAD_BUDGET + 5] {
            for c in chunk_message(id, &vec![9u8; len]).expect("chunk") {
                assert_eq!(c.len(), CHUNK_PLAIN_LEN);
            }
        }
    }

    /// The file plane (F2) sizes its chunks to the RELAY publish budget, not
    /// to the SMP block: the sized chunker keeps the header layout and the
    /// uniform-block property (every chunk of a series is one size), and
    /// the reassembler takes the sized chunks unchanged.
    #[test]
    fn the_sized_chunker_roundtrips_at_a_relay_budget() {
        let id = msg_id("a", "b", 4);
        let plain_len = 60_000;
        let budget = plain_len - CHUNK_HEADER_LEN;
        let payload: Vec<u8> = (0..(2 * budget + 123))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let chunks = chunk_message_sized(id, &payload, plain_len).expect("chunk");
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            assert_eq!(c.len(), plain_len, "uniform blocks at the sized budget");
        }
        let mut r = Reassembler::new_sized(plain_len);
        let mut out = None;
        for c in chunks {
            if let PushOutcome::Complete(_, m) = r.push(&c).expect("push") {
                out = Some(m);
            }
        }
        assert_eq!(out.expect("completes"), payload);
        // a plain length that leaves no payload room is refused, not looped
        assert!(chunk_message_sized(id, b"x", CHUNK_HEADER_LEN).is_err());
    }

    #[test]
    fn reassembly_converges_under_duplication_and_reordering() {
        let id = msg_id("a", "b", 3);
        let payload: Vec<u8> = (0..(2 * CHUNK_PAYLOAD_BUDGET + 100))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let chunks = chunk_message(id, &payload).expect("chunk");
        // deliver reversed, then everything again (duplicates)
        let mut r = Reassembler::new();
        let mut complete = 0;
        for c in chunks.iter().rev().chain(chunks.iter()) {
            match r.push(c).expect("push") {
                PushOutcome::Complete(_, m) => {
                    assert_eq!(m, payload);
                    complete += 1;
                }
                PushOutcome::Buffered(_) | PushOutcome::Duplicate(_) => {}
            }
        }
        assert_eq!(complete, 1, "the message completes exactly once");
    }

    #[test]
    fn hostile_headers_are_rejected_not_panicking() {
        let mut r = Reassembler::new();
        // wrong size
        assert!(r.push(&[0u8; 10]).is_err());
        // count 0
        let mut c = vec![0u8; CHUNK_PLAIN_LEN];
        assert!(r.push(&c).is_err());
        // index >= count
        c[MSG_ID_LEN] = 5;
        c[MSG_ID_LEN + 2] = 1;
        assert!(r.push(&c).is_err());
        // len over budget
        let mut c = vec![0u8; CHUNK_PLAIN_LEN];
        c[MSG_ID_LEN + 2] = 1; // count = 1
        c[MSG_ID_LEN + 4] = 0xff;
        c[MSG_ID_LEN + 5] = 0xff;
        assert!(r.push(&c).is_err());
    }

    /// Fuzz-style sweep (concept §7 — server input is untrusted): 40 000
    /// pseudo-random chunks, valid-length and not, never panic; every outcome is
    /// a graceful `Ok`/`Err`, and the reassembler still completes a well-formed
    /// message afterwards (its state was not corrupted by the hostile stream).
    #[test]
    fn reassembler_survives_a_hostile_byte_sweep() {
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let bounded = |v: u64, n: u64| usize::try_from(v % n).unwrap_or(0);
        let mut r = Reassembler::new();
        for _ in 0..12_000 {
            let plain = u64::try_from(CHUNK_PLAIN_LEN).unwrap_or(u64::MAX);
            let len = match next() % 5 {
                0 => CHUNK_PLAIN_LEN,
                1 => CHUNK_PLAIN_LEN.saturating_sub(bounded(next(), 8)),
                2 => CHUNK_PLAIN_LEN + bounded(next(), 8),
                _ => bounded(next(), plain + 32),
            };
            let chunk: Vec<u8> = (0..len).map(|_| next().to_le_bytes()[0]).collect();
            // the contract: never panics on any bytes
            let _ = r.push(&chunk);
        }
        // and it is still functional
        let payload: Vec<u8> = (0..(2 * CHUNK_PAYLOAD_BUDGET + 7))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let mut out = None;
        for c in chunk_message(msg_id("x", "y", 42), &payload).expect("chunk") {
            if let PushOutcome::Complete(_, m) = r.push(&c).expect("push") {
                out = Some(m);
            }
        }
        assert_eq!(out.expect("completes"), payload, "still works after the sweep");
    }

    #[test]
    fn msg_ids_are_deterministic_and_link_scoped() {
        assert_eq!(msg_id("a", "b", 7), msg_id("a", "b", 7));
        assert_ne!(msg_id("a", "b", 7), msg_id("a", "b", 8));
        assert_ne!(msg_id("a", "b", 7), msg_id("b", "a", 7));
        // the separator prevents ("ab","c") == ("a","bc")
        assert_ne!(msg_id("ab", "c", 7), msg_id("a", "bc", 7));
    }

    /// Delivery guarantee §4.5: epoch 0 is BYTE-identical to the unsalted id
    /// (first sends and old-node interop keep today's wire ids exactly),
    /// while every later epoch mints a distinct id — the receiver's
    /// completed ring can never swallow a rewound resend unread.
    #[test]
    fn resend_epochs_salt_the_msg_id_but_epoch_zero_is_legacy() {
        assert_eq!(msg_id("a", "b", 7), msg_id_epoch("a", "b", 7, 0));
        assert_ne!(msg_id_epoch("a", "b", 7, 0), msg_id_epoch("a", "b", 7, 1));
        assert_ne!(msg_id_epoch("a", "b", 7, 1), msg_id_epoch("a", "b", 7, 2));
        // deterministic per epoch: same-attempt fan-out copies share the id
        assert_eq!(msg_id_epoch("a", "b", 7, 3), msg_id_epoch("a", "b", 7, 3));
    }
}
