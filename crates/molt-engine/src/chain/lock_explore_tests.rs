// SPDX-License-Identifier: GPL-3.0-or-later

//! Exhaustive small-scope model checker of the sealing lock at ONE slot
//! (`docs/chain/chain_lock_design.md` §4, §4.2, §6, §14). Explicit-state
//! BFS over every interleaving. Each seat runs the REAL `Acceptor` and
//! `Leader` of `lock.rs`: a seat stores them compactly (the acceptor as
//! its promise/acceptance/free mark at the slot, the leader as the log of
//! its calls) and rebuilds them for every step. The wire rules of
//! `lock_wire.rs` are mirrored per seat, on that seat's own knowledge.
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
//! | choice | `lock_choose`, `lock_choose_skipping` | smallest known open id; voices minus subjects, free_seen; >= max(m-1,1) and need; restore gate; stale order; F4: voices of promisers only |
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
//! | saw / yield | `lock_saw` | highest ballot (also while behind); a higher foreign ballot drops the run |
//! | frames | `lock_on_prepare`..`lock_on_nack` | per frame, incl. locked/accepted_seen notes and free Nack |
//! | who leads | `lock_drive`, `lock_tick`, `lock_line` | main: any seat (timing free) unless subject in own view, then only when ready; live: rank 0 of alive non-subjects or ready; a busy run may restart; F2: a subject in own view never |
//! | withdrawal | `proposals.rs` withdraw | proposer; own approval dropped, intent kept, registration gone |
//! | amnesia floor | `lock_note_peer_height`, `lock_amnesiac_at` | rule A as built (first peer head); B, C candidate rules (options) |
//!
//! Heights: the slot is s; a seat's head is s-2 (behind: frames at s are
//! only seen), s-1 (taking part) or s (sealed). Block s-1 always exists.
//!
//! NOT MODELLED: cuts (need n); more than one slot, reorgs, lineages;
//! Committed blocks reaching others (a sealer stops, others go on - as if
//! every block frame were lost); declines; persistence failures; the
//! restore settle timer (any order allowed); B3's window (a revived
//! subject is heard forever, pre-loss frames never); `chain_unsettled`;
//! takeover/backoff timing (any order allowed); local vault/wallet gates;
//! B6 free sealing is global (m free signatures anywhere seal, restore
//! needs max(m-1,1) free); an old-build seat keeps no intents; with `lost`
//! the subject's device holds no lock state before it is lost; amnesia
//! keeps the event log (cards, intents).
//! Approximated: a frame is lost only when its sender dies (all its frames
//! in flight at once) - a loss between two live seats is a delay, since
//! the fault-free continuation re-sends (leader retransmit, re-served
//! cards, withdrawals, intents, asks); retransmits are not explored before
//! it (a duplicate is the `max_dups` fault). Without an older-build seat
//! the free/locked/accepted-seen sets are not kept (nobody is free there).
//! Liveness = some fault-free continuation seals (a fresh card `late` may
//! appear in it), within `live_extra` more ballots per seat, asked only
//! while max(n-m+1, m) seats are alive.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use super::lock::{Acceptor, Ballot, Leader, Quorum, Reply, Slot, Value};

const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];
const H: u64 = 1;

/// A ballot as (round, leader seat); seat order = name order.
type B = (u8, u8);

fn slot() -> Slot {
    Slot { height: H, base: "l".into() }
}

fn name(i: usize) -> String {
    NAMES[i].to_string()
}

fn u(i: usize) -> u8 {
    u8::try_from(i).expect("small index")
}

fn bit(i: usize) -> u8 {
    1u8 << i
}

fn has(mask: u8, i: usize) -> bool {
    mask & bit(i) != 0
}

fn ones(mask: u8) -> usize {
    usize::try_from(mask.count_ones()).expect("count")
}

fn ballot(b: B) -> Ballot {
    Ballot { round: u64::from(b.0), leader: name(usize::from(b.1)) }
}

fn bal(b: &Ballot) -> B {
    let seat = NAMES.iter().position(|x| *x == b.leader).expect("model seat name");
    (u8::try_from(b.round).expect("small round"), u(seat))
}

fn digest(c: usize) -> String {
    format!("v{c}")
}

fn card_of(d: &str) -> usize {
    d[1..].parse().expect("model digest")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fixes {
    f1: bool,
    f2: bool,
    f3: bool,
    f4: bool,
}

impl Fixes {
    const ALL: Fixes = Fixes { f1: true, f2: true, f3: true, f4: true };
}

/// The amnesia floor under test (§6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// As built: passive up to 1 + max(own head, the first peer head heard).
    A,
    /// Passive until n-m+1 distinct other seats answered; floor = 1 + their max head.
    B,
    /// max(m, n-m+1) answers; floor = max `touched`; own ballots above the
    /// highest round any answer saw from this seat; an amnesiac seat
    /// answers no ask.
    C,
    /// O3 as on master: an import opens with the lock state lost and no
    /// amnesia at all.
    Import,
}

#[derive(Clone, Debug)]
struct Card {
    proposer: usize,
    restore_of: Option<usize>,
    withdrawable: bool,
    /// Proposed only in a fault-free continuation: a fresh card the slot
    /// should still decide.
    late: bool,
    /// Scope: the seats that may ever approve it.
    approvers: u8,
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
    /// Seats whose head starts at s-2.
    behind: u8,
    max_ballots: u8,
    live_extra: u8,
    max_deaths: u8,
    max_revivals: u8,
    max_dups: u8,
    /// A dying seat may lose its frames in flight.
    drops: bool,
    amnesia: Option<Rule>,
    max_amnesia: u8,
    max_announce: u8,
    /// A recovered seat accepts an n-value only once every other seat
    /// reported it as its latest acceptance.
    all_report: bool,
    max_msgs: usize,
    max_states: usize,
    /// Scope: the seats that may open a ballot, and that may die.
    may_lead: u8,
    may_die: u8,
    /// Scope cut: Promise, Accepted and Nack go to the ballot's leader only
    /// (no learner seals, no third-party `seen`/yield).
    replies_to_leader: bool,
    live_budget: usize,
    fixes: Fixes,
    /// The wire of 8f9e7ad3..b12c97a6 instead of the pinned one: need(R)
    /// = m for m < n; subjects from open restores (a heard subject is none
    /// where m < n) plus held values; acceptors refuse a subject's Prepare
    /// and Accept; a subject never leads; voices keep subjects; any seat
    /// completes a forced value carried on the Accept; a value's own
    /// subject never counts.
    v2: bool,
    /// O2: signatures of OTHER seats a seat without its own intent needs
    /// on a forced value (0 = master: none).
    evidence: u8,
    /// One Byzantine seat: sends any frame, signs anything, forges nothing.
    byz: Option<usize>,
    max_byz: u8,
    /// The operator may approve a withdrawn card to complete it.
    operator: bool,
}

/// What a seat tells an amnesiac one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Rep {
    head: u8,
    touched: u8,
    round: u8,
    latest: Option<u8>,
    answer: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum P {
    Card(u8),
    Intent(u8),
    Withdraw(u8),
    FreeSig(u8),
    Prepare(u8),
    Promise(B, Option<(B, u8)>),
    /// round, card, forced, evidence (signers carried).
    Accept(u8, u8, bool, u8),
    Accepted(B, u8),
    Nack(B, Option<B>),
    Ask,
    Report(Rep),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Msg {
    to: u8,
    from: u8,
    inc: u8,
    p: P,
}

/// One call on the real `Leader`, replayed to rebuild it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LCall {
    Promise(u8, Option<(B, u8)>),
    HeardFree(u8),
    Excuse(u8, u8),
    Propose(Option<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Run {
    ballot: B,
    log: Vec<LCall>,
    promised: u8,
    ids: u8,
    value: Option<u8>,
    s3_over: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Seat {
    alive: bool,
    inc: u8,
    // the acceptor at the slot
    promised: Option<B>,
    accepted: Option<(B, u8)>,
    free: bool,
    run: Option<Run>,
    known: u8,
    withdrawn: u8,
    own: u8,
    intents: [u8; 4],
    free_seen: u8,
    locked_seen: u8,
    accepted_seen: u8,
    seen: Option<B>,
    lock_known: u8,
    sigs: Vec<(B, u8, u8)>,
    learned: Option<(B, u8)>,
    stale: Vec<u8>,
    heard_subject: bool,
    retry: Option<(B, u8, bool, u8)>,
    sealed: Option<u8>,
    ballots: u8,
    /// 0 = s-2, 1 = s-1, 2 = s.
    head: u8,
    /// Amnesiac, the floor not fixed yet: passive.
    am: bool,
    /// The floor covers s.
    passive: bool,
    recovered: bool,
    round_floor: u8,
    /// Lock frames at s seen here.
    touched: bool,
    seen_from: [u8; 5],
    answered: u8,
    reps: [Option<Rep>; 5],
}

impl Seat {
    fn new(alive: bool, head: u8) -> Self {
        Seat {
            alive,
            inc: 0,
            promised: None,
            accepted: None,
            free: false,
            run: None,
            known: 0,
            withdrawn: 0,
            own: 0,
            intents: [0; 4],
            free_seen: 0,
            locked_seen: 0,
            accepted_seen: 0,
            seen: None,
            lock_known: 0,
            sigs: Vec::new(),
            learned: None,
            stale: Vec::new(),
            heard_subject: false,
            retry: None,
            sealed: None,
            ballots: 0,
            head,
            am: false,
            passive: false,
            recovered: false,
            round_floor: 0,
            touched: false,
            seen_from: [0; 5],
            answered: 0,
            reps: [None; 5],
        }
    }

    /// Volatile state goes with the process; acceptor and log stay.
    fn forget_volatile(&mut self) {
        self.run = None;
        self.free_seen = 0;
        self.locked_seen = 0;
        self.accepted_seen = 0;
        self.seen = None;
        self.lock_known = 0;
        self.sigs.clear();
        self.learned = None;
        self.stale.clear();
        self.heard_subject = false;
        self.retry = None;
        self.touched = false;
        self.seen_from = [0; 5];
        self.answered = 0;
        self.reps = [None; 5];
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    seats: Vec<Seat>,
    msgs: Vec<(Msg, u8)>,
    born: u8,
    withdrawn: u8,
    /// History: every acceptance ever given, per ballot and card.
    hist: Vec<(B, u8, u8)>,
    free_sigs: [u8; 4],
    chosen: u8,
    sealed_lock: u8,
    sealed_free: u8,
    deaths: u8,
    revivals: u8,
    dups: u8,
    amnesias: u8,
    announces: u8,
    /// Every seat that ever signed each card (any ballot).
    signed: [u8; 4],
    /// Cards some honest seat cast an intent for.
    honest_intent: u8,
    byz_acts: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Act {
    Deliver(Msg),
    Dup(Msg),
    Intent(u8, u8),
    Start(u8),
    Drive(u8),
    Retry(u8),
    Release(u8),
    LearnSeal(u8),
    FreeSeal(u8, u8),
    Withdraw(u8),
    Die(u8, bool),
    Revive(u8),
    Propose(u8),
    Resend(Msg),
    CatchUp(u8),
    Amnesia(u8, bool),
    Announce(u8),
    /// The Byzantine seat sends one frame.
    Byz(Msg),
    /// An operator approves a withdrawn card its seat never approved.
    OperatorApprove(u8, u8),
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

fn mask_add(v: &mut Vec<(B, u8, u8)>, b: B, c: u8, who: usize) -> u8 {
    match v.binary_search_by(|e| (e.0, e.1).cmp(&(b, c))) {
        Ok(k) => {
            v[k].2 |= bit(who);
            v[k].2
        }
        Err(k) => {
            v.insert(k, (b, c, bit(who)));
            bit(who)
        }
    }
}

fn mask_get(v: &[(B, u8, u8)], b: B, c: u8) -> u8 {
    v.binary_search_by(|e| (e.0, e.1).cmp(&(b, c))).map_or(0, |k| v[k].2)
}

fn msg_add(w: &mut World, m: Msg) {
    if let Err(k) = w.msgs.binary_search_by(|e| e.0.cmp(&m)) {
        w.msgs.insert(k, (m, 1));
    }
}

fn msg_take(w: &mut World, m: &Msg) {
    if let Ok(k) = w.msgs.binary_search_by(|e| e.0.cmp(m)) {
        w.msgs[k].1 -= 1;
        if w.msgs[k].1 == 0 {
            w.msgs.remove(k);
        }
    }
}

fn msg_count(w: &World) -> usize {
    w.msgs.iter().map(|(_, k)| usize::from(*k)).sum()
}

impl Config {
    fn q(&self) -> Quorum {
        Quorum { n: self.n, m: self.m }
    }

    fn card_name(&self, c: usize) -> String {
        match self.cards[c].restore_of {
            Some(z) => format!("R({})", NAMES[z].to_uppercase()),
            None if self.cards[c].late => "LATE".to_string(),
            None => ["X", "Y", "W", "V"][c.min(3)].to_string(),
        }
    }

    fn is_restore(&self, c: usize) -> bool {
        self.cards[c].restore_of.is_some()
    }

    fn need(&self, c: usize) -> usize {
        match self.cards[c].restore_of {
            Some(_) if (self.fixes.f1 || self.v2) && self.m < self.n => self.m,
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

    fn subject_seat(&self, j: usize) -> bool {
        self.cards.iter().any(|c| c.restore_of == Some(j))
    }

    // ---- the real types, rebuilt ----

    fn acceptor(&self, s: &Seat) -> Acceptor {
        let mut a = Acceptor::default();
        if s.free {
            let _ = a.sign_freely(H);
            return a;
        }
        if let Some((b, c)) = s.accepted {
            let _ = a.accept(&slot(), &ballot(b), &self.val(usize::from(c)));
        }
        if let Some(p) = s.promised {
            let _ = a.prepare(&slot(), &ballot(p));
        }
        a
    }

    fn store_acceptor(s: &mut Seat, a: &Acceptor) {
        s.free = a.signed_freely(H);
        s.promised = a.promised(&slot()).map(bal);
        s.accepted = a.accepted(&slot()).map(|(b, v)| (bal(b), u(card_of(&v.digest))));
    }

    /// Run `f` on the seat's real acceptor and keep what it left.
    fn with_acceptor<T>(&self, w: &mut World, i: usize, f: impl FnOnce(&mut Acceptor) -> T) -> T {
        let mut a = self.acceptor(&w.seats[i]);
        let out = f(&mut a);
        Self::store_acceptor(&mut w.seats[i], &a);
        out
    }

    fn leader(&self, run: &Run) -> Leader {
        let b = ballot(run.ballot);
        let mut l = Leader::new(slot(), self.q(), b.clone());
        for call in &run.log {
            match call {
                LCall::Promise(j, acc) => {
                    l.promise(&name(usize::from(*j)), &b, acc.map(|(ab, c)| (ballot(ab), self.val(usize::from(c)))));
                }
                LCall::HeardFree(j) => l.heard_free(&name(usize::from(*j))),
                LCall::Excuse(c, z) => l.excuse(&digest(usize::from(*c)), &name(usize::from(*z))),
                LCall::Propose(own) => {
                    let _ = l.propose(own.map(|c| self.val(usize::from(c))));
                }
            }
        }
        l
    }

    fn show(&self, a: &Act) -> String {
        let s = |i: &u8| NAMES[usize::from(*i)].to_uppercase();
        let c = |c: &u8| self.card_name(usize::from(*c));
        match a {
            Act::Deliver(m) => format!("deliver {}->{} {}", s(&m.from), s(&m.to), self.show_p(&m.p)),
            Act::Dup(m) => format!("DUP {}->{} {}", s(&m.from), s(&m.to), self.show_p(&m.p)),
            Act::Intent(i, x) => format!("{} intent {}", s(i), c(x)),
            Act::Start(i) => format!("{} opens a ballot", s(i)),
            Act::Drive(i) => format!("{} tick: proposes (S3 timer over)", s(i)),
            Act::Retry(i) => format!("{} tick: retries its withheld accept", s(i)),
            Act::Release(i) => format!("{} tick: release / free standing intents", s(i)),
            Act::LearnSeal(i) => format!("{} seals as learner", s(i)),
            Act::FreeSeal(i, x) => format!("{} seals {} from free signatures", s(i), c(x)),
            Act::Withdraw(x) => format!("{} WITHDRAWS {}", s(&u(self.cards[usize::from(*x)].proposer)), c(x)),
            Act::Die(i, lose) => format!("{} DIES{}", s(i), if *lose { ", its frames in flight lost" } else { "" }),
            Act::Revive(i) => format!("{} REVIVES (old device)", s(i)),
            Act::Propose(x) => format!("{} proposes {}", s(&u(self.cards[usize::from(*x)].proposer)), c(x)),
            Act::Resend(m) => format!("resend {}->{} {}", s(&m.from), s(&m.to), self.show_p(&m.p)),
            Act::CatchUp(i) => format!("{} adopts block s-1", s(i)),
            Act::Amnesia(i, tail) => {
                format!("{} LOSES its lock state{}", s(i), if *tail { " and its chain tail" } else { "" })
            }
            Act::Announce(i) => format!("{} gossips its head", s(i)),
            Act::Byz(m) => format!("BYZANTINE {}->{} {}", s(&m.from), s(&m.to), self.show_p(&m.p)),
            Act::OperatorApprove(i, x) => format!("{}'s operator approves {}", s(i), c(x)),
        }
    }

    fn show_p(&self, p: &P) -> String {
        let c = |c: &u8| self.card_name(usize::from(*c));
        let b = |b: &B| format!("{}:{}", b.0, NAMES[usize::from(b.1)].to_uppercase());
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
            P::Accept(r, x, f, ev) => format!(
                "Accept(r{r}, {}{}{})",
                c(x),
                if *f { ", forced" } else { "" },
                if *ev != 0 { format!(", evidence {ev:#b}") } else { String::new() }
            ),
            P::Accepted(bb, x) => format!("Accepted({}, {})", b(bb), c(x)),
            P::Nack(bb, p) => format!("Nack({}, {})", b(bb), p.as_ref().map_or("free".to_string(), b)),
            P::Ask => "Ask".to_string(),
            P::Report(r) => format!(
                "Report(head s{}, touched s{}, round {}, latest {}{})",
                i64::from(r.head) - 2,
                i64::from(r.touched) - 2,
                r.round,
                r.latest.as_ref().map_or("-".to_string(), c),
                if r.answer { "" } else { ", unasked" }
            ),
        }
    }

    // ---- views ----

    /// The restore cards whose subject this seat sees as one.
    fn subject_cards(&self, w: &World, i: usize) -> u8 {
        let s = &w.seats[i];
        let mut out = 0;
        for (c, card) in self.cards.iter().enumerate() {
            let Some(z) = card.restore_of else { continue };
            let mut registered = has(s.known, c) && !has(s.withdrawn, c);
            if self.v2 && self.m < self.n && z != i && s.heard_subject {
                registered = false;
            }
            let held = s.accepted.is_some_and(|(_, x)| usize::from(x) == c)
                || s.run.as_ref().is_some_and(|r| has(r.ids, c))
                || has(s.lock_known, c);
            if registered || held {
                out |= bit(c);
            }
        }
        out
    }

    fn subjects(&self, w: &World, i: usize) -> u8 {
        let cards = self.subject_cards(w, i);
        (0..self.cards.len())
            .filter(|c| has(cards, *c))
            .filter_map(|c| self.cards[c].restore_of)
            .fold(0, |m, z| m | bit(z))
    }

    /// `proposal_change(id)` resolves: a known card; a restore only while registered.
    fn resolvable(&self, w: &World, i: usize, c: usize) -> bool {
        let s = &w.seats[i];
        has(s.known, c) && (!self.is_restore(c) || !has(s.withdrawn, c))
    }

    fn known_free(&self, w: &World, i: usize) -> u8 {
        let s = &w.seats[i];
        let cand = s.free_seen & !bit(i) & !self.subjects(w, i) & !s.accepted_seen;
        let free = cand & !s.locked_seen;
        if self.q().lock_dead(ones(free), false) {
            cand
        } else {
            free
        }
    }

    fn lock_dead_at(&self, w: &World, i: usize) -> bool {
        self.q().lock_dead(ones(self.known_free(w, i)), w.seats[i].accepted.is_some())
    }

    fn locked_mode(&self, w: &World, i: usize) -> bool {
        if self.is_old(i) {
            return false;
        }
        let a = self.acceptor(&w.seats[i]);
        if !a.may_sign_freely(H) {
            return true;
        }
        !(a.signed_freely(H) || self.lock_dead_at(w, i))
    }

    /// The seat answers and leads at s (head s-1, no amnesia floor over s).
    fn takes_part(&self, w: &World, i: usize) -> bool {
        let s = &w.seats[i];
        s.alive && s.head == 1 && !s.am && !s.passive && s.sealed.is_none() && !self.is_old(i)
    }

    fn choose(&self, w: &World, i: usize, promised: Option<u8>) -> Option<usize> {
        let order = w.seats[i].stale.clone();
        let stale: u8 = order.iter().fold(0, |m, c| m | bit(usize::from(*c)));
        self.choose_skipping(w, i, promised, stale)
            .or_else(|| order.iter().find_map(|id| self.choose_skipping(w, i, promised, stale & !bit(usize::from(*id)))))
    }

    fn choose_skipping(&self, w: &World, i: usize, promised: Option<u8>, skip: u8) -> Option<usize> {
        let s = &w.seats[i];
        let subj = self.subjects(w, i);
        for c in 0..self.cards.len() {
            if has(skip, c) || !has(s.known, c) || has(s.withdrawn, c) {
                continue;
            }
            let mut voices = s.intents[c] & !s.free_seen;
            if !self.v2 {
                voices &= !subj;
            }
            if self.fixes.f4 {
                if let Some(p) = promised {
                    voices &= p;
                }
            }
            let voices = ones(voices);
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

    fn busy(&self, run: &Run) -> bool {
        let l = self.leader(run);
        !l.ready() || l.forced().is_some() || l.proposed().is_some()
    }

    // ---- effects ----

    fn send(&self, w: &mut World, from: usize, p: P, lock: bool) {
        for to in 0..self.n {
            if to == from || (lock && self.is_old(to)) || w.seats[to].sealed.is_some() || self.byz == Some(to) {
                continue;
            }
            if !w.seats[to].alive && w.revivals >= self.max_revivals {
                continue;
            }
            let msg = Msg { to: u(to), from: u(from), inc: w.seats[from].inc, p: p.clone() };
            msg_add(w, msg);
        }
    }

    fn reply(&self, w: &mut World, from: usize, leader: usize, p: P) {
        if !self.replies_to_leader {
            self.send(w, from, p, true);
        } else if leader != from && w.seats[leader].sealed.is_none() {
            self.send_to(w, from, leader, p);
        }
    }

    fn send_to(&self, w: &mut World, from: usize, to: usize, p: P) {
        if w.seats[to].alive || w.revivals < self.max_revivals {
            let msg = Msg { to: u(to), from: u(from), inc: w.seats[from].inc, p };
            msg_add(w, msg);
        }
    }

    fn sign_free(&self, w: &mut World, i: usize, c: usize) {
        if !self.is_old(i) && self.with_acceptor(w, i, |a| a.sign_freely(H)).is_none() {
            return;
        }
        w.free_sigs[c] |= bit(i);
        self.send(w, i, P::FreeSig(u(c)), false);
    }

    fn sign_standing(&self, w: &mut World, i: usize) {
        let s = &w.seats[i];
        let standing: Vec<usize> = (0..self.cards.len())
            .filter(|c| has(s.own, *c) && !has(s.withdrawn, *c) && !has(w.free_sigs[*c], i))
            .collect();
        for c in standing {
            self.sign_free(w, i, c);
        }
    }

    fn try_release(&self, w: &mut World, i: usize) {
        if self.is_old(i) || w.seats[i].free || !self.lock_dead_at(w, i) {
            return;
        }
        let free = ones(self.known_free(w, i));
        let q = self.q();
        if !self.with_acceptor(w, i, |a| a.release_to_free(H, q, free)) {
            return;
        }
        w.seats[i].run = None;
        self.sign_standing(w, i);
    }

    fn saw(&self, w: &mut World, i: usize, b: B) {
        let s = &mut w.seats[i];
        if s.seen.map_or(true, |x| b > x) {
            s.seen = Some(b);
        }
        let from = usize::from(b.1);
        if self.amnesia.is_some() {
            s.seen_from[from] = s.seen_from[from].max(b.0);
        }
        if from == i {
            return;
        }
        if s.run.as_ref().is_some_and(|r| r.ballot < b) {
            s.run = None;
        }
    }

    /// Append a leader call unless replaying it again changes nothing.
    fn log(run: &mut Run, call: LCall) {
        let repeat = match &call {
            LCall::Promise(j, _) => {
                run.log.iter().rev().find(|c| matches!(c, LCall::Promise(k, _) if k == j)) == Some(&call)
            }
            _ => run.log.contains(&call),
        };
        if !repeat {
            run.log.push(call);
        }
    }

    fn excuse(&self, w: &mut World, i: usize) {
        let Some(run) = w.seats[i].run.as_mut() else { return };
        for (c, card) in self.cards.iter().enumerate() {
            if let (Some(z), true) = (card.restore_of, has(run.ids, c)) {
                Self::log(run, LCall::Excuse(u(c), u(z)));
            }
        }
    }

    fn start(&self, w: &mut World, i: usize) {
        let known_free = self.known_free(w, i);
        let mut stale_pick = None;
        if let Some(run) = &w.seats[i].run {
            if self.leader(run).forced().is_none() {
                stale_pick = run.value;
            }
        }
        if let Some(c) = stale_pick {
            let s = &mut w.seats[i];
            s.stale.retain(|x| *x != c);
            s.stale.push(c);
        }
        let seen = w.seats[i].seen.map_or(0, |b| b.0);
        let next = self.acceptor(&w.seats[i]).next_round(&slot());
        let round = u8::try_from(next).expect("round").max(seen + 1).max(w.seats[i].round_floor + 1);
        let b = (round, u(i));
        let Reply::Promise { accepted, .. } = self.with_acceptor(w, i, |a| a.prepare(&slot(), &ballot(b))).reply else {
            return;
        };
        let acc = accepted.map(|(ab, v)| (bal(&ab), u(card_of(&v.digest))));
        let mut run = Run { ballot: b, log: Vec::new(), promised: bit(i), ids: 0, value: None, s3_over: false };
        if let Some((_, c)) = acc {
            run.ids |= bit(usize::from(c));
        }
        Self::log(&mut run, LCall::Promise(u(i), acc));
        for j in (0..self.n).filter(|j| has(known_free, *j)) {
            Self::log(&mut run, LCall::HeardFree(u(j)));
        }
        let s = &mut w.seats[i];
        s.seen = Some(b);
        if self.amnesia.is_some() {
            s.seen_from[i] = s.seen_from[i].max(round);
            s.touched = true;
        }
        s.run = Some(run);
        s.ballots += 1;
        self.excuse(w, i);
        self.send(w, i, P::Prepare(round), true);
        self.try_propose(w, i);
    }

    fn try_propose(&self, w: &mut World, i: usize) {
        if !self.takes_part(w, i) {
            return;
        }
        let Some(run) = &w.seats[i].run else { return };
        let l = self.leader(run);
        if !l.ready() || l.proposed().is_some() {
            return;
        }
        let forced = l.forced();
        let own = match &forced {
            Some(v) => {
                if !has(run.ids, card_of(&v.digest)) || (l.heard() < self.n && !run.s3_over) {
                    return;
                }
                None
            }
            None => {
                let promised = run.promised;
                let Some(c) = self.choose(w, i, Some(promised)) else { return };
                Some(u(c))
            }
        };
        let Some(run) = w.seats[i].run.as_mut() else { return };
        Self::log(run, LCall::Propose(own));
        let Some(v) = self.leader(run).proposed().cloned() else { return };
        let c = card_of(&v.digest);
        run.value = Some(u(c));
        let b = run.ballot;
        let is_forced = forced.is_some();
        let ev = match &w.seats[i].run {
            Some(run) if is_forced => self.evidence_of(w, i, run, c),
            _ => 0,
        };
        self.send(w, i, P::Accept(b.0, u(c), is_forced, ev), true);
        self.acceptor_accept(w, i, b, c, is_forced, ev);
    }

    fn withheld(&self, w: &World, i: usize, c: usize) -> bool {
        let s = &w.seats[i];
        if !has(s.own, c) || has(s.withdrawn, c) {
            return true;
        }
        self.cards[c].restore_of.is_some_and(|z| z == i || s.heard_subject)
    }

    /// The signatures of other seats a leader holds on `c`: promisers that
    /// reported holding it, and acceptances it counted.
    fn evidence_of(&self, w: &World, i: usize, run: &Run, c: usize) -> u8 {
        let mut ev = 0;
        for call in &run.log {
            if let LCall::Promise(j, Some((_, x))) = call {
                if usize::from(*x) == c {
                    ev |= bit(usize::from(*j));
                }
            }
        }
        for (_, x, who) in &w.seats[i].sigs {
            if usize::from(*x) == c {
                ev |= who;
            }
        }
        let s = &w.seats[i];
        let own = s.accepted.is_some_and(|(_, x)| usize::from(x) == c) || has(s.intents[c], i);
        // the leader adds its own signature only where it would accept itself
        if own || ones(ev & !bit(i)) >= 2 {
            ev | bit(i)
        } else {
            ev & !bit(i)
        }
    }

    fn acceptor_accept(&self, w: &mut World, i: usize, b: B, c: usize, forced: bool, ev: u8) {
        if !self.takes_part(w, i) {
            return;
        }
        self.try_release(w, i);
        if self.is_restore(c) && !has(w.seats[i].known, c) {
            w.seats[i].known |= bit(c);
        }
        let s = &w.seats[i];
        let kept = s.accepted.is_some_and(|(_, x)| usize::from(x) == c);
        let approver = forced
            && !kept
            && (self.fixes.f3 || self.v2 || (has(s.known, c) && has(s.withdrawn, c) && has(s.intents[c], i)));
        let resolvable = self.resolvable(w, i, c) || kept || (approver && (self.is_restore(c) || self.v2));
        if !resolvable {
            w.seats[i].retry = Some((b, u(c), forced, ev));
            return;
        }
        if self.is_restore(c) {
            w.seats[i].lock_known |= bit(c);
        }
        if !kept && !approver && self.withheld(w, i, c) {
            w.seats[i].retry = Some((b, u(c), forced, ev));
            return;
        }
        // O2: without its own intent a seat completes a forced value only on
        // real signatures on that change: 1 = one seat other than the sender
        // (the first proposal), 2 = two distinct seats, the sender may be one
        let sender = usize::from(b.1);
        let signers = match self.evidence {
            1 => ones(ev & !bit(sender) & !bit(i)),
            _ => ones(ev & !bit(i)),
        };
        let wanted = usize::from(self.evidence.min(1)) + usize::from(self.evidence >= 2);
        if approver && !has(w.seats[i].intents[c], i) && signers < wanted {
            w.seats[i].retry = Some((b, u(c), forced, ev));
            return;
        }
        // the all-report gate binds a recovered seat on every path
        if self.all_report && w.seats[i].recovered && self.need(c) >= self.n && self.withheld_by_reports(w, i, c) {
            w.seats[i].retry = Some((b, u(c), forced, ev));
            return;
        }
        w.seats[i].retry = None;
        let v = self.val(c);
        match self.with_acceptor(w, i, |a| a.accept(&slot(), &ballot(b), &v)).reply {
            Reply::Accepted { .. } => {
                mask_add(&mut w.hist, b, u(c), i);
                w.signed[c] |= bit(i);
                w.seats[i].touched = self.amnesia.is_some();
                self.reply(w, i, usize::from(b.1), P::Accepted(b, u(c)));
                self.count(w, i, i, b, c);
            }
            Reply::Nack { promised, .. } => {
                self.reply(w, i, usize::from(b.1), P::Nack(b, promised.as_ref().map(bal)));
            }
            Reply::Promise { .. } => {}
        }
    }

    fn withheld_by_reports(&self, w: &World, i: usize, c: usize) -> bool {
        let s = &w.seats[i];
        !(0..self.n).filter(|j| *j != i).all(|j| s.reps[j].is_some_and(|r| r.latest == Some(u(c))))
    }

    fn count(&self, w: &mut World, i: usize, from: usize, b: B, c: usize) {
        let skip = if self.v2 {
            has(self.subject_cards(w, i), c) && self.cards[c].restore_of == Some(from)
        } else {
            self.is_restore(c) && has(self.subjects(w, i), from)
        };
        if skip {
            return;
        }
        let set = mask_add(&mut w.seats[i].sigs, b, u(c), from);
        if ones(set) < self.need(c) {
            return;
        }
        if usize::from(b.1) == i {
            self.seal(w, i, b, c);
        } else if w.seats[i].learned.is_none() {
            w.seats[i].learned = Some((b, u(c)));
        }
    }

    fn seal(&self, w: &mut World, i: usize, b: B, c: usize) {
        if w.seats[i].sealed.is_some() {
            return;
        }
        if let Some(z) = self.cards[c].restore_of {
            if z == i || w.seats[i].heard_subject {
                w.seats[i].learned = Some((b, u(c)));
                return;
            }
        }
        let mut by = mask_get(&w.seats[i].sigs, b, u(c));
        if self.is_restore(c) {
            by &= if self.v2 { !self.cards[c].restore_of.map_or(0, bit) } else { !self.subjects(w, i) };
        }
        if ones(by) < self.need(c) {
            return;
        }
        let s = &mut w.seats[i];
        s.sealed = Some(u(c));
        s.head = 2;
        s.learned = None;
        w.sealed_lock |= bit(c);
    }

    /// Without an older-build seat nobody is ever free, so the seen-sets
    /// can only feed an empty free count: not tracked (state space).
    fn note_seen(&self, w: &mut World, i: usize, j: usize, accepting: bool) {
        if self.old.is_some() {
            w.seats[i].locked_seen |= bit(j);
            if accepting {
                w.seats[i].accepted_seen |= bit(j);
            }
        }
    }

    fn note_free(&self, w: &mut World, i: usize, j: usize) {
        if j != i && !has(w.seats[i].free_seen, j) {
            w.seats[i].free_seen |= bit(j);
            self.try_release(w, i);
        }
    }

    fn intent(&self, w: &mut World, i: usize, c: usize) {
        w.seats[i].own |= bit(c);
        if self.is_old(i) {
            self.sign_free(w, i, c);
            return;
        }
        w.seats[i].intents[c] |= bit(i);
        if self.byz != Some(i) {
            w.honest_intent |= bit(c);
        }
        if self.locked_mode(w, i) {
            self.send(w, i, P::Intent(u(c)), false);
            self.try_propose(w, i);
        } else {
            self.sign_free(w, i, c);
        }
    }

    fn report(&self, w: &World, i: usize, to: usize, answer: bool) -> Rep {
        let s = &w.seats[i];
        let state = s.promised.is_some() || s.accepted.is_some() || s.touched;
        Rep {
            head: s.head,
            touched: (s.head + 1).max(if state { 2 } else { 0 }),
            round: s.seen_from[to],
            latest: s.accepted.map(|(_, c)| c),
            answer,
        }
    }

    fn on_report(&self, w: &mut World, i: usize, j: usize, r: Rep) {
        let rule = self.amnesia;
        let s = &mut w.seats[i];
        s.reps[j] = Some(r);
        if !s.am {
            return;
        }
        let floor = match rule {
            Some(Rule::A) => s.head.max(r.head) + 1,
            Some(Rule::B) | Some(Rule::C) => {
                if !r.answer {
                    return;
                }
                s.answered |= bit(j);
                let want = if rule == Some(Rule::B) {
                    self.q().promises()
                } else {
                    self.q().promises().max(self.m)
                };
                if ones(s.answered) < want {
                    return;
                }
                let reps = (0..self.n).filter(|k| has(s.answered, *k)).filter_map(|k| s.reps[k]);
                if rule == Some(Rule::B) {
                    1 + reps.map(|r| r.head).max().unwrap_or(0)
                } else {
                    let all: Vec<Rep> = reps.collect();
                    s.round_floor = all.iter().map(|r| r.round).max().unwrap_or(0);
                    all.iter().map(|r| r.touched).max().unwrap_or(0)
                }
            }
            None | Some(Rule::Import) => return,
        };
        s.am = false;
        s.passive = floor >= 2;
        s.recovered = !s.passive;
    }

    fn deliver(&self, w: &mut World, m: &Msg) {
        let i = usize::from(m.to);
        let j = usize::from(m.from);
        if !w.seats[i].alive {
            return;
        }
        match &m.p {
            P::Ask => {
                // R-c: a seat that lost its own state has nothing to report
                if self.amnesia == Some(Rule::C) && w.seats[i].am {
                    return;
                }
                let r = self.report(w, i, j, true);
                self.send_to(w, i, j, P::Report(r));
                return;
            }
            P::Report(r) => {
                self.on_report(w, i, j, *r);
                return;
            }
            _ => {}
        }
        if w.seats[i].sealed.is_some() {
            return;
        }
        if m.inc > 0 && self.subject_seat(j) {
            w.seats[i].heard_subject = true;
        }
        if self.v2 && matches!(m.p, P::Prepare(_) | P::Accept(..)) && has(self.subjects(w, i), j) {
            return;
        }
        let lock_frame = matches!(m.p, P::Prepare(_) | P::Promise(..) | P::Accept(..) | P::Accepted(..) | P::Nack(..));
        if lock_frame && !self.is_old(i) {
            w.seats[i].touched = self.amnesia.is_some();
            // a seat that does not take part at s still sees the ballot (lock_saw)
            if !self.takes_part(w, i) {
                match &m.p {
                    P::Prepare(r) => self.saw(w, i, (*r, u(j))),
                    P::Accept(r, ..) => self.saw(w, i, (*r, u(j))),
                    P::Promise(b, _) | P::Accepted(b, _) => self.saw(w, i, *b),
                    P::Nack(_, Some(pb)) => self.saw(w, i, *pb),
                    _ => {}
                }
                return;
            }
        }
        match &m.p {
            P::Card(c) => {
                w.seats[i].known |= bit(usize::from(*c));
            }
            P::Withdraw(c) => {
                w.seats[i].withdrawn |= bit(usize::from(*c));
                w.seats[i].own &= !bit(usize::from(*c));
            }
            _ if self.is_old(i) => {}
            P::Intent(c) => {
                w.seats[i].intents[usize::from(*c)] |= bit(j);
                self.try_propose(w, i);
            }
            P::FreeSig(c) => {
                w.seats[i].intents[usize::from(*c)] |= bit(j);
                if has(w.seats[i].known, usize::from(*c)) {
                    self.note_free(w, i, j);
                }
            }
            P::Prepare(r) => {
                let b = (*r, u(j));
                self.note_seen(w, i, j, false);
                self.saw(w, i, b);
                self.try_release(w, i);
                match self.with_acceptor(w, i, |a| a.prepare(&slot(), &ballot(b))).reply {
                    Reply::Promise { accepted, .. } => {
                        let acc = accepted.map(|(ab, v)| (bal(&ab), u(card_of(&v.digest))));
                        self.reply(w, i, j, P::Promise(b, acc));
                    }
                    Reply::Nack { promised, .. } => self.reply(w, i, j, P::Nack(b, promised.as_ref().map(bal))),
                    Reply::Accepted { .. } => {}
                }
            }
            P::Promise(b, acc) => {
                self.note_seen(w, i, j, acc.is_some());
                self.saw(w, i, *b);
                if usize::from(b.1) != i || has(self.subjects(w, i), j) {
                    return;
                }
                let Some(run) = w.seats[i].run.as_mut() else { return };
                if run.ballot != *b {
                    return;
                }
                if let Some((_, c)) = acc {
                    run.ids |= bit(usize::from(*c));
                }
                run.promised |= bit(j);
                Self::log(run, LCall::Promise(u(j), *acc));
                self.excuse(w, i);
                self.try_propose(w, i);
            }
            P::Accept(r, c, f, ev) => {
                let b = (*r, u(j));
                self.note_seen(w, i, j, true);
                self.saw(w, i, b);
                self.acceptor_accept(w, i, b, usize::from(*c), *f, *ev);
            }
            P::Accepted(b, c) => {
                let c = usize::from(*c);
                self.note_seen(w, i, j, true);
                self.saw(w, i, *b);
                if !(has(w.seats[i].lock_known, c) || self.resolvable(w, i, c)) {
                    return;
                }
                if self.is_restore(c) {
                    w.seats[i].lock_known |= bit(c);
                }
                self.count(w, i, j, *b, c);
            }
            P::Nack(b, p) => match p {
                Some(pb) => {
                    self.note_seen(w, i, j, false);
                    self.saw(w, i, *pb);
                }
                None => {
                    if usize::from(b.1) == i {
                        if let Some(run) = w.seats[i].run.as_mut() {
                            Self::log(run, LCall::HeardFree(u(j)));
                        }
                    }
                    self.note_free(w, i, j);
                    self.try_propose(w, i);
                }
            },
            P::Ask | P::Report(_) => {}
        }
    }

    fn update_chosen(&self, w: &mut World) {
        for (_, c, who) in &w.hist {
            let c = usize::from(*c);
            let z = self.cards[c].restore_of.map_or(0, bit);
            if ones(who & !z) >= self.need(c) {
                w.chosen |= bit(c);
            }
        }
    }

    // ---- the transition relation ----

    fn may_start(&self, w: &World, i: usize, live: bool) -> bool {
        let s = &w.seats[i];
        let cap = if live { self.max_ballots + self.live_extra } else { self.max_ballots };
        if !self.takes_part(w, i) || s.ballots >= cap || !self.locked_mode(w, i) || (!live && !has(self.may_lead, i)) {
            return false;
        }
        let subjects = self.subjects(w, i);
        let subject = has(subjects, i);
        if subject && (self.fixes.f2 || self.v2) {
            return false;
        }
        match &s.run {
            Some(run) => self.busy(run),
            None if subject => self.ready(w, i),
            None if !live => true,
            None => {
                let rank0 = (0..self.n).find(|m| w.seats[*m].alive && !has(subjects, *m)) == Some(i);
                rank0 || self.ready(w, i)
            }
        }
    }

    fn apply(&self, w: &World, act: &Act) -> World {
        let mut w = w.clone();
        match act {
            Act::Deliver(m) => {
                msg_take(&mut w, m);
                self.deliver(&mut w, m);
            }
            Act::Dup(m) => {
                w.dups += 1;
                if let Ok(k) = w.msgs.binary_search_by(|e| e.0.cmp(m)) {
                    w.msgs[k].1 += 1;
                }
            }
            Act::Intent(i, c) => self.intent(&mut w, usize::from(*i), usize::from(*c)),
            Act::Start(i) => self.start(&mut w, usize::from(*i)),
            Act::Drive(i) => {
                if let Some(run) = w.seats[usize::from(*i)].run.as_mut() {
                    run.s3_over = true;
                }
                self.try_propose(&mut w, usize::from(*i));
            }
            Act::Retry(i) => {
                let i = usize::from(*i);
                if let Some((b, c, f, ev)) = w.seats[i].retry.take() {
                    self.acceptor_accept(&mut w, i, b, usize::from(c), f, ev);
                }
            }
            Act::Release(i) => {
                let i = usize::from(*i);
                self.try_release(&mut w, i);
                if !self.locked_mode(&w, i) {
                    self.sign_standing(&mut w, i);
                }
            }
            Act::LearnSeal(i) => {
                let i = usize::from(*i);
                if let Some((b, c)) = w.seats[i].learned {
                    self.seal(&mut w, i, b, usize::from(c));
                }
            }
            Act::FreeSeal(i, c) => {
                let s = &mut w.seats[usize::from(*i)];
                s.sealed = Some(*c);
                s.head = 2;
                w.sealed_free |= bit(usize::from(*c));
            }
            Act::Withdraw(c) => {
                let c = usize::from(*c);
                let p = self.cards[c].proposer;
                w.withdrawn |= bit(c);
                w.seats[p].withdrawn |= bit(c);
                w.seats[p].own &= !bit(c);
                self.send(&mut w, p, P::Withdraw(u(c)), false);
            }
            Act::Die(i, lose) => {
                let i = usize::from(*i);
                w.deaths += 1;
                w.seats[i].alive = false;
                w.seats[i].forget_volatile();
                if *lose {
                    w.msgs.retain(|(m, _)| usize::from(m.from) != i);
                }
                if w.revivals >= self.max_revivals {
                    w.msgs.retain(|(m, _)| usize::from(m.to) != i);
                }
            }
            Act::Revive(i) => {
                let i = usize::from(*i);
                w.revivals += 1;
                w.seats[i].alive = true;
                w.seats[i].inc += 1;
                if w.revivals >= self.max_revivals {
                    let dead: u8 = (0..self.n).filter(|s| !w.seats[*s].alive).fold(0, |m, s| m | bit(s));
                    w.msgs.retain(|(m, _)| !has(dead, usize::from(m.to)));
                }
            }
            Act::Propose(c) => {
                let c = usize::from(*c);
                w.born |= bit(c);
                if self.cards[c].late {
                    for s in w.seats.iter_mut().filter(|s| s.alive) {
                        s.known |= bit(c);
                    }
                } else {
                    let p = self.cards[c].proposer;
                    w.seats[p].known |= bit(c);
                    self.send(&mut w, p, P::Card(u(c)), false);
                }
            }
            Act::Resend(m) => msg_add(&mut w, m.clone()),
            Act::CatchUp(i) => w.seats[usize::from(*i)].head = 1,
            Act::Amnesia(i, tail) => {
                let i = usize::from(*i);
                w.amnesias += 1;
                let s = &mut w.seats[i];
                s.promised = None;
                s.accepted = None;
                s.free = false;
                s.forget_volatile();
                if *tail {
                    s.head = 0;
                    s.sealed = None;
                }
                s.am = self.amnesia != Some(Rule::Import);
                s.passive = false;
                s.recovered = false;
                s.round_floor = 0;
                if self.amnesia != Some(Rule::Import) {
                    self.send(&mut w, i, P::Ask, false);
                }
            }
            Act::Byz(m) => {
                w.byz_acts += 1;
                let b = usize::from(m.from);
                match &m.p {
                    P::Accepted(bb, c) => {
                        mask_add(&mut w.hist, *bb, *c, b);
                        w.signed[usize::from(*c)] |= bit(b);
                    }
                    P::Promise(_, Some((_, c))) => w.signed[usize::from(*c)] |= bit(b),
                    _ => {}
                }
                msg_add(&mut w, m.clone());
            }
            Act::OperatorApprove(i, c) => {
                let (i, c) = (usize::from(*i), usize::from(*c));
                w.seats[i].own |= bit(c);
                w.seats[i].intents[c] |= bit(i);
                w.honest_intent |= bit(c);
                self.send(&mut w, i, P::Intent(u(c)), false);
                self.try_propose(&mut w, i);
            }
            Act::Announce(i) => {
                let i = usize::from(*i);
                w.announces += 1;
                for to in (0..self.n).filter(|t| *t != i) {
                    let r = self.report(&w, i, to, false);
                    self.send_to(&mut w, i, to, P::Report(r));
                }
            }
        }
        self.update_chosen(&mut w);
        w
    }

    fn actions(&self, w: &World, live: bool) -> Vec<Act> {
        let mut out = Vec::new();
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() || self.byz == Some(i) {
                continue;
            }
            let part = self.takes_part(w, i);
            if part && s.learned.is_some() && self.locked_mode(w, i) {
                out.push(Act::LearnSeal(u(i)));
            }
            if self.is_old(i) || (part && !self.locked_mode(w, i)) {
                for (c, f) in w.free_sigs.iter().enumerate() {
                    let z = self.cards.get(c).and_then(|x| x.restore_of).map_or(0, bit);
                    let need = if z != 0 { self.m.saturating_sub(1).max(1) } else { self.m };
                    if ones(f & !z) >= need && has(s.known, c) {
                        out.push(Act::FreeSeal(u(i), u(c)));
                    }
                }
            }
            if part
                && s.run.as_ref().is_some_and(|r| {
                    let l = self.leader(r);
                    l.ready() && l.proposed().is_none()
                })
            {
                out.push(Act::Drive(u(i)));
            }
        }
        for (m, _) in &w.msgs {
            out.push(Act::Deliver(m.clone()));
        }
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() || self.byz == Some(i) {
                continue;
            }
            if s.head == 0 {
                out.push(Act::CatchUp(u(i)));
            }
            if s.retry.is_some() && self.takes_part(w, i) && self.locked_mode(w, i) {
                out.push(Act::Retry(u(i)));
            }
            if self.old.is_some() && self.takes_part(w, i) {
                out.push(Act::Release(u(i)));
            }
            for c in 0..self.cards.len() {
                if has(w.born, c)
                    && has(s.known, c)
                    && !has(s.withdrawn, c)
                    && !has(s.own, c)
                    && self.cards[c].restore_of != Some(i)
                    && (live || has(self.cards[c].approvers, i))
                {
                    out.push(Act::Intent(u(i), u(c)));
                }
            }
        }
        for (c, card) in self.cards.iter().enumerate() {
            if has(w.born, c) {
                continue;
            }
            match card.restore_of {
                Some(z) if !w.seats[z].alive && w.seats[card.proposer].alive => out.push(Act::Propose(u(c))),
                None if card.late && live => out.push(Act::Propose(u(c))),
                _ => {}
            }
        }
        if live {
            self.resends(w, &mut out);
        }
        if self.operator && live {
            out.extend(self.operator_acts(w));
        }
        if let (Some(b), false) = (self.byz, live) {
            if w.byz_acts < self.max_byz {
                self.byz_acts(w, b, &mut out);
            }
        }
        for i in 0..self.n {
            if self.byz != Some(i) && self.may_start(w, i, live) {
                out.push(Act::Start(u(i)));
            }
        }
        if !live {
            for (m, k) in &w.msgs {
                if w.dups < self.max_dups && *k == 1 {
                    out.push(Act::Dup(m.clone()));
                }
            }
            for (c, card) in self.cards.iter().enumerate() {
                if card.withdrawable && has(w.born, c) && !has(w.withdrawn, c) && w.seats[card.proposer].alive {
                    out.push(Act::Withdraw(u(c)));
                }
            }
            for i in 0..self.n {
                let s = &w.seats[i];
                if s.alive && w.deaths < self.max_deaths && has(self.may_die, i) && self.byz != Some(i) {
                    out.push(Act::Die(u(i), false));
                    if self.drops && w.msgs.iter().any(|(m, _)| usize::from(m.from) == i) {
                        out.push(Act::Die(u(i), true));
                    }
                }
                if !s.alive && w.revivals < self.max_revivals {
                    out.push(Act::Revive(u(i)));
                }
                if self.amnesia.is_some() && s.alive && !self.is_old(i) && w.amnesias < self.max_amnesia {
                    if s.sealed.is_none() {
                        out.push(Act::Amnesia(u(i), false));
                    }
                    out.push(Act::Amnesia(u(i), true));
                }
                if self.amnesia == Some(Rule::A) && s.alive && w.announces < self.max_announce {
                    out.push(Act::Announce(u(i)));
                }
            }
        }
        out
    }

    /// A seat's operator may approve a card withdrawn there that its
    /// seat holds no intent for, while a forced value waits on it.
    fn operator_acts(&self, w: &World) -> Vec<Act> {
        let mut out = Vec::new();
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() || self.byz == Some(i) {
                continue;
            }
            for c in 0..self.cards.len() {
                if has(s.withdrawn, c) && !has(s.intents[c], i) && s.retry.is_some_and(|r| usize::from(r.1) == c) {
                    out.push(Act::OperatorApprove(u(i), u(c)));
                }
            }
        }
        out
    }

    fn byz_acts(&self, w: &World, b: usize, out: &mut Vec<Act>) {
        let msg = |to: usize, p: P| Act::Byz(Msg { to: u(to), from: u(b), inc: 0, p });
        for t in (0..self.n).filter(|t| *t != b && w.seats[*t].alive && w.seats[*t].sealed.is_none()) {
            for r in 1..=3u8 {
                out.push(msg(t, P::Prepare(r)));
                for c in 0..self.cards.len() {
                    if self.cards[c].late {
                        continue;
                    }
                    let evs = [0, w.signed[c] | bit(b)];
                    for ev in evs.iter().take(if evs[1] == 0 { 1 } else { 2 }) {
                        out.push(msg(t, P::Accept(r, u(c), true, *ev)));
                    }
                    out.push(msg(t, P::Accepted((r, u(b)), u(c))));
                }
            }
            if let Some(run) = &w.seats[t].run {
                out.push(msg(t, P::Promise(run.ballot, None)));
                for c in (0..self.cards.len()).filter(|c| !self.cards[*c].late) {
                    out.push(msg(t, P::Promise(run.ballot, Some(((1, u(b)), u(c))))));
                    out.push(msg(t, P::Accepted(run.ballot, u(c))));
                }
            }
        }
    }

    /// Fault-free continuation only: the leader's retransmit (§7, S4),
    /// the re-serve of open cards, withdrawals and own intents, and the
    /// amnesiac's repeated ask.
    fn resends(&self, w: &World, out: &mut Vec<Act>) {
        let inflight = |m: &Msg| w.msgs.binary_search_by(|e| e.0.cmp(m)).is_ok();
        for (i, s) in w.seats.iter().enumerate() {
            if !s.alive || s.sealed.is_some() || self.byz == Some(i) {
                continue;
            }
            let mut push = |to: usize, p: P| {
                let m = Msg { to: u(to), from: u(i), inc: s.inc, p };
                if to != i && w.seats[to].alive && w.seats[to].sealed.is_none() && !inflight(&m) {
                    out.push(Act::Resend(m));
                }
            };
            if let Some(run) = s.run.as_ref().filter(|r| self.takes_part(w, i) && self.busy(r)) {
                let l = self.leader(run);
                let p = match (l.proposed(), run.value) {
                    (Some(_), Some(c)) => {
                        let forced = l.forced().is_some();
                        let ev = if forced { self.evidence_of(w, i, run, usize::from(c)) } else { 0 };
                        Some(P::Accept(run.ballot.0, c, forced, ev))
                    }
                    (None, _) if !l.ready() => Some(P::Prepare(run.ballot.0)),
                    _ => None,
                };
                if let Some(p) = p {
                    for to in (0..self.n).filter(|t| !self.is_old(*t)) {
                        push(to, p.clone());
                    }
                }
            }
            for c in 0..self.cards.len() {
                if !has(w.born, c) || !has(s.known, c) {
                    continue;
                }
                for to in 0..self.n {
                    if has(s.withdrawn, c) {
                        if !has(w.seats[to].withdrawn, c) {
                            push(to, P::Withdraw(u(c)));
                        }
                    } else if !has(w.seats[to].known, c) {
                        push(to, P::Card(u(c)));
                    }
                }
            }
            for c in (0..self.cards.len()).filter(|c| has(s.own, *c)) {
                let free = has(w.free_sigs[c], i);
                for to in 0..self.n {
                    if !has(w.seats[to].intents[c], i) || (free && !has(w.seats[to].free_seen, i)) {
                        push(to, if free { P::FreeSig(u(c)) } else { P::Intent(u(c)) });
                    }
                }
            }
            if s.am {
                for to in (0..self.n).filter(|t| !has(s.answered, *t) && s.reps[*t].is_none()) {
                    push(to, P::Ask);
                }
            }
        }
    }

    fn init(&self) -> World {
        let mut w = World {
            seats: (0..self.n).map(|i| Seat::new(true, if has(self.behind, i) { 0 } else { 1 })).collect(),
            msgs: Vec::new(),
            born: 0,
            withdrawn: 0,
            hist: Vec::new(),
            free_sigs: [0; 4],
            chosen: 0,
            sealed_lock: 0,
            sealed_free: 0,
            deaths: 0,
            revivals: 0,
            dups: 0,
            amnesias: 0,
            announces: 0,
            signed: [0; 4],
            honest_intent: 0,
            byz_acts: 0,
        };
        for card in &self.cards {
            if let (Some(z), true) = (card.restore_of, self.lost) {
                w.seats[z].alive = false;
            }
        }
        for (c, card) in self.cards.iter().enumerate() {
            if (card.restore_of.is_none() && !card.late) || (card.restore_of.is_some() && self.lost) {
                w.born |= bit(c);
                w.seats[card.proposer].known |= bit(c);
                self.send(&mut w, card.proposer, P::Card(u(c)), false);
            }
        }
        w
    }

    fn violation(&self, w: &World) -> Option<&'static str> {
        let lock = w.chosen | w.sealed_lock;
        if self.byz.is_some() {
            // with a Byzantine signer only the honest-approval property holds
            let b = self.byz.map_or(0, bit);
            let honest_signed = (0..self.cards.len()).filter(|c| w.signed[*c] & !b != 0).fold(0, |m, c| m | bit(c));
            return (honest_signed & !w.honest_intent != 0).then_some("unapproved");
        }
        if ones(lock) >= 2 {
            Some("lock-lock")
        } else if lock != 0 && w.sealed_free & !lock != 0 {
            Some("lock-free")
        } else if ones(w.sealed_free) >= 2 {
            Some("free-free")
        } else {
            None
        }
    }

    fn sealed_any(w: &World) -> bool {
        w.sealed_lock | w.sealed_free != 0
    }

    /// Liveness is asked only where enough seats live to decide anything.
    /// Survivors: alive seats other than a lost seat being restored (its
    /// old device, if back, is no survivor).
    fn liveness_applies(&self, w: &World) -> bool {
        let lost: u8 = (0..self.cards.len())
            .filter(|c| has(w.born, *c))
            .filter_map(|c| self.cards[c].restore_of)
            .fold(0, |m, z| m | bit(z));
        let survivors = (0..self.n).filter(|i| w.seats[*i].alive && !has(lost, *i) && self.byz != Some(*i)).count();
        let need = (0..self.cards.len()).filter(|c| !self.cards[*c].late).map(|c| self.need(c)).min().unwrap_or(self.m);
        !Self::sealed_any(w) && survivors >= self.q().promises().max(need)
    }

    /// A halt nothing chose while a lost seat's old device runs: B3 holds
    /// its restore by design.
    fn b3_hold(&self, w: &World) -> bool {
        w.chosen == 0
            && self.cards.iter().enumerate().any(|(c, k)| {
                has(w.born, c) && k.restore_of.is_some_and(|z| w.seats[z].alive && w.seats[z].inc > 0)
            })
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
    /// No canonical fair continuation seals and the whole closure is
    /// beyond `live_budget`: a halt under fair schedules, unproven.
    live_unknown: usize,
    fair_halt_trace: Option<Vec<String>>,
    /// Halts (either kind) where an operator approval is enabled.
    operator_resolvable: usize,
    b3_holds: usize,
    b3_trace: Option<Vec<String>>,
    /// Every trace up to this many steps was explored.
    depth: usize,
}

struct Live {
    live: HashSet<u128>,
    dead: HashSet<u128>,
}

impl Config {
    /// Canonical fair continuations, one per first leader: every seat casts
    /// every intent it may, the fresh card appears, every frame is
    /// delivered, re-serves once per round, then the next seat in turn
    /// opens a ballot. A seal on any = live (the states passed are live).
    fn fair_seal(&self, memo: &mut Live, w0: &World) -> bool {
        let quiet = |a: &Act| {
            matches!(
                a,
                Act::Deliver(_)
                    | Act::Intent(..)
                    | Act::Propose(_)
                    | Act::Drive(_)
                    | Act::Retry(_)
                    | Act::LearnSeal(_)
                    | Act::FreeSeal(..)
                    | Act::Release(_)
                    | Act::CatchUp(_)
            )
        };
        let rounds = self.n * (usize::from(self.live_extra) + 1);
        for (first, intents_first) in (0..self.n).flat_map(|f| [(f, true), (f, false)]) {
            let mut w = w0.clone();
            let mut path = vec![hash_world(&w)];
            for round in 0..rounds {
                for act in self.actions(&w, true).into_iter().filter(|a| matches!(a, Act::Resend(_))) {
                    w = self.apply(&w, &act);
                }
                for _ in 0..400 {
                    let acts = self.actions(&w, true);
                    let pick = if intents_first {
                        acts.iter().find(|a| matches!(a, Act::Intent(..))).or_else(|| acts.iter().find(|a| quiet(a)))
                    } else {
                        acts.iter().find(|a| quiet(a))
                    };
                    let Some(act) = pick.cloned() else { break };
                    let next = self.apply(&w, &act);
                    if hash_world(&next) == *path.last().unwrap_or(&0) {
                        break;
                    }
                    w = next;
                    path.push(hash_world(&w));
                    if Self::sealed_any(&w) || memo.live.contains(path.last().unwrap_or(&0)) {
                        memo.live.extend(path);
                        return true;
                    }
                }
                let leader = (0..self.n).map(|k| (first + round + k) % self.n).find(|i| self.may_start(&w, *i, true));
                let Some(leader) = leader else { break };
                w = self.apply(&w, &Act::Start(u(leader)));
                path.push(hash_world(&w));
                if Self::sealed_any(&w) {
                    memo.live.extend(path);
                    return true;
                }
            }
        }
        false
    }

    /// Whether some fault-free continuation of `w` seals. `None` = budget.
    fn can_seal(&self, memo: &mut Live, w: &World) -> Option<bool> {
        let h0 = hash_world(w);
        if Self::sealed_any(w) || memo.live.contains(&h0) {
            return Some(true);
        }
        if memo.dead.contains(&h0) {
            return Some(false);
        }
        let mut visited: HashSet<u128> = HashSet::from([h0]);
        let mut first = self.actions(w, true);
        first.reverse();
        let mut stack: Vec<(u128, World, Vec<Act>, Option<Act>)> = vec![(h0, w.clone(), first, None)];
        while let Some((_, top, acts, _)) = stack.last_mut() {
            let Some(act) = acts.pop() else {
                stack.pop();
                continue;
            };
            let next = self.apply(top, &act);
            let h = hash_world(&next);
            if Self::sealed_any(&next) || memo.live.contains(&h) {
                if std::env::var_os("LOCK_LIVE_PATH").is_some() {
                    for (_, _, _, via) in &stack {
                        if let Some(a) = via {
                            eprintln!("  live: {}", self.show(a));
                        }
                    }
                    eprintln!("  live: {} (sealed {})", self.show(&act), Self::sealed_any(&next));
                }
                memo.live.extend(stack.iter().map(|(h, _, _, _)| *h));
                memo.live.insert(h);
                return Some(true);
            }
            if memo.dead.contains(&h) || !visited.insert(h) {
                continue;
            }
            if visited.len() > self.live_budget {
                if std::env::var_os("LOCK_LIVE_DEBUG").is_some() {
                    eprintln!("can_seal: budget hit, stack depth {}", stack.len());
                }
                return None;
            }
            let mut acts = self.actions(&next, true);
            // popped from the end: progress first
            acts.reverse();
            stack.push((h, next, acts, Some(act)));
        }
        if std::env::var_os("LOCK_LIVE_DEBUG").is_some() {
            eprintln!("can_seal: closure of {} states, no seal", visited.len());
        }
        memo.dead.extend(visited);
        Some(false)
    }

    fn explore(&self, liveness: bool) -> Report {
        self.explore_after(&[], liveness)
    }

    /// The world after `prefix`, each step named as `show` prints it.
    fn after(&self, prefix: &[&str]) -> World {
        let mut w = self.init();
        for step in prefix {
            let act = self
                .actions(&w, false)
                .into_iter()
                .find(|a| self.show(a) == *step)
                .unwrap_or_else(|| panic!("step not enabled: {step}"));
            w = self.apply(&w, &act);
        }
        w
    }

    /// Exhaustive from the end of a scenario prefix.
    fn explore_after(&self, prefix: &[&str], liveness: bool) -> Report {
        let mut r = Report { complete: true, ..Report::default() };
        let init = self.after(prefix);
        let h0 = hash_world(&init);
        let mut parent: HashMap<u128, (u128, Act)> = HashMap::new();
        let mut seen: HashSet<u128> = HashSet::from([h0]);
        let mut queue: VecDeque<(u128, World, usize)> = VecDeque::from([(h0, init, 0)]);
        let mut cut_at = usize::MAX;
        let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
        let trace = |parent: &HashMap<u128, (u128, Act)>, mut h: u128| {
            let mut steps: Vec<String> = Vec::new();
            while let Some((p, a)) = parent.get(&h) {
                steps.push(self.show(a));
                h = *p;
            }
            steps.extend(prefix.iter().rev().map(|p| format!("{p} [prefix]")));
            steps.reverse();
            steps
        };
        let progress = std::env::var_os("LOCK_EXPLORE_PROGRESS").is_some();
        while let Some((h, w, d)) = queue.pop_front() {
            r.depth = r.depth.max(d).min(cut_at);
            r.states += 1;
            if progress && r.states % 100_000 == 0 {
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
            // the first halts in BFS order carry the shortest trace; past a
            // few hundred the count is a lower bound and checking stops
            let halts_seen = r.halts + r.live_unknown + r.b3_holds;
            if liveness && halts_seen < 200 && self.liveness_applies(&w) {
                if !self.fair_seal(&mut memo, &w) {
                    let resolvable = !self.operator_acts(&w).is_empty();
                    let verdict = self.can_seal(&mut memo, &w);
                    if verdict != Some(true) && resolvable {
                        r.operator_resolvable += 1;
                    }
                    if verdict != Some(true) && self.b3_hold(&w) {
                        r.b3_holds += 1;
                        if r.b3_trace.is_none() {
                            r.b3_trace = Some(trace(&parent, h));
                        }
                    }
                    match if self.b3_hold(&w) { Some(true) } else { verdict } {
                        Some(true) => {}
                        Some(false) => {
                            r.halts += 1;
                            if r.halt_trace.is_none() {
                                r.halt_trace = Some(trace(&parent, h));
                            }
                        }
                        None => {
                            r.live_unknown += 1;
                            if r.fair_halt_trace.is_none() {
                                r.fair_halt_trace = Some(trace(&parent, h));
                            }
                        }
                    }
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
                if msg_count(&next) > self.max_msgs {
                    r.pruned += 1;
                    continue;
                }
                if seen.len() > self.max_states {
                    r.complete = false;
                    cut_at = cut_at.min(d);
                    continue;
                }
                parent.insert(hn, (h, act));
                queue.push_back((hn, next, d + 1));
            }
        }
        r
    }
}

impl Config {
    /// Bug hunt beyond the BFS depth: randomized depth-first walks (not
    /// exhaustive). Returns the first violation trace per kind.
    fn hunt(&self, seed: u64, walks: usize, depth: usize, live: bool) -> BTreeMap<&'static str, Vec<String>> {
        let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
        let mut rng = seed | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut found = BTreeMap::new();
        for _ in 0..walks {
            let mut w = self.init();
            let mut steps = Vec::new();
            for _ in 0..depth {
                let acts = self.actions(&w, false);
                if acts.is_empty() {
                    break;
                }
                let k = usize::try_from(next() % u64::try_from(acts.len()).expect("len")).expect("index");
                let nw = self.apply(&w, &acts[k]);
                if msg_count(&nw) > self.max_msgs {
                    continue;
                }
                steps.push(self.show(&acts[k]));
                w = nw;
                let kind = self.violation(&w).or_else(|| {
                    (live && self.liveness_applies(&w) && self.can_seal(&mut memo, &w) == Some(false)).then_some("halt")
                });
                if let Some(kind) = kind {
                    let shorter = found.get(kind).map_or(true, |t: &Vec<String>| steps.len() < t.len());
                    if shorter {
                        found.insert(kind, steps.clone());
                    }
                    break;
                }
            }
        }
        found
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
        behind: 0,
        max_ballots: 1,
        live_extra: 2,
        max_deaths: 1,
        max_revivals: 0,
        max_dups: 0,
        drops: false,
        amnesia: None,
        max_amnesia: 0,
        max_announce: 0,
        all_report: false,
        max_msgs: 12,
        max_states: 400_000,
        replies_to_leader: false,
        may_lead: 0xff,
        may_die: 0xff,
        live_budget: 200_000,
        fixes: Fixes::default(),
        v2: false,
        evidence: 0,
        byz: None,
        max_byz: 0,
        operator: false,
    }
}

fn card(proposer: usize, withdrawable: bool) -> Card {
    Card { proposer, restore_of: None, withdrawable, late: false, approvers: 0xff }
}

fn late() -> Card {
    Card { proposer: 0, restore_of: None, withdrawable: false, late: true, approvers: 0xff }
}

fn restore(proposer: usize, subject: usize) -> Card {
    Card { proposer, restore_of: Some(subject), withdrawable: false, late: false, approvers: 0xff }
}

/// A withdrawable card and one seat that may die (H1's shape).
fn shape_withdraw(n: usize, m: usize) -> Config {
    base(n, m, vec![card(1, true), late()])
}

/// A lost last seat whose old device may come back, its restore and an
/// ordinary card (H2's shape).
fn shape_restore(n: usize, m: usize) -> Config {
    let mut c = base(n, m, vec![restore(0, n - 1), card(1, false), late()]);
    c.lost = true;
    c.max_revivals = 1;
    c.drops = true;
    c
}

/// A restore its proposer may withdraw: the subject's registration then
/// lives only in what the lock holds (R2).
fn shape_restore_withdraw(n: usize, m: usize) -> Config {
    let mut c = shape_restore(n, m);
    c.cards[0].withdrawable = true;
    c
}

/// An older-build seat (free signatures only) beside the lock (§14).
fn shape_old(n: usize, m: usize) -> Config {
    let mut c = base(n, m, vec![card(0, false), card(n - 1, false), late()]);
    c.old = Some(n - 1);
    c.max_deaths = 0;
    c
}

/// Two cards, the last seat one block behind, one amnesia (§6).
fn shape_amnesia(n: usize, m: usize, rule: Rule) -> Config {
    let mut c = base(n, m, vec![card(0, false), card(1, false), late()]);
    c.behind = bit(n - 1);
    c.max_deaths = 0;
    c.amnesia = Some(rule);
    c.max_amnesia = 1;
    c.max_announce = 1;
    c
}

fn shape(name: &str, n: usize, m: usize) -> Config {
    if let Some(rest) = name.strip_suffix("_x2") {
        let mut c = shape(rest, n, m);
        c.max_amnesia = 2;
        return c;
    }
    match name {
        "withdraw" => shape_withdraw(n, m),
        "withdraw_nolate" => {
            let mut c = shape_withdraw(n, m);
            c.cards.truncate(1);
            c
        }
        "restore" => shape_restore(n, m),
        "restore_withdraw" => shape_restore_withdraw(n, m),
        "old" => shape_old(n, m),
        "amnesia_a" => shape_amnesia(n, m, Rule::A),
        "amnesia_b" => shape_amnesia(n, m, Rule::B),
        "amnesia_c" => shape_amnesia(n, m, Rule::C),
        "amnesia_import" => shape_amnesia(n, m, Rule::Import),
        // O2: the last seat is Byzantine and proposes F, which no honest seat approves
        "byz" => {
            let mut f = card(n - 1, false);
            f.approvers = 0;
            let mut c = base(n, m, vec![card(0, false), f]);
            c.byz = Some(n - 1);
            c.max_byz = 3;
            c.max_deaths = 0;
            c.v2 = true;
            c
        }
        "amnesia_c_all" => {
            let mut c = shape_amnesia(n, m, Rule::C);
            c.all_report = true;
            c
        }
        other => panic!("unknown shape {other}"),
    }
}

fn summary(cfg: &Config, r: &Report) -> String {
    let mut s = format!(
        "n={} m={} fixes={:?} v2={} evidence={}: states={} edges={} pruned={} complete={} depth={} halts={} fair_halts={} operator_resolvable={} b3_holds={}",
        cfg.n,
        cfg.m,
        cfg.fixes,
        cfg.v2,
        cfg.evidence,
        r.states,
        r.edges,
        r.pruned,
        r.complete,
        r.depth,
        r.halts,
        r.live_unknown,
        r.operator_resolvable,
        r.b3_holds
    );
    for (k, (count, t)) in &r.violations {
        let _ = write!(s, "\n  VIOLATION {k} x{count}:");
        for (i, step) in t.iter().enumerate() {
            let _ = write!(s, "\n    {:2}. {step}", i + 1);
        }
    }
    if let Some(t) = &r.b3_trace {
        let _ = write!(s, "\n  B3 HOLD (nothing chosen, a lost seat's old device runs):");
        for (i, step) in t.iter().enumerate() {
            let _ = write!(s, "\n    {:2}. {step}", i + 1);
        }
    }
    if let (None, Some(t)) = (&r.halt_trace, &r.fair_halt_trace) {
        let _ = write!(s, "\n  HALT under the canonical fair schedules (closure beyond the budget):");
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

fn env_u(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.parse().ok()
}

fn bounds_from_env(cfg: &mut Config) {
    let u8_of = |k: &str| env_u(k).map(|v| u8::try_from(v).expect("small bound"));
    if let Some(v) = u8_of("LOCK_BALLOTS") {
        cfg.max_ballots = v;
    }
    if let Some(v) = u8_of("LOCK_LIVE_EXTRA") {
        cfg.live_extra = v;
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
    if let Some(v) = u8_of("LOCK_BEHIND") {
        cfg.behind = v;
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
    if let Some(v) = env_u("LOCK_LIVE_BUDGET") {
        cfg.live_budget = v;
    }
    if let Some(v) = env_u("LOCK_V2") {
        cfg.v2 = v != 0;
    }
    if let Some(v) = u8_of("LOCK_EVIDENCE") {
        cfg.evidence = v;
    }
    if let Some(v) = u8_of("LOCK_MAX_BYZ") {
        cfg.max_byz = v;
    }
    if let Some(v) = env_u("LOCK_OPERATOR") {
        cfg.operator = v != 0;
    }
    if let Ok(v) = std::env::var("LOCK_APPROVERS") {
        for (card, mask) in cfg.cards.iter_mut().zip(v.split(',')) {
            card.approvers = mask.parse().expect("approver mask");
        }
    }
    if let Some(v) = u8_of("LOCK_MAY_LEAD") {
        cfg.may_lead = v;
    }
    if let Some(v) = u8_of("LOCK_MAY_DIE") {
        cfg.may_die = v;
    }
    if let Some(v) = env_u("LOCK_REPLIES_TO_LEADER") {
        cfg.replies_to_leader = v != 0;
    }
}

fn fixes_of(bits: &str) -> Fixes {
    let b: Vec<bool> = bits.chars().map(|c| c == '1').collect();
    Fixes { f1: b[0], f2: b[1], f3: b[2], f4: b[3] }
}

const H1_TRACE: [&str; 13] = [
    "deliver B->A Card(X)",
    "deliver B->C Card(X)",
    "B intent X",
    "C intent X",
    "deliver B->A Intent(X)",
    "deliver C->A Intent(X)",
    "A opens a ballot",
    "deliver A->B Prepare(r1)",
    "deliver B->A Promise(1:A)",
    "deliver A->B Accept(r1, X)",
    "B WITHDRAWS X",
    "deliver B->A Withdraw(X)",
    "C DIES",
];

const H2_PREFIX: [&str; 10] = [
    "C REVIVES (old device)",
    "A intent R(C)",
    "A opens a ballot",
    "deliver A->B Prepare(r1)",
    "deliver B->A Promise(1:A)",
    "deliver B->C Card(Y)",
    "B intent Y",
    "deliver B->C Intent(Y)",
    "C intent Y",
    "A DIES, its frames in flight lost",
];

/// H1: every fault-free continuation of the trace stays unsealed under the
/// pinned rules (the whole closure, no fresh card); master completes it.
#[test]
fn a_forced_withdrawn_value_halts_on_the_pinned_rules_and_seals_on_master() {
    let mut cfg = shape("withdraw_nolate", 3, 2);
    let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
    cfg.live_budget = 200_000;
    assert_eq!(cfg.can_seal(&mut memo, &cfg.after(&H1_TRACE)), Some(false), "H1 not found");
    cfg.v2 = true;
    let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
    assert_eq!(cfg.can_seal(&mut memo, &cfg.after(&H1_TRACE)), Some(true));
}

/// H2: from the prefix the pinned rules fork within four steps; master
/// does not within the same bound.
#[test]
fn a_revived_subject_forks_on_the_pinned_rules_and_not_on_master() {
    let mut cfg = shape_restore(3, 2);
    cfg.max_states = 30_000;
    let r = cfg.explore_after(&H2_PREFIX, false);
    assert!(r.violations.contains_key("lock-lock"), "H2 not found");
    cfg.v2 = true;
    let r = cfg.explore_after(&H2_PREFIX, false);
    assert!(r.violations.is_empty(), "{}", summary(&cfg, &r));
}

/// (L) A deviating seat that reports a made-up value keeps it forced
/// exactly while `1 + unheard >= m`: the real `Leader::forced`.
#[test]
fn a_made_up_holding_is_forced_only_while_m_minus_one_others_are_unheard() {
    for n in 2..=5usize {
        for m in 2..=n {
            for unheard in 0..n {
                let mut l = Leader::new(slot(), Quorum { n, m }, ballot((5, 0)));
                // seats 0..n-1-unheard honest and heard, the last one deviates
                for j in 0..n - 1 - unheard {
                    l.promise(&name(j), &ballot((5, 0)), None);
                }
                l.promise(&name(n - 1), &ballot((5, 0)), Some((ballot((4, u(n - 1))), Value { digest: digest(1), need: m })));
                assert_eq!(l.forced().is_some(), 1 + unheard >= m, "n={n} m={m} unheard={unheard}");
            }
        }
    }
}

/// `LOCK_EXPLORE="withdraw:3:2:0000 restore:3:2:1111"` (fixes = F1..F4
/// bits); bounds from `LOCK_BALLOTS`, `LOCK_DEATHS`, ... (`bounds_from_env`).
#[test]
#[ignore = "runner: LOCK_EXPLORE=... cargo test --release -p molt-engine --lib explore_runner -- --ignored --nocapture"]
fn explore_runner() {
    let spec = std::env::var("LOCK_EXPLORE").unwrap_or_default();
    for item in spec.split_whitespace() {
        let parts: Vec<&str> = item.split(':').collect();
        let (n, m) = (parts[1].parse().expect("n"), parts[2].parse().expect("m"));
        let mut cfg = shape(parts[0], n, m);
        cfg.fixes = fixes_of(parts.get(3).unwrap_or(&"0000"));
        bounds_from_env(&mut cfg);
        let live = env_u("LOCK_LIVE").map_or(true, |v| v != 0);
        let t = std::time::Instant::now();
        let r = cfg.explore(live);
        eprintln!("[{item}] {:.1?}\n{}", t.elapsed(), summary(&cfg, &r));
    }
}

/// One line per (shape, n, m, fix set): `L` lock-lock, `F` lock-free,
/// `f` free-free violations, `H` halts, `!` incomplete (a bound hit).
/// `LOCK_MATRIX="withdraw restore" LOCK_SIZES="3:2 4:3" LOCK_SETS="none all"`.
#[test]
#[ignore = "runner: LOCK_MATRIX=... cargo test --release -p molt-engine --lib explore_matrix -- --ignored --nocapture"]
fn explore_matrix() {
    let shapes = std::env::var("LOCK_MATRIX").unwrap_or_default();
    let sizes = std::env::var("LOCK_SIZES").unwrap_or_else(|_| "2:2 3:2 3:3 4:2 4:3".into());
    let wanted = std::env::var("LOCK_SETS").unwrap_or_else(|_| "none F1 F2 F3 F4 all".into());
    let sets = [
        ("none", Fixes::default()),
        ("F1", Fixes { f1: true, ..Fixes::default() }),
        ("F2", Fixes { f2: true, ..Fixes::default() }),
        ("F3", Fixes { f3: true, ..Fixes::default() }),
        ("F4", Fixes { f4: true, ..Fixes::default() }),
        ("all", Fixes::ALL),
    ];
    let traces = env_u("LOCK_TRACES").is_some_and(|v| v != 0);
    for sh in shapes.split_whitespace() {
        for size in sizes.split_whitespace() {
            let (n, m) = size.split_once(':').expect("n:m");
            let (n, m): (usize, usize) = (n.parse().expect("n"), m.parse().expect("m"));
            for (label, fixes) in sets.iter().filter(|(l, _)| wanted.split_whitespace().any(|x| x == *l)) {
                let mut cfg = shape(sh, n, m);
                cfg.fixes = *fixes;
                bounds_from_env(&mut cfg);
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
                if traces || (*label == "all" && marks.chars().any(|c| c != '!')) {
                    eprintln!("{}", summary(&cfg, &r));
                }
            }
        }
    }
}

/// `LOCK_HUNT="restore:3:2:0000" LOCK_WALKS=1000000 LOCK_DEPTH=40`.
#[test]
#[ignore = "runner: LOCK_HUNT=... cargo test --release -p molt-engine --lib explore_hunt -- --ignored --nocapture"]
fn explore_hunt() {
    let spec = std::env::var("LOCK_HUNT").unwrap_or_default();
    let walks = env_u("LOCK_WALKS").unwrap_or(100_000);
    let depth = env_u("LOCK_DEPTH").unwrap_or(40);
    for item in spec.split_whitespace() {
        let parts: Vec<&str> = item.split(':').collect();
        let (n, m) = (parts[1].parse().expect("n"), parts[2].parse().expect("m"));
        let mut cfg = shape(parts[0], n, m);
        cfg.fixes = fixes_of(parts.get(3).unwrap_or(&"0000"));
        bounds_from_env(&mut cfg);
        let seed = u64::try_from(env_u("LOCK_SEED").unwrap_or(7)).expect("seed");
        let t = std::time::Instant::now();
        let live = env_u("LOCK_LIVE").is_some_and(|v| v != 0);
        let found = cfg.hunt(seed, walks, depth, live);
        eprintln!("[hunt {item}] {:.1?} walks={walks} depth={depth}", t.elapsed());
        for (k, steps) in found {
            eprintln!("  VIOLATION {k} ({} steps):", steps.len());
            for (i, st) in steps.iter().enumerate() {
                eprintln!("    {:2}. {st}", i + 1);
            }
        }
    }
}

/// `LOCK_PROBE="withdraw:3:2:0000"`: the fault-free continuation from the start.
#[test]
#[ignore = "runner: LOCK_PROBE=... cargo test --release -p molt-engine --lib explore_probe -- --ignored --nocapture"]
fn explore_probe() {
    let spec = std::env::var("LOCK_PROBE").unwrap_or_default();
    for item in spec.split_whitespace() {
        let parts: Vec<&str> = item.split(':').collect();
        let (n, m) = (parts[1].parse().expect("n"), parts[2].parse().expect("m"));
        let mut cfg = shape(parts[0], n, m);
        cfg.fixes = fixes_of(parts.get(3).unwrap_or(&"0000"));
        bounds_from_env(&mut cfg);
        let mut memo = Live { live: HashSet::new(), dead: HashSet::new() };
        let t = std::time::Instant::now();
        let steps = std::env::var("LOCK_STEPS").unwrap_or_default();
        let steps: Vec<&str> = steps.split('|').filter(|x| !x.is_empty()).collect();
        let w = cfg.after(&steps);
        eprintln!("fair_seal: {}", cfg.fair_seal(&mut memo, &w));
        let r = cfg.can_seal(&mut memo, &w);
        eprintln!("[probe {item}] {:?} live={} dead={} {:.1?}", r, memo.live.len(), memo.dead.len(), t.elapsed());
    }
}

/// A scenario: a shape and a prefix that sets the situation up; the search
/// is exhaustive from its end (faults included).
fn scenario(name: &str) -> (Config, Vec<&'static str>) {
    match name {
        // H1: X approved by B and C, A never; A leads, B accepts; then free
        "h1" => (
            shape_withdraw(3, 2),
            vec![
                "deliver B->A Card(X)",
                "deliver B->C Card(X)",
                "B intent X",
                "C intent X",
                "deliver B->A Intent(X)",
                "deliver C->A Intent(X)",
                "A opens a ballot",
                "deliver A->B Prepare(r1)",
                "deliver B->A Promise(1:A)",
                "deliver A->B Accept(r1, X)",
            ],
        ),
        // H2: C's old device is back; A, knowing the restore, leads it
        "h2" => (
            shape_restore(3, 2),
            vec!["C REVIVES (old device)", "A intent R(C)", "A opens a ballot", "deliver A->B Prepare(r1)", "deliver B->A Promise(1:A)"],
        ),
        // the m = n twin: C's old device is back, nobody told it
        "h2_strict" => (shape_restore(3, 3), vec!["C REVIVES (old device)"]),
        // a restore whose card stops being open (withdrawn as the stand-in
        // for declined) while its subject's old device is back
        "restore_closed" => (shape_restore_withdraw(3, 2), vec!["C REVIVES (old device)", "A intent R(C)"]),
        // A never sees X; B leads X, accepts it alone; then free
        "forced_unseen" => (
            shape_withdraw(3, 2),
            vec![
                "deliver B->C Card(X)",
                "B intent X",
                "C intent X",
                "deliver C->B Intent(X)",
                "B opens a ballot",
                "deliver B->C Prepare(r1)",
                "deliver C->B Promise(1:B)",
            ],
        ),
        // O1: R(C) chosen by A and B while C is gone; then C's old device
        // comes back and is heard
        "o1" => (
            shape_restore(3, 2),
            vec![
                "deliver A->B Card(R(C))",
                "A intent R(C)",
                "B intent R(C)",
                "deliver B->A Intent(R(C))",
                "A opens a ballot",
                "deliver A->B Prepare(r1)",
                "deliver B->A Promise(1:A)",
                "deliver A->B Accept(r1, R(C))",
            ],
        ),
        // O3/amnesia: the scenario is the shape itself
        "import" => (shape_amnesia(3, 2, Rule::Import), vec![]),
        other => panic!("unknown scenario {other}"),
    }
}

/// `LOCK_SCENARIO="h1 h2"`, bounds as `bounds_from_env`.
#[test]
#[ignore = "runner: LOCK_SCENARIO=... cargo test --release -p molt-engine --lib explore_scenario -- --ignored --nocapture"]
fn explore_scenario() {
    let names = std::env::var("LOCK_SCENARIO").unwrap_or_default();
    for name in names.split_whitespace() {
        let (mut cfg, prefix) = scenario(name);
        if let Ok(f) = std::env::var("LOCK_FIXES") {
            cfg.fixes = fixes_of(&f);
        }
        bounds_from_env(&mut cfg);
        let live = env_u("LOCK_LIVE").map_or(true, |v| v != 0);
        let t = std::time::Instant::now();
        let r = cfg.explore_after(&prefix, live);
        eprintln!("[scenario {name} v2={}] {:.1?}\n{}", cfg.v2, t.elapsed(), summary(&cfg, &r));
    }
}
