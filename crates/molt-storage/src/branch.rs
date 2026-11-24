// SPDX-License-Identifier: GPL-3.0-or-later

//! The branch choice's two files (`docs/chain/chain_rescue_plan.md` §2, §5):
//! this seat's choice record (local, sealed, never exported - a restored
//! device must not inherit it) and the passphrase-sealed evidence bundle
//! the operators move between split seats.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::{
    decrypt_state_file, encode_frame, hkdf32, read_capped, write_atomic, OpenedWorkspace, StorageError,
    READ_CAP_STATE,
};

/// This seat's branch choices, its abandoned mark and the evidence it holds.
pub const BRANCH_FILE: &str = "branch.state";
/// AAD segment marker for [`BRANCH_FILE`].
pub(crate) const BRANCH_SEGMENT: u64 = u64::MAX - 14;

/// The bundle magic.
pub const BUNDLE_MAGIC: &[u8; 15] = b"molt-branch-v1\0";
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const BUNDLE_MAX: u64 = 256 * 1024 * 1024;
const BUNDLE_M_KIB_MAX: u32 = 1024 * 1024;
const BUNDLE_T_MAX: u32 = 16;
const BUNDLE_P_MAX: u32 = 8;

fn branch_key(ws_key: &[u8; 32], id: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(hkdf32(ws_key, "molt-branch-state", id))
}

impl OpenedWorkspace {
    /// [`BRANCH_FILE`]; `None` = no file.
    ///
    /// # Errors
    /// Unreadable, over the cap, or a frame that does not authenticate.
    pub fn read_branch_state(&self) -> Result<Option<Vec<u8>>, StorageError> {
        let data = match read_capped(&self.dir.join(BRANCH_FILE), READ_CAP_STATE, BRANCH_FILE) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StorageError::Corrupt(format!("reading {BRANCH_FILE}: {e}"))),
        };
        decrypt_state_file(&branch_key(&self.key, &self.id), &self.id, BRANCH_SEGMENT, &data).map(Some)
    }

    /// Rewrite [`BRANCH_FILE`] (atomic, sealed, synced).
    ///
    /// # Errors
    /// The seal or the write failed.
    pub fn write_branch_state(&self, bytes: &[u8]) -> Result<(), StorageError> {
        let frame = encode_frame(&branch_key(&self.key, &self.id), &self.id, BRANCH_SEGMENT, 0, bytes)?;
        write_atomic(&self.dir, BRANCH_FILE, &frame, true)
    }
}

fn bundle_aad(salt: &[u8], m: u32, t: u32, p: u32) -> Vec<u8> {
    let mut aad = BUNDLE_MAGIC.to_vec();
    aad.extend_from_slice(salt);
    for v in [m, t, p] {
        aad.extend_from_slice(&v.to_le_bytes());
    }
    aad
}

/// Seal a bundle under a passphrase (Argon2id + XChaCha20-Poly1305, the
/// export's parameters). Blocking: off the actor only.
///
/// # Errors
/// The passphrase policy, or the key derivation.
pub fn seal_bundle(plain: &[u8], passphrase: &str) -> Result<Vec<u8>, StorageError> {
    crate::export::check_passphrase_policy(passphrase)?;
    let (m, t, p) = (crate::export::EXPORT_ARGON2_M_KIB, crate::export::EXPORT_ARGON2_T, crate::export::EXPORT_ARGON2_P);
    let salt: [u8; SALT_LEN] = crate::os_random()?;
    let nonce: [u8; NONCE_LEN] = crate::os_random()?;
    let key = crate::export::passphrase_key(passphrase, &salt, m, t, p)?;
    let aad = bundle_aad(&salt, m, t, p);
    let ct = XChaCha20Poly1305::new((&*key).into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: &aad })
        .map_err(|_| StorageError::Crypto("bundle seal failed".to_string()))?;
    let mut out = aad;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a bundle [`seal_bundle`] wrote. Blocking: off the actor only.
///
/// # Errors
/// Not a bundle, parameters out of bounds, or a wrong passphrase.
pub fn open_bundle(data: &[u8], passphrase: &str) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    let head = BUNDLE_MAGIC.len() + SALT_LEN + 12;
    if data.len() < head + NONCE_LEN || !data.starts_with(BUNDLE_MAGIC) {
        return Err(StorageError::BadFile("not a branch bundle".to_string()));
    }
    let salt = &data[BUNDLE_MAGIC.len()..BUNDLE_MAGIC.len() + SALT_LEN];
    let word = |i: usize| {
        let at = BUNDLE_MAGIC.len() + SALT_LEN + 4 * i;
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    };
    let (m, t, p) = (word(0), word(1), word(2));
    if m > BUNDLE_M_KIB_MAX || t > BUNDLE_T_MAX || p > BUNDLE_P_MAX {
        return Err(StorageError::BadFile("bundle kdf parameters out of bounds".to_string()));
    }
    let key = crate::export::passphrase_key(passphrase, salt, m, t, p)?;
    let nonce = &data[head..head + NONCE_LEN];
    let pt = XChaCha20Poly1305::new((&*key).into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: &data[head + NONCE_LEN..], aad: &data[..head] })
        .map_err(|_| StorageError::Crypto("wrong passphrase or damaged bundle".to_string()))?;
    Ok(Zeroizing::new(pt))
}

/// Write a sealed bundle to `dest` (atomic replace).
///
/// # Errors
/// The seal or the write failed.
pub fn write_bundle_file(dest: &std::path::Path, plain: &[u8], passphrase: &str) -> Result<u64, StorageError> {
    let sealed = seal_bundle(plain, passphrase)?;
    if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &sealed)?;
    std::fs::rename(&tmp, dest)?;
    Ok(u64::try_from(sealed.len()).unwrap_or(u64::MAX))
}

/// Read and open the bundle at `path`.
///
/// # Errors
/// Missing, too large, or [`open_bundle`]'s errors.
pub fn read_bundle_file(path: &std::path::Path, passphrase: &str) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    let data = read_capped(path, BUNDLE_MAX, "branch bundle")?;
    open_bundle(&data, passphrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_round_trips_and_refuses_a_wrong_passphrase() {
        let sealed = seal_bundle(b"{\"blocks\":[]}", "correct horse battery").expect("seal");
        assert!(sealed.starts_with(BUNDLE_MAGIC));
        assert_eq!(&*open_bundle(&sealed, "correct horse battery").expect("open"), b"{\"blocks\":[]}");
        assert!(open_bundle(&sealed, "wrong horse battery!").is_err());
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(open_bundle(&flipped, "correct horse battery").is_err(), "tamper");
        let mut params = sealed;
        params[BUNDLE_MAGIC.len() + SALT_LEN + 4] ^= 1;
        assert!(open_bundle(&params, "correct horse battery").is_err(), "the parameters are bound");
        assert!(seal_bundle(b"x", "short").is_err(), "the export's passphrase policy");
    }

    #[test]
    fn the_branch_state_round_trips_sealed() {
        use molt_core::{EventEnvelope, WorkspaceEvent};
        let tmp = tempfile::tempdir().expect("tmp");
        let seed = crate::seed_entropy(&crate::generate_seed_phrase().expect("gen")).expect("entropy");
        let founded = EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 42,
            by: "a".to_string(),
            body: WorkspaceEvent::Founded {
                name: "Club".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "a".to_string(),
                roster: vec!["a".to_string(), "b".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
                relays: Vec::new(),
                features: None,
            },
        };
        let ws = crate::create_workspace(tmp.path(), &seed, &founded).expect("create");
        assert_eq!(ws.read_branch_state().expect("read"), None);
        ws.write_branch_state(b"{\"choices\":{}}").expect("write");
        assert_eq!(ws.read_branch_state().expect("read").as_deref(), Some(&b"{\"choices\":{}}"[..]));
        let raw = std::fs::read(ws.dir().join(BRANCH_FILE)).expect("raw");
        assert!(!raw.windows(7).any(|w| w == b"choices"), "sealed at rest");
    }
}
