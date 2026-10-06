// SPDX-License-Identifier: GPL-3.0-or-later

//! **The vault as ONE commitment** (vault spec §9.3, plan stage S5).
//!
//! Every cut of a roster-v6 republic folds the vault group: the current
//! version of each `(depositor, name)` slot and the grants on exactly that
//! version become one `molt-vault-base-v1` stream, and the checkpointed
//! state carries a single `vault_base` entry committing to it. Like the
//! wiki fold it is deterministic: a node folding the raw blocks and a node
//! folding onto a held base reach the same base, so they sign the same cut.

use std::collections::BTreeMap;

use molt_core::vault::{
    secret_id, vault_base_canonical_bytes, VaultBase, VaultBaseDeposit, VaultBaseGrant, VaultOp,
    VaultPayloadRef,
};
use serde_json::Value;

/// The op the fold writes in place of the vault group.
pub(crate) const VAULT_BASE_OP: &str = "vault_base";

/// The fold entry's proposal id: names no real proposal (`next_id` starts
/// at 1), like the wiki base entry.
const BASE_ENTRY_ID: u64 = 0;

/// The commitment a base carries: content hash and byte length.
pub(crate) fn commitment(base: &VaultBase) -> (String, u64) {
    let bytes = vault_base_canonical_bytes(base);
    let size = u64::try_from(bytes.len())
        .expect("field exceeds the u32/u64 framing - ambiguous signed bytes are never written");
    (molt_storage::content_hash(&bytes), size)
}

/// The commitment of a vault payload entry, if `payload` is the fold entry.
pub(crate) fn base_commitment_of(payload: &Value) -> Option<(String, u64)> {
    (payload.get("op").and_then(Value::as_str) == Some(VAULT_BASE_OP)).then(|| {
        (
            payload.get("hash").and_then(Value::as_str).unwrap_or_default().to_string(),
            payload.get("size").and_then(Value::as_u64).unwrap_or_default(),
        )
    })
}

/// The slots of a base being folded: `(depositor, name)` -> the current
/// version and its grants.
type Slots = BTreeMap<(String, String), VaultBaseDeposit>;

fn seed(slots: &mut Slots, base: &VaultBase) {
    for d in &base.deposits {
        slots.insert((d.deposit.depositor.clone(), d.deposit.name.clone()), d.clone());
    }
}

/// Fold a vault group: `base` is the base this node holds for a commitment
/// the group ALREADY carries (`None` when it holds none). Deposits are
/// last-wins per slot; a grant survives only on the version current at its
/// commit and still current at the cut (D16, plan 1.3.10).
///
/// # Errors
/// The group commits to a base this node does not hold, carries two
/// commitments, or holds an entry outside the closed op set.
pub(crate) fn summarize(
    group: &[(u64, Value)],
    held: Option<&VaultBase>,
    republic_id: &str,
) -> Result<(Vec<(u64, Value)>, VaultBase), String> {
    let mut slots = Slots::new();
    let mut seeded = false;
    for (id, payload) in group {
        if let Some((want, _)) = base_commitment_of(payload) {
            if seeded {
                return Err("the vault group carries two base commitments".to_string());
            }
            seeded = true;
            let Some(base) = held.filter(|b| commitment(b).0 == want) else {
                return Err(format!("the vault base this node holds is not the committed one ({want})"));
            };
            seed(&mut slots, base);
            continue;
        }
        match serde_json::from_value::<VaultOp>(payload.clone()) {
            Ok(VaultOp::Deposit(dep)) => {
                let slot = (dep.depositor.clone(), dep.name.clone());
                // the projection's void rule (`VaultState::extends`)
                let current = slots.get(&slot).map(|d| secret_id(republic_id, &d.deposit)).unwrap_or_default();
                if current == dep.replaces {
                    slots.insert(slot, VaultBaseDeposit { deposit: dep, grants: Vec::new() });
                }
            }
            Ok(VaultOp::Grant(grant)) => {
                let current = slots
                    .values_mut()
                    .find(|d| secret_id(republic_id, &d.deposit) == grant.secret_id);
                if let Some(d) = current {
                    d.grants.push(VaultBaseGrant { proposal_id: *id, grant });
                }
            }
            _ => return Err("a vault entry outside the closed op set".to_string()),
        }
    }
    let base = VaultBase { deposits: slots.into_values().collect() };
    let (hash, size) = commitment(&base);
    let entry = serde_json::to_value(VaultOp::VaultBase(VaultPayloadRef { hash, size }))
        .map_err(|e| format!("vault base entry: {e}"))?;
    Ok((vec![(BASE_ENTRY_ID, entry)], base))
}

/// Fold the vault group of a checkpoint state in place.
///
/// # Errors
/// See [`summarize`]; a state without a vault group is malformed here.
pub(crate) fn summarize_state(
    state: &mut molt_core::CheckpointState,
    held: Option<&VaultBase>,
) -> Result<VaultBase, String> {
    let rid = state.republic_id.clone();
    let Some((_, group)) = state.applied.iter_mut().find(|(s, _)| *s == molt_core::Surface::Vault) else {
        return Err("the state has no vault group".to_string());
    };
    let (folded, base) = summarize(group, held, &rid)?;
    *group = folded;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::vault::{VaultDeposit, VaultGrant};
    use serde_json::json;

    fn dep(depositor: &str, name: &str, hash: char) -> VaultDeposit {
        VaultDeposit {
            depositor: depositor.to_string(),
            name: name.to_string(),
            kind: "text".to_string(),
            m: 2,
            payload: VaultPayloadRef { hash: std::iter::repeat_n(hash, 64).collect(), size: 3 },
            ..VaultDeposit::default()
        }
    }

    fn op(o: VaultOp) -> Value {
        serde_json::to_value(o).expect("op")
    }

    fn grant(d: &VaultDeposit, reader: &str, id: u64) -> (u64, Value) {
        let sid = secret_id("r", d);
        let g = VaultGrant { grant_id: molt_core::vault::grant_id(&sid, reader, id), secret_id: sid, reader: reader.to_string() };
        (id, op(VaultOp::Grant(g)))
    }

    /// D16: a replace drops the old version and every grant on it; a grant
    /// on the current version survives.
    #[test]
    fn summarize_drops_replaced_versions_and_their_grants() {
        let v1 = dep("a", "x", '1');
        let v2 = VaultDeposit { replaces: secret_id("r", &v1), ..dep("a", "x", '2') };
        let other = dep("b", "y", '3');
        let group = vec![
            (1, op(VaultOp::Deposit(v1.clone()))),
            grant(&v1, "c", 2),
            (3, op(VaultOp::Deposit(other.clone()))),
            (4, op(VaultOp::Deposit(v2.clone()))),
            grant(&v2, "d", 5),
        ];
        let (folded, base) = summarize(&group, None, "r").expect("folds");
        assert_eq!(folded.len(), 1, "one entry");
        assert_eq!(folded[0].0, 0);
        assert_eq!(base_commitment_of(&folded[0].1), Some(commitment(&base)));
        assert_eq!(base.deposits.len(), 2);
        let x = base.deposits.iter().find(|d| d.deposit.name == "x").expect("x");
        assert_eq!(x.deposit, v2, "the current version");
        assert_eq!(x.grants.len(), 1);
        assert_eq!(x.grants[0].grant.reader, "d");
        assert_eq!(x.grants[0].proposal_id, 5);
        let y = base.deposits.iter().find(|d| d.deposit.name == "y").expect("y");
        assert!(y.grants.is_empty());
    }

    /// A raw-chain holder and a holder folding onto its base agree.
    #[test]
    fn folding_from_the_genesis_and_from_a_held_base_agree() {
        let v1 = dep("a", "x", '1');
        let v2 = VaultDeposit { replaces: secret_id("r", &v1), ..dep("a", "x", '2') };
        let all = vec![(1, op(VaultOp::Deposit(v1.clone()))), grant(&v1, "c", 2), (3, op(VaultOp::Deposit(v2.clone())))];
        let (full, full_base) = summarize(&all, None, "r").expect("full");
        let (first, base) = summarize(&all[..2], None, "r").expect("first cut");
        let mut group = first;
        group.push(all[2].clone());
        assert!(summarize(&group, None, "r").is_err(), "the base is needed");
        let (suffix, suffix_base) = summarize(&group, Some(&base), "r").expect("onto the base");
        assert_eq!(full, suffix);
        assert_eq!(full_base, suffix_base);
    }

    /// An empty vault still folds to one entry; a foreign op refuses.
    #[test]
    fn an_empty_group_folds_to_the_empty_base_and_foreign_ops_refuse() {
        let (folded, base) = summarize(&[], None, "r").expect("folds");
        assert_eq!(base, VaultBase::default());
        assert_eq!(base_commitment_of(&folded[0].1), Some(commitment(&VaultBase::default())));
        assert!(summarize(&[(1, json!({ "op": "seal_secret" }))], None, "r").is_err());
        let twice = vec![folded[0].clone(), folded[0].clone()];
        assert!(summarize(&twice, Some(&base), "r").is_err(), "one cut, one base");
    }
}
