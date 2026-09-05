// SPDX-License-Identifier: GPL-3.0-or-later

//! **The shared memory as ONE commitment** (`knowledge_base_scale.md` §4.9).
//!
//! A compaction cut FOLDS the wiki: the memory group's ratified patches
//! collapse into the tree they produce, and the checkpointed state carries
//! that tree's content hash instead of the patches. History stops
//! accumulating in every later blob; the tree itself travels on the file
//! plane, not inside the trust root a rejoiner is handed.
//!
//! The fold is deterministic and self-verifying: a node folding from the
//! genesis and a node folding onto a fetched base reach the same tree, so
//! they reach the same commitment - which is what makes a folded cut
//! signable at all (sign-what-you-see: every signer recomputes it).

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// K6 lands in stages, and the fold is only safe once a holder KEEPS the
/// folded tree locally and can fetch a missing one: without that a cut
/// would drop the patches and leave the wiki empty. Accepting a folded cut
/// works from the start; PROPOSING one waits for the base store.
pub(crate) const FOLD_CUTS: bool = false;

/// A memory group in its folded form, and the tree it commits to.
type Folded = (Vec<(u64, Value)>, BTreeMap<String, String>);

/// The op a folded cut writes in place of the memory group's patches.
pub(crate) const WIKI_BASE_OP: &str = "wiki_base";

/// The synthetic entry's proposal id. `next_id` starts at 1 on every node,
/// so 0 names no real proposal; `consumed_ids` is untouched by the fold, so
/// nothing stops being consumed.
const BASE_ENTRY_ID: u64 = 0;

/// Content-address a tree the way a folded cut commits to it: the hash the
/// chain carries, and the byte length that bounds a fetch.
pub(crate) fn commitment(tree: &BTreeMap<String, String>) -> (String, u64) {
    let bytes = molt_core::wiki_fold::wiki_base_canonical_bytes(tree);
    let size = u64::try_from(bytes.len())
        .expect("field exceeds the u32/u64 framing - ambiguous signed bytes are never written");
    (molt_storage::content_hash(&bytes), size)
}

/// The commitment an applied Memory payload carries, if it is a folded
/// cut's base entry.
pub(crate) fn base_commitment_of(payload: &Value) -> Option<(String, u64)> {
    (payload.get("op").and_then(Value::as_str) == Some(WIKI_BASE_OP)).then(|| {
        (
            payload
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            payload.get("size").and_then(Value::as_u64).unwrap_or_default(),
        )
    })
}

/// Fold a memory group into its commitment form: one `wiki_base` entry
/// first, every non-wiki entry after it in order. `base` is the tree this
/// node holds for a commitment the group ALREADY carries (empty when it
/// carries none) - a suffix holder folds onto what it fetched, a full
/// holder folds from nothing, and both reach the same tree.
///
/// Returns the new group and the tree it commits to.
///
/// # Errors
/// The group commits to a base this node does not hold, or carries two
/// commitments (a forged group - one cut, one base).
pub(crate) fn summarize(
    group: &[(u64, Value)],
    base: &BTreeMap<String, String>,
) -> Result<Folded, String> {
    let mut tree = BTreeMap::new();
    let mut kept: Vec<(u64, Value)> = Vec::new();
    let mut seeded = false;
    for (id, payload) in group {
        match payload.get("op").and_then(Value::as_str) {
            Some(WIKI_BASE_OP) => {
                if seeded {
                    return Err("the memory group carries two base commitments".to_string());
                }
                let want = payload.get("hash").and_then(Value::as_str).unwrap_or_default();
                let (have, _) = commitment(base);
                if have != want {
                    return Err(format!(
                        "the shared memory base this node holds ({have}) is not the committed one ({want})"
                    ));
                }
                tree.clone_from(base);
                seeded = true;
            }
            // the patches this cut folds away — their ids stay consumed
            Some("wiki_patch") => {
                molt_core::wiki_fold::fold_one(&mut tree, payload);
            }
            _ => kept.push((*id, payload.clone())),
        }
    }
    let (hash, size) = commitment(&tree);
    let mut out = Vec::with_capacity(kept.len() + 1);
    out.push((
        BASE_ENTRY_ID,
        json!({ "op": WIKI_BASE_OP, "hash": hash, "size": size }),
    ));
    out.extend(kept);
    Ok((out, tree))
}

/// Summarize the memory group of a checkpoint state in place (the three
/// hash sites all go through here, so they cannot drift apart). A state
/// with no memory group is left alone: a republic that never wrote a wiki
/// has nothing to fold, and must keep hashing as it always did.
pub(crate) fn summarize_state(
    state: &mut molt_core::CheckpointState,
    base: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let Some((_, group)) = state
        .applied
        .iter_mut()
        .find(|(s, _)| *s == molt_core::Surface::Memory)
    else {
        return Ok(BTreeMap::new());
    };
    let (folded, tree) = summarize(group, base)?;
    *group = folded;
    Ok(tree)
}

/// Is there anything to fold? A cut proposes the folded variant only when
/// the memory group actually carries wiki content - otherwise it stays on
/// the legacy layout and every republic without a wiki keeps its bytes.
pub(crate) fn worth_folding(state: &molt_core::CheckpointState) -> bool {
    state
        .applied
        .iter()
        .find(|(s, _)| *s == molt_core::Surface::Memory)
        .is_some_and(|(_, g)| {
            g.iter().any(|(_, p)| {
                matches!(
                    p.get("op").and_then(Value::as_str),
                    Some("wiki_patch" | WIKI_BASE_OP)
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(path: &str, body: &str) -> Value {
        let lines: Vec<&str> = body.split('\n').collect();
        let mut p = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for l in lines {
            p.push('+');
            p.push_str(l);
            p.push('\n');
        }
        json!({ "op": "wiki_patch", "value": p })
    }

    /// The fold is the point: many patches in, one commitment out, and the
    /// entries that are not wiki survive untouched and in order.
    #[test]
    fn a_folded_group_is_one_commitment_plus_what_is_not_wiki() {
        let group = vec![
            (1, patch("a.md", "A")),
            (2, json!({ "op": "add_note", "title": "keep me" })),
            (3, patch("b.md", "B")),
        ];
        let (folded, tree) = summarize(&group, &BTreeMap::new()).expect("folds");
        assert_eq!(tree.len(), 2, "both documents are in the tree");
        assert_eq!(folded.len(), 2, "one commitment, one kept entry");
        assert_eq!(folded[0].0, 0);
        assert_eq!(
            folded[0].1.get("op").and_then(Value::as_str),
            Some(WIKI_BASE_OP)
        );
        assert_eq!(folded[1], group[1], "a non-wiki entry is not touched");
        let (hash, size) = commitment(&tree);
        assert_eq!(folded[0].1.get("hash").and_then(Value::as_str), Some(hash.as_str()));
        assert_eq!(folded[0].1.get("size").and_then(Value::as_u64), Some(size));
    }

    /// A full holder (folds from the genesis) and a suffix holder (folds
    /// onto the base it fetched) MUST reach the same commitment - if they
    /// did not, the two could never sign the same cut.
    #[test]
    fn folding_from_the_genesis_and_from_a_fetched_base_agree() {
        let all = vec![(1, patch("a.md", "A")), (2, patch("b.md", "B"))];
        let (full, full_tree) = summarize(&all, &BTreeMap::new()).expect("folds");

        // the suffix holder: cut after the first patch, then the second
        let (first_cut, base) = summarize(&all[..1], &BTreeMap::new()).expect("folds");
        let mut group = first_cut;
        group.push((2, patch("b.md", "B")));
        let (suffix, suffix_tree) = summarize(&group, &base).expect("folds onto the fetched base");
        assert_eq!(full_tree, suffix_tree);
        assert_eq!(full, suffix);
    }

    /// A node that does not hold the committed base cannot fold - and says
    /// so, rather than committing to a tree it invented.
    #[test]
    fn a_missing_base_refuses_the_fold() {
        let (cut, base) = summarize(&[(1, patch("a.md", "A"))], &BTreeMap::new()).expect("folds");
        assert!(summarize(&cut, &BTreeMap::new()).is_err(), "empty is not the base");
        assert!(summarize(&cut, &base).is_ok());
        let twice: Vec<(u64, Value)> = vec![cut[0].clone(), cut[0].clone()];
        assert!(summarize(&twice, &base).is_err(), "one cut, one base");
    }

    /// Nothing to fold, nothing folded: a republic with no wiki keeps the
    /// legacy layout, so its cuts stay byte-identical.
    #[test]
    fn a_republic_without_a_wiki_is_not_worth_folding() {
        let mut s = molt_core::CheckpointState {
            founding_name: "Chess Club".to_string(),
            rule_m: 2,
            rule_n: 2,
            founding_identities: Vec::new(),
            agenda: String::new(),
            relays: Vec::new(),
            founding_features: None,
            republic_id: "f00".to_string(),
            roster: Vec::new(),
            applied: Vec::new(),
            consumed_ids: Vec::new(),
            anchors: Vec::new(),
            member_relays: Vec::new(),
            upto: 0,
        };
        assert!(!worth_folding(&s));
        s.applied = vec![(
            molt_core::Surface::Memory,
            vec![(1, json!({ "op": "add_note" }))],
        )];
        assert!(!worth_folding(&s));
        s.applied = vec![(molt_core::Surface::Memory, vec![(1, patch("a.md", "A"))])];
        assert!(worth_folding(&s));
    }
}
