// SPDX-License-Identifier: GPL-3.0-or-later

//! **The sealing lock** (`docs/chain/chain_lock_design.md`): Flexible Paxos
//! per chain slot, pure - no I/O, no clock, no RNG.
//!
//! - [`Acceptor`]: one seat's durable promises and acceptances, plus the
//!   heights it signed freely (B6). `prepare`/`accept` return the reply and
//!   whether the state must reach the disk BEFORE that reply leaves.
//! - [`Leader`]: one ballot at one slot: collect promises, then `propose`
//!   the forced value or the caller's own.
//! - [`Tally`]: same-ballot acceptances; `accepted` names the signers once a
//!   value is sealable. Signatures of different ballots never combine.
//!
//! The engine wiring is `lock_wire.rs`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// One consensus instance: a height on one lineage (the change digests
/// from the last cut up to `height - 1`), so a fork below never shares it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Slot {
    pub height: u64,
    pub base: String,
}

/// Totally ordered by round, then leader name; unique per leader.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Ballot {
    pub round: u64,
    pub leader: String,
}

/// The decided thing: the digest of the change's approval bytes at the
/// slot's height (attestation variants are one value), and how many LIVE
/// acceptances seal it (m; n for a cut; m - 1 for a consented restore).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Value {
    pub digest: String,
    pub need: usize,
}

/// The slot's roster size and threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quorum {
    pub n: usize,
    pub m: usize,
}

impl Quorum {
    /// Q1 = n - m + 1: intersects every m-quorum (and every larger one).
    pub(crate) fn promises(self) -> usize {
        self.n.saturating_sub(self.m).saturating_add(1)
    }

    /// §14: with `free` OTHER seats known to have signed freely at a height,
    /// no lock value can ever be chosen there once this seat leaves: either
    /// the non-free seats cannot answer phase 1 (`free >= m`), or they are
    /// too few to accept m (`free >= n - m`, plus one when this seat's own
    /// acceptance still counts among them).
    pub(crate) fn lock_dead(self, free: usize, holds_acceptance: bool) -> bool {
        free > 0 && free >= self.m.min(self.n.saturating_sub(self.m) + usize::from(holds_acceptance))
    }
}

/// An acceptor's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reply {
    Promise { slot: Slot, ballot: Ballot, accepted: Option<(Ballot, Value)> },
    Accepted { slot: Slot, ballot: Ballot, value: Value },
    /// `promised: None` = this seat signed freely at the height.
    Nack { slot: Slot, ballot: Ballot, promised: Option<Ballot> },
}

/// A reply plus whether the acceptor changed and must be persisted first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub reply: Reply,
    pub persist: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SlotState {
    #[serde(default)]
    promised: Option<Ballot>,
    #[serde(default)]
    accepted: Option<(Ballot, Value)>,
}

/// One seat's durable lock state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Acceptor {
    #[serde(default)]
    slots: BTreeMap<u64, BTreeMap<String, SlotState>>,
    #[serde(default)]
    free: BTreeSet<u64>,
    /// The simulation's control: accept and promise regardless of the
    /// promise, which must let it find a fork.
    #[cfg(test)]
    #[serde(skip)]
    pub(crate) skip_promise_check: bool,
}

/// How far below the head a free mark is kept: a reorg may re-open a
/// height, and a free signature stays valid on every branch.
pub(crate) const FREE_MARK_KEEP: u64 = 4096;

impl Acceptor {
    fn state(&self, slot: &Slot) -> Option<&SlotState> {
        self.slots.get(&slot.height)?.get(&slot.base)
    }

    fn checks(&self) -> bool {
        #[cfg(test)]
        if self.skip_promise_check {
            return false;
        }
        true
    }

    /// Phase 1b: promise `ballot` unless a higher one is promised or this
    /// seat signed freely at the height.
    pub(crate) fn prepare(&mut self, slot: &Slot, ballot: &Ballot) -> Step {
        if self.free.contains(&slot.height) {
            return nack(slot, ballot, None);
        }
        let checks = self.checks();
        let st = self.slots.entry(slot.height).or_default().entry(slot.base.clone()).or_default();
        if checks && st.promised.as_ref().is_some_and(|p| p > ballot) {
            let promised = st.promised.clone();
            return nack(slot, ballot, promised);
        }
        let persist = st.promised.as_ref() != Some(ballot);
        if persist {
            st.promised = Some(ballot.clone());
        }
        Step {
            reply: Reply::Promise { slot: slot.clone(), ballot: ballot.clone(), accepted: st.accepted.clone() },
            persist,
        }
    }

    /// Phase 2b: accept `value` at `ballot` if nothing higher is promised.
    /// One value per ballot; the caller has already checked this seat
    /// approves the value.
    pub(crate) fn accept(&mut self, slot: &Slot, ballot: &Ballot, value: &Value) -> Step {
        if self.free.contains(&slot.height) {
            return nack(slot, ballot, None);
        }
        let checks = self.checks();
        let st = self.slots.entry(slot.height).or_default().entry(slot.base.clone()).or_default();
        if checks && st.promised.as_ref().is_some_and(|p| p > ballot) {
            let promised = st.promised.clone();
            return nack(slot, ballot, promised);
        }
        if let Some((b, v)) = &st.accepted {
            if b == ballot && v != value {
                let promised = st.promised.clone();
                return nack(slot, ballot, promised);
            }
        }
        let now = Some((ballot.clone(), value.clone()));
        let persist = st.accepted != now || st.promised.as_ref() != Some(ballot);
        st.promised = Some(ballot.clone());
        st.accepted = now;
        Step {
            reply: Reply::Accepted { slot: slot.clone(), ballot: ballot.clone(), value: value.clone() },
            persist,
        }
    }

    /// Whether a B6 signature at `height` is allowed: no lock state there.
    pub(crate) fn may_sign_freely(&self, height: u64) -> bool {
        self.slots
            .get(&height)
            .map_or(true, |bases| bases.values().all(|s| s.promised.is_none() && s.accepted.is_none()))
    }

    /// This seat signed freely at `height`.
    pub(crate) fn signed_freely(&self, height: u64) -> bool {
        self.free.contains(&height)
    }

    /// This seat already accepted exactly `value` at `slot`.
    pub(crate) fn holds(&self, slot: &Slot, value: &Value) -> bool {
        self.state(slot).and_then(|s| s.accepted.as_ref()).is_some_and(|(_, v)| v == value)
    }

    /// This seat accepted a value at `height` (on any lineage).
    pub(crate) fn holds_acceptance(&self, height: u64) -> bool {
        self.slots.get(&height).is_some_and(|bases| bases.values().any(|s| s.accepted.is_some()))
    }

    /// Trade every lock state at `height` for a free mark once `free` other
    /// seats known free there leave the lock dead ([`Quorum::lock_dead`],
    /// chain_lock_design.md §14). `true` = changed, persist first.
    pub(crate) fn release_to_free(&mut self, height: u64, q: Quorum, free: usize) -> bool {
        if self.free.contains(&height) || !q.lock_dead(free, self.holds_acceptance(height)) {
            return false;
        }
        self.slots.remove(&height);
        self.free.insert(height);
        true
    }

    /// The rule as first shipped on the WIP branch (promise-only state,
    /// `free >= n - m`), kept as the simulation's control.
    #[cfg(test)]
    pub(crate) fn release_promises_only(&mut self, height: u64) -> bool {
        if self.holds_acceptance(height) {
            return false;
        }
        self.slots.remove(&height);
        self.free.insert(height);
        true
    }

    /// Record a B6 signature at `height`. `None` = refused (lock state
    /// there); `Some(persist)` otherwise.
    pub(crate) fn sign_freely(&mut self, height: u64) -> Option<bool> {
        if !self.may_sign_freely(height) {
            return None;
        }
        Some(self.free.insert(height))
    }

    /// The head moved to `head`: drop decided slots and old free marks.
    /// Returns whether anything changed.
    pub(crate) fn settle(&mut self, head: u64) -> bool {
        let before = (self.slots.len(), self.free.len());
        self.slots.retain(|h, _| *h > head);
        let floor = head.saturating_sub(FREE_MARK_KEEP);
        self.free.retain(|h| *h > floor);
        before != (self.slots.len(), self.free.len())
    }

    /// A round above everything this seat promised or accepted at `slot`.
    pub(crate) fn next_round(&self, slot: &Slot) -> u64 {
        let st = self.state(slot);
        let promised = st.and_then(|s| s.promised.as_ref()).map_or(0, |b| b.round);
        let accepted = st.and_then(|s| s.accepted.as_ref()).map_or(0, |(b, _)| b.round);
        promised.max(accepted).saturating_add(1)
    }

    /// The digests of every value accepted at an open slot.
    pub(crate) fn accepted_digests(&self) -> BTreeSet<String> {
        self.slots
            .values()
            .flat_map(|bases| bases.values())
            .filter_map(|s| s.accepted.as_ref().map(|(_, v)| v.digest.clone()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn promised(&self, slot: &Slot) -> Option<&Ballot> {
        self.state(slot)?.promised.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn accepted(&self, slot: &Slot) -> Option<&(Ballot, Value)> {
        self.state(slot)?.accepted.as_ref()
    }
}

fn nack(slot: &Slot, ballot: &Ballot, promised: Option<Ballot>) -> Step {
    Step { reply: Reply::Nack { slot: slot.clone(), ballot: ballot.clone(), promised }, persist: false }
}

/// One ballot at one slot, volatile: a restarted leader starts a new
/// ballot above its own acceptor's promise.
#[derive(Debug, Clone)]
pub(crate) struct Leader {
    pub slot: Slot,
    pub ballot: Ballot,
    quorum: Quorum,
    promises: BTreeMap<String, Option<(Ballot, Value)>>,
    /// Seats that answered "signed freely here": they hold no acceptance
    /// that can be part of a chosen value.
    free: BTreeSet<String>,
    proposed: Option<Value>,
}

impl Leader {
    pub(crate) fn new(slot: Slot, quorum: Quorum, ballot: Ballot) -> Self {
        Self { slot, ballot, quorum, promises: BTreeMap::new(), free: BTreeSet::new(), proposed: None }
    }

    /// A free `Nack` from `from`: heard, holding nothing that counts.
    pub(crate) fn heard_free(&mut self, from: &str) {
        if !self.promises.contains_key(from) {
            self.free.insert(from.to_string());
        }
    }

    /// A promise for this ballot (others are ignored; duplicates count once).
    pub(crate) fn promise(&mut self, from: &str, ballot: &Ballot, accepted: Option<(Ballot, Value)>) {
        if *ballot == self.ballot {
            self.promises.insert(from.to_string(), accepted);
        }
    }

    /// Phase 1 is complete.
    pub(crate) fn ready(&self) -> bool {
        self.promises.len() >= self.quorum.promises()
    }

    /// The value this ballot must carry: the highest accepted one among the
    /// promises, unless it cannot have been chosen - fewer seats hold it as
    /// their latest than it needs, counting every unheard seat as holding it.
    pub(crate) fn forced(&self) -> Option<Value> {
        let (_, top) = self.promises.values().flatten().max()?;
        let holding = self.promises.values().flatten().filter(|(_, v)| v == top).count();
        let unheard = self.quorum.n.saturating_sub(self.promises.len() + self.free.len());
        (holding + unheard >= top.need).then(|| top.clone())
    }

    /// Phase 2a, once per ballot: the forced value, else `own`. `None` =
    /// not ready, already proposed, or nothing to propose.
    pub(crate) fn propose(&mut self, own: Option<Value>) -> Option<Value> {
        if !self.ready() || self.proposed.is_some() {
            return None;
        }
        let value = self.forced().or(own)?;
        self.proposed = Some(value.clone());
        Some(value)
    }

    pub(crate) fn proposed(&self) -> Option<&Value> {
        self.proposed.as_ref()
    }
}

/// What a slot may be sealed with: one ballot's acceptances of one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sealable {
    pub ballot: Ballot,
    pub value: Value,
    /// The `need` lowest-named acceptors.
    pub signers: Vec<String>,
}

/// Same-ballot acceptances for one slot, volatile.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tally {
    by: BTreeMap<(Ballot, Value), BTreeSet<String>>,
}

impl Tally {
    /// Count one acceptance; `Some` once its ballot holds `need` of them.
    pub(crate) fn accepted(&mut self, from: &str, ballot: &Ballot, value: &Value) -> Option<Sealable> {
        let set = self.by.entry((ballot.clone(), value.clone())).or_default();
        set.insert(from.to_string());
        (set.len() >= value.need).then(|| Sealable {
            ballot: ballot.clone(),
            value: value.clone(),
            signers: set.iter().take(value.need).cloned().collect(),
        })
    }
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod tests;
