// SPDX-License-Identifier: GPL-3.0-or-later

//! N3 (`docs/transport/nostr_n3_plan.md` §2): the OUTER layer of a kind-445
//! group event — the wrapper that hides the MLS frames from relays.
//!
//! Shape, decided in concept §10.11 (the CURRENT Marmot form, not the older
//! derived-keypair NIP-44 one):
//!
//! ```text
//! content = base64( nonce ‖ ChaCha20Poly1305(exporter_secret, plaintext, aad = "") )
//! ```
//!
//! One sealing, keyed by the epoch's exporter secret itself. The secret
//! authenticates nothing and grants no MLS read capability — it hides frames
//! from OUTSIDERS, never from a group member (concept §7). Opening walks the
//! current secret first and then the bounded exporter ring
//! ([`crate::mls::EXPORTER_RING_K`]); past the ring an event is
//! **epoch-opaque** and must be reported loudly (G4), never silently
//! skipped.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// ChaCha20-Poly1305's nonce width — the per-event random prefix.
const NONCE_LEN: usize = 12;

/// The AEAD tag width: a frame shorter than nonce+tag cannot be a sealing.
const TAG_LEN: usize = 16;

/// What went wrong opening an outer envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// The content is not `base64(nonce ‖ ciphertext‖tag)` at all — refused
    /// on shape, before any key is tried.
    #[error("malformed 445 content: {0}")]
    Shape(String),
    /// No available secret opens it: either it belongs to an epoch older
    /// than the ring reaches, or it is not ours (relay spam, a tampered
    /// frame). Both are the same observation from here — and both must be
    /// reported, never silently dropped.
    #[error("epoch-opaque: no current or ringed exporter secret opens this event")]
    EpochOpaque,
    /// Sealing failed (the RNG, or an over-long payload).
    #[error("sealing failed: {0}")]
    Seal(String),
}

/// Seal `plaintext` under `secret` into the base64 content of a kind-445
/// event. A FRESH random nonce per call: two sealings of the same bytes
/// never share ciphertext, so a relay cannot recognize repeated content.
pub fn seal_outer(secret: &[u8; 32], plaintext: &[u8]) -> Result<String, EnvelopeError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| EnvelopeError::Seal(format!("os rng unavailable: {e}")))?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload { msg: plaintext, aad: b"" },
        )
        .map_err(|_| EnvelopeError::Seal("aead rejected the payload".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&sealed);
    Ok(encode_base64(&out))
}

/// Open a kind-445 content with the secrets available to this node, in
/// order: the CURRENT epoch's exporter secret first (the common case, one
/// AEAD attempt), then the ring, newest first.
///
/// Shape is validated before any key is tried, so a malformed frame costs
/// nothing and is distinguishable from an unopenable one.
pub fn open_outer(secrets: &[[u8; 32]], content: &str) -> Result<Vec<u8>, EnvelopeError> {
    let raw = decode_base64(content)?;
    if raw.len() < NONCE_LEN + TAG_LEN {
        return Err(EnvelopeError::Shape(format!(
            "{} bytes is shorter than a nonce plus tag",
            raw.len()
        )));
    }
    let (nonce_bytes, sealed) = raw.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    for secret in secrets {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
        if let Ok(plaintext) = cipher.decrypt(nonce, Payload { msg: sealed, aad: b"" }) {
            return Ok(plaintext);
        }
    }
    Err(EnvelopeError::EpochOpaque)
}

/// The base64 alphabet of the 445 content (standard, padded — what every
/// Nostr client encodes with).
pub fn encode_base64(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// Decode a 445 content's base64, as a shape error when it is not base64.
pub fn decode_base64(content: &str) -> Result<Vec<u8>, EnvelopeError> {
    base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|e| EnvelopeError::Shape(format!("not base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape is exactly `base64(nonce ‖ ciphertext ‖ tag)`: the
    /// overhead is 28 bytes, never a hidden header (§10.11's "33 bytes
    /// smaller than the NIP-44 form" rests on this).
    #[test]
    fn the_wire_shape_is_nonce_then_sealed_bytes() {
        let sealed = seal_outer(&[7u8; 32], b"0123456789").expect("seal");
        let raw = decode_base64(&sealed).expect("base64");
        assert_eq!(
            raw.len(),
            NONCE_LEN + 10 + TAG_LEN,
            "nonce + plaintext + tag, no framing of our own"
        );
        // an empty payload is still a valid sealing (nonce + bare tag)
        let empty = decode_base64(&seal_outer(&[7u8; 32], b"").expect("seal")).expect("b64");
        assert_eq!(empty.len(), NONCE_LEN + TAG_LEN);
    }

    /// A wrong key never opens a frame — the group-shared secret is the only
    /// thing standing between a relay and the MLS frames.
    #[test]
    fn a_foreign_secret_does_not_open_it() {
        let sealed = seal_outer(&[1u8; 32], b"ours").expect("seal");
        assert_eq!(
            open_outer(&[[2u8; 32], [3u8; 32]], &sealed),
            Err(EnvelopeError::EpochOpaque)
        );
        assert_eq!(open_outer(&[[1u8; 32]], &sealed).expect("ours opens"), b"ours");
    }
}
