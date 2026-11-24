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

/// Own seqs one listing names; a longer range is listed in pages.
pub const GAP_LIST_MAX: usize = 256;

/// Widest range one want may name: the receiver's accept window.
pub const GAP_RANGE_MAX: u64 = molt_core::ACCEPT_WINDOW_BITS;

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
    /// `(after, upto]`: list `of`'s own envelopes in this span, resend none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(u64, u64)>,
}

/// The sender's own ackable seqs in `(after, through]`, complete;
/// `through == after` with nothing listed says the log ends at `after`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapListing {
    /// Exclusive lower bound, as asked.
    pub after: u64,
    /// Inclusive upper bound this listing covers.
    pub through: u64,
    /// Every own ackable seq in the span.
    pub own: Vec<u64>,
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
    /// The answer to a range want.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<GapListing>,
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
    /// A range or listing out of shape.
    BadRange,
}

impl std::fmt::Display for GapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GapError::NotAGap => write!(f, "not a gap frame"),
            GapError::TooBig(n) => write!(f, "gap frame is {n} bytes"),
            GapError::Malformed => write!(f, "malformed gap frame"),
            GapError::UnknownVersion(v) => write!(f, "gap frame version {v}"),
            GapError::BadSeqs => write!(f, "gap frame seqs"),
            GapError::BadRange => write!(f, "gap frame range"),
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
        GapWant { v: GAP_V, by, of, seqs, range: None }
    }

    /// A want by `by` for the listing of `of`'s own seqs in `(after, upto]`.
    #[must_use]
    pub fn range(by: MemberId, of: MemberId, after: u64, upto: u64) -> GapWant {
        let upto = upto.min(after.saturating_add(GAP_RANGE_MAX));
        GapWant { v: GAP_V, by, of, seqs: Vec::new(), range: Some((after, upto)) }
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
        GapAnswer { v: GAP_V, by, to, resent, gone, listing: None }
    }

    /// A range answer by `by` to `to`.
    #[must_use]
    pub fn listing(by: MemberId, to: MemberId, listing: GapListing) -> GapAnswer {
        GapAnswer { v: GAP_V, by, to, resent: Vec::new(), gone: Vec::new(), listing: Some(listing) }
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
        if plaintext.starts_with(GAP_WANT_TAG) {
            let b = body(plaintext, GAP_WANT_TAG)?;
            let w: GapWant = serde_json::from_slice(b).map_err(|_| GapError::Malformed)?;
            if w.v != GAP_V {
                return Err(GapError::UnknownVersion(w.v));
            }
            if (w.seqs.is_empty() && w.range.is_none()) || !seqs_ok(&w.seqs) {
                return Err(GapError::BadSeqs);
            }
            if w.range.is_some_and(|(a, u)| a >= u || u - a > GAP_RANGE_MAX) {
                return Err(GapError::BadRange);
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
        if a.listing.as_ref().is_some_and(|l| {
            // `after == through`: "nothing above", an empty listing only
            l.after > l.through
                || (l.after == l.through && !l.own.is_empty())
                || l.own.len() > GAP_LIST_MAX
                || l.own.iter().any(|s| *s <= l.after || *s > l.through)
        }) {
            return Err(GapError::BadRange);
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
        let want = |seqs: Vec<u64>, range| GapWant { v: GAP_V, by: "a".into(), of: "b".into(), seqs, range };
        let zero = crate::framed(GAP_WANT_TAG, &want(vec![0], None));
        assert_eq!(GapFrame::from_frame(&zero), Err(GapError::BadSeqs));
        let empty = crate::framed(GAP_WANT_TAG, &want(vec![], None));
        assert_eq!(GapFrame::from_frame(&empty), Err(GapError::BadSeqs));
        let wide = crate::framed(GAP_WANT_TAG, &want(vec![], Some((10, 11 + GAP_RANGE_MAX))));
        assert_eq!(GapFrame::from_frame(&wide), Err(GapError::BadRange));
        let r = GapWant::range("a".into(), "b".into(), 10, 99_999);
        assert_eq!(r.range, Some((10, 10 + GAP_RANGE_MAX)), "clamped to one window");
        assert_eq!(GapFrame::from_frame(&r.to_frame()), Ok(GapFrame::Want(r)));
        let stray = GapAnswer::listing("b".into(), "a".into(), GapListing { after: 10, through: 20, own: vec![21] });
        assert_eq!(GapFrame::from_frame(&stray.to_frame()), Err(GapError::BadRange));
        let level = GapAnswer::listing("b".into(), "a".into(), GapListing { after: 10, through: 10, own: vec![] });
        assert_eq!(GapFrame::from_frame(&level.to_frame()), Ok(GapFrame::Answer(level)), "nothing above");
        let empty_lie = GapAnswer::listing("b".into(), "a".into(), GapListing { after: 10, through: 10, own: vec![10] });
        assert_eq!(GapFrame::from_frame(&empty_lie.to_frame()), Err(GapError::BadRange));
        let behind = GapAnswer::listing("b".into(), "a".into(), GapListing { after: 10, through: 9, own: vec![] });
        assert_eq!(GapFrame::from_frame(&behind.to_frame()), Err(GapError::BadRange));
        let mut big = GAP_WANT_TAG.to_vec();
        big.extend(std::iter::repeat_n(b' ', GAP_MAX_BYTES + 1));
        assert!(matches!(GapFrame::from_frame(&big), Err(GapError::TooBig(_))));
    }
}
