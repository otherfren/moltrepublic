// SPDX-License-Identifier: GPL-3.0-or-later

//! Ed448 signature verification (RFC 8032 §5.2), pure Rust.
//!
//! The official SimpleX servers present **Ed448** TLS certificates, and
//! `rustls-rustcrypto` (the pure-Rust provider) does not verify Ed448. So
//! we implement the verify equation ourselves over the audited
//! `ed448-goldilocks` curve arithmetic + `sha3` SHAKE256, and prove it
//! correct against the RFC 8032 known-answer vectors (see the tests). Only
//! verification — never signing — is implemented; we are a client.
//!
//! This keeps the no-C-toolchain / reproducible-build posture while
//! supporting every SimpleX server (Ed25519 *and* Ed448).

use ed448_goldilocks::curve::edwards::{CompressedEdwardsY, ExtendedPoint};
use ed448_goldilocks::Scalar;
use sha3::digest::{ExtendableOutput, Update, XofReader};
use sha3::Shake256;

/// Public key / point-encoding length.
const POINT_LEN: usize = 57;
/// Signature length: `R (57) ‖ S (57)`.
const SIG_LEN: usize = 114;

/// Verify a pure-Ed448 signature (empty context) of `message` under the
/// 57-byte public key. Returns `false` on any malformed input — never
/// panics, never accepts on error.
pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let (Ok(a_bytes), Ok(sig)) = (
        <[u8; POINT_LEN]>::try_from(public_key),
        <[u8; SIG_LEN]>::try_from(signature),
    ) else {
        return false;
    };

    // A: decode the public-key point
    let Some(a_point) = CompressedEdwardsY(a_bytes).decompress() else {
        return false;
    };
    // R: first 57 bytes of the signature (kept both as point and bytes)
    let mut r_bytes = [0u8; POINT_LEN];
    r_bytes.copy_from_slice(&sig[..POINT_LEN]);
    let Some(r_point) = CompressedEdwardsY(r_bytes).decompress() else {
        return false;
    };
    // S: last 57 bytes, little-endian scalar; the top byte MUST be zero
    if sig[SIG_LEN - 1] != 0 {
        return false;
    }
    let mut s_bytes = [0u8; 56];
    s_bytes.copy_from_slice(&sig[POINT_LEN..POINT_LEN + 56]);
    let s_scalar = Scalar::from_bytes(s_bytes);

    // k = SHAKE256(dom4 ‖ R ‖ A ‖ M) mod L, 114-byte XOF output.
    // dom4 for pure Ed448 = "SigEd448" ‖ phflag(0) ‖ contextLen(0)
    let mut h = Shake256::default();
    h.update(b"SigEd448");
    h.update(&[0u8, 0u8]);
    h.update(&r_bytes);
    h.update(&a_bytes);
    h.update(message);
    let mut reader = h.finalize_xof();
    let mut k_wide = [0u8; 114];
    reader.read(&mut k_wide);
    let k = Scalar::from_bytes_mod_order_wide(&k_wide);

    // group equation: [S]B == R + [k]A
    let lhs = ExtendedPoint::generator().scalar_mul(&s_scalar);
    let rhs = r_point.add(&a_point.scalar_mul(&k));
    lhs.compress().0 == rhs.compress().0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("hex")
    }

    /// RFC 8032 §7.4, first Ed448 vector ("-----" / empty message).
    #[test]
    fn rfc8032_ed448_empty_message() {
        let pk = unhex(
            "5fd7449b59b461fd2ce787ec616ad46a1da1342485a70e1f8a0ea75d80e967\
             78edf124769b46c7061bd6783df1e50f6cd1fa1abeafe8256180",
        );
        let sig = unhex(
            "533a37f6bbe457251f023c0d88f976ae2dfb504a843e34d2074fd823d41a59\
             1f2b233f034f628281f2fd7a22ddd47d7828c59bd0a21bfd3980ff0d2028d4\
             b18a9df63e006c5d1c2d345b925d8dc00b4104852db99ac5c7cdda8530a113\
             a0f4dbb61149f05a7363268c71d95808ff2e652600",
        );
        assert!(verify(&pk, b"", &sig), "official RFC 8032 Ed448 vector must verify");
        // negative: a flipped signature bit must NOT verify
        let mut bad = sig.clone();
        bad[0] ^= 0x01;
        assert!(!verify(&pk, b"", &bad), "tampered signature must fail");
        // negative: a different message must NOT verify
        assert!(!verify(&pk, b"x", &sig), "wrong message must fail");
        // malformed lengths never panic
        assert!(!verify(&pk[..10], b"", &sig));
        assert!(!verify(&pk, b"", &sig[..100]));
    }

    /// RFC 8032 §7.4, the 1-byte-message ("03") Ed448 vector.
    #[test]
    fn rfc8032_ed448_one_byte_message() {
        let pk = unhex(
            "43ba28f430cdff456ae531545f7ecd0ac834a55d9358c0372bfa0c6c6798c0\
             866aea01eb00742802b8438ea4cb82169c235160627b4c3a9480",
        );
        let msg = unhex("03");
        let sig = unhex(
            "26b8f91727bd62897af15e41eb43c377efb9c610d48f2335cb0bd0087810f4\
             352541b143c4b981b7e18f62de8ccdf633fc1bf037ab7cd779805e0dbcc0aa\
             e1cbcee1afb2e027df36bc04dcecbf154336c19f0af7e0a6472905e799f195\
             3d2a0ff3348ab21aa4adafd1d234441cf807c03a00",
        );
        assert!(verify(&pk, &msg, &sig), "RFC 8032 Ed448 1-byte vector must verify");
    }
}
