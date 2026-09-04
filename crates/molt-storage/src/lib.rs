// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-storage`: the on-disk reality of a workspace.
//!
//! Implements `docs_archive/storage/concept-workspace-storage.md`: a workspace directory
//! holds a plaintext `manifest.toml` (the identity card the Open screen lists
//! without decrypting), local `prefs.toml`, a sealed workspace key, an
//! **encrypted, append-only event log** in framed segments, and state
//! snapshots. The typed structs in `molt-core` are the schema; this crate is
//! only the I/O.
//!
//! Frame layout (`.mlog` segments; the whole segment is encrypted per frame
//! so appends never rewrite):
//!
//! ```text
//! frame     := len:u32le | crc32c(ciphertext):u32le | nonce:24B | ciphertext
//! plaintext := serde_json(EventEnvelope)
//! aad       := workspace_id(32B) | segment_no:u64le | seq:u64le
//! ```
//!
//! * The CRC is over the **ciphertext** — it exists solely for torn-write /
//!   bitrot detection without decrypting. A plaintext crc would hand an
//!   attacker a confirmation oracle for guessed content.
//! * The AAD binds each frame to its position: an intact frame cannot be
//!   reordered, replayed, or transplanted into another segment or workspace
//!   without the AEAD open failing.
//! * Torn-write recovery: a frame whose `len`/`crc` does not check out marks
//!   the torn tail of the **last** segment — it is truncated to the last
//!   valid frame boundary. The same damage in a *middle* segment is bitrot
//!   and a hard error.
//!
//! Key hierarchy: the recovery seed (real OS-CSPRNG entropy, rendered as a
//! BIP-39 phrase) is the root. `workspace_id = HKDF(seed, "molt-ws-id",
//! member)`, `workspace_key = HKDF(seed, "molt-ws-key", workspace_id)`. The
//! key is stored sealed to a device key (`~/.moltrepublic/device.key`, 0600)
//! so day-to-day opens never touch the seed. The seed entropy itself is also
//! stored device-sealed (`keys/seed.sealed`, own AAD domain — decision
//! 2026-07-15) so the Open screen can show the phrase of an
//! at-rest-unencrypted workspace. The opt-in S6 phrase sealing ([`sealing`])
//! removes BOTH key files: the recovery phrase becomes the only credential
//! (derive-and-verify — no phrase-sealed copy is stored).
//! Honest threat-model note: the device-sealed default protects the synced /
//! backed-up workspace dir, not a fully compromised home directory — that is
//! what the opt-in phrase sealing is for.

pub mod sealing;
pub use sealing::{is_restored, is_sealed, seal_at_rest, unseal_at_rest};

pub mod export;
pub mod import;
mod segkeys;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, VerifyingKey};
pub use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use molt_core::{
    EventEnvelope, ManifestWorkspace, RawEnvelope, TransportState, WorkspaceEvent, WorkspaceId,
    WorkspaceManifest, WorkspacePrefs, WorkspaceSnapshot, MANIFEST_FORMAT, STORAGE_VERSION,
    TRANSPORT_STATE_VERSION,
};
use sha2::Sha256;
use zeroize::Zeroizing;

/// Segment rotation threshold (~8 MiB keeps recovery scans and future
/// S3 diff-uploads bounded).
pub const SEGMENT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Snapshots kept per workspace (newest N; older ones are deleted).
pub const SNAPSHOTS_KEPT: usize = 2;
/// Upper bound a frame's `len` field may claim (corruption guard).
const FRAME_MAX_LEN: u32 = 64 * 1024 * 1024;

/// How many times its own length a damaged segment's tail may be CRC'd
/// before the torn-tail scan gives up (see `has_valid_frame_after`).
const TORN_SCAN_BUDGET_FACTOR: usize = 4;
/// The XChaCha20 nonce size.
const NONCE_LEN: usize = 24;
/// Frame header: len(4) + crc(4).
const FRAME_HEADER_LEN: usize = 8;
/// L8 read caps, derived from the writers' own bounds — the cap is
/// checked on METADATA before any allocation, so a sparse, bit-rotted or
/// hostile file becomes a typed refusal instead of an OOM.
pub(crate) const READ_CAP_KEY: u64 = 4 * 1024; // sealed key/seed blobs, LOCK pids
pub(crate) const READ_CAP_TOML: u64 = 64 * 1024; // manifest.toml / prefs.toml scalars
/// Single-frame state files (chain/transport/keys.state/snapshots): the
/// writer emits exactly one frame, so header + nonce + max frame is a
/// structural ceiling it cannot exceed.
#[allow(clippy::as_conversions)] // usize/u32 → u64 in const context is lossless
pub(crate) const READ_CAP_STATE: u64 =
    (FRAME_HEADER_LEN as u64) + (NONCE_LEN as u64) + (FRAME_MAX_LEN as u64);
/// Log segments rotate BEFORE the append at [`SEGMENT_ROTATE_BYTES`], so
/// an honest segment is bounded by the threshold plus one max frame.
#[allow(clippy::as_conversions)] // usize/u32 → u64 in const context is lossless
pub(crate) const READ_CAP_SEGMENT: u64 =
    SEGMENT_ROTATE_BYTES + (FRAME_HEADER_LEN as u64) + (NONCE_LEN as u64) + (FRAME_MAX_LEN as u64);
pub(crate) const READ_CAP_CONTENT: u64 = 16 * 1024 * 1024; // wiki draft / logo files

/// The ONE sanctioned whole-file read (L8): metadata-checked against the
/// cap before the bytes are touched. An over-cap file surfaces as
/// `InvalidData` so every existing "unreadable" error path applies.
pub(crate) fn read_capped(path: &Path, cap: u64, what: &str) -> std::io::Result<Vec<u8>> {
    let len = fs::metadata(path)?.len();
    if len > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{what} is {len} bytes - beyond the {cap}-byte cap"),
        ));
    }
    fs::read(path) // READ_CAPPED_HELPER
}

/// [`read_capped`] for the two small TOML/text files.
pub(crate) fn read_string_capped(path: &Path, cap: u64, what: &str) -> std::io::Result<String> {
    let len = fs::metadata(path)?.len();
    if len > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{what} is {len} bytes - beyond the {cap}-byte cap"),
        ));
    }
    fs::read_to_string(path) // READ_CAPPED_HELPER
}

/// AAD segment number that marks a snapshot frame (never a real segment).
const SNAPSHOT_SEGMENT: u64 = u64::MAX;
/// AAD segment number that marks the `transport.state` frame.
const TRANSPORT_SEGMENT: u64 = u64::MAX - 1;
/// AAD segment number that marks the `chain.state` frame (the persistent
/// commit-block chain — `docs_archive/chain/persistent_chain.md`).
const CHAIN_SEGMENT: u64 = u64::MAX - 2;
/// AAD segment number that marks the `log/keys.state` frame (the per-segment
/// log key table — WP4a, [`segkeys`]).
const KEYS_SEGMENT: u64 = u64::MAX - 3;

/// The on-disk shape of `chain.state` (WP4b): historically a bare block
/// array; a PRUNED holder stores the checkpoint blob next to its suffix.
/// Untagged: an array parses as `Full`, an object as `Pruned` — old files
/// keep reading, old code meets the unknown `Checkpoint` variant inside a
/// pruned file's blocks and refuses (additive-only rule).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
// the pruned arm carries a whole founding summary and the full arm a Vec; the
// gap grew when the checkpoint gained the relay pool. Boxing would change the
// serde shape of a PERSISTED file for a transient enum that exists only to
// pick a parse — not a trade worth making.
#[allow(clippy::large_enum_variant)]
enum ChainStateFile {
    Pruned {
        checkpoint_blob: molt_core::CheckpointState,
        blocks: Vec<molt_core::ChainBlock>,
    },
    Full(Vec<molt_core::ChainBlock>),
}
/// Group-commit window: fsync at most this often under sustained load.
const GROUP_COMMIT: Duration = Duration::from_millis(50);
/// Bound of the writer queue; a full queue means the disk is falling behind.
const WRITER_QUEUE: usize = 1024;
/// `.trash` entries older than this are purged at startup (30 days).
pub const TRASH_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong between a workspace dir and the engine.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Underlying I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The workspace is locked by another opener.
    #[error("workspace is busy (held by pid {0})")]
    Busy(String),
    /// Structural damage that is not a recoverable torn tail.
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// A cryptographic operation failed (wrong key, tampered frame, …).
    #[error("crypto: {0}")]
    Crypto(String),
    /// The workspace was written by a newer node version.
    #[error("workspace version {0} is newer than this build supports")]
    NewerVersion(u32),
    /// The workspace (carried by id) is phrase-sealed at rest (S6): no key
    /// material on disk — opening is impossible by design, not an I/O
    /// accident. Mapped to [`molt_core::MoltError::WorkspaceEncrypted`] so
    /// every frontend routes it to the decrypt flow.
    #[error("workspace is sealed at rest - decrypt it with its recovery phrase first")]
    Sealed(String),
    /// The recovery phrase did not parse / carry valid entropy.
    #[error("seed: {0}")]
    BadSeed(String),
    /// The target directory already exists.
    #[error("workspace directory already exists: {0}")]
    Exists(PathBuf),
    /// A malformed file that is not the log (manifest, prefs, key file).
    #[error("{0}")]
    BadFile(String),
}

impl StorageError {
    /// Map into the shared [`molt_core::MoltError`] vocabulary.
    pub fn into_molt(self) -> molt_core::MoltError {
        match self {
            StorageError::Busy(pid) => molt_core::MoltError::WorkspaceBusy(pid),
            StorageError::Sealed(id) => molt_core::MoltError::WorkspaceEncrypted(id),
            other => molt_core::MoltError::Storage(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Seed & key hierarchy
// ---------------------------------------------------------------------------

/// Generate a fresh recovery phrase: 256 bits from the OS CSPRNG, rendered
/// as 24 BIP-39 words — the 32-byte root the concept demands, matching the
/// Monero posture (25-word/256-bit seeds) the wallet surface will need
/// anyway. The full key hierarchy expands from this entropy via HKDF.
pub fn generate_seed_phrase() -> Result<String, StorageError> {
    let mut entropy = [0u8; 32];
    getrandom::getrandom(&mut entropy)
        .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
    let m = bip39::Mnemonic::from_entropy(&entropy)
        .map_err(|e| StorageError::BadSeed(e.to_string()))?;
    Ok(m.to_string())
}

/// Parse a recovery phrase back into its seed entropy (checksummed by the
/// BIP-39 wordlist; typos are caught here, not by a failed decrypt later).
pub fn seed_entropy(phrase: &str) -> Result<Vec<u8>, StorageError> {
    let m = bip39::Mnemonic::parse_normalized(phrase.trim())
        .map_err(|e| StorageError::BadSeed(e.to_string()))?;
    Ok(m.to_entropy())
}

/// HKDF-SHA256: 32 bytes from `(ikm, tag || context)`.
fn hkdf32(ikm: &[u8], tag: &str, context: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut info = Vec::with_capacity(tag.len() + context.len());
    info.extend_from_slice(tag.as_bytes());
    info.extend_from_slice(context);
    let mut okm = [0u8; 32];
    hk.expand(&info, &mut okm)
        .expect("32 bytes is a valid HKDF output length");
    okm
}

/// `workspace_id = HKDF(seed, "molt-ws-id", member)` — deterministic, so
/// seed + own handle re-derive the identity; including the member handle
/// gives two local instances of the same republic distinct ids and dirs.
pub fn derive_workspace_id(seed: &[u8], member: &str) -> WorkspaceId {
    hex::encode(hkdf32(seed, "molt-ws-id", member.as_bytes()))
}

/// `workspace_key = HKDF(seed, "molt-ws-key", workspace_id)` (32 B), used
/// with XChaCha20-Poly1305 for frames, snapshots and exports.
pub fn derive_workspace_key(seed: &[u8], id: &str) -> [u8; 32] {
    hkdf32(seed, "molt-ws-key", id.as_bytes())
}

/// The republic's shared id: a **neutral, content-derived** value every
/// member computes identically from the sealed roster. It is the salt the
/// roster table is signed over, and — stored in the `Founded` genesis — the
/// anchor the attestations verify against. Unlike a [`derive_workspace_id`]
/// it depends on **no member's seed**, so the founder is not privileged and
/// every member's local workspace (its own seed, its own id) still verifies
/// the same roster. v2 commits to the full anchor content: each member's
/// `(identity_pk, nostr_pk)` PAIR is hashed as one sorted unit, so the id
/// stays roster-order-independent but anchor pairings cannot be permuted.
///
/// **Every field is le32-length-prefixed and the entry count is hashed**, so
/// the preimage is INJECTIVE for arbitrary field content — v1's 0-separated
/// layout was only injective because every field was hex; the moment a field
/// can carry a NUL (a member supplies its own `nostr_pk`), separators alone
/// let one roster's preimage equal another's with extra identities spliced in,
/// and `republic_id` is the genesis-forgery anchor a pruned-chain holder
/// checks. Validation at ingest is the other half of that defense; this
/// layout does not depend on it.
///
/// `hex(SHA-256("molt-republic-id-v2\0" ‖ le32|name| ‖ name ‖ m ‖ n ‖
/// le32(count) ‖ per pair sorted by identity pk: (le32|identity_pk| ‖
/// identity_pk ‖ le32|nostr_pk| ‖ nostr_pk)))`.
pub fn republic_id(
    name: &str,
    rule_m: u8,
    rule_n: u8,
    identities: &[molt_core::MemberIdentity],
) -> String {
    use sha2::Digest;
    let mut pairs: Vec<(&str, &str)> = identities
        .iter()
        .map(|i| (i.identity_pk.as_str(), i.nostr_pk.as_str()))
        .collect();
    pairs.sort_unstable();
    // the same le32 framing as every other canonical layout (one
    // definition: `molt_core::put_bytes`), built as a preimage buffer and
    // hashed whole — byte-identical to the streamed v2 layout
    let mut pre = Vec::new();
    molt_core::put_bytes(&mut pre, name.as_bytes());
    pre.extend_from_slice(&[rule_m, rule_n]);
    molt_core::put_count(&mut pre, pairs.len());
    for (pk, npk) in pairs {
        molt_core::put_bytes(&mut pre, pk.as_bytes());
        molt_core::put_bytes(&mut pre, npk.as_bytes());
    }
    let h = Sha256::new_with_prefix(b"molt-republic-id-v2\0").chain_update(&pre);
    hex::encode(h.finalize())
}

/// Lowercase-hex SHA-256 of `bytes`. The persistent chain hashes each block's
/// [`molt_core::block_link_bytes`] with this to get the link the *next* block's
/// `prev` points at; the bytes already carry their own domain tag, so no prefix
/// is added here.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(bytes))
}

/// The member's per-workspace **identity keypair** (transport concept
/// §3.3): Ed25519, derived from the member's own recovery seed via the
/// same HKDF hierarchy — per-workspace (keeps the fresh-per-group rule),
/// deterministic (the phrase re-derives it after total loss), never
/// random. Returns `(signing key, public key hex)`.
pub fn derive_identity_key(seed: &[u8], id: &str) -> (SigningKey, String) {
    let sk_bytes = hkdf32(seed, "molt-ws-identity", id.as_bytes());
    let sk = SigningKey::from_bytes(&sk_bytes);
    let pk = hex::encode(sk.verifying_key().to_bytes());
    (sk, pk)
}

/// What a member signs to attest its recovery-phrase BACKUP during the
/// founding (`docs_archive/ritual/seed_backup_confirmation.md` ❻½): a domain tag
/// followed by the sha256 of the RATIFIED canonical table, so the
/// attestation is bound to exactly this ritual's charter and cannot
/// replay into another founding. It deliberately claims no key
/// possession beyond what the ratify signature already proved — it is
/// the second, separate human act ("my phrase is stored").
pub fn backup_confirm_bytes(table: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let mut out = b"molt-backup-confirmed-v1".to_vec();
    out.extend_from_slice(&Sha256::digest(table));
    out
}

/// Sign `msg` with a member's identity key; returns the signature as
/// lowercase hex (64 bytes).
pub fn identity_sign(sk: &SigningKey, msg: &[u8]) -> String {
    hex::encode(sk.sign(msg).to_bytes())
}

/// Verify an identity signature (hex sig, hex pk) over `msg`. False on any
/// malformed input — never panics on untrusted data.
pub fn identity_verify(pk_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Ok(pk_bytes) = hex::decode(pk_hex) else {
        return false;
    };
    let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes.as_slice()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk_arr) else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    vk.verify_strict(msg, &Signature::from_bytes(&sig_arr)).is_ok()
}

/// Decode the hex workspace id into the 32 raw bytes the AAD uses.
fn id_bytes(id: &str) -> Result<[u8; 32], StorageError> {
    let v = hex::decode(id).map_err(|e| StorageError::BadFile(format!("workspace id: {e}")))?;
    <[u8; 32]>::try_from(v.as_slice())
        .map_err(|_| StorageError::BadFile("workspace id is not 32 bytes".to_string()))
}

/// Where the device key lives for a given workspace root
/// (`<root>/../device.key`, i.e. `~/.moltrepublic/device.key` by default).
pub fn device_key_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .parent()
        .unwrap_or(workspace_root)
        .join("device.key")
}

/// Load the device key, creating it (0600) on first use.
pub fn load_or_create_device_key(path: &Path) -> Result<[u8; 32], StorageError> {
    match read_capped(path, READ_CAP_KEY, "device.key") {
        Ok(bytes) => <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            StorageError::BadFile(format!("device key {} is not 32 bytes", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut key = [0u8; 32];
            getrandom::getrandom(&mut key)
                .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(path) {
                Ok(mut f) => {
                    f.write_all(&key)?;
                    f.sync_all()?;
                    Ok(key)
                }
                // lost a creation race: the other creator's key is the key
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    load_or_create_device_key(path)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// `nonce || ciphertext` under `key`, bound to `aad` — the ONE sealing
/// shape of every device-sealed key blob (`keys/workspace.key`,
/// `keys/seed.sealed`). The wrappers below only pick the AAD domain.
fn seal_blob(key: &[u8; 32], aad: &[u8], msg: &[u8], what: &str) -> Result<Vec<u8>, StorageError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg, aad })
        .map_err(|_| StorageError::Crypto(format!("sealing the {what} failed")))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a [`seal_blob`] blob: too short → `BadFile`; not authenticating
/// (foreign device key, tampered, the other AAD domain) → `Crypto`.
fn unseal_blob(key: &[u8; 32], aad: &[u8], blob: &[u8], what: &str) -> Result<Vec<u8>, StorageError> {
    if blob.len() <= NONCE_LEN {
        return Err(StorageError::BadFile(format!("sealed {what} is too short")));
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| {
            StorageError::Crypto(format!("unsealing the {what} failed (wrong device key?)"))
        })
}

/// Seal the workspace key to the device key (the workspace id is the AAD,
/// binding the blob to its workspace).
fn seal_workspace_key(
    device_key: &[u8; 32],
    id: &[u8; 32],
    ws_key: &[u8; 32],
) -> Result<Vec<u8>, StorageError> {
    seal_blob(device_key, id, ws_key, "workspace key")
}

/// Unseal `keys/workspace.key` with the device key.
fn unseal_workspace_key(
    device_key: &[u8; 32],
    id: &[u8; 32],
    blob: &[u8],
) -> Result<[u8; 32], StorageError> {
    let pt = unseal_blob(device_key, id, blob, "workspace key")?;
    <[u8; 32]>::try_from(pt.as_slice())
        .map_err(|_| StorageError::BadFile("unsealed workspace key is not 32 bytes".to_string()))
}

/// The AAD for `keys/seed.sealed` — its own domain (`molt-seed-v1` ‖ id),
/// so the sealed seed and the sealed workspace key can never be swapped
/// for one another on disk.
fn seed_seal_aad(id: &[u8; 32]) -> [u8; 44] {
    let mut aad = [0u8; 44];
    aad[..12].copy_from_slice(b"molt-seed-v1");
    aad[12..].copy_from_slice(id);
    aad
}

/// Seal the recovery-seed entropy to the device key (AAD
/// [`seed_seal_aad`]). Stored so the details panel can show the phrase
/// of an at-rest-unencrypted workspace (decision 2026-07-15); the opt-in
/// passphrase sealing (S6) removes the file.
fn seal_seed_entropy(
    device_key: &[u8; 32],
    id: &[u8; 32],
    entropy: &[u8],
) -> Result<Vec<u8>, StorageError> {
    seal_blob(device_key, &seed_seal_aad(id), entropy, "seed")
}

/// Unseal a `keys/seed.sealed` blob with the device key (AAD
/// [`seed_seal_aad`]). The ONE unseal path for the sealed seed — the phrase
/// readout and the export both go through here, so the wire format never
/// forks (S6 changes it in exactly one place).
fn unseal_seed_entropy(
    device_key: &[u8; 32],
    id: &[u8; 32],
    blob: &[u8],
) -> Result<Vec<u8>, StorageError> {
    unseal_blob(device_key, &seed_seal_aad(id), blob, "seed")
}

/// Read a workspace's recovery phrase back from `keys/seed.sealed`.
/// `None` for anything that isn't a healthy sealed seed — absent file
/// (pre-seed-storage workspace), foreign device key, tampered blob —
/// the Open screen shows an honest "not stored" instead of failing.
pub fn read_sealed_seed(root: &Path, ws_dir: &Path, id_hex: &str) -> Option<String> {
    let blob = match read_capped(&ws_dir.join("keys").join("seed.sealed"), READ_CAP_KEY, "seed.sealed") {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(dir = %ws_dir.display(), error = %e, "sealed seed unreadable");
            return None;
        }
    };
    let id = id_bytes(id_hex).ok()?;
    let device_key = match load_or_create_device_key(&device_key_path(root)) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "device key unavailable for the sealed seed");
            return None;
        }
    };
    let entropy = match unseal_seed_entropy(&device_key, &id, &blob) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %ws_dir.display(), error = %e, "sealed seed unusable");
            return None;
        }
    };
    Some(bip39::Mnemonic::from_entropy(&entropy).ok()?.to_string())
}

/// Decrypt just the genesis (frame 1 of segment 1) of a workspace directory —
/// enough for the Open screen to show a CLOSED workspace's roster and charter
/// without replaying its log or taking its lock. `None` for anything that
/// isn't a healthy readable genesis (foreign device key, corrupt frame, …).
pub fn peek_genesis(root: &Path, ws_dir: &Path, id_hex: &str) -> Option<EventEnvelope> {
    let id = id_bytes(id_hex).ok()?;
    let manifest = read_manifest(ws_dir).ok()?;
    let sealed = read_capped(&ws_dir.join(&manifest.crypto.key_file), READ_CAP_KEY, "workspace key").ok()?;
    let device_key = load_or_create_device_key(&device_key_path(root)).ok()?;
    let key = unseal_workspace_key(&device_key, &id, &sealed).ok()?;
    // the genesis is the log's first frame — while the log still HAS one.
    // WP4a compaction encrypts each segment under its own key and may drop
    // the earliest segments entirely, so try that first, then fall back to
    // the snapshot, which carries every genesis-derived fact by design (it
    // has to: the genesis is before the snapshot and never replayed).
    if let Ok(env) = genesis_frame_at(ws_dir, &key, &id) {
        return Some(env);
    }
    genesis_from_snapshot(ws_dir, &manifest, &key, &id)
}

/// Why the genesis frame (segment 1, seq 1) did not open. The three
/// readers (the Open screen's peek, the phrase check of a seal/unseal, the
/// import's blob check) map it to their own wording.
pub(crate) enum GenesisFault {
    /// The first segment holds no structurally valid frame.
    NoFrame,
    /// Neither key authenticates the frame.
    Auth,
}

/// Decrypt the genesis frame out of a first-segment image (`data` =
/// `log/000001.mlog`, on disk or inside a blob) under the segment's own
/// key, falling back to the workspace key for a half-migrated segment 1 —
/// the same rule `open_workspace`'s replay applies to every segment.
/// Decrypt only: the caller parses (the phrase check never needs to).
pub(crate) fn decrypt_genesis_frame(
    data: &[u8],
    seg_key: &[u8; 32],
    ws_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, GenesisFault> {
    let (frames, _torn) = split_frames(data);
    let first = frames.first().ok_or(GenesisFault::NoFrame)?;
    decrypt_frame(seg_key, id, 1, 1, first.nonce, first.ciphertext)
        .or_else(|e| {
            if seg_key == ws_key {
                Err(e)
            } else {
                decrypt_frame(ws_key, id, 1, 1, first.nonce, first.ciphertext)
            }
        })
        .map(Zeroizing::new)
        .map_err(|_| GenesisFault::Auth)
}

/// The genesis envelope as it lies in a workspace directory's first log
/// segment, under whatever key that segment uses (a compacted log gives
/// it its own DEK). The segment unreadable → `Io`; no frame → `Corrupt`;
/// not authenticating → `Crypto`.
pub(crate) fn genesis_frame_at(
    ws_dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<EventEnvelope, StorageError> {
    let seg_key = segkeys::read_table(ws_dir, ws_key, id)
        .ok()
        .flatten()
        .and_then(|t| t.dek(1))
        .unwrap_or(*ws_key);
    let data = read_capped(&ws_dir.join("log").join(segment_name(1)), READ_CAP_SEGMENT, "log segment")?;
    let plaintext = decrypt_genesis_frame(&data, &seg_key, ws_key, id).map_err(|f| match f {
        GenesisFault::NoFrame => StorageError::Corrupt("workspace has no genesis frame".to_string()),
        GenesisFault::Auth => StorageError::Crypto("the genesis frame does not authenticate".to_string()),
    })?;
    serde_json::from_slice(&plaintext)
        .map_err(|e| StorageError::Corrupt(format!("genesis envelope: {e}")))
}

/// Rebuild the genesis facts from the newest snapshot — the honest source once
/// compaction has dropped the segment the real genesis frame lived in. Every
/// field comes from persisted state (`rule_n` from the manifest's identity
/// card); the attestation set is deliberately empty, exactly as for a
/// checkpoint-recovered workspace: this is display/bootstrap metadata, never
/// consensus input, and the chain holds the authority either way.
fn genesis_from_snapshot(
    ws_dir: &Path,
    manifest: &WorkspaceManifest,
    key: &[u8; 32],
    id: &[u8; 32],
) -> Option<EventEnvelope> {
    let mut snaps = list_sorted(&ws_dir.join("snapshots"), ".msnap");
    snaps.reverse();
    for (at_seq, path) in snaps {
        let Ok(snap) = read_snapshot(key, id, at_seq, &path) else {
            continue;
        };
        return Some(genesis_envelope_of(snap, manifest));
    }
    None
}

/// The genesis envelope a snapshot implies (every genesis-derived fact is
/// carried by design — the genesis itself is before the snapshot and never
/// replayed). Attestations and relays are not in the snapshot: empty.
pub(crate) fn genesis_envelope_of(
    snap: WorkspaceSnapshot,
    manifest: &WorkspaceManifest,
) -> EventEnvelope {
    let st = snap.state;
    EventEnvelope {
        prev_seq: 0,
        seq: 1,
        ts: st.founded_ts,
        by: st.member.clone(),
        body: WorkspaceEvent::Founded {
            name: st.name,
            rule_m: st.rule_m,
            rule_n: manifest.workspace.rule_n,
            member: st.member,
            roster: st.roster,
            identities: st.identities,
            attestations: Vec::new(),
            republic_id: st.republic_id,
            agenda: st.agenda,
            relays: Vec::new(),
            features: st.features,
        },
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// The AAD that binds a frame to `(workspace, segment, seq)`.
fn frame_aad(id: &[u8; 32], segment: u64, seq: u64) -> [u8; 48] {
    let mut aad = [0u8; 48];
    aad[..32].copy_from_slice(id);
    aad[32..40].copy_from_slice(&segment.to_le_bytes());
    aad[40..48].copy_from_slice(&seq.to_le_bytes());
    aad
}

/// Encrypt + frame one plaintext.
fn encode_frame(
    key: &[u8; 32],
    id: &[u8; 32],
    segment: u64,
    seq: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, StorageError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let aad = frame_aad(id, segment, seq);
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| StorageError::Crypto("frame encryption failed".to_string()))?;
    let len = u32::try_from(ct.len())
        .map_err(|_| StorageError::Corrupt("frame over 4 GiB".to_string()))?;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc32c::crc32c(&ct).to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt one frame body back into its plaintext.
fn decrypt_frame(
    key: &[u8; 32],
    id: &[u8; 32],
    segment: u64,
    seq: u64,
    nonce: &[u8],
    ct: &[u8],
) -> Result<Vec<u8>, StorageError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let aad = frame_aad(id, segment, seq);
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &aad })
        .map_err(|_| {
            StorageError::Crypto(format!(
                "frame authentication failed at segment {segment}, seq {seq} \
                 (tampered, transplanted, or wrong key)"
            ))
        })
}

/// Decrypt a single-frame state file (`transport.state`, `chain.state`,
/// `log/keys.state`): exactly one well-formed frame under `key`, at the
/// file's own AAD segment marker and seq 0.
fn decrypt_state_file(
    key: &[u8; 32],
    id: &[u8; 32],
    segment: u64,
    data: &[u8],
) -> Result<Vec<u8>, StorageError> {
    let frame = single_frame(data)
        .ok_or_else(|| StorageError::Corrupt("state-file framing".to_string()))?;
    decrypt_frame(key, id, segment, 0, frame.nonce, frame.ciphertext)
}

/// The one frame of a single-frame state file — `None` for anything else
/// (no frame, several, or a torn tail behind the one).
fn single_frame(data: &[u8]) -> Option<RawFrame<'_>> {
    let (mut frames, torn) = split_frames(data);
    if frames.len() != 1 || torn.is_some() {
        return None;
    }
    frames.pop()
}

/// One structurally valid frame inside a segment buffer.
struct RawFrame<'a> {
    nonce: &'a [u8],
    ciphertext: &'a [u8],
    /// Byte offset just past this frame.
    end: usize,
}

/// Split a segment buffer into structurally valid frames. Returns the frames
/// and, when the buffer does not end on a frame boundary, the offset of the
/// first invalid byte (the torn tail begins there).
/// Does a structurally valid frame begin anywhere after `from`?
///
/// The discriminator between a torn write and in-place corruption (M6). The
/// writer only appends, so a torn append can have nothing behind its partial
/// frame: the file ends there. Anything valid behind the damage means the
/// file was whole, and the maximal-valid-prefix truncation would be
/// destroying acknowledged history rather than recovering a tail.
///
/// The CRC is only computed once a length has passed its sanity check, so
/// random bytes are rejected in a few instructions each; this runs once, on
/// a segment already known to be damaged.
fn has_valid_frame_after(data: &[u8], from: usize) -> bool {
    // BUDGETED (review 2026-08-25 S3): a planted tail whose every offset
    // reads as a plausible length costs one CRC over that length per
    // offset — quadratic, hours on a large segment, under the LOCK. Past a
    // few multiples of the segment the answer is "cannot classify", which
    // the caller treats as the conservative refusal.
    let mut budget = data.len().saturating_mul(TORN_SCAN_BUDGET_FACTOR);
    let mut pos = from.saturating_add(1);
    while pos + FRAME_HEADER_LEN + NONCE_LEN <= data.len() {
        let rest = &data[pos..];
        let len_bytes: [u8; 4] = rest[0..4].try_into().unwrap_or([0; 4]);
        let crc_bytes: [u8; 4] = rest[4..8].try_into().unwrap_or([0; 4]);
        let len = u32::from_le_bytes(len_bytes);
        if len != 0 && len <= FRAME_MAX_LEN {
            if let Ok(len_usize) = usize::try_from(len) {
                let total = FRAME_HEADER_LEN + NONCE_LEN + len_usize;
                if rest.len() >= total {
                    if budget < len_usize {
                        return false;
                    }
                    budget -= len_usize;
                    let ciphertext = &rest[FRAME_HEADER_LEN + NONCE_LEN..total];
                    if crc32c::crc32c(ciphertext) == u32::from_le_bytes(crc_bytes) {
                        return true;
                    }
                }
            }
        }
        pos += 1;
    }
    false
}

fn split_frames(data: &[u8]) -> (Vec<RawFrame<'_>>, Option<usize>) {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let rest = &data[pos..];
        if rest.len() < FRAME_HEADER_LEN + NONCE_LEN {
            return (frames, Some(pos));
        }
        let len_bytes: [u8; 4] = rest[0..4].try_into().unwrap_or([0; 4]);
        let crc_bytes: [u8; 4] = rest[4..8].try_into().unwrap_or([0; 4]);
        let len = u32::from_le_bytes(len_bytes);
        if len == 0 || len > FRAME_MAX_LEN {
            return (frames, Some(pos));
        }
        let Ok(len_usize) = usize::try_from(len) else {
            return (frames, Some(pos));
        };
        let total = FRAME_HEADER_LEN + NONCE_LEN + len_usize;
        if rest.len() < total {
            return (frames, Some(pos));
        }
        let nonce = &rest[FRAME_HEADER_LEN..FRAME_HEADER_LEN + NONCE_LEN];
        let ciphertext = &rest[FRAME_HEADER_LEN + NONCE_LEN..total];
        if crc32c::crc32c(ciphertext) != u32::from_le_bytes(crc_bytes) {
            return (frames, Some(pos));
        }
        pos += total;
        frames.push(RawFrame {
            nonce,
            ciphertext,
            end: pos,
        });
    }
    (frames, None)
}

// ---------------------------------------------------------------------------
// Paths, atomic writes, slug
// ---------------------------------------------------------------------------

/// Expand a leading `~` / `~/` against `$HOME`; leave other paths untouched.
pub fn expand_tilde(input: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if input == "~" {
        if let Some(h) = home {
            return h;
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    PathBuf::from(input)
}

// The slug rule lives in molt-core (shared vocabulary: the GUI previews
// the directory name live); re-exported here so storage callers keep
// their one import path.
pub use molt_core::slugify;

/// The directory name of a workspace: `<slug>.<short-id>`. Display names may
/// repeat; the id disambiguates. Never parsed back.
pub fn workspace_dirname(name: &str, id: &str) -> String {
    format!("{}.{}", slugify(name), &id[..id.len().min(6)])
}

/// Write a file atomically through the workspace's same-fs `tmp/` dir.
fn write_atomic(ws_dir: &Path, rel: &str, bytes: &[u8], mode_600: bool) -> Result<(), StorageError> {
    let tmp_dir = ws_dir.join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join(rel.replace('/', "_"));
    {
        // a FRESH file every time: a leftover (or planted) tmp file would
        // keep its own mode — `mode` applies at creation only — and a
        // symlink there would be followed (review 2026-08-25 S8)
        let _ = fs::remove_file(&tmp);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if mode_600 {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    let target = ws_dir.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&tmp, &target)?;
    // make the rename itself durable: without the parent-dir fsync a power
    // loss can undo the rename even though the data blocks were synced. A
    // FAILED barrier is an error, not a swallowed success — the chain
    // commit's durability promise rests on it (review 2026-08-25 S9)
    if let Some(parent) = target.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Seconds since the Unix epoch (pre-epoch clocks clamp to 0). The one
/// clock helper the storage layer, the engine and the tools share — event
/// timestamps and backup/trash age math must not drift apart.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Manifest & prefs I/O
// ---------------------------------------------------------------------------

/// Read and (leniently) parse a workspace's `manifest.toml`.
pub fn read_manifest(ws_dir: &Path) -> Result<WorkspaceManifest, StorageError> {
    let path = ws_dir.join("manifest.toml");
    let text = read_string_capped(&path, READ_CAP_TOML, "manifest.toml")?;
    let m: WorkspaceManifest = toml::from_str(&text)
        .map_err(|e| StorageError::BadFile(format!("{}: {e}", path.display())))?;
    if m.format != MANIFEST_FORMAT {
        return Err(StorageError::BadFile(format!(
            "{} is not a workspace manifest (format `{}`)",
            path.display(),
            m.format
        )));
    }
    // the key path is joined and, at a seal, zero-filled and unlinked — a
    // manifest is plaintext anyone syncing or importing can write, so only
    // the one canonical location is ever honoured (review 2026-08-25)
    if m.crypto.key_file != molt_core::DEFAULT_KEY_FILE {
        return Err(StorageError::BadFile(format!(
            "{}: key_file must be {}",
            path.display(),
            molt_core::DEFAULT_KEY_FILE
        )));
    }
    Ok(m)
}

fn write_manifest(ws_dir: &Path, m: &WorkspaceManifest) -> Result<(), StorageError> {
    let text = toml::to_string_pretty(m)
        .map_err(|e| StorageError::BadFile(format!("rendering manifest: {e}")))?;
    write_atomic(ws_dir, "manifest.toml", text.as_bytes(), false)
}

/// S7 — the verbatim blob a fetched backup stub holds, relative to its dir.
pub const RESTORED_BLOB_FILE: &str = "restore.molt.enc";

/// S7 (`backup_restore_design.md` §10): land a fetched backup blob as a
/// SEALED stub the Open list shows. The directory holds a minimal manifest
/// (the id pseudonym as its label — the real name is inside the ciphertext,
/// by design) and the blob byte-for-byte as the bucket served it. No key
/// material, no log: opening runs the verified restore pipeline. Refused
/// when any directory already carries this id.
pub fn write_restored_stub(
    root: &Path,
    id: &str,
    ts: u64,
    blob: &[u8],
) -> Result<PathBuf, StorageError> {
    if find_workspace_dir(root, id).is_some() {
        return Err(StorageError::BadFile(format!(
            "workspace {id} already exists locally"
        )));
    }
    let short: String = id.chars().take(12).collect();
    let dir = root.join(format!("restored-{short}"));
    fs::create_dir_all(&dir)?;
    let manifest = WorkspaceManifest {
        format: MANIFEST_FORMAT.to_string(),
        version: STORAGE_VERSION,
        workspace: molt_core::ManifestWorkspace {
            id: id.to_string(),
            name: format!("restored {short}…"),
            created: ts,
            rule_m: 0,
            rule_n: 0,
        },
        crypto: molt_core::CryptoParams {
            sealed: molt_core::SEALED_RESTORED.to_string(),
            ..molt_core::CryptoParams::default()
        },
    };
    write_manifest(&dir, &manifest)?;
    write_atomic(&dir, RESTORED_BLOB_FILE, blob, true)?;
    Ok(dir)
}

/// Read a workspace's `prefs.toml`; a missing or broken file falls back to
/// defaults (prefs are local convenience, never history).
pub fn read_prefs(ws_dir: &Path) -> WorkspacePrefs {
    let path = ws_dir.join("prefs.toml");
    match read_string_capped(&path, READ_CAP_TOML, "prefs.toml") {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => WorkspacePrefs::default(),
    }
}

/// Rewrite a workspace's `prefs.toml` (atomic via `tmp/`).
pub fn write_prefs(ws_dir: &Path, p: &WorkspacePrefs) -> Result<(), StorageError> {
    let text = toml::to_string_pretty(p)
        .map_err(|e| StorageError::BadFile(format!("rendering prefs: {e}")))?;
    write_atomic(ws_dir, "prefs.toml", text.as_bytes(), false)
}

/// Read a workspace's local wiki DRAFT ("" = none): the member's unvoted
/// working copy (`shared_memory_real.md` WP-D). Local convenience like
/// prefs — never history, never exported (the export allowlist does not
/// carry it), sealed at rest with the directory.
pub fn read_wiki_draft(ws_dir: &Path) -> String {
    read_string_capped(&ws_dir.join("wiki_draft.json"), READ_CAP_CONTENT, "wiki draft").unwrap_or_default()
}

/// Rewrite the local wiki draft (atomic via `tmp/`); an empty draft
/// removes the file.
pub fn write_wiki_draft(ws_dir: &Path, draft: &str) -> Result<(), StorageError> {
    if draft.is_empty() {
        match fs::remove_file(ws_dir.join("wiki_draft.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        write_atomic(ws_dir, "wiki_draft.json", draft.as_bytes(), false)
    }
}

// ---------------------------------------------------------------------------
// The LOCK
// ---------------------------------------------------------------------------

/// The held per-workspace flock; dropping it releases the lock.
struct WorkspaceLock {
    _file: File,
}

fn acquire_lock(ws_dir: &Path) -> Result<WorkspaceLock, StorageError> {
    let path = ws_dir.join("LOCK");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            // record the holder for the Busy message of the *next* claimant
            let mut f = &file;
            let _ = rustix::fs::ftruncate(&file, 0);
            let _ = f.write_all(std::process::id().to_string().as_bytes());
            Ok(WorkspaceLock { _file: file })
        }
        Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::AGAIN => {
            let holder = read_string_capped(&path, READ_CAP_KEY, "LOCK").unwrap_or_default();
            let holder = holder.trim();
            Err(StorageError::Busy(if holder.is_empty() {
                "unknown".to_string()
            } else {
                holder.to_string()
            }))
        }
        Err(e) => Err(StorageError::Io(e.into())),
    }
}

// ---------------------------------------------------------------------------
// The opened workspace
// ---------------------------------------------------------------------------

/// What replaying the log (plus the newest snapshot) yields on open. The
/// append position is [`OpenedWorkspace::next_seq`] — the one source of
/// truth for where the log continues.
pub struct LoadedState {
    /// The newest valid snapshot, if any.
    pub snapshot: Option<WorkspaceSnapshot>,
    /// Envelopes with `seq > snapshot.at_seq`, in order.
    pub tail: Vec<EventEnvelope>,
    /// Frames written by a newer node that this build cannot apply. The
    /// frames themselves stay untouched on disk; only this count surfaces.
    /// Non-zero means the caller must not write to this workspace (a
    /// partial history would fork state).
    pub unknown_events: u64,
    /// The compaction floor (WP4a): the highest seq physically dropped from
    /// this log. 0 = the log is complete. A peer whose delivery cursor is at
    /// or below it can no longer be served from the log (§A.1 C2).
    pub compaction_floor: u64,
}

/// An open (locked) workspace directory: the append handle of the active
/// segment plus everything needed to frame, encrypt and rotate.
pub struct OpenedWorkspace {
    dir: PathBuf,
    /// The plaintext identity card.
    pub manifest: WorkspaceManifest,
    /// The local node preferences.
    pub prefs: WorkspacePrefs,
    key: Zeroizing<[u8; 32]>,
    id: [u8; 32],
    _lock: WorkspaceLock,
    /// The per-segment log key table (WP4a §A.3), once this workspace has
    /// been compacted at least once. `None` = never compacted: every segment
    /// is under the workspace key, exactly the pre-WP4a shape.
    seg_keys: Option<segkeys::SegmentKeyTable>,
    seg_no: u64,
    seg: File,
    seg_len: u64,
    /// The next seq this log expects (strictly monotonic).
    pub next_seq: u64,
    /// Unsynced appends are pending.
    dirty: bool,
    /// `transport.state` as last decoded or written through this handle
    /// (S11): the file carries the whole MLS ratchet blob, and the
    /// supervisor's frequent cursor saves each read-modify-write it — one
    /// decrypt per open, not per save. `None` = not read yet, or the last
    /// write failed (the file's content is then unknown). Sound because
    /// the flock makes this handle the only writer of the file.
    transport: std::sync::Mutex<Option<TransportState>>,
}

impl OpenedWorkspace {
    /// The workspace directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one envelope to the active segment (rotating first when the
    /// segment is full). The write is buffered by the OS; call [`Self::sync`]
    /// (or let the writer task group-commit) to make it durable.
    pub fn append(&mut self, env: &EventEnvelope) -> Result<(), StorageError> {
        if env.seq != self.next_seq {
            return Err(StorageError::Corrupt(format!(
                "append out of order: got seq {}, expected {}",
                env.seq, self.next_seq
            )));
        }
        if self.seg_len >= SEGMENT_ROTATE_BYTES {
            self.rotate()?;
        }
        let plaintext = serde_json::to_vec(env)
            .map_err(|e| StorageError::Corrupt(format!("encoding envelope: {e}")))?;
        let frame = encode_frame(
            &self.segment_key(self.seg_no),
            &self.id,
            self.seg_no,
            env.seq,
            &plaintext,
        )?;
        self.seg.write_all(&frame)?;
        self.seg_len += u64::try_from(frame.len()).unwrap_or(u64::MAX);
        self.next_seq += 1;
        self.dirty = true;
        Ok(())
    }

    /// The key one segment's frames are encrypted under: its own data key
    /// once the log has been migrated (WP4a §A.3), else the workspace key —
    /// which is where every segment of a never-compacted workspace lives.
    fn segment_key(&self, no: u64) -> [u8; 32] {
        self.seg_keys
            .as_ref()
            .and_then(|t| t.dek(no))
            .unwrap_or(*self.key)
    }

    /// Decrypt one log frame, tolerating a **half-finished migration**: the
    /// table gets its DEKs before the segment files are rewritten (losing the
    /// key of an already-rewritten segment would lose the log), so a crash in
    /// between leaves segments that are still under the workspace key while
    /// the table already names a DEK. Trying the DEK first and the workspace
    /// key second makes the migration crash-safe and repeatable; a genuinely
    /// bad frame still fails, with the DEK error (the expected one).
    fn decrypt_log_frame(
        &self,
        no: u64,
        seq: u64,
        nonce: &[u8],
        ct: &[u8],
    ) -> Result<Vec<u8>, StorageError> {
        let dek = self.seg_keys.as_ref().and_then(|t| t.dek(no));
        match dek {
            Some(dek) => match decrypt_frame(&dek, &self.id, no, seq, nonce, ct) {
                Ok(p) => Ok(p),
                Err(e) => decrypt_frame(&self.key, &self.id, no, seq, nonce, ct).map_err(|_| e),
            },
            None => decrypt_frame(&self.key, &self.id, no, seq, nonce, ct),
        }
    }

    /// The compaction floor: the highest seq physically dropped (0 = the log
    /// is complete).
    pub fn compaction_floor(&self) -> u64 {
        self.seg_keys.as_ref().map(|t| t.floor).unwrap_or(0)
    }

    /// **F6 migration: put every existing segment under its own data key.**
    /// Idempotent and crash-safe — the table (with the new keys AND each
    /// segment's first seq) is written first, then each segment is rewritten
    /// via `tmp/` + rename; a crash leaves a segment readable under either
    /// key ([`Self::decrypt_log_frame`]) and the next run finishes the job.
    /// Runs once, before the first drop: from then on there is exactly ONE
    /// deletion class (erase the key, unlink the file).
    pub fn migrate_to_segment_keys(&mut self) -> Result<(), StorageError> {
        // the active segment is rewritten from what is ON DISK, so any
        // buffered append has to be there first
        self.sync()?;
        let mut table = match self.seg_keys.take() {
            Some(t) => t,
            None => segkeys::SegmentKeyTable::new(),
        };
        // 1) every segment gets an entry (number, first seq, fresh key).
        // Once the table exists, "no key" below its highest entry means
        // ERASED by a drop, not unmigrated: a dropped file that reappears
        // (an unlink error, a sync tool restoring it) must not be minted a
        // fresh key and a bogus first seq — that poisoned the table and the
        // next open failed on it (review 2026-08-25 S2)
        let highest_keyed = table.highest_no();
        let segments = list_sorted(&self.dir.join("log"), ".mlog");
        let mut seq = table.floor;
        for (no, path) in &segments {
            if table.dek(*no).is_none() && highest_keyed.is_some_and(|h| *no < h) {
                tracing::warn!(segment = no, "an erased segment's file is back, not resurrecting it");
                continue;
            }
            let first_seq = seq + 1;
            if table.dek(*no).is_none() {
                table.put(segkeys::SegmentKey {
                    no: *no,
                    first_seq,
                    dek: segkeys::SegmentKeyTable::fresh_dek()?,
                });
            }
            let data = read_capped(path, READ_CAP_SEGMENT, "log segment")?;
            let (frames, _torn) = split_frames(&data);
            seq += u64::try_from(frames.len()).unwrap_or(0);
        }
        segkeys::write_table(&self.dir, &self.key, &self.id, &table)?;
        self.seg_keys = Some(table);

        // 2) rewrite each segment under its key (skipping ones already done)
        for (no, path) in &segments {
            if highest_keyed.is_some_and(|h| *no < h)
                && self.seg_keys.as_ref().and_then(|t| t.dek(*no)).is_none()
            {
                continue; // erased, see above
            }
            let data = read_capped(path, READ_CAP_SEGMENT, "log segment")?;
            let (frames, torn) = split_frames(&data);
            if torn.is_some() {
                return Err(StorageError::Corrupt(format!(
                    "segment {} has a torn tail - not migrating a damaged log",
                    path.display()
                )));
            }
            let Some(dek) = self.seg_keys.as_ref().and_then(|t| t.dek(*no)) else {
                continue;
            };
            let first_seq = self
                .seg_keys
                .as_ref()
                .and_then(|t| t.first_seq(*no))
                .unwrap_or(1);
            let mut out = Vec::with_capacity(data.len());
            let mut migrated = false;
            for (i, frame) in frames.iter().enumerate() {
                let seq = first_seq + u64::try_from(i).unwrap_or(0);
                // already under the DEK? then this segment is done
                if decrypt_frame(&dek, &self.id, *no, seq, frame.nonce, frame.ciphertext).is_ok() {
                    continue;
                }
                let plaintext =
                    decrypt_frame(&self.key, &self.id, *no, seq, frame.nonce, frame.ciphertext)?;
                out.extend_from_slice(&encode_frame(&dek, &self.id, *no, seq, &plaintext)?);
                migrated = true;
            }
            if !migrated {
                continue; // this segment was already rewritten
            }
            let rel = format!("log/{}", segment_name(*no));
            write_atomic(&self.dir, &rel, &out, false)?;
            // the ACTIVE segment's append handle now points at the replaced
            // file — reopen it, or the next append would write into an
            // unlinked inode and vanish
            if *no == self.seg_no {
                self.seg = OpenOptions::new().append(true).open(self.dir.join(&rel))?;
                self.seg_len = u64::try_from(out.len()).unwrap_or(self.seg_len);
            }
        }
        // the log is now under per-segment keys, which a pre-WP4a binary
        // cannot read: raise the version gate NOW, not only at the first
        // drop, or such a binary reports "corrupt" instead of "newer"
        self.bump_pruned_version()?;
        Ok(())
    }

    /// **Drop every segment that lies entirely at or below `floor`** — erase
    /// its data key, then unlink the file (WP4a §A.4 order: the key first, so
    /// a crash cannot leave readable bytes without a key entry). Partially
    /// covered segments are never touched: compaction drops whole segments,
    /// it never rewrites one. The active segment is never dropped.
    ///
    /// Requires the F6 migration (a log still under the workspace key would
    /// leave decryptable bytes behind on an unlink); it is a no-op otherwise.
    /// Returns how many segments went.
    pub fn drop_segments_below(&mut self, floor: u64) -> Result<usize, StorageError> {
        let Some(mut table) = self.seg_keys.clone() else {
            return Ok(0);
        };
        let segments = list_sorted(&self.dir.join("log"), ".mlog");
        // (segment, its file, its last seq)
        let mut doomed: Vec<(u64, PathBuf, u64)> = Vec::new();
        for (idx, (no, path)) in segments.iter().enumerate() {
            if *no == self.seg_no {
                continue;
            }
            // the segment's last seq = the next segment's first_seq - 1
            let Some(first) = table.first_seq(*no) else {
                continue;
            };
            let next_first = segments
                .get(idx + 1)
                .and_then(|(n, _)| table.first_seq(*n))
                .unwrap_or(self.next_seq);
            let last = next_first.saturating_sub(1);
            if last <= floor && first <= last {
                doomed.push((*no, path.clone(), last));
            }
        }
        if doomed.is_empty() {
            return Ok(0);
        }
        let dropped_to = doomed
            .iter()
            .map(|(_, _, last)| *last)
            .max()
            .unwrap_or(table.floor);
        for (no, _, _) in &doomed {
            table.forget(*no);
        }
        table.floor = table.floor.max(dropped_to);
        // 1) the keys are gone and durable …
        segkeys::write_table(&self.dir, &self.key, &self.id, &table)?;
        self.seg_keys = Some(table);
        // 2) … only then the bytes (a crash in between leaves files nobody
        //    can read, which the next run unlinks)
        for (_, path, _) in &doomed {
            let _ = fs::remove_file(path);
        }
        Ok(doomed.len())
    }

    /// Unlink log segments the table has no key for — the crash-recovery half
    /// of [`Self::drop_segments_below`] (§A.4: orphans under the floor are
    /// cleaned by the next run). Their bytes are already worthless.
    fn sweep_keyless_segments(&self) -> usize {
        let Some(table) = self.seg_keys.as_ref() else {
            return 0;
        };
        let mut swept = 0;
        for (no, path) in list_sorted(&self.dir.join("log"), ".mlog") {
            if table.dek(no).is_none() && fs::remove_file(&path).is_ok() {
                tracing::info!(segment = no, "swept a log segment whose key was erased");
                swept += 1;
            }
        }
        swept
    }

    /// fsync the active segment (the group-commit point).
    pub fn sync(&mut self) -> Result<(), StorageError> {
        if self.dirty {
            self.seg.sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), StorageError> {
        self.sync()?;
        let no = self.seg_no.saturating_add(1);
        // a compacted log gives every segment its own key — minted and made
        // DURABLE before the first frame lands in it, or a crash would leave
        // frames nobody holds a key for (unrecoverable, unlike the reverse)
        if let Some(table) = self.seg_keys.clone() {
            let mut table = table;
            table.put(segkeys::SegmentKey {
                no,
                first_seq: self.next_seq,
                dek: segkeys::SegmentKeyTable::fresh_dek()?,
            });
            segkeys::write_table(&self.dir, &self.key, &self.id, &table)?;
            self.seg_keys = Some(table);
        }
        self.seg_no = no;
        let path = self.dir.join("log").join(segment_name(self.seg_no));
        self.seg = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)?;
        self.seg_len = 0;
        Ok(())
    }

    /// Write a state snapshot (atomic via `tmp/` + rename; a torn snapshot
    /// never shadows an older valid one), then prune to the newest
    /// [`SNAPSHOTS_KEPT`].
    pub fn write_snapshot(&mut self, snap: &WorkspaceSnapshot) -> Result<(), StorageError> {
        let plaintext = serde_json::to_vec(snap)
            .map_err(|e| StorageError::Corrupt(format!("encoding snapshot: {e}")))?;
        let frame = encode_frame(&self.key, &self.id, SNAPSHOT_SEGMENT, snap.at_seq, &plaintext)?;
        let rel = format!("snapshots/{:012}.msnap", snap.at_seq);
        write_atomic(&self.dir, &rel, &frame, false)?;
        self.prune_snapshots();
        Ok(())
    }

    fn prune_snapshots(&self) {
        let mut files = list_sorted(&self.dir.join("snapshots"), ".msnap");
        files.reverse(); // newest first
        for (_, path) in files.into_iter().skip(SNAPSHOTS_KEPT) {
            let _ = fs::remove_file(path);
        }
    }

    /// Rewrite the manifest's display name (atomic; no-op when unchanged).
    /// The directory name deliberately stays — it is never parsed back.
    pub fn set_display_name(&mut self, name: &str) -> Result<(), StorageError> {
        if self.manifest.workspace.name == name {
            return Ok(());
        }
        self.manifest.workspace.name = name.to_string();
        write_manifest(&self.dir, &self.manifest)
    }

    /// Reconcile the workspace's `logo.<ext>` file with the applied image
    /// state: write the wanted bytes (atomic, skipped when identical),
    /// remove every other `logo.*`. `None` removes the logo entirely.
    pub fn set_logo(&mut self, logo: Option<(String, Vec<u8>)>) -> Result<(), StorageError> {
        let want_name = logo.as_ref().map(|(ext, _)| format!("logo.{ext}"));
        // drop stale logo files (an older extension, or all of them)
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("logo.") && Some(&name) != want_name.as_ref() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        if let (Some(name), Some((_, bytes))) = (want_name, logo) {
            let path = self.dir.join(&name);
            if read_capped(&path, READ_CAP_CONTENT, "logo").is_ok_and(|have| have == bytes) {
                return Ok(()); // already materialized
            }
            write_atomic(&self.dir, &name, &bytes, false)?;
        }
        Ok(())
    }

    /// The per-member twin of [`WorkspaceStore::set_logo`]: reconcile ONE
    /// member's `avatar-<stem>.<ext>` with its applied profile picture,
    /// dropping any other file of that stem. `None` removes it. The stem
    /// is the engine's stable per-member name — nothing here parses it back.
    pub fn set_avatar(
        &mut self,
        stem: &str,
        avatar: Option<(String, Vec<u8>)>,
    ) -> Result<(), StorageError> {
        // the stem lands in a file name: keep the path-escape door shut here
        // too, not only at the (single, sanitizing) caller
        if stem.is_empty() || !stem.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(StorageError::BadFile(format!("bad avatar name: {stem}")));
        }
        let prefix = format!("avatar-{stem}.");
        let want_name = avatar.as_ref().map(|(ext, _)| format!("{prefix}{ext}"));
        // drop this member's stale avatar files (an older extension, or all)
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) && Some(&name) != want_name.as_ref() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        if let (Some(name), Some((_, bytes))) = (want_name, avatar) {
            let path = self.dir.join(&name);
            if read_capped(&path, READ_CAP_CONTENT, "avatar").is_ok_and(|have| have == bytes) {
                return Ok(()); // already materialized
            }
            write_atomic(&self.dir, &name, &bytes, false)?;
        }
        Ok(())
    }

    /// Persist new prefs for this workspace.
    pub fn set_prefs(&mut self, p: WorkspacePrefs) -> Result<(), StorageError> {
        write_prefs(&self.dir, &p)?;
        self.prefs = p;
        Ok(())
    }

    /// Read `transport.state` (transport concept §6): node-local encrypted
    /// transport bookkeeping. Absent, damaged or newer-versioned files fall
    /// back to defaults. For the resendable state (cursors, ratchets, queue
    /// credentials) that costs resends/re-negotiation, never history — but
    /// since N1 (v3) the file may also hold `nostr_sk`, the seat's
    /// NON-re-derivable transport secret (its salting ticket died with the
    /// ritual), so a fallback on an EXISTING file is a loud, named loss,
    /// not a shrug (see [`read_transport_state_at`]). Decoded once per
    /// handle (S11), then served from the cache every write keeps current.
    pub fn read_transport_state(&self) -> TransportState {
        let mut cache = self.transport.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get_or_insert_with(|| read_transport_state_at(&self.dir, &self.key, &self.id))
            .clone()
    }

    /// Rewrite `transport.state` atomically (via `tmp/`, mode 0600), old
    /// content discarded — this file must never accrete history (from T2
    /// it holds ratchet state whose deletion IS forward secrecy).
    pub fn write_transport_state(&self, state: &TransportState) -> Result<(), StorageError> {
        // the lock spans the write: cache and file move together
        let mut cache = self.transport.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match write_transport_state_at(&self.dir, &self.key, &self.id, state) {
            Ok(written) => {
                *cache = Some(written);
                Ok(())
            }
            Err(e) => {
                // what the file holds now is unknown — the next read decodes it
                *cache = None;
                Err(e)
            }
        }
    }

    /// Read `chain.state`: the republic's persistent commit-block chain
    /// (`docs_archive/chain/persistent_chain.md`). Absent → `Ok(empty)` (a
    /// pre-chain or freshly-founded-before-write workspace). PRESENT but
    /// unreadable → a typed error (L7): a damaged chain read as "no chain"
    /// made the open run a chain republic CHAINLESS on the legacy counted
    /// path, and the next governance write then overwrote the damaged
    /// file — destroying the evidence. Same policy as
    /// `sealing::chain_version_floor` and the import path: on this file,
    /// present-but-unreadable is the dangerous case, never "absent".
    pub fn read_chain(
        &self,
    ) -> Result<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>), StorageError>
    {
        read_chain_at(&self.dir, &self.key, &self.id)
    }

    /// WP4b stage 5: raise the manifest to [`molt_core::STORAGE_VERSION_PRUNED`]
    /// once — the additive-only stop for older binaries (they refuse the
    /// whole workspace at the manifest gate instead of running chainless
    /// on a partial view). Idempotent.
    pub fn bump_pruned_version(&mut self) -> Result<(), StorageError> {
        if self.manifest.version >= molt_core::STORAGE_VERSION_PRUNED {
            return Ok(());
        }
        self.manifest.version = molt_core::STORAGE_VERSION_PRUNED;
        write_manifest(&self.dir, &self.manifest)
    }

    /// Rewrite `chain.state` atomically (via `tmp/`, mode 0600). The chain is
    /// append-only in meaning but written whole each time (it is small — one
    /// block per committed governance change, not per message).
    pub fn write_chain(
        &self,
        blob: Option<&molt_core::CheckpointState>,
        chain: &[molt_core::ChainBlock],
    ) -> Result<(), StorageError> {
        let plaintext = match blob {
            // WP4b: a pruned holder persists the checkpoint blob next to
            // its suffix. A FULL chain keeps the bare-array layout, so
            // pre-checkpoint files and unpruned republics stay byte-shaped
            // as before (additive rule).
            Some(blob) => serde_json::to_vec(&ChainStateFile::Pruned {
                checkpoint_blob: blob.clone(),
                blocks: chain.to_vec(),
            }),
            None => serde_json::to_vec(chain),
        }
        .map_err(|e| StorageError::Corrupt(format!("encoding chain.state: {e}")))?;
        let frame = encode_frame(
            &chain_state_key(&self.key, &self.id),
            &self.id,
            CHAIN_SEGMENT,
            0,
            &plaintext,
        )?;
        write_atomic(&self.dir, "chain.state", &frame, true)
    }

    /// Read every envelope with `seq >= from_seq` from the log — the
    /// log-backed outbox source (transport concept §2). Seq is positional
    /// (frame k of the whole log is seq k), so frames before `from_seq`
    /// are counted but not decrypted. Reads on the writer thread are
    /// consistently ordered with its own appends (same handle, page cache
    /// — no fsync needed to see them).
    pub fn read_log_from(&self, from_seq: u64) -> Result<Vec<EventEnvelope>, StorageError> {
        // the common wakeup finds the cursor at the tip — answer without
        // touching a single segment (the full scan below is O(log bytes);
        // a per-segment seq index for long logs is T3 work)
        if from_seq >= self.next_seq {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        // seq of the previous frame; current = seq + 1. On a compacted log the
        // first surviving segment starts above 1 and says so.
        let mut seq: u64 = self.compaction_floor();
        for (seg_no, path) in list_sorted(&self.dir.join("log"), ".mlog") {
            if let Some(table) = self.seg_keys.as_ref() {
                if table.dek(seg_no).is_none() {
                    continue; // dropped, awaiting the unlink sweep
                }
                if let Some(first) = table.first_seq(seg_no) {
                    seq = first.saturating_sub(1);
                }
            }
            let data = read_capped(&path, READ_CAP_SEGMENT, "log segment")?;
            let (frames, _torn) = split_frames(&data);
            for frame in frames {
                seq += 1;
                if seq < from_seq {
                    continue;
                }
                let plaintext =
                    self.decrypt_log_frame(seg_no, seq, frame.nonce, frame.ciphertext)?;
                match serde_json::from_slice::<EventEnvelope>(&plaintext) {
                    Ok(env) => out.push(env),
                    Err(_) => {
                        // an event from a newer node: it stays on disk and
                        // is not ours to fan out
                        tracing::warn!(seq, "skipping an unknown event in the outbox read");
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The `chain.state` sub-key (its own HKDF tag, distinct from the
/// transport key).
fn chain_state_key(ws_key: &[u8; 32], id: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(hkdf32(ws_key, "molt-chain-state", id))
}

/// The `transport.state` sub-key.
fn transport_state_key(ws_key: &[u8; 32], id: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(hkdf32(ws_key, "molt-transport-state", id))
}

/// Why a `chain.state` image did not yield a chain. The three policies map
/// it themselves: the open (typed errors, [`read_chain_at`]), the unseal's
/// version floor (every fault is the conservative PRUNED answer) and the
/// import (hard errors in the blob's wording).
pub(crate) enum ChainStateFault {
    /// Not exactly one well-formed frame.
    Framing,
    /// The frame does not authenticate under the chain sub-key.
    Auth(StorageError),
    /// The plaintext is no chain-state layout this build knows.
    Decode(serde_json::Error),
}

/// Decrypt + parse one `chain.state` image (`data` = the file's bytes, or
/// a blob entry's) into `(checkpoint, blocks)` — the ONE decoder of the
/// [`ChainStateFile`] layout.
pub(crate) fn decode_chain_state(
    ws_key: &[u8; 32],
    id: &[u8; 32],
    data: &[u8],
) -> Result<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>), ChainStateFault> {
    let frame = single_frame(data).ok_or(ChainStateFault::Framing)?;
    let plaintext = decrypt_frame(
        &chain_state_key(ws_key, id),
        id,
        CHAIN_SEGMENT,
        0,
        frame.nonce,
        frame.ciphertext,
    )
    .map(Zeroizing::new)
    .map_err(ChainStateFault::Auth)?;
    match serde_json::from_slice::<ChainStateFile>(&plaintext) {
        Ok(ChainStateFile::Full(chain)) => Ok((None, chain)),
        Ok(ChainStateFile::Pruned {
            checkpoint_blob,
            blocks,
        }) => Ok((Some(checkpoint_blob), blocks)),
        Err(e) => Err(ChainStateFault::Decode(e)),
    }
}

/// Read the `chain.state` of a workspace directory: absent → `Ok((None,
/// []))`; present but unreadable → a typed error (the L7 policy, see
/// [`OpenedWorkspace::read_chain`]).
pub(crate) fn read_chain_at(
    dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<(Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>), StorageError> {
    let data = match read_capped(&dir.join("chain.state"), READ_CAP_STATE, "chain.state") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, Vec::new())),
        Err(e) => return Err(StorageError::Corrupt(format!("reading chain.state: {e}"))),
    };
    decode_chain_state(ws_key, id, &data).map_err(|f| match f {
        ChainStateFault::Framing => {
            StorageError::Corrupt("chain.state framing is damaged".to_string())
        }
        ChainStateFault::Auth(e) => {
            StorageError::Crypto(format!("chain.state does not authenticate: {e}"))
        }
        ChainStateFault::Decode(e) => StorageError::Corrupt(format!("chain.state decode: {e}")),
    })
}

/// Why a `transport.state` did not yield a state. Two policies read it:
/// the open gate refuses only `Newer` ([`open_transport_state`]); the read
/// path starts fresh on everything, loudly on everything but `Absent`
/// ([`read_transport_state_at`]).
enum TransportStateFault {
    /// No file — a fresh workspace.
    Absent,
    Read(std::io::Error),
    /// Not exactly one well-formed frame.
    Framing,
    Auth(StorageError),
    /// Written by a newer build than this one.
    Newer(u32),
    Decode(serde_json::Error),
}

/// Decode a workspace directory's `transport.state`, every fault typed —
/// the ONE reader behind both policies.
fn read_transport_state_raw(
    dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
) -> Result<TransportState, TransportStateFault> {
    let data = match read_capped(&dir.join("transport.state"), READ_CAP_STATE, "transport.state") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(TransportStateFault::Absent),
        Err(e) => return Err(TransportStateFault::Read(e)),
    };
    let frame = single_frame(&data).ok_or(TransportStateFault::Framing)?;
    let plaintext = decrypt_frame(
        &transport_state_key(ws_key, id),
        id,
        TRANSPORT_SEGMENT,
        0,
        frame.nonce,
        frame.ciphertext,
    )
    .map_err(TransportStateFault::Auth)?;
    match serde_json::from_slice::<TransportState>(&plaintext) {
        Ok(st) if st.version <= TRANSPORT_STATE_VERSION => Ok(st),
        Ok(st) => Err(TransportStateFault::Newer(st.version)),
        Err(e) => Err(TransportStateFault::Decode(e)),
    }
}

/// Read a `transport.state` for a workspace directory — shared by
/// [`OpenedWorkspace::read_transport_state`] and the import commit (which carries
/// the replaced dir's non-re-derivable `nostr_sk` over). An ABSENT file is
/// silently the default (a fresh workspace has none). A file that EXISTS
/// but cannot be read (damaged framing, failed authentication, decode
/// error, newer version) also falls back to the default — but LOUDLY, at
/// error level, naming what may be lost: since v3 the file can hold the
/// seat's nostr transport secret, which no phrase or seed re-derives (the
/// salting ticket died with the founding ritual), so "starting fresh" is
/// only harmless for the resendable state (cursors, ratchets, creds), not
/// for that identity. Honesty is the whole fix here: the fallback behavior
/// is unchanged, the silence is not.
fn read_transport_state_at(dir: &Path, ws_key: &[u8; 32], id: &[u8; 32]) -> TransportState {
    // starting fresh loses the non-re-derivable nostr_sk until a recovery
    // ritual re-anchors the seat; the fields keep the log line greppable
    const LOST: &str = "transport.state unreadable, starting fresh";
    match read_transport_state_raw(dir, ws_key, id) {
        Ok(st) => return st,
        Err(TransportStateFault::Absent) => {}
        Err(TransportStateFault::Read(e)) => {
            tracing::error!(error = %e, cause = "read", loses = "nostr_sk", "{LOST}");
        }
        Err(TransportStateFault::Framing) => {
            tracing::error!(cause = "framing", loses = "nostr_sk", "{LOST}");
        }
        Err(TransportStateFault::Auth(e)) => {
            tracing::error!(error = %e, cause = "auth", loses = "nostr_sk", "{LOST}");
        }
        Err(TransportStateFault::Newer(version)) => {
            tracing::error!(version, cause = "newer", loses = "nostr_sk", "{LOST}");
        }
        Err(TransportStateFault::Decode(e)) => {
            tracing::error!(error = %e, cause = "decode", loses = "nostr_sk", "{LOST}");
        }
    }
    TransportState::default()
}

/// Write a `transport.state` for a workspace directory that is not (yet)
/// open — shared by [`OpenedWorkspace::write_transport_state`] and the
/// import commit (which writes the fresh minimal identity-only state into
/// its staging dir before the rename). Same framing, same sub-key
/// derivation, atomic via `tmp/`, mode 0600. Returns the state as written
/// (this build's version stamped), for the handle's cache.
fn write_transport_state_at(
    dir: &Path,
    ws_key: &[u8; 32],
    id: &[u8; 32],
    state: &TransportState,
) -> Result<TransportState, StorageError> {
    let mut state = state.clone();
    state.version = TRANSPORT_STATE_VERSION;
    let plaintext = serde_json::to_vec(&state)
        .map_err(|e| StorageError::Corrupt(format!("encoding transport.state: {e}")))?;
    let frame = encode_frame(
        &transport_state_key(ws_key, id),
        id,
        TRANSPORT_SEGMENT,
        0,
        &plaintext,
    )?;
    write_atomic(dir, "transport.state", &frame, true)?;
    Ok(state)
}

/// `000042.mlog`-style segment file name.
fn segment_name(no: u64) -> String {
    format!("{no:06}.mlog")
}

/// Numeric-sorted `(number, path)` list of files with `ext` in `dir`.
fn list_sorted(dir: &Path, ext: &str) -> Vec<(u64, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(ext) {
            if let Ok(no) = stem.parse::<u64>() {
                // the top numbers are the reserved AAD markers (snapshot,
                // transport, chain, keys): a planted file up there would
                // become the active segment under a marker's domain, and
                // its successor number overflows — ignore it (review S5)
                if no >= KEYS_SEGMENT {
                    tracing::warn!(path = %path.display(), "ignoring a file with a reserved number");
                    continue;
                }
                out.push((no, path));
            }
        }
    }
    out.sort_by_key(|(no, _)| *no);
    out
}

// ---------------------------------------------------------------------------
// Create & open
// ---------------------------------------------------------------------------

/// Materialize a new workspace directory under `root` from its genesis
/// event: manifest, prefs, sealed key, and a log whose frame 1 is the
/// `Founded` genesis — the log is never empty. The directory is built in a
/// same-fs staging dir and atomically renamed into place. Returns the
/// workspace opened (locked, ready to append seq 2).
pub fn create_workspace(
    root: &Path,
    seed: &[u8],
    genesis: &EventEnvelope,
) -> Result<OpenedWorkspace, StorageError> {
    let WorkspaceEvent::Founded {
        name,
        rule_m,
        rule_n,
        member,
        ..
    } = &genesis.body
    else {
        return Err(StorageError::Corrupt(
            "the genesis event must be Founded".to_string(),
        ));
    };
    if genesis.seq != 1 {
        return Err(StorageError::Corrupt("genesis must be seq 1".to_string()));
    }

    let id_hex = derive_workspace_id(seed, member);
    let key = derive_workspace_key(seed, &id_hex);
    let id = id_bytes(&id_hex)?;

    fs::create_dir_all(root)?;
    let dirname = workspace_dirname(name, &id_hex);
    let final_dir = root.join(&dirname);
    if final_dir.exists() {
        return Err(StorageError::Exists(final_dir));
    }

    // stage under a dot-name (the Open scan skips dot-entries), same fs
    let staging = root.join(format!(".create-{dirname}"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for sub in ["keys", "log", "snapshots", "tmp"] {
        fs::create_dir_all(staging.join(sub))?;
    }

    let manifest = WorkspaceManifest {
        format: MANIFEST_FORMAT.to_string(),
        version: STORAGE_VERSION,
        workspace: ManifestWorkspace {
            id: id_hex.clone(),
            name: name.clone(),
            created: genesis.ts,
            rule_m: *rule_m,
            rule_n: *rule_n,
        },
        crypto: molt_core::CryptoParams::default(),
    };
    write_manifest(&staging, &manifest)?;
    let prefs = WorkspacePrefs::default();
    write_prefs(&staging, &prefs)?;

    let device_key = load_or_create_device_key(&device_key_path(root))?;
    let sealed = seal_workspace_key(&device_key, &id, &key)?;
    write_atomic(&staging, "keys/workspace.key", &sealed, true)?;
    // the recovery phrase, device-sealed (own AAD domain): the Open screen's
    // details panel shows it while the workspace is at-rest-unencrypted
    let sealed_seed = seal_seed_entropy(&device_key, &id, seed)?;
    write_atomic(&staging, "keys/seed.sealed", &sealed_seed, true)?;

    // frame 1: the genesis
    let plaintext = serde_json::to_vec(genesis)
        .map_err(|e| StorageError::Corrupt(format!("encoding genesis: {e}")))?;
    let frame = encode_frame(&key, &id, 1, genesis.seq, &plaintext)?;
    let seg_path = staging.join("log").join(segment_name(1));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&seg_path)?;
        f.write_all(&frame)?;
        f.sync_all()?;
    }

    fs::rename(&staging, &final_dir)?;
    // best-effort: make the rename itself durable
    if let Ok(d) = File::open(root) {
        let _ = d.sync_all();
    }

    let lock = acquire_lock(&final_dir)?;
    let seg = OpenOptions::new()
        .append(true)
        .open(final_dir.join("log").join(segment_name(1)))?;
    let seg_len = u64::try_from(frame.len()).unwrap_or(0);
    Ok(OpenedWorkspace {
        dir: final_dir,
        manifest,
        prefs,
        key: Zeroizing::new(key),
        id,
        _lock: lock,
        // a fresh workspace has never been compacted: its segments live
        // under the workspace key until the first compaction migrates them
        seg_keys: None,
        seg_no: 1,
        seg,
        seg_len,
        next_seq: 2,
        dirty: false,
        // a fresh workspace has no transport.state yet
        transport: std::sync::Mutex::new(Some(TransportState::default())),
    })
}

/// The manifest-level preconditions of opening: version within this
/// build's ceiling, and not phrase-sealed (S6 — a sealed dir has NO key
/// material on disk; opening it is impossible by design, not an I/O
/// accident, and the typed [`StorageError::Sealed`] routes every frontend
/// to the decrypt flow).
fn openable_gate(manifest: &WorkspaceManifest) -> Result<(), StorageError> {
    if manifest.version > molt_core::STORAGE_VERSION_SEALED {
        return Err(StorageError::NewerVersion(manifest.version));
    }
    // both sealed shapes route to the decrypt flow: S6 (unseal) and the S7
    // restored stub (whose "decrypt" is the verified restore pipeline)
    if sealing::is_sealed(manifest) || sealing::is_restored(manifest) {
        return Err(StorageError::Sealed(manifest.workspace.id.clone()));
    }
    Ok(())
}

/// Open an existing workspace directory: check the manifest version, take
/// the LOCK, unseal the key, load the newest valid snapshot, replay the log
/// tail (recovering a torn last segment), and position the writer.
pub fn open_workspace(ws_dir: &Path) -> Result<(OpenedWorkspace, LoadedState), StorageError> {
    // cheap pre-check on the un-locked manifest: never create a LOCK file
    // in a directory that is not an openable workspace
    openable_gate(&read_manifest(ws_dir)?)?;
    let lock = acquire_lock(ws_dir)?;
    // re-read under the flock: a seal_at_rest may have raced the pre-check
    // (its marker + key deletion land under the flock we now hold) — the
    // stale copy would misreport a healthy sealed dir as corruption
    let manifest = read_manifest(ws_dir)?;
    openable_gate(&manifest)?;

    let (key, id) = open_key_material(ws_dir, &manifest)?;
    let transport = open_transport_state(ws_dir, &key, &id)?;
    let prefs = read_prefs(ws_dir);

    // The per-segment key table (WP4a). Absent = never compacted: every
    // segment is under the workspace key and seq counts from 1, exactly as
    // before. Present = the log may start above 1 and each segment names both
    // its first seq and its own key.
    let seg_keys = segkeys::read_table(ws_dir, &key, &id)?;
    let compaction_floor = seg_keys.as_ref().map(|t| t.floor).unwrap_or(0);
    let replay = replay_log(ws_dir, &key, &id, seg_keys.as_ref())?;
    let snapshot = covering_snapshot(ws_dir, &key, &id, compaction_floor, &replay)?;
    let floor = snapshot.as_ref().map(|s| s.at_seq).unwrap_or(0);
    let tail: Vec<EventEnvelope> = replay.history.into_iter().filter(|e| e.seq > floor).collect();

    let (seg_no, seg_len) = replay.last_seg;
    let seg = OpenOptions::new()
        .append(true)
        .open(ws_dir.join("log").join(segment_name(seg_no)))?;
    let opened = OpenedWorkspace {
        dir: ws_dir.to_path_buf(),
        manifest,
        prefs,
        key: Zeroizing::new(key),
        id,
        _lock: lock,
        seg_keys,
        seg_no,
        seg,
        seg_len,
        next_seq: replay.last_seq + 1,
        dirty: false,
        transport: std::sync::Mutex::new(transport),
    };
    // §A.4 crash recovery: a segment whose key was erased but whose file
    // survived the crash is already unreadable — unlink it now (the replay
    // above skipped it, so this only reclaims the bytes)
    opened.sweep_keyless_segments();
    Ok((
        opened,
        LoadedState {
            snapshot,
            tail,
            unknown_events: replay.unknown_events,
            compaction_floor,
        },
    ))
}

/// Open step 1: the workspace key (unsealed with the device key) and the
/// binary workspace id.
fn open_key_material(
    ws_dir: &Path,
    manifest: &WorkspaceManifest,
) -> Result<([u8; 32], [u8; 32]), StorageError> {
    let root = ws_dir.parent().unwrap_or(ws_dir);
    let device_key = load_or_create_device_key(&device_key_path(root))?;
    let id = id_bytes(&manifest.workspace.id)?;
    let sealed = match read_capped(&ws_dir.join(&manifest.crypto.key_file), READ_CAP_KEY, "workspace key") {
        Ok(b) => b,
        // marker and key files disagree (crashed seal?): honest corruption,
        // never a guess — decrypting with the recovery phrase repairs it
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::BadFile(format!(
                "{} says device-sealed but the key material is missing - \
                 decrypt with the recovery phrase to repair",
                ws_dir.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let key = unseal_workspace_key(&device_key, &id, &sealed)?;
    Ok((key, id))
}

/// Open step 2: the same refusal for `transport.state` that the manifest
/// gets (M5). The read path falls back to a default on damage — right for
/// a ratchet, which re-establishes — but a file written by a NEWER node is
/// not damage: the first save would rewrite it under this build's version,
/// and with it go the MLS ratchet and the transport identity's
/// non-re-derivable secret. An older reader meeting something unknown
/// must not WRITE (the additive-only rule), so it does not open at all.
///
/// Missing, unreadable, unauthenticated or undecodable all stay silent
/// here: turning them into a refusal would make a recoverable workspace
/// unopenable. `Some` = the decoded state, seeding the handle's cache so
/// the open's decrypt is the only one; `None` = damaged, left for the
/// first read to report as the loud loss it is.
fn open_transport_state(
    ws_dir: &Path,
    key: &[u8; 32],
    id: &[u8; 32],
) -> Result<Option<TransportState>, StorageError> {
    match read_transport_state_raw(ws_dir, key, id) {
        Ok(st) => Ok(Some(st)),
        Err(TransportStateFault::Absent) => Ok(Some(TransportState::default())),
        Err(TransportStateFault::Newer(v)) => Err(StorageError::NewerVersion(v)),
        Err(_) => Ok(None),
    }
}

/// What replaying the surviving log segments produced.
struct LogReplay {
    /// Every decodable envelope, in seq order.
    history: Vec<EventEnvelope>,
    /// Frames from a newer node: kept on disk, not applied.
    unknown_events: u64,
    /// The seq of the first surviving frame — what a usable snapshot must
    /// reach on a compacted log.
    log_starts_at: u64,
    /// The seq of the last frame.
    last_seq: u64,
    /// The active segment: (number, byte length after torn-tail recovery).
    last_seg: (u64, u64),
}

/// Open step 3: replay the segments. Seq is positional and strictly
/// monotonic — from 1 on a complete log, from the surviving segment's own
/// first seq after a compaction dropped the ones below it.
fn replay_log(
    ws_dir: &Path,
    key: &[u8; 32],
    id: &[u8; 32],
    seg_keys: Option<&segkeys::SegmentKeyTable>,
) -> Result<LogReplay, StorageError> {
    let mut segments = list_sorted(&ws_dir.join("log"), ".mlog");
    if let Some(table) = seg_keys {
        // a segment whose key was erased is dropped-but-not-yet-unlinked (a
        // crash between §A.4's key-erase and unlink): its bytes are already
        // worthless, so it is not part of the log
        segments.retain(|(no, _)| table.dek(*no).is_some());
    }
    if segments.is_empty() {
        return Err(StorageError::Corrupt(
            "workspace has no log segments".to_string(),
        ));
    }
    let mut history = Vec::new();
    let mut unknown_events: u64 = 0;
    let mut expected_seq: u64 = match (seg_keys, segments.first()) {
        (Some(table), Some((no, _))) => table.first_seq(*no).unwrap_or(1),
        _ => 1,
    };
    let log_starts_at = expected_seq;
    let last_idx = segments.len() - 1;
    let mut last_seg = (1u64, 0u64);
    for (idx, (seg_no, path)) in segments.iter().enumerate() {
        let data = read_capped(path, READ_CAP_SEGMENT, "log segment")?;
        let (frames, torn_at) = split_frames(&data);
        if let Some(pos) = torn_at {
            // A torn APPEND leaves a partial frame at the end of the file
            // and nothing behind it — the writer only ever appends, so
            // there is no later content to survive. A valid frame BEHIND
            // the damage therefore means the file was complete and
            // something corrupted it in place, and truncating there would
            // throw away history this node already acknowledged. That is
            // the same situation the middle segments refuse to guess about.
            if idx == last_idx && has_valid_frame_after(&data, pos) {
                return Err(StorageError::Corrupt(format!(
                    "segment {seg_no} is damaged at byte {pos} but valid frames \
                     follow it - refusing to truncate history away"
                )));
            }
            if idx == last_idx {
                tracing::warn!(
                    segment = seg_no,
                    at = pos,
                    "torn tail truncated to the last valid frame boundary"
                );
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(u64::try_from(pos).unwrap_or(0))?;
                f.sync_all()?;
            } else {
                return Err(StorageError::Corrupt(format!(
                    "segment {} is damaged at byte {} but is not the last segment - \
                     refusing to guess",
                    path.display(),
                    pos
                )));
            }
        }
        // a compacted log states each segment's first seq; a disagreement with
        // the running count means the table and the files no longer describe
        // the same log — refuse rather than replay at a shifted seq (the AAD
        // would fail anyway, but the honest error names the cause)
        if let Some(stated) = seg_keys.and_then(|t| t.first_seq(*seg_no)) {
            if stated != expected_seq {
                return Err(StorageError::Corrupt(format!(
                    "log key table says segment {seg_no} starts at seq {stated}, \
                     the surviving log reaches it at {expected_seq}"
                )));
            }
        }
        let seg_key = seg_keys.and_then(|t| t.dek(*seg_no)).unwrap_or(*key);
        let mut seg_len = 0u64;
        for frame in &frames {
            let plaintext = match decrypt_frame(
                &seg_key,
                id,
                *seg_no,
                expected_seq,
                frame.nonce,
                frame.ciphertext,
            ) {
                Ok(p) => p,
                // a half-finished F6 migration: this segment is still under
                // the workspace key (see `decrypt_log_frame`)
                Err(e) if seg_key != *key => {
                    decrypt_frame(key, id, *seg_no, expected_seq, frame.nonce, frame.ciphertext)
                        .map_err(|_| e)?
                }
                Err(e) => return Err(e),
            };
            match serde_json::from_slice::<EventEnvelope>(&plaintext) {
                Ok(env) => {
                    if env.seq != expected_seq {
                        return Err(StorageError::Corrupt(format!(
                            "envelope seq {} at log position {}",
                            env.seq, expected_seq
                        )));
                    }
                    history.push(env);
                }
                Err(_) => {
                    // a frame from a newer node: keep it on disk, refuse to apply
                    let raw: Result<RawEnvelope, _> = serde_json::from_slice(&plaintext);
                    if raw.is_err() {
                        return Err(StorageError::Corrupt(format!(
                            "frame at seq {expected_seq} is not an event envelope"
                        )));
                    }
                    unknown_events += 1;
                }
            }
            expected_seq += 1;
            seg_len = u64::try_from(frame.end).unwrap_or(seg_len);
        }
        last_seg = (*seg_no, seg_len);
    }
    Ok(LogReplay {
        history,
        unknown_events,
        log_starts_at,
        last_seq: expected_seq - 1,
        last_seg,
    })
}

/// Open step 4: the newest decodable snapshot the surviving log can
/// continue from. A snapshot ahead of the log (partial dir copy, torn
/// tail behind an old backup) would make the append position diverge
/// from the positional seq the AAD binds, bricking every later open; such
/// a snapshot is skipped and the state rebuilt from the log alone.
///
/// WP4a: on a COMPACTED log the surviving frames start above 1, so a
/// snapshot must reach at least to the seq before them or the replay would
/// have a hole where the dropped segments used to be. An older snapshot
/// (kept as the spare) is exactly such a hole — skip it, and if none
/// covers, that is a hard error rather than silently thinner state.
fn covering_snapshot(
    ws_dir: &Path,
    key: &[u8; 32],
    id: &[u8; 32],
    compaction_floor: u64,
    replay: &LogReplay,
) -> Result<Option<WorkspaceSnapshot>, StorageError> {
    let log_starts_at = replay.log_starts_at;
    let last_seq = replay.last_seq;
    let mut snapshot: Option<WorkspaceSnapshot> = None;
    let mut snaps = list_sorted(&ws_dir.join("snapshots"), ".msnap");
    snaps.reverse();
    for (at_seq, path) in snaps {
        if compaction_floor > 0 && at_seq.saturating_add(1) < log_starts_at {
            tracing::warn!(
                path = %path.display(),
                at_seq,
                log_starts_at,
                "snapshot predates the compacted log, skipping it"
            );
            continue;
        }
        if at_seq > last_seq {
            tracing::warn!(
                path = %path.display(),
                last_seq,
                "snapshot is ahead of the log, skipping it"
            );
            continue;
        }
        match read_snapshot(key, id, at_seq, &path) {
            Ok(s) => {
                snapshot = Some(s);
                break;
            }
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping snapshot"),
        }
    }
    if compaction_floor > 0 && snapshot.is_none() {
        // the dropped history lived only in that snapshot; replaying the
        // surviving tail alone would present a partial state as complete
        return Err(StorageError::Corrupt(format!(
            "the log starts at seq {log_starts_at} (compacted) but no usable \
             snapshot covers what came before - refusing to open a partial state"
        )));
    }
    Ok(snapshot)
}

fn read_snapshot(
    key: &[u8; 32],
    id: &[u8; 32],
    at_seq: u64,
    path: &Path,
) -> Result<WorkspaceSnapshot, StorageError> {
    let data = read_capped(path, READ_CAP_STATE, "snapshot")?;
    decode_snapshot(key, id, at_seq, &data)
}

/// [`read_snapshot`] on bytes already in memory (the import reads them out
/// of a blob before anything is on disk).
pub(crate) fn decode_snapshot(
    key: &[u8; 32],
    id: &[u8; 32],
    at_seq: u64,
    data: &[u8],
) -> Result<WorkspaceSnapshot, StorageError> {
    let (frames, torn) = split_frames(data);
    if frames.len() != 1 || torn.is_some() {
        return Err(StorageError::Corrupt("snapshot framing".to_string()));
    }
    let plaintext = decrypt_frame(
        key,
        id,
        SNAPSHOT_SEGMENT,
        at_seq,
        frames[0].nonce,
        frames[0].ciphertext,
    )?;
    let snap: WorkspaceSnapshot = serde_json::from_slice(&plaintext)
        .map_err(|e| StorageError::Corrupt(format!("snapshot decode: {e}")))?;
    if snap.at_seq != at_seq {
        return Err(StorageError::Corrupt(
            "snapshot at_seq does not match its file name".to_string(),
        ));
    }
    Ok(snap)
}

// ---------------------------------------------------------------------------
// Scanning, trash
// ---------------------------------------------------------------------------

/// One workspace directory found under the root.
pub struct ScanEntry {
    /// The workspace directory.
    pub dir: PathBuf,
    /// Its plaintext identity card.
    pub manifest: WorkspaceManifest,
    /// Its local prefs.
    pub prefs: WorkspacePrefs,
    /// On-disk size in KiB.
    pub size_kib: u64,
}

impl ScanEntry {
    /// Project this directory entry into the session's workspace-list shape.
    /// Only plaintext facts: sync/presence fields stay neutral (they are the
    /// transport's runtime state) and roster/seed stay empty here. The app's
    /// startup scan fills them via [`read_sealed_seed`] / [`peek_genesis`]
    /// (both open with the device-sealed key material) so the details panel
    /// of an at-rest-unencrypted workspace shows the real phrase and roster.
    pub fn info(&self) -> molt_core::WorkspaceInfo {
        let w = &self.manifest.workspace;
        let last_backup_min = self
            .prefs
            .last_backup
            .map(|ts| u32::try_from(now_secs().saturating_sub(ts) / 60).unwrap_or(u32::MAX - 1))
            .unwrap_or(molt_core::WorkspaceInfo::NEVER);
        molt_core::WorkspaceInfo {
            id: w.id.clone(),
            name: w.name.clone(),
            detail: molt_core::WorkspaceInfo::rule_detail(w.rule_m, usize::from(w.rule_n)),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3: self.prefs.s3_backup,
            size_kib: u32::try_from(self.size_kib).unwrap_or(u32::MAX),
            last_backup_min,
            // bucket-side facts appear only from a real listing/attempt
            backup_copies: 0,
            backup_error: String::new(),
            seed: String::new(),
            // the manifest carries no network label — the caller stamps the
            // effective global setting (`molt_core::effective_net_label`);
            // claiming one here would mislabel every entry after a restart
            net: String::new(),
            // derived from the directory (S6/S7 markers), so the sealed
            // state survives restarts instead of living in session memory
            encrypted: sealing::is_sealed(&self.manifest) || sealing::is_restored(&self.manifest),
            restored: sealing::is_restored(&self.manifest),
            members: Vec::new(),
            // the charter is in the encrypted genesis — filled in on open
            // (refresh_active_entry), like the roster
            agenda: String::new(),
        }
    }
}

/// Scan `root/*/manifest.toml` — cheap, no decryption: exactly what the Open
/// screen needs. Unreadable entries are skipped with a log line; dot-entries
/// (`.trash`, staging) are ignored. Sorted by display name.
pub fn scan_workspaces(root: &Path) -> Vec<ScanEntry> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        match read_manifest(&dir) {
            Ok(manifest) => {
                let prefs = read_prefs(&dir);
                let size_kib = workspace_size_kib(&dir);
                out.push(ScanEntry {
                    dir,
                    manifest,
                    prefs,
                    size_kib,
                });
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping non-workspace entry");
            }
        }
    }
    out.sort_by(|a, b| a.manifest.workspace.name.cmp(&b.manifest.workspace.name));
    out
}

/// Find the directory of the workspace with this id, if present under root.
/// Deliberately lighter than [`scan_workspaces`]: it reads one manifest per
/// candidate directory and never walks file sizes — this runs on the engine
/// actor for open/delete/prefs, where a recursive stat storm would stall
/// every operator.
pub fn find_workspace_dir(root: &Path, id: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(root).ok()?;
    for entry in rd.flatten() {
        let dir = entry.path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        if let Ok(manifest) = read_manifest(&dir) {
            if manifest.workspace.id == id {
                return Some(dir);
            }
        }
    }
    None
}

/// The real on-disk footprint of a workspace directory in KiB, rounded
/// **up** (a directory holding any file bytes never reports 0). One
/// recursive walk, tolerant of concurrent writers: entries that vanish
/// mid-walk (or a directory that does not exist at all) simply contribute
/// nothing — never an error, never a panic. Symlinks are **not** followed:
/// a link cycle must not recurse and a link must not pull foreign bytes
/// into the footprint. Both the boot scan and the engine's list-entry
/// refreshes report through this one helper so the two can never disagree
/// on what "size" means.
pub fn workspace_size_kib(dir: &Path) -> u64 {
    dir_size(dir).div_ceil(1024)
}

fn dir_size(dir: &Path) -> u64 {
    dir_size_to(dir, 0)
}

/// How deep [`dir_size`] recurses. A workspace is three levels at most
/// (`log/`, `tmp/`, `.trash/`); anything past this is not a workspace and
/// must not be able to walk the open path into a stack overflow. Symlinks
/// are already not followed, so this is only about honest depth.
const DIR_SIZE_MAX_DEPTH: u32 = 16;

fn dir_size_to(dir: &Path, depth: u32) -> u64 {
    let mut total = 0u64;
    if depth > DIR_SIZE_MAX_DEPTH {
        tracing::warn!(dir = %dir.display(), "directory nesting past the scan depth, size not counted below");
        return total;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return total;
    };
    for entry in rd.flatten() {
        // the readdir entry's own type (one syscall at most, and symlinks
        // are NOT followed — no cycle recursion, no foreign bytes)
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            total += dir_size_to(&entry.path(), depth + 1);
        } else if ft.is_file() {
            if let Ok(md) = entry.metadata() {
                total += md.len();
            }
        }
    }
    total
}

/// Move a workspace directory to `root/.trash/<name>-<ts>` (recoverable
/// delete). Returns the trash location.
pub fn trash_workspace(root: &Path, ws_dir: &Path) -> Result<PathBuf, StorageError> {
    let trash = root.join(".trash");
    fs::create_dir_all(&trash)?;
    let base = ws_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let target = trash.join(format!("{base}-{}", now_secs()));
    fs::rename(ws_dir, &target)?;
    Ok(target)
}

/// Delete `.trash` entries older than `max_age_secs` (called at startup).
pub fn purge_trash(root: &Path, max_age_secs: u64) {
    let trash = root.join(".trash");
    let Ok(rd) = fs::read_dir(&trash) else {
        return;
    };
    let cutoff = now_secs().saturating_sub(max_age_secs);
    for entry in rd.flatten() {
        let path = entry.path();
        // the timestamp is the suffix after the last '-'
        let stamp = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit('-').next())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(ts) = stamp {
            if ts <= cutoff {
                if let Err(e) = fs::remove_dir_all(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "purging trash entry failed");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The writer task: one per open workspace
// ---------------------------------------------------------------------------

enum WriterMsg {
    Append(EventEnvelope),
    Prefs(WorkspacePrefs),
    /// Rewrite the manifest's display name (an applied `set_name`): the
    /// plaintext identity card must agree with the replayed state so the
    /// undecrypted Open-screen scan lists the effective name.
    Rename(String),
    /// Reconcile the workspace's logo file with the applied image state:
    /// `Some((ext, bytes))` materializes `logo.<ext>` (removing any other
    /// `logo.*`), `None` removes it. Idempotent.
    Logo(Option<(String, Vec<u8>)>),
    /// The per-member twin of [`WriterMsg::Logo`]: reconcile ONE member's
    /// `avatar-<stem>.<ext>` with its applied profile picture. Idempotent.
    Avatar(String, Option<(String, Vec<u8>)>),
    Snapshot(WorkspaceSnapshot),
    /// Outbox read: every envelope with `seq >= from`. Served by the
    /// writer thread so reads are consistently ordered with queued appends
    /// (same channel, FIFO — a read enqueued after an append sees it).
    ReadFrom(u64, tokio::sync::oneshot::Sender<Vec<EventEnvelope>>),
    /// Persist `transport.state` (atomic rewrite).
    SaveTransport(TransportState),
    /// Merge the engine's receive-side accept windows (delivery guarantee
    /// §4.2/§4.7) into `transport.state`, touching nothing else. Its own
    /// message (not part of `SaveTransport`) because the ENGINE owns the
    /// windows while the SUPERVISOR owns the cursors — each overlays only
    /// its own fields, so neither clobbers the other.
    SaveAccepted(std::collections::BTreeMap<molt_core::MemberId, molt_core::AcceptedWindow>),
    /// A piece fetch's bookkeeping (mirroring §3.2): upsert by series.
    SaveFetchJob(Box<molt_core::FetchJob>),
    /// The fetch of this series ended.
    RemoveFetchJob(String),
    /// A mirror's bookkeeping (mirroring §3.3): upsert by series.
    SaveMirrorJob(String, Box<molt_core::MirrorJob>),
    /// The mirror of this series is gone.
    RemoveMirrorJob(String),
    /// Merge the runtime crypto (MLS snapshot + queue creds) into the CURRENT
    /// `transport.state` — read-modify-write on the writer thread, so it
    /// preserves the outbox/inbound cursors the supervisor left. Acks when
    /// durable (a blocking clean-close persist).
    MergeCrypto {
        mls: Option<Vec<u8>>,
        smp_queues: Option<Vec<u8>>,
        /// `Some` replaces the persisted mesh links (dynamic mesh membership:
        /// a grown/re-keyed mesh must survive a reopen); `None` leaves them.
        mesh: Option<Vec<molt_core::MeshLink>>,
        /// `true` = the CLEAN-CLOSE merge: later `SaveTransport` cursor saves
        /// from a supervisor winding down are ignored. `false` = a live
        /// mid-session merge (mesh extension) — the running supervisor keeps
        /// saving cursors afterwards.
        seal: bool,
        ack: mpsc::SyncSender<bool>,
    },
    /// Load `transport.state` (defaults when absent/damaged).
    LoadTransport(tokio::sync::oneshot::Sender<TransportState>),
    /// **One compaction round** (WP4a): write the trimmed snapshot, then drop
    /// every log segment the snapshot and the peers no longer need. The floor
    /// is computed HERE because the writer owns both inputs — the snapshot
    /// position (R1) and the persisted delivery cursors (R2); the engine
    /// contributes the policy: which content aged out (it trimmed the
    /// snapshot) and which peers are past their grace and no longer hold the
    /// floor (`ignore_peers`, F4).
    Compact {
        snapshot: WorkspaceSnapshot,
        holding_peers: Vec<String>,
        ack: mpsc::SyncSender<CompactionOutcome>,
    },
    /// Flush + fsync everything queued so far, acking when durable. The
    /// group-commit means a just-appended event can still be in the buffer;
    /// anything that COPIES the directory (backup, export) has to force it
    /// out first or the copy silently misses the newest frames.
    Flush(mpsc::SyncSender<bool>),
    /// Persist the whole persistent commit-block chain (`chain.state`), acking
    /// when durable — a governance commit must not be lost, so it uses the same
    /// blocking-ack shape as `MergeCrypto`.
    PersistChain {
        blob: Option<molt_core::CheckpointState>,
        blocks: Vec<molt_core::ChainBlock>,
        ack: mpsc::SyncSender<bool>,
    },
    Close(mpsc::SyncSender<()>),
}

/// A cheap handle to a workspace's writer thread (one writer per open
/// workspace, same single-owner pattern as the engine actor). The engine
/// applies an event in memory and enqueues the envelope; the thread frames,
/// encrypts, appends and group-commits (fsync at most every 50 ms). The
/// actor never blocks on a healthy disk.
#[derive(Clone)]
pub struct StorageHandle {
    tx: mpsc::SyncSender<WriterMsg>,
    failed: Arc<AtomicBool>,
    /// Serializes every read-modify-write of `transport.state` across the
    /// handle's clones (the group runtime's tasks each hold one): the state
    /// file is rewritten whole, so two interleaved updaters lost each
    /// other's writes (review 2026-08-25 M7).
    transport_gate: Arc<tokio::sync::Mutex<()>>,
}

impl StorageHandle {
    /// The gate every `transport.state` updater takes (see the field).
    pub fn transport_gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.transport_gate.clone()
    }

    /// Enqueue one envelope. Returns `false` when the bounded queue was full
    /// (storage is lagging — the caller should surface that honestly); the
    /// envelope is still delivered, blocking until there is room.
    pub fn append(&self, env: EventEnvelope) -> bool {
        match self.tx.try_send(WriterMsg::Append(env)) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(msg)) => {
                let _ = self.tx.send(msg);
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.failed.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    /// Persist new prefs.
    pub fn set_prefs(&self, p: WorkspacePrefs) {
        let _ = self.tx.send(WriterMsg::Prefs(p));
    }

    /// Rewrite the manifest's display name (an applied `set_name`); no-op
    /// when unchanged. Fire-and-forget like `set_prefs`.
    pub fn set_display_name(&self, name: String) {
        let _ = self.tx.send(WriterMsg::Rename(name));
    }

    /// Reconcile the workspace's logo file with the applied image state
    /// (`Some((ext, bytes))` materializes, `None` removes). Fire-and-forget.
    pub fn set_logo(&self, logo: Option<(String, Vec<u8>)>) {
        let _ = self.tx.send(WriterMsg::Logo(logo));
    }

    /// Reconcile ONE member's avatar file with its applied profile picture
    /// (`Some((ext, bytes))` materializes, `None` removes). Fire-and-forget.
    pub fn set_avatar(&self, stem: String, avatar: Option<(String, Vec<u8>)>) {
        let _ = self.tx.send(WriterMsg::Avatar(stem, avatar));
    }

    /// Force everything queued so far to disk and wait for it. Call before
    /// copying the workspace directory (backup/export): without it the copy
    /// can miss events the caller already considers written.
    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    pub fn flush_blocking(&self) -> bool {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.send(WriterMsg::Flush(ack_tx)).is_err() {
            return false; // the writer is gone: nothing reached the disk
        }
        ack_rx.recv().unwrap_or(false)
    }

    /// Enqueue a snapshot write.
    pub fn snapshot(&self, snap: WorkspaceSnapshot) {
        let _ = self.tx.send(WriterMsg::Snapshot(snap));
    }

    /// Run one compaction round on the writer thread and wait for it (WP4a
    /// §A.4). Blocking, like the other durability-critical calls: the trimmed
    /// snapshot must be on disk before any segment is dropped, and the caller
    /// wants the honest outcome to log.
    pub fn compact_blocking(
        &self,
        snapshot: WorkspaceSnapshot,
        holding_peers: Vec<String>,
    ) -> CompactionOutcome {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(WriterMsg::Compact {
                snapshot,
                holding_peers,
                ack: ack_tx,
            })
            .is_err()
        {
            return CompactionOutcome::default();
        }
        ack_rx.recv().unwrap_or_default()
    }

    /// Enqueue one message from an async context. The writer queue is a
    /// bounded std channel whose `send` blocks when the disk falls behind
    /// — that must stall a blocking-pool thread, never a tokio worker
    /// (a couple of blocked workers would freeze the engine and UI).
    async fn send_from_async(&self, msg: WriterMsg) -> bool {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || tx.send(msg).is_ok())
            .await
            .unwrap_or(false)
    }

    /// Read every envelope with `seq >= from_seq` — the log-backed outbox
    /// source. Empty when the writer is gone.
    pub async fn read_log_from(&self, from_seq: u64) -> Vec<EventEnvelope> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if !self.send_from_async(WriterMsg::ReadFrom(from_seq, tx)).await {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Queue a `transport.state` rewrite (fire-and-forget, like prefs).
    /// Persist the engine's receive-side accept windows (delivery guarantee
    /// §4.7), merged into `transport.state` without touching the cursors or
    /// crypto. Fire-and-forget like [`Self::save_transport_state`]: a lost
    /// save only regresses the windows, which resends + re-dedup absorb.
    pub fn save_accepted(
        &self,
        accepted: std::collections::BTreeMap<molt_core::MemberId, molt_core::AcceptedWindow>,
    ) {
        match self.tx.try_send(WriterMsg::SaveAccepted(accepted)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "accept-window save", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Upsert one piece fetch's job (`docs/files/mirroring.md` §3.2) into
    /// `transport.state`, touching nothing else. Fire-and-forget and
    /// synchronous, so a fetch task can save from its sink; a lost save
    /// only re-lands pieces the relay replays anyway.
    pub fn save_fetch_job(&self, job: molt_core::FetchJob) {
        match self.tx.try_send(WriterMsg::SaveFetchJob(Box::new(job))) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "fetch job save", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Forget the fetch job of `series` (it completed or failed).
    pub fn remove_fetch_job(&self, series: &str) {
        match self.tx.try_send(WriterMsg::RemoveFetchJob(series.to_string())) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "fetch job removal", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Upsert one mirrored series' job (`docs/files/mirroring.md` §3.3);
    /// fire-and-forget like [`Self::save_fetch_job`].
    pub fn save_mirror_job(&self, series: &str, job: molt_core::MirrorJob) {
        match self.tx.try_send(WriterMsg::SaveMirrorJob(series.to_string(), Box::new(job))) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "mirror job save", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Forget the mirror job of `series`.
    pub fn remove_mirror_job(&self, series: &str) {
        match self.tx.try_send(WriterMsg::RemoveMirrorJob(series.to_string())) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "mirror job removal", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// A full writer queue drops the save with a warning: stale cursors
    /// only cost resends, which the peers' dedup absorbs — better than
    /// blocking the transport on a struggling disk.
    pub fn save_transport_state(&self, state: TransportState) {
        match self.tx.try_send(WriterMsg::SaveTransport(state)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "transport.state save", "writer queue full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Merge the runtime crypto (MLS snapshot + queue creds) into the current
    /// `transport.state`, preserving the delivery cursors, and BLOCK until it is
    /// durable (fsync'd). The clean-close persist that lets a reopened node
    /// resume the mesh.
    ///
    /// Returns whether it really is durable. It used to ack unconditionally,
    /// so a failed write reported success and the node closed believing its
    /// ratchet snapshot was on disk.
    #[must_use]
    pub fn persist_crypto_blocking(&self, mls: Option<Vec<u8>>, smp_queues: Option<Vec<u8>>) -> bool {
        self.merge_crypto_blocking(mls, smp_queues, None, true)
    }

    /// A **live** (mid-session) variant of [`Self::persist_crypto_blocking`]
    /// that can also replace the persisted **mesh links** — dynamic mesh
    /// membership grows/re-keys the mesh at runtime, and a reopen must resume
    /// the grown mesh, not the founded one. Does NOT seal `transport.state`:
    /// the (rebuilt) supervisor keeps saving its cursors afterwards.
    ///
    /// Returns whether it really is durable — see
    /// [`Self::persist_crypto_blocking`].
    #[must_use]
    pub fn persist_mesh_crypto_blocking(
        &self,
        mls: Option<Vec<u8>>,
        smp_queues: Option<Vec<u8>>,
        mesh: Vec<molt_core::MeshLink>,
    ) -> bool {
        self.merge_crypto_blocking(mls, smp_queues, Some(mesh), false)
    }

    /// Fire-and-forget MLS-snapshot merge (delivery guarantee §4.6 / E6, the
    /// debounced ratchet persist): merges ONLY the MLS blob, no seal, does
    /// not wait for the fsync. `try_send`, never `send` — this rides the
    /// engine actor's hot path (`record`), and a writer stalled in a
    /// compaction round or a slow fsync must cost a dropped merge (the next
    /// debounce beat retries; a lost merge only widens the crash-regression
    /// window the resend heals), never a frozen actor (E7 review).
    pub fn merge_mls_async(&self, mls: Vec<u8>) -> bool {
        let (ack_tx, _ack_rx) = mpsc::sync_channel(1);
        match self.tx.try_send(WriterMsg::MergeCrypto {
            mls: Some(mls),
            smp_queues: None,
            mesh: None,
            seal: false,
            ack: ack_tx,
        }) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(dropped = "live MLS merge", "writer queue full");
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    fn merge_crypto_blocking(
        &self,
        mls: Option<Vec<u8>>,
        smp_queues: Option<Vec<u8>>,
        mesh: Option<Vec<molt_core::MeshLink>>,
        seal: bool,
    ) -> bool {
        if mls.is_none() && smp_queues.is_none() && mesh.is_none() {
            return true; // nothing asked for, nothing owed
        }
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(WriterMsg::MergeCrypto {
                mls,
                smp_queues,
                mesh,
                seal,
                ack: ack_tx,
            })
            .is_err()
        {
            return false; // the writer is gone: nothing reached the disk
        }
        ack_rx.recv().unwrap_or(false)
    }

    /// Persist the whole persistent commit-block chain and BLOCK until it is
    /// durable (fsync'd) — a governance commit must survive a crash the instant
    /// it is broadcast.
    ///
    /// Returns whether it really is durable. It used to ack unconditionally,
    /// so a failed write let the engine broadcast a threshold-signed block
    /// while nothing was on the disk.
    #[must_use]
    pub fn persist_chain_blocking(
        &self,
        blob: Option<molt_core::CheckpointState>,
        blocks: Vec<molt_core::ChainBlock>,
    ) -> bool {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(WriterMsg::PersistChain {
                blob,
                blocks,
                ack: ack_tx,
            })
            .is_err()
        {
            return false; // the writer is gone: nothing reached the disk
        }
        ack_rx.recv().unwrap_or(false)
    }

    /// Load `transport.state` (defaults when absent, damaged, or the
    /// writer is gone).
    pub async fn load_transport_state(&self) -> TransportState {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if !self.send_from_async(WriterMsg::LoadTransport(tx)).await {
            return TransportState::default();
        }
        rx.await.unwrap_or_default()
    }

    /// Whether the writer hit a fatal error (dying disk); appends after this
    /// are lost and the workspace should be closed.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Flush everything, optionally write a closing snapshot, fsync, release
    /// the LOCK, and join the thread. Blocks until durable.
    pub fn close(self, closing_snapshot: Option<WorkspaceSnapshot>) {
        if let Some(snap) = closing_snapshot {
            let _ = self.tx.send(WriterMsg::Snapshot(snap));
        }
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.send(WriterMsg::Close(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

/// What one compaction round did — honest zeroes when nothing was eligible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// The floor after this round: the highest seq physically dropped.
    pub floor: u64,
    /// How many log segments were dropped.
    pub segments_dropped: usize,
}

/// One compaction round on the writer thread (WP4a §A.4), in the order that
/// makes every step idempotent and a crash between them harmless:
///
/// 1. the TRIMMED snapshot, atomically — the surviving state must be durable
///    before anything is dropped (R1: the floor can never exceed it);
/// 2. the floor: the snapshot position, held back by the delivery cursor of
///    every peer still inside its grace (`holding_peers`; R2/C2 — a peer past
///    its grace is redirected to the chain catch-up instead of pinning the log
///    forever). A holding peer with NO cursor at all — never delivered to, or
///    its `transport.state` was lost — counts as cursor 0 and stops the round:
///    "no record of having sent it anything" must never read as "it has
///    everything";
/// 3. the F6 migration, once, so every segment is under its own key;
/// 4. the drop itself (keys erased first, then the files).
///
/// The manifest version is raised on the first real drop: an older binary,
/// which knows neither the key table nor a log that starts above seq 1, then
/// refuses the workspace politely instead of reading it as damaged.
fn compact_once(
    ws: &mut OpenedWorkspace,
    snapshot: &WorkspaceSnapshot,
    holding_peers: &[String],
) -> Result<CompactionOutcome, StorageError> {
    ws.write_snapshot(snapshot)?;
    ws.sync()?;
    let cursors = ws.read_transport_state().outbound;
    let mut floor = snapshot.at_seq;
    for peer in holding_peers {
        // delivery guarantee §4.9: a proven-acking peer holds the log down to
        // its ACKED floor, not merely to the send cursor — the unacked tail
        // must stay resendable (every rebuild rewinds to the floor). An old
        // (never-acking) peer keeps the plain cursor gate.
        floor = floor.min(cursors.get(peer).map_or(0, |c| {
            if c.ack_seen {
                c.acked_floor
            } else {
                c.log_seq
            }
        }));
    }
    if floor == 0 {
        return Ok(CompactionOutcome {
            floor: ws.compaction_floor(),
            segments_dropped: 0,
        });
    }
    ws.migrate_to_segment_keys()?;
    let segments_dropped = ws.drop_segments_below(floor)?;
    if segments_dropped > 0 {
        ws.bump_pruned_version()?;
    }
    Ok(CompactionOutcome {
        floor: ws.compaction_floor(),
        segments_dropped,
    })
}

/// Move an opened workspace onto its own writer thread and return the handle.
pub fn start_writer(mut ws: OpenedWorkspace) -> StorageHandle {
    let (tx, rx) = mpsc::sync_channel::<WriterMsg>(WRITER_QUEUE);
    let failed = Arc::new(AtomicBool::new(false));
    let failed_flag = failed.clone();
    std::thread::Builder::new()
        .name("molt-storage-writer".to_string())
        .spawn(move || {
            let mut last_sync = Instant::now();
            let fail = |flag: &AtomicBool, what: &str, e: &StorageError| {
                tracing::error!(error = %e, "storage writer: {what} failed");
                flag.store(true, Ordering::Relaxed);
            };
            let mut close_ack = None;
            // unsynced appends are pending an fsync deadline
            let mut dirty = false;
            // once a clean-close crypto merge lands, this handle is terminal for
            // transport.state: a `MergeCrypto` is always immediately followed by
            // close/switch, so a LATER `SaveTransport` can only be a stale cursor
            // update from a supervisor task still winding down — it must not
            // clobber the merged MLS + queue creds (else reopen loses the mesh)
            let mut crypto_sealed = false;
            loop {
                // idle and clean: sleep until work arrives (no 50 ms wakeups
                // on an idle workspace); dirty: wake at the commit deadline
                let msg = if dirty {
                    rx.recv_timeout(GROUP_COMMIT)
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match msg {
                    Ok(WriterMsg::Append(env)) => {
                        if let Err(e) = ws.append(&env) {
                            fail(&failed_flag, "append", &e);
                        }
                        if !dirty {
                            last_sync = Instant::now();
                        }
                        dirty = true;
                        if last_sync.elapsed() >= GROUP_COMMIT {
                            if let Err(e) = ws.sync() {
                                fail(&failed_flag, "fsync", &e);
                            }
                            dirty = false;
                            last_sync = Instant::now();
                        }
                    }
                    Ok(WriterMsg::Prefs(p)) => {
                        if let Err(e) = ws.set_prefs(p) {
                            fail(&failed_flag, "prefs write", &e);
                        }
                    }
                    Ok(WriterMsg::Rename(name)) => {
                        if let Err(e) = ws.set_display_name(&name) {
                            fail(&failed_flag, "manifest rename", &e);
                        }
                    }
                    Ok(WriterMsg::Logo(logo)) => {
                        if let Err(e) = ws.set_logo(logo) {
                            fail(&failed_flag, "logo write", &e);
                        }
                    }
                    Ok(WriterMsg::Avatar(stem, avatar)) => {
                        if let Err(e) = ws.set_avatar(&stem, avatar) {
                            fail(&failed_flag, "avatar write", &e);
                        }
                    }
                    Ok(WriterMsg::ReadFrom(from_seq, reply)) => {
                        match ws.read_log_from(from_seq) {
                            Ok(events) => {
                                let _ = reply.send(events);
                            }
                            Err(e) => {
                                // the outbox treats an empty read as "nothing
                                // pending"; a failing disk surfaces via the
                                // writer's own failure flag on the next append
                                tracing::warn!(error = %e, "outbox log read failed");
                                let _ = reply.send(Vec::new());
                            }
                        }
                    }
                    Ok(WriterMsg::SaveTransport(state)) => {
                        // a stale cursor update from a supervisor task winding
                        // down after the clean-close merge — dropping it protects
                        // the merged crypto (the workspace is closing anyway)
                        if crypto_sealed {
                            tracing::debug!("ignoring a post-merge transport.state save");
                        } else {
                            // the supervisor owns ONLY the delivery cursors; the
                            // rest of its in-memory clone (mls/mesh/creds) may
                            // predate a LIVE crypto merge (dynamic mesh
                            // extension), so merge the cursor maps into the
                            // current file state instead of writing the whole
                            // stale clone back over the merged values
                            let mut ts = ws.read_transport_state();
                            ts.outbound = state.outbound;
                            ts.inbound = state.inbound;
                            // …and the broadcast cursor. Without this line it
                            // works in-process and silently resets on every
                            // open, which reads as "the runtime republishes
                            // the whole log after a restart".
                            ts.group = state.group;
                            // …and the file plane (mirroring §3.2/§3.4): the
                            // publish queue, the daily counter and the gossip
                            // ride the cursor saves; the fetch and mirror jobs
                            // have their own messages and stay untouched (a
                            // load-modify-save around one would clobber it)
                            ts.file_jobs.publish = state.file_jobs.publish;
                            ts.file_jobs.sent_day = state.file_jobs.sent_day;
                            ts.file_jobs.sent_bytes = state.file_jobs.sent_bytes;
                            ts.mirror.on = state.mirror.on;
                            ts.mirror.quota = state.mirror.quota;
                            ts.mirror.rev = state.mirror.rev;
                            ts.mirror.decls = state.mirror.decls;
                            ts.mirror.status = state.mirror.status;
                            if let Err(e) = ws.write_transport_state(&ts) {
                                fail(&failed_flag, "transport.state write", &e);
                            }
                        }
                    }
                    Ok(WriterMsg::SaveAccepted(accepted)) => {
                        // same posture as SaveTransport: after the clean-close
                        // merge sealed the file, the close path's own flush has
                        // already landed (enqueued before the merge) — ignore
                        // stragglers rather than disturb the merged crypto
                        if crypto_sealed {
                            tracing::debug!("ignoring a post-merge accept-window save");
                        } else {
                            let mut ts = ws.read_transport_state();
                            ts.accepted = accepted;
                            if let Err(e) = ws.write_transport_state(&ts) {
                                fail(&failed_flag, "accept-window write", &e);
                            }
                        }
                    }
                    Ok(WriterMsg::SaveFetchJob(job)) => {
                        if crypto_sealed {
                            tracing::debug!("ignoring a post-merge fetch job save");
                        } else {
                            let mut ts = ws.read_transport_state();
                            match ts.file_jobs.fetch.iter_mut().find(|j| j.series == job.series) {
                                Some(slot) => *slot = *job,
                                None => ts.file_jobs.fetch.push(*job),
                            }
                            if let Err(e) = ws.write_transport_state(&ts) {
                                fail(&failed_flag, "fetch job write", &e);
                            }
                        }
                    }
                    Ok(WriterMsg::SaveMirrorJob(series, job)) => {
                        if !crypto_sealed {
                            let mut ts = ws.read_transport_state();
                            ts.mirror.jobs.insert(series, *job);
                            if let Err(e) = ws.write_transport_state(&ts) {
                                fail(&failed_flag, "mirror job write", &e);
                            }
                        }
                    }
                    Ok(WriterMsg::RemoveMirrorJob(series)) => {
                        if !crypto_sealed {
                            let mut ts = ws.read_transport_state();
                            if ts.mirror.jobs.remove(&series).is_some() {
                                if let Err(e) = ws.write_transport_state(&ts) {
                                    fail(&failed_flag, "mirror job write", &e);
                                }
                            }
                        }
                    }
                    Ok(WriterMsg::RemoveFetchJob(series)) => {
                        if !crypto_sealed {
                            let mut ts = ws.read_transport_state();
                            let before = ts.file_jobs.fetch.len();
                            ts.file_jobs.fetch.retain(|j| j.series != series);
                            if before != ts.file_jobs.fetch.len() {
                                if let Err(e) = ws.write_transport_state(&ts) {
                                    fail(&failed_flag, "fetch job write", &e);
                                }
                            }
                        }
                    }
                    Ok(WriterMsg::MergeCrypto { mls, smp_queues, mesh, seal, ack }) => {
                        let mut ts = ws.read_transport_state();
                        if mls.is_some() {
                            ts.mls = mls;
                        }
                        if smp_queues.is_some() {
                            ts.smp_queues = smp_queues;
                        }
                        if let Some(mesh) = mesh {
                            ts.mesh = mesh;
                        }
                        let ok = match ws.write_transport_state(&ts).and_then(|()| ws.sync()) {
                            Ok(()) => true,
                            Err(e) => {
                                fail(&failed_flag, "crypto merge write", &e);
                                false
                            }
                        };
                        // only the CLEAN-CLOSE merge seals — a live mesh-extension
                        // merge is followed by a rebuilt supervisor that keeps
                        // saving its cursors
                        if seal {
                            crypto_sealed = true;
                        }
                        let _ = ack.send(ok);
                    }
                    Ok(WriterMsg::LoadTransport(reply)) => {
                        let _ = reply.send(ws.read_transport_state());
                    }
                    Ok(WriterMsg::Flush(ack)) => {
                        let ok = match ws.sync() {
                            Ok(()) => true,
                            Err(e) => {
                                fail(&failed_flag, "flush", &e);
                                false
                            }
                        };
                        let _ = ack.send(ok);
                    }
                    Ok(WriterMsg::Compact { snapshot, holding_peers, ack }) => {
                        let outcome = match compact_once(&mut ws, &snapshot, &holding_peers) {
                            Ok(o) => o,
                            Err(e) => {
                                // compaction is hygiene, never correctness: a
                                // failed round leaves the log exactly as it
                                // was and the next one retries — so it is a
                                // warning, never the fatal storage flag
                                tracing::warn!(error = %e, "log compaction failed - next round retries");
                                CompactionOutcome::default()
                            }
                        };
                        let _ = ack.send(outcome);
                    }
                    Ok(WriterMsg::PersistChain { blob, blocks, ack }) => {
                        // WP4b: raise the manifest version BEFORE writing a
                        // pruned chain.state (total-review fix). Bumping
                        // first is strictly safe — a crash in between only
                        // OVER-describes (an old binary refuses a still-full
                        // workspace: availability loss). The reverse order
                        // left a window where a pruned chain.state sat under
                        // the old version, and an old binary would run
                        // chainless on it (a governance fork).
                        let mut ok = true;
                        if blob.is_some() {
                            if let Err(e) = ws.bump_pruned_version() {
                                fail(&failed_flag, "manifest version bump", &e);
                                ok = false;
                            }
                        }
                        if let Err(e) =
                            ws.write_chain(blob.as_ref(), &blocks).and_then(|()| ws.sync())
                        {
                            fail(&failed_flag, "chain.state write", &e);
                            ok = false;
                        }
                        let _ = ack.send(ok);
                    }
                    Ok(WriterMsg::Snapshot(snap)) => {
                        if let Err(e) = ws.sync().and_then(|()| ws.write_snapshot(&snap)) {
                            fail(&failed_flag, "snapshot", &e);
                        }
                        dirty = false;
                        last_sync = Instant::now();
                    }
                    Ok(WriterMsg::Close(ack)) => {
                        if let Err(e) = ws.sync() {
                            fail(&failed_flag, "closing fsync", &e);
                        }
                        close_ack = Some(ack);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(e) = ws.sync() {
                            fail(&failed_flag, "fsync", &e);
                        }
                        dirty = false;
                        last_sync = Instant::now();
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = ws.sync();
                        break;
                    }
                }
            }
            // the LOCK must be released before the closer is acked —
            // otherwise an immediate reopen races the drop and sees Busy
            drop(ws);
            if let Some(ack) = close_ack {
                let _ = ack.send(());
            }
        })
        .expect("spawning the storage writer thread");
    StorageHandle { tx, failed, transport_gate: Arc::new(tokio::sync::Mutex::new(())) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{ChatMessage, MemberId, MemberIdentity};
    use std::collections::BTreeMap;

    fn ids() -> Vec<MemberIdentity> {
        vec![
            MemberIdentity {
                member: "founder".into(),
                identity_pk: "aa".repeat(32),
                nostr_pk: "dd".repeat(32),
            },
            MemberIdentity {
                member: "juno".into(),
                identity_pk: "bb".repeat(32),
                nostr_pk: "ee".repeat(32),
            },
            MemberIdentity {
                member: "mira".into(),
                identity_pk: "cc".repeat(32),
                nostr_pk: "ff".repeat(32),
            },
        ]
    }

    /// N1 PIN — `molt-republic-id-v2` binds identity/nostr anchor PAIRS
    /// (sorted by identity_pk): `sha256(tag ‖ name ‖ 0 ‖ m ‖ n ‖ per sorted
    /// pair (0 ‖ identity_pk ‖ 0 ‖ nostr_pk))`. Fixture computed
    /// INDEPENDENTLY (python hashlib) — the pairing lives inside the sorted
    /// unit so a founder cannot permute nostr anchors against identity keys
    /// without changing the id.
    #[test]
    fn republic_id_v2_binds_anchor_pairs() {
        let ids = vec![
            MemberIdentity {
                member: "ada".to_string(),
                identity_pk: "aa".repeat(32),
                nostr_pk: "cc".repeat(32),
            },
            MemberIdentity {
                member: "bob".to_string(),
                identity_pk: "bb".repeat(32),
                nostr_pk: "dd".repeat(32),
            },
        ];
        let id = republic_id("R", 1, 2, &ids);
        assert_eq!(
            id, "a0414686dbfce13d1967053e6892a9d0dfb4d2fa16ce672cbb407818ec8f91b9",
            "independently computed v2 fixture"
        );
        // permuting the nostr anchors between seats changes the id
        let mut permuted = ids.clone();
        permuted[0].nostr_pk = "dd".repeat(32);
        permuted[1].nostr_pk = "cc".repeat(32);
        assert_ne!(republic_id("R", 1, 2, &permuted), id);
        // roster-order independence survives v2 (pairs are sorted)
        let reversed: Vec<_> = ids.iter().rev().cloned().collect();
        assert_eq!(republic_id("R", 1, 2, &reversed), id);
    }

    /// The republic id must be INJECTIVE over arbitrary field content, not
    /// just over hex. A member supplies its own `nostr_pk`, so a hostile one
    /// can put NULs (and anything else) in it; with v1's separator-only
    /// layout that let a 2-seat roster's preimage equal a 3-seat roster's
    /// with an attacker identity spliced in — and `republic_id` is exactly
    /// what a pruned-chain holder recomputes to reject a forged genesis
    /// (`verify_suffix_chain`). Length prefixes make the splice impossible
    /// regardless of ingest validation.
    #[test]
    fn republic_id_v2_resists_a_spliced_nostr_anchor() {
        let seat = |idpk: &str, npk: &str| MemberIdentity {
            member: "x".to_string(),
            identity_pk: idpk.to_string(),
            nostr_pk: npk.to_string(),
        };
        // a crafted anchor that CONTINUES the hash stream of the old layout:
        // <64hex> NUL <evil identity> NUL <evil anchor>
        let spliced_anchor = format!("{}\0{}\0{}", "bb".repeat(32), "33".repeat(32), "ee".repeat(32));
        let crafted = vec![
            seat(&"11".repeat(32), &"aa".repeat(32)),
            seat(&"22".repeat(32), &spliced_anchor),
        ];
        let forged_table = vec![
            seat(&"11".repeat(32), &"aa".repeat(32)),
            seat(&"22".repeat(32), &"bb".repeat(32)),
            seat(&"33".repeat(32), &"ee".repeat(32)),
        ];
        assert_ne!(
            republic_id("Club", 2, 2, &crafted),
            republic_id("Club", 2, 2, &forged_table),
            "a spliced anchor must not collide with a larger founding table"
        );
        // the same holds for the boundary between the two fields of ONE pair
        let a = vec![seat("aabb", "ccdd")];
        let b = vec![seat("aa", "bbccdd")];
        assert_ne!(republic_id("Club", 1, 1, &a), republic_id("Club", 1, 1, &b));
        // …and for the name/roster boundary
        assert_ne!(
            republic_id("Club", 1, 1, &[seat("aa", "bb")]),
            republic_id("Clu", 1, 1, &[seat("baa", "bb")]),
        );
    }

    #[test]
    fn republic_id_is_order_independent_and_deterministic() {
        let a = ids();
        let mut rev = a.clone();
        rev.reverse();
        // the load-bearing property: every member computes the SAME id no
        // matter what order its local identity table happens to be in
        assert_eq!(
            republic_id("Chess Club", 2, 3, &a),
            republic_id("Chess Club", 2, 3, &rev),
        );
        // deterministic across calls
        assert_eq!(republic_id("Chess Club", 2, 3, &a), republic_id("Chess Club", 2, 3, &a));
    }

    #[test]
    fn republic_id_changes_with_name_rules_and_keys() {
        let base = republic_id("Chess Club", 2, 3, &ids());
        assert_ne!(base, republic_id("Chess Clubs", 2, 3, &ids()), "name matters");
        assert_ne!(base, republic_id("Chess Club", 3, 3, &ids()), "m matters");
        assert_ne!(base, republic_id("Chess Club", 2, 4, &ids()), "n matters");
        let mut changed = ids();
        changed[0].identity_pk = "dd".repeat(32);
        assert_ne!(base, republic_id("Chess Club", 2, 3, &changed), "a key matters");
    }

    fn founded(seq_ts: u64) -> EventEnvelope {
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: seq_ts,
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
                relays: Vec::new(),
                features: None,
            },
        }
    }

    /// A deterministic non-nil message id for hand-built test envelopes.
    fn test_msg_id(seq: u64) -> molt_core::MessageId {
        let mut b = [0xa5u8; 16];
        b[..8].copy_from_slice(&seq.to_le_bytes());
        molt_core::MessageId(b)
    }

    fn chat(seq: u64, body: &str) -> EventEnvelope {
        EventEnvelope { prev_seq: 0,
            seq,
            ts: 1_000_000 + seq,
            by: "mithra".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage {
                id: test_msg_id(seq),
                from: "mithra".to_string(),
                body: body.to_string(),
                ts: 1_000_000 + seq,
                quote: None,
                quote_id: None,
                channel: molt_core::ChannelRef::Group,
                kind: molt_core::ChatKind::User,
                reactions: BTreeMap::new(),
                read_by: Default::default(),
                deleted_by: None,
                file: None,
            }),
        }
    }

    fn make_ws(root: &Path, extra_events: u64) -> PathBuf {
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(root, &seed, &founded(42)).expect("create");
        for i in 0..extra_events {
            ws.append(&chat(2 + i, &format!("msg {i}"))).expect("append");
        }
        ws.sync().expect("sync");
        ws.dir().to_path_buf()
    }

    /// S3 (review 2026-08-25): a tail whose every offset reads as a
    /// plausible frame length cost one CRC over that length per offset —
    /// quadratic, hours under the LOCK. The scan is budgeted and answers
    /// "cannot classify" instead.
    #[test]
    fn a_torn_tail_scan_is_budgeted() {
        // every 4-byte word says "length 1 MiB"; the next word (the CRC
        // slot) never matches
        let word = 0x0010_0000u32.to_le_bytes();
        let data: Vec<u8> = word.iter().copied().cycle().take(2 * 1024 * 1024).collect();
        let started = std::time::Instant::now();
        assert!(!has_valid_frame_after(&data, 0));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the scan must give up within its budget"
        );
    }

    /// S5: a planted file with a reserved number (the AAD markers at the
    /// top of `u64`) is ignored — it must neither become the active
    /// segment under a marker's domain nor overflow its successor.
    #[test]
    fn planted_reserved_numbers_are_ignored() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = make_ws(tmp.path(), 2);
        std::fs::write(dir.join("log").join(format!("{}.mlog", u64::MAX)), b"").expect("plant");
        std::fs::write(
            dir.join("snapshots").join(format!("{}.msnap", u64::MAX)),
            b"",
        )
        .expect("plant");
        let (ws, loaded) = open_workspace(&dir).expect("opens despite the planted files");
        assert_eq!(loaded.tail.len(), 3);
        assert!(ws.seg_no < KEYS_SEGMENT);
    }

    /// S2: once the key table exists, a keyless segment BELOW its highest
    /// entry was erased by a drop — a file that reappears must not be
    /// minted a fresh key (the next open would fail on it).
    #[test]
    fn migration_does_not_resurrect_an_erased_segment() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = make_ws(tmp.path(), 0);
        let (mut ws, _) = open_workspace(&dir).expect("open");
        rotate_n(&mut ws, 1, 2);
        ws.migrate_to_segment_keys().expect("migrate");
        // simulate the drop's key erasure while the file stays behind
        let mut table = ws.seg_keys.clone().expect("table");
        assert!(table.dek(1).is_some());
        table.forget(1);
        segkeys::write_table(&ws.dir, &ws.key, &ws.id, &table).expect("write");
        ws.seg_keys = Some(table);
        ws.migrate_to_segment_keys().expect("migrate again");
        assert!(
            ws.seg_keys.as_ref().and_then(|t| t.dek(1)).is_none(),
            "an erased segment stays erased"
        );
        assert!(ws.seg_keys.as_ref().and_then(|t| t.dek(2)).is_some());
    }

    /// S4: a migrated log is unreadable for a pre-WP4a binary — the
    /// version gate rises with the migration, not only with the first drop.
    #[test]
    fn migration_raises_the_version_gate() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = make_ws(tmp.path(), 1);
        let (mut ws, _) = open_workspace(&dir).expect("open");
        ws.migrate_to_segment_keys().expect("migrate");
        drop(ws);
        assert_eq!(
            read_manifest(&dir).expect("manifest").version,
            molt_core::STORAGE_VERSION_PRUNED
        );
    }

    /// S8: a leftover tmp file keeps its own mode (`mode` applies at
    /// creation only) — the writer starts fresh every time.
    #[cfg(unix)]
    #[test]
    fn write_atomic_never_reuses_a_leftover_tmp_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("ws");
        std::fs::create_dir_all(dir.join("tmp")).expect("tmp dir");
        let leftover = dir.join("tmp").join("keys_workspace.key");
        std::fs::write(&leftover, b"old").expect("leftover");
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_atomic(&dir, "keys/workspace.key", b"new", true).expect("write");
        let mode = std::fs::metadata(dir.join("keys/workspace.key"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// S7: the DEK never reaches Debug output.
    #[test]
    fn a_segment_key_debugs_without_its_dek() {
        let key = segkeys::SegmentKey { no: 7, first_seq: 1, dek: [0xabu8; 32] };
        let shown = format!("{key:?}");
        assert!(shown.contains("no: 7"));
        assert!(!shown.contains("ab, ab") && !shown.contains("171"), "{shown}");
    }

    /// A compacted workspace — segments under their own keys, segment 1
    /// DROPPED, the genesis living only in the snapshot — exports and
    /// imports (review 2026-08-26: the import decrypted frame 1 under the
    /// workspace key and required the segment, so every backup of a
    /// compacted republic was unrestorable).
    #[test]
    fn a_compacted_workspace_exports_and_imports() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let dir = make_ws(&root, 0);
        {
            let (mut ws, _) = open_workspace(&dir).expect("open");
            rotate_n(&mut ws, 2, 2);
            ws.migrate_to_segment_keys().expect("migrate");
            cover_with_snapshot(&mut ws);
            assert!(ws.drop_segments_below(u64::MAX).expect("drop") >= 1);
            assert!(!dir.join("log").join(segment_name(1)).exists(), "segment 1 is gone");
        }
        let pass = "a passphrase long enough for the export";
        let mut blob = Vec::new();
        crate::export::export_dir(&root, &dir, &crate::export::ExportKey::passphrase(pass), &mut blob)
            .expect("export");
        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");
        let staging = crate::import::import_stage(&dest_root, &blob, pass).expect("stage");
        assert_eq!(staging.genesis.seq, 1, "the genesis comes from the snapshot");
        let imported = staging.commit(&dest_root, false, None).expect("commit");
        let (_ws, loaded) = open_workspace(&imported).expect("the import opens");
        assert!(loaded.snapshot.is_some(), "restored from the snapshot");
    }

    /// The lighter twin: migrated (frame 1 under the segment's DEK) but not
    /// dropped — the import must read the table, not assume the workspace key.
    #[test]
    fn a_migrated_workspace_exports_and_imports() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let dir = make_ws(&root, 2);
        {
            let (mut ws, _) = open_workspace(&dir).expect("open");
            ws.migrate_to_segment_keys().expect("migrate");
        }
        let pass = "a passphrase long enough for the export";
        let mut blob = Vec::new();
        crate::export::export_dir(&root, &dir, &crate::export::ExportKey::passphrase(pass), &mut blob)
            .expect("export");
        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");
        let staging = crate::import::import_stage(&dest_root, &blob, pass).expect("stage");
        let imported = staging.commit(&dest_root, false, None).expect("commit");
        let (_ws, loaded) = open_workspace(&imported).expect("the import opens");
        assert_eq!(loaded.tail.len(), 3);
    }

    /// A KNOWLEDGE ARCHIVE (what an MCP client may export) carries no
    /// recovery seed and imports SEALED: reading it needs the phrase the
    /// human holds, and blob + passphrase never becomes a seat (MCP audit
    /// 2026-08-26 H1).
    #[test]
    fn an_archive_export_carries_no_seed_and_imports_sealed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let phrase = generate_seed_phrase().expect("gen");
        let seed = seed_entropy(&phrase).expect("entropy");
        let dir = {
            let mut ws = create_workspace(&root, &seed, &founded(42)).expect("create");
            ws.append(&chat(2, "knowledge")).expect("append");
            ws.sync().expect("sync");
            ws.dir().to_path_buf()
        };
        let pass = "a passphrase long enough for the export";
        let mut blob = Vec::new();
        crate::export::export_archive(&root, &dir, &crate::export::ExportKey::passphrase(pass), &mut blob)
            .expect("export");
        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");
        let staging = crate::import::import_stage(&dest_root, &blob, pass).expect("stage");
        assert!(staging.seed_entropy().is_none(), "no seed in an archive");
        assert_eq!(staging.at_rest, molt_core::SEALED_PHRASE);
        let imported = staging.commit(&dest_root, false, None).expect("commit");
        assert!(
            is_sealed(&read_manifest(&imported).expect("manifest")),
            "the import is sealed - the phrase opens it"
        );
        assert!(open_workspace(&imported).is_err(), "no keys on disk");
        crate::sealing::unseal_at_rest(&dest_root, &imported, &phrase).expect("the phrase opens it");
        let (_ws, loaded) = open_workspace(&imported).expect("open after unseal");
        assert_eq!(loaded.tail.len(), 2, "the knowledge is all there");
    }

    /// Append until the active segment has rotated `n` times — the compactor
    /// can only drop WHOLE segments, so every drop test needs a multi-segment
    /// log. Rotation is size-driven in production (8 MiB); driving it
    /// directly keeps the test to a handful of frames.
    fn rotate_n(ws: &mut OpenedWorkspace, n: usize, per_segment: u64) {
        for _ in 0..n {
            for _ in 0..per_segment {
                let seq = ws.next_seq;
                ws.append(&chat(seq, &format!("seg {} msg {seq}", ws.seg_no)))
                    .expect("append");
            }
            ws.rotate().expect("rotate");
        }
        for _ in 0..per_segment {
            let seq = ws.next_seq;
            ws.append(&chat(seq, &format!("tail msg {seq}"))).expect("append");
        }
        ws.sync().expect("sync");
    }

    /// Write a snapshot covering the whole log so far — what the real
    /// compactor always does BEFORE dropping anything (R1). The drop tests
    /// exercise the primitives directly, so they have to do it themselves.
    fn cover_with_snapshot(ws: &mut OpenedWorkspace) {
        // a real snapshot carries the genesis-derived facts (the genesis is
        // before it and never replayed) — the empty default would make the
        // compacted workspace look rosterless, which is not what production
        // writes
        let state = molt_core::EngineStateDump {
            name: "Chess Club".to_string(),
            member: "mithra".to_string(),
            rule_m: 2,
            roster: vec!["mithra".to_string(), "anahita".to_string()],
            founded_ts: 42,
            ..molt_core::EngineStateDump::default()
        };
        let snap = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq: ws.next_seq - 1,
            state,
        };
        ws.write_snapshot(&snap).expect("snapshot");
    }

    /// **WP4a F6 migration.** Putting every segment under its own data key is
    /// invisible to the log: the same events replay, at the same seqs, and the
    /// workspace reopens exactly as before. It is also idempotent — running it
    /// twice changes nothing.
    #[test]
    fn migrating_to_segment_keys_keeps_the_log_replaying_identically() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 2, 3);
        let dir = ws.dir().to_path_buf();
        let before: Vec<EventEnvelope> = ws.read_log_from(1).expect("read");
        assert_eq!(before.len(), 10, "genesis + 3 segments à 3 messages");

        ws.migrate_to_segment_keys().expect("migrate");
        ws.migrate_to_segment_keys().expect("migrate again (idempotent)");
        assert_eq!(ws.read_log_from(1).expect("read"), before, "the live handle still reads it");
        drop(ws);

        let (ws, loaded) = open_workspace(&dir).expect("reopen");
        assert_eq!(ws.read_log_from(1).expect("read"), before, "and so does a fresh open");
        assert_eq!(loaded.compaction_floor, 0, "migration alone drops nothing");
        assert_eq!(ws.next_seq, 11, "the append position is unchanged");
        // the segments really are under their own keys now: the workspace key
        // no longer opens a frame
        let (no, path) = list_sorted(&dir.join("log"), ".mlog").remove(0);
        let data = fs::read(&path).expect("segment bytes");
        let (frames, _) = split_frames(&data);
        assert!(
            decrypt_frame(&ws.key, &ws.id, no, 1, frames[0].nonce, frames[0].ciphertext).is_err(),
            "a migrated segment is no longer readable with the workspace key"
        );
    }

    /// **WP4a §A.4: dropping a segment erases its key first, then unlinks the
    /// file** — and what survives replays from the floor. This is the whole
    /// point of compaction: after it, the dropped content is not merely
    /// unreachable but undecryptable, while the surviving log opens normally
    /// at its own starting seq.
    #[test]
    fn dropping_segments_erases_their_keys_and_the_rest_replays_from_the_floor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 2, 3);
        let dir = ws.dir().to_path_buf();
        ws.migrate_to_segment_keys().expect("migrate");
        cover_with_snapshot(&mut ws);
        // keep a copy of segment 1's bytes: after the drop they must be
        // undecryptable even if the file itself is recovered
        let seg1_path = dir.join("log").join(segment_name(1));
        let seg1_bytes = fs::read(&seg1_path).expect("segment 1");

        // segment 1 holds seqs 1..=4 (genesis + 3), segment 2 starts at 5
        let dropped = ws.drop_segments_below(4).expect("drop");
        assert_eq!(dropped, 1, "exactly the fully covered segment goes");
        assert_eq!(ws.compaction_floor(), 4);
        assert!(!seg1_path.exists(), "the file is unlinked");
        // …and the ACTIVE segment is never dropped, however high the floor
        assert_eq!(ws.drop_segments_below(u64::MAX).expect("drop the rest"), 1);
        assert!(ws.next_seq > ws.compaction_floor(), "the active segment survives");
        drop(ws);

        let (ws, loaded) = open_workspace(&dir).expect("reopen after compaction");
        assert_eq!(loaded.compaction_floor, 7, "the floor persisted");
        let rest = ws.read_log_from(1).expect("read");
        assert_eq!(
            rest.first().map(|e| e.seq),
            Some(8),
            "the surviving log starts above the floor"
        );
        assert_eq!(ws.next_seq, 11, "the append position is untouched by compaction");

        // the erasure is real: the recovered bytes of segment 1 no longer
        // decrypt under ANY key this workspace still holds
        let (frames, _) = split_frames(&seg1_bytes);
        assert!(
            decrypt_frame(&ws.key, &ws.id, 1, 1, frames[0].nonce, frames[0].ciphertext).is_err(),
            "recovered bytes of a dropped segment stay undecryptable"
        );
    }

    /// A crash between erasing a segment's key (§A.4 step 3a) and unlinking
    /// its file (3b) must be harmless: the orphan is not part of the log, and
    /// the next open sweeps it. The reverse order — unlink first — would leave
    /// a live key for bytes still on the medium, which is why it is not used.
    #[test]
    fn a_keyless_orphan_segment_is_ignored_and_swept_on_the_next_open() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 1, 3);
        let dir = ws.dir().to_path_buf();
        ws.migrate_to_segment_keys().expect("migrate");
        cover_with_snapshot(&mut ws);
        let seg1 = dir.join("log").join(segment_name(1));
        let bytes = fs::read(&seg1).expect("segment 1");
        ws.drop_segments_below(4).expect("drop");
        drop(ws);
        // the crash: the file comes back (an interrupted unlink, a restored
        // backup copy) while its key stays erased
        fs::write(&seg1, &bytes).expect("resurrect the file");

        let (ws, loaded) = open_workspace(&dir).expect("reopen");
        assert_eq!(loaded.compaction_floor, 4);
        assert!(!seg1.exists(), "the keyless orphan is swept");
        assert_eq!(
            ws.read_log_from(1).expect("read").first().map(|e| e.seq),
            Some(5),
            "and was never part of the replayed log"
        );
    }

    /// **One full compaction round through the real writer** (WP4a §A.4): the
    /// trimmed snapshot lands first, the floor respects the slowest peer that
    /// is still inside its grace (R2), the covered segments go, and the
    /// manifest version rises so an older binary refuses the compacted
    /// workspace instead of reading it as damaged.
    #[test]
    fn a_compaction_round_writes_the_snapshot_then_drops_what_nobody_needs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 3, 3); // segments 1..4, seqs 1..=13
        let dir = ws.dir().to_path_buf();
        // two peers: one still around but far behind, one long gone
        let mut ts = TransportState::default();
        ts.outbound.insert(
            "slowpoke".to_string(),
            molt_core::OutboundCursor { log_seq: 5, wire_seq: 1, ..Default::default() },
        );
        ts.outbound.insert(
            "ghost".to_string(),
            molt_core::OutboundCursor { log_seq: 2, wire_seq: 1, ..Default::default() },
        );
        ws.write_transport_state(&ts).expect("transport state");
        let at_seq = ws.next_seq - 1;
        let handle = start_writer(ws);

        // the ghost is past its peer grace and no longer holds the log back;
        // the slow peer does — so the floor is ITS cursor, not the snapshot
        let snap = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq,
            state: molt_core::EngineStateDump::default(),
        };
        let out = handle.compact_blocking(snap, vec!["slowpoke".to_string()]);
        assert_eq!(out.floor, 4, "the floor stops at the slow peer's covered segment");
        assert_eq!(out.segments_dropped, 1, "only segment 1 is fully below it");
        handle.close(None);

        let manifest = read_manifest(&dir).expect("manifest");
        assert_eq!(
            manifest.version,
            molt_core::STORAGE_VERSION_PRUNED,
            "a compacted workspace stops older binaries at the gate"
        );
        let (ws, loaded) = open_workspace(&dir).expect("reopen");
        assert_eq!(loaded.compaction_floor, 4);
        assert_eq!(
            ws.read_log_from(1).expect("read").first().map(|e| e.seq),
            Some(5),
            "the surviving log starts right above the floor"
        );
        assert_eq!(ws.next_seq, at_seq + 1, "the append position is untouched");
    }

    /// Delivery guarantee §4.9: a proven-acking peer holds the log down to
    /// its ACKED floor, not to the (further-ahead) send cursor — the unacked
    /// tail must stay resendable across the rebuild rewind. An old peer
    /// (never acked) keeps the plain cursor gate.
    #[test]
    fn an_acking_peer_holds_the_log_down_to_its_acked_floor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(43)).expect("create");
        rotate_n(&mut ws, 3, 3); // segments 1..4, seqs 1..=13
        let mut ts = TransportState::default();
        // everything SENT (cursor at the head), but only seqs ≤ 5 CONFIRMED
        ts.outbound.insert(
            "acker".to_string(),
            molt_core::OutboundCursor {
                log_seq: 13,
                wire_seq: 9,
                acked_floor: 5,
                ack_seen: true,
                resend_epoch: 1,
            },
        );
        ws.write_transport_state(&ts).expect("transport state");
        let at_seq = ws.next_seq - 1;
        let handle = start_writer(ws);
        let snap = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq,
            state: molt_core::EngineStateDump::default(),
        };
        let out = handle.compact_blocking(snap, vec!["acker".to_string()]);
        assert_eq!(
            out.floor, 4,
            "the ACKED floor gates the round (send cursor 13 would have dropped \
             three segments) — the unacked tail stays resendable"
        );
        assert_eq!(out.segments_dropped, 1, "only the fully-confirmed segment goes");
        handle.close(None);
    }

    /// A round with no eligible terrain must be a **no-op**, not a partial
    /// migration: a peer sitting at cursor 0 (never delivered anything) holds
    /// the whole log, so nothing is dropped and nothing is rewritten.
    #[test]
    fn a_compaction_round_that_can_drop_nothing_changes_nothing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 1, 3);
        let dir = ws.dir().to_path_buf();
        let mut ts = TransportState::default();
        ts.outbound.insert(
            "fresh-peer".to_string(),
            molt_core::OutboundCursor { log_seq: 0, wire_seq: 0, ..Default::default() },
        );
        ws.write_transport_state(&ts).expect("transport state");
        let at_seq = ws.next_seq - 1;
        let handle = start_writer(ws);
        let snap = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq,
            state: molt_core::EngineStateDump::default(),
        };
        let out = handle.compact_blocking(snap, vec!["fresh-peer".to_string()]);
        assert_eq!(out, CompactionOutcome::default(), "nothing dropped, no floor");
        handle.close(None);
        assert!(
            !dir.join(segkeys::KEYS_FILE).exists(),
            "a no-op round does not even migrate the log"
        );
        assert_eq!(
            read_manifest(&dir).expect("manifest").version,
            molt_core::STORAGE_VERSION,
            "and leaves older binaries able to open it"
        );
    }

    /// **A compacted log with no snapshot covering what was dropped must not
    /// open.** The dropped history lived only in that snapshot; replaying the
    /// surviving tail alone would present a PARTIAL state as complete — the
    /// one failure mode compaction must never have. (`compact_once` writes the
    /// snapshot first, so this is the guard against a torn/lost one.)
    #[test]
    fn a_compacted_log_without_a_covering_snapshot_refuses_to_open() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 1, 3);
        let dir = ws.dir().to_path_buf();
        ws.migrate_to_segment_keys().expect("migrate");
        cover_with_snapshot(&mut ws);
        ws.drop_segments_below(4).expect("drop");
        drop(ws);
        // the covering snapshot is lost (torn write, partial restore)
        for (_, path) in list_sorted(&dir.join("snapshots"), ".msnap") {
            fs::remove_file(path).expect("remove snapshot");
        }
        let err = match open_workspace(&dir) {
            Err(e) => e,
            Ok(_) => panic!("a partial state must not open"),
        };
        assert!(
            err.to_string().contains("compacted"),
            "the error names the cause: {err}"
        );
    }

    /// **A peer inside its grace with NO cursor at all must stop the round.**
    /// "We have no record of ever delivering to it" is the opposite of "it has
    /// everything" — treating an absent cursor as satisfied would drop the log
    /// out from under exactly the peers that still need it (a fresh member, or
    /// one whose `transport.state` was lost, which the design elsewhere calls
    /// merely a cause for resends).
    #[test]
    fn a_holding_peer_without_a_cursor_stops_the_round() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 2, 3);
        let dir = ws.dir().to_path_buf();
        // one peer HAS delivered far along; the other has no cursor at all
        let mut ts = TransportState::default();
        ts.outbound.insert(
            "delivered".to_string(),
            molt_core::OutboundCursor { log_seq: 9, wire_seq: 3, ..Default::default() },
        );
        ws.write_transport_state(&ts).expect("transport state");
        let at_seq = ws.next_seq - 1;
        let handle = start_writer(ws);
        let snap = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq,
            state: molt_core::EngineStateDump::default(),
        };
        let out = handle.compact_blocking(
            snap,
            vec!["delivered".to_string(), "never-heard-from".to_string()],
        );
        assert_eq!(out, CompactionOutcome::default(), "the cursorless peer holds everything");
        handle.close(None);
        assert!(
            !dir.join(segkeys::KEYS_FILE).exists(),
            "and the log is not even migrated"
        );
    }

    /// **The Open screen must still know the republic after a compaction.**
    /// `peek_genesis` reads the log's first frame; compaction re-keys that
    /// segment and eventually drops it entirely, which would leave the
    /// workspace list without a roster or charter for exactly the long-lived
    /// workspaces. Both stages are covered: keyed segment, then snapshot.
    #[test]
    fn the_genesis_stays_peekable_across_migration_and_drop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        rotate_n(&mut ws, 2, 3);
        let dir = ws.dir().to_path_buf();
        let id = ws.manifest.workspace.id.clone();
        let roster_of = |env: &EventEnvelope| match &env.body {
            WorkspaceEvent::Founded { roster, .. } => roster.clone(),
            _ => panic!("not a genesis"),
        };
        let before = peek_genesis(tmp.path(), &dir, &id).expect("peek before");

        // 1) after the migration the frame lives under the segment key
        ws.migrate_to_segment_keys().expect("migrate");
        let after_migrate = peek_genesis(tmp.path(), &dir, &id).expect("peek after migrate");
        assert_eq!(roster_of(&after_migrate), roster_of(&before));

        // 2) after the drop it comes from the snapshot, with the same facts
        cover_with_snapshot(&mut ws);
        assert_eq!(ws.drop_segments_below(4).expect("drop"), 1, "segment 1 goes");
        drop(ws);
        let after_drop = peek_genesis(tmp.path(), &dir, &id).expect("peek after drop");
        assert_eq!(
            roster_of(&after_drop),
            roster_of(&before),
            "the roster survives the loss of the genesis frame"
        );
    }

    /// The table is the log's map: a version from a newer build must stop the
    /// open, never make this build guess which segments it may drop.
    #[test]
    fn a_newer_key_table_refuses_the_open() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        ws.migrate_to_segment_keys().expect("migrate");
        let dir = ws.dir().to_path_buf();
        let (key, id) = (*ws.key, ws.id);
        let mut table = segkeys::read_table(&dir, &key, &id)
            .expect("read")
            .expect("a migrated workspace has a table");
        table.version = segkeys::KEYS_VERSION + 1;
        segkeys::write_table(&dir, &key, &id, &table).expect("write");
        drop(ws);
        assert!(
            matches!(open_workspace(&dir), Err(StorageError::NewerVersion(_))),
            "a too-new key table stops the open"
        );
    }

    /// **H3: "blocks until durable" now answers whether it IS durable.**
    ///
    /// The blocking persists acked unconditionally — a failed write, or a
    /// writer that was already gone, both reported success. The engine then
    /// broadcast a threshold-signed block, or closed believing its ratchet
    /// snapshot was on disk, with nothing written. The ack carries the
    /// outcome now, and `#[must_use]` is what makes every call site say what
    /// it does with it.
    ///
    /// A gone writer is the case a test can produce without breaking a
    /// filesystem, and it is the honest one to pin: nothing was written, so
    /// nothing may be claimed.
    #[test]
    fn a_gone_writer_reports_failure_instead_of_durability() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let created = create_workspace(tmp.path(), &seed, &founded(7)).expect("create");
        let handle = start_writer(created);
        assert!(handle.flush_blocking(), "a live writer really does flush");
        handle.clone().close(None);

        assert!(
            !handle.flush_blocking(),
            "a flush that reached no writer is not a flush"
        );
        assert!(
            !handle.persist_chain_blocking(None, Vec::new()),
            "a chain that reached no writer is not persisted - the caller \
             must not broadcast on the strength of it"
        );
        assert!(
            !handle.persist_crypto_blocking(Some(b"mls".to_vec()), None),
            "…and neither is a ratchet snapshot"
        );
    }

    /// **M5: a `transport.state` from a newer build stops the open, instead
    /// of being silently rewritten at this build's version.**
    ///
    /// The read path answers "start fresh" for anything it cannot use — the
    /// right call for a ratchet, which re-establishes. But a NEWER file is
    /// not damage: the first save rewrote it under the old version, taking
    /// the MLS ratchet and the transport identity's non-re-derivable secret
    /// with it. The additive-only rule says an older reader meeting
    /// something unknown must not WRITE; here that means it must not open.
    #[test]
    fn a_newer_transport_state_refuses_the_open() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        let dir = ws.dir().to_path_buf();
        let (key, id) = (*ws.key, ws.id);
        // …written the way a future build would write it
        let st = TransportState {
            version: TRANSPORT_STATE_VERSION + 1,
            ..TransportState::default()
        };
        let plaintext = serde_json::to_vec(&st).expect("encode");
        let tkey = hkdf32(&key, "molt-transport-state", &id);
        let frame = encode_frame(&tkey, &id, TRANSPORT_SEGMENT, 0, &plaintext).expect("frame");
        drop(ws);
        write_atomic(&dir, "transport.state", &frame, true).expect("write");

        assert!(
            matches!(open_workspace(&dir), Err(StorageError::NewerVersion(v)) if v == TRANSPORT_STATE_VERSION + 1),
            "a too-new transport.state stops the open"
        );
        // …and the file is still there, untouched, for the build that wrote it
        let after = fs::read(dir.join("transport.state")).expect("still there");
        assert_eq!(after, frame, "the refusal must not have rewritten it");
    }

    /// WP4b stage 5: the FIRST pruned chain persist raises the manifest
    /// version, so an OLDER binary refuses the whole workspace instead of
    /// running chainless on a partial view (additive-only stop). Unpruned
    /// workspaces keep the old version — old binaries read them unchanged.
    #[test]
    fn a_pruned_persist_raises_the_manifest_version_gate() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = create_workspace(tmp.path(), &seed, &founded(42)).expect("create");
        assert_eq!(ws.manifest.version, molt_core::STORAGE_VERSION);
        // a FULL chain persist does not bump (old binaries keep reading)
        ws.write_chain(None, &[]).expect("full write");
        assert_eq!(ws.manifest.version, molt_core::STORAGE_VERSION);
        // the first PRUNED persist bumps — through the real writer wiring
        // (the production trigger), not a direct method call
        let dir = ws.dir().to_path_buf();
        let handle = start_writer(ws);
        let blob = molt_core::CheckpointState {
            founding_name: "t".to_string(),
            rule_m: 1,
            rule_n: 1,
            founding_identities: Vec::new(),
            agenda: String::new(),
            founding_features: None,
            republic_id: String::new(),
            roster: Vec::new(),
            applied: Vec::new(),
            consumed_ids: Vec::new(),
            anchors: Vec::new(),
            member_relays: Vec::new(),
            upto: 0,
            relays: Vec::new(),
        };
        assert!(handle.persist_chain_blocking(Some(blob), Vec::new()), "durable");
        handle.close(None);
        let on_disk = read_manifest(&dir).expect("manifest reads");
        assert_eq!(on_disk.version, molt_core::STORAGE_VERSION_PRUNED);
        let mut m = read_manifest(&dir).expect("manifest");
        // one past everything this build understands (S6 raised the ceiling
        // to STORAGE_VERSION_SEALED)
        m.version = molt_core::STORAGE_VERSION_SEALED + 1;
        {
            let text = toml::to_string_pretty(&m).expect("render");
            fs::write(dir.join("manifest.toml"), text).expect("write");
        }
        assert!(
            open_workspace(&dir).is_err(),
            "a too-new manifest version refuses the workspace"
        );
    }

    #[test]
    fn seed_phrase_is_24_words_and_roundtrips() {
        let phrase = generate_seed_phrase().expect("gen");
        assert_eq!(phrase.split(' ').count(), 24);
        let entropy = seed_entropy(&phrase).expect("parse");
        assert_eq!(entropy.len(), 32, "the 32-byte root of the key hierarchy");
        assert!(seed_entropy("amber basalt cedar").is_err());
        // two generations never collide
        assert_ne!(phrase, generate_seed_phrase().expect("gen2"));
    }

    #[test]
    fn the_seed_phrase_is_stored_sealed_and_reads_back() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let phrase = generate_seed_phrase().expect("gen");
        let seed = seed_entropy(&phrase).expect("entropy");
        let ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        let dir = ws.dir().to_path_buf();
        let id_hex = ws.manifest.workspace.id.clone();
        drop(ws);
        assert!(dir.join("keys").join("seed.sealed").exists());
        assert_eq!(
            read_sealed_seed(&root, &dir, &id_hex).as_deref(),
            Some(phrase.as_str())
        );
    }

    #[test]
    fn a_missing_tampered_or_foreign_sealed_seed_reads_as_none() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let phrase = generate_seed_phrase().expect("gen");
        let seed = seed_entropy(&phrase).expect("entropy");
        let ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        let dir = ws.dir().to_path_buf();
        let id_hex = ws.manifest.workspace.id.clone();
        drop(ws);
        let sealed = dir.join("keys").join("seed.sealed");

        // the sealed workspace KEY must not unseal as a seed (distinct
        // AAD domain — otherwise the two blobs would be interchangeable)
        let key_blob = fs::read(dir.join("keys").join("workspace.key")).expect("read key");
        let seed_blob = fs::read(&sealed).expect("read seed");
        fs::write(&sealed, &key_blob).expect("swap");
        assert_eq!(read_sealed_seed(&root, &dir, &id_hex), None);

        // a truncated/tampered blob reads as None, never panics
        fs::write(&sealed, &seed_blob[..10]).expect("truncate");
        assert_eq!(read_sealed_seed(&root, &dir, &id_hex), None);

        // a pre-seed-storage workspace (no file) reads as None
        fs::remove_file(&sealed).expect("rm");
        assert_eq!(read_sealed_seed(&root, &dir, &id_hex), None);

        // a foreign device key cannot unseal the phrase
        fs::write(&sealed, &seed_blob).expect("restore");
        fs::write(device_key_path(&root), [9u8; 32]).expect("swap device key");
        assert_eq!(read_sealed_seed(&root, &dir, &id_hex), None);
    }

    #[test]
    fn peek_genesis_reads_frame_one_without_a_replay() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        // extra chat frames prove the peek stops at the genesis
        let dir = make_ws(&root, 3);
        let manifest = read_manifest(&dir).expect("manifest");
        let id_hex = manifest.workspace.id.clone();

        let genesis = peek_genesis(&root, &dir, &id_hex).expect("peek");
        assert_eq!(genesis.seq, 1);
        match genesis.body {
            WorkspaceEvent::Founded { ref name, .. } => assert_eq!(name, "Chess Club"),
            ref other => panic!("expected Founded, got {other:?}"),
        }

        // a foreign device key cannot peek
        fs::write(device_key_path(&root), [9u8; 32]).expect("swap device key");
        assert!(peek_genesis(&root, &dir, &id_hex).is_none());
    }

    #[test]
    fn identity_key_derives_deterministically_and_signs() {
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let id = derive_workspace_id(&seed, "petra");
        let (sk, pk) = derive_identity_key(&seed, &id);
        // deterministic: the phrase re-derives the same key
        let (_sk2, pk2) = derive_identity_key(&seed, &id);
        assert_eq!(pk, pk2);
        assert_eq!(pk.len(), 64);
        // per-workspace: a different id yields a different identity
        let other = derive_workspace_id(&seed, "walter");
        assert_ne!(pk, derive_identity_key(&seed, &other).1);
        // sign/verify round-trips; tampered message or wrong key fail
        let msg = b"the canonical roster table";
        let sig = identity_sign(&sk, msg);
        assert!(identity_verify(&pk, msg, &sig));
        assert!(!identity_verify(&pk, b"other bytes", &sig));
        assert!(!identity_verify(&derive_identity_key(&seed, &other).1, msg, &sig));
        // malformed inputs never panic
        assert!(!identity_verify("zz", msg, &sig));
        assert!(!identity_verify(&pk, msg, "beef"));
    }

    #[test]
    fn derivations_are_deterministic_and_member_scoped() {
        let seed = [7u8; 16];
        let a = derive_workspace_id(&seed, "mithra");
        assert_eq!(a.len(), 64);
        assert_eq!(a, derive_workspace_id(&seed, "mithra"));
        // same DAO under two member identities => two distinct ids
        assert_ne!(a, derive_workspace_id(&seed, "anahita"));
        let k = derive_workspace_key(&seed, &a);
        assert_eq!(k, derive_workspace_key(&seed, &a));
        assert_ne!(k[..], derive_workspace_key(&seed, &derive_workspace_id(&seed, "anahita"))[..]);
    }

    #[test]
    fn create_then_open_replays_the_full_history() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 3);

        let (ws, loaded) = open_workspace(&dir).expect("open");
        assert_eq!(ws.next_seq, 5);
        assert!(loaded.snapshot.is_none());
        assert_eq!(loaded.unknown_events, 0);
        assert_eq!(loaded.tail.len(), 4);
        assert!(matches!(
            loaded.tail[0].body,
            WorkspaceEvent::Founded { .. }
        ));
        assert!(matches!(loaded.tail[3].body, WorkspaceEvent::Chat(ref m) if m.body == "msg 2"));
        assert_eq!(ws.manifest.workspace.name, "Chess Club");
    }

    #[test]
    fn second_open_gets_busy_with_the_holder_pid() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 0);
        let (_ws, _loaded) = open_workspace(&dir).expect("open");
        match open_workspace(&dir) {
            Err(StorageError::Busy(pid)) => {
                assert_eq!(pid, std::process::id().to_string());
            }
            other => panic!("expected Busy, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn truncate_anywhere_recovers_the_maximal_valid_prefix() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 2); // 3 frames total
        let seg = dir.join("log").join(segment_name(1));
        let full = fs::read(&seg).expect("read");
        let (frames, torn) = split_frames(&full);
        assert_eq!(frames.len(), 3);
        assert!(torn.is_none());
        let boundaries: Vec<usize> = frames.iter().map(|f| f.end).collect();

        for cut in 0..=full.len() {
            fs::write(&seg, &full[..cut]).expect("chop");
            let (ws, loaded) = open_workspace(&dir).expect("open never panics");
            // the recovered history is the maximal prefix of whole frames
            let want = boundaries.iter().filter(|b| **b <= cut).count();
            assert_eq!(loaded.tail.len(), want, "cut at {cut}");
            assert_eq!(ws.next_seq, u64::try_from(want).expect("small") + 1);
            drop(ws);
            // the truncation is persistent: the file now ends on a boundary
            let after = fs::read(&seg).expect("reread");
            assert_eq!(after.len(), boundaries[..want].last().copied().unwrap_or(0));
            fs::write(&seg, &full).expect("restore");
        }
    }

    /// **M6: damage with VALID frames behind it is not a torn tail, and
    /// truncating it destroys already-acked history.**
    ///
    /// The recovery treated the last segment's first invalid byte as the
    /// start of a torn write and cut the file there — so one flipped bit
    /// early in an 8 MiB segment silently discarded every good frame after
    /// it, and `open_workspace` then "succeeded" on the shortened history.
    ///
    /// The two cases are distinguishable, and cheaply: the writer only ever
    /// APPENDS, so a torn append leaves a partial frame at the END of the
    /// file with nothing behind it. Anything valid behind the damage means
    /// the file was complete and something corrupted it in place — the same
    /// situation the middle segments already refuse to guess about.
    #[test]
    fn bitrot_with_good_frames_behind_it_is_refused_not_truncated() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 2); // 3 frames in one segment
        let seg = dir.join("log").join(segment_name(1));
        let full = fs::read(&seg).expect("read");
        let (frames, torn) = split_frames(&full);
        assert_eq!(frames.len(), 3);
        assert!(torn.is_none());

        // flip a bit INSIDE the first frame's ciphertext: its CRC now fails,
        // and frames 2 and 3 are still perfectly good behind it
        let mut rotted = full.clone();
        let victim = FRAME_HEADER_LEN + NONCE_LEN + 1;
        rotted[victim] ^= 0b0000_1000;
        fs::write(&seg, &rotted).expect("rot");

        match open_workspace(&dir) {
            Err(StorageError::Corrupt(_)) => {}
            other => panic!(
                "damage with history behind it must be refused, got {:?}",
                other.map(|_| ())
            ),
        }
        // …and above all: the good frames are STILL THERE
        let after = fs::read(&seg).expect("reread");
        assert_eq!(
            after.len(),
            rotted.len(),
            "the refusal must not have truncated anything away"
        );
    }

    #[test]
    fn transplanted_frames_fail_authentication() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 2);
        let seg = dir.join("log").join(segment_name(1));
        let full = fs::read(&seg).expect("read");
        let (frames, _) = split_frames(&full);
        let (a, b) = (frames[1].end, frames[2].end);
        // swap frames 2 and 3: structurally valid (crc intact), but the AAD
        // binds each frame to its seq — the AEAD open must fail
        let mut swapped = full[..frames[0].end].to_vec();
        swapped.extend_from_slice(&full[a..b]);
        swapped.extend_from_slice(&full[frames[0].end..a]);
        fs::write(&seg, &swapped).expect("swap");
        match open_workspace(&dir) {
            Err(StorageError::Crypto(_)) => {}
            other => panic!("expected Crypto error, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn snapshot_plus_tail_equals_replay_from_zero() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        for i in 0..4u64 {
            ws.append(&chat(2 + i, &format!("m{i}"))).expect("append");
        }
        ws.sync().expect("sync");
        // snapshot at seq 3 (pretend state), then two more events
        let snap = WorkspaceSnapshot {
            version: STORAGE_VERSION,
            at_seq: 3,
            state: molt_core::EngineStateDump {
                name: "Chess Club".to_string(),
                member: "mithra".to_string(),
                ..Default::default()
            },
        };
        ws.write_snapshot(&snap).expect("snapshot");
        drop(ws);

        let (ws, loaded) = open_workspace(&root.join(workspace_dirname(
            "Chess Club",
            &derive_workspace_id(&seed, "mithra"),
        )))
        .expect("open");
        let got = loaded.snapshot.expect("snapshot loaded");
        assert_eq!(got.at_seq, 3);
        assert_eq!(got.state.name, "Chess Club");
        // tail holds only seq 4 and 5
        let seqs: Vec<u64> = loaded.tail.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4, 5]);
        assert_eq!(ws.next_seq, 6);
    }

    #[test]
    fn snapshots_are_pruned_to_the_newest_two() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        for at in [1u64, 2, 3] {
            let snap = WorkspaceSnapshot {
                version: STORAGE_VERSION,
                at_seq: at,
                state: molt_core::EngineStateDump::default(),
            };
            // at_seq beyond the log is fine for this pruning-only test
            ws.write_snapshot(&snap).expect("snapshot");
        }
        let files = list_sorted(&ws.dir().join("snapshots"), ".msnap");
        let nos: Vec<u64> = files.iter().map(|(n, _)| *n).collect();
        assert_eq!(nos, vec![2, 3]);
    }

    #[test]
    fn unknown_event_variants_are_counted_not_fatal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        // simulate a newer node: append a frame whose body variant we don't know
        let raw = serde_json::json!({
            "seq": 2, "ts": 5, "by": "x",
            "body": { "type": "from_the_future", "x": 1 }
        });
        let plaintext = serde_json::to_vec(&raw).expect("encode");
        let frame = encode_frame(
            &derive_workspace_key(&seed, &ws.manifest.workspace.id),
            &id_bytes(&ws.manifest.workspace.id).expect("id"),
            1,
            2,
            &plaintext,
        )
        .expect("frame");
        use std::io::Write as _;
        let mut f = OpenOptions::new()
            .append(true)
            .open(ws.dir().join("log").join(segment_name(1)))
            .expect("open seg");
        f.write_all(&frame).expect("write");
        f.sync_all().expect("sync");
        let dir = ws.dir().to_path_buf();
        drop(ws);

        let (ws, loaded) = open_workspace(&dir).expect("open");
        assert_eq!(loaded.unknown_events, 1);
        assert_eq!(loaded.tail.len(), 1); // only the genesis is applicable
        assert_eq!(ws.next_seq, 3, "the unknown frame still occupies seq 2");
    }

    #[test]
    fn manifest_version_gate_refuses_newer_workspaces() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 0);
        let manifest_path = dir.join("manifest.toml");
        let text = fs::read_to_string(&manifest_path)
            .expect("read")
            .replace("version = 1", "version = 99");
        fs::write(&manifest_path, text).expect("write");
        // still listable (forward compatibility of the list screen) …
        let entries = scan_workspaces(&root);
        assert_eq!(entries.len(), 1);
        // … but not openable
        match open_workspace(&dir) {
            Err(StorageError::NewerVersion(99)) => {}
            other => panic!("expected NewerVersion, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn wrong_device_key_cannot_unseal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 0);
        // rotate the device key out from under the workspace
        fs::remove_file(device_key_path(&root)).expect("rm");
        let _ = load_or_create_device_key(&device_key_path(&root)).expect("new key");
        match open_workspace(&dir) {
            Err(StorageError::Crypto(msg)) => assert!(msg.contains("device key")),
            other => panic!("expected Crypto, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn workspace_size_kib_sums_recursively_and_rounds_up() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("ws");
        fs::create_dir_all(dir.join("sub")).expect("mkdir");
        fs::write(dir.join("a.bin"), vec![0u8; 1500]).expect("write a");
        fs::write(dir.join("sub").join("b.bin"), vec![0u8; 600]).expect("write b");
        // 2100 bytes round UP to 3 KiB — a non-empty dir is never under-reported
        assert_eq!(workspace_size_kib(&dir), 3);
        // exactly on a KiB boundary there is nothing to round
        fs::write(dir.join("a.bin"), vec![0u8; 1448]).expect("rewrite a");
        assert_eq!(workspace_size_kib(&dir), 2, "1448 + 600 = 2048 = 2 KiB");
        // a dir that vanished (or never existed) reports 0 instead of failing
        assert_eq!(workspace_size_kib(&tmp.path().join("gone")), 0);
        // an empty dir occupies no KiB
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).expect("mkdir empty");
        assert_eq!(workspace_size_kib(&empty), 0);
        // symlinks are never followed: a link to a large outside dir must not
        // inflate the footprint, and a self-link must not recurse forever
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            fs::create_dir_all(&outside).expect("mkdir outside");
            fs::write(outside.join("big.bin"), vec![0u8; 8192]).expect("write big");
            std::os::unix::fs::symlink(&outside, dir.join("link-out")).expect("link out");
            std::os::unix::fs::symlink(&dir, dir.join("link-self")).expect("link self");
            assert_eq!(workspace_size_kib(&dir), 2, "links contribute nothing");
        }
    }

    #[test]
    fn scan_lists_trash_hides_and_purge_expires() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let dir = make_ws(&root, 1);
        let entries = scan_workspaces(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].manifest.workspace.name, "Chess Club");
        assert!(entries[0].size_kib > 0);
        // the manifest does not persist a network label, so a scanned entry
        // must not claim one — the app stamps the effective global setting
        // (a hardcoded "tor" here mislabeled every workspace after a restart)
        assert_eq!(entries[0].info().net, "");
        let id = entries[0].manifest.workspace.id.clone();
        assert_eq!(find_workspace_dir(&root, &id), Some(dir.clone()));

        let trashed = trash_workspace(&root, &dir).expect("trash");
        assert!(scan_workspaces(&root).is_empty());
        assert!(trashed.exists());
        // young entries survive a purge, expired ones do not
        purge_trash(&root, TRASH_MAX_AGE_SECS);
        assert!(trashed.exists());
        purge_trash(&root, 0);
        assert!(!trashed.exists());
    }

    #[test]
    fn writer_thread_appends_and_closes_durably() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let created = create_workspace(&root, &seed, &founded(42)).expect("create");
        let dir = created.dir().to_path_buf();
        let handle = start_writer(created);
        for i in 0..5u64 {
            assert!(handle.append(chat(2 + i, &format!("t{i}"))));
        }
        let by: MemberId = "mithra".to_string();
        handle.close(Some(WorkspaceSnapshot {
            version: STORAGE_VERSION,
            at_seq: 6,
            state: molt_core::EngineStateDump {
                member: by,
                ..Default::default()
            },
        }));

        let (ws, loaded) = open_workspace(&dir).expect("reopen");
        assert_eq!(ws.next_seq, 7);
        assert_eq!(loaded.snapshot.expect("snap").at_seq, 6);
        assert!(loaded.tail.is_empty()); // everything is under the snapshot floor
    }

    /// The clean-close crypto merge (MLS snapshot + queue creds) must NOT clobber
    /// the delivery cursors the supervisor left — else a reopen re-sends all
    /// history and the peer re-applies it as duplicates.
    #[test]
    fn merge_crypto_preserves_delivery_cursors() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let created = create_workspace(&root, &seed, &founded(1)).expect("create");
        let dir = created.dir().to_path_buf();
        let handle = start_writer(created);

        // the supervisor persisted delivery cursors (with the stale load-time MLS)
        let mut outbound = std::collections::BTreeMap::new();
        outbound.insert(
            "bob".to_string(),
            molt_core::OutboundCursor { log_seq: 7, wire_seq: 3, ..Default::default() },
        );
        let mut inbound = std::collections::BTreeMap::new();
        inbound.insert("bob".to_string(), 5u64);
        handle.save_transport_state(TransportState {
            outbound,
            inbound,
            ..TransportState::default()
        });

        // a clean close merges the advanced MLS + queue creds — cursors survive
        assert!(
            handle.persist_crypto_blocking(Some(b"mls-blob".to_vec()), Some(b"queue-creds".to_vec())),
            "durable"
        );

        // a supervisor task winding down enqueues one last stale save (its
        // in-memory state still has the load-time MLS = None and no creds). The
        // writer is SEALED by the merge and must ignore it — else reopen loses
        // the mesh (the exact close-persist race).
        handle.save_transport_state(TransportState::default());
        handle.close(None);

        let (ws, _loaded) = open_workspace(&dir).expect("reopen");
        let ts = ws.read_transport_state();
        assert_eq!(
            ts.mls.as_deref(),
            Some(b"mls-blob".as_slice()),
            "MLS merged and NOT clobbered by the late save"
        );
        assert_eq!(
            ts.smp_queues.as_deref(),
            Some(b"queue-creds".as_slice()),
            "queue creds merged and NOT clobbered by the late save"
        );
        assert_eq!(
            ts.outbound.get("bob").map(|c| (c.log_seq, c.wire_seq)),
            Some((7, 3)),
            "outbound cursor preserved through the merge"
        );
        assert_eq!(ts.inbound.get("bob").copied(), Some(5), "inbound cursor preserved");
    }

    /// Delivery guarantee §4.7: the ENGINE-owned accept windows and the
    /// SUPERVISOR-owned cursors overlay only their own fields — a cursor
    /// save never clobbers the windows, a window save never clobbers the
    /// cursors, and the clean-close flush (enqueued before the merge) lands
    /// while a post-seal straggler is ignored.
    #[test]
    fn accept_window_saves_and_cursor_saves_do_not_clobber_each_other() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let created = create_workspace(&root, &seed, &founded(1)).expect("create");
        let dir = created.dir().to_path_buf();
        let handle = start_writer(created);

        // the engine persists an accept window for bob…
        let mut win = molt_core::AcceptedWindow::default();
        assert!(win.accept(41));
        assert!(win.accept(43));
        let mut accepted = std::collections::BTreeMap::new();
        accepted.insert("bob".to_string(), win.clone());
        handle.save_accepted(accepted.clone());

        // …then the supervisor saves cursors (its clone knows no windows)
        let mut outbound = std::collections::BTreeMap::new();
        outbound.insert(
            "bob".to_string(),
            molt_core::OutboundCursor { log_seq: 7, wire_seq: 3, ..Default::default() },
        );
        handle.save_transport_state(TransportState {
            outbound,
            ..TransportState::default()
        });

        // …the engine flushes a GROWN window right before the close merge
        assert!(win.accept(44));
        accepted.insert("bob".to_string(), win);
        handle.save_accepted(accepted);
        assert!(
            handle.persist_crypto_blocking(Some(b"mls-blob".to_vec()), Some(b"creds".to_vec())),
            "durable"
        );
        // a straggler after the seal is ignored
        handle.save_accepted(std::collections::BTreeMap::new());
        handle.close(None);

        let (ws, _loaded) = open_workspace(&dir).expect("reopen");
        let ts = ws.read_transport_state();
        let bob = ts.accepted.get("bob").expect("bob's window survived");
        assert_eq!(bob.high, 44, "the flushed (grown) window landed");
        assert!(bob.is_accepted(41) && bob.is_accepted(43));
        assert!(!bob.is_accepted(42), "unseen seq stays unaccepted");
        assert_eq!(
            ts.outbound.get("bob").map(|c| c.log_seq),
            Some(7),
            "cursor save survived the window saves"
        );
        assert_eq!(ts.mls.as_deref(), Some(b"mls-blob".as_slice()), "merge landed");
    }

    /// **A live mesh-extension merge must survive later cursor saves.** The
    /// live merge (dynamic mesh membership) deliberately does NOT seal the
    /// state — the rebuilt supervisor keeps saving cursors afterwards. Those
    /// saves come from a full in-memory `TransportState` clone the supervisor
    /// loaded BEFORE the merge, so a save must only carry the supervisor-owned
    /// cursor maps into the file — never write its stale mls/mesh/creds copies
    /// back over the merged values.
    #[test]
    fn a_cursor_save_never_clobbers_a_live_crypto_merge() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let created = create_workspace(&root, &seed, &founded(1)).expect("create");
        let dir = created.dir().to_path_buf();
        let handle = start_writer(created);

        // the extension merges the grown mesh + fresh crypto, LIVE (no seal)
        let grown = vec![molt_core::MeshLink {
            member: "bob".to_string(),
            snd_server: "smp://fp@host".to_string(),
            snd_queue: "aa".to_string(),
            snd_wrap: "bb".to_string(),
            rcv_server: String::new(),
            rcv_queue: "cc".to_string(),
            rcv_wrap: "dd".to_string(),
            snd_extra: Vec::new(),
            rcv_extra: Vec::new(),
        }];
        assert!(
            handle.persist_mesh_crypto_blocking(
                Some(b"fresh-mls".to_vec()),
                Some(b"fresh-creds".to_vec()),
                grown.clone(),
            ),
            "durable"
        );

        // the rebuilt supervisor saves a cursor advance from its PRE-merge
        // in-memory clone (mls/mesh/creds all stale/empty in that clone)
        let mut outbound = std::collections::BTreeMap::new();
        outbound.insert(
            "bob".to_string(),
            molt_core::OutboundCursor { log_seq: 9, wire_seq: 4, ..Default::default() },
        );
        let mut inbound = std::collections::BTreeMap::new();
        inbound.insert("bob".to_string(), 6u64);
        handle.save_transport_state(TransportState {
            outbound,
            inbound,
            ..TransportState::default()
        });
        handle.close(None);

        let (ws, _loaded) = open_workspace(&dir).expect("reopen");
        let ts = ws.read_transport_state();
        assert_eq!(
            ts.mesh, grown,
            "the grown mesh survives a later cursor save"
        );
        assert_eq!(
            ts.mls.as_deref(),
            Some(b"fresh-mls".as_slice()),
            "the merged MLS snapshot survives a later cursor save"
        );
        assert_eq!(
            ts.smp_queues.as_deref(),
            Some(b"fresh-creds".as_slice()),
            "the merged queue creds survive a later cursor save"
        );
        assert_eq!(
            ts.outbound.get("bob").map(|c| (c.log_seq, c.wire_seq)),
            Some((9, 4)),
            "the cursor save itself lands"
        );
        assert_eq!(ts.inbound.get("bob").copied(), Some(6));
    }

    /// S11: `transport.state` is decoded ONCE per open handle. Every
    /// cursor / accept-window save merges into that cached copy — the file
    /// carries the whole MLS ratchet blob, and re-decrypting it for each
    /// of the supervisor's frequent saves was the largest read a save did.
    /// Proven by clobbering the file behind the open handle: a re-decrypt
    /// would now read garbage (and fall back to the default).
    #[test]
    fn a_cursor_save_reads_the_cached_transport_state() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = create_workspace(&root, &seed, &founded(1)).expect("create");
        ws.write_transport_state(&TransportState {
            mls: Some(b"ratchet".to_vec()),
            ..TransportState::default()
        })
        .expect("write");
        fs::write(ws.dir().join("transport.state"), b"clobbered behind the handle")
            .expect("clobber");
        assert_eq!(
            ws.read_transport_state().mls.as_deref(),
            Some(b"ratchet".as_slice()),
            "a read after a write is served from the cached state, not a re-decrypt"
        );
        drop(ws);
        // …and a fresh open decodes the file exactly once, at the open
        let dir = find_workspace_dir(&root, &derive_workspace_id(&seed, "mithra")).expect("dir");
        let (ws, _loaded) = open_workspace(&dir).expect("open");
        assert!(ws.read_transport_state().mls.is_none(), "the clobbered file reads as fresh");
        ws.write_transport_state(&TransportState {
            mls: Some(b"again".to_vec()),
            ..TransportState::default()
        })
        .expect("write");
        fs::write(ws.dir().join("transport.state"), b"clobbered again").expect("clobber");
        assert_eq!(ws.read_transport_state().mls.as_deref(), Some(b"again".as_slice()));
    }

    /// A snapshot pointing past the surviving log (partial dir copy, old
    /// backup) must not poison the append position — it is skipped and the
    /// state rebuilds from the log alone.
    #[test]
    fn snapshot_ahead_of_the_log_is_skipped() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
        let mut ws = create_workspace(&root, &seed, &founded(42)).expect("create");
        ws.append(&chat(2, "only real event")).expect("append");
        ws.sync().expect("sync");
        ws.write_snapshot(&WorkspaceSnapshot {
            version: STORAGE_VERSION,
            at_seq: 999, // claims history the log does not hold
            state: molt_core::EngineStateDump::default(),
        })
        .expect("snapshot");
        let dir = ws.dir().to_path_buf();
        drop(ws);

        let (mut ws, loaded) = open_workspace(&dir).expect("open");
        assert!(loaded.snapshot.is_none(), "the phantom snapshot is ignored");
        assert_eq!(loaded.tail.len(), 2);
        assert_eq!(ws.next_seq, 3, "append continues exactly after the log");
        // and the workspace keeps working: append + reopen round-trips
        ws.append(&chat(3, "after recovery")).expect("append");
        ws.sync().expect("sync");
        drop(ws);
        let (ws, loaded) = open_workspace(&dir).expect("reopen");
        assert_eq!(loaded.tail.len(), 3);
        assert_eq!(ws.next_seq, 4);
    }

    #[test]
    fn slugs_and_dirnames_are_tame() {
        assert_eq!(slugify("Family Office"), "family-office");
        assert_eq!(slugify("  Ünïcode!! DAO  "), "ünïcode-dao");
        assert_eq!(slugify("///"), "workspace");
        let id = "a1b2c3d4e5f6";
        assert_eq!(workspace_dirname("Family Office", id), "family-office.a1b2c3");
    }
}
