// SPDX-License-Identifier: GPL-3.0-or-later

//! **The vault** (`docs/vault/vault_threshold_disclosure.md`, built per
//! `docs/vault/vault_build_plan.md`). Each submodule belongs to one build
//! stage; this file only wires them.

use molt_core::vault::{VaultCtx, VaultRefusal, VaultView};
use molt_core::MoltError;
use serde_json::Value;

pub(crate) mod deposit;
pub(crate) mod fold;
pub(crate) mod grant;
pub(crate) mod read;
pub(crate) mod receipts;

/// The approve arm of a Vault op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VaultArm {
    Deposit,
    Grant,
    /// The v5 mock's generic ops.
    Mock,
}

/// Route a Vault payload; a real vault is a closed op set (plan 1.3.9).
pub(crate) fn vault_arm(payload: &Value, real: bool) -> Result<VaultArm, MoltError> {
    match payload.get("op").and_then(Value::as_str) {
        Some("deposit") => Ok(VaultArm::Deposit),
        Some("grant") => Ok(VaultArm::Grant),
        _ if real => Err(MoltError::Vault(VaultRefusal::UnknownOp)),
        _ => Ok(VaultArm::Mock),
    }
}

impl crate::State {
    /// The adopted chain's vault context; `None` outside a vault republic.
    /// For commands and the view only - a walk takes the context of the
    /// chain it walks.
    #[expect(dead_code, reason = "S2 fills it, S3b reads it")]
    pub(crate) fn vault_ctx(&self) -> Option<VaultCtx> {
        None
    }

    /// The adopted chain's genesis is roster-v6.
    pub(crate) fn is_vault_republic(&self) -> bool {
        false
    }

    /// The republic folded its vault into a base this node does not hold.
    #[expect(dead_code, reason = "S3b guards with it, S5 fills it")]
    pub(crate) fn vault_base_pending(&self) -> bool {
        false
    }

    /// The vault surface's read model.
    pub(crate) fn vault_view(&self) -> VaultView {
        let mut view = VaultView {
            real: self.is_vault_republic(),
            m: self.replica.as_ref().map_or(0, |r| r.rule_m),
            n: self
                .replica
                .as_ref()
                .map_or(0, |r| u8::try_from(r.roster.len()).unwrap_or(u8::MAX)),
            ..VaultView::default()
        };
        deposit::fill(self, &mut view);
        receipts::fill(self, &mut view);
        grant::fill(self, &mut view);
        view
    }

    /// The vault arm of `approve`, by op.
    #[expect(dead_code, reason = "S3b wires it into cmd_approve")]
    pub(crate) fn vault_approve_check(&self, payload: &Value) -> Result<(), MoltError> {
        match vault_arm(payload, self.is_vault_republic())? {
            VaultArm::Deposit => self.approve_check_deposit(payload),
            VaultArm::Grant => self.approve_check_grant(payload),
            VaultArm::Mock => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Plan 1.3.9: a real vault takes deposit and grant only; the v5 mock
    /// keeps its generic ops.
    #[test]
    fn approve_takes_the_closed_op_set_in_a_real_vault() {
        for real in [false, true] {
            assert_eq!(vault_arm(&json!({"op": "deposit"}), real).expect("deposit"), VaultArm::Deposit);
            assert_eq!(vault_arm(&json!({"op": "grant"}), real).expect("grant"), VaultArm::Grant);
        }
        for other in [json!({"op": "seal_secret"}), json!({"op": "vault_base"}), json!({"op": "x"}), json!({})] {
            assert_eq!(vault_arm(&other, false).expect("mock"), VaultArm::Mock);
            assert!(matches!(
                vault_arm(&other, true),
                Err(MoltError::Vault(VaultRefusal::UnknownOp))
            ));
        }
    }
}
