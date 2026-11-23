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

/// One voter is told its vote was dropped at most this often.
const VOTE_REFUSAL_DEBOUNCE_SECS: u64 = 30;

/// L3: open cards one proposer may hold at once — a flooding member can
/// only crowd itself (the shed card is re-earned by the WP2 re-serve).
/// A pre-v0.1.1 receiver charges a re-server for every card it relays:
/// a sender's cards beyond that miscount are dropped there, unseen by
/// the sender, until a later re-serve re-earns them.
pub(super) const OPEN_CARDS_PER_PROPOSER_MAX: usize = 64;

/// A2.2 seal pacing (mcp_agent_friction_fixes.md): over a real transport a
/// local seal waits this long after the head last moved - one propagation
/// round, so a contender for the fresh block can still win the tip
/// tie-break before anything is built on it. The tie-break only settles
/// the TIP; a burst of seals buried a contended slot and forked a live
/// republic three ways (F00). Loopback has no round to wait for.
pub(super) const SEAL_PACE_SECS: u64 = 5;

/// S5: how long a sealer gets before the next seat in line takes over.
/// A late takeover only delays, an early one forks; Tor alone costs a few
/// seconds per hop.
pub(super) const SEALER_GRACE_SECS: u64 = 60;

/// B6: head+2 approvals parked at most, one per (proposal, signer): 64 open
/// cards from each of four seats. Beyond it they are refused as before.
const PARKED_APPROVALS_MAX: usize = 256;
/// C4: parked entries one relaying sender may hold.
const PARKED_PER_SENDER_MAX: usize = 64;

/// B5: the blocker a relay change meets on a forked chain.
pub(crate) const RELAYS_WAIT: &str = "relays wait: chain diverged";
/// C5: what an approve answers while its relay change waits.
pub(crate) const APPROVAL_HELD: &str = "held - chain diverged";
/// B6: the blocker a cut meets on a forked chain.
pub(crate) const CUT_WAIT: &str = "cut waits: chain diverged";

/// A pool edit (`set_relays`).
fn is_relay_change(change: &ChainChange) -> bool {
    matches!(change, ChainChange::Applied { surface: Surface::Organization, payload, .. }
        if payload.get("op").and_then(serde_json::Value::as_str) == Some("set_relays"))
}

/// What the `rank`-th seat in line does `waited` seconds after a proposal
/// became sealable.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SealerWait {
    Seal,
    Wait,
    /// Its turn has come: first ask the peers for a block it missed.
    Ask,
}

/// S5: how long a fallback waits for answers after asking (one serve
/// debounce, so its request is not swallowed by a recent one).
pub(super) const FALLBACK_ASK_SECS: u64 = 30;

pub(super) fn sealer_wait(rank: u64, waited: u64, grace: u64) -> SealerWait {
    let turn = rank.saturating_mul(grace);
    if rank == 0 || waited >= turn.saturating_add(FALLBACK_ASK_SECS.min(grace)) {
        SealerWait::Seal
    } else if waited >= turn {
        SealerWait::Ask
    } else {
        SealerWait::Wait
    }
}

/// B6 (D3): an approval for head+2, held until the head moves.
#[derive(Debug, Clone)]
pub(crate) struct ParkedApproval {
    pub(crate) height: u64,
    pub(crate) sig: String,
    /// The wire sender that relayed it (the per-sender cap).
    pub(crate) via: molt_core::MemberId,
    /// Checked at park time against the registered change.
    pub(crate) verified: bool,
}

impl ParkedApproval {
    /// Who may replace whom: verified over the signer's own frame over a
    /// relayed copy.
    fn rank(&self, by: &str) -> u8 {
        if self.verified {
            2
        } else {
            u8::from(self.via == by)
        }
    }
}

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
    /// Members whose CURRENT signature verified against the live target's
    /// approval bytes (L2): the DISPLAY reads only these — a raw collected
    /// sig could paint a forged stance onto a named seat. `try_commit`
    /// keeps its own authoritative filter; a sig unverifiable YET (its
    /// card has not landed) stays collected and is re-checked on arrival.
    pub verified: std::collections::BTreeSet<String>,
}

/// Whether `p` is the approval card of this membership change: the reserved
/// op + member (never wearable via cmd_propose).
fn is_membership_card(p: &molt_core::ProposalRecord, op: MembershipOp, member: &str) -> bool {
    let want = match op {
        MembershipOp::Restored => "restore_member",
        MembershipOp::Joined => "add_member",
    };
    p.surface == Surface::Organization
        && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(want)
        && p.payload.get("member").and_then(serde_json::Value::as_str) == Some(member)
}

impl State {
    /// A workspace whose governance runs through the chain (real m-of-n
    /// signatures) rather than the single-operator path.
    pub(crate) fn is_chain_governed(&self) -> bool {
        self.chain.head.is_some()
    }

    /// The committed change a pending proposal would enact. A registered change
    /// (any kind — e.g. a `Membership` re-admission) wins; otherwise it is a
    /// gated `Applied` reconstructed from the surface proposal.
    pub(super) fn proposal_change(&self, id: u64) -> Option<ChainChange> {
        if let Some(change) = self.chain.proposal_changes.get(&id) {
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
        if let Some(existing) = self.chain.proposal_changes.get(&id) {
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
                is_membership_card(p, *op, member)
            }
        }
    }

    /// Distinct collected approvals for a proposal (for the UI progress).
    pub(crate) fn chain_approval_count(&self, id: u64) -> usize {
        // L2: the DISPLAYED count is the verified one — raw collected sigs
        // could inflate progress with junk a peer gossiped
        let signed = self.chain.pending_sigs.get(&id).map(|p| p.verified.len()).unwrap_or(0);
        // B7: at a locked height intents are the voices (§9)
        let next = self.chain.head.as_ref().map_or(1, |h| h.height + 1);
        if self.lock_mode(next) == LockMode::Lock {
            signed.max(self.lock_voices(id))
        } else {
            signed
        }
    }

    /// The replay guard every AUTOMATIC co-sign runs (a consented restore,
    /// a matching checkpoint cut): does this node already hold its OWN
    /// signature for `id` at the current target height? A re-received frame
    /// must not amplify into fresh `Approved` gossip. Headless (no chain
    /// head) there is no target, hence nothing standing.
    pub(crate) fn own_signature_stands(&self, id: u64) -> bool {
        let Some(head) = self.chain.head.as_ref() else {
            return false;
        };
        let me = self.member();
        let target = head.height + 1;
        // B7: at a locked height the announced intent is the standing voice
        if self.lock_mode(target) == LockMode::Lock && self.chain.lock.intent_sent.contains(&id) {
            return true;
        }
        self.chain.pending_sigs
            .get(&id)
            .is_some_and(|p| p.height == target && p.sigs.iter().any(|a| a.member == me))
    }

    /// Sign this node's approval of a proposal at the current head+1 and
    /// record + gossip it (the outbox fans the self-authored `Approved`
    /// envelope out over the mesh). Then try to seal. The proposer's own
    /// co-signature and every explicit approve funnel through here.
    /// Why this seat could not sign an approval of `id` right now - the
    /// silent early returns of [`Self::chain_sign_and_gossip_approval`],
    /// named, so `approve` can refuse instead of answering like a counted
    /// vote (field report v0.0.11, point 2). `None` = it can sign.
    pub(crate) fn sign_blocker(&self, id: u64) -> Option<String> {
        if self.identity_sk.is_none() {
            return Some("no signing key on this seat".to_string());
        }
        if self.chain.head.is_none() {
            return Some("no chain yet".to_string());
        }
        // G: a chain that runs the lock is beyond this build: no signature
        if self.lock_chain_unsupported() {
            return Some(crate::chain::LOCK_CHAIN_UNSUPPORTED.to_string());
        }
        let Some(change) = self.proposal_change(id) else {
            return Some("change unknown here".to_string());
        };
        if CutKind::of(&change).is_some() && (self.vault_base_pending() || self.wiki_base_pending()) {
            return Some("base still loading".to_string());
        }
        if let Some(why) = self.lock_enable_blocker(&change) {
            return Some(why);
        }
        // item 3: a card registered before the mode turned on may be too
        // large for its lock frames - it stays open and withdrawable
        if let ChainChange::Applied { surface, payload, .. } = &change {
            if self.lock_enabled() {
                let framed = crate::proposals::lock_frame_plaintext_len(*surface, payload, &self.roster());
                let ceiling = crate::proposals::transport_plaintext_ceiling();
                if framed > ceiling {
                    return Some(format!("the proposal is {} KiB over what this republic's relays carry", (framed - ceiling).div_ceil(1024)));
                }
            }
        }
        if self.chain_unsettled() {
            if CutKind::of(&change).is_some() {
                return Some(CUT_WAIT.to_string());
            }
            // N6: a pool sealed on one branch only can strand the other
            if is_relay_change(&change) {
                return Some(RELAYS_WAIT.to_string());
            }
        }
        None
    }

    /// A peer is on another branch or blocks of one are held. No state
    /// tracks an attestation variant this node won, so it does not count.
    /// A diverged seat gone silent does not count either: only it could
    /// clear its entry.
    pub(crate) fn chain_unsettled(&self) -> bool {
        self.chain_unsettled_besides(None)
    }

    /// [`Self::chain_unsettled`], ignoring `seat`'s own entry as well.
    pub(crate) fn chain_unsettled_besides(&self, seat: Option<&str>) -> bool {
        if !self.chain.fork_candidates.is_empty() {
            return true;
        }
        if self.chain.diverged.is_empty() {
            return false;
        }
        let silent: Vec<String> = self.chain_lag().silent.into_iter().map(|s| s.member).collect();
        self.chain.diverged.keys().any(|p| Some(p.as_str()) != seat && !silent.contains(p))
    }

    /// B5: sign the held relay approvals once the chain has healed; the
    /// head-move twin is [`Self::rebase_pending_approvals`].
    pub(crate) fn sign_held_approvals(&mut self) {
        if !self.is_chain_governed() || self.chain_unsettled() {
            return;
        }
        let held: Vec<u64> = self
            .standing_unsigned()
            .into_iter()
            .filter(|id| self.proposal_change(*id).is_some_and(|c| is_relay_change(&c)) && self.sign_blocker(*id).is_none())
            .collect();
        for id in held {
            self.chain_sign_and_gossip_approval(id);
        }
    }

    /// WP2: open cards this seat approved whose own signature does not
    /// stand at head+1. The stance is `own_approvals`, never `pending_sigs`:
    /// a card displaced by a tip swap lost its signatures with its block.
    fn standing_unsigned(&self) -> Vec<u64> {
        self.chain
            .own_approvals
            .iter()
            .copied()
            .filter(|id| {
                self.proposals.get(id).is_some_and(|p| p.state == ProposalState::Proposed)
                    && !self.own_signature_stands(*id)
            })
            .collect()
    }

    /// WP2: sign every standing approval at the current head+1, once per
    /// (card, height) - a signed one stands. Held relay decisions wait for
    /// [`Self::sign_held_approvals`]; inside a block batch nothing signs.
    pub(super) fn resign_standing_approvals(&mut self) {
        if self.chain.head.is_none() || self.chain.resign_hold > 0 {
            return;
        }
        let due: Vec<u64> = self
            .standing_unsigned()
            .into_iter()
            .filter(|id| self.sign_blocker(*id).as_deref() != Some(RELAYS_WAIT))
            .collect();
        for id in due {
            self.chain_sign_and_gossip_approval(id);
        }
    }

    /// WP2: a peer may have dropped this seat's standing signatures while
    /// the two chains differed: re-send each one that stands, once per
    /// (card, head+1), and sign the rest.
    pub(super) fn resend_standing_approvals(&mut self) {
        let Some(target) = self.chain.head.as_ref().map(|h| h.height + 1) else {
            return;
        };
        if self.chain.resign_hold > 0 {
            return;
        }
        let open: BTreeSet<u64> = self
            .chain
            .own_approvals
            .iter()
            .copied()
            .filter(|id| self.proposals.get(id).is_some_and(|p| p.state == ProposalState::Proposed))
            .collect();
        self.chain.resent_at.retain(|id, _| open.contains(id));
        let standing: Vec<u64> = open
            .into_iter()
            .filter(|id| self.own_signature_stands(*id) && self.chain.resent_at.get(id) != Some(&target))
            .collect();
        for id in standing {
            self.chain.resent_at.insert(id, target);
            self.reserve_signatures(id);
        }
        self.resign_standing_approvals();
    }

    /// WP2: apply a batch of blocks; the standing approvals re-sign once,
    /// at the batch's final head.
    pub(super) fn in_block_batch<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.chain.resign_hold += 1;
        let out = f(self);
        self.chain.resign_hold -= 1;
        self.resign_standing_approvals();
        out
    }

    /// C5: [`APPROVAL_HELD`] while this seat's decision on `id` waits for
    /// the heal.
    pub(crate) fn approval_held(&self, id: u64) -> Option<String> {
        let waits = self.chain.own_approvals.contains(&id)
            && !self.own_signature_stands(id)
            && self.sign_blocker(id).as_deref() == Some(RELAYS_WAIT);
        waits.then(|| APPROVAL_HELD.to_string())
    }

    pub(crate) fn chain_sign_and_gossip_approval(&mut self, id: u64) {
        if let Some(why) = self.sign_blocker(id) {
            // B5: the decision stands, the signature waits for the heal; no
            // Approved envelope carries it, so the snapshot does
            if why == RELAYS_WAIT && self.chain.own_approvals.insert(id) {
                if let Some(active) = &self.active {
                    active.handle.snapshot(self.snapshot_now());
                }
            }
            // the re-base path re-signs a STANDING decision: never silent
            tracing::warn!(id, reason = %why, "approval not signed");
            return;
        }
        let (Some(_), Some(head)) = (self.identity_sk.as_ref(), self.chain.head.as_ref()) else {
            return;
        };
        let height = head.height + 1;
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        // plan 1.3.15: no cut over a base (vault or wiki) not held here
        if CutKind::of(&change).is_some() && (self.vault_base_pending() || self.wiki_base_pending())
        {
            tracing::warn!(id, reason = "base_pending", "cut not signed");
            return;
        }
        // the vault gate holds on every path that signs, not only `approve`
        if let ChainChange::Applied { surface: Surface::Vault, payload, .. } = &change {
            if let Err(e) = self.vault_approve_check(payload) {
                tracing::warn!(id, error = %e, "vault: not signing");
                return;
            }
        }
        if let ChainChange::Applied { surface: Surface::Wallet, payload, .. } = &change {
            if let Err(e) = self.wallet_approve_check(payload) {
                tracing::info!(id, error = %e, "wallet: not signing");
                return;
            }
        }
        // B7: at a locked height the approval is an intent; a free signature
        // passes the lock's one gate, durable before it leaves
        match self.lock_mode(height) {
            LockMode::Lock => {
                self.lock_intent(id);
                return;
            }
            LockMode::Free => {
                if !self.lock_sign_freely(height) {
                    tracing::warn!(id, height, "approval not signed reason=lock");
                    return;
                }
            }
        }
        let Some(sk) = self.identity_sk.as_ref() else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        let sig = molt_storage::identity_sign(sk, &bytes);
        let me = self.member();
        // the decision register: the ONLY writer (see `rebase_pending_approvals`)
        self.chain.own_approvals.insert(id);
        // the own signature is genuine by construction (L2)
        self.collect_sig(id, height, &me, &sig, true);
        if let Some(p) = self.chain.pending_sigs.get_mut(&id) {
            p.verified.insert(me.clone());
        }
        self.note_signer(&me, height);
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
    /// block's m signatures stay the only chain truth. Called at EVERY
    /// terminal transition (G17, 2026-09-06): a decline, a withdraw and a
    /// superseded patch lose their collection to the next re-base, and a
    /// post-mortem must still read who voted how.
    pub(crate) fn stash_voted(&mut self, id: u64) {
        let members: Vec<molt_core::MemberId> = self
            .chain.pending_sigs
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

    /// D4: the applied card's DISPLAY stamp - the block's `Committed`
    /// envelope ts, taken like `chain.block_ts` itself (the earliest seen
    /// wins, so two holders converge on one moment). 0 = the log never
    /// carried that envelope; the stamp stays unknown.
    pub(crate) fn stamp_applied_at(&mut self, proposal_id: u64, at: u64) {
        if at == 0 {
            return;
        }
        if let Some(p) = self.proposals.get_mut(&proposal_id) {
            let cur = p.applied_at;
            p.applied_at = if cur == 0 { at } else { cur.min(at) };
        }
    }

    /// Drop the ephemeral vote bookkeeping of a DECIDED proposal id: its
    /// collected signatures (the voter set is stashed for the display
    /// first) and its registered chain change. Idempotent.
    pub(super) fn forget_vote(&mut self, id: u64) {
        self.stash_voted(id);
        self.chain.pending_sigs.remove(&id);
        self.chain.proposal_changes.remove(&id);
    }

    /// [`State::forget_vote`] for a sealed change that carries no proposal
    /// id (a Membership or Checkpoint block): every registered entry with
    /// exactly this content is decided — the sealer (which also cleans by
    /// id), every passive applier and a catch-up all settle identically.
    pub(super) fn forget_votes_for(&mut self, sealed: &ChainChange) {
        let ids: Vec<u64> = self
            .chain.proposal_changes
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
    pub(crate) fn collect_sig(
        &mut self,
        id: u64,
        height: u64,
        member: &str,
        sig: &str,
        verified: bool,
    ) {
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
        let rostered = match self.chain.head.as_ref() {
            Some(h) => h.identities.iter().any(|i| i.member == member),
            // the log replay runs BEFORE the chain is adopted (the reopen
            // order), so the genesis roster is the only membership fact
            // available yet; `verified` stays false until the adoption
            None => self.roster().iter().any(|m| m == member),
        };
        if !rostered {
            return;
        }
        if !verified && member == self.member() {
            return;
        }
        let entry = self.chain.pending_sigs.entry(id).or_default();
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
        let Some(head) = self.chain.head.as_ref() else {
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
        let Some(pending) = self.chain.pending_sigs.get(&id) else {
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
                if let Some(p) = self.chain.pending_sigs.get_mut(&id) {
                    p.verified.insert(member.clone());
                }
                self.note_signer(&member, height);
            }
        }
    }

    /// Try to seal a block for a proposal that has gathered the threshold of
    /// valid, distinct signatures at the current head+1. Deterministic: the m
    /// lowest-named valid signers are chosen, so two nodes that both reach the
    /// threshold seal the byte-identical block (it self-dedups on receipt).
    pub(crate) fn try_commit(&mut self, id: u64) {
        let Some(head) = self.chain.head.clone() else {
            return;
        };
        // a chain that runs the lock is extended by the lock only
        if self.lock_chain_unsupported() {
            return;
        }
        // already committed?
        if matches!(self.proposals.get(&id), Some(p) if p.state != ProposalState::Proposed) {
            return;
        }
        let target = head.height + 1;
        // item 7: a locked height is sealed by the lock alone
        if self.lock_mode(target) == super::LockMode::Lock {
            return;
        }
        let Some(change) = self.proposal_change(id) else {
            return;
        };
        let bytes = approval_bytes(&self.republic_id(), target, &change);
        let Some(pending) = self.chain.pending_sigs.get(&id) else {
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
                // §7e: re-judged at THIS height - a re-based v2 consent may
                // have left its window or been spent meanwhile, and the
                // verifier rejects such a block whole
                if let Err(why) = self.restore_consent_at(&change, target) {
                    tracing::warn!(%id, reason = why, "consented restore no longer seals");
                    return;
                }
                valid.retain(|a| a.member != *member);
                1
            }
            _ => 0,
        };
        // A2 (user, 2026-09-06): a CUT seals n-of-n, like the genesis. It
        // drops history every seat has to be able to reproduce, and it has
        // no hurry - a silent seat leaves the cut pending, and the next
        // committed block re-proposes it at the new head.
        let need = if CutKind::of(&change).is_some() {
            head.identities.len()
        } else {
            usize::from(head.rule_m)
        };
        if valid.len() >= need {
            // enough survivor signatures on their own — the consent still
            // rides the change, but never displaces a survivor's voice
            valid.truncate(need);
        } else if valid.len() + consented < need {
            return;
        }
        if self.restore_seal_held(id, &change, true) {
            return;
        }
        // S5/B6: one sealer per height; the others wait their turn
        if !self.sealer_turn(id, target, &change) {
            self.chain.seal_held.insert(id);
            return;
        }
        // A2.2: one round after the head moved, a local seal may build on it
        let pace = self.seal_pace_secs();
        if pace > 0 && self.presence_now().saturating_sub(self.chain.head_moved_at) < pace {
            self.chain.seal_held.insert(id);
            return;
        }
        if self.sealed_a_smaller_id(id, target) {
            return;
        }
        self.chain.seal_held.remove(&id);
        let block = ChainBlock {
            height: target,
            prev: head.hash.clone(),
            change,
            sigs: valid,
        };
        self.adopt_committed_block(block, id);
    }

    /// B6 (`docs_archive/chain/rejoin_divergence_build_plan.md`): the sealing line
    /// at `height` - the roster seats heard within
    /// [`super::sync::SILENT_AFTER_SECS`] (this seat always), by name. Per
    /// height, never per proposal: two proposals at one height must not
    /// have two first sealers. A seat on another branch cannot seal on
    /// ours and leaves the line (every seat learns that from the same
    /// block; its catch-up request after a re-base clears it). A seat
    /// merely behind stays: its height here is stale while it catches up.
    pub(crate) fn sealing_line(&self, _height: u64) -> Vec<String> {
        let me = self.member();
        let unseen = self.unseen_seats();
        let mut line: Vec<String> = self
            .roster()
            .into_iter()
            .filter(|m| *m == me || (!unseen.contains(m) && !self.chain.diverged.contains_key(m)))
            .collect();
        line.sort();
        line.dedup();
        line
    }

    /// B6: who seals at `height` before any fallback. The hook a lock
    /// stage (B7) calls or overrides.
    pub(crate) fn turn_holder(&self, height: u64) -> Option<String> {
        self.sealing_line(height).into_iter().next()
    }

    /// S5/B6: whether this node may seal at `target` now. The first seat of
    /// the line seals at once; the k-th only once `k` grace periods passed
    /// since a proposal first became sealable here at this height, and
    /// after asking its peers for a block it may have missed. Off without
    /// a real transport.
    fn sealer_turn(&mut self, id: u64, target: u64, change: &ChainChange) -> bool {
        if self.seal_pace_secs() == 0 {
            return true;
        }
        let now = self.presence_now();
        self.prune_ready_at(target);
        let fresh = !self.chain.ready_at.contains_key(&id);
        self.chain.ready_at.entry(id).or_insert((target, now));
        // the wait is the HEIGHT's: a proposal sealable later inherits it
        let since = self.chain.ready_at.values().map(|(_, t)| *t).min().unwrap_or(now);
        let me = self.member();
        let mut line = self.sealing_line(target);
        // the seat being restored has no node in the group to seal with
        if let ChainChange::Membership { op: MembershipOp::Restored, member, .. } = change {
            line.retain(|m| m != member);
        }
        let rank = line
            .iter()
            .position(|m| *m == me)
            .unwrap_or(line.len());
        let rank = u64::try_from(rank).unwrap_or(u64::MAX);
        let waited = now.saturating_sub(since);
        let turn = sealer_wait(rank, waited, self.lock_grace());
        // waiting on a seat ahead: hand it the signatures held here, once,
        // or it may lack one it needs and the wait runs to the fallback
        if fresh && turn != SealerWait::Seal {
            self.reserve_signatures(id);
        }
        match turn {
            SealerWait::Seal => true,
            SealerWait::Wait => false,
            SealerWait::Ask => {
                if self.chain.fallback_asked != Some(target) {
                    self.chain.fallback_asked = Some(target);
                    tracing::info!(target, rank, waited, "sealer=fallback");
                    self.ask_for_height(target);
                }
                false
            }
        }
    }

    /// C2: keep a "sealable since" only while its proposal is still
    /// sealable at `target`. Re-judged at every use and every delivery
    /// tick, so no terminal path (withdraw, decline, supersede, a lost
    /// signature) can leave a stale start behind.
    fn prune_ready_at(&mut self, target: u64) {
        let ids: Vec<u64> = self.chain.ready_at.keys().copied().collect();
        for id in ids {
            let keep = self.chain.ready_at.get(&id).is_some_and(|(h, _)| *h == target)
                && self.still_sealable(id, target);
            if !keep {
                self.chain.ready_at.remove(&id);
            }
        }
    }

    /// Open, its change registered, and holding the needed verified
    /// signatures at `target` (the counting rule of [`Self::try_commit`]).
    fn still_sealable(&self, id: u64, target: u64) -> bool {
        let Some(head) = self.chain.head.as_ref() else {
            return false;
        };
        let card = self.proposals.get(&id);
        if card.is_some_and(|p| p.state != ProposalState::Proposed) {
            return false;
        }
        let (Some(change), Some(pending)) = (self.proposal_change(id), self.chain.pending_sigs.get(&id)) else {
            return false;
        };
        if pending.height != target {
            return false;
        }
        let decliners = card.map(|p| p.decliners.as_slice()).unwrap_or_default();
        let restored = match &change {
            ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } => Some(member),
            _ => None,
        };
        let voices = pending
            .verified
            .iter()
            .filter(|m| !decliners.contains(m) && restored != Some(m))
            .count();
        let need = if CutKind::of(&change).is_some() {
            head.identities.len()
        } else {
            usize::from(head.rule_m)
        };
        voices + usize::from(restored.is_some()) >= need
    }

    /// B6: the turn seals the SMALLEST sealable id at `target`, so a seat
    /// that takes over seals what the turn-holder would. Tries every
    /// smaller open id first; `true` when one of them sealed.
    fn sealed_a_smaller_id(&mut self, id: u64, target: u64) -> bool {
        if self.seal_pace_secs() == 0 || self.chain.sealing_smaller {
            return false;
        }
        let mut smaller: Vec<u64> = self
            .chain
            .pending_sigs
            .iter()
            .filter(|(other, p)| **other < id && p.height == target)
            .map(|(other, _)| *other)
            .collect();
        if smaller.is_empty() {
            return false;
        }
        smaller.sort_unstable();
        let moved = |st: &Self| st.chain.head.as_ref().is_some_and(|h| h.height >= target);
        self.chain.sealing_smaller = true;
        for other in smaller {
            self.try_commit(other);
            if moved(self) {
                break;
            }
        }
        self.chain.sealing_smaller = false;
        moved(self)
    }

    /// Seats with no recent sign of life here: never seen, or silent for
    /// [`super::sync::SILENT_AFTER_SECS`]. The sealing line takes only seats
    /// it has seen, so an unknown seat (a fresh founding, a seat being
    /// restored) never holds the others back.
    pub(crate) fn unseen_seats(&self) -> Vec<String> {
        let now = self.presence_now();
        self.roster()
            .into_iter()
            .filter(|m| {
                let seen = self.member_last_seen(m);
                seen == molt_core::MemberInfo::NEVER || now.saturating_sub(seen) >= super::sync::SILENT_AFTER_SECS
            })
            .collect()
    }

    /// The pacing round: [`SEAL_PACE_SECS`] over a Nostr transport, none
    /// without one (the loopback twin delivers inside the same call).
    fn seal_pace_secs(&self) -> u64 {
        if self.nostr.is_some() {
            SEAL_PACE_SECS
        } else {
            0
        }
    }

    /// C1: how long this node's held seal for `id` still waits, `None`
    /// when nothing is held for it. The pacing round is measured from the
    /// last head move, so the wait shrinks with every read.
    pub(crate) fn seal_hold_secs(&self, id: u64) -> Option<u64> {
        if !self.chain.seal_held.contains(&id) {
            return None;
        }
        let waited = self.presence_now().saturating_sub(self.chain.head_moved_at);
        Some(self.seal_pace_secs().saturating_sub(waited))
    }

    /// The delivery tick's half of the pacing: retry every held seal, which
    /// lands once the round has passed and is dropped once its proposal is
    /// no longer open (a peer sealed it, or it was withdrawn).
    pub(crate) fn drain_held_seals(&mut self) {
        if let Some(target) = self.chain.head.as_ref().map(|h| h.height + 1) {
            self.prune_ready_at(target);
        }
        let open: Vec<u64> = self
            .chain
            .seal_held
            .iter()
            .copied()
            // a checkpoint or membership change has no surface card; its
            // registered change is the only thing to check
            .filter(|id| {
                self.proposals
                    .get(id)
                    .map_or(true, |p| p.state == ProposalState::Proposed)
            })
            .collect();
        self.chain.seal_held.clear();
        for id in open {
            self.try_commit(id);
        }
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
        // the ONE write of this seal — behind the bookkeeping, because a
        // Checkpoint block's cut (blob set, history below it dropped) is
        // part of what must reach the disk before the block is announced.
        // The re-base waits for the `Committed` below (B6 D2): approvals
        // for the next height that outran their block were dropped.
        self.settle_block(&block);
        let durable = self.persist_chain_now();
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
            self.rebase_pending_approvals();
            return;
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Committed(block.clone()));
        self.record(env);
        self.rebase_pending_approvals();
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
    /// batch to a crash costs a re-fetch: any survivor re-serves the blocks
    /// on the next catch-up - and, under the lock, the slots above the
    /// chain on disk stay in the lock file (`durable_head`, B-F1).
    pub(super) fn persist_chain_now(&mut self) -> bool {
        if let Some(base) = self.chain.pool_adopt_base.take() {
            self.adopt_ratified_relays(&base);
        }
        #[cfg(test)]
        CHAIN_PERSISTS.with(|c| c.set(c.get() + 1));
        #[cfg(test)]
        if self.chain.lock.fail_chain_writes {
            return false;
        }
        let Some(active) = &self.active else {
            self.chain.lock.durable_head = self.chain.head.as_ref().map_or(0, |h| h.height);
            return true;
        };
        let durable = active
            .handle
            .persist_chain_blocking(self.chain.checkpoint_blob.clone(), self.chain.blocks.clone());
        if durable {
            // B-F1: the lock may now forget what this chain covers
            self.chain.lock.durable_head = self.chain.head.as_ref().map_or(0, |h| h.height);
        }
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
        // verify BEFORE appending — the block only ever touches `self.chain.blocks`
        // once it has passed, so there is nothing to roll back
        match self.extend_own(&block) {
            Ok(head) => {
                self.chain.head = Some(head);
                self.chain.head_moved_at = self.presence_now();
                // an append only ADDS to the projection — no whole-chain refold
                self.project_one(&block);
                self.chain.blocks.push(block);
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
        self.settle_block(block);
        self.rebase_pending_approvals();
    }

    /// [`Self::after_block_applied`] without the re-base: the block's own
    /// bookkeeping (cards, re-key, the cut).
    fn settle_block(&mut self, block: &ChainBlock) {
        match &block.change {
            ChainChange::Applied {
                proposal_id,
                surface,
                payload,
            } => {
                // a block for a proposal this node never heard of (lost
                // gossip, late join) still yields a full accepted card
                self.ensure_applied_record(*proposal_id, *surface, payload.clone());
                let me = self.member();
                let mut sealed_withdrawn = false;
                if let Some(p) = self.proposals.get_mut(proposal_id) {
                    // A1 guard: the card is that decision only if it carries
                    // that change - an own card under a colliding id must
                    // not read as a success it never had
                    if p.payload == *payload || p.by.is_empty() {
                        p.settle_applied();
                        // R7: the lock chose it before the withdrawal; the
                        // record keeps `withdrawn` (replay sets no flag either)
                        sealed_withdrawn = p.withdrawn && p.by == me;
                    } else {
                        tracing::warn!(id = proposal_id, "a block names an id whose local card is another change");
                    }
                }
                if sealed_withdrawn {
                    tracing::warn!(id = proposal_id, "sealed_after_withdrawal");
                    self.session.notice = format!("sealed-after-withdrawal:{proposal_id}");
                }
                // the Memory base moved: the supersede walk retires the
                // OTHER pending patches it left behind (deterministic - the
                // append, catch-up and rebuild paths all reach here). It
                // runs after the settle above, because the patch just
                // folded never applies to the tree that now contains it
                if *surface == Surface::Memory {
                    let moved = Self::wiki_payload_paths(payload);
                    self.note_wiki_change(moved.as_ref());
                    // the fold cache extends by this block HERE, not on the
                    // next read: the revision stamp on the card (§4.11) is
                    // a function of the chain, and the walk below returns
                    // early when nothing is open
                    self.refresh_wiki_cache();
                    self.supersede_stale_wiki(moved.as_ref());
                }
                if *surface == Surface::Quests {
                    self.refresh_kanban_fold();
                    // every node wakes its own seat; the list says what is due
                    self.wake_trigger("kanban", "");
                }
                self.stash_voted(*proposal_id);
                self.chain.pending_sigs.remove(proposal_id);
                // a peer's block was stamped by the ingest before this runs;
                // our OWN seal records its envelope afterwards and is
                // backfilled by the `Committed` arm
                let at = self.chain.block_ts.get(&block.height).copied().unwrap_or(0);
                self.stamp_applied_at(*proposal_id, at);
                self.emit(Event::Applied {
                    id: ProposalId(*proposal_id),
                    surface: *surface,
                });
                if *surface == Surface::Organization {
                    self.after_org_applied();
                } else if *surface == Surface::Files {
                    self.after_files_applied(payload);
                } else if *surface == Surface::Vault {
                    self.after_vault_applied(payload);
                } else if *surface == Surface::Wallet {
                    self.after_wallet_applied(*proposal_id, payload);
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
                self.recovery.mesh_extension_at.remove(member);
                // item 7: a device without Strict Order Mode is not welcomed
                // into a republic that runs it
                let unwelcomed = self.coordinates_restore(member, &block.change)
                    && self.recovery.pending.get(member).cloned().is_some_and(|p| self.restore_unwelcomed(member, &p));
                if unwelcomed {
                    self.recovery.pending.remove(member);
                } else if self.coordinates_restore(member, &block.change) {
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
            ChainChange::Checkpoint { .. }
            | ChainChange::CheckpointFolded { .. }
            | ChainChange::CheckpointVault { .. } => {
                let Some((upto, _, kind)) = CutKind::of(&block.change) else {
                    return;
                };
                self.forget_votes_for(&block.change);
                // B-F2: drop the summarized history locally, automatically —
                // the vote just confirmed this summary is correct. The blob
                // becomes the holder's trust anchor; the chain keeps the
                // checkpoint block and everything after it.
                let anchor_height = block.height;
                // NOT persisted here: every caller of `after_block_applied`
                // writes the chain once after it (the seal, the tie-break,
                // the catch-up batch), and the cut is part of that write
                match self.own_cut_state(upto, kind) {
                    // K6: the folded tree reaches the DISK before the
                    // patches that produce it are dropped. Losing that
                    // order once loses the wiki: the blocks are gone and
                    // the commitment names bytes nobody kept.
                    Ok((_, folded)) if folded.tree.as_ref().is_some_and(|t| !self.persist_wiki_base(Some(t))) => {
                        tracing::error!(
                            "the folded wiki base did not reach the disk - keeping full history"
                        );
                    }
                    // the vault base likewise (spec §9.3); the wiki base
                    // just written goes back, or it answers no commitment
                    Ok((_, folded)) if folded.vault.as_ref().is_some_and(|b| !self.persist_vault_base(Some(b))) => {
                        let wiki_back = folded.tree.is_none() || self.persist_wiki_base(self.chain.wiki_base.as_ref());
                        tracing::error!(wiki_restored = wiki_back, "vault_base=not_persisted action=keep_history");
                    }
                    Ok((blob, folded)) => {
                        if let Some(tree) = folded.tree {
                            self.chain.wiki_base = Some(tree);
                        }
                        if let Some(base) = folded.vault {
                            self.set_vault_base(Some(base));
                        }
                        self.set_checkpoint_blob(Some(blob));
                        self.chain.blocks.retain(|b| b.height >= anchor_height);
                        self.apply_chain_to_state();
                        // A4: the cut re-based every open patch; it killed none
                        self.mark_open_wiki_patches_rebased();
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
        // R10: a new device behind a seat: everyone declares again for it
        self.mirror_audience_changed();
        let ids: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed && is_membership_card(p, *op, member))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.settle_applied();
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
        let Some(at) = self.chain.head.as_ref().map(|h| h.height) else {
            return;
        };
        let target = at + 1;
        // T5: an own free signature at head+1 is marked before the re-sign
        // below can start a ballot (and a durable promise) there
        self.lock_mark_after_move(at);
        let stale: Vec<u64> = self
            .chain.pending_sigs
            .iter()
            .filter(|(_, p)| p.height < target)
            .map(|(id, _)| *id)
            .collect();
        // sweep checkpoint entries that never made it into pending_sigs
        // (a proposer without self-cosign, a bailed sign): a cut below the
        // new head can never seal (upto == height-1 is enforced) and must
        // not linger as bookkeeping a late Approved could resurrect
        let head_height = at;
        let swept: Vec<u64> = self
            .chain.proposal_changes
            .iter()
            .filter(|(_, c)| {
                CutKind::of(c).is_some_and(|(upto, _, _)| upto < head_height)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in swept {
            self.chain.proposal_changes.remove(&id);
            self.chain.pending_sigs.remove(&id);
            // closure for the proposer/operator: this cut can never seal —
            // re-propose at the new head (the stale loop below can no
            // longer see these entries, so the emit lives HERE)
            self.emit(Event::CheckpointStale { id: ProposalId(id) });
        }
        for id in stale {
            // WP4b: a checkpoint's change is CUT-bound (upto == height - 1,
            // enforced by the verifier) — after the head moved, re-signing
            // the old cut could only seal an invalid block. Drop it; the
            // proposer re-proposes at the new head (doc §B.2).
            self.chain.pending_sigs.remove(&id);
        }
        // the LOCAL decision register, never the wire-collected set: a peer
        // can put junk under this node's name into `pending_sigs`, and
        // re-signing from there was a threshold bypass (2026-08-25)
        self.resign_standing_approvals();
        self.sweep_consented_restores();
        self.judge_parked_approvals();
        self.lock_on_head_moved();
    }

    /// B6 (D3): park an approval for head+2 - its signer holds one block
    /// more than this node. One slot per (proposal, signer); `false` when
    /// it cannot be parked (not a seat, a signature that fails, the
    /// sender's share or the park full). C4: verified against the
    /// registered change where there is one; an entry never gives way to a
    /// lower-ranked one ([`ParkedApproval::rank`]), and a full park gives
    /// up an unverified entry only for a verified one.
    fn park_approval(&mut self, via: &str, id: u64, by: &str, height: u64, sig: &str) -> bool {
        let Some(head) = self.chain.head.as_ref() else {
            return false;
        };
        let Some(identity) = head.identities.iter().find(|i| i.member == by) else {
            return false;
        };
        let verified = match self.proposal_change(id) {
            Some(change) => {
                let bytes = approval_bytes(&self.republic_id(), height, &change);
                if !molt_storage::identity_verify(&identity.identity_pk, &bytes, sig) {
                    return false;
                }
                true
            }
            None => false,
        };
        let entry = ParkedApproval { height, sig: sig.to_string(), via: via.to_string(), verified };
        let key = (id, by.to_string());
        let park = &mut self.chain.parked_approvals;
        if let Some(held) = park.get(&key) {
            if held.rank(by) > entry.rank(by) || (held.rank(by) == 0 && entry.rank(by) == 0) {
                return true;
            }
        } else {
            let share = park.values().filter(|p| p.via == via).count();
            if share >= PARKED_PER_SENDER_MAX {
                return false;
            }
            if park.len() >= PARKED_APPROVALS_MAX {
                let loose = park.iter().find(|(_, p)| !p.verified).map(|(k, _)| k.clone());
                match loose {
                    Some(k) if verified => {
                        park.remove(&k);
                    }
                    _ => return false,
                }
            }
        }
        tracing::debug!(id, %by, %via, height, verified, "approval=parked");
        park.insert(key, entry);
        true
    }

    /// B6 (D3): the head moved - parked approvals for the new target count
    /// now, those still one ahead stay, the rest are stale.
    pub(super) fn judge_parked_approvals(&mut self) {
        if self.chain.parked_approvals.is_empty() {
            return;
        }
        let Some(target) = self.chain.head.as_ref().map(|h| h.height + 1) else {
            return;
        };
        let parked = std::mem::take(&mut self.chain.parked_approvals);
        let mut due = Vec::new();
        for ((id, by), p) in parked {
            if p.height == target {
                due.push((id, by, p.height, p.sig));
            } else if p.height == target + 1 {
                self.chain.parked_approvals.insert((id, by), p);
            }
        }
        for (id, by, height, sig) in due {
            self.receive_approval(id, &by, height, &sig);
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
        match &self.chain.checkpoint_blob {
            None => walk_chain(blocks),
            Some(blob) => walk_suffix_chain(blob, blocks, &self.republic_id(), self.held_bases()),
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
        let mut walk = match self.chain.walk.take() {
            Some(w) if w.describes(&self.chain.blocks, self.chain.checkpoint_blob.as_ref()) => w,
            _ => self.walk_own(&self.chain.blocks)?,
        };
        // the base as this holder holds it NOW, not as the cached walk once
        // saw it (D9)
        let stepped = walk.step(block, self.held_bases());
        let head = walk.head.clone();
        self.chain.walk = Some(walk);
        stepped.map(|()| head)
    }

    /// The canonical state at `upto` from THIS holder's own material —
    /// genesis-rooted for a full holder, blob-based for a pruned one.
    /// What the propose/verify-before-sign paths hash.
    pub(crate) fn own_checkpoint_state(
        &self,
        upto: u64,
        folded: bool,
    ) -> Result<molt_core::CheckpointState, String> {
        Ok(self.own_cut_state(upto, self.cut_kind(folded))?.0)
    }

    /// The cut this holder proposes or verifies: a vault republic's is
    /// always [`ChainChange::CheckpointVault`] (plan 1.3.6), `wiki` the
    /// wiki choice either way.
    pub(crate) fn cut_kind(&self, wiki: bool) -> CutKind {
        CutKind { wiki, vault: self.is_vault_prepared() }
    }

    /// The state hash this holder attests for a cut at `upto` - the value
    /// the propose announces and the co-sign compares against.
    ///
    /// # Errors
    /// See [`State::own_cut_state`].
    pub(crate) fn own_cut_hash(&self, upto: u64, kind: CutKind) -> Result<String, String> {
        Ok(self.own_cut_state(upto, kind)?.1.hash)
    }

    /// The cut's state, the tree a folded one commits to - the bytes this
    /// holder has to keep once the patches that produce them are dropped
    /// (K6) - and the state hash a signer attests.
    ///
    /// Every cut path (propose, co-sign, receive-verify, apply) reaches the
    /// fold through here and through [`super::wiki_base::fold_cut`], over
    /// the base this holder holds RIGHT NOW.
    ///
    /// # Errors
    /// The projection cannot be recomputed, or the fold needs a base this
    /// holder does not hold.
    pub(crate) fn own_cut_state(
        &self,
        upto: u64,
        kind: CutKind,
    ) -> Result<(molt_core::CheckpointState, FoldedCut), String> {
        let mut state = match &self.chain.checkpoint_blob {
            None => checkpoint_state(&self.chain.blocks, upto)?,
            // the anchor block in chain[0] is state-neutral for the fold
            Some(blob) => fold_state(blob.clone(), &self.chain.blocks, upto)?,
        };
        let folded = super::wiki_base::fold_cut(&mut state, kind, self.held_bases())?;
        Ok((state, folded))
    }

    /// Every base this holder keeps right now.
    pub(crate) fn held_bases(&self) -> Held<'_> {
        Held { wiki: self.chain.wiki_base.as_ref(), vault: self.vault_base_now() }
    }

    /// The wiki is folded into a commitment this node does not hold.
    pub(crate) fn wiki_base_pending(&self) -> bool {
        self.wiki_base_committed().is_some() && self.chain.wiki_base.is_none()
    }

    /// The ratified wiki tree this holder keeps for its blob's commitment
    /// (K6). `None` where there is nothing to hold - or where the tree is
    /// still being fetched, which is why a fold refuses rather than
    /// inventing an empty base.
    pub(crate) fn held_wiki_base(&self) -> Option<&WikiTree> {
        self.chain.wiki_base.as_ref()
    }

    /// L3: a peer-chosen proposal id far past the mint counter is garbage —
    /// registering it (or even bumping `next_id` for it) would poison every
    /// later local mint (a u64::MAX id would freeze proposing for good).
    /// Window shared with the decline park.
    pub(crate) fn plausible_wire_id(&self, id: u64) -> bool {
        id <= self
            .next_id
            .saturating_add(crate::proposals::PARKED_DECLINE_ID_WINDOW)
    }

    /// `by`'s open cards that `by` delivered itself, membership records
    /// included: what a receiver charges to `by`'s cap.
    pub(crate) fn open_cards_by(&self, by: &str) -> usize {
        self.proposals
            .values()
            .filter(|p| p.state == ProposalState::Proposed && p.by == by && p.relayed_by.is_empty())
            .count()
    }

    /// Open cards `via` relayed for a seat that has not delivered them.
    fn open_relayed_by(&self, via: &str) -> usize {
        self.proposals
            .values()
            .filter(|p| p.state == ProposalState::Proposed && p.relayed_by == via)
            .count()
    }

    /// A relayer honestly carries at most every other seat's full cap.
    fn relay_budget(&self) -> usize {
        OPEN_CARDS_PER_PROPOSER_MAX * self.roster().len().saturating_sub(1).max(1)
    }

    /// A card belongs to its id's minting seat, on every node however it
    /// arrived. Until that seat delivers it itself, the wire sender `via`
    /// is kept as its relayer. Without a roster `via` is the proposer.
    pub(crate) fn attribute_card(&mut self, id: u64, via: &str) {
        let minter = self.minting_seat(id).unwrap_or_else(|| via.to_string());
        let Some(p) = self.proposals.get_mut(&id) else {
            return;
        };
        if via == minter {
            p.relayed_by.clear();
        } else if p.by != minter && p.relayed_by.is_empty() {
            p.relayed_by = std::mem::take(&mut p.by);
        }
        p.by = minter;
    }

    /// Own open cards, however they came back here: the sender counts at
    /// least what any receiver charges it.
    fn own_open(&self) -> usize {
        let me = self.member();
        self.proposals
            .values()
            .filter(|p| p.state == ProposalState::Proposed && p.by == me)
            .count()
    }

    /// The sender twin of the L3 cap: a card every peer would drop is
    /// never put up. Membership cards and cuts do not pass through it.
    pub(crate) fn open_cap_gate(&self) -> Result<(), molt_core::MoltError> {
        if self.own_open() >= OPEN_CARDS_PER_PROPOSER_MAX {
            return Err(molt_core::MoltError::OpenProposalsCap(OPEN_CARDS_PER_PROPOSER_MAX));
        }
        Ok(())
    }

    /// `(own open cards, the cap)` for `status`.
    pub(crate) fn own_open_cards(&self) -> (usize, usize) {
        (self.own_open(), OPEN_CARDS_PER_PROPOSER_MAX)
    }

    /// Inbound: a peer proposed something (gossip). Record it as pending so it
    /// shows up and can be approved here. `by` is the authenticated wire
    /// sender — the proposer on a direct delivery, the serving peer on a
    /// WP2 re-serve (a display hint, never an authorization input).
    /// Returns `true` only when the proposal was genuinely NEW here — a
    /// refused id collision or a deduplicated re-serve (WP2 catch-up
    /// re-wraps open proposals under the serving peer's name) returns
    /// `false`, and the caller must not announce it on the event stream.
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
        // a decided card that aged out: a late re-serve must not reopen it
        if self.retired.contains(&id) {
            tracing::debug!(%id, "refusing a retired proposal");
            return false;
        }
        if is_base_entry_op(&payload) {
            tracing::warn!(%id, %by, "refusing a proposal with a reserved op");
            return false;
        }
        if let Some(r) = self.vault_enable_refusal(surface, &payload) {
            tracing::warn!(%id, %by, refusal = %r, "refusing a set_features adding the vault");
            return false;
        }
        if surface == Surface::Vault {
            if let Err(e) = self.vault_wire_check(id, &payload) {
                tracing::warn!(%id, %by, error = %e, "vault: refusing a proposal");
                return false;
            }
        }
        if surface == Surface::Wallet && !crate::wallet::purse_op_ok(&payload) {
            tracing::warn!(%id, %by, "wallet: refusing a malformed op");
            return false;
        }
        if let Some(e) = self.wallet_feature_refusal(surface, &payload) {
            tracing::warn!(%id, %by, error = %e, "refusing a set_features adding the wallet");
            return false;
        }
        // L3: a flooding proposer may only crowd ITSELF — the newest card
        // is refused (the WP2 re-serve re-earns an honest one later), and
        // another member's cards are never evicted
        // A relayed card (a re-serve, or a forgery in a third seat's id
        // stripe) is charged to the relayer's own relay budget, never to
        // the seat the id names
        if !self.proposals.contains_key(&id) {
            let minter = self.minting_seat(id);
            let full = if minter.as_deref().map_or(true, |m| m == by) {
                self.open_cards_by(by) >= OPEN_CARDS_PER_PROPOSER_MAX
            } else {
                self.open_relayed_by(by) >= self.relay_budget()
            };
            if full {
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
        if self.chain.proposal_changes.contains_key(&id) {
            tracing::warn!(%id, "refusing a surface proposal whose id names a chain change");
            return false;
        }
        // an id the verified chain already consumed (the walk's double-apply
        // guard, blob-seeded on a pruned holder) can only be a stale resend —
        // a fresh card would resurrect a decided vote. The reopen twin of
        // this guard is `settle_cards_against_chain`.
        if self.chain.walk.as_ref().is_some_and(|w| w.seen.contains(&id)) {
            tracing::debug!(%id, "refusing a proposal the chain already consumed");
            return false;
        }
        // A1 guard: the id is taken by ANOTHER change - a collision, not a
        // re-serve. The first card stands; the newcomer is refused and
        // remembered so the surfaces can say so (silently absorbing it is
        // how a seat once saw "applied 2/2" for a page that never landed).
        if let Some(existing) = self.proposals.get(&id) {
            if existing.surface != surface || existing.payload != payload {
                tracing::warn!(%id, %by, "refusing a proposal whose id names another change");
                if self.chain.id_collisions.len() < crate::proposals::ID_COLLISIONS_MAX
                    || self.chain.id_collisions.contains_key(&id)
                {
                    self.chain.id_collisions.insert(id, by.to_string());
                }
                return false;
            }
        }
        let mut inserted = false;
        self.proposals.entry(id).or_insert_with(|| {
            inserted = true;
            molt_core::ProposalRecord::new(surface, payload, by.to_string())
        });
        self.attribute_card(id, by);
        if inserted && surface == Surface::Vault {
            self.after_vault_proposed(id);
        }
        if inserted && surface == Surface::Memory {
            // registration-time check (shared_memory_real.md §4): a patch
            // learned LATE against an already-moved base registers
            // superseded right away — no zombie pending cards on rejoiners
            self.supersede_stale_wiki(None);
        }
        if inserted {
            // L2: signatures that OUTRAN this card become displayable now
            self.reverify_pending(id);
        }
        inserted
    }

    /// Inbound: a peer's signed approval (gossip). Collect + try to seal.
    /// `true` when it was dropped for a height beyond the next block.
    pub(crate) fn receive_approval(&mut self, id: u64, by: &str, height: u64, sig: &str) -> bool {
        self.receive_approval_via(by, id, by, height, sig)
    }

    /// [`Self::receive_approval`] as relayed by the wire sender `via`.
    pub(crate) fn receive_approval_via(&mut self, via: &str, id: u64, by: &str, height: u64, sig: &str) -> bool {
        if sig.is_empty() || self.retired.contains(&id) {
            return false;
        }
        // L3: an approval may OUTRUN its card (collected, displayed once it
        // lands) - but only inside the same id window everything else uses,
        // or unknown-id entries grow without bound
        if !self.plausible_wire_id(id) {
            tracing::warn!(%id, "dropping an approval for an implausible proposal id");
            return false;
        }
        // SECURITY: `height` is peer-supplied. A legitimate approval can
        // only be for the current target (head + 1) or a value we already
        // hold; an out-of-range height (e.g. u64::MAX) would let collect_sig
        // adopt it, clear the real signatures, and — since rebase only
        // sweeps heights BELOW the target — never recover, permanently
        // freezing the proposal (governance-liveness DoS). Bound it here.
        let target = self.chain.head.as_ref().map(|h| h.height + 1);
        if target.is_some_and(|t| height > t) {
            // B6 (D3): one block ahead is the sender's own seal racing it
            if target.is_some_and(|t| height == t + 1) && self.park_approval(via, id, by, height, sig) {
                return false;
            }
            tracing::warn!(%id, height, "dropping an approval for an implausible future height");
            return true;
        }
        let verified = self.approval_verifies(id, height, by, sig);
        self.collect_sig(id, height, by, sig, verified);
        if verified {
            if let Some(p) = self.chain.pending_sigs.get_mut(&id) {
                p.verified.insert(by.to_string());
            }
            self.note_signer(by, height);
        }
        self.try_commit(id);
        // B7 §14: a verified free signature is evidence for the lock too
        if verified {
            self.lock_note_free(by, height);
        }
        false
    }

    /// A6: answer a dropped vote with the one fact its sender is missing -
    /// this node's head. Only for a member of the roster, never for our own
    /// signature, and once per voter per [`VOTE_REFUSAL_DEBOUNCE_SECS`]: every
    /// peer records and publishes the answer.
    pub(crate) fn tell_the_voter_it_was_dropped(&mut self, id: u64, voter: &str, height: u64) {
        let me = self.member();
        if voter == me || !self.roster().iter().any(|m| m == voter) {
            return;
        }
        // D4: catching up, this seat is the one behind - not the voter
        if self.chain.catchup_from.is_some() || self.chain_lag().peers_ahead > 0 {
            return;
        }
        let now = self.presence_now();
        if self
            .chain
            .refused_at
            .get(voter)
            .is_some_and(|t| now.saturating_sub(*t) < VOTE_REFUSAL_DEBOUNCE_SECS)
        {
            return;
        }
        self.chain.refused_at.insert(voter.to_string(), now);
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let env = self.make_env(
            me,
            WorkspaceEvent::VoteRefused {
                id: ProposalId(id),
                voter: voter.to_string(),
                height,
                head,
            },
        );
        self.record(env);
    }

    /// INTERNAL (A6): a peer dropped a vote this seat cast. Surface it - the
    /// vote reply already reported this node's own head, and now the seat
    /// learns the republic's.
    pub(crate) fn cmd_net_vote_refused(
        &mut self,
        proposal: ProposalId,
        by: &str,
        head: u64,
    ) -> Result<molt_core::Reply, molt_core::MoltError> {
        let own = self.chain.head.as_ref().map_or(0, |h| h.height);
        tracing::warn!(proposal = proposal.0, peer = %by, peer_head = head, head = own, "vote=refused");
        self.session.notice = format!("vote-refused:{}:{by}:{head}", proposal.0);
        self.emit_session(crate::SessionScope::Full);
        Ok(molt_core::Reply::Ack)
    }

    /// D4: `peer`'s own vote counts here at the current height - it is
    /// level again, so its `vote-refused` notice goes.
    pub(crate) fn clear_vote_refused_by(&mut self, peer: &str, height: u64) {
        if self.chain.head.as_ref().map(|h| h.height + 1) != Some(height) {
            return;
        }
        let shows = self
            .session
            .notice
            .strip_prefix("vote-refused:")
            .and_then(|rest| rest.split(':').nth(1))
            .is_some_and(|p| p == peer);
        if shows {
            self.session.notice.clear();
            self.emit_session(crate::SessionScope::Full);
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
            // membership records stay out (their window is liveness-bound, see
            // the doc above) — re-serving one as a plain `Proposed` would make
            // receivers register a SURFACE change under the membership id and
            // sign different bytes than everyone else
            .filter(|(id, _)| {
                !matches!(
                    self.chain.proposal_changes.get(id),
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
            if let Some(pending) = self.chain.pending_sigs.get(id) {
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
            // B7: the own intent, the only one this seat may speak for
            if self.chain.lock.intent_sent.contains(id) {
                events.push(WorkspaceEvent::Approved {
                    id: ProposalId(*id),
                    by: self.member(),
                    height: self.chain.head.as_ref().map_or(0, |h| h.height + 1),
                    sig: String::new(),
                });
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
                    self.chain.proposal_changes.get(id),
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
            .chain(self.chain.pending_declines.iter().filter_map(|(id, parked)| {
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
        // A2: a cut needs EVERY seat, so the seat that was away must still
        // be able to co-sign the one on the table - it lost the announce
        // with its RAM like any other open card. At most one entry: a cut
        // is registered only at the current head ("ONE cut per head").
        if let Some(head) = self.chain.head.as_ref() {
            for (id, change) in &self.chain.proposal_changes {
                let Some((upto, state_hash, kind)) = CutKind::of(change) else {
                    continue;
                };
                let folded = kind.wiki;
                if upto != head.height {
                    continue;
                }
                events.push(WorkspaceEvent::CheckpointProposed {
                    id: ProposalId(*id),
                    upto,
                    state_hash: state_hash.to_string(),
                    folded,
                });
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
    /// Re-send this seat's own signature for `id` (S5): the seat ahead may
    /// have missed it. Only the own one - re-sending all would multiply.
    fn reserve_signatures(&mut self, id: u64) {
        let me = self.member();
        let Some(pending) = self.chain.pending_sigs.get(&id) else {
            return;
        };
        let events: Vec<WorkspaceEvent> = pending
            .sigs
            .iter()
            .filter(|a| a.member == me && pending.verified.contains(&a.member))
            .map(|a| WorkspaceEvent::Approved {
                id: ProposalId(id),
                by: a.member.clone(),
                height: pending.height,
                sig: a.sig.clone(),
            })
            .collect();
        let me = self.member();
        for body in events {
            let env = self.make_env(me.clone(), body);
            self.record(env);
        }
    }

    pub(crate) fn serve_open_governance(&mut self) {
        let events = self.open_governance_events();
        let me = self.member();
        for body in events {
            let env = self.make_env(me.clone(), body);
            self.record(env);
        }
    }
}
