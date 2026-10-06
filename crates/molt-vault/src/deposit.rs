// SPDX-License-Identifier: GPL-3.0-or-later
//! Building and checking a deposit record (spec §6, §7).

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use molt_core::vault::{
    check_vault_kind, check_vault_name, deal_info, deposit_signing_bytes, eph_info, payload_aad,
    secret_id, share_aad, SecretText, VaultCtx, VaultDeposit, VaultPayloadRef, VAULT_PAYLOAD_MAX,
};
use zeroize::Zeroizing;

use crate::key::VaultSecretKey;
use crate::payload::{dek, encrypt_payload, PAYLOAD_OVERHEAD};
use crate::rand_core::{CryptoRng, RngCore};
use crate::seal::{open_share, seal_share, SHARE_CT_LEN};
use crate::share::{deal, is_point, secret_from_seed, verify_share, SecretScalar, Share};
use crate::{hex_array, hkdf_expand, is_hex, VaultError};

/// What a depositor seals.
#[derive(Debug)]
pub struct DepositInput<'a> {
    /// The republic's id.
    pub republic_id: &'a str,
    /// This seat.
    pub depositor: &'a str,
    /// The deposit's name (D8: visible).
    pub name: &'a str,
    /// Its kind (D8: visible).
    pub kind: &'a str,
    /// The slot's current `secret_id`, empty for a first deposit.
    pub replaces: &'a str,
    /// The plaintext.
    pub text: &'a SecretText,
    /// The republic's vault context.
    pub ctx: &'a VaultCtx,
}

/// Why a holder's own share check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareFault {
    /// This seat holds no share of the deposit.
    NotAHolder,
    /// The sealed share does not open with this seat's key.
    DoesNotOpen,
    /// The opened share is off the committed polynomial.
    FailsFeldman,
}

/// `(name, x, vault_pk)` of every holder: every seat but the depositor,
/// genesis order, x = 1-based genesis position (plan 1.3.5).
pub(crate) fn holders_of<'c>(
    ctx: &'c VaultCtx,
    depositor: &str,
) -> Result<Vec<(&'c str, u8, &'c str)>, VaultError> {
    let mut found = false;
    let mut out = Vec::new();
    for (i, (name, _, vault_pk)) in ctx.holders_in_genesis_order.iter().enumerate() {
        let x = u8::try_from(i + 1).map_err(|_| VaultError::Threshold)?;
        if name == depositor {
            found = true;
        } else {
            out.push((name.as_str(), x, vault_pk.as_str()));
        }
    }
    if !found {
        return Err(VaultError::NotASeat(depositor.to_string()));
    }
    Ok(out)
}

fn deal_seed(vault_seed: &[u8; 32], republic_id: &str, dep: &VaultDeposit) -> Zeroizing<[u8; 32]> {
    hkdf_expand::<32>(
        vault_seed,
        &deal_info(republic_id, &dep.name, &dep.kind, &dep.nonce),
    )
}

fn eph(vault_seed: &[u8; 32], secret_id: &str, seat: &str) -> Zeroizing<[u8; 32]> {
    hkdf_expand::<32>(vault_seed, &eph_info(secret_id, seat))
}

pub(crate) fn commitments_of(dep: &VaultDeposit) -> Result<Vec<[u8; 32]>, VaultError> {
    dep.commitments
        .iter()
        .map(|c| hex_array::<32>(c).ok_or(VaultError::Shape("commitment")))
        .collect()
}

/// Build a deposit: a fresh public `nonce` from `rng` (plan 1.3.3), the
/// deterministic dealing, the payload file under the derived DEK, one
/// HPKE share per holder with its derived ephemeral, the signature.
/// Returns the record and the payload file.
///
/// # Errors
/// A bad label, an oversize text, a depositor outside `ctx`, a threshold
/// that does not fit, or a holder key that is not a vault key.
pub fn build_deposit(
    input: &DepositInput<'_>,
    vault_seed: &[u8; 32],
    signing_key: &SigningKey,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(VaultDeposit, Vec<u8>), VaultError> {
    check_vault_name(input.name).map_err(VaultError::Label)?;
    check_vault_kind(input.kind).map_err(VaultError::Label)?;
    if input.text.0.len() > VAULT_PAYLOAD_MAX {
        return Err(VaultError::TooLarge);
    }
    let holders = holders_of(input.ctx, input.depositor)?;
    let m = input.ctx.m;
    let mut nonce = [0u8; 16];
    rng.fill_bytes(&mut nonce);
    let mut aead_nonce = [0u8; 24];
    rng.fill_bytes(&mut aead_nonce);
    let mut dep = VaultDeposit {
        depositor: input.depositor.to_string(),
        name: input.name.to_string(),
        kind: input.kind.to_string(),
        replaces: input.replaces.to_string(),
        m,
        holders: holders.iter().map(|(n, _, _)| (*n).to_string()).collect(),
        nonce: hex::encode(nonce),
        ..VaultDeposit::default()
    };
    let seed = deal_seed(vault_seed, input.republic_id, &dep);
    let xs: Vec<u8> = holders.iter().map(|(_, x, _)| *x).collect();
    let dealt = deal(&seed, m, &xs)?;
    dep.commitments = dealt.commitments.iter().map(hex::encode).collect();
    let key = dek(
        &dealt.s,
        &molt_core::vault::dek_info(input.republic_id, input.depositor, input.name, input.kind),
    );
    let file = encrypt_payload(
        &key,
        input.text,
        &payload_aad(input.republic_id, input.depositor, input.name, input.kind),
        &aead_nonce,
    )?;
    dep.payload = VaultPayloadRef {
        hash: {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(&file))
        },
        size: u64::try_from(file.len()).map_err(|_| VaultError::TooLarge)?,
    };
    let sid = secret_id(input.republic_id, &dep);
    for ((name, _, vault_pk), (_, share)) in holders.iter().zip(&dealt.shares) {
        let sealed = seal_share(
            vault_pk,
            share,
            &share_aad(&sid, name),
            &eph(vault_seed, &sid, name),
        )?;
        dep.enc_share.push(hex::encode(sealed));
    }
    dep.sig_depositor = hex::encode(
        signing_key
            .sign(&deposit_signing_bytes(input.republic_id, &dep))
            .to_bytes(),
    );
    Ok((dep, file))
}

/// The record's shape against the context of the chain it sits in:
/// holders = every seat but the depositor in genesis order, `m`
/// commitments that are points, one 80-byte share per holder, hex
/// fields of the right length, a payload size within the cap.
///
/// # Errors
/// [`VaultError::Shape`] (or a label / seat error) naming the field.
pub fn verify_deposit_shape(dep: &VaultDeposit, ctx: &VaultCtx) -> Result<(), VaultError> {
    check_vault_name(&dep.name).map_err(VaultError::Label)?;
    check_vault_kind(&dep.kind).map_err(VaultError::Label)?;
    let holders = holders_of(ctx, &dep.depositor)?;
    if dep.m != ctx.m {
        return Err(VaultError::Shape("m"));
    }
    if dep.holders.len() != holders.len()
        || dep
            .holders
            .iter()
            .zip(&holders)
            .any(|(a, (b, _, _))| a != b)
    {
        return Err(VaultError::Shape("holders"));
    }
    if dep.commitments.len() != usize::from(dep.m) {
        return Err(VaultError::Shape("commitments"));
    }
    if commitments_of(dep)?.iter().any(|c| !is_point(c)) {
        return Err(VaultError::Shape("commitment"));
    }
    if dep.enc_share.len() != holders.len()
        || dep.enc_share.iter().any(|e| !is_hex(e, SHARE_CT_LEN))
    {
        return Err(VaultError::Shape("enc_share"));
    }
    if !is_hex(&dep.payload.hash, 32) {
        return Err(VaultError::Shape("payload hash"));
    }
    let max =
        u64::try_from(VAULT_PAYLOAD_MAX + PAYLOAD_OVERHEAD).map_err(|_| VaultError::TooLarge)?;
    let min = u64::try_from(PAYLOAD_OVERHEAD).map_err(|_| VaultError::TooLarge)?;
    if dep.payload.size < min || dep.payload.size > max {
        return Err(VaultError::Shape("payload size"));
    }
    if !dep.replaces.is_empty() && !is_hex(&dep.replaces, 32) {
        return Err(VaultError::Shape("replaces"));
    }
    if !is_hex(&dep.nonce, 16) {
        return Err(VaultError::Shape("nonce"));
    }
    if !is_hex(&dep.sig_depositor, 64) {
        return Err(VaultError::Shape("sig_depositor"));
    }
    Ok(())
}

/// `sig_depositor` against the depositor's identity key.
///
/// # Errors
/// [`VaultError::Signature`].
pub fn verify_deposit_sig(
    dep: &VaultDeposit,
    republic_id: &str,
    depositor_identity_pk: &str,
) -> Result<(), VaultError> {
    let pk: [u8; 32] = hex_array(depositor_identity_pk).ok_or(VaultError::Signature)?;
    let sig: [u8; 64] = hex_array(&dep.sig_depositor).ok_or(VaultError::Signature)?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|_| VaultError::Signature)?;
    vk.verify_strict(
        &deposit_signing_bytes(republic_id, dep),
        &ed25519_dalek::Signature::from_bytes(&sig),
    )
    .map_err(|_| VaultError::Signature)
}

/// A holder opens its share and checks it against the commitments at
/// its genesis x, taken from `ctx`.
///
/// # Errors
/// The [`ShareFault`] that stopped it.
pub fn check_my_share(
    dep: &VaultDeposit,
    republic_id: &str,
    ctx: &VaultCtx,
    my_name: &str,
    vault_sk: &VaultSecretKey,
) -> Result<Share, ShareFault> {
    let my_x = holders_of(ctx, &dep.depositor)
        .map_err(|_| ShareFault::NotAHolder)?
        .iter()
        .find(|(n, _, _)| *n == my_name)
        .map(|(_, x, _)| *x)
        .ok_or(ShareFault::NotAHolder)?;
    let i = dep
        .holders
        .iter()
        .position(|h| h == my_name)
        .ok_or(ShareFault::NotAHolder)?;
    let sealed = dep
        .enc_share
        .get(i)
        .and_then(|e| hex_array::<SHARE_CT_LEN>(e))
        .ok_or(ShareFault::DoesNotOpen)?;
    let sid = secret_id(republic_id, dep);
    let share = open_share(vault_sk, &sealed, &share_aad(&sid, my_name))
        .map_err(|_| ShareFault::DoesNotOpen)?;
    let commitments = commitments_of(dep).map_err(|_| ShareFault::FailsFeldman)?;
    if !verify_share(&commitments, my_x, &share) {
        return Err(ShareFault::FailsFeldman);
    }
    Ok(share)
}

/// The depositor re-deals a holder's share and its ephemeral `ikmE` to
/// answer a complaint; it stores nothing (plan 1.3.3).
///
/// # Errors
/// [`VaultError::WrongSeed`] when the seed does not re-deal this record,
/// or a seat / threshold error.
pub fn rederive_share(
    dep: &VaultDeposit,
    republic_id: &str,
    ctx: &VaultCtx,
    vault_seed: &[u8; 32],
    holder: &str,
) -> Result<(Share, Zeroizing<[u8; 32]>), VaultError> {
    let holders = holders_of(ctx, &dep.depositor)?;
    let i = holders
        .iter()
        .position(|(n, _, _)| *n == holder)
        .ok_or_else(|| VaultError::NotASeat(holder.to_string()))?;
    let xs: Vec<u8> = holders.iter().map(|(_, x, _)| *x).collect();
    let dealt = deal(&deal_seed(vault_seed, republic_id, dep), dep.m, &xs)?;
    if dealt
        .commitments
        .iter()
        .map(hex::encode)
        .ne(dep.commitments.iter().cloned())
    {
        return Err(VaultError::WrongSeed);
    }
    let (_, share) = dealt
        .shares
        .into_iter()
        .nth(i)
        .ok_or(VaultError::WrongSeed)?;
    Ok((share, eph(vault_seed, &secret_id(republic_id, dep), holder)))
}

/// The depositor recovers `s` from its seed to re-seal without re-typing.
///
/// # Errors
/// [`VaultError::WrongSeed`] when `g^s` is not the record's first commitment.
pub fn recover_secret(
    dep: &VaultDeposit,
    republic_id: &str,
    vault_seed: &[u8; 32],
) -> Result<SecretScalar, VaultError> {
    let s = secret_from_seed(&deal_seed(vault_seed, republic_id, dep));
    if dep.commitments.first() != Some(&hex::encode(s.commitment())) {
        return Err(VaultError::WrongSeed);
    }
    Ok(s)
}
