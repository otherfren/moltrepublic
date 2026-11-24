// SPDX-License-Identifier: GPL-3.0-or-later

//! **The branch choice** (`docs/chain/chain_rescue_plan.md`): m members
//! vote which branch of a lasting fork stays the chain. The vote is the
//! next block on the chosen branch, [`ChainChange::BranchChosen`].

use super::*;

pub(crate) use super::branch_run::ABANDONED;

/// The most runs one choice may drop.
pub(crate) const DROPPED_RUNS_MAX: usize = 16;

/// §6: what a seat the chain left behind refuses until it rejoins.
pub(crate) fn stopped_when_abandoned(cmd: &molt_core::Command) -> bool {
    use molt_core::Command;
    matches!(
        cmd,
        Command::Propose { .. }
            | Command::Approve { .. }
            | Command::Decline { .. }
            | Command::Withdraw { .. }
            | Command::ProposeCheckpoint
            | Command::RecoverInviteStart { .. }
            | Command::ChooseBranch { .. }
            | Command::ApproveBranch { .. }
    )
}

/// The block-local rules of a branch choice (§1): it sits right above its
/// tip, the tip is at or above the base, and it drops a second branch
/// from the base (Q1: no reset without one).
pub(super) fn check_shape(block: &ChainBlock) -> Result<(), String> {
    let ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, chosen_root, dropped } =
        &block.change
    else {
        return Ok(());
    };
    let h = block.height;
    if block.height.checked_sub(1) != Some(*tip_height) {
        return Err(format!("branch choice tip {tip_height} must be its block height {h} minus one"));
    }
    if *tip_hash != block.prev {
        return Err(format!("branch choice at {h} names a tip that is not its predecessor"));
    }
    if base_height > tip_height {
        return Err(format!("branch choice base {base_height} lies above its tip {tip_height}"));
    }
    let at_base = base_height == tip_height;
    if at_base != chosen_root.is_empty() || (at_base && base_hash != tip_hash) {
        return Err(format!("branch choice at {h} has a root that does not match its tip"));
    }
    if dropped.is_empty() || dropped.len() > DROPPED_RUNS_MAX {
        return Err(format!("branch choice at {h} drops {} runs", dropped.len()));
    }
    let fork = base_height + 1;
    let mut roots = BTreeSet::new();
    for run in dropped {
        let placed = run.from_height == fork || (!at_base && run.from_height == tip_height + 1);
        if !placed || run.head_height < run.from_height {
            return Err(format!("branch choice at {h} drops a run that is not a branch"));
        }
        if run.from_height == fork {
            if run.first_hash == *chosen_root {
                return Err(format!("branch choice at {h} drops its own root"));
            }
            roots.insert(run.first_hash.as_str());
        }
    }
    // a second branch: one other root, or two when both are dropped
    let need = if at_base { 2 } else { 1 };
    if roots.len() < need {
        return Err(format!("branch choice at {h} has no second branch"));
    }
    Ok(())
}

/// The walk half (§3): the base and the chosen root are blocks of this
/// chain, at or above its anchor. `links` holds the link hash per height
/// from the anchor to the block's predecessor.
pub(super) fn check_on_walk(links: &[String], block: &ChainBlock) -> Result<(), String> {
    let ChainChange::BranchChosen { base_height, base_hash, tip_height, chosen_root, .. } = &block.change
    else {
        return Ok(());
    };
    let anchor = block.height.saturating_sub(u64::try_from(links.len()).unwrap_or(u64::MAX));
    if *base_height < anchor {
        return Err(format!("branch choice base {base_height} lies below the anchor {anchor}"));
    }
    let at = |height: u64| usize::try_from(height - anchor).ok().and_then(|i| links.get(i));
    if at(*base_height) != Some(base_hash) {
        return Err(format!("branch choice base {base_height} is not on this chain"));
    }
    if base_height < tip_height && at(base_height + 1) != Some(chosen_root) {
        return Err(format!("branch choice root at {} is not on this chain", base_height + 1));
    }
    Ok(())
}
