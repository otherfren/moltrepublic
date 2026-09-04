// SPDX-License-Identifier: GPL-3.0-or-later

//! **The holder's projection of its verified chain into `State`**: block 0
//! from a sealed roster, adopting a chain (full walk, hard-reject), the
//! per-block and whole-chain folds into the chain-owned applied logs, the
//! working transport anchors and the relay ledger, the effective relay
//! pool / feature set every reader asks for, settling the ephemeral
//! proposal cards against chain truth, and the co-equal Chain-History
//! read. Nothing here signs or verifies — that is [`super::verify`] and
//! [`super::governance`].

use super::*;

impl State {
    /// Build block 0 of the persistent chain from a sealed roster — but only
    /// for a **real** founding (a content-derived republic id and one
    /// attestation per member). A pre-ritual/demo materialize gets no chain
    /// (empty) and stays on the honest single-operator path.
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
                // the pool the founders SIGNED — genesis approval_bytes is
                // roster_canonical_bytes, so this must be exactly what the
                // attestations below were made over
                relays: sealed.relays.clone(),
                // same rule for the ratified feature set (roster-v5):
                // presence decides the tag the verifier recomputes under
                features: sealed.features.clone(),
            },
            sigs: sealed.attestations.clone(),
        }]
    }

    /// Verify a freshly-loaded or freshly-built chain and adopt it as the open
    /// workspace's chain + head, then re-project the persistent state from it.
    /// A chain that fails verification is **hard-rejected**: the head stays
    /// `None` and nothing is projected (a partially-trusted chain could fork
    /// state — `docs_archive/chain/persistent_chain.md`).
    pub(crate) fn adopt_chain(&mut self, chain: Vec<ChainBlock>) {
        // a chain from OUTSIDE this holder's own verified prefix: always the
        // full walk, never the cache. The walk it produces is then kept, so
        // the adoption pays for the next append too.
        match self.walk_own(&chain) {
            Ok(walk) => {
                self.chain.blocks = chain;
                self.chain.head = Some(walk.head.clone());
                self.chain.walk = Some(walk);
                self.bump_next_id_past_chain();
                self.apply_chain_to_state();
            }
            Err(e) => {
                tracing::warn!(error = %e, "rejecting an unverifiable chain");
                self.chain.blocks.clear();
                self.chain.head = None;
                self.chain.walk = None;
                self.set_checkpoint_blob(None);
            }
        }
    }

    /// The highest proposal id a chain has consumed (`Applied` blocks) —
    /// what the mint counter must clear BEFORE the ephemeral tail replays
    /// (review E1 residual: a blob-seeded rejoiner's tail replayed with the
    /// gate closed while the counter was still at its snapshot value).
    pub(crate) fn max_applied_proposal_id(blocks: &[ChainBlock]) -> Option<u64> {
        blocks
            .iter()
            .filter_map(|b| match &b.change {
                ChainChange::Applied { proposal_id, .. } => Some(*proposal_id),
                _ => None,
            })
            .max()
    }

    /// The mint counter must stay AHEAD of every proposal id the verified
    /// chain has consumed: `receive_proposed` (and its membership twin)
    /// refuses an already-consumed id on every peer, so a locally minted
    /// collision could never seal — a silent liveness hole for any holder
    /// that adopted its chain without the ephemeral event log to bump
    /// `next_id` for it (a blob-seeded rejoiner after total loss). Called
    /// wherever the walk adopts or extends; `max` keeps it monotone.
    pub(super) fn bump_next_id_past_chain(&mut self) {
        if let Some(top) = self
            .chain.walk
            .as_ref()
            .and_then(|w| w.seen.iter().next_back())
        {
            self.next_id = self.next_id.max(top.saturating_add(1));
        }
    }

    /// Set (or clear) the checkpoint anchor. **The one way to do it** — the
    /// cached walk is seeded from the blob (`seen` from its consumed ids,
    /// `running` from its state), so a blob swap must invalidate it. The
    /// chain-shape backstop in [`ChainWalk::describes`] cannot see a blob
    /// replaced at the same coverage, which is why this is a setter and not
    /// a comment asking callers to remember.
    pub(crate) fn set_checkpoint_blob(&mut self, blob: Option<molt_core::CheckpointState>) {
        self.chain.checkpoint_blob = blob;
        self.chain.walk = None;
        // the blob SEEDS the Memory projection, so the fold cache it fed
        // describes a base that no longer exists (§4.1)
        self.bump_applied_epoch();
    }

    /// The transport anchor to ADDRESS this member at right now: the seat's
    /// re-anchored key if a `Restored` block gave it one, else the immutable
    /// founding anchor from the roster. Empty for an unknown member — never
    /// somebody else's key.
    ///
    /// Every gift-wrap send resolves through this (the Nostr re-key's
    /// Welcome, the reattach request's targets, the live-anchor replay
    /// check at ingest). Reaching for `identities[i].nostr_pk` directly
    /// addresses a key a recovered member no longer holds, and the send
    /// simply vanishes.
    pub(crate) fn working_nostr_pk(&self, member: &str) -> String {
        if let Some(pk) = self.chain.anchors.get(member) {
            return pk.clone();
        }
        self.replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.nostr_pk.clone())
            .unwrap_or_default()
    }

    /// The relays `member` is on record as reaching (R3b, the ledger): its
    /// declared pool if a `Membership` block carried one, else the ratified
    /// GROUP pool — a founding member never declared anything because the
    /// genesis pool it co-signed covers it. The split-detection input (R4).
    pub(crate) fn member_relays(&self, member: &str) -> Vec<String> {
        if let Some(declared) = self.chain.member_relays.get(member) {
            return declared.clone();
        }
        self.effective_relays()
    }

    /// R4 — split detection: every pair of roster members whose EFFECTIVE
    /// relay sets do not intersect, `(a, b)` in roster order. Such a pair
    /// can never exchange a frame no matter how healthy each side's own
    /// relay is, so the republic's threshold may silently be unable to
    /// assemble — a named state, never a silence. Computable by every
    /// member from the same chain (the ledger, R3b).
    pub(crate) fn relay_splits(&self) -> Vec<(molt_core::MemberId, molt_core::MemberId)> {
        let roster = self.roster();
        let mut out = Vec::new();
        for i in 0..roster.len() {
            for j in i + 1..roster.len() {
                let a = self.member_relays(&roster[i]);
                let b = self.member_relays(&roster[j]);
                // no data is no verdict: a non-Nostr chain has no pools, and
                // an empty side must not read as "split from everyone"
                if a.is_empty() || b.is_empty() {
                    continue;
                }
                if a.iter().any(|r| b.contains(r)) {
                    continue;
                }
                out.push((roster[i].clone(), roster[j].clone()));
            }
        }
        out
    }

    /// R4: log every split pair ONCE (structured, greppable) — the run-log
    /// half of the verdict; the members surface carries the per-member
    /// marker. Rides every chain adoption/append.
    pub(crate) fn note_relay_splits(&mut self) {
        for (a, b) in self.relay_splits() {
            if self.chain.split_noted.insert((a.clone(), b.clone())) {
                let bridge = self.member_relays(&a).first().cloned().unwrap_or_default();
                tracing::warn!(%a, %b, %bridge, "relay split - no shared relay");
            }
        }
    }

    /// The EFFECTIVE group pool (R6): the latest applied `set_relays` edit,
    /// else the ratified founding pool. This is the answer every reader
    /// wants — the pool as governed, not as founded.
    pub(crate) fn effective_relays(&self) -> Vec<String> {
        let mut pool = self.ratified_relays();
        for v in self.applied_org_entries() {
            if v.get("op").and_then(serde_json::Value::as_str) == Some("set_relays") {
                Self::fold_pool_edit(
                    &mut pool,
                    v.get("value").and_then(serde_json::Value::as_str).unwrap_or_default(),
                );
            }
        }
        pool
    }

    /// The EFFECTIVE feature set (`charter_features.md` D5): the ratified
    /// founding baseline unioned with every applied `set_features` edit,
    /// sorted + deduped.
    ///
    /// **The fold is a UNION on purpose** — the deterministic twin of the
    /// propose-time enable-only gate: a block that tried to drop a feature
    /// folds as pure addition on every holder, so "features can never be
    /// switched off" is a construction property, not a courtesy (the
    /// `fold_pool_edit` lesson). Unknown keys are kept — an older build
    /// must not un-enable what a newer one ratified; readers ignore keys
    /// they cannot render.
    ///
    /// The baseline (D6, user-decided 2026-08-11): a republic founded
    /// before roster-v5 (`features: None`) keeps exactly what was usable
    /// before the gating existed — Shared Memory.
    pub(crate) fn effective_features(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> =
            match self.replica.as_ref().and_then(|r| r.features.clone()) {
                Some(f) => f.into_iter().collect(),
                None => molt_core::Surface::LEGACY_FEATURES
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            };
        for v in self.applied_org_entries() {
            if v.get("op").and_then(serde_json::Value::as_str) == Some("set_features") {
                let value =
                    v.get("value").and_then(serde_json::Value::as_str).unwrap_or_default();
                set.extend(value.split_whitespace().map(str::to_string));
            }
        }
        set.into_iter().collect()
    }

    /// The D7 gate: refuse an optional surface the charter has not enabled.
    /// The nav HIDES such a surface; this is the engine-side twin an MCP
    /// agent meets (co-equality — clickable or refused must be one verdict).
    pub(crate) fn require_feature(&self, surface: Surface) -> Result<(), molt_core::MoltError> {
        if surface.is_charter_feature()
            && !self.effective_features().iter().any(|f| f == surface.as_str())
        {
            return Err(molt_core::MoltError::FeatureDisabled(surface.as_str()));
        }
        Ok(())
    }

    /// The R6 fold rule both effective views share (`effective_relays` and
    /// `org_effective`): an applied `set_relays` entry replaces the pool
    /// only if it is non-empty AND shares a relay with the pool accumulated
    /// so far — make-before-break at the FOLD, the only place every holder
    /// passes deterministically. The propose-time gates are local courtesy;
    /// a peer on another build (or a hand-crafted payload) bypasses them,
    /// and two individually-legal pending edits can compose into a
    /// zero-overlap transition (review 2026-08-09). A zero-overlap
    /// transition applied for real would tear the republic at that commit,
    /// so it deterministically becomes a no-op instead.
    pub(crate) fn fold_pool_edit(pool: &mut Vec<String>, value: &str) {
        let parsed: Vec<String> = value.split_whitespace().map(str::to_string).collect();
        if parsed.is_empty() {
            return;
        }
        if !pool.is_empty() && !pool.iter().any(|r| parsed.contains(r)) {
            return;
        }
        *pool = parsed;
    }

    /// R6: the governed pool moved — carry it into the LIVE transport. The
    /// runtime rebuild is the accepted whole-group blip (Track C option A,
    /// 2026-07-23); the ratchet is handed over as the SHARED Arc, exactly
    /// like the mesh-extension rebuild, so no sender generation is reused.
    /// A workspace without a live runtime just adopts the list.
    pub(crate) fn adopt_pool_change(&mut self) {
        let pool = self.effective_relays();
        let Some(nostr) = self.nostr.as_mut() else {
            return;
        };
        if pool.is_empty() || nostr.relays == pool {
            return;
        }
        nostr.relays = pool;
        if let Some(old) = self.group_net.take() {
            tracing::info!(
                relays = self.nostr.as_ref().map_or(0, |n| n.relays.len()),
                "the governed relay pool moved - rebuilding the group runtime"
            );
            let mls = old.mls.clone();
            // dropping the handle latches the stop (watch, not Notify) —
            // the old outbox ends at its next poll
            drop(old);
            self.group_net = self.build_group_net_shared(mls);
        }
    }

    /// The ratified GROUP pool: the checkpoint summary's if this holder
    /// pruned, else the genesis block's. Empty on a non-Nostr chain.
    pub(crate) fn ratified_relays(&self) -> Vec<String> {
        if let Some(blob) = &self.chain.checkpoint_blob {
            return blob.relays.clone();
        }
        self.chain.blocks
            .first()
            .and_then(|b| match &b.change {
                ChainChange::Genesis { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Fold ONE freshly-appended block into the projection.
    ///
    /// [`State::apply_chain_to_state`] rebuilds from the whole chain, which is
    /// right when entries must DISAPPEAR (a re-base, a prune) and wrong for an
    /// append: it re-clones every payload in the chain for every block, so a
    /// catch-up draining N blocks cloned the applied log N²/2 times.
    ///
    /// An append can only add, so this runs the same three folds for the one
    /// block — the projection it produces is the one the full rebuild would.
    pub(super) fn project_one(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } => {
                self.chain.applied
                    .entry(*surface)
                    .or_default()
                    .push((Some(*proposal_id), payload.clone()));
                self.chain.applied_sigs
                    .insert(*proposal_id, block.sigs.clone());
                // R6: a committed pool edit reaches the live transport
                if payload.get("op").and_then(serde_json::Value::as_str) == Some("set_relays") {
                    self.adopt_pool_change();
                }
                // the Memory base moved — the supersede walk retires
                // pending wiki patches it left behind (deterministic:
                // this runs on append, catch-up and rebuild alike)
                if *surface == Surface::Memory {
                    let moved = Self::wiki_payload_paths(payload);
                    self.supersede_stale_wiki(moved.as_ref());
                }
            }
            // the LAST Restored block for a seat wins, and an append is the
            // last; an empty anchor leaves the previous one standing, exactly
            // as the full rebuild's insert-only fold does
            ChainChange::Membership {
                member,
                nostr_pk,
                relays,
                ..
            } => {
                if let Some(pk) = nostr_pk.as_ref().filter(|p| !p.is_empty()) {
                    self.chain.anchors.insert(member.clone(), pk.clone());
                }
                // R3b: the relay ledger follows the same last-wins fold
                if !relays.is_empty() {
                    self.chain.member_relays.insert(member.clone(), relays.clone());
                    self.note_relay_splits();
                }
            }
            _ => {}
        }
        self.adopt_head_roster();
    }

    /// The verified head carries the roster after every membership block —
    /// adopt it so the newcomers/rekeys show up in the roster + approvals.
    /// The one fold both the per-block append and the whole-chain rebuild
    /// end on.
    fn adopt_head_roster(&mut self) {
        if let Some(head) = &self.chain.head {
            if let Some(r) = &mut self.replica {
                r.identities = head.identities.clone();
                r.roster = head.identities.iter().map(|i| i.member.clone()).collect();
            }
        }
    }

    /// The chain is the durable record; the `Proposed` gossip is ephemeral
    /// RAM on every RECEIVER (only the proposer's own log carries it). A
    /// holder that adopts an Applied block without the card — a reopen, a
    /// catch-up past lost gossip — materializes the record FROM the block,
    /// so the Accepted view keeps its id, title, patch shape and (via the
    /// sealed sigs, resolved in `view`) its voters. The proposer stays
    /// unattributed: the block does not record it, and inventing one would
    /// be a forgery.
    pub(super) fn ensure_applied_record(
        &mut self,
        proposal_id: u64,
        surface: Surface,
        payload: serde_json::Value,
    ) {
        self.proposals
            .entry(proposal_id)
            .or_insert_with(|| molt_core::ProposalRecord {
                surface,
                payload,
                approvals: 0,
                state: ProposalState::Applied,
                declined_at: 0,
                declined_by: molt_core::MemberId::new(),
                decliners: Vec::new(),
                voted: Vec::new(),
                by: molt_core::MemberId::new(),
                superseded: false,
                withdrawn: false,
            });
    }

    /// Settle the gossip-built proposal cards against the verified chain:
    /// every proposal a block (or the checkpoint blob below a cut) consumed
    /// shows Applied, every sealed membership change settles its
    /// content-matched cards. Idempotent — the re-base/prune rebuilds run it
    /// harmlessly; the reopen order makes it load-bearing. Deliberation is
    /// ephemeral: after a replay only chain truth remains, so a card the
    /// chain consumed can only honestly read Applied.
    fn settle_cards_against_chain(&mut self) {
        // ONE pass over blob + chain: records missing entirely (this holder
        // never was the proposer and its ephemeral gossip is gone) come
        // back from the durable evidence — the blob's summarized applied
        // payloads below the cut (their voter pills stay open: only
        // chain-provable votes are shown, the sigs went with the cut, and
        // without this a pruned and an unpruned holder of the SAME republic
        // showed different Accepted tables) and the live blocks above it.
        // Replay-resurrected open cards settle to Applied.
        let mut materialize: Vec<(u64, Surface, serde_json::Value)> = Vec::new();
        let mut settle: Vec<(u64, Surface)> = Vec::new();
        if let Some(blob) = &self.chain.checkpoint_blob {
            for (surface, entries) in &blob.applied {
                for (id, payload) in entries {
                    match self.proposals.get(id) {
                        None => materialize.push((*id, *surface, payload.clone())),
                        Some(p) if p.state != ProposalState::Applied => {
                            settle.push((*id, *surface));
                        }
                        _ => {}
                    }
                }
            }
            // consumed ids whose payload the summary dropped (LWW slots):
            // no card to materialize, but a surviving open card still
            // settles — the id was decided
            for id in &blob.consumed_ids {
                if let Some(p) = self.proposals.get(id) {
                    if p.state != ProposalState::Applied {
                        settle.push((*id, p.surface));
                    }
                }
            }
        }
        for block in &self.chain.blocks {
            if let ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } = &block.change
            {
                match self.proposals.get(proposal_id) {
                    None => materialize.push((*proposal_id, *surface, payload.clone())),
                    Some(p) if p.state != ProposalState::Applied => {
                        settle.push((*proposal_id, *surface));
                    }
                    _ => {}
                }
            }
        }
        for (id, surface, payload) in materialize {
            self.ensure_applied_record(id, surface, payload);
            // signatures that outran the card this holder never had: stash
            // their voters onto the record it just built, then drop them —
            // the id is decided
            self.forget_vote(id);
        }
        for (id, surface) in settle {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Applied;
            }
            self.forget_vote(id);
            self.emit(Event::Applied {
                id: ProposalId(id),
                surface,
            });
        }
        // membership blocks carry no proposal id — settle by content, the
        // `after_block_applied` pattern
        let membership: Vec<ChainChange> = self
            .chain.blocks
            .iter()
            .filter(|b| matches!(b.change, ChainChange::Membership { .. }))
            .map(|b| b.change.clone())
            .collect();
        for change in membership {
            self.settle_membership_records(&change);
        }
    }

    /// Re-project the persistent state from the whole chain: the gated
    /// surfaces' applied logs (into the chain-owned [`ChainProjection::applied`], a
    /// full clear-and-refold so a re-base is free) and the roster/identities
    /// (taken from the already-verified head, which evolved them across the
    /// membership blocks). Chat, [`State::applied`] and pending proposals are
    /// left untouched — they are ephemeral or legacy-owned.
    pub(crate) fn apply_chain_to_state(&mut self) {
        let mut projected: std::collections::HashMap<
            Surface,
            Vec<(Option<u64>, serde_json::Value)>,
        > = std::collections::HashMap::new();
        // WP4b: a pruned holder seeds the projection from the checkpoint
        // blob — the pre-cut applied entries stay readable after the drop
        if let Some(blob) = &self.chain.checkpoint_blob {
            for (surface, entries) in &blob.applied {
                let list = projected.entry(*surface).or_default();
                for (id, payload) in entries {
                    list.push((Some(*id), payload.clone()));
                }
            }
        }
        let mut sigs: std::collections::HashMap<u64, Vec<molt_core::RosterAttestation>> =
            std::collections::HashMap::new();
        for block in &self.chain.blocks {
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
                sigs.insert(*proposal_id, block.sigs.clone());
            }
        }
        self.chain.applied = projected;
        self.chain.applied_sigs = sigs;
        // a wholesale re-projection can also REMOVE entries (a re-base, a
        // prune): the fold cache cannot be extended across it (§4.1)
        self.bump_applied_epoch();
        // the gossip-replayed proposal CARDS are older than the chain on a
        // reopen (`open_stored_workspace` replays them first) — settle them
        // against the verified truth or every restart resurrects decided
        // votes as open cards
        self.settle_cards_against_chain();
        // …and the supersede walk reaches the same terminal states a live
        // node reached (shared_memory_real.md §4 replay determinism)
        self.supersede_stale_wiki(None);
        // …and the working transport anchors. A pruned holder SEEDS them from
        // the blob: the `Restored` blocks that established them were dropped
        // at the cut, and the roster keeps each seat's founding anchor by
        // design — so folding the surviving suffix alone would silently
        // re-address every recovered member to the key it no longer holds.
        let mut anchors: std::collections::HashMap<molt_core::MemberId, String> = self
            .chain.checkpoint_blob
            .as_ref()
            .map(|b| b.anchors.iter().cloned().collect())
            .unwrap_or_default();
        anchors.extend(working_anchors(&self.chain.blocks));
        self.chain.anchors = anchors;
        // …and the relay ledger, seeded from the blob for the same reason
        // (the declaring blocks are gone after a cut — R3b/v6)
        let mut ledger: std::collections::HashMap<molt_core::MemberId, Vec<String>> = self
            .chain.checkpoint_blob
            .as_ref()
            .map(|b| b.member_relays.iter().cloned().collect())
            .unwrap_or_default();
        ledger.extend(declared_relays(&self.chain.blocks));
        self.chain.member_relays = ledger;
        self.note_relay_splits();
        // R6: an adopted chain may carry pool edits this node has not lived
        // through (catch-up, restore) — adopt the governed pool it lands on
        self.adopt_pool_change();
        self.adopt_head_roster();
    }

    /// Surface a chain workspace that opened without its local signing key: it
    /// can still verify and follow the chain, but cannot itself co-sign
    /// governance approvals (a reopen that lost `transport.state`'s
    /// `identity_sk`, or a pre-chain workspace). Cheap invariant check, logged.
    pub(crate) fn note_governance_readiness(&self) {
        if self.chain.head.is_some() && self.identity_sk.is_none() {
            tracing::warn!(
                republic = %self.republic_id(),
                "chain workspace has no local signing key - it can follow governance but not co-sign it"
            );
        }
    }

    /// The co-equal Chain-History read (`Command::ReadChain`): every
    /// committed block of the open republic as a display view, newest
    /// first — checkpoint blocks included. Read-only and synchronous.
    ///
    /// A PRUNED holder (`checkpoint_blob` is `Some`) APPENDS synthetic
    /// views for the history below the cut, rebuilt from the blob: the
    /// pre-cut applied entries (newest first, per the blob's per-surface
    /// projections) and one genesis view from the founding table. Pre-cut
    /// heights are NOT reconstructible per entry — the blob folds the
    /// dropped blocks into per-surface `(proposal_id, payload)` lists and
    /// loses each block's position (and its signature set, so `signers`
    /// stays empty), which is why every synthetic entry carries height 0:
    /// it marks "below the cut", not a real chain position. The blob also
    /// loses the cross-surface interleaving, so the synthetic ordering is
    /// per-surface block order, best-effort.
    pub(crate) fn cmd_read_chain(&self) -> Result<molt_core::Reply, molt_core::MoltError> {
        let mut blocks: Vec<molt_core::ChainBlockView> =
            self.chain.blocks.iter().rev().map(chain_block_view).collect();
        if let Some(blob) = &self.chain.checkpoint_blob {
            let mut pre: Vec<molt_core::ChainBlockView> = Vec::new();
            for (surface, entries) in &blob.applied {
                for (id, payload) in entries {
                    pre.push(molt_core::ChainBlockView {
                        height: 0,
                        kind: "applied".to_string(),
                        surface: surface.as_str().to_string(),
                        payload: payload.clone(),
                        proposal_id: *id,
                        signers: Vec::new(),
                    });
                }
            }
            pre.reverse(); // blob order is oldest-first; the read is newest-first
            blocks.extend(pre);
            // the founding constitution, rebuilt from the blob's (rid-pinned)
            // founding table — the genesis is n-of-n by chain invariant, so
            // the founding members ARE its signers even though the block
            // (and its attestation bytes) were dropped with the history
            blocks.push(molt_core::ChainBlockView {
                height: 0,
                kind: "genesis".to_string(),
                surface: String::new(),
                payload: serde_json::Value::String(blob.founding_name.clone()),
                proposal_id: 0,
                signers: blob
                    .founding_identities
                    .iter()
                    .map(|i| i.member.clone())
                    .collect(),
            });
        }
        Ok(molt_core::Reply::Chain { blocks })
    }
}
