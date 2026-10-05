// SPDX-License-Identifier: GPL-3.0-or-later

//! Deposit: seal, the verify-gated approve, apply and the projection
//! (plan stage S3b).

use molt_core::vault::{SecretText, VaultView};
use molt_core::{MoltError, Reply};
use serde_json::Value;

impl crate::State {
    pub(crate) fn cmd_vault_seal(
        &mut self,
        _name: String,
        _kind: String,
        _text: SecretText,
    ) -> Result<Reply, MoltError> {
        Err(MoltError::FeatureDisabled("vault"))
    }

    pub(crate) fn approve_check_deposit(&self, _payload: &Value) -> Result<(), MoltError> {
        Ok(())
    }

    /// Side effects of an applied Vault block (idempotent, keyed by id).
    #[expect(dead_code, reason = "S3b wires it into every apply site")]
    pub(crate) fn after_vault_applied(&mut self, _payload: &Value) {}
}

/// Deposit cards.
pub(crate) fn fill(_st: &crate::State, _view: &mut VaultView) {}
