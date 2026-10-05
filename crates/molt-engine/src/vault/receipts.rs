// SPDX-License-Identifier: GPL-3.0-or-later

//! Receipts, complaints, reveals and the re-seal (plan stage S3c).

use molt_core::vault::{SecretHex, VaultView};
use molt_core::{MemberId, MoltError, Reply};

impl crate::State {
    pub(crate) fn cmd_vault_reseal(&mut self, _secret_id: String) -> Result<Reply, MoltError> {
        Err(MoltError::FeatureDisabled("vault"))
    }

    pub(crate) fn cmd_net_vault_receipt(
        &mut self,
        _from: &MemberId,
        _secret_id: String,
        _verdict: String,
        _rev: u64,
    ) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_reveal(
        &mut self,
        _from: &MemberId,
        _secret_id: String,
        _holder: MemberId,
        _share: SecretHex,
        _ikm: SecretHex,
    ) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }
}

/// A deposit (pending or committed) appeared: check, send the receipt.
#[expect(dead_code, reason = "S3b calls it, S3c fills it")]
pub(crate) fn on_deposit(_st: &mut crate::State, _secret_id: &str) {}

/// Receipt counts and complaint lines.
pub(crate) fn fill(_st: &crate::State, _view: &mut VaultView) {}
