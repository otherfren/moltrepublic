// SPDX-License-Identifier: GPL-3.0-or-later

//! S3 (`docs/chain/chain_divergence_plan.md`): a checkpoint blob too large
//! for one frame travels as [`WorkspaceEvent::CheckpointServedPart`]s and is
//! reassembled, hash-checked, then handled like a `CheckpointServed`.

use std::collections::BTreeMap;

use base64::Engine as _;
use molt_core::{CheckpointState, WorkspaceEvent};

/// The most parts one blob may have (a blob past this is not served).
pub(crate) const MAX_BLOB_PARTS: u32 = 64;

/// Room for the event fields around a part's data.
const PART_RESERVE: usize = 512;

/// The events that carry `blob` within `cap` bytes per event: one
/// `CheckpointServed` when it fits, else its parts. `None` = too large even
/// in parts.
pub(crate) fn blob_events(blob: &CheckpointState, cap: usize) -> Option<Vec<WorkspaceEvent>> {
    let whole = WorkspaceEvent::CheckpointServed { blob: blob.clone() };
    let whole_len = serde_json::to_vec(&whole).map_or(usize::MAX, |b| b.len());
    if whole_len <= cap {
        return Some(vec![whole]);
    }
    let bytes = serde_json::to_vec(blob).ok()?;
    // base64 grows the data by 4/3
    let chunk = cap.saturating_sub(PART_RESERVE).saturating_mul(3) / 4;
    if chunk == 0 {
        return None;
    }
    let total = u32::try_from(bytes.len().div_ceil(chunk)).ok()?;
    if total > MAX_BLOB_PARTS {
        tracing::warn!(bytes = bytes.len(), parts = total, max = MAX_BLOB_PARTS, "checkpoint_blob=too_large");
        return None;
    }
    let blob_hash = molt_storage::content_hash(&bytes);
    Some(
        bytes
            .chunks(chunk)
            .zip(0u32..)
            .map(|(data, index)| WorkspaceEvent::CheckpointServedPart {
                blob_hash: blob_hash.clone(),
                index,
                total,
                data: base64::engine::general_purpose::STANDARD.encode(data),
            })
            .collect(),
    )
}

/// One blob being reassembled. A part of another blob replaces it.
#[derive(Debug, Default)]
pub(crate) struct BlobAssembly {
    hash: String,
    total: u32,
    parts: BTreeMap<u32, Vec<u8>>,
}

impl BlobAssembly {
    /// Take one part; the whole blob once the last part makes it complete
    /// and its bytes match `blob_hash`.
    pub(crate) fn accept(&mut self, blob_hash: &str, index: u32, total: u32, data: &str) -> Option<CheckpointState> {
        if total == 0 || total > MAX_BLOB_PARTS || index >= total {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
        if self.hash != blob_hash || self.total != total {
            *self = Self { hash: blob_hash.to_string(), total, parts: BTreeMap::new() };
        }
        self.parts.insert(index, bytes);
        if self.parts.len() < usize::try_from(total).unwrap_or(usize::MAX) {
            return None;
        }
        let whole: Vec<u8> = std::mem::take(&mut self.parts).into_values().flatten().collect();
        let ok = molt_storage::content_hash(&whole) == self.hash;
        *self = Self::default();
        if !ok {
            tracing::warn!(blob_hash, "checkpoint_blob=hash_mismatch");
            return None;
        }
        serde_json::from_slice(&whole).ok()
    }
}
