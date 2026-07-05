// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-queue wrapping (concept §3.2, **mandatory**).
//!
//! A group message is one ciphertext fanned out to n−1 queues — without
//! wrapping, all copies are byte-identical and any server hosting two
//! members' queues links them into a group at a glance. Every chunk is
//! therefore encrypted `XChaCha20-Poly1305(key_q, random nonce, chunk)`
//! under a **fresh symmetric key per queue** before it becomes a block.
//! The purpose is *copy unlinkability*, not confidentiality (MLS owns
//! that, from T2); the padding lives *inside* this layer — the plaintext
//! is always exactly [`CHUNK_PLAIN_LEN`], so the wrapped output is always
//! exactly one padded block and the server never sees a length.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::block::{PaddedBlock, PADDED_BLOCK_LEN};
use crate::NetError;

/// XChaCha20 nonce length.
const WRAP_NONCE_LEN: usize = 24;
/// Poly1305 tag length.
const WRAP_TAG_LEN: usize = 16;

/// The chunk plaintext size: one padded block minus nonce and AEAD tag.
/// The chunker always fills to exactly this length (zero padding inside
/// the encryption), so every wrapped block is exactly
/// [`PADDED_BLOCK_LEN`].
pub const CHUNK_PLAIN_LEN: usize = PADDED_BLOCK_LEN - WRAP_NONCE_LEN - WRAP_TAG_LEN;

/// A per-queue wrapping key. Fresh at queue creation, rotates with its
/// queue, lives in `transport.state` (concept §6) — never in the shared
/// log.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct WrapKey([u8; 32]);

impl WrapKey {
    /// A fresh key from the OS CSPRNG.
    pub fn fresh() -> Result<WrapKey, NetError> {
        let mut k = [0u8; 32];
        getrandom::getrandom(&mut k)
            .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
        Ok(WrapKey(k))
    }

    /// Wrap an existing key (in-band key handover, tests).
    pub fn from_bytes(k: [u8; 32]) -> WrapKey {
        WrapKey(k)
    }
}

impl std::fmt::Debug for WrapKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WrapKey(…)") // never print key material
    }
}

/// Wrap one chunk plaintext (exactly [`CHUNK_PLAIN_LEN`] bytes) into one
/// padded block: `nonce || ciphertext+tag`. A fresh random nonce per call
/// makes two wraps of the same chunk byte-distinct.
pub fn wrap(key: &WrapKey, chunk_plain: &[u8]) -> Result<PaddedBlock, NetError> {
    if chunk_plain.len() != CHUNK_PLAIN_LEN {
        return Err(NetError::Framing(format!(
            "chunk plaintext must be {CHUNK_PLAIN_LEN} bytes, got {}",
            chunk_plain.len()
        )));
    }
    let mut nonce = [0u8; WRAP_NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: chunk_plain,
                aad: &[],
            },
        )
        .map_err(|_| NetError::Crypto("wrapping failed".to_string()))?;
    let mut out = Vec::with_capacity(PADDED_BLOCK_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    PaddedBlock::from_bytes(out)
}

/// Unwrap one padded block back into its chunk plaintext. Fails on a
/// tampered block or the wrong queue key.
pub fn unwrap_block(key: &WrapKey, block: &PaddedBlock) -> Result<Vec<u8>, NetError> {
    let bytes = block.as_slice();
    let (nonce, ct) = bytes.split_at(WRAP_NONCE_LEN);
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &[] })
        .map_err(|_| NetError::Crypto("unwrap failed (tampered block or wrong queue key)".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_roundtrips_and_fills_exactly_one_block() {
        let key = WrapKey::fresh().expect("rng");
        let chunk = vec![7u8; CHUNK_PLAIN_LEN];
        let block = wrap(&key, &chunk).expect("wrap");
        assert_eq!(block.as_slice().len(), PADDED_BLOCK_LEN);
        assert_eq!(unwrap_block(&key, &block).expect("unwrap"), chunk);
    }

    /// The load-bearing property: two wraps of the SAME chunk are
    /// byte-distinct (fresh nonce), so fan-out copies never match.
    #[test]
    fn same_chunk_wraps_to_distinct_blocks() {
        let key = WrapKey::fresh().expect("rng");
        let chunk = vec![7u8; CHUNK_PLAIN_LEN];
        let a = wrap(&key, &chunk).expect("wrap a");
        let b = wrap(&key, &chunk).expect("wrap b");
        assert_ne!(a.as_slice(), b.as_slice());
    }

    #[test]
    fn wrong_key_and_tampering_are_rejected() {
        let key = WrapKey::fresh().expect("rng");
        let chunk = vec![1u8; CHUNK_PLAIN_LEN];
        let block = wrap(&key, &chunk).expect("wrap");
        let other = WrapKey::fresh().expect("rng");
        assert!(unwrap_block(&other, &block).is_err());
        let mut bytes = block.as_slice().to_vec();
        bytes[100] ^= 1;
        let tampered = PaddedBlock::from_bytes(bytes).expect("size unchanged");
        assert!(unwrap_block(&key, &tampered).is_err());
    }

    #[test]
    fn oversized_and_undersized_chunks_are_refused() {
        let key = WrapKey::fresh().expect("rng");
        assert!(wrap(&key, &vec![0u8; CHUNK_PLAIN_LEN - 1]).is_err());
        assert!(wrap(&key, &vec![0u8; CHUNK_PLAIN_LEN + 1]).is_err());
    }
}
