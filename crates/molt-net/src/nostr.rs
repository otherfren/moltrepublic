// SPDX-License-Identifier: GPL-3.0-or-later

//! The Nostr transport identity — the roster's **third anchor**
//! (`nostr_transport_marmot.md` §3).
//!
//! Nostr signs with secp256k1 Schnorr (BIP-340) while the roster/chain
//! identity stays Ed25519; both derive from the member's ONE recovery
//! phrase. The secp256k1 derivation is **salted with the seat's single-use
//! ticket** — the only secret both parties share at join time — so the same
//! person presents a different Nostr key in every republic (no
//! cross-republic correlation handle) and a founder can bind the presented
//! key to exactly this seat via the invite MAC v2 ([`crate::invite`]).
//! The flip side is deliberate: the ticket dies with the ritual, so the key
//! is NOT re-derivable later — it lives on in the workspace's encrypted
//! `transport.state`, beside `identity_sk`.
//!
//! The curve backend is rust-nostr's C `libsecp256k1` (ADR-0002) — the one
//! sanctioned default-build C exception, contained to this crate.

use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::NetError;

/// Derive the member's Nostr transport identity from its recovery-seed
/// entropy and the seat's single-use invite ticket. Deterministic — the
/// founding-time derivation and any re-derivation from the same material
/// agree — and domain-separated in both inputs. Returns the 32-byte
/// secp256k1 secret scalar and the x-only BIP-340 public key as 64-char
/// lowercase hex (the form anchored in the signed roster bytes).
///
/// Candidates are `SHA-256("molt-nostr-identity-v1\0" ‖ entropy ‖ 0 ‖
/// ticket ‖ 0 ‖ ctr)`; a candidate rejected by libsecp256k1 (zero, or ≥ the
/// group order n) retries with `ctr + 1`. The retry is astronomically
/// unlikely to ever run: n ≈ 2^256 − 2^128.06, so a uniform 256-bit hash
/// output is invalid with probability ≈ 2^-128 — the loop exists for
/// correctness, not because a second iteration is expected in this universe.
pub fn nostr_identity(entropy: &[u8], ticket: &str) -> ([u8; 32], String) {
    let mut ctr = 0u8;
    loop {
        let mut h = Sha256::new_with_prefix(b"molt-nostr-identity-v1\0");
        h.update(entropy);
        h.update([0u8]);
        h.update(ticket.as_bytes());
        h.update([0u8, ctr]);
        let mut candidate: [u8; 32] = h.finalize().into();
        match ::nostr::SecretKey::from_slice(&candidate) {
            Ok(sk) => {
                // rust-nostr's Keys yields the BIP-340 x-only public key
                // (even-y normalized), so one secret has ONE signed-byte form
                let pk = ::nostr::Keys::new(sk).public_key().to_hex();
                return (candidate, pk);
            }
            Err(_) => {
                candidate.zeroize();
                ctr = ctr
                    .checked_add(1)
                    .expect("secp256k1 rejected 256 successive SHA-256 outputs");
            }
        }
    }
}

/// Canonicalize a Nostr anchor arriving **from the wire** into the one form
/// that may enter the signed roster bytes, or reject it.
///
/// The anchor is the only roster field a member supplies freely: unlike
/// `identity_pk` (which the MLS KeyPackage binding forces to be the hex of a
/// real Ed25519 signature key), nothing about `nostr_pk` is implied by the
/// rest of the ritual, and the MAC only proves the *ticket holder* chose it.
/// Because the value is length-prefixed into `roster_canonical_bytes` and
/// hashed into `republic_id` **forever**, every ingest path must run it
/// through here first: exactly 32 bytes of hex, a valid BIP-340 x-only point,
/// and normalized to the single lowercase even-y form the design requires
/// (x-only keys share an x for `d` and `n−d`, so without normalization one
/// key would have two signed-byte forms).
pub fn canonical_nostr_pk(candidate: &str) -> Result<String, NetError> {
    if candidate.len() != 64 {
        return Err(NetError::Crypto(format!(
            "nostr anchor must be 64 hex chars, got {}",
            candidate.len()
        )));
    }
    let raw = hex::decode(candidate)
        .map_err(|_| NetError::Crypto("nostr anchor is not hex".to_string()))?;
    // `nostr::PublicKey::from_slice` only length-checks and copies bytes (it
    // defers curve validation), so parse through secp256k1's x-only parser —
    // the one that actually rejects an x that is not on the curve.
    let parsed = ::nostr::secp256k1::XOnlyPublicKey::from_slice(&raw)
        .map_err(|e| NetError::Crypto(format!("nostr anchor is not a valid x-only key: {e}")))?;
    // re-serialize the parsed point: an uppercase (or otherwise non-canonical)
    // presentation cannot reach the signed bytes as a second form
    Ok(hex::encode(parsed.serialize()))
}

/// The x-only BIP-340 public key (64-char lowercase hex — the anchored form)
/// of a 32-byte secp256k1 secret scalar. The check the engine runs before it
/// PERSISTS a ritual-carried `nostr_sk`: the secret is not re-derivable (the
/// salting ticket dies with the ritual), so sealing a workspace whose stored
/// secret is not the private half of its roster-anchored `nostr_pk` would
/// only surface when the transport first uses the key — with no repair path.
pub fn nostr_pk_for_sk(sk: &[u8]) -> Result<String, NetError> {
    let sk = ::nostr::SecretKey::from_slice(sk)
        .map_err(|e| NetError::Crypto(format!("not a valid secp256k1 secret scalar: {e}")))?;
    Ok(::nostr::Keys::new(sk).public_key().to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nostr_pk_for_sk_matches_the_derivation_and_rejects_bad_scalars() {
        let (sk, pk) = nostr_identity(b"seed-entropy-for-the-test", "cafe");
        assert_eq!(
            nostr_pk_for_sk(&sk).expect("a derived scalar has a public half"),
            pk,
            "the sk→pk check agrees with the one derivation"
        );
        // wrong length and out-of-range scalars are rejected, never panicked on
        assert!(nostr_pk_for_sk(&[]).is_err());
        assert!(nostr_pk_for_sk(&sk[..31]).is_err());
        assert!(nostr_pk_for_sk(&[0u8; 32]).is_err(), "zero is not a valid scalar");
        assert!(nostr_pk_for_sk(&[0xffu8; 32]).is_err(), "≥ group order is rejected");
    }

    #[test]
    fn canonical_nostr_pk_accepts_only_one_form_of_a_real_key() {
        let (_, pk) = nostr_identity(b"seed-entropy-for-the-test", "deadbeef");
        // a derived anchor is already canonical
        assert_eq!(canonical_nostr_pk(&pk).expect("derived anchors pass"), pk);
        // uppercase is normalized, never anchored as a second form
        assert_eq!(
            canonical_nostr_pk(&pk.to_uppercase()).expect("uppercase normalizes"),
            pk
        );
        // everything a hostile or pre-N1 joiner might present is rejected
        for bad in [
            "",                            // the legacy/empty marker
            &"cc".repeat(31),              // too short
            &"cc".repeat(33),              // too long
            &"zz".repeat(32),              // not hex
            &"ff".repeat(32),              // 64 hex chars, not a curve point
            &format!("{}\0{}", "bb".repeat(32), "33".repeat(32)), // splice attempt
        ] {
            assert!(
                canonical_nostr_pk(bad).is_err(),
                "must reject {:?}",
                &bad[..bad.len().min(20)]
            );
        }
    }

    #[test]
    fn the_domain_separators_hold() {
        // entropy/ticket boundary games must not collide: moving a byte
        // across the separator changes the derived key
        let (_, a) = nostr_identity(b"abcd", "ef");
        let (_, b) = nostr_identity(b"abc", "def");
        assert_ne!(a, b, "the 0-separator splits entropy from ticket");
    }
}
