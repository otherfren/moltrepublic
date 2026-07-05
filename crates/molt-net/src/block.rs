// SPDX-License-Identifier: GPL-3.0-or-later

//! The uniform SMP transport block and the payload-budget math.
//!
//! SMP transports fixed-size blocks; our framing MUST always fill to block
//! size (padding is not optional — uniform sizes are one of the two
//! load-bearing unlinkability defenses, concept §6). The *usable* payload
//! budget is smaller than the block: block size minus the SMP framing
//! reserve, the per-queue wrapping AEAD overhead and the chunk header.
//! Every layer's size is a named constant here; nothing anywhere in the
//! code may assume "payload == 16 KiB".

use crate::NetError;

/// One SMP transport block (SMP fixes this at 16 KiB).
pub const SMP_BLOCK_LEN: usize = 16 * 1024;

/// Bytes reserved for SMP's own transport framing inside a block (command
/// tag, ids, correlation data). The exact split lands with `SmpTransport`
/// (T3); until then the reserve keeps every layer honest about not owning
/// the full block.
pub const SMP_FRAMING_RESERVE: usize = 64;

/// The size of the opaque payload we hand to `SEND`: one block minus the
/// SMP framing reserve. Every [`PaddedBlock`] is exactly this long.
pub const PADDED_BLOCK_LEN: usize = SMP_BLOCK_LEN - SMP_FRAMING_RESERVE;

/// One padded transport payload — always exactly [`PADDED_BLOCK_LEN`]
/// bytes. The type exists so a wrongly sized buffer cannot reach a
/// transport: the length is checked at construction, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaddedBlock(Vec<u8>);

impl PaddedBlock {
    /// Wrap a buffer that is already exactly [`PADDED_BLOCK_LEN`] long.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<PaddedBlock, NetError> {
        if bytes.len() != PADDED_BLOCK_LEN {
            return Err(NetError::Framing(format!(
                "padded block must be {PADDED_BLOCK_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        Ok(PaddedBlock(bytes))
    }

    /// The block's bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_block_enforces_its_one_size() {
        assert!(PaddedBlock::from_bytes(vec![0u8; PADDED_BLOCK_LEN]).is_ok());
        for bad in [0, 1, PADDED_BLOCK_LEN - 1, PADDED_BLOCK_LEN + 1, SMP_BLOCK_LEN] {
            assert!(PaddedBlock::from_bytes(vec![0u8; bad]).is_err(), "len {bad}");
        }
    }
}
