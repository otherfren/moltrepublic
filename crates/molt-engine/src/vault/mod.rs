// SPDX-License-Identifier: GPL-3.0-or-later

//! **The vault** (`docs/vault/vault_threshold_disclosure.md`, built per
//! `docs/vault/vault_build_plan.md`). Each submodule belongs to one build
//! stage; this file only wires them.

use molt_core::vault::VaultView;
use molt_core::{MemberId, MoltError};
use serde_json::Value;

pub(crate) mod deposit;
pub(crate) mod fold;
pub(crate) mod grant;
pub(crate) mod read;
pub(crate) mod receipts;

/// The vault context of ONE chain, built from its genesis roster or its
/// anchor's `founding_identities` - never from node state (plan 1.3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaultCtx {
    /// The threshold.
    pub(crate) m: u8,
    /// `(name, identity_pk, vault_pk)` in genesis founding-table order;
    /// a seat's Shamir x is its 1-based position here.
    pub(crate) holders_in_genesis_order: Vec<(MemberId, String, String)>,
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
        match payload.get("op").and_then(Value::as_str) {
            Some("deposit") => self.approve_check_deposit(payload),
            Some("grant") => self.approve_check_grant(payload),
            _ => Ok(()),
        }
    }
}
