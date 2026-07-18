// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-storage`: the on-disk reality of a workspace.
//!
//! Implements `documents/concept-workspace-storage.md`: a workspace directory
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
pub use sealing::{is_sealed, seal_at_rest, unseal_at_rest};

pub mod export;

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

/// Segment rotation threshold (~8 MiB keeps recovery scans and future
/// S3 diff-uploads bounded).
pub const SEGMENT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Snapshots kept per workspace (newest N; older ones are deleted).
pub const SNAPSHOTS_KEPT: usize = 2;
/// Upper bound a frame's `len` field may claim (corruption guard).
const FRAME_MAX_LEN: u32 = 64 * 1024 * 1024;
/// The XChaCha20 nonce size.
const NONCE_LEN: usize = 24;
/// Frame header: len(4) + crc(4).
const FRAME_HEADER_LEN: usize = 8;
/// AAD segment number that marks a snapshot frame (never a real segment).
const SNAPSHOT_SEGMENT: u64 = u64::MAX;
/// AAD segment number that marks the `transport.state` frame.
const TRANSPORT_SEGMENT: u64 = u64::MAX - 1;
/// AAD segment number that marks the `chain.state` frame (the persistent
/// commit-block chain — `documents/persistent_chain.md`).
const CHAIN_SEGMENT: u64 = u64::MAX - 2;

/// The on-disk shape of `chain.state` (WP4b): historically a bare block
/// array; a PRUNED holder stores the checkpoint blob next to its suffix.
/// Untagged: an array parses as `Full`, an object as `Pruned` — old files
/// keep reading, old code meets the unknown `Checkpoint` variant inside a
/// pruned file's blocks and refuses (additive-only rule).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
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
    #[error("workspace is sealed at rest — decrypt it with its recovery phrase first")]
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
/// the same roster. `hex(SHA-256("molt-republic-id-v1\0" ‖ name ‖ 0 ‖ m ‖ n
/// ‖ each sorted identity pk, 0-separated))`.
pub fn republic_id(
    name: &str,
    rule_m: u8,
    rule_n: u8,
    identities: &[molt_core::MemberIdentity],
) -> String {
    use sha2::Digest;
    let mut pks: Vec<&str> = identities.iter().map(|i| i.identity_pk.as_str()).collect();
    pks.sort_unstable();
    let mut h = Sha256::new_with_prefix(b"molt-republic-id-v1\0");
    h.update(name.as_bytes());
    h.update([0u8, rule_m, rule_n]);
    for pk in pks {
        h.update([0u8]);
        h.update(pk.as_bytes());
    }
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
    match fs::read(path) {
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

/// Seal the workspace key to the device key (`nonce || ciphertext`; the
/// workspace id is the AAD, binding the blob to its workspace).
fn seal_workspace_key(
    device_key: &[u8; 32],
    id: &[u8; 32],
    ws_key: &[u8; 32],
) -> Result<Vec<u8>, StorageError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new(device_key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: ws_key,
                aad: id,
            },
        )
        .map_err(|_| StorageError::Crypto("sealing the workspace key failed".to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Unseal `keys/workspace.key` with the device key.
fn unseal_workspace_key(
    device_key: &[u8; 32],
    id: &[u8; 32],
    blob: &[u8],
) -> Result<[u8; 32], StorageError> {
    if blob.len() <= NONCE_LEN {
        return Err(StorageError::BadFile(
            "sealed workspace key is too short".to_string(),
        ));
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(device_key.into());
    let pt = cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: id })
        .map_err(|_| {
            StorageError::Crypto(
                "unsealing the workspace key failed (wrong device key?)".to_string(),
            )
        })?;
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

/// Seal the recovery-seed entropy to the device key (`nonce || ciphertext`,
/// AAD [`seed_seal_aad`]). Stored so the details panel can show the phrase
/// of an at-rest-unencrypted workspace (decision 2026-07-15); the opt-in
/// passphrase sealing (S6) removes the file.
fn seal_seed_entropy(
    device_key: &[u8; 32],
    id: &[u8; 32],
    entropy: &[u8],
) -> Result<Vec<u8>, StorageError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| StorageError::Crypto(format!("os rng unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new(device_key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: entropy,
                aad: &seed_seal_aad(id),
            },
        )
        .map_err(|_| StorageError::Crypto("sealing the seed failed".to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
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
    if blob.len() <= NONCE_LEN {
        return Err(StorageError::BadFile("sealed seed is too short".to_string()));
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(device_key.into());
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: &seed_seal_aad(id),
            },
        )
        .map_err(|_| StorageError::Crypto("unsealing the stored seed failed".to_string()))
}

/// Read a workspace's recovery phrase back from `keys/seed.sealed`.
/// `None` for anything that isn't a healthy sealed seed — absent file
/// (pre-seed-storage workspace), foreign device key, tampered blob —
/// the Open screen shows an honest "not stored" instead of failing.
pub fn read_sealed_seed(root: &Path, ws_dir: &Path, id_hex: &str) -> Option<String> {
    let blob = match fs::read(ws_dir.join("keys").join("seed.sealed")) {
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
    let sealed = fs::read(ws_dir.join(&manifest.crypto.key_file)).ok()?;
    let device_key = load_or_create_device_key(&device_key_path(root)).ok()?;
    let key = unseal_workspace_key(&device_key, &id, &sealed).ok()?;
    let data = fs::read(ws_dir.join("log").join(segment_name(1))).ok()?;
    let (frames, _torn) = split_frames(&data);
    let first = frames.first()?;
    let plaintext = decrypt_frame(&key, &id, 1, 1, first.nonce, first.ciphertext).ok()?;
    serde_json::from_slice(&plaintext).ok()
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
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
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
    // loss can undo the rename even though the data blocks were synced
    if let Some(parent) = target.parent() {
        if let Ok(d) = File::open(parent) {
            let _ = d.sync_all();
        }
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
    let text = fs::read_to_string(&path)?;
    let m: WorkspaceManifest = toml::from_str(&text)
        .map_err(|e| StorageError::BadFile(format!("{}: {e}", path.display())))?;
    if m.format != MANIFEST_FORMAT {
        return Err(StorageError::BadFile(format!(
            "{} is not a workspace manifest (format `{}`)",
            path.display(),
            m.format
        )));
    }
    Ok(m)
}

fn write_manifest(ws_dir: &Path, m: &WorkspaceManifest) -> Result<(), StorageError> {
    let text = toml::to_string_pretty(m)
        .map_err(|e| StorageError::BadFile(format!("rendering manifest: {e}")))?;
    write_atomic(ws_dir, "manifest.toml", text.as_bytes(), false)
}

/// Read a workspace's `prefs.toml`; a missing or broken file falls back to
/// defaults (prefs are local convenience, never history).
pub fn read_prefs(ws_dir: &Path) -> WorkspacePrefs {
    let path = ws_dir.join("prefs.toml");
    match fs::read_to_string(&path) {
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
            let holder = fs::read_to_string(&path).unwrap_or_default();
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
}

/// An open (locked) workspace directory: the append handle of the active
/// segment plus everything needed to frame, encrypt and rotate.
pub struct OpenedWorkspace {
    dir: PathBuf,
    /// The plaintext identity card.
    pub manifest: WorkspaceManifest,
    /// The local node preferences.
    pub prefs: WorkspacePrefs,
    key: [u8; 32],
    id: [u8; 32],
    _lock: WorkspaceLock,
    seg_no: u64,
    seg: File,
    seg_len: u64,
    /// The next seq this log expects (strictly monotonic).
    pub next_seq: u64,
    /// Unsynced appends are pending.
    dirty: bool,
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
        let frame = encode_frame(&self.key, &self.id, self.seg_no, env.seq, &plaintext)?;
        self.seg.write_all(&frame)?;
        self.seg_len += u64::try_from(frame.len()).unwrap_or(u64::MAX);
        self.next_seq += 1;
        self.dirty = true;
        Ok(())
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
        self.seg_no += 1;
        let path = self.dir.join("log").join(segment_name(self.seg_no));
        self.seg = OpenOptions::new().append(true).create_new(true).open(path)?;
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
            if fs::read(&path).is_ok_and(|have| have == bytes) {
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

    /// The `transport.state` sub-key: derived from the workspace key, so
    /// the file shares the workspace's protection without reusing its key.
    fn transport_key(&self) -> [u8; 32] {
        hkdf32(&self.key, "molt-transport-state", &self.id)
    }

    /// The `chain.state` sub-key (distinct HKDF tag from the transport key).
    fn chain_key(&self) -> [u8; 32] {
        hkdf32(&self.key, "molt-chain-state", &self.id)
    }

    /// Read `transport.state` (transport concept §6): node-local encrypted
    /// transport bookkeeping. Absent, damaged or newer-versioned files fall
    /// back to defaults — losing this file costs resends (the peers' dedup
    /// absorbs them), never history.
    pub fn read_transport_state(&self) -> TransportState {
        let path = self.dir.join("transport.state");
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TransportState::default(),
            Err(e) => {
                tracing::warn!(error = %e, "reading transport.state failed — starting fresh");
                return TransportState::default();
            }
        };
        let (frames, torn) = split_frames(&data);
        if frames.len() != 1 || torn.is_some() {
            tracing::warn!("transport.state framing is damaged — starting fresh");
            return TransportState::default();
        }
        let plaintext = match decrypt_frame(
            &self.transport_key(),
            &self.id,
            TRANSPORT_SEGMENT,
            0,
            frames[0].nonce,
            frames[0].ciphertext,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "transport.state does not authenticate — starting fresh");
                return TransportState::default();
            }
        };
        match serde_json::from_slice::<TransportState>(&plaintext) {
            Ok(st) if st.version <= TRANSPORT_STATE_VERSION => st,
            Ok(st) => {
                tracing::warn!(
                    version = st.version,
                    "transport.state was written by a newer node — starting fresh (safe: resends dedup)"
                );
                TransportState::default()
            }
            Err(e) => {
                tracing::warn!(error = %e, "transport.state decode failed — starting fresh");
                TransportState::default()
            }
        }
    }

    /// Rewrite `transport.state` atomically (via `tmp/`, mode 0600), old
    /// content discarded — this file must never accrete history (from T2
    /// it holds ratchet state whose deletion IS forward secrecy).
    pub fn write_transport_state(&self, state: &TransportState) -> Result<(), StorageError> {
        let mut state = state.clone();
        state.version = TRANSPORT_STATE_VERSION;
        let plaintext = serde_json::to_vec(&state)
            .map_err(|e| StorageError::Corrupt(format!("encoding transport.state: {e}")))?;
        let frame =
            encode_frame(&self.transport_key(), &self.id, TRANSPORT_SEGMENT, 0, &plaintext)?;
        write_atomic(&self.dir, "transport.state", &frame, true)
    }

    /// Read `chain.state`: the republic's persistent commit-block chain
    /// (`documents/persistent_chain.md`). Absent → empty (a pre-chain or
    /// freshly-founded-before-write workspace). A damaged file returns empty
    /// with a loud warning — unlike `transport.state`, the chain is shared
    /// history the caller must then treat as missing (its `verify_chain` will
    /// reject an empty chain for a republic that should have a genesis).
    pub fn read_chain(&self) -> (Option<molt_core::CheckpointState>, Vec<molt_core::ChainBlock>) {
        let path = self.dir.join("chain.state");
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, Vec::new()),
            Err(e) => {
                tracing::warn!(error = %e, "reading chain.state failed — no chain loaded");
                return (None, Vec::new());
            }
        };
        let (frames, torn) = split_frames(&data);
        if frames.len() != 1 || torn.is_some() {
            tracing::warn!("chain.state framing is damaged — no chain loaded");
            return (None, Vec::new());
        }
        let plaintext = match decrypt_frame(
            &self.chain_key(),
            &self.id,
            CHAIN_SEGMENT,
            0,
            frames[0].nonce,
            frames[0].ciphertext,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "chain.state does not authenticate — no chain loaded");
                return (None, Vec::new());
            }
        };
        match serde_json::from_slice::<ChainStateFile>(&plaintext) {
            Ok(ChainStateFile::Full(chain)) => (None, chain),
            Ok(ChainStateFile::Pruned {
                checkpoint_blob,
                blocks,
            }) => (Some(checkpoint_blob), blocks),
            Err(e) => {
                tracing::warn!(error = %e, "chain.state decode failed — no chain loaded");
                (None, Vec::new())
            }
        }
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
        let frame = encode_frame(&self.chain_key(), &self.id, CHAIN_SEGMENT, 0, &plaintext)?;
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
        let mut seq: u64 = 0; // seq of the previous frame; current = seq + 1
        for (seg_no, path) in list_sorted(&self.dir.join("log"), ".mlog") {
            let data = fs::read(&path)?;
            let (frames, _torn) = split_frames(&data);
            for frame in frames {
                seq += 1;
                if seq < from_seq {
                    continue;
                }
                let plaintext =
                    decrypt_frame(&self.key, &self.id, seg_no, seq, frame.nonce, frame.ciphertext)?;
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
        key,
        id,
        _lock: lock,
        seg_no: 1,
        seg,
        seg_len,
        next_seq: 2,
        dirty: false,
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
    if sealing::is_sealed(manifest) {
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

    let root = ws_dir.parent().unwrap_or(ws_dir);
    let device_key = load_or_create_device_key(&device_key_path(root))?;
    let id = id_bytes(&manifest.workspace.id)?;
    let sealed = match fs::read(ws_dir.join(&manifest.crypto.key_file)) {
        Ok(b) => b,
        // marker and key files disagree (crashed seal?): honest corruption,
        // never a guess — decrypting with the recovery phrase repairs it
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::BadFile(format!(
                "{} says device-sealed but the key material is missing \
                 (interrupted encrypt?) — decrypt with the recovery phrase \
                 to repair",
                ws_dir.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let key = unseal_workspace_key(&device_key, &id, &sealed)?;
    let prefs = read_prefs(ws_dir);

    // replay the segments; seq is implicit and strictly monotonic from 1
    let segments = list_sorted(&ws_dir.join("log"), ".mlog");
    if segments.is_empty() {
        return Err(StorageError::Corrupt(
            "workspace has no log segments".to_string(),
        ));
    }
    let mut history = Vec::new();
    let mut unknown_events: u64 = 0;
    let mut expected_seq: u64 = 1;
    let last_idx = segments.len() - 1;
    let mut last_seg = (1u64, 0u64); // (segment number, byte length after recovery)
    for (idx, (seg_no, path)) in segments.iter().enumerate() {
        let data = fs::read(path)?;
        let (frames, torn_at) = split_frames(&data);
        if let Some(pos) = torn_at {
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
                    "segment {} is damaged at byte {} but is not the last segment \
                     (bitrot?) — refusing to guess",
                    path.display(),
                    pos
                )));
            }
        }
        let mut seg_len = 0u64;
        for frame in &frames {
            let plaintext = decrypt_frame(
                &key,
                &id,
                *seg_no,
                expected_seq,
                frame.nonce,
                frame.ciphertext,
            )?;
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
    let last_seq = expected_seq - 1;

    // newest decodable snapshot wins — but only one the surviving log can
    // continue from. A snapshot ahead of the log (partial dir copy, torn
    // tail behind an old backup) would make the append position diverge
    // from the positional seq the AAD binds, bricking every later open;
    // such a snapshot is skipped and the state rebuilt from the log alone.
    let mut snapshot: Option<WorkspaceSnapshot> = None;
    let mut snaps = list_sorted(&ws_dir.join("snapshots"), ".msnap");
    snaps.reverse();
    for (at_seq, path) in snaps {
        if at_seq > last_seq {
            tracing::warn!(
                path = %path.display(),
                last_seq,
                "snapshot is ahead of the log (partial restore?) — skipping it"
            );
            continue;
        }
        match read_snapshot(&key, &id, at_seq, &path) {
            Ok(s) => {
                snapshot = Some(s);
                break;
            }
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping snapshot"),
        }
    }
    let floor = snapshot.as_ref().map(|s| s.at_seq).unwrap_or(0);
    let tail: Vec<EventEnvelope> = history.into_iter().filter(|e| e.seq > floor).collect();

    let (seg_no, seg_len) = last_seg;
    let seg = OpenOptions::new()
        .append(true)
        .open(ws_dir.join("log").join(segment_name(seg_no)))?;
    Ok((
        OpenedWorkspace {
            dir: ws_dir.to_path_buf(),
            manifest,
            prefs,
            key,
            id,
            _lock: lock,
            seg_no,
            seg,
            seg_len,
            next_seq: last_seq + 1,
            dirty: false,
        },
        LoadedState {
            snapshot,
            tail,
            unknown_events,
        },
    ))
}

fn read_snapshot(
    key: &[u8; 32],
    id: &[u8; 32],
    at_seq: u64,
    path: &Path,
) -> Result<WorkspaceSnapshot, StorageError> {
    let data = fs::read(path)?;
    let (frames, torn) = split_frames(&data);
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
            seed: String::new(),
            // the manifest carries no network label — the caller stamps the
            // effective global setting (`molt_core::effective_net_label`);
            // claiming one here would mislabel every entry after a restart
            net: String::new(),
            // derived from the directory (S6 marker), so the sealed state
            // survives restarts instead of living in session memory
            encrypted: sealing::is_sealed(&self.manifest),
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
    let mut total = 0u64;
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
            total += dir_size(&entry.path());
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
    Snapshot(WorkspaceSnapshot),
    /// Outbox read: every envelope with `seq >= from`. Served by the
    /// writer thread so reads are consistently ordered with queued appends
    /// (same channel, FIFO — a read enqueued after an append sees it).
    ReadFrom(u64, tokio::sync::oneshot::Sender<Vec<EventEnvelope>>),
    /// Persist `transport.state` (atomic rewrite).
    SaveTransport(TransportState),
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
        ack: mpsc::SyncSender<()>,
    },
    /// Load `transport.state` (defaults when absent/damaged).
    LoadTransport(tokio::sync::oneshot::Sender<TransportState>),
    /// Persist the whole persistent commit-block chain (`chain.state`), acking
    /// when durable — a governance commit must not be lost, so it uses the same
    /// blocking-ack shape as `MergeCrypto`.
    PersistChain {
        blob: Option<molt_core::CheckpointState>,
        blocks: Vec<molt_core::ChainBlock>,
        ack: mpsc::SyncSender<()>,
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
}

impl StorageHandle {
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

    /// Enqueue a snapshot write.
    pub fn snapshot(&self, snap: WorkspaceSnapshot) {
        let _ = self.tx.send(WriterMsg::Snapshot(snap));
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
    /// A full writer queue drops the save with a warning: stale cursors
    /// only cost resends, which the peers' dedup absorbs — better than
    /// blocking the transport on a struggling disk.
    pub fn save_transport_state(&self, state: TransportState) {
        match self.tx.try_send(WriterMsg::SaveTransport(state)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!("writer queue full — dropping a transport.state save");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Merge the runtime crypto (MLS snapshot + queue creds) into the current
    /// `transport.state`, preserving the delivery cursors, and BLOCK until it is
    /// durable (fsync'd). The clean-close persist that lets a reopened node
    /// resume the mesh. A gone writer is a silent no-op (nothing to resume into).
    pub fn persist_crypto_blocking(&self, mls: Option<Vec<u8>>, smp_queues: Option<Vec<u8>>) {
        self.merge_crypto_blocking(mls, smp_queues, None, true);
    }

    /// A **live** (mid-session) variant of [`Self::persist_crypto_blocking`]
    /// that can also replace the persisted **mesh links** — dynamic mesh
    /// membership grows/re-keys the mesh at runtime, and a reopen must resume
    /// the grown mesh, not the founded one. Does NOT seal `transport.state`:
    /// the (rebuilt) supervisor keeps saving its cursors afterwards.
    pub fn persist_mesh_crypto_blocking(
        &self,
        mls: Option<Vec<u8>>,
        smp_queues: Option<Vec<u8>>,
        mesh: Vec<molt_core::MeshLink>,
    ) {
        self.merge_crypto_blocking(mls, smp_queues, Some(mesh), false);
    }

    fn merge_crypto_blocking(
        &self,
        mls: Option<Vec<u8>>,
        smp_queues: Option<Vec<u8>>,
        mesh: Option<Vec<molt_core::MeshLink>>,
        seal: bool,
    ) {
        if mls.is_none() && smp_queues.is_none() && mesh.is_none() {
            return;
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
            .is_ok()
        {
            let _ = ack_rx.recv();
        }
    }

    /// Persist the whole persistent commit-block chain and BLOCK until it is
    /// durable (fsync'd) — a governance commit must survive a crash the instant
    /// it is broadcast. A gone writer is a silent no-op.
    pub fn persist_chain_blocking(
        &self,
        blob: Option<molt_core::CheckpointState>,
        blocks: Vec<molt_core::ChainBlock>,
    ) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(WriterMsg::PersistChain {
                blob,
                blocks,
                ack: ack_tx,
            })
            .is_ok()
        {
            let _ = ack_rx.recv();
        }
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
                            if let Err(e) = ws.write_transport_state(&ts) {
                                fail(&failed_flag, "transport.state write", &e);
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
                        if let Err(e) = ws.write_transport_state(&ts).and_then(|()| ws.sync()) {
                            fail(&failed_flag, "crypto merge write", &e);
                        }
                        // only the CLEAN-CLOSE merge seals — a live mesh-extension
                        // merge is followed by a rebuilt supervisor that keeps
                        // saving its cursors
                        if seal {
                            crypto_sealed = true;
                        }
                        let _ = ack.send(());
                    }
                    Ok(WriterMsg::LoadTransport(reply)) => {
                        let _ = reply.send(ws.read_transport_state());
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
                        if blob.is_some() {
                            if let Err(e) = ws.bump_pruned_version() {
                                fail(&failed_flag, "manifest version bump", &e);
                            }
                        }
                        if let Err(e) =
                            ws.write_chain(blob.as_ref(), &blocks).and_then(|()| ws.sync())
                        {
                            fail(&failed_flag, "chain.state write", &e);
                        }
                        let _ = ack.send(());
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
    StorageHandle { tx, failed }
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
            MemberIdentity { member: "founder".into(), identity_pk: "aa".repeat(32) },
            MemberIdentity { member: "juno".into(), identity_pk: "bb".repeat(32) },
            MemberIdentity { member: "mira".into(), identity_pk: "cc".repeat(32) },
        ]
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
        EventEnvelope {
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
        EventEnvelope {
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
            republic_id: String::new(),
            roster: Vec::new(),
            applied: Vec::new(),
            consumed_ids: Vec::new(),
            upto: 0,
        };
        handle.persist_chain_blocking(Some(blob), Vec::new());
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
            molt_core::OutboundCursor { log_seq: 7, wire_seq: 3 },
        );
        let mut inbound = std::collections::BTreeMap::new();
        inbound.insert("bob".to_string(), 5u64);
        handle.save_transport_state(TransportState {
            outbound,
            inbound,
            ..TransportState::default()
        });

        // a clean close merges the advanced MLS + queue creds — cursors survive
        handle.persist_crypto_blocking(Some(b"mls-blob".to_vec()), Some(b"queue-creds".to_vec()));

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
            rcv_queue: "cc".to_string(),
            rcv_wrap: "dd".to_string(),
        }];
        handle.persist_mesh_crypto_blocking(
            Some(b"fresh-mls".to_vec()),
            Some(b"fresh-creds".to_vec()),
            grown.clone(),
        );

        // the rebuilt supervisor saves a cursor advance from its PRE-merge
        // in-memory clone (mls/mesh/creds all stale/empty in that clone)
        let mut outbound = std::collections::BTreeMap::new();
        outbound.insert(
            "bob".to_string(),
            molt_core::OutboundCursor { log_seq: 9, wire_seq: 4 },
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
