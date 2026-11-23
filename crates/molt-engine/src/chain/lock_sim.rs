// SPDX-License-Identifier: GPL-3.0-or-later

//! A wire-faithful simulation of the lock: the real [`Acceptor`],
//! [`Leader`] and [`Tally`] driven by the rules of `lock_wire.rs`, with
//! per-seat knowledge over a lossy network. The older simulation in
//! `lock_tests.rs` checks the pure state machine; this one checks the
//! wiring's application rules against it.
//!
//! | rule in `lock_wire.rs`                                   | modelled in |
//! |----------------------------------------------------------|-------------|
//! | card gossip (`receive_proposed`, membership register)     | `Msg::Card`, lossy; an `Accept`/`Promise` naming a restore teaches it too |
//! | intents (unsigned `Approved`), counted by the leader      | `Msg::Intent`, lossy; per seat, a dead seat's intent stays counted |
//! | `lock_choose`: voices >= need, gates, R5 stale rotation   | `free_pick` (voices from every seat not known as subject; no rotation: ballots restart instead) |
//! | `lock_restore_subjects` (registered or held, per seat)    | `subjects_of(i)` from the seat's own card knowledge |
//! | a known subject's promise is dropped (`lock_on_promise`)   | `on_promise` |
//! | T1 excusal per restore digest (`lock_excuse_subjects`)    | `on_promise` |
//! | S3: a forced value waits one interval while heard < n      | `waiting`, released on the next tick |
//! | `forced` flag on `LockAccept`                             | `Msg::Accept { forced }` |
//! | acceptor: kept (S1), approver rule (T1), gates             | `accepts` |
//! | restore gates: settle, seat-still-active, own restore      | `accepts` (settle = known for `SETTLE` steps) |
//! | a subject's acceptance of a membership value not counted  | `on_accepted` |
//! | seal: a chosen value seals, no gate (M3)                  | `seal` |
//! | a sealed restore evicts the old device at later heights   | not modelled: heights are independent here, the old device stays (worst case); the rejoiner is not modelled |
//! | need(R) = m where m < n, n - 1 where m = n (`Quorum::restore_need`) | `restore_need` |
//! | a seat that is a subject in its own view leads (H2 fix: never) | `may_lead` |
//! | acceptors refuse a subject's Prepare/Accept (A-F1)       | `refuses_frame` |
//! | subjects: open cards only (C-HIGH-1), held values, per value intents | `subjects_of`, `ready_cards` |
//! | persist before the frame; crash between loses both        | `step_acceptor` |
//! | withdrawal by the proposer                                | `Msg::Withdraw`, lossy |
//!
//! | B-F1: a slot above the durable chain is never forgotten   | not modelled (no chain here): `a_slot_above_the_durable_chain_is_never_forgotten`, `a_lock_file_ahead_of_the_chain_means_amnesia` |
//! | D-F1: a slot inside the reorg window is kept              | not modelled (no lineages here): `a_slot_below_the_head_is_kept_for_a_reorg_back` |
//!
//! Not modelled (covered elsewhere): free signers and the release rule
//! (`lock_tests.rs` mixed runs), cuts, lineages/forks below.
//!
//! Every [`Fixes`] option against the wire: the function that carries it
//! and the wire test that goes red without it (each checked by disabling
//! the wire rule).
//!
//! | option | wire function | wire test red without it |
//! |--------|---------------|--------------------------|
//! | `h2` need(R)          | `Quorum::restore_need` | `a_restore_needs_m_where_m_is_below_n` |
//! | `h2` never lead       | `lock_start` (own subject check) | `a_restore_subject_never_leads` |
//! | `h2` refuse a subject | `lock_on_prepare`, `lock_on_accept` | `an_acceptor_refuses_a_restore_subjects_prepare` |
//! | `h1`                  | `lock_acceptor_accept` -> `lock_forced_invalid` | `a_forced_withdrawn_value_completes_with_any_promiser` |
//! | `m3`                  | `lock_seal` (no gate) | `a_chosen_restore_seals_while_the_old_device_is_heard` |
//! | `amnesia_rule`        | NOT BUILT (wiring plan, open items) | none |

use super::*;

const SETTLE: usize = 6;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Change,
    Restore(usize),
}

#[derive(Clone, Debug)]
struct Card {
    height: u64,
    kind: Kind,
    need: usize,
    proposer: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Alive,
    Dead,
    Revived,
}

#[derive(Clone, Debug)]
enum Msg {
    Card { digest: String },
    Intent { digest: String, from: usize },
    Withdraw { digest: String },
    Prepare { slot: Slot, ballot: Ballot },
    Accept { slot: Slot, ballot: Ballot, value: Value, forced: bool },
    Reply { from: usize, reply: Reply },
    Decided { height: u64, value: Value },
    /// An amnesiac seat asks the others what it may have touched.
    AmnesiaAsk { from: usize, nonce: u64 },
    /// `touched`: the highest height the replier holds or saw lock state
    /// at (or its head + 1); `round`: the highest round it saw from the
    /// asker.
    AmnesiaReply { from: usize, nonce: u64, touched: u64, round: u64 },
}

#[derive(Default)]
struct View {
    withdrawn: bool,
    known_at: usize,
}

struct Run {
    leader: Leader,
    forced: bool,
    waiting: bool,
}

#[derive(Default)]
struct Seat {
    acceptor: Acceptor,
    disk: String,
    runs: BTreeMap<u64, Run>,
    tallies: BTreeMap<u64, Tally>,
    held: BTreeMap<u64, Sealable>,
    decided: BTreeMap<u64, Value>,
    seen_round: BTreeMap<u64, u64>,
    knows: BTreeMap<String, View>,
    mine: BTreeSet<String>,
    intents: BTreeMap<String, BTreeSet<usize>>,
    /// R5/T2: cards whose free ballot went stale, per height, oldest first.
    stale: BTreeMap<u64, Vec<String>>,
    /// The highest height this seat touched (lock state held or seen).
    touched: u64,
    /// The highest round seen from each leader.
    rounds_by: BTreeMap<usize, u64>,
    /// Lost its lock state: the replies collected, and the floor and
    /// round floor once enough are in.
    amnesia: Option<Amnesia>,
}

#[derive(Default)]
struct Amnesia {
    nonce: u64,
    replies: BTreeMap<usize, (u64, u64)>,
    floor: Option<(u64, u64)>,
}

/// Which fixes run.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Fixes {
    /// need(R) = m where m < n; a self-subject never leads.
    pub h2: bool,
    /// A forced value is accepted by any promiser (validity only).
    pub h1: bool,
    /// A chosen value seals; the seal never gates.
    pub m3: bool,
    /// An amnesiac seat stays passive at heights up to the highest one
    /// max(m, n - m + 1) OTHER seats report it may have touched, and
    /// leads above the highest round they saw from it. Off: it carries on
    /// with a fresh acceptor at once (the control).
    pub amnesia_rule: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct WConfig {
    pub n: usize,
    pub m: usize,
    pub heights: u64,
    /// Seats lost from the start (the last ones).
    pub lost: usize,
    /// Seats that die at `die_at` (just before the lost ones).
    pub dies: usize,
    pub die_at: usize,
    /// How many lost or dead seats' old devices come back at `revive_at`.
    pub revive: usize,
    pub revive_at: usize,
    /// Percent of steps that withdraw a card.
    pub withdraw: u64,
    /// Percent of card receipts that cast an intent.
    pub approve: u64,
    /// Percent of frames lost (chaos phase).
    pub drop: u64,
    /// Per mille of steps that wipe a seat's lock state (amnesia).
    pub amnesia: u64,
    /// An amnesia also loses the seat's newest decided heights.
    pub tail_loss: bool,
}

#[derive(Debug, Default)]
pub(super) struct WOutcome {
    pub forks: usize,
    pub learned_twice: usize,
    pub undecided: usize,
    pub decided: usize,
}

struct Sim {
    cfg: WConfig,
    fix: Fixes,
    rng: Rng,
    seats: Vec<Seat>,
    life: Vec<Life>,
    cards: BTreeMap<String, Card>,
    net: Vec<(usize, Msg)>,
    /// Log events lost in the chaos phase: the delivery guarantee
    /// (at-least-once) brings them later; lock frames are not retried.
    deferred: Vec<(usize, Msg)>,
    ghost: BTreeMap<u64, BTreeMap<(Ballot, Value), BTreeSet<usize>>>,
    learned_twice: usize,
    step: usize,
    serial: u64,
    drop: u64,
    crash: u64,
}

fn name(i: usize) -> String {
    format!("s{i}")
}

impl Sim {
    fn new(cfg: WConfig, fix: Fixes, seed: u64) -> Self {
        let mut seats: Vec<Seat> = (0..cfg.n).map(|_| Seat::default()).collect();
        for s in &mut seats {
            s.disk = serde_json::to_string(&s.acceptor).expect("serializes");
        }
        let life = (0..cfg.n).map(|i| if i >= cfg.n - cfg.lost { Life::Dead } else { Life::Alive }).collect();
        Self {
            cfg,
            fix,
            rng: Rng(seed),
            seats,
            life,
            cards: BTreeMap::new(),
            net: Vec::new(),
            deferred: Vec::new(),
            ghost: BTreeMap::new(),
            learned_twice: 0,
            step: 0,
            serial: 0,
            drop: cfg.drop,
            crash: 1,
        }
    }

    fn quorum(&self) -> Quorum {
        Quorum { n: self.cfg.n, m: self.cfg.m }
    }

    fn acting(&self, i: usize) -> bool {
        matches!(self.life[i], Life::Alive | Life::Revived)
    }

    fn slot(h: u64) -> Slot {
        Slot { height: h, base: format!("l{h}") }
    }

    fn restore_need(&self) -> usize {
        let q = self.quorum();
        if self.fix.h2 {
            q.restore_need()
        } else {
            q.m.saturating_sub(1).max(1)
        }
    }

    /// The subjects of the restore cards seat `i` knows, until a restore
    /// of that seat applied there (the wire drops a decided card).
    fn subjects_of(&self, i: usize) -> BTreeSet<usize> {
        let restored: BTreeSet<usize> = self.seats[i]
            .decided
            .values()
            .filter_map(|v| match self.cards.get(&v.digest)?.kind {
                Kind::Restore(z) => Some(z),
                Kind::Change => None,
            })
            .collect();
        if !self.fix.h2 {
            return self.seats[i]
                .knows
                .keys()
                .filter_map(|d| match self.cards.get(d)?.kind {
                    Kind::Restore(z) if !restored.contains(&z) => Some(z),
                    _ => None,
                })
                .collect();
        }
        // the fix (C-HIGH-1): an OPEN restore card (where m < n not one
        // whose subject is heard), and every restore value this seat's
        // acceptor holds
        let strict = self.cfg.m >= self.cfg.n;
        let mut out: BTreeSet<usize> = self.seats[i]
            .knows
            .iter()
            .filter_map(|(d, view)| match self.cards.get(d)?.kind {
                Kind::Restore(z) if !restored.contains(&z) && !view.withdrawn && (strict || !self.acting(z)) => Some(z),
                _ => None,
            })
            .collect();
        for h in 1..=self.cfg.heights {
            if let Some((_, v)) = self.seats[i].acceptor.accepted(&Self::slot(h)) {
                if let Some(Kind::Restore(z)) = self.cards.get(&v.digest).map(|c| c.kind) {
                    out.insert(z);
                }
            }
        }
        out
    }

    /// The fix (A-F1): an acceptor takes no `Prepare`/`Accept` from a seat
    /// that is a restore's subject in its own view.
    fn refuses_frame(&self, i: usize, ballot: &Ballot) -> bool {
        self.fix.h2 && self.subjects_of(i).contains(&ballot.leader[1..].parse::<usize>().expect("a seat"))
    }

    /// The amnesia rule: passive while the floor is unknown, and at every
    /// height up to it.
    fn passive_at(&self, i: usize, h: u64) -> bool {
        self.fix.amnesia_rule
            && self.seats[i].amnesia.as_ref().is_some_and(|a| a.floor.map_or(true, |(f, _)| h <= f))
    }

    /// A seat loses its lock state (a crash between writes, a restored
    /// backup), and with `tail_loss` its newest decided heights.
    fn amnesia(&mut self, i: usize) {
        let seat = &mut self.seats[i];
        seat.acceptor = Acceptor::default();
        seat.disk = serde_json::to_string(&seat.acceptor).expect("serializes");
        seat.runs.clear();
        seat.tallies.clear();
        seat.held.clear();
        seat.seen_round.clear();
        seat.rounds_by.clear();
        seat.touched = 0;
        if self.cfg.tail_loss {
            if let Some(h) = seat.decided.keys().max().copied() {
                seat.decided.remove(&h);
            }
        }
        if self.fix.amnesia_rule {
            self.serial += 1;
            let nonce = self.serial;
            self.seats[i].amnesia = Some(Amnesia { nonce, ..Amnesia::default() });
            self.send_all(i, &Msg::AmnesiaAsk { from: i, nonce });
        }
    }

    fn may_lead(&self, i: usize) -> bool {
        self.acting(i) && !(self.fix.h2 && self.subjects_of(i).contains(&i))
    }

    fn send_all(&mut self, from: usize, msg: &Msg) {
        for to in 0..self.cfg.n {
            if to != from {
                self.net.push((to, msg.clone()));
            }
        }
    }

    fn learn(&mut self, i: usize, digest: &str) {
        if !self.cards.contains_key(digest) {
            return;
        }
        let step = self.step;
        self.seats[i].knows.entry(digest.to_string()).or_insert(View { withdrawn: false, known_at: step });
    }

    fn new_card(&mut self, i: usize, h: u64, kind: Kind) {
        self.serial += 1;
        let digest = match kind {
            Kind::Change => format!("c{h}-{}", self.serial),
            Kind::Restore(z) => format!("r{h}-{}-z{z}", self.serial),
        };
        let need = match kind {
            Kind::Change => self.cfg.m,
            Kind::Restore(_) => self.restore_need(),
        };
        self.cards.insert(digest.clone(), Card { height: h, kind, need, proposer: i });
        self.learn(i, &digest);
        self.cast_intent(i, &digest);
        self.send_all(i, &Msg::Card { digest });
    }

    fn cast_intent(&mut self, i: usize, digest: &str) {
        if let Some(Kind::Restore(z)) = self.cards.get(digest).map(|c| c.kind) {
            if z == i {
                return;
            }
        }
        self.seats[i].mine.insert(digest.to_string());
        self.seats[i].intents.entry(digest.to_string()).or_default().insert(i);
        self.send_all(i, &Msg::Intent { digest: digest.to_string(), from: i });
    }

    fn deliver(&mut self, i: usize, msg: Msg) {
        if !self.acting(i) {
            return;
        }
        match &msg {
            Msg::Prepare { slot, ballot } | Msg::Accept { slot, ballot, .. } => {
                let leader: usize = ballot.leader[1..].parse().expect("a seat");
                let r = self.seats[i].rounds_by.entry(leader).or_default();
                *r = (*r).max(ballot.round);
                self.seats[i].touched = self.seats[i].touched.max(slot.height);
                if self.passive_at(i, slot.height) {
                    return;
                }
            }
            Msg::Reply { reply: Reply::Promise { slot, .. } | Reply::Accepted { slot, .. }, .. } => {
                self.seats[i].touched = self.seats[i].touched.max(slot.height);
            }
            Msg::AmnesiaAsk { from, nonce } => {
                // a seat that lost its own state knows nothing to report
                if self.seats[i].amnesia.is_some() {
                    return;
                }
                let touched = self.seats[i].touched.max(self.seats[i].decided.keys().max().copied().unwrap_or(0) + 1);
                let round = self.seats[i].rounds_by.get(from).copied().unwrap_or(0);
                self.net.push((*from, Msg::AmnesiaReply { from: i, nonce: *nonce, touched, round }));
                return;
            }
            Msg::AmnesiaReply { from, nonce, touched, round } => {
                let need = self.cfg.m.max(self.cfg.n - self.cfg.m + 1);
                if let Some(a) = self.seats[i].amnesia.as_mut().filter(|a| a.nonce == *nonce && *from != i) {
                    a.replies.insert(*from, (*touched, *round));
                    if a.floor.is_none() && a.replies.len() >= need {
                        let floor = a.replies.values().map(|(t, _)| *t).max().unwrap_or(0);
                        let rounds = a.replies.values().map(|(_, r)| *r).max().unwrap_or(0);
                        a.floor = Some((floor, rounds));
                    }
                }
                return;
            }
            _ => {}
        }
        match msg {
            Msg::Card { digest } => {
                let fresh = !self.seats[i].knows.contains_key(&digest);
                self.learn(i, &digest);
                if fresh && self.rng.chance(self.cfg.approve) {
                    self.cast_intent(i, &digest);
                }
            }
            Msg::Intent { digest, from } => {
                self.seats[i].intents.entry(digest).or_default().insert(from);
            }
            Msg::Withdraw { digest } => {
                self.learn(i, &digest);
                if let Some(v) = self.seats[i].knows.get_mut(&digest) {
                    v.withdrawn = true;
                }
            }
            Msg::Prepare { slot, ballot } if self.refuses_frame(i, &ballot) => {
                let _ = slot;
            }
            Msg::Prepare { slot, ballot } => {
                let seen = self.seats[i].seen_round.entry(slot.height).or_default();
                *seen = (*seen).max(ballot.round);
                if self.seats[i].decided.contains_key(&slot.height) {
                    return;
                }
                let step = self.seats[i].acceptor.prepare(&slot, &ballot);
                if let Some(reply) = self.step_acceptor(i, step) {
                    self.on_reply(i, i, reply.clone());
                    self.send_all(i, &Msg::Reply { from: i, reply });
                }
            }
            Msg::Accept { ballot, .. } if self.refuses_frame(i, &ballot) => {}
            Msg::Accept { slot, ballot, value, forced } => {
                // a membership change rides the frame: the seat learns it
                self.learn(i, &value.digest);
                if self.seats[i].decided.contains_key(&slot.height) || !self.accepts(i, &slot, &value, forced) {
                    return;
                }
                let step = self.seats[i].acceptor.accept(&slot, &ballot, &value);
                if let Some(reply) = self.step_acceptor(i, step) {
                    self.on_reply(i, i, reply.clone());
                    self.send_all(i, &Msg::Reply { from: i, reply });
                }
            }
            Msg::Reply { from, reply } => self.on_reply(i, from, reply),
            Msg::Decided { height, value } => self.decide(i, height, &value, false),
            Msg::AmnesiaAsk { .. } | Msg::AmnesiaReply { .. } => {}
        }
    }

    /// `lock_acceptor_accept`'s judgement.
    fn accepts(&self, i: usize, slot: &Slot, value: &Value, forced: bool) -> bool {
        if self.seats[i].acceptor.holds(slot, value) {
            return true;
        }
        let Some(card) = self.cards.get(&value.digest) else { return false };
        // validity: a restore's subject never signs its own restore
        if card.kind == Kind::Restore(i) {
            return false;
        }
        if forced && self.fix.h1 {
            return true;
        }
        let Some(view) = self.seats[i].knows.get(&value.digest) else { return false };
        let mine = self.seats[i].mine.contains(&value.digest);
        if forced && view.withdrawn && mine {
            return true;
        }
        if view.withdrawn || !mine {
            return false;
        }
        if let Kind::Restore(z) = card.kind {
            if self.acting(z) || self.step < view.known_at + SETTLE {
                return false;
            }
        }
        true
    }

    fn step_acceptor(&mut self, i: usize, step: Step) -> Option<Reply> {
        if step.persist {
            if self.rng.chance(self.crash) {
                self.restart(i);
                return None;
            }
            self.seats[i].disk = serde_json::to_string(&self.seats[i].acceptor).expect("serializes");
        }
        if let Reply::Accepted { slot, ballot, value } = &step.reply {
            self.ghost.entry(slot.height).or_default().entry((ballot.clone(), value.clone())).or_default().insert(i);
        }
        Some(step.reply)
    }

    fn restart(&mut self, i: usize) {
        let seat = &mut self.seats[i];
        seat.acceptor = serde_json::from_str(&seat.disk).expect("deserializes");
        seat.runs.clear();
        seat.tallies.clear();
    }

    fn start_ballot(&mut self, i: usize, h: u64) {
        if !self.may_lead(i) || self.seats[i].decided.contains_key(&h) || self.passive_at(i, h) {
            return;
        }
        // a free pick whose ballot did not decide goes to the back (R5/T2)
        if let Some(d) = self.seats[i].runs.get(&h).filter(|r| !r.forced).and_then(|r| r.leader.proposed()).map(|v| v.digest.clone()) {
            let order = self.seats[i].stale.entry(h).or_default();
            order.retain(|x| *x != d);
            order.push(d);
        }
        let s = Self::slot(h);
        let reported = self.seats[i].amnesia.as_ref().and_then(|a| a.floor).map_or(0, |(_, r)| r);
        let round = self.seats[i]
            .acceptor
            .next_round(&s)
            .max(self.seats[i].seen_round.get(&h).copied().unwrap_or(0) + 1)
            .max(reported + 1);
        self.seats[i].touched = self.seats[i].touched.max(h);
        let ballot = Ballot { round, leader: name(i) };
        let q = self.quorum();
        self.seats[i].runs.insert(h, Run { leader: Leader::new(s.clone(), q, ballot.clone()), forced: false, waiting: false });
        self.seats[i].seen_round.insert(h, round);
        let msg = Msg::Prepare { slot: s, ballot };
        self.deliver(i, msg.clone());
        self.send_all(i, &msg);
    }

    fn on_reply(&mut self, i: usize, from: usize, reply: Reply) {
        match reply {
            Reply::Promise { slot, ballot, accepted } => self.on_promise(i, from, &slot, &ballot, accepted),
            Reply::Accepted { slot, ballot, value } => self.on_accepted(i, from, &slot, &ballot, &value),
            Reply::Nack { slot, promised: Some(p), .. } => {
                let seen = self.seats[i].seen_round.entry(slot.height).or_default();
                *seen = (*seen).max(p.round);
            }
            Reply::Nack { .. } => {}
        }
    }

    fn on_promise(&mut self, i: usize, from: usize, slot: &Slot, ballot: &Ballot, accepted: Option<(Ballot, Value)>) {
        if ballot.leader != name(i) {
            return;
        }
        if let Some((_, v)) = &accepted {
            self.learn(i, &v.digest);
        }
        // a seat known as a restore's subject is no promiser
        if from != i && self.subjects_of(i).contains(&from) {
            return;
        }
        let excuse = accepted.as_ref().and_then(|(_, v)| match self.cards.get(&v.digest)?.kind {
            Kind::Restore(z) => Some((v.digest.clone(), z)),
            Kind::Change => None,
        });
        let Some(run) = self.seats[i].runs.get_mut(&slot.height) else { return };
        if run.leader.ballot != *ballot {
            return;
        }
        if let Some((d, z)) = excuse {
            run.leader.excuse(&d, &name(z));
        }
        run.leader.promise(&name(from), ballot, accepted);
        self.try_propose(i, slot.height, false);
    }

    /// `lock_try_propose`, with S3's one-interval wait.
    fn try_propose(&mut self, i: usize, h: u64, interval_passed: bool) {
        let n = self.cfg.n;
        let Some(run) = self.seats[i].runs.get(&h) else { return };
        if !run.leader.ready() || run.leader.proposed().is_some() {
            return;
        }
        let forced = run.leader.forced();
        if forced.is_some() && run.leader.heard() < n && !interval_passed {
            if let Some(run) = self.seats[i].runs.get_mut(&h) {
                run.waiting = true;
            }
            return;
        }
        let own = if forced.is_some() { None } else { self.free_pick(i, h) };
        let Some(run) = self.seats[i].runs.get_mut(&h) else { return };
        let Some(value) = run.leader.propose(own) else { return };
        run.forced = forced.is_some();
        run.waiting = false;
        let msg = Msg::Accept { slot: run.leader.slot.clone(), ballot: run.leader.ballot.clone(), value, forced: run.forced };
        self.deliver(i, msg.clone());
        self.send_all(i, &msg);
    }

    /// `lock_choose`: a card the leader knows, not withdrawn there, with
    /// voices >= need (any seat's intent, a dead seat's too) and its gates.
    fn free_pick(&self, i: usize, h: u64) -> Option<Value> {
        let stale = self.seats[i].stale.get(&h).cloned().unwrap_or_default();
        let ready: Vec<Value> = self.ready_cards(i, h);
        ready
            .iter()
            .find(|v| !stale.contains(&v.digest))
            .or_else(|| stale.iter().find_map(|d| ready.iter().find(|v| v.digest == *d)))
            .cloned()
    }

    fn ready_cards(&self, i: usize, h: u64) -> Vec<Value> {
        let subjects = self.subjects_of(i);
        self.seats[i].knows.iter().filter_map(|(d, view)| {
            let card = self.cards.get(d)?;
            if card.height != h || view.withdrawn {
                return None;
            }
            if let Kind::Restore(z) = card.kind {
                // the restore gate (seat still active) and the settle
                if self.acting(z) || self.step < view.known_at + SETTLE {
                    return None;
                }
            }
            // the fix counts a subject's intent for every other card
            let voices = self.seats[i]
                .intents
                .get(d)
                .map_or(0, |s| s.iter().filter(|v| self.fix.h2 || !subjects.contains(v)).count());
            (voices >= card.need).then(|| Value { digest: d.clone(), need: card.need })
        })
        .collect()
    }

    fn on_accepted(&mut self, i: usize, from: usize, slot: &Slot, ballot: &Ballot, value: &Value) {
        let restore = matches!(self.cards.get(&value.digest).map(|c| c.kind), Some(Kind::Restore(_)));
        if restore && from != i && self.subjects_of(i).contains(&from) {
            return;
        }
        let sealable = self.seats[i].tallies.entry(slot.height).or_default().accepted(&name(from), ballot, value);
        if let Some(s) = sealable {
            self.seal(i, s);
        }
    }

    /// `lock_seal`: the restore gate holds a restore while its subject is
    /// heard (unless the M3 fix).
    fn seal(&mut self, i: usize, s: Sealable) {
        let h = Self::slot_height(&s);
        if let Some(Kind::Restore(z)) = self.cards.get(&s.value.digest).map(|c| c.kind) {
            if !self.fix.m3 && self.acting(z) {
                self.seats[i].held.insert(h, s);
                return;
            }
        }
        self.seats[i].held.remove(&h);
        self.decide(i, h, &s.value, true);
    }

    fn slot_height(s: &Sealable) -> u64 {
        // the sealable's height is carried by the tally's key in the seat
        s.value.digest[1..].split('-').next().and_then(|x| x.parse().ok()).expect("a height in the digest")
    }

    fn decide(&mut self, i: usize, h: u64, value: &Value, announce: bool) {
        match self.seats[i].decided.get(&h) {
            Some(known) if known != value => self.learned_twice += 1,
            Some(_) => {}
            None => {
                self.seats[i].decided.insert(h, value.clone());
                if announce {
                    self.send_all(i, &Msg::Decided { height: h, value: value.clone() });
                }
            }
        }
    }

    fn tick(&mut self, i: usize) {
        if !self.acting(i) {
            return;
        }
        let heights: Vec<u64> = self.seats[i].runs.keys().copied().collect();
        for h in heights {
            if self.seats[i].decided.contains_key(&h) {
                continue;
            }
            let waiting = self.seats[i].runs.get(&h).is_some_and(|r| r.waiting);
            if waiting {
                self.try_propose(i, h, true);
            }
            // the leader re-sends its Accept
            if let Some(run) = self.seats[i].runs.get(&h) {
                if let Some(v) = run.leader.proposed() {
                    let msg = Msg::Accept { slot: run.leader.slot.clone(), ballot: run.leader.ballot.clone(), value: v.clone(), forced: run.forced };
                    self.send_all(i, &msg);
                }
            }
        }
        let held: Vec<Sealable> = self.seats[i].held.values().cloned().collect();
        for s in held {
            self.seal(i, s);
        }
    }

    fn lifecycle(&mut self) {
        let (n, lost, dies) = (self.cfg.n, self.cfg.lost, self.cfg.dies);
        if self.step == self.cfg.die_at {
            for z in n - lost - dies..n - lost {
                self.life[z] = Life::Dead;
            }
        }
        if self.cfg.revive > 0 && self.step == self.cfg.revive_at {
            let mut left = self.cfg.revive;
            for z in (0..n).rev() {
                if left > 0 && self.life[z] == Life::Dead {
                    self.life[z] = Life::Revived;
                    left -= 1;
                }
            }
        }
    }

    fn act(&mut self) {
        let n = self.cfg.n;
        let roll = self.rng.next_u64() % 100;
        let i = self.rng.below(n);
        let h = 1 + self.rng.next_u64() % self.cfg.heights;
        if roll < 6 {
            if self.acting(i) && self.life[i] == Life::Alive {
                let dead: Vec<usize> = (0..n).filter(|z| matches!(self.life[*z], Life::Dead | Life::Revived)).collect();
                let kind = if !dead.is_empty() && self.rng.chance(50) {
                    Kind::Restore(dead[self.rng.below(dead.len())])
                } else {
                    Kind::Change
                };
                self.new_card(i, h, kind);
            }
        } else if roll < 6 + self.cfg.withdraw {
            let mine: Vec<String> = self.cards.iter().filter(|(_, c)| c.proposer == i).map(|(d, _)| d.clone()).collect();
            if self.acting(i) && !mine.is_empty() {
                let d = mine[self.rng.below(mine.len())].clone();
                if let Some(v) = self.seats[i].knows.get_mut(&d) {
                    v.withdrawn = true;
                }
                self.send_all(i, &Msg::Withdraw { digest: d });
            }
        } else if roll < 16 + self.cfg.withdraw {
            self.start_ballot(i, h);
        } else if roll < 22 + self.cfg.withdraw {
            self.tick(i);
        } else if roll < 22 + self.cfg.withdraw + self.crash {
            if self.acting(i) {
                self.restart(i);
            }
        } else if !self.net.is_empty() {
            let k = self.rng.below(self.net.len());
            let (to, msg) = self.net.swap_remove(k);
            if !self.rng.chance(self.drop) {
                self.deliver(to, msg);
            } else if matches!(msg, Msg::Card { .. } | Msg::Intent { .. } | Msg::Withdraw { .. }) {
                self.deferred.push((to, msg));
            }
        }
    }

    fn chaos(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step += 1;
            self.lifecycle();
            if self.cfg.amnesia > 0 && self.rng.next_u64() % 1000 < self.cfg.amnesia {
                let i = self.rng.below(self.cfg.n);
                if self.acting(i) {
                    self.amnesia(i);
                }
            }
            self.act();
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
            .filter(|h| (0..self.cfg.n).any(|i| self.acting(i) && !self.seats[i].decided.contains_key(h)))
            .collect()
    }

    /// Reliable delivery, no crashes: every height must decide on every
    /// acting seat; new cards (a restore where a seat is lost) keep coming.
    fn settle(&mut self) -> usize {
        self.drop = 0;
        self.crash = 0;
        let late = std::mem::take(&mut self.deferred);
        self.net.extend(late);
        for round in 0..30 {
            self.step += SETTLE;
            self.drain();
            let undecided = self.undecided();
            if undecided.is_empty() {
                return 0;
            }
            let leader = (0..self.cfg.n).find(|i| self.may_lead(*i) && self.life[*i] == Life::Alive);
            for h in undecided {
                if let Some(i) = (0..self.cfg.n).find(|i| self.seats[*i].decided.contains_key(&h)) {
                    let value = self.seats[i].decided[&h].clone();
                    self.send_all(i, &Msg::Decided { height: h, value });
                    continue;
                }
                let Some(l) = leader else { continue };
                if round % 3 == 0 {
                    let dead: Vec<usize> = (0..self.cfg.n).filter(|z| matches!(self.life[*z], Life::Dead | Life::Revived)).collect();
                    let kind = dead.first().map_or(Kind::Change, |z| Kind::Restore(*z));
                    self.new_card(l, h, Kind::Change);
                    if kind != Kind::Change {
                        self.new_card(l, h, kind);
                    }
                }
                self.drain();
                self.step += SETTLE;
                for i in 0..self.cfg.n {
                    self.tick(i);
                }
                self.drain();
                if !self.seats[l].decided.contains_key(&h) {
                    self.start_ballot(l, h);
                }
            }
        }
        self.undecided().len()
    }

    fn outcome(&self) -> WOutcome {
        let mut out = WOutcome { learned_twice: self.learned_twice, ..WOutcome::default() };
        for by in self.ghost.values() {
            let chosen: BTreeSet<&Value> = by.iter().filter(|((_, v), who)| who.len() >= v.need).map(|((_, v), _)| v).collect();
            out.forks += usize::from(chosen.len() > 1);
            out.decided += usize::from(!chosen.is_empty());
        }
        out
    }
}

pub(super) fn run(cfg: WConfig, fix: Fixes, seed: u64) -> WOutcome {
    let mut sim = Sim::new(cfg, fix, seed);
    sim.chaos(700);
    let mut out = sim.outcome();
    out.undecided = sim.settle();
    let after = sim.outcome();
    out.forks = out.forks.max(after.forks);
    out.learned_twice = after.learned_twice;
    out.decided = after.decided;
    out
}

pub(super) fn base(n: usize, m: usize) -> WConfig {
    WConfig {
        n,
        m,
        heights: 3,
        lost: 0,
        dies: 0,
        die_at: 0,
        revive: 0,
        revive_at: 0,
        withdraw: 3,
        approve: 85,
        drop: 15,
        amnesia: 0,
        tail_loss: false,
    }
}

pub(super) fn configs() -> Vec<(&'static str, WConfig)> {
    vec![
        ("2-of-3", base(3, 2)),
        ("3-of-5", base(5, 3)),
        ("2-of-3, a seat dies", WConfig { dies: 1, die_at: 150, ..base(3, 2) }),
        ("3-of-5, a seat dies", WConfig { dies: 1, die_at: 150, ..base(5, 3) }),
        ("2-of-3, lost, comes back", WConfig { lost: 1, revive: 1, revive_at: 250, ..base(3, 2) }),
        ("3-of-5, lost, comes back", WConfig { lost: 1, revive: 1, revive_at: 250, ..base(5, 3) }),
        ("4-of-7, lost", WConfig { lost: 1, ..base(7, 4) }),
        ("4-of-7, lost, comes back", WConfig { lost: 1, revive: 1, revive_at: 250, ..base(7, 4) }),
        ("3-of-5, two lost", WConfig { lost: 2, ..base(5, 3) }),
        ("3-of-5, two lost, one back", WConfig { lost: 2, revive: 1, revive_at: 250, ..base(5, 3) }),
        ("3-of-3, lost", WConfig { lost: 1, ..base(3, 3) }),
        ("2-of-2, lost", WConfig { lost: 1, ..base(2, 2) }),
        ("2-of-3, dies, comes back", WConfig { dies: 1, die_at: 150, revive: 1, revive_at: 350, ..base(3, 2) }),
    ]
}

// ---- tests ----

const SIM_SEEDS: u64 = 600;

fn find(cfg: WConfig, fix: Fixes, what: fn(&WOutcome) -> bool) -> Option<u64> {
    (0..SIM_SEEDS * 4).find(|seed| what(&run(cfg, fix, *seed)))
}

/// The wire rules as of the third review: the simulation finds H2's fork
/// (a lost seat's old device returns and promises where a restore was
/// chosen with need m - 1) and H1's halt (a forced value only approvers
/// may complete, one of them dead).
#[test]
fn the_wire_rules_before_the_fourth_review_fork_and_halt() {
    let none = Fixes::default();
    let fork = find(WConfig { lost: 1, revive: 1, revive_at: 250, drop: 45, ..base(3, 2) }, none, |o| o.forks > 0);
    eprintln!("H2 fork (2-of-3, lost, comes back): seed {fork:?}");
    let halt = find(WConfig { dies: 1, die_at: 150, ..base(3, 2) }, none, |o| o.undecided > 0);
    eprintln!("H1 halt (2-of-3, a seat dies): seed {halt:?}");
    let (Some(fork), Some(halt)) = (fork, halt) else { panic!("H2 or H1 not found") };
    let h2 = WConfig { lost: 1, revive: 1, revive_at: 250, drop: 45, ..base(3, 2) };
    assert_eq!(run(h2, Fixes { h2: true, ..Fixes::default() }, fork).forks, 0, "the H2 fix closes its seed");
    let h1 = WConfig { dies: 1, die_at: 150, ..base(3, 2) };
    assert_eq!(run(h1, Fixes { h1: true, ..Fixes::default() }, halt).undecided, 0, "the H1 fix closes its seed");
}

/// With the fixes (need(R) = m where m < n, a self-subject never leads,
/// any promiser completes a forced value, the seal never gates): no fork,
/// every height decides - over every shape, lost, dying, coming back,
/// two lost, n-of-n.
#[test]
fn with_the_fixes_no_fork_and_every_height_decides() {
    let fix = Fixes { h2: true, h1: true, m3: true, amnesia_rule: false };
    for (name, cfg) in configs() {
        for drop in [15, 45] {
            for seed in 0..SIM_SEEDS {
                let out = run(WConfig { drop, ..cfg }, fix, seed);
                assert_eq!(out.forks, 0, "{name} drop {drop}: fork at seed {seed}");
                assert_eq!(out.learned_twice, 0, "{name} drop {drop}: learned twice at seed {seed}");
                assert_eq!(out.undecided, 0, "{name} drop {drop}: undecided at seed {seed}");
            }
        }
    }
}

fn amnesia_configs() -> Vec<(&'static str, WConfig)> {
    vec![
        ("2-of-3", WConfig { amnesia: 8, ..base(3, 2) }),
        ("3-of-5", WConfig { amnesia: 8, ..base(5, 3) }),
        ("2-of-5", WConfig { amnesia: 8, ..base(5, 2) }),
        ("3-of-4", WConfig { amnesia: 8, ..base(4, 3) }),
        ("2-of-3, tail lost", WConfig { amnesia: 8, tail_loss: true, ..base(3, 2) }),
        ("3-of-5, tail lost", WConfig { amnesia: 8, tail_loss: true, ..base(5, 3) }),
    ]
}

/// Amnesia (B-F2, the corrected rule): a seat that lost its lock state
/// (as acceptor, promiser or leader, with or without its newest chain
/// heights) asks the others; with replies from max(m, n - m + 1) OTHER
/// seats it is passive up to the highest height they report it may have
/// touched, and leads above the highest round they saw from it. The
/// control (carrying on at once) forks; the rule never does. m = n is
/// left out on purpose: no reply set proves anything there, the
/// recovery ritual is the only exit.
#[test]
fn an_amnesiac_seat_waits_for_the_others_and_never_forks() {
    let fix = Fixes { h2: true, h1: true, m3: true, amnesia_rule: true };
    let control = Fixes { amnesia_rule: false, ..fix };
    let mut control_forks = 0;
    for (name, cfg) in amnesia_configs() {
        for seed in 0..SIM_SEEDS {
            let out = run(cfg, fix, seed);
            assert_eq!(out.forks, 0, "{name}: fork at seed {seed}");
            assert_eq!(out.learned_twice, 0, "{name}: learned twice at seed {seed}");
            control_forks += run(cfg, control, seed).forks;
        }
    }
    assert!(control_forks > 0, "amnesia without the rule never forked");
    eprintln!("amnesia without the rule: {control_forks} forks");
}
