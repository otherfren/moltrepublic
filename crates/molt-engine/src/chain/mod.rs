// SPDX-License-Identifier: GPL-3.0-or-later

//! **The persistent commit-block chain on the engine side** — the
//! threshold-signed single-branch state model of
//! `docs_archive/chain/persistent_chain.md`, split by responsibility:
//!
//! - [`verify`] — pure verification, no `State`: the genesis/next-block
//!   checks, the cached [`ChainWalk`], the checkpoint fold and the served /
//!   suffix / wiki-export verifiers;
//! - the holder's projection of a verified chain into `State`, the live
//!   threshold governance, membership (recovery re-admission + re-key),
//!   checkpoints (compaction) and catch-up sync live in the sibling
//!   modules below, each as `impl State` blocks.
//!
//! Everything a caller outside this module needs is re-exported here under
//! `crate::chain::…`; the sibling modules share ONE namespace through
//! `use super::*`, so an item's file is a matter of reading order, never of
//! reachability.

use std::collections::BTreeSet;

use molt_core::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, Event, MemberIdentity, MembershipOp,
    ProposalId, ProposalState, RosterAttestation, SealedRoster, Surface, WorkspaceEvent,
    GENESIS_PREV,
};

use crate::State;

mod checkpoint;
mod governance;
mod membership;
mod projection;
mod sync;
pub(crate) mod vault_base;
mod verify;
mod wiki_base;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod governance_tests;
#[cfg(test)]
mod membership_tests;
#[cfg(test)]
mod projection_tests;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod verify_tests;
#[cfg(test)]
mod vault_founding_tests;

pub use verify::{verify_chain, verify_wiki_export, ChainHead, WikiExportReport};
pub(crate) use verify::{
    checkpoint_state, checkpoint_state_hash, effective_relays_of_served, verify_served,
    verify_suffix_chain, working_anchors, ChainWalk, ServedChainWire,
};
#[cfg(test)]
pub(crate) use verify::block_hash;
#[cfg(test)]
pub(crate) use verify::walk_suffix_chain;
/// The ratified wiki as the fold produces it: path -> document.
pub(crate) type WikiTree = std::collections::BTreeMap<String, String>;

/// The folded bases a holder keeps RIGHT NOW - passed to every fold, never
/// cached in a walk (the §4.9.5 lesson, D9).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Held<'a> {
    /// The ratified wiki tree behind a wiki commitment.
    pub(crate) wiki: Option<&'a WikiTree>,
    /// The vault base behind a vault commitment.
    pub(crate) vault: Option<&'a molt_core::vault::VaultBase>,
}

impl<'a> Held<'a> {
    /// Only a wiki tree.
    pub(crate) fn wiki(wiki: Option<&'a WikiTree>) -> Self {
        Self { wiki, vault: None }
    }
}

/// Which groups a cut folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CutKind {
    /// The memory group (`CheckpointFolded`, or `CheckpointVault` with
    /// `wiki_folded`).
    pub(crate) wiki: bool,
    /// The vault group (`CheckpointVault`).
    pub(crate) vault: bool,
}

impl CutKind {
    /// The cut a change commits to, `None` for a non-cut.
    pub(crate) fn of(change: &ChainChange) -> Option<(u64, &str, Self)> {
        match change {
            ChainChange::Checkpoint { upto, state_hash } => Some((*upto, state_hash, Self::default())),
            ChainChange::CheckpointFolded { upto, state_hash } => {
                Some((*upto, state_hash, Self { wiki: true, vault: false }))
            }
            ChainChange::CheckpointVault { upto, state_hash, wiki_folded } => {
                Some((*upto, state_hash, Self { wiki: *wiki_folded, vault: true }))
            }
            _ => None,
        }
    }

    /// The change a cut of this kind is.
    pub(crate) fn change(self, upto: u64, state_hash: String) -> ChainChange {
        match (self.vault, self.wiki) {
            (true, wiki_folded) => ChainChange::CheckpointVault { upto, state_hash, wiki_folded },
            (false, true) => ChainChange::CheckpointFolded { upto, state_hash },
            (false, false) => ChainChange::Checkpoint { upto, state_hash },
        }
    }
}

/// What a cut fold yields: the wiki tree and the vault base a holder must
/// keep once the folded entries are dropped, and the state hash.
#[derive(Debug, Clone)]
pub(crate) struct FoldedCut {
    pub(crate) tree: Option<WikiTree>,
    pub(crate) vault: Option<molt_core::vault::VaultBase>,
    pub(crate) hash: String,
}

pub(crate) use wiki_base::{base_commitment_of, commitment as wiki_base_commitment, rev_at_cut_of};

/// Whether `payload` wears a cut's base-entry op - written only by the fold,
/// never a proposal.
pub(crate) fn is_base_entry_op(payload: &serde_json::Value) -> bool {
    matches!(
        payload.get("op").and_then(serde_json::Value::as_str),
        Some(wiki_base::WIKI_BASE_OP | vault_base::VAULT_BASE_OP)
    )
}
pub(crate) use governance::PendingApproval;
pub(crate) use membership::{NostrRekey, PendingRecovery, RecoverProgressReport};

use verify::*;

#[cfg(test)]
thread_local! {
    /// Test-only count of per-block verifications. **Thread-local on purpose**:
    /// each test runs on its own thread, so a shared counter would be raced by
    /// every other chain test in the binary.
    ///
    /// This is the only way to state the complexity claim as an assertion
    /// rather than a timing: a holder that re-walks its chain per block shows
    /// up here as `N²`, not `N`.
    pub(crate) static VERIFY_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Test-only count of whole-chain writes. Same reason as `VERIFY_STEPS`:
    /// the write is a BLOCKING round-trip to the storage writer, so "once per
    /// batch" is a claim worth asserting rather than describing.
    pub(crate) static CHAIN_PERSISTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
