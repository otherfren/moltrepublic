// SPDX-License-Identifier: GPL-3.0-or-later

//! The branch choice on the open workspace (`docs/chain/chain_rescue_plan.md`
//! §2-§8): the evidence of a second branch, the bundle, the card, the
//! seat's one durable choice per fork point, the seal and its adoption,
//! and the stop of a seat the choice leaves behind. Nothing here acts by
//! itself: a human (or MCP) proposes, approves, exports and imports.

use std::collections::BTreeMap;

use molt_core::branch::{
    BranchAbandoned, BranchBlockRow, BranchCardView, BranchStatusView, BranchView, ForkView, OwnBranchChoice,
};
use molt_core::{MemberId, MoltError, Reply};
use serde::{Deserialize, Serialize};

use super::branch::check_shape;
use super::*;

/// Open choice cards held at once.
const CARDS_MAX: usize = 16;
/// Other branches held as evidence.
const EVIDENCE_MAX: usize = 16;
/// Signatures that arrived before their card.
const PARKED_MAX: usize = 64;
/// The bundle's format tag.
const BUNDLE_FORMAT: &str = "molt-branch-bundle-v1";

/// Refusals a human reads (one fact each).
pub(crate) const ABANDONED: &str = "branch abandoned";
pub(crate) const NO_SECOND_BRANCH: &str = "no second branch";
pub(crate) const BELOW_CUT: &str = "below a cut";
pub(crate) const AMNESIAC: &str = "amnesiac: this device was restored above the fork point";
pub(crate) const DIGEST_DIFFERS: &str = "dropped list differs";
pub(crate) const DROPPED_UNKNOWN: &str = "dropped blocks not held here";

/// One open choice card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BranchCard {
    pub(crate) change: ChainChange,
    pub(crate) by: MemberId,
    /// Verified signatures, by signer.
    #[serde(default)]
    pub(crate) sigs: BTreeMap<MemberId, String>,
}

/// This seat's one choice at a fork point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Choice {
    pub(crate) base_hash: String,
    pub(crate) tip_hash: String,
}

/// `branch.state`: written blocking before an own signature leaves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BranchFile {
    #[serde(default)]
    pub(crate) choices: BTreeMap<u64, Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) abandoned: Option<BranchAbandoned>,
    /// Verified blocks of other branches, keyed by their first block's hash.
    #[serde(default)]
    pub(crate) evidence: BTreeMap<String, Vec<ChainBlock>>,
    #[serde(default)]
    pub(crate) cards: BTreeMap<u64, BranchCard>,
}

/// The branch choice's runtime on the open workspace.
#[derive(Debug, Default)]
pub(crate) struct BranchRuntime {
    pub(crate) file: BranchFile,
    pub(crate) parked: Vec<(u64, MemberId, String)>,
    #[cfg(test)]
    pub(crate) disk: Option<Vec<u8>>,
    #[cfg(test)]
    pub(crate) fail_writes: bool,
}

/// What a bundle file carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Bundle {
    pub(crate) format: String,
    pub(crate) republic_id: String,
    pub(crate) from: MemberId,
    /// Runs of blocks: the exporter's chain above its fork points, then
    /// the other branches it holds.
    pub(crate) runs: Vec<Vec<ChainBlock>>,
    pub(crate) cards: Vec<(u64, BranchCard)>,
}

/// One block as a status row.
pub(crate) fn block_row(hash: String, block: &ChainBlock) -> BranchBlockRow {
    let (kind, surface, op, proposal_id) = match &block.change {
        ChainChange::Applied { proposal_id, surface, payload } => (
            "applied",
            surface.as_str().to_string(),
            payload.get("op").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
            *proposal_id,
        ),
        ChainChange::Membership { op, .. } => (
            "membership",
            String::new(),
            match op {
                MembershipOp::Joined => "joined",
                MembershipOp::Restored => "restored",
            }
            .to_string(),
            0,
        ),
        ChainChange::BranchChosen { .. } => ("branch", String::new(), String::new(), 0),
        ChainChange::Genesis { .. } => ("genesis", String::new(), String::new(), 0),
        _ => ("checkpoint", String::new(), String::new(), 0),
    };
    BranchBlockRow {
        height: block.height,
        hash,
        kind: kind.to_string(),
        surface,
        op,
        proposal_id,
        signers: block.sigs.iter().map(|a| a.member.clone()).collect(),
        risk: risk_of(&block.change).to_string(),
    }
}

/// §4: what dropping a block cannot undo.
fn risk_of(change: &ChainChange) -> &'static str {
    let ChainChange::Applied { surface, payload, .. } = change else {
        return "";
    };
    let op = payload.get("op").and_then(serde_json::Value::as_str).unwrap_or_default();
    match surface {
        Surface::Vault if op == "grant" => "irreversible",
        Surface::Wallet if op == crate::wallet::WALLET_INIT => "irreversible",
        Surface::Vault | Surface::Wallet | Surface::Files => "flagged",
        Surface::Organization if op == "set_features" => "flagged",
        _ => "",
    }
}

/// The digest an approval names: the change at its height plus the
/// dropped rows.
pub(crate) fn dropped_digest(rid: &str, change: &ChainChange, rows: &[BranchBlockRow]) -> String {
    let height = match change {
        ChainChange::BranchChosen { tip_height, .. } => tip_height + 1,
        _ => 0,
    };
    let mut bytes = approval_bytes(rid, height, change);
    bytes.extend_from_slice(&serde_json::to_vec(rows).unwrap_or_default());
    molt_storage::content_hash(&bytes)
}

fn err(why: impl Into<String>) -> MoltError {
    MoltError::Branch(why.into())
}

impl State {
    /// Whether the chain chose another branch here.
    pub(crate) fn branch_abandoned(&self) -> bool {
        self.chain.branch.file.abandoned.is_some()
    }

    /// Adopt `branch.state` at open; an unreadable file counts as none.
    pub(crate) fn load_branch_state(&mut self, stored: Result<Option<Vec<u8>>, String>) {
        self.chain.branch.parked.clear();
        self.chain.branch.file = match stored {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "branch=unreadable");
                BranchFile::default()
            }),
            Ok(None) => BranchFile::default(),
            Err(e) => {
                tracing::warn!(error = %e, "branch=unreadable");
                BranchFile::default()
            }
        };
    }

    /// Write `branch.state`; `false` = it did not reach the disk.
    pub(crate) fn persist_branch(&mut self) -> bool {
        let Ok(bytes) = serde_json::to_vec(&self.chain.branch.file) else {
            return false;
        };
        #[cfg(test)]
        {
            if self.chain.branch.fail_writes {
                tracing::error!("branch=unwritten");
                return false;
            }
            self.chain.branch.disk = Some(bytes.clone());
        }
        let durable = self.active.as_ref().map_or(true, |a| a.handle.persist_branch_blocking(bytes));
        if !durable {
            tracing::error!("branch=unwritten");
        }
        durable
    }

    /// The height this device's incarnation began at: its last own
    /// `Restored` block (0 = the founding device).
    pub(crate) fn incarnation_height(&self) -> u64 {
        let me = self.member();
        self.chain
            .blocks
            .iter()
            .filter(|b| matches!(&b.change, ChainChange::Membership { op: MembershipOp::Restored, member, .. } if *member == me))
            .map(|b| b.height)
            .max()
            .unwrap_or(0)
    }

    fn own_hash(&self, height: u64) -> Option<String> {
        let first = self.chain.blocks.first()?.height;
        let i = usize::try_from(height.checked_sub(first)?).ok()?;
        self.chain.blocks.get(i).filter(|b| b.height == height).map(|b| self.block_id_of(b))
    }

    fn majority(&self) -> (bool, u8, usize) {
        let m = self.chain.head.as_ref().map_or(0, |h| h.rule_m);
        let n = self.chain.head.as_ref().map_or(0, |h| h.identities.len());
        let seats = u8::try_from(n).unwrap_or(u8::MAX);
        (molt_core::majority::threshold_is_majority(m, seats), m, n)
    }

    fn branch_gate(&self) -> Result<(), MoltError> {
        if self.chain.head.is_none() {
            return Err(err("no chain yet"));
        }
        if self.branch_abandoned() {
            return Err(err(ABANDONED));
        }
        let (ok, m, n) = self.majority();
        if ok {
            Ok(())
        } else {
            Err(err(format!("needs a majority of the members: {m} of {n} is not one")))
        }
    }

    fn amnesia_gate(&self, base_height: u64, ack: bool) -> Result<(), MoltError> {
        if !ack && self.incarnation_height() > base_height {
            return Err(err(AMNESIAC));
        }
        Ok(())
    }

    /// Verify a run of another branch standalone: contiguous, linked from a
    /// block this chain holds at or above its anchor, every block carrying
    /// its threshold against the roster (whose identity keys never move).
    fn verify_run(&self, run: &[ChainBlock]) -> Result<(), String> {
        let rid = self.republic_id();
        let head = self.chain.head.as_ref().ok_or("no chain")?;
        let first = run.first().ok_or("empty run")?;
        let base = first.height.checked_sub(1).ok_or("a run cannot start at the genesis")?;
        if self.own_hash(base).as_deref() != Some(first.prev.as_str()) {
            return Err(format!("run at {} does not link to this chain", first.height));
        }
        let mut prev: Option<&ChainBlock> = None;
        for b in run {
            if let Some(p) = prev {
                if b.height != p.height + 1 || b.prev != self.block_id_of(p) {
                    return Err(format!("run breaks at {}", b.height));
                }
            }
            if matches!(b.change, ChainChange::Genesis { .. }) {
                return Err("a genesis cannot appear after height 0".to_string());
            }
            check_shape(b)?;
            let need = if CutKind::of(&b.change).is_some() { head.identities.len() } else { usize::from(head.rule_m) };
            let signers = block_signers(&rid, &head.identities, b, head.block_v2_from)?;
            if signers.len() < need {
                return Err(format!("block {} has {} valid approvals, threshold is {need}", b.height, signers.len()));
            }
            prev = Some(b);
        }
        Ok(())
    }

    /// Keep a verified run of another branch: the blocks above the last
    /// one this chain shares. `Ok(false)` = nothing new.
    fn keep_evidence(&mut self, rest: &[ChainBlock]) -> Result<bool, String> {
        let Some(first) = rest.first() else {
            return Ok(false);
        };
        self.verify_run(rest)?;
        let root = self.block_id_of(first);
        let held = self.chain.branch.file.evidence.get(&root).map(Vec::len);
        if held.is_some_and(|n| n >= rest.len()) {
            return Ok(false);
        }
        if held.is_none() && self.chain.branch.file.evidence.len() >= EVIDENCE_MAX {
            return Err("evidence store full".to_string());
        }
        self.chain.branch.file.evidence.insert(root, rest.to_vec());
        Ok(true)
    }

    /// The part of `run` this chain does not hold.
    fn unheld<'a>(&self, run: &'a [ChainBlock]) -> &'a [ChainBlock] {
        let split = run.iter().position(|b| !self.holds_block(b.height, &self.block_id_of(b))).unwrap_or(run.len());
        &run[split..]
    }

    /// Retain the fork candidates that link into this chain as evidence:
    /// the tie-break clears them once it decides.
    pub(crate) fn harvest_branch_evidence(&mut self) {
        if self.chain.fork_candidates.is_empty() || self.branch_abandoned() || self.chain.head.is_none() {
            return;
        }
        let mut runs: Vec<Vec<ChainBlock>> = Vec::new();
        for c in self.chain.fork_candidates.values() {
            match runs.last_mut() {
                Some(run) if run.last().is_some_and(|p| p.height + 1 == c.height && self.block_id_of(p) == c.prev) => {
                    run.push(c.clone());
                }
                _ => runs.push(vec![c.clone()]),
            }
        }
        let mut changed = false;
        for run in runs {
            let rest = self.unheld(&run).to_vec();
            let linked = rest.first().is_some_and(|f| f.height > 0 && self.own_hash(f.height - 1).as_deref() == Some(f.prev.as_str()));
            if linked && self.keep_evidence(&rest).unwrap_or(false) {
                changed = true;
            }
        }
        if changed {
            tracing::info!(branches = self.chain.branch.file.evidence.len(), "branch=evidence_kept");
            self.persist_branch();
        }
    }

    /// The other branches at `base_height` whose base is this chain's block.
    fn branches_at(&self, base_height: u64) -> Vec<Vec<ChainBlock>> {
        let base = self.own_hash(base_height);
        self.chain
            .branch
            .file
            .evidence
            .values()
            .filter(|run| run.first().is_some_and(|f| f.height == base_height + 1 && Some(&f.prev) == base.as_ref()))
            .cloned()
            .collect()
    }

    /// The blocks a dropped run names, from this chain or the evidence.
    fn run_blocks(&self, run: &molt_core::chain::DroppedRun) -> Option<Vec<ChainBlock>> {
        let blocks: Vec<ChainBlock> = if self.own_hash(run.from_height).as_deref() == Some(run.first_hash.as_str()) {
            self.chain.blocks.iter().filter(|b| b.height >= run.from_height && b.height <= run.head_height).cloned().collect()
        } else {
            self.chain.branch.file.evidence.values().find_map(|e| {
                let i = e.iter().position(|b| b.height == run.from_height && self.block_id_of(b) == run.first_hash)?;
                Some(e[i..].iter().filter(|b| b.height <= run.head_height).cloned().collect::<Vec<_>>())
            })?
        };
        let last = blocks.last()?;
        let whole = last.height == run.head_height
            && self.block_id_of(last) == run.head_hash
            && u64::try_from(blocks.len()).ok() == Some(run.head_height - run.from_height + 1);
        whole.then_some(blocks)
    }

    /// What a choice drops, as this node sees it; `None` = not all held.
    pub(crate) fn dropped_rows(&self, change: &ChainChange) -> Option<Vec<BranchBlockRow>> {
        let ChainChange::BranchChosen { dropped, .. } = change else {
            return None;
        };
        let mut rows = Vec::new();
        for run in dropped {
            rows.extend(self.run_blocks(run)?.iter().map(|b| block_row(self.block_id_of(b), b)));
        }
        Some(rows)
    }

    /// `branch_status`.
    pub(crate) fn cmd_branch_status(&mut self) -> Result<Reply, MoltError> {
        self.harvest_branch_evidence();
        Ok(Reply::Branches(Box::new(self.branch_status_view())))
    }

    fn branch_view(&self, blocks: &[ChainBlock], own: bool) -> BranchView {
        let rows: Vec<BranchBlockRow> = blocks.iter().map(|b| block_row(self.block_id_of(b), b)).collect();
        let mut counts = BTreeMap::new();
        let mut signers: BTreeSet<MemberId> = BTreeSet::new();
        for r in &rows {
            let what = if r.surface.is_empty() { &r.op } else { &r.surface };
            *counts.entry(format!("{}:{what}", r.kind)).or_insert(0u32) += 1;
            signers.extend(r.signers.iter().cloned());
        }
        BranchView {
            root_hash: rows.first().map(|r| r.hash.clone()).unwrap_or_default(),
            head_height: rows.last().map_or(0, |r| r.height),
            head_hash: rows.last().map(|r| r.hash.clone()).unwrap_or_default(),
            own,
            irreversible: u32::try_from(rows.iter().filter(|r| r.risk == "irreversible").count()).unwrap_or(u32::MAX),
            counts,
            signers: signers.into_iter().collect(),
            blocks: rows,
        }
    }

    pub(crate) fn branch_status_view(&self) -> BranchStatusView {
        let rid = self.republic_id();
        let me = self.member();
        let (majority, m, n) = self.majority();
        let mut bases: BTreeMap<u64, String> = BTreeMap::new();
        for run in self.chain.branch.file.evidence.values() {
            if let Some(f) = run.first() {
                if f.height > 0 && self.own_hash(f.height - 1).as_deref() == Some(f.prev.as_str()) {
                    bases.insert(f.height - 1, f.prev.clone());
                }
            }
        }
        let forks = bases
            .into_iter()
            .map(|(base_height, base_hash)| {
                let own: Vec<ChainBlock> = self.chain.blocks.iter().filter(|b| b.height > base_height).cloned().collect();
                let mut branches = vec![self.branch_view(&own, true)];
                branches.extend(self.branches_at(base_height).iter().map(|r| self.branch_view(r, false)));
                let mut seen: BTreeMap<MemberId, usize> = BTreeMap::new();
                for b in &branches {
                    for s in &b.signers {
                        *seen.entry(s.clone()).or_insert(0) += 1;
                    }
                }
                ForkView {
                    base_height,
                    base_hash,
                    shared: seen.into_iter().filter(|(_, c)| *c > 1).map(|(s, _)| s).collect(),
                    branches,
                }
            })
            .collect();
        let cards = self
            .chain
            .branch
            .file
            .cards
            .iter()
            .map(|(id, card)| {
                let rows = self.dropped_rows(&card.change);
                BranchCardView {
                    id: *id,
                    by: card.by.clone(),
                    change: card.change.clone(),
                    signers: card.sigs.keys().cloned().collect(),
                    need: m,
                    digest: rows.as_ref().map(|r| dropped_digest(&rid, &card.change, r)).unwrap_or_default(),
                    dropped: rows.unwrap_or_default(),
                    approved_by_me: card.sigs.contains_key(&me),
                }
            })
            .collect();
        BranchStatusView {
            rule_m: m,
            rule_n: u8::try_from(n).unwrap_or(u8::MAX),
            majority,
            forks,
            cards,
            choices: self
                .chain
                .branch
                .file
                .choices
                .iter()
                .map(|(h, c)| OwnBranchChoice { base_height: *h, tip_hash: c.tip_hash.clone() })
                .collect(),
            incarnation: self.incarnation_height(),
            abandoned: self.chain.branch.file.abandoned.clone(),
        }
    }

    /// Build the change that keeps the branch ending at `tip_hash` at the
    /// fork point `base_height`.
    fn build_choice(&self, base_height: u64, tip_hash: &str) -> Result<ChainChange, MoltError> {
        let anchor = self.chain.blocks.first().map_or(0, |b| b.height);
        if base_height < anchor {
            return Err(err(BELOW_CUT));
        }
        let base_hash = self.own_hash(base_height).ok_or_else(|| err(NO_SECOND_BRANCH))?;
        let others = self.branches_at(base_height);
        if others.is_empty() {
            return Err(err(NO_SECOND_BRANCH));
        }
        let mut branches = vec![self.chain.blocks.iter().filter(|b| b.height > base_height).cloned().collect::<Vec<_>>()];
        branches.extend(others);
        let run_of = |blocks: &[ChainBlock]| -> Option<molt_core::chain::DroppedRun> {
            let (f, l) = (blocks.first()?, blocks.last()?);
            Some(molt_core::chain::DroppedRun {
                from_height: f.height,
                first_hash: self.block_id_of(f),
                head_height: l.height,
                head_hash: self.block_id_of(l),
            })
        };
        let (tip_height, chosen) = if tip_hash == base_hash {
            (base_height, None)
        } else {
            let found = branches
                .iter()
                .enumerate()
                .find_map(|(i, br)| br.iter().find(|b| self.block_id_of(b) == tip_hash).map(|b| (b.height, i)));
            let (h, i) = found.ok_or_else(|| err("tip not on a branch of this fork"))?;
            (h, Some(i))
        };
        let mut dropped = Vec::new();
        let mut chosen_root = String::new();
        for (i, br) in branches.iter().enumerate() {
            if Some(i) == chosen {
                chosen_root = br.first().map(|b| self.block_id_of(b)).unwrap_or_default();
                let tail: Vec<ChainBlock> = br.iter().filter(|b| b.height > tip_height).cloned().collect();
                dropped.extend(run_of(&tail));
            } else {
                dropped.extend(run_of(br));
            }
        }
        let change = ChainChange::BranchChosen {
            base_height,
            base_hash,
            tip_height,
            tip_hash: tip_hash.to_string(),
            chosen_root,
            dropped,
        };
        let probe = ChainBlock { height: tip_height + 1, prev: tip_hash.to_string(), change: change.clone(), sigs: Vec::new() };
        check_shape(&probe).map_err(|e| err(if e.contains(NO_SECOND_BRANCH) { NO_SECOND_BRANCH.to_string() } else { e }))?;
        Ok(change)
    }

    /// `choose_branch`: propose the card. Nothing is signed here.
    pub(crate) fn cmd_choose_branch(&mut self, base_height: u64, tip_hash: &str, amnesia_ack: bool) -> Result<Reply, MoltError> {
        self.branch_gate()?;
        self.harvest_branch_evidence();
        self.amnesia_gate(base_height, amnesia_ack)?;
        let change = self.build_choice(base_height, tip_hash)?;
        let rid = self.republic_id();
        let rows = self.dropped_rows(&change).ok_or_else(|| err(DROPPED_UNKNOWN))?;
        let digest = dropped_digest(&rid, &change, &rows);
        let known = self.chain.branch.file.cards.iter().find(|(_, c)| c.change == change).map(|(id, _)| *id);
        let id = match known {
            Some(id) => id,
            None => {
                if self.chain.branch.file.cards.len() >= CARDS_MAX {
                    return Err(err("too many open choices"));
                }
                let id = self.mint_proposal_id();
                let me = self.member();
                self.chain.branch.file.cards.insert(id, BranchCard { change: change.clone(), by: me, sigs: BTreeMap::new() });
                self.persist_branch();
                id
            }
        };
        tracing::info!(id, base = base_height, dropped = rows.len(), "branch=proposed");
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::BranchChoiceProposed { id: ProposalId(id), change });
        self.record(env);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::BranchProposed { id: ProposalId(id), dropped: rows, digest })
    }

    /// `approve_branch`: sign the card, once per fork point, the choice
    /// durable before the signature leaves.
    pub(crate) fn cmd_approve_branch(&mut self, id: u64, digest: &str, amnesia_ack: bool) -> Result<Reply, MoltError> {
        self.branch_gate()?;
        let card = self.chain.branch.file.cards.get(&id).cloned().ok_or(MoltError::UnknownProposal(ProposalId(id)))?;
        let ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, .. } = card.change.clone() else {
            return Err(MoltError::UnknownProposal(ProposalId(id)));
        };
        self.amnesia_gate(base_height, amnesia_ack)?;
        let rid = self.republic_id();
        let rows = self.dropped_rows(&card.change).ok_or_else(|| err(DROPPED_UNKNOWN))?;
        if dropped_digest(&rid, &card.change, &rows) != digest {
            return Err(err(DIGEST_DIFFERS));
        }
        let recorded = self.chain.branch.file.choices.get(&base_height);
        let other_sealed = self.chain.blocks.iter().any(|b| {
            matches!(&b.change, ChainChange::BranchChosen { base_height: h, tip_hash: t, .. } if *h == base_height && *t != tip_hash)
        });
        if other_sealed || recorded.is_some_and(|c| c.tip_hash != tip_hash || c.base_hash != base_hash) {
            return Err(err(format!("already chose at {base_height}")));
        }
        let Some(sk) = self.identity_sk.clone() else {
            return Err(err("no signing key on this seat"));
        };
        let had = self.chain.branch.file.choices.insert(base_height, Choice { base_hash, tip_hash: tip_hash.clone() });
        if !self.persist_branch() {
            match had {
                Some(c) => self.chain.branch.file.choices.insert(base_height, c),
                None => self.chain.branch.file.choices.remove(&base_height),
            };
            return Err(err("choice not saved"));
        }
        let sig = molt_storage::identity_sign(&sk, &signing_bytes(&rid, tip_height + 1, &tip_hash, &card.change, self.block_v2_from()));
        let me = self.member();
        if let Some(c) = self.chain.branch.file.cards.get_mut(&id) {
            c.sigs.insert(me.clone(), sig.clone());
        }
        #[cfg(test)]
        RECORDED_CHAIN.with(|r| r.borrow_mut().push(("branch_approved", tip_height + 1)));
        tracing::info!(id, base = base_height, "branch=approved");
        let env = self.make_env(me.clone(), WorkspaceEvent::BranchChoiceApproved { id: ProposalId(id), by: me, sig });
        self.record(env);
        self.persist_branch();
        self.try_seal_branch(id);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A card from the wire or a bundle.
    pub(crate) fn receive_branch_card(&mut self, by: &str, id: u64, change: ChainChange) {
        if self.branch_abandoned() || !self.is_chain_governed() {
            return;
        }
        let ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, .. } = &change else {
            return;
        };
        let probe = ChainBlock { height: tip_height + 1, prev: tip_hash.clone(), change: change.clone(), sigs: Vec::new() };
        // a fork point this chain does not hold is no choice it can judge
        if let Err(e) = check_shape(&probe).and_then(|()| {
            if self.holds_block(*base_height, base_hash) { Ok(()) } else { Err("base not held here".to_string()) }
        }) {
            tracing::warn!(id, %by, error = %e, "branch=card_refused");
            return;
        }
        match self.chain.branch.file.cards.get(&id) {
            Some(c) if c.change != change => {
                tracing::warn!(id, %by, "branch=card_collision");
                return;
            }
            Some(_) => {}
            None => {
                if self.chain.branch.file.cards.len() >= CARDS_MAX || self.proposals.contains_key(&id) {
                    return;
                }
                self.chain.branch.file.cards.insert(id, BranchCard { change, by: by.to_string(), sigs: BTreeMap::new() });
                if self.plausible_wire_id(id) {
                    self.next_id = self.next_id.max(id.saturating_add(1));
                }
                tracing::info!(id, %by, "branch=card_received");
            }
        }
        let parked: Vec<(u64, MemberId, String)> = self.chain.branch.parked.iter().filter(|(i, _, _)| *i == id).cloned().collect();
        self.chain.branch.parked.retain(|(i, _, _)| *i != id);
        for (_, signer, sig) in parked {
            self.add_branch_sig(id, &signer, &sig);
        }
        self.persist_branch();
        self.try_seal_branch(id);
        self.emit_session(crate::SessionScope::Full);
    }

    /// A signature from the wire or a bundle.
    pub(crate) fn receive_branch_sig(&mut self, id: u64, by: &str, sig: &str) {
        if self.branch_abandoned() || !self.is_chain_governed() {
            return;
        }
        if !self.chain.branch.file.cards.contains_key(&id) {
            if self.chain.branch.parked.len() < PARKED_MAX {
                self.chain.branch.parked.push((id, by.to_string(), sig.to_string()));
            }
            return;
        }
        if self.add_branch_sig(id, by, sig) {
            self.persist_branch();
            self.try_seal_branch(id);
            self.emit_session(crate::SessionScope::Full);
        }
    }

    fn add_branch_sig(&mut self, id: u64, by: &str, sig: &str) -> bool {
        let rid = self.republic_id();
        let Some(card) = self.chain.branch.file.cards.get(&id) else {
            return false;
        };
        let ChainChange::BranchChosen { tip_height, tip_hash, .. } = &card.change else {
            return false;
        };
        // sealed at tip+1 of the chosen branch: its `prev` is the tip
        let bytes = signing_bytes(&rid, tip_height + 1, tip_hash, &card.change, self.block_v2_from());
        let ok = self.chain.head.as_ref().is_some_and(|h| {
            h.identities.iter().any(|i| i.member == by && molt_storage::identity_verify(&i.identity_pk, &bytes, sig))
        });
        if !ok {
            tracing::warn!(id, %by, "branch=sig_refused");
            return false;
        }
        self.chain.branch.file.cards.get_mut(&id).is_some_and(|c| c.sigs.insert(by.to_string(), sig.to_string()).is_none())
    }

    /// Seal a card at m on a seat holding its tip: the m lowest-named
    /// signers, so two sealers seal the same block.
    fn try_seal_branch(&mut self, id: u64) {
        let Some(card) = self.chain.branch.file.cards.get(&id).cloned() else {
            return;
        };
        let ChainChange::BranchChosen { tip_height, tip_hash, .. } = &card.change else {
            return;
        };
        let m = usize::from(self.chain.head.as_ref().map_or(u8::MAX, |h| h.rule_m));
        if card.sigs.len() < m || !self.holds_block(*tip_height, tip_hash) || self.branch_abandoned() {
            return;
        }
        let sigs: Vec<RosterAttestation> =
            card.sigs.iter().take(m).map(|(member, sig)| RosterAttestation { member: member.clone(), sig: sig.clone() }).collect();
        let block = ChainBlock { height: tip_height + 1, prev: tip_hash.clone(), change: card.change.clone(), sigs };
        let me = self.member();
        if self.apply_branch_choice(&me, block.clone()) {
            tracing::info!(id, height = block.height, "branch=sealed");
            let env = self.make_env(me, WorkspaceEvent::Committed(block));
            self.record(env);
        }
    }

    /// The choice block on a seat that holds its tip: the chain becomes the
    /// chosen branch up to the tip plus the block; what it drops returns to
    /// the vote with stances cleared (Q3).
    fn apply_branch_choice(&mut self, from: &str, block: ChainBlock) -> bool {
        let ChainChange::BranchChosen { base_height, tip_height, .. } = &block.change else {
            return false;
        };
        let (base_height, tip_height) = (*base_height, *tip_height);
        let mut candidate: Vec<ChainBlock> = self.chain.blocks.iter().filter(|b| b.height <= tip_height).cloned().collect();
        candidate.push(block.clone());
        if let Err(e) = self.walk_own(&candidate) {
            let rule = refusal_rule(&e);
            let hash = self.note_refused(from, &block, rule);
            tracing::warn!(%from, %hash, rule, error = %e, "branch=choice_refused");
            return false;
        }
        let dropped: Vec<ChainBlock> = self.chain.blocks.iter().filter(|b| b.height > tip_height).cloned().collect();
        if !dropped.is_empty() {
            self.forget_wiki_stamps();
        }
        self.adopt_chain(candidate);
        for b in &dropped {
            if let ChainChange::Applied { proposal_id, .. } = &b.change {
                self.reopen_dropped_card(*proposal_id, base_height);
            }
        }
        self.vault_displaced(&dropped, &BTreeSet::new());
        let parked = std::mem::take(&mut self.chain.parked_approvals);
        self.in_block_batch(|st| {
            st.after_block_applied(&block);
            st.chain.pending_sigs.clear();
            st.chain.parked_approvals.extend(parked);
            st.chain.fork_candidates.clear();
            st.chain.diverged.clear();
            st.chain.resent_at.clear();
        });
        self.judge_parked_approvals();
        self.persist_chain_now();
        let file = &mut self.chain.branch.file;
        file.cards.retain(|_, c| !matches!(&c.change, ChainChange::BranchChosen { base_height: h, .. } if *h == base_height));
        file.evidence.retain(|_, run| run.first().is_some_and(|f| f.height != base_height + 1));
        self.persist_branch();
        tracing::warn!(%from, base = base_height, height = block.height, dropped = dropped.len(), "branch=chosen");
        self.emit_session(crate::SessionScope::Full);
        true
    }

    /// Q3: a dropped decision is a vote again, every stance cleared.
    fn reopen_dropped_card(&mut self, id: u64, base: u64) {
        self.chain.own_approvals.remove(&id);
        self.chain.pending_sigs.remove(&id);
        if self.proposals.get(&id).is_some_and(|p| p.by.is_empty()) {
            self.proposals.remove(&id);
            return;
        }
        if let Some(p) = self.proposals.get_mut(&id) {
            p.state = ProposalState::Proposed;
            p.applied_at = 0;
            p.approvals = 0;
            p.voted.clear();
            p.decliners.clear();
            p.stances.clear();
            tracing::info!(id, base, "branch=card_reopened");
        }
    }

    /// The inbound-block hook: `true` = handled here. An abandoned seat
    /// takes no block; a choice block is adopted where its tip is held,
    /// stops a seat on another branch, and otherwise rides the normal path.
    pub(crate) fn branch_intercepts(&mut self, from: &str, block: &ChainBlock) -> bool {
        if self.branch_abandoned() {
            return true;
        }
        let ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, chosen_root, .. } = &block.change else {
            return false;
        };
        let rid = self.republic_id();
        if self.holds_block(block.height, &self.block_id_of(block)) {
            return false;
        }
        let Some(head) = self.chain.head.clone() else {
            return false;
        };
        let standalone = check_shape(block).and_then(|()| block_signers(&rid, &head.identities, block, head.block_v2_from));
        if !standalone.is_ok_and(|s| s.len() >= usize::from(head.rule_m)) {
            let hash = self.note_refused(from, block, "branch");
            tracing::warn!(%from, %hash, "branch=choice_refused");
            return true;
        }
        if self.holds_block(*tip_height, tip_hash) {
            self.apply_branch_choice(from, block.clone());
            return true;
        }
        if self.holds_block(*base_height, base_hash) {
            let on_chosen = self.own_hash(base_height + 1).map_or(true, |own| !chosen_root.is_empty() && own == *chosen_root);
            if !on_chosen {
                self.enter_abandoned(block);
                return true;
            }
        }
        false
    }

    /// §6: stop, say so once, wait for a link.
    fn enter_abandoned(&mut self, block: &ChainBlock) {
        let ChainChange::BranchChosen { base_height, .. } = &block.change else {
            return;
        };
        let base_height = *base_height;
        let by: Vec<MemberId> = block.sigs.iter().map(|a| a.member.clone()).collect();
        self.chain.branch.file.abandoned = Some(BranchAbandoned { base_height, by: by.clone(), height: block.height });
        self.chain.branch.file.cards.clear();
        self.chain.branch.parked.clear();
        self.persist_branch();
        let names = by.join(", ");
        tracing::warn!(base = base_height, by = %names, "branch=abandoned");
        self.wake_trigger(molt_core::kanban_wake::CHAIN_STUCK_REASON, "");
        let me = self.member();
        let line = format!("branch abandoned at {base_height} by {names} - rejoin with a link");
        if let Err(e) = self.post_message(me, line, None, molt_core::ChannelRef::Group) {
            tracing::warn!(error = %e, "branch=abandoned chat=failed");
        }
        self.session.notice = format!("branch-abandoned:{base_height}:{}", by.join(","));
        self.emit_session(crate::SessionScope::Full);
    }

    /// §6/§7: a seat the last choice left behind - not among its signers
    /// and not seen on this branch since. Its restore needs no silence and
    /// ignores the divergence the choice left.
    pub(crate) fn seat_left_by_choice(&self, member: &str) -> bool {
        if member == self.member() {
            return false;
        }
        let Some(choice) = self.chain.blocks.iter().rev().find(|b| matches!(b.change, ChainChange::BranchChosen { .. })) else {
            return false;
        };
        let signed = choice.sigs.iter().any(|a| a.member == member);
        let seen = self.chain.peer_on_branch.get(member).is_some_and(|(_, h, _)| *h >= choice.height);
        !signed && !seen
    }

    /// [`State::chain_unsettled_besides`] that also ignores every seat the
    /// last choice left behind.
    pub(crate) fn unsettled_besides_left(&self, member: &str) -> bool {
        if !self.chain.fork_candidates.is_empty() {
            return true;
        }
        let silent: Vec<String> = self.chain_lag().silent.into_iter().map(|s| s.member).collect();
        self.chain.diverged.keys().any(|p| p != member && !silent.contains(p) && !self.seat_left_by_choice(p))
    }

    /// The open cards and their signatures, re-served with the open
    /// governance.
    pub(crate) fn branch_open_events(&self) -> Vec<WorkspaceEvent> {
        let mut out = Vec::new();
        for (id, card) in &self.chain.branch.file.cards {
            out.push(WorkspaceEvent::BranchChoiceProposed { id: ProposalId(*id), change: card.change.clone() });
            for (by, sig) in &card.sigs {
                out.push(WorkspaceEvent::BranchChoiceApproved { id: ProposalId(*id), by: by.clone(), sig: sig.clone() });
            }
        }
        out
    }

    /// The bundle this seat exports: its chain above the lowest fork point
    /// it knows (above its anchor when it knows none), the other branches
    /// it holds, the open cards.
    pub(crate) fn branch_bundle(&self) -> Bundle {
        let file = &self.chain.branch.file;
        let anchor = self.chain.blocks.first().map_or(0, |b| b.height);
        let from = file.evidence.values().filter_map(|r| r.first().map(|f| f.height)).min().unwrap_or(anchor + 1);
        let mut runs = vec![self.chain.blocks.iter().filter(|b| b.height >= from).cloned().collect::<Vec<_>>()];
        runs.extend(file.evidence.values().cloned());
        Bundle {
            format: BUNDLE_FORMAT.to_string(),
            republic_id: self.republic_id(),
            from: self.member(),
            runs,
            cards: file.cards.iter().map(|(id, c)| (*id, c.clone())).collect(),
        }
    }

    /// `branch_export`.
    pub(crate) fn cmd_branch_export(&mut self, dest: &str, passphrase: String) -> Result<Reply, MoltError> {
        if self.chain.head.is_none() {
            return Err(err("no chain yet"));
        }
        molt_storage::export::check_passphrase_policy(&passphrase).map_err(|e| MoltError::BadPayload(e.to_string()))?;
        self.harvest_branch_evidence();
        let path = molt_storage::expand_tilde(&self.host_path(dest)?);
        let plain = serde_json::to_vec(&self.branch_bundle()).map_err(|e| err(e.to_string()))?;
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Engine("engine is shutting down".to_string()));
        };
        let passphrase = zeroize::Zeroizing::new(passphrase);
        tokio::spawn(async move {
            let shown = path.display().to_string();
            let res = tokio::task::spawn_blocking(move || molt_storage::branch::write_bundle_file(&path, &plain, &passphrase)).await;
            let error = match res {
                Ok(Ok(_)) => String::new(),
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
            };
            crate::send_internal(&cmd_tx, molt_core::Command::NetBranchExported { dest: shown, error }).await;
        });
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_branch_exported(&mut self, dest: &str, error: &str) -> Result<Reply, MoltError> {
        if error.is_empty() {
            tracing::info!(%dest, "branch=exported");
            self.session.notice = format!("branch-exported:{dest}");
        } else {
            tracing::warn!(%dest, %error, "branch=export_failed");
            self.session.notice = format!("branch-export-failed:{error}");
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// `branch_import`.
    pub(crate) fn cmd_branch_import(&mut self, path: &str, passphrase: String) -> Result<Reply, MoltError> {
        if self.chain.head.is_none() {
            return Err(err("no chain yet"));
        }
        let path = molt_storage::expand_tilde(&self.host_path(path)?);
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Engine("engine is shutting down".to_string()));
        };
        let passphrase = zeroize::Zeroizing::new(passphrase);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || molt_storage::branch::read_bundle_file(&path, &passphrase)).await;
            let cmd = match res {
                Ok(Ok(plain)) => molt_core::Command::NetBranchLoaded {
                    bundle: String::from_utf8_lossy(&plain).into_owned(),
                    error: String::new(),
                },
                Ok(Err(e)) => molt_core::Command::NetBranchLoaded { bundle: String::new(), error: e.to_string() },
                Err(e) => molt_core::Command::NetBranchLoaded { bundle: String::new(), error: e.to_string() },
            };
            crate::send_internal(&cmd_tx, cmd).await;
        });
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_branch_loaded(&mut self, bundle: &str, error: &str) -> Result<Reply, MoltError> {
        let outcome = if error.is_empty() { self.import_bundle(bundle) } else { Err(error.to_string()) };
        self.session.notice = match outcome {
            Ok((runs, cards)) => format!("branch-imported:{runs}:{cards}"),
            Err(e) => {
                tracing::warn!(error = %e, "branch=import_failed");
                format!("branch-import-failed:{e}")
            }
        };
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Take a bundle in: `(branches kept, cards known)`. Blocks that extend
    /// this chain take the normal path; every run of another branch
    /// verifies standalone first, or the whole bundle is refused.
    pub(crate) fn import_bundle(&mut self, json: &str) -> Result<(usize, usize), String> {
        let bundle: Bundle = serde_json::from_str(json).map_err(|e| format!("bundle: {e}"))?;
        if bundle.format != BUNDLE_FORMAT {
            return Err("not a branch bundle".to_string());
        }
        if bundle.republic_id != self.republic_id() {
            return Err("another republic".to_string());
        }
        let Some(head) = self.chain.head.clone() else {
            return Err("no chain yet".to_string());
        };
        if self.branch_abandoned() {
            return Err(ABANDONED.to_string());
        }
        let extends = |st: &Self, rest: &[ChainBlock]| {
            rest.first().is_some_and(|f| f.height == head.height + 1 && st.own_hash(head.height).as_deref() == Some(f.prev.as_str()))
        };
        for run in &bundle.runs {
            let rest = self.unheld(run);
            if !rest.is_empty() && !extends(self, rest) {
                self.verify_run(rest)?;
            }
        }
        let from = bundle.from.clone();
        let mut kept = 0;
        let mut choices = Vec::new();
        for run in &bundle.runs {
            let rest = self.unheld(run).to_vec();
            if rest.is_empty() {
                continue;
            }
            if extends(self, &rest) {
                for b in rest {
                    self.receive_block_at(&from, None, b);
                }
                continue;
            }
            if self.keep_evidence(&rest).is_ok() {
                kept += 1;
            }
            choices.extend(rest.into_iter().filter(|b| matches!(b.change, ChainChange::BranchChosen { .. })));
        }
        self.persist_branch();
        for choice in choices {
            self.receive_block_at(&from, None, choice);
        }
        let mut cards = 0;
        for (id, card) in bundle.cards {
            self.receive_branch_card(&card.by, id, card.change.clone());
            for (by, sig) in &card.sigs {
                self.receive_branch_sig(id, by, sig);
            }
            cards += 1;
        }
        tracing::info!(%from, branches = kept, cards, "branch=imported");
        Ok((kept, cards))
    }
}
