// SPDX-License-Identifier: GPL-3.0-or-later
//! Deciding a complaint from the depositor's reveal (spec §7).

use molt_core::vault::{secret_id, share_aad, VaultDeposit, VaultRevealOutcome};

use crate::seal::seal_share;
use crate::share::{verify_share, Share};
use crate::{hex_array, VaultError};

/// Anyone recomputes holder `holder`'s sealed share from the revealed
/// `share` and `ikm_e` under its FOUNDING `holder_vault_pk` (x = its
/// genesis position): a different ciphertext is a lie, a matching one off
/// the polynomial a bad share, a matching one on it a false complaint.
///
/// # Errors
/// `holder` holds no share of `dep`, or its key or the record is malformed.
pub fn decide(
    dep: &VaultDeposit,
    republic_id: &str,
    holder: &str,
    holder_vault_pk: &str,
    x: u8,
    share: &Share,
    ikm_e: &[u8; 32],
) -> Result<VaultRevealOutcome, VaultError> {
    let i = dep
        .holders
        .iter()
        .position(|h| h == holder)
        .ok_or_else(|| VaultError::NotASeat(holder.to_string()))?;
    let recorded = dep.enc_share.get(i).ok_or(VaultError::Shape("enc_share"))?;
    let sid = secret_id(republic_id, dep);
    let again = seal_share(holder_vault_pk, share, &share_aad(&sid, holder), ikm_e)?;
    if hex::encode(again) != *recorded {
        return Ok(VaultRevealOutcome::Lie);
    }
    let commitments: Vec<[u8; 32]> = dep
        .commitments
        .iter()
        .map(|c| hex_array::<32>(c).ok_or(VaultError::Shape("commitment")))
        .collect::<Result<_, _>>()?;
    Ok(if verify_share(&commitments, x, share) {
        VaultRevealOutcome::FalseComplaint
    } else {
        VaultRevealOutcome::BadShare
    })
}
