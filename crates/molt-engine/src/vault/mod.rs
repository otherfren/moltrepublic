// SPDX-License-Identifier: GPL-3.0-or-later

//! **The vault** (`docs_archive/vault/vault_threshold_disclosure.md`, built per
//! `docs_archive/vault/vault_build_plan.md`). Each submodule belongs to one build
//! stage; this file only wires them.

use molt_core::vault::{VaultCtx, VaultRefusal, VaultView};
use molt_core::{MemberIdentity, MoltError, Surface};
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

/// The feature key of the vault.
pub(crate) const VAULT: &str = "vault";

/// The vault context a founding table establishes: `Some` iff the table
/// is roster-v6 (any seat keyed). Callers pass a VERIFIED founding table
/// (genesis or anchor), never a working one (plan 1.3.8, 1.3.13).
pub(crate) fn ctx_from_founding(rule_m: u8, founding: &[MemberIdentity]) -> Option<VaultCtx> {
    if founding.iter().all(|i| i.vault_pk.is_empty()) {
        return None;
    }
    Some(VaultCtx {
        m: rule_m,
        holders_in_genesis_order: founding
            .iter()
            .map(|i| (i.member.clone(), i.identity_pk.clone(), i.vault_pk.clone()))
            .collect(),
    })
}

/// Spec §3: m >= 2 and m <= n - 2.
pub(crate) fn bounds_ok(rule_m: u8, rule_n: u8) -> bool {
    rule_m >= 2 && u16::from(rule_m) + 2 <= u16::from(rule_n)
}

fn has_vault(features: Option<&[String]>) -> bool {
    features.is_some_and(|f| f.iter().any(|k| k == VAULT))
}

/// The roster rule every door applies (plan 1.3.12): any vault key means
/// every seat keyed with canonical, unique keys and a feature set - a
/// prepared vault (E2); `vault` in the features of a keyed table also needs
/// the bounds. A table without keys passes, `vault` or not (the v5 mock).
/// Returns whether the table is keyed.
pub(crate) fn check_roster_keys(
    rule_m: u8,
    rule_n: u8,
    identities: &[MemberIdentity],
    features: Option<&[String]>,
) -> Result<bool, String> {
    if identities.iter().all(|i| i.vault_pk.is_empty()) {
        return Ok(false);
    }
    // v6 writes `None` and `Some([])` as the same bytes: only one may exist
    if features.is_none() {
        return Err("vault keys without a feature set".to_string());
    }
    if has_vault(features) && !bounds_ok(rule_m, rule_n) {
        return Err(VaultRefusal::Bounds.to_string());
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in identities {
        if id.vault_pk.is_empty() {
            return Err(format!("seat {} has no vault key", id.member));
        }
        if molt_vault::canonical_vault_pk(&id.vault_pk).ok().as_deref() != Some(id.vault_pk.as_str())
        {
            return Err(format!("seat {} carries an invalid vault key", id.member));
        }
        if !seen.insert(id.vault_pk.as_str()) {
            return Err(format!("seat {} shares its vault key", id.member));
        }
    }
    Ok(true)
}

/// A NEW founding adds the converse: `vault` in the features needs keys.
pub(crate) fn check_new_founding_keys(
    rule_m: u8,
    rule_n: u8,
    identities: &[MemberIdentity],
    features: Option<&[String]>,
) -> Result<bool, String> {
    let real = check_roster_keys(rule_m, rule_n, identities, features)?;
    if !real && has_vault(features) {
        return Err("the vault feature without vault keys".to_string());
    }
    Ok(real)
}

/// Is the vault on at the walked state (E5)? Judged from the chain under
/// verification only: its founding features plus every applied
/// `set_features`, and the bounds of its own context, so an
/// out-of-bounds enable that committed anyway enables nothing.
pub(crate) fn walk_enabled(state: &molt_core::CheckpointState, ctx: &VaultCtx) -> bool {
    let org = state
        .applied
        .iter()
        .filter(|(s, _)| *s == Surface::Organization)
        .flat_map(|(_, list)| list)
        .map(|(_, p)| p);
    enabled_by(ctx, state.founding_features.as_deref(), org)
}

/// The one "enabled" rule: the bounds of `ctx`, and `vault` among the
/// founding features or named by an applied `set_features`.
fn enabled_by<'a>(ctx: &VaultCtx, founding: Option<&[String]>, mut org: impl Iterator<Item = &'a Value>) -> bool {
    ctx_bounds_ok(ctx) && (has_vault(founding) || org.any(sets_vault))
}

fn ctx_bounds_ok(ctx: &VaultCtx) -> bool {
    bounds_ok(ctx.m, u8::try_from(ctx.holders_in_genesis_order.len()).unwrap_or(u8::MAX))
}

/// An Organization payload whose `set_features` names the vault.
pub(crate) fn sets_vault(payload: &Value) -> bool {
    payload.get("op").and_then(Value::as_str) == Some("set_features")
        && payload
            .get("value")
            .and_then(Value::as_str)
            .is_some_and(|v| v.split_whitespace().any(|k| k == VAULT))
}

/// This seat's vault key from its phrase entropy and its FOUNDING seat:
/// `(seed, vault_pk)`.
pub(crate) fn seat_vault_key(
    entropy: &[u8],
    founding_nostr_pk: &str,
    identity_pk: &str,
) -> (zeroize::Zeroizing<[u8; 32]>, String) {
    let seed = molt_vault::derive_vault_seed(entropy, founding_nostr_pk, identity_pk);
    let (_, pk) = molt_vault::vault_keypair(&seed);
    (seed, pk)
}

/// The seed to persist for `identity_pk`'s seat of a vault founding table,
/// only when it re-derives that seat's FOUNDING `vault_pk` (plan 1.3.2).
pub(crate) fn seed_for_seat(
    entropy: &[u8],
    founding: &[MemberIdentity],
    identity_pk: &str,
) -> Option<molt_core::vault::SecretBytes> {
    let seat = founding.iter().find(|i| i.identity_pk == identity_pk)?;
    if seat.vault_pk.is_empty() {
        return None;
    }
    let (seed, pk) = seat_vault_key(entropy, &seat.nostr_pk, &seat.identity_pk);
    if pk != seat.vault_pk {
        tracing::warn!(member = %seat.member, "vault_seed=mismatch");
        return None;
    }
    Some(molt_core::vault::SecretBytes(seed.to_vec()))
}

/// A persisted seed as the 32 bytes it must be; anything else is missing.
pub(crate) fn seed_array(b: &molt_core::vault::SecretBytes) -> Option<zeroize::Zeroizing<[u8; 32]>> {
    <[u8; 32]>::try_from(b.0.as_slice()).ok().map(zeroize::Zeroizing::new)
}

impl crate::State {
    /// The adopted chain's vault context while the vault is ENABLED (E5);
    /// `None` outside a vault republic and while it is only prepared. For
    /// commands and the view only - a walk takes the context of the chain
    /// it walks.
    pub(crate) fn vault_ctx(&self) -> Option<VaultCtx> {
        let ctx = self.vault_founding_ctx()?;
        self.vault_on_chain(&ctx).then_some(ctx)
    }

    /// [`walk_enabled`] over the adopted chain: its founding features (the
    /// genesis, or the anchor's) and its applied `set_features`.
    fn vault_on_chain(&self, ctx: &VaultCtx) -> bool {
        let founding = match (&self.chain.checkpoint_blob, self.chain.blocks.first().map(|b| &b.change)) {
            (Some(blob), _) => blob.founding_features.as_deref(),
            (None, Some(molt_core::ChainChange::Genesis { features, .. })) => features.as_deref(),
            _ => None,
        };
        let org = self.chain.applied.get(&Surface::Organization).into_iter().flatten().map(|(_, p)| p);
        enabled_by(ctx, founding, org)
    }

    /// The adopted chain's founding vault context: `Some` iff its genesis
    /// (or anchor) is roster-v6 - the vault is PREPARED (E2), enabled or not.
    pub(crate) fn vault_founding_ctx(&self) -> Option<VaultCtx> {
        self.chain.head.as_ref()?;
        if let Some(blob) = &self.chain.checkpoint_blob {
            return ctx_from_founding(blob.rule_m, &blob.founding_identities);
        }
        match self.chain.blocks.first().map(|b| &b.change) {
            Some(molt_core::ChainChange::Genesis { rule_m, identities, .. }) => {
                ctx_from_founding(*rule_m, identities)
            }
            _ => None,
        }
    }

    /// The vault is real and enabled here.
    pub(crate) fn is_vault_republic(&self) -> bool {
        self.vault_ctx().is_some()
    }

    /// The adopted chain's genesis is roster-v6 (cut variant, op set, seed).
    pub(crate) fn is_vault_prepared(&self) -> bool {
        self.vault_founding_ctx().is_some()
    }

    /// The vault commands' entry gate: `vault: not enabled` like any
    /// disabled feature, `no vault` for the v5 mock.
    pub(crate) fn require_vault(&self) -> Result<VaultCtx, MoltError> {
        if let Some(ctx) = self.vault_ctx() {
            return Ok(ctx);
        }
        let mock = !self.is_vault_prepared() && self.effective_features().iter().any(|f| f == VAULT);
        Err(if mock {
            MoltError::Vault(VaultRefusal::NoVault)
        } else {
            MoltError::FeatureDisabled(Surface::Vault.as_str())
        })
    }

    /// Whether a `set_features` vote can switch the vault on (spec §3).
    pub(crate) fn vault_enable(&self) -> molt_core::vault::VaultEnable {
        use molt_core::vault::VaultEnable;
        match self.vault_founding_ctx() {
            Some(ctx) if self.vault_on_chain(&ctx) => VaultEnable::On,
            Some(ctx) if ctx_bounds_ok(&ctx) => VaultEnable::Offer,
            Some(_) => VaultEnable::Bounds,
            // the v5 mock
            None if self.effective_features().iter().any(|f| f == VAULT) => VaultEnable::On,
            None => VaultEnable::NeedsNewerRepublic,
        }
    }

    /// E4: why a NEW proposal may not add the vault (propose, approve,
    /// the wire); `None` when it adds nothing or may add it. History folds
    /// regardless.
    pub(crate) fn vault_enable_refusal(&self, surface: Surface, payload: &Value) -> Option<VaultRefusal> {
        if surface != Surface::Organization || !sets_vault(payload) {
            return None;
        }
        match self.vault_enable() {
            molt_core::vault::VaultEnable::NeedsNewerRepublic => Some(VaultRefusal::NeedsNewerRepublic),
            molt_core::vault::VaultEnable::Bounds => Some(VaultRefusal::Bounds),
            _ => None,
        }
    }

    /// The republic folded its vault into a base this node does not hold.
    pub(crate) fn vault_base_pending(&self) -> bool {
        #[cfg(test)]
        if self.vault_seams.base_pending() {
            return true;
        }
        self.vault_base_pending_now()
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
            base_pending: self.vault_base_progress(),
            ..VaultView::default()
        };
        // a pending base is a typed state, never an empty vault
        if view.base_pending.is_some() {
            return view;
        }
        deposit::fill(self, &mut view);
        receipts::fill(self, &mut view);
        grant::fill(self, &mut view);
        view
    }

    /// The vault arm of `approve`, by op.
    pub(crate) fn vault_approve_check(&self, payload: &Value) -> Result<(), MoltError> {
        if self.vault_seams.skip_checks.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        match vault_arm(payload, self.is_vault_prepared())? {
            VaultArm::Deposit => self.approve_check_deposit(payload),
            VaultArm::Grant => self.approve_check_grant(payload),
            VaultArm::Mock => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::test_support::{chain_signer, genesis_seat, wire, Builder};
    use molt_core::{ProposalId, Surface, WorkspaceEvent};
    use serde_json::json;

    fn add_vault() -> Value {
        json!({ "op": "set_features", "value": "memory vault" })
    }

    fn refused_with(r: Result<molt_core::Reply, MoltError>, want: &VaultRefusal) {
        match r {
            Err(MoltError::Vault(got)) if &got == want => {}
            other => panic!("expected `{want}`, got {other:?}"),
        }
    }

    fn card(payload: Value, by: &str) -> molt_core::ProposalRecord {
        molt_core::ProposalRecord {
            surface: Surface::Organization,
            payload,
            approvals: 0,
            state: molt_core::ProposalState::Proposed,
            applied_at: 0,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: by.to_string(),
            superseded: false,
            superseded_kind: None,
            withdrawn: false,
            wiki_rev: None,
        }
    }

    /// Propose, the wire and approve all give `want` for adding the vault
    /// on `st` (whose peer `by` sends the wire twin).
    fn every_door_refuses(st: &mut crate::State, by: &str, want: &VaultRefusal) {
        refused_with(st.cmd_propose(Surface::Organization, add_vault()), want);
        wire(
            st,
            by,
            1,
            WorkspaceEvent::Proposed { id: ProposalId(2), surface: Surface::Organization, payload: add_vault() },
        );
        assert!(!st.proposals.contains_key(&2), "the wire twin drops it");
        assert!(!st.receive_proposed(4, Surface::Organization, add_vault(), by));
        assert!(!st.proposals.contains_key(&4));
        // a card that got in anyway (an older build's pool) is never signed
        st.proposals.insert(6, card(add_vault(), by));
        refused_with(st.cmd_approve(ProposalId(6), None), want);
    }

    /// E4: a roster-v5 genesis has no keys, so no vote can add the vault.
    #[test]
    fn set_features_vault_needs_a_newer_republic_on_v5() {
        // roster-v4 (no feature set) and the real v5 shape
        for b in [
            Builder::new(&["petra", "walter"], 2),
            Builder::new_with_features(&["petra", "walter"], 2, &["memory"], false),
        ] {
            let mut st = genesis_seat("petra", &b, b.blocks.clone());
            assert!(st.is_chain_governed() && !st.is_vault_prepared());
            every_door_refuses(&mut st, "walter", &VaultRefusal::NeedsNewerRepublic);
        }
    }

    /// E4: a prepared republic outside `2 <= m <= n-2` cannot enable it.
    #[test]
    fn set_features_vault_refuses_outside_the_bounds() {
        let b = Builder::new_with_features(&["a", "b", "c"], 2, &["memory"], true);
        let mut st = genesis_seat("a", &b, b.blocks.clone());
        assert!(st.is_vault_prepared());
        every_door_refuses(&mut st, "b", &VaultRefusal::Bounds);
    }

    /// E4: a prepared republic within the bounds votes the vault in at
    /// every door.
    #[test]
    fn set_features_enables_the_vault_in_a_prepared_republic() {
        let b = Builder::new_with_features(&["a", "b", "c", "d"], 2, &["memory"], true);
        let mut st = genesis_seat("a", &b, b.blocks.clone());
        st.cmd_propose(Surface::Organization, add_vault()).expect("propose takes it");
        wire(
            &mut st,
            "b",
            1,
            WorkspaceEvent::Proposed { id: ProposalId(20), surface: Surface::Organization, payload: add_vault() },
        );
        assert!(st.proposals.contains_key(&20), "the wire twin keeps it");
        st.cmd_approve(ProposalId(20), None).expect("approve signs it");
    }

    /// E4: history is never refused - a live v5 republic that applied the
    /// mock's `set_features vault` keeps verifying.
    #[test]
    fn a_historic_set_features_vault_block_still_verifies() {
        for mut b in [
            Builder::new(&["petra", "walter"], 2),
            Builder::new_with_features(&["petra", "walter"], 2, &["memory"], false),
        ] {
            b.commit_org(10, "set_features", "memory vault", &["petra", "walter"]);
            crate::chain::verify_chain(&b.blocks).expect("the historic block verifies");
            let st = genesis_seat("petra", &b, b.blocks.clone());
            assert!(st.effective_features().iter().any(|f| f == VAULT), "the mock is effective");
            assert!(!st.is_vault_republic(), "and stays the mock");
            assert_eq!(st.vault_enable(), molt_core::vault::VaultEnable::On);
        }
    }

    /// The vault is never a keep-requirement: a republic where it is
    /// effective still votes other features without naming it.
    #[test]
    fn a_republic_with_the_vault_still_votes_other_features() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut st = chain_signer("petra", &b, b.blocks.clone());
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(vec!["memory".to_string(), "vault".to_string()]);
        }
        st.cmd_propose(
            Surface::Organization,
            json!({ "op": "set_features", "value": "memory quests" }),
        )
        .expect("vault is kept by the union, not by the proposal");
    }

    /// Refusing applies to ADDING the vault: where it is already effective
    /// (a live v5 mock), an older peer's `set_features` naming it stays
    /// votable.
    #[test]
    fn set_features_naming_an_effective_vault_is_not_refused() {
        let named = json!({ "op": "set_features", "value": "memory quests vault" });
        let b = Builder::new(&["petra", "walter"], 2);
        let mut st = chain_signer("petra", &b, b.blocks.clone());
        if let Some(r) = st.replica.as_mut() {
            r.features = Some(vec!["memory".to_string(), "vault".to_string()]);
        }
        wire(
            &mut st,
            "walter",
            1,
            WorkspaceEvent::Proposed { id: ProposalId(2), surface: Surface::Organization, payload: named.clone() },
        );
        assert!(st.proposals.contains_key(&2), "the wire twin keeps it");
        assert!(!matches!(
            st.cmd_approve(ProposalId(2), None),
            Err(MoltError::Vault(VaultRefusal::NeedsNewerRepublic))
        ));
        assert!(!matches!(
            st.cmd_propose(Surface::Organization, named),
            Err(MoltError::Vault(VaultRefusal::NeedsNewerRepublic))
        ));
    }

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
