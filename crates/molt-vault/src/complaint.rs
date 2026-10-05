// SPDX-License-Identifier: GPL-3.0-or-later
//! Deciding a complaint from the depositor's reveal (spec §7).

use molt_core::vault::{secret_id, share_aad, VaultCtx, VaultDeposit, VaultRevealOutcome};

use crate::deposit::{commitments_of, holders_of, verify_deposit_shape};
use crate::seal::seal_share;
use crate::share::{verify_share, Share};
use crate::VaultError;

/// Anyone recomputes holder `holder`'s sealed share from the revealed
/// `share` and `ikm_e` under its FOUNDING vault key and genesis x, both
/// taken from `ctx`: a different ciphertext is a lie, a matching one off
/// the polynomial a bad share, a matching one on it a false complaint.
///
/// # Errors
/// `holder` holds no share of `dep`, or the record does not fit `ctx`.
pub fn decide(
    dep: &VaultDeposit,
    republic_id: &str,
    ctx: &VaultCtx,
    holder: &str,
    share: &Share,
    ikm_e: &[u8; 32],
) -> Result<VaultRevealOutcome, VaultError> {
    verify_deposit_shape(dep, ctx)?;
    let holders = holders_of(ctx, &dep.depositor)?;
    let (i, (_, x, holder_vault_pk)) = holders
        .iter()
        .enumerate()
        .find(|(_, (n, _, _))| *n == holder)
        .ok_or_else(|| VaultError::NotASeat(holder.to_string()))?;
    let recorded = dep.enc_share.get(i).ok_or(VaultError::Shape("enc_share"))?;
    let sid = secret_id(republic_id, dep);
    let again = seal_share(holder_vault_pk, share, &share_aad(&sid, holder), ikm_e)?;
    if hex::encode(again) != *recorded {
        return Ok(VaultRevealOutcome::Lie);
    }
    Ok(if verify_share(&commitments_of(dep)?, *x, share) {
        VaultRevealOutcome::FalseComplaint
    } else {
        VaultRevealOutcome::BadShare
    })
}
