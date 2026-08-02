// SPDX-License-Identifier: GPL-3.0-or-later

//! **N5.3 — the broadcast delivery ACK.**
//!
//! On the queue mesh an ACK needs no subject: it travels one leg, so "whose
//! acceptance is this" is answered by the leg itself and the recv loop only
//! has to pin `from == peer.member`. A kind-445 reaches every member at once,
//! which destroys exactly that answer — so the frame has to carry it.
//!
//! And it cannot carry it as one window. **Log seqs are node-private**:
//! `State::make_env` stamps `seq` from the local `next_seq` for own AND
//! foreign events, so A's seq 7 and B's seq 7 are unrelated events. A single
//! window is therefore meaningless to anyone but the one sender it describes,
//! and the sheet must be a MAP keyed by the subject it speaks about.
//!
//! The one inversion that compiles and is silently wrong: **the window I SEND
//! about A is in A's seq space; the window I RECEIVE from A is in mine.** The
//! sender ships `accepted` keyed by whom each window describes; the receiver
//! looks up its OWN name. That is pinned by a test, not by this comment.

use std::collections::BTreeMap;

use molt_core::{AcceptedWindow, MemberId};
use serde::{Deserialize, Serialize};

/// Tag of a broadcast ack's MLS plaintext: one NUL + 17 ASCII.
///
/// A NEW tag, never a reshape of [`crate::MESH_ACK_TAG`]. `AcceptedWindow`
/// carries `#[serde(default)]` on both fields and no `deny_unknown_fields`, so
/// ANY JSON object deserializes as `{ high: 0, bits: [] }` — a mesh-era reader
/// meeting this frame under the old tag would latch `ack_seen` at floor 0 and
/// republish the entire log to the entire republic, from nothing but a version
/// mismatch. The tag string IS the version boundary; `v` guards a FUTURE shape
/// from reading as this one.
pub const GROUP_ACK_TAG: &[u8] = b"\x00molt-group-ack-v1";

/// The wire version this build writes and accepts.
pub const GROUP_ACK_V: u32 = 1;

/// Refused UNPARSED above this — a bound before serde, not after.
pub const GROUP_ACK_MAX_BYTES: usize = 64 * 1024;

/// One member's claim sheet: what it has accepted, per subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAck {
    /// Wire version. REQUIRED — deliberately no serde default, so a shape
    /// change cannot silently read as v1.
    pub v: u32,
    /// The claimant. REQUIRED, and checked against the MLS credential on
    /// arrival — this field is SELF-DESCRIPTION, never the authentication.
    /// Carrying it turns a future routing bug into a drop instead of a
    /// misapplication.
    pub by: MemberId,
    /// subject → what `by` accepted of THAT SUBJECT's log seqs.
    pub claims: BTreeMap<MemberId, AcceptedWindow>,
}

/// Why a plaintext is not a usable ack.
#[derive(Debug, PartialEq, Eq)]
pub enum GroupAckError {
    /// No ack tag — some other control frame, or an application envelope.
    NotAnAck,
    /// Beyond [`GROUP_ACK_MAX_BYTES`], refused before parsing.
    TooBig(usize),
    /// Tagged, but not a `GroupAck`.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
}

impl std::fmt::Display for GroupAckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupAckError::NotAnAck => write!(f, "not a group ack"),
            GroupAckError::TooBig(n) => write!(f, "group ack is {n} bytes"),
            GroupAckError::Malformed => write!(f, "malformed group ack"),
            GroupAckError::UnknownVersion(v) => write!(f, "group ack version {v}"),
        }
    }
}

impl GroupAck {
    /// A sheet from `by` over `claims`.
    #[must_use]
    pub fn new(by: MemberId, claims: BTreeMap<MemberId, AcceptedWindow>) -> GroupAck {
        GroupAck {
            v: GROUP_ACK_V,
            by,
            claims,
        }
    }

    /// TAG ‖ JSON — the ONLY producer of these bytes.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = GROUP_ACK_TAG.to_vec();
        if let Ok(json) = serde_json::to_vec(self) {
            out.extend_from_slice(&json);
        }
        out
    }

    /// The ONLY consumer. Bounds the input before serde sees it.
    pub fn from_frame(plaintext: &[u8]) -> Result<GroupAck, GroupAckError> {
        let Some(body) = plaintext.strip_prefix(GROUP_ACK_TAG) else {
            return Err(GroupAckError::NotAnAck);
        };
        if body.len() > GROUP_ACK_MAX_BYTES {
            return Err(GroupAckError::TooBig(body.len()));
        }
        let ack: GroupAck = serde_json::from_slice(body).map_err(|_| GroupAckError::Malformed)?;
        if ack.v != GROUP_ACK_V {
            return Err(GroupAckError::UnknownVersion(ack.v));
        }
        Ok(ack)
    }

    /// What `me` may act on from this sheet, once the MLS credential has
    /// already proven who sent it.
    ///
    /// `None` — and it MUST stay `None` rather than becoming a zero — when the
    /// sheet says nothing about `me`, or claims a window that asserts nothing.
    /// A floor of 0 latched as "proven" makes the sender rewind to the start
    /// and republish its whole log to the whole republic, which is precisely
    /// the amplification one small frame must never buy.
    #[must_use]
    pub fn window_for(&self, me: &str) -> Option<&AcceptedWindow> {
        self.claims.get(me).filter(|w| w.high > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(high: u64) -> AcceptedWindow {
        let mut w = AcceptedWindow::default();
        w.accept(high);
        w
    }

    /// A sheet round-trips, and only through its own tag.
    #[test]
    fn a_sheet_round_trips_under_its_own_tag() {
        let mut claims = BTreeMap::new();
        claims.insert("walter".to_string(), win(42));
        let ack = GroupAck::new("petra".to_string(), claims);
        let frame = ack.to_frame();
        assert!(frame.starts_with(GROUP_ACK_TAG));
        assert_eq!(GroupAck::from_frame(&frame).expect("round trip"), ack);

        // an untagged plaintext is not an ack — that is how an application
        // envelope on the same channel stays an application envelope
        assert_eq!(
            GroupAck::from_frame(br#"{"v":1,"by":"petra","claims":{}}"#),
            Err(GroupAckError::NotAnAck)
        );
    }

    /// The mesh ack tag must not read as a group ack, nor the reverse.
    ///
    /// `AcceptedWindow` has serde defaults on every field and no
    /// `deny_unknown_fields`, so almost any JSON object parses as
    /// `{high: 0, bits: []}`. If the two frames shared a tag, a reader would
    /// latch a floor of 0 as PROVEN and the sender would republish its entire
    /// log to the entire republic — an amplification bought with one small
    /// frame and a version mismatch.
    #[test]
    fn the_group_tag_and_the_mesh_tag_do_not_cross() {
        let ack = GroupAck::new("petra".to_string(), BTreeMap::new());
        let frame = ack.to_frame();
        assert!(
            !frame.starts_with(crate::MESH_ACK_TAG),
            "a group ack must not present itself as a mesh ack"
        );
        let mesh = [crate::MESH_ACK_TAG, br#"{"high":9,"bits":[1]}"#].concat();
        assert_eq!(GroupAck::from_frame(&mesh), Err(GroupAckError::NotAnAck));
    }

    /// A future version is refused, not read as v1.
    #[test]
    fn an_unknown_version_is_refused() {
        let mut ack = GroupAck::new("petra".to_string(), BTreeMap::new());
        ack.v = 7;
        assert_eq!(
            GroupAck::from_frame(&ack.to_frame()),
            Err(GroupAckError::UnknownVersion(7))
        );
    }

    /// A sheet that says nothing about me yields nothing to act on — never a
    /// floor of zero.
    #[test]
    fn a_sheet_silent_about_me_proves_nothing() {
        let mut claims = BTreeMap::new();
        claims.insert("zoe".to_string(), win(11));
        // …and an explicitly empty window is silence too, not a claim of zero
        claims.insert("walter".to_string(), AcceptedWindow::default());
        let ack = GroupAck::new("petra".to_string(), claims);
        assert!(ack.window_for("walter").is_none(), "high == 0 proves nothing");
        assert!(ack.window_for("quentin").is_none(), "absent proves nothing");
        assert!(ack.window_for("zoe").is_some());
    }

    /// Oversized input is refused before serde is asked to parse it.
    #[test]
    fn an_oversized_sheet_is_refused_unparsed() {
        let body = vec![b'x'; GROUP_ACK_MAX_BYTES + 1];
        let frame = [GROUP_ACK_TAG, &body].concat();
        assert!(matches!(
            GroupAck::from_frame(&frame),
            Err(GroupAckError::TooBig(_))
        ));
    }
}
