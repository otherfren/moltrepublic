// SPDX-License-Identifier: GPL-3.0-or-later

//! B7b: the lock wired into the engine, seats in one process.

use super::*;
use crate::chain::test_support::*;
use molt_core::{ChainChange, EventEnvelope, MembershipOp, ProposalId, Surface, WorkspaceEvent};
use molt_storage::identity_sign;
use serde_json::json;

const T: u64 = 1_751_000_000;
/// The sealing line by name: dora leads, mara is next, walter last.
const SEATS: [&str; 3] = ["dora", "mara", "walter"];

/// `st` heard exactly `seats` at `now`.
fn hear(st: &mut crate::State, now: u64, seats: &[&str]) {
    st.presence.clock_override = Some(now);
    st.session.active_workspace = "w-lock".to_string();
    let roster = st.roster();
    let members = molt_core::roster_members(&roster, now, |m| {
        if seats.contains(&m) || m == st.member() {
            now
        } else {
            molt_core::MemberInfo::NEVER
        }
    });
    st.session.workspaces.retain(|w| w.id != "w-lock");
    st.session.workspaces.push(molt_core::WorkspaceInfo {
        id: "w-lock".to_string(),
        name: "Chess Club".to_string(),
        detail: "2-of-3".to_string(),
        synced: false,
        state: 0,
        last_sync_min: 0,
        sync_queue: 0,
        s3: false,
        size_kib: 0,
        last_backup_min: molt_core::WorkspaceInfo::NEVER,
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

/// A capable 2-of-3 seat that heard every seat at `T`.
fn seat(member: &str, b: &Builder) -> crate::State {
    let mut st = genesis_seat(member, b, b.blocks.clone());
    st.chain.lock.all_capable = Some(true);
    st.lock_force(true);
    hear(&mut st, T, &SEATS);
    st
}

/// Seats in one process; `pump` delivers every recorded own frame to
/// every other seat unless `cut` says the link is down.
struct Net {
    seats: Vec<crate::State>,
    down: Vec<(usize, usize)>,
}

impl Net {
    fn new(b: &Builder) -> Self {
        Self { seats: SEATS.iter().map(|m| seat(m, b)).collect(), down: Vec::new() }
    }

    fn at(&mut self, now: u64) {
        for s in &mut self.seats {
            let heard: Vec<&str> = SEATS.to_vec();
            hear(s, now, &heard);
        }
    }

    /// Deliver until nothing moves; returns the frames delivered.
    fn pump(&mut self) -> usize {
        let mut moved = 0;
        for _ in 0..64 {
            let mut frames: Vec<(usize, EventEnvelope)> = Vec::new();
            for (i, s) in self.seats.iter_mut().enumerate() {
                frames.extend(std::mem::take(&mut s.chain.lock.outbox).into_iter().map(|e| (i, e)));
            }
            if frames.is_empty() {
                break;
            }
            for (from, env) in frames {
                for to in 0..self.seats.len() {
                    if to == from || self.down.contains(&(from, to)) {
                        continue;
                    }
                    moved += 1;
                    let env = EventEnvelope { prev_seq: 0, ..env.clone() };
                    let by = env.by.clone();
                    self.seats[to].cmd_net_delivered(by, env, None).expect("a delivery acks");
                }
            }
        }
        moved
    }

    fn tick(&mut self, now: u64) {
        self.at(now);
        for s in &mut self.seats {
            s.lock_tick();
        }
        self.pump();
    }

    fn heights(&self) -> Vec<u64> {
        self.seats.iter().map(|s| s.chain.head.as_ref().expect("head").height).collect()
    }

    fn heads(&self) -> Vec<String> {
        self.seats.iter().map(|s| s.chain.head.as_ref().expect("head").hash.clone()).collect()
    }
}

fn note(st: &mut crate::State, title: &str) -> u64 {
    propose(st, Surface::Memory, json!({ "op": "add_note", "title": title }))
}

fn applied_id(st: &crate::State, height: u64) -> Option<u64> {
    st.chain.blocks.iter().find(|b| b.height == height).and_then(|b| match &b.change {
        ChainChange::Applied { proposal_id, .. } => Some(*proposal_id),
        _ => None,
    })
}

fn own_sigs(st: &crate::State) -> usize {
    let me = st.member();
    st.chain.pending_sigs.values().filter(|p| p.sigs.iter().any(|a| a.member == me)).count()
}

fn sent_kinds(st: &crate::State) -> Vec<&'static str> {
    st.chain
        .lock
        .outbox
        .iter()
        .map(|e| match &e.body {
            WorkspaceEvent::LockPrepare { .. } => "prepare",
            WorkspaceEvent::LockPromise { .. } => "promise",
            WorkspaceEvent::LockAccept { .. } => "accept",
            WorkspaceEvent::LockAccepted { .. } => "accepted",
            WorkspaceEvent::LockNack { promised: None, .. } => "nack-free",
            WorkspaceEvent::LockNack { .. } => "nack",
            WorkspaceEvent::Approved { sig, .. } if sig.is_empty() => "intent",
            WorkspaceEvent::Approved { .. } => "signed",
            WorkspaceEvent::Committed(_) => "committed",
            _ => "other",
        })
        .filter(|k| *k != "other")
        .collect()
}

/// The gate both ways: a seat that signed freely at a height answers a
/// `Prepare` there with a free `Nack`; a seat holding a promise there
/// gives no free signature, whatever it believes about the capability.
#[test]
fn the_gate_keeps_free_signing_and_the_lock_apart_per_height() {
    let b = Builder::new(&SEATS, 2);
    let mut walter = seat("walter", &b);
    walter.chain.lock.all_capable = Some(false);
    let id = note(&mut walter, "free");
    assert_eq!(own_sigs(&walter), 1, "a free seat signs (B6)");
    assert!(walter.chain.lock.acceptor.signed_freely(1), "the free mark");
    assert!(walter.chain.lock.disk.is_some(), "durable before the frame");
    walter.chain.lock.outbox.clear();
    let base = walter.lock_slot().expect("slot").base;
    wire(&mut walter, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 1 });
    assert_eq!(sent_kinds(&walter), vec!["nack-free"], "never a promise where it signed freely");
    let _ = id;

    let mut mara = seat("mara", &b);
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 1 });
    assert_eq!(sent_kinds(&mara), vec!["promise"]);
    mara.chain.lock.all_capable = Some(false);
    assert!(!mara.lock_sign_freely(1), "the gate refuses a free signature over a promise");
    mara.chain.lock.outbox.clear();
    note(&mut mara, "after the promise");
    assert_eq!(own_sigs(&mara), 0, "no signature left the seat");
    assert_eq!(sent_kinds(&mara), vec!["intent"], "the approval stays an intent");
}

/// At a locked height an approval is an unsigned intent, sent once; a
/// head move re-signs nothing (§9).
#[test]
fn an_intent_is_unsigned_and_a_head_move_resends_nothing() {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = seat("walter", &b);
    let id = note(&mut walter, "one");
    assert_eq!(own_sigs(&walter), 0);
    assert_eq!(sent_kinds(&walter).iter().filter(|k| **k == "intent").count(), 1);
    assert!(walter.own_signature_stands(id), "the intent is the standing voice");
    walter.chain_sign_and_gossip_approval(id);
    walter.chain.lock.outbox.clear();
    b.commit_applied(90, &["dora", "mara"]);
    walter.receive_block_from("dora", b.blocks[1].clone());
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 1);
    let kinds = sent_kinds(&walter);
    assert!(!kinds.contains(&"intent") && !kinds.contains(&"signed"), "nothing re-signed: {kinds:?}");
    assert_eq!(walter.chain_approval_count(id), 1, "the display counts the intent");
}

/// Three capable seats: intents, one ballot, one block - the same on all
/// three, signed by the ballot's acceptors, no free signature anywhere.
#[test]
fn three_seats_seal_through_the_lock() {
    let b = Builder::new(&SEATS, 2);
    let mut net = Net::new(&b);
    let id = note(&mut net.seats[2], "page");
    net.pump();
    net.seats[1].chain_sign_and_gossip_approval(id);
    net.pump();
    assert_eq!(net.heights(), vec![1, 1, 1]);
    let heads = net.heads();
    assert!(heads.iter().all(|h| *h == heads[0]), "one head");
    for s in &net.seats {
        assert_eq!(applied_id(s, 1), Some(id));
        assert_eq!(own_sigs(s), 0, "{}: no free signature", s.member());
        assert_eq!(s.chain.fork_decisions, 0);
    }
    verify_chain(&net.seats[0].chain.blocks).expect("an ordinary chain");
    // the next slot is open at once: the default leader prepared it
    assert!(net.seats[0].chain_sync().lock_ballot.ends_with(":dora"), "{:?}", net.seats[0].chain_sync());
    assert!(net.seats[0].chain_sync().lock_active);
}

/// A failed lock write sends nothing and keeps nothing: the reply never
/// leaves and the state on "disk" (and in memory) is the old one.
#[test]
fn a_failed_lock_write_sends_nothing() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    let slot = mara.lock_slot().expect("slot");
    mara.chain.lock.fail_writes = true;
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: slot.base.clone(), round: 3 });
    assert!(sent_kinds(&mara).is_empty(), "no promise left");
    assert_eq!(mara.chain.lock.acceptor.promised(&slot), None, "rolled back");
    assert_eq!(mara.chain.lock.disk, None, "nothing written");
    // the same for a free signature
    mara.chain.lock.all_capable = Some(false);
    note(&mut mara, "free");
    assert_eq!(own_sigs(&mara), 0, "no signature without its durable mark");
    assert!(!mara.chain.lock.acceptor.signed_freely(1));
    // a crash after the change, before the write: the reload is the old state
    mara.chain.lock.fail_writes = false;
    mara.chain.lock.all_capable = Some(true);
    let disk = mara.chain.lock.disk.clone();
    mara.load_chain_lock(Ok(disk), true);
    mara.chain.lock.all_capable = Some(true);
    assert_eq!(mara.chain.lock.acceptor.promised(&slot), None);
}

/// A seat restarted mid-height keeps its promise: reloaded from its file,
/// it nacks a lower ballot and accepts nothing below it.
#[test]
fn a_restarted_seat_keeps_its_promise() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    let slot = mara.lock_slot().expect("slot");
    wire(&mut mara, "walter", 1, WorkspaceEvent::LockPrepare { height: 1, base: slot.base.clone(), round: 5 });
    let disk = mara.chain.lock.disk.clone().expect("written");

    let mut again = seat("mara", &b);
    again.load_chain_lock(Ok(Some(disk)), true);
    again.chain.lock.all_capable = Some(true);
    assert_eq!(
        again.chain.lock.acceptor.promised(&slot),
        Some(&Ballot { round: 5, leader: "walter".into() })
    );
    wire(&mut again, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: slot.base.clone(), round: 4 });
    let id = note(&mut again, "x");
    again.chain.lock.outbox.retain(|e| !matches!(e.body, WorkspaceEvent::Approved { .. }));
    let digest = again.lock_value(1, &again.proposal_change(id).expect("change")).expect("value").digest;
    wire(
        &mut again,
        "dora",
        2,
        WorkspaceEvent::LockAccept { height: 1, base: slot.base, round: 4, digest, need: 2, id: ProposalId(id), change: None },
    );
    assert_eq!(sent_kinds(&again), vec!["nack", "nack"], "the lower ballot is refused twice");
}

/// §6: a chain without its lock file answers nothing and signs nothing
/// up to max(own head, the first peer report) + 1; past the floor it is
/// an ordinary seat again.
#[test]
fn amnesia_answers_nothing_up_to_the_floor() {
    let mut b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    mara.load_chain_lock(Ok(None), true);
    mara.chain.lock.all_capable = Some(true);
    let slot = mara.lock_slot().expect("slot");
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: slot.base, round: 1 });
    mara.chain.lock.all_capable = Some(false);
    note(&mut mara, "free");
    assert!(sent_kinds(&mara).is_empty(), "nothing answered: {:?}", sent_kinds(&mara));
    assert_eq!(own_sigs(&mara), 0);
    // a peer reports height 1: floor = 2
    b.commit_applied(70, &["dora", "walter"]);
    mara.receive_block_from("dora", b.blocks[1].clone());
    assert_eq!(mara.chain.lock.amnesia, Some(Some(2)));
    assert!(mara.lock_amnesiac_at(2) && !mara.lock_amnesiac_at(3));
    b.commit_applied(71, &["dora", "walter"]);
    mara.receive_block_from("dora", b.blocks[2].clone());
    assert_eq!(mara.chain.lock.amnesia, None, "past the floor");
    assert!(mara.chain.lock.disk.is_some(), "the file exists again");
}

/// §3.1: the slot base is one function of the changes - a pruned holder
/// (anchored on the cut) computes it like a full one, a relink onto an
/// attestation variant keeps it, another change moves it.
#[test]
fn the_lineage_is_one_for_full_pruned_and_relinked_holders() {
    let mut b = Builder::new(&SEATS, 2);
    b.commit_applied(1, &["dora", "mara"]);
    b.commit_applied(2, &["dora", "mara"]);
    b.commit(ChainChange::Checkpoint { upto: 2, state_hash: "ab".repeat(32) }, &SEATS);
    b.commit_applied(4, &["dora", "mara"]);
    b.commit_applied(5, &["mara", "walter"]);
    let rid = b.republic_id.clone();
    let full = lineage(&rid, &b.blocks).expect("full");
    let pruned = lineage(&rid, &b.blocks[3..]).expect("pruned");
    assert_eq!(full, pruned, "a pruned holder names the same slot");

    // an attestation variant of block 4 (other signers), relinked above
    let mut v = b.clone();
    v.blocks.truncate(4);
    v.head_hash = block_hash(&rid, &v.blocks[3]);
    v.commit_applied(4, &["mara", "walter"]);
    v.commit_applied(5, &["dora", "walter"]);
    assert_ne!(block_hash(&rid, &v.blocks[5]), block_hash(&rid, &b.blocks[5]), "another chain of hashes");
    assert_eq!(lineage(&rid, &v.blocks).expect("variant"), full, "the same slot after a relink");

    let mut other = b.clone();
    other.blocks.truncate(5);
    other.head_hash = block_hash(&rid, &other.blocks[4]);
    other.commit_applied(6, &["mara", "walter"]);
    assert_ne!(lineage(&rid, &other.blocks).expect("other"), full, "another decision, another slot");

    // the engine's cached slot agrees with the function, block by block
    let mut st = seat("walter", &Builder::new(&SEATS, 2));
    let mut c = Builder::new(&SEATS, 2);
    for id in [11, 12, 13] {
        let block = c.commit(
            ChainChange::Applied { proposal_id: id, surface: Surface::Memory, payload: json!({ "op": "add_note", "id": id }) },
            &["dora", "mara"],
        );
        st.receive_block_from("dora", block);
        let slot = st.lock_slot().expect("slot");
        assert_eq!(slot.base, hex::encode(lineage(&c.republic_id, &c.blocks).expect("l")));
    }
}

/// A seat that catches up through a served checkpoint (blob + anchor +
/// suffix, no history below the cut) names the same slot as a full holder.
#[test]
fn a_seat_caught_up_through_a_served_checkpoint_names_the_same_slot() {
    let mut b = Builder::new(&SEATS, 2);
    b.commit_applied(1, &["dora", "mara"]);
    b.commit_applied(2, &["dora", "mara"]);
    let state = checkpoint_state(&b.blocks, 2).expect("state at the cut");
    b.commit(ChainChange::Checkpoint { upto: 2, state_hash: checkpoint_state_hash(&state) }, &SEATS);
    b.commit_applied(4, &["mara", "walter"]);
    let mut full = seat("mara", &b);
    let mut late = genesis_seat("walter", &b, b.blocks[..1].to_vec());
    late.chain.lock.all_capable = Some(true);
    late.lock_force(true);
    for block in &b.blocks[3..] {
        late.chain.pending_blocks.insert(block.height, block.clone());
    }
    late.receive_checkpoint_blob(state);
    assert_eq!(late.chain.head.as_ref().expect("head").height, 4, "re-anchored on the served cut");
    assert_eq!(late.chain.blocks.len(), 2, "no history below the cut");
    assert_eq!(late.lock_slot(), full.lock_slot(), "one slot for the full and the caught-up holder");
}

/// What an older node does with an intent: nothing (`receive_approval`
/// returns early on an empty signature), and it keeps none of it.
#[test]
fn an_older_node_ignores_an_unsigned_approval() {
    let b = Builder::new(&SEATS, 2);
    let mut old = seat("mara", &b);
    old.lock_force(false);
    let id = note(&mut old, "card");
    old.chain.lock.outbox.clear();
    assert!(!old.receive_approval(id, "dora", 1, ""), "not a refused future height either");
    wire(&mut old, "dora", 1, WorkspaceEvent::Approved { id: ProposalId(id), by: "dora".into(), height: 1, sig: String::new() });
    wire(&mut old, "walter", 2, WorkspaceEvent::Approved { id: ProposalId(id), by: "walter".into(), height: 1, sig: String::new() });
    assert_eq!(old.chain.head.as_ref().expect("head").height, 0, "no block from intents");
    assert!(
        old.chain.pending_sigs.get(&id).map_or(true, |p| p.sigs.iter().all(|a| a.member == "mara")),
        "nothing collected"
    );
    let base = old.lock_slot().expect("slot").base;
    wire(&mut old, "dora", 3, WorkspaceEvent::LockPrepare { height: 1, base, round: 1 });
    assert!(old.chain.lock.outbox.is_empty(), "an older build answers no lock frame");
}

/// §5: two seats open ballots at once; the lower is refused, its leader
/// yields and backs off, one ballot seals.
#[test]
fn two_simultaneous_takeovers_back_off_and_one_wins() {
    let b = Builder::new(&SEATS, 2);
    let mut net = Net::new(&b);
    // dora is unreachable; mara and walter both take over
    net.down = vec![(0, 1), (0, 2), (1, 0), (2, 0)];
    let id = note(&mut net.seats[1], "contested");
    net.pump();
    net.seats[2].chain_sign_and_gossip_approval(id);
    for s in &mut net.seats[1..] {
        s.lock_start(0);
    }
    net.pump();
    let mut now = T;
    for _ in 0..40 {
        if net.heights()[1..] == [1, 1] {
            break;
        }
        now += 10;
        net.tick(now);
    }
    assert_eq!(net.heights()[1..], [1, 1], "one ballot sealed");
    assert_eq!(applied_id(&net.seats[1], 1), Some(id));
    assert_eq!(net.seats[1].chain.head, net.seats[2].chain.head, "one head");
    assert_eq!(net.seats[1].chain.fork_decisions + net.seats[2].chain.fork_decisions, 0);
}

/// A leader stops after its `Accept` left, one acceptance in; the next
/// seat in line takes over after its grace and seals the SAME change.
#[test]
fn a_stopped_leader_is_completed_by_the_next_with_the_same_change() {
    let b = Builder::new(&SEATS, 2);
    let mut net = Net::new(&b);
    net.seats[0].lock_seams.stop_after_accept.store(true, std::sync::atomic::Ordering::SeqCst);
    // walter hears nobody: dora's Accept reaches mara only
    net.down = vec![(0, 2), (1, 2)];
    let second = note(&mut net.seats[2], "second");
    let first = note(&mut net.seats[2], "first");
    assert!(second < first);
    net.pump();
    net.seats[1].chain_sign_and_gossip_approval(first);
    net.pump();
    assert!(net.seats[0].lock_seams.stopped.load(std::sync::atomic::Ordering::SeqCst), "dora proposed and stopped");
    assert_eq!(net.heights(), vec![0, 0, 0], "one acceptance, nothing sealed");
    // dora is gone; a second card is ready too, but the slot is forced
    net.down = vec![(0, 1), (0, 2), (1, 0), (2, 0)];
    // the smaller id becomes ready now: a fresh choice would take it
    net.pump();
    net.seats[1].chain_sign_and_gossip_approval(second);
    net.pump();
    let mut now = T;
    for _ in 0..60 {
        if net.heights()[1..] == [1, 1] {
            break;
        }
        now += 5;
        net.tick(now);
    }
    assert_eq!(net.heights()[1..], [1, 1], "the next leader sealed");
    assert_eq!(applied_id(&net.seats[1], 1), Some(first), "the forced value, not a fresh choice");
    assert_eq!(applied_id(&net.seats[2], 1), Some(first));
}

/// §4.2: a consented restore of a lost seat, both survivors reachable:
/// one survivor signature plus the consent seals it. With one survivor
/// alone phase 1 never completes, and the status says why.
#[test]
fn a_consented_restore_needs_both_survivors_in_two_of_three() {
    let b = Builder::new(&SEATS, 2);
    for both in [true, false] {
        let mut net = Net::new(&b);
        for s in &mut net.seats {
            hear(s, T, &["dora", "mara"]);
        }
        if !both {
            net.down = vec![(0, 1), (1, 0)];
        }
        let rid = b.republic_id.clone();
        let ticket = "ticket";
        let kp_hex = kp_hex_for(&b, "walter");
        let proof = crate::make_seat_proof(b.key("walter"), ticket, &kp_hex, &rid, "", &[]);
        let consent = consent_v2_for(&b, "walter", "", &[], 0, &"cc".repeat(32));
        let id = net.seats[1]
            .verify_and_propose_restore("walter", &b.pk("walter"), &kp_hex, ticket, &proof, "", &[], &consent, "")
            .expect("proposed");
        net.pump();
        let mut now = T;
        for _ in 0..20 {
            now += 5;
            for s in &mut net.seats {
                hear(s, now, &["dora", "mara"]);
                s.lock_tick();
            }
            net.pump();
        }
        let dora = &net.seats[0];
        if both {
            assert_eq!(dora.chain.head.as_ref().expect("head").height, 1, "sealed");
            let block = &dora.chain.blocks[1];
            assert!(matches!(&block.change, ChainChange::Membership { op: MembershipOp::Restored, .. }));
            assert_eq!(block.sigs.len(), 1, "one survivor signature beside the consent");
            verify_chain(&dora.chain.blocks).expect("verifies");
            let _ = id;
        } else {
            assert_eq!(net.heights(), vec![0, 0, 0], "one survivor alone does not seal");
            let sync = net.seats[1].chain_sync();
            assert_eq!(sync.lock_wait, "promises 1/2 missing dora", "{sync:?}");
        }
    }
}

/// §4.1: a cut the leader proposed with every promise in hand, refused by
/// one seat at accept time, holds no quorum; the escape frees the slot
/// and the next ordinary proposal seals.
#[test]
fn a_refused_cut_frees_the_slot_for_the_next_proposal() {
    let mut b = Builder::new(&SEATS, 2);
    b.commit_applied(1, &["dora", "mara"]);
    let mut net = Net { seats: SEATS.iter().map(|m| seat(m, &b)).collect(), down: Vec::new() };
    // walter is on a fork: it co-signs no cut and accepts none
    net.seats[2].chain.fork_candidates.insert(9, b.blocks[1].clone());
    net.seats[0].cmd_propose_checkpoint().expect("cut proposed");
    let cut = *net.seats[0].chain.proposal_changes.keys().next().expect("the cut");
    // dora believes walter co-signed: it proposes the cut with every promise
    net.seats[0].lock_note_intent(cut, "walter");
    net.pump();
    assert!(
        net.seats[1].chain.lock.outbox.is_empty() && net.seats[0].chain.lock.run.as_ref().is_some_and(|r| r.value.is_some()),
        "the cut was proposed"
    );
    assert_eq!(net.heights(), vec![1, 1, 1], "two acceptances of a cut that needs three");
    if let Some(s) = net.seats[0].chain.lock.intents.get_mut(&cut) {
        s.remove("walter");
    }
    // walter stays on its fork: it keeps refusing the cut, not the card
    let id = note(&mut net.seats[2], "after the cut");
    net.pump();
    net.seats[1].chain_sign_and_gossip_approval(id);
    net.pump();
    let mut now = T;
    for _ in 0..80 {
        if net.heights().iter().all(|h| *h == 2) {
            break;
        }
        now += 10;
        net.tick(now);
    }
    let changes: Vec<String> = net.seats[0].chain.blocks.iter().map(|b| format!("{}:{:?}", b.height, std::mem::discriminant(&b.change))).collect();
    assert_eq!(net.heights(), vec![2, 2, 2], "{changes:?} {:?}", net.seats[0].chain.blocks.last().map(|b| &b.change));
    assert_eq!(applied_id(&net.seats[0], 2), Some(id), "the escape freed the slot");
    let heads = net.heads();
    assert!(heads.iter().all(|h| *h == heads[0]));
}

/// Activation is not simultaneous: in 2-of-2 one seat already runs the
/// lock (its leader promised itself), the other still signs freely. With
/// exclusivity alone the height can never decide - the free seat never
/// accepts, the locked seat never signs freely. Once the free signers
/// leave the lock no quorum (`|F| >= n - m`), a seat holding only
/// promises there releases them durably and signs freely (§14).
#[test]
fn a_height_split_by_activation_still_decides() {
    let b = Builder::new(&["dora", "mara"], 2);
    let dora = seat("dora", &b);
    let mut mara = seat("mara", &b);
    mara.chain.lock.all_capable = Some(false);
    let id = note(&mut mara, "split");
    assert_eq!(own_sigs(&mara), 1, "mara signed freely");
    let mut seats = [dora, mara];
    swap(&mut seats);
    seats[0].chain_sign_and_gossip_approval(id);
    for _ in 0..8 {
        swap(&mut seats);
    }
    let [dora, mara] = seats;
    assert_eq!(mara.chain.head.as_ref().expect("head").height, 1, "the block sealed");
    assert_eq!(dora.chain.head, mara.chain.head);
    assert_eq!(applied_id(&dora, 1), Some(id));
    assert!(dora.chain.lock.acceptor.signed_freely(1), "dora released its promise into a free mark");
}

/// The counterexample to the first release rule (`lock_tests`): in 2-of-2
/// the leader already ACCEPTED its value when the other seat signed
/// freely. No lock value can be chosen without that seat any more, so the
/// acceptance is released too and the height decides freely.
#[test]
fn an_acceptance_is_released_once_no_quorum_can_complete_it() {
    let b = Builder::new(&["dora", "mara"], 2);
    let dora = seat("dora", &b);
    let mara = seat("mara", &b);
    let mut seats = [dora, mara];
    let id = note(&mut seats[1], "split");
    swap(&mut seats);
    seats[0].chain_sign_and_gossip_approval(id);
    let slot = seats[0].lock_slot().expect("slot");
    assert!(seats[0].chain.lock.acceptor.accepted(&slot).is_some(), "dora proposed and accepted");
    // mara has not heard dora yet and now believes the height free
    seats[0].chain.lock.outbox.clear();
    seats[1].chain.lock.all_capable = Some(false);
    seats[1].chain.lock.intent_sent.clear();
    seats[1].chain_sign_and_gossip_approval(id);
    assert_eq!(own_sigs(&seats[1]), 1, "mara signed freely");
    for _ in 0..8 {
        swap(&mut seats);
    }
    let [dora, mara] = seats;
    assert!(dora.chain.lock.acceptor.signed_freely(1), "the acceptance was released");
    assert_eq!(mara.chain.head.as_ref().expect("head").height, 1, "the height decided");
    assert_eq!(dora.chain.head, mara.chain.head);
}

/// The residual the release rule does not close (design §14): 3-of-5, one
/// seat signed freely in an activation race, two lock seats approve the
/// same card. Three voices, but neither path has three: the height waits
/// for a third lock voice. B6 alone seals the same votes at once.
#[test]
fn a_value_split_between_a_free_and_two_lock_voices_waits() {
    let names = ["dora", "erik", "mara", "nils", "walter"];
    let b = Builder::new(&names, 3);
    for lock in [true, false] {
        let seats: Vec<crate::State> = names
            .iter()
            .map(|m| {
                let mut st = genesis_seat(m, &b, b.blocks.clone());
                st.chain.lock.all_capable = Some(lock && *m != "walter");
                st.lock_force(lock);
                hear(&mut st, T, &names);
                st
            })
            .collect();
        let mut net = Net { seats, down: Vec::new() };
        let id = note(&mut net.seats[4], "split");
        net.pump();
        net.seats[0].chain_sign_and_gossip_approval(id);
        net.seats[2].chain_sign_and_gossip_approval(id);
        net.pump();
        let mut now = T;
        for _ in 0..30 {
            now += 10;
            for s in &mut net.seats {
                hear(s, now, &names);
                s.lock_tick();
            }
            net.pump();
        }
        let heights = net.heights();
        if lock {
            assert_eq!(heights, vec![0; 5], "the lock waits");
            assert!(net.seats[0].chain.lock.run.is_some(), "dora still leads the height");
        } else {
            assert_eq!(heights, vec![1; 5], "B6 seals the three voices");
        }
    }
}

/// A capability goes away while an approval is only an intent: once
/// head+1 is free, the tick signs the standing intent freely (a B6
/// re-base re-signs collected signatures only), on both sides.
#[test]
fn an_intent_is_signed_freely_once_the_height_goes_free() {
    let b = Builder::new(&SEATS, 2);
    let mut walter = seat("walter", &b);
    let id = note(&mut walter, "one");
    assert_eq!(own_sigs(&walter), 0, "an intent first");
    walter.chain.lock.all_capable = Some(false);
    walter.lock_tick();
    assert_eq!(own_sigs(&walter), 1, "signed freely once the height is free");
    walter.lock_tick();
    assert_eq!(sent_kinds(&walter).iter().filter(|k| **k == "signed").count(), 1, "once");
    // the seat that itself lost the capability does the same
    let mut mara = seat("mara", &b);
    let id2 = note(&mut mara, "two");
    mara.lock_force(false);
    mara.lock_tick();
    assert_eq!(own_sigs(&mara), 1);
    let _ = (id, id2);
}

/// Deliver each seat's recorded frames to the other.
fn swap(seats: &mut [crate::State; 2]) {
    for from in 0..2 {
        let frames: Vec<EventEnvelope> = std::mem::take(&mut seats[from].chain.lock.outbox);
        for env in frames {
            let by = env.by.clone();
            seats[1 - from].cmd_net_delivered(by, EventEnvelope { prev_seq: 0, ..env }, None).expect("ack");
        }
    }
}

/// With fewer free signers than `n - m` the lock still decides, and a
/// seat holding a promise keeps it.
#[test]
fn one_free_signer_in_three_of_five_releases_nothing() {
    let names = ["dora", "erik", "mara", "nils", "walter"];
    let b = Builder::new(&names, 3);
    let mut dora = genesis_seat("dora", &b, b.blocks.clone());
    dora.chain.lock.all_capable = Some(true);
    dora.lock_force(true);
    hear(&mut dora, T, &names);
    let id = note(&mut dora, "x");
    assert!(dora.chain.lock.run.is_some(), "dora leads and promised itself");
    let change = dora.proposal_change(id).expect("change");
    let sig = identity_sign(b.key("walter"), &approval_bytes(&b.republic_id, 1, &change));
    wire(&mut dora, "walter", 1, WorkspaceEvent::Approved { id: ProposalId(id), by: "walter".into(), height: 1, sig });
    assert!(!dora.chain.lock.acceptor.signed_freely(1), "1 < n - m = 2: the promise stands");
    assert!(!dora.chain.lock.acceptor.may_sign_freely(1));
}

/// The slot type stays what the wire carries.
#[test]
fn a_slot_is_height_and_lineage() {
    let s = Slot { height: 1, base: "ab".into() };
    assert_eq!(s.height, 1);
    let _ = identity_sign;
}
