// SPDX-License-Identifier: GPL-3.0-or-later

//! The vault's four control frames (`docs/vault/vault_threshold_disclosure.md`
//! §7, §8.3; plan stage S3a): a holder's receipt or complaint, the
//! depositor's complaint reveal, a holder's answer to a grant, and a
//! reader's ask. Status and transport, never chain state; authenticated by
//! the MLS sender credential (`by` is self-description and must equal it).
//! Each tag is its own version boundary: an older build drops them as
//! unknown control frames.

use molt_core::vault::SecretHex;
use molt_core::MemberId;
use serde::{Deserialize, Serialize};

/// Tag of a receipt / complaint.
pub const VAULT_RECEIPT_TAG: &[u8] = b"\x00molt-vrcpt-v1";
/// Tag of a complaint reveal.
pub const VAULT_REVEAL_TAG: &[u8] = b"\x00molt-vrevl-v1";
/// Tag of an answer to a grant.
pub const VAULT_RESP_TAG: &[u8] = b"\x00molt-vresp-v1";
/// Tag of a reader's ask.
pub const VAULT_ASK_TAG: &[u8] = b"\x00molt-vask-v1";

/// The four tags, for the disjointness checks.
pub const VAULT_TAGS: [&[u8]; 4] = [VAULT_RECEIPT_TAG, VAULT_REVEAL_TAG, VAULT_RESP_TAG, VAULT_ASK_TAG];

/// The wire version this build writes and accepts, all four frames.
pub const VAULT_V: u32 = 1;

/// Refused unparsed above this: every honest frame is a few hundred bytes.
pub const VAULT_FRAME_MAX_BYTES: usize = 4 * 1024;

/// Hex length of a `secret_id` / `grant_id` (sha256) and of a share or an
/// ephemeral ikm (32 bytes).
const HEX32: usize = 64;
/// Hex length of an answer: one HPKE-sealed share (80 bytes).
const HEX_RESP: usize = 160;
/// The longest member name a frame may carry.
const NAME_MAX: usize = 256;

/// Why a plaintext is not a usable vault frame.
#[derive(Debug, PartialEq, Eq)]
pub enum VaultFrameError {
    /// None of the four tags.
    NotThisFrame,
    /// Beyond [`VAULT_FRAME_MAX_BYTES`].
    TooBig(usize),
    /// Tagged, but not the frame.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
}

impl std::fmt::Display for VaultFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultFrameError::NotThisFrame => write!(f, "not a vault frame"),
            VaultFrameError::TooBig(n) => write!(f, "vault frame is {n} bytes"),
            VaultFrameError::Malformed => write!(f, "malformed vault frame"),
            VaultFrameError::UnknownVersion(v) => write!(f, "vault frame version {v}"),
        }
    }
}

/// A holder's verdict on its share and the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultVerdict {
    /// Share and payload verified.
    Verified,
    /// The share or the payload failed.
    Complaint,
}

impl VaultVerdict {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            VaultVerdict::Verified => "verified",
            VaultVerdict::Complaint => "complaint",
        }
    }
}

/// `by` verified (or complains about) its share of `secret_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultReceiptFrame {
    /// Wire version.
    pub v: u32,
    /// Self-description, checked against the MLS credential.
    pub by: MemberId,
    /// The deposit version.
    pub secret_id: String,
    /// The verdict.
    pub verdict: VaultVerdict,
    /// Revision; last wins.
    pub rev: u64,
}

/// The depositor `by` answers `holder`'s complaint with its share and
/// ephemeral ikm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRevealFrame {
    /// Wire version.
    pub v: u32,
    /// Self-description, checked against the MLS credential.
    pub by: MemberId,
    /// The deposit version.
    pub secret_id: String,
    /// The complaining holder.
    pub holder: MemberId,
    /// The holder's share, hex.
    pub share: SecretHex,
    /// The holder's ephemeral ikm, hex.
    pub ikm: SecretHex,
}

/// `by` answers grant `grant_id` with its share sealed to the reader. No
/// seat field: the reader takes the seat from the MLS sender (plan 1.3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRespFrame {
    /// Wire version.
    pub v: u32,
    /// Self-description, checked against the MLS credential.
    pub by: MemberId,
    /// The grant answered.
    pub grant_id: String,
    /// The sealed share, hex.
    pub enc: SecretHex,
}

/// The reader `by` asks the holders to answer `grant_id` again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultAskFrame {
    /// Wire version.
    pub v: u32,
    /// Self-description, checked against the MLS credential.
    pub by: MemberId,
    /// The grant asked about.
    pub grant_id: String,
}

/// One vault control frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultFrame {
    /// A receipt or complaint.
    Receipt(VaultReceiptFrame),
    /// A complaint reveal.
    Reveal(VaultRevealFrame),
    /// An answer to a grant.
    Resp(VaultRespFrame),
    /// A reader's ask.
    Ask(VaultAskFrame),
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= NAME_MAX
}

fn framed<T: Serialize>(tag: &[u8], value: &T) -> Vec<u8> {
    let mut out = tag.to_vec();
    if let Ok(json) = serde_json::to_vec(value) {
        out.extend_from_slice(&json);
    }
    out
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, VaultFrameError> {
    // the version first, so a newer frame reads as a version, not as garbage
    #[derive(Deserialize)]
    struct V {
        v: u32,
    }
    let v: V = serde_json::from_slice(body).map_err(|_| VaultFrameError::Malformed)?;
    if v.v != VAULT_V {
        return Err(VaultFrameError::UnknownVersion(v.v));
    }
    serde_json::from_slice(body).map_err(|_| VaultFrameError::Malformed)
}

impl VaultFrame {
    /// The self-described sender.
    #[must_use]
    pub fn by(&self) -> &MemberId {
        match self {
            VaultFrame::Receipt(f) => &f.by,
            VaultFrame::Reveal(f) => &f.by,
            VaultFrame::Resp(f) => &f.by,
            VaultFrame::Ask(f) => &f.by,
        }
    }

    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        match self {
            VaultFrame::Receipt(f) => framed(VAULT_RECEIPT_TAG, f),
            VaultFrame::Reveal(f) => framed(VAULT_REVEAL_TAG, f),
            VaultFrame::Resp(f) => framed(VAULT_RESP_TAG, f),
            VaultFrame::Ask(f) => framed(VAULT_ASK_TAG, f),
        }
    }

    /// The ONLY consumer: tag, size cap, version, shape.
    pub fn from_frame(plaintext: &[u8]) -> Result<VaultFrame, VaultFrameError> {
        let (tag, body) = VAULT_TAGS
            .iter()
            .find_map(|t| plaintext.strip_prefix(*t).map(|b| (*t, b)))
            .ok_or(VaultFrameError::NotThisFrame)?;
        if body.len() > VAULT_FRAME_MAX_BYTES {
            return Err(VaultFrameError::TooBig(body.len()));
        }
        let frame = if tag == VAULT_RECEIPT_TAG {
            let f: VaultReceiptFrame = parse(body)?;
            (name_ok(&f.by) && is_hex(&f.secret_id, HEX32)).then_some(VaultFrame::Receipt(f))
        } else if tag == VAULT_REVEAL_TAG {
            let f: VaultRevealFrame = parse(body)?;
            (name_ok(&f.by)
                && name_ok(&f.holder)
                && is_hex(&f.secret_id, HEX32)
                && is_hex(&f.share.0, HEX32)
                && is_hex(&f.ikm.0, HEX32))
            .then_some(VaultFrame::Reveal(f))
        } else if tag == VAULT_RESP_TAG {
            let f: VaultRespFrame = parse(body)?;
            (name_ok(&f.by) && is_hex(&f.grant_id, HEX32) && is_hex(&f.enc.0, HEX_RESP))
                .then_some(VaultFrame::Resp(f))
        } else {
            let f: VaultAskFrame = parse(body)?;
            (name_ok(&f.by) && is_hex(&f.grant_id, HEX32)).then_some(VaultFrame::Ask(f))
        };
        frame.ok_or(VaultFrameError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char, n: usize) -> String {
        std::iter::repeat_n(c, n).collect()
    }

    fn all() -> Vec<VaultFrame> {
        vec![
            VaultFrame::Receipt(VaultReceiptFrame {
                v: VAULT_V,
                by: "a".into(),
                secret_id: h('1', 64),
                verdict: VaultVerdict::Complaint,
                rev: 3,
            }),
            VaultFrame::Reveal(VaultRevealFrame {
                v: VAULT_V,
                by: "a".into(),
                secret_id: h('2', 64),
                holder: "b".into(),
                share: SecretHex(h('3', 64)),
                ikm: SecretHex(h('4', 64)),
            }),
            VaultFrame::Resp(VaultRespFrame {
                v: VAULT_V,
                by: "c".into(),
                grant_id: h('5', 64),
                enc: SecretHex(h('6', 160)),
            }),
            VaultFrame::Ask(VaultAskFrame { v: VAULT_V, by: "d".into(), grant_id: h('7', 64) }),
        ]
    }

    /// Each frame round-trips under its own tag; a newer version, a wrong
    /// shape or an oversize body is refused, and no vault frame reads as
    /// another control frame.
    #[test]
    fn vault_frames_round_trip_and_reject_wrong_version() {
        for f in all() {
            let wire = f.to_frame();
            assert_eq!(VaultFrame::from_frame(&wire).expect("round trip"), f);
            let body = &wire[wire.iter().skip(1).position(|b| *b == b'{').expect("json") + 1..];
            let tag = &wire[..wire.len() - body.len()];
            let mut json: serde_json::Value = serde_json::from_slice(body).expect("json");
            json["v"] = serde_json::json!(2);
            let mut newer = tag.to_vec();
            newer.extend_from_slice(&serde_json::to_vec(&json).expect("json"));
            assert_eq!(VaultFrame::from_frame(&newer), Err(VaultFrameError::UnknownVersion(2)));
            let mut short = tag.to_vec();
            short.extend_from_slice(br#"{"v":1}"#);
            assert_eq!(VaultFrame::from_frame(&short), Err(VaultFrameError::Malformed));
            let mut big = tag.to_vec();
            big.extend(std::iter::repeat_n(b' ', VAULT_FRAME_MAX_BYTES + 1));
            assert!(matches!(VaultFrame::from_frame(&big), Err(VaultFrameError::TooBig(_))));
            assert_eq!(
                crate::poke::Poke::from_frame(&wire),
                Err(crate::poke::PokeError::NotAPoke)
            );
        }
        let bad_id = VaultFrame::Ask(VaultAskFrame { v: VAULT_V, by: "d".into(), grant_id: h('A', 64) });
        assert_eq!(VaultFrame::from_frame(&bad_id.to_frame()), Err(VaultFrameError::Malformed));
        let bad_enc = VaultFrame::Resp(VaultRespFrame {
            v: VAULT_V,
            by: "c".into(),
            grant_id: h('5', 64),
            enc: SecretHex(h('6', 158)),
        });
        assert_eq!(VaultFrame::from_frame(&bad_enc.to_frame()), Err(VaultFrameError::Malformed));
        let nobody = VaultFrame::Ask(VaultAskFrame { v: VAULT_V, by: String::new(), grant_id: h('7', 64) });
        assert_eq!(VaultFrame::from_frame(&nobody.to_frame()), Err(VaultFrameError::Malformed));
        assert_eq!(VaultFrame::from_frame(b"\x00molt-poke-v1{}"), Err(VaultFrameError::NotThisFrame));
    }

    /// Plan 1.3.17: a share, an ikm or a sealed answer never reaches a log
    /// through `Debug`.
    #[test]
    fn vault_frame_debug_redacts_shares() {
        for f in all() {
            let dbg = format!("{f:?}");
            for secret in [h('3', 64), h('4', 64), h('6', 160)] {
                assert!(!dbg.contains(&secret), "Debug leaks a secret: {dbg}");
            }
        }
    }
}
