// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault's crypto (`docs_archive/vault/vault_threshold_disclosure.md`, plan
//! `docs_archive/vault/vault_build_plan.md` S1): Shamir/Feldman dealing over
//! Ristretto (`vsss-rs`), HPKE share transport with a caller-derived
//! ephemeral (`hpke`), the payload AEAD, deposit signing and the checks
//! every holder, reader, approver and fold runs.
//!
//! Pure functions, no I/O; randomness comes from the caller. The byte
//! layouts live in `molt_core::vault`.

mod base;
mod complaint;
mod deposit;
mod key;
mod payload;
mod read;
mod rng;
mod seal;
mod share;

pub use base::verify_base;
pub use complaint::decide;
pub use deposit::{
    build_deposit, check_my_share, recover_secret, rederive_share, verify_deposit_shape,
    verify_deposit_sig, DepositInput, ShareFault,
};
pub use key::{canonical_vault_pk, derive_vault_seed, vault_keypair, VaultSecretKey};
pub use payload::{decrypt_payload, dek, encrypt_payload, open_payload, PAYLOAD_OVERHEAD};
pub use read::{read, Opened, ReadFault, SeatShare};
pub use rng::IkmRng;
pub use seal::{open_resp, open_share, seal_resp, seal_share, SHARE_CT_LEN};
pub use share::{combine, deal, verify_share, Dealt, SecretScalar, Share};

/// Re-exported so callers pass the same `rand_core` (0.6) the dealing uses.
pub use rand_chacha::rand_core;

/// Why a vault primitive refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// A name or kind failed `check_vault_name` / `check_vault_kind`.
    #[error("bad label: {0}")]
    Label(String),
    /// Over `VAULT_PAYLOAD_MAX`.
    #[error("too large")]
    TooLarge,
    /// The member is not in the vault context.
    #[error("not a seat: {0}")]
    NotASeat(String),
    /// The threshold does not fit the seats.
    #[error("bad threshold")]
    Threshold,
    /// A record field has the wrong shape.
    #[error("bad shape: {0}")]
    Shape(&'static str),
    /// `sig_depositor` does not verify.
    #[error("bad signature")]
    Signature,
    /// Not a canonical vault key.
    #[error("bad vault key")]
    Key,
    /// A ciphertext does not open.
    #[error("does not open")]
    Open,
    /// The dealing library refused.
    #[error("dealing failed")]
    Deal,
    /// The seed does not re-derive this deposit.
    #[error("wrong seed")]
    WrongSeed,
    /// The folded base is not consistent.
    #[error("bad base: {0}")]
    Base(String),
}

fn hkdf_expand<const N: usize>(ikm: &[u8], info: &[u8]) -> zeroize::Zeroizing<[u8; N]> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, ikm);
    let mut out = zeroize::Zeroizing::new([0u8; N]);
    hk.expand(info, out.as_mut())
        .expect("at most 64 bytes is within the HKDF-SHA256 expand limit");
    out
}

/// Lowercase hex of exactly `bytes` bytes.
fn is_hex(s: &str, bytes: usize) -> bool {
    s.len() == bytes * 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex_array<const N: usize>(s: &str) -> Option<[u8; N]> {
    if !is_hex(s, N) {
        return None;
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}
