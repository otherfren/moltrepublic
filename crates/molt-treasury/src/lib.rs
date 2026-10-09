// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse's crypto (`docs/chain/wallet_treasury_design.md`, plan
//! `docs/chain/multi-sig-wallet-plan.md` §5.1): the PedPoP DKG among the n
//! founding seats, the shared view key, the standard main address, the
//! seats' attestations, the keys record and the scan core.
//!
//! Pure functions, no I/O; randomness comes from the caller. Every byte
//! layout is tagged, length-prefixed and entry-counted with
//! `molt_core::put_bytes`/`put_count`; integers are little-endian.

pub mod attest;
pub mod dkg;
pub mod keys;
pub mod scan;

pub use dalek_ff_group::{Ed25519, EdwardsPoint};
pub use dkg_pedpop::{Participant, ThresholdKeys, ThresholdParams};
pub use monero_wallet::address::Network;
pub use monero_wallet::ed25519::Scalar as MoneroScalar;
/// Re-exported so callers pass the same `rand_core` (0.6) the DKG uses.
pub use rand_chacha::rand_core;
use zeroize::Zeroize;

/// Why a treasury primitive refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreasuryError {
    /// `t`, `n` or the seat index is out of range.
    #[error("bad parameters: {0}")]
    Params(String),
    /// A frame from this participant did not decode.
    #[error("bad frame from participant {0}")]
    Frame(u16),
    /// The frames do not come from exactly the expected participants.
    #[error("unexpected participant set")]
    Participants,
    /// This participant's frame carries the identity point.
    #[error("identity point from participant {0}")]
    Identity(u16),
    /// This participant sent two different round-1 messages.
    #[error("equivocation by participant {0}")]
    Equivocation(u16),
    /// This participant's transcript hash differs from ours.
    #[error("transcript mismatch with participant {0}")]
    Transcript(u16),
    /// This participant's attestation does not verify.
    #[error("attestation of participant {0} invalid")]
    Attestation(u16),
    /// A keys record is damaged or inconsistent.
    #[error("keys record invalid")]
    Record,
    /// A scan file image is damaged or inconsistent.
    #[error("scan state invalid")]
    ScanState,
    /// The DKG library refused (its error, debug-formatted).
    #[error("dkg: {0}")]
    Dkg(String),
    /// The group key is the identity or torsioned.
    #[error("invalid spend key")]
    SpendKey,
}

/// One run of one init: what every run-bound layout starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Zeroize)]
pub struct RunId {
    /// The republic id, raw (the hex id decoded).
    pub republic_id: [u8; 32],
    /// The applied `wallet_init` proposal id.
    pub init_id: u64,
    /// The run nonce `r`.
    pub run: [u8; 32],
}

impl RunId {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.republic_id);
        out.extend_from_slice(&self.init_id.to_le_bytes());
        out.extend_from_slice(&self.run);
    }

    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(Self {
            republic_id: r.array()?,
            init_id: r.u64()?,
            run: r.array()?,
        })
    }
}

/// The fixed network byte (Monero's own order).
pub fn network_byte(network: Network) -> u8 {
    match network {
        Network::Mainnet => 0,
        Network::Testnet => 1,
        Network::Stagenet => 2,
    }
}

fn network_from_byte(b: u8) -> Option<Network> {
    match b {
        0 => Some(Network::Mainnet),
        1 => Some(Network::Testnet),
        2 => Some(Network::Stagenet),
        _ => None,
    }
}

/// A strict cursor over a tagged layout.
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(u32::from_le_bytes(self.array()?)).ok()?;
        self.take(len)
    }

    pub(crate) fn done(&self) -> bool {
        self.0.is_empty()
    }
}
