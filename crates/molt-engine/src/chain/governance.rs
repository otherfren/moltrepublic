// SPDX-License-Identifier: GPL-3.0-or-later

//! **Live threshold governance over the transport**: the ephemeral
//! signature collection ([`PendingApproval`]), signing + gossiping an
//! approval, collecting and verifying peers' signatures, sealing a block
//! at m (deterministic: the m lowest-named valid signers), the post-apply
//! bookkeeping every block runs (`after_block_applied`), the re-base of
//! standing approvals onto a new head, the holder's cached incremental
//! verification (`extend_own`), the inbound `Proposed`/`Approved` gossip
//! and the open-governance re-serve. The chain write (`persist_chain_now`)
//! lives here too: once per accepted batch, never per block.

use super::*;

/// The **ephemeral** signature collection for one pending proposal on a
/// chain-governed republic (never persisted; rebuilt from gossip). The
/// committer bundles these into a block once `sigs` reaches the threshold. A
/// re-base (the head advanced past `height`) clears it and re-signs.
/// L3: open cards one proposer may hold at once — a flooding member can
/// only crowd itself (the shed card is re-earned by the WP2 re-serve).
pub(super) const OPEN_CARDS_PER_PROPOSER_MAX: usize = 64;

#[derive(Debug, Clone, Default)]
pub(crate) struct PendingApproval {
    /// The chain height every signature here is bound to.
    pub height: u64,
    /// One signature per distinct member (latest wins).
    pub sigs: Vec<RosterAttestation>,
    /// Members whose CURRENT signature verified against the live target's
    /// approval bytes (L2): the DISPLAY reads only these — a raw collected
    /// sig could paint a forged stance onto a named seat. `try_commit`
    /// keeps its own authoritative filter; a sig unverifiable YET (its
    /// card has not landed) stays collected and is re-checked on arrival.
    pub verified: std::collections::BTreeSet<String>,
}

impl State {
    /// A workspace whose governance runs through the chain (real m-of-n
    /// signatures) rather than the single-operator path.
    pub(crate) fn is_chain_governed(&self) -> bool {
        self.chain_head.is_some()
    }

    /// The committed change a pending proposal would enact. A registered change
    /// (any kind — e.g. a `Membership` re-admission) wins; otherwise it is a
    /// gated `Applied` reconstructed from the surface proposal.
    pub(super) fn proposal_change(&self, id: u64) -> Option<ChainChange> {
        if let Some(change) = self.proposal_changes.get(&id) {
            return Some(change.clone());
        }
        let p = self.proposals.get(&id)?;
        // a MEMBERSHIP record without its registered chain change must not
        // fall through to the Applied shape — an approve would then sign a
        // fabricated surface transition instead of the membership bytes
        // everyone else signs (the reserved ops below never pass
        // `validate_org_payload`, so no user proposal can wear them)
        if matches!(
            p.payload.get("op").and_then(serde_json::Value::as_str),
            Some("restore_member" | "add_member")
        ) {
            return None;
        }
        Some(ChainChange::Applied {
            proposal_id: id,
            surface: p.surface,
            payload: p.payload.clone(),
        })
    }

    /// Whether `id` may register `change`: free unless it already names a
    /// surface proposal (`self.proposals`) or a *different* pending chain
    /// change. Re-gossip of the identical change is idempotent (true). The
    /// shared collision guard for every peer-chosen proposal id
    /// (`receive_proposed` / `receive_membership_proposal` /
    /// `receive_checkpoint_proposal`) — see the security note there.
    pub(crate) fn id_free_for(&self, id: u64, change: &ChainChange) -> bool {
        // the identical change re-gossiped is idempotent — checked FIRST,
        // because a membership proposal now also owns a ProposalRecord under
        // its id (the approval surface), and reading that record as a
        // collision would refuse the legitimate re-serve of the very change
        // it belongs to
        if let Some(existing) = self.proposal_changes.get(&id) {
            return existing == change;
        }
        match self.proposals.get(&id) {
            None => true,
            // a membership RECORD may precede its chain-side registration —
            // the log applier runs first in the same ingest turn. It is the
            // same proposal, not a collision, exactly when the record wears
            // this change's reserved op + member (a record can never wear
            // them via cmd_propose — validate_org_payload knows no such op).
            // The threshold stays the security gate, as it always was for
            // membership gossip.
            Some(p) => {
                let ChainChange::Membership { op, member, .. } = change else {
                    return false;
                };
                let want = match op {
                    MembershipOp::Restored => "restore_member",
                    MembershipOp::Joined => "add_member",
                };
                p.surface == Surface::Organization
                    && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(want)
                    && p.payload.get("member").and_then(serde_json::Value::as_str)
                        == Some(member.as_str())
            }
        }
    }

    /// Distinct collected approvals for a proposal (for the UI progress).
    pub(crate) fn chain_approval_count(&self, id: u64) -> usize {
        // L2: the DISPLAYED count is the verified one — raw collected sigs
        // could inflate progress with junk a peer gossiped
        self.pending_sigs.get(&id).map(|p| p.verified.len()).unwrap_or(0)
    }

    /// The replay guard every AUTOMATIC co-sign runs (a consented restore,
    /// a matching checkpoint cut): does this node already hold its OWN
    /// signature for `id` at the current target height? A re-received frame
    /// must not amplify into fresh `Approved` gossip. Headless (no chain
    /// head) there is no target, hence nothing standing.
    pub(super) fn own_signature_stands(&self, id: u64) -> bool {
        let Some(head) = self.chain_head.as_ref() else {
            return false;
        };
        let me = self.member();
        let target = head.height + 1;
        self.pending_sigs
            .get(&id)
            .is_some_and(|p| p.height == target && p.sigs.iter().any(|a| a.member == me))
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
        // the decision register: the ONLY writer (see `rebase_pending_approvals`)
        self.own_approvals.insert(id);
        // the own signature is genuine by construction (L2)
        self.collect_sig(id, height, &me, &sig, true);
        if let Some(p) = self.pending_sigs.get_mut(&id) {
            p.verified.insert(me.clone());
        }
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

    /// D6: keep the collected voter set as record-side DISPLAY data before
    /// the ephemeral signatures are cleared at a seal — each holder shows
    /// the voices that reached IT (over-subscribed voters included); the
    /// block's m signatures stay the only chain truth.
    pub(super) fn stash_voted(&mut self, id: u64) {
        let members: Vec<molt_core::MemberId> = self
            .pending_sigs
            .get(&id)
            .map(|s| s.sigs.iter().map(|a| a.member.clone()).collect())
            .unwrap_or_default();
        if members.is_empty() {
            return;
        }
        if let Some(p) = self.proposals.get_mut(&id) {
            for m in members {
                if !p.voted.contains(&m) {
                    p.voted.push(m);
                }
            }
        }
    }

    /// Drop the ephemeral vote bookkeeping of a DECIDED proposal id: its
    /// collected signatures (the voter set is stashed for the display
    /// first) and its registered chain change. Idempotent.
    pub(super) fn forget_vote(&mut self, id: u64) {
        self.stash_voted(id);
        self.pending_sigs.remove(&id);
        self.proposal_changes.remove(&id);
    }

    /// [`State::forget_vote`] for a sealed change that carries no proposal
    /// id (a Membership or Checkpoint block): every registered entry with
    /// exactly this content is decided — the sealer (which also cleans by
    /// id), every passive applier and a catch-up all settle identically.
    pub(super) fn forget_votes_for(&mut self, sealed: &ChainChange) {
        let ids: Vec<u64> = self
            .proposal_changes
            .iter()
            .filter(|(_, c)| *c == sealed)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.forget_vote(id);
        }
    }

    /// Collect one signature into a proposal's pending set: dedup by member,
    /// and rebase the set to a newer `height` (dropping stale signatures) —
    /// a signature for an already-superseded height is ignored. A TERMINAL
    /// card collects nothing (D6): a post-seal approval must not resurrect
    /// the ephemeral set the seal just cleared.
    ///
    /// `verified` is the caller's verdict on THIS signature. Among two for
    /// one member the later one wins — unless the held one verifies and the
    /// new one does not: junk under a roster name must never evict a
    /// genuine approval (one insider could otherwise stall every vote), and
    /// under the OWN name an unverified signature is never held at all.
    fn collect_sig(&mut self, id: u64, height: u64, member: &str, sig: &str, verified: bool) {
        if self
            .proposals
            .get(&id)
            .is_some_and(|p| p.state != ProposalState::Proposed)
        {
            return;
        }
        // L3: only roster members' signatures collect — dedup is by the
        // free-form member string, so distinct fake names grew one Vec
        // without bound. Roster membership (not link identity) is the rule:
        // the WP2 re-serve legitimately relays other members' signatures.
        if !self
            .chain_head
            .as_ref()
            .is_some_and(|h| h.identities.iter().any(|i| i.member == member))
        {
            return;
        }
        if !verified && member == self.member() {
            return;
        }
        let entry = self.pending_sigs.entry(id).or_default();
        if height > entry.height {
            entry.height = height;
            entry.sigs.clear();
            entry.verified.clear();
        } else if height < entry.height {
            return;
        }
        if !verified && entry.verified.contains(member) {
            return;
        }
        entry.sigs.retain(|a| a.member != member);
        // the REPLACED signature's verdict must not survive the replacement
        entry.verified.remove(member);
        entry.sigs.push(RosterAttestation {
            member: member.to_string(),
            sig: sig.to_string(),
        });
    }

    /// L2: does this (member, sig) verify against the LIVE target's
    /// approval bytes? Checkable only when the head exists, the height is
    /// the current target and the change is registered here — anything
    /// else is "not verifiable yet", which callers treat as not-displayed
    /// rather than dropped (liveness: an approval may outrun its card).
    fn approval_verifies(&self, id: u64, height: u64, member: &str, sig: &str) -> bool {
        let Some(head) = self.chain_head.as_ref() else {
            return false;
        };
        if height != head.height + 1 {
            return false;
        }
        let Some(change) = self.proposal_change(id) else {
            return false;
        };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        head.identities
            .iter()
            .any(|i| i.member == member && molt_storage::identity_verify(&i.identity_pk, &bytes, sig))
    }

    /// L2: re-check every collected-but-unverified signature of `id` — the
    /// card (or its registered change) just landed, so sigs that outran it
    /// become displayable now.
    pub(crate) fn reverify_pending(&mut self, id: u64) {
        let Some(pending) = self.pending_sigs.get(&id) else {
            return;
        };
        let height = pending.height;
        let candidates: Vec<(String, String)> = pending
            .sigs
            .iter()
            .filter(|a| !pending.verified.contains(&a.member))
            .map(|a| (a.member.clone(), a.sig.clone()))
            .collect();
        for (member, sig) in candidates {
            if self.approval_verifies(id, height, &member, &sig) {
                if let Some(p) = self.pending_sigs.get_mut(&id) {
                    p.verified.insert(member);
                }
            }
        }
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
        // D2 (last vote counts): a CURRENT decliner's signature never
        // counts toward m — a stale re-served sig from a peer that missed
        // the decline must not seal a majority-declined proposal here. (A
        // block sealed elsewhere still wins on arrival; the chain is the
        // record.)
        let current_decliners: Vec<molt_core::MemberId> = self
            .proposals
            .get(&id)
            .map(|p| p.decliners.clone())
            .unwrap_or_default();
        let mut valid: Vec<RosterAttestation> = pending
            .sigs
            .iter()
            .filter(|a| {
                !current_decliners.contains(&a.member)
                    && head.identities.iter().any(|i| {
                        i.member == a.member
                            && molt_storage::identity_verify(&i.identity_pk, &bytes, &a.sig)
                    })
            })
            .cloned()
            .collect();
        valid.sort_by(|a, b| a.member.cmp(&b.member));
        valid.dedup_by(|a, b| a.member == b.member);
        // the restored member's consent is one distinct signer (recovery
        // approval design, 2026-08-08) — the sealer must count EXACTLY like
        // `verify_next`, or it seals blocks the verifiers reject. The consent
        // was validated when the change was registered; the member's own
        // roster signature (it is not on the mesh) cannot legitimately be in
        // `pending`, and dropping it here keeps the distinctness rule the
        // verifier enforces.
        let consented = match &change {
            ChainChange::Membership {
                op: MembershipOp::Restored,
                member,
                consent: Some(_),
                ..
            } => {
                valid.retain(|a| a.member != *member);
                1
            }
            _ => 0,
        };
        let need = usize::from(head.rule_m);
        if valid.len() >= need {
            // enough survivor signatures on their own — the consent still
            // rides the change, but never displaces a survivor's voice
            valid.truncate(need);
        } else if valid.len() + consented < need {
            return;
        }
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
    pub(super) fn adopt_committed_block(&mut self, block: ChainBlock, proposal_id: u64) {
        if !self.append_committed_block(block.clone()) {
            return;
        }
        let durable = self.persist_chain_now();
        self.after_block_applied(&block);
        // clean up the proposal we just committed — a Membership block carries
        // no proposal id for after_block_applied to key on, so drop it here
        self.forget_vote(proposal_id);
        // **H3 second half (total_review.md): broadcast only what is
        // durable.** The block stays appended and projected — the m
        // signatures are real, and the peers seal the byte-identical block
        // from the same approval gossip themselves — but a node whose disk
        // did not take it must not spread it as republic history: after a
        // crash it would be asking the group for the very block it
        // announced. The writer's failed flag turns the next record into
        // the operator's storage-failed notice.
        if !durable {
            tracing::error!(
                height = block.height,
                proposal = proposal_id,
                "sealed block held back from broadcast - not durable; \
                 peers seal it from the gossip themselves"
            );
            return;
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Committed(block.clone()));
        self.record(env);
        // an accepted VOTE posts its summary into its discussion (story
        // 2026-08-09) — minted exactly once, by the sealer (a passively
        // applied broadcast/catch-up block receives this message over the
        // wire instead); sequenced AFTER the Committed envelope, so
        // receivers fold the decision before its notice
        if let ChainChange::Applied { payload, .. } = &block.change {
            let payload = payload.clone();
            self.post_decision_summary(proposal_id, &payload, None);
        }
        tracing::debug!(height = block.height, %proposal_id, "sealed and broadcast a chain block");
        // WP4b automation (2026-07-18): checkpoints trigger themselves —
        // HERE and only here, because reaching adopt_committed_block means
        // THIS node just sealed at the live head with fresh signatures. A
        // passively applied block (apply_next_block: catch-up serve,
        // another sealer's broadcast) must never trigger: a node draining
        // a catch-up would propose at a stale intermediate head, and in a
        // lockstep whole-republic catch-up m nodes could even co-sign that
        // stale cut and fork a holder AFTER it dropped its history. After
        // the re-base above, so a cut this very block staled is swept (and
        // announced stale) before the re-propose at the new head.
        self.maybe_auto_checkpoint();
    }

    /// Write the chain as it stands. Returns whether it is DURABLE — the
    /// seal path gates its broadcast on this (H3 second half); a state
    /// without storage has promised nothing, so it reports `true`.
    ///
    /// **Once per accepted batch, never per block.** The round-trip is
    /// synchronous — `persist_chain_blocking` waits on the writer's ack — so
    /// a catch-up draining N blocks used to sit through N blocking
    /// whole-chain writes inside one uninterruptible actor turn. Losing a
    /// batch to a crash costs a re-fetch and nothing else: any survivor
    /// re-serves the blocks on the next catch-up.
    pub(super) fn persist_chain_now(&self) -> bool {
        #[cfg(test)]
        CHAIN_PERSISTS.with(|c| c.set(c.get() + 1));
        let Some(active) = &self.active else {
            return true;
        };
        let durable = active
            .handle
            .persist_chain_blocking(self.checkpoint_blob.clone(), self.chain.clone());
        if !durable {
            // The writer also raises its `failed` flag, which the next
            // `record` turns into the operator's "storage-failed" notice.
            // Named here as well because THIS is the write whose loss
            // matters most: the chain is the republic's agreed history, and
            // a block that never reached the disk is one this node will ask
            // for again after a crash.
            tracing::error!("the chain did not reach the disk - it is only in memory");
        }
        durable
    }

    /// Verify a block as the extension of our chain, append it, and re-project
    /// state. Returns whether it was accepted. **Does not persist** — the
    /// caller does, once per batch ([`State::persist_chain_now`]).
    pub(super) fn append_committed_block(&mut self, block: ChainBlock) -> bool {
        // verify BEFORE appending — the block only ever touches `self.chain`
        // once it has passed, so there is nothing to roll back
        match self.extend_own(&block) {
            Ok(head) => {
                self.chain_head = Some(head);
                // an append only ADDS to the projection — no whole-chain refold
                self.project_one(&block);
                self.chain.push(block);
                self.bump_next_id_past_chain();
                true
            }
            Err(e) => {
                // routine, not an internal fault: a stale re-serve during
                // catch-up and a hostile peer both land here
                tracing::warn!(height = block.height, error = %e, "refused a chain block");
                false
            }
        }
    }

    /// After a block is applied (by us or a peer): mark its proposal committed,
    /// emit, clear its collected signatures, and re-base every other pending
    /// proposal onto the new head (their old-height signatures are now stale).
    pub(super) fn after_block_applied(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } => {
                // a block for a proposal this node never heard of (lost
                // gossip, late join) still yields a full accepted card
                self.ensure_applied_record(*proposal_id, *surface, payload.clone());
                if let Some(p) = self.proposals.get_mut(proposal_id) {
                    p.state = ProposalState::Applied;
                }
                self.stash_voted(*proposal_id);
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
                // the approval surface (recovery approval design, 2026-08-08):
                // flip the visible membership record and drop the vote
                // bookkeeping on EVERY node. A Membership block carries no
                // proposal id, so match by content — the Checkpoint arm's
                // pattern (the committer also cleans by id upstream).
                self.settle_membership_records(&block.change);
                self.mesh_extension_at.remove(member);
                if self.pending_recovery.contains_key(member) {
                    // the completed checklist, BEFORE the re-key consumes the
                    // pending entry: the live per-approval frames stop at the
                    // seal (`push_recover_progress` reports only while the
                    // vote is open), so the block itself reports completion
                    if let Some(report) = self.recover_complete_report(block) {
                        self.send_recover_progress_frame(report);
                    }
                    let member = member.clone();
                    self.coordinator_rekey(&member);
                }
            }
            ChainChange::Membership {
                op: MembershipOp::Joined,
                ..
            } => {
                self.settle_membership_records(&block.change);
            }
            // WP4b: a checkpoint sealed — on EVERY node, drop the matching
            // proposal bookkeeping (the committer also cleans by id in
            // adopt_committed_block; receivers find it by content). Local
            // block-dropping below `upto` is stage 4.
            ChainChange::Checkpoint { upto, .. } => {
                self.forget_votes_for(&block.change);
                // B-F2: drop the summarized history locally, automatically —
                // the vote just confirmed this summary is correct. The blob
                // becomes the holder's trust anchor; the chain keeps the
                // checkpoint block and everything after it.
                let upto = *upto;
                let anchor_height = block.height;
                match self.own_checkpoint_state(upto) {
                    Ok(blob) => {
                        self.set_checkpoint_blob(Some(blob));
                        self.chain.retain(|b| b.height >= anchor_height);
                        self.apply_chain_to_state();
                        self.persist_chain_now();
                        self.emit(Event::CheckpointSealed {
                            height: anchor_height,
                            upto,
                        });
                        tracing::info!(height = anchor_height, upto, "checkpoint sealed - history below the cut dropped");
                    }
                    Err(e) => {
                        // keep full history rather than drop on a state we
                        // could not recompute (should be impossible: the
                        // verifier just matched this very state)
                        tracing::warn!(error = %e, "checkpoint sealed but the blob could not be built - keeping full history");
                    }
                }
            }
            _ => {}
        }
        self.rebase_pending_approvals();
    }

    /// A membership block sealed — settle its approval surface on THIS node
    /// (recovery approval design, 2026-08-08): flip every open membership
    /// record that describes exactly this change to `Applied` and drop the
    /// matching vote bookkeeping. Content-matched (a Membership block carries
    /// no proposal id), so the sealer, every passive applier and a catch-up
    /// all settle identically.
    pub(super) fn settle_membership_records(&mut self, sealed: &ChainChange) {
        let ChainChange::Membership { op, member, .. } = sealed else {
            return;
        };
        let want_op = match op {
            MembershipOp::Restored => "restore_member",
            MembershipOp::Joined => "add_member",
        };
        let ids: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| {
                p.state == ProposalState::Proposed
                    && p.surface == Surface::Organization
                    && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(want_op)
                    && p.payload.get("member").and_then(serde_json::Value::as_str)
                        == Some(member.as_str())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Applied;
            }
            self.emit(Event::Applied {
                id: ProposalId(id),
                surface: Surface::Organization,
            });
        }
        self.forget_votes_for(sealed);
    }

    /// Re-sign this node's standing approvals at the new head+1: an approval
    /// this node already gave (its signature is in the stale set) is a decision
    /// that still stands, only its position moved — so re-express it (the human
    /// is not asked again). Proposals this node did not approve are just cleared.
    pub(super) fn rebase_pending_approvals(&mut self) {
        let Some(head) = self.chain_head.as_ref() else {
            return;
        };
        let target = head.height + 1;
        let stale: Vec<u64> = self
            .pending_sigs
            .iter()
            .filter(|(_, p)| p.height < target)
            .map(|(id, _)| *id)
            .collect();
        // sweep checkpoint entries that never made it into pending_sigs
        // (a proposer without self-cosign, a bailed sign): a cut below the
        // new head can never seal (upto == height-1 is enforced) and must
        // not linger as bookkeeping a late Approved could resurrect
        let head_height = head.height;
        let swept: Vec<u64> = self
            .proposal_changes
            .iter()
            .filter(|(_, c)| {
                matches!(c, ChainChange::Checkpoint { upto, .. } if *upto < head_height)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in swept {
            self.proposal_changes.remove(&id);
            self.pending_sigs.remove(&id);
            // closure for the proposer/operator: this cut can never seal —
            // re-propose at the new head (the stale loop below can no
            // longer see these entries, so the emit lives HERE)
            self.emit(Event::CheckpointStale { id: ProposalId(id) });
        }
        for id in stale {
            // the LOCAL decision register, never the wire-collected set: a
            // peer can put junk under this node's name into `pending_sigs`,
            // and re-signing from there was a threshold bypass (2026-08-25)
            let mine = self.own_approvals.contains(&id);
            self.pending_sigs.remove(&id);
            // WP4b: a checkpoint's change is CUT-bound (upto == height - 1,
            // enforced by the verifier) — after the head moved, re-signing
            // the old cut could only seal an invalid block. Drop it; the
            // proposer re-proposes at the new head (doc §B.2).

            // only re-sign for proposals still pending that this node approved
            if mine && matches!(self.proposals.get(&id), Some(p) if p.state == ProposalState::Proposed)
            {
                self.chain_sign_and_gossip_approval(id);
            }
        }
    }

    /// WP4b stage 4: verify a candidate chain in THIS holder's context —
    /// a full holder verifies from the genesis, a pruned holder from its
    /// checkpoint blob (`verify_suffix_chain`). The one entry every
    /// adopt/append/probe path routes through.
    pub(crate) fn verify_own(&self, blocks: &[ChainBlock]) -> Result<ChainHead, String> {
        Ok(self.walk_own(blocks)?.head)
    }

    /// [`State::verify_own`], keeping the walk.
    pub(crate) fn walk_own(&self, blocks: &[ChainBlock]) -> Result<ChainWalk, String> {
        match &self.checkpoint_blob {
            None => walk_chain(blocks),
            Some(blob) => walk_suffix_chain(blob, blocks, &self.republic_id()),
        }
    }

    /// Verify ONE block as the extension of the chain we already hold — the
    /// hot path, and the reason the walk is cached.
    ///
    /// Re-walking the whole chain per block made catching up N blocks cost
    /// `m·N(N+1)` signature verifications inside a single actor turn; a
    /// catching-up node then looked exactly like a dead one to its peers.
    /// This is the same verification, with the intermediate state kept
    /// instead of thrown away: the walk is driven by the identical
    /// [`ChainWalk::step`] the full verifiers use.
    ///
    /// The cache is never trusted on its word — it is used only while it
    /// still describes our chain, and rebuilt by a full walk otherwise. A
    /// **refused** block leaves it intact (`step` is atomic), so a peer
    /// spamming bad blocks cannot force a re-walk per block either.
    fn extend_own(&mut self, block: &ChainBlock) -> Result<ChainHead, String> {
        let mut walk = match self.chain_walk.take() {
            Some(w) if w.describes(&self.chain, self.checkpoint_blob.as_ref()) => w,
            _ => self.walk_own(&self.chain)?,
        };
        let stepped = walk.step(block);
        let head = walk.head.clone();
        self.chain_walk = Some(walk);
        stepped.map(|()| head)
    }

    /// The canonical state at `upto` from THIS holder's own material —
    /// genesis-rooted for a full holder, blob-based for a pruned one.
    /// What the propose/verify-before-sign paths hash.
    pub(crate) fn own_checkpoint_state(
        &self,
        upto: u64,
    ) -> Result<molt_core::CheckpointState, String> {
        match &self.checkpoint_blob {
            None => checkpoint_state(&self.chain, upto),
            // the anchor block in chain[0] is state-neutral for the fold
            Some(blob) => fold_state(blob.clone(), &self.chain, upto),
        }
    }

    /// Inbound: a peer proposed something (gossip). Record it as pending so it
    /// shows up and can be approved here. `by` is the authenticated wire
    /// sender — the proposer on a direct delivery, the serving peer on a
    /// WP2 re-serve (a display hint, never an authorization input).
    /// Returns `true` only when the proposal was genuinely NEW here — a
    /// refused id collision or a deduplicated re-serve (WP2 catch-up
    /// re-wraps open proposals under the serving peer's name) returns
    /// `false`, and the caller must not announce it on the event stream.
    /// L3: a peer-chosen proposal id far past the mint counter is garbage —
    /// registering it (or even bumping `next_id` for it) would poison every
    /// later local mint (a u64::MAX id would freeze proposing for good).
    /// Window shared with the decline park.
    pub(crate) fn plausible_wire_id(&self, id: u64) -> bool {
        id <= self
            .next_id
            .saturating_add(crate::proposals::PARKED_DECLINE_ID_WINDOW)
    }

    pub(crate) fn receive_proposed(
        &mut self,
        id: u64,
        surface: Surface,
        payload: serde_json::Value,
        by: &str,
    ) -> bool {
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a proposal with an implausible id");
            return false;
        }
        // L3: a flooding proposer may only crowd ITSELF — the newest card
        // is refused (the WP2 re-serve re-earns an honest one later), and
        // another member's cards are never evicted
        if !self.proposals.contains_key(&id) {
            let open_by = self
                .proposals
                .values()
                .filter(|p| p.state == ProposalState::Proposed && p.by == by)
                .count();
            if open_by >= OPEN_CARDS_PER_PROPOSER_MAX {
                tracing::warn!(%id, %by, "refusing a proposal beyond the per-proposer open cap");
                return false;
            }
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        // SECURITY (symmetric to receive_membership_proposal): an id already
        // registered in `proposal_changes` (a membership/checkpoint change)
        // must not also become a surface proposal — `proposal_change` would
        // keep resolving it to the chain change, so approvals of this
        // "surface proposal" would sign that change's bytes.
        if self.proposal_changes.contains_key(&id) {
            tracing::warn!(%id, "refusing a surface proposal whose id names a chain change");
            return false;
        }
        // an id the verified chain already consumed (the walk's double-apply
        // guard, blob-seeded on a pruned holder) can only be a stale resend —
        // a fresh card would resurrect a decided vote. The reopen twin of
        // this guard is `settle_cards_against_chain`.
        if self.chain_walk.as_ref().is_some_and(|w| w.seen.contains(&id)) {
            tracing::debug!(%id, "refusing a proposal the chain already consumed");
            return false;
        }
        let mut inserted = false;
        self.proposals.entry(id).or_insert_with(|| {
            inserted = true;
            molt_core::ProposalRecord {
                surface,
                payload,
                approvals: 0,
                state: ProposalState::Proposed,
                declined_at: 0,
                declined_by: String::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: by.to_string(),
                superseded: false,
                withdrawn: false,
            }
        });
        if inserted && surface == Surface::Memory {
            // registration-time check (shared_memory_real.md §4): a patch
            // learned LATE against an already-moved base registers
            // superseded right away — no zombie pending cards on rejoiners
            self.supersede_stale_wiki();
        }
        if inserted {
            // L2: signatures that OUTRAN this card become displayable now
            self.reverify_pending(id);
        }
        inserted
    }

    /// Inbound: a peer's signed approval (gossip). Collect + try to seal.
    pub(crate) fn receive_approval(&mut self, id: u64, by: &str, height: u64, sig: &str) {
        if sig.is_empty() {
            return;
        }
        // SECURITY: `height` is peer-supplied. A legitimate approval can
        // only be for the current target (head + 1) or a value we already
        // hold; an out-of-range height (e.g. u64::MAX) would let collect_sig
        // adopt it, clear the real signatures, and — since rebase only
        // sweeps heights BELOW the target — never recover, permanently
        // freezing the proposal (governance-liveness DoS). Bound it here.
        let target = self.chain_head.as_ref().map(|h| h.height + 1);
        if target.is_some_and(|t| height > t) {
            tracing::warn!(%id, height, "dropping an approval for an implausible future height");
            return;
        }
        // L3: an approval may OUTRUN its card (collected, displayed once it
        // lands) — but only inside the same id window everything else uses,
        // or unknown-id entries grow without bound
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "dropping an approval for an implausible proposal id");
            return;
        }
        let verified = self.approval_verifies(id, height, by, sig);
        self.collect_sig(id, height, by, sig, verified);
        if verified {
            if let Some(p) = self.pending_sigs.get_mut(&id) {
                p.verified.insert(by.to_string());
            }
        }
        self.try_commit(id);
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
            // membership records stay out (their window is liveness-bound, see
            // the doc above) — re-serving one as a plain `Proposed` would make
            // receivers register a SURFACE change under the membership id and
            // sign different bytes than everyone else
            .filter(|(id, _)| {
                !matches!(
                    self.proposal_changes.get(id),
                    Some(ChainChange::Membership { .. })
                )
            })
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
                // L2: only VERIFIED signatures are re-served — junk a peer
                // once gossiped must not be amplified to the next node
                for a in pending.sigs.iter().filter(|a| pending.verified.contains(&a.member)) {
                    events.push(WorkspaceEvent::Approved {
                        id: ProposalId(*id),
                        by: a.member.clone(),
                        height: pending.height,
                        sig: a.sig.clone(),
                    });
                }
            }
        }
        // the OWN declines, and only those: a decline carries no signature,
        // so the link identity is the only mouth it may come out of — a
        // foreign decline is never re-attested. Served for open cards, for
        // REJECTED cards (the terminal state is gossip-derived; a peer that
        // missed the vote would keep it open forever) and for parked voices
        // (the own log replayed a decline whose proposal is not back yet).
        // A receiver without the card parks the voice symmetrically.
        // the OWN withdraw re-serves like the own declines below: a peer
        // that was closed while the proposer pulled back must still learn
        // the verdict (same retention gate as the rejected declines)
        let me = self.member();
        let cutoff = self.chat_retention_cutoff();
        let mut own_withdrawn: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.withdrawn && p.by == me && p.declined_at >= cutoff)
            .map(|(id, _)| *id)
            .collect();
        own_withdrawn.sort_unstable();
        for id in own_withdrawn {
            events.push(WorkspaceEvent::Withdrawn {
                id: ProposalId(id),
                by: me.clone(),
            });
        }
        let mut declined: Vec<(u64, String)> = self
            .proposals
            .iter()
            .filter(|(id, p)| {
                // the same Membership exclusion as the Proposed loop above —
                // a membership id must never re-serve, in any clothing
                !matches!(
                    self.proposal_changes.get(id),
                    Some(ChainChange::Membership { .. })
                ) && p.decliners.iter().any(|d| d == &me)
                    && match p.state {
                        ProposalState::Proposed => true,
                        // a rejected card re-serves only while some view
                        // still shows it: past the display retention it has
                        // no convergence audience, and the batch stays
                        // bounded instead of growing with the republic's
                        // whole rejected history
                        ProposalState::Rejected => !Self::aged_out_at(cutoff, p.declined_at),
                        _ => false,
                    }
            })
            // a registered voice recomputes its anchor from the own record;
            // a parked voice re-serves the hash it ARRIVED with (D1)
            .map(|(id, p)| (*id, crate::State::decline_payload_hash(&p.payload)))
            .chain(self.pending_declines.iter().filter_map(|(id, parked)| {
                parked
                    .iter()
                    .find(|(m, _, _)| m == &me)
                    .map(|(_, _, h)| (*id, h.clone()))
            }))
            .collect();
        declined.sort_unstable();
        declined.dedup();
        for (id, hash) in declined {
            events.push(WorkspaceEvent::Declined {
                id: ProposalId(id),
                by: me.clone(),
                hash,
            });
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
}
