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
mod verify;
mod wiki_base;

#[cfg(test)]
mod test_support;
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

pub use verify::{verify_chain, verify_wiki_export, ChainHead, WikiExportReport};
pub(crate) use verify::{
    checkpoint_state, checkpoint_state_hash, effective_relays_of_served, verify_served,
    verify_suffix_chain, working_anchors, ChainWalk, ServedChainWire,
};
#[cfg(test)]
pub(crate) use verify::block_hash;
/// The ratified wiki as the fold produces it: path -> document.
pub(crate) type WikiTree = std::collections::BTreeMap<String, String>;

pub(crate) use wiki_base::{base_commitment_of, commitment as wiki_base_commitment};
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
