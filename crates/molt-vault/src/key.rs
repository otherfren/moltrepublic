// SPDX-License-Identifier: GPL-3.0-or-later
//! The seat's vault key (spec §5, plan 1.3.1).

use hpke::kem::X25519HkdfSha256;
use hpke::{Kem, Serializable};
use zeroize::Zeroizing;

use crate::{hex_array, hkdf_expand, VaultError};

type PrivateKey = <X25519HkdfSha256 as Kem>::PrivateKey;
type PublicKey = <X25519HkdfSha256 as Kem>::PublicKey;

/// `vault_ikm = HKDF-SHA256(entropy, molt-vault-x25519-v1 ‖ founding_nostr_pk ‖ identity_pk)`.
/// The founding `nostr_pk` is ticket-salted, so one phrase gets a
/// different key in every republic, and fixed for life in the genesis.
#[must_use]
pub fn derive_vault_seed(
    entropy: &[u8],
    founding_nostr_pk: &str,
    identity_pk: &str,
) -> Zeroizing<[u8; 32]> {
    hkdf_expand::<32>(
        entropy,
        &molt_core::vault::vault_key_info(founding_nostr_pk, identity_pk),
    )
}

/// A seat's vault private key. `Debug` never prints it.
pub struct VaultSecretKey(pub(crate) PrivateKey);

impl std::fmt::Debug for VaultSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<vault key>")
    }
}

/// `DeriveKeyPair(seed)` (RFC 9180): the private key and the public key
/// as 64 lowercase hex.
#[must_use]
pub fn vault_keypair(seed: &[u8; 32]) -> (VaultSecretKey, String) {
    let (sk, pk) = X25519HkdfSha256::derive_keypair(seed);
    (VaultSecretKey(sk), hex::encode(pk.to_bytes()))
}

/// p = 2^255 - 19, little-endian.
const P: [u8; 32] = {
    let mut p = [0xff; 32];
    p[0] = 0xed;
    p[31] = 0x7f;
    p
};

/// Normalize-or-reject a vault key: 64 lowercase hex naming a canonical
/// u-coordinate (top bit clear, u < p), so no two strings name one
/// effective key; low-order points fail through HPKE's all-zero-DH check.
///
/// # Errors
/// [`VaultError::Key`] for anything else.
pub fn canonical_vault_pk(s: &str) -> Result<String, VaultError> {
    let raw: [u8; 32] = hex_array(s).ok_or(VaultError::Key)?;
    if raw[31] & 0x80 != 0 {
        return Err(VaultError::Key);
    }
    // little-endian compare from the top byte
    if raw.iter().rev().cmp(P.iter().rev()) != std::cmp::Ordering::Less {
        return Err(VaultError::Key);
    }
    crate::seal::seal_raw(&raw, &[0u8; 32], b"", &[7u8; 32]).map_err(|_| VaultError::Key)?;
    Ok(s.to_string())
}

pub(crate) fn parse_pk(s: &str) -> Result<PublicKey, VaultError> {
    let raw: [u8; 32] = hex_array(s).ok_or(VaultError::Key)?;
    <PublicKey as hpke::Deserializable>::from_bytes(&raw).map_err(|_| VaultError::Key)
}
