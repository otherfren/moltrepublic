// SPDX-License-Identifier: GPL-3.0-or-later

//! What the branch choice shows (`docs/chain/chain_rescue_plan.md` §8):
//! the forks this node can prove, the open choice cards, the own choices
//! and the abandoned state. Display data, never consensus input.

use serde::{Deserialize, Serialize};

use crate::{ChainChange, MemberId};

/// One block of a branch, as the status and a card's dropped list show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchBlockRow {
    /// The block height.
    pub height: u64,
    /// Its link hash.
    pub hash: String,
    /// `applied` | `membership` | `checkpoint` | `branch`.
    pub kind: String,
    /// The surface of an applied block ("" otherwise).
    #[serde(default)]
    pub surface: String,
    /// The payload's `op`, or the membership op ("" when none).
    #[serde(default)]
    pub op: String,
    /// The applied proposal id (0 = none).
    #[serde(default)]
    pub proposal_id: u64,
    /// The signers, in block order.
    #[serde(default)]
    pub signers: Vec<MemberId>,
    /// "" | `flagged` (an effect outside the chain may remain) |
    /// `irreversible` (it cannot be undone).
    #[serde(default)]
    pub risk: String,
}

/// One branch above a fork point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchView {
    /// The branch's block at the fork point plus one.
    pub root_hash: String,
    /// Its highest block this node holds.
    pub head_height: u64,
    /// That block's link hash.
    pub head_hash: String,
    /// This node's own chain.
    pub own: bool,
    /// The blocks above the fork point.
    pub blocks: Vec<BranchBlockRow>,
    /// Blocks per kind (`applied:memory`, `membership:restored`, ...).
    pub counts: std::collections::BTreeMap<String, u32>,
    /// Rows flagged `irreversible`.
    pub irreversible: u32,
    /// Every seat that signed a block of the branch.
    pub signers: Vec<MemberId>,
}

/// A fork this node can prove: its own chain and at least one verified
/// other branch from the same block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkView {
    /// The last block both branches share.
    pub base_height: u64,
    /// Its link hash.
    pub base_hash: String,
    /// The branches, the own one first.
    pub branches: Vec<BranchView>,
    /// Seats whose signature is on more than one branch.
    pub shared: Vec<MemberId>,
}

/// An open branch choice card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCardView {
    /// The card id.
    pub id: u64,
    /// The proposer.
    pub by: MemberId,
    /// The change it seals.
    pub change: ChainChange,
    /// Seats whose signature verified here.
    pub signers: Vec<MemberId>,
    /// Signatures it needs (m).
    pub need: u8,
    /// What it drops, as this node computes it (empty when unknown here).
    pub dropped: Vec<BranchBlockRow>,
    /// The digest an approval names ("" = the dropped blocks are not held here).
    pub digest: String,
    /// This seat signed it.
    pub approved_by_me: bool,
    /// The own chain moved above the tip: the card drops a block nobody
    /// saw; a new card is needed.
    #[serde(default)]
    pub stale: bool,
}

/// This seat's recorded choice at one fork point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnBranchChoice {
    /// The fork point.
    pub base_height: u64,
    /// The tip this seat signed for.
    pub tip_hash: String,
}

/// The chain chose a branch this node is not on: it stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchAbandoned {
    /// The fork point of the choice.
    pub base_height: u64,
    /// The seats that signed it.
    pub by: Vec<MemberId>,
    /// The choice block's height.
    pub height: u64,
}

/// The evidence exchange with one seat (`status.seats[].evidence`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatEvidence {
    /// What this seat sends: "" | `offered` | `sending` | `sent` | `refused`.
    #[serde(default)]
    pub out: String,
    /// Parts sent so far.
    #[serde(default)]
    pub out_parts: u32,
    /// Parts in the offer.
    #[serde(default)]
    pub out_total: u32,
    /// What arrived: "" | `receiving` | `ready` | `adopted` | `refused`.
    #[serde(default, rename = "in")]
    pub inbound: String,
    /// Parts received.
    #[serde(default)]
    pub in_parts: u32,
    /// Parts expected.
    #[serde(default)]
    pub in_total: u32,
    /// New blocks in what arrived.
    #[serde(default)]
    pub blocks: u32,
    /// Its first new height.
    #[serde(default)]
    pub from_height: u64,
    /// `extends` (this chain, a catch-up) | `other-branch` (a fork).
    #[serde(default)]
    pub kind: String,
    /// Applied blocks of this chain the other branch does not hold.
    #[serde(default)]
    pub drops: u32,
    /// Why it was refused ("" = it was not).
    #[serde(default)]
    pub reason: String,
}

impl SeatEvidence {
    /// Nothing exchanged with this seat.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        *self == Self::default()
    }
}

/// `branch_status`: everything a seat needs to decide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStatusView {
    /// The threshold m.
    pub rule_m: u8,
    /// The seats n.
    pub rule_n: u8,
    /// 2m > n: a branch choice is offered.
    pub majority: bool,
    /// The forks this node can prove.
    pub forks: Vec<ForkView>,
    /// Open choice cards.
    pub cards: Vec<BranchCardView>,
    /// This seat's recorded choices.
    pub choices: Vec<OwnBranchChoice>,
    /// The height this seat's current device was restored at (0 = never).
    pub incarnation: u64,
    /// Set once the chain chose another branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<BranchAbandoned>,
}
