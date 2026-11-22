// SPDX-License-Identifier: GPL-3.0-or-later

//! The gap control frames (`docs_archive/transport/delivery_guarantee.md`
//! §10): a receiver holding a sender's stream behind a missing seq names
//! it ([`GapWant`]); the sender re-publishes exactly those seqs and says
//! which it has not got to give ([`GapAnswer`]). Control-frame space, like
//! the poke: an older build drops both tags as a no-op.

use molt_core::MemberId;
use serde::{Deserialize, Serialize};

/// Tag of a gap want's MLS plaintext.
pub const GAP_WANT_TAG: &[u8] = b"\x00molt-gapwant-v1";

/// Tag of a gap answer's MLS plaintext.
pub const GAP_ANSWER_TAG: &[u8] = b"\x00molt-gapans-v1";

/// The wire version this build writes and accepts.
pub const GAP_V: u32 = 1;

/// Seqs one frame names; a longer gap asks again.
pub const GAP_MAX_SEQS: usize = 64;

/// Refused UNPARSED above this.
pub const GAP_MAX_BYTES: usize = 8 * 1024;

/// `by` misses these seqs of `of`'s stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapWant {
    /// Wire version. REQUIRED, no serde default.
    pub v: u32,
    /// The receiver asking - SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
    /// The sender whose seqs are missing; only it answers.
    pub of: MemberId,
    /// The missing seqs, in `of`'s log seq space.
    pub seqs: Vec<u64>,
}

/// `by`'s answer to `to`'s want: `resent` went out again just before this
/// frame, `gone` will never come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapAnswer {
    /// Wire version. REQUIRED, no serde default.
    pub v: u32,
    /// The answering sender - SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
    /// The receiver that asked.
    pub to: MemberId,
    /// Re-published.
    pub resent: Vec<u64>,
    /// Not an envelope the asker is owed.
    pub gone: Vec<u64>,
}

/// One gap frame, either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapFrame {
    /// A receiver names what it misses.
    Want(GapWant),
    /// The sender's answer.
    Answer(GapAnswer),
}

impl GapFrame {
    /// The self-described sender.
    #[must_use]
    pub fn by(&self) -> &MemberId {
        match self {
            GapFrame::Want(w) => &w.by,
            GapFrame::Answer(a) => &a.by,
        }
    }
}

/// Why a plaintext is not a usable gap frame.
#[derive(Debug, PartialEq, Eq)]
pub enum GapError {
    /// No gap tag.
    NotAGap,
    /// Beyond [`GAP_MAX_BYTES`].
    TooBig(usize),
    /// Tagged, but not the shape.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
    /// Empty, or more than [`GAP_MAX_SEQS`] seqs.
    BadSeqs,
}

impl std::fmt::Display for GapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GapError::NotAGap => write!(f, "not a gap frame"),
            GapError::TooBig(n) => write!(f, "gap frame is {n} bytes"),
            GapError::Malformed => write!(f, "malformed gap frame"),
            GapError::UnknownVersion(v) => write!(f, "gap frame version {v}"),
            GapError::BadSeqs => write!(f, "gap frame seqs"),
        }
    }
}

impl GapWant {
    /// A want by `by` for `of`'s `seqs` (the lowest [`GAP_MAX_SEQS`] kept).
    #[must_use]
    pub fn new(by: MemberId, of: MemberId, mut seqs: Vec<u64>) -> GapWant {
        seqs.sort_unstable();
        seqs.dedup();
        seqs.truncate(GAP_MAX_SEQS);
        GapWant { v: GAP_V, by, of, seqs }
    }

    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        crate::framed(GAP_WANT_TAG, self)
    }
}

impl GapAnswer {
    /// An answer by `by` to `to`.
    #[must_use]
    pub fn new(by: MemberId, to: MemberId, resent: Vec<u64>, gone: Vec<u64>) -> GapAnswer {
        GapAnswer { v: GAP_V, by, to, resent, gone }
    }

    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        crate::framed(GAP_ANSWER_TAG, self)
    }
}

fn body<'a>(plaintext: &'a [u8], tag: &[u8]) -> Result<&'a [u8], GapError> {
    let body = plaintext.strip_prefix(tag).ok_or(GapError::NotAGap)?;
    if body.len() > GAP_MAX_BYTES {
        return Err(GapError::TooBig(body.len()));
    }
    Ok(body)
}

fn seqs_ok(seqs: &[u64]) -> bool {
    seqs.len() <= GAP_MAX_SEQS && !seqs.contains(&0)
}

impl GapFrame {
    /// The ONLY consumer of both tags. Bounds the input before serde.
    pub fn from_frame(plaintext: &[u8]) -> Result<GapFrame, GapError> {
        if let Ok(b) = body(plaintext, GAP_WANT_TAG) {
            let w: GapWant = serde_json::from_slice(b).map_err(|_| GapError::Malformed)?;
            if w.v != GAP_V {
                return Err(GapError::UnknownVersion(w.v));
            }
            if w.seqs.is_empty() || !seqs_ok(&w.seqs) {
                return Err(GapError::BadSeqs);
            }
            return Ok(GapFrame::Want(w));
        }
        let b = body(plaintext, GAP_ANSWER_TAG)?;
        let a: GapAnswer = serde_json::from_slice(b).map_err(|_| GapError::Malformed)?;
        if a.v != GAP_V {
            return Err(GapError::UnknownVersion(a.v));
        }
        if !seqs_ok(&a.resent) || !seqs_ok(&a.gone) {
            return Err(GapError::BadSeqs);
        }
        Ok(GapFrame::Answer(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_gap_frames_round_trip_under_their_own_tags() {
        let want = GapWant::new("ada".into(), "bob".into(), vec![9, 3, 3]);
        assert_eq!(want.seqs, vec![3, 9], "sorted, deduped");
        let frame = want.to_frame();
        assert!(frame.starts_with(GAP_WANT_TAG));
        assert_eq!(GapFrame::from_frame(&frame), Ok(GapFrame::Want(want)));
        let ans = GapAnswer::new("bob".into(), "ada".into(), vec![3], vec![9]);
        assert_eq!(GapFrame::from_frame(&ans.to_frame()), Ok(GapFrame::Answer(ans)));
        assert_eq!(GapFrame::from_frame(b"\x00molt-poke-v1{}"), Err(GapError::NotAGap));
    }

    #[test]
    fn a_gap_want_is_bounded_and_never_names_seq_zero() {
        let many: Vec<u64> = (1..=200).collect();
        assert_eq!(GapWant::new("a".into(), "b".into(), many).seqs.len(), GAP_MAX_SEQS);
        let zero = crate::framed(GAP_WANT_TAG, &GapWant { v: GAP_V, by: "a".into(), of: "b".into(), seqs: vec![0] });
        assert_eq!(GapFrame::from_frame(&zero), Err(GapError::BadSeqs));
        let empty = crate::framed(GAP_WANT_TAG, &GapWant { v: GAP_V, by: "a".into(), of: "b".into(), seqs: vec![] });
        assert_eq!(GapFrame::from_frame(&empty), Err(GapError::BadSeqs));
        let mut big = GAP_WANT_TAG.to_vec();
        big.extend(std::iter::repeat_n(b' ', GAP_MAX_BYTES + 1));
        assert!(matches!(GapFrame::from_frame(&big), Err(GapError::TooBig(_))));
    }
}
