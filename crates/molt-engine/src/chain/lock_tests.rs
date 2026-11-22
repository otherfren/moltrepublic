// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

fn slot(h: u64) -> Slot {
    Slot { height: h, base: format!("base-{h}") }
}

fn b(round: u64, leader: &str) -> Ballot {
    Ballot { round, leader: leader.to_string() }
}

fn v(d: &str, need: usize) -> Value {
    Value { digest: d.to_string(), need }
}

// ---- acceptor rules ----

#[test]
fn a_promise_raises_and_a_lower_ballot_is_refused() {
    let mut a = Acceptor::default();
    let s = slot(1);
    let step = a.prepare(&s, &b(2, "x"));
    assert!(step.persist);
    assert_eq!(step.reply, Reply::Promise { slot: s.clone(), ballot: b(2, "x"), accepted: None });
    let low = a.prepare(&s, &b(1, "z"));
    assert_eq!(low.reply, Reply::Nack { slot: s.clone(), ballot: b(1, "z"), promised: Some(b(2, "x")) });
    assert!(!low.persist);
    // same round, lower name: still lower
    assert!(matches!(a.prepare(&s, &b(2, "a")).reply, Reply::Nack { .. }));
}

#[test]
fn a_repeated_prepare_is_answered_without_a_write() {
    let mut a = Acceptor::default();
    let s = slot(1);
    assert!(a.prepare(&s, &b(1, "x")).persist);
    let again = a.prepare(&s, &b(1, "x"));
    assert!(!again.persist);
    assert!(matches!(again.reply, Reply::Promise { .. }));
}

#[test]
fn accept_needs_a_ballot_at_or_above_the_promise() {
    let mut a = Acceptor::default();
    let s = slot(1);
    a.prepare(&s, &b(3, "y"));
    assert!(matches!(a.accept(&s, &b(2, "x"), &v("p", 2)).reply, Reply::Nack { .. }));
    let ok = a.accept(&s, &b(3, "y"), &v("p", 2));
    assert!(ok.persist);
    assert!(matches!(ok.reply, Reply::Accepted { .. }));
    // a higher accept without a prepare raises the promise too
    a.accept(&s, &b(5, "x"), &v("q", 2));
    assert_eq!(a.promised(&s), Some(&b(5, "x")));
    assert_eq!(a.accepted(&s), Some(&(b(5, "x"), v("q", 2))));
}

#[test]
fn one_value_per_ballot() {
    let mut a = Acceptor::default();
    let s = slot(1);
    a.accept(&s, &b(1, "x"), &v("p", 2));
    assert!(matches!(a.accept(&s, &b(1, "x"), &v("q", 2)).reply, Reply::Nack { .. }));
    let dup = a.accept(&s, &b(1, "x"), &v("p", 2));
    assert!(matches!(dup.reply, Reply::Accepted { .. }));
    assert!(!dup.persist);
}

#[test]
fn a_promise_reports_the_accepted_pair() {
    let mut a = Acceptor::default();
    let s = slot(4);
    a.accept(&s, &b(1, "x"), &v("p", 2));
    let step = a.prepare(&s, &b(2, "y"));
    assert_eq!(
        step.reply,
        Reply::Promise { slot: s, ballot: b(2, "y"), accepted: Some((b(1, "x"), v("p", 2))) }
    );
}

#[test]
fn slots_on_different_lineages_are_independent() {
    let mut a = Acceptor::default();
    let one = Slot { height: 3, base: "a".into() };
    let two = Slot { height: 3, base: "b".into() };
    a.prepare(&one, &b(9, "x"));
    assert!(matches!(a.prepare(&two, &b(1, "y")).reply, Reply::Promise { .. }));
}

#[test]
fn free_signing_and_the_lock_exclude_each_other_per_height() {
    let mut a = Acceptor::default();
    assert_eq!(a.sign_freely(5), Some(true));
    assert_eq!(a.sign_freely(5), Some(false));
    let s = Slot { height: 5, base: "any".into() };
    assert_eq!(a.prepare(&s, &b(1, "x")).reply, Reply::Nack { slot: s.clone(), ballot: b(1, "x"), promised: None });
    assert!(matches!(a.accept(&s, &b(1, "x"), &v("p", 2)).reply, Reply::Nack { promised: None, .. }));
    // the other direction: lock state at 6 forbids a free signature there
    a.prepare(&slot(6), &b(1, "x"));
    assert!(!a.may_sign_freely(6));
    assert_eq!(a.sign_freely(6), None);
    assert!(a.may_sign_freely(7));
}

#[test]
fn state_is_released_into_a_free_mark_only_where_the_lock_is_dead() {
    let q23 = Quorum { n: 3, m: 2 };
    let mut a = Acceptor::default();
    a.prepare(&slot(3), &b(2, "x"));
    assert!(!a.release_to_free(3, q23, 0), "no free seat known");
    assert!(a.release_to_free(3, q23, 1));
    assert!(a.signed_freely(3));
    assert!(matches!(a.prepare(&slot(3), &b(9, "y")).reply, Reply::Nack { promised: None, .. }));
    a.accept(&slot(4), &b(1, "x"), &v("p", 2));
    assert!(!a.release_to_free(4, q23, 1), "the own acceptance may still complete a quorum");
    assert!(a.release_to_free(4, q23, 2));
    assert!(a.signed_freely(4));
}

#[test]
fn the_lock_is_dead_once_phase_one_or_phase_two_cannot_complete() {
    let dead = |n, m, free, acc| q(n, m).lock_dead(free, acc);
    // 2-of-2 and 3-of-3: one free seat ends it, even for an acceptor
    assert!(dead(2, 2, 1, false) && dead(2, 2, 1, true));
    assert!(dead(3, 3, 1, true));
    // 2-of-3: one free seat for a promiser, two for an acceptor
    assert!(dead(3, 2, 1, false) && !dead(3, 2, 1, true) && dead(3, 2, 2, true));
    // 2-of-5: two free seats leave three, short of the four promises
    assert!(!dead(5, 2, 1, false) && dead(5, 2, 2, false) && dead(5, 2, 2, true));
    // 3-of-5 and 4-of-7
    assert!(!dead(5, 3, 1, false) && dead(5, 3, 2, false) && !dead(5, 3, 2, true) && dead(5, 3, 3, true));
    assert!(!dead(7, 4, 2, false) && dead(7, 4, 3, false) && dead(7, 4, 4, true));
    assert!(!dead(3, 2, 0, false), "never without a free seat");
}

#[test]
fn a_free_answer_counts_as_heard_for_the_forced_value() {
    // 2-of-5: one acceptor holds p, the fifth seat answered "free": p
    // cannot have two acceptors left unheard, so the leader is free
    let mut l = Leader::new(slot(1), q(5, 2), b(3, "a"));
    for s in ["a", "b", "c"] {
        l.promise(s, &b(3, "a"), None);
    }
    l.promise("d", &b(3, "a"), Some((b(1, "d"), v("p", 2))));
    assert_eq!(l.forced(), Some(v("p", 2)), "e unheard may hold p");
    l.heard_free("e");
    assert_eq!(l.forced(), None);
}

#[test]
fn settle_drops_decided_slots_and_keeps_recent_free_marks() {
    let mut a = Acceptor::default();
    a.prepare(&slot(10), &b(1, "x"));
    a.prepare(&slot(11), &b(1, "x"));
    a.sign_freely(9);
    assert!(a.settle(10));
    assert!(a.promised(&slot(10)).is_none());
    assert!(a.promised(&slot(11)).is_some());
    assert!(!a.may_sign_freely(11));
    // the free mark survives a head move (a reorg may re-open the height)
    assert!(matches!(a.prepare(&slot(9), &b(1, "x")).reply, Reply::Nack { promised: None, .. }));
    a.settle(9 + FREE_MARK_KEEP);
    assert!(matches!(a.prepare(&slot(9), &b(1, "x")).reply, Reply::Promise { .. }));
}

#[test]
fn next_round_is_above_everything_seen_here() {
    let mut a = Acceptor::default();
    let s = slot(1);
    assert_eq!(a.next_round(&s), 1);
    a.prepare(&s, &b(4, "y"));
    assert_eq!(a.next_round(&s), 5);
    a.accept(&s, &b(7, "z"), &v("p", 2));
    assert_eq!(a.next_round(&s), 8);
}

#[test]
fn the_acceptor_round_trips_through_serde() {
    let mut a = Acceptor::default();
    a.prepare(&slot(3), &b(2, "x"));
    a.accept(&slot(4), &b(1, "y"), &v("p", 2));
    a.sign_freely(2);
    let json = serde_json::to_string(&a).expect("the acceptor serializes");
    let back: Acceptor = serde_json::from_str(&json).expect("the acceptor deserializes");
    assert_eq!(a, back);
    let empty: Acceptor = serde_json::from_str("{}").expect("an empty file is an empty acceptor");
    assert_eq!(empty, Acceptor::default());
}

#[test]
fn the_persisted_shape_is_pinned() {
    let mut a = Acceptor::default();
    a.accept(&Slot { height: 3, base: "ab".into() }, &b(2, "x"), &v("d", 2));
    a.sign_freely(2);
    assert_eq!(
        serde_json::to_string(&a).expect("the acceptor serializes"),
        r#"{"slots":{"3":{"ab":{"promised":{"round":2,"leader":"x"},"accepted":[{"round":2,"leader":"x"},{"digest":"d","need":2}]}}},"free":[2]}"#
    );
}

// ---- leader rules ----

fn q(n: usize, m: usize) -> Quorum {
    Quorum { n, m }
}

#[test]
fn phase_one_needs_n_minus_m_plus_one() {
    assert_eq!(q(3, 2).promises(), 2);
    assert_eq!(q(5, 3).promises(), 3);
    assert_eq!(q(2, 2).promises(), 1);
    assert_eq!(q(3, 3).promises(), 1);
    let mut l = Leader::new(slot(1), q(5, 3), b(1, "a"));
    l.promise("a", &b(1, "a"), None);
    l.promise("b", &b(1, "a"), None);
    l.promise("b", &b(1, "a"), None);
    assert!(!l.ready());
    // a promise for another ballot does not count
    l.promise("c", &b(2, "c"), None);
    assert!(!l.ready());
    l.promise("c", &b(1, "a"), None);
    assert!(l.ready());
}

#[test]
fn the_highest_accepted_value_is_forced() {
    let mut l = Leader::new(slot(1), q(3, 2), b(5, "a"));
    l.promise("a", &b(5, "a"), Some((b(1, "x"), v("old", 2))));
    l.promise("b", &b(5, "a"), Some((b(3, "y"), v("new", 2))));
    assert_eq!(l.forced(), Some(v("new", 2)));
    assert_eq!(l.propose(Some(v("mine", 2))), Some(v("new", 2)));
    assert_eq!(l.propose(Some(v("mine", 2))), None, "one value per ballot");
}

#[test]
fn a_free_leader_proposes_its_own_value() {
    let mut l = Leader::new(slot(1), q(3, 2), b(1, "a"));
    assert_eq!(l.propose(Some(v("mine", 2))), None, "not ready");
    l.promise("a", &b(1, "a"), None);
    l.promise("b", &b(1, "a"), None);
    assert_eq!(l.forced(), None);
    assert_eq!(l.propose(None), None);
    assert_eq!(l.propose(Some(v("mine", 2))), Some(v("mine", 2)));
}

#[test]
fn a_cut_that_cannot_have_been_chosen_is_not_forced() {
    // 2-of-3, a cut needs all three: one promise without it proves it lost
    let cut = v("cut", 3);
    let mut l = Leader::new(slot(1), q(3, 2), b(2, "a"));
    l.promise("a", &b(2, "a"), Some((b(1, "x"), cut.clone())));
    l.promise("b", &b(2, "a"), None);
    assert_eq!(l.forced(), None);
    // both promisers hold it and the third is unheard: it may be chosen
    let mut l = Leader::new(slot(1), q(3, 2), b(2, "a"));
    l.promise("a", &b(2, "a"), Some((b(1, "x"), cut.clone())));
    l.promise("b", &b(2, "a"), Some((b(1, "x"), cut.clone())));
    assert_eq!(l.forced(), Some(cut.clone()));
    // all three heard, one without it: free again
    l.promise("c", &b(2, "a"), Some((b(0, "c"), v("other", 2))));
    assert_eq!(l.forced(), None);
}

// ---- tally ----

#[test]
fn only_one_ballot_seals_and_the_lowest_names_sign() {
    let mut t = Tally::default();
    let p = v("p", 2);
    assert_eq!(t.accepted("c", &b(1, "x"), &p), None);
    // the same value at another ballot does not combine
    assert_eq!(t.accepted("b", &b(2, "y"), &p), None);
    assert_eq!(t.accepted("c", &b(1, "x"), &p), None, "a duplicate counts once");
    let s = t.accepted("a", &b(1, "x"), &p).expect("two at ballot 1");
    assert_eq!(s.signers, vec!["a".to_string(), "c".to_string()]);
    let s = t.accepted("b", &b(1, "x"), &p).expect("three at ballot 1");
    assert_eq!(s.signers, vec!["a".to_string(), "b".to_string()]);
}

// ---- simulation ----

/// splitmix64: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(n.max(1)).expect("fits")).expect("fits")
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

#[derive(Debug, Clone)]
enum Msg {
    Prepare { slot: Slot, ballot: Ballot },
    Accept { slot: Slot, ballot: Ballot, value: Value },
    Reply { from: usize, reply: Reply },
    Decided { slot: Slot, value: Value },
    /// A B6 signature `from` gave at `height` (mixed runs only).
    Free { from: usize, height: u64 },
}

/// How the mixed runs release lock state into a free mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Release {
    /// Never: no seat signs freely.
    Off,
    /// The WIP branch's rule: promise-only state, `|F| >= n - m`.
    Shipped,
    /// [`Acceptor::release_to_free`] as built now.
    Corrected,
    /// The control: promise-only state on the first free signer.
    Eager,
}

#[derive(Clone, Copy)]
struct Config {
    n: usize,
    m: usize,
    heights: u64,
    /// Seats that never act (a restored seat's lost device); the last ones.
    dead: usize,
    /// Percent of wishes that are cuts (need n).
    cuts: u64,
    /// Percent of wishes that are consented restores (need m - 1).
    restores: u64,
    /// Seats on an older build (sign freely, drop every lock frame), just
    /// before the dead ones.
    old: usize,
    /// The step the old seats appear at.
    old_from: usize,
}

struct Seat {
    name: String,
    acceptor: Acceptor,
    disk: String,
    leaders: BTreeMap<u64, Leader>,
    tallies: BTreeMap<u64, Tally>,
    seen_round: BTreeMap<u64, u64>,
    decided: BTreeMap<u64, Value>,
    free_known: BTreeMap<u64, BTreeSet<usize>>,
}

#[derive(Default, Debug)]
struct Outcome {
    /// Slots where two different values each held `need` same-ballot
    /// acceptances.
    forks: usize,
    /// Slots where two different values each held `need` acceptances when
    /// signatures of different ballots are pooled.
    cross_ballot: usize,
    learned_twice: usize,
    undecided: usize,
    /// Slots where a lock-chosen value and another free-sealable value meet.
    lock_vs_free: usize,
    /// Slots decided by the lock, and slots with a free-sealable value.
    lock_chosen: usize,
    free_sealable: usize,
    /// Releases that happened (mixed runs).
    releases: usize,
}

struct Sim {
    cfg: Config,
    rng: Rng,
    seats: Vec<Seat>,
    net: Vec<(usize, Msg)>,
    /// Ghost: every acceptance that happened, by slot.
    ghost: BTreeMap<u64, BTreeMap<(Ballot, Value), BTreeSet<usize>>>,
    /// A seat learned a second, different value for a height.
    learned_twice: usize,
    /// Ghost: every free signature, by height.
    free_ghost: BTreeMap<u64, BTreeMap<Value, BTreeSet<usize>>>,
    release: Release,
    releases: usize,
    step: usize,
    drop: u64,
    dup: u64,
    crash: u64,
}

const STATE_CRASH_BEFORE_WRITE: u64 = 2;

impl Sim {
    fn new(cfg: Config, seed: u64, skip_promise_check: bool) -> Self {
        let seats = (0..cfg.n)
            .map(|i| {
                let acceptor = Acceptor { skip_promise_check, ..Acceptor::default() };
                Seat {
                    name: format!("s{i}"),
                    disk: serde_json::to_string(&acceptor).expect("serializes"),
                    acceptor,
                    leaders: BTreeMap::new(),
                    tallies: BTreeMap::new(),
                    seen_round: BTreeMap::new(),
                    decided: BTreeMap::new(),
                    free_known: BTreeMap::new(),
                }
            })
            .collect();
        Self {
            cfg,
            rng: Rng(seed),
            seats,
            net: Vec::new(),
            ghost: BTreeMap::new(),
            learned_twice: 0,
            free_ghost: BTreeMap::new(),
            release: Release::Off,
            releases: 0,
            step: 0,
            drop: 15,
            dup: 10,
            crash: 2,
        }
    }

    fn quorum(&self) -> Quorum {
        Quorum { n: self.cfg.n, m: self.cfg.m }
    }

    /// Runs this build: answers lock frames, may lead.
    fn runs_lock(&self, i: usize) -> bool {
        i < self.cfg.n - self.cfg.dead - self.cfg.old
    }

    fn is_old(&self, i: usize) -> bool {
        !self.runs_lock(i) && i < self.cfg.n - self.cfg.dead
    }

    fn live(&self, i: usize) -> bool {
        self.runs_lock(i) || (self.is_old(i) && self.step >= self.cfg.old_from)
    }

    fn acting(&self) -> usize {
        self.cfg.n - self.cfg.dead
    }

    fn slot(h: u64) -> Slot {
        Slot { height: h, base: format!("l{h}") }
    }

    fn kind(&mut self) -> (&'static str, usize) {
        let roll = self.rng.next_u64() % 100;
        let (kind, need) = if roll < self.cfg.cuts {
            ("cut", self.cfg.n)
        } else if roll < self.cfg.cuts + self.cfg.restores {
            ("restore", self.cfg.m - 1)
        } else {
            ("change", self.cfg.m)
        };
        (kind, need.max(1))
    }

    fn wish(&mut self, seat: usize, h: u64) -> Value {
        let (kind, need) = self.kind();
        Value { digest: format!("{kind}-{h}-{seat}-{}", self.rng.below(3)), need }
    }

    fn send_all(&mut self, from: usize, msg: &Msg) {
        for to in 0..self.cfg.n {
            if to != from && self.live(to) {
                self.net.push((to, msg.clone()));
            }
        }
    }

    /// Persist-then-send; a crash between the change and the write loses
    /// both the change and the reply.
    fn acceptor_step(&mut self, i: usize, step: Step) -> Option<Reply> {
        if step.persist {
            if self.rng.chance(STATE_CRASH_BEFORE_WRITE) {
                self.restart(i);
                return None;
            }
            self.seats[i].disk = serde_json::to_string(&self.seats[i].acceptor).expect("serializes");
        }
        if let Reply::Accepted { slot, ballot, value } = &step.reply {
            self.ghost
                .entry(slot.height)
                .or_default()
                .entry((ballot.clone(), value.clone()))
                .or_default()
                .insert(i);
        }
        Some(step.reply)
    }

    fn restart(&mut self, i: usize) {
        let skip = self.seats[i].acceptor.skip_promise_check;
        let seat = &mut self.seats[i];
        seat.acceptor = serde_json::from_str(&seat.disk).expect("the disk copy deserializes");
        seat.acceptor.skip_promise_check = skip;
        seat.leaders.clear();
        seat.tallies.clear();
    }

    fn start_ballot(&mut self, i: usize, h: u64) {
        let s = Self::slot(h);
        let seat = &self.seats[i];
        let round = seat.acceptor.next_round(&s).max(seat.seen_round.get(&h).copied().unwrap_or(0) + 1);
        let ballot = Ballot { round, leader: seat.name.clone() };
        let leader = Leader::new(s.clone(), self.quorum(), ballot.clone());
        self.seats[i].leaders.insert(h, leader);
        self.seats[i].seen_round.insert(h, round);
        let msg = Msg::Prepare { slot: s, ballot };
        // the leader is its own first acceptor
        self.deliver(i, msg.clone());
        if self.seats[i].leaders.contains_key(&h) {
            self.send_all(i, &msg);
        }
    }

    fn decide(&mut self, i: usize, slot: &Slot, value: &Value, announce: bool) {
        let h = slot.height;
        match self.seats[i].decided.get(&h) {
            Some(known) => {
                if known != value {
                    self.learned_twice += 1;
                }
            }
            None => {
                self.seats[i].decided.insert(h, value.clone());
                if announce {
                    self.send_all(i, &Msg::Decided { slot: slot.clone(), value: value.clone() });
                }
            }
        }
    }

    fn deliver(&mut self, i: usize, msg: Msg) {
        if !self.live(i) {
            return;
        }
        if !self.runs_lock(i) && !matches!(msg, Msg::Decided { .. }) {
            // an older build drops every lock frame
            return;
        }
        match msg {
            Msg::Prepare { slot, ballot } => {
                let seen = self.seats[i].seen_round.entry(slot.height).or_default();
                *seen = (*seen).max(ballot.round);
                let step = self.seats[i].acceptor.prepare(&slot, &ballot);
                if let Some(reply) = self.acceptor_step(i, step) {
                    self.on_reply(i, i, reply.clone());
                    self.send_all(i, &Msg::Reply { from: i, reply });
                }
            }
            Msg::Accept { slot, ballot, value } => {
                let step = self.seats[i].acceptor.accept(&slot, &ballot, &value);
                if let Some(reply) = self.acceptor_step(i, step) {
                    self.on_reply(i, i, reply.clone());
                    self.send_all(i, &Msg::Reply { from: i, reply });
                }
            }
            Msg::Reply { from, reply } => self.on_reply(i, from, reply),
            Msg::Decided { slot, value } => self.decide(i, &slot, &value, false),
            Msg::Free { from, height } => self.note_free(i, from, height),
        }
    }

    /// Seat `i` learned that `from` signed freely at `height`.
    fn note_free(&mut self, i: usize, from: usize, height: u64) {
        if from == i {
            return;
        }
        self.seats[i].free_known.entry(height).or_default().insert(from);
        self.maybe_release(i, height);
    }

    /// The release rule under test, at seat `i` and `height`.
    fn maybe_release(&mut self, i: usize, height: u64) {
        if !self.runs_lock(i) || self.seats[i].acceptor.signed_freely(height) {
            return;
        }
        let free = self.seats[i].free_known.get(&height).map_or(0, BTreeSet::len);
        let q = self.quorum();
        let acc = &mut self.seats[i].acceptor;
        let released = match self.release {
            Release::Off => false,
            Release::Shipped => free > 0 && free >= q.n - q.m && acc.release_promises_only(height),
            Release::Corrected => acc.release_to_free(height, q, free),
            Release::Eager => free > 0 && acc.release_promises_only(height),
        };
        if released {
            self.releases += 1;
            self.persist_free(i, height);
        }
    }

    /// The race: seat `i` still believes `height` free and signs there
    /// (refused where it holds lock state).
    fn sign_freely(&mut self, i: usize, height: u64) {
        if self.seats[i].acceptor.sign_freely(height).is_none() {
            return;
        }
        self.persist_free(i, height);
    }

    /// The free mark reaches the disk before the signature leaves; a crash
    /// in between loses both.
    fn persist_free(&mut self, i: usize, height: u64) {
        if self.rng.chance(self.crash) {
            self.restart(i);
            return;
        }
        self.seats[i].disk = serde_json::to_string(&self.seats[i].acceptor).expect("serializes");
        // free signers agree among themselves; a lock value never equals one
        let (kind, need) = self.kind();
        let value = Value { digest: format!("free-{kind}-{height}-{}", self.rng.below(2)), need };
        self.free_ghost.entry(height).or_default().entry(value).or_default().insert(i);
        self.send_all(i, &Msg::Free { from: i, height });
    }

    fn on_reply(&mut self, i: usize, from: usize, reply: Reply) {
        let from_name = self.seats[from].name.clone();
        match reply {
            Reply::Promise { slot, ballot, accepted } => {
                let h = slot.height;
                let Some(leader) = self.seats[i].leaders.get_mut(&h) else { return };
                leader.promise(&from_name, &ballot, accepted);
                if leader.ready() && leader.proposed().is_none() {
                    let own = self.wish(i, h);
                    let Some(leader) = self.seats[i].leaders.get_mut(&h) else { return };
                    if let Some(value) = leader.propose(Some(own)) {
                        let msg = Msg::Accept { slot, ballot, value };
                        self.deliver(i, msg.clone());
                        self.send_all(i, &msg);
                    }
                }
            }
            Reply::Accepted { slot, ballot, value } => {
                let h = slot.height;
                let sealed = self.seats[i].tallies.entry(h).or_default().accepted(&from_name, &ballot, &value);
                if let Some(s) = sealed {
                    self.decide(i, &slot, &s.value, true);
                }
            }
            Reply::Nack { slot, promised, .. } => match promised {
                Some(p) => {
                    let seen = self.seats[i].seen_round.entry(slot.height).or_default();
                    *seen = (*seen).max(p.round);
                }
                None if self.release != Release::Off => {
                    if let Some(leader) = self.seats[i].leaders.get_mut(&slot.height) {
                        if from != i {
                            leader.heard_free(&from_name);
                        }
                    }
                    self.note_free(i, from, slot.height);
                }
                None => {}
            },
        }
    }

    fn chaos(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step += 1;
            let roll = self.rng.next_u64() % 100;
            if roll < 8 || self.net.is_empty() {
                // a seat times out and takes over a height it has not decided
                let i = self.rng.below(self.acting());
                let h = 1 + self.rng.next_u64() % self.cfg.heights;
                if self.seats[i].decided.contains_key(&h) {
                    continue;
                }
                if self.runs_lock(i) {
                    self.start_ballot(i, h);
                } else if self.live(i) {
                    self.sign_freely(i, h);
                }
            } else if roll < 8 + self.crash {
                let i = self.rng.below(self.cfg.n - self.cfg.dead - self.cfg.old);
                self.restart(i);
            } else if self.release != Release::Off && roll < 14 + self.crash {
                // an activation race: a seat that still believes the height
                // free, or whose belief flipped since
                let i = self.rng.below(self.acting());
                let h = 1 + self.rng.next_u64() % self.cfg.heights;
                if self.live(i) {
                    self.sign_freely(i, h);
                }
            } else {
                let k = self.rng.below(self.net.len());
                let (to, msg) = if self.rng.chance(self.dup) { self.net[k].clone() } else { self.net.swap_remove(k) };
                if !self.rng.chance(self.drop) {
                    self.deliver(to, msg);
                }
            }
        }
    }

    fn drain(&mut self) {
        while !self.net.is_empty() {
            let k = self.rng.below(self.net.len());
            let (to, msg) = self.net.swap_remove(k);
            self.deliver(to, msg);
        }
    }

    fn undecided(&self) -> Vec<u64> {
        (1..=self.cfg.heights)
            .filter(|h| {
                let live: Vec<usize> = (0..self.cfg.n).filter(|i| self.live(*i)).collect();
                let decided = live.iter().all(|i| self.seats[*i].decided.contains_key(h));
                // every seat free: B6 decides from here
                let free = live.iter().all(|i| self.seats[*i].acceptor.signed_freely(*h));
                !decided && !free
            })
            .collect()
    }

    /// Reliable delivery, no crashes, one leader retrying: every height
    /// must decide on every live seat.
    fn settle(&mut self) -> usize {
        self.drop = 0;
        self.dup = 0;
        self.crash = 0;
        for _ in 0..12 {
            self.drain();
            let undecided = self.undecided();
            if undecided.is_empty() {
                return 0;
            }
            for h in undecided {
                match self.seats[0].decided.get(&h).cloned() {
                    // the block gossip a catch-up re-serves
                    Some(value) => self.send_all(0, &Msg::Decided { slot: Self::slot(h), value }),
                    None => self.start_ballot(0, h),
                }
            }
        }
        self.undecided().len()
    }

    /// Mixed liveness: reliable delivery, and every seat's belief settled
    /// (an old seat present: new heights go free; otherwise lock). Lock
    /// state finishes under the lock or is released; free seats re-serve
    /// their signatures (B6), the lowest non-free seat leads.
    fn settle_mixed(&mut self) -> usize {
        self.drop = 0;
        self.dup = 0;
        self.crash = 0;
        self.step = self.step.max(self.cfg.old_from);
        let old_present = self.cfg.old > 0;
        for _ in 0..40 {
            self.drain();
            let undecided = self.undecided();
            if undecided.is_empty() {
                return 0;
            }
            for h in undecided {
                if let Some(i) = (0..self.cfg.n).find(|i| self.live(*i) && self.seats[*i].decided.contains_key(&h)) {
                    let value = self.seats[i].decided[&h].clone();
                    self.send_all(i, &Msg::Decided { slot: Self::slot(h), value });
                    continue;
                }
                for i in 0..self.acting() {
                    let believes_free = !self.runs_lock(i) || old_present;
                    if believes_free || self.seats[i].acceptor.signed_freely(h) {
                        self.sign_freely(i, h);
                    }
                }
                for i in 0..self.acting() {
                    self.maybe_release(i, h);
                }
                if let Some(i) = (0..self.acting()).find(|i| self.runs_lock(*i) && !self.seats[*i].acceptor.signed_freely(h)) {
                    self.start_ballot(i, h);
                }
            }
        }
        self.undecided().len()
    }

    fn outcome(&self) -> Outcome {
        let mut out = Outcome { learned_twice: self.learned_twice, releases: self.releases, ..Outcome::default() };
        for by in self.ghost.values() {
            let chosen: BTreeSet<&Value> = by.iter().filter(|((_, v), who)| who.len() >= v.need).map(|((_, v), _)| v).collect();
            if chosen.len() > 1 {
                out.forks += 1;
            }
            let mut pooled: BTreeMap<&Value, BTreeSet<usize>> = BTreeMap::new();
            for ((_, v), who) in by {
                pooled.entry(v).or_default().extend(who.iter().copied());
            }
            if pooled.iter().filter(|(v, who)| who.len() >= v.need).count() > 1 {
                out.cross_ballot += 1;
            }
        }
        for (h, by) in &self.ghost {
            let chosen: BTreeSet<&Value> = by.iter().filter(|((_, v), who)| who.len() >= v.need).map(|((_, v), _)| v).collect();
            let free: BTreeSet<&Value> = self
                .free_ghost
                .get(h)
                .map(|f| f.iter().filter(|(v, who)| who.len() >= v.need).map(|(v, _)| v).collect())
                .unwrap_or_default();
            out.lock_chosen += usize::from(!chosen.is_empty());
            if chosen.iter().any(|c| free.iter().any(|f| f != c)) {
                out.lock_vs_free += 1;
            }
        }
        out.free_sealable = self
            .free_ghost
            .values()
            .filter(|f| f.iter().any(|(v, who)| who.len() >= v.need))
            .count();
        out
    }
}

fn run(cfg: Config, seed: u64, skip_promise_check: bool, liveness: bool) -> Outcome {
    let mut sim = Sim::new(cfg, seed, skip_promise_check);
    sim.chaos(400);
    let mut out = sim.outcome();
    if liveness {
        out.undecided = sim.settle();
        let after = sim.outcome();
        out.forks = out.forks.max(after.forks);
        out.cross_ballot = out.cross_ballot.max(after.cross_ballot);
        out.learned_twice = after.learned_twice;
    }
    out
}

const SEEDS: u64 = 1500;

fn plain(n: usize, m: usize) -> Config {
    Config { n, m, heights: 3, dead: 0, cuts: 0, restores: 0, old: 0, old_from: 0 }
}

fn configs() -> Vec<(&'static str, Config)> {
    vec![
        ("2-of-3", plain(3, 2)),
        ("3-of-5", plain(5, 3)),
        ("2-of-2", plain(2, 2)),
        ("3-of-3", plain(3, 3)),
        ("2-of-5", plain(5, 2)),
        ("2-of-3 with cuts", Config { cuts: 40, ..plain(3, 2) }),
        ("3-of-5 with cuts", Config { cuts: 40, ..plain(5, 3) }),
        ("2-of-3 restoring a lost seat", Config { dead: 1, restores: 50, ..plain(3, 2) }),
        ("3-of-5 restoring a lost seat", Config { dead: 1, restores: 50, ..plain(5, 3) }),
    ]
}

#[test]
fn simulation_never_seals_two_values_at_one_slot_and_every_slot_decides() {
    for (name, cfg) in configs() {
        for seed in 0..SEEDS {
            let out = run(cfg, seed, false, true);
            assert_eq!(out.forks, 0, "{name}: fork at seed {seed}");
            assert_eq!(out.learned_twice, 0, "{name}: a seat learned two values at seed {seed}");
            assert_eq!(out.undecided, 0, "{name}: undecided slots at seed {seed}");
        }
    }
}

#[test]
fn simulation_without_the_promise_check_finds_a_fork() {
    for (name, cfg) in [("2-of-3", configs()[0].1), ("3-of-5", configs()[1].1)] {
        let found = (0..SEEDS).find(|seed| {
            let out = run(cfg, *seed, true, false);
            out.forks > 0 && out.learned_twice > 0
        });
        assert!(found.is_some(), "{name}: the control found no fork in {SEEDS} seeds");
        eprintln!("{name}: control fork at seed {found:?}");
    }
}

#[test]
fn signatures_of_different_ballots_must_not_be_pooled() {
    // the reason a block is sealed from one ballot: pooling every
    // signature a seat ever gave would fork even with the lock intact
    let cfg = configs()[0].1;
    let found = (0..SEEDS).find(|seed| run(cfg, *seed, false, false).cross_ballot > 0);
    assert!(found.is_some(), "no pooled-signature fork in {SEEDS} seeds");
    eprintln!("pooled-signature fork at seed {found:?}");
}

fn run_mixed(cfg: Config, seed: u64, release: Release) -> Outcome {
    let mut sim = Sim::new(cfg, seed, false);
    sim.release = release;
    sim.chaos(400);
    let mut out = sim.outcome();
    out.undecided = sim.settle_mixed();
    let after = sim.outcome();
    out.forks = out.forks.max(after.forks);
    out.lock_vs_free = out.lock_vs_free.max(after.lock_vs_free);
    out.lock_chosen = after.lock_chosen;
    out.free_sealable = after.free_sealable;
    out.learned_twice = after.learned_twice;
    out.releases = after.releases;
    out
}

const MIXED_SEEDS: u64 = 800;

fn mixed_configs() -> Vec<(&'static str, Config)> {
    vec![
        ("2-of-2", plain(2, 2)),
        ("2-of-3", plain(3, 2)),
        ("3-of-3", plain(3, 3)),
        ("3-of-4", plain(4, 3)),
        ("2-of-5", plain(5, 2)),
        ("3-of-5", plain(5, 3)),
        ("4-of-7", plain(7, 4)),
        ("2-of-3 with cuts", Config { cuts: 40, ..plain(3, 2) }),
        ("3-of-5 with cuts", Config { cuts: 40, ..plain(5, 3) }),
        ("2-of-3 restoring a lost seat", Config { dead: 1, restores: 50, ..plain(3, 2) }),
        ("3-of-5 restoring a lost seat", Config { dead: 1, restores: 50, ..plain(5, 3) }),
        ("4-of-7 restoring a lost seat", Config { dead: 1, restores: 50, ..plain(7, 4) }),
        ("2-of-3, an old seat appears mid-run", Config { old: 1, old_from: 150, ..plain(3, 2) }),
        ("3-of-5, an old seat from the start", Config { old: 1, ..plain(5, 3) }),
        ("4-of-7, two old seats appear mid-run", Config { old: 2, old_from: 200, ..plain(7, 4) }),
        ("3-of-5 restoring onto an old build", Config { dead: 1, old: 1, old_from: 100, restores: 50, ..plain(5, 3) }),
    ]
}

/// §14 as built: seats that sign freely beside the lock (activation races,
/// flipping beliefs, old builds appearing mid-height, crashes inside a
/// release), with reliable delivery at the end. Never two lock values,
/// never a lock value beside a different free-sealable one, and every
/// height decides - by the lock, or by every seat going free (B6).
#[test]
fn simulation_with_free_signers_and_the_release_rule_never_forks_and_decides() {
    for (name, cfg) in mixed_configs() {
        let (mut chosen, mut free, mut released) = (0, 0, 0);
        for seed in 0..MIXED_SEEDS {
            let out = run_mixed(cfg, seed, Release::Corrected);
            assert_eq!(out.forks, 0, "{name}: lock fork at seed {seed}");
            assert_eq!(out.learned_twice, 0, "{name}: a seat learned two values at seed {seed}");
            assert_eq!(out.lock_vs_free, 0, "{name}: lock beside free at seed {seed}");
            assert_eq!(out.undecided, 0, "{name}: undecided at seed {seed}");
            chosen += out.lock_chosen;
            free += out.free_sealable;
            released += out.releases;
        }
        assert!(chosen > 0 && free > 0, "{name}: both paths decided somewhere ({chosen}, {free})");
        eprintln!("{name}: lock-decided {chosen}, free-sealable {free}, releases {released}");
    }
}

/// The counterexamples to the rule first shipped on the WIP branch: safe,
/// but a height can stall for good. 2-of-2: the leader accepted its value,
/// the other seat signed freely, and an acceptance was never released.
/// 2-of-5: two free seats leave three, short of the four promises phase 1
/// needs, while `|F| >= n - m` asks for three.
#[test]
fn the_shipped_release_rule_stalls_and_the_corrected_one_decides() {
    for (name, cfg) in [("2-of-2", plain(2, 2)), ("2-of-5", plain(5, 2)), ("3-of-3", plain(3, 3))] {
        let stalled = (0..MIXED_SEEDS).find(|seed| {
            let out = run_mixed(cfg, *seed, Release::Shipped);
            assert_eq!(out.forks + out.lock_vs_free, 0, "{name}: the shipped rule forked at seed {seed}");
            out.undecided > 0
        });
        let seed = stalled.unwrap_or_else(|| panic!("{name}: no stall of the shipped rule in {MIXED_SEEDS} seeds"));
        eprintln!("{name}: shipped rule stalls at seed {seed}");
        assert_eq!(run_mixed(cfg, seed, Release::Corrected).undecided, 0, "{name}: seed {seed}");
    }
}

#[test]
fn releasing_on_the_first_free_signer_forks() {
    let found = (0..MIXED_SEEDS).find(|seed| run_mixed(plain(5, 2), *seed, Release::Eager).lock_vs_free > 0);
    assert!(found.is_some(), "the eager control found no fork");
    eprintln!("eager release fork at seed {found:?}");
}
