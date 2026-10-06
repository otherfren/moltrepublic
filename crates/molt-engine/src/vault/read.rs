// SPDX-License-Identifier: GPL-3.0-or-later

//! The reader side: combine answers, decrypt, never store (plan stage S4,
//! spec §8 step 4).

use molt_core::vault::{VaultDeposit, VaultRefusal};
use molt_core::{MoltError, Reply};

/// Combine `shares`, open `file`: the text, never kept.
fn finish(dep: &VaultDeposit, republic_id: &str, secret_id: String, shares: &[molt_vault::SeatShare], file: &[u8]) -> Result<Reply, MoltError> {
    match molt_vault::read(dep, republic_id, shares, file) {
        Ok(opened) => Ok(Reply::VaultText {
            secret_id,
            name: dep.name.clone(),
            kind: dep.kind.clone(),
            text: opened.text,
        }),
        Err(fault) => {
            tracing::warn!(secret_id = %secret_id, bad = ?fault.bad_seats, payload = fault.payload, "vault: read failed");
            Err(MoltError::Vault(VaultRefusal::NotVerified))
        }
    }
}

impl crate::State {
    pub(crate) fn cmd_vault_read(&mut self, secret_id: String) -> Result<Reply, MoltError> {
        if self.vault_ctx().is_none() {
            return Err(MoltError::Vault(VaultRefusal::NoVault));
        }
        // an empty projection must not read as `not the reader`
        if self.vault_base_pending() {
            return Err(MoltError::VaultBasePending { have: 0, size: 0, want: String::new() });
        }
        let me = self.member();
        let vs = self.vault_state();
        let granted = vs.is_current(&secret_id)
            && vs.grants.iter().any(|g| !g.void && g.grant.secret_id == secret_id && g.grant.reader == me);
        let Some(dep) = vs.versions.get(&secret_id).map(|v| v.deposit.clone()).filter(|_| granted) else {
            return Err(MoltError::Vault(VaultRefusal::NotTheReader));
        };
        if self.vault_seed.is_none() {
            return Err(MoltError::Vault(VaultRefusal::NoVaultKey));
        }
        if !self.vault_payload_held(&secret_id) {
            return Err(MoltError::Vault(VaultRefusal::PayloadNotHeld));
        }
        let shares = self.vault_reader_shares(&secret_id);
        let have = u8::try_from(shares.len()).unwrap_or(u8::MAX);
        if have < dep.m {
            self.vault_ask(&secret_id);
            return Ok(Reply::VaultPending { secret_id, have, need: dep.m });
        }
        let rid = self.republic_id();
        #[cfg(test)]
        if self.active.is_none() {
            let file = self.grants_rt().payloads.get(&secret_id).cloned().unwrap_or_default();
            return finish(&dep, &rid, secret_id, &shares, &file);
        }
        let storage = self
            .active
            .as_ref()
            .map(|a| a.handle.clone())
            .ok_or(MoltError::Vault(VaultRefusal::PayloadNotHeld))?;
        // taken LAST: every refusal above is answered by the actor loop
        let Some(reply) = self.deferred_reply.take() else {
            return Err(MoltError::Engine("vault read: no reply channel".to_string()));
        };
        tokio::spawn(async move {
            let out = match storage.load_vault_payload(&secret_id).await {
                Some(file) => finish(&dep, &rid, secret_id, &shares, &file),
                None => Err(MoltError::Vault(VaultRefusal::PayloadNotHeld)),
            };
            let _ = reply.send(out);
        });
        Ok(Reply::Ack)
    }
}
