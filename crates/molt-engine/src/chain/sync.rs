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

/// Hold a block buffer to [`CATCHUP_BUFFER_WINDOW`] entries by shedding
/// the highest.
fn shed_highest(buf: &mut std::collections::BTreeMap<u64, ChainBlock>) {
    let cap = usize::try_from(CATCHUP_BUFFER_WINDOW).unwrap_or(usize::MAX);
    while buf.len() > cap {
        buf.pop_last();
    }
}

/// The proposal id an applied block consumed (0 otherwise).
fn applied_id(block: &ChainBlock) -> u64 {
    match &block.change {
        ChainChange::Applied { proposal_id, .. } => *proposal_id,
        _ => 0,
    }
}

/// A5: what a peer's report says about its height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerClaim {
    /// "My head is h" (a catch-up request or beacon, a vote refusal, a
    /// lock nack): sets the claim, so a peer that reorged reads lower.
    Head,
    /// "A block at h exists" (a block, a re-serve, a cut proposal): only
    /// raises it.
    Holds,
}

/// The first 12 hex characters of a hash, for logs and status.
fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// S4: the first catch-up retry, and the most the retries back off to.
const CATCHUP_RETRY_SECS: u64 = 60;
const HEAL_BACKOFF_MAX_SECS: u64 = 600;
/// S4: an unhealed divergence goes loud after this long.
const HEAL_STUCK_SECS: u64 = 900;
/// S4: the head beacon's period (plus a per-seat offset below a minute).
const CHAIN_BEACON_SECS: u64 = 300;
/// S4: a retry names this many recent heights one by one.
const DENSE_KNOWN_HEADS: u64 = 64;
/// S4a: a seal-first restore's anchor is re-pushed this often while the
/// seat stays unheard.
const ANCHOR_REPUSH_SECS: u64 = 600;
/// Review R5: re-pushes per restore, across restarts. Every push is stored
/// on the relays and a listening rejoiner reads all of them, so the count
/// bounds this node's work; a new link restarts everything.
pub(super) const ANCHOR_REPUSH_MAX: u32 = 12;
/// S4: fork candidates that never linked are dropped after this long.
const CANDIDATE_TTL_SECS: u64 = 1_200;

/// G3: a catch-up round repeating this often without progress is stuck.
pub(crate) const HEAL_LOOP_ROUNDS: u32 = 3;
/// H1: refused blocks kept for `status`.
const REFUSALS_KEPT: usize = 8;
/// A4: an identical log line goes out once per key per this long.
const LOG_GATE_SECS: u64 = 60;

/// H1: the stable code of a block refusal, from the verifier's text.
pub(crate) fn refusal_rule(error: &str) -> &'static str {
    const RULES: &[(&str, &str)] = &[
        ("does not follow", "height"),
        ("does not link to its predecessor", "link"),
        ("a genesis cannot appear", "genesis_late"),
        ("is applied twice", "double_apply"),
        ("must be exactly its block height", "cut_upto"),
        ("valid approvals, threshold is", "threshold"),
        ("consent is spent", "consent_spent"),
        ("consent is stale", "consent_stale"),
        ("consent does not verify", "consent_invalid"),
        ("consent is malformed", "consent_malformed"),
        ("twice - consent plus", "consent_twice"),
        ("carries a consent on a non-restore", "consent_misplaced"),
        ("unknown member", "unknown_member"),
        ("anchored identity key", "restore_key"),
        ("`joined` is not supported", "joined"),
        ("own projection", "cut_state"),
        ("below the blob coverage", "cut_floor"),
        ("legacy checkpoint", "cut_kind"),
        ("vault checkpoint outside", "cut_kind"),
        ("lies below the anchor", "below_cut"),
        ("branch choice", "branch"),
        ("vault", "vault"),
    ];
    RULES.iter().find(|(needle, _)| error.contains(needle)).map_or("other", |(_, rule)| rule)
}

/// How many `(height, hash)` samples a fork-aware catch-up request carries.
const KNOWN_HEADS_MAX: usize = 16;

/// A5: a seat unheard-of for this long is reported silent. Two presence
/// ticks (30 s each) plus slack - shorter and every ordinary gap rings.
pub(super) const SILENT_AFTER_SECS: u64 = 120;
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
    #[cfg(test)]
    pub(crate) fn receive_block_from(&mut self, from: &str, block: ChainBlock) {
        self.receive_block_at(from, None, block);
    }

    /// [`Self::receive_block_from`] with the frame's sender log seq, so a
    /// delayed old block cannot outrank a newer head report.
    pub(crate) fn receive_block_at(&mut self, from: &str, seq: Option<u64>, block: ChainBlock) {
        self.note_peer_claim(from, block.height, seq, PeerClaim::Holds);
        if self.branch_intercepts(from, &block) {
            return;
        }
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
                        let hash = self.note_refused(from, &block, "genesis_foreign");
                        tracing::warn!(%republic_id, %hash, %from, rule = "genesis_foreign", "refusing a genesis for another republic");
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
                shed_highest(&mut self.chain.pending_blocks);
                // WP4b: with a served blob stashed, the buffered block may
                // be the missing anchor/suffix piece
                self.try_adopt_from_blob();
            }
            return;
        };
        if block.height == head.height + 1 {
            if block.prev != head.hash {
                if self.left_that_branch(from, seq, block.height) {
                    tracing::debug!(%from, height = block.height, "fork=stale_block");
                    return;
                }
                // the sender built on a block we do not hold: another branch
                self.note_divergence(from, head.height, &block);
                self.stash_fork_candidate(block);
                self.try_reorg(from);
                return;
            }
            // the buffered suffix drains behind it — ONE write for the
            // whole batch, at the end
            let (height, hash) = (block.height, self.block_id_of(&block));
            let applied = self.in_block_batch(|st| {
                let ok = st.apply_next_block(from, block);
                if ok {
                    st.drain_buffered_blocks();
                }
                ok
            });
            if applied {
                self.note_on_branch(from, seq, height, hash);
                self.clear_divergence(from);
                self.persist_chain_now();
            }
        } else if block.height <= head.height {
            self.tie_break(from, seq, block);
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
                let hash = self.note_refused(from, &block, "far_future");
                tracing::warn!(height = block.height, head = head.height, %hash, %from, rule = "far_future", "refusing to buffer a block far past the head");
                return;
            }
            self.chain.pending_blocks.retain(|h, _| *h > head.height);
            self.chain.pending_from.insert(block.height, from.to_string());
            self.chain.pending_blocks.insert(block.height, block);
            shed_highest(&mut self.chain.pending_blocks);
            self.try_adopt_from_blob();
            self.request_catchup(head.height + 1);
        }
    }

    /// Verify a block against the current head, append + apply it, and run the
    /// post-apply bookkeeping. Returns whether it was accepted.
    fn apply_next_block(&mut self, from: &str, block: ChainBlock) -> bool {
        // no probe clone: `append_committed_block` verifies before it appends,
        // so an unverifiable block never touches the chain. The probe used to
        // verify the whole chain a SECOND time per block — an exact doubling
        // of the catch-up cost that bought nothing.
        if self.append_block_from(from, block.clone()) {
            // the head moved: no refused block of ours repeats any more
            self.chain.heal_loops.retain(|l| !l.asker);
            let now = self.presence_now();
            self.chain.applied_at.push_back(now);
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
            let from = self.chain.pending_from.remove(&next).unwrap_or_default();
            if !self.apply_next_block(&from, block) {
                break;
            }
        }
        let head_h = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.chain.pending_blocks.retain(|h, _| *h > head_h);
        let buffered: BTreeSet<u64> = self.chain.pending_blocks.keys().copied().collect();
        self.chain.pending_from.retain(|h, _| buffered.contains(h));
    }

    /// Broadcast a catch-up request for every block from `from` onward (deduped
    /// while the same gap is outstanding). No-op if we cannot be behind.
    /// The request carries our `known` heads, so a server on another branch
    /// can serve from the fork point (`chain_reorg.md` R5).
    pub(crate) fn request_catchup(&mut self, from: u64) {
        if self.chain.head.is_none() || self.chain.catchup_from == Some(from) {
            return;
        }
        self.open_catchup(from, self.presence_now());
        self.send_chain_request(from, false);
    }

    fn open_catchup(&mut self, from: u64, now: u64) {
        self.chain.catchup_from = Some(from);
        self.chain.catchup_since = now;
        self.chain.catchup_unheld = 0;
    }

    /// Gap 28: the open catch-up height above the head that no peer holds -
    /// every heard peer reported its own head below it since the ask. A
    /// `beyond_head` request is never answered, so these reports are the
    /// only evidence the asker gets.
    fn catchup_nobody_holds(&self, head: u64) -> Option<u64> {
        let f = self.chain.catchup_from.filter(|f| *f > head && !self.chain.pending_blocks.contains_key(f))?;
        let me = self.member();
        let silent: Vec<String> = self.chain_lag().silent.into_iter().map(|s| s.member).collect();
        let heard: Vec<String> = self.roster().into_iter().filter(|m| *m != me && !silent.contains(m)).collect();
        let below = |m: &String| {
            self.chain.peer_head_at.get(m).is_some_and(|t| *t > self.chain.catchup_since)
                && self.chain.peer_heights.get(m).is_some_and(|h| *h < f)
        };
        let claimed = self.furthest_peer_ahead().is_some_and(|(_, h)| h >= f);
        (!claimed && !heard.is_empty() && heard.iter().all(below)).then_some(f)
    }

    /// Our `(height, hash)` samples, head first, going back geometrically
    /// (head, head-1, head-2, head-4, ...) down to the anchor - at most
    /// [`KNOWN_HEADS_MAX`] entries.
    pub(crate) fn known_heads(&self) -> Vec<molt_core::HeightHash> {
        let Some(head) = self.chain.blocks.last().map(|b| b.height) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut back: u64 = 0;
        while out.len() < KNOWN_HEADS_MAX {
            let Some(h) = head.checked_sub(back) else { break };
            if let Some(b) = self.chain.blocks.iter().find(|b| b.height == h) {
                out.push(molt_core::HeightHash { height: h, hash: self.block_id_of(b) });
            } else {
                break;
            }
            back = if back == 0 { 1 } else { back.saturating_mul(2) };
        }
        out
    }

    /// [`Self::known_heads`] for a retry: the last [`DENSE_KNOWN_HEADS`]
    /// heights one by one, then the geometric samples below them, so the
    /// fork point is found whatever its depth.
    pub(crate) fn dense_known_heads(&self) -> Vec<molt_core::HeightHash> {
        let Some(head) = self.chain.blocks.last().map(|b| b.height) else {
            return Vec::new();
        };
        let floor = head.saturating_sub(DENSE_KNOWN_HEADS);
        let mut out: Vec<molt_core::HeightHash> = self
            .chain
            .blocks
            .iter()
            .rev()
            .take_while(|b| b.height > floor)
            .map(|b| molt_core::HeightHash { height: b.height, hash: self.block_id_of(b) })
            .collect();
        out.extend(self.known_heads().into_iter().filter(|k| k.height <= floor));
        out
    }

    /// The first height of this chain whose identity is v2.
    pub(crate) fn block_v2_from(&self) -> Option<u64> {
        self.chain.head.as_ref().and_then(|h| h.block_v2_from)
    }

    /// The identity of `block` on this chain ([`block_id`]).
    pub(crate) fn block_id_of(&self, block: &ChainBlock) -> String {
        block_id(&self.republic_id(), block, self.block_v2_from())
    }

    /// Whether this node holds the block at `height` with identity `hash`.
    pub(crate) fn holds_block(&self, height: u64, hash: &str) -> bool {
        self.chain.blocks.iter().any(|b| b.height == height && self.block_id_of(b) == hash)
    }

    /// S5: a fallback sealer asks its peers for a block at `height` it may
    /// have missed, outside the catch-up dedup.
    pub(crate) fn ask_for_height(&mut self, height: u64) {
        self.send_chain_request(height, false);
    }

    /// Send a catch-up request outside the dedup of [`Self::request_catchup`]
    /// (the retry and the beacon), with dense samples when asked.
    fn send_chain_request(&mut self, from: u64, dense: bool) {
        let known = if dense { self.dense_known_heads() } else { self.known_heads() };
        let me = self.member();
        self.chain.requests_sent = self.chain.requests_sent.wrapping_add(1);
        let first = known.first().map(|k| short_hash(&k.hash).to_string()).unwrap_or_default();
        let last = known.last().map(|k| short_hash(&k.hash).to_string()).unwrap_or_default();
        let line = format!("{from}:{dense}:{}:{first}:{last}", known.len());
        if let Some(repeated) = self.log_gate("requested", &line) {
            tracing::info!(from, dense, known = known.len(), first = %first, last = %last, repeated, "chain_catchup=requested");
        }
        let env = self.make_env(me, WorkspaceEvent::ChainRequest { from_height: from, known });
        self.record(env);
    }

    /// B3: serve our branch from `fork` to a peer that lost the fork,
    /// at most once per serve debounce per peer.
    fn serve_branch_to(&mut self, peer: &str, fork: u64) {
        if peer.is_empty() {
            return;
        }
        let now = self.presence_now();
        let recently = self
            .chain
            .served_at
            .get(peer)
            .is_some_and(|t| now.saturating_sub(*t) < crate::net::CHAIN_SERVE_DEBOUNCE_SECS);
        if recently {
            return;
        }
        self.chain.served_at.insert(peer.to_string(), now);
        tracing::info!(%peer, fork, "fork=served_winner");
        self.serve_chain_from(peer, fork);
    }

    /// Whether this seat is first in the sealing line (S5): the one that
    /// serves what every seat could serve.
    fn first_in_line(&self) -> bool {
        let next = self.chain.head.as_ref().map_or(0, |h| h.height + 1);
        self.turn_holder(next).is_some_and(|m| m == self.member())
    }

    /// S4/S4a, on the delivery tick: retry a stalled catch-up with backoff,
    /// beacon the head, go loud on a divergence that does not heal, and
    /// re-push the anchor to a restored seat that never arrived.
    pub(crate) fn chain_heal_tick(&mut self) {
        if !self.is_chain_governed() {
            return;
        }
        let Some(head) = self.chain.head.as_ref().map(|h| h.height) else {
            return;
        };
        let now = self.presence_now();
        // a fork that went loud keeps its other branch as evidence
        if self.chain.heal_escalated {
            self.harvest_branch_evidence();
        }
        // candidates that never linked in a while are a branch nobody holds
        // any more; a catch-up mark at or below the head is stale
        if self.chain.fork_candidates.is_empty() {
            self.chain.candidates_since = 0;
        } else if self.chain.candidates_since == 0 {
            self.chain.candidates_since = now;
        } else if now.saturating_sub(self.chain.candidates_since) >= CANDIDATE_TTL_SECS {
            tracing::info!(dropped = self.chain.fork_candidates.len(), "fork=candidates_expired");
            self.chain.fork_candidates.clear();
            self.chain.candidates_since = 0;
        }
        if self.chain.fork_candidates.is_empty() && self.chain.catchup_from.is_some_and(|f| f <= head) {
            self.chain.catchup_from = None;
        }
        if self.chain.catchup_unheld != 0 && self.chain.catchup_unheld <= head {
            self.chain.catchup_unheld = 0;
        }
        if let Some(f) = self.chain.fork_candidates.is_empty().then(|| self.catchup_nobody_holds(head)).flatten() {
            tracing::info!(from = f, head, reason = "nobody_holds", "chain_catchup=retired");
            self.chain.catchup_from = None;
            self.chain.catchup_unheld = f;
        }
        // a peer gone silent cannot heal; it is not this node's trouble
        if self.chain_unsettled() {
            if self.chain.heal_since == 0 {
                self.chain.heal_since = now;
                self.chain.heal_backoff = CATCHUP_RETRY_SECS;
                // the retry delay runs from the request already out
                let asked = if self.chain.catchup_from.is_some() { self.chain.catchup_since } else { now };
                self.chain.heal_next_at = asked.saturating_add(CATCHUP_RETRY_SECS);
            }
            if now >= self.chain.heal_next_at {
                let anchor = self.chain.blocks.first().map_or(0, |b| b.height);
                let from = self
                    .chain
                    .fork_candidates
                    .keys()
                    .next()
                    .map_or(head.saturating_add(1), |l| l.saturating_sub(1).max(anchor.saturating_add(1)));
                // a mark at head+1 would mute a later real gap there
                if from <= head {
                    self.open_catchup(from, now);
                }
                self.send_chain_request(from, true);
                self.chain.heal_backoff = self.chain.heal_backoff.saturating_mul(2).min(HEAL_BACKOFF_MAX_SECS);
                self.chain.heal_next_at = now.saturating_add(self.chain.heal_backoff);
            }
            if !self.chain.heal_escalated && now.saturating_sub(self.chain.heal_since) >= HEAL_STUCK_SECS {
                self.chain.heal_escalated = true;
                self.escalate_unhealed(now);
            }
        } else if self.chain.heal_since != 0 {
            tracing::info!(secs = now.saturating_sub(self.chain.heal_since), "chain_healed");
            self.chain.heal_since = 0;
            self.chain.heal_escalated = false;
        }
        if now >= self.chain.beacon_at {
            if self.chain.beacon_at != 0 {
                self.send_chain_request(head.saturating_add(1), false);
            }
            let offset = self.member().bytes().map(u64::from).sum::<u64>() % 60;
            self.chain.beacon_at = now.saturating_add(CHAIN_BEACON_SECS + offset);
        }
        if now >= self.chain.repush_check_at {
            self.chain.repush_check_at = now.saturating_add(60);
            self.repush_unfinished_restores(now);
        }
        self.evidence_tick(now);
        self.refresh_sync_feedback(now);
    }

    /// R2/R3 (`recovery_feedback_plan.md`): the catch-up and each seat's
    /// sync into the session, at most every two seconds and only on change.
    fn refresh_sync_feedback(&mut self, now: u64) {
        while self.chain.applied_at.front().is_some_and(|t| now.saturating_sub(*t) > 60) {
            self.chain.applied_at.pop_front();
        }
        if now.saturating_sub(self.chain.feedback_emitted_at) < 2 {
            return;
        }
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        // a peer on another branch claims heights this node will never reach
        let target = self.counted_peer_heights().map(|(_, h)| h).max().unwrap_or(0).max(head);
        let catchup = molt_core::CatchUp {
            head,
            target,
            rate: u32::try_from(self.chain.applied_at.len()).unwrap_or(u32::MAX),
            asking: self.chain.catchup_from.is_some(),
            fork_candidates: u32::try_from(self.chain.fork_candidates.len()).unwrap_or(u32::MAX),
        };
        let syncs: Vec<(String, String)> = self.roster().into_iter().map(|m| {
            let s = self.seat_sync_label(&m, head);
            (m, s)
        }).collect();
        let active = self.session.active_workspace.clone();
        let mut changed = self.session.catchup != catchup;
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == active) {
            for m in &mut ws.members {
                if let Some((_, s)) = syncs.iter().find(|(n, _)| *n == m.name) {
                    if m.sync != *s {
                        m.sync = s.clone();
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.session.catchup = catchup;
            self.chain.feedback_emitted_at = now;
            self.emit_session(crate::SessionScope::Full);
        }
    }

    /// R3: one seat's sync as this node sees it.
    pub(crate) fn seat_sync_label(&self, member: &str, head: u64) -> String {
        if member == self.member() {
            return String::new();
        }
        let open = self.chain.proposal_changes.iter().find_map(|(id, c)| match c {
            ChainChange::Membership { op: molt_core::MembershipOp::Restored, member: m, .. }
                if m == member && (self.chain.pending_sigs.contains_key(id) || self.chain.lock.intents.contains_key(id)) =>
            {
                Some(*id)
            }
            _ => None,
        });
        if let Some(id) = open {
            // every seat sees the vote, not only the coordinator: verified
            // signatures plus the returning seat's own consent
            let mut voices: std::collections::BTreeSet<String> =
                self.chain.pending_sigs.get(&id).map(|p| p.verified.iter().cloned().collect()).unwrap_or_default();
            // B7: at a locked height the survivors' intents are the voices
            voices.extend(self.chain.lock.intents.get(&id).into_iter().flatten().cloned());
            if matches!(self.chain.proposal_changes.get(&id), Some(ChainChange::Membership { consent: Some(_), .. })) {
                voices.insert(member.to_string());
            }
            let have = voices.len();
            let need = self.chain.head.as_ref().map_or(0, |h| h.rule_m);
            return format!("recovering:{have}:{need}");
        }
        let anchor = self.chain.blocks.iter().rev().find_map(|b| match &b.change {
            ChainChange::Membership { op: molt_core::MembershipOp::Restored, member: m, nostr_pk, .. } if m == member => {
                Some(nostr_pk.clone().unwrap_or_default())
            }
            _ => None,
        });
        if anchor.is_some_and(|a| self.restore_never_finished(member, &a)) {
            return "readmitted".to_string();
        }
        match self.chain.peer_heights.get(member) {
            Some(h) if h.saturating_add(3) < head => format!("behind:{h}:{head}"),
            _ => String::new(),
        }
    }

    /// The divergence did not heal in [`HEAL_STUCK_SECS`]: a wake, a chat
    /// line from this seat and a warning, once per episode.
    fn escalate_unhealed(&mut self, now: u64) {
        let (peer, since) = self
            .chain
            .diverged
            .values()
            .min_by_key(|d| d.since_height)
            .map(|d| (d.peer.clone(), d.since_height))
            .unwrap_or_else(|| (String::new(), self.chain.fork_candidates.keys().next().copied().unwrap_or(0)));
        let secs = now.saturating_sub(self.chain.heal_since);
        self.announce_heal_stuck(&peer, since, secs);
    }

    /// The loud half of a stuck heal: a warning, a wake, a chat line from
    /// this seat and the session notice.
    fn announce_heal_stuck(&mut self, peer: &str, since: u64, secs: u64) {
        tracing::warn!(%peer, since, secs, "chain_heal_stuck");
        self.wake_trigger(molt_core::kanban_wake::CHAIN_STUCK_REASON, "");
        let me = self.member();
        let body = if peer.is_empty() {
            format!("chain not healed since height {since}")
        } else {
            format!("chain not healed with {peer} since height {since}")
        };
        if let Err(e) = self.post_message(me, body, None, molt_core::ChannelRef::Group) {
            tracing::warn!(error = %e, "chain_heal_stuck chat=failed");
        }
        self.session.notice = format!("chain-heal-stuck:{peer}:{since}");
        self.emit_session(crate::SessionScope::Full);
    }

    /// S4a, review R5: a seal-first restore whose seat was never heard
    /// since its block gets the anchor bootstrap again - for a rejoiner
    /// that listens on the group; an anchor-first one holds it already.
    /// Ends on events: the seat was heard, a later restore of the seat
    /// sealed or is under way, or [`ANCHOR_REPUSH_MAX`] pushes. Pushed by
    /// the node that coordinated it, or by the sealing line's first seat
    /// for a restore sealed after its own open (an older one it cannot
    /// know anybody waits for).
    fn repush_unfinished_restores(&mut self, now: u64) {
        let me = self.member();
        let mut latest: std::collections::BTreeMap<String, (String, u64)> = std::collections::BTreeMap::new();
        for b in &self.chain.blocks {
            if let ChainChange::Membership { op: molt_core::MembershipOp::Restored, member, nostr_pk, .. } = &b.change {
                latest.insert(member.clone(), (nostr_pk.clone().unwrap_or_default(), b.height));
            }
        }
        let unfinished: Vec<((String, String), u64)> = latest
            .into_iter()
            .filter(|(m, (pk, _))| *m != me && self.restore_never_finished(m, pk))
            .map(|(m, (pk, h))| ((m, pk), h))
            .collect();
        let before = (self.recovery.repushes.len(), self.recovery.seal_first_seats.len());
        self.recovery.repushes.retain(|k, _| unfinished.iter().any(|(u, _)| u == k));
        self.recovery.seal_first_seats.retain(|k| unfinished.iter().any(|(u, _)| u == k));
        let mut changed = before != (self.recovery.repushes.len(), self.recovery.seal_first_seats.len());
        let line = self.first_in_line();
        let opened = self.recovery.opened_at;
        let due: Vec<(String, String)> = unfinished
            .into_iter()
            .filter(|(seat, height)| {
                let coordinated = self.recovery.seal_first_seats.contains(seat);
                let sealed_since_open =
                    opened > 0 && self.chain.block_ts.get(height).is_some_and(|ts| *ts >= opened);
                // a reopen is no reason to push at once
                let (count, last) = self.recovery.repushes.get(seat).copied().unwrap_or((0, 0));
                !self.recovery.anchor_first_seats.contains(seat)
                    && !self.restore_superseded(&seat.0)
                    && (coordinated || (line && sealed_since_open))
                    && count < ANCHOR_REPUSH_MAX
                    && now.saturating_sub(last.max(opened)) >= ANCHOR_REPUSH_SECS
            })
            .map(|(seat, _)| seat)
            .collect();
        for seat in &due {
            let e = self.recovery.repushes.entry(seat.clone()).or_insert((0, 0));
            *e = (e.0 + 1, now);
            tracing::info!(member = %seat.0, count = e.0, max = ANCHOR_REPUSH_MAX, "anchor=repushed");
            changed = true;
        }
        if changed {
            self.snapshot_to_disk();
        }
        if !due.is_empty() {
            self.serve_chain_anchor();
        }
    }

    /// A newer run for the seat is under way here: the unfinished restore's
    /// rejoiner is gone by definition.
    fn restore_superseded(&self, member: &str) -> bool {
        self.readmission_in_flight(member) || self.recovery.anchor_first.values().any(|a| a.member == member)
    }

    /// Where a fork-aware request wants serving from: one above the highest
    /// `known` sample that names a block we hold, else `from_height`.
    pub(crate) fn serve_from_for(&self, from_height: u64, known: &[molt_core::HeightHash]) -> u64 {
        known
            .iter()
            .filter(|k| self.holds_block(k.height, &k.hash))
            .map(|k| k.height.saturating_add(1))
            .max()
            .unwrap_or(from_height)
    }

    /// Keep a block of another branch for the deep tie-break (R5); bounded
    /// like the catch-up buffer, shedding the highest when full.
    fn stash_fork_candidate(&mut self, block: ChainBlock) {
        self.chain.fork_candidates.insert(block.height, block);
        shed_highest(&mut self.chain.fork_candidates);
    }

    /// The deep tie-break (`docs_archive/chain/chain_reorg.md`): once the fork
    /// candidates link into our chain at some height f, compare the two
    /// blocks at f (R1); if theirs is smaller, verify our prefix + their
    /// suffix as a whole (R3) and adopt it, returning the displaced
    /// proposals to the vote (R4). A run that does not link yet asks the
    /// peer for the heights below it (R5). Never below the anchor (R2).
    fn try_reorg(&mut self, from: &str) {
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
                if self.block_id_of(&below) == cand.prev {
                    fork = Some(*h);
                    break;
                }
            }
        }
        let Some(f) = fork else {
            // nothing links yet: ask for the run below the lowest candidate
            if let Some((lowest, cand)) = self.chain.fork_candidates.iter().next().map(|(h, c)| (*h, c.clone())) {
                if lowest > anchor.saturating_add(1) {
                    let want = lowest.saturating_sub(1).max(anchor.saturating_add(1));
                    let suppressed = self.chain.catchup_from == Some(want);
                    let theirs = self.block_id_of(&cand);
                    let ours = ours_at(self, lowest);
                    let mine = ours.as_ref().map(|b| self.block_id_of(b)).unwrap_or_default();
                    let (mine_id, theirs_id) = (ours.as_ref().map_or(0, applied_id), applied_id(&cand));
                    let line = format!("{want}:{mine}:{theirs}");
                    if let Some(repeated) = self.log_gate(&format!("unlinked:{from}:{lowest}"), &line) {
                        tracing::info!(
                            %from, lowest, requested = want, suppressed, mine = short_hash(&mine), theirs = short_hash(&theirs),
                            mine_id, theirs_id, repeated, "fork=unlinked"
                        );
                    }
                    if !from.is_empty() {
                        self.note_heal_round(from, true, lowest, &theirs, "unlinked");
                    }
                    self.request_catchup(want);
                }
            }
            return;
        };
        // the contiguous run of candidates from f upward
        let mut suffix: Vec<ChainBlock> = Vec::new();
        let mut h = f;
        // the candidates' own activation: their branch may enable it above f
        let mut v2 = self.block_v2_from().filter(|from| *from <= f);
        while let Some(c) = self.chain.fork_candidates.get(&h) {
            if let Some(prev) = suffix.last() {
                if c.prev != block_id(&self.republic_id(), prev, v2) {
                    break;
                }
                v2 = block_v2_after(v2, prev);
            }
            suffix.push(c.clone());
            h = h.saturating_add(1);
        }
        let Some(ours_f) = ours_at(self, f) else {
            return;
        };
        let (mine, theirs) = (self.block_id_of(&ours_f), self.block_id_of(&suffix[0]));
        let ids = (applied_id(&ours_f), applied_id(&suffix[0]));
        // a branch choice above the fork outranks the hash (rescue plan §9);
        // an attestation variant of the same decisions still relinks
        let chosen = self.chain.blocks.iter().any(|b| b.height >= f && matches!(b.change, ChainChange::BranchChosen { .. }))
            && !suffix.iter().all(|c| ours_at(self, c.height).map_or(true, |o| o.change == c.change));
        if mine <= theirs || chosen {
            // ours wins at the fork (R1): the peer re-bases, not this node
            self.note_fork(from, f, if chosen { "chosen" } else { "kept" }, &mine, &theirs, ids);
            self.chain.fork_candidates.clear();
            // B3: hand the peer our branch instead of waiting for it to ask
            self.serve_branch_to(from, f);
            self.resend_standing_approvals();
            return;
        }
        let mut candidate: Vec<ChainBlock> =
            self.chain.blocks.iter().filter(|b| b.height < f).cloned().collect();
        candidate.extend(suffix.iter().cloned());
        let same_decisions = suffix
            .iter()
            .all(|c| ours_at(self, c.height).map_or(true, |o| o.change == c.change));
        // a relink rewrites `prev`, which a v3 signature covers: re-base instead
        let top = suffix.last().map_or(f, |b| b.height);
        let rebinds = self.chain.blocks.iter().any(|b| b.height > top && prev_bound(self.block_v2_from(), b.height));
        if same_decisions && !rebinds {
            self.relink_onto_variant(from, f, candidate, (&mine, &theirs, ids));
            return;
        }
        if let Err(e) = self.walk_own(&candidate) {
            let rule = refusal_rule(&e);
            let hash = self.note_refused(from, &suffix[0], rule);
            tracing::warn!(%from, fork = f, %hash, rule, error = %e, "the other branch does not verify - dropped");
            self.note_fork(from, f, "dropped", &mine, &theirs, ids);
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
        self.note_fork(from, f, "rebased", &mine, &theirs, ids);
        // the dropped suffix's fold order stamped cards; the other branch
        // folds them differently
        self.forget_wiki_stamps();
        self.adopt_chain(candidate);
        // R4: what our dropped suffix decided and theirs does not is a vote again
        // top-down: the newest restore of a seat finds its card first
        for b in dropped.iter().rev() {
            if let ChainChange::Applied { proposal_id, .. } = &b.change {
                if kept.contains(proposal_id) {
                    continue;
                }
                self.return_displaced_card(*proposal_id, b);
            } else if !suffix.iter().any(|s| s.change == b.change) {
                self.return_displaced_restore(b);
            }
        }
        self.vault_displaced(&dropped, &kept);
        // the adopted suffix's bookkeeping (emits, cleared sigs, the re-base
        // of every open card onto the new head); the park is judged after
        // the clear below, or what it releases is cleared with the rest
        let parked = std::mem::take(&mut self.chain.parked_approvals);
        // every signature collected on the old branch is position-bound to
        // heights that no longer exist here; this node's own decisions
        // re-sign at the new heights, on a healed chain (B5)
        self.in_block_batch(|st| {
            for b in &suffix {
                st.after_block_applied(b);
            }
            st.chain.pending_sigs.clear();
            st.chain.parked_approvals.extend(parked);
            st.chain.fork_candidates.clear();
            st.chain.diverged.clear();
            st.chain.resent_at.clear();
        });
        self.judge_parked_approvals();
        self.persist_chain_now();
        self.emit_session(crate::SessionScope::Full);
        // whatever the peer holds above what we adopted
        if let Some(head) = self.chain.head.as_ref().map(|h| h.height) {
            self.chain.catchup_from = None;
            self.request_catchup(head.saturating_add(1));
        }
    }

    /// The other branch differs from ours only in WHO attested (the link
    /// hash covers the attestation list, member signatures do not cover
    /// `prev`): take its blocks and re-link ours above them. Nothing is
    /// decided again, so a ground variant can move hashes, never history.
    fn relink_onto_variant(
        &mut self,
        from: &str,
        f: u64,
        mut candidate: Vec<ChainBlock>,
        (mine, theirs, ids): (&str, &str, (u64, u64)),
    ) {
        let old_head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let top = candidate.last().map_or(0, |b| b.height);
        for b in self.chain.blocks.iter().filter(|b| b.height > top) {
            let mut b = b.clone();
            b.prev = candidate.last().map(|p| self.block_id_of(p)).unwrap_or_default();
            candidate.push(b);
        }
        self.chain.fork_candidates.clear();
        if let Err(e) = self.walk_own(&candidate) {
            let rule = refusal_rule(&e);
            if let Some(b) = candidate.iter().find(|b| b.height == f).cloned() {
                self.note_refused(from, &b, rule);
            }
            tracing::warn!(%from, fork = f, rule, error = %e, "attestation variant does not verify - dropped");
            self.note_fork(from, f, "dropped", mine, theirs, ids);
            return;
        }
        tracing::info!(%from, fork = f, "attestation variant holds the slot - re-linking");
        self.note_fork(from, f, "relinked", mine, theirs, ids);
        let fresh: Vec<ChainBlock> = candidate.iter().filter(|b| b.height > old_head).cloned().collect();
        self.adopt_chain(candidate);
        self.in_block_batch(|st| {
            for b in &fresh {
                st.after_block_applied(b);
            }
        });
        self.clear_divergence(from);
        self.persist_chain_now();
        if !fresh.is_empty() {
            self.emit_session(crate::SessionScope::Full);
        }
    }

    /// The events that serve `blob`: whole when it fits one frame, else in
    /// parts (S3); nothing when it is too large even for that.
    pub(crate) fn served_blob_events(blob: &molt_core::CheckpointState) -> Vec<WorkspaceEvent> {
        super::blob_parts::blob_events(blob, Self::served_blob_cap()).unwrap_or_default()
    }

    /// The most a served blob event may serialize to: one frame, less a
    /// generous reserve for the envelope around it. An over-budget event is
    /// a PERMANENT publish stall (K6 §4.9.8), so nothing larger is ever
    /// recorded.
    pub(crate) fn served_blob_cap() -> usize {
        const ENVELOPE_RESERVE: usize = 2048;
        crate::proposals::transport_plaintext_ceiling().saturating_sub(ENVELOPE_RESERVE)
    }

    /// Serve a peer's catch-up request from our OWN chain: re-broadcast every
    /// block we hold from `from` onward (as `Committed`, re-authored so the
    /// outbox fans it out). A single survivor thus reconstitutes the chain for
    /// everyone — independent of who originally committed each block.
    pub(crate) fn serve_chain_from(&mut self, peer: &str, from: u64) {
        let blocks: Vec<ChainBlock> = self
            .chain.blocks
            .iter()
            .filter(|b| b.height >= from)
            .cloned()
            .collect();
        let first = blocks.first().map(|b| short_hash(&self.block_id_of(b)).to_string()).unwrap_or_default();
        let last = blocks.last().map(|b| short_hash(&self.block_id_of(b)).to_string()).unwrap_or_default();
        let line = format!("{from}:{}:{first}:{last}", blocks.len());
        if let Some(repeated) = self.log_gate(&format!("served:{peer}"), &line) {
            tracing::info!(%peer, from, blocks = blocks.len(), first = %first, last = %last, repeated, "chain_catchup=served");
        }
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
            if from < anchor.height {
                for ev in Self::served_blob_events(blob) {
                    let env = self.make_env(me.clone(), ev);
                    self.record(env);
                }
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
        if let Some(blob) = self.chain.checkpoint_blob.as_ref() {
            out.extend(Self::served_blob_events(blob));
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
            let bytes = serde_json::to_vec(&ev).map_or(0, |b| b.len());
            let what = match &ev {
                WorkspaceEvent::CheckpointServedPart { index, total, .. } => format!("checkpoint-part {}/{total}", index + 1),
                WorkspaceEvent::CheckpointServed { .. } => "checkpoint".to_string(),
                WorkspaceEvent::Committed(b) => format!("block {}", b.height),
                _ => "other".to_string(),
            };
            tracing::info!(what = %what, bytes, "anchor=served");
            let env = self.make_env(me.clone(), ev);
            self.record(env);
        }
    }

    /// The displaced proposal returns to pending and re-bases —
    /// but ONLY a card with a deliberation behind it (a proposer
    /// this holder learned via gossip). A record MATERIALIZED
    /// from the now-displaced block (`ensure_applied_record`,
    /// by == "") has no vote to return to here: flipping it open
    /// would mint an unowned, unwithdrawable phantom card that
    /// re-gossips forever and blocks auto-checkpoints. Drop it —
    /// the holder returns to "never heard of it", and the WP2
    /// re-serve restores the real card while the vote is open.
    ///
    /// WP2: the seal consumed this seat's signature and the head move
    /// pruned its stance; the displaced block or the stashed voters prove
    /// it, so the approval stands again and re-signs at the new head.
    /// A displaced `Restored` block goes back to the vote like an applied
    /// card: its card reopens (the newest one this seat settled for the
    /// seat) with the block's change registered again, consent included,
    /// and this seat's stance stands where its signature or vote shows it.
    /// The consent is height- and prev-free; a v2 one is re-judged at the
    /// new height. The coordinator's re-key already ran and is not repeated.
    fn return_displaced_restore(&mut self, displaced: &ChainBlock) {
        let ChainChange::Membership { op: MembershipOp::Restored, member, .. } = &displaced.change else {
            return;
        };
        // the card this change sealed; else (a restart forgot it) only a
        // closed card whose own change is gone, never one that kept its own
        let recorded = self.chain.sealed_restores.iter().rposition(|(_, c)| *c == displaced.change);
        let card = match recorded {
            Some(i) => self.chain.sealed_restores.remove(i).map(|(id, _)| id),
            None => self
                .proposals
                .iter()
                .filter(|(id, p)| {
                    p.state == ProposalState::Applied
                        && !self.chain.proposal_changes.contains_key(*id)
                        && p.payload.get("op").and_then(serde_json::Value::as_str) == Some("restore_member")
                        && p.payload.get("member").and_then(serde_json::Value::as_str) == Some(member.as_str())
                })
                .map(|(id, _)| *id)
                .max(),
        };
        let Some(id) = card else {
            // the card comes back with the re-serve of the open governance
            tracing::info!(%member, height = displaced.height, "restore=displaced card=unknown");
            return;
        };
        let me = self.member();
        self.chain.proposal_changes.insert(id, displaced.change.clone());
        if let Some(p) = self.proposals.get_mut(&id) {
            p.state = ProposalState::Proposed;
            let approved = (displaced.sigs.iter().any(|a| a.member == me) || p.voted.contains(&me))
                && !p.decliners.contains(&me);
            if approved {
                self.chain.own_approvals.insert(id);
            }
        }
        tracing::info!(%member, id, height = displaced.height, "restore=displaced");
    }

    fn return_displaced_card(&mut self, proposal_id: u64, displaced: &ChainBlock) {
        let me = self.member();
        let materialized = self.proposals.get(&proposal_id).is_some_and(|p| p.by.is_empty());
        if materialized {
            self.proposals.remove(&proposal_id);
        } else if let Some(p) = self.proposals.get_mut(&proposal_id) {
            p.state = ProposalState::Proposed;
            let approved = (displaced.sigs.iter().any(|a| a.member == me) || p.voted.contains(&me))
                && !p.decliners.contains(&me);
            if approved {
                self.chain.own_approvals.insert(proposal_id);
            }
        }
    }

    /// Resolve a competing block at a slot we already filled: identical block →
    /// a duplicate broadcast, ignore; a different block at the tip with a
    /// smaller hash wins the single branch, so adopt it and re-base the
    /// displaced proposal. A deeper conflict is logged (deep reorg is Phase 3).
    fn tie_break(&mut self, from: &str, seq: Option<u64>, block: ChainBlock) {
        let Some(existing) = self.chain.blocks.iter().find(|b| b.height == block.height) else {
            return;
        };
        let (incoming, current) = (self.block_id_of(&block), self.block_id_of(existing));
        if incoming == current {
            // the block we hold, or another attestation of it (v2): the
            // sender holds our history at this height; the first one stays
            let hash = incoming;
            self.note_on_branch(from, seq, block.height, hash);
            // a shared block below the fork height proves nothing (review R4)
            if self.chain.diverged.get(from).map_or(true, |d| block.height >= d.since_height) {
                self.clear_divergence(from);
            }
            return;
        }
        let rid = self.republic_id();
        let existing_id = applied_id(existing);
        let existing_chosen = matches!(existing.change, ChainChange::BranchChosen { .. });
        let is_tip = self.chain.blocks.last().is_some_and(|b| b.height == block.height);
        // A2.1: a contender on a foreign `prev`, or one below the tip, is
        // not a race the tip rule can settle - the sender is on another
        // branch (a deep re-base is what would reconcile it)
        let shared_prev = self
            .chain
            .blocks
            .iter()
            .find(|b| b.height.saturating_add(1) == block.height)
            .map_or(true, |below| self.block_id_of(below) == block.prev);
        // CHEAP FIRST (review C5): a ground low-hash block costs a full
        // re-walk per frame; the signatures are what any contender must
        // carry, so they are checked against the roster before anything
        // moves (the roster is stable across blocks — `Joined` is refused)
        let signed = self.chain.head.as_ref().is_some_and(|h| {
            block_signers(&rid, &h.identities, &block, h.block_v2_from)
                .is_ok_and(|signers| signers.len() >= usize::from(h.rule_m))
        });
        if shared_prev && existing.change == block.change {
            // the same decision under another attestation list: not a fork
            if incoming < current && signed {
                self.stash_fork_candidate(block);
                self.try_reorg(from);
            }
            return;
        }
        if !shared_prev || !is_tip {
            if self.left_that_branch(from, seq, block.height) {
                tracing::debug!(%from, height = block.height, "fork=stale_block");
                return;
            }
            let since = if shared_prev { block.height } else { block.height.saturating_sub(1) };
            self.note_divergence(from, since, &block);
            self.stash_fork_candidate(block);
            self.try_reorg(from);
            return;
        }
        if is_tip && incoming > current && signed {
            // ours keeps the tip: the sender swaps onto it and needs the
            // standing votes it dropped while its block stood
            self.resend_standing_approvals();
            return;
        }
        if is_tip && incoming < current && !signed {
            let hash = self.note_refused(from, &block, "threshold");
            tracing::warn!(height = block.height, %hash, %from, rule = "threshold", "tie-break contender without a valid threshold - dropped");
            return;
        }
        if is_tip && incoming < current && !existing_chosen {
            let ids = (existing_id, applied_id(&block));
            self.note_fork(from, block.height, "tip_swapped", &current, &incoming, ids);
            // the incoming block wins the tip; swap it in and re-verify
            let displaced = self.chain.blocks.pop();
            self.chain.blocks.push(block.clone());
            // the walk too: a kept stale walk still holds the displaced id
            if let Ok(walk) = self.walk_own(&self.chain.blocks) {
                self.chain.head = Some(walk.head.clone());
                self.chain.walk = Some(walk);
                self.chain.head_moved_at = self.presence_now();
                self.apply_chain_to_state();
                if let Some((ChainChange::Applied { proposal_id, .. }, b)) =
                    displaced.as_ref().map(|b| (&b.change, b))
                {
                    self.return_displaced_card(*proposal_id, b);
                    // the displaced block was folded: the stamps above its
                    // height belong to the fold that just changed
                    self.forget_wiki_stamps();
                } else if let Some(b) = displaced.as_ref().filter(|b| b.change != block.change) {
                    self.return_displaced_restore(b);
                }
                if let Some(b) = displaced.as_ref() {
                    let kept: BTreeSet<u64> = match &block.change {
                        ChainChange::Applied { proposal_id, .. } => BTreeSet::from([*proposal_id]),
                        _ => BTreeSet::new(),
                    };
                    self.vault_displaced(std::slice::from_ref(b), &kept);
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

    /// Record one fork decision: one log line, the status row, the counter.
    fn note_fork(&mut self, from: &str, height: u64, outcome: &str, mine: &str, theirs: &str, ids: (u64, u64)) {
        let (mine, theirs) = (short_hash(mine), short_hash(theirs));
        tracing::info!(%from, fork = height, outcome, mine, theirs, mine_id = ids.0, theirs_id = ids.1, "fork_decision");
        self.chain.fork_decisions = self.chain.fork_decisions.saturating_add(1);
        self.chain.last_fork = Some(molt_core::ForkDecision {
            peer: from.to_string(),
            height,
            outcome: outcome.to_string(),
            mine: mine.to_string(),
            theirs: theirs.to_string(),
            at: self.presence_now(),
        });
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

    /// A5: a height `from` claimed outside any wire frame (raise only).
    #[cfg(test)]
    pub(crate) fn note_peer_height(&mut self, from: &str, height: u64) {
        self.note_peer_claim(from, height, None, PeerClaim::Holds);
    }

    /// A5: what `from` reported in the frame at its log `seq`. A frame no
    /// newer than the last one counted is ignored, so a delayed old frame
    /// cannot re-pin a peer that has since reported lower.
    pub(crate) fn note_peer_claim(&mut self, from: &str, height: u64, seq: Option<u64>, claim: PeerClaim) {
        if from.is_empty() || from == self.member() {
            return;
        }
        if let Some(seq) = seq {
            if self.chain.peer_claim_seq.get(from).is_some_and(|last| seq <= *last) {
                return;
            }
            self.chain.peer_claim_seq.insert(from.to_string(), seq);
        }
        // a replayed old frame is no fresh report
        if seq.is_some_and(|s| self.newest_seen(from, s)) && matches!(claim, PeerClaim::Head) {
            let now = self.presence_now();
            self.chain.peer_head_at.insert(from.to_string(), now);
        }
        let e = self.chain.peer_heights.entry(from.to_string()).or_insert(0);
        *e = match claim {
            PeerClaim::Head => height,
            PeerClaim::Holds => (*e).max(height),
        };
    }

    /// A5: a new incarnation of `member` restarts its seq space and its head.
    pub(crate) fn forget_peer_claim(&mut self, member: &str) {
        self.chain.peer_heights.remove(member);
        self.chain.peer_head_at.remove(member);
        self.chain.peer_claim_seq.remove(member);
        self.chain.peer_on_branch.remove(member);
        self.chain.request_seq.remove(member);
        self.chain.tombstones_served.remove(member);
        self.chain.governance_claim.remove(member);
        self.chain.heard_since.remove(member);
        self.chain.heal_loops.retain(|l| l.peer != member);
        // D3: a new device's declarations carry its own clock
        self.files.caps_at.remove(member);
    }

    /// The claims that count: roster seats on this node's branch - a peer
    /// on another branch claims heights this node will never reach.
    fn counted_peer_heights(&self) -> impl Iterator<Item = (&molt_core::MemberId, u64)> + '_ {
        let roster = self.roster();
        self.chain
            .peer_heights
            .iter()
            .filter(move |(p, _)| !self.chain.diverged.contains_key(*p) && roster.contains(*p))
            .map(|(p, h)| (p, *h))
    }

    /// A6: the furthest counted peer above this node's head, if any.
    pub(crate) fn furthest_peer_ahead(&self) -> Option<(molt_core::MemberId, u64)> {
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.counted_peer_heights()
            .filter(|(_, h)| *h > head)
            .max_by(|(pa, ha), (pb, hb)| ha.cmp(hb).then_with(|| pb.cmp(pa)))
            .map(|(p, h)| (p.clone(), h))
    }

    /// S1: the sync state `status` reports.
    pub(crate) fn chain_sync(&self) -> molt_core::ChainSync {
        let blob_bytes = self.chain.checkpoint_blob.as_ref().map_or(0, |blob| {
            serde_json::to_vec(&WorkspaceEvent::CheckpointServed { blob: blob.clone() })
                .map_or(0, |b| u64::try_from(b.len()).unwrap_or(u64::MAX))
        });
        let (lock_active, lock_ballot, lock_wait) = self.lock_status();
        let (ahead_peer, ahead_height) = self.furthest_peer_ahead().unwrap_or_default();
        let (block_v2_since, block_v2_wait) = self.block_v2_status();
        molt_core::ChainSync {
            cut_wait: self.cut_wait(),
            block_v2: self.block_v2_from().is_some(),
            block_v2_since,
            block_v2_wait,
            ahead_peer,
            ahead_height,
            lock_active,
            lock_ballot,
            lock_wait,
            lock_since: self.lock_since(),
            head_hash: self.chain.head.as_ref().map(|h| h.hash.clone()).unwrap_or_default(),
            catchup_from: self.chain.catchup_from.unwrap_or(0),
            catchup_since: if self.chain.catchup_from.is_some() { self.chain.catchup_since } else { 0 },
            catchup_unheld: self.chain.catchup_unheld,
            fork_candidates: u32::try_from(self.chain.fork_candidates.len()).unwrap_or(u32::MAX),
            fork_lowest: self.chain.fork_candidates.keys().next().copied().unwrap_or(0),
            fork_highest: self.chain.fork_candidates.keys().next_back().copied().unwrap_or(0),
            fork_decisions: self.chain.fork_decisions,
            last_fork: self.chain.last_fork.clone(),
            blob_bytes,
            blob_cap: u64::try_from(Self::served_blob_cap()).unwrap_or(u64::MAX),
            refused: self.chain.refusals.iter().cloned().collect(),
            heal_stuck: self.heal_stuck(),
            abandoned: self.chain.branch.file.abandoned.clone(),
        }
    }

    /// G3: the oldest catch-up round that repeats, else a divergence that
    /// went loud.
    fn heal_stuck(&self) -> Option<molt_core::HealStuck> {
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let looping = self
            .chain
            .heal_loops
            .iter()
            .filter(|l| l.rounds >= HEAL_LOOP_ROUNDS && (!l.asker || l.height > head))
            .min_by_key(|l| l.since)
            .map(|l| molt_core::HealStuck { peer: l.peer.clone(), since: l.since, height: l.height, rule: l.rule.clone() });
        looping.or_else(|| {
            self.chain.heal_escalated.then(|| {
                let (peer, height) = self
                    .chain
                    .diverged
                    .values()
                    .min_by_key(|d| d.since_height)
                    .map(|d| (d.peer.clone(), d.since_height))
                    .unwrap_or_default();
                molt_core::HealStuck { peer, since: self.chain.heal_since, height, rule: String::new() }
            })
        })
    }

    /// H1: keep a refused block for `status` and count it as a heal round
    /// with its sender; the block's short hash, for the caller's line.
    pub(crate) fn note_refused(&mut self, from: &str, block: &ChainBlock, rule: &str) -> String {
        let full = self.block_id_of(block);
        let hash = short_hash(&full).to_string();
        let at = self.presence_now();
        self.chain.refusals.push_back(molt_core::BlockRefusal {
            height: block.height,
            hash: hash.clone(),
            from: from.to_string(),
            rule: rule.to_string(),
            at,
        });
        while self.chain.refusals.len() > REFUSALS_KEPT {
            self.chain.refusals.pop_front();
        }
        if !from.is_empty() && from != self.member() {
            self.note_heal_round(from, true, block.height, &full, rule);
        }
        hash
    }

    /// G3: one more round with `peer` that moved nothing - the same refused
    /// block (`asker`) or the same request. Loud once at
    /// [`HEAL_LOOP_ROUNDS`].
    fn note_heal_round(&mut self, peer: &str, asker: bool, height: u64, key: &str, rule: &str) {
        let now = self.presence_now();
        let pos = self.chain.heal_loops.iter().position(|l| l.peer == peer && l.asker == asker);
        let same = pos.is_some_and(|i| self.chain.heal_loops[i].height == height && self.chain.heal_loops[i].key == key);
        // the asker's round is one of its own requests: a served batch
        // repeats its refused block per frame, not per round
        let mark = (peer.to_string(), asker);
        let asked = self.chain.requests_sent;
        if same && asker && self.chain.round_marks.get(&mark) == Some(&asked) {
            return;
        }
        self.chain.round_marks.insert(mark, asked);
        let i = match pos {
            Some(i) if same => {
                let l = &mut self.chain.heal_loops[i];
                l.rounds = l.rounds.saturating_add(1);
                l.rule = rule.to_string();
                i
            }
            _ => {
                let fresh = molt_core::HealLoop {
                    peer: peer.to_string(),
                    asker,
                    height,
                    key: key.to_string(),
                    rounds: 1,
                    since: now,
                    rule: rule.to_string(),
                    loud: false,
                };
                match pos {
                    Some(i) => {
                        self.chain.heal_loops[i] = fresh;
                        i
                    }
                    None => {
                        self.chain.heal_loops.push(fresh);
                        self.chain.heal_loops.len() - 1
                    }
                }
            }
        };
        let l = &mut self.chain.heal_loops[i];
        if l.rounds >= HEAL_LOOP_ROUNDS && !l.loud {
            l.loud = true;
            let (rounds, since, rule) = (l.rounds, l.since, l.rule.clone());
            tracing::info!(%peer, asker, height, rounds, rule = %rule, "heal=stuck");
            self.announce_heal_stuck(peer, height, now.saturating_sub(since));
        }
    }

    /// A catch-up request from `from` (frame `seq`): its top sample on our
    /// branch is evidence it left any other; a request that repeats while
    /// both heads stand still is a heal round, one beyond our head ends it.
    pub(crate) fn note_request_round(
        &mut self,
        from: &str,
        seq: u64,
        known: &[molt_core::HeightHash],
        serve_from: u64,
        beyond_head: bool,
    ) {
        let top = known.iter().max_by_key(|k| k.height);
        if let Some(top) = top {
            if self.holds_block(top.height, &top.hash) {
                self.note_on_branch(from, Some(seq), top.height, top.hash.clone());
            }
        }
        if beyond_head {
            self.chain.heal_loops.retain(|l| l.peer != from || l.asker);
            return;
        }
        let key = top.map(|k| k.hash.clone()).unwrap_or_default();
        self.note_heal_round(from, false, serve_from, &key, "");
    }

    /// F1: frame `seq` of `from` showed it holding our block at `height`.
    fn note_on_branch(&mut self, from: &str, seq: Option<u64>, height: u64, hash: String) {
        let Some(seq) = seq else { return };
        if from.is_empty() || self.chain.peer_on_branch.get(from).is_some_and(|(at, _, _)| *at >= seq) {
            return;
        }
        self.chain.peer_on_branch.insert(from.to_string(), (seq, height, hash));
    }

    /// F1: a newer frame of `from` than this one showed it on our branch
    /// at or above `block_height`, and we still hold what it named - the
    /// block is from a branch `from` has left, not a divergence. Evidence
    /// below it is a shared prefix and proves nothing (review R4).
    fn left_that_branch(&self, from: &str, seq: Option<u64>, block_height: u64) -> bool {
        let (Some(seq), Some((at, height, hash))) = (seq, self.chain.peer_on_branch.get(from)) else {
            return false;
        };
        seq < *at && *height >= block_height && self.holds_block(*height, hash)
    }

    /// H1: whether frame `seq` is the newest `ChainRequest` of `from` in
    /// its current incarnation; a re-delivered older one is history.
    pub(crate) fn fresh_chain_request(&mut self, from: &str, seq: u64) -> bool {
        if self.chain.request_seq.get(from).is_some_and(|last| seq <= *last) || !self.newest_seen(from, seq) {
            return false;
        }
        self.chain.request_seq.insert(from.to_string(), seq);
        true
    }

    /// A4: whether a line under `key` goes out now - `Some(folded)` with
    /// how many identical lines were held back since the last one, `None`
    /// while the same text repeats within [`LOG_GATE_SECS`].
    pub(crate) fn log_gate(&mut self, key: &str, text: &str) -> Option<u32> {
        let now = self.presence_now();
        if let Some((last, at, folded)) = self.chain.log_gate.get_mut(key) {
            if last == text && now.saturating_sub(*at) < LOG_GATE_SECS {
                *folded = folded.saturating_add(1);
                return None;
            }
            let held = *folded;
            *last = text.to_string();
            *at = now;
            *folded = 0;
            return Some(held);
        }
        self.chain.log_gate.insert(key.to_string(), (text.to_string(), now, 0));
        Some(0)
    }

    /// G3: loops restored from the snapshot count again only after one
    /// fresh round.
    pub(crate) fn restore_heal_loops(&mut self, loops: &[molt_core::HealLoop]) {
        self.chain.heal_loops = loops
            .iter()
            .cloned()
            .map(|mut l| {
                l.rounds = l.rounds.min(HEAL_LOOP_ROUNDS - 1);
                l
            })
            .collect();
    }


    /// A5: how far the peers are ahead, and who has gone quiet. The two
    /// facts a partitioned seat cannot see for itself - `chain_diverged`
    /// answers a different question (contradiction, not distance).
    pub(crate) fn chain_lag(&self) -> molt_core::ChainLag {
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let peers_ahead = self.furthest_peer_ahead().map_or(0, |(_, h)| h.saturating_sub(head));
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
            let last = self.last_signed(&id.member).unwrap_or(0);
            if last + STALE_AFTER_BLOCKS <= head.height {
                out.push(molt_core::StaleSigner {
                    member: id.member.clone(),
                    last_signed_height: last,
                });
            }
        }
        out
    }

    /// The highest height `member` signed for: in a block this holder
    /// keeps, or verified here before a seal.
    pub(crate) fn last_signed(&self, member: &str) -> Option<u64> {
        let sealed = self
            .chain
            .blocks
            .iter()
            .filter(|b| b.sigs.iter().any(|a| a.member == member))
            .map(|b| b.height)
            .max();
        sealed.max(self.chain.seen_signing.get(member).copied())
    }

    /// Where each roster seat stands, as far as this node knows.
    pub(crate) fn seat_syncs(&self, persistent: &std::collections::HashSet<molt_core::MessageId>) -> Vec<molt_core::SeatSync> {
        let me = self.member();
        let now = self.presence_now();
        let holders = self.mirror_holders();
        let governed = self.is_chain_governed();
        self.roster()
            .into_iter()
            .map(|member| {
                let own = member == me;
                let height = if !governed {
                    None
                } else if own {
                    self.chain.head.as_ref().map(|h| h.height)
                } else {
                    self.chain.peer_heights.get(&member).copied()
                };
                let files = holders
                    .iter()
                    .filter(|(id, who)| persistent.contains(id) && who.contains(&member))
                    .count();
                molt_core::SeatSync {
                    last_seen: if own { now } else { self.member_last_seen(&member) },
                    height,
                    signed: if governed { self.last_signed(&member) } else { None },
                    files: u32::try_from(files).unwrap_or(u32::MAX),
                    dup_streak: self.dup_streak(&member),
                    restore_unfinished: self.recovery.unfinished.contains(&member),
                    evidence: self.seat_evidence(&member),
                    member,
                    ..Default::default()
                }
            })
            .collect()
    }

    /// A block from `from` that fits our chain: whatever it was on, it is
    /// on our branch now.
    pub(crate) fn clear_divergence(&mut self, from: &str) {
        if self.chain.diverged.remove(from).is_some() {
            tracing::info!(%from, "peer is back on this branch");
            // what it claimed above our head was on the branch it left
            let head = self.chain.head.as_ref().map_or(0, |h| h.height);
            if let Some(h) = self.chain.peer_heights.get_mut(from) {
                *h = (*h).min(head);
            }
            self.chain.resent_at.clear();
            self.sign_held_approvals();
            self.resend_standing_approvals();
            self.emit_session(crate::SessionScope::Full);
        }
    }
}
