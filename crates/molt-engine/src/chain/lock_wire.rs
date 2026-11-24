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
use super::lock::{amnesia_floor, amnesia_need, amnesia_touched, evidence_holds, Acceptor, Ballot, Leader, Quorum, Reply, Sealable, Slot, Step, Subjects, Tally, Value, SLOT_KEEP};
use molt_core::chain::LockEvidence;
use super::*;

/// The enabling vote waits: a seat runs a build without the lock.
pub(crate) const LOCK_NOT_EVERYWHERE: &str = "a seat runs a build without the lock";

/// The chain lock ships DARK (not available in release builds) pending
/// review: off, a founding ratifies no `chain-lock`, a `set_features`
/// naming it is refused, the capability is not announced, and a chain that
/// carries the key is not signed on. A test seam turns it on per engine.
pub(crate) const CHAIN_LOCK_AVAILABLE: bool = false;

/// The refusal while the lock is dark.
pub(crate) const LOCK_NOT_AVAILABLE: &str = "Strict Order Mode is not available in this build";

/// Strict Order Mode needs at least two approvals per decision (A-F4).
pub(crate) const LOCK_NEEDS_TWO: &str = "Strict Order Mode needs at least 2 approvals per decision";

/// Not offered where m = n: a seat lost mid-height would pin it for good.
pub(crate) const LOCK_NEEDS_FEWER: &str = "Strict Order Mode needs fewer approvals per decision than members";

/// The enabling vote waits while a restore is open (item 7).
pub(crate) const LOCK_RESTORE_PENDING: &str = "Strict Order Mode waits for a pending restore";

/// F3: no honest ballot comes near this round - rounds start near 1 per
/// slot and grow by one per attempt above the highest seen (2^40 attempts
/// at the 10 s minimum backoff are some 350 000 years). A peer frame
/// above it is refused, so `+ 1` on a seen round never overflows and no
/// peer pins a slot at `u64::MAX`.
pub(crate) const ROUND_MAX: u64 = 1 << 40;

/// A forced value withheld for want of evidence (item 3): log reason.
const LOCK_NEEDS_APPROVAL: &str = "needs approval";

/// Why this build signs nothing on a chain that runs the lock.
pub(crate) const LOCK_CHAIN_UNSUPPORTED: &str = "this republic uses Strict Order Mode - needs a newer build";

/// A leader re-sends its open phase this often (§7).
pub(crate) const LOCK_RETRANSMIT_SECS: u64 = 15;
/// The cap of a leader's retry backoff (§5).
const LOCK_BACKOFF_MAX_SECS: u64 = 600;

/// Test seams of the lock (never a `Command`).
#[derive(Default)]
pub(crate) struct LockSeams {
    /// 0 = this build ([`CHAIN_LOCK_AVAILABLE`]), 1 = the lock on (the
    /// test seam), 2 = an older build (no capability, every lock frame
    /// ignored, free signatures only).
    pub(crate) cap: std::sync::atomic::AtomicU8,
    /// The next founding ratifies the lock in its charter (test only: a
    /// founding starts without it, §11).
    pub(crate) found_locked: AtomicBool,
    /// Takeover grace in seconds, 0 = [`SEALER_GRACE_SECS`].
    pub(crate) grace_secs: AtomicU64,
    /// Retransmit interval in seconds, 0 = [`LOCK_RETRANSMIT_SECS`].
    pub(crate) retransmit_secs: AtomicU64,
    /// The next leader to send a `LockAccept` stops right after it.
    pub(crate) stop_after_accept: AtomicBool,
    /// The same, after its own acceptance went out too.
    pub(crate) stop_after_own_accept: AtomicBool,
    /// Lock activity of this seat has stopped (a killed leader).
    pub(crate) stopped: AtomicBool,
    /// The enabling vote ignores an open restore card (item 7's race,
    /// staged in one step).
    pub(crate) ignore_open_restore: AtomicBool,
    /// Free-mode restore seals wait (a slow sealer, staged).
    pub(crate) hold_restore_seals: AtomicBool,
}

/// What a seat initiates at a height (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockMode {
    Lock,
    Free,
}

/// `chain_lock.state`: the acceptor, and the id behind each digest it
/// accepted (plus the whole change at an open slot), so a promise after a
/// restart still names what it holds.
#[derive(Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    acceptor: Acceptor,
    #[serde(default)]
    values: BTreeMap<String, (u64, Option<ChainChange>)>,
    /// The height up to which slots were forgotten (B-F1): a file whose
    /// floor is above the chain on disk forgot what the chain still needs.
    #[serde(default)]
    floor: u64,
}

/// Intents kept for cards not yet seen (R8); the oldest ids go first.
const LOCK_EARLY_INTENTS_MAX: usize = 1024;

/// Item 3's exit, staged: `since` the first refusal of the forced value
/// `id` at `height`, `waiting` for the seats whose acceptance would vouch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NeedsApproval {
    pub(crate) height: u64,
    pub(crate) id: u64,
    pub(crate) since: u64,
    pub(crate) waiting: Vec<String>,
    /// The leader that sent it: its evidence already says what it vouches
    /// for, so a wait on it alone never holds the offer back.
    pub(crate) sender: String,
}

/// Answers to an amnesiac seat's ask, by seat: (touched, round).
type AmnesiaAnswers = BTreeMap<String, (u64, u64)>;

/// A withheld acceptance to retry: slot, ballot, value, id, the carried
/// change, and `Some(evidence)` where the value was forced.
type RetryAccept = (Slot, Ballot, Value, u64, Option<ChainChange>, Option<Vec<LockEvidence>>);

/// One ballot this seat leads.
struct Run {
    leader: Leader,
    promised: BTreeSet<String>,
    /// digest -> (id, change) as the promises named them.
    ids: BTreeMap<String, (u64, Option<ChainChange>)>,
    /// The proposed value's id and wire change.
    value: Option<(u64, Option<ChainChange>)>,
    /// Phase 1 forced the proposed value.
    forced: bool,
    /// digest -> another seat's acceptance a promise reported (item 3).
    evidence: BTreeMap<String, Vec<LockEvidence>>,
    /// The evidence the proposed forced value carries.
    sent_evidence: Vec<LockEvidence>,
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
    retry_accept: Option<RetryAccept>,
    /// A forced value withheld for want of evidence. The status first
    /// names the seats whose acceptance would complete it; after one
    /// retry interval it asks for an approve (item 3's exit).
    pub(crate) needs_approval: Option<NeedsApproval>,
    /// An amnesiac seat's open ask: (nonce, sent at, answers by seat as
    /// (touched, round)).
    amnesia_ask: Option<(u64, u64, AmnesiaAnswers)>,
    /// This seat leads above this round (the highest its answers saw).
    pub(crate) amnesia_round: u64,
    /// The highest round seen from each leader (what this seat answers).
    rounds_by: BTreeMap<String, u64>,
    /// Other seats seen asking (amnesiac themselves: they answer nothing).
    amnesiac_others: BTreeSet<String>,
    /// Ids approved through that exit.
    approved_forced: BTreeSet<u64>,
    /// Seats seen with lock state at a height (a promise, an acceptance,
    /// a leader's frame): never counted free there - a seat that later
    /// signs freely at that height was downgraded, not free (S2).
    locked_seen: BTreeMap<u64, BTreeSet<String>>,
    /// Cards proposed freely at a height whose ballot went stale there,
    /// longest stale first: skipped while another card is ready, then
    /// taken in turn (R5, T2).
    stale_ids: Vec<u64>,
    /// The last withheld acceptance logged at INFO (C-MED-6, once each).
    withheld_logged: Option<(u64, u64, String)>,
    /// The seats among them seen holding an acceptance there.
    accepted_seen: BTreeMap<u64, BTreeSet<String>>,
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
    /// Test seam: the chain write fails (ENOSPC, a crash before it).
    #[cfg(test)]
    pub(crate) fail_chain_writes: bool,
    /// The chain height known on disk (B-F1): nothing above it is forgotten.
    pub(crate) durable_head: u64,
    /// The height up to which slots were forgotten (persisted).
    floor: u64,
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
/// The seat a restore re-admits: never a voice for its own restore.
fn restored_member(change: &ChainChange) -> Option<&str> {
    match change {
        ChainChange::Membership { op: MembershipOp::Restored, member, .. } => Some(member),
        _ => None,
    }
}

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
    pub(crate) fn lock_seam(&self, flag: &AtomicBool) -> bool {
        flag.load(SeqCst)
    }

    /// This build runs the lock (an older-build seam turns it off).
    pub(crate) fn lock_cap_on(&self) -> bool {
        match self.lock_seams.cap.load(SeqCst) {
            1 => true,
            2 => false,
            _ => CHAIN_LOCK_AVAILABLE,
        }
    }

    /// G: this build cannot take part in a chain that runs the lock - it
    /// signs nothing there (like a reader meeting a newer chain).
    pub(crate) fn lock_chain_unsupported(&self) -> bool {
        self.lock_seams.cap.load(SeqCst) == 0 && !CHAIN_LOCK_AVAILABLE && self.lock_enabled()
    }

    #[cfg(test)]
    pub(crate) fn lock_force(&self, on: bool) {
        self.lock_seams.cap.store(if on { 1 } else { 2 }, SeqCst);
    }

    fn lock_listens(&self) -> bool {
        self.is_chain_governed() && self.lock_cap_on() && !self.lock_seam(&self.lock_seams.stopped)
    }

    /// The lock's clock (B-F7): monotonic, so a wall-clock jump neither
    /// stalls a deadline nor fires every one at once; the test clock
    /// drives it like presence.
    pub(crate) fn lock_now(&self) -> u64 {
        static START: std::sync::OnceLock<(std::time::Instant, u64)> = std::sync::OnceLock::new();
        self.presence.clock_override.unwrap_or_else(|| {
            let (at, wall) = START.get_or_init(|| (std::time::Instant::now(), crate::now_secs()));
            wall + at.elapsed().as_secs()
        })
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
                q.restore_need()
            }
            _ => q.m,
        };
        Some(Value { digest: change_digest(&self.republic_id(), height, change), need })
    }

    /// Seats that are the subject of some restore here (see
    /// [`Self::lock_subjects`]).
    fn lock_restore_subjects(&self) -> BTreeSet<String> {
        self.lock_subjects().seats()
    }

    /// The consented restores that may still be chosen at head+1, as this
    /// seat sees them: an OPEN card (a declined or withdrawn one no
    /// longer counts, C-HIGH-1; where m < n also not one the
    /// seat-still-active gate holds - its subject is alive), and every
    /// restore value the lock itself holds at head+1 (R2: chosen values
    /// outlive their card).
    pub(crate) fn lock_subjects(&self) -> Subjects {
        let mut out = Subjects::default();
        let Some(next) = self.chain.head.as_ref().map(|h| h.height + 1) else { return out };
        let rid = self.republic_id();
        let strict = self.lock_quorum().is_some_and(|q| q.m >= q.n);
        let me = self.member();
        for (id, change) in &self.chain.proposal_changes {
            let ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } = change else {
                continue;
            };
            let open = self.proposals.get(id).map_or(true, |p| p.state == ProposalState::Proposed && !p.withdrawn);
            // in its own view a seat is always "active": it stays a subject
            let alive = !strict
                && *member != me
                && self.restore_gate(member) == Some(super::membership::SEAT_STILL_ACTIVE);
            if !open || alive {
                continue;
            }
            out.insert(change_digest(&rid, next, change), member.clone());
        }
        let lock = &self.chain.lock;
        let held = lock
            .values
            .iter()
            .filter_map(|(d, (_, c))| Some((d, c.as_ref()?)))
            .chain(lock.run.iter().flat_map(|r| r.ids.iter().filter_map(|(d, (_, c))| Some((d, c.as_ref()?)))))
            .chain(lock.known.iter().filter(|((h, _), _)| *h == next).map(|((_, d), (_, c))| (d, c)));
        for (digest, change) in held {
            if let ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } = change {
                if change_digest(&rid, next, change) == *digest {
                    out.insert(digest.clone(), member.clone());
                }
            }
        }
        out
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
    /// The first seat that keeps the lock off, and why (R9): this seat's
    /// own build, a seat announced without the lock, or one never heard.
    pub(crate) fn lock_not_capable(&self) -> Option<String> {
        // D2: a build without the mode says so first, whatever the republic
        if !self.lock_cap_on() {
            return Some(LOCK_NOT_AVAILABLE.to_string());
        }
        // A-F4: with m = 1 phase 1 would need every seat
        if self.lock_quorum().is_some_and(|q| q.m < 2) {
            return Some(LOCK_NEEDS_TWO.to_string());
        }
        // m = n: a seat lost between the others' acceptance and its own
        // pins the height for good (it may have sealed alone), and amnesia
        // has no safe return there
        if self.lock_quorum().is_some_and(|q| q.m >= q.n) {
            return Some(LOCK_NEEDS_FEWER.to_string());
        }
        // item 7: a restore in flight may welcome a device without the mode
        let restoring = self.chain.proposal_changes.iter().any(|(id, c)| {
            matches!(c, ChainChange::Membership { op: MembershipOp::Restored, .. })
                && self.proposals.get(id).map_or(true, |p| p.state == ProposalState::Proposed && !p.withdrawn)
        });
        if restoring && !self.lock_seam(&self.lock_seams.ignore_open_restore) {
            return Some(LOCK_RESTORE_PENDING.to_string());
        }
        #[cfg(test)]
        if let Some(all) = self.chain.lock.caps_override {
            return (!all).then(|| LOCK_NOT_EVERYWHERE.to_string());
        }
        let me = self.member();
        let cap = molt_core::chain::CHAIN_LOCK_CAP;
        let known = self.active.as_ref().map(|a| &a.prefs.seat_caps);
        let Some(head) = self.chain.head.as_ref() else {
            return Some(LOCK_NOT_EVERYWHERE.to_string());
        };
        head.identities.iter().filter(|i| i.member != me).find_map(|i| match known.and_then(|k| k.get(&i.member)) {
            None => Some(format!("{} has not announced", i.member)),
            Some(c) if !c.iter().any(|c| c == cap) => Some(format!("{} runs a build without the lock", i.member)),
            Some(_) => None,
        })
    }

    /// Why this seat neither proposes nor approves enabling the lock now:
    /// a seat it knows of runs a build without it.
    pub(crate) fn lock_enable_blocker(&self, change: &ChainChange) -> Option<String> {
        let ChainChange::Applied { surface: Surface::Organization, payload, .. } = change else {
            return None;
        };
        let enables = payload.get("op").and_then(serde_json::Value::as_str) == Some("set_features")
            && payload
                .get("value")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|v| v.split_whitespace().any(|k| k == molt_core::chain::CHAIN_LOCK_FEATURE));
        if !enables {
            return None;
        }
        self.lock_not_capable()
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
        self.lock_known_free(height).len()
    }

    /// The other seats known free at `height`. A seat seen with lock state
    /// there counts only as a sound release (S2/R4): one seen merely
    /// promising once the others known free kill the lock for a seat
    /// holding nothing; one seen accepting never.
    fn lock_known_free(&self, height: u64) -> Vec<String> {
        let me = self.member();
        let subjects = self.lock_restore_subjects();
        let lock = &self.chain.lock;
        let Some(seen) = lock.free_seen.get(&height) else { return Vec::new() };
        let locked = lock.locked_seen.get(&height);
        let accepted = lock.accepted_seen.get(&height);
        let (mut free, mut cascade): (Vec<String>, Vec<String>) = seen
            .iter()
            .filter(|m| **m != me && !subjects.contains(*m) && !accepted.is_some_and(|a| a.contains(*m)))
            .cloned()
            .partition(|m| !locked.is_some_and(|l| l.contains(m)));
        let Some(q) = self.lock_quorum() else { return free };
        if q.lock_dead(free.len(), false) {
            free.append(&mut cascade);
        }
        free
    }

    /// §14: the free signers at `height` leave the lock no decision there
    /// once this seat leaves too ([`Quorum::lock_dead`]).
    fn lock_dead_at(&self, height: u64) -> bool {
        let Some(q) = self.lock_quorum() else { return false };
        q.lock_dead(self.lock_free_count(height), self.chain.lock.acceptor.holds_acceptance(height))
    }

    /// `member` signed freely at `height` (verified, or its free `Nack`).
    pub(crate) fn lock_note_free(&mut self, member: &str, height: u64) {
        // R3: a chain without the lock keeps no lock state (W7)
        if !self.lock_cap_on() || !self.lock_enabled() || !self.lock_rostered(member) || member == self.member() {
            return;
        }
        // F4: only head+1 is contended; a far height would poison the map
        if !self.lock_at_next(height) {
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
            || !self.lock_enabled()
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
        let file = LockFile {
            acceptor: self.chain.lock.acceptor.clone(),
            values: self.chain.lock.values.clone(),
            floor: self.chain.lock.floor,
        };
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
        if !self.lock_cap_on() {
            return true;
        }
        // A-F3: lock state at this height on any lineage forbids it - the
        // signature is base-agnostic and would count free where it holds
        let a = &self.chain.lock.acceptor;
        if !a.may_sign_freely(height) && !a.signed_freely(height) {
            tracing::warn!(height, "lock=refused_free reason=lock_state");
            return false;
        }
        // W7: no free mark, no amnesia where the chain never enabled the lock
        if !self.lock_enabled() {
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
        let order = self.chain.lock.stale_ids.clone();
        let stale: BTreeSet<u64> = order.iter().copied().collect();
        // R5: a card whose ballot went stale waits while another is ready;
        // once every ready card went stale they take turns (T2)
        self.lock_choose_skipping(slot, promised, Some(&stale)).or_else(|| {
            order.iter().find_map(|id| {
                let others: BTreeSet<u64> = stale.iter().copied().filter(|x| x != id).collect();
                self.lock_choose_skipping(slot, promised, Some(&others))
            })
        })
    }

    fn lock_choose_skipping(
        &self,
        slot: &Slot,
        promised: Option<&BTreeSet<String>>,
        skip: Option<&BTreeSet<u64>>,
    ) -> Option<(u64, ChainChange, Value)> {
        let q = self.lock_quorum()?;
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
            if skip.is_some_and(|s| s.contains(&id))
                || self.proposals.get(&id).is_some_and(|p| p.state != ProposalState::Proposed)
            {
                continue;
            }
            // W5: the cheap count first; the change and its digest only for
            // a card that has the voices
            let decliners: Vec<String> = self.proposals.get(&id).map(|p| p.decliners.clone()).unwrap_or_default();
            let voices = self.chain.lock.intents.get(&id).map_or(0, |s| {
                s.iter()
                    .filter(|m| !decliners.contains(m) && !free.contains(*m) && self.lock_rostered(m))
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
                if upto.checked_add(1) != Some(slot.height) || promised.is_some_and(|p| p.len() < q.n) {
                    continue;
                }
            }
            // W2: a card every seat would refuse now must not starve the
            // height while another one is ready
            if self.lock_gate(id, &change, slot.height, false).is_some() || !self.restore_settled(id, &change) {
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
        self.lock_gate(id, change, height, true)
    }

    /// The gates on `change`: the chain-wide ones (relay wait, restore gate
    /// and consent, the cut's hash) a leader never proposes past (W2); with
    /// `local` also this seat's own vault/wallet checks, which only stop
    /// its acceptance - another seat may still accept the card.
    fn lock_gate(&self, id: u64, change: &ChainChange, height: u64, local: bool) -> Option<String> {
        if let Some(why) = self.sign_blocker(id) {
            return Some(why);
        }
        match change {
            ChainChange::Applied { surface: Surface::Vault, payload, .. } if local => {
                if let Err(e) = self.vault_approve_check(payload) {
                    return Some(e.to_string());
                }
            }
            ChainChange::Applied { surface: Surface::Wallet, payload, .. } if local => {
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
                    if upto.checked_add(1) != Some(height) {
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

    /// F4: `height` is head+1, the only height lock state is kept for.
    fn lock_at_next(&self, height: u64) -> bool {
        self.chain.head.as_ref().and_then(|h| h.height.checked_add(1)) == Some(height)
    }

    /// `member` holds lock state at `height` (it sent a lock frame there).
    fn lock_note_locked(&mut self, height: u64, member: &str) {
        self.chain.lock.locked_seen.entry(height).or_default().insert(member.to_string());
    }

    fn lock_note_accepting(&mut self, height: u64, member: &str) {
        self.lock_note_locked(height, member);
        self.chain.lock.accepted_seen.entry(height).or_default().insert(member.to_string());
    }

    /// Record a ballot seen at `slot`; a higher foreign one stops this
    /// seat's own run there and restarts the takeover wait.
    fn lock_saw(&mut self, slot: &Slot, ballot: &Ballot) {
        if self.chain.lock.seen.get(slot).map_or(true, |b| ballot > b) {
            self.chain.lock.seen.insert(slot.clone(), ballot.clone());
        }
        let r = self.chain.lock.rounds_by.entry(ballot.leader.clone()).or_default();
        *r = (*r).max(ballot.round);
        let me = self.member();
        if ballot.leader == me {
            return;
        }
        let now = self.lock_now();
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
        // H2: a seat that is a restore's subject in its own view never leads
        if self.lock_restore_subjects().contains(&me) {
            return;
        }
        let now = self.lock_now();
        let seen = self.chain.lock.seen.get(&slot).map_or(0, |b| b.round);
        let round = self
            .chain
            .lock
            .acceptor
            .next_round(&slot)
            .max(seen.saturating_add(1))
            .max(self.chain.lock.amnesia_round.saturating_add(1));
        let ballot = Ballot { round, leader: me.clone() };
        let Some((Reply::Promise { accepted, .. }, _)) = self.lock_step(|a| a.prepare(&slot, &ballot), None) else {
            return;
        };
        let mut run = Run {
            leader: Leader::new(slot.clone(), q, ballot.clone()),
            promised: BTreeSet::from([me.clone()]),
            ids: BTreeMap::new(),
            value: None,
            forced: false,
            evidence: BTreeMap::new(),
            sent_evidence: Vec::new(),
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
        // S5: seats known free here hold nothing that can be chosen
        for m in self.lock_known_free(slot.height) {
            run.leader.heard_free(&m);
        }
        self.chain.lock.seen.insert(slot.clone(), ballot.clone());
        self.chain.lock.run = Some(run);
        self.lock_excuse_subjects();
        self.chain.lock.wait = None;
        tracing::info!(height = slot.height, ballot = %fmt_ballot(&ballot), attempt, "lock=prepare");
        self.lock_send(WorkspaceEvent::LockPrepare { height: slot.height, base: slot.base.clone(), round });
        self.lock_try_propose();
    }

    /// §4.2 per value (R2, T1): a consented restore's subject can never
    /// hold that restore, so it is no unheard voice for it - and for that
    /// value only: its old device may be back and accept any other.
    fn lock_excuse_subjects(&mut self) {
        let rid = self.republic_id();
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        let height = run.leader.slot.height;
        let restores: Vec<(String, String)> = run
            .ids
            .iter()
            .filter_map(|(digest, (_, change))| match change.as_ref()? {
                c @ ChainChange::Membership { op: MembershipOp::Restored, member, consent: Some(_), .. } => {
                    (change_digest(&rid, height, c) == *digest).then(|| (digest.clone(), member.clone()))
                }
                _ => None,
            })
            .collect();
        for (digest, member) in restores {
            run.leader.excuse(&digest, &member);
        }
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
                        self.chain.lock.run.as_ref().map_or((0, 0), |r| (r.leader.heard(), r.started));
                    let waiting = self.lock_now() < started + self.lock_retransmit();
                    if heard < n && waiting {
                        tracing::debug!(height = slot.height, id, heard, "lock=forced_waits_for_hearing");
                        return;
                    }
                    // C-MED-5: the whole change rides a forced Accept
                    let change = change.or_else(|| self.proposal_change(id));
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
        let now = self.lock_now();
        // item 3: the acceptances behind a forced value ride it - the
        // promises', the frames seen, and the leader's own where it
        // accepts the value itself (it signs only what it accepts)
        let evidence = match &forced {
            Some(v) => self.lock_forced_evidence(&slot, &ballot, v, id, change.as_ref()),
            None => Vec::new(),
        };
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        let Some(value) = run.leader.propose(own) else { return };
        run.value = Some((id, change.clone()));
        run.forced = forced.is_some();
        run.sent_at = now;
        run.sent_evidence = evidence.clone();
        let is_forced = run.forced;
        tracing::info!(height = slot.height, ballot = %fmt_ballot(&ballot), id, forced = is_forced, "lock=propose");
        self.lock_send(WorkspaceEvent::LockAccept {
            height: slot.height,
            base: slot.base.clone(),
            round: ballot.round,
            digest: value.digest.clone(),
            need: need_u64(&value),
            id: ProposalId(id),
            change: change.clone(),
            forced: is_forced,
            evidence: evidence.clone(),
        });
        if self.lock_seam(&self.lock_seams.stop_after_accept) {
            self.lock_seams.stopped.store(true, SeqCst);
            tracing::warn!(height = slot.height, "lock=stopped seam=stop_after_accept");
            return;
        }
        self.lock_acceptor_accept(&slot, &ballot, &value, id, change, is_forced.then_some(evidence));
        if self.lock_seam(&self.lock_seams.stop_after_own_accept) {
            self.lock_seams.stopped.store(true, SeqCst);
            tracing::warn!(height = slot.height, "lock=stopped seam=stop_after_own_accept");
        }
    }

    /// What a seat checks before signing a forced value it may never have
    /// approved (M4): the change it signs is the one carried, bound to the
    /// digest by the caller; here the checks without which the block would
    /// not verify or apply, the same an incoming card meets
    /// (`receive_proposed`): no reserved op, the vault and wallet wire
    /// shapes, the feature-set refusals, no other change under the id; a
    /// valid restore consent at the height; a cut of this height with this
    /// seat's state hash; any other membership change only as registered
    /// here. The withdrawal, the settle, the seat-still-active gate and
    /// the local vault/wallet checks do not hold.
    fn lock_forced_invalid(&self, id: u64, change: &ChainChange, height: u64) -> Option<String> {
        if self.identity_sk.is_none() {
            return Some("no signing key".into());
        }
        if self.lock_consumed(change) {
            return Some("already applied".into());
        }
        match change {
            ChainChange::Applied { proposal_id, surface, payload } => {
                if *proposal_id != id || super::is_base_entry_op(payload) {
                    return Some("invalid change".into());
                }
                if self.proposals.get(&id).is_some_and(|p| p.surface != *surface || p.payload != *payload) {
                    return Some("another change under the id".into());
                }
                if self.vault_enable_refusal(*surface, payload).is_some()
                    || self.wallet_feature_refusal(*surface, payload).is_some()
                    || (*surface == Surface::Vault && self.vault_wire_check(id, payload).is_err())
                    || (*surface == Surface::Wallet && !crate::wallet::purse_op_ok(payload))
                {
                    return Some("invalid change".into());
                }
            }
            ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. } => {}
            ChainChange::Membership { .. } if self.chain.proposal_changes.get(&id) != Some(change) => {
                return Some("unregistered membership change".into());
            }
            _ => {}
        }
        if matches!(change, ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. }) {
            if let Err(why) = self.restore_consent_at(change, height) {
                return Some(why.to_string());
            }
        }
        if let Some((upto, hash, kind)) = CutKind::of(change) {
            if upto.checked_add(1) != Some(height) || self.own_cut_hash(upto, kind).ok().as_deref() != Some(hash) {
                return Some("cut mismatch".into());
            }
        }
        None
    }

    /// F2: an `Applied` change whose proposal id this chain already
    /// applied (the walk's double-apply guard): never accepted, never a
    /// reported value - every seal of it would fail and pin the height.
    fn lock_consumed(&self, change: &ChainChange) -> bool {
        match change {
            ChainChange::Applied { proposal_id, .. } => self.chain.walk.as_ref().is_some_and(|w| w.seen.contains(proposal_id)),
            _ => false,
        }
    }

    /// **Phase 2b, the second door**: accept `value` at `ballot` only for a
    /// change this seat approves, at or above its promise, durable before
    /// the signature leaves.
    fn lock_acceptor_accept(
        &mut self,
        slot: &Slot,
        ballot: &Ballot,
        value: &Value,
        id: u64,
        wire: Option<ChainChange>,
        // `Some(evidence)`: phase 1 forced the leader to this value (item 3)
        forced: Option<Vec<LockEvidence>>,
    ) {
        let (forced, evidence) = (forced.is_some(), forced.unwrap_or_default());
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
        // H1: a forced value may already be chosen - any promiser completes
        // it, its approval or not; validity gates only
        let forced_path = forced && !kept;
        let stored = || self.chain.lock.values.get(&value.digest).and_then(|(_, c)| c.clone());
        let resolved = if forced_path {
            wire.clone().or_else(|| self.proposal_change(id))
        } else {
            self.proposal_change(id).or_else(|| if kept { stored() } else { None })
        };
        let Some(change) = resolved else {
            tracing::debug!(height = slot.height, id, "lock=withheld reason=unknown");
            // W9: the card may still be on its way
            self.chain.lock.retry_accept = Some((slot.clone(), ballot.clone(), value.clone(), id, wire, forced.then_some(evidence)));
            return;
        };
        if self.lock_value(slot.height, &change).as_ref() != Some(value) {
            tracing::debug!(height = slot.height, id, "lock=withheld reason=value");
            return;
        }
        self.chain.lock.known.insert((slot.height, value.digest.clone()), (id, change.clone()));
        // F1: a restore settles before this seat accepts it, so a withdrawal
        // right behind the card wins here too
        // a seat never accepts its own restore, on any path (the count and
        // the seal would filter it, but no filter stands in for a refusal)
        let own = restored_member(&change) == Some(self.member().as_str());
        let withheld = if own {
            Some("own restore".to_string())
        } else if self.lock_consumed(&change) {
            Some("already applied".to_string())
        } else if kept {
            None
        } else if forced_path {
            if self.lock_forced_vouched(slot.height, &ballot.leader, id, &change, &evidence) {
                self.lock_forced_invalid(id, &change, slot.height)
            } else {
                // the exit: the status asks this seat's human to approve -
                // a card it never had is registered first, so the human
                // reads what it is asked to complete
                self.lock_register_completion(id, &change, &ballot.leader);
                let me = self.member();
                let subjects = self.lock_restore_subjects();
                let waiting: Vec<String> = self
                    .roster()
                    .into_iter()
                    .filter(|m| *m != me && !subjects.contains(m) && !evidence.iter().any(|e| e.member == *m))
                    .collect();
                let now = self.lock_now();
                let same = self.chain.lock.needs_approval.as_ref().filter(|n| n.height == slot.height && n.id == id);
                let since = same.map_or(now, |n| n.since);
                let sender = ballot.leader.clone();
                self.chain.lock.needs_approval = Some(NeedsApproval { height: slot.height, id, since, waiting, sender });
                Some(LOCK_NEEDS_APPROVAL.to_string())
            }
        } else if matches!(change, ChainChange::Membership { op: MembershipOp::Restored, .. })
            && self.restore_settling(id)
        {
            Some("settling".to_string())
        } else {
            self.lock_withheld(id, &change, slot.height)
        };
        if let Some(why) = withheld {
            let key = (slot.height, id, why.clone());
            if self.chain.lock.withheld_logged.as_ref() != Some(&key) {
                tracing::info!(height = slot.height, id, reason = %why, "lock=withheld");
                self.chain.lock.withheld_logged = Some(key);
            }
            // item 8: the carried change too - a forced Applied value may
            // be known here only from the frame
            let carried = wire.or_else(|| wire_change(&change));
            self.chain.lock.retry_accept = Some((slot.clone(), ballot.clone(), value.clone(), id, carried, forced.then_some(evidence)));
            return;
        }
        if self.chain.lock.retry_accept.as_ref().is_some_and(|r| r.0 == *slot) {
            self.chain.lock.retry_accept = None;
        }
        if self.chain.lock.needs_approval.as_ref().is_some_and(|n| n.height == slot.height && n.id == id) {
            self.chain.lock.needs_approval = None;
        }
        // the file keeps the whole change: a withdrawal may remove the card
        let step = self.lock_step(|a| a.accept(slot, ballot, value), Some((value.digest.clone(), id, Some(change.clone()))));
        let Some((reply, changed)) = step else { return };
        match reply {
            Reply::Accepted { .. } => {
                let Some(sk) = self.identity_sk.as_ref() else { return };
                let sig = molt_storage::identity_sign(sk, &self.signing_bytes_at(slot.height, &change));
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
        // a restore's subject never counts for its own restore - by the
        // change, as `lock_seal` strips it (item 9)
        if restored_member(&change) == Some(from) {
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
        let current = self.lock_slot();
        if ballot.leader == self.member() {
            self.lock_seal(slot, &sealable, id, change);
        } else if self.chain.lock.learned.as_ref().map_or(true, |l| Some(&l.slot) != current.as_ref()) {
            let since = self.lock_now();
            self.chain.lock.learned = Some(Learned { slot: slot.clone(), sealable, since, id, change });
        }
    }

    /// Seal head+1 from one ballot's acceptances (§3.3: never pooled).
    fn lock_seal(&mut self, slot: &Slot, s: &Sealable, id: u64, change: ChainChange) -> bool {
        if self.lock_seam(&self.lock_seams.stopped) || self.lock_slot().as_ref() != Some(slot) {
            return false;
        }
        // M3: a chosen value seals - no gate here (R1); the re-key evicts
        // a still-running old device
        let Some(head) = self.chain.head.clone() else { return false };
        let Some(by) = self.chain.lock.sigs.get(&(slot.clone(), s.ballot.clone(), s.value.digest.clone())) else {
            return false;
        };
        let consented = matches!(change, ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. });
        let subject = restored_member(&change);
        let mut sigs: Vec<RosterAttestation> = by
            .iter()
            .filter(|(m, _)| subject != Some(m.as_str()))
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
        if round > ROUND_MAX || !self.lock_at_next(height) {
            return;
        }
        // A-F1: a restore's subject never leads, known to it or not
        if self.lock_restore_subjects().contains(from) {
            tracing::debug!(%from, height, "lock=refused reason=restore_subject");
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
                    // the whole change (C-MED-5): a leader forced to it can
                    // carry it to seats that never saw the card
                    let (id, change) = self.chain.lock.values.get(&v.digest).cloned().unwrap_or((0, None));
                    // item 3: the holder's own signature, evidence for a
                    // leader forced to the value
                    let full = change.clone().or_else(|| self.proposal_change(id));
                    let sig = match (full, self.identity_sk.as_ref()) {
                        (Some(c), Some(sk)) => molt_storage::identity_sign(sk, &self.signing_bytes_at(height, &c)),
                        _ => String::new(),
                    };
                    molt_core::chain::LockAcceptedValue {
                        round: b.round,
                        leader: b.leader,
                        need: need_u64(&v),
                        digest: v.digest,
                        id,
                        change,
                        sig,
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
        if round > ROUND_MAX || accepted.as_ref().is_some_and(|a| a.round > ROUND_MAX) || !self.lock_at_next(height) {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader };
        if accepted.is_some() {
            self.lock_note_accepting(height, from);
        } else {
            self.lock_note_locked(height, from);
        }
        self.lock_saw(&slot, &ballot);
        if ballot.leader != self.member() || self.lock_restore_subjects().contains(from) {
            return;
        }
        let me = self.member();
        // a reported value's `need` is the change's, never the sender's
        // word: a promise naming need 1 would force its value at every
        // ballot (item 3's liveness residual)
        // F5: the holder's signature counts as evidence only where it
        // verifies over exactly the reported change
        let mut sig_ok = false;
        let accepted = accepted.and_then(|mut a| {
            let full = a.change.clone().or_else(|| self.proposal_change(a.id)).or_else(|| self.lock_resolve(height, &a.digest).map(|(_, c)| c));
            match full {
                // F2: a value the chain already applied is no value here
                Some(c) if self.lock_consumed(&c) => {
                    tracing::warn!(%from, height, "lock=bad_report reason=applied");
                    return None;
                }
                Some(c) => {
                    // structural, no gate: a value another seat accepted
                    // stays a value here even where its consent aged out
                    let q = self.lock_quorum()?;
                    let need = match &c {
                        c if CutKind::of(c).is_some() => q.n,
                        ChainChange::Membership { op: MembershipOp::Restored, consent: Some(_), .. } => q.restore_need(),
                        _ => q.m,
                    };
                    if change_digest(&self.republic_id(), height, &c) != a.digest {
                        tracing::warn!(%from, height, "lock=bad_report reason=digest");
                        return None;
                    }
                    a.need = u64::try_from(need).unwrap_or(u64::MAX);
                    let bytes = self.signing_bytes_at(height, &c);
                    sig_ok = !a.sig.is_empty()
                        && self.chain.head.as_ref().is_some_and(|h| {
                            h.identities.iter().any(|i| i.member == from && molt_storage::identity_verify(&i.identity_pk, &bytes, &a.sig))
                        });
                }
                None => {
                    let floor = self.lock_quorum().map_or(1, |q| q.m.min(q.restore_need()));
                    a.need = a.need.max(u64::try_from(floor).unwrap_or(u64::MAX));
                }
            }
            Some(a)
        });
        let Some(run) = self.chain.lock.run.as_mut() else { return };
        if run.leader.slot != slot || run.leader.ballot != ballot {
            return;
        }
        let acc = accepted.map(|a| {
            if sig_ok && from != me {
                let held = run.evidence.entry(a.digest.clone()).or_default();
                if !held.iter().any(|e| e.member == from) {
                    held.push(LockEvidence { member: from.to_string(), sig: a.sig });
                }
            }
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
        self.lock_excuse_subjects();
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
        forced: bool,
        evidence: Vec<LockEvidence>,
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) || from == self.member() {
            return;
        }
        if round > ROUND_MAX || !self.lock_at_next(height) {
            return;
        }
        if self.lock_restore_subjects().contains(from) {
            tracing::debug!(%from, height, "lock=refused reason=restore_subject");
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader: from.to_string() };
        self.lock_note_accepting(height, from);
        self.lock_saw(&slot, &ballot);
        let value = Value { digest, need: usize::try_from(need).unwrap_or(usize::MAX) };
        self.lock_acceptor_accept(&slot, &ballot, &value, id, change, forced.then_some(evidence));
    }

    /// A forced `Applied` value whose card never reached this seat: a
    /// withdrawn record marked `pending_completion` (its proposer from the
    /// id's stripe), never open, never a free pick; dropped when the
    /// height decides without it.
    fn lock_register_completion(&mut self, id: u64, change: &ChainChange, sender: &str) {
        let ChainChange::Applied { surface, payload, .. } = change else { return };
        if self.proposals.contains_key(&id) || self.retired.contains(&id) {
            return;
        }
        let by = self.minting_seat(id).unwrap_or_else(|| sender.to_string());
        let mut card = molt_core::ProposalRecord::new(*surface, payload.clone(), by);
        card.state = ProposalState::Rejected;
        card.withdrawn = true;
        card.pending_completion = true;
        tracing::info!(id, "lock=pending_completion");
        self.proposals.insert(id, card);
    }

    /// The id the status asks a human to approve: a withheld forced value
    /// still unvouched one retry interval after its first refusal, while
    /// every seat that could still vouch for it is silent here (a seat
    /// heard within the still-active window will most likely answer - the
    /// offer would be noise) and the value is not already being sealed.
    pub(crate) fn lock_prompt(&self) -> Option<u64> {
        let n = self.chain.lock.needs_approval.as_ref()?;
        let due = self.lock_now() >= n.since + self.lock_retransmit();
        let sealing = self.chain.lock.learned.as_ref().is_some_and(|l| l.slot.height == n.height);
        let silent = n.waiting.iter().filter(|m| **m != n.sender).all(|m| !self.lock_seat_heard(m));
        (due && silent && !sealing).then_some(n.id)
    }

    /// `member` was heard here within the still-active window (the
    /// restore gate's reading).
    fn lock_seat_heard(&self, member: &str) -> bool {
        let window = match self.seat_active.load(SeqCst) {
            u64::MAX => super::sync::SILENT_AFTER_SECS,
            secs => secs,
        };
        let seen = self.restore_heard_at(member);
        seen != molt_core::MemberInfo::NEVER && self.presence_now().saturating_sub(seen) < window
    }

    /// Item 3's exit: an approve of the withdrawn card the status names.
    /// `false` for any other card (the caller judges it as before).
    pub(crate) fn lock_approve_forced(&mut self, id: u64) -> bool {
        let named = self.lock_prompt().is_some_and(|i| i == id);
        if !named || !self.proposals.get(&id).is_some_and(|p| p.withdrawn) {
            return false;
        }
        tracing::info!(id, "lock=approved_forced");
        self.chain.lock.approved_forced.insert(id);
        if let Some((s, b, v, id, change, forced)) = self.chain.lock.retry_accept.take() {
            self.lock_acceptor_accept(&s, &b, &v, id, change, forced);
        }
        true
    }

    /// Item 3: a forced value may be accepted without this seat's own
    /// approval only on real acceptances of exactly that change by two
    /// distinct seats (`lock::evidence_holds`; the sender may be one).
    fn lock_forced_vouched(&self, height: u64, sender: &str, id: u64, change: &ChainChange, evidence: &[LockEvidence]) -> bool {
        let me = self.member();
        // F1: an approval is of the change this seat's own record holds
        // under the id - never of whatever a frame carries under it
        let approved = (self.chain.own_approvals.contains(&id)
            || self.chain.lock.approved_forced.contains(&id)
            || self.chain.lock.intents.get(&id).is_some_and(|v| v.contains(&me)))
            && self.proposal_change(id).as_ref() == Some(change);
        if approved {
            return true;
        }
        let bytes = self.signing_bytes_at(height, change);
        let Some(head) = self.chain.head.as_ref() else { return false };
        let genuine = |m: &str| {
            evidence.iter().any(|e| {
                e.member == m
                    && head.identities.iter().any(|i| i.member == m && molt_storage::identity_verify(&i.identity_pk, &bytes, &e.sig))
            })
        };
        let named: Vec<&str> = evidence.iter().map(|e| e.member.as_str()).collect();
        evidence_holds(sender, &named, true, genuine)
    }

    /// The evidence a leader sends with forced `value`: every acceptance
    /// of it a promise reported or a `LockAccepted` showed, plus its own
    /// signature where it accepts the value itself.
    fn lock_forced_evidence(&self, slot: &Slot, ballot: &Ballot, value: &Value, id: u64, carried: Option<&ChainChange>) -> Vec<LockEvidence> {
        let me = self.member();
        let mut out: Vec<LockEvidence> = self.chain.lock.run.as_ref().and_then(|r| r.evidence.get(&value.digest).cloned()).unwrap_or_default();
        let seen = self
            .chain
            .lock
            .sigs
            .iter()
            .filter(|((s, _, d), _)| s == slot && *d == value.digest)
            .flat_map(|(_, by)| by.iter())
            .filter(|(m, _)| m.as_str() != me);
        for (m, sig) in seen {
            if !out.iter().any(|e| e.member == *m) {
                out.push(LockEvidence { member: m.clone(), sig: sig.clone() });
            }
        }
        let change = carried.cloned().or_else(|| self.proposal_change(id));
        if let (Some(change), Some(sk)) = (change, self.identity_sk.as_ref()) {
            let holds = self.chain.lock.acceptor.holds(slot, value);
            // F1: only what this seat would accept itself, every check run
            let vouched = self.lock_forced_vouched(slot.height, &ballot.leader, id, &change, &out)
                && self.lock_forced_invalid(id, &change, slot.height).is_none();
            if holds || vouched {
                let sig = molt_storage::identity_sign(sk, &self.signing_bytes_at(slot.height, &change));
                out.insert(0, LockEvidence { member: me, sig });
            }
        }
        // two are all a seat needs; the frame stays the size of a block
        out.truncate(2);
        out
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
        if round > ROUND_MAX || !self.lock_at_next(height) {
            return;
        }
        let slot = Slot { height, base };
        let ballot = Ballot { round, leader };
        self.lock_note_accepting(height, from);
        self.lock_saw(&slot, &ballot);
        if self.lock_slot().as_ref() != Some(&slot) {
            return;
        }
        let Some((id, change)) = self.lock_resolve(height, digest) else { return };
        let Some(head) = self.chain.head.as_ref() else { return };
        let bytes = self.signing_bytes_at(height, &change);
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
        (head, seq): (u64, u64),
    ) {
        if !self.lock_listens() || !self.lock_rostered(from) {
            return;
        }
        self.note_peer_claim(from, head, Some(seq), super::PeerClaim::Head);
        if round > ROUND_MAX || promised.as_ref().is_some_and(|p| p.round > ROUND_MAX) || !self.lock_at_next(height) {
            return;
        }
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
        // dark: nothing here holds state a dark build keeps (review R1)
        if !self.lock_listens() {
            return;
        }
        let Some(head) = self.chain.head.as_ref().map(|h| h.height) else { return };
        // B-F1/D-F1: never forget a slot that can still be contended - one
        // above the chain on disk, or inside the reorg window above the
        // last cut (the only finality the fork rule has)
        let cut = self
            .chain
            .blocks
            .iter()
            .rev()
            .find(|b| b.height == 0 || CutKind::of(&b.change).is_some())
            .map_or(0, |b| b.height);
        let lock = &mut self.chain.lock;
        let floor = lock.durable_head.min(cut.max(head.saturating_sub(SLOT_KEEP)));
        lock.acceptor.settle(floor, head);
        lock.floor = lock.floor.max(floor);
        let held = lock.acceptor.accepted_digests();
        lock.values.retain(|d, _| held.contains(d));
        // item 4: the whole change only for an open slot, digests below -
        // the file is rewritten whole on every step
        let open = lock.acceptor.accepted_digests_above(head);
        for (d, (_, change)) in &mut lock.values {
            if !open.contains(d) {
                *change = None;
            }
        }
        lock.sigs.retain(|(s, _, _), _| s.height > head);
        lock.tallies.retain(|s, _| s.height > head);
        lock.seen.retain(|s, _| s.height > head);
        lock.known.retain(|(h, _), _| *h > head);
        lock.free_seen.retain(|h, _| *h > head);
        lock.locked_seen.retain(|h, _| *h > head);
        lock.accepted_seen.retain(|h, _| *h > head);
        // C-MED-3: a card stale at one height stays behind the others at
        // the next (a never-sealable card no longer costs every height)
        lock.passive.retain(|h| *h > head);
        if lock.needs_approval.as_ref().is_some_and(|n| n.height <= head) {
            lock.needs_approval = None;
        }
        lock.approved_forced.clear();
        // F2: an applied card's approval approves nothing any more
        if self.lock_enabled() {
            let chain = &mut self.chain;
            if let Some(w) = chain.walk.as_ref() {
                chain.own_approvals.retain(|id| !w.seen.contains(id));
            }
        }
        // a completion card whose height decided without it goes
        let stale: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.pending_completion && p.state != ProposalState::Applied)
            .map(|(id, _)| *id)
            .collect();
        let asked = self.chain.lock.needs_approval.as_ref().map(|n| n.id);
        for id in stale.into_iter().filter(|id| Some(*id) != asked) {
            self.proposals.remove(&id);
        }
        for p in self.proposals.values_mut().filter(|p| p.pending_completion) {
            if p.state == ProposalState::Applied {
                p.pending_completion = false;
            }
        }
        let lock = &mut self.chain.lock;
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
        self.lock_mark_after_move(head);
        // R8: an intent may outrun its card; only a retired card's go
        let known: BTreeSet<u64> = self.proposals.keys().copied().collect();
        self.chain.lock.intents.retain(|id, _| open.contains(id) || !known.contains(id));
        let unknown: Vec<u64> =
            self.chain.lock.intents.keys().filter(|id| !known.contains(id) && !open.contains(id)).copied().collect();
        for id in unknown.iter().take(unknown.len().saturating_sub(LOCK_EARLY_INTENTS_MAX)) {
            self.chain.lock.intents.remove(id);
        }
        self.chain.lock.intent_sent.retain(|id| open.contains(id));
        self.chain.lock.stale_ids.retain(|id| open.contains(id));
        self.chain.lock.intent_at.retain(|id, _| open.contains(id));
        let slot = self.lock_slot();
        if self.chain.lock.run.as_ref().is_some_and(|r| Some(&r.leader.slot) != slot.as_ref()) {
            self.chain.lock.run = None;
        }
        if let Some(Some(floor)) = self.chain.lock.amnesia {
            if head >= floor {
                self.lock_amnesia_over(head, floor);
            }
        }
        self.lock_unknown_means_amnesia();
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
        // a backoff after yielding to a seat that is now being restored
        // waits on a ballot nobody will finish (§4.2: its old device is gone)
        let subjects = self.lock_restore_subjects();
        let yielded_to_gone = self
            .chain
            .lock
            .seen
            .iter()
            .filter(|(s, _)| s.height == next)
            .map(|(_, b)| b)
            .max()
            .is_some_and(|b| subjects.contains(&b.leader));
        if yielded_to_gone && self.chain.lock.wait.as_ref().is_some_and(|w| w.attempt > 0) {
            tracing::info!(height = next, "lock=backoff_dropped reason=restore_subject");
            self.chain.lock.wait = None;
        }
        let backing_off = self.chain.lock.wait.as_ref().is_some_and(|w| w.height == next && w.attempt > 0);
        if !backing_off && self.lock_line(next).first() == Some(&self.member()) {
            self.lock_start(0);
        }
    }

    /// §6, the corrected rule: an amnesiac seat asks; a single peer's
    /// height is no floor (it may not know what this seat touched).
    fn lock_amnesia_ask(&mut self) {
        if self.chain.lock.amnesia != Some(None) || !self.lock_enabled() {
            return;
        }
        let now = self.lock_now();
        let retransmit = self.lock_retransmit();
        let nonce = match &mut self.chain.lock.amnesia_ask {
            Some((_, sent, _)) if now < *sent + retransmit => return,
            Some((nonce, sent, _)) => {
                *sent = now;
                *nonce
            }
            None => {
                let mut b = [0u8; 8];
                if getrandom::getrandom(&mut b).is_err() {
                    return;
                }
                let nonce = u64::from_le_bytes(b);
                self.chain.lock.amnesia_ask = Some((nonce, now, BTreeMap::new()));
                nonce
            }
        };
        self.lock_send(WorkspaceEvent::LockAmnesiaAsk { nonce });
    }

    /// The answers an amnesiac seat needs: max(m, n - m + 1) other seats.
    fn lock_amnesia_need(&self) -> usize {
        self.lock_quorum().map_or(usize::MAX, amnesia_need)
    }

    /// Answer an amnesiac seat - only from this seat's own lock state; an
    /// amnesiac seat answers nothing.
    pub(crate) fn lock_on_amnesia_ask(&mut self, from: &str, nonce: u64) {
        if !self.lock_listens() || from == self.member() || !self.lock_rostered(from) {
            return;
        }
        self.chain.lock.amnesiac_others.insert(from.to_string());
        if self.chain.lock.amnesia.is_some() {
            tracing::debug!(%from, "lock=amnesia_unanswered reason=amnesiac");
            return;
        }
        let lock = &self.chain.lock;
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        // F4: from this seat's own state only - peer-supplied heights
        // could pin the asker passive for good
        let touched = amnesia_touched(head, lock.acceptor.highest().min(head.saturating_add(1)));
        let round = lock.rounds_by.get(from).copied().unwrap_or(0);
        self.lock_send(WorkspaceEvent::LockAmnesiaReply { to: from.to_string(), nonce, touched, round });
    }

    /// Count an answer; with max(m, n - m + 1) of them the seat is passive
    /// up to the highest height they report and leads above the highest
    /// round they saw from it.
    pub(crate) fn lock_on_amnesia_reply(&mut self, from: &str, to: &str, nonce: u64, touched: u64, round: u64) {
        if !self.lock_listens() {
            return;
        }
        // an answer comes only from a seat that holds its own state
        self.chain.lock.amnesiac_others.remove(from);
        let me = self.member();
        if to != me || from == me || !self.lock_rostered(from) || self.chain.lock.amnesia != Some(None) || round > ROUND_MAX {
            return;
        }
        let need = self.lock_amnesia_need();
        let Some((asked, _, answers)) = self.chain.lock.amnesia_ask.as_mut() else { return };
        if *asked != nonce {
            return;
        }
        answers.insert(from.to_string(), (touched, round));
        let all: Vec<(u64, u64)> = answers.values().copied().collect();
        let Some((floor, rounds)) = amnesia_floor(&all, need) else { return };
        self.chain.lock.amnesia_ask = None;
        self.chain.lock.amnesia = Some(Some(floor));
        self.chain.lock.amnesia_round = self.chain.lock.amnesia_round.max(rounds);
        tracing::warn!(floor, round = rounds, answers = need, "lock=amnesia_floor");
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        if head >= floor {
            self.lock_amnesia_over(head, floor);
        }
    }

    /// Past the floor: an ordinary seat again; an import's unknown state is
    /// known from here on.
    fn lock_amnesia_over(&mut self, head: u64, floor: u64) {
        self.chain.lock.amnesia = None;
        tracing::info!(head, floor, "lock=amnesia_over");
        self.persist_lock();
        if let Some(a) = self.active.as_mut() {
            if a.prefs.lock_unknown {
                a.prefs.lock_unknown = false;
                a.handle.set_prefs(a.prefs.clone());
            }
        }
    }

    /// Item 2: an imported seat whose chain runs the mode (from the open,
    /// or first from a block caught up) is amnesiac until answered.
    fn lock_unknown_means_amnesia(&mut self) {
        let unknown = self.active.as_ref().is_some_and(|a| a.prefs.lock_unknown);
        if unknown && self.chain.lock.amnesia.is_none() && self.lock_cap_on() && self.lock_enabled() {
            tracing::warn!("lock=amnesia reason=import");
            self.chain.lock.amnesia = Some(None);
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
        self.lock_amnesia_ask();
        let Some(slot) = self.lock_slot() else { return };
        self.lock_try_release(slot.height);
        if self.lock_mode(slot.height) != LockMode::Lock || self.lock_amnesiac_at(slot.height) {
            return;
        }
        if let Some((s, b, v, id, change, forced)) = self.chain.lock.retry_accept.take() {
            if s == slot {
                self.lock_acceptor_accept(&s, &b, &v, id, change, forced);
            }
        }
        let now = self.lock_now();
        let grace = self.lock_grace();
        if let Some(l) = self.chain.lock.learned.clone() {
            if l.slot == slot && now >= l.since + grace {
                self.lock_seal(&l.slot, &l.sealable, l.id, l.change);
                return;
            }
        }
        if let Some(run) = &self.chain.lock.run {
            // S4/W4: a run that is not idle - promises missing, a forced
            // value waiting, or a value out - is re-sent at a doubling
            // interval up to the cap and goes stale after its backoff. An
            // idle leader (promises in hand, nothing to propose) is silent.
            let busy = !run.leader.ready() || run.leader.forced().is_some() || run.leader.proposed().is_some();
            let stale = busy && now >= run.started + self.lock_backoff(slot.height, run.attempt);
            let interval = self.lock_retransmit().saturating_mul(1 << run.resends.min(10)).min(LOCK_BACKOFF_MAX_SECS);
            let resend = busy && now >= run.sent_at + interval;
            let attempt = run.attempt;
            let free_pick = run.value.as_ref().filter(|_| run.leader.forced().is_none()).map(|(id, _)| *id);
            // S3: a forced value that waited for a fuller hearing goes out
            // once its interval has passed
            if run.leader.ready() && run.leader.proposed().is_none() && !stale {
                self.lock_try_propose();
            }
            if self.chain.lock.run.is_none() {
                return;
            }
            if stale {
                if let Some(id) = free_pick {
                    let order = &mut self.chain.lock.stale_ids;
                    order.retain(|x| *x != id);
                    order.push(id);
                }
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
        let forced = run.forced;
        let (proposed, value) = (run.leader.proposed().cloned(), run.value.clone());
        // an acceptance heard since the first send may vouch now
        let evidence = match (&proposed, &value) {
            (Some(v), Some((id, change))) if forced => self.lock_forced_evidence(&slot, &ballot, v, *id, change.as_ref()),
            _ => Vec::new(),
        };
        if let Some(run) = self.chain.lock.run.as_mut() {
            run.sent_evidence = evidence.clone();
        }
        let body = match (proposed, value) {
            (Some(v), Some((id, change))) => WorkspaceEvent::LockAccept {
                height: slot.height,
                base: slot.base.clone(),
                round: ballot.round,
                need: need_u64(&v),
                digest: v.digest,
                id: ProposalId(id),
                change,
                forced,
                evidence,
            },
            // phase 1 (only while busy, see the tick): a missing promise may
            // still free a forced value or complete a cut's hearing (S3, S4)
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
        // the chain just read from disk is the durable one
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.chain.lock.durable_head = head;
        let on = self.lock_cap_on() && self.lock_enabled();
        if self.lock_chain_unsupported() {
            tracing::warn!(reason = LOCK_CHAIN_UNSUPPORTED, "chain_unsupported");
        }
        // T3: the marker means "the lock wrote here"; on a chain without
        // the lock and with no file it cannot be true (a pre-lock import)
        if marker && !self.lock_enabled() && matches!(parsed, Ok(None)) {
            if let Some(a) = self.active.as_mut() {
                a.prefs.chain_lock = false;
                a.handle.set_prefs(a.prefs.clone());
                tracing::info!("lock=marker_cleared reason=no_lock_on_chain");
            }
        }
        match parsed {
            Ok(Some(file)) if file.floor > head && on => {
                // B-F1: the file forgot slots the chain on disk still needs;
                // what it kept above them still holds
                tracing::warn!(floor = file.floor, head, "lock=amnesia reason=ahead_of_chain");
                self.chain.lock.amnesia = Some(Some(file.floor.saturating_add(1)));
                self.chain.lock.acceptor = file.acceptor;
                self.chain.lock.values = file.values;
                self.chain.lock.floor = file.floor;
                return;
            }
            Ok(Some(file)) => {
                self.chain.lock.acceptor = file.acceptor;
                self.chain.lock.values = file.values;
                self.chain.lock.floor = file.floor;
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
        self.chain.lock.amnesia_ask = None;
        self.lock_unknown_means_amnesia();
        if !on || self.chain.lock.amnesia.is_some() {
            return;
        }
        // own replayed signatures were free ones (a build without the lock
        // ran here, or none ever wrote the file): mark them first
        let before = self.chain.lock.acceptor.clone();
        self.lock_mark_own_free(0);
        if self.is_chain_governed() && (self.chain.lock.acceptor != before || !marker) && !self.persist_lock() {
            // W6: marks that never reached the disk would let a free
            // signature out unwritten
            self.chain.lock.acceptor = before;
            self.chain.lock.amnesia = Some(None);
            tracing::warn!("lock=amnesia reason=unwritten");
        }
    }

    /// Own signatures above `above` were given freely: mark them (or go
    /// passive where lock state conflicts).
    fn lock_mark_own_free(&mut self, above: u64) {
        let me = self.member();
        let own: Vec<u64> = self
            .chain
            .pending_sigs
            .values()
            .filter(|p| p.height > above && p.sigs.iter().any(|a| a.member == me))
            .map(|p| p.height)
            .collect();
        for h in own {
            if self.chain.lock.acceptor.signed_freely(h) || self.chain.lock.passive.contains(&h) {
                continue;
            }
            if self.chain.lock.acceptor.sign_freely(h).is_none() {
                tracing::warn!(height = h, "lock=passive reason=conflict");
                self.chain.lock.passive.insert(h);
            }
        }
    }

    /// R6: a free signature given where the lock was not enabled (the losing
    /// side of a fork at the enabling height) is marked before any lock
    /// reply once the head reaches a lineage that runs the lock.
    pub(crate) fn lock_mark_after_move(&mut self, head: u64) {
        if !self.lock_cap_on() || !self.lock_enabled() || self.chain.lock.amnesia.is_some() || !self.is_chain_governed() {
            return;
        }
        let before = self.chain.lock.acceptor.clone();
        self.lock_mark_own_free(head);
        if self.chain.lock.acceptor != before && !self.persist_lock() {
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
            let wait = if self.lock_chain_unsupported() { LOCK_CHAIN_UNSUPPORTED.to_string() } else { String::new() };
            return (false, ballot, wait);
        }
        let wait = if self.chain.lock.amnesia == Some(None) {
            let have = self.chain.lock.amnesia_ask.as_ref().map_or(0, |(_, _, a)| a.len());
            let need = self.lock_amnesia_need();
            let n = self.lock_quorum().map_or(0, |q| q.n);
            // too many others amnesiac too: no answer set can ever form
            if n.saturating_sub(1 + self.chain.lock.amnesiac_others.len()) < need {
                "amnesia - restore this seat with a new link".to_string()
            } else {
                format!("amnesia - waiting for {} of {need} seats to answer", need.saturating_sub(have))
            }
        } else if self.lock_amnesiac_at(next) {
            "amnesia".to_string()
        } else if let Some(n) = &self.chain.lock.needs_approval {
            if self.lock_prompt().is_some() {
                format!("decision {} may already be made - approve to complete it", n.id)
            } else {
                format!("decision {}: waiting for {}", n.id, n.waiting.join(", "))
            }
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
                    let by = self.chain.lock.sigs.get(&(run.leader.slot.clone(), q, v.digest.clone()));
                    let have = by.map_or(0, BTreeMap::len);
                    let missing: Vec<String> = run
                        .promised
                        .iter()
                        .filter(|m| !by.is_some_and(|b| b.contains_key(*m)))
                        .cloned()
                        .collect();
                    format!("acceptances {have}/{} missing {}", v.need, missing.join(","))
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
