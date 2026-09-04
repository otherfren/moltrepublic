// SPDX-License-Identifier: GPL-3.0-or-later

//! **Checkpoints — log compaction** (`docs_archive/chain/log_compaction.md`,
//! WP4b): the self-proposing cut (`maybe_auto_checkpoint`), the co-equal
//! `propose_checkpoint` verb, the receive side's verify-before-sign, the
//! local cut once a checkpoint block sealed (the blob becomes the trust
//! anchor, history below it is dropped), and a lagging holder's re-anchor
//! on a served blob + suffix. The fold that produces the summarized state
//! is [`super::verify::checkpoint_state`] / `fold_state`.

use super::*;

/// WP4b automation: the chain length (blocks held locally, anchor
/// included) at which the lowest-named member auto-proposes the next
/// compaction cut. Blocks are governance decisions — rare — so this is
/// months of activity for a small republic, and after each cut the local
/// chain shrinks back to the anchor + suffix. A constant, not a setting:
/// compaction is hygiene, not policy (`docs_archive/chain/log_compaction.md`).
pub(crate) const AUTO_CHECKPOINT_MIN_LEN: usize = 32;

impl State {
    /// WP4b automation (product decision 2026-07-18): the compaction cut
    /// proposes ITSELF — the GUI button is gone; `propose_checkpoint`
    /// stays as the co-equal MCP verb (manual override). Collision-free
    /// and deterministic by construction:
    ///
    /// - only the alphabetically LOWEST-named roster member triggers
    ///   (one proposer — proposal ids are node-local, two simultaneous
    ///   auto-proposers would collide); if that member is offline no cut
    ///   happens, exactly like a manual proposer being away,
    /// - only right after THIS node itself sealed a block at the live
    ///   head (`adopt_committed_block`) — every member that just
    ///   co-signed is at the same head, which is what the receivers'
    ///   verify-before-sign recomputation needs. Passively applied
    ///   blocks (`apply_next_block`: catch-up serves, another sealer's
    ///   broadcast) never trigger — a catching-up node would propose at
    ///   a stale intermediate head (the `catchup_from`/`pending_blocks`
    ///   guard below is defense in depth only: the first served block
    ///   already clears `catchup_from`),
    /// - never while a vote is open (an interfering seal would stale
    ///   the cut; the commit resolving the last open vote re-fires
    ///   this check),
    /// - a staled cut needs no timer or backoff: the very block that
    ///   staled it lands here again and re-proposes at the new head, so
    ///   there is at most one auto-propose per committed block.
    pub(super) fn maybe_auto_checkpoint(&mut self) {
        if self.chain.blocks.len() < AUTO_CHECKPOINT_MIN_LEN {
            return;
        }
        let Some(head) = self.chain.head.as_ref() else {
            return;
        };
        // Only a buffered block ADJACENT to head pins the cut: it is about
        // to apply and would stale the checkpoint on arrival. A gap block
        // cannot apply next, and the buffer accepts claims up to head+4096
        // — gating on "any buffered block" let one plausible far-future
        // claim freeze compaction until a drain cleared it (known-debt
        // refinement, 2026-08-16 list).
        if self.chain.catchup_from.is_some() || self.chain.pending_blocks.contains_key(&(head.height + 1)) {
            return;
        }
        let me = self.member();
        let lowest = head.identities.iter().map(|i| i.member.as_str()).min();
        if lowest != Some(me.as_str()) {
            return;
        }
        // "a vote is open": a surface proposal still Proposed, signatures
        // still being collected, or a cut already in flight. Committed
        // membership residue in `proposal_changes` (never swept on
        // receivers) must NOT block the automation forever, so registered
        // changes only count via their pending signatures — except a
        // checkpoint entry, which means a cut is already pending.
        let vote_open = self
            .proposals
            .values()
            .any(|p| p.state == ProposalState::Proposed)
            || !self.chain.pending_sigs.is_empty()
            || self
                .chain.proposal_changes
                .values()
                .any(|c| matches!(c, ChainChange::Checkpoint { .. }));
        if vote_open {
            return;
        }
        match self.cmd_propose_checkpoint() {
            Ok(_) => {
                tracing::info!(len = self.chain.blocks.len(), "auto-proposed a compaction checkpoint");
            }
            Err(e) => tracing::warn!(error = %e, "auto-checkpoint propose failed"),
        }
    }

    /// WP4b stage 3: the human verb — propose the compaction cut at the
    /// CURRENT head (`upto` = head height, B-F1). The engine computes the
    /// canonical state hash itself, announces it, and co-signs; every
    /// receiver recomputes before signing (`receive_checkpoint_proposal`).
    pub(crate) fn cmd_propose_checkpoint(
        &mut self,
    ) -> Result<molt_core::Reply, molt_core::MoltError> {
        if !self.is_chain_governed() {
            return Err(molt_core::MoltError::BadPayload(
                "checkpoints need a chain-governed republic".into(),
            ));
        }
        let Some(head) = self.chain.head.as_ref() else {
            return Err(molt_core::MoltError::BadPayload("no chain head".into()));
        };
        let upto = head.height;
        let state = self
            .own_checkpoint_state(upto)
            .map_err(molt_core::MoltError::BadPayload)?;
        let state_hash = checkpoint_state_hash(&state);
        let id = self.next_id;
        self.next_id += 1;
        self.chain.proposal_changes.insert(
            id,
            ChainChange::Checkpoint {
                upto,
                state_hash: state_hash.clone(),
            },
        );
        let me = self.member();
        let env = self.make_env(
            me,
            WorkspaceEvent::CheckpointProposed {
                id: ProposalId(id),
                upto,
                state_hash,
            },
        );
        self.record(env);
        if self.config.self_cosign {
            self.chain_sign_and_gossip_approval(id);
        }
        Ok(molt_core::Reply::Proposed {
            id: ProposalId(id),
            warnings: Vec::new(),
        })
    }

    /// WP4b stage 3, receive side: verify BEFORE sign. Recompute the
    /// canonical state from OUR OWN chain at the proposed cut and co-sign
    /// only on an exact hash match — nobody ever signs a foreign blob. A
    /// cut that is not our current head is skipped and NOT buffered: a
    /// lagging node simply misses this cut (v1 liveness limit, stage-5
    /// pin in `docs_archive/chain/log_compaction.md`) — the proposer re-proposes
    /// at the then-current head; a stale cut dies on re-base anyway.
    pub(crate) fn receive_checkpoint_proposal(&mut self, id: u64, upto: u64, state_hash: &str) {
        // L3: the guard runs BEFORE the bump — `id + 1` on u64::MAX was a
        // one-frame remote ABORT (overflow-checks + panic=abort), and an
        // in-range absurd id would poison the mint counter
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "refusing a checkpoint proposal with an implausible id");
            return;
        }
        self.next_id = self.next_id.max(id.saturating_add(1));
        let Some(head) = self.chain.head.as_ref() else {
            return;
        };
        if head.height != upto {
            tracing::debug!(%id, upto, head = head.height, "ignoring a checkpoint cut that is not our head");
            return;
        }
        let ours = match self.own_checkpoint_state(upto) {
            Ok(state) => checkpoint_state_hash(&state),
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot recompute the proposed checkpoint state");
                return;
            }
        };
        if ours != state_hash {
            tracing::warn!(%id, "refusing to co-sign a checkpoint that does not match our own projection");
            return;
        }
        // NO id-collision signing (review finding): the peer chose the id,
        // and chain_sign_and_gossip_approval signs whatever change the id
        // RESOLVES to — an id that already names a surface or membership
        // proposal would turn this auto-cosign into an unattended approval
        // of a human-decision change (or let human approvals of that
        // proposal silently sign checkpoint bytes). Refuse any occupied id
        // that is not this exact checkpoint.
        let this = ChainChange::Checkpoint {
            upto,
            state_hash: state_hash.to_string(),
        };
        if !self.id_free_for(id, &this) {
            tracing::warn!(%id, "refusing a checkpoint proposal whose id names a different change");
            return;
        }
        // L3: ONE cut per head — the identical (upto, state_hash) under a
        // second id would mint one registry entry + one signed Approved
        // per frame (1:1 outbound amplification); the first id IS the cut
        if self
            .chain.proposal_changes
            .iter()
            .any(|(other, c)| *other != id && *c == this)
        {
            tracing::debug!(%id, upto, "ignoring a duplicate checkpoint cut under a fresh id");
            return;
        }
        self.chain.proposal_changes.insert(id, this);
        // replay guard: one signature per member per cut
        if self.own_signature_stands(id) {
            return;
        }
        // correctness attestation, not a product decision: co-sign directly
        self.chain_sign_and_gossip_approval(id);
    }

    /// WP4b: a served blob arrives ahead of its anchor. Stash it (runtime
    /// only) after the cheap forgery check — the REAL verification happens
    /// in [`State::try_adopt_from_blob`] once the anchor block is here.
    pub(crate) fn receive_checkpoint_blob(&mut self, blob: molt_core::CheckpointState) {
        // only useful when we are strictly BEHIND the served cut (head ==
        // upto means only the anchor is missing — the normal apply path
        // covers that without the full-state blob)
        let behind = match &self.chain.head {
            None => true,
            Some(head) => head.height < blob.upto,
        };
        if !behind {
            return;
        }
        // first stash wins until it is consumed or invalidated — an
        // overwritable slot would let one insider race garbage over a
        // legitimate blob forever (griefing; per-peer stashes are the
        // fuller fix, doc §B.6)
        if self.chain.pending_served_blob.is_some() {
            return;
        }
        let rid = molt_storage::republic_id(
            &blob.founding_name,
            blob.rule_m,
            blob.rule_n,
            &blob.founding_identities,
        );
        if rid != self.republic_id() || rid != blob.republic_id {
            tracing::warn!("dropping a served checkpoint blob that does not recompute to this republic");
            return;
        }
        self.chain.pending_served_blob = Some(blob);
        self.try_adopt_from_blob();
    }

    /// Adopt blob + buffered anchor/suffix once both are here: build the
    /// longest consecutive candidate from the buffer and run the FULL
    /// suffix verification — all-or-nothing, nothing is trusted from the
    /// stash until it passes.
    pub(crate) fn try_adopt_from_blob(&mut self) {
        let Some(blob) = self.chain.pending_served_blob.clone() else {
            return;
        };
        // the chain advanced past the cut through the normal apply path —
        // the stash is dead weight now
        if self
            .chain.head
            .as_ref()
            .is_some_and(|h| h.height > blob.upto)
        {
            self.chain.pending_served_blob = None;
            return;
        }
        // an attacker-served blob.upto could be u64::MAX; a saturating add
        // makes the lookup miss rather than overflow (overflow-checks abort)
        let Some(anchor_height) = blob.upto.checked_add(1) else {
            self.chain.pending_served_blob = None;
            return;
        };
        if !self.chain.pending_blocks.contains_key(&anchor_height) {
            return;
        }
        let mut candidate = Vec::new();
        let mut h = anchor_height;
        while let Some(b) = self.chain.pending_blocks.get(&h) {
            candidate.push(b.clone());
            let Some(next) = h.checked_add(1) else { break };
            h = next;
        }
        match verify_suffix_chain(&blob, &candidate, &self.republic_id()) {
            Ok(head) => {
                let new_height = head.height;
                self.set_checkpoint_blob(Some(blob));
                self.chain.blocks = candidate.clone();
                self.chain.head = Some(head);
                self.chain.pending_served_blob = None;
                self.chain.pending_blocks.retain(|h, _| *h > new_height);
                self.apply_chain_to_state();
                // The cards are settled: `apply_chain_to_state` folded the
                // blob's consumed ids (else they zombie as Proposed and the
                // re-base re-signs them into dead gossip — review finding)
                // and every suffix block's card through
                // `settle_cards_against_chain` — terminal state, stashed
                // voters, dropped signatures. What the block-by-block path
                // (`after_block_applied`) still owes per block: the Applied
                // event, the org refresh and a Restored seat's stale
                // announce-cooldown; stale signatures re-base once at the end.
                let mut org_touched = false;
                for block in &candidate {
                    match &block.change {
                        ChainChange::Applied {
                            proposal_id,
                            surface,
                            ..
                        } => {
                            self.emit(Event::Applied {
                                id: ProposalId(*proposal_id),
                                surface: *surface,
                            });
                            if *surface == Surface::Organization {
                                org_touched = true;
                            }
                        }
                        ChainChange::Membership {
                            op: MembershipOp::Restored,
                            member,
                            ..
                        } => {
                            self.recovery.mesh_extension_at.remove(member);
                        }
                        _ => {}
                    }
                }
                if org_touched {
                    self.after_org_applied();
                }
                self.rebase_pending_approvals();
                self.persist_chain_now();
                if self.chain.catchup_from.is_some_and(|f| f <= new_height) {
                    self.chain.catchup_from = None;
                }
                tracing::info!(height = new_height, "re-anchored on a served checkpoint");
            }
            Err(e) => {
                // drop THIS stash so a later honest re-serve can land — a
                // failed pairing must not wedge the slot forever
                self.chain.pending_served_blob = None;
                tracing::warn!(error = %e, "served checkpoint blob + suffix do not verify - stash cleared");
            }
        }
    }
}
