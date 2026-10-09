// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse's crypto (`docs/chain/wallet_treasury_design.md`, plan
//! `docs/chain/multi-sig-wallet-plan.md` §5.1): the PedPoP DKG among the n
//! founding seats and the standard main address of the resulting group key.
//!
//! Pure functions, no I/O; randomness comes from the caller.

pub mod dkg;
pub mod keys;

pub use dalek_ff_group::{Ed25519, EdwardsPoint};
pub use dkg_pedpop::{Participant, ThresholdKeys, ThresholdParams};
pub use monero_wallet::address::Network;
pub use monero_wallet::ed25519::Scalar as MoneroScalar;
/// Re-exported so callers pass the same `rand_core` (0.6) the DKG uses.
pub use rand_chacha::rand_core;

/// Why a treasury primitive refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreasuryError {
    /// `t`, `n` or the seat index is out of range.
    #[error("bad parameters: {0}")]
    Params(String),
    /// A frame from this participant did not decode.
    #[error("bad frame from participant {0}")]
    Frame(u16),
    /// The DKG library refused (its error, debug-formatted).
    #[error("dkg: {0}")]
    Dkg(String),
    /// The group key is the identity or torsioned.
    #[error("invalid spend key")]
    SpendKey,
}
