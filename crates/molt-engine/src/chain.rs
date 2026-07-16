// SPDX-License-Identifier: GPL-3.0-or-later

//! **Verification of the persistent commit-block chain** (see
//! [`molt_core::chain`]). This is the security layer that makes a handed-over
//! chain *self-authenticating*: a member — or a rejoiner who fetched the chain
//! from an untrusted peer — checks it here with no live mesh and no trust in the
//! deliverer.
//!
//! The split mirrors the founding roster exactly: the canonical bytes live in
//! `molt-core`, the SHA-256 hash in `molt-storage`, and the Ed25519 checks here
//! next to [`crate::founding::verify_sealed_roster`] — of which the genesis
//! check below is a strict generalization (same `republic_id` content-match,
//! same per-member `identity_verify` over [`molt_core::roster_canonical_bytes`]).
//!
//! **Every check is hard-reject.** A bad signature, a broken `prev` link, a gap
//! in the heights, a genesis whose id does not match its content, a double-apply
//! — any of these fails the whole verification. Determinism across every
//! member's converged state demands nothing weaker.

use std::collections::BTreeSet;

use molt_core::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, Event, MemberIdentity, MembershipOp,
    ProposalId, ProposalState, RosterAttestation, SealedRoster, Surface, WorkspaceEvent,
    GENESIS_PREV,
};

use crate::State;

/// The **ephemeral** signature collection for one pending proposal on a
/// chain-governed republic (never persisted; rebuilt from gossip). The
/// committer bundles these into a block once `sigs` reaches the threshold. A
/// re-base (the head advanced past `height`) clears it and re-signs.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingApproval {
    /// The chain height every signature here is bound to.
    pub height: u64,
    /// One signature per distinct member (latest wins).
    pub sigs: Vec<RosterAttestation>,
}

/// A recovery in flight on the coordinator: the returning member's fresh MLS
/// KeyPackage + reply-queue handover, kept keyed by the re-admission proposal id
/// until its `Restored` block commits — then the coordinator re-keys the group
/// (`restore_member`) and sends the Welcome back to `reply`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the MLS re-key increment (restore_member + Welcome)
pub(crate) struct PendingRecovery {
    pub member: String,
    pub key_package: String,
    pub reply: String,
}

/// The verified head of a chain plus the roster it establishes: everything a
/// caller needs to check the *next* block or to trust a synced chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    /// Height of the last verified block.
    pub height: u64,
    /// [`block_hash`] of the last verified block — the `prev` the next block
    /// must carry.
    pub hash: String,
    /// The content-derived republic id, fixed by the genesis.
    pub republic_id: String,
    /// Approval threshold (m) for every post-genesis block.
    pub rule_m: u8,
    /// The live roster (name → identity key) after all membership blocks.
    pub identities: Vec<MemberIdentity>,
}

/// SHA-256 (hex) of a block's [`block_link_bytes`] — the value the next block's
/// `prev` points at.
pub(crate) fn block_hash(republic_id: &str, block: &ChainBlock) -> String {
    molt_storage::content_hash(&block_link_bytes(republic_id, block))
}

/// The distinct roster members who validly signed `bytes`. A signature from an
/// unknown member, a bad signature, or the same member signing twice can never
/// inflate the count past the number of real, distinct approvers.
fn valid_signers(
    identities: &[MemberIdentity],
    bytes: &[u8],
    sigs: &[RosterAttestation],
) -> BTreeSet<String> {
    let mut ok = BTreeSet::new();
    for att in sigs {
        let Some(id) = identities.iter().find(|i| i.member == att.member) else {
            continue;
        };
        if molt_storage::identity_verify(&id.identity_pk, bytes, &att.sig) {
            ok.insert(att.member.clone());
        }
    }
    ok
}

/// Evolve the roster for the blocks that *follow* a membership change.
fn apply_membership(
    identities: &mut Vec<MemberIdentity>,
    op: MembershipOp,
    member: &str,
    identity_pk: &str,
) -> Result<(), String> {
    match op {
        MembershipOp::Joined => {
            if identities.iter().any(|i| i.member == member) {
                return Err(format!("member {member} is already in the roster"));
            }
            identities.push(MemberIdentity {
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
            });
        }
        MembershipOp::Restored => {
            let Some(id) = identities.iter_mut().find(|i| i.member == member) else {
                return Err(format!("cannot restore unknown member {member}"));
            };
            // recovery re-derives the SAME identity: a Restored block re-keys
            // the MLS leaf, never the roster identity (`recovery_ritual.md`
            // §6). A block that presents a different key would let m-of-n
            // survivors hijack a seat — hard-reject it here, at the verifier,
            // not only at the coordinator's propose step.
            if id.identity_pk != identity_pk {
                return Err(format!(
                    "a Restored block must keep {member}'s anchored identity key"
                ));
            }
        }
    }
    Ok(())
}

/// Verify block 0: it must be a genesis whose `republic_id` is the neutral
/// content-derived value, sealed **unanimously** (n-of-n) by every anchored
/// member — the founding attestations. Returns the initial head.
fn verify_genesis(block: &ChainBlock) -> Result<ChainHead, String> {
    if block.height != 0 {
        return Err("genesis is not at height 0".to_string());
    }
    if block.prev != GENESIS_PREV {
        return Err("genesis prev is not the zero root".to_string());
    }
    let ChainChange::Genesis {
        name,
        republic_id,
        rule_m,
        rule_n,
        identities,
        agenda: _,
    } = &block.change
    else {
        return Err("block 0 is not a genesis".to_string());
    };
    if *rule_m == 0 || rule_m > rule_n {
        return Err("genesis threshold is out of range".to_string());
    }
    if usize::from(*rule_n) != identities.len() {
        return Err("genesis roster size does not match n".to_string());
    }
    let rid = molt_storage::republic_id(name, *rule_m, *rule_n, identities);
    if &rid != republic_id {
        return Err("genesis republic id does not match its content".to_string());
    }
    let bytes = approval_bytes(republic_id, 0, &block.change);
    let signers = valid_signers(identities, &bytes, &block.sigs);
    if signers.len() != identities.len() {
        return Err("genesis is not fully signed by every member".to_string());
    }
    Ok(ChainHead {
        height: 0,
        hash: block_hash(republic_id, block),
        republic_id: republic_id.clone(),
        rule_m: *rule_m,
        identities: identities.clone(),
    })
}

/// Verify one post-genesis block against the current head, returning the
/// advanced head. `seen_proposals` accumulates applied proposal ids across the
/// chain so a proposal cannot be committed twice.
fn verify_next(
    head: &ChainHead,
    block: &ChainBlock,
    seen_proposals: &mut BTreeSet<u64>,
) -> Result<ChainHead, String> {
    if block.height != head.height + 1 {
        return Err(format!(
            "block height {} does not follow {}",
            block.height, head.height
        ));
    }
    if block.prev != head.hash {
        return Err(format!("block {} does not link to its predecessor", block.height));
    }
    match &block.change {
        ChainChange::Genesis { .. } => {
            return Err("a genesis cannot appear after height 0".to_string());
        }
        ChainChange::Applied { proposal_id, .. } => {
            if !seen_proposals.insert(*proposal_id) {
                return Err(format!("proposal {proposal_id} is applied twice"));
            }
        }
        ChainChange::Membership { .. } => {}
    }
    let bytes = approval_bytes(&head.republic_id, block.height, &block.change);
    let signers = valid_signers(&head.identities, &bytes, &block.sigs);
    if signers.len() < usize::from(head.rule_m) {
        return Err(format!(
            "block {} has {} valid approvals, threshold is {}",
            block.height,
            signers.len(),
            head.rule_m
        ));
    }
    let mut identities = head.identities.clone();
    if let ChainChange::Membership {
        op,
        member,
        identity_pk,
    } = &block.change
    {
        apply_membership(&mut identities, *op, member, identity_pk)?;
    }
    Ok(ChainHead {
        height: block.height,
        hash: block_hash(&head.republic_id, block),
        republic_id: head.republic_id.clone(),
        rule_m: head.rule_m,
        identities,
    })
}

/// Verify a whole chain from its genesis and return its head. Any failure is
/// hard: the chain is rejected in full (a partially-valid chain is not a thing
/// — a rejoiner that trusted a prefix could fork the republic's state).
pub fn verify_chain(blocks: &[ChainBlock]) -> Result<ChainHead, String> {
    let Some((genesis, rest)) = blocks.split_first() else {
        return Err("empty chain".to_string());
    };
    let mut head = verify_genesis(genesis)?;
    let mut seen = BTreeSet::new();
    for block in rest {
        head = verify_next(&head, block, &mut seen)?;
    }
    Ok(head)
}

impl State {
    /// Build block 0 of the persistent chain from a sealed roster — but only
    /// for a **real** founding (a content-derived republic id and one
    /// attestation per member). A pre-ritual/demo materialize gets no chain
    /// (empty), so the running single-operator simulation is untouched.
    pub(crate) fn genesis_chain(&self, sealed: &SealedRoster) -> Vec<ChainBlock> {
        if sealed.republic_id.is_empty()
            || sealed.identities.is_empty()
            || sealed.attestations.len() != sealed.identities.len()
        {
            return Vec::new();
        }
        vec![ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            change: ChainChange::Genesis {
                name: sealed.name.clone(),
                republic_id: sealed.republic_id.clone(),
                rule_m: sealed.rule_m,
                rule_n: sealed.rule_n,
                identities: sealed.identities.clone(),
                agenda: sealed.agenda.clone(),
            },
            sigs: sealed.attestations.clone(),
        }]
    }

    /// Verify a freshly-loaded or freshly-built chain and adopt it as the open
    /// workspace's chain + head, then re-project the persistent state from it.
    /// A chain that fails verification is **hard-rejected**: the head stays
    /// `None` and nothing is projected (a partially-trusted chain could fork
    /// state — `documents/persistent_chain.md`).
    pub(crate) fn adopt_chain(&mut self, chain: Vec<ChainBlock>) {
        match verify_chain(&chain) {
            Ok(head) => {
                self.chain = chain;
                self.chain_head = Some(head);
                self.apply_chain_to_state();
            }
            Err(e) => {
                tracing::warn!(error = %e, "rejecting an unverifiable chain");
                self.chain.clear();
                self.chain_head = None;
            }
        }
    }

    /// Re-project the persistent state from the whole chain: the gated
    /// surfaces' applied logs (into the chain-owned [`State::chain_applied`], a
    /// full clear-and-refold so a re-base is free) and the roster/identities
    /// (taken from the already-verified head, which evolved them across the
    /// membership blocks). Chat, [`State::applied`] and pending proposals are
    /// left untouched — they are ephemeral or legacy-owned.
    pub(crate) fn apply_chain_to_state(&mut self) {
        let mut projected: std::collections::HashMap<
            Surface,
            Vec<(Option<u64>, serde_json::Value)>,
        > = std::collections::HashMap::new();
        for block in &self.chain {
            if let ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } = &block.change
            {
                projected
                    .entry(*surface)
                    .or_default()
                    .push((Some(*proposal_id), payload.clone()));
            }
        }
        self.chain_applied = projected;
        // the verified head carries the roster after every membership block —
        // adopt it so the newcomers/rekeys show up in the roster + approvals
        if let Some(head) = &self.chain_head {
            if let Some(r) = &mut self.replica {
                r.identities = head.identities.clone();
                r.roster = head.identities.iter().map(|i| i.member.clone()).collect();
            }
        }
    }

    /// Surface a chain workspace that opened without its local signing key: it
    /// can still verify and follow the chain, but cannot itself co-sign
    /// governance approvals (a reopen that lost `transport.state`'s
    /// `identity_sk`, or a pre-chain workspace). Cheap invariant check, logged.
    pub(crate) fn note_governance_readiness(&self) {
        if self.chain_head.is_some() && self.identity_sk.is_none() {
            tracing::warn!(
                republic = %self.republic_id(),
                "chain workspace has no local signing key — it can follow governance but not co-sign it"
            );
        }
    }
}

// ---- runtime chain governance (real threshold over the mesh) ---------------

impl State {
    /// A workspace whose governance runs through the chain (real m-of-n
    /// signatures) rather than the legacy counted simulation.
    pub(crate) fn is_chain_governed(&self) -> bool {
        self.chain_head.is_some()
    }

    /// The committed change a pending proposal would enact. A registered change
    /// (any kind — e.g. a `Membership` re-admission) wins; otherwise it is a
    /// gated `Applied` reconstructed from the surface proposal.
    fn proposal_change(&self, id: u64) -> Option<ChainChange> {
        if let Some(change) = self.proposal_changes.get(&id) {
            return Some(change.clone());
        }
        let p = self.proposals.get(&id)?;
        Some(ChainChange::Applied {
            proposal_id: id,
            surface: p.surface,
            payload: p.payload.clone(),
        })
    }

    /// Propose a membership change (re-admit a returning member, or add a seat)
    /// and co-sign it — the producer for `Membership` blocks (recovery step ❹).
    /// Further approvals arrive from the other members; a block seals at m-of-n.
    /// Returns the proposal id.
    pub(crate) fn propose_membership(
        &mut self,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.proposal_changes.insert(
            id,
            ChainChange::Membership {
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
            },
        );
        // announce the proposal over the mesh so every member registers + signs
        // the SAME change (the membership twin of a gated `Proposed`)
        let me = self.member();
        let env = self.make_env(
            me,
            WorkspaceEvent::MembershipProposed {
                id: ProposalId(id),
                op,
                member: member.to_string(),
                identity_pk: identity_pk.to_string(),
            },
        );
        self.record(env);
        if self.config.self_cosign {
            self.chain_sign_and_gossip_approval(id);
        }
        id
    }

    /// Register a membership proposal another member put forward, so this node
    /// signs the SAME change (its bytes) when it approves.
    pub(crate) fn receive_membership_proposal(
        &mut self,
        id: u64,
        op: MembershipOp,
        member: &str,
        identity_pk: &str,
    ) {
        self.proposal_changes.entry(id).or_insert_with(|| ChainChange::Membership {
            op,
            member: member.to_string(),
            identity_pk: identity_pk.to_string(),
        });
        self.next_id = self.next_id.max(id + 1);
    }

    /// A recovery coordinator's re-admit decision (recovery step ❸): verify a
    /// returning member's seat proof against its ANCHORED identity, then propose
    /// the threshold `Membership{Restored}` block. Recovery re-derives the same
    /// identity, so the requested key must equal the anchored one (it re-keys the
    /// MLS leaf, not the roster). Returns the proposal id, or the refusal reason.
    ///
    /// A verified request also registers the [`PendingRecovery`] (the fresh
    /// KeyPackage + `reply` handover the MLS re-key consumes) — and it must do
    /// so **before** proposing: with a lone coordinator (m=1, self-cosign) the
    /// `Restored` block commits *synchronously inside* `propose_membership`,
    /// and `after_block_applied` keys the re-key on this entry. Registering it
    /// afterwards would silently skip the re-key (the recovery E2E pins this).
    pub(crate) fn verify_and_propose_restore(
        &mut self,
        member: &str,
        requested_pk: &str,
        key_package_hex: &str,
        ticket: &str,
        seat_proof: &str,
        reply: &str,
    ) -> Result<u64, String> {
        let anchored = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone())
            .ok_or_else(|| format!("no anchored seat for {member}"))?;
        if requested_pk != anchored {
            return Err("recovery must re-derive the seat's own identity key".to_string());
        }
        let rid = self.republic_id();
        if !crate::founding::verify_seat_proof(&anchored, ticket, key_package_hex, &rid, seat_proof) {
            return Err(format!("seat proof for {member} does not verify"));
        }
        self.pending_recovery.insert(
            member.to_string(),
            PendingRecovery {
                member: member.to_string(),
                key_package: key_package_hex.to_string(),
                reply: reply.to_string(),
            },
        );
        Ok(self.propose_membership(MembershipOp::Restored, member, &anchored))
    }

    /// Distinct collected approvals for a proposal (for the UI progress).
    pub(crate) fn chain_approval_count(&self, id: u64) -> usize {
        self.pending_sigs.get(&id).map(|p| p.sigs.len()).unwrap_or(0)
    }

    /// Sign this node's approval of a proposal at the current head+1 and
    /// record + gossip it (the outbox fans the self-authored `Approved`
    /// envelope out over the mesh). Then try to seal. The proposer's own
    /// co-signature and every explicit approve funnel through here.
    pub(crate) fn chain_sign_and_gossip_approval(&mut self, id: u64) {
        let (Some(sk), Some(head)) = (self.identity_sk.as_ref(), self.chain_head.as_ref()) else {
            return;
        };
        let height = head.height + 1;
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        let sig = molt_storage::identity_sign(sk, &bytes);
        let me = self.member();
        self.collect_sig(id, height, &me, &sig);
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Approved {
                id: ProposalId(id),
                by: me,
                height,
                sig,
            },
        );
        self.record(env);
        self.try_commit(id);
    }

    /// Collect one signature into a proposal's pending set: dedup by member
    /// (latest wins), and rebase the set to a newer `height` (dropping stale
    /// signatures) — a signature for an already-superseded height is ignored.
    fn collect_sig(&mut self, id: u64, height: u64, member: &str, sig: &str) {
        let entry = self.pending_sigs.entry(id).or_default();
        if height > entry.height {
            entry.height = height;
            entry.sigs.clear();
        } else if height < entry.height {
            return;
        }
        entry.sigs.retain(|a| a.member != member);
        entry.sigs.push(RosterAttestation {
            member: member.to_string(),
            sig: sig.to_string(),
        });
    }

    /// Try to seal a block for a proposal that has gathered the threshold of
    /// valid, distinct signatures at the current head+1. Deterministic: the m
    /// lowest-named valid signers are chosen, so two nodes that both reach the
    /// threshold seal the byte-identical block (it self-dedups on receipt).
    pub(crate) fn try_commit(&mut self, id: u64) {
        let Some(head) = self.chain_head.clone() else {
            return;
        };
        // already committed?
        if matches!(self.proposals.get(&id), Some(p) if p.state != ProposalState::Proposed) {
            return;
        }
        let target = head.height + 1;
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), target, &change);
        let Some(pending) = self.pending_sigs.get(&id) else {
            return;
        };
        if pending.height != target {
            return; // stale set awaiting a re-base
        }
        let mut valid: Vec<RosterAttestation> = pending
            .sigs
            .iter()
            .filter(|a| {
                head.identities.iter().any(|i| {
                    i.member == a.member && molt_storage::identity_verify(&i.identity_pk, &bytes, &a.sig)
                })
            })
            .cloned()
            .collect();
        valid.sort_by(|a, b| a.member.cmp(&b.member));
        valid.dedup_by(|a, b| a.member == b.member);
        if valid.len() < usize::from(head.rule_m) {
            return;
        }
        valid.truncate(usize::from(head.rule_m));
        let block = ChainBlock {
            height: target,
            prev: head.hash.clone(),
            change,
            sigs: valid,
        };
        self.adopt_committed_block(block, id);
    }

    /// Append a block we sealed ourselves: adopt it, then broadcast it to the
    /// mesh (record a self-authored `Committed` envelope the outbox fans out).
    ///
    /// ORDER is load-bearing: `after_block_applied` runs **before** the
    /// `Committed` envelope is recorded. A `Restored` block's re-key advances
    /// this node's MLS epoch and records the raw `MlsCommit` — and because the
    /// outbox encrypts lazily at *send* time, any envelope sequenced before
    /// that `MlsCommit` gets new-epoch ciphertext the still-old-epoch peers
    /// drop (no cross-epoch buffer). Recording `Committed` after the re-key
    /// puts it *behind* the `MlsCommit` in the per-link stream, so every
    /// survivor merges the commit first and then decrypts the block. (The
    /// ephemeral Proposed/Approved gossip sequenced earlier is caught by the
    /// receive side's cross-epoch retry — held until the commit merges — but
    /// this sender-side ordering keeps the BLOCK's delivery independent of
    /// that bounded buffer.) The recovery E2E with a live survivor pins this.
    fn adopt_committed_block(&mut self, block: ChainBlock, proposal_id: u64) {
        if !self.append_committed_block(block.clone()) {
            return;
        }
        self.after_block_applied(&block);
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Committed(block.clone()));
        self.record(env);
        // clean up the proposal we just committed — a Membership block carries
        // no proposal id for after_block_applied to key on, so drop it here
        self.pending_sigs.remove(&proposal_id);
        self.proposal_changes.remove(&proposal_id);
        tracing::debug!(height = block.height, %proposal_id, "sealed and broadcast a chain block");
    }

    /// Verify a block against the current chain, append + persist it, and
    /// re-project state. Returns whether it was accepted (a block that fails
    /// full-chain verification is refused and rolled back).
    fn append_committed_block(&mut self, block: ChainBlock) -> bool {
        self.chain.push(block);
        match verify_chain(&self.chain) {
            Ok(head) => {
                self.chain_head = Some(head);
                self.apply_chain_to_state();
                let chain = self.chain.clone();
                if let Some(active) = &self.active {
                    active.handle.persist_chain_blocking(chain);
                }
                true
            }
            Err(e) => {
                self.chain.pop();
                tracing::error!(error = %e, "refused a block that fails chain verification");
                false
            }
        }
    }

    /// After a block is applied (by us or a peer): mark its proposal committed,
    /// emit, clear its collected signatures, and re-base every other pending
    /// proposal onto the new head (their old-height signatures are now stale).
    fn after_block_applied(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                ..
            } => {
                if let Some(p) = self.proposals.get_mut(proposal_id) {
                    p.state = ProposalState::Applied;
                }
                self.pending_sigs.remove(proposal_id);
                self.emit(Event::Applied {
                    id: ProposalId(*proposal_id),
                    surface: *surface,
                });
                if *surface == Surface::Organization {
                    self.after_org_applied();
                }
            }
            // a re-admission committed: on EVERY node, a threshold-approved
            // recovery outranks the announce rate limit (the member's fresh
            // announce must never be swallowed by a cooldown stamped for its
            // previous life — e.g. a re-recovery within the window); if THIS
            // node coordinated it (holds the returning member's fresh
            // KeyPackage), it also drives the MLS re-key
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member,
                ..
            } => {
                self.mesh_extension_at.remove(member);
                if self.pending_recovery.contains_key(member) {
                    let member = member.clone();
                    self.coordinator_rekey(&member);
                }
            }
            _ => {}
        }
        self.rebase_pending_approvals();
    }

    /// The coordinator's MLS re-key once a `Restored` block committed: run
    /// `restore_member` on the runtime group with the returning member's fresh
    /// KeyPackage → `(commit, welcome)`, then distribute both. The commit is
    /// broadcast to the survivors over the mesh (a recorded `MlsCommit`, sent raw
    /// so each survivor advances to the new epoch); the welcome goes to the
    /// returning member's reply queue. Finally the rejoin is announced in the
    /// group chat. Consumes the pending recovery. A node with no runtime group
    /// logs and does nothing.
    fn coordinator_rekey(&mut self, member: &str) {
        let Some(pending) = self.pending_recovery.remove(member) else {
            return;
        };
        let Ok(kp) = hex::decode(&pending.key_package) else {
            tracing::warn!(%member, "recovery KeyPackage is not valid hex");
            return;
        };
        match self.net.as_ref().and_then(|n| n.restore_member_on_group(member, &kp)) {
            Some(Ok((commit, welcome))) => {
                let me = self.member();
                // 1) broadcast the raw re-key commit to the survivors: recorded as
                // an `MlsCommit`, the outbox fans it out; every survivor merges it
                // and advances to the new epoch (it MUST precede any new-epoch
                // traffic — hence recorded before the announcement below).
                let env =
                    self.make_env(me.clone(), WorkspaceEvent::MlsCommit { commit: hex::encode(&commit) });
                self.record(env);
                // 2) deliver the welcome + the whole chain to the returning
                // member's reply queue so it rejoins the group AND catches its
                // state up over this same channel (option A). Off the actor.
                if let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) {
                    let chain_json = serde_json::to_string(&self.chain).unwrap_or_default();
                    crate::recovery::spawn_welcome_send(
                        transport,
                        pending.reply.clone(),
                        welcome,
                        chain_json,
                    );
                }
                // 3) announce the rejoin in the group chat — AFTER the commit, so
                // the survivors have advanced to the epoch this notice is
                // encrypted at (ephemeral, best-effort like all chat). A
                // System-kind message: every frontend renders it as a quiet
                // system line, not as the coordinator speaking.
                if let Err(e) = self.post_message_with_kind(
                    me,
                    format!("🔑 {member} rejoined the republic after recovery"),
                    None,
                    molt_core::ChannelRef::Group,
                    molt_core::ChatKind::System,
                ) {
                    // best-effort, like all chat — never blocks the re-key
                    tracing::warn!(error = %e, "could not post the rejoin notice");
                }
                // 4) dynamic mesh membership: the rejoiner's mesh announce
                // follows on this same recovery queue — accept it for exactly
                // this member (documents/dynamic_mesh.md §3)
                self.recovery_mesh_window.insert(member.to_string());
                tracing::info!(%member, "re-keyed the group, broadcast the commit, sent the welcome");
            }
            Some(Err(e)) => tracing::warn!(%member, error = %e, "MLS re-key failed"),
            None => tracing::warn!(%member, "no runtime MLS group to re-key (state-only)"),
        }
    }

    /// Re-sign this node's standing approvals at the new head+1: an approval
    /// this node already gave (its signature is in the stale set) is a decision
    /// that still stands, only its position moved — so re-express it (the human
    /// is not asked again). Proposals this node did not approve are just cleared.
    fn rebase_pending_approvals(&mut self) {
        let Some(head) = self.chain_head.as_ref() else {
            return;
        };
        let target = head.height + 1;
        let me = self.member();
        let stale: Vec<u64> = self
            .pending_sigs
            .iter()
            .filter(|(_, p)| p.height < target)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            let mine = self
                .pending_sigs
                .get(&id)
                .is_some_and(|p| p.sigs.iter().any(|a| a.member == me));
            self.pending_sigs.remove(&id);
            // only re-sign for proposals still pending that this node approved
            if mine && matches!(self.proposals.get(&id), Some(p) if p.state == ProposalState::Proposed)
            {
                self.chain_sign_and_gossip_approval(id);
            }
        }
    }

    /// Inbound: a peer proposed something (gossip). Record it as pending so it
    /// shows up and can be approved here.
    pub(crate) fn receive_proposed(&mut self, id: u64, surface: Surface, payload: serde_json::Value) {
        self.proposals
            .entry(id)
            .or_insert_with(|| molt_core::ProposalRecord {
                surface,
                payload,
                approvals: 0,
                state: ProposalState::Proposed,
                declined_at: 0,
                declined_by: String::new(),
            });
        self.next_id = self.next_id.max(id + 1);
    }

    /// Inbound: a peer's signed approval (gossip). Collect + try to seal.
    pub(crate) fn receive_approval(&mut self, id: u64, by: &str, height: u64, sig: &str) {
        if sig.is_empty() {
            return;
        }
        self.collect_sig(id, height, by, sig);
        self.try_commit(id);
    }

    /// Inbound: a peer broadcast (or re-served) a committed block. Extend the
    /// single branch when it is the next height, tie-break a contended slot we
    /// already filled, or — when it is ahead of us — buffer it and request the
    /// missing suffix (catch-up).
    pub(crate) fn receive_block(&mut self, block: ChainBlock) {
        let Some(head) = self.chain_head.clone() else {
            // a headless rejoiner (total device loss) bootstraps its chain from
            // the genesis a survivor serves, then drains whatever else arrived
            // first; a non-genesis block is buffered until the genesis lands
            if block.height == 0 {
                self.adopt_chain(vec![block]);
                if self.chain_head.is_some() {
                    self.drain_buffered_blocks();
                }
            } else {
                self.pending_blocks.insert(block.height, block);
            }
            return;
        };
        if block.height == head.height + 1 {
            if self.apply_next_block(block) {
                self.drain_buffered_blocks();
            }
        } else if block.height <= head.height {
            self.tie_break(block);
        } else {
            // a gap: we are behind. Buffer this block and ask the mesh for the
            // blocks we are missing (any survivor re-serves them).
            self.pending_blocks.insert(block.height, block);
            self.request_catchup(head.height + 1);
        }
    }

    /// Verify a block against the current head, append + apply it, and run the
    /// post-apply bookkeeping. Returns whether it was accepted.
    fn apply_next_block(&mut self, block: ChainBlock) -> bool {
        let mut probe = self.chain.clone();
        probe.push(block.clone());
        if verify_chain(&probe).is_err() {
            tracing::warn!(height = block.height, "rejecting an unverifiable inbound block");
            return false;
        }
        if self.append_committed_block(block.clone()) {
            self.after_block_applied(&block);
            // the head advanced — a catch-up request that reached this height is done
            if self.catchup_from.is_some_and(|f| f <= block.height) {
                self.catchup_from = None;
            }
            true
        } else {
            false
        }
    }

    /// Apply buffered catch-up blocks while the next height is available, then
    /// drop any stale buffered blocks at or below the head.
    fn drain_buffered_blocks(&mut self) {
        while let Some(head) = self.chain_head.clone() {
            let next = head.height + 1;
            let Some(block) = self.pending_blocks.remove(&next) else {
                break;
            };
            if !self.apply_next_block(block) {
                break;
            }
        }
        let head_h = self.chain_head.as_ref().map_or(0, |h| h.height);
        self.pending_blocks.retain(|h, _| *h > head_h);
    }

    /// Broadcast a catch-up request for every block from `from` onward (deduped
    /// while the same gap is outstanding). No-op if we cannot be behind.
    pub(crate) fn request_catchup(&mut self, from: u64) {
        if self.chain_head.is_none() || self.catchup_from == Some(from) {
            return;
        }
        self.catchup_from = Some(from);
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::ChainRequest { from_height: from });
        self.record(env);
    }

    /// Serve a peer's catch-up request from our OWN chain: re-broadcast every
    /// block we hold from `from` onward (as `Committed`, re-authored so the
    /// outbox fans it out). A single survivor thus reconstitutes the chain for
    /// everyone — independent of who originally committed each block.
    pub(crate) fn serve_chain_from(&mut self, from: u64) {
        let blocks: Vec<ChainBlock> = self
            .chain
            .iter()
            .filter(|b| b.height >= from)
            .cloned()
            .collect();
        if blocks.is_empty() {
            return;
        }
        let me = self.member();
        for block in blocks {
            let env = self.make_env(me.clone(), WorkspaceEvent::Committed(block));
            self.record(env);
        }
    }

    /// The event bodies a catch-up answer re-gossips (WP2): per OPEN surface
    /// proposal a regular `Proposed` plus every already-collected `Approved`
    /// signature — verbatim and position-bound (`(id, by, height, sig)`),
    /// nothing is re-signed. Pure so the unit test pins the batch;
    /// [`State::serve_open_governance`] puts it on the wire. Membership
    /// proposals (recovery) are deliberately absent: their window is
    /// mesh-liveness-bound and their tickets are in-memory by design.
    pub(crate) fn open_governance_events(&self) -> Vec<WorkspaceEvent> {
        let mut events = Vec::new();
        let mut open: Vec<(&u64, &molt_core::ProposalRecord)> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .collect();
        // deterministic order (the map is a HashMap): by id
        open.sort_by_key(|(id, _)| **id);
        for (id, p) in open {
            events.push(WorkspaceEvent::Proposed {
                id: ProposalId(*id),
                surface: p.surface,
                payload: p.payload.clone(),
            });
            if let Some(pending) = self.pending_sigs.get(id) {
                for a in &pending.sigs {
                    events.push(WorkspaceEvent::Approved {
                        id: ProposalId(*id),
                        by: a.member.clone(),
                        height: pending.height,
                        sig: a.sig.clone(),
                    });
                }
            }
        }
        events
    }

    /// Answer a peer's catch-up request with the OPEN governance state, the
    /// ephemeral twin of [`State::serve_chain_from`]: a reopened member lost
    /// the Proposed/Approved gossip with its RAM (deliberately unpersisted —
    /// the chain's ephemeral-until-block boundary), so whoever serves the
    /// chain suffix re-serves the open proposals too. Re-gossip of identical
    /// events is idempotent on every receiver (`receive_proposed` or-inserts,
    /// `collect_sig` keeps one signature per member, `try_commit` refuses
    /// decided proposals), so several answering peers converge harmlessly.
    pub(crate) fn serve_open_governance(&mut self) {
        let events = self.open_governance_events();
        let me = self.member();
        for body in events {
            let env = self.make_env(me.clone(), body);
            self.record(env);
        }
    }

    /// Resolve a competing block at a slot we already filled: identical block →
    /// a duplicate broadcast, ignore; a different block at the tip with a
    /// smaller hash wins the single branch, so adopt it and re-base the
    /// displaced proposal. A deeper conflict is logged (deep reorg is Phase 3).
    fn tie_break(&mut self, block: ChainBlock) {
        let Some(existing) = self.chain.iter().find(|b| b.height == block.height) else {
            return;
        };
        if existing == &block {
            return; // duplicate broadcast of the block we already hold
        }
        let rid = self.republic_id();
        let incoming = molt_storage::content_hash(&block_link_bytes(&rid, &block));
        let current = molt_storage::content_hash(&block_link_bytes(&rid, existing));
        let is_tip = self.chain.last().is_some_and(|b| b.height == block.height);
        if is_tip && incoming < current {
            // the incoming block wins the tip; swap it in and re-verify
            let displaced = self.chain.pop();
            self.chain.push(block.clone());
            if verify_chain(&self.chain).is_ok() {
                if let Ok(head) = verify_chain(&self.chain) {
                    self.chain_head = Some(head);
                }
                self.apply_chain_to_state();
                let chain = self.chain.clone();
                if let Some(active) = &self.active {
                    active.handle.persist_chain_blocking(chain);
                }
                // the displaced proposal returns to pending and re-bases
                if let Some(ChainChange::Applied { proposal_id, .. }) =
                    displaced.as_ref().map(|b| &b.change)
                {
                    if let Some(p) = self.proposals.get_mut(proposal_id) {
                        p.state = ProposalState::Proposed;
                    }
                }
                self.after_block_applied(&block);
            } else {
                // revert — should not happen for a verified block
                self.chain.pop();
                if let Some(b) = displaced {
                    self.chain.push(b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{ChainChange, MembershipOp, Surface};
    use molt_storage::{derive_identity_key, identity_sign, SigningKey};
    use serde_json::json;

    /// A minimal chain builder: derives each member's identity key from a seed,
    /// seals the genesis with everyone (n-of-n) and appends later blocks signed
    /// by a chosen subset — exactly what the real founding + threshold path
    /// will produce.
    struct Builder {
        republic_id: String,
        keys: Vec<(String, SigningKey)>,
        blocks: Vec<ChainBlock>,
        head_hash: String,
    }

    impl Builder {
        fn new(members: &[&str], rule_m: u8) -> Builder {
            let mut keys: Vec<(String, SigningKey)> = Vec::new();
            let mut identities: Vec<MemberIdentity> = Vec::new();
            for (i, m) in members.iter().enumerate() {
                let seed = [u8::try_from(i + 1).unwrap_or(1); 32];
                let (sk, pk) = derive_identity_key(&seed, m);
                identities.push(MemberIdentity {
                    member: (*m).to_string(),
                    identity_pk: pk,
                });
                keys.push(((*m).to_string(), sk));
            }
            let rule_n = u8::try_from(members.len()).expect("small roster");
            let republic_id = molt_storage::republic_id("Chess Club", rule_m, rule_n, &identities);
            let change = ChainChange::Genesis {
                name: "Chess Club".to_string(),
                republic_id: republic_id.clone(),
                rule_m,
                rule_n,
                identities: identities.clone(),
                agenda: "play chess".to_string(),
            };
            let mut b = Builder {
                republic_id: republic_id.clone(),
                keys,
                blocks: Vec::new(),
                head_hash: GENESIS_PREV.to_string(),
            };
            // genesis is unanimous
            let all: Vec<&str> = members.to_vec();
            let block = b.seal(0, change, &all);
            b.push(block);
            b
        }

        /// Sign `change` at `height` with each named member and return the block.
        fn seal(&self, height: u64, change: ChainChange, signers: &[&str]) -> ChainBlock {
            let bytes = approval_bytes(&self.republic_id, height, &change);
            let sigs = signers
                .iter()
                .map(|name| {
                    let (_, sk) = self
                        .keys
                        .iter()
                        .find(|(m, _)| m == name)
                        .expect("known signer");
                    RosterAttestation {
                        member: (*name).to_string(),
                        sig: identity_sign(sk, &bytes),
                    }
                })
                .collect();
            ChainBlock {
                height,
                prev: self.head_hash.clone(),
                change,
                sigs,
            }
        }

        fn push(&mut self, block: ChainBlock) {
            self.head_hash = block_hash(&self.republic_id, &block);
            self.blocks.push(block);
        }

        /// Commit a gated Applied change signed by `signers` at the next height.
        fn commit_applied(&mut self, proposal_id: u64, signers: &[&str]) {
            let height = u64::try_from(self.blocks.len()).expect("small chain");
            let change = ChainChange::Applied {
                proposal_id,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "id": proposal_id }),
            };
            let block = self.seal(height, change, signers);
            self.push(block);
        }

        /// A member's signing key.
        fn key(&self, member: &str) -> &SigningKey {
            &self
                .keys
                .iter()
                .find(|(m, _)| m == member)
                .expect("known member")
                .1
        }

        /// A member's anchored identity pk (from the genesis roster).
        fn pk(&self, member: &str) -> String {
            let ChainChange::Genesis { identities, .. } = &self.blocks[0].change else {
                panic!("block 0 is not a genesis");
            };
            identities
                .iter()
                .find(|i| i.member == member)
                .expect("anchored member")
                .identity_pk
                .clone()
        }
    }

    #[test]
    fn genesis_then_applied_verifies() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        let head = verify_chain(&b.blocks).expect("valid chain verifies");
        assert_eq!(head.height, 1);
        assert_eq!(head.rule_m, 2);
        assert_eq!(head.identities.len(), 3);
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        // rewrite the applied payload without re-signing
        if let ChainChange::Applied { payload, .. } = &mut b.blocks[1].change {
            *payload = json!({ "op": "add_note", "id": 999 });
        }
        assert!(verify_chain(&b.blocks).is_err(), "signatures cover the payload");
    }

    #[test]
    fn a_broken_prev_link_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].prev = GENESIS_PREV.to_string();
        assert!(verify_chain(&b.blocks).is_err(), "the chain link is broken");
    }

    #[test]
    fn below_threshold_approvals_are_rejected() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(1, &["petra"]); // only 1 of the required 2
        assert!(verify_chain(&b.blocks).is_err(), "one approval is below m=2");
    }

    #[test]
    fn a_repeated_signature_does_not_reach_threshold() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        // petra signs, then her attestation is duplicated — still one signer
        b.commit_applied(1, &["petra"]);
        let dup = b.blocks[1].sigs[0].clone();
        b.blocks[1].sigs.push(dup);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "one member signing twice is still one approver"
        );
    }

    #[test]
    fn applying_a_proposal_twice_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        b.commit_applied(7, &["petra", "walter"]); // same proposal id again
        assert!(verify_chain(&b.blocks).is_err(), "no double-apply");
    }

    #[test]
    fn a_height_gap_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        b.commit_applied(1, &["petra", "walter"]);
        b.blocks[1].height = 5; // signatures are height-bound, so this also fails the sig check
        assert!(verify_chain(&b.blocks).is_err(), "heights must be gapless");
    }

    #[test]
    fn a_forged_genesis_id_is_rejected() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        if let ChainChange::Genesis { republic_id, .. } = &mut b.blocks[0].change {
            *republic_id = "deadbeef".to_string();
        }
        assert!(
            verify_chain(&b.blocks).is_err(),
            "the republic id must match the roster content"
        );
    }

    #[test]
    fn a_membership_block_grows_the_roster_and_lets_the_newcomer_approve() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        // add dora with her own derived identity key
        let (dora_sk, dora_pk) = derive_identity_key(&[9u8; 32], "dora");
        let height = u64::try_from(b.blocks.len()).expect("small chain");
        let join = ChainChange::Membership {
            op: MembershipOp::Joined,
            member: "dora".to_string(),
            identity_pk: dora_pk,
        };
        let block = b.seal(height, join, &["petra", "walter"]);
        b.push(block);
        b.keys.push(("dora".to_string(), dora_sk));
        // now an Applied block signed by dora + walter must count dora
        b.commit_applied(1, &["dora", "walter"]);
        let head = verify_chain(&b.blocks).expect("newcomer approval counts");
        assert_eq!(head.identities.len(), 3);
        assert_eq!(head.height, 2);
    }

    /// A member that only holds the genesis receives a peer's broadcast commit
    /// block, verifies + adopts it, and its persistent state converges (the
    /// `receive_block` path that a non-committer follows).
    /// WP1: the chain projection feeds the snapshot's parallel id track —
    /// a committed `Applied` block's `proposal_id` reaches the read contract
    /// positionally next to its payload.
    #[test]
    fn chain_applied_entries_carry_their_proposal_id() {
        let mut b = Builder::new(&["petra", "walter", "dora"], 2);
        b.commit_applied(7, &["petra", "walter"]);
        let mut peer = crate::tests::plain_state();
        peer.adopt_chain(b.blocks.clone());
        let snap = peer.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied.len(), 1, "one committed Applied block");
        assert_eq!(
            snap.applied_ids,
            vec![Some(7)],
            "the block's proposal id rides the id track"
        );
    }

    #[test]
    fn a_peer_adopts_a_broadcast_block_and_converges() {
        let b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        // a block committed elsewhere: an Applied change signed by both members
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "minutes" }),
        };
        let block = b.seal(1, change, &["petra", "walter"]);

        // walter holds only the genesis, then the block arrives over the mesh
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(), // adopt_chain fills these from the verified head
            agenda: "play chess".to_string(),
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(genesis);
        assert!(peer.is_chain_governed());
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        peer.receive_block(block);
        assert_eq!(peer.chain.len(), 2, "the peer adopted the broadcast block");
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 1);
        let applied = peer
            .chain_applied
            .get(&Surface::Memory)
            .expect("memory projection");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, Some(1), "the projection keeps the proposal id");
        assert_eq!(applied[0].1["title"], json!("minutes"));

        // an invalid block (tampered payload, sigs no longer match) is rejected
        let mut forged = b.seal(
            2,
            ChainChange::Applied {
                proposal_id: 2,
                surface: Surface::Memory,
                payload: json!({ "op": "add_note", "title": "real" }),
            },
            &["petra", "walter"],
        );
        forged.prev = peer.chain_head.as_ref().expect("head").hash.clone();
        if let ChainChange::Applied { payload, .. } = &mut forged.change {
            *payload = json!({ "op": "add_note", "title": "forged" });
        }
        peer.receive_block(forged);
        assert_eq!(peer.chain.len(), 2, "a tampered block is hard-rejected");
    }

    /// A 2-member chain-governed peer holding only the genesis `b` roots.
    fn chain_peer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut peer = crate::tests::plain_state();
        peer.replica = Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: member.to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            republic_id: b.republic_id.clone(),
            founded_ts: 0,
        });
        peer.adopt_chain(chain);
        peer
    }

    /// A block arriving ahead of our head is buffered, then applied once the
    /// gap fills — catch-up converges regardless of arrival order.
    #[test]
    fn out_of_order_blocks_buffer_and_converge() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut peer = chain_peer("walter", &b, genesis);
        // the height-2 block arrives first — a gap, so it is buffered
        peer.receive_block(block2);
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            0,
            "a gap block is buffered, not applied"
        );
        assert_eq!(peer.pending_blocks.len(), 1);
        // the height-1 block fills the gap; the buffered height-2 drains behind it
        peer.receive_block(block1);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 2);
        assert!(peer.pending_blocks.is_empty(), "the buffer drained");
    }

    /// One survivor holding the full chain re-serves a lagging member the whole
    /// missing suffix — the resilience property (any survivor suffices), and the
    /// suffix applies even delivered out of order.
    #[test]
    fn a_survivor_serves_a_lagging_member_the_full_suffix() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis = b.blocks.clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let full = b.blocks.clone();

        let mut peer = chain_peer("walter", &b, genesis);
        assert_eq!(peer.chain_head.as_ref().expect("head").height, 0);

        // the survivor serves every block from the peer's head+1 (=1) onward,
        // straight out of its own chain — exactly what serve_chain_from does
        let served: Vec<ChainBlock> = full.iter().filter(|bl| bl.height >= 1).cloned().collect();
        assert_eq!(served.len(), 2, "survivor serves b1 + b2 from its chain");
        for bl in served.into_iter().rev() {
            peer.receive_block(bl); // delivered newest-first to exercise buffering
        }
        assert_eq!(
            peer.chain_head.as_ref().expect("head").height,
            2,
            "the lagging member caught up to the survivor"
        );
        assert!(peer.pending_blocks.is_empty());
    }

    /// WP2 pin: the catch-up re-gossip relies on the receive side being
    /// idempotent — a duplicated `Proposed` stays ONE pending entry, a
    /// duplicated `Approved` stays ONE signature per member, and neither
    /// resurrects a proposal whose block already committed.
    #[test]
    fn regossiped_proposals_and_approvals_are_idempotent() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });

        // a re-gossiped Proposed lands once
        walter.receive_proposed(1, Surface::Memory, payload.clone());
        walter.receive_proposed(1, Surface::Memory, payload.clone());
        let pending: Vec<_> = walter
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .collect();
        assert_eq!(pending.len(), 1, "one entry, not two");

        // a re-gossiped Approved lands as ONE signature for that member
        let change = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: payload.clone(),
        };
        let bytes = approval_bytes(&b.republic_id, 1, &change);
        let petra_sig = identity_sign(b.key("petra"), &bytes);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        let sigs = &walter.pending_sigs.get(&1).expect("pending set").sigs;
        assert_eq!(sigs.len(), 1, "one signature per member: {sigs:?}");

        // walter co-signs — the block seals at 2-of-3
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "the proposal committed"
        );

        // LATE re-gossip (another answering peer) must not resurrect it
        walter.receive_proposed(1, Surface::Memory, payload);
        walter.receive_approval(1, "petra", 1, &petra_sig);
        assert!(
            matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
            "a committed proposal stays committed"
        );
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "no second block for the same proposal"
        );
    }

    /// WP2: whoever answers a `ChainRequest` also re-serves the OPEN
    /// governance state — per open proposal a regular `Proposed` plus the
    /// already-collected `Approved` signatures (verbatim, position-bound —
    /// nothing is re-signed). A reopened member replays those through its
    /// normal receive arms and can then co-sign; the block seals at m.
    #[test]
    fn a_catchup_answer_reserves_open_governance() {
        let b = Builder::new(&["petra", "walter"], 2);
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let payload = json!({ "op": "add_note", "title": "minutes" });
        petra
            .cmd_propose(Surface::Memory, payload.clone())
            .expect("petra proposes");

        // what petra's catch-up answer re-gossips: the open proposal and
        // her own collected co-signature
        let bodies = petra.open_governance_events();
        let (mut saw_proposed, mut relayed_sig) = (false, None);
        for body in &bodies {
            match body {
                WorkspaceEvent::Proposed { id, surface, payload: p } => {
                    assert_eq!((id.0, *surface), (1, Surface::Memory));
                    assert_eq!(p, &payload, "the payload rides unchanged");
                    saw_proposed = true;
                }
                WorkspaceEvent::Approved { id, by, height, sig } => {
                    assert_eq!((id.0, by.as_str(), *height), (1, "petra", 1));
                    relayed_sig = Some(sig.clone());
                }
                other => panic!("unexpected re-gossip event: {other:?}"),
            }
        }
        assert!(saw_proposed, "the open proposal is re-served");
        let relayed_sig = relayed_sig.expect("petra's collected signature is re-served");

        // walter — the reopened member: RAM lost the gossip, the chain has
        // only the genesis. The re-gossip restores proposal + count, then
        // his own co-signature seals the block (2-of-2).
        let mut walter = chain_signer("walter", &b, b.blocks.clone());
        walter.receive_proposed(1, Surface::Memory, payload);
        walter.receive_approval(1, "petra", 1, &relayed_sig);
        assert_eq!(
            walter.pending_sigs.get(&1).map(|s| s.sigs.len()),
            Some(1),
            "the reopened member sees the collected approval count"
        );
        walter.chain_sign_and_gossip_approval(1);
        assert_eq!(
            walter.chain_head.as_ref().expect("head").height,
            1,
            "the recovered proposal is fully approvable — the block seals"
        );
    }

    /// A chain-governed member that can also SIGN (holds its identity key).
    fn chain_signer(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
        let mut s = chain_peer(member, b, chain);
        s.identity_sk = Some(b.key(member).clone());
        s
    }

    /// Re-admission (recovery step ❹): a survivor proposes a `Membership{Restored}`
    /// change and, once the threshold of members has signed it (here + "over the
    /// mesh"), a Restored block seals — the group's threshold-gated authorization
    /// of a returning member. Recovery keeps the same anchored identity key.
    #[test]
    fn a_threshold_restored_block_re_admits_a_member() {
        let b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let mut petra = chain_signer("petra", &b, b.blocks.clone());
        let mut walter = chain_signer("walter", &b, b.blocks.clone());

        // petra proposes re-admitting walter and co-signs (1 of 2 — pending)
        let id = petra.propose_membership(MembershipOp::Restored, "walter", &walter_pk);
        assert_eq!(
            petra.chain_head.as_ref().expect("head").height,
            0,
            "one signature does not re-admit"
        );

        // walter learns the proposal + petra's signature, then co-signs
        walter.receive_membership_proposal(id, MembershipOp::Restored, "walter", &walter_pk);
        let petra_sig = petra
            .pending_sigs
            .get(&id)
            .expect("petra's pending set")
            .sigs
            .iter()
            .find(|a| a.member == "petra")
            .expect("petra signed")
            .sig
            .clone();
        walter.receive_approval(id, "petra", 1, &petra_sig);
        walter.chain_sign_and_gossip_approval(id);

        // the Restored block seals at 2-of-2
        let head = walter.chain_head.as_ref().expect("head");
        assert_eq!(head.height, 1);
        assert!(
            matches!(
                walter.chain.last().expect("block").change,
                ChainChange::Membership {
                    op: MembershipOp::Restored,
                    ..
                }
            ),
            "the sealed block re-admits the member"
        );
    }

    /// Recovery step ❸: a coordinator re-admits a returning member ONLY on a
    /// valid seat proof against the anchored identity — a forged proof, or a
    /// request that would re-key to a different identity, is refused. A pass
    /// proposes the threshold Restored block.
    #[test]
    fn a_coordinator_re_admits_only_a_valid_seat_proof() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        let rid = b.republic_id.clone();
        let ticket = "recovery-ticket-xyz";
        let kp_hex = "beef";

        // the returning member (dora) signs the seat proof with its OWN key
        let good = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid);
        let id = coord
            .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &good, "")
            .expect("a valid seat proof re-admits");
        assert!(matches!(
            coord.proposal_changes.get(&id),
            Some(ChainChange::Membership {
                op: MembershipOp::Restored,
                ..
            })
        ));
        // a verified request registers the pending recovery (the MLS re-key
        // consumes it the moment the block commits — even synchronously)
        assert!(coord.pending_recovery.contains_key("dora"));

        // a proof signed by the WRONG key (petra forging dora's) is rejected
        let forged = crate::make_seat_proof(b.key("petra"), ticket, kp_hex, &rid);
        assert!(coord
            .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &forged, "")
            .is_err());

        // a request that re-keys the seat to a DIFFERENT identity is rejected —
        // recovery re-derives the SAME key
        assert!(coord
            .verify_and_propose_restore("dora", &b.pk("walter"), kp_hex, ticket, &good, "")
            .is_err());
    }

    /// When a `Restored` block commits, the coordinator (the node holding the
    /// pending recovery for that member) consumes it to drive the MLS re-key;
    /// a node without a pending recovery for that member does nothing. Here
    /// there is no runtime group, so the re-key is a logged no-op — but the
    /// trigger CONDITION (consume the pending recovery on commit) is exercised.
    #[test]
    fn a_restored_commit_triggers_the_coordinators_rekey() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut coord = chain_signer("petra", &b, b.blocks.clone());
        coord.pending_recovery.insert(
            "walter".to_string(),
            PendingRecovery {
                member: "walter".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );

        // build a Restored block for walter and hand it to the coordinator
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        coord.receive_block(block);

        assert_eq!(coord.chain_head.as_ref().expect("head").height, 1);
        assert!(
            !coord.pending_recovery.contains_key("walter"),
            "the coordinator consumed the pending recovery on the Restored commit"
        );
    }

    /// **Re-mint failover (decision A1, 2026-07-11), chain level.** When the
    /// recovery coordinator dies, any survivor mints a NEW recovery link and a
    /// complete second recovery round runs — producing a SECOND `Restored`
    /// block for the SAME seat. The chain must accept it: same anchored
    /// `identity_pk` at two consecutive heights (only the MLS leaf re-keys
    /// again; the roster identity never moves). Counter-assertion: a `Restored`
    /// block that re-keys the roster identity to a DIFFERENT key is rejected
    /// (`recovery_ritual.md` §6 — rotation is out of scope; the coordinator's
    /// refusal to *propose* such a change is pinned separately in
    /// `a_coordinator_re_admits_only_a_valid_seat_proof`).
    #[test]
    fn a_second_restored_block_for_the_same_seat_verifies() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let walter_pk = b.pk("walter");
        let restored = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk.clone(),
        };
        // round 1: the first recovery attempt's Restored block commits …
        let block = b.seal(1, restored.clone(), &["petra", "walter"]);
        b.push(block);
        // … then the coordinator dies; the re-mint failover runs a COMPLETE
        // second round: a second Restored block for the same seat, same key
        let block = b.seal(2, restored, &["petra", "walter"]);
        b.push(block);
        let head = verify_chain(&b.blocks).expect("two Restored blocks for one seat verify");
        assert_eq!(head.height, 2);
        assert_eq!(
            head.identities
                .iter()
                .find(|i| i.member == "walter")
                .expect("walter stays anchored")
                .identity_pk,
            walter_pk,
            "recovery re-keys the MLS leaf, never the roster identity"
        );

        // counter: a threshold of survivors must NOT be able to swap the seat
        // to a different identity key via a Restored block — hard-reject
        let (_, other_pk) = derive_identity_key(&[42u8; 32], "walter");
        let hijack = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: other_pk,
        };
        let block = b.seal(3, hijack, &["petra", "walter"]);
        b.push(block);
        assert!(
            verify_chain(&b.blocks).is_err(),
            "a Restored block with a non-anchored identity key must be rejected"
        );
    }

    /// **Re-mint failover, engine level: a survivor (or a restarted, amnesiac
    /// coordinator) adopting a committed `Restored` block it holds NO pending
    /// recovery for is inert.** The chain extends normally, but
    /// `coordinator_rekey` never runs: nothing is recorded (no
    /// `WorkspaceEvent::MlsCommit` broadcast), the mesh window is not armed,
    /// and a pending recovery for a DIFFERENT member is left untouched. This
    /// is the crash-before-re-key case: the block committed, the coordinator
    /// died, and the re-mint failover's second round supplies the re-key.
    #[test]
    fn a_restored_commit_without_a_pending_recovery_is_inert() {
        let b = Builder::new(&["petra", "walter", "dora"], 2);
        let walter_pk = b.pk("walter");
        let mut node = chain_signer("petra", &b, b.blocks.clone());
        // a pending recovery for ANOTHER member must survive walter's commit
        node.pending_recovery.insert(
            "dora".to_string(),
            PendingRecovery {
                member: "dora".to_string(),
                key_package: "beef".to_string(),
                reply: String::new(),
            },
        );
        let seq_before = node.next_seq;

        // a Restored block for walter — committed elsewhere — arrives; this
        // node holds no pending recovery for walter
        let change = ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "walter".to_string(),
            identity_pk: walter_pk,
        };
        let block = b.seal(1, change, &["petra", "walter"]);
        node.receive_block(block);

        // the chain extends …
        assert_eq!(node.chain_head.as_ref().expect("head").height, 1);
        // … but the re-key trigger stayed inert: no envelope of any kind was
        // recorded (make_env is the only seq stamp, so an MlsCommit broadcast
        // or a chat notice would have advanced next_seq) …
        assert_eq!(node.next_seq, seq_before, "no MlsCommit/notice was recorded");
        // … the recovery mesh window was never armed …
        assert!(node.recovery_mesh_window.is_empty());
        // … and only walter's (absent) entry was consulted — dora's pending
        // recovery is untouched
        assert!(node.pending_recovery.contains_key("dora"));
    }

    /// A rejoiner that lost everything (no chain, no head) bootstraps from the
    /// genesis a survivor serves and then catches up the whole chain — even when
    /// later blocks arrive before the genesis (they buffer until it lands). The
    /// state-recovery core of Phase 4.
    #[test]
    fn a_headless_rejoiner_bootstraps_from_a_served_genesis() {
        let mut b = Builder::new(&["petra", "walter"], 2);
        let genesis_block = b.blocks[0].clone();
        b.commit_applied(1, &["petra", "walter"]);
        b.commit_applied(2, &["petra", "walter"]);
        let block1 = b.blocks[1].clone();
        let block2 = b.blocks[2].clone();

        let mut rejoiner = crate::tests::plain_state();
        assert!(!rejoiner.is_chain_governed());

        // a block arrives before the genesis — buffered, still headless
        rejoiner.receive_block(block2);
        assert!(!rejoiner.is_chain_governed());
        assert_eq!(rejoiner.pending_blocks.len(), 1);

        // the survivor serves the genesis — adopt it as the root
        rejoiner.receive_block(genesis_block);
        assert!(rejoiner.is_chain_governed(), "adopted the served genesis");
        assert_eq!(rejoiner.chain_head.as_ref().expect("head").height, 0);

        // the middle block fills the gap; the buffered tail drains behind it
        rejoiner.receive_block(block1);
        assert_eq!(
            rejoiner.chain_head.as_ref().expect("head").height,
            2,
            "the rejoiner caught up the full chain from genesis"
        );
        assert!(rejoiner.pending_blocks.is_empty());
    }
}
