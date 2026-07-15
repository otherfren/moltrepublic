// SPDX-License-Identifier: GPL-3.0-or-later

//! File-transfer frames: the wire vocabulary that moves a shared file's
//! BYTES from the sharer to a requester — off the workspace event log,
//! over a dedicated queue pair (the recovery side-channel pattern).
//!
//! Control plane: a [`FetchRequest`] rides the running mesh as MLS
//! ciphertext (`WorkspaceEvent::FileRequested { ct }`, the `MeshAnnounce`
//! posture — the log stores only ciphertext; queue keys never enter shared
//! history). Data plane: bincode-encoded [`TransferFrame`]s flow over the
//! requester-minted reply queue (per-queue [`crate::WrapKey`] AEAD), acked
//! piece-by-piece with [`TransferAck`]s on a sharer-minted ack queue —
//! flow control, because SMP queues have bounded quotas.

use serde::{Deserialize, Serialize};

use crate::invite::ReplyHandover;
use crate::NetError;

/// One data piece: 256 KiB — a single framed message of ~17 wire blocks.
/// Small enough that reassembly memory stays bounded and progress is
/// observable; large enough that per-message overhead stays negligible.
pub const PIECE_LEN: usize = 256 * 1024;

/// [`PIECE_LEN`] as the u64 the size math runs in (a separate literal, so
/// no silent conversion).
pub const PIECE_LEN64: u64 = 256 * 1024;

/// The sharer keeps at most this many unacked pieces in flight (~1 MiB) —
/// backpressure against the receiving queue's quota.
pub const PIECE_WINDOW: u32 = 4;

/// requester → sharer, MLS-encrypted inside `FileRequested { ct }`: "send
/// me the file of share `id` to this fresh reply queue". Authenticated by
/// successful group decryption (only members encrypt to the group).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    /// The share message's stable chat id, 32-char lowercase hex.
    pub id: String,
    /// The requester's fresh reply queue + wrap key (the data plane).
    pub reply: ReplyHandover,
    /// Unix seconds after which the sharer must NOT serve: the mesh outbox
    /// is store-and-forward, so a request may reach a long-offline sharer
    /// after the requester's recv loop is gone.
    pub expires: u64,
}

/// sharer → requester, over the reply queue (bincode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferFrame {
    /// First frame: what follows.
    Manifest {
        /// The share id this transfer answers (binds frames to the request).
        id: String,
        /// Exact byte count to follow.
        size: u64,
        /// `pieces_for(size)` — 0 for an empty file.
        pieces: u32,
        /// sha256 over the whole file, lowercase hex, recomputed at serve
        /// time (the receiver verifies against the log-anchored share
        /// checksum — a serve of different bytes fails there).
        sha256: String,
        /// The sharer's ack queue: the requester confirms each received
        /// piece there (flow control).
        ack: ReplyHandover,
    },
    /// One data piece, `index` ∈ 0..pieces, in order.
    Piece {
        /// 0-based piece number.
        index: u32,
        /// `PIECE_LEN` bytes (the last piece may be shorter).
        bytes: Vec<u8>,
    },
    /// Honest refusal/failure instead of silence (file changed, share
    /// removed, unknown id, expired request).
    Refused {
        /// The share id this answers.
        id: String,
        /// Human-readable reason, shown to the requester.
        reason: String,
    },
}

/// requester → sharer, over the ack queue (bincode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferAck {
    /// Piece `index` landed — release the next window slot.
    Received {
        /// The confirmed 0-based piece number.
        index: u32,
    },
    /// The requester gave up (disk error, checksum mismatch) — stop sending.
    Abort {
        /// Human-readable reason (tracing only on the sharer).
        reason: String,
    },
}

/// How many [`PIECE_LEN`] pieces a file of `size` bytes splits into.
pub fn pieces_for(size: u64) -> u32 {
    u32::try_from(size.div_ceil(PIECE_LEN64)).unwrap_or(u32::MAX)
}

/// Encode one data-plane frame (bincode — the pieces are raw bytes; JSON
/// + hex would double every frame on the wire).
pub fn encode_frame(f: &TransferFrame) -> Result<Vec<u8>, NetError> {
    bincode::serialize(f).map_err(|e| NetError::Framing(format!("encoding transfer frame: {e}")))
}

/// Decode one data-plane frame.
pub fn decode_frame(bytes: &[u8]) -> Result<TransferFrame, NetError> {
    bincode::deserialize(bytes)
        .map_err(|e| NetError::Framing(format!("decoding transfer frame: {e}")))
}

/// Encode one ack frame.
pub fn encode_ack(a: &TransferAck) -> Result<Vec<u8>, NetError> {
    bincode::serialize(a).map_err(|e| NetError::Framing(format!("encoding transfer ack: {e}")))
}

/// Decode one ack frame.
pub fn decode_ack(bytes: &[u8]) -> Result<TransferAck, NetError> {
    bincode::deserialize(bytes).map_err(|e| NetError::Framing(format!("decoding transfer ack: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handover() -> ReplyHandover {
        ReplyHandover {
            server: "smp://fp@example.org".to_string(),
            queue_id: "aa11".to_string(),
            wrap: "bb22".to_string(),
        }
    }

    /// Every frame kind survives the wire encoding byte-exactly.
    #[test]
    fn frames_round_trip() {
        let frames = [
            TransferFrame::Manifest {
                id: "ab".repeat(16),
                size: 5,
                pieces: 1,
                sha256: "cd".repeat(32),
                ack: handover(),
            },
            TransferFrame::Piece {
                index: 3,
                bytes: vec![7u8; 1234],
            },
            TransferFrame::Refused {
                id: "ab".repeat(16),
                reason: "the file changed since it was shared".to_string(),
            },
        ];
        for f in frames {
            let bytes = encode_frame(&f).expect("encode");
            let back = decode_frame(&bytes).expect("decode");
            assert_eq!(format!("{f:?}"), format!("{back:?}"));
        }
        // garbage is an error, not a panic
        assert!(decode_frame(&[0xff; 3]).is_err());
    }

    /// Acks round-trip too.
    #[test]
    fn acks_round_trip() {
        for a in [
            TransferAck::Received { index: 42 },
            TransferAck::Abort {
                reason: "disk full".to_string(),
            },
        ] {
            let bytes = encode_ack(&a).expect("encode");
            let back = decode_ack(&bytes).expect("decode");
            assert_eq!(format!("{a:?}"), format!("{back:?}"));
        }
    }

    /// The piece math: exact multiples, ragged tails, empty files.
    #[test]
    fn piece_math_covers_the_edges() {
        let len = PIECE_LEN64;
        assert_eq!(pieces_for(0), 0, "an empty file has no pieces");
        assert_eq!(pieces_for(1), 1);
        assert_eq!(pieces_for(len), 1, "an exact piece is one piece");
        assert_eq!(pieces_for(len + 1), 2, "one byte over spills");
        assert_eq!(pieces_for(4 * len + 17), 5, "ragged tail counts");
    }

    /// The fetch request is JSON (it travels inside MLS ciphertext next to
    /// the other JSON ritual messages, not on the bincode data plane).
    #[test]
    fn fetch_request_round_trips_as_json() {
        let req = FetchRequest {
            id: "ef".repeat(16),
            reply: handover(),
            expires: 1_800_000_000,
        };
        let json = serde_json::to_string(&req).expect("to json");
        let back: FetchRequest = serde_json::from_str(&json).expect("from json");
        assert_eq!(back.id, req.id);
        assert_eq!(back.reply.queue_id, "aa11");
        assert_eq!(back.expires, req.expires);
    }
}
