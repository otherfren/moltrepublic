// SPDX-License-Identifier: GPL-3.0-or-later
//! The one-shot RNG that turns HPKE's random ephemeral into a derived one.

use hpke::rand_core::{CryptoRng, RngCore};
use zeroize::Zeroize;

/// Yields its 32 bytes exactly once, as one 32-byte draw, and panics on
/// anything else. `hpke` 0.13's `encap` draws one private-key-sized
/// `ikm` and runs `DeriveKeyPair(ikm)` (RFC 9180), so sealing through
/// this RNG gives the deterministic ephemeral complaints rest on; an
/// `hpke` that drew differently panics instead of silently going random.
pub struct IkmRng {
    ikm: Option<[u8; 32]>,
}

impl IkmRng {
    /// An RNG that hands out `ikm` once.
    #[must_use]
    pub fn new(ikm: [u8; 32]) -> Self {
        Self { ikm: Some(ikm) }
    }
}

impl Drop for IkmRng {
    fn drop(&mut self) {
        if let Some(ikm) = self.ikm.as_mut() {
            ikm.zeroize();
        }
    }
}

impl RngCore for IkmRng {
    fn next_u32(&mut self) -> u32 {
        panic!("IkmRng: only one 32-byte draw");
    }

    fn next_u64(&mut self) -> u64 {
        panic!("IkmRng: only one 32-byte draw");
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        match self.ikm.take() {
            Some(mut ikm) if dst.len() == ikm.len() => {
                dst.copy_from_slice(&ikm);
                ikm.zeroize();
            }
            _ => panic!("IkmRng: only one 32-byte draw"),
        }
    }
}

impl CryptoRng for IkmRng {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_yields_its_ikm_once() {
        let mut rng = IkmRng::new([7; 32]);
        let mut out = [0u8; 32];
        rng.fill_bytes(&mut out);
        assert_eq!(out, [7; 32]);
    }

    #[test]
    #[should_panic(expected = "only one 32-byte draw")]
    fn a_second_draw_panics() {
        let mut rng = IkmRng::new([7; 32]);
        let mut out = [0u8; 32];
        rng.fill_bytes(&mut out);
        rng.fill_bytes(&mut out);
    }

    #[test]
    #[should_panic(expected = "only one 32-byte draw")]
    fn a_draw_of_another_size_panics() {
        let mut rng = IkmRng::new([7; 32]);
        let mut out = [0u8; 16];
        rng.fill_bytes(&mut out);
    }
}
