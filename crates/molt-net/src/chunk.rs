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
    let mut h = Sha256::new();
    h.update(b"molt-msg");
    h.update(sender.as_bytes());
    h.update([0u8]);
    h.update(recipient.as_bytes());
    h.update(wire_seq.to_le_bytes());
    let d = h.finalize();
    let mut id = [0u8; MSG_ID_LEN];
    id.copy_from_slice(&d[..MSG_ID_LEN]);
    MsgId(id)
}

/// Split one message into chunk plaintexts (each exactly
/// [`CHUNK_PLAIN_LEN`], zero-padded after the payload). An empty message
/// still yields one chunk — a message always has at least one block.
pub fn chunk_message(id: MsgId, payload: &[u8]) -> Result<Vec<Vec<u8>>, NetError> {
    let count = payload.len().div_ceil(CHUNK_PAYLOAD_BUDGET).max(1);
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
        let start = index * CHUNK_PAYLOAD_BUDGET;
        let end = payload.len().min(start + CHUNK_PAYLOAD_BUDGET);
        let part = &payload[start.min(payload.len())..end];
        let index_u16 = u16::try_from(index)
            .map_err(|_| NetError::Framing("chunk index overflow".to_string()))?;
        let len_u16 = u16::try_from(part.len())
            .map_err(|_| NetError::Framing("chunk payload overflow".to_string()))?;
        let mut chunk = Vec::with_capacity(CHUNK_PLAIN_LEN);
        chunk.extend_from_slice(&id.0);
        chunk.extend_from_slice(&index_u16.to_le_bytes());
        chunk.extend_from_slice(&count_u16.to_le_bytes());
        chunk.extend_from_slice(&len_u16.to_le_bytes());
        chunk.extend_from_slice(part);
        chunk.resize(CHUNK_PLAIN_LEN, 0);
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

/// One partially received message.
struct Partial {
    count: u16,
    have: usize,
    chunks: Vec<Option<Vec<u8>>>,
}

/// Reassembles chunks into messages, deduplicating by
/// `(message id, chunk index)`. Bounded: at most [`MAX_PARTIALS`] open
/// messages, the oldest evicted first.
pub struct Reassembler {
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
        Reassembler {
            partial: HashMap::new(),
            order: VecDeque::new(),
            completed: VecDeque::new(),
        }
    }

    /// Feed one chunk plaintext (untrusted — header fields are validated).
    pub fn push(&mut self, chunk: &[u8]) -> Result<PushOutcome, NetError> {
        if chunk.len() != CHUNK_PLAIN_LEN {
            return Err(NetError::Framing(format!(
                "chunk must be {CHUNK_PLAIN_LEN} bytes, got {}",
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
        if count == 0 || index >= count || usize::from(len) > CHUNK_PAYLOAD_BUDGET {
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
                    have: 0,
                    chunks: vec![None; usize::from(count)],
                })
            }
        };
        let slot = &mut partial.chunks[usize::from(index)];
        if slot.is_some() {
            return Ok(PushOutcome::Duplicate(id));
        }
        *slot = Some(payload);
        partial.have += 1;
        if partial.have < usize::from(partial.count) {
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
        for c in done.chunks.into_iter().flatten() {
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

    #[test]
    fn msg_ids_are_deterministic_and_link_scoped() {
        assert_eq!(msg_id("a", "b", 7), msg_id("a", "b", 7));
        assert_ne!(msg_id("a", "b", 7), msg_id("a", "b", 8));
        assert_ne!(msg_id("a", "b", 7), msg_id("b", "a", 7));
        // the separator prevents ("ab","c") == ("a","bc")
        assert_ne!(msg_id("ab", "c", 7), msg_id("a", "bc", 7));
    }
}
