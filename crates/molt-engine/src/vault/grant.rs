// SPDX-License-Identifier: GPL-3.0-or-later

//! Grant: propose, approve check, answer on commit and on request, the
//! audit (plan stage S4).

use molt_core::vault::{SecretHex, VaultView};
use molt_core::{MemberId, MoltError, Reply};
use serde_json::Value;

impl crate::State {
    pub(crate) fn cmd_vault_grant(
        &mut self,
        _secret_id: String,
        _reader: MemberId,
    ) -> Result<Reply, MoltError> {
        Err(MoltError::FeatureDisabled("vault"))
    }

    pub(crate) fn cmd_net_vault_resp(
        &mut self,
        _from: &MemberId,
        _grant_id: String,
        _enc: SecretHex,
    ) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_ask(
        &mut self,
        _from: &MemberId,
        _grant_id: String,
    ) -> Result<Reply, MoltError> {
        Ok(Reply::Ack)
    }

    pub(crate) fn approve_check_grant(&self, _payload: &Value) -> Result<(), MoltError> {
        Ok(())
    }
}

/// A grant committed: answer it if this seat holds a share.
pub(crate) fn on_commit(_st: &mut crate::State, _grant_id: &str) {}

/// A reorg displaced an applied grant (plan 1.4).
pub(crate) fn on_displaced(_st: &mut crate::State, _grant_id: &str) {}

/// Grant cards.
pub(crate) fn fill(_st: &crate::State, _view: &mut VaultView) {}
