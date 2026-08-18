// SPDX-License-Identifier: GPL-3.0-or-later

//! The poke control frame: a directed, ephemeral nudge.
//!
//! A poke is **not** a log event. It rides the reserved control-frame space
//! (`\x00molt-…`) exactly like the delivery acks, which buys three things a
//! `WorkspaceEvent` cannot:
//!
//! * an OLDER build discards an unknown control tag as a no-op
//!   (`supervisor::decode`), instead of refusing to open a workspace whose
//!   log holds a variant it cannot decode;
//! * the sender's log stays free of a frame that carries no state, so
//!   compaction and the acked floor never see it;
//! * fire-and-forget is the honest delivery semantics for a nudge — a poke
//!   that waited three days for a peer to come back is noise, not news.
//!
//! The frame is a broadcast on the group channel: every member decrypts it,
//! only `to` reacts. That is a property of the one shared channel, not a
//! promise of privacy — the tag says who poked whom to everyone in the group.

use molt_core::MemberId;
use serde::{Deserialize, Serialize};

/// Tag of a poke's MLS plaintext: one NUL + 12 ASCII.
///
/// Its own tag, never a reshape of another control frame's: the tag string
/// IS the version boundary, and `v` guards a FUTURE shape from reading as
/// this one.
pub const POKE_TAG: &[u8] = b"\x00molt-poke-v1";

/// The wire version this build writes and accepts.
pub const POKE_V: u32 = 1;

/// Refused UNPARSED above this — a bound before serde, not after. A poke
/// carries two member handles and nothing else, so the bound is generous
/// already.
pub const POKE_MAX_BYTES: usize = 4 * 1024;

/// How many pokes may await publication before further ones are dropped.
/// Small on purpose: a nudge is worth one best-effort attempt, and a queue
/// that absorbs a flood would only publish it late.
pub const POKE_QUEUE: usize = 8;

/// One nudge from `by` to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poke {
    /// Wire version. REQUIRED — deliberately no serde default, so a shape
    /// change cannot silently read as v1.
    pub v: u32,
    /// The poker. REQUIRED, and checked against the MLS credential on
    /// arrival — SELF-DESCRIPTION, never the authentication.
    pub by: MemberId,
    /// The poked member. Everyone decrypts the frame; only this member acts.
    pub to: MemberId,
}

/// Why a plaintext is not a usable poke.
#[derive(Debug, PartialEq, Eq)]
pub enum PokeError {
    /// No poke tag — some other control frame, or an application envelope.
    NotAPoke,
    /// Beyond [`POKE_MAX_BYTES`], refused before parsing.
    TooBig(usize),
    /// Tagged, but not a `Poke`.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
}

impl std::fmt::Display for PokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PokeError::NotAPoke => write!(f, "not a poke"),
            PokeError::TooBig(n) => write!(f, "poke is {n} bytes"),
            PokeError::Malformed => write!(f, "malformed poke"),
            PokeError::UnknownVersion(v) => write!(f, "poke version {v}"),
        }
    }
}

impl Poke {
    /// A nudge from `by` to `to`.
    #[must_use]
    pub fn new(by: MemberId, to: MemberId) -> Poke {
        Poke { v: POKE_V, by, to }
    }

    /// TAG ‖ JSON — the ONLY producer of these bytes.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = POKE_TAG.to_vec();
        if let Ok(json) = serde_json::to_vec(self) {
            out.extend_from_slice(&json);
        }
        out
    }

    /// The ONLY consumer. Bounds the input before serde sees it.
    pub fn from_frame(plaintext: &[u8]) -> Result<Poke, PokeError> {
        let Some(body) = plaintext.strip_prefix(POKE_TAG) else {
            return Err(PokeError::NotAPoke);
        };
        if body.len() > POKE_MAX_BYTES {
            return Err(PokeError::TooBig(body.len()));
        }
        let poke: Poke = serde_json::from_slice(body).map_err(|_| PokeError::Malformed)?;
        if poke.v != POKE_V {
            return Err(PokeError::UnknownVersion(poke.v));
        }
        Ok(poke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poke round-trips, and only through its own tag.
    #[test]
    fn a_poke_round_trips_under_its_own_tag() {
        let poke = Poke::new("petra".to_string(), "walter".to_string());
        let frame = poke.to_frame();
        assert!(frame.starts_with(POKE_TAG));
        assert_eq!(Poke::from_frame(&frame).expect("round trip"), poke);

        // an untagged plaintext is not a poke — that is how an application
        // envelope on the same channel stays an application envelope
        assert_eq!(
            Poke::from_frame(br#"{"v":1,"by":"petra","to":"walter"}"#),
            Err(PokeError::NotAPoke)
        );
    }

    /// The control tags never cross: a poke is not an ack and vice versa.
    /// Both would otherwise parse loosely enough to be mistaken, and an ack
    /// misread as a poke (or the reverse) is a wedge, not a nuisance.
    #[test]
    fn the_poke_tag_does_not_cross_the_ack_tags() {
        let poke = Poke::new("petra".to_string(), "walter".to_string());
        let frame = poke.to_frame();
        assert!(!frame.starts_with(crate::MESH_ACK_TAG));
        assert!(!frame.starts_with(crate::group_ack::GROUP_ACK_TAG));
        assert_eq!(
            crate::group_ack::GroupAck::from_frame(&frame),
            Err(crate::group_ack::GroupAckError::NotAnAck)
        );

        let ack = crate::group_ack::GroupAck::new("petra".to_string(), Default::default());
        assert_eq!(Poke::from_frame(&ack.to_frame()), Err(PokeError::NotAPoke));
    }

    /// A future shape is refused rather than read as this one, and an
    /// oversized body never reaches serde.
    #[test]
    fn a_future_version_and_an_oversized_body_are_refused() {
        let mut frame = POKE_TAG.to_vec();
        frame.extend_from_slice(br#"{"v":2,"by":"petra","to":"walter"}"#);
        assert_eq!(Poke::from_frame(&frame), Err(PokeError::UnknownVersion(2)));

        let mut big = POKE_TAG.to_vec();
        big.extend_from_slice(&vec![b'x'; POKE_MAX_BYTES + 1]);
        assert_eq!(
            Poke::from_frame(&big),
            Err(PokeError::TooBig(POKE_MAX_BYTES + 1))
        );

        let mut junk = POKE_TAG.to_vec();
        junk.extend_from_slice(b"not json");
        assert_eq!(Poke::from_frame(&junk), Err(PokeError::Malformed));
    }
}
