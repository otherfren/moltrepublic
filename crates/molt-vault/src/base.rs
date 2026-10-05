// SPDX-License-Identifier: GPL-3.0-or-later
//! Re-verifying a folded base before adoption (plan 1.3.16).

use std::collections::BTreeSet;

use molt_core::vault::{grant_id, secret_id, VaultBase, VaultCtx};

use crate::deposit::{verify_deposit_shape, verify_deposit_sig};
use crate::VaultError;

/// Every deposit's shape and signature under the base's own context, one
/// deposit per `(depositor, name)`, every grant on its deposit's version
/// with a recomputing `grant_id` and a seat as reader, no grant twice.
///
/// # Errors
/// [`VaultError::Base`] or the deposit check that failed.
pub fn verify_base(base: &VaultBase, republic_id: &str, ctx: &VaultCtx) -> Result<(), VaultError> {
    let mut slots = BTreeSet::new();
    let mut grant_ids = BTreeSet::new();
    for entry in &base.deposits {
        let dep = &entry.deposit;
        if !slots.insert((dep.depositor.as_str(), dep.name.as_str())) {
            return Err(VaultError::Base("a slot twice".to_string()));
        }
        verify_deposit_shape(dep, ctx)?;
        let identity_pk = ctx
            .holders_in_genesis_order
            .iter()
            .find(|(n, _, _)| *n == dep.depositor)
            .map(|(_, pk, _)| pk.as_str())
            .ok_or_else(|| VaultError::NotASeat(dep.depositor.clone()))?;
        verify_deposit_sig(dep, republic_id, identity_pk)?;
        let sid = secret_id(republic_id, dep);
        for g in &entry.grants {
            if g.grant.secret_id != sid {
                return Err(VaultError::Base("a grant on another version".to_string()));
            }
            if !ctx
                .holders_in_genesis_order
                .iter()
                .any(|(n, _, _)| *n == g.grant.reader)
            {
                return Err(VaultError::NotASeat(g.grant.reader.clone()));
            }
            if grant_id(&sid, &g.grant.reader, g.proposal_id) != g.grant.grant_id {
                return Err(VaultError::Base(
                    "a grant id does not recompute".to_string(),
                ));
            }
            if !grant_ids.insert(g.grant.grant_id.as_str()) {
                return Err(VaultError::Base("a grant twice".to_string()));
            }
        }
    }
    Ok(())
}
