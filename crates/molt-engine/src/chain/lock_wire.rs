// SPDX-License-Identifier: GPL-3.0-or-later

//! **The sealing lock, wired** (`docs/chain/chain_lock_design.md`, B7b):
//! the per-height mode, the one gate of a free (B6) position signature,
//! the acceptor's durable answers to the five lock frames, the leader and
//! its takeover, the same-ballot seal, the amnesia floor and the status.
//!
//! Every position signature this seat gives leaves through exactly two
//! doors: [`State::lock_sign_freely`] (the free mark is durable first) and
//! [`State::lock_acceptor_accept`] (the acceptance is durable first).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::governance::{FALLBACK_ASK_SECS, SEALER_GRACE_SECS};
use super::lock::{Acceptor, Ballot, Leader, Quorum, Reply, Sealable, Slot, Step, Tally, Value};
use super::*;

/// The enabling vote waits: a seat runs a build without the lock.
pub(crate) const LOCK_NOT_EVERYWHERE: &str = "a seat runs a build without the lock";

/// A leader re-sends its open phase this often (§7).
pub(crate) const LOCK_RETRANSMIT_SECS: u64 = 15;
/// The cap of a leader's retry backoff (§5).
const LOCK_BACKOFF_MAX_SECS: u64 = 600;

/// Test seams of the lock (never a `Command`).
#[derive(Default)]
pub(crate) struct LockSeams {
    /// 0/1 = this build, 2 = an older build (no capability, every lock
    /// frame ignored, free signatures only).
    pub(crate) cap: std::sync::atomic::AtomicU8,
    /// The next founding leaves the lock out of the charter (a republic
    /// founded before the lock existed).
    pub(crate) found_pre_lock: AtomicBool,
    /// Takeover grace in seconds, 0 = [`SEALER_GRACE_SECS`].
    pub(crate) grace_secs: AtomicU64,
    /// Retransmit interval in seconds, 0 = [`LOCK_RETRANSMIT_SECS`].
    pub(crate) retransmit_secs: AtomicU64,
    /// The restore settle step in seconds, 0 = `RESTORE_SETTLE_SECS`.
    pub(crate) settle_secs: AtomicU64,
    /// The next leader to send a `LockAccept` stops right after it.
    pub(crate) stop_after_accept: AtomicBool,
    /// Lock activity of this seat has stopped (a killed leader).
    pub(crate) stopped: AtomicBool,
}

/// What a seat initiates at a height (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockMode {
    Lock,
    Free,
}

/// `chain_lock.state`: the acceptor, and the id (plus the change of a
/// membership change or cut) behind each digest it accepted, so a promise
/// after a restart still names what it holds.
#[derive(Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    acceptor: Acceptor,
    #[serde(default)]
    values: BTreeMap<String, (u64, Option<ChainChange>)>,
}

/// One ballot this seat leads.
struct Run {
    leader: Leader,
    promised: BTreeSet<String>,
    /// digest -> (id, change) as the promises named them.
    ids: BTreeMap<String, (u64, Option<ChainChange>)>,
    /// The proposed value's id and wire change.
    value: Option<(u64, Option<ChainChange>)>,
    started: u64,
    sent_at: u64,
    attempt: u32,
    /// Re-sends of the open phase since the last new promise (W4 backoff).
    resends: u32,
}

/// The takeover clock of head+1.
#[derive(Debug, Clone)]
struct Wait {
    height: u64,
    since: u64,
    attempt: u32,
    next_at: u64,
    asked: bool,
}

/// A same-ballot quorum another leader has not sealed yet.
#[derive(Clone)]
struct Learned {
    slot: Slot,
    sealable: Sealable,
    since: u64,
    id: u64,
    change: ChainChange,
}

/// The lock's state on this seat; only `acceptor` and `values` are durable.
#[derive(Default)]
pub(crate) struct LockRuntime {
    pub(crate) acceptor: Acceptor,
    values: BTreeMap<String, (u64, Option<ChainChange>)>,
    /// §6: `Some(None)` = no lock file where one was kept, no peer answered
    /// yet (passive everywhere); `Some(Some(floor))` = passive up to `floor`.
    pub(crate) amnesia: Option<Option<u64>>,
    /// Standing approve intents per proposal id (envelope author = voter).
    pub(crate) intents: BTreeMap<u64, BTreeSet<String>>,
    /// Ids this seat already announced its intent for.
    pub(crate) intent_sent: BTreeSet<u64>,
    /// The head+1 each own intent was first cast at (the reopen's rebase
    /// verdict dates the card by it).
    pub(crate) intent_at: BTreeMap<u64, u64>,
    run: Option<Run>,
    sigs: BTreeMap<(Slot, Ballot, String), BTreeMap<String, String>>,
    tallies: BTreeMap<Slot, Tally>,
    learned: Option<Learned>,
    seen: BTreeMap<Slot, Ballot>,
    known: BTreeMap<(u64, String), (u64, ChainChange)>,
    wait: Option<Wait>,
    lineage: Option<(u64, String, [u8; 32])>,
    /// The capability state this seat last announced.
    pub(crate) announced: Option<bool>,
    /// The head+1 whose standing intents were last re-expressed freely.
    freed_at: Option<u64>,
    /// An `Accept` this seat withheld at head+1 (a restore settling, an
    /// intent not yet cast): retried on the tick, not only on the
    /// leader's next resend - the leader's own acceptance has no resend.
    retry_accept: Option<(Slot, Ballot, Value, u64, Option<ChainChange>)>,
    /// Seats seen with lock state at a height (a promise, an acceptance,
    /// a leader's frame): never counted free there - a seat that later
    /// signs freely at that height was downgraded, not free (S2).
    locked_seen: BTreeMap<u64, BTreeSet<String>>,
    /// Heights where this seat holds its own free signature beside lock
    /// state (a downgraded build ran here): it neither promises, accepts
    /// nor signs there.
    passive: BTreeSet<u64>,
    /// Seats known to have signed freely at a height (a verified free
    /// signature or a free `Nack`).
    free_seen: BTreeMap<u64, BTreeSet<String>>,
    /// Test override of "the chain enabled the lock".
    #[cfg(test)]
    pub(crate) enabled_override: Option<bool>,
    /// Test override of "every seat announced the capability".
    #[cfg(test)]
    pub(crate) caps_override: Option<bool>,
    /// Test: every own envelope this seat put on the wire.
    #[cfg(test)]
    pub(crate) outbox: Vec<molt_core::EventEnvelope>,
    /// Test: the bytes the last successful write left on "disk".
    #[cfg(test)]
    pub(crate) disk: Option<Vec<u8>>,
    /// Test: every write fails.
    #[cfg(test)]
    pub(crate) fail_writes: bool,
}

fn digest_raw(rid: &str, block: &ChainBlock) -> [u8; 32] {
    Sha256::digest(approval_bytes(rid, block.height, &block.change)).into()
}

/// `d(h)` in hex: the digest of `change`'s approval bytes at `height`.
pub(crate) fn change_digest(rid: &str, height: u64, change: &ChainChange) -> String {
    hex::encode(Sha256::digest(approval_bytes(rid, height, change)))
}

/// `L(head)` (§3.1): from the last cut block (a pruned holder's anchor) or
/// the genesis, over every change digest up to the head. Attestations do
/// not enter it, so a relink keeps it.
pub(crate) fn lineage(rid: &str, blocks: &[ChainBlock]) -> Option<[u8; 32]> {
    let start = blocks.iter().rposition(|b| b.height == 0 || CutKind::of(&b.change).is_some())?;
    let anchor = &blocks[start];
    let mut h = Sha256::new();
    h.update(b"molt-lock-lineage-v1\0");
    h.update(anchor.height.to_le_bytes());
    h.update(digest_raw(rid, anchor));
    let mut l: [u8; 32] = h.finalize().into();
    for b in &blocks[start + 1..] {
        l = extend(l, rid, b);
    }
    Some(l)
}

fn extend(l: [u8; 32], rid: &str, block: &ChainBlock) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(l);
    h.update(digest_raw(rid, block));
    h.finalize().into()
}

/// The change the wire carries beside the digest: membership changes and
/// cuts only (§3.2).
fn wire_change(change: &ChainChange) -> Option<ChainChange> {
    (matches!(change, ChainChange::Membership { .. }) || CutKind::of(change).is_some()).then(|| change.clone())
}

fn fmt_ballot(b: &Ballot) -> String {
    format!("{}:{}", b.round, b.leader)
}

fn need_u64(v: &Value) -> u64 {
    u64::try_from(v.need).unwrap_or(u64::MAX)
}

impl State {
    fn lock_seam(&self, flag: &AtomicBool) -> bool {
        flag.load(SeqCst)
    }

    /// This build runs the lock (an older-build seam turns it off).
    pub(crate) fn lock_cap_on(&self) -> bool {
        self.lock_seams.cap.load(SeqCst) != 2
    }

    #[cfg(test)]
    pub(crate) fn lock_force(&self, on: bool) {
        self.lock_seams.cap.store(if on { 1 } else { 2 }, SeqCst);
    }

    fn lock_listens(&self) -> bool {
        self.is_chain_governed() && self.lock_cap_on() && !self.lock_seam(&self.lock_seams.stopped)
    }

    pub(crate) fn lock_grace(&self) -> u64 {
        match self.lock_seams.grace_secs.load(SeqCst) {
            0 => SEALER_GRACE_SECS,
            s => s,
        }
    }

    fn lock_retransmit(&self) -> u64 {
        match self.lock_seams.retransmit_secs.load(SeqCst) {
            0 => LOCK_RETRANSMIT_SECS,
            s => s,
        }
    }

    /// The retry delay of `attempt`: grace x (attempt + 1), capped, plus a
    /// jitter in [0, grace/2) from the seat's name (§5).
    fn lock_backoff(&self, height: u64, attempt: u32) -> u64 {
        let grace = self.lock_grace();
        let base = grace.saturating_mul(u64::from(attempt) + 1).min(LOCK_BACKOFF_MAX_SECS);
        let mut h = Sha256::new();
        h.update(self.member().as_bytes());
        h.update(height.to_le_bytes());
        h.update(attempt.to_le_bytes());
        let d = h.finalize();
        let jitter = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]) % (grace / 2).max(1);
        base + jitter
    }

    fn lock_quorum(&self) -> Option<Quorum> {
        let head = self.chain.head.as_ref()?;
        Some(Quorum { n: head.identities.len(), m: usize::from(head.rule_m) })
    }

    fn lock_rostered(&self, member: &str) -> bool {
        self.chain.head.as_ref().is_some_and(|h| h.identities.iter().any(|i| i.member == member))
    }

    /// The slot of head+1. `L(head)` is extended block by block and
    /// recomputed whenever the head did not move by exactly one.
    pub(crate) fn lock_slot(&mut self) -> Option<Slot> {
        let head = self.chain.head.as_ref()?;
        let (height, hash) = (head.height, head.hash.clone());
        let rid = self.republic_id();
        let last = self.chain.blocks.last();
        let l = match &self.chain.lock.lineage {
            Some((h, hh, l)) if *h == height && *hh == hash => *l,
            Some((h, hh, l))
                if h + 1 == height
                    && last.is_some_and(|b| b.height == height && b.prev == *hh && CutKind::of(&b.change).is_none()) =>
            {
                extend(*l, &rid, last?)
            }
            _ => lineage(&rid, &self.chain.blocks)?,
        };
        self.chain.lock.lineage = Some((height, hash, l));
        Some(Slot { height: height + 1, base: hex::encode(l) })
    }

    /// What `change` is as a value at `height`: its digest and the live
    /// acceptances that seal it (§3.2). `None` = no value there.
    pub(crate) fn lock_value(&self, height: u64, change: &ChainChange) -> Option<Value> {
        let q = self.lock_quorum()?;
        let need = match change {
            ChainChange::Genesis { .. } => return None,
            c if CutKind::of(c).is_some() => q.n,
            ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. } => {
                self.restore_consent_at(change, height).ok()?;
                q.m.saturating_sub(1).max(1)
            }
            _ => q.m,
        };
        Some(Value { digest: change_digest(&self.republic_id(), height, change), need })
    }

    /// Seats whose consented restore is registered: never counted as
    /// promisers (§4.2) nor as a restore's acceptors.
    fn lock_restore_subjects(&self) -> BTreeSet<String> {
        self.chain
            .proposal_changes
            .values()
            .filter_map(|c| match c {
                ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } => {
                    Some(member.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The chain enabled the lock at head+1 (§8 as built): the effective
    /// features at the head carry [`molt_core::chain::CHAIN_LOCK_FEATURE`] -
    /// ratified at the founding, or added by a sealed `set_features` vote.
    /// Chain state, so every holder (full or pruned) agrees.
    pub(crate) fn lock_enabled(&self) -> bool {
        #[cfg(test)]
        if let Some(on) = self.chain.lock.enabled_override {
            return on;
        }
        self.effective_features().iter().any(|f| f == molt_core::chain::CHAIN_LOCK_FEATURE)
    }

    /// The first locked height this holder can see: 1 for a lock the
    /// founding ratified, the height after the enabling block, or - when
    /// that block lies below a pruned holder's anchor - the anchor's next.
    pub(crate) fn lock_since(&self) -> u64 {
        let key = molt_core::chain::CHAIN_LOCK_FEATURE;
        if !self.lock_enabled() {
            return 0;
        }
        if self.replica.as_ref().and_then(|r| r.features.as_ref()).is_some_and(|f| f.iter().any(|k| k == key)) {
            return 1;
        }
        let enabling = self.chain.blocks.iter().find(|b| match &b.change {
            ChainChange::Applied { surface: Surface::Organization, payload, .. } => {
                payload.get("op").and_then(serde_json::Value::as_str) == Some("set_features")
                    && payload
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|v| v.split_whitespace().any(|k| k == key))
            }
            _ => false,
        });
        enabling.or_else(|| self.chain.blocks.first()).map_or(0, |b| b.height + 1)
    }

    /// Every other roster seat announced [`molt_core::chain::CHAIN_LOCK_CAP`]
    /// (information only: it gates the enabling vote, never the mode).
    pub(crate) fn lock_all_capable(&self) -> bool {
        #[cfg(test)]
        if let Some(all) = self.chain.lock.caps_override {
            return all;
        }
        let me = self.member();
        let cap = molt_core::chain::CHAIN_LOCK_CAP;
        let known = self.active.as_ref().map(|a| &a.prefs.seat_caps);
        let Some(head) = self.chain.head.as_ref() else {
            return false;
        };
        head.identities.iter().all(|i| {
            i.member == me || known.and_then(|k| k.get(&i.member)).is_some_and(|c| c.iter().any(|c| c == cap))
        })
    }

    /// Why this seat neither proposes nor approves enabling the lock now:
    /// a seat it knows of runs a build without it.
    pub(crate) fn lock_enable_blocker(&self, change: &ChainChange) -> Option<&'static str> {
        let ChainChange::Applied { surface: Surface::Organization, payload, .. } = change else {
            return None;
        };
        let enables = payload.get("op").and_then(serde_json::Value::as_str) == Some("set_features")
            && payload
                .get("value")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|v| v.split_whitespace().any(|k| k == molt_core::chain::CHAIN_LOCK_FEATURE));
        (enables && !(self.lock_cap_on() && self.lock_all_capable())).then_some(LOCK_NOT_EVERYWHERE)
    }

    /// §8: lock state here -> lock; a free mark (or a dead lock, §14) ->
    /// free; else lock iff the chain enabled it.
    pub(crate) fn lock_mode(&self, height: u64) -> LockMode {
        if !self.lock_cap_on() {
            return LockMode::Free;
        }
        let a = &self.chain.lock.acceptor;
        if !a.may_sign_freely(height) {
            return LockMode::Lock;
        }
        if a.signed_freely(height) || self.lock_dead_at(height) {
            return LockMode::Free;
        }
        if self.lock_enabled() {
            LockMode::Lock
        } else {
            LockMode::Free
        }
    }

    /// Other seats known to have signed freely at `height`; a registered
    /// restore's subject never counts (§4.2: its old device is gone).
    fn lock_free_count(&self, height: u64) -> usize {
        let me = self.member();
        let subjects = self.lock_restore_subjects();
        self.chain
            .lock
            .free_seen
            .get(&height)
            .map_or(0, |f| {
                let locked = self.chain.lock.locked_seen.get(&height);
                f.iter()
                    .filter(|m| **m != me && !subjects.contains(*m) && !locked.is_some_and(|l| l.contains(*m)))
                    .count()
            })
    }

    /// The other seats known free at `height` (S2: never one seen locked).
    fn lock_known_free(&self, height: u64) -> Vec<String> {
        let me = self.member();
        let subjects = self.lock_restore_subjects();
        let locked = self.chain.lock.locked_seen.get(&height);
        self.chain
            .lock
            .free_seen
            .get(&height)
            .map(|f| {
                f.iter()
                    .filter(|m| **m != me && !subjects.contains(*m) && !locked.is_some_and(|l| l.contains(*m)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// §14: the free signers at `height` leave the lock no decision there
    /// once this seat leaves too ([`Quorum::lock_dead`]).
    fn lock_dead_at(&self, height: u64) -> bool {
        let Some(q) = self.lock_quorum() else { return false };
        q.lock_dead(self.lock_free_count(height), self.chain.lock.acceptor.holds_acceptance(height))
    }

    /// `member` signed freely at `height` (verified, or its free `Nack`).
    pub(crate) fn lock_note_free(&mut self, member: &str, height: u64) {
        if !self.lock_cap_on() || !self.lock_rostered(member) || member == self.member() {
            return;
        }
        if self.chain.lock.free_seen.entry(height).or_default().insert(member.to_string()) {
            self.lock_try_release(height);
        }
    }

    /// Once the lock is dead at head+1 for this seat, trade its lock state
    /// there durably for a free mark and re-express its standing approvals
    /// freely.
    fn lock_try_release(&mut self, height: u64) {
        let next = self.chain.head.as_ref().map_or(0, |h| h.height + 1);
        // an amnesiac seat may have forgotten an acceptance: it stays passive
        if height != next
            || self.chain.lock.acceptor.signed_freely(height)
            || self.lock_amnesiac_at(height)
            || !self.lock_dead_at(height)
        {
            return;
        }
        let Some(q) = self.lock_quorum() else { return };
        let free = self.lock_free_count(height);
        let before = self.chain.lock.acceptor.clone();
        if !self.chain.lock.acceptor.release_to_free(height, q, free) {
            return;
        }
        if !self.persist_lock() {
            self.chain.lock.acceptor = before;
            return;
        }
        tracing::info!(height, free, "lock=released");
        if self.chain.lock.run.as_ref().is_some_and(|r| r.leader.slot.height == height) {
            self.chain.lock.run = None;
        }
        let standing: Vec<u64> = self
            .chain
            .own_approvals
            .iter()
            .copied()
            .filter(|id| self.proposals.get(id).map_or(true, |p| p.state == ProposalState::Proposed))
            .filter(|id| !self.own_signature_stands_freely(*id, height))
            .collect();
        for id in standing {
            self.chain_sign_and_gossip_approval(id);
        }
    }

    /// `height` went free while this seat's approvals are unsigned intents
    /// (a capability changed): sign them freely, once per height. A B6
    /// re-base re-signs only collected signatures, never an intent.
    fn lock_free_standing_intents(&mut self, height: u64) {
        if self.chain.lock.freed_at == Some(height) || self.lock_mode(height) != LockMode::Free {
            return;
        }
        self.chain.lock.freed_at = Some(height);
        let intents: Vec<u64> = self
            .chain
            .lock
            .intent_sent
            .iter()
            .copied()
            .filter(|id| self.chain.own_approvals.contains(id))
            .filter(|id| self.proposals.get(id).map_or(true, |p| p.state == ProposalState::Proposed))
            .filter(|id| !self.own_signature_stands_freely(*id, height))
            .collect();
        for id in intents {
            tracing::debug!(height, id, "lock=intent_signed_freely");
            self.chain_sign_and_gossip_approval(id);
        }
    }

    fn own_signature_stands_freely(&self, id: u64, height: u64) -> bool {
        let me = self.member();
        self.chain.pending_sigs.get(&id).is_some_and(|p| p.height == height && p.sigs.iter().any(|a| a.member == me))
    }

    /// §6: this seat may have promised at `height` and forgot.
    pub(crate) fn lock_amnesiac_at(&self, height: u64) -> bool {
        match self.chain.lock.amnesia {
            None => false,
            Some(None) => true,
            Some(Some(floor)) => height <= floor,
        }
    }

    /// Write the acceptor durably; `false` = it did not reach the disk.
    fn persist_lock(&mut self) -> bool {
        let file = LockFile { acceptor: self.chain.lock.acceptor.clone(), values: self.chain.lock.values.clone() };
        let Ok(bytes) = serde_json::to_vec(&file) else {
            return false;
        };
        #[cfg(test)]
        {
            if self.chain.lock.fail_writes {
                tracing::error!("lock=unwritten");
                return false;
            }
            self.chain.lock.disk = Some(bytes.clone());
        }
        let durable = self.active.as_ref().map_or(true, |a| a.handle.persist_chain_lock_blocking(bytes));
        if !durable {
            tracing::error!("lock=unwritten");
            return false;
        }
        if let Some(a) = &mut self.active {
            if !a.prefs.chain_lock {
                a.prefs.chain_lock = true;
                a.handle.set_prefs(a.prefs.clone());
            }
        }
        true
    }

    /// One acceptor change, kept only once durable: on a failed write the
    /// acceptor is rolled back and no reply exists. `(reply, changed)`.
    fn lock_step(
        &mut self,
        f: impl FnOnce(&mut Acceptor) -> Step,
        value: Option<(String, u64, Option<ChainChange>)>,
    ) -> Option<(Reply, bool)> {
        let before = (self.chain.lock.acceptor.clone(), self.chain.lock.values.clone());
        let step = f(&mut self.chain.lock.acceptor);
        if step.persist {
            if let Some((digest, id, change)) = value {
                self.chain.lock.values.insert(digest, (id, change));
            }
            if !self.persist_lock() {
                (self.chain.lock.acceptor, self.chain.lock.values) = before;
                return None;
            }
        }
        Some((step.reply, step.persist))
    }

    /// **The one gate of a free (B6) position signature** at `height`:
    /// refused where this seat holds lock state or is amnesiac; the free
    /// mark is durable before `true`.
    pub(crate) fn lock_sign_freely(&mut self, height: u64) -> bool {
        // W7: no free mark, no amnesia where the chain never enabled the lock
        if !self.lock_cap_on() || !self.lock_enabled() {
            return true;
        }
        if self.chain.lock.passive.contains(&height) {
            tracing::warn!(height, "lock=passive reason=conflict");
            return false;
        }
        if self.lock_amnesiac_at(height) {
            tracing::info!(height, "lock=passive reason=amnesia");
            return false;
        }
        let before = self.chain.lock.acceptor.clone();
        match self.chain.lock.acceptor.sign_freely(height) {
            None => {
                tracing::warn!(height, "lock=refused_free reason=lock_state");
                false
            }
            Some(false) => true,
            Some(true) => {
                if self.persist_lock() {
                    true
                } else {
                    self.chain.lock.acceptor = before;
                    false
                }
            }
        }
    }

    fn lock_send(&mut self, body: WorkspaceEvent) {
        let me = self.member();
        let env = self.make_env(me, body);
        self.record(env);
    }

    /// Record `member`'s standing intent for `id`.
    pub(crate) fn lock_note_intent(&mut self, id: u64, member: &str) {
        if self.lock_cap_on() && (self.lock_rostered(member) || self.roster().iter().any(|m| m == member)) {
            self.chain.lock.intents.entry(id).or_default().insert(member.to_string());
        }
    }

    /// This seat approves `id` at a locked height: the intent (an unsigned
    /// `Approved`, once), and the leader acts on it (§7).
    pub(crate) fn lock_intent(&mut self, id: u64) {
        let me = self.member();
        self.chain.own_approvals.insert(id);
        self.lock_note_intent(id, &me);
        if self.chain.lock.intent_sent.insert(id) {
            let height = self.chain.head.as_ref().map_or(0, |h| h.height + 1);
            self.chain.lock.intent_at.entry(id).or_insert(height);
            let env = self.make_env(
                me.clone(),
                WorkspaceEvent::Approved { id: ProposalId(id), by: me, height, sig: String::new() },
            );
            self.record(env);
        }
        self.lock_drive();
    }

    /// The standing intents as a snapshot keeps them.
    pub(crate) fn lock_dumped_intents(&self) -> Vec<molt_core::DumpedVote> {
        let me = self.member();
        let me = me.as_str();
        self.chain
            .lock
            .intents
            .iter()
            .flat_map(|(id, who)| {
                let at = self.chain.lock.intent_at.get(id).copied().unwrap_or(0);
                who.iter().map(move |m| molt_core::DumpedVote {
                    id: *id,
                    height: if *m == me { at } else { 0 },
                    member: m.clone(),
                    sig: String::new(),
                })
            })
            .collect()
    }

    /// Rebuild the intents a snapshot covered (the replay twin is the
    /// `Approved` arm of `apply`).
    pub(crate) fn lock_restore_intents(&mut self, intents: &[molt_core::DumpedVote]) {
        let me = self.member();
        for v in intents {
            self.lock_note_intent(v.id, &v.member);
            if v.member == me {
                self.chain.lock.intent_sent.insert(v.id);
                if v.height > 0 {
                    self.chain.lock.intent_at.insert(v.id, v.height);
                    if self.chain.adoption_pending {
                        self.note_replay_held(v.id, v.height);
                    }
                }
            }
        }
    }

    /// Distinct standing voices for `id`: intents and verified signatures,
    /// current decliners left out (the display count at a locked height).
    pub(crate) fn lock_voices(&self, id: u64) -> usize {
        if !self.lock_cap_on() {
            return 0;
        }
        let decliners: Vec<String> = self.proposals.get(&id).map(|p| p.decliners.clone()).unwrap_or_default();
        let mut voices: BTreeSet<&String> = self.chain.lock.intents.get(&id).map(|s| s.iter().collect()).unwrap_or_default();
        if let Some(p) = self.chain.pending_sigs.get(&id) {
            voices.extend(p.verified.iter());
        }
        voices.into_iter().filter(|m| !decliners.contains(m)).count()
    }

    /// The B6 choice (§5): the smallest open id whose intents reach its
    /// `need`; a cut only with every seat's promise in hand (§4.1).
    fn lock_choose(&self, slot: &Slot, promised: Option<&BTreeSet<String>>) -> Option<(u64, ChainChange, Value)> {
        let q = self.lock_quorum()?;
        let subjects = self.lock_restore_subjects();
        // a seat that signed freely here never accepts: its intent cannot help
        let free = self.chain.lock.free_seen.get(&slot.height).cloned().unwrap_or_default();
        let mut ids: BTreeSet<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .map(|(id, _)| *id)
            .collect();
        ids.extend(self.chain.proposal_changes.keys().copied());
        for id in ids {
            if self.proposals.get(&id).is_some_and(|p| p.state != ProposalState::Proposed) {
                continue;
            }
            // W5: the cheap count first; the change and its digest only for
            // a card that has the voices
            let decliners: Vec<String> = self.proposals.get(&id).map(|p| p.decliners.clone()).unwrap_or_default();
            let voices = self.chain.lock.intents.get(&id).map_or(0, |s| {
                s.iter()
                    .filter(|m| !decliners.contains(m) && !subjects.contains(*m) && !free.contains(*m) && self.lock_rostered(m))
                    .count()
            });
            if voices < q.m.saturating_sub(1).max(1) {
                continue;
            }
            let Some(change) = self.proposal_change(id) else { continue };
            let Some(value) = self.lock_value(slot.height, &change) else { continue };
            if voices < value.need {
                continue;
            }
            if let Some((upto, _, _)) = CutKind::of(&change) {
                if upto + 1 != slot.height || promised.is_some_and(|p| p.len() < q.n) {
                    continue;
                }
            }
            // W2: a card every seat would refuse now must not starve the
            // height while another one is ready
            if self.lock_gate(id, &change, slot.height).is_some() || !self.restore_settled(id, &change) {
                continue;
            }
            return Some((id, change, value));
        }
        None
    }

    /// Why this seat withholds its acceptance of `id` at `height` (`None` =
    /// it approves): the checks of a signature, made at accept time.
    fn lock_withheld(&self, id: u64, change: &ChainChange, height: u64) -> Option<String> {
        if self.identity_sk.is_none() {
            return Some("no signing key".into());
        }
        let me = self.member();
        if !self.chain.own_approvals.contains(&id) {
            return Some("no intent".into());
        }
        if self.proposals.get(&id).is_some_and(|p| p.state != ProposalState::Proposed || p.decliners.contains(&me)) {
            return Some("not open".into());
        }
        self.lock_gate(id, change, height)
    }

    /// The gates every seat applies to `change` alike (relay wait, restore
    /// gate and consent, vault/wallet checks, the cut's hash): a leader
    /// never proposes past one (W2), an acceptor never accepts past one.
    fn lock_gate(&self, id: u64, change: &ChainChange, height: u64) -> Option<String> {
        if let Some(why) = self.sign_blocker(id) {
            return Some(why);
        }
        match change {
            ChainChange::Applied { surface: Surface::Vault, payload, .. } => {
                if let Err(e) = self.vault_approve_check(payload) {
                    return Some(e.to_string());
                }
            }
            ChainChange::Applied { surface: Surface::Wallet, payload, .. } => {
                if let Err(e) = self.wallet_approve_check(payload) {
                    return Some(format!("{e:?}"));
                }
            }
            ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } => {
                if let Err(why) = self.restore_consent_at(change, height) {
                    return Some(why.to_string());
                }
                if let Some(why) = self.restore_gate(member) {
                    return Some(why.to_string());
                }
            }
            c => {
                if let Some((upto, hash, kind)) = CutKind::of(c) {
                    if upto + 1 != height {
                        return Some("stale cut".into());
                    }
                    if self.own_cut_hash(upto, kind).ok().as_deref() != Some(hash) {
                        return Some("cut mismatch".into());
                    }
                }
            }
        }
        None
    }

    /// Register a membership change or cut a leader carried, through the
    /// paths that gate and auto-approve it.
    fn lock_register(&mut self, id: u64, change: ChainChange) {
        match change {
            ChainChange::Membership { op, member, identity_pk, nostr_pk, relays, consent } => {
                if self.admits_membership_proposal(
                    id,
                    &ChainChange::Membership {
                        op,
                        member: member.clone(),
                        identity_pk: identity_pk.clone(),
                        nostr_pk: nostr_pk.clone(),
                        relays: relays.clone(),
                        consent: consent.clone(),
                    },
                ) {
                    self.receive_membership_proposal(id, op, &member, &identity_pk, nostr_pk, relays, consent);
                }
            }
            c => {
                if let Some((upto, hash, kind)) = CutKind::of(&c) {
                    let hash = hash.to_string();
                    self.receive_checkpoint_proposal(id, upto, &hash, kind.wiki);
                }
            }
        }
    }

    /// `member` holds lock state at `height` (it sent a lock frame there).
    fn lock_note_locked(&mut self, height: u64, member: &str) {
        self.chain.lock.locked_seen.entry(height).or_default().insert(member.to_string());
    }

    /// Record a ballot seen at `slot`; a higher foreign one stops this
    /// seat's own run there and restarts the takeover wait.
    fn lock_saw(&mut self, slot: &Slot, ballot: &Ballot) {
        if self.chain.lock.seen.get(slot).map_or(true, |b| ballot > b) {
            self.chain.lock.seen.insert(slot.clone(), ballot.clone());
        }
        let me = self.member();
        if ballot.leader == me {
            return;
        }
        let now = self.presence_now();
        let yielded = match &self.chain.lock.run {
            Some(run) if run.leader.slot == *slot && run.leader.ballot < *ballot => Some(run.attempt + 1),
            _ => None,
        };
        if let Some(attempt) = yielded {
            tracing::info!(height = slot.height, ballot = %fmt_ballot(ballot), "lock=yield");
            self.chain.lock.run = None;
            let next_at = now + self.lock_backoff(slot.height, attempt);
            self.chain.lock.wait =
                Some(Wait { height: slot.height, since: now, attempt, next_at, asked: true });
        } else if let Some(w) = self.chain.lock.wait.as_mut() {
            if w.height == slot.height && w.attempt == 0 {
                w.since = now;
            }
        }
    }

    /// Open a ballot at head+1 as its leader (§5): handle the own
    /// `Prepare` durably, then send it.
    fn lock_start(&mut self, attempt: u32) {
        let Some(slot) = self.lock_slot() else { return };
        let Some(q) = self.lock_quorum() else { return };
        if self.lock_mode(slot.height) != LockMode::Lock
            || self.lock_amnesiac_at(slot.height)
            || self.chain.lock.passive.contains(&slot.height)
            || self.lock_seam(&self.lock_seams.stopped)
            || self.identity_sk.is_none()
        {
            return;
        }
        let me = self.member();
        let now = self.presence_now();
        let seen = self.chain.lock.seen.get(&slot).map_or(0, |b| b.round);
        let round = self.chain.lock.acceptor.next_round(&slot).max(seen + 1);
        let ballot = Ballot { round, leader: me.clone() };
        let Some((Reply::Promise { accepted, .. }, _)) = self.lock_step(|a| a.prepare(&slot, &ballot), None) else {
            return;
        };
        let mut run = Run {
            leader: Leader::new(slot.clone(), q, ballot.clone()),
            promised: BTreeSet::from([me.clone()]),
            ids: BTreeMap::new(),
            value: None,
            started: now,
            sent_at: now,
            attempt,
            resends: 0,
        };
        if let Some((_, v)) = &accepted {
            if let Some(known) = self.chain.lock.values.get(&v.digest) {
                run.ids.insert(v.digest.clone(), known.clone());
            }
        }
        run.leader.promise(&me, &ballot, accepted);
        // S5: seats known free here hold nothing that can be chosen; nor
        // does a registered restore's subject - its old device is gone (§4.2)
        for m in self.lock_known_free(slot.height).into_iter().chain(self.lock_restore_subjects()) {
            run.leader.heard_free(&m);
        }
        self.chain.lock.seen.insert(slot.clone(), ballot.clone());
        self.chain.lock.run = Some(run);
        self.chain.lock.wait = None;
        tracing::info!(height = slot.height, ballot = %fmt_ballot(&ballot), attempt, "lock=prepare");
        self.lock_send(WorkspaceEvent::LockPrepare { height: slot.height, base: slot.base.clone(), round });
        self.lock_try_propose();
    }

    /// Phase 2a once the promises are in: the forced value, else the B6
    /// choice. The leader is its own first acceptor.
    fn lock_try_propose(&mut self) {
        let (slot, ballot) = match &self.chain.lock.run {
            Some(run) if run.leader.ready() && run.leader.proposed().is_none() => {
                (run.leader.slot.clone(), run.leader.ballot.clone())
            }
            _ => return,
        };
        if self.lock_slot().as_ref() != Some(&slot) {
            return;
        }
        let Some(run) = &self.chain.lock.run else { return };
        let forced = run.leader.forced();
        let promised = run.promised.clone();
        let forced_id = forced.as_ref().and_then(|v| run.ids.get(&v.digest).cloned());
        // a forced cut waits for every promise: only a full hearing lets the
        // escape free a slot one seat refused (§4.1); waiting is always safe
        let n = self.lock_quorum().map_or(0, |q| q.n);
        if forced.as_ref().is_some_and(|v| v.need >= n) && promised.len() < n {
            return;
        }
        let (id, change, own) = match &forced {
            Some(_) => match forced_id {
                Some((id, change)) => {
                    // S3: a forced value may not be chosen at all - its holders
                    // keep it alive while the others refuse it; a fuller hearing
                    // frees the slot. Only for one retransmit interval: a seat
                    // that never answers must not keep the holders of a chosen
                    // value from completing it (S1)
                    let (heard, started) =
                        self.chain.lock.run.as_ref().map_or((0, 0), |r| (r.promised.len() + r.leader.free_count(), r.started));
                    let waiting = self.presence_now() < started + self.lock_retransmit();
                    if heard < n && waiting {
                        tracing::debug!(height = slot.height, id, heard, "lock=forced_waits_for_hearing");
                        return;
                    }
                    (id, change, None)
                }
                None => {
                    tracing::warn!(height = slot.height, "lock=forced_value_unknown");
                    return;
                }
            },
            None => match self.lock_choose(&slot, Some(&promised)) {
                Some((id, change, value)) => (id, wire_change(&change), Some(value)),
                None => return,
            },
        };
        let now = self.presence_now();
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        let Some(value) = run.leader.propose(own) else { return };
        run.value = Some((id, change.clone()));
        run.sent_at = now;
        tracing::info!(height = slot.height, ballot = %fmt_ballot(&ballot), id, forced = forced.is_some(), "lock=propose");
        self.lock_send(WorkspaceEvent::LockAccept {
            height: slot.height,
            base: slot.base.clone(),
            round: ballot.round,
            digest: value.digest.clone(),
            need: need_u64(&value),
            id: ProposalId(id),
            change: change.clone(),
        });
        if self.lock_seam(&self.lock_seams.stop_after_accept) {
            self.lock_seams.stopped.store(true, SeqCst);
            tracing::warn!(height = slot.height, "lock=stopped seam=stop_after_accept");
            return;
        }
        self.lock_acceptor_accept(&slot, &ballot, &value, id, change);
    }

    /// **Phase 2b, the second door**: accept `value` at `ballot` only for a
    /// change this seat approves, at or above its promise, durable before
    /// the signature leaves.
    fn lock_acceptor_accept(&mut self, slot: &Slot, ballot: &Ballot, value: &Value, id: u64, wire: Option<ChainChange>) {
        if self.lock_slot().as_ref() != Some(slot)
            || self.lock_amnesiac_at(slot.height)
            || self.chain.lock.passive.contains(&slot.height)
        {
            return;
        }
        self.lock_try_release(slot.height);
        if self.proposal_change(id).is_none() {
            if let Some(c) = wire.clone() {
                self.lock_register(id, c);
            }
        }
        // W1: a value this seat already accepted may be chosen; accepting it
        // again is a promise kept, not a new vote - no approval gate applies
        let kept = self.chain.lock.acceptor.holds(slot, value);
        let stored = || self.chain.lock.values.get(&value.digest).and_then(|(_, c)| c.clone());
        let Some(change) = self.proposal_change(id).or_else(|| if kept { stored() } else { None }) else {
            tracing::debug!(height = slot.height, id, "lock=withheld reason=unknown");
            // W9: the card may still be on its way
            self.chain.lock.retry_accept = Some((slot.clone(), ballot.clone(), value.clone(), id, wire));
            return;
        };
        if self.lock_value(slot.height, &change).as_ref() != Some(value) {
            tracing::debug!(height = slot.height, id, "lock=withheld reason=value");
            return;
        }
        self.chain.lock.known.insert((slot.height, value.digest.clone()), (id, change.clone()));
        // F1: a restore settles before this seat accepts it, so a withdrawal
        // right behind the card wins here too
        let withheld = if kept {
            None
        } else if matches!(change, ChainChange::Membership { op: MembershipOp::Restored, .. })
            && self.restore_settling(id)
        {
            Some("settling".to_string())
        } else {
            self.lock_withheld(id, &change, slot.height)
        };
        if let Some(why) = withheld {
            tracing::debug!(height = slot.height, id, reason = %why, "lock=withheld");
            self.chain.lock.retry_accept = Some((slot.clone(), ballot.clone(), value.clone(), id, wire_change(&change)));
            return;
        }
        if self.chain.lock.retry_accept.as_ref().is_some_and(|r| r.0 == *slot) {
            self.chain.lock.retry_accept = None;
        }
        // the file keeps the whole change: a withdrawal may remove the card
        let step = self.lock_step(|a| a.accept(slot, ballot, value), Some((value.digest.clone(), id, Some(change.clone()))));
        let Some((reply, changed)) = step else { return };
        match reply {
            Reply::Accepted { .. } => {
                let Some(sk) = self.identity_sk.as_ref() else { return };
                let sig = molt_storage::identity_sign(sk, &approval_bytes(&self.republic_id(), slot.height, &change));
                if changed {
                    tracing::info!(height = slot.height, ballot = %fmt_ballot(ballot), id, "lock=accepted");
                }
                self.lock_send(WorkspaceEvent::LockAccepted {
                    height: slot.height,
                    base: slot.base.clone(),
                    round: ballot.round,
                    leader: ballot.leader.clone(),
                    digest: value.digest.clone(),
                    sig: sig.clone(),
                });
                let me = self.member();
                self.note_signer(&me, slot.height);
                self.lock_count_accepted(&me, slot, ballot, &sig, id, change);
            }
            Reply::Nack { promised, .. } => self.lock_send_nack(slot, ballot, promised),
            Reply::Promise { .. } => {}
        }
    }

    fn lock_send_nack(&mut self, slot: &Slot, ballot: &Ballot, promised: Option<Ballot>) {
        tracing::info!(height = slot.height, ballot = %fmt_ballot(ballot), free = promised.is_none(), "lock=nack");
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.lock_send(WorkspaceEvent::LockNack {
            height: slot.height,
            base: slot.base.clone(),
            round: ballot.round,
            leader: ballot.leader.clone(),
            promised: promised.map(|p| molt_core::chain::LockBallot { round: p.round, leader: p.leader }),
            head,
        });
    }

    /// One acceptance of `change` at `ballot`, signature already verified:
    /// the same-ballot tally, and the seal once it holds `need`.
    fn lock_count_accepted(&mut self, from: &str, slot: &Slot, ballot: &Ballot, sig: &str, id: u64, change: ChainChange) {
        let Some(value) = self.lock_value(slot.height, &change) else { return };
        if self.lock_restore_subjects().contains(from) && matches!(change, ChainChange::Membership { .. }) {
            return;
        }
        self.chain
            .lock
            .sigs
            .entry((slot.clone(), ballot.clone(), value.digest.clone()))
            .or_default()
            .insert(from.to_string(), sig.to_string());
        let Some(sealable) = self.chain.lock.tallies.entry(slot.clone()).or_default().accepted(from, ballot, &value) else {
            return;
        };
        if ballot.leader == self.member() {
            self.lock_seal(slot, &sealable, id, change);
        } else if self.chain.lock.learned.is_none() {
            let since = self.presence_now();
            self.chain.lock.learned = Some(Learned { slot: slot.clone(), sealable, since, id, change });
        }
    }

    /// Seal head+1 from one ballot's acceptances (§3.3: never pooled).
    fn lock_seal(&mut self, slot: &Slot, s: &Sealable, id: u64, change: ChainChange) -> bool {
        if self.lock_seam(&self.lock_seams.stopped) || self.lock_slot().as_ref() != Some(slot) {
            return false;
        }
        if self.restore_seal_held(id, &change) {
            // the tick retries it once the gate opens
            self.chain.lock.learned = Some(Learned { slot: slot.clone(), sealable: s.clone(), since: 0, id, change });
            return false;
        }
        let Some(head) = self.chain.head.clone() else { return false };
        let Some(by) = self.chain.lock.sigs.get(&(slot.clone(), s.ballot.clone(), s.value.digest.clone())) else {
            return false;
        };
        let consented = matches!(change, ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. });
        let subjects = self.lock_restore_subjects();
        let mut sigs: Vec<RosterAttestation> = by
            .iter()
            .filter(|(m, _)| !(consented && subjects.contains(*m)))
            .map(|(m, sig)| RosterAttestation { member: m.clone(), sig: sig.clone() })
            .collect();
        // a consented restore keeps up to m survivor voices beside the
        // consent, like `try_commit`
        sigs.truncate(if consented { usize::from(head.rule_m) } else { s.value.need });
        if sigs.len() < s.value.need {
            return false;
        }
        tracing::info!(height = slot.height, ballot = %fmt_ballot(&s.ballot), id, signers = sigs.len(), "lock=sealed");
        self.chain.lock.learned = None;
        let block = ChainBlock { height: slot.height, prev: head.hash.clone(), change, sigs };
        self.adopt_committed_block(block, id);
        true
    }

    /// Resolve `digest` at `height` to the change behind it.
    fn lock_resolve(&self, height: u64, digest: &str) -> Option<(u64, ChainChange)> {
        if let Some(k) = self.chain.lock.known.get(&(height, digest.to_string())) {
            return Some(k.clone());
        }
        let rid = self.republic_id();
        let mut ids: BTreeSet<u64> = self.proposals.keys().copied().collect();
        ids.extend(self.chain.proposal_changes.keys().copied());
        ids.into_iter().find_map(|id| {
            let c = self.proposal_change(id)?;
            (change_digest(&rid, height, &c) == digest).then_some((id, c))
        })
    }

    // ---- the five frames ----

    pub(crate) fn lock_on_prepare(&mut self, from: &str, height: u64, base: String, round: u64) {
        if !self.lock_listens() || !self.lock_rostered(from) || from == self.member() {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader: from.to_string() };
        self.lock_note_locked(height, from);
        self.lock_saw(&slot, &ballot);
        if self.lock_slot().as_ref() != Some(&slot)
            || self.lock_amnesiac_at(height)
            || self.chain.lock.passive.contains(&height)
        {
            return;
        }
        self.lock_try_release(height);
        let Some((reply, changed)) = self.lock_step(|a| a.prepare(&slot, &ballot), None) else { return };
        match reply {
            Reply::Promise { accepted, .. } => {
                if changed {
                    tracing::info!(height, ballot = %fmt_ballot(&ballot), "lock=promised");
                }
                let accepted = accepted.map(|(b, v)| {
                    let (id, change) = self.chain.lock.values.get(&v.digest).cloned().unwrap_or((0, None));
                    let change = change.as_ref().and_then(wire_change);
                    molt_core::chain::LockAcceptedValue {
                        round: b.round,
                        leader: b.leader,
                        need: need_u64(&v),
                        digest: v.digest,
                        id,
                        change,
                    }
                });
                self.lock_send(WorkspaceEvent::LockPromise {
                    height,
                    base: slot.base.clone(),
                    round,
                    leader: from.to_string(),
                    accepted,
                });
            }
            Reply::Nack { promised, .. } => self.lock_send_nack(&slot, &ballot, promised),
            Reply::Accepted { .. } => {}
        }
    }

    pub(crate) fn lock_on_promise(
        &mut self,
        from: &str,
        height: u64,
        base: String,
        round: u64,
        leader: String,
        accepted: Option<molt_core::chain::LockAcceptedValue>,
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader };
        self.lock_note_locked(height, from);
        self.lock_saw(&slot, &ballot);
        if ballot.leader != self.member() || self.lock_restore_subjects().contains(from) {
            return;
        }
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        if run.leader.slot != slot || run.leader.ballot != ballot {
            return;
        }
        let acc = accepted.map(|a| {
            if a.id != 0 {
                run.ids.insert(a.digest.clone(), (a.id, a.change));
            }
            (
                Ballot { round: a.round, leader: a.leader },
                Value { digest: a.digest, need: usize::try_from(a.need).unwrap_or(usize::MAX) },
            )
        });
        if run.promised.insert(from.to_string()) {
            run.resends = 0;
        }
        run.leader.promise(from, &ballot, acc);
        self.lock_try_propose();
    }

    #[allow(clippy::too_many_arguments)] // one wire frame's fields
    pub(crate) fn lock_on_accept(
        &mut self,
        from: &str,
        height: u64,
        base: String,
        round: u64,
        digest: String,
        need: u64,
        id: u64,
        change: Option<ChainChange>,
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) || from == self.member() {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader: from.to_string() };
        self.lock_note_locked(height, from);
        self.lock_saw(&slot, &ballot);
        let value = Value { digest, need: usize::try_from(need).unwrap_or(usize::MAX) };
        self.lock_acceptor_accept(&slot, &ballot, &value, id, change);
    }

    #[allow(clippy::too_many_arguments)] // one wire frame's fields
    pub(crate) fn lock_on_accepted(
        &mut self,
        from: &str,
        height: u64,
        base: String,
        round: u64,
        leader: String,
        digest: &str,
        sig: &str,
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) || from == self.member() {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader };
        self.lock_note_locked(height, from);
        self.lock_saw(&slot, &ballot);
        if self.lock_slot().as_ref() != Some(&slot) {
            return;
        }
        let Some((id, change)) = self.lock_resolve(height, digest) else { return };
        let Some(head) = self.chain.head.as_ref() else { return };
        let bytes = approval_bytes(&self.republic_id(), height, &change);
        let genuine = head
            .identities
            .iter()
            .any(|i| i.member == from && molt_storage::identity_verify(&i.identity_pk, &bytes, sig));
        if !genuine {
            tracing::warn!(%from, height, "lock=bad_signature");
            return;
        }
        self.chain.lock.known.insert((height, digest.to_string()), (id, change.clone()));
        self.note_signer(from, height);
        self.lock_count_accepted(from, &slot, &ballot, sig, id, change);
    }

    #[allow(clippy::too_many_arguments)] // one wire frame's fields
    pub(crate) fn lock_on_nack(
        &mut self,
        from: &str,
        height: u64,
        base: String,
        round: u64,
        leader: String,
        promised: Option<molt_core::chain::LockBallot>,
        head: u64,
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) {
            return;
        }
        self.note_peer_height(from, head);
        let slot = Slot { height, base };
        match promised {
            Some(p) => {
                self.lock_note_locked(height, from);
                self.lock_saw(&slot, &Ballot { round: p.round, leader: p.leader });
            }
            None => {
                tracing::debug!(%from, height, round, %leader, "lock=nack_free");
                if leader == self.member() {
                    if let Some(run) = self.chain.lock.run.as_mut().filter(|r| r.leader.slot == slot) {
                        run.leader.heard_free(from);
                    }
                }
                self.lock_note_free(from, height);
                self.lock_try_propose();
            }
        }
    }

    // ---- lifecycle ----

    /// The head moved (append, tie-break, reorg, relink, adopt): drop what
    /// it decided, leave amnesia once past the floor, and let the default
    /// leader open the next slot at once (§5).
    pub(crate) fn lock_on_head_moved(&mut self) {
        let Some(head) = self.chain.head.as_ref().map(|h| h.height) else { return };
        let lock = &mut self.chain.lock;
        lock.acceptor.settle(head);
        let held = lock.acceptor.accepted_digests();
        lock.values.retain(|d, _| held.contains(d));
        lock.sigs.retain(|(s, _, _), _| s.height > head);
        lock.tallies.retain(|s, _| s.height > head);
        lock.seen.retain(|s, _| s.height > head);
        lock.known.retain(|(h, _), _| *h > head);
        lock.free_seen.retain(|h, _| *h > head);
        lock.locked_seen.retain(|h, _| *h > head);
        lock.passive.retain(|h| *h > head);
        if lock.learned.as_ref().is_some_and(|l| l.slot.height <= head) {
            lock.learned = None;
        }
        lock.wait = None;
        let open: BTreeSet<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed)
            .map(|(id, _)| *id)
            .chain(self.chain.proposal_changes.keys().copied())
            .collect();
        self.chain.lock.intents.retain(|id, _| open.contains(id));
        self.chain.lock.intent_sent.retain(|id| open.contains(id));
        self.chain.lock.intent_at.retain(|id, _| open.contains(id));
        let slot = self.lock_slot();
        if self.chain.lock.run.as_ref().is_some_and(|r| Some(&r.leader.slot) != slot.as_ref()) {
            self.chain.lock.run = None;
        }
        if let Some(Some(floor)) = self.chain.lock.amnesia {
            if head >= floor {
                self.chain.lock.amnesia = None;
                tracing::info!(head, floor, "lock=amnesia_over");
                self.persist_lock();
            }
        }
        self.lock_drive();
    }

    /// B6's sealing line without a registered restore's subject (§5): its
    /// old device is gone and must not lead.
    fn lock_line(&self, height: u64) -> Vec<String> {
        let subjects = self.lock_restore_subjects();
        self.sealing_line(height).into_iter().filter(|m| !subjects.contains(m)).collect()
    }

    /// Intents changed or the head moved: the leader proposes what became
    /// ready; the default leader opens a ballot when it holds none.
    pub(crate) fn lock_drive(&mut self) {
        if !self.lock_listens() {
            return;
        }
        if self.chain.lock.run.is_some() {
            self.lock_try_propose();
            return;
        }
        let Some(next) = self.chain.head.as_ref().map(|h| h.height + 1) else { return };
        let backing_off = self.chain.lock.wait.as_ref().is_some_and(|w| w.height == next && w.attempt > 0);
        if !backing_off && self.lock_line(next).first() == Some(&self.member()) {
            self.lock_start(0);
        }
    }

    /// §6: a peer reported `height`; the first report after an open
    /// without the lock file fixes the amnesia floor.
    pub(crate) fn lock_note_peer_height(&mut self, height: u64) {
        if self.chain.lock.amnesia == Some(None) {
            let head = self.chain.head.as_ref().map_or(0, |h| h.height);
            let floor = head.max(height) + 1;
            self.chain.lock.amnesia = Some(Some(floor));
            tracing::warn!(floor, "lock=amnesia_floor");
        }
    }

    /// The delivery tick: retransmit, retry a stalled ballot, seal what
    /// another leader left, and take over by rank (§5).
    pub(crate) fn lock_tick(&mut self) {
        if !self.is_chain_governed() {
            return;
        }
        if let Some(next) = self.chain.head.as_ref().map(|h| h.height + 1) {
            self.lock_free_standing_intents(next);
        }
        if !self.lock_listens() {
            return;
        }
        let Some(slot) = self.lock_slot() else { return };
        self.lock_try_release(slot.height);
        if self.lock_mode(slot.height) != LockMode::Lock || self.lock_amnesiac_at(slot.height) {
            return;
        }
        if let Some((s, b, v, id, change)) = self.chain.lock.retry_accept.take() {
            if s == slot {
                self.lock_acceptor_accept(&s, &b, &v, id, change);
            }
        }
        let now = self.presence_now();
        let grace = self.lock_grace();
        if let Some(l) = self.chain.lock.learned.clone() {
            if l.slot == slot && now >= l.since + grace {
                self.lock_seal(&l.slot, &l.sealable, l.id, l.change);
                return;
            }
        }
        if let Some(run) = &self.chain.lock.run {
            // S4/W4: every run goes stale after its backoff, proposed or not;
            // the open phase is re-sent at a doubling interval up to the cap
            let stale = now >= run.started + self.lock_backoff(slot.height, run.attempt);
            let interval = self.lock_retransmit().saturating_mul(1 << run.resends.min(10)).min(LOCK_BACKOFF_MAX_SECS);
            let resend = now >= run.sent_at + interval;
            let attempt = run.attempt;
            // S3: a forced value that waited for a fuller hearing goes out
            // once its interval has passed
            if run.leader.ready() && run.leader.proposed().is_none() && !stale {
                self.lock_try_propose();
            }
            if self.chain.lock.run.is_none() {
                return;
            }
            if stale {
                tracing::info!(height = slot.height, attempt = attempt + 1, "lock=retry");
                self.chain.lock.run = None;
                self.lock_start(attempt + 1);
            } else if resend {
                self.lock_resend(now);
            }
            return;
        }
        let me = self.member();
        let line = self.lock_line(slot.height);
        let rank = u64::try_from(line.iter().position(|m| *m == me).unwrap_or(line.len())).unwrap_or(u64::MAX);
        let ready = self.lock_choose(&slot, None).is_some();
        if rank > 0 && !ready {
            return;
        }
        let w = self.chain.lock.wait.get_or_insert(Wait {
            height: slot.height,
            since: now,
            attempt: 0,
            next_at: 0,
            asked: false,
        });
        if w.height != slot.height {
            *w = Wait { height: slot.height, since: now, attempt: 0, next_at: 0, asked: false };
        }
        let w = w.clone();
        let due = if w.attempt > 0 { w.next_at } else { w.since + rank.saturating_mul(grace) };
        if now < due {
            return;
        }
        if rank > 0 && w.attempt == 0 {
            if !w.asked {
                if let Some(w) = self.chain.lock.wait.as_mut() {
                    w.asked = true;
                }
                tracing::info!(height = slot.height, rank, "lock=takeover_ask");
                self.ask_for_height(slot.height);
                return;
            }
            if now < due + FALLBACK_ASK_SECS.min(grace) {
                return;
            }
        }
        tracing::info!(height = slot.height, rank, attempt = w.attempt, "lock=takeover");
        self.lock_start(w.attempt);
    }

    /// Roster seats whose promise this run still lacks (a restore's subject
    /// and free seats never promise).
    fn lock_missing_promises(&self, run: &Run) -> Vec<String> {
        let subjects = self.lock_restore_subjects();
        let free = self.chain.lock.free_seen.get(&run.leader.slot.height);
        self.chain
            .head
            .as_ref()
            .map(|h| {
                h.identities
                    .iter()
                    .map(|i| i.member.clone())
                    .filter(|m| !run.promised.contains(m) && !subjects.contains(m) && !free.is_some_and(|f| f.contains(m)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn lock_resend(&mut self, now: u64) {
        if let Some(run) = self.chain.lock.run.as_ref().filter(|r| !r.leader.ready()) {
            let missing = self.lock_missing_promises(run).join(",");
            let height = run.leader.slot.height;
            // under the lock a restore needs n - m + 1 promises (§4.2)
            if self.lock_restore_subjects().is_empty() {
                tracing::debug!(height, %missing, "lock=wait");
            } else {
                tracing::info!(height, %missing, reason = "restore_needs_promises", "lock=wait");
            }
        }
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        run.sent_at = now;
        run.resends = run.resends.saturating_add(1);
        let (slot, ballot) = (run.leader.slot.clone(), run.leader.ballot.clone());
        let body = match (run.leader.proposed().cloned(), run.value.clone()) {
            (Some(v), Some((id, change))) => WorkspaceEvent::LockAccept {
                height: slot.height,
                base: slot.base.clone(),
                round: ballot.round,
                need: need_u64(&v),
                digest: v.digest,
                id: ProposalId(id),
                change,
            },
            // phase 1, ready or not: a missing promise may still free a
            // forced value or complete a cut's hearing (S3, S4)
            _ => WorkspaceEvent::LockPrepare { height: slot.height, base: slot.base.clone(), round: ballot.round },
        };
        tracing::debug!(height = slot.height, ballot = %fmt_ballot(&ballot), "lock=resend");
        self.lock_send(body);
    }

    /// Adopt the stored lock state at open (§6). No file where one was
    /// kept (`marker`), or an unreadable one: amnesia. No file and none
    /// kept: a seat that never ran the lock - its own signatures at head+1
    /// are marked free first.
    pub(crate) fn load_chain_lock(&mut self, stored: Result<Option<Vec<u8>>, String>, marker: bool) {
        // the replay already rebuilt the intents; only the durable part loads here
        self.chain.lock.acceptor = Acceptor::default();
        self.chain.lock.values.clear();
        self.chain.lock.amnesia = None;
        let parsed = match stored {
            Ok(Some(bytes)) => serde_json::from_slice::<LockFile>(&bytes).map(Some).map_err(|e| e.to_string()),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        let on = self.lock_cap_on() && self.lock_enabled();
        match parsed {
            Ok(Some(file)) => {
                self.chain.lock.acceptor = file.acceptor;
                self.chain.lock.values = file.values;
            }
            Ok(None) if marker && on => {
                tracing::warn!("lock=amnesia reason=no_file");
                self.chain.lock.amnesia = Some(None);
                return;
            }
            Ok(None) => {}
            Err(e) if on => {
                tracing::warn!(error = %e, "lock=amnesia reason=unreadable");
                self.chain.lock.amnesia = Some(None);
                return;
            }
            Err(_) => {}
        }
        if !on {
            return;
        }
        // own replayed signatures were free ones (a build without the lock
        // ran here, or none ever wrote the file): mark them first
        let me = self.member();
        let own: Vec<u64> = self
            .chain
            .pending_sigs
            .values()
            .filter(|p| p.sigs.iter().any(|a| a.member == me))
            .map(|p| p.height)
            .collect();
        let before = self.chain.lock.acceptor.clone();
        for h in own {
            if self.chain.lock.acceptor.sign_freely(h).is_none() {
                tracing::warn!(height = h, "lock=passive reason=conflict");
                self.chain.lock.passive.insert(h);
            }
        }
        if self.is_chain_governed() && (self.chain.lock.acceptor != before || !marker) && !self.persist_lock() {
            // W6: marks that never reached the disk would let a free
            // signature out unwritten
            self.chain.lock.acceptor = before;
            self.chain.lock.amnesia = Some(None);
            tracing::warn!("lock=amnesia reason=unwritten");
        }
    }

    /// `(lock active at head+1, highest ballot seen there, why it waits)`.
    pub(crate) fn lock_status(&self) -> (bool, String, String) {
        let Some(head) = self.chain.head.as_ref() else {
            return (false, String::new(), String::new());
        };
        let next = head.height + 1;
        let active = self.lock_cap_on() && self.lock_mode(next) == LockMode::Lock;
        let ballot = self
            .chain
            .lock
            .seen
            .iter()
            .filter(|(s, _)| s.height == next)
            .map(|(_, b)| b)
            .max()
            .map(fmt_ballot)
            .unwrap_or_default();
        if !active {
            return (false, ballot, String::new());
        }
        let wait = if self.lock_amnesiac_at(next) {
            "amnesia".to_string()
        } else if let Some(run) = &self.chain.lock.run {
            let q = run.leader.ballot.clone();
            match run.leader.proposed() {
                None if !run.leader.ready() => {
                    let want = self.lock_quorum().map_or(0, Quorum::promises);
                    let missing = self.lock_missing_promises(run).join(",");
                    format!("promises {}/{want} missing {missing}", run.promised.len())
                }
                None => "no value ready".to_string(),
                Some(v) => {
                    let have = self
                        .chain
                        .lock
                        .sigs
                        .get(&(run.leader.slot.clone(), q, v.digest.clone()))
                        .map_or(0, BTreeMap::len);
                    format!("acceptances {have}/{}", v.need)
                }
            }
        } else if let Some(b) = self.chain.lock.seen.iter().filter(|(s, _)| s.height == next).map(|(_, b)| b).max() {
            format!("leader {}", b.leader)
        } else {
            String::new()
        };
        (true, ballot, wait)
    }
}

#[cfg(test)]
#[path = "lock_wire_tests.rs"]
mod tests;
