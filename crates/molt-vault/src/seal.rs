// SPDX-License-Identifier: GPL-3.0-or-later
//! HPKE share transport (spec §7 step 4, §8 step 3): base mode,
//! X25519-HKDF-SHA256, ChaCha20-Poly1305, empty info (the AAD binds).

use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};

use crate::key::{parse_pk, VaultSecretKey};
use crate::rand_core::{CryptoRng, RngCore};
use crate::rng::IkmRng;
use crate::share::Share;
use crate::VaultError;

/// Encapsulated key (32) + share (32) + tag (16).
pub const SHARE_CT_LEN: usize = 80;

type Encapped = <X25519HkdfSha256 as Kem>::EncappedKey;

pub(crate) fn seal_raw(
    pk: &[u8; 32],
    plaintext: &[u8; 32],
    aad: &[u8],
    ikm_e: &[u8; 32],
) -> Result<[u8; SHARE_CT_LEN], VaultError> {
    let pk = <<X25519HkdfSha256 as Kem>::PublicKey as Deserializable>::from_bytes(pk)
        .map_err(|_| VaultError::Key)?;
    seal_to(&pk, plaintext, aad, ikm_e)
}

fn seal_to(
    pk: &<X25519HkdfSha256 as Kem>::PublicKey,
    plaintext: &[u8; 32],
    aad: &[u8],
    ikm_e: &[u8; 32],
) -> Result<[u8; SHARE_CT_LEN], VaultError> {
    let mut rng = IkmRng::new(*ikm_e);
    let (enc, ct) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
        &OpModeS::Base,
        pk,
        b"",
        plaintext,
        aad,
        &mut rng,
    )
    .map_err(|_| VaultError::Key)?;
    let mut out = [0u8; SHARE_CT_LEN];
    let enc = enc.to_bytes();
    let (head, tail) = out.split_at_mut(enc.len());
    head.copy_from_slice(&enc);
    if tail.len() != ct.len() {
        return Err(VaultError::Shape("hpke ciphertext"));
    }
    tail.copy_from_slice(&ct);
    Ok(out)
}

fn open_raw(sk: &VaultSecretKey, sealed: &[u8], aad: &[u8]) -> Result<Share, VaultError> {
    if sealed.len() != SHARE_CT_LEN {
        return Err(VaultError::Open);
    }
    let (enc, ct) = sealed.split_at(32);
    let enc = Encapped::from_bytes(enc).map_err(|_| VaultError::Open)?;
    let pt = zeroize::Zeroizing::new(
        hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
            &OpModeR::Base,
            &sk.0,
            &enc,
            b"",
            ct,
            aad,
        )
        .map_err(|_| VaultError::Open)?,
    );
    let bytes: [u8; 32] = pt.as_slice().try_into().map_err(|_| VaultError::Open)?;
    Ok(Share::from_bytes(bytes))
}

/// Seal a dealt share to a holder with the derived ephemeral `ikm_e`:
/// the same inputs give the same 80 bytes, which is what makes a
/// complaint decidable.
///
/// # Errors
/// [`VaultError::Key`] for a malformed or low-order key.
pub fn seal_share(
    vault_pk: &str,
    share: &Share,
    aad: &[u8],
    ikm_e: &[u8; 32],
) -> Result<[u8; SHARE_CT_LEN], VaultError> {
    seal_to(&parse_pk(vault_pk)?, share.as_bytes(), aad, ikm_e)
}

/// Open a dealt share with this seat's vault key.
///
/// # Errors
/// [`VaultError::Open`] when it does not open under `aad`.
pub fn open_share(sk: &VaultSecretKey, sealed: &[u8], aad: &[u8]) -> Result<Share, VaultError> {
    open_raw(sk, sealed, aad)
}

/// Seal an answer to the reader with a fresh ephemeral from `rng`.
///
/// # Errors
/// [`VaultError::Key`] for a malformed or low-order key.
pub fn seal_resp(
    reader_pk: &str,
    share: &Share,
    aad: &[u8],
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<[u8; SHARE_CT_LEN], VaultError> {
    let mut ikm = zeroize::Zeroizing::new([0u8; 32]);
    rng.fill_bytes(ikm.as_mut());
    seal_to(&parse_pk(reader_pk)?, share.as_bytes(), aad, &ikm)
}

/// Open an answer with the reader's vault key.
///
/// # Errors
/// [`VaultError::Open`] when it does not open under `aad`.
pub fn open_resp(sk: &VaultSecretKey, sealed: &[u8], aad: &[u8]) -> Result<Share, VaultError> {
    open_raw(sk, sealed, aad)
}
