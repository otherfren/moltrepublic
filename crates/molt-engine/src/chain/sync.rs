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

/// How many `(height, hash)` samples a fork-aware catch-up request carries.
const KNOWN_HEADS_MAX: usize = 16;

/// A5: a seat unheard-of for this long is reported silent. Two presence
/// ticks (30 s each) plus slack - shorter and every ordinary gap rings.
const SILENT_AFTER_SECS: u64 = 120;
/// A5: a seat is silent once its signature is in none of the last this
/// many blocks. One missed block is a lost race, not silence.
const STALE_AFTER_BLOCKS: u64 = 2;

impl State {
    /// Inbound: a peer broadcast (or re-served) a committed block. Extend the
    /// single branch when it is the next height, tie-break a contended slot we
    /// already filled, or — when it is ahead of us — buffer it and request the
    /// missing suffix (catch-up).
    #[cfg(test)]
    pub(crate) fn receive_block(&mut self, block: ChainBlock) {
        self.receive_block_from("", block);
    }

    /// [`Self::receive_block`] with the wire sender, so a block from another
    /// branch can flag ITS sender (A2.1): a `prev` this node does not hold at
    /// head+1, a tip contender on a foreign `prev`, or any contender below
    /// the tip. A block from that peer that extends the chain clears it.
    pub(crate) fn receive_block_from(&mut self, from: &str, block: ChainBlock) {
        self.note_peer_height(from, block.height);
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
            if block.prev != head.hash {
                // the sender built on a block we do not hold: another branch
                self.note_divergence(from, head.height, &block);
                self.stash_fork_candidate(block);
                self.try_reorg(from);
                return;
            }
            if self.apply_next_block(block) {
                self.clear_divergence(from);
                // the buffered suffix drains behind it — ONE write for the
                // whole batch, at the end
                self.drain_buffered_blocks();
                self.persist_chain_now();
            }
        } else if block.height <= head.height {
            self.tie_break(from, block);
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
    /// The request carries our `known` heads, so a server on another branch
    /// can serve from the fork point (`chain_reorg.md` R5).
    pub(crate) fn request_catchup(&mut self, from: u64) {
        if self.chain.head.is_none() || self.chain.catchup_from == Some(from) {
            return;
        }
        self.chain.catchup_from = Some(from);
        let me = self.member();
        tracing::debug!(me = %me, from, "chain catch-up requested");
        let known = self.known_heads();
        let env = self.make_env(me, WorkspaceEvent::ChainRequest { from_height: from, known });
        self.record(env);
    }

    /// Our `(height, hash)` samples, head first, going back geometrically
    /// (head, head-1, head-2, head-4, ...) down to the anchor - at most
    /// [`KNOWN_HEADS_MAX`] entries.
    pub(crate) fn known_heads(&self) -> Vec<molt_core::HeightHash> {
        let rid = self.republic_id();
        let Some(head) = self.chain.blocks.last().map(|b| b.height) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut back: u64 = 0;
        while out.len() < KNOWN_HEADS_MAX {
            let Some(h) = head.checked_sub(back) else { break };
            if let Some(b) = self.chain.blocks.iter().find(|b| b.height == h) {
                out.push(molt_core::HeightHash { height: h, hash: block_hash(&rid, b) });
            } else {
                break;
            }
            back = if back == 0 { 1 } else { back.saturating_mul(2) };
        }
        out
    }

    /// Where a fork-aware request wants serving from: one above the highest
    /// `known` sample that names a block we hold, else `from_height`.
    pub(crate) fn serve_from_for(&self, from_height: u64, known: &[molt_core::HeightHash]) -> u64 {
        let rid = self.republic_id();
        known
            .iter()
            .filter(|k| {
                self.chain
                    .blocks
                    .iter()
                    .any(|b| b.height == k.height && block_hash(&rid, b) == k.hash)
            })
            .map(|k| k.height.saturating_add(1))
            .max()
            .unwrap_or(from_height)
    }

    /// Keep a block of another branch for the deep tie-break (R5); bounded
    /// like the catch-up buffer, shedding the highest when full.
    fn stash_fork_candidate(&mut self, block: ChainBlock) {
        self.chain.fork_candidates.insert(block.height, block);
        while self.chain.fork_candidates.len() > usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX) {
            if let Some(top) = self.chain.fork_candidates.keys().next_back().copied() {
                self.chain.fork_candidates.remove(&top);
            } else {
                break;
            }
        }
    }

    /// The deep tie-break (`docs_archive/chain/chain_reorg.md`): once the fork
    /// candidates link into our chain at some height f, compare the two
    /// blocks at f (R1); if theirs is smaller, verify our prefix + their
    /// suffix as a whole (R3) and adopt it, returning the displaced
    /// proposals to the vote (R4). A run that does not link yet asks the
    /// peer for the heights below it (R5). Never below the anchor (R2).
    fn try_reorg(&mut self, from: &str) {
        let rid = self.republic_id();
        let Some(anchor) = self.chain.blocks.first().map(|b| b.height) else {
            return;
        };
        let ours_at = |st: &Self, h: u64| st.chain.blocks.iter().find(|b| b.height == h).cloned();
        // the lowest candidate that links into OUR chain
        let mut fork = None;
        for (h, cand) in &self.chain.fork_candidates {
            if *h <= anchor {
                continue;
            }
            if let Some(below) = ours_at(self, h - 1) {
                if block_hash(&rid, &below) == cand.prev {
                    fork = Some(*h);
                    break;
                }
            }
        }
        let Some(f) = fork else {
            // nothing links yet: ask for the run below the lowest candidate
            if let Some(lowest) = self.chain.fork_candidates.keys().next().copied() {
                if lowest > anchor.saturating_add(1) {
                    self.request_catchup(lowest.saturating_sub(1).max(anchor.saturating_add(1)));
                }
            }
            return;
        };
        // the contiguous run of candidates from f upward
        let mut suffix: Vec<ChainBlock> = Vec::new();
        let mut h = f;
        while let Some(c) = self.chain.fork_candidates.get(&h) {
            if let Some(prev) = suffix.last() {
                if c.prev != block_hash(&rid, prev) {
                    break;
                }
            }
            suffix.push(c.clone());
            h = h.saturating_add(1);
        }
        let Some(ours_f) = ours_at(self, f) else {
            return;
        };
        let (mine, theirs) = (block_hash(&rid, &ours_f), block_hash(&rid, &suffix[0]));
        if mine <= theirs {
            // ours wins at the fork (R1): the peer re-bases, not this node
            tracing::info!(%from, fork = f, "keeping this branch at the fork - the peer re-bases");
            self.chain.fork_candidates.clear();
            return;
        }
        let mut candidate: Vec<ChainBlock> =
            self.chain.blocks.iter().filter(|b| b.height < f).cloned().collect();
        candidate.extend(suffix.iter().cloned());
        if let Err(e) = self.walk_own(&candidate) {
            tracing::warn!(%from, fork = f, error = %e, "the other branch does not verify - dropped");
            self.chain.fork_candidates.clear();
            return;
        }
        let dropped: Vec<ChainBlock> =
            self.chain.blocks.iter().filter(|b| b.height >= f).cloned().collect();
        let kept: BTreeSet<u64> = suffix
            .iter()
            .filter_map(|b| match &b.change {
                ChainChange::Applied { proposal_id, .. } => Some(*proposal_id),
                _ => None,
            })
            .collect();
        tracing::warn!(%from, fork = f, dropped = dropped.len(), adopted = suffix.len(), "re-basing onto the other branch");
        self.adopt_chain(candidate);
        // R4: what our dropped suffix decided and theirs does not is a vote again
        for b in &dropped {
            if let ChainChange::Applied { proposal_id, .. } = &b.change {
                if kept.contains(proposal_id) {
                    continue;
                }
                let materialized = self.proposals.get(proposal_id).is_some_and(|p| p.by.is_empty());
                if materialized {
                    self.proposals.remove(proposal_id);
                } else if let Some(p) = self.proposals.get_mut(proposal_id) {
                    p.state = ProposalState::Proposed;
                }
            }
        }
        // the adopted suffix's bookkeeping (emits, cleared sigs, the re-base
        // of every open card onto the new head)
        for b in &suffix {
            self.after_block_applied(b);
        }
        // every signature collected on the old branch is position-bound to
        // heights that no longer exist here; this node's own decisions
        // re-sign at the new heights
        self.chain.pending_sigs.clear();
        let mine: Vec<u64> = self
            .chain
            .own_approvals
            .iter()
            .copied()
            .filter(|id| {
                self.proposals
                    .get(id)
                    .is_some_and(|p| p.state == ProposalState::Proposed)
            })
            .collect();
        for id in mine {
            self.chain_sign_and_gossip_approval(id);
        }
        self.chain.fork_candidates.clear();
        self.chain.diverged.clear();
        self.persist_chain_now();
        self.emit_session(crate::SessionScope::Full);
        // whatever the peer holds above what we adopted
        if let Some(head) = self.chain.head.as_ref().map(|h| h.height) {
            self.chain.catchup_from = None;
            self.request_catchup(head.saturating_add(1));
        }
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
    fn tie_break(&mut self, from: &str, block: ChainBlock) {
        let Some(existing) = self.chain.blocks.iter().find(|b| b.height == block.height) else {
            return;
        };
        if existing == &block {
            // duplicate broadcast of the block we already hold: the sender
            // holds our history at this height
            self.clear_divergence(from);
            return;
        }
        let rid = self.republic_id();
        let incoming = molt_storage::content_hash(&block_link_bytes(&rid, &block));
        let current = molt_storage::content_hash(&block_link_bytes(&rid, existing));
        let is_tip = self.chain.blocks.last().is_some_and(|b| b.height == block.height);
        // A2.1: a contender on a foreign `prev`, or one below the tip, is
        // not a race the tip rule can settle - the sender is on another
        // branch (a deep re-base is what would reconcile it)
        let shared_prev = self
            .chain
            .blocks
            .iter()
            .find(|b| b.height.saturating_add(1) == block.height)
            .map_or(true, |below| block_hash(&rid, below) == block.prev);
        if !shared_prev || !is_tip {
            let since = if shared_prev { block.height } else { block.height.saturating_sub(1) };
            self.note_divergence(from, since, &block);
            self.stash_fork_candidate(block);
            self.try_reorg(from);
            return;
        }
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
                self.chain.head_moved_at = self.presence_now();
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

    /// Remember that `from` is on another branch since `since_height` (the
    /// lowest height the two chains may still share; an earlier sighting
    /// keeps its lower value). A NEW entry goes loud: warn + session notice
    /// `chain-diverged:<peer>:<height>` - the one fact a partitioned seat
    /// could not see for itself (F00).
    fn note_divergence(&mut self, from: &str, since_height: u64, block: &ChainBlock) {
        tracing::warn!(%from, height = block.height, since_height, "a block from another branch");
        if from.is_empty() {
            return;
        }
        let now = self.presence_now();
        let fresh = !self.chain.diverged.contains_key(from);
        let entry = self
            .chain
            .diverged
            .entry(from.to_string())
            .or_insert_with(|| molt_core::ChainDivergence {
                peer: from.to_string(),
                since_height,
                seen_ts: now,
            });
        entry.since_height = entry.since_height.min(since_height);
        entry.seen_ts = now;
        if fresh {
            self.session.notice = format!("chain-diverged:{from}:{since_height}");
            self.emit_session(crate::SessionScope::Full);
        }
    }

    /// A5: remember the highest height `from` has claimed. Monotone per
    /// peer, so one late re-serve of an old block cannot make a peer look
    /// like it fell behind.
    pub(crate) fn note_peer_height(&mut self, from: &str, height: u64) {
        if from.is_empty() || from == self.member() {
            return;
        }
        let e = self.chain.peer_heights.entry(from.to_string()).or_insert(0);
        *e = (*e).max(height);
    }

    /// A5: how far the peers are ahead, and who has gone quiet. The two
    /// facts a partitioned seat cannot see for itself - `chain_diverged`
    /// answers a different question (contradiction, not distance).
    pub(crate) fn chain_lag(&self) -> molt_core::ChainLag {
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let peers_ahead = self
            .chain
            .peer_heights
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_sub(head);
        let now = self.presence_now();
        let founded = self.replica.as_ref().map_or(0, |r| r.founded_ts);
        let me = self.member();
        let silent = self
            .roster()
            .into_iter()
            .filter(|m| *m != me)
            .filter_map(|member| {
                let seen = self.member_last_seen(&member);
                // never seen and no founding date known: silent, age unknown
                let (secs, unknown) = match (seen, founded) {
                    (molt_core::MemberInfo::NEVER, 0) => (0, true),
                    (molt_core::MemberInfo::NEVER, f) => (now.saturating_sub(f), false),
                    (s, _) => (now.saturating_sub(s), false),
                };
                (unknown || secs >= SILENT_AFTER_SECS)
                    .then_some(molt_core::SilentMember { member, secs })
            })
            .collect();
        molt_core::ChainLag { peers_ahead, silent }
    }

    /// A5: a seat's signature verified here for `height` - sealed or not.
    pub(crate) fn note_signer(&mut self, member: &str, height: u64) {
        let e = self.chain.seen_signing.entry(member.to_string()).or_insert(0);
        *e = (*e).max(height);
    }

    /// A5: the highest height each roster seat signed for - in a block this
    /// holder still keeps, or verified here before a seal. A seat that
    /// stopped signing is silent, not forked - and that is the distinction
    /// the detector could not draw.
    pub(crate) fn stale_signers(&self) -> Vec<molt_core::StaleSigner> {
        let Some(head) = self.chain.head.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<molt_core::StaleSigner> = Vec::new();
        for id in &head.identities {
            let sealed = self
                .chain
                .blocks
                .iter()
                .filter(|b| b.sigs.iter().any(|a| a.member == id.member))
                .map(|b| b.height)
                .max()
                .unwrap_or(0);
            let seen = self.chain.seen_signing.get(&id.member).copied().unwrap_or(0);
            let last = sealed.max(seen);
            if last + STALE_AFTER_BLOCKS <= head.height {
                out.push(molt_core::StaleSigner {
                    member: id.member.clone(),
                    last_signed_height: last,
                });
            }
        }
        out
    }

    /// A block from `from` that fits our chain: whatever it was on, it is
    /// on our branch now.
    fn clear_divergence(&mut self, from: &str) {
        if self.chain.diverged.remove(from).is_some() {
            tracing::info!(%from, "peer is back on this branch");
            self.emit_session(crate::SessionScope::Full);
        }
    }
}
