// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-segment log keys — WP4a §A.3 (`docs_archive/chain/log_compaction.md`).
//!
//! Compaction promises that expired chat content is *really* gone, not merely
//! unlinked. An unlinked file's blocks survive on the medium; so each log
//! segment is encrypted under its **own** data key (DEK), and dropping a
//! segment means **erasing that key** as well as unlinking the file. Without
//! its DEK, forensically recovered segment bytes are worthless.
//!
//! The DEKs live in one small table, `log/keys.state`, encrypted under a
//! workspace sub-key (`molt-log-keys`) in the same frame format as
//! `transport.state`/`chain.state` and rewritten whole, atomically, on every
//! change.
//!
//! The table is created by the **first compaction**, never before (F6): an
//! un-compacted workspace keeps its segments under the workspace key exactly
//! as before, so nothing changes for a workspace that never prunes. From the
//! migration on, every segment — old ones rewritten, new ones at rotation —
//! has an entry.
//!
//! It also carries what a pruned log can no longer derive by counting: each
//! segment's **first seq**. Seq is positional (frame *k* of the log is seq
//! *k*), so once whole segments are gone the replay has to be told where the
//! surviving log starts.
//!
//! Erasure is meant literally, so the keys are wiped from MEMORY too: an
//! erased DEK is zeroized before its entry is dropped, and every plaintext
//! copy of the table (the JSON it is serialized to, the JSON it is decrypted
//! from) is held in `Zeroizing` buffers. Otherwise "the key is gone" would
//! hold only for the file while copies of it sat in freed heap, a swap file
//! or a core dump.
//!
//! Honest limits (documented, not solved here): (1) on a journaling/flash
//! filesystem an OLD copy of this small table can survive a rewrite — a hard
//! guarantee needs TRIM or full-disk encryption; (2) S3 backup copies taken
//! before a compaction still contain the dropped content until
//! `s3_keep_copies` ages them out (F7).

use std::path::Path;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::StorageError;

/// The key table's path inside a workspace directory.
pub(crate) const KEYS_FILE: &str = "log/keys.state";

/// Table schema version. A reader that meets a higher one must refuse the
/// workspace rather than guess which segments it may drop.
pub(crate) const KEYS_VERSION: u32 = 1;

/// One segment's compaction metadata.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentKey {
    /// Segment number (the `000042.mlog` part).
    pub no: u64,
    /// The seq of this segment's FIRST frame — what a pruned log can no
    /// longer derive by counting from the beginning.
    pub first_seq: u64,
    /// This segment's data key. Erasing it is the deletion.
    pub dek: [u8; 32],
}

// manual: the DEK is key material — never in Debug output
impl std::fmt::Debug for SegmentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentKey")
            .field("no", &self.no)
            .field("first_seq", &self.first_seq)
            .finish_non_exhaustive()
    }
}

impl Drop for SegmentKey {
    /// A dropped entry must not leave the data key in freed memory — the
    /// whole point of erasing it is that the segment's bytes become
    /// worthless.
    fn drop(&mut self) {
        self.dek.zeroize();
    }
}

/// The whole table (`log/keys.state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentKeyTable {
    /// [`KEYS_VERSION`].
    pub version: u32,
    /// The compaction floor: the highest seq this node has physically
    /// dropped. 0 = nothing dropped yet (the table exists but the log is
    /// still complete). A peer whose delivery cursor sits at or below the
    /// floor cannot be served from the log any more and is redirected to the
    /// chain catch-up (§A.1 C2).
    #[serde(default)]
    pub floor: u64,
    /// One entry per segment file present, ascending by `no`.
    pub segments: Vec<SegmentKey>,
}

impl SegmentKeyTable {
    /// A fresh table for a log whose segments are about to be migrated.
    pub(crate) fn new() -> SegmentKeyTable {
        SegmentKeyTable {
            version: KEYS_VERSION,
            floor: 0,
            segments: Vec::new(),
        }
    }

    /// The highest segment number with an entry (`None` on an empty table):
    /// everything below it without a key was ERASED, never unmigrated.
    pub(crate) fn highest_no(&self) -> Option<u64> {
        self.segments.iter().map(|s| s.no).max()
    }

    /// This segment's data key, if the table knows it.
    pub(crate) fn dek(&self, no: u64) -> Option<[u8; 32]> {
        self.segments.iter().find(|s| s.no == no).map(|s| s.dek)
    }

    /// The seq of this segment's first frame, if known.
    pub(crate) fn first_seq(&self, no: u64) -> Option<u64> {
        self.segments
            .iter()
            .find(|s| s.no == no)
            .map(|s| s.first_seq)
    }

    /// Record a segment (replacing any entry with the same number, so a
    /// repeated migration step is idempotent). Kept sorted.
    pub(crate) fn put(&mut self, entry: SegmentKey) {
        self.segments.retain(|s| s.no != entry.no);
        self.segments.push(entry);
        self.segments.sort_by_key(|s| s.no);
    }

    /// **Erase** a segment's key — the crypto half of dropping it. The
    /// removed entry zeroizes its key as it goes ([`SegmentKey::drop`]).
    pub(crate) fn forget(&mut self, no: u64) {
        self.segments.retain(|s| s.no != no);
    }

    /// Mint a fresh data key from the OS CSPRNG.
    pub(crate) fn fresh_dek() -> Result<[u8; 32], StorageError> {
        let mut dek = [0u8; 32];
        getrandom::getrandom(&mut dek)
            .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
        Ok(dek)
    }
}

/// Read the table of a workspace directory. `Ok(None)` = this workspace has
/// never been compacted (every segment is under the workspace key, the
/// pre-WP4a shape). An unreadable or too-new table is an error: guessing
/// which segments may be dropped is exactly what must not happen.
pub(crate) fn read_table(
    dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<Option<SegmentKeyTable>, StorageError> {
    let path = dir.join(KEYS_FILE);
    let data = match crate::read_capped(&path, crate::READ_CAP_STATE, "keys.state") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let plaintext = Zeroizing::new(
        crate::decrypt_state_file(&keys_key(ws_key, id), id, crate::KEYS_SEGMENT, &data)
            .map_err(|e| StorageError::Corrupt(format!("log key table: {e}")))?,
    );
    let table: SegmentKeyTable = serde_json::from_slice(&plaintext)
        .map_err(|e| StorageError::Corrupt(format!("log key table decode: {e}")))?;
    if table.version > KEYS_VERSION {
        return Err(StorageError::NewerVersion(table.version));
    }
    Ok(Some(table))
}

/// Rewrite the table atomically (tmp + fsync + rename, mode 0600) — the same
/// discipline as the snapshot write, because losing it loses the log.
pub(crate) fn write_table(
    dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
    table: &SegmentKeyTable,
) -> Result<(), StorageError> {
    // the serialized table is every DEK in the clear — never leave it in
    // freed heap
    let plaintext = Zeroizing::new(
        serde_json::to_vec(table)
            .map_err(|e| StorageError::Corrupt(format!("encoding the log key table: {e}")))?,
    );
    let frame = crate::encode_frame(
        &keys_key(ws_key, id),
        id,
        crate::KEYS_SEGMENT,
        0,
        &plaintext,
    )?;
    crate::write_atomic(dir, KEYS_FILE, &frame, true)
}

/// The table's own encryption key — a workspace sub-key, like the transport
/// and chain state files use.
fn keys_key(ws_key: &[u8; 32], id: &[u8; 32]) -> [u8; 32] {
    crate::hkdf32(ws_key, "molt-log-keys", id)
}
