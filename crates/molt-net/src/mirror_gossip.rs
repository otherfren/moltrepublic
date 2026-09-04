// SPDX-License-Identifier: GPL-3.0-or-later

//! The mirror gossip (`docs/files/mirroring.md` §3.4): a seat's mirror
//! DECLARATION (on/off, quota), its hold STATUS per series, and the ask
//! that makes every holder answer with its status. Three control frames
//! in the reserved space, authenticated by the MLS credential like the
//! poke - gossip, never chain state; an older build drops the unknown
//! tags as no-ops.

use molt_core::MemberId;
use serde::{Deserialize, Serialize};

/// Tag of a declaration's MLS plaintext.
pub const MIRROR_DECL_TAG: &[u8] = b"\x00molt-mdecl-v1";
/// Tag of a status's MLS plaintext.
pub const MIRROR_STATUS_TAG: &[u8] = b"\x00molt-mstat-v1";
/// Tag of the ask's MLS plaintext.
pub const MIRROR_WHO_TAG: &[u8] = b"\x00molt-mwho-v1";

/// The wire version this build writes and accepts, all three frames.
pub const MIRROR_V: u32 = 1;

/// Refused UNPARSED above this: a status names every series a seat
/// holds, so it is the roomy one.
pub const MIRROR_FRAME_MAX_BYTES: usize = 256 * 1024;

/// Series per status frame.
pub const MIRROR_STATUS_MAX_HOLDS: usize = 4_096;

/// Why a plaintext is not a usable mirror frame.
#[derive(Debug, PartialEq, Eq)]
pub enum MirrorFrameError {
    /// Not this frame's tag.
    NotThisFrame,
    /// Beyond [`MIRROR_FRAME_MAX_BYTES`].
    TooBig(usize),
    /// Tagged, but not the frame.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
}

impl std::fmt::Display for MirrorFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MirrorFrameError::NotThisFrame => write!(f, "not a mirror frame"),
            MirrorFrameError::TooBig(n) => write!(f, "mirror frame is {n} bytes"),
            MirrorFrameError::Malformed => write!(f, "malformed mirror frame"),
            MirrorFrameError::UnknownVersion(v) => write!(f, "mirror frame version {v}"),
        }
    }
}

fn body_of<'a>(tag: &[u8], plaintext: &'a [u8]) -> Result<&'a [u8], MirrorFrameError> {
    let body = plaintext.strip_prefix(tag).ok_or(MirrorFrameError::NotThisFrame)?;
    if body.len() > MIRROR_FRAME_MAX_BYTES {
        return Err(MirrorFrameError::TooBig(body.len()));
    }
    Ok(body)
}

fn framed<T: Serialize>(tag: &[u8], value: &T) -> Vec<u8> {
    let mut out = tag.to_vec();
    if let Ok(json) = serde_json::to_vec(value) {
        out.extend_from_slice(&json);
    }
    out
}

/// `by` mirrors (or not) with `quota` bytes; `rev` orders declarations
/// (unix seconds of the change, last wins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorDeclFrame {
    /// Wire version. REQUIRED.
    pub v: u32,
    /// SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
    /// Consent.
    pub on: bool,
    /// The seat's total mirror budget for this republic, bytes.
    pub quota: u64,
    /// Revision; a lower one never overwrites a higher one.
    pub rev: u64,
}

impl MirrorDeclFrame {
    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        framed(MIRROR_DECL_TAG, self)
    }

    /// The ONLY consumer.
    pub fn from_frame(plaintext: &[u8]) -> Result<MirrorDeclFrame, MirrorFrameError> {
        let body = body_of(MIRROR_DECL_TAG, plaintext)?;
        let me: MirrorDeclFrame =
            serde_json::from_slice(body).map_err(|_| MirrorFrameError::Malformed)?;
        if me.v != MIRROR_V {
            return Err(MirrorFrameError::UnknownVersion(me.v));
        }
        Ok(me)
    }
}

/// What `by` holds: per series (share id hex) the verified data pieces
/// and the series' data piece count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorStatusFrame {
    /// Wire version. REQUIRED.
    pub v: u32,
    /// SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
    /// `(share id hex, held, of)`.
    pub holds: Vec<(String, u32, u32)>,
}

impl MirrorStatusFrame {
    /// TAG ‖ JSON (the first [`MIRROR_STATUS_MAX_HOLDS`] kept).
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        let mut me = self.clone();
        me.holds.truncate(MIRROR_STATUS_MAX_HOLDS);
        framed(MIRROR_STATUS_TAG, &me)
    }

    /// The ONLY consumer.
    pub fn from_frame(plaintext: &[u8]) -> Result<MirrorStatusFrame, MirrorFrameError> {
        let body = body_of(MIRROR_STATUS_TAG, plaintext)?;
        let me: MirrorStatusFrame =
            serde_json::from_slice(body).map_err(|_| MirrorFrameError::Malformed)?;
        if me.v != MIRROR_V {
            return Err(MirrorFrameError::UnknownVersion(me.v));
        }
        if me.holds.len() > MIRROR_STATUS_MAX_HOLDS || me.holds.iter().any(|(_, held, of)| held > of) {
            return Err(MirrorFrameError::Malformed);
        }
        Ok(me)
    }
}

/// `by` asks every holder for its status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorWhoFrame {
    /// Wire version. REQUIRED.
    pub v: u32,
    /// SELF-DESCRIPTION, checked against the MLS credential.
    pub by: MemberId,
}

impl MirrorWhoFrame {
    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        framed(MIRROR_WHO_TAG, self)
    }

    /// The ONLY consumer.
    pub fn from_frame(plaintext: &[u8]) -> Result<MirrorWhoFrame, MirrorFrameError> {
        let body = body_of(MIRROR_WHO_TAG, plaintext)?;
        let me: MirrorWhoFrame =
            serde_json::from_slice(body).map_err(|_| MirrorFrameError::Malformed)?;
        if me.v != MIRROR_V {
            return Err(MirrorFrameError::UnknownVersion(me.v));
        }
        Ok(me)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each frame round-trips under its own tag only; the tags never
    /// cross each other or the poke's.
    #[test]
    fn the_three_frames_round_trip_and_keep_to_their_tags() {
        let decl = MirrorDeclFrame { v: MIRROR_V, by: "petra".into(), on: true, quota: 5, rev: 9 };
        let status = MirrorStatusFrame {
            v: MIRROR_V,
            by: "petra".into(),
            holds: vec![("aa01".into(), 3, 3), ("aa02".into(), 1, 4)],
        };
        let who = MirrorWhoFrame { v: MIRROR_V, by: "petra".into() };
        assert_eq!(MirrorDeclFrame::from_frame(&decl.to_frame()).expect("decl"), decl);
        assert_eq!(MirrorStatusFrame::from_frame(&status.to_frame()).expect("status"), status);
        assert_eq!(MirrorWhoFrame::from_frame(&who.to_frame()).expect("who"), who);
        assert_eq!(MirrorDeclFrame::from_frame(&status.to_frame()), Err(MirrorFrameError::NotThisFrame));
        assert_eq!(MirrorStatusFrame::from_frame(&who.to_frame()), Err(MirrorFrameError::NotThisFrame));
        assert_eq!(MirrorWhoFrame::from_frame(&decl.to_frame()), Err(MirrorFrameError::NotThisFrame));
        assert_eq!(
            crate::poke::Poke::from_frame(&decl.to_frame()),
            Err(crate::poke::PokeError::NotAPoke)
        );
        let inverted = MirrorStatusFrame { holds: vec![("aa".into(), 5, 2)], ..status.clone() };
        assert_eq!(MirrorStatusFrame::from_frame(&inverted.to_frame()), Err(MirrorFrameError::Malformed));
        let mut future = MIRROR_WHO_TAG.to_vec();
        future.extend_from_slice(br#"{"v":2,"by":"p"}"#);
        assert_eq!(MirrorWhoFrame::from_frame(&future), Err(MirrorFrameError::UnknownVersion(2)));
    }
}
