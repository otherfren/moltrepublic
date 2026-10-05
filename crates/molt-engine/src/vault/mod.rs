// SPDX-License-Identifier: GPL-3.0-or-later

//! **The vault** (`docs/vault/vault_threshold_disclosure.md`, built per
//! `docs/vault/vault_build_plan.md`). Each submodule belongs to one build
//! stage; this file only wires them.

use molt_core::vault::{VaultCtx, VaultRefusal, VaultView};
use molt_core::{MemberIdentity, MoltError};
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
/// every seat keyed, `vault` in the features, the bounds, and canonical,
/// unique keys. A table without keys passes, `vault` or not (the v5 mock).
/// Returns whether the table is a real vault.
pub(crate) fn check_roster_keys(
    rule_m: u8,
    rule_n: u8,
    identities: &[MemberIdentity],
    features: Option<&[String]>,
) -> Result<bool, String> {
    if identities.iter().all(|i| i.vault_pk.is_empty()) {
        return Ok(false);
    }
    if !has_vault(features) {
        return Err("vault keys without the vault feature".to_string());
    }
    if !bounds_ok(rule_m, rule_n) {
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

impl crate::State {
    /// The adopted chain's vault context; `None` outside a vault republic.
    /// For commands and the view only - a walk takes the context of the
    /// chain it walks.
    pub(crate) fn vault_ctx(&self) -> Option<VaultCtx> {
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

    /// The adopted chain's genesis is roster-v6.
    pub(crate) fn is_vault_republic(&self) -> bool {
        self.vault_ctx().is_some()
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
    use crate::chain::test_support::{chain_signer, wire, Builder};
    use molt_core::{ProposalId, Surface, WorkspaceEvent};
    use serde_json::json;

    fn add_vault() -> Value {
        json!({ "op": "set_features", "value": "memory vault" })
    }

    fn founding_only(r: Result<molt_core::Reply, MoltError>) {
        match r {
            Err(MoltError::Vault(VaultRefusal::FoundingOnly)) => {}
            other => panic!("expected `founding only`, got {other:?}"),
        }
    }

    /// D11: no NEW proposal adds the vault - not at propose, not at approve,
    /// not off the wire.
    #[test]
    fn set_features_refuses_vault_at_propose_approve_and_wire() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut st = chain_signer("petra", &b, b.blocks.clone());
        assert!(st.is_chain_governed());

        founding_only(st.cmd_propose(Surface::Organization, add_vault()));

        wire(
            &mut st,
            "walter",
            1,
            WorkspaceEvent::Proposed { id: ProposalId(2), surface: Surface::Organization, payload: add_vault() },
        );
        assert!(!st.proposals.contains_key(&2), "the wire twin drops it");
        assert!(!st.receive_proposed(4, Surface::Organization, add_vault(), "walter"));
        assert!(!st.proposals.contains_key(&4));

        // a card that got in anyway (an older build's pool) is never signed
        st.proposals.insert(
            6,
            molt_core::ProposalRecord {
                surface: Surface::Organization,
                payload: add_vault(),
                approvals: 0,
                state: molt_core::ProposalState::Proposed,
                applied_at: 0,
                declined_at: 0,
                declined_by: String::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: "walter".to_string(),
                superseded: false,
                superseded_kind: None,
                withdrawn: false,
                wiki_rev: None,
            },
        );
        founding_only(st.cmd_approve(ProposalId(6), None));
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

    /// D11 refuses ADDING the vault: where it is already effective (a live
    /// v5 mock), an older peer's `set_features` naming it stays votable.
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
            Err(MoltError::Vault(VaultRefusal::FoundingOnly))
        ));
        assert!(!matches!(
            st.cmd_propose(Surface::Organization, named),
            Err(MoltError::Vault(VaultRefusal::FoundingOnly))
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
