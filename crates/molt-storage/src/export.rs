// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-export-v1` — the encrypted single-file workspace export (milestone
//! S4, `documents/backup_restore_design.md` §3).
//!
//! One `.molt.enc` file + one secret = restored **knowledge**: the manifest,
//! the encrypted history (verbatim ciphertext — the frames stay under the
//! workspace key, whose AAD already binds them to this workspace), the
//! threshold-signed chain, the newest snapshot and the logo. Live protocol
//! state — the MLS ratchet and the SMP queue credentials in
//! `transport.state` — is **hard-excluded** (design §3.3): restoring it would
//! fork the ratchet (nonce reuse), freeze forward secrecy and fight the
//! original for its queues. Membership comes back via the recovery ritual,
//! never via this blob.
//!
//! Byte layout (design §3.5):
//!
//! ```text
//! .molt.enc := magic(15) | header_len:u32le | header JSON | chunk*
//! chunk     := nonce(24) | ct_len:u32le | XChaCha20-Poly1305 ciphertext
//! aad(i)    := magic ‖ workspace_id(32) ‖ i:u64le ‖ final:u8
//! payload   := meta_len:u32le | meta JSON | entry*
//! entry     := path_len:u16le | path | data_len:u64le | data
//! ```
//!
//! The header is plaintext but needs no MAC: the stream key binds it —
//! `k_stream = HKDF(k_root, "molt-export-stream-v1", header_bytes)` — so any
//! header tampering fails authentication on the first chunk. The AAD's chunk
//! index kills reorder, its `final` flag kills truncation, and a non-final
//! chunk must decrypt to exactly `chunk_bytes` (no splice-shortening).
//!
//! Key modes (design §3.4): `passphrase` (manual export, Argon2id over the
//! NFC-normalized passphrase) and `workspace`
//! (`HKDF(workspace_key, "molt-export-backup-v1", id)` — the S5 auto-backup,
//! restorable from recovery phrase + workspace id, no prompt).
//!
//! The functions here run Argon2 and file I/O — they **block** and must only
//! ever be called off-actor (`spawn_blocking` / spawned tasks).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::StorageError;

/// The plaintext magic every export starts with (repo tag convention).
pub const EXPORT_MAGIC: &[u8; 15] = b"molt-export-v1\0";
/// Format version this build writes and reads.
pub const EXPORT_VERSION: u32 = 1;
/// Plaintext chunk size the writer cuts the payload at (4 MiB).
pub const EXPORT_CHUNK_BYTES: u32 = 4 * 1024 * 1024;
/// Minimum export-passphrase length (characters of the NFC form; length
/// only, no composition rules — design §3.4).
pub const EXPORT_PASSPHRASE_MIN_CHARS: usize = 10;
/// Argon2id v1 defaults (RFC 9106's second recommended profile).
pub const EXPORT_ARGON2_M_KIB: u32 = 64 * 1024;
/// Argon2id iterations (t_cost).
pub const EXPORT_ARGON2_T: u32 = 3;
/// Argon2id lanes (p_cost).
pub const EXPORT_ARGON2_P: u32 = 1;

// Import-side caps — a malicious header must not DoS the reader (§3.4).
const IMPORT_ARGON2_M_KIB_MAX: u32 = 1024 * 1024; // 1 GiB
const IMPORT_ARGON2_T_MAX: u32 = 16;
const IMPORT_ARGON2_P_MAX: u32 = 8;
const IMPORT_CHUNK_BYTES_MAX: u32 = 64 * 1024 * 1024;
const IMPORT_HEADER_MAX: u32 = 64 * 1024;
const IMPORT_META_MAX: u32 = 16 * 1024 * 1024;

/// The HKDF domain deriving `k_root` in `workspace` key mode.
const EXPORT_BACKUP_TAG: &str = "molt-export-backup-v1";
/// The HKDF domain binding the header into the stream key.
const EXPORT_STREAM_TAG: &str = "molt-export-stream-v1";

/// Included files as `(relative path, absolute path)` + the skipped names.
type CollectedEntries = (Vec<(String, PathBuf)>, Vec<String>);

/// Lossless `u32 → usize` (every supported target has ≥32-bit pointers).
fn usize_of(v: u32) -> usize {
    usize::try_from(v).expect("u32 fits in usize")
}

/// How the export blob's protection key is derived (design §3.4).
pub enum ExportKey {
    /// Manual export (story 9): a user-chosen passphrase, Argon2id-stretched.
    /// Minimum [`EXPORT_PASSPHRASE_MIN_CHARS`] characters (NFC form).
    Passphrase(String),
    /// Automatic backup (story 12): the key derives from the workspace's own
    /// key — restorable from recovery phrase + workspace id, promptless.
    Workspace,
}

/// The secret the reading side supplies (story 13 maps a typed recovery
/// phrase to [`ExportSecret::WorkspaceKey`] via the id in the header).
pub enum ExportSecret {
    /// For `key_mode = "passphrase"` blobs.
    Passphrase(String),
    /// For `key_mode = "workspace"` blobs: the re-derived workspace key.
    WorkspaceKey([u8; 32]),
}

/// What an export produced — the caller reports these honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Total bytes written to the output.
    pub bytes: u64,
    /// Unknown files in the workspace dir the blob does NOT contain
    /// (honesty: the user sees what was left out).
    pub skipped: Vec<String>,
    /// Unix seconds the export was taken (also authenticated in the meta).
    pub created: u64,
}

/// The plaintext header (kept minimal on purpose: the workspace-id pseudonym
/// — never the shared republic id, which would let a storage provider
/// correlate different members' backups — plus the crypto parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportHeader {
    /// `"molt-export-v1"`.
    pub format: String,
    /// [`EXPORT_VERSION`].
    pub version: u32,
    /// The per-member workspace-id pseudonym (64 hex chars).
    pub workspace_id: String,
    /// `"passphrase" | "workspace"`.
    pub key_mode: String,
    /// KDF parameters (passphrase mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<ExportKdf>,
    /// `"xchacha20poly1305"`.
    pub cipher: String,
    /// Plaintext bytes per chunk.
    pub chunk_bytes: u32,
}

/// The `kdf` table of a passphrase-mode header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportKdf {
    /// `"argon2id"`.
    pub algo: String,
    /// Memory cost in KiB.
    pub m_kib: u32,
    /// Iterations.
    pub t: u32,
    /// Lanes.
    pub p: u32,
    /// 32-byte salt, hex.
    pub salt: String,
}

/// The authenticated metadata at the head of the payload (design §3.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMeta {
    /// Unix seconds the export was taken.
    pub created: u64,
    /// The exporting crate version.
    pub exporter: String,
    /// At-rest state of the source dir at export time: `"device" | "phrase"`.
    pub at_rest: String,
    /// The workspace key, hex — re-keyed under the export secret so the
    /// history inside is readable off-device.
    pub workspace_key: String,
    /// The 32-byte recovery-seed entropy, hex, when the exporting side has
    /// it (design §3.6: blob + secret then replaces the recovery phrase —
    /// full seat capability; `None` on phrase-sealed sources).
    #[serde(default)]
    pub seed: Option<String>,
    /// Number of file entries following.
    pub files: u64,
}

/// One decrypted file entry of the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportEntry {
    /// Relative path inside the workspace dir (validated: no `..`, no
    /// absolute paths, no empty components).
    pub path: String,
    /// The file bytes, verbatim.
    pub data: Vec<u8>,
}

/// A fully decrypted and verified export blob (storage-layer view; the
/// engine-side import of story 13 stages, chain-verifies and materializes it).
#[derive(Debug)]
pub struct ExportArchive {
    /// The plaintext header (already authenticated via the stream key).
    pub header: ExportHeader,
    /// The authenticated payload metadata.
    pub meta: ExportMeta,
    /// The file entries, in payload order (lexicographic by path).
    pub entries: Vec<ExportEntry>,
}

// ---------------------------------------------------------------------------
// Export (writer)
// ---------------------------------------------------------------------------

/// Export the workspace directory `ws_dir` (under the workspace root `root`,
/// which holds the device key) as one `molt-export-v1` blob into `out`.
///
/// The include/exclude table is design §3.2: manifest, prefs, every log
/// segment (verbatim ciphertext — crash-consistent even while a writer
/// appends), the newest snapshot, `chain.state`, the logo. `transport.state`
/// (MLS ratchet + SMP queue credentials) is **never** exported; unknown extra
/// files are skipped and named in the outcome. Blocking (Argon2 + I/O) —
/// call off-actor only.
pub fn export_dir(
    root: &Path,
    ws_dir: &Path,
    key: &ExportKey,
    out: &mut dyn Write,
) -> Result<ExportOutcome, StorageError> {
    export_dir_chunked(root, ws_dir, key, EXPORT_CHUNK_BYTES, out)
}

/// [`export_dir`] with an explicit chunk size (tests exercise the chunking
/// edge cases with tiny chunks; production always uses the default).
pub(crate) fn export_dir_chunked(
    root: &Path,
    ws_dir: &Path,
    key: &ExportKey,
    chunk_bytes: u32,
    out: &mut dyn Write,
) -> Result<ExportOutcome, StorageError> {
    if chunk_bytes == 0 {
        return Err(StorageError::BadFile("export chunk size must be > 0".to_string()));
    }
    // passphrase policy first — before touching the directory
    if let ExportKey::Passphrase(p) = key {
        check_passphrase_policy(p)?;
    }

    let manifest = crate::read_manifest(ws_dir)?;
    let id_hex = manifest.workspace.id.clone();
    let id = crate::id_bytes(&id_hex)?;
    let device_key = crate::load_or_create_device_key(&crate::device_key_path(root))?;
    let sealed = fs::read(ws_dir.join(&manifest.crypto.key_file)).map_err(|e| {
        StorageError::BadFile(format!(
            "no device-sealed workspace key ({e}) — a phrase-sealed workspace \
             cannot be exported without its phrase yet"
        ))
    })?;
    let ws_key = Zeroizing::new(crate::unseal_workspace_key(&device_key, &id, &sealed)?);

    // the seed entropy, when stored (design §3.6): unseal, and pin the key
    // hierarchy — an inconsistent dir must refuse, not export a broken blob
    let seed = read_seed_entropy(ws_dir, &device_key, &id)?;
    if let Some(s) = &seed {
        if crate::derive_workspace_key(s, &id_hex) != *ws_key {
            return Err(StorageError::Crypto(
                "the stored seed does not derive this workspace's key — \
                 refusing to export an inconsistent workspace"
                    .to_string(),
            ));
        }
    }

    let (files, skipped) = collect_entries(ws_dir)?;

    let kdf = match key {
        ExportKey::Passphrase(_) => {
            let mut salt = [0u8; 32];
            getrandom::getrandom(&mut salt)
                .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
            Some(ExportKdf {
                algo: "argon2id".to_string(),
                m_kib: EXPORT_ARGON2_M_KIB,
                t: EXPORT_ARGON2_T,
                p: EXPORT_ARGON2_P,
                salt: hex::encode(salt),
            })
        }
        ExportKey::Workspace => None,
    };
    let header = ExportHeader {
        format: "molt-export-v1".to_string(),
        version: EXPORT_VERSION,
        workspace_id: id_hex.clone(),
        key_mode: match key {
            ExportKey::Passphrase(_) => "passphrase".to_string(),
            ExportKey::Workspace => "workspace".to_string(),
        },
        kdf,
        cipher: "xchacha20poly1305".to_string(),
        chunk_bytes,
    };
    let header_bytes = serde_json::to_vec(&header)
        .map_err(|e| StorageError::Corrupt(format!("encoding export header: {e}")))?;

    let k_root: Zeroizing<[u8; 32]> = match key {
        ExportKey::Passphrase(p) => {
            let kdf = header.kdf.as_ref().expect("passphrase header carries kdf");
            let salt = hex::decode(&kdf.salt).expect("salt was just hex-encoded");
            passphrase_key(p, &salt, kdf.m_kib, kdf.t, kdf.p)?
        }
        ExportKey::Workspace => Zeroizing::new(crate::hkdf32(&*ws_key, EXPORT_BACKUP_TAG, &id)),
    };
    let k_stream = Zeroizing::new(crate::hkdf32(&*k_root, EXPORT_STREAM_TAG, &header_bytes));

    // magic + header
    let mut written: u64 = 0;
    out.write_all(EXPORT_MAGIC)?;
    let header_len = u32::try_from(header_bytes.len())
        .map_err(|_| StorageError::Corrupt("export header too large".to_string()))?;
    out.write_all(&header_len.to_le_bytes())?;
    out.write_all(&header_bytes)?;
    written += 15 + 4 + u64::try_from(header_bytes.len()).unwrap_or(0);

    // encrypted payload stream
    let created = crate::now_secs();
    let meta = ExportMeta {
        created,
        exporter: env!("CARGO_PKG_VERSION").to_string(),
        // story 10 (S6) introduces the "phrase" state; today every
        // exportable dir is device-sealed (a phrase-sealed dir has no
        // key file and was refused above)
        at_rest: "device".to_string(),
        workspace_key: hex::encode(*ws_key),
        seed: seed.as_ref().map(|s| hex::encode(s.as_slice())),
        files: u64::try_from(files.len()).unwrap_or(0),
    };
    let meta_bytes = serde_json::to_vec(&meta)
        .map_err(|e| StorageError::Corrupt(format!("encoding export meta: {e}")))?;

    let mut w = ChunkWriter::new(out, &k_stream, id, usize_of(chunk_bytes));
    let meta_len = u32::try_from(meta_bytes.len())
        .map_err(|_| StorageError::Corrupt("export meta too large".to_string()))?;
    w.write(&meta_len.to_le_bytes())?;
    w.write(&meta_bytes)?;
    for (rel, path) in &files {
        let data = fs::read(path)?;
        let path_len = u16::try_from(rel.len())
            .map_err(|_| StorageError::Corrupt(format!("entry path too long: {rel}")))?;
        w.write(&path_len.to_le_bytes())?;
        w.write(rel.as_bytes())?;
        w.write(&u64::try_from(data.len()).unwrap_or(0).to_le_bytes())?;
        w.write(&data)?;
    }
    written += w.finish()?;

    Ok(ExportOutcome { bytes: written, skipped, created })
}

/// Enforce the passphrase policy (design §3.4): at least
/// [`EXPORT_PASSPHRASE_MIN_CHARS`] characters of the NFC form. Public so the
/// engine enforces the same rule synchronously before spawning the task.
pub fn check_passphrase_policy(pass: &str) -> Result<(), StorageError> {
    if pass.nfc().count() < EXPORT_PASSPHRASE_MIN_CHARS {
        return Err(StorageError::BadFile(format!(
            "the export passphrase needs at least {EXPORT_PASSPHRASE_MIN_CHARS} characters"
        )));
    }
    Ok(())
}

/// Unseal `keys/seed.sealed`, if present. Absent file → `None` (a workspace
/// from before seed storage exports as a knowledge archive); an unreadable
/// or tampered blob is a hard error — exporting a backup that silently lost
/// its seat capability would be dishonest.
fn read_seed_entropy(
    ws_dir: &Path,
    device_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError> {
    let blob = match fs::read(ws_dir.join("keys").join("seed.sealed")) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if blob.len() <= crate::NONCE_LEN {
        return Err(StorageError::BadFile("sealed seed is too short".to_string()));
    }
    let (nonce, ct) = blob.split_at(crate::NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(device_key.into());
    let entropy = cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload { msg: ct, aad: &crate::seed_seal_aad(id) },
        )
        .map_err(|_| StorageError::Crypto("unsealing the stored seed failed".to_string()))?;
    Ok(Some(Zeroizing::new(entropy)))
}

/// Walk the workspace dir into the design-§3.2 include table. Returns the
/// included files as `(relative path, absolute path)` sorted lexicographically
/// (deterministic payload), plus the skipped *unknown* files by name.
/// Designed exclusions (keys, LOCK, tmp, dot-files, `transport.state`, older
/// snapshots) are silent — they are the format's contract, not a surprise.
fn collect_entries(
    ws_dir: &Path,
) -> Result<CollectedEntries, StorageError> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for entry in fs::read_dir(ws_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // runtime scratch (staging, trash markers)
        }
        let path = entry.path();
        let is_dir = entry.file_type()?.is_dir();
        match name.as_str() {
            "manifest.toml" | "prefs.toml" | "chain.state" if !is_dir => {
                files.push((name, path));
            }
            // §3.3 hard exclusion + runtime scratch
            "transport.state" | "LOCK" => {}
            "keys" | "tmp" if is_dir => {}
            "log" if is_dir => {
                for seg in fs::read_dir(&path)? {
                    let seg = seg?;
                    let seg_name = seg.file_name().to_string_lossy().into_owned();
                    if seg_name.ends_with(".mlog") && seg.file_type()?.is_file() {
                        files.push((format!("log/{seg_name}"), seg.path()));
                    } else if !seg_name.starts_with('.') {
                        skipped.push(format!("log/{seg_name}"));
                    }
                }
            }
            "snapshots" if is_dir => {
                // newest snapshot only — snapshots are droppable
                // optimizations, one keeps the blob small (§3.2)
                if let Some((_, newest)) = crate::list_sorted(&path, ".msnap").pop() {
                    let rel = format!(
                        "snapshots/{}",
                        newest.file_name().unwrap_or_default().to_string_lossy()
                    );
                    files.push((rel, newest));
                }
                for snap in fs::read_dir(&path)? {
                    let snap = snap?;
                    let snap_name = snap.file_name().to_string_lossy().into_owned();
                    if !snap_name.ends_with(".msnap") && !snap_name.starts_with('.') {
                        skipped.push(format!("snapshots/{snap_name}"));
                    }
                }
            }
            _ if !is_dir && name.starts_with("logo.") => {
                files.push((name, path));
            }
            _ => {
                // unknown — named honestly so the user sees what the blob
                // does not contain
                skipped.push(if is_dir { format!("{name}/") } else { name });
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    skipped.sort();
    Ok((files, skipped))
}

/// Argon2id: stretch the NFC-normalized passphrase into 32 key bytes.
fn passphrase_key(
    pass: &str,
    salt: &[u8],
    m_kib: u32,
    t: u32,
    p: u32,
) -> Result<Zeroizing<[u8; 32]>, StorageError> {
    let norm: Zeroizing<String> = Zeroizing::new(pass.nfc().collect());
    let params = argon2::Params::new(m_kib, t, p, Some(32))
        .map_err(|e| StorageError::BadFile(format!("export kdf parameters: {e}")))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; 32]);
    a.hash_password_into(norm.as_bytes(), salt, &mut *out)
        .map_err(|e| StorageError::Crypto(format!("export kdf failed: {e}")))?;
    Ok(out)
}

/// The AAD binding chunk `index` to this blob: magic ‖ id ‖ index ‖ final.
fn export_aad(id: &[u8; 32], index: u64, last: bool) -> [u8; 56] {
    let mut aad = [0u8; 56];
    aad[..15].copy_from_slice(EXPORT_MAGIC);
    aad[15..47].copy_from_slice(id);
    aad[47..55].copy_from_slice(&index.to_le_bytes());
    aad[55] = u8::from(last);
    aad
}

/// Buffers plaintext and emits full AEAD chunks; `finish` seals the final
/// (possibly empty) chunk with the `final` AAD flag set.
struct ChunkWriter<'a> {
    out: &'a mut dyn Write,
    cipher: XChaCha20Poly1305,
    id: [u8; 32],
    chunk_bytes: usize,
    buf: Vec<u8>,
    index: u64,
    written: u64,
}

impl<'a> ChunkWriter<'a> {
    fn new(out: &'a mut dyn Write, key: &[u8; 32], id: [u8; 32], chunk_bytes: usize) -> Self {
        ChunkWriter {
            out,
            cipher: XChaCha20Poly1305::new(key.into()),
            id,
            chunk_bytes,
            buf: Vec::new(),
            index: 0,
            written: 0,
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= self.chunk_bytes {
            let rest = self.buf.split_off(self.chunk_bytes);
            let full = std::mem::replace(&mut self.buf, rest);
            self.emit(&full, false)?;
        }
        Ok(())
    }

    fn emit(&mut self, plaintext: &[u8], last: bool) -> Result<(), StorageError> {
        let mut nonce = [0u8; crate::NONCE_LEN];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
        let aad = export_aad(&self.id, self.index, last);
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad: &aad })
            .map_err(|_| StorageError::Crypto("sealing an export chunk failed".to_string()))?;
        self.out.write_all(&nonce)?;
        let ct_len = u32::try_from(ct.len())
            .map_err(|_| StorageError::Corrupt("export chunk too large".to_string()))?;
        self.out.write_all(&ct_len.to_le_bytes())?;
        self.out.write_all(&ct)?;
        self.index += 1;
        self.written += u64::try_from(crate::NONCE_LEN + 4 + ct.len()).unwrap_or(0);
        Ok(())
    }

    /// Seal the trailing plaintext as the final chunk (empty when the payload
    /// is an exact multiple of the chunk size — the flag still travels).
    fn finish(mut self) -> Result<u64, StorageError> {
        let tail = std::mem::take(&mut self.buf);
        self.emit(&tail, true)?;
        self.out.flush()?;
        Ok(self.written)
    }
}

// ---------------------------------------------------------------------------
// Read side (the storage-layer decrypt story 13's import stages on)
// ---------------------------------------------------------------------------

/// Parse, decrypt and verify one export blob. Every failure is honest and
/// hard: bad magic, unknown version (refused *before* any KDF work), KDF
/// parameters beyond the caps, a failed chunk (wrong secret and tampering are
/// deliberately indistinguishable — the AEAD cannot tell and we do not
/// guess), truncation, reorder, a short non-final chunk, malformed entry
/// paths, or a payload whose seed does not derive its workspace key.
/// Blocking (Argon2) — call off-actor only.
pub fn read_export(
    input: &mut dyn Read,
    secret: &ExportSecret,
) -> Result<ExportArchive, StorageError> {
    // magic + header
    let mut magic = [0u8; 15];
    read_exact(input, &mut magic, "export magic")?;
    if magic != *EXPORT_MAGIC {
        return Err(StorageError::BadFile(
            "not a molt export (bad magic)".to_string(),
        ));
    }
    let mut len4 = [0u8; 4];
    read_exact(input, &mut len4, "export header length")?;
    let header_len = u32::from_le_bytes(len4);
    if header_len == 0 || header_len > IMPORT_HEADER_MAX {
        return Err(StorageError::BadFile("implausible export header length".to_string()));
    }
    let mut header_bytes = vec![0u8; usize_of(header_len)];
    read_exact(input, &mut header_bytes, "export header")?;
    let header: ExportHeader = serde_json::from_slice(&header_bytes)
        .map_err(|e| StorageError::BadFile(format!("export header: {e}")))?;
    if header.format != "molt-export-v1" {
        return Err(StorageError::BadFile(format!(
            "unknown export format `{}`",
            header.format
        )));
    }
    // version gate BEFORE any KDF work — a newer blob gets a polite refusal
    if header.version != EXPORT_VERSION {
        return Err(StorageError::NewerVersion(header.version));
    }
    if header.cipher != "xchacha20poly1305" {
        return Err(StorageError::BadFile(format!(
            "unsupported export cipher `{}`",
            header.cipher
        )));
    }
    if header.chunk_bytes == 0 || header.chunk_bytes > IMPORT_CHUNK_BYTES_MAX {
        return Err(StorageError::BadFile("implausible export chunk size".to_string()));
    }
    let id = crate::id_bytes(&header.workspace_id)?;

    // derive the stream key per key mode
    let k_root: Zeroizing<[u8; 32]> = match (header.key_mode.as_str(), secret) {
        ("passphrase", ExportSecret::Passphrase(p)) => {
            let kdf = header.kdf.as_ref().ok_or_else(|| {
                StorageError::BadFile("passphrase export without kdf parameters".to_string())
            })?;
            if kdf.algo != "argon2id" {
                return Err(StorageError::BadFile(format!(
                    "unsupported export kdf `{}`",
                    kdf.algo
                )));
            }
            if kdf.m_kib > IMPORT_ARGON2_M_KIB_MAX
                || kdf.t > IMPORT_ARGON2_T_MAX
                || kdf.p > IMPORT_ARGON2_P_MAX
            {
                return Err(StorageError::BadFile(
                    "export kdf parameters beyond the import caps".to_string(),
                ));
            }
            let salt = hex::decode(&kdf.salt)
                .map_err(|e| StorageError::BadFile(format!("export kdf salt: {e}")))?;
            if salt.len() != 32 {
                return Err(StorageError::BadFile("export kdf salt is not 32 bytes".to_string()));
            }
            passphrase_key(p, &salt, kdf.m_kib, kdf.t, kdf.p)?
        }
        ("workspace", ExportSecret::WorkspaceKey(k)) => {
            Zeroizing::new(crate::hkdf32(k, EXPORT_BACKUP_TAG, &id))
        }
        (mode @ ("passphrase" | "workspace"), _) => {
            return Err(StorageError::BadFile(format!(
                "this export uses key mode `{mode}` — a different secret kind was supplied"
            )));
        }
        (other, _) => {
            return Err(StorageError::BadFile(format!(
                "unknown export key mode `{other}`"
            )));
        }
    };
    let k_stream = Zeroizing::new(crate::hkdf32(&*k_root, EXPORT_STREAM_TAG, &header_bytes));
    let cipher = XChaCha20Poly1305::new((&*k_stream).into());

    // decrypt the chunk stream
    let mut rest = Vec::new();
    input.read_to_end(&mut rest)?;
    if rest.is_empty() {
        return Err(StorageError::Corrupt(
            "truncated export (missing final chunk)".to_string(),
        ));
    }
    let mut plaintext: Vec<u8> = Vec::new();
    let mut offset = 0usize;
    let mut index = 0u64;
    while offset < rest.len() {
        if rest.len() - offset < crate::NONCE_LEN + 4 {
            return Err(StorageError::Corrupt("truncated export chunk".to_string()));
        }
        let nonce = &rest[offset..offset + crate::NONCE_LEN];
        offset += crate::NONCE_LEN;
        let ct_len = usize_of(u32::from_le_bytes(
            rest[offset..offset + 4].try_into().expect("4-byte slice"),
        ));
        offset += 4;
        if ct_len > usize_of(header.chunk_bytes) + 16 {
            return Err(StorageError::Corrupt("implausible export chunk length".to_string()));
        }
        if rest.len() - offset < ct_len {
            return Err(StorageError::Corrupt("truncated export chunk".to_string()));
        }
        let ct = &rest[offset..offset + ct_len];
        offset += ct_len;
        let last = offset == rest.len();
        let aad = export_aad(&id, index, last);
        let pt = cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &aad })
            .map_err(|_| {
                StorageError::Crypto(
                    "wrong passphrase or damaged blob (chunk authentication failed)".to_string(),
                )
            })?;
        if !last && pt.len() != usize_of(header.chunk_bytes) {
            return Err(StorageError::Corrupt(
                "short non-final export chunk (spliced blob?)".to_string(),
            ));
        }
        plaintext.extend_from_slice(&pt);
        index += 1;
    }

    let (meta, entries) = parse_payload(&plaintext)?;

    // key-hierarchy pin (design §3.6): a blob whose seed does not derive its
    // workspace key violates the hierarchy invariant — refuse it
    let ws_key = hex::decode(&meta.workspace_key)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
        .ok_or_else(|| {
            StorageError::Corrupt("export meta carries no valid workspace key".to_string())
        })?;
    if let Some(seed_hex) = &meta.seed {
        let seed = hex::decode(seed_hex)
            .map_err(|e| StorageError::Corrupt(format!("export meta seed: {e}")))?;
        if crate::derive_workspace_key(&seed, &header.workspace_id) != ws_key {
            return Err(StorageError::Crypto(
                "blob is internally inconsistent (key hierarchy)".to_string(),
            ));
        }
    }

    Ok(ExportArchive { header, meta, entries })
}

/// Split the decrypted payload into meta + entries, validating lengths and
/// entry paths (relative, no `..`, no empty components — subsumes traversal).
fn parse_payload(pt: &[u8]) -> Result<(ExportMeta, Vec<ExportEntry>), StorageError> {
    let take = |offset: &mut usize, n: usize, what: &str| -> Result<(), StorageError> {
        if pt.len() - *offset < n {
            return Err(StorageError::Corrupt(format!("truncated export payload ({what})")));
        }
        *offset += n;
        Ok(())
    };
    let mut offset = 0usize;
    take(&mut offset, 4, "meta length")?;
    let meta_len = u32::from_le_bytes(pt[0..4].try_into().expect("4-byte slice"));
    if meta_len > IMPORT_META_MAX {
        return Err(StorageError::Corrupt("implausible export meta length".to_string()));
    }
    let meta_start = offset;
    take(&mut offset, usize_of(meta_len), "meta")?;
    let meta: ExportMeta = serde_json::from_slice(&pt[meta_start..offset])
        .map_err(|e| StorageError::Corrupt(format!("export meta: {e}")))?;

    let mut entries = Vec::new();
    while offset < pt.len() {
        let at = offset;
        take(&mut offset, 2, "entry path length")?;
        let path_len =
            usize::from(u16::from_le_bytes(pt[at..at + 2].try_into().expect("2-byte slice")));
        let p_start = offset;
        take(&mut offset, path_len, "entry path")?;
        let path = std::str::from_utf8(&pt[p_start..offset])
            .map_err(|_| StorageError::Corrupt("entry path is not UTF-8".to_string()))?
            .to_string();
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.contains('\0')
            || path.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(StorageError::Corrupt(format!("illegal entry path `{path}`")));
        }
        let at = offset;
        take(&mut offset, 8, "entry data length")?;
        let data_len = u64::from_le_bytes(pt[at..at + 8].try_into().expect("8-byte slice"));
        let data_len = usize::try_from(data_len)
            .map_err(|_| StorageError::Corrupt("implausible entry length".to_string()))?;
        let d_start = offset;
        take(&mut offset, data_len, "entry data")?;
        entries.push(ExportEntry { path, data: pt[d_start..offset].to_vec() });
    }
    if u64::try_from(entries.len()).unwrap_or(u64::MAX) != meta.files {
        return Err(StorageError::Corrupt(
            "entry count does not match the authenticated meta".to_string(),
        ));
    }
    Ok((meta, entries))
}

/// `read_exact` with an honest per-field truncation error.
fn read_exact(input: &mut dyn Read, buf: &mut [u8], what: &str) -> Result<(), StorageError> {
    input.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            StorageError::Corrupt(format!("truncated export ({what})"))
        } else {
            StorageError::Io(e)
        }
    })
}

// ---------------------------------------------------------------------------
// Tests — the red anchors of design §8.1 that live at the storage layer
// (full import/materialization round-trip is story 13)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{EventEnvelope, WorkspaceEvent};

    fn founded() -> EventEnvelope {
        EventEnvelope {
            seq: 1,
            ts: 42,
            by: "mithra".to_string(),
            body: WorkspaceEvent::Founded {
                name: "Chess Club".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "mithra".to_string(),
                roster: vec!["mithra".to_string(), "anahita".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
            },
        }
    }

    /// A populated workspace dir: genesis + chain.state + logo + a decoy
    /// transport.state, an unknown file, and two snapshots (raw fixtures —
    /// the export copies snapshot bytes verbatim, so their content does not
    /// matter here). Returns `(root, ws_dir, seed)`.
    fn make_ws(tmp: &std::path::Path) -> (PathBuf, PathBuf, Vec<u8>) {
        let root = tmp.join("workspaces");
        let seed =
            crate::seed_entropy(&crate::generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = crate::create_workspace(&root, &seed, &founded()).expect("create");
        ws.write_chain(None, &[]).expect("chain.state");
        let dir = ws.dir().to_path_buf();
        drop(ws); // release the LOCK — the export reads a closed dir
        fs::write(dir.join("logo.png"), b"not really a png").expect("logo");
        fs::write(dir.join("transport.state"), b"MUST NEVER BE EXPORTED").expect("ts");
        fs::write(dir.join("notes.txt"), b"unknown extra file").expect("notes");
        fs::write(dir.join("snapshots").join("000005.msnap"), b"old snap").expect("snap5");
        fs::write(dir.join("snapshots").join("000009.msnap"), b"new snap").expect("snap9");
        (root, dir, seed)
    }

    fn entry<'a>(a: &'a ExportArchive, path: &str) -> Option<&'a ExportEntry> {
        a.entries.iter().find(|e| e.path == path)
    }

    const PASS: &str = "correct horse battery";

    /// Round-trip keystone (storage half): everything the include table
    /// names is in the blob byte-identical, the exclusions are hard, the
    /// unknown file is named, and the meta carries the real key material.
    #[test]
    fn round_trip_passphrase_restores_every_included_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, seed) = make_ws(tmp.path());
        let mut blob = Vec::new();
        let outcome = export_dir(&root, &dir, &ExportKey::Passphrase(PASS.into()), &mut blob)
            .expect("export");
        assert_eq!(outcome.bytes, u64::try_from(blob.len()).expect("len"), "honest byte count");
        assert_eq!(outcome.skipped, vec!["notes.txt".to_string()], "unknown file named");

        let a = read_export(&mut blob.as_slice(), &ExportSecret::Passphrase(PASS.into()))
            .expect("decrypt");
        // include table
        for p in ["manifest.toml", "prefs.toml", "chain.state", "logo.png", "log/000001.mlog"] {
            let e = entry(&a, p).unwrap_or_else(|| panic!("blob misses {p}"));
            assert_eq!(e.data, fs::read(dir.join(p)).expect("disk"), "{p} byte-identical");
        }
        // newest snapshot only
        assert!(entry(&a, "snapshots/000009.msnap").is_some(), "newest snapshot travels");
        assert!(entry(&a, "snapshots/000005.msnap").is_none(), "older snapshot dropped");
        // §3.3 exclusion pin — the regression fence
        assert!(entry(&a, "transport.state").is_none(), "transport.state must never travel");
        assert!(
            a.entries.iter().all(|e| !e.path.starts_with("keys/")),
            "sealed keys must never travel"
        );
        assert!(entry(&a, "notes.txt").is_none(), "unknown files are skipped, not shipped");
        // deterministic order
        let mut sorted: Vec<&str> = a.entries.iter().map(|e| e.path.as_str()).collect();
        let orig = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(orig, sorted, "entries are lexicographically ordered");
        // meta: real key material, hierarchy-consistent
        let id_hex = &a.header.workspace_id;
        assert_eq!(
            a.meta.workspace_key,
            hex::encode(crate::derive_workspace_key(&seed, id_hex)),
            "workspace key travels re-keyed"
        );
        assert_eq!(a.meta.seed.as_deref(), Some(hex::encode(&seed).as_str()), "seed travels");
        assert_eq!(a.meta.at_rest, "device");
        assert_eq!(a.meta.files, u64::try_from(a.entries.len()).expect("count"));
        assert_eq!(a.header.key_mode, "passphrase");
        assert_eq!(a.header.chunk_bytes, EXPORT_CHUNK_BYTES);
    }

    /// Wrong passphrase → the §4.2 message; deliberately indistinguishable
    /// from tampering.
    #[test]
    fn wrong_passphrase_is_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, _) = make_ws(tmp.path());
        let mut blob = Vec::new();
        export_dir(&root, &dir, &ExportKey::Passphrase(PASS.into()), &mut blob).expect("export");
        let err = read_export(
            &mut blob.as_slice(),
            &ExportSecret::Passphrase("not the passphrase".into()),
        )
        .expect_err("wrong passphrase must fail");
        assert!(
            err.to_string().contains("wrong passphrase or damaged blob"),
            "honest, non-oracle error: {err}"
        );
    }

    /// The engine-enforced policy also holds at the storage layer: a short
    /// passphrase never produces a blob.
    #[test]
    fn passphrase_below_minimum_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, _) = make_ws(tmp.path());
        let mut blob = Vec::new();
        let err = export_dir(&root, &dir, &ExportKey::Passphrase("neunchars".into()), &mut blob)
            .expect_err("9 chars must be refused");
        assert!(err.to_string().contains("at least 10 characters"), "{err}");
        assert!(blob.is_empty(), "nothing may be written on refusal");
    }

    /// Workspace key mode round-trips (the S5 auto-backup mode of §3.4) —
    /// also the fast fixture the tamper/truncation loops build on.
    fn workspace_mode_blob(chunk: u32) -> (Vec<u8>, [u8; 32]) {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, seed) = make_ws(tmp.path());
        let id_hex = crate::read_manifest(&dir).expect("manifest").workspace.id;
        let key = crate::derive_workspace_key(&seed, &id_hex);
        let mut blob = Vec::new();
        export_dir_chunked(&root, &dir, &ExportKey::Workspace, chunk, &mut blob)
            .expect("export");
        (blob, key)
    }

    #[test]
    fn workspace_key_mode_round_trips() {
        let (blob, key) = workspace_mode_blob(EXPORT_CHUNK_BYTES);
        let a = read_export(&mut blob.as_slice(), &ExportSecret::WorkspaceKey(key))
            .expect("decrypt");
        assert_eq!(a.header.key_mode, "workspace");
        assert!(a.header.kdf.is_none(), "workspace mode needs no kdf table");
        assert!(entry(&a, "manifest.toml").is_some());
        // supplying the wrong secret KIND is refused with a clear message
        let err = read_export(&mut blob.as_slice(), &ExportSecret::Passphrase(PASS.into()))
            .expect_err("secret kind mismatch");
        assert!(err.to_string().contains("key mode"), "{err}");
    }

    /// Tamper rejection: flipping ANY single byte of the blob — magic,
    /// header, every chunk — must hard-reject (property-style loop; small
    /// chunks keep the blob multi-chunk and the loop meaningful).
    #[test]
    fn any_flipped_byte_is_rejected() {
        let (blob, key) = workspace_mode_blob(256);
        assert!(blob.len() > 1000, "fixture should be multi-chunk");
        // step through the whole blob (every byte would be ~2min of AEAD
        // work; a stride covers magic, header and every chunk region, and
        // the boundaries are hit explicitly)
        let mut offsets: Vec<usize> = (0..blob.len()).step_by(37).collect();
        offsets.extend([0, 14, 15, 18, 19, blob.len() - 1]);
        for at in offsets {
            let mut bad = blob.clone();
            bad[at] ^= 0x01;
            assert!(
                read_export(&mut bad.as_slice(), &ExportSecret::WorkspaceKey(key)).is_err(),
                "flipped byte at {at} must reject"
            );
        }
    }

    /// Truncation: cut at every chunk boundary and mid-chunk → reject
    /// (missing final flag / short chunk). Also: chunk reorder → reject.
    #[test]
    fn truncation_and_reorder_are_rejected() {
        let (blob, key) = workspace_mode_blob(256);
        // walk the chunk boundaries after the header
        let header_len =
            usize_of(u32::from_le_bytes(blob[15..19].try_into().expect("4-byte slice")));
        let chunks_at = 15 + 4 + header_len;
        let mut boundaries = vec![];
        let mut off = chunks_at;
        while off < blob.len() {
            let ct_len = usize_of(u32::from_le_bytes(
                blob[off + 24..off + 28].try_into().expect("4-byte slice"),
            ));
            off += 24 + 4 + ct_len;
            boundaries.push(off);
        }
        assert!(boundaries.len() >= 3, "fixture should be multi-chunk");
        assert_eq!(*boundaries.last().expect("nonempty"), blob.len());
        // cut at every chunk boundary except the true end: the now-last
        // chunk was sealed with final=0 → its AAD check fails
        for b in &boundaries[..boundaries.len() - 1] {
            assert!(
                read_export(&mut blob[..*b].to_vec().as_slice(), &ExportSecret::WorkspaceKey(key))
                    .is_err(),
                "cut at chunk boundary {b} must reject"
            );
        }
        // cut mid-chunk
        let mid = boundaries[0] + 13;
        assert!(
            read_export(&mut blob[..mid].to_vec().as_slice(), &ExportSecret::WorkspaceKey(key))
                .is_err(),
            "mid-chunk cut must reject"
        );
        // swap chunk 0 and 1 (position binding via the AAD index)
        let (a0, a1, a2) = (chunks_at, boundaries[0], boundaries[1]);
        let mut swapped = blob[..chunks_at].to_vec();
        swapped.extend_from_slice(&blob[a1..a2]);
        swapped.extend_from_slice(&blob[a0..a1]);
        swapped.extend_from_slice(&blob[a2..]);
        assert_eq!(swapped.len(), blob.len());
        assert!(
            read_export(&mut swapped.as_slice(), &ExportSecret::WorkspaceKey(key)).is_err(),
            "reordered chunks must reject"
        );
    }

    /// Chunk-boundary edge: chunk size 1 forces the payload to be an exact
    /// multiple of the chunk size — the final chunk is EMPTY and the blob
    /// must still round-trip (the final flag travels on an empty chunk).
    #[test]
    fn exact_multiple_payload_round_trips_with_empty_final_chunk() {
        let (blob, key) = workspace_mode_blob(1);
        let a = read_export(&mut blob.as_slice(), &ExportSecret::WorkspaceKey(key))
            .expect("decrypt");
        assert!(entry(&a, "manifest.toml").is_some());
    }

    /// Format gates: bad magic; a newer version is refused politely BEFORE
    /// any KDF work; KDF parameters beyond the caps are refused before
    /// allocation; a zero chunk size is implausible.
    #[test]
    fn format_and_kdf_gates_hold() {
        let forge = |header: &serde_json::Value| -> Vec<u8> {
            let hb = serde_json::to_vec(header).expect("json");
            let mut b = EXPORT_MAGIC.to_vec();
            b.extend_from_slice(&u32::try_from(hb.len()).expect("len").to_le_bytes());
            b.extend_from_slice(&hb);
            b.extend_from_slice(&[0u8; 64]); // never reached
            b
        };
        let secret = ExportSecret::Passphrase(PASS.into());
        // bad magic
        let mut bad = forge(&serde_json::json!({}));
        bad[0] ^= 0xff;
        let err = read_export(&mut bad.as_slice(), &secret).expect_err("bad magic");
        assert!(err.to_string().contains("bad magic"), "{err}");
        // newer version → polite NewerVersion refusal, no KDF attempted
        // (the huge m_kib would violate the caps if it were consulted)
        let v2 = forge(&serde_json::json!({
            "format": "molt-export-v1", "version": 2,
            "workspace_id": "00".repeat(32), "key_mode": "passphrase",
            "kdf": { "algo": "argon2id", "m_kib": 8 * 1024 * 1024, "t": 1, "p": 1,
                     "salt": "11".repeat(32) },
            "cipher": "xchacha20poly1305", "chunk_bytes": 4096
        }));
        match read_export(&mut v2.as_slice(), &secret) {
            Err(StorageError::NewerVersion(2)) => {}
            other => panic!("expected NewerVersion(2), got {other:?}", other = other.err()),
        }
        // KDF caps: m_cost over 1 GiB → refusal before any allocation
        let big = forge(&serde_json::json!({
            "format": "molt-export-v1", "version": 1,
            "workspace_id": "00".repeat(32), "key_mode": "passphrase",
            "kdf": { "algo": "argon2id", "m_kib": 2 * 1024 * 1024, "t": 1, "p": 1,
                     "salt": "11".repeat(32) },
            "cipher": "xchacha20poly1305", "chunk_bytes": 4096
        }));
        let err = read_export(&mut big.as_slice(), &secret).expect_err("m_kib cap");
        assert!(err.to_string().contains("import caps"), "{err}");
        // zero chunk size
        let zero = forge(&serde_json::json!({
            "format": "molt-export-v1", "version": 1,
            "workspace_id": "00".repeat(32), "key_mode": "workspace",
            "cipher": "xchacha20poly1305", "chunk_bytes": 0
        }));
        let err = read_export(&mut zero.as_slice(), &ExportSecret::WorkspaceKey([0u8; 32]))
            .expect_err("zero chunk size");
        assert!(err.to_string().contains("chunk size"), "{err}");
    }

    /// Key-hierarchy pin, export side: a dir whose stored seed does not
    /// derive its workspace key must refuse to export at all.
    #[test]
    fn export_refuses_a_dir_with_inconsistent_seed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, _) = make_ws(tmp.path());
        // replace the sealed seed with entropy from a DIFFERENT phrase,
        // validly sealed to the same device key + id
        let other =
            crate::seed_entropy(&crate::generate_seed_phrase().expect("gen")).expect("entropy");
        let id_hex = crate::read_manifest(&dir).expect("manifest").workspace.id;
        let id = crate::id_bytes(&id_hex).expect("id");
        let dk = crate::load_or_create_device_key(&crate::device_key_path(&root)).expect("dk");
        let sealed = crate::seal_seed_entropy(&dk, &id, &other).expect("seal");
        fs::write(dir.join("keys").join("seed.sealed"), sealed).expect("write");
        let mut blob = Vec::new();
        let err = export_dir(&root, &dir, &ExportKey::Passphrase(PASS.into()), &mut blob)
            .expect_err("inconsistent hierarchy must refuse");
        assert!(err.to_string().contains("does not derive"), "{err}");
    }

    /// Key-hierarchy pin, read side: a forged blob whose authenticated meta
    /// carries a seed that does not derive its workspace key is rejected —
    /// and so are traversal entry paths.
    #[test]
    fn read_rejects_inconsistent_hierarchy_and_traversal_paths() {
        let key = [7u8; 32];
        let id_hex = "ab".repeat(32);
        let forge = |meta: &serde_json::Value, entries: &[(&str, &[u8])]| -> Vec<u8> {
            let header = ExportHeader {
                format: "molt-export-v1".to_string(),
                version: 1,
                workspace_id: id_hex.clone(),
                key_mode: "workspace".to_string(),
                kdf: None,
                cipher: "xchacha20poly1305".to_string(),
                chunk_bytes: 4096,
            };
            let hb = serde_json::to_vec(&header).expect("json");
            let id = crate::id_bytes(&id_hex).expect("id");
            let k_root = crate::hkdf32(&key, EXPORT_BACKUP_TAG, &id);
            let k_stream = crate::hkdf32(&k_root, EXPORT_STREAM_TAG, &hb);
            let mut blob = EXPORT_MAGIC.to_vec();
            blob.extend_from_slice(&u32::try_from(hb.len()).expect("len").to_le_bytes());
            blob.extend_from_slice(&hb);
            let mb = serde_json::to_vec(meta).expect("meta json");
            let mut payload =
                u32::try_from(mb.len()).expect("len").to_le_bytes().to_vec();
            payload.extend_from_slice(&mb);
            for (p, d) in entries {
                payload
                    .extend_from_slice(&u16::try_from(p.len()).expect("len").to_le_bytes());
                payload.extend_from_slice(p.as_bytes());
                payload
                    .extend_from_slice(&u64::try_from(d.len()).expect("len").to_le_bytes());
                payload.extend_from_slice(d);
            }
            let mut w = ChunkWriter::new(&mut blob, &k_stream, id, 4096);
            // route via a local Vec: ChunkWriter borrows blob mutably
            w.write(&payload).expect("chunk");
            w.finish().expect("finish");
            blob
        };
        // seed that does NOT derive workspace_key
        let bad_meta = serde_json::json!({
            "created": 1, "exporter": "test", "at_rest": "device",
            "workspace_key": "22".repeat(32), "seed": "33".repeat(32), "files": 0
        });
        let err = read_export(
            &mut forge(&bad_meta, &[]).as_slice(),
            &ExportSecret::WorkspaceKey(key),
        )
        .expect_err("hierarchy violation");
        assert!(err.to_string().contains("key hierarchy"), "{err}");
        // traversal / absolute entry paths
        let plain_meta = serde_json::json!({
            "created": 1, "exporter": "test", "at_rest": "device",
            "workspace_key": "22".repeat(32), "files": 1
        });
        for evil in ["../x", "/etc/passwd", "a//b", "log/../../x", "."] {
            let err = read_export(
                &mut forge(&plain_meta, &[(evil, b"x")]).as_slice(),
                &ExportSecret::WorkspaceKey(key),
            )
            .expect_err("illegal path");
            assert!(
                err.to_string().contains("entry path") || err.to_string().contains("illegal"),
                "path `{evil}`: {err}"
            );
        }
    }
}
