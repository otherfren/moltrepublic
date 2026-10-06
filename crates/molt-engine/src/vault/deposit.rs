// SPDX-License-Identifier: GPL-3.0-or-later

//! Deposit: seal, the verify-gated approve, apply and the projection
//! (plan stage S3b, spec §6-§7).

use std::collections::{BTreeMap, BTreeSet};

use molt_core::vault::{
    secret_id, SecretText, VaultCtx, VaultDeposit, VaultDepositState, VaultDepositView, VaultGrant,
    VaultMyCheck, VaultOp, VaultRefusal, VaultView,
};
use molt_core::{ChainBlock, ChainChange, MoltError, ProposalState, Reply, Surface};
use serde_json::Value;

use crate::net::vault_payload::NamedPayload;

/// A Vault op checked against the context of the chain it sits in: the
/// closed op set (plan 1.3.9), the canonical JSON, a deposit's shape and
/// `sig_depositor`, a grant's `grant_id` and reader (plan 1.3.10).
pub(crate) fn check_vault_op(
    republic_id: &str,
    proposal_id: u64,
    payload: &Value,
    ctx: &VaultCtx,
) -> Result<VaultOp, String> {
    let op: VaultOp = serde_json::from_value(payload.clone()).map_err(|_| "unknown op".to_string())?;
    if serde_json::to_value(&op).ok().as_ref() != Some(payload) {
        return Err("not canonical".to_string());
    }
    match &op {
        VaultOp::Deposit(dep) => {
            molt_vault::verify_deposit_shape(dep, ctx).map_err(|e| format!("deposit: {e}"))?;
            let pk = ctx
                .holders_in_genesis_order
                .iter()
                .find(|(n, _, _)| *n == dep.depositor)
                .map(|(_, pk, _)| pk.as_str())
                .ok_or("deposit: unknown depositor")?;
            molt_vault::verify_deposit_sig(dep, republic_id, pk).map_err(|e| format!("deposit: {e}"))?;
        }
        VaultOp::Grant(g) => check_grant(g, proposal_id, ctx)?,
        VaultOp::VaultBase(_) => return Err("vault_base outside a cut".to_string()),
    }
    Ok(op)
}

fn check_grant(g: &VaultGrant, proposal_id: u64, ctx: &VaultCtx) -> Result<(), String> {
    if !ctx.holders_in_genesis_order.iter().any(|(n, _, _)| *n == g.reader) {
        return Err("grant: the reader is not a seat".to_string());
    }
    if g.grant_id != molt_core::vault::grant_id(&g.secret_id, &g.reader, proposal_id) {
        return Err("grant: grant_id does not recompute".to_string());
    }
    Ok(())
}

fn refusal(e: molt_vault::VaultError) -> MoltError {
    match e {
        molt_vault::VaultError::TooLarge => MoltError::Vault(VaultRefusal::TooLarge),
        molt_vault::VaultError::Label(l) => MoltError::BadPayload(l),
        other => MoltError::Engine(format!("vault: {other}")),
    }
}

/// The OS RNG behind a ChaCha stream: the deposit nonce and the AEAD
/// nonce (plan 1.3.3: fresh for every deposit).
pub(crate) fn os_rng() -> Result<rand_chacha::ChaCha20Rng, MoltError> {
    use rand_chacha::rand_core::SeedableRng as _;
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::getrandom(seed.as_mut()).map_err(|e| MoltError::Engine(format!("os rng: {e}")))?;
    Ok(rand_chacha::ChaCha20Rng::from_seed(*seed))
}

/// One committed deposit version.
#[derive(Debug, Clone)]
pub(crate) struct VaultVersion {
    pub(crate) deposit: VaultDeposit,
    /// The version it replaced.
    pub(crate) replaces: Option<String>,
}

/// One committed grant.
#[derive(Debug, Clone)]
pub(crate) struct VaultAppliedGrant {
    pub(crate) grant: VaultGrant,
    /// Not current at its commit, or replaced since (plan 1.3.10).
    pub(crate) void: bool,
}

/// The vault projection: a pure function of the adopted chain.
#[derive(Debug, Clone, Default)]
pub(crate) struct VaultState {
    /// `(depositor, name)` -> the current `secret_id`.
    pub(crate) current: BTreeMap<(String, String), String>,
    /// Every committed version above the anchor, replaced ones included.
    pub(crate) versions: BTreeMap<String, VaultVersion>,
    /// Committed grants, in chain order.
    pub(crate) grants: Vec<VaultAppliedGrant>,
}

impl VaultState {
    /// Whether `dep` names its slot's current version (empty: no version).
    pub(crate) fn extends(&self, dep: &VaultDeposit) -> bool {
        self.current.get(&(dep.depositor.clone(), dep.name.clone())).map_or("", String::as_str) == dep.replaces
    }

    /// Stale even for a node that may lag: the slot has moved past what
    /// `dep` replaces, or `dep` itself already committed.
    pub(crate) fn is_stale(&self, republic_id: &str, dep: &VaultDeposit) -> bool {
        !self.extends(dep)
            && self.current.contains_key(&(dep.depositor.clone(), dep.name.clone()))
            && (dep.replaces.is_empty()
                || self.versions.contains_key(&dep.replaces)
                || self.versions.contains_key(&secret_id(republic_id, dep)))
    }

    /// Whether `secret_id` is the current version of its slot.
    pub(crate) fn is_current(&self, secret_id: &str) -> bool {
        self.versions.get(secret_id).is_some_and(|v| {
            self.current.get(&(v.deposit.depositor.clone(), v.deposit.name.clone()))
                .is_some_and(|c| c == secret_id)
        })
    }
}

impl crate::State {
    pub(crate) fn cmd_vault_seal(
        &mut self,
        name: String,
        kind: String,
        text: SecretText,
    ) -> Result<Reply, MoltError> {
        let me = self.member();
        self.vault_seal_inner(&me, &name, &kind, &text)
    }

    /// Seal `text` naming `depositor`; only the `__vault_seal_as` seam
    /// passes another seat than this one (a forged record).
    pub(crate) fn vault_seal_inner(
        &mut self,
        depositor: &str,
        name: &str,
        kind: &str,
        text: &SecretText,
    ) -> Result<Reply, MoltError> {
        let ctx = self.vault_ctx().ok_or(MoltError::Vault(VaultRefusal::NoVault))?;
        let seed = self.vault_seed.clone().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?;
        let sk = self.identity_sk.clone().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?;
        let rid = self.republic_id();
        let replaces = self.vault_slot_current(depositor, name)?;
        let input = molt_vault::DepositInput { republic_id: &rid, depositor, name, kind, replaces: &replaces, text, ctx: &ctx };
        let (dep, file) = molt_vault::build_deposit(&input, &seed, &sk, &mut os_rng()?).map_err(refusal)?;
        self.vault_propose_deposit(dep, &file)
    }

    /// The `secret_id` a new deposit in `(depositor, name)` replaces.
    pub(crate) fn vault_slot_current(&self, depositor: &str, name: &str) -> Result<String, MoltError> {
        if self.vault_base_pending() {
            return Err(self.vault_base_pending_error());
        }
        Ok(self.vault_state().current.get(&(depositor.to_string(), name.to_string())).cloned().unwrap_or_default())
    }

    /// Hold `file` and propose `dep`; the payload is published once the
    /// proposal exists.
    pub(crate) fn vault_propose_deposit(&mut self, dep: VaultDeposit, file: &[u8]) -> Result<Reply, MoltError> {
        let rid = self.republic_id();
        let named = NamedPayload {
            secret_id: secret_id(&rid, &dep),
            hash: dep.payload.hash.clone(),
            size: dep.payload.size,
        };
        let payload = serde_json::to_value(VaultOp::Deposit(dep))
            .map_err(|e| MoltError::Engine(format!("vault: {e}")))?;
        // the payload is held before the proposal exists: the own co-sign
        // needs it, and so does every approver fetching from here
        self.vault_sync_held();
        if !self.vault_store_payload(&named, file) {
            return Err(MoltError::Storage("vault payload not stored".to_string()));
        }
        match self.propose_payload(Surface::Vault, payload) {
            Ok(reply) => {
                self.enqueue_vault_publish(&named.secret_id, &named.hash, named.size);
                tracing::info!(secret_id = %named.secret_id, size = named.size, "vault: deposit proposed");
                Ok(reply)
            }
            Err(e) => {
                self.files.vault.held.remove(&named.secret_id);
                if let Some(a) = &self.active {
                    let _ = a.handle.persist_vault_payload_blocking(&named.secret_id, None);
                }
                Err(e)
            }
        }
    }

    /// The deposit arm of `approve` (spec §7, D12), run on every call: the
    /// record and its signature against the adopted chain, then this
    /// seat's own share and the held payload.
    pub(crate) fn approve_check_deposit(&self, payload: &Value) -> Result<(), MoltError> {
        let ctx = self.vault_ctx().ok_or(MoltError::Vault(VaultRefusal::NoVault))?;
        let rid = self.republic_id();
        let Ok(VaultOp::Deposit(dep)) = check_vault_op(&rid, 0, payload, &ctx) else {
            return Err(MoltError::Vault(VaultRefusal::NotVerified));
        };
        if self.vault_base_pending() {
            return Err(self.vault_base_pending_error());
        }
        if !self.vault_state().extends(&dep) {
            return Err(MoltError::Vault(VaultRefusal::Stale));
        }
        let me = self.member();
        let mine = dep.depositor == me;
        let seed = if mine {
            None
        } else {
            Some(self.vault_seed.as_ref().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?)
        };
        let sid = secret_id(&rid, &dep);
        if !self.vault_payload_held(&sid) {
            return Err(MoltError::Vault(VaultRefusal::PayloadNotHeld));
        }
        if let Some(seed) = seed {
            let (sk, _) = molt_vault::vault_keypair(seed);
            molt_vault::check_my_share(&dep, &rid, &ctx, &me, &sk)
                .map_err(|_| MoltError::Vault(VaultRefusal::NotVerified))?;
        }
        Ok(())
    }

    /// The wire twin (`receive_proposed`): a real vault takes only a
    /// well-formed, correctly signed op; the v5 mock is left alone.
    pub(crate) fn vault_wire_check(&self, id: u64, payload: &Value) -> Result<(), String> {
        if self.vault_seams.skip_checks.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let Some(ctx) = self.vault_ctx() else {
            return Ok(());
        };
        let rid = self.republic_id();
        match check_vault_op(&rid, id, payload, &ctx)? {
            VaultOp::Deposit(dep) if !self.vault_base_pending() && self.vault_state().is_stale(&rid, &dep) => {
                Err("deposit: stale".to_string())
            }
            _ => Ok(()),
        }
    }

    /// The projection over the adopted chain (plus the base, S5).
    pub(crate) fn vault_state(&self) -> VaultState {
        let rid = self.republic_id();
        let mut st = VaultState::default();
        for value in self.applied_payloads(Surface::Vault) {
            // the folded base seeds the projection (S5); a pending one
            // seeds nothing, and every caller checks base-pending first
            if crate::chain::vault_base::base_commitment_of(value).is_some() {
                for d in self.vault_base_now().map(|b| b.deposits.as_slice()).unwrap_or_default() {
                    let sid = secret_id(&rid, &d.deposit);
                    st.current.insert((d.deposit.depositor.clone(), d.deposit.name.clone()), sid.clone());
                    st.versions.insert(sid, VaultVersion { deposit: d.deposit.clone(), replaces: None });
                    for g in &d.grants {
                        st.grants.push(VaultAppliedGrant { grant: g.grant.clone(), void: false });
                    }
                }
                continue;
            }
            match serde_json::from_value::<VaultOp>(value.clone()) {
                Ok(VaultOp::Deposit(dep)) => {
                    // a replay, or a replace that lost a race: void
                    if !st.extends(&dep) {
                        continue;
                    }
                    let sid = secret_id(&rid, &dep);
                    let replaces =
                        st.current.insert((dep.depositor.clone(), dep.name.clone()), sid.clone());
                    if let Some(old) = &replaces {
                        for g in st.grants.iter_mut().filter(|g| &g.grant.secret_id == old) {
                            g.void = true;
                        }
                    }
                    st.versions.insert(sid, VaultVersion { deposit: dep, replaces });
                }
                Ok(VaultOp::Grant(grant)) => {
                    let void = !st.is_current(&grant.secret_id);
                    st.grants.push(VaultAppliedGrant { grant, void });
                }
                _ => {}
            }
        }
        st
    }

    /// A Vault card registered here: its holder check (S3c) and, for a
    /// grant learned late, the supersede a replace would have run.
    pub(crate) fn after_vault_proposed(&mut self, id: u64) {
        let Some(payload) = self.proposals.get(&id).map(|p| p.payload.clone()) else {
            return;
        };
        match serde_json::from_value::<VaultOp>(payload) {
            Ok(VaultOp::Deposit(dep)) => {
                self.supersede_stale_vault_cards();
                if self.proposals.get(&id).is_some_and(|p| p.state == ProposalState::Proposed) {
                    let sid = secret_id(&self.republic_id(), &dep);
                    super::receipts::on_deposit(self, &sid);
                }
            }
            Ok(VaultOp::Grant(_)) => self.supersede_stale_vault_cards(),
            _ => {}
        }
    }

    /// Side effects of an applied Vault block (idempotent, keyed by id).
    /// The projection itself is a pure function of the chain.
    pub(crate) fn after_vault_applied(&mut self, payload: &Value) {
        match serde_json::from_value::<VaultOp>(payload.clone()) {
            Ok(VaultOp::Deposit(dep)) => {
                // a replace: the old version's pending grants and racing
                // replaces die; its payload stays until the cut (plan 1.3.14)
                self.supersede_stale_vault_cards();
                let sid = secret_id(&self.republic_id(), &dep);
                if !self.vault_base_pending() && !self.vault_state().is_current(&sid) {
                    tracing::info!(secret_id = %sid, "vault: deposit void");
                } else {
                    super::receipts::on_deposit(self, &sid);
                }
            }
            Ok(VaultOp::Grant(g)) => super::grant::on_commit(self, &g.grant_id),
            _ => {}
        }
        self.emit_session(crate::SessionScope::Full);
    }

    /// Spec §8.2: a pending grant on a replaced version, or a deposit whose
    /// `replaces` the slot has moved past, drops like a stale wiki patch;
    /// one a reorg makes current again is a vote again. A version not
    /// committed here is not judged.
    pub(crate) fn supersede_stale_vault_cards(&mut self) {
        if self.vault_base_pending() {
            return;
        }
        let st = self.vault_state();
        let rid = self.republic_id();
        let mut stale = Vec::new();
        let mut revived = Vec::new();
        for (id, p) in &self.proposals {
            if p.surface != Surface::Vault {
                continue;
            }
            let replaced = match serde_json::from_value::<VaultOp>(p.payload.clone()) {
                Ok(VaultOp::Grant(g)) => st.versions.contains_key(&g.secret_id) && !st.is_current(&g.secret_id),
                Ok(VaultOp::Deposit(d)) => st.is_stale(&rid, &d),
                _ => continue,
            };
            match p.state {
                ProposalState::Proposed if replaced => stale.push(*id),
                ProposalState::Rejected
                    if !replaced
                        && p.superseded
                        && p.superseded_kind == Some(molt_core::SupersededKind::Conflict) =>
                {
                    revived.push(*id);
                }
                _ => {}
            }
        }
        for id in stale {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Rejected;
                p.superseded = true;
                p.superseded_kind = Some(molt_core::SupersededKind::Conflict);
            }
            self.stash_voted(id);
            tracing::info!(id, "vault: card superseded by a replace");
        }
        for id in revived {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Proposed;
                p.superseded = false;
                p.superseded_kind = None;
            }
            tracing::info!(id, "vault: card open again after a reorg");
        }
    }

    /// A reorg displaced `dropped` (minus the proposal ids the new branch
    /// `kept`): every applied grant among them goes to the audit (plan 1.4).
    pub(crate) fn vault_displaced(&mut self, dropped: &[ChainBlock], kept: &BTreeSet<u64>) {
        for b in dropped {
            let ChainChange::Applied { proposal_id, surface: Surface::Vault, payload } = &b.change else {
                continue;
            };
            if kept.contains(proposal_id) {
                continue;
            }
            if let Ok(VaultOp::Grant(g)) = serde_json::from_value::<VaultOp>(payload.clone()) {
                #[cfg(test)]
                self.vault_seams.note_displaced(&g.grant_id);
                super::grant::on_displaced(self, &g.grant_id);
            }
        }
    }

    /// This seat's own check of a deposit, for its card.
    fn vault_my_check(&self, dep: &VaultDeposit, secret_id: &str, ctx: &VaultCtx) -> VaultMyCheck {
        let me = self.member();
        if dep.depositor == me || !dep.holders.contains(&me) {
            return VaultMyCheck::None;
        }
        let Some(seed) = self.vault_seed.as_ref() else {
            return VaultMyCheck::Pending;
        };
        let (sk, _) = molt_vault::vault_keypair(seed);
        if molt_vault::check_my_share(dep, &self.republic_id(), ctx, &me, &sk).is_err() {
            return VaultMyCheck::Bad;
        }
        if self.vault_payload_held(secret_id) {
            VaultMyCheck::Ok
        } else {
            VaultMyCheck::Pending
        }
    }

    fn deposit_card(
        &self,
        dep: &VaultDeposit,
        sid: String,
        ctx: &VaultCtx,
        state: VaultDepositState,
    ) -> VaultDepositView {
        let n = u8::try_from(ctx.holders_in_genesis_order.len()).unwrap_or(u8::MAX);
        VaultDepositView {
            depositor: dep.depositor.clone(),
            name: dep.name.clone(),
            kind: dep.kind.clone(),
            size: dep.payload.size,
            state,
            holders: n.saturating_sub(1),
            readable_by: ctx.m,
            mine: dep.depositor == self.member(),
            held: self.vault_payload_held(&sid),
            my_check: self.vault_my_check(dep, &sid, ctx),
            secret_id: sid,
            ..VaultDepositView::default()
        }
    }
}

/// Deposit cards: every current committed version, then every open
/// deposit proposal.
pub(crate) fn fill(st: &crate::State, view: &mut VaultView) {
    let Some(ctx) = st.vault_ctx() else {
        return;
    };
    let rid = st.republic_id();
    let vs = st.vault_state();
    for sid in vs.current.values() {
        let Some(v) = vs.versions.get(sid) else {
            continue;
        };
        let mut card = st.deposit_card(&v.deposit, sid.clone(), &ctx, VaultDepositState::Committed);
        card.replaces = v.replaces.clone();
        view.deposits.push(card);
    }
    for (id, p) in &st.proposals {
        if p.surface != Surface::Vault || p.state != ProposalState::Proposed || p.withdrawn {
            continue;
        }
        let Ok(VaultOp::Deposit(dep)) = serde_json::from_value::<VaultOp>(p.payload.clone()) else {
            continue;
        };
        let sid = secret_id(&rid, &dep);
        let replaces = vs.current.get(&(dep.depositor.clone(), dep.name.clone())).cloned();
        let mut card = st.deposit_card(&dep, sid, &ctx, VaultDepositState::Pending);
        card.proposal = Some(*id);
        card.replaces = replaces;
        view.deposits.push(card);
    }
}

#[cfg(test)]
#[path = "deposit_tests.rs"]
mod tests;
