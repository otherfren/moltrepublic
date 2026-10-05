// SPDX-License-Identifier: GPL-3.0-or-later
//! The reader's combine-and-decrypt (spec §8 step 4).

use molt_core::vault::{SecretText, VaultDeposit};

use crate::hex_array;
use crate::payload::open_payload;
use crate::share::{combine, verify_share, Share};

/// One share at the reader: the seat it came from (the MLS sender, never
/// a frame field), that seat's x, the share.
#[derive(Debug, Clone)]
pub struct SeatShare {
    /// The seat.
    pub seat: String,
    /// Its genesis position.
    pub x: u8,
    /// The share.
    pub share: Share,
}

/// A successful read: the text, and every seat whose share failed Feldman.
#[derive(Debug)]
pub struct Opened {
    /// The plaintext. Never persisted.
    pub text: SecretText,
    /// Seats whose share was off the polynomial.
    pub bad_seats: Vec<String>,
}

/// A failed read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFault {
    /// Seats whose share was off the polynomial.
    pub bad_seats: Vec<String>,
    /// Valid shares held.
    pub have: u8,
    /// Shares needed.
    pub need: u8,
    /// Enough shares, but the payload did not open.
    pub payload: bool,
}

/// Check each share against the commitments, combine any `m` valid ones,
/// open the payload file.
///
/// # Errors
/// Fewer than `m` valid shares, or a payload that does not open.
pub fn read(
    dep: &VaultDeposit,
    republic_id: &str,
    shares: &[SeatShare],
    payload_file: &[u8],
) -> Result<Opened, ReadFault> {
    let commitments: Vec<[u8; 32]> = dep
        .commitments
        .iter()
        .filter_map(|c| hex_array::<32>(c))
        .collect();
    let mut bad_seats = Vec::new();
    let mut good: Vec<(u8, Share)> = Vec::new();
    for s in shares {
        if commitments.len() == dep.commitments.len() && verify_share(&commitments, s.x, &s.share) {
            if !good.iter().any(|(x, _)| *x == s.x) {
                good.push((s.x, s.share.clone()));
            }
        } else if !bad_seats.contains(&s.seat) {
            bad_seats.push(s.seat.clone());
        }
    }
    let have = u8::try_from(good.len()).unwrap_or(u8::MAX);
    let fault = |payload| ReadFault {
        bad_seats: bad_seats.clone(),
        have,
        need: dep.m,
        payload,
    };
    let s = combine(dep.m, &good).map_err(|_| fault(false))?;
    if dep.commitments.first() != Some(&hex::encode(s.commitment())) {
        return Err(fault(false));
    }
    let text = open_payload(dep, republic_id, &s, payload_file).map_err(|_| fault(true))?;
    Ok(Opened { text, bad_seats })
}
