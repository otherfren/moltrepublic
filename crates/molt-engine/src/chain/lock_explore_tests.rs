// SPDX-License-Identifier: GPL-3.0-or-later

//! Exhaustive small-scope model checker of the sealing lock at ONE slot
//! (`docs/chain/chain_lock_design.md` §4, §4.2, §14). Explicit-state BFS
//! over every interleaving, with the REAL `Acceptor` and `Leader` of
//! `lock.rs` inside each seat and the wire rules of `lock_wire.rs`
//! mirrored per seat, on that seat's own knowledge.
//!
//! | rule | `lock_wire.rs` | modelled |
//! |---|---|---|
//! | need(v) | `lock_value` | m; restore max(m-1,1); F1: m when m<n |
//! | restore subjects | `lock_restore_subjects` | registered (known, not withdrawn) or held: own acceptance, run ids, `known` |
//! | free count, cascade | `lock_known_free` | free_seen minus self, subjects, accepted_seen; locked_seen only once dead |
//! | dead / mode | `lock_dead_at`, `lock_mode` | `Quorum::lock_dead`; mode via `may_sign_freely`/`signed_freely` |
//! | release | `lock_try_release` | `release_to_free`, run dropped, standing own approvals signed freely |
//! | free standing intents | `lock_free_standing_intents` | Release action re-signs in Free mode |
//! | free signature gate | `lock_sign_freely` | `sign_freely`, refused with lock state |
//! | intent | `lock_intent`, `lock_note_intent` | unsigned Approved; author = voter; old build keeps none |
//! | choice | `lock_choose`, `lock_choose_skipping` | smallest known open id; voices minus subjects, free_seen; >= max(m-1,1) and need; restore gate; stale order; F4 filters by promised |
//! | ballot open | `lock_start` | round max(next_round, seen+1); own prepare; heard_free(known free); excuse; Prepare |
//! | excuse | `lock_excuse_subjects` | restores named in run ids |
//! | phase 2a | `lock_try_propose` | forced via `Leader::forced`, S3 wait while heard<n until its timer (Drive); else choice |
//! | approver path | `lock_forced_approver` | forced, withdrawn here, own intent on record; F3: any seat |
//! | accept | `lock_acceptor_accept` | release first; register wire restore; kept / approver / `lock_withheld`; `Acceptor::accept` |
//! | withheld | `lock_withheld`, `lock_gate` | own intent, open, restore gate (self or revived subject heard) |
//! | retry | `retry_accept` in `lock_tick` | Retry action |
//! | tally | `lock_count_accepted` | subject's acceptance of a restore skipped; per ballot; leader seals, others learn |
//! | seal | `lock_seal` | restore gate holds; subjects filtered; >= need |
//! | learner seal | `lock_tick` learned | LearnSeal action (grace ignored) |
//! | saw / yield | `lock_saw` | highest ballot; a higher foreign ballot drops the run |
//! | frames | `lock_on_prepare`..`lock_on_nack` | per frame, incl. locked/accepted_seen notes and free Nack |
//! | who leads | `lock_drive`, `lock_tick`, `lock_line` | main: any seat (timing free) unless subject in own view, then only when ready; live: rank 0 of alive non-subjects or ready; a busy run may restart; F2: a subject in own view never |
//! | withdrawal | `proposals.rs` withdraw | proposer; own approval dropped, intent kept, registration gone |
//!
//! NOT MODELLED: cuts (need n); more than one slot, head moves, reorgs,
//! lineages; Committed blocks reaching others (a sealer stops, others go
//! on - as if every block frame were lost); catch-up; amnesia, passive
//! heights, persistence failures; declines; the restore settle timer and
//! `restore_settled` (any order allowed); B3's window (a revived subject
//! is heard forever, pre-loss frames never); `chain_unsettled`;
//! takeover/backoff timing (any order allowed); local vault/wallet gates;
//! B6 free sealing is global (m free signatures anywhere seal, restore
//! needs max(m-1,1) free); an old-build seat keeps no intents; a lost
//! seat's device has no lock state before it is lost when `lost` is set.
//! Approximated: a frame is lost only when its sender dies (all its
//! frames in flight at once) - a loss between two live seats is a delay,
//! since the fault-free continuation re-sends (leader retransmit, re-served
//! cards, withdrawals and intents); retransmits are not explored before
//! the continuation (a duplicate is the `max_dups` fault). Without an
//! older-build seat the free/locked/accepted-seen sets are not kept
//! (nobody is ever free there). Liveness = some fault-free continuation
//! seals (a fresh card `late` may appear in it), within `live_extra` more
//! ballots per seat, asked only while max(n-m+1, m) seats are alive.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use super::lock::{Acceptor, Ballot, Leader, Quorum, Reply, Slot, Value};

const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];
const H: u64 = 1;

fn slot() -> Slot {
    Slot { height: H, base: "l".into() }
}

fn name(i: usize) -> String {
    NAMES[i].to_string()
}

fn seat_of(n: &str) -> usize {
    NAMES.iter().position(|x| *x == n).expect("model seat name")
}

fn digest(c: usize) -> String {
    format!("v{c}")
}

fn card_of(d: &str) -> usize {
    d[1..].parse().expect("model digest")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fixes {
    pub f1: bool,
    pub f2: bool,
    pub f3: bool,
    pub f4: bool,
}

impl Fixes {
    const ALL: Fixes = Fixes { f1: true, f2: true, f3: true, f4: true };
}

#[derive(Clone, Debug)]
struct Card {
    proposer: usize,
    restore_of: Option<usize>,
    withdrawable: bool,
    /// Proposed only in a fault-free continuation: a fresh card the slot
    /// should still decide.
    late: bool,
}

#[derive(Clone, Debug)]
struct Config {
    n: usize,
    m: usize,
    cards: Vec<Card>,
    /// The restore subject starts dead (its card exists from the start);
    /// otherwise a restore card is proposed once its subject died.
    lost: bool,
    old: Option<usize>,
    max_ballots: u8,
    live_extra: u8,
    max_deaths: u8,
    max_revivals: u8,
    max_dups: u8,
    drops: bool,
    max_msgs: usize,
    max_states: usize,
    live_budget: usize,
    fixes: Fixes,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum P {
    Card(usize),
    Intent(usize),
    Withdraw(usize),
    FreeSig(usize),
    Prepare(u64),
    Promise(Ballot, Option<(Ballot, usize)>),
    Accept(u64, usize, bool),
    Accepted(Ballot, usize),
    Nack(Ballot, Option<Ballot>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Msg {
    to: usize,
    from: usize,
    inc: u8,
    p: P,
}

#[derive(Clone, Debug)]
struct Run {
    leader: Leader,
    /// Mirror of the leader's private promise map and free set (the state
    /// hash cannot read them); every leader call below updates both.
    promises: BTreeMap<usize, Option<(Ballot, usize)>>,
    free: BTreeSet<usize>,
    promised: BTreeSet<usize>,
    ids: BTreeSet<usize>,
    value: Option<usize>,
    s3_over: bool,
}

#[derive(Clone, Debug)]
struct Seat {
    alive: bool,
    inc: u8,
    acc: Acceptor,
    run: Option<Run>,
    known: BTreeSet<usize>,
    withdrawn: BTreeSet<usize>,
    intents: BTreeMap<usize, BTreeSet<usize>>,
    own: BTreeSet<usize>,
    free_seen: BTreeSet<usize>,
    locked_seen: BTreeSet<usize>,
    accepted_seen: BTreeSet<usize>,
    seen: Option<Ballot>,
    lock_known: BTreeSet<usize>,
    sigs: BTreeMap<(Ballot, usize), BTreeSet<usize>>,
    learned: Option<(Ballot, usize)>,
    stale: Vec<usize>,
    heard_subject: bool,
    retry: Option<(Ballot, usize, bool)>,
    sealed: Option<usize>,
    ballots: u8,
}

impl Seat {
    fn new(alive: bool) -> Self {
        Seat {
            alive,
            inc: 0,
            acc: Acceptor::default(),
            run: None,
            known: BTreeSet::new(),
            withdrawn: BTreeSet::new(),
            intents: BTreeMap::new(),
            own: BTreeSet::new(),
            free_seen: BTreeSet::new(),
            locked_seen: BTreeSet::new(),
            accepted_seen: BTreeSet::new(),
            seen: None,
            lock_known: BTreeSet::new(),
            sigs: BTreeMap::new(),
            learned: None,
            stale: Vec::new(),
            heard_subject: false,
            retry: None,
            sealed: None,
            ballots: 0,
        }
    }

    /// Volatile state goes with the process; acceptor and log stay.
    fn crash(&mut self) {
        self.alive = false;
        self.run = None;
        self.free_seen.clear();
        self.locked_seen.clear();
        self.accepted_seen.clear();
        self.seen = None;
        self.lock_known.clear();
        self.sigs.clear();
        self.learned = None;
        self.stale.clear();
        self.heard_subject = false;
        self.retry = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SealKind {
    Lock,
    Free,
}

#[derive(Clone, Debug, Hash)]
struct World {
    seats: Vec<Seat>,
    msgs: BTreeMap<Msg, u8>,
    born: Vec<bool>,
    withdrawn: Vec<bool>,
    /// History: every acceptance ever given, per ballot and card.
    hist: BTreeMap<(Ballot, usize), BTreeSet<usize>>,
    free_sigs: BTreeMap<usize, BTreeSet<usize>>,
    chosen: BTreeSet<usize>,
    sealed: BTreeMap<usize, SealKind>,
    deaths: u8,
    revivals: u8,
    dups: u8,
}

#[derive(Clone, Debug)]
enum Act {
    Deliver(Msg),
    Dup(Msg),
    Intent(usize, usize),
    Start(usize),
    Drive(usize),
    Retry(usize),
    Release(usize),
    LearnSeal(usize),
    FreeSeal(usize, usize),
    Withdraw(usize),
    /// A seat dies; with `true` every frame it still had in flight is lost.
    Die(usize, bool),
    Revive(usize),
    ProposeRestore(usize),
    Resend(Msg),
}

impl Hash for Run {
    fn hash<S: Hasher>(&self, h: &mut S) {
        self.leader.ballot.hash(h);
        self.leader.proposed().hash(h);
        self.promises.hash(h);
        self.free.hash(h);
        self.promised.hash(h);
        self.ids.hash(h);
        self.value.hash(h);
        self.s3_over.hash(h);
    }
}

impl Hash for Seat {
    fn hash<S: Hasher>(&self, h: &mut S) {
        let sl = slot();
        self.acc.promised(&sl).hash(h);
        self.acc.accepted(&sl).hash(h);
        self.acc.signed_freely(H).hash(h);
        (self.alive, self.inc, &self.run, &self.known, &self.withdrawn, &self.intents, &self.own).hash(h);
        (&self.free_seen, &self.locked_seen, &self.accepted_seen, &self.seen, &self.lock_known).hash(h);
        (&self.sigs, &self.learned, &self.stale, self.heard_subject, &self.retry, self.sealed, self.ballots).hash(h);
    }
}

/// Two multiply-rotate lanes: a 128-bit state fingerprint.
struct Fp(u64, u64);

impl Hasher for Fp {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            let x = u64::from_le_bytes(b);
            self.0 = (self.0 ^ x).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
            self.1 = (self.1 ^ x.rotate_left(17)).wrapping_mul(0xC2B2_AE3D_27D4_EB4F).rotate_left(31);
        }
        self.0 ^= 0xff;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn hash_world(w: &World) -> u128 {
    let mut f = Fp(0x243F_6A88_85A3_08D3, 0x1319_8A2E_0370_7344);
    w.hash(&mut f);
    let mix = |mut x: u64| {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^ (x >> 33)
    };
    (u128::from(mix(f.0)) << 64) | u128::from(mix(f.1 ^ f.0))
}

fn show_p(p: &P, cfg: &Config) -> String {
    let c = |c: &usize| cfg.card_name(*c);
    let b = |b: &Ballot| format!("{}:{}", b.round, b.leader);
    match p {
        P::Card(x) => format!("Card({})", c(x)),
        P::Intent(x) => format!("Intent({})", c(x)),
        P::Withdraw(x) => format!("Withdraw({})", c(x)),
        P::FreeSig(x) => format!("FreeSig({})", c(x)),
        P::Prepare(r) => format!("Prepare(r{r})"),
        P::Promise(bb, a) => match a {
            Some((ab, x)) => format!("Promise({}, holds {}@{})", b(bb), c(x), b(ab)),
            None => format!("Promise({})", b(bb)),
        },
        P::Accept(r, x, f) => format!("Accept(r{r}, {}{})", c(x), if *f { ", forced" } else { "" }),
        P::Accepted(bb, x) => format!("Accepted({}, {})", b(bb), c(x)),
        P::Nack(bb, p) => format!("Nack({}, {})", b(bb), p.as_ref().map_or("free".to_string(), b)),
    }
}

impl Config {
    fn q(&self) -> Quorum {
        Quorum { n: self.n, m: self.m }
    }

    fn card_name(&self, c: usize) -> String {
        match self.cards[c].restore_of {
            Some(z) => format!("R({})", NAMES[z]),
            None => ["X", "Y", "W", "V"][c.min(3)].to_string(),
        }
    }

    fn is_restore(&self, c: usize) -> bool {
        self.cards[c].restore_of.is_some()
    }

    fn need(&self, c: usize) -> usize {
        match self.cards[c].restore_of {
            Some(_) if self.fixes.f1 && self.m < self.n => self.m,
            Some(_) => self.m.saturating_sub(1).max(1),
            None => self.m,
        }
    }

    fn val(&self, c: usize) -> Value {
        Value { digest: digest(c), need: self.need(c) }
    }

    fn is_old(&self, i: usize) -> bool {
        self.old == Some(i)
    }

    fn show(&self, a: &Act) -> String {
        let s = |i: &usize| NAMES[*i].to_uppercase();
        match a {
            Act::Deliver(m) => format!("deliver {}->{} {}", s(&m.from), s(&m.to), show_p(&m.p, self)),
            Act::Dup(m) => format!("DUP {}->{} {}", s(&m.from), s(&m.to), show_p(&m.p, self)),
            Act::Intent(i, c) => format!("{} intent {}", s(i), self.card_name(*c)),
            Act::Start(i) => format!("{} opens a ballot", s(i)),
            Act::Drive(i) => format!("{} tick: propose (S3 timer over)", s(i)),
            Act::Retry(i) => format!("{} tick: retry withheld accept", s(i)),
            Act::Release(i) => format!("{} tick: release / free standing intents", s(i)),
            Act::LearnSeal(i) => format!("{} seals as learner", s(i)),
            Act::FreeSeal(i, c) => format!("{} seals {} from free signatures", s(i), self.card_name(*c)),
            Act::Withdraw(c) => format!("{} WITHDRAWS {}", s(&self.cards[*c].proposer), self.card_name(*c)),
            Act::Die(i, lose) => format!("{} DIES{}", s(i), if *lose { ", its frames in flight lost" } else { "" }),
            Act::Revive(i) => format!("{} REVIVES (old device)", s(i)),
            Act::ProposeRestore(c) => format!("{} proposes {}", s(&self.cards[*c].proposer), self.card_name(*c)),
            Act::Resend(m) => format!("resend {}->{} {}", s(&m.from), s(&m.to), show_p(&m.p, self)),
        }
    }

    // ---- views ----

    fn subjects(&self, w: &World, i: usize) -> BTreeSet<usize> {
        let s = &w.seats[i];
        let mut out = BTreeSet::new();
        for (c, card) in self.cards.iter().enumerate() {
            let Some(z) = card.restore_of else { continue };
            let registered = s.known.contains(&c) && !s.withdrawn.contains(&c);
            let held = s.acc.holds(&slot(), &self.val(c))
                || s.run.as_ref().is_some_and(|r| r.ids.contains(&c))
                || s.lock_known.contains(&c);
            if registered || held {
                out.insert(z);
            }
        }
        out
    }

    /// `proposal_change(id)` resolves: a known card; a restore only while registered.
    fn resolvable(&self, w: &World, i: usize, c: usize) -> bool {
        let s = &w.seats[i];
        s.known.contains(&c) && (!self.is_restore(c) || !s.withdrawn.contains(&c))
    }

    fn known_free(&self, w: &World, i: usize) -> Vec<usize> {
        let s = &w.seats[i];
        let subj = self.subjects(w, i);
        let (mut free, mut cascade): (Vec<usize>, Vec<usize>) = s
            .free_seen
            .iter()
            .filter(|m| **m != i && !subj.contains(*m) && !s.accepted_seen.contains(*m))
            .copied()
            .partition(|m| !s.locked_seen.contains(m));
        if self.q().lock_dead(free.len(), false) {
            free.append(&mut cascade);
        }
        free
    }

    fn lock_dead_at(&self, w: &World, i: usize) -> bool {
        self.q().lock_dead(self.known_free(w, i).len(), w.seats[i].acc.holds_acceptance(H))
    }

    fn locked_mode(&self, w: &World, i: usize) -> bool {
        if self.is_old(i) {
            return false;
        }
        let a = &w.seats[i].acc;
        if !a.may_sign_freely(H) {
            return true;
        }
        !(a.signed_freely(H) || self.lock_dead_at(w, i))
    }

    fn choose(&self, w: &World, i: usize, promised: Option<&BTreeSet<usize>>) -> Option<usize> {
        let order = w.seats[i].stale.clone();
        let stale: BTreeSet<usize> = order.iter().copied().collect();
        self.choose_skipping(w, i, promised, &stale).or_else(|| {
            order.iter().find_map(|id| {
                let others: BTreeSet<usize> = stale.iter().copied().filter(|x| x != id).collect();
                self.choose_skipping(w, i, promised, &others)
            })
        })
    }

    fn choose_skipping(
        &self,
        w: &World,
        i: usize,
        promised: Option<&BTreeSet<usize>>,
        skip: &BTreeSet<usize>,
    ) -> Option<usize> {
        let s = &w.seats[i];
        let subj = self.subjects(w, i);
        for c in 0..self.cards.len() {
            if skip.contains(&c) || !s.known.contains(&c) || s.withdrawn.contains(&c) {
                continue;
            }
            let voices = s.intents.get(&c).map_or(0, |v| {
                v.iter()
                    .filter(|m| !subj.contains(*m) && !s.free_seen.contains(*m))
                    .filter(|m| !self.fixes.f4 || promised.map_or(true, |p| p.contains(*m)))
                    .count()
            });
            if voices < self.m.saturating_sub(1).max(1) || voices < self.need(c) {
                continue;
            }
            if let Some(z) = self.cards[c].restore_of {
                if z == i || s.heard_subject {
                    continue;
                }
            }
            return Some(c);
        }
        None
    }

    fn ready(&self, w: &World, i: usize) -> bool {
        self.choose(w, i, None).is_some()
    }

    fn busy(run: &Run) -> bool {
        !run.leader.ready() || run.leader.forced().is_some() || run.leader.proposed().is_some()
    }

    // ---- effects ----

    fn send(&self, w: &mut World, from: usize, p: P, lock: bool) {
        for to in 0..self.n {
            if to == from || (lock && self.is_old(to)) || w.seats[to].sealed.is_some() {
                continue;
            }
            if !w.seats[to].alive && w.revivals >= self.max_revivals {
                continue;
            }
            let msg = Msg { to, from, inc: w.seats[from].inc, p: p.clone() };
            w.msgs.entry(msg).or_insert(1);
        }
    }

    fn sign_free(&self, w: &mut World, i: usize, c: usize) {
        if !self.is_old(i) && w.seats[i].acc.sign_freely(H).is_none() {
            return;
        }
        w.free_sigs.entry(c).or_default().insert(i);
        self.send(w, i, P::FreeSig(c), false);
    }

    fn sign_standing(&self, w: &mut World, i: usize) {
        let s = &w.seats[i];
        let standing: Vec<usize> = s
            .own
            .iter()
            .copied()
            .filter(|c| !s.withdrawn.contains(c))
            .filter(|c| !w.free_sigs.get(c).is_some_and(|f| f.contains(&i)))
            .collect();
        for c in standing {
            self.sign_free(w, i, c);
        }
    }

    fn try_release(&self, w: &mut World, i: usize) {
        if self.is_old(i) || w.seats[i].acc.signed_freely(H) || !self.lock_dead_at(w, i) {
            return;
        }
        let free = self.known_free(w, i).len();
        if !w.seats[i].acc.release_to_free(H, self.q(), free) {
            return;
        }
        w.seats[i].run = None;
        self.sign_standing(w, i);
    }

    fn saw(w: &mut World, i: usize, b: &Ballot) {
        let s = &mut w.seats[i];
        if s.seen.as_ref().map_or(true, |x| b > x) {
            s.seen = Some(b.clone());
        }
        if b.leader == name(i) {
            return;
        }
        if s.run.as_ref().is_some_and(|r| r.leader.ballot < *b) {
            s.run = None;
        }
    }

    fn heard_free(run: &mut Run, j: usize) {
        if !run.promises.contains_key(&j) {
            run.free.insert(j);
        }
        run.leader.heard_free(&name(j));
    }

    fn excuse(&self, w: &mut World, i: usize) {
        let Some(run) = w.seats[i].run.as_mut() else { return };
        let restores: Vec<(usize, usize)> =
            run.ids.iter().filter_map(|c| self.cards[*c].restore_of.map(|z| (*c, z))).collect();
        for (c, z) in restores {
            run.leader.excuse(&digest(c), &name(z));
        }
    }

    fn start(&self, w: &mut World, i: usize) {
        let known_free = self.known_free(w, i);
        let s = &mut w.seats[i];
        if let Some(run) = &s.run {
            if let (Some(c), None) = (run.value, run.leader.forced()) {
                s.stale.retain(|x| *x != c);
                s.stale.push(c);
            }
        }
        let seen = s.seen.as_ref().map_or(0, |b| b.round);
        let round = s.acc.next_round(&slot()).max(seen + 1);
        let ballot = Ballot { round, leader: name(i) };
        let Reply::Promise { accepted, .. } = s.acc.prepare(&slot(), &ballot).reply else { return };
        let mut run = Run {
            leader: Leader::new(slot(), self.q(), ballot.clone()),
            promises: BTreeMap::new(),
            free: BTreeSet::new(),
            promised: BTreeSet::from([i]),
            ids: BTreeSet::new(),
            value: None,
            s3_over: false,
        };
        if let Some((_, v)) = &accepted {
            run.ids.insert(card_of(&v.digest));
        }
        run.promises.insert(i, accepted.as_ref().map(|(b, v)| (b.clone(), card_of(&v.digest))));
        run.leader.promise(&name(i), &ballot, accepted);
        for m in known_free {
            Self::heard_free(&mut run, m);
        }
        s.seen = Some(ballot);
        s.run = Some(run);
        s.ballots += 1;
        self.excuse(w, i);
        self.send(w, i, P::Prepare(round), true);
        self.try_propose(w, i);
    }

    fn try_propose(&self, w: &mut World, i: usize) {
        if w.seats[i].sealed.is_some() {
            return;
        }
        let Some(run) = &w.seats[i].run else { return };
        if !run.leader.ready() || run.leader.proposed().is_some() {
            return;
        }
        let forced = run.leader.forced();
        let own = match &forced {
            Some(v) => {
                if !run.ids.contains(&card_of(&v.digest)) {
                    return;
                }
                if run.leader.heard() < self.n && !run.s3_over {
                    return;
                }
                None
            }
            None => {
                let promised = run.promised.clone();
                let Some(c) = self.choose(w, i, Some(&promised)) else { return };
                Some(self.val(c))
            }
        };
        let Some(run) = w.seats[i].run.as_mut() else { return };
        let Some(v) = run.leader.propose(own) else { return };
        let c = card_of(&v.digest);
        run.value = Some(c);
        let ballot = run.leader.ballot.clone();
        let is_forced = forced.is_some();
        self.send(w, i, P::Accept(ballot.round, c, is_forced), true);
        self.acceptor_accept(w, i, &ballot, c, is_forced);
    }

    fn withheld(&self, w: &World, i: usize, c: usize) -> bool {
        let s = &w.seats[i];
        if !s.own.contains(&c) || s.withdrawn.contains(&c) {
            return true;
        }
        self.cards[c].restore_of.is_some_and(|z| z == i || s.heard_subject)
    }

    fn acceptor_accept(&self, w: &mut World, i: usize, ballot: &Ballot, c: usize, forced: bool) {
        if w.seats[i].sealed.is_some() {
            return;
        }
        self.try_release(w, i);
        if self.is_restore(c) && !w.seats[i].known.contains(&c) {
            w.seats[i].known.insert(c);
        }
        let v = self.val(c);
        let s = &w.seats[i];
        let kept = s.acc.holds(&slot(), &v);
        let approver = forced
            && !kept
            && (self.fixes.f3
                || (s.known.contains(&c)
                    && s.withdrawn.contains(&c)
                    && s.intents.get(&c).is_some_and(|x| x.contains(&i))));
        let resolvable = self.resolvable(w, i, c) || kept || (approver && self.is_restore(c));
        if !resolvable {
            w.seats[i].retry = Some((ballot.clone(), c, forced));
            return;
        }
        w.seats[i].lock_known.insert(c);
        let withheld = !kept && !approver && self.withheld(w, i, c);
        if withheld {
            w.seats[i].retry = Some((ballot.clone(), c, forced));
            return;
        }
        w.seats[i].retry = None;
        match w.seats[i].acc.accept(&slot(), ballot, &v).reply {
            Reply::Accepted { .. } => {
                w.hist.entry((ballot.clone(), c)).or_default().insert(i);
                self.send(w, i, P::Accepted(ballot.clone(), c), true);
                self.count(w, i, i, ballot, c);
            }
            Reply::Nack { promised, .. } => self.send(w, i, P::Nack(ballot.clone(), promised), true),
            Reply::Promise { .. } => {}
        }
    }

    fn count(&self, w: &mut World, i: usize, from: usize, ballot: &Ballot, c: usize) {
        if self.is_restore(c) && self.subjects(w, i).contains(&from) {
            return;
        }
        let set = w.seats[i].sigs.entry((ballot.clone(), c)).or_default();
        set.insert(from);
        if set.len() < self.need(c) {
            return;
        }
        if ballot.leader == name(i) {
            self.seal(w, i, ballot, c);
        } else if w.seats[i].learned.is_none() {
            w.seats[i].learned = Some((ballot.clone(), c));
        }
    }

    fn seal(&self, w: &mut World, i: usize, ballot: &Ballot, c: usize) {
        if w.seats[i].sealed.is_some() {
            return;
        }
        if let Some(z) = self.cards[c].restore_of {
            if z == i || w.seats[i].heard_subject {
                w.seats[i].learned = Some((ballot.clone(), c));
                return;
            }
        }
        let subj = self.subjects(w, i);
        let restore = self.is_restore(c);
        let have = w.seats[i]
            .sigs
            .get(&(ballot.clone(), c))
            .map_or(0, |s| s.iter().filter(|m| !(restore && subj.contains(*m))).count());
        if have < self.need(c) {
            return;
        }
        w.seats[i].sealed = Some(c);
        w.seats[i].learned = None;
        w.sealed.entry(c).or_insert(SealKind::Lock);
    }

    /// Without an older-build seat nobody is ever free, so the seen-sets
    /// can only feed an empty free count: not tracked (state space).
    fn note_seen(&self, w: &mut World, i: usize, j: usize, accepting: bool) {
        if self.old.is_some() {
            w.seats[i].locked_seen.insert(j);
            if accepting {
                w.seats[i].accepted_seen.insert(j);
            }
        }
    }

    fn note_free(&self, w: &mut World, i: usize, j: usize) {
        if j != i && w.seats[i].free_seen.insert(j) {
            self.try_release(w, i);
        }
    }

    fn intent(&self, w: &mut World, i: usize, c: usize) {
        w.seats[i].own.insert(c);
        if self.is_old(i) {
            self.sign_free(w, i, c);
            return;
        }
        w.seats[i].intents.entry(c).or_default().insert(i);
        if self.locked_mode(w, i) {
            self.send(w, i, P::Intent(c), false);
            self.try_propose(w, i);
        } else {
            self.sign_free(w, i, c);
        }
    }

    fn deliver(&self, w: &mut World, m: &Msg) {
        let i = m.to;
        let j = m.from;
        if !w.seats[i].alive || w.seats[i].sealed.is_some() {
            return;
        }
        if m.inc > 0 && self.cards.iter().any(|c| c.restore_of == Some(j)) {
            w.seats[i].heard_subject = true;
        }
        match &m.p {
            P::Card(c) => {
                w.seats[i].known.insert(*c);
            }
            P::Withdraw(c) => {
                w.seats[i].withdrawn.insert(*c);
                w.seats[i].own.remove(c);
            }
            _ if self.is_old(i) => {}
            P::Intent(c) => {
                w.seats[i].intents.entry(*c).or_default().insert(j);
                self.try_propose(w, i);
            }
            P::FreeSig(c) => {
                w.seats[i].intents.entry(*c).or_default().insert(j);
                if w.seats[i].known.contains(c) {
                    self.note_free(w, i, j);
                }
            }
            P::Prepare(r) => {
                let b = Ballot { round: *r, leader: name(j) };
                self.note_seen(w, i, j, false);
                Self::saw(w, i, &b);
                self.try_release(w, i);
                match w.seats[i].acc.prepare(&slot(), &b).reply {
                    Reply::Promise { accepted, .. } => {
                        let acc = accepted.map(|(bb, v)| (bb, card_of(&v.digest)));
                        self.send(w, i, P::Promise(b, acc), true);
                    }
                    Reply::Nack { promised, .. } => self.send(w, i, P::Nack(b, promised), true),
                    Reply::Accepted { .. } => {}
                }
            }
            P::Promise(b, acc) => {
                self.note_seen(w, i, j, acc.is_some());
                Self::saw(w, i, b);
                if b.leader != name(i) || self.subjects(w, i).contains(&j) {
                    return;
                }
                let Some(run) = w.seats[i].run.as_mut() else { return };
                if run.leader.ballot != *b {
                    return;
                }
                if let Some((_, c)) = acc {
                    run.ids.insert(*c);
                }
                run.promised.insert(j);
                run.free.remove(&j);
                run.promises.insert(j, acc.clone());
                run.leader.promise(&name(j), b, acc.as_ref().map(|(bb, c)| (bb.clone(), self.val(*c))));
                self.excuse(w, i);
                self.try_propose(w, i);
            }
            P::Accept(r, c, f) => {
                let b = Ballot { round: *r, leader: name(j) };
                self.note_seen(w, i, j, true);
                Self::saw(w, i, &b);
                self.acceptor_accept(w, i, &b, *c, *f);
            }
            P::Accepted(b, c) => {
                self.note_seen(w, i, j, true);
                Self::saw(w, i, b);
                if !(w.seats[i].lock_known.contains(c) || self.resolvable(w, i, *c)) {
                    return;
                }
                w.seats[i].lock_known.insert(*c);
                self.count(w, i, j, b, *c);
            }
            P::Nack(b, p) => match p {
                Some(pb) => {
                    self.note_seen(w, i, j, false);
                    Self::saw(w, i, pb);
                }
                None => {
                    if b.leader == name(i) {
                        if let Some(run) = w.seats[i].run.as_mut() {
                            Self::heard_free(run, j);
                        }
                    }
                    self.note_free(w, i, j);
                    self.try_propose(w, i);
                }
            },
        }
    }

    fn update_chosen(&self, w: &mut World) {
        for ((_, c), who) in &w.hist {
            let z = self.cards[*c].restore_of;
            if who.iter().filter(|s| Some(**s) != z).count() >= self.need(*c) {
                w.chosen.insert(*c);
            }
        }
    }

    // ---- the transition relation ----

    fn may_start(&self, w: &World, i: usize, live: bool) -> bool {
        let s = &w.seats[i];
        let cap = if live { self.max_ballots + self.live_extra } else { self.max_ballots };
        if self.is_old(i) || s.ballots >= cap || !self.locked_mode(w, i) {
            return false;
        }
        let subjects = self.subjects(w, i);
        let subject = subjects.contains(&i);
        if subject && self.fixes.f2 {
            return false;
        }
        match &s.run {
            Some(run) => Self::busy(run),
            None if subject => self.ready(w, i),
            None if !live => true,
            None => {
                let rank0 = (0..self.n).find(|m| w.seats[*m].alive && !subjects.contains(m)) == Some(i);
                rank0 || self.ready(w, i)
            }
        }
    }

    fn apply(&self, w: &World, act: &Act) -> World {
        let mut w = w.clone();
        match act {
            Act::Deliver(m) => {
                take(&mut w, m);
                self.deliver(&mut w, m);
            }
            Act::Dup(m) => {
                w.dups += 1;
                *w.msgs.entry(m.clone()).or_insert(0) += 1;
            }
            Act::Intent(i, c) => self.intent(&mut w, *i, *c),
            Act::Start(i) => self.start(&mut w, *i),
            Act::Drive(i) => {
                if let Some(run) = w.seats[*i].run.as_mut() {
                    run.s3_over = true;
                }
                self.try_propose(&mut w, *i);
            }
            Act::Retry(i) => {
                if let Some((b, c, f)) = w.seats[*i].retry.take() {
                    self.acceptor_accept(&mut w, *i, &b, c, f);
                }
            }
            Act::Release(i) => {
                self.try_release(&mut w, *i);
                if !self.locked_mode(&w, *i) {
                    self.sign_standing(&mut w, *i);
                }
            }
            Act::LearnSeal(i) => {
                if let Some((b, c)) = w.seats[*i].learned.clone() {
                    self.seal(&mut w, *i, &b, c);
                }
            }
            Act::FreeSeal(i, c) => {
                w.seats[*i].sealed = Some(*c);
                w.sealed.entry(*c).or_insert(SealKind::Free);
            }
            Act::Withdraw(c) => {
                let p = self.cards[*c].proposer;
                w.withdrawn[*c] = true;
                w.seats[p].withdrawn.insert(*c);
                w.seats[p].own.remove(c);
                self.send(&mut w, p, P::Withdraw(*c), false);
            }
            Act::Die(i, lose) => {
                w.deaths += 1;
                w.seats[*i].crash();
                if *lose {
                    w.msgs.retain(|m, _| m.from != *i);
                }
                if w.revivals >= self.max_revivals {
                    w.msgs.retain(|m, _| m.to != *i);
                }
            }
            Act::Revive(i) => {
                w.revivals += 1;
                w.seats[*i].alive = true;
                w.seats[*i].inc += 1;
                if w.revivals >= self.max_revivals {
                    let dead: Vec<usize> = (0..self.n).filter(|s| !w.seats[*s].alive).collect();
                    w.msgs.retain(|m, _| !dead.contains(&m.to));
                }
            }
            Act::ProposeRestore(c) if self.cards[*c].late => {
                w.born[*c] = true;
                for s in w.seats.iter_mut().filter(|s| s.alive) {
                    s.known.insert(*c);
                }
            }
            Act::ProposeRestore(c) => {
                let p = self.cards[*c].proposer;
                w.born[*c] = true;
                w.seats[p].known.insert(*c);
                self.send(&mut w, p, P::Card(*c), false);
            }
            Act::Resend(m) => {
                w.msgs.entry(m.clone()).or_insert(1);
            }
        }
        self.update_chosen(&mut w);
        w
    }

    fn actions(&self, w: &World, live: bool) -> Vec<Act> {
        let mut out = Vec::new();
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() {
                continue;
            }
            if s.learned.is_some() && self.locked_mode(w, i) {
                out.push(Act::LearnSeal(i));
            }
            if self.is_old(i) || !self.locked_mode(w, i) {
                for (c, f) in &w.free_sigs {
                    let z = self.cards[*c].restore_of;
                    let have = f.iter().filter(|x| Some(**x) != z).count();
                    let need = if z.is_some() { self.m.saturating_sub(1).max(1) } else { self.m };
                    if have >= need && s.known.contains(c) {
                        out.push(Act::FreeSeal(i, *c));
                    }
                }
            }
            if s.run.as_ref().is_some_and(|r| r.leader.ready() && r.leader.proposed().is_none()) {
                out.push(Act::Drive(i));
            }
        }
        for m in w.msgs.keys() {
            out.push(Act::Deliver(m.clone()));
        }
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() {
                continue;
            }
            if s.retry.is_some() && self.locked_mode(w, i) {
                out.push(Act::Retry(i));
            }
            if self.old.is_some() && !self.is_old(i) {
                out.push(Act::Release(i));
            }
            for c in 0..self.cards.len() {
                if w.born[c]
                    && s.known.contains(&c)
                    && !s.withdrawn.contains(&c)
                    && !s.own.contains(&c)
                    && self.cards[c].restore_of != Some(i)
                {
                    out.push(Act::Intent(i, c));
                }
            }
        }
        for (c, card) in self.cards.iter().enumerate() {
            if let Some(z) = card.restore_of {
                if !w.born[c] && !w.seats[z].alive && w.seats[card.proposer].alive {
                    out.push(Act::ProposeRestore(c));
                }
            } else if card.late && live && !w.born[c] {
                out.push(Act::ProposeRestore(c));
            }
        }
        if live {
            self.resends(w, &mut out);
        }
        for i in 0..self.n {
            if w.seats[i].alive && w.seats[i].sealed.is_none() && self.may_start(w, i, live) {
                out.push(Act::Start(i));
            }
        }
        if !live {
            for (m, k) in &w.msgs {
                if w.dups < self.max_dups && *k == 1 {
                    out.push(Act::Dup(m.clone()));
                }
            }
            for (c, card) in self.cards.iter().enumerate() {
                if card.withdrawable && w.born[c] && !w.withdrawn[c] && w.seats[card.proposer].alive {
                    out.push(Act::Withdraw(c));
                }
            }
            for i in 0..self.n {
                if w.seats[i].alive && w.deaths < self.max_deaths {
                    out.push(Act::Die(i, false));
                    if self.drops && w.msgs.keys().any(|m| m.from == i) {
                        out.push(Act::Die(i, true));
                    }
                }
                if !w.seats[i].alive && w.revivals < self.max_revivals {
                    out.push(Act::Revive(i));
                }
            }
        }
        out
    }

    /// Fault-free continuation only: the leader's retransmit (§7, S4) and
    /// the re-serve of open cards, withdrawals and own intents.
    fn resends(&self, w: &World, out: &mut Vec<Act>) {
        let inflight = |m: &Msg| w.msgs.contains_key(m);
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() {
                continue;
            }
            let mut push = |to: usize, p: P| {
                let m = Msg { to, from: i, inc: s.inc, p };
                if to != i && w.seats[to].alive && w.seats[to].sealed.is_none() && !inflight(&m) {
                    out.push(Act::Resend(m));
                }
            };
            if let Some(run) = s.run.as_ref().filter(|r| Self::busy(r)) {
                let b = &run.leader.ballot;
                let p = match (run.leader.proposed(), run.value) {
                    (Some(_), Some(c)) => Some(P::Accept(b.round, c, run.leader.forced().is_some())),
                    (None, _) if !run.leader.ready() => Some(P::Prepare(b.round)),
                    _ => None,
                };
                if let Some(p) = p {
                    for to in (0..self.n).filter(|t| !self.is_old(*t)) {
                        push(to, p.clone());
                    }
                }
            }
            for c in 0..self.cards.len() {
                if !w.born[c] || !s.known.contains(&c) {
                    continue;
                }
                for to in 0..self.n {
                    if s.withdrawn.contains(&c) {
                        if !w.seats[to].withdrawn.contains(&c) {
                            push(to, P::Withdraw(c));
                        }
                    } else if !w.seats[to].known.contains(&c) {
                        push(to, P::Card(c));
                    }
                }
            }
            for c in &s.own {
                let free = w.free_sigs.get(c).is_some_and(|f| f.contains(&i));
                for to in 0..self.n {
                    if !w.seats[to].intents.get(c).is_some_and(|x| x.contains(&i))
                        || (free && !w.seats[to].free_seen.contains(&i))
                    {
                        push(to, if free { P::FreeSig(*c) } else { P::Intent(*c) });
                    }
                }
            }
        }
    }

    fn init(&self) -> World {
        let mut w = World {
            seats: (0..self.n).map(|_| Seat::new(true)).collect(),
            msgs: BTreeMap::new(),
            born: vec![false; self.cards.len()],
            withdrawn: vec![false; self.cards.len()],
            hist: BTreeMap::new(),
            free_sigs: BTreeMap::new(),
            chosen: BTreeSet::new(),
            sealed: BTreeMap::new(),
            deaths: 0,
            revivals: 0,
            dups: 0,
        };
        for card in &self.cards {
            if let (Some(z), true) = (card.restore_of, self.lost) {
                w.seats[z].alive = false;
            }
        }
        for (c, card) in self.cards.iter().enumerate() {
            if (card.restore_of.is_none() && !card.late) || (card.restore_of.is_some() && self.lost) {
                w.born[c] = true;
                w.seats[card.proposer].known.insert(c);
                self.send(&mut w, card.proposer, P::Card(c), false);
            }
        }
        w
    }

    fn violation(&self, w: &World) -> Option<&'static str> {
        let lock: BTreeSet<usize> = w
            .chosen
            .iter()
            .copied()
            .chain(w.sealed.iter().filter(|(_, k)| **k == SealKind::Lock).map(|(c, _)| *c))
            .collect();
        let free: BTreeSet<usize> = w.sealed.iter().filter(|(_, k)| **k == SealKind::Free).map(|(c, _)| *c).collect();
        if lock.len() >= 2 {
            Some("lock-lock")
        } else if !lock.is_empty() && free.iter().any(|c| !lock.contains(c)) {
            Some("lock-free")
        } else if free.len() >= 2 {
            Some("free-free")
        } else {
            None
        }
    }

    /// Liveness is asked only where enough seats live to decide anything.
    fn liveness_applies(&self, w: &World) -> bool {
        let alive = w.seats.iter().filter(|s| s.alive).count();
        w.sealed.is_empty() && alive >= self.q().promises().max(self.m)
    }
}

fn take(w: &mut World, m: &Msg) {
    if let Some(k) = w.msgs.get_mut(m) {
        *k -= 1;
        if *k == 0 {
            w.msgs.remove(m);
        }
    }
}

#[derive(Debug, Default)]
struct Report {
    states: usize,
    edges: usize,
    pruned: usize,
    complete: bool,
    violations: BTreeMap<&'static str, (usize, Vec<String>)>,
    halts: usize,
    halt_trace: Option<Vec<String>>,
    live_unknown: usize,
}

struct Live {
    live: HashSet<u128>,
    dead: HashSet<u128>,
}

impl Config {
    /// Whether some fault-free continuation of `w` seals. `None` = budget.
    fn can_seal(&self, memo: &mut Live, w: &World) -> Option<bool> {
        let h0 = hash_world(w);
        if !w.sealed.is_empty() || memo.live.contains(&h0) {
            return Some(true);
        }
        if memo.dead.contains(&h0) {
            return Some(false);
        }
        let mut visited: HashSet<u128> = HashSet::from([h0]);
        let mut stack: Vec<(u128, World, Vec<Act>)> = vec![(h0, w.clone(), self.actions(w, true))];
        while let Some((_, top, acts)) = stack.last_mut() {
            let Some(act) = acts.pop() else {
                stack.pop();
                continue;
            };
            let next = self.apply(top, &act);
            let h = hash_world(&next);
            if !next.sealed.is_empty() || memo.live.contains(&h) {
                memo.live.extend(stack.iter().map(|(h, _, _)| *h));
                memo.live.insert(h);
                return Some(true);
            }
            if memo.dead.contains(&h) || !visited.insert(h) {
                continue;
            }
            if visited.len() > self.live_budget {
                return None;
            }
            let mut acts = self.actions(&next, true);
            // the vector is popped from its end: progress first
            acts.reverse();
            stack.push((h, next, acts));
        }
        memo.dead.extend(visited);
        Some(false)
    }

    fn explore(&self, liveness: bool) -> Report {
        let mut r = Report::default();
        let init = self.init();
        let h0 = hash_world(&init);
        let mut parent: HashMap<u128, (u128, Act)> = HashMap::new();
        let mut seen: HashSet<u128> = HashSet::from([h0]);
        let mut queue: VecDeque<(u128, World)> = VecDeque::from([(h0, init)]);
        let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
        let trace = |parent: &HashMap<u128, (u128, Act)>, mut h: u128| {
            let mut steps = Vec::new();
            while let Some((p, a)) = parent.get(&h) {
                steps.push(self.show(a));
                h = *p;
            }
            steps.reverse();
            steps
        };
        r.complete = true;
        while let Some((h, w)) = queue.pop_front() {
            r.states += 1;
            if r.states % 200_000 == 0 && std::env::var_os("LOCK_EXPLORE_PROGRESS").is_some() {
                eprintln!(
                    "progress states={} queue={} seen={} live={} dead={}",
                    r.states,
                    queue.len(),
                    seen.len(),
                    memo.live.len(),
                    memo.dead.len()
                );
            }
            if let Some(kind) = self.violation(&w) {
                let e = r.violations.entry(kind).or_insert_with(|| (0, trace(&parent, h)));
                e.0 += 1;
                continue;
            }
            if liveness && self.liveness_applies(&w) {
                match self.can_seal(&mut memo, &w) {
                    Some(true) => {}
                    Some(false) => {
                        r.halts += 1;
                        if r.halt_trace.is_none() {
                            r.halt_trace = Some(trace(&parent, h));
                        }
                    }
                    None => r.live_unknown += 1,
                }
            }
            for act in self.actions(&w, false) {
                let next = self.apply(&w, &act);
                let hn = hash_world(&next);
                if hn == h {
                    continue;
                }
                r.edges += 1;
                if !seen.insert(hn) {
                    continue;
                }
                if next.msgs.values().map(|k| usize::from(*k)).sum::<usize>() > self.max_msgs {
                    r.pruned += 1;
                    continue;
                }
                parent.insert(hn, (h, act));
                if seen.len() > self.max_states {
                    r.complete = false;
                    continue;
                }
                queue.push_back((hn, next));
            }
        }
        r
    }
}

// ---- shapes ----

fn base(n: usize, m: usize, cards: Vec<Card>) -> Config {
    Config {
        n,
        m,
        cards,
        lost: false,
        old: None,
        max_ballots: 1,
        live_extra: 2,
        max_deaths: 1,
        max_revivals: 0,
        max_dups: 0,
        drops: false,
        max_msgs: 12,
        max_states: 400_000,
        live_budget: 200_000,
        fixes: Fixes::default(),
    }
}

fn card(proposer: usize, withdrawable: bool) -> Card {
    Card { proposer, restore_of: None, withdrawable, late: false }
}

fn late(proposer: usize) -> Card {
    Card { proposer, restore_of: None, withdrawable: false, late: true }
}

fn restore(proposer: usize, subject: usize) -> Card {
    Card { proposer, restore_of: Some(subject), withdrawable: false, late: false }
}

/// A withdrawable card, a standing one and one lost seat (H1's shape).
fn shape_withdraw(n: usize, m: usize) -> Config {
    let mut c = base(n, m, vec![card(1, true), late(0)]);
    c.max_ballots = 1;
    c
}

/// A lost last seat whose old device may come back, its restore and an
/// ordinary card (H2's shape).
fn shape_restore(n: usize, m: usize) -> Config {
    let mut c = base(n, m, vec![restore(0, n - 1), card(1, false), late(0)]);
    c.lost = true;
    c.max_revivals = 1;
    c.drops = true;
    c
}

/// A restore its proposer may withdraw, beside an ordinary card: the
/// subject's registration then lives only in what the lock holds (R2).
fn shape_restore_withdraw(n: usize, m: usize) -> Config {
    let mut c = shape_restore(n, m);
    c.cards[0].withdrawable = true;
    c
}

/// An older-build seat (free signatures only) beside the lock (§14).
fn shape_old(n: usize, m: usize) -> Config {
    let mut c = base(n, m, vec![card(0, false), card(n - 1, false), late(0)]);
    c.old = Some(n - 1);
    c.max_deaths = 0;
    c
}

fn shape(name: &str, n: usize, m: usize) -> Config {
    match name {
        "withdraw" => shape_withdraw(n, m),
        "restore" => shape_restore(n, m),
        "restore_withdraw" => shape_restore_withdraw(n, m),
        "old" => shape_old(n, m),
        other => panic!("unknown shape {other}"),
    }
}

fn summary(cfg: &Config, r: &Report) -> String {
    let mut s = format!(
        "n={} m={} fixes={:?}: states={} edges={} pruned={} complete={} halts={} live_unknown={}",
        cfg.n, cfg.m, cfg.fixes, r.states, r.edges, r.pruned, r.complete, r.halts, r.live_unknown
    );
    for (k, (count, t)) in &r.violations {
        let _ = write!(s, "\n  VIOLATION {k} x{count}:");
        for (i, step) in t.iter().enumerate() {
            let _ = write!(s, "\n    {:2}. {step}", i + 1);
        }
    }
    if let Some(t) = &r.halt_trace {
        let _ = write!(s, "\n  HALT (no fault-free continuation seals):");
        for (i, step) in t.iter().enumerate() {
            let _ = write!(s, "\n    {:2}. {step}", i + 1);
        }
    }
    s
}

#[test]
fn exhaustive_search_finds_the_withdrawn_forced_halt() {
    let cfg = shape_withdraw(3, 2);
    let r = cfg.explore(true);
    eprintln!("{}", summary(&cfg, &r));
    assert!(r.complete);
    assert!(r.halts > 0, "H1 not found");
}

#[test]
fn exhaustive_search_finds_the_revived_subject_fork() {
    let cfg = shape_restore(3, 2);
    let r = cfg.explore(false);
    eprintln!("{}", summary(&cfg, &r));
    assert!(r.complete);
    assert!(r.violations.contains_key("lock-lock"), "H2 not found");
}

fn env_u(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.parse().ok()
}

/// `LOCK_EXPLORE="withdraw:3:2:0000 restore:3:2:1111"` (fixes = F1..F4
/// bits); bounds from `LOCK_BALLOTS`, `LOCK_DEATHS`, `LOCK_REVIVALS`,
/// `LOCK_DUPS`, `LOCK_DROPS`, `LOCK_MSGS`, `LOCK_STATES`, `LOCK_LIVE`.
#[test]
#[ignore = "runner: LOCK_EXPLORE=... cargo test --release -p molt-engine --lib explore_runner -- --ignored --nocapture"]
fn explore_runner() {
    let spec = std::env::var("LOCK_EXPLORE").unwrap_or_default();
    for item in spec.split_whitespace() {
        let parts: Vec<&str> = item.split(':').collect();
        let (n, m) = (parts[1].parse().expect("n"), parts[2].parse().expect("m"));
        let mut cfg = shape(parts[0], n, m);
        let bits: Vec<bool> = parts.get(3).unwrap_or(&"0000").chars().map(|c| c == '1').collect();
        cfg.fixes = Fixes { f1: bits[0], f2: bits[1], f3: bits[2], f4: bits[3] };
        let u8_of = |k: &str| env_u(k).map(|v| u8::try_from(v).expect("small bound"));
        if let Some(v) = u8_of("LOCK_BALLOTS") {
            cfg.max_ballots = v;
        }
        if let Some(v) = u8_of("LOCK_DEATHS") {
            cfg.max_deaths = v;
        }
        if let Some(v) = u8_of("LOCK_REVIVALS") {
            cfg.max_revivals = v;
        }
        if let Some(v) = u8_of("LOCK_DUPS") {
            cfg.max_dups = v;
        }
        if let Some(v) = env_u("LOCK_DROPS") {
            cfg.drops = v != 0;
        }
        if let Some(v) = env_u("LOCK_MSGS") {
            cfg.max_msgs = v;
        }
        if let Some(v) = env_u("LOCK_STATES") {
            cfg.max_states = v;
        }
        let live = env_u("LOCK_LIVE").map_or(true, |v| v != 0);
        let t = std::time::Instant::now();
        let r = cfg.explore(live);
        eprintln!("[{item}] {:.1?}\n{}", t.elapsed(), summary(&cfg, &r));
    }
}

/// One line per (shape, n, m, fix set): `L` lock-lock, `F` lock-free,
/// `f` free-free violations, `H` halts, `!` incomplete (a bound hit).
/// `LOCK_MATRIX="withdraw restore" LOCK_SIZES="3:2 4:3"`.
#[test]
#[ignore = "runner: LOCK_MATRIX=... LOCK_SIZES=... cargo test --release -p molt-engine --lib explore_matrix -- --ignored --nocapture"]
fn explore_matrix() {
    let shapes = std::env::var("LOCK_MATRIX").unwrap_or_default();
    let sizes = std::env::var("LOCK_SIZES").unwrap_or_else(|_| "2:2 3:2 3:3 4:2 4:3".into());
    let sets = [
        ("none", Fixes::default()),
        ("F1", Fixes { f1: true, ..Fixes::default() }),
        ("F2", Fixes { f2: true, ..Fixes::default() }),
        ("F3", Fixes { f3: true, ..Fixes::default() }),
        ("F4", Fixes { f4: true, ..Fixes::default() }),
        ("all", Fixes::ALL),
    ];
    for sh in shapes.split_whitespace() {
        for size in sizes.split_whitespace() {
            let (n, m) = size.split_once(':').expect("n:m");
            let (n, m): (usize, usize) = (n.parse().expect("n"), m.parse().expect("m"));
            for (label, fixes) in sets {
                let mut cfg = shape(sh, n, m);
                cfg.fixes = fixes;
                if let Some(v) = env_u("LOCK_STATES") {
                    cfg.max_states = v;
                }
                let t = std::time::Instant::now();
                let r = cfg.explore(true);
                let mut marks = String::new();
                for (k, mark) in [("lock-lock", 'L'), ("lock-free", 'F'), ("free-free", 'f')] {
                    if r.violations.contains_key(k) {
                        marks.push(mark);
                    }
                }
                if r.halts > 0 {
                    marks.push('H');
                }
                if !r.complete || r.live_unknown > 0 {
                    marks.push('!');
                }
                eprintln!(
                    "{sh:16} {n}-{m} {label:4} {:6} states={} halts={} unknown={} {:.1?}",
                    if marks.is_empty() { "ok" } else { marks.as_str() },
                    r.states,
                    r.halts,
                    r.live_unknown,
                    t.elapsed()
                );
                if label == "all" && !marks.is_empty() {
                    eprintln!("{}", summary(&cfg, &r));
                }
            }
        }
    }
}
