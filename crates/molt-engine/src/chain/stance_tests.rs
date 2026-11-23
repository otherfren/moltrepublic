// SPDX-License-Identifier: GPL-3.0-or-later

//! WP2: a seat's standing approval outlives the position it was signed
//! at - a fork, a tip swap and a heal re-express it at the new head+1.

use super::*;
use crate::chain::test_support::*;
use molt_core::{EventEnvelope, ProposalId, Surface, WorkspaceEvent};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const T: u64 = 1_751_000_000;
/// Sorted: dora seals first, petra and walter are the forked pair.
const NAMES: [&str; 3] = ["dora", "petra", "walter"];
const D: usize = 0;
const P: usize = 1;
const W: usize = 2;

fn hear(st: &mut crate::State, now: u64) {
    st.presence.clock_override = Some(now);
    st.session.active_workspace = "w-stance".to_string();
    let roster = st.roster();
    let members = molt_core::roster_members(&roster, now, |_| now);
    st.session.workspaces.retain(|w| w.id != "w-stance");
    st.session.workspaces.push(molt_core::WorkspaceInfo {
        id: "w-stance".to_string(),
        name: "Chess Club".to_string(),
        detail: "2-of-3".to_string(),
        synced: false,
        state: 0,
        last_sync_min: 0,
        sync_queue: 0,
        s3: false,
        size_kib: 0,
        last_backup_min: molt_core::WorkspaceInfo::NEVER,
        backup_checked_min: molt_core::WorkspaceInfo::NEVER,
        backup_copies: 0,
        backup_error: String::new(),
        has_seed: false,
        net: "nostr".to_string(),
        encrypted: false,
        members,
        agenda: String::new(),
        restored: false,
    });
}

/// Seats in one process with an ordered queue per directed link: a link
/// that is down holds its frames (the delivery guarantee), it never loses
/// them.
struct Net {
    seats: Vec<crate::State>,
    links: BTreeMap<(usize, usize), VecDeque<EventEnvelope>>,
    down: BTreeSet<(usize, usize)>,
    /// Every own frame each seat sent, in order.
    sent: Vec<Vec<EventEnvelope>>,
}

impl Net {
    fn new() -> Self {
        let b = Builder::new(&NAMES, 2);
        let seats: Vec<crate::State> = NAMES
            .iter()
            .map(|m| {
                let mut st = genesis_seat(m, &b, b.blocks.clone());
                hear(&mut st, T);
                st
            })
            .collect();
        Self { seats, links: BTreeMap::new(), down: BTreeSet::new(), sent: vec![Vec::new(); NAMES.len()] }
    }

    fn cut(&mut self, a: usize, b: usize) {
        self.down.insert((a, b));
        self.down.insert((b, a));
    }

    fn flush(&mut self) {
        for i in 0..self.seats.len() {
            let out = std::mem::take(&mut self.seats[i].chain.lock.outbox);
            for env in out {
                self.sent[i].push(env.clone());
                for j in 0..self.seats.len() {
                    if j != i {
                        self.links.entry((i, j)).or_default().push_back(env.clone());
                    }
                }
            }
        }
    }

    /// Deliver the next queued frame on `from -> to`, down or not.
    fn deliver_one(&mut self, from: usize, to: usize) -> bool {
        self.flush();
        let Some(env) = self.links.get_mut(&(from, to)).and_then(VecDeque::pop_front) else {
            return false;
        };
        let env = EventEnvelope { prev_seq: 0, ..env };
        let by = env.by.clone();
        self.seats[to].cmd_net_delivered(by, env, None).expect("a delivery acks");
        self.flush();
        true
    }

    fn deliver(&mut self, from: usize, to: usize) {
        while self.deliver_one(from, to) {}
    }

    /// Deliver over every up link until nothing moves.
    fn pump(&mut self) {
        loop {
            self.flush();
            let mut moved = false;
            let pairs: Vec<(usize, usize)> = self.links.keys().copied().collect();
            for (a, b) in pairs {
                if !self.down.contains(&(a, b)) && self.deliver_one(a, b) {
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }

    fn heal(&mut self) {
        self.down.clear();
        self.pump();
    }

    fn tip(&self, i: usize) -> String {
        self.seats[i].chain.head.as_ref().expect("head").hash.clone()
    }

    fn state(&self, i: usize, id: u64) -> Option<ProposalState> {
        self.seats[i].proposals.get(&id).map(|p| p.state)
    }

    /// Approved frames `seat` sent for `(id, height)`.
    fn approvals_sent(&self, seat: usize, id: u64, height: u64) -> usize {
        self.sent[seat]
            .iter()
            .filter(|e| matches!(&e.body, WorkspaceEvent::Approved { id: i, height: h, .. } if i.0 == id && *h == height))
            .count()
    }
}

fn note(st: &mut crate::State, title: &str) -> u64 {
    propose(st, Surface::Memory, json!({ "op": "add_note", "title": title }))
}

/// A fork at height 1: dora co-signs petra's card `c1` and walter's card
/// `c2` at height 1; dora seals `c1`, walter seals `c2` before either
/// block reaches the other side. petra and walter cannot reach each other.
fn forked(salt: u32) -> (Net, u64, u64) {
    let mut net = Net::new();
    net.cut(P, W);
    let c1 = note(&mut net.seats[P], &format!("one {salt}"));
    let c2 = note(&mut net.seats[W], &format!("two {salt}"));
    // dora learns c2 without walter's signature, and co-signs it at 1
    assert!(net.deliver_one(W, D), "walter's card reaches dora");
    net.seats[D].cmd_approve(ProposalId(c2), None).expect("dora approves c2");
    net.deliver(P, D);
    net.seats[D].cmd_approve(ProposalId(c1), None).expect("dora approves c1");
    assert_eq!(net.state(D, c1), Some(ProposalState::Applied), "dora sealed c1 at 1");
    // dora's c2 signature at 1 reaches walter ahead of dora's block
    assert!(net.deliver_one(D, W));
    assert_eq!(net.state(W, c2), Some(ProposalState::Applied), "walter sealed c2 at 1");
    assert_ne!(net.tip(D), net.tip(W), "two blocks at height 1");
    (net, c1, c2)
}

/// The salt whose fork dora's block wins (`true`) or walter's.
fn salt_where(dora_wins: bool) -> u32 {
    (0..64)
        .find(|s| {
            let (net, _, _) = forked(*s);
            (net.tip(D) < net.tip(W)) == dora_wins
        })
        .expect("both outcomes occur")
}

fn assert_converged(net: &Net, ids: &[u64]) {
    for (i, name) in NAMES.iter().enumerate() {
        assert_eq!(net.tip(i), net.tip(D), "{name} is on dora's head");
        for id in ids {
            assert_eq!(net.state(i, *id), Some(ProposalState::Applied), "{name} holds {id} applied");
        }
        let applied: Vec<u64> = net.seats[i]
            .chain
            .blocks
            .iter()
            .filter_map(|b| match &b.change {
                ChainChange::Applied { proposal_id, .. } => Some(*proposal_id),
                _ => None,
            })
            .collect();
        for id in ids {
            assert_eq!(applied.iter().filter(|a| *a == id).count(), 1, "{id} sealed once on {name}");
        }
    }
}

/// The operators' case: walter's block loses the tip; its card returns to
/// the vote and the fork closes with no operator action.
#[test]
fn a_fork_closes_and_both_cards_apply_without_an_operator() {
    let (mut net, c1, c2) = forked(salt_where(true));
    net.heal();
    assert_converged(&net, &[c1, c2]);
}

/// The same with walter's block winning: the kept side's head never moves
/// on its own, dora's and petra's displaced card still applies.
#[test]
fn a_fork_closes_where_the_winning_head_never_moves() {
    let (mut net, c1, c2) = forked(salt_where(false));
    let winner = net.tip(W);
    net.heal();
    assert_converged(&net, &[c1, c2]);
    assert_eq!(net.seats[W].chain.blocks[1].height, 1);
    assert_eq!(block_hash(&net.seats[W].republic_id(), &net.seats[W].chain.blocks[1]), winner, "walter kept its block");
}

/// The displaced card regains this seat's own vote at the new head+1, at
/// once and exactly once.
#[test]
fn a_card_displaced_by_a_tip_swap_regains_the_own_vote() {
    let (mut net, _, c2) = forked(salt_where(true));
    // dora's block reaches walter: the tip swaps, c2 is open again
    while net.tip(W) != net.tip(D) {
        assert!(net.deliver_one(D, W), "dora's block is queued");
    }
    assert_eq!(net.state(W, c2), Some(ProposalState::Proposed));
    let walter = &net.seats[W];
    let pending = walter.chain.pending_sigs.get(&c2).expect("c2 collects again");
    assert_eq!(pending.height, 2);
    assert!(pending.sigs.iter().any(|a| a.member == "walter"), "walter's own vote stands at 2");
    assert_eq!(net.approvals_sent(W, c2, 2), 1, "signed once at 2");
}

/// The kept side re-sends its standing signature once per height when a
/// losing contender shows the other side still has to swap.
#[test]
fn the_kept_side_re_sends_its_standing_vote_once() {
    let (mut net, _, c2) = forked(salt_where(true));
    // dora signed c2 at 2 after sealing c1; walter dropped it (c2 was sealed
    // on walter's branch then)
    assert_eq!(net.approvals_sent(D, c2, 2), 1);
    net.cut(D, W);
    // walter's block reaches dora (and once more, as a re-serve would)
    let y = net.seats[W].chain.blocks[1].clone();
    wire(&mut net.seats[D], "walter", 900, WorkspaceEvent::Committed(y.clone()));
    wire(&mut net.seats[D], "walter", 901, WorkspaceEvent::Committed(y));
    net.flush();
    assert_eq!(net.approvals_sent(D, c2, 2), 2, "re-sent once, not per contender");
}

/// A repeated `approve` re-sends the own signature and answers the same
/// vote; it never counts twice.
#[test]
fn approving_twice_is_ok_and_counts_once() {
    let mut net = Net::new();
    let c = note(&mut net.seats[W], "a");
    // petra holds the card, not walter's signature: one vote stays open
    assert!(net.deliver_one(W, P));
    let votes = |r: molt_core::Reply| match r {
        molt_core::Reply::Vote { approvals, state, .. } => (approvals, state),
        other => panic!("unexpected reply {other:?}"),
    };
    let first = votes(net.seats[P].cmd_approve(ProposalId(c), None).expect("approve"));
    let again = votes(net.seats[P].cmd_approve(ProposalId(c), None).expect("approve again"));
    assert_eq!(first, again, "the same vote, counted once");
    assert_eq!(first.0, 1);
    net.flush();
    assert_eq!(net.approvals_sent(P, c, 1), 2, "the repeat re-sends");
    let pending = net.seats[P].chain.pending_sigs.get(&c).expect("collected");
    assert_eq!(pending.sigs.len(), 1, "one signature per member");
}

fn diverge(st: &mut crate::State, peer: &str) {
    st.chain.diverged.insert(
        peer.to_string(),
        molt_core::ChainDivergence { peer: peer.to_string(), since_height: 0, seen_ts: T },
    );
}

/// A heal re-sends every standing signature once; a heal with another
/// peer at the same height is a new audience and re-sends again.
#[test]
fn a_heal_re_sends_the_standing_votes_once_per_heal() {
    let mut net = Net::new();
    let c = note(&mut net.seats[W], "a");
    diverge(&mut net.seats[W], "dora");
    net.seats[W].clear_divergence("dora");
    net.seats[W].clear_divergence("dora");
    net.flush();
    assert_eq!(net.approvals_sent(W, c, 1), 2, "the proposal's vote plus one re-send");
    diverge(&mut net.seats[W], "petra");
    net.seats[W].clear_divergence("petra");
    net.flush();
    assert_eq!(net.approvals_sent(W, c, 1), 3, "a second heal re-sends again");
}

/// A catch-up batch re-signs the standing votes once, at its final head -
/// not once per block.
#[test]
fn a_catch_up_batch_re_signs_once_at_the_final_head() {
    let mut b = Builder::new(&NAMES, 2);
    let blocks: Vec<ChainBlock> = (0..3u64)
        .map(|i| {
            b.commit(
                ChainChange::Applied { proposal_id: 100 + i, surface: Surface::Memory, payload: json!({ "op": "add_note", "title": i }) },
                &["dora", "petra"],
            )
        })
        .collect();
    let mut net = Net::new();
    let c = note(&mut net.seats[W], "a");
    wire(&mut net.seats[W], "dora", 10, WorkspaceEvent::Committed(blocks[2].clone()));
    wire(&mut net.seats[W], "dora", 11, WorkspaceEvent::Committed(blocks[1].clone()));
    wire(&mut net.seats[W], "dora", 12, WorkspaceEvent::Committed(blocks[0].clone()));
    net.flush();
    assert_eq!(net.seats[W].chain.head.as_ref().expect("head").height, 3);
    let heights: Vec<u64> = (1..=4).filter(|h| net.approvals_sent(W, c, *h) > 0).collect();
    assert_eq!(heights, vec![1, 4], "signed at the proposal and at the final head only");
}

/// Review R1: the stance a tip swap restored survives the NEXT head move
/// too - another card takes h+1, and the displaced card re-signs at h+2.
#[test]
fn a_displaced_card_re_signs_after_a_second_head_move() {
    let (mut net, c1, c2) = forked(salt_where(true));
    while net.tip(W) != net.tip(D) {
        assert!(net.deliver_one(D, W), "dora's block is queued");
    }
    // walter's re-sign at 2 and dora's c2 vote at 2 stay queued
    net.cut(D, W);
    let c3 = note(&mut net.seats[D], "three");
    net.deliver(D, P);
    net.seats[P].cmd_approve(ProposalId(c3), None).expect("petra approves c3");
    assert_eq!(net.state(P, c3), Some(ProposalState::Applied), "c3 took height 2");
    net.down.remove(&(P, W));
    net.deliver(P, W);
    assert_eq!(net.seats[W].chain.head.as_ref().expect("head").height, 2);
    assert_eq!(net.approvals_sent(W, c2, 3), 1, "walter's stance re-signs at 3");
    net.heal();
    assert_converged(&net, &[c1, c2, c3]);
}

/// Review R1, second path: an approval held unsigned (no signature in the
/// block, none in the stashed voters) keeps its stance through the block
/// that sealed it without this seat and the swap that displaced it.
#[test]
fn a_held_approval_survives_its_block_being_displaced() {
    let b = Builder::new(&NAMES, 2);
    let mut net = Net::new();
    let c = note(&mut net.seats[P], "held");
    assert!(net.deliver_one(P, W), "walter holds the card");
    let walter = &mut net.seats[W];
    walter.chain.own_approvals.insert(c);
    let payload = walter.proposals.get(&c).expect("the card").payload.clone();
    let sealed = b.seal(1, ChainChange::Applied { proposal_id: c, surface: Surface::Memory, payload }, &["dora", "petra"]);
    let rid = b.republic_id.clone();
    let rival = (0..64)
        .map(|i| b.seal(1, ChainChange::Applied { proposal_id: 500 + i, surface: Surface::Memory, payload: json!({ "op": "add_note", "title": i }) }, &["dora", "petra"]))
        .find(|r| block_hash(&rid, r) < block_hash(&rid, &sealed))
        .expect("a smaller rival");
    wire(walter, "dora", 900, WorkspaceEvent::Committed(sealed));
    assert_eq!(walter.proposals.get(&c).map(|p| p.state), Some(ProposalState::Applied));
    wire(walter, "dora", 901, WorkspaceEvent::Committed(rival));
    assert_eq!(walter.proposals.get(&c).map(|p| p.state), Some(ProposalState::Proposed), "displaced");
    assert!(walter.chain.own_approvals.contains(&c), "the stance stands");
    net.flush();
    assert_eq!(net.approvals_sent(W, c, 2), 1, "signed at the new head+1");
}
