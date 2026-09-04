// SPDX-License-Identifier: GPL-3.0-or-later

//! The `PieceWanted` control frame (`docs_archive/files/mirroring.md` §3.2): a
//! member names the pieces of a v2 series it still misses; the holders
//! that have them re-publish exactly those. Rides the reserved
//! control-frame space like the poke - no log event, fire-and-forget, an
//! older build drops the unknown tag as a no-op.

use molt_core::MemberId;
use serde::{Deserialize, Serialize};

/// Tag of the frame's MLS plaintext: one NUL + 13 ASCII.
pub const PIECE_WANT_TAG: &[u8] = b"\x00molt-pwant-v1";

/// The wire version this build writes and accepts.
pub const PIECE_WANT_V: u32 = 1;

/// Refused UNPARSED above this.
pub const PIECE_WANT_MAX_BYTES: usize = 8 * 1024;

/// Ranges per frame; a longer miss list asks again later.
pub const PIECE_WANT_MAX_RANGES: usize = 64;

/// The pieces `by` misses of series `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceWanted {
    /// Wire version. REQUIRED, no serde default.
    pub v: u32,
    /// The requester - SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
    /// The share message id, lowercase hex.
    pub id: String,
    /// Inclusive piece index ranges, `lo <= hi`.
    pub ranges: Vec<(u32, u32)>,
}

/// Why a plaintext is not a usable `PieceWanted`.
#[derive(Debug, PartialEq, Eq)]
pub enum PieceWantError {
    /// No `PieceWanted` tag.
    NotAWant,
    /// Beyond [`PIECE_WANT_MAX_BYTES`].
    TooBig(usize),
    /// Tagged, but not a `PieceWanted`.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
    /// More than [`PIECE_WANT_MAX_RANGES`] ranges, or a range with `lo > hi`.
    BadRanges,
}

impl std::fmt::Display for PieceWantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PieceWantError::NotAWant => write!(f, "not a piece want"),
            PieceWantError::TooBig(n) => write!(f, "piece want is {n} bytes"),
            PieceWantError::Malformed => write!(f, "malformed piece want"),
            PieceWantError::UnknownVersion(v) => write!(f, "piece want version {v}"),
            PieceWantError::BadRanges => write!(f, "piece want ranges"),
        }
    }
}

impl PieceWanted {
    /// A want by `by` for `ranges` of `id` (the first
    /// [`PIECE_WANT_MAX_RANGES`] kept).
    #[must_use]
    pub fn new(by: MemberId, id: String, mut ranges: Vec<(u32, u32)>) -> PieceWanted {
        ranges.truncate(PIECE_WANT_MAX_RANGES);
        PieceWanted { v: PIECE_WANT_V, by, id, ranges }
    }

    /// TAG ‖ JSON - the ONLY producer of these bytes.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = PIECE_WANT_TAG.to_vec();
        if let Ok(json) = serde_json::to_vec(self) {
            out.extend_from_slice(&json);
        }
        out
    }

    /// The ONLY consumer. Bounds the input before serde sees it.
    pub fn from_frame(plaintext: &[u8]) -> Result<PieceWanted, PieceWantError> {
        let Some(body) = plaintext.strip_prefix(PIECE_WANT_TAG) else {
            return Err(PieceWantError::NotAWant);
        };
        if body.len() > PIECE_WANT_MAX_BYTES {
            return Err(PieceWantError::TooBig(body.len()));
        }
        let want: PieceWanted = serde_json::from_slice(body).map_err(|_| PieceWantError::Malformed)?;
        if want.v != PIECE_WANT_V {
            return Err(PieceWantError::UnknownVersion(want.v));
        }
        if want.ranges.is_empty()
            || want.ranges.len() > PIECE_WANT_MAX_RANGES
            || want.ranges.iter().any(|(lo, hi)| lo > hi)
        {
            return Err(PieceWantError::BadRanges);
        }
        Ok(want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A want round-trips under its own tag only, and its ranges are checked.
    #[test]
    fn a_piece_want_round_trips_and_checks_its_ranges() {
        let want = PieceWanted::new("petra".into(), "aa01".into(), vec![(1, 1), (4, 9)]);
        let frame = want.to_frame();
        assert!(frame.starts_with(PIECE_WANT_TAG));
        assert!(!frame.starts_with(crate::poke::POKE_TAG));
        assert_eq!(PieceWanted::from_frame(&frame).expect("round trip"), want);
        assert_eq!(
            crate::poke::Poke::from_frame(&frame),
            Err(crate::poke::PokeError::NotAPoke)
        );
        let inverted = PieceWanted { ranges: vec![(5, 2)], ..want.clone() };
        assert_eq!(PieceWanted::from_frame(&inverted.to_frame()), Err(PieceWantError::BadRanges));
        let mut too_many = want.clone();
        too_many.ranges = (0..70).map(|i| (i, i)).collect();
        assert_eq!(PieceWanted::from_frame(&too_many.to_frame()), Err(PieceWantError::BadRanges));
        assert_eq!(
            PieceWanted::new("p".into(), "x".into(), (0..70).map(|i| (i, i)).collect()).ranges.len(),
            PIECE_WANT_MAX_RANGES
        );
        let mut future = PIECE_WANT_TAG.to_vec();
        future.extend_from_slice(br#"{"v":2,"by":"p","id":"x","ranges":[[0,0]]}"#);
        assert_eq!(PieceWanted::from_frame(&future), Err(PieceWantError::UnknownVersion(2)));
    }
}
