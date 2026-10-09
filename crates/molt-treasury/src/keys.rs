// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse's standard main address.

use dalek_ff_group::EdwardsPoint;
use monero_wallet::address::{MoneroAddress, Network};
use monero_wallet::ed25519::{Point, Scalar};
use monero_wallet::ViewPair;
use zeroize::Zeroizing;

use crate::TreasuryError;

/// The standard (legacy) main address of `(group spend key, private view key)`.
pub fn standard_address(
    group_key: EdwardsPoint,
    view: Zeroizing<Scalar>,
    network: Network,
) -> Result<MoneroAddress, TreasuryError> {
    let pair =
        ViewPair::new(Point::from(group_key.0), view).map_err(|_| TreasuryError::SpendKey)?;
    Ok(pair.legacy_address(network))
}
