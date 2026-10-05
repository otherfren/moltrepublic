// SPDX-License-Identifier: GPL-3.0-or-later
//! Shamir + Feldman over Ristretto (`vsss-rs` 5.1.0), dealt
//! deterministically from a seed (plan 1.3.3).

use rand_chacha::rand_core::SeedableRng;
use vsss_rs::curve25519::{WrappedRistretto, WrappedScalar};
use vsss_rs::curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use vsss_rs::curve25519_dalek::scalar::Scalar;
use vsss_rs::{
    FeldmanVerifierSet, IdentifierPrimeField, ParticipantIdGeneratorType, ReadableShareSet,
    ValueGroup,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{hkdf_expand, VaultError};

type Id = IdentifierPrimeField<WrappedScalar>;
type VShare = (Id, Id);
type Verifier = ValueGroup<WrappedRistretto>;

/// One Shamir share value: a scalar's 32 bytes, as dealt or as opened
/// (possibly non-canonical when a depositor cheats). `Debug` never prints
/// it; wiped on drop.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct Share([u8; 32]);

impl Share {
    /// Wrap raw share bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn scalar(&self) -> Option<Scalar> {
        Option::from(Scalar::from_canonical_bytes(self.0))
    }
}

impl std::fmt::Debug for Share {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<share>")
    }
}

/// The shared scalar `s`. `Debug` never prints it; wiped on drop.
#[derive(Clone)]
pub struct SecretScalar(pub(crate) Scalar);

impl SecretScalar {
    /// The scalar's canonical bytes (the DEK's HKDF input).
    #[must_use]
    pub fn to_bytes(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.0.to_bytes())
    }

    /// Its Feldman commitment `g^s`, compressed.
    #[must_use]
    pub fn commitment(&self) -> [u8; 32] {
        (RistrettoPoint::mul_base(&self.0)).compress().to_bytes()
    }
}

impl Drop for SecretScalar {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SecretScalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<secret>")
    }
}

/// A deterministic dealing.
#[derive(Debug)]
pub struct Dealt {
    /// The shared scalar.
    pub s: SecretScalar,
    /// `(x, share)` in the order of the `xs` given.
    pub shares: Vec<(u8, Share)>,
    /// Feldman commitments, one per coefficient (`m`).
    pub commitments: Vec<[u8; 32]>,
}

fn id(x: u8) -> Id {
    IdentifierPrimeField(WrappedScalar::from(u64::from(x)))
}

/// `s` = wide reduction of `HKDF(deal_seed, molt-vault-secret-scalar-v1)`.
pub(crate) fn secret_from_seed(deal_seed: &[u8; 32]) -> SecretScalar {
    let wide = hkdf_expand::<64>(deal_seed, &molt_core::vault::secret_scalar_info());
    SecretScalar(Scalar::from_bytes_mod_order_wide(&wide))
}

/// Deal `s` from `deal_seed` at threshold `m` to the 1-based `xs`; the
/// coefficients come from `ChaCha20Rng::from_seed(deal_seed)`, so the
/// same seed re-deals the same shares.
///
/// # Errors
/// [`VaultError::Threshold`] unless `2 <= m <= xs.len()`; a zero or
/// repeated x is [`VaultError::Deal`].
pub fn deal(deal_seed: &[u8; 32], m: u8, xs: &[u8]) -> Result<Dealt, VaultError> {
    if m < 2 || usize::from(m) > xs.len() {
        return Err(VaultError::Threshold);
    }
    let mut seen = std::collections::BTreeSet::new();
    if xs.iter().any(|x| *x == 0 || !seen.insert(*x)) {
        return Err(VaultError::Deal);
    }
    let s = secret_from_seed(deal_seed);
    let ids: Vec<Id> = xs.iter().copied().map(id).collect();
    let rng = rand_chacha::ChaCha20Rng::from_seed(*deal_seed);
    let (shares, verifiers) =
        vsss_rs::feldman::split_secret_with_participant_generator::<VShare, Verifier>(
            usize::from(m),
            xs.len(),
            &IdentifierPrimeField(WrappedScalar(s.0)),
            None,
            rng,
            &[ParticipantIdGeneratorType::list(&ids)],
        )
        .map_err(|_| VaultError::Deal)?;
    let commitments =
        <Vec<Verifier> as FeldmanVerifierSet<VShare, Verifier>>::verifiers(&verifiers)
            .iter()
            .map(|v| v.0 .0.compress().to_bytes())
            .collect();
    let shares = xs
        .iter()
        .zip(shares)
        .map(|(x, sh)| (*x, Share(sh.1 .0 .0.to_bytes())))
        .collect();
    Ok(Dealt {
        s,
        shares,
        commitments,
    })
}

fn verifier_set(commitments: &[[u8; 32]]) -> Option<Vec<Verifier>> {
    let mut set = vec![ValueGroup(WrappedRistretto(
        vsss_rs::curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT,
    ))];
    for c in commitments {
        let p = CompressedRistretto::from_slice(c).ok()?.decompress()?;
        set.push(ValueGroup(WrappedRistretto(p)));
    }
    Some(set)
}

/// A commitment decompresses to a Ristretto point.
pub(crate) fn is_point(c: &[u8; 32]) -> bool {
    CompressedRistretto(*c).decompress().is_some()
}

/// Feldman: does `share` at `x` lie on the committed polynomial? A
/// non-canonical scalar or an undecodable commitment fails.
#[must_use]
pub fn verify_share(commitments: &[[u8; 32]], x: u8, share: &Share) -> bool {
    let (Some(v), Some(set)) = (share.scalar(), verifier_set(commitments)) else {
        return false;
    };
    if commitments.is_empty() || x == 0 {
        return false;
    }
    let sh: VShare = (id(x), IdentifierPrimeField(WrappedScalar(v)));
    <Vec<Verifier> as FeldmanVerifierSet<VShare, Verifier>>::verify_share(&set, &sh).is_ok()
}

/// Lagrange-combine the first `m` shares with distinct x.
///
/// # Errors
/// [`VaultError::Threshold`] with fewer than `m` distinct, canonical shares.
pub fn combine(m: u8, shares: &[(u8, Share)]) -> Result<SecretScalar, VaultError> {
    let mut seen = std::collections::BTreeSet::new();
    let picked: Vec<VShare> = shares
        .iter()
        .filter(|(x, _)| *x != 0 && seen.insert(*x))
        .filter_map(|(x, sh)| {
            sh.scalar()
                .map(|v| (id(*x), IdentifierPrimeField(WrappedScalar(v))))
        })
        .take(usize::from(m))
        .collect();
    if m < 2 || picked.len() < usize::from(m) {
        return Err(VaultError::Threshold);
    }
    let s = picked.combine().map_err(|_| VaultError::Threshold)?;
    Ok(SecretScalar(s.0 .0))
}
