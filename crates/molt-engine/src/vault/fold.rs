// SPDX-License-Identifier: GPL-3.0-or-later

//! The folded vault base on this holder (plan stage S5, spec §9.3): what
//! the chain commits to, whether it is here, adopting it only after the
//! hash check and a full re-verification (plan 1.3.16), and retiring
//! replaced payloads once a cut made them unreachable (plan 1.3.14).

use std::collections::BTreeSet;

use molt_core::vault::{VaultBase, VaultBaseProgress, VaultOp};
use molt_core::{MoltError, Reply, SessionScope, Surface};

use crate::chain::vault_base::{base_commitment_of, commitment};

/// The base of a republic whose cut found no deposit: derivable, so never
/// fetched.
static EMPTY: VaultBase = VaultBase { deposits: Vec::new() };

impl crate::State {
    /// The commitment the vault group carries after a vault cut.
    pub(crate) fn vault_base_committed(&self) -> Option<(String, u64)> {
        if !self.is_vault_prepared() {
            return None;
        }
        self.applied_payloads(Surface::Vault).find_map(base_commitment_of)
    }

    /// The base this holder keeps for the committed hash, the empty one
    /// included; `None` when nothing is committed or it is not here.
    pub(crate) fn vault_base_now(&self) -> Option<&VaultBase> {
        let (want, _) = self.vault_base_committed()?;
        match (&self.chain.vault_base, &self.chain.vault_base_hash) {
            (Some(b), Some(h)) if *h == want => Some(b),
            _ if want == commitment(&EMPTY).0 => Some(&EMPTY),
            _ => None,
        }
    }

    /// Keep `base` (`None` forgets it) with its hash.
    pub(crate) fn set_vault_base(&mut self, base: Option<VaultBase>) {
        self.chain.vault_base_hash = base.as_ref().map(|b| commitment(b).0);
        self.chain.vault_base = base;
    }

    /// The real base-pending check (plan 1.3.15).
    pub(crate) fn vault_base_pending_now(&self) -> bool {
        self.vault_base_committed().is_some() && self.vault_base_now().is_none()
    }

    /// The typed refusal while the base is pending.
    pub(crate) fn vault_base_pending_error(&self) -> MoltError {
        let (want, size) = self.vault_base_committed().unwrap_or_default();
        MoltError::VaultBasePending { have: 0, size, want }
    }

    /// The view's progress line while pending.
    pub(crate) fn vault_base_progress(&self) -> Option<VaultBaseProgress> {
        if !self.vault_base_pending() {
            return None;
        }
        let (_, size) = self.vault_base_committed().unwrap_or_default();
        Some(VaultBaseProgress { have: 0, size })
    }

    /// Write the base to disk (`None` drops it). Returns whether durable.
    pub(crate) fn persist_vault_base(&self, base: Option<&VaultBase>) -> bool {
        let Some(active) = &self.active else {
            return true;
        };
        let bytes = base.map(molt_core::vault::vault_base_canonical_bytes);
        active.handle.persist_vault_base_blocking(bytes)
    }

    /// Take `bytes` as the base only if they hash to the commitment AND
    /// every record re-verifies under the base's own context (plan 1.3.16).
    /// Anything else is deleted for a refetch. Returns whether adopted.
    pub(crate) fn adopt_vault_base(&mut self, bytes: Option<Vec<u8>>) -> bool {
        let Some(bytes) = bytes else {
            return false;
        };
        let Some((want, _)) = self.vault_base_committed() else {
            // nothing commits to a base: the file is residue
            self.set_vault_base(None);
            let _ = self.persist_vault_base(None);
            return false;
        };
        let have = molt_storage::content_hash(&bytes);
        let verdict = if have != want {
            Err("hash".to_string())
        } else {
            molt_core::vault::decode_vault_base(&bytes).and_then(|base| {
                let ctx = self.vault_founding_ctx().ok_or("no vault")?;
                molt_vault::verify_base(&base, &self.republic_id(), &ctx)
                    .map(|()| base)
                    .map_err(|e| e.to_string())
            })
        };
        match verdict {
            Ok(base) => {
                self.set_vault_base(Some(base));
                true
            }
            Err(reason) => {
                tracing::warn!(%have, %want, %reason, "vault_base=refused action=refetch");
                if self.chain.vault_base_hash.as_deref() != Some(want.as_str()) {
                    self.set_vault_base(None);
                }
                let _ = self.persist_vault_base(None);
                false
            }
        }
    }

    /// Forget a held base the projection no longer commits to.
    pub(crate) fn drop_a_stale_vault_base(&mut self) {
        let want = self.vault_base_committed().map(|(h, _)| h);
        if self.chain.vault_base_hash.is_some() && self.chain.vault_base_hash != want {
            self.set_vault_base(None);
        }
    }

    /// The base arrived: what waited on it runs now.
    pub(crate) fn after_vault_base_adopted(&mut self) {
        self.vault_flush_queued();
        self.supersede_stale_vault_cards();
        self.bump_applied_epoch();
        self.emit_session(SessionScope::Full);
    }

    /// Plan 1.3.14: once a cut dropped the blocks naming a replaced
    /// version, its payload file goes; never while base-pending. Returns
    /// whether every such file is gone.
    pub(crate) fn vault_retire_at_cut(&mut self) -> bool {
        if !self.is_vault_prepared() || self.vault_base_pending() {
            return false;
        }
        self.vault_sync_held();
        let named: BTreeSet<String> =
            self.vault_named_payloads().into_iter().map(|n| n.secret_id).collect();
        let on_disk: BTreeSet<String> = self
            .active
            .as_ref()
            .map(|a| molt_storage::list_vault_payloads(&a.dir).into_iter().collect())
            .unwrap_or_default();
        let gone: Vec<String> = self
            .files
            .vault
            .held
            .iter()
            .chain(&on_disk)
            .filter(|sid| !named.contains(*sid))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut retired = 0usize;
        for sid in &gone {
            if let Some(a) = &self.active {
                if !a.handle.persist_vault_payload_blocking(sid, None) {
                    continue;
                }
            }
            self.files.vault.held.remove(sid);
            self.files.vault.on_disk.remove(sid);
            retired += 1;
        }
        if !gone.is_empty() {
            tracing::info!(count = retired, failed = gone.len() - retired, "vault_payload=retired");
        }
        retired == gone.len()
    }

    /// The founding table of the adopted chain: the genesis, or after a cut
    /// the anchor's `founding_identities` (same order, plan 1.3.5).
    pub(crate) fn vault_founding_table(&self) -> Vec<molt_core::MemberIdentity> {
        if let Some(blob) = &self.chain.checkpoint_blob {
            return blob.founding_identities.clone();
        }
        match self.chain.blocks.first().map(|b| &b.change) {
            Some(molt_core::ChainChange::Genesis { identities, .. }) => identities.clone(),
            _ => Vec::new(),
        }
    }

    /// Plan 1.3.2 / S5 step 7: re-derive this seat's vault seed from its
    /// phrase entropy and its FOUNDING row, and keep and persist it only
    /// when it re-derives the founding `vault_pk`. Returns whether it did.
    pub(crate) fn vault_seed_from_entropy(&mut self, entropy: &[u8]) -> bool {
        if !self.is_vault_prepared() || self.vault_seed.is_some() {
            return false;
        }
        let Some(pk) = self.identity_sk.as_ref().map(|sk| hex::encode(sk.verifying_key().to_bytes())) else {
            return false;
        };
        let Some(seed) = super::seed_for_seat(entropy, &self.vault_founding_table(), &pk) else {
            tracing::warn!("vault_seed=mismatch action=refuse");
            return false;
        };
        self.vault_seed = super::seed_array(&seed);
        if let Some(store) = self.file_store() {
            tokio::spawn(async move {
                use molt_net::supervisor::StateStore as _;
                store
                    .update(|s| {
                        s.vault_seed = Some(seed);
                        true
                    })
                    .await;
            });
        }
        tracing::info!("vault_seed=rederived");
        true
    }

    /// The deposits a held base names, as Applied payloads.
    pub(crate) fn vault_base_payloads(&self) -> Vec<serde_json::Value> {
        self.vault_base_now()
            .map(|b| {
                b.deposits
                    .iter()
                    .filter_map(|d| serde_json::to_value(VaultOp::Deposit(d.deposit.clone())).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn cmd_net_vault_base_fetched(&mut self, bytes: Vec<u8>) -> Result<Reply, MoltError> {
        if !self.vault_base_pending() {
            return Ok(Reply::Ack);
        }
        if !self.adopt_vault_base(Some(bytes)) {
            return Ok(Reply::Ack);
        }
        let durable = self.persist_vault_base(self.chain.vault_base.as_ref());
        if !durable {
            tracing::warn!("vault_base=fetched persisted=false");
        }
        tracing::info!(deposits = self.chain.vault_base.as_ref().map_or(0, |b| b.deposits.len()), "vault_base=adopted");
        self.after_vault_base_adopted();
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_base_failed(&mut self) -> Result<Reply, MoltError> {
        tracing::debug!("vault_base=fetch_incomplete");
        Ok(Reply::Ack)
    }
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;
