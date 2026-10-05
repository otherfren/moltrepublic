// SPDX-License-Identifier: GPL-3.0-or-later
//! The payload file: `nonce24 ‖ XChaCha20-Poly1305(DEK, text, aad)`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use molt_core::vault::{SecretText, VaultDeposit, VAULT_PAYLOAD_MAX};
use zeroize::Zeroizing;

use crate::share::SecretScalar;
use crate::{hkdf_expand, VaultError};

/// Nonce (24) + tag (16): a payload file is at most
/// `VAULT_PAYLOAD_MAX + PAYLOAD_OVERHEAD` bytes.
pub const PAYLOAD_OVERHEAD: usize = 40;

/// `DEK = HKDF-SHA256(s, info)` with `info = molt_core::vault::dek_info(..)`.
#[must_use]
pub fn dek(s: &SecretScalar, info: &[u8]) -> Zeroizing<[u8; 32]> {
    hkdf_expand::<32>(s.to_bytes().as_ref(), info)
}

/// Encrypt the text under `dek`; the nonce leads the file.
///
/// # Errors
/// [`VaultError::TooLarge`] over `VAULT_PAYLOAD_MAX` bytes.
pub fn encrypt_payload(
    dek: &[u8; 32],
    text: &SecretText,
    aad: &[u8],
    nonce24: &[u8; 24],
) -> Result<Vec<u8>, VaultError> {
    if text.0.len() > VAULT_PAYLOAD_MAX {
        return Err(VaultError::TooLarge);
    }
    let cipher = XChaCha20Poly1305::new(dek.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(nonce24),
            Payload {
                msg: text.0.as_bytes(),
                aad,
            },
        )
        .map_err(|_| VaultError::TooLarge)?;
    let mut out = Vec::with_capacity(nonce24.len() + ct.len());
    out.extend_from_slice(nonce24);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a payload file.
///
/// # Errors
/// [`VaultError::Open`] when it does not open under `aad`, or is not UTF-8.
pub fn decrypt_payload(dek: &[u8; 32], file: &[u8], aad: &[u8]) -> Result<SecretText, VaultError> {
    if file.len() < PAYLOAD_OVERHEAD || file.len() > VAULT_PAYLOAD_MAX + PAYLOAD_OVERHEAD {
        return Err(VaultError::Open);
    }
    let (nonce, ct) = file.split_at(24);
    let cipher = XChaCha20Poly1305::new(dek.into());
    let pt = Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| VaultError::Open)?,
    );
    let text = std::str::from_utf8(&pt).map_err(|_| VaultError::Open)?;
    Ok(SecretText(text.to_string()))
}

/// Open a deposit's payload file with its recovered `s`: checks the
/// file against the record's hash and size first.
///
/// # Errors
/// [`VaultError::Open`] on a mismatch or a failed decryption.
pub fn open_payload(
    dep: &VaultDeposit,
    republic_id: &str,
    s: &SecretScalar,
    file: &[u8],
) -> Result<SecretText, VaultError> {
    use sha2::Digest;
    if hex::encode(sha2::Sha256::digest(file)) != dep.payload.hash
        || u64::try_from(file.len()).ok() != Some(dep.payload.size)
    {
        return Err(VaultError::Open);
    }
    let key = dek(
        s,
        &molt_core::vault::dek_info(republic_id, &dep.depositor, &dep.name, &dep.kind),
    );
    decrypt_payload(
        &key,
        file,
        &molt_core::vault::payload_aad(republic_id, &dep.depositor, &dep.name, &dep.kind),
    )
}
