// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse run's control frames (`docs/chain/wallet_treasury_design.md`
//! §3.3-§3.5, plan §7.4): start, daemon hint, readiness, round 1, round 2,
//! attestation, abort. Ephemeral and MLS-authenticated: the seat is the
//! MLS sender, never a frame field. Each tag is its own version boundary.

use std::collections::BTreeMap;

use molt_core::vault::SecretHex;
use serde::{Deserialize, Serialize};

/// Start of a run.
pub const WALLET_START_TAG: &[u8] = b"\x00molt-wstart-v1";
/// A daemon URL offered while a purse stage is open.
pub const WALLET_HINT_TAG: &[u8] = b"\x00molt-wdhint-v1";
/// Readiness and consent, or a decline.
pub const WALLET_READY_TAG: &[u8] = b"\x00molt-wrdy-v1";
/// Round 1: view contribution and commitments.
pub const WALLET_R1_TAG: &[u8] = b"\x00molt-wr1-v1";
/// Round 2: the shares and the transcript hash.
pub const WALLET_R2_TAG: &[u8] = b"\x00molt-wr2-v1";
/// A seat's attestation.
pub const WALLET_ATTEST_TAG: &[u8] = b"\x00molt-watt-v1";
/// An abort with its reason.
pub const WALLET_ABORT_TAG: &[u8] = b"\x00molt-wabrt-v1";

/// The seven tags, for the disjointness checks.
pub const WALLET_TAGS: [&[u8]; 7] = [
    WALLET_START_TAG,
    WALLET_HINT_TAG,
    WALLET_READY_TAG,
    WALLET_R1_TAG,
    WALLET_R2_TAG,
    WALLET_ATTEST_TAG,
    WALLET_ABORT_TAG,
];

/// The wire version this build writes and accepts.
pub const WALLET_V: u32 = 1;

/// Refused unparsed above this; round 2 of a 200-seat run fits.
pub const WALLET_FRAME_MAX_BYTES: usize = 64 * 1024;

const HEX32: usize = 64;
const HEX_SIG: usize = 128;
/// One round-2 share: key, PoP nonce, PoP scalar, encrypted share.
const HEX_SHARE: usize = 256;
const URL_MAX: usize = 512;
const REASON_MAX: usize = 32;

/// Why a plaintext is not a usable wallet frame.
#[derive(Debug, PartialEq, Eq)]
pub enum WalletFrameError {
    /// None of the seven tags.
    NotThisFrame,
    /// Beyond [`WALLET_FRAME_MAX_BYTES`].
    TooBig(usize),
    /// Tagged, but not the frame.
    Malformed,
    /// A version this build does not read.
    UnknownVersion(u32),
}

impl std::fmt::Display for WalletFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletFrameError::NotThisFrame => write!(f, "not a wallet frame"),
            WalletFrameError::TooBig(n) => write!(f, "wallet frame is {n} bytes"),
            WalletFrameError::Malformed => write!(f, "malformed wallet frame"),
            WalletFrameError::UnknownVersion(v) => write!(f, "wallet frame version {v}"),
        }
    }
}

/// Start run `run` of init `init`; `starter` is the sender's founding position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletStartFrame {
    /// Wire version.
    pub v: u32,
    /// The applied `wallet_init` proposal id.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// The starter's founding position (1-based).
    pub starter: u16,
}

/// The sender's daemon URL, offered for the stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletHintFrame {
    /// Wire version.
    pub v: u32,
    /// The init the stage is for.
    pub init: u64,
    /// The daemon URL.
    pub url: String,
}

/// Ready (build, daemon, consent) or, with `ok = false`, a decline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletReadyFrame {
    /// Wire version.
    pub v: u32,
    /// The init.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// `false` declines (W6).
    pub ok: bool,
}

/// Round 1: `c_i ‖ commitments`, hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletRound1Frame {
    /// Wire version.
    pub v: u32,
    /// The init.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// The round-1 message, hex.
    pub msg: SecretHex,
}

/// Round 2: one encrypted share per recipient position, and `T`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletRound2Frame {
    /// Wire version.
    pub v: u32,
    /// The init.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// Recipient position -> share, hex.
    pub shares: BTreeMap<u16, SecretHex>,
    /// The sender's transcript hash, hex.
    pub transcript: String,
}

/// The sender's attestation of the run's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletAttestFrame {
    /// Wire version.
    pub v: u32,
    /// The init.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// The Ed25519 signature, hex.
    pub sig: String,
}

/// The run ended; `reason` is one word, never a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletAbortFrame {
    /// Wire version.
    pub v: u32,
    /// The init.
    pub init: u64,
    /// The run nonce, hex.
    pub run: String,
    /// Why.
    pub reason: String,
}

/// One wallet control frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletFrame {
    /// A start.
    Start(WalletStartFrame),
    /// A daemon hint.
    Hint(WalletHintFrame),
    /// Readiness or a decline.
    Ready(WalletReadyFrame),
    /// Round 1.
    Round1(WalletRound1Frame),
    /// Round 2.
    Round2(WalletRound2Frame),
    /// An attestation.
    Attest(WalletAttestFrame),
    /// An abort.
    Abort(WalletAbortFrame),
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && is_hex_any(s)
}

fn is_hex_any(s: &str) -> bool {
    s.len() % 2 == 0 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn framed<T: Serialize>(tag: &[u8], value: &T) -> Vec<u8> {
    let mut out = tag.to_vec();
    if let Ok(json) = serde_json::to_vec(value) {
        out.extend_from_slice(&json);
    }
    out
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, WalletFrameError> {
    #[derive(Deserialize)]
    struct V {
        v: u32,
    }
    let v: V = serde_json::from_slice(body).map_err(|_| WalletFrameError::Malformed)?;
    if v.v != WALLET_V {
        return Err(WalletFrameError::UnknownVersion(v.v));
    }
    serde_json::from_slice(body).map_err(|_| WalletFrameError::Malformed)
}

impl WalletFrame {
    /// The init this frame belongs to.
    #[must_use]
    pub fn init(&self) -> u64 {
        match self {
            WalletFrame::Start(f) => f.init,
            WalletFrame::Hint(f) => f.init,
            WalletFrame::Ready(f) => f.init,
            WalletFrame::Round1(f) => f.init,
            WalletFrame::Round2(f) => f.init,
            WalletFrame::Attest(f) => f.init,
            WalletFrame::Abort(f) => f.init,
        }
    }

    /// The run nonce, hex; `None` for a hint.
    #[must_use]
    pub fn run(&self) -> Option<&str> {
        match self {
            WalletFrame::Start(f) => Some(&f.run),
            WalletFrame::Hint(_) => None,
            WalletFrame::Ready(f) => Some(&f.run),
            WalletFrame::Round1(f) => Some(&f.run),
            WalletFrame::Round2(f) => Some(&f.run),
            WalletFrame::Attest(f) => Some(&f.run),
            WalletFrame::Abort(f) => Some(&f.run),
        }
    }

    /// TAG ‖ JSON.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        match self {
            WalletFrame::Start(f) => framed(WALLET_START_TAG, f),
            WalletFrame::Hint(f) => framed(WALLET_HINT_TAG, f),
            WalletFrame::Ready(f) => framed(WALLET_READY_TAG, f),
            WalletFrame::Round1(f) => framed(WALLET_R1_TAG, f),
            WalletFrame::Round2(f) => framed(WALLET_R2_TAG, f),
            WalletFrame::Attest(f) => framed(WALLET_ATTEST_TAG, f),
            WalletFrame::Abort(f) => framed(WALLET_ABORT_TAG, f),
        }
    }

    /// The ONLY consumer: tag, size cap, version, shape.
    pub fn from_frame(plaintext: &[u8]) -> Result<WalletFrame, WalletFrameError> {
        let (tag, body) = WALLET_TAGS
            .iter()
            .find_map(|t| plaintext.strip_prefix(*t).map(|b| (*t, b)))
            .ok_or(WalletFrameError::NotThisFrame)?;
        if body.len() > WALLET_FRAME_MAX_BYTES {
            return Err(WalletFrameError::TooBig(body.len()));
        }
        let frame = if tag == WALLET_START_TAG {
            let f: WalletStartFrame = parse(body)?;
            (is_hex(&f.run, HEX32) && f.starter > 0).then_some(WalletFrame::Start(f))
        } else if tag == WALLET_HINT_TAG {
            let f: WalletHintFrame = parse(body)?;
            (!f.url.is_empty() && f.url.len() <= URL_MAX && !f.url.chars().any(char::is_control))
                .then_some(WalletFrame::Hint(f))
        } else if tag == WALLET_READY_TAG {
            let f: WalletReadyFrame = parse(body)?;
            is_hex(&f.run, HEX32).then_some(WalletFrame::Ready(f))
        } else if tag == WALLET_R1_TAG {
            let f: WalletRound1Frame = parse(body)?;
            (is_hex(&f.run, HEX32) && !f.msg.0.is_empty() && is_hex_any(&f.msg.0))
                .then_some(WalletFrame::Round1(f))
        } else if tag == WALLET_R2_TAG {
            let f: WalletRound2Frame = parse(body)?;
            (is_hex(&f.run, HEX32)
                && is_hex(&f.transcript, HEX32)
                && !f.shares.is_empty()
                && f.shares.iter().all(|(to, s)| *to > 0 && is_hex(&s.0, HEX_SHARE)))
            .then_some(WalletFrame::Round2(f))
        } else if tag == WALLET_ATTEST_TAG {
            let f: WalletAttestFrame = parse(body)?;
            (is_hex(&f.run, HEX32) && is_hex(&f.sig, HEX_SIG)).then_some(WalletFrame::Attest(f))
        } else {
            let f: WalletAbortFrame = parse(body)?;
            (is_hex(&f.run, HEX32)
                && !f.reason.is_empty()
                && f.reason.len() <= REASON_MAX
                && f.reason.bytes().all(|b| b.is_ascii_lowercase() || b == b' '))
            .then_some(WalletFrame::Abort(f))
        };
        frame.ok_or(WalletFrameError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char, n: usize) -> String {
        std::iter::repeat_n(c, n).collect()
    }

    fn all() -> Vec<WalletFrame> {
        let run = h('1', 64);
        vec![
            WalletFrame::Start(WalletStartFrame { v: WALLET_V, init: 7, run: run.clone(), starter: 1 }),
            WalletFrame::Hint(WalletHintFrame { v: WALLET_V, init: 7, url: "http://x.onion".into() }),
            WalletFrame::Ready(WalletReadyFrame { v: WALLET_V, init: 7, run: run.clone(), ok: false }),
            WalletFrame::Round1(WalletRound1Frame { v: WALLET_V, init: 7, run: run.clone(), msg: SecretHex(h('2', 320)) }),
            WalletFrame::Round2(WalletRound2Frame {
                v: WALLET_V,
                init: 7,
                run: run.clone(),
                shares: BTreeMap::from([(2, SecretHex(h('3', 256))), (3, SecretHex(h('4', 256)))]),
                transcript: h('5', 64),
            }),
            WalletFrame::Attest(WalletAttestFrame { v: WALLET_V, init: 7, run: run.clone(), sig: h('6', 128) }),
            WalletFrame::Abort(WalletAbortFrame { v: WALLET_V, init: 7, run, reason: "not ready".into() }),
        ]
    }

    /// Each frame round-trips under its own tag; a newer version, a wrong
    /// shape or an oversize body is refused, and no wallet frame reads as
    /// a vault frame.
    #[test]
    fn wallet_frames_round_trip_and_reject_wrong_version() {
        for f in all() {
            let wire = f.to_frame();
            assert_eq!(WalletFrame::from_frame(&wire).expect("round trip"), f);
            assert_eq!(f.init(), 7);
            let body = &wire[wire.iter().skip(1).position(|b| *b == b'{').expect("json") + 1..];
            let tag = &wire[..wire.len() - body.len()];
            let mut json: serde_json::Value = serde_json::from_slice(body).expect("json");
            json["v"] = serde_json::json!(2);
            let mut newer = tag.to_vec();
            newer.extend_from_slice(&serde_json::to_vec(&json).expect("json"));
            assert_eq!(WalletFrame::from_frame(&newer), Err(WalletFrameError::UnknownVersion(2)));
            let mut short = tag.to_vec();
            short.extend_from_slice(br#"{"v":1}"#);
            assert_eq!(WalletFrame::from_frame(&short), Err(WalletFrameError::Malformed));
            let mut big = tag.to_vec();
            big.extend(std::iter::repeat_n(b' ', WALLET_FRAME_MAX_BYTES + 1));
            assert!(matches!(WalletFrame::from_frame(&big), Err(WalletFrameError::TooBig(_))));
            assert_eq!(
                crate::vault_frames::VaultFrame::from_frame(&wire),
                Err(crate::vault_frames::VaultFrameError::NotThisFrame)
            );
        }
        let bad_run = WalletFrame::Ready(WalletReadyFrame { v: WALLET_V, init: 1, run: h('A', 64), ok: true });
        assert_eq!(WalletFrame::from_frame(&bad_run.to_frame()), Err(WalletFrameError::Malformed));
        let bad_share = WalletFrame::Round2(WalletRound2Frame {
            v: WALLET_V,
            init: 1,
            run: h('1', 64),
            shares: BTreeMap::from([(0, SecretHex(h('3', 256)))]),
            transcript: h('5', 64),
        });
        assert_eq!(WalletFrame::from_frame(&bad_share.to_frame()), Err(WalletFrameError::Malformed));
        let named = WalletFrame::Abort(WalletAbortFrame { v: WALLET_V, init: 1, run: h('1', 64), reason: "Bob".into() });
        assert_eq!(WalletFrame::from_frame(&named.to_frame()), Err(WalletFrameError::Malformed));
    }

    /// The wire bytes of a start are pinned.
    #[test]
    fn a_start_frame_is_byte_pinned() {
        let f = WalletFrame::Start(WalletStartFrame { v: 1, init: 9, run: h('a', 64), starter: 2 });
        let mut want = b"\x00molt-wstart-v1".to_vec();
        want.extend_from_slice(format!(r#"{{"v":1,"init":9,"run":"{}","starter":2}}"#, h('a', 64)).as_bytes());
        assert_eq!(f.to_frame(), want);
    }

    /// A share or a view contribution never reaches a log through `Debug`.
    #[test]
    fn wallet_frame_debug_redacts_shares() {
        for f in all() {
            let dbg = format!("{f:?}");
            for secret in [h('2', 320), h('3', 256), h('4', 256)] {
                assert!(!dbg.contains(&secret), "Debug leaks a secret: {dbg}");
            }
        }
    }
}
