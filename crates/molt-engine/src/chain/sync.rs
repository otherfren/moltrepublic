// SPDX-License-Identifier: GPL-3.0-or-later

//! **Catch-up sync — how blocks travel between holders**: the inbound
//! `Committed` block (extend, tie-break a contended tip, or buffer + ask
//! for the missing suffix), the bounded catch-up buffer and its drain,
//! the `ChainRequest` and the survivor's answer from its own chain (a
//! pruned holder serves its blob first), and the smallest standalone
//! prefix a coordinator hands a rejoiner (`anchor_bootstrap`).

use super::*;

/// L3: how far past the head a buffered future block may claim to be, and
/// the buffer's size bound — larger than any served suffix batch, small
/// enough that ~96 KiB frames cannot pin unbounded RAM.
const CATCHUP_BUFFER_WINDOW: u64 = 4096;

impl State {
    /// Inbound: a peer broadcast (or re-served) a committed block. Extend the
    /// single branch when it is the next height, tie-break a contended slot we
    /// already filled, or — when it is ahead of us — buffer it and request the
    /// missing suffix (catch-up).
    pub(crate) fn receive_block(&mut self, block: ChainBlock) {
        let Some(head) = self.chain.head.clone() else {
            // a headless rejoiner (total device loss) bootstraps its chain from
            // the genesis a survivor serves, then drains whatever else arrived
            // first; a non-genesis block is buffered until the genesis lands
            if block.height == 0 {
                // a valid genesis is trivially forgeable (n-of-n over
                // attacker keys): only THIS republic's, when the replica
                // knows which one that is (review C6)
                let expected = self.republic_id();
                if let ChainChange::Genesis { republic_id, .. } = &block.change {
                    if !expected.is_empty() && *republic_id != expected {
                        tracing::warn!(%republic_id, "refusing a genesis for another republic");
                        return;
                    }
                }
                self.adopt_chain(vec![block]);
                if self.chain.head.is_some() {
                    self.drain_buffered_blocks();
                    self.persist_chain_now();
                }
            } else {
                // L3: headless too, the buffer is size-capped (no head to
                // window against) — shed the highest, the re-serve re-earns
                self.chain.pending_blocks.insert(block.height, block);
                while self.chain.pending_blocks.len()
                    > usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX)
                {
                    if let Some(top) = self.chain.pending_blocks.keys().next_back().copied() {
                        self.chain.pending_blocks.remove(&top);
                    } else {
                        break;
                    }
                }
                // WP4b: with a served blob stashed, the buffered block may
                // be the missing anchor/suffix piece
                self.try_adopt_from_blob();
            }
            return;
        };
        if block.height == head.height + 1 {
            if self.apply_next_block(block) {
                // the buffered suffix drains behind it — ONE write for the
                // whole batch, at the end
                self.drain_buffered_blocks();
                self.persist_chain_now();
            }
        } else if block.height <= head.height {
            self.tie_break(block);
        } else {
            // a gap: we are behind. Buffer this block and ask the mesh for the
            // blocks we are missing (any survivor re-serves them). L3: only
            // heights the drain could ever reach are buffered (contiguous
            // upward from head+1, or the stashed blob's re-anchor run), and
            // the buffer is capped — when full the HIGHEST height is shed
            // (furthest from applicable; a re-served suffix re-earns it).
            let anchor_ok = self
                .chain.pending_served_blob
                .as_ref()
                .is_some_and(|blob| {
                    block.height > blob.upto
                        && block.height <= blob.upto.saturating_add(CATCHUP_BUFFER_WINDOW)
                });
            if block.height > head.height.saturating_add(CATCHUP_BUFFER_WINDOW) && !anchor_ok {
                tracing::warn!(height = block.height, head = head.height, "refusing to buffer a block far past the head");
                return;
            }
            self.chain.pending_blocks.retain(|h, _| *h > head.height);
            self.chain.pending_blocks.insert(block.height, block);
            while self.chain.pending_blocks.len() > usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX) {
                if let Some(top) = self.chain.pending_blocks.keys().next_back().copied() {
                    self.chain.pending_blocks.remove(&top);
                } else {
                    break;
                }
            }
            self.try_adopt_from_blob();
            self.request_catchup(head.height + 1);
        }
    }

    /// Verify a block against the current head, append + apply it, and run the
    /// post-apply bookkeeping. Returns whether it was accepted.
    fn apply_next_block(&mut self, block: ChainBlock) -> bool {
        // no probe clone: `append_committed_block` verifies before it appends,
        // so an unverifiable block never touches the chain. The probe used to
        // verify the whole chain a SECOND time per block — an exact doubling
        // of the catch-up cost that bought nothing.
        if self.append_committed_block(block.clone()) {
            self.after_block_applied(&block);
            // the head advanced — a catch-up request that reached this height is done
            if self.chain.catchup_from.is_some_and(|f| f <= block.height) {
                self.chain.catchup_from = None;
            }
            true
        } else {
            false
        }
    }

    /// Apply buffered catch-up blocks while the next height is available, then
    /// drop any stale buffered blocks at or below the head.
    fn drain_buffered_blocks(&mut self) {
        while let Some(head) = self.chain.head.clone() {
            let next = head.height + 1;
            let Some(block) = self.chain.pending_blocks.remove(&next) else {
                break;
            };
            if !self.apply_next_block(block) {
                break;
            }
        }
        let head_h = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.chain.pending_blocks.retain(|h, _| *h > head_h);
    }

    /// Broadcast a catch-up request for every block from `from` onward (deduped
    /// while the same gap is outstanding). No-op if we cannot be behind.
    pub(crate) fn request_catchup(&mut self, from: u64) {
        if self.chain.head.is_none() || self.chain.catchup_from == Some(from) {
            return;
        }
        self.chain.catchup_from = Some(from);
        let me = self.member();
        tracing::debug!(me = %me, from, "chain catch-up requested");
        let env = self.make_env(me, WorkspaceEvent::ChainRequest { from_height: from });
        self.record(env);
    }

    /// **Does a served blob fit one transport frame?** (K6 §4.9.8.)
    ///
    /// An over-budget `WorkspaceEvent` is a PERMANENT publish stall: the
    /// node writes nothing more, across restarts. So a pruned holder whose
    /// blob outgrew the frame budget would brick its own outbox the first
    /// time a peer asked for catch-up below its anchor. Nothing else
    /// measures a WorkspaceEvent against the transport budget
    /// (`payload_fits` covers proposals), and this is the one event whose
    /// size a PEER's request decides.
    ///
    /// Not serving costs that peer one bootstrap source; serving would
    /// cost this node every future write.
    pub(crate) fn served_blob_fits(blob: &molt_core::CheckpointState) -> bool {
        // room for the envelope around the event; the number it guards is
        // tens of kilobytes, so the reserve is deliberately generous
        const ENVELOPE_RESERVE: usize = 2048;
        let len = serde_json::to_vec(&WorkspaceEvent::CheckpointServed { blob: blob.clone() })
            .map_or(usize::MAX, |b| b.len());
        let cap = crate::proposals::transport_plaintext_ceiling().saturating_sub(ENVELOPE_RESERVE);
        if len > cap {
            tracing::warn!(
                bytes = len,
                cap,
                upto = blob.upto,
                "checkpoint blob does not fit one frame - not serving it"
            );
        }
        len <= cap
    }

    /// Serve a peer's catch-up request from our OWN chain: re-broadcast every
    /// block we hold from `from` onward (as `Committed`, re-authored so the
    /// outbox fans it out). A single survivor thus reconstitutes the chain for
    /// everyone — independent of who originally committed each block.
    pub(crate) fn serve_chain_from(&mut self, from: u64) {
        let blocks: Vec<ChainBlock> = self
            .chain.blocks
            .iter()
            .filter(|b| b.height >= from)
            .cloned()
            .collect();
        tracing::debug!(me = %self.member(), from, served = blocks.len(), "serving chain catch-up");
        if blocks.is_empty() {
            return;
        }
        let me = self.member();
        // WP4b: a pruned holder cannot serve below its anchor — it serves
        // the BLOB instead, ahead of the anchor/suffix, so the requester
        // can hard-verify and re-anchor (suffix rules)
        if let (Some(blob), Some(anchor)) = (&self.chain.checkpoint_blob, self.chain.blocks.first()) {
            // strictly below: a requester missing only the anchor block can
            // verify it against its own history — the full-state blob would
            // be pure fan-out amplification
            if from < anchor.height && Self::served_blob_fits(blob) {
                let blob = blob.clone();
                let env = self.make_env(me.clone(), WorkspaceEvent::CheckpointServed { blob });
                self.record(env);
            }
        }
        for block in blocks {
            let env = self.make_env(me.clone(), WorkspaceEvent::Committed(block));
            self.record(env);
        }
    }

    /// The **smallest prefix that verifies standalone** — what a coordinator
    /// hands a rejoiner so it can materialize a workspace at all.
    ///
    /// Not the chain: one `set_image` block exceeds the gift-wrap cap
    /// (`welcome_chain_budget.rs`), so "the chain fits" is one proposal away
    /// from false, forever. Not a bare head either: `verify_chain` is
    /// all-or-nothing from the anchor, so a head without its chain is an
    /// unverified claim, and a headless node drops every block served to it
    /// (`is_chain_governed()` gates the ingest).
    ///
    /// So: this holder's `chain[0]` — the genesis, or after a compaction the
    /// checkpoint anchor block with the blob that roots it, since by then no
    /// node anywhere still holds a genesis. Everything above arrives over the
    /// ordinary catch-up, once the rejoiner has a head and asking works.
    pub(crate) fn anchor_bootstrap(&self) -> Vec<WorkspaceEvent> {
        let Some(anchor) = self.chain.blocks.first() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(blob) = self.chain.checkpoint_blob.as_ref().filter(|b| Self::served_blob_fits(b)) {
            out.push(WorkspaceEvent::CheckpointServed { blob: blob.clone() });
        }
        out.push(WorkspaceEvent::Committed(anchor.clone()));
        out
    }

    /// Broadcast [`State::anchor_bootstrap`]. The coordinator pushes this
    /// right after a recovery Welcome, because a rejoiner cannot ASK: it has
    /// no workspace to record a `ChainRequest` from yet. Called from the
    /// Nostr re-key ([`State::coordinator_rekey_nostr`]); the offer's shape
    /// is pinned by `the_served_anchor_is_the_smallest_prefix_that_verifies`.
    pub(crate) fn serve_chain_anchor(&mut self) {
        let me = self.member();
        for ev in self.anchor_bootstrap() {
            let env = self.make_env(me.clone(), ev);
            self.record(env);
        }
    }

    /// Resolve a competing block at a slot we already filled: identical block →
    /// a duplicate broadcast, ignore; a different block at the tip with a
    /// smaller hash wins the single branch, so adopt it and re-base the
    /// displaced proposal. A deeper conflict is logged (deep reorg is Phase 3).
    fn tie_break(&mut self, block: ChainBlock) {
        let Some(existing) = self.chain.blocks.iter().find(|b| b.height == block.height) else {
            return;
        };
        if existing == &block {
            return; // duplicate broadcast of the block we already hold
        }
        let rid = self.republic_id();
        let incoming = molt_storage::content_hash(&block_link_bytes(&rid, &block));
        let current = molt_storage::content_hash(&block_link_bytes(&rid, existing));
        let is_tip = self.chain.blocks.last().is_some_and(|b| b.height == block.height);
        // CHEAP FIRST (review C5): a ground low-hash block costs a full
        // re-walk per frame; the signatures are what any contender must
        // carry, so they are checked against the roster before anything
        // moves (the roster is stable across blocks — `Joined` is refused)
        let signed = self.chain.head.as_ref().is_some_and(|h| {
            block_signers(&rid, &h.identities, &block)
                .is_ok_and(|signers| signers.len() >= usize::from(h.rule_m))
        });
        if is_tip && incoming < current && !signed {
            tracing::warn!(height = block.height, "tie-break contender without a valid threshold - dropped");
            return;
        }
        if is_tip && incoming < current {
            // the incoming block wins the tip; swap it in and re-verify
            let displaced = self.chain.blocks.pop();
            self.chain.blocks.push(block.clone());
            if let Ok(head) = self.verify_own(&self.chain.blocks) {
                self.chain.head = Some(head);
                self.apply_chain_to_state();
                // the displaced proposal returns to pending and re-bases —
                // but ONLY a card with a deliberation behind it (a proposer
                // this holder learned via gossip). A record MATERIALIZED
                // from the now-displaced block (`ensure_applied_record`,
                // by == "") has no vote to return to here: flipping it open
                // would mint an unowned, unwithdrawable phantom card that
                // re-gossips forever and blocks auto-checkpoints. Drop it —
                // the holder returns to "never heard of it", and the WP2
                // re-serve restores the real card while the vote is open.
                if let Some(ChainChange::Applied { proposal_id, .. }) =
                    displaced.as_ref().map(|b| &b.change)
                {
                    let materialized = self
                        .proposals
                        .get(proposal_id)
                        .is_some_and(|p| p.by.is_empty());
                    if materialized {
                        self.proposals.remove(proposal_id);
                    } else if let Some(p) = self.proposals.get_mut(proposal_id) {
                        p.state = ProposalState::Proposed;
                    }
                }
                self.after_block_applied(&block);
                self.persist_chain_now();
            } else {
                // revert — should not happen for a verified block
                self.chain.blocks.pop();
                if let Some(b) = displaced {
                    self.chain.blocks.push(b);
                }
            }
        }
    }
}
