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
    st.chain.lock.enabled_override = Some(true);
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
    // dora signed freely at 1: in 2-of-3 that leaves the lock no decision
    walter.lock_note_free("dora", 1);
    let id = note(&mut walter, "free");
    assert_eq!(own_sigs(&walter), 1, "a free height signs (B6)");
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
    // the same for a free mark: the release that cannot be written is not made
    mara.lock_note_free("walter", 1);
    note(&mut mara, "free");
    assert_eq!(own_sigs(&mara), 0, "no signature without its durable mark");
    assert!(!mara.chain.lock.acceptor.signed_freely(1));
    // a crash after the change, before the write: the reload is the old state
    mara.chain.lock.fail_writes = false;
    mara.chain.lock.enabled_override = Some(true);
    let disk = mara.chain.lock.disk.clone();
    mara.load_chain_lock(Ok(disk), true);
    mara.chain.lock.enabled_override = Some(true);
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
    again.chain.lock.enabled_override = Some(true);
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
        WorkspaceEvent::LockAccept { height: 1, base: slot.base, round: 4, digest, need: 2, id: ProposalId(id), change: None, forced: false },
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
    mara.chain.lock.enabled_override = Some(true);
    let slot = mara.lock_slot().expect("slot");
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: slot.base, round: 1 });
    assert!(!mara.lock_sign_freely(1), "no free signature up to the floor");
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
    late.chain.lock.enabled_override = Some(true);
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
        // walter's device is lost: nothing reaches it, nothing comes from it
        net.down = vec![(2, 0), (2, 1), (0, 2), (1, 2)];
        if !both {
            net.down.extend([(0, 1), (1, 0)]);
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
            assert_eq!(block.sigs.len(), 2, "both survivors beside the consent (H2: need m where m < n)");
            verify_chain(&dora.chain.blocks).expect("verifies");
            let _ = id;
        } else {
            // the restore needs both survivors' voices (H2): mara alone has
            // nothing ready and takes nothing over
            assert_eq!(net.heights(), vec![0, 0, 0], "one survivor alone does not seal");
            assert!(net.seats[1].chain.lock.run.is_none(), "no ballot without the voices");
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
    mara.chain.lock.enabled_override = Some(false);
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
    seats[1].chain.lock.enabled_override = Some(false);
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
                st.chain.lock.enabled_override = Some(lock);
                st.lock_force(lock && *m != "walter");
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
    walter.chain.lock.enabled_override = Some(false);
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

/// The chain decides where the lock runs (§8 as built): a charter that
/// carries it locks height 1, no capability heard; an older republic is
/// B6 until a sealed `set_features` vote, then locked from the next
/// height - on a full holder and on one anchored on a later cut.
#[test]
fn the_chain_decides_where_the_lock_runs() {
    use molt_core::chain::CHAIN_LOCK_FEATURE;
    let founded = Builder::new_with_features(&SEATS, 2, &[CHAIN_LOCK_FEATURE, "memory"], false);
    let st = genesis_seat("mara", &founded, founded.blocks.clone());
    st.lock_force(true);
    assert_eq!((st.lock_mode(1), st.lock_since()), (LockMode::Lock, 1), "locked from the founding");

    let mut old = Builder::new(&SEATS, 2);
    old.commit_applied(1, &["dora", "mara"]);
    let mut full = genesis_seat("mara", &old, old.blocks.clone());
    full.lock_force(true);
    assert_eq!((full.lock_mode(2), full.lock_since()), (LockMode::Free, 0), "B6 before the vote");
    old.commit_org(5, "set_features", CHAIN_LOCK_FEATURE, &["dora", "mara"]);
    full.receive_block_from("dora", old.blocks[2].clone());
    assert_eq!(full.chain.head.as_ref().expect("head").height, 2);
    assert_eq!((full.lock_mode(3), full.lock_since()), (LockMode::Lock, 3), "locked after the enabling block");

    let state = checkpoint_state(&old.blocks, 2).expect("state at the cut");
    old.commit(ChainChange::Checkpoint { upto: 2, state_hash: checkpoint_state_hash(&state) }, &SEATS);
    let mut late = genesis_seat("walter", &old, old.blocks[..1].to_vec());
    late.lock_force(true);
    late.chain.pending_blocks.insert(3, old.blocks[3].clone());
    late.receive_checkpoint_blob(state);
    assert_eq!(late.chain.head.as_ref().expect("head").height, 3, "anchored on the cut");
    assert_eq!(late.lock_mode(4), LockMode::Lock, "a pruned holder reads the same chain state");
    full.receive_block_from("dora", old.blocks[3].clone());
    assert_eq!(full.lock_mode(4), late.lock_mode(4));
}

/// The enabling vote waits while a seat runs a build without the lock:
/// the proposer refuses to put it up, an approver refuses to sign it.
#[test]
fn enabling_the_lock_waits_until_every_seat_runs_it() {
    use molt_core::chain::CHAIN_LOCK_FEATURE;
    let b = Builder::new(&SEATS, 2);
    let mut mara = genesis_seat("mara", &b, b.blocks.clone());
    mara.chain.lock.caps_override = Some(false);
    let enable = json!({ "op": "set_features", "value": format!("{CHAIN_LOCK_FEATURE} memory") });
    let err = mara.cmd_propose(Surface::Organization, enable.clone()).expect_err("refused");
    assert!(err.to_string().contains(LOCK_NOT_EVERYWHERE), "{err}");
    mara.chain.lock.caps_override = Some(true);
    let id = propose(&mut mara, Surface::Organization, enable);
    let mut walter = genesis_seat("walter", &b, b.blocks.clone());
    walter.chain.lock.caps_override = Some(false);
    let change = mara.proposal_change(id).expect("change");
    let ChainChange::Applied { payload, .. } = change else { panic!("an org change") };
    assert!(walter.receive_proposed(id, Surface::Organization, payload, "mara"));
    assert_eq!(walter.sign_blocker(id).as_deref(), Some(LOCK_NOT_EVERYWHERE));
    walter.chain.lock.caps_override = Some(true);
    assert_eq!(walter.sign_blocker(id), None);
}

/// After activation a seat on a build without the lock signs freely and
/// answers no lock frame; the capable seats still decide every height
/// (exclusivity keeps the two apart), on one head.
#[test]
fn a_seat_without_the_lock_after_activation_neither_stalls_nor_forks() {
    let with_lock = old_seat_run(true);
    let b6 = old_seat_run(false);
    eprintln!("fork decisions: lock {with_lock}, B6 {b6}");
    assert!(with_lock <= b6, "no more forks than B6: {with_lock} > {b6}");
}

/// Three cards through seats dora and mara on the lock (or all on B6),
/// walter on a build without it; the summed fork decisions.
fn old_seat_run(lock: bool) -> u64 {
    let b = Builder::new(&SEATS, 2);
    let mut net = Net::new(&b);
    for s in &mut net.seats {
        s.chain.lock.enabled_override = Some(lock);
    }
    net.seats[2].lock_force(false);
    let mut now = T;
    for title in ["one", "two", "three"] {
        let id = note(&mut net.seats[2], title);
        net.pump();
        net.seats[0].chain_sign_and_gossip_approval(id);
        net.seats[1].chain_sign_and_gossip_approval(id);
        net.pump();
        for _ in 0..20 {
            now += 5;
            net.tick(now);
        }
    }
    assert_eq!(net.heights(), vec![3, 3, 3], "every height decided");
    let heads = net.heads();
    assert!(heads.iter().all(|h| *h == heads[0]), "one head");
    verify_chain(&net.seats[0].chain.blocks).expect("an ordinary chain");
    net.seats.iter().map(|s| s.chain.fork_decisions).sum()
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
    dora.chain.lock.enabled_override = Some(true);
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

/// W1: an acceptance is a promise kept. A value this seat already
/// accepted may be chosen (its `LockAccepted` frames may all be lost); a
/// later withdraw or decline of the card must not stop this seat from
/// accepting it again at a higher ballot, or a forced value stalls the
/// height for good.
#[test]
fn an_accepted_value_is_accepted_again_after_a_withdraw_or_decline() {
    for undo in ["withdraw", "decline"] {
        let b = Builder::new(&SEATS, 2);
        let mut dora = seat("dora", &b);
        let id = note(&mut dora, "x");
        let change = dora.proposal_change(id).expect("change");
        let value = dora.lock_value(1, &change).expect("value");
        let base = dora.lock_slot().expect("slot").base;
        let accept = |st: &mut crate::State, seq: u64, round: u64| {
            wire(
                st,
                "mara",
                seq,
                WorkspaceEvent::LockAccept {
                    height: 1,
                    base: base.clone(),
                    round,
                    digest: value.digest.clone(),
                    need: 2,
                    id: ProposalId(id),
                    change: None,
                    forced: false,
                },
            );
        };
        let accepted = |st: &crate::State| {
            st.chain.lock.outbox.iter().filter(|e| matches!(e.body, WorkspaceEvent::LockAccepted { .. })).count()
        };
        accept(&mut dora, 1, 5);
        assert_eq!(accepted(&dora), 1, "{undo}: accepted at ballot 5");
        match undo {
            "withdraw" => {
                dora.cmd_withdraw(ProposalId(id), None).expect("withdrawn");
            }
            _ => {
                dora.cmd_decline(ProposalId(id), None).expect("declined");
            }
        }
        dora.chain.lock.outbox.clear();
        accept(&mut dora, 2, 9);
        assert_eq!(accepted(&dora), 1, "{undo}: the same value accepted again at ballot 9");
        // a value this seat never accepted still needs its approval
        let other = note(&mut dora, "y");
        let _ = other;
    }
}

/// S2 (i): walter accepted at height 1 (its `LockAccepted` reached mara),
/// then a downgraded walter signs another card freely there. A seat seen
/// with lock state is not free: mara keeps her promise, no release.
#[test]
fn a_seat_seen_locked_is_never_counted_free_there() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    let v = note(&mut mara, "v");
    let w = note(&mut mara, "w");
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: base.clone(), round: 1 });
    assert!(!mara.chain.lock.acceptor.may_sign_freely(1), "mara promised");
    let change_v = mara.proposal_change(v).expect("v");
    let digest = mara.lock_value(1, &change_v).expect("value").digest;
    let sig_v = identity_sign(b.key("walter"), &approval_bytes(&b.republic_id, 1, &change_v));
    wire(
        &mut mara,
        "walter",
        2,
        WorkspaceEvent::LockAccepted { height: 1, base, round: 1, leader: "dora".into(), digest, sig: sig_v },
    );
    let change_w = mara.proposal_change(w).expect("w");
    let sig_w = identity_sign(b.key("walter"), &approval_bytes(&b.republic_id, 1, &change_w));
    wire(&mut mara, "walter", 3, WorkspaceEvent::Approved { id: ProposalId(w), by: "walter".into(), height: 1, sig: sig_w });
    assert!(!mara.chain.lock.acceptor.signed_freely(1), "walter is not free at 1");
    assert!(!mara.chain.lock.acceptor.may_sign_freely(1), "the promise stands");
}

/// S2 (ii): an older build ran on this seat and signed freely at a height
/// where its lock file holds a promise. Back on this build the seat is
/// passive there: no free signature, no promise, no acceptance.
#[test]
fn own_free_signature_beside_lock_state_makes_the_height_passive() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base: base.clone(), round: 1 });
    let disk = mara.chain.lock.disk.clone().expect("the promise is on disk");
    // the downgraded build ignores the file and signs freely
    mara.lock_force(false);
    let id = note(&mut mara, "free");
    assert_eq!(own_sigs(&mara), 1, "a free signature at 1");
    // back on this build
    mara.lock_force(true);
    mara.load_chain_lock(Ok(Some(disk)), true);
    assert!(mara.chain.lock.passive.contains(&1), "passive at 1");
    mara.chain.lock.outbox.clear();
    assert!(!mara.lock_sign_freely(1));
    wire(&mut mara, "walter", 2, WorkspaceEvent::LockPrepare { height: 1, base, round: 7 });
    assert!(sent_kinds(&mara).is_empty(), "no promise at a passive height: {:?}", sent_kinds(&mara));
    let _ = id;
}

/// W2: a card every seat would refuse now (a relay change while the chain
/// is unsettled) does not starve the height: the leader picks the next
/// ready card.
#[test]
fn a_card_held_by_a_gate_does_not_starve_the_height() {
    let mut b = Builder::new(&SEATS, 2);
    b.commit_applied(1, &["dora", "mara"]);
    let mut dora = seat("dora", &b);
    let relays = propose(&mut dora, Surface::Organization, json!({ "op": "set_relays", "value": "wss://relay.example" }));
    let page = note(&mut dora, "page");
    assert!(relays < page);
    dora.lock_note_intent(relays, "mara");
    dora.lock_note_intent(page, "mara");
    dora.chain.fork_candidates.insert(9, b.blocks[1].clone());
    let slot = dora.lock_slot().expect("slot");
    let chosen = dora.lock_choose(&slot, None).map(|(id, _, _)| id);
    assert_eq!(chosen, Some(page), "the relay card waits for the heal, the page does not");
}

/// W4/S4: a leader that cannot complete phase 1 re-sends its `Prepare` at
/// a doubling interval, not every 15 s for good.
#[test]
fn phase_one_backs_off() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let mut now = T;
    dora.presence.clock_override = Some(now);
    dora.lock_start(0);
    for _ in 0..360 {
        now += 10;
        hear(&mut dora, now, &SEATS);
        dora.lock_tick();
    }
    let prepares = sent_kinds(&dora).iter().filter(|k| **k == "prepare").count();
    assert!(prepares < 60, "{prepares} prepares in an hour");
    assert!(prepares > 3, "still asking: {prepares}");
}

/// S3: a forced value this leader would withhold (its card withdrawn here)
/// is not proposed on the first Q1 promises while seats are unheard; a
/// fuller hearing frees the slot.
#[test]
fn a_forced_value_waits_for_a_fuller_hearing() {
    let names = ["dora", "erik", "mara", "nils", "walter"];
    let b = Builder::new(&names, 3);
    let mut dora = genesis_seat("dora", &b, b.blocks.clone());
    dora.chain.lock.enabled_override = Some(true);
    dora.lock_force(true);
    hear(&mut dora, T, &names);
    let v = note(&mut dora, "v");
    let change = dora.proposal_change(v).expect("change");
    let digest = dora.lock_value(1, &change).expect("value").digest;
    dora.cmd_withdraw(ProposalId(v), None).expect("withdrawn");
    dora.chain.lock.outbox.clear();
    dora.lock_start(0);
    let (base, round) = {
        let run = dora.chain.lock.run.as_ref().expect("dora leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    let promise = |st: &mut crate::State, from: &str, seq: u64, holds: bool| {
        let accepted = holds.then(|| molt_core::chain::LockAcceptedValue {
            round: 1,
            leader: "mara".into(),
            need: 3,
            digest: digest.clone(),
            id: v,
            change: None,
        });
        wire(st, from, seq, WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round, leader: "dora".into(), accepted });
    };
    promise(&mut dora, "erik", 1, true);
    promise(&mut dora, "nils", 2, false);
    assert!(!sent_kinds(&dora).contains(&"accept"), "three of five heard: v may be chosen, dora withholds it, she waits");
    promise(&mut dora, "mara", 3, false);
    promise(&mut dora, "walter", 4, false);
    assert!(!sent_kinds(&dora).contains(&"accept"), "all heard: v was not chosen, nothing else is ready");
    assert!(dora.chain.lock.run.as_ref().is_some_and(|r| r.leader.forced().is_none()), "the slot is free");
}

/// W6: free marks made at open that cannot be written leave the seat
/// amnesiac, not signing at those heights from memory only.
#[test]
fn a_failed_write_at_open_means_amnesia() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    mara.chain.lock.fail_writes = true;
    mara.load_chain_lock(Ok(None), false);
    assert_eq!(mara.chain.lock.amnesia, Some(None));
}

/// An idle leader - its promises in hand, nothing to propose - sends
/// nothing: no re-sent `Prepare`, no new ballot, for an hour.
#[test]
fn an_idle_leader_is_silent() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let mut now = T;
    dora.presence.clock_override = Some(now);
    dora.lock_start(0);
    let (base, round) = {
        let run = dora.chain.lock.run.as_ref().expect("dora leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    wire(&mut dora, "mara", 1, WorkspaceEvent::LockPromise { height: 1, base, round, leader: "dora".into(), accepted: None });
    assert!(dora.chain.lock.run.as_ref().is_some_and(|r| r.leader.ready()), "phase 1 complete");
    dora.chain.lock.outbox.clear();
    for _ in 0..360 {
        now += 10;
        hear(&mut dora, now, &SEATS);
        dora.lock_tick();
    }
    assert!(sent_kinds(&dora).is_empty(), "an idle leader sent {:?}", sent_kinds(&dora));
}

/// A seat yielded to the ballot of a seat that is then lost and restored:
/// its backoff waits on a ballot nobody will finish. Once the restore is
/// registered the backoff is dropped and the next seat in line leads.
#[test]
fn a_backoff_after_yielding_to_a_restored_seat_is_dropped() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    hear(&mut dora, T, &["dora", "mara"]);
    dora.lock_start(0);
    let base = dora.lock_slot().expect("slot").base;
    // walter, about to be lost, took the height over at a higher ballot
    wire(&mut dora, "walter", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 9 });
    assert!(dora.chain.lock.run.is_none(), "dora yielded");
    assert!(dora.chain.lock.wait.as_ref().is_some_and(|w| w.attempt > 0), "and backs off");
    // walter's device is gone: the restore gate does not wait for it
    dora.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let ticket = "ticket";
    let kp_hex = kp_hex_for(&b, "walter");
    let proof = crate::make_seat_proof(b.key("walter"), ticket, &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "walter", "", &[], 0, &"cc".repeat(32));
    dora.verify_and_propose_restore("walter", &b.pk("walter"), &kp_hex, ticket, &proof, "", &[], &consent, "")
        .expect("proposed");
    dora.lock_tick();
    let run = dora.chain.lock.run.as_ref().expect("dora leads again at once");
    assert!(run.leader.ballot.round > 9, "above walter's ballot");
}

/// Review R3: where the chain does not run the lock, a peer's free
/// signature writes no lock state - no file, no manifest bump, no marker.
#[test]
fn a_pre_lock_republic_writes_no_lock_state() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    mara.chain.lock.enabled_override = Some(false);
    for m in ["walter", "dora"] {
        mara.lock_note_free(m, 1);
    }
    mara.lock_tick();
    assert!(mara.chain.lock.disk.is_none(), "nothing persisted");
    assert!(!mara.chain.lock.acceptor.signed_freely(1), "no free mark");
}

/// Review R2: a restore withdrawn after ONE survivor accepted it is not
/// forced on a leader who heard every survivor - the lost seat is still
/// the subject of the restore the lock holds, so it is no unheard voice.
#[test]
fn a_withdrawn_restore_held_by_one_survivor_is_not_forced() {
    const FIVE: [&str; 5] = ["dora", "mara", "olga", "paul", "walter"];
    let b = Builder::new(&FIVE, 3);
    let mut olga = genesis_seat("olga", &b, b.blocks.clone());
    olga.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let kp_hex = kp_hex_for(&b, "walter");
    let proof = crate::make_seat_proof(b.key("walter"), "ticket", &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "walter", "", &[], 0, &"cc".repeat(32));
    let id = olga
        .verify_and_propose_restore("walter", &b.pk("walter"), &kp_hex, "ticket", &proof, "", &[], &consent, "")
        .expect("proposed");
    let change = olga.chain.proposal_changes.get(&id).cloned().expect("the restore");

    // the card is gone at dora (withdrawn); mara alone accepted it earlier
    let mut dora = genesis_seat("dora", &b, b.blocks.clone());
    dora.chain.lock.enabled_override = Some(true);
    dora.lock_force(true);
    hear(&mut dora, T, &FIVE[..4]);
    let value = dora.lock_value(1, &change).expect("a value");
    dora.lock_start(0);
    let (base, round) = {
        let run = dora.chain.lock.run.as_ref().expect("dora leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    let held = molt_core::chain::LockAcceptedValue {
        round: 1,
        leader: "olga".into(),
        digest: value.digest.clone(),
        need: u64::try_from(value.need).expect("small"),
        id,
        change: Some(change),
    };
    for (seq, (from, accepted)) in [("mara", Some(held)), ("olga", None), ("paul", None)].into_iter().enumerate() {
        let promise = WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round, leader: "dora".into(), accepted };
        wire(&mut dora, from, u64::try_from(seq).expect("small") + 1, promise);
    }
    let run = dora.chain.lock.run.as_ref().expect("still leading");
    assert!(run.leader.ready(), "every survivor promised");
    assert_eq!(run.leader.forced(), None, "one holder and the restored seat cannot have chosen it");
}

/// Review R4: 3-of-4, o on an older build signs w freely; b only
/// promised, then released soundly (one free signer kills the lock for a
/// seat holding nothing) and signed v freely. a, who accepted v, counts
/// b free - a cascaded release - and does not stall. A seat seen
/// ACCEPTING is still never counted (S2 (i)).
#[test]
fn seats_seen_promising_that_released_soundly_count_free() {
    const FOUR: [&str; 4] = ["a", "b", "c", "o"];
    let b = Builder::new(&FOUR, 3);
    let mut a = genesis_seat("a", &b, b.blocks.clone());
    a.chain.lock.enabled_override = Some(true);
    a.lock_force(true);
    hear(&mut a, T, &FOUR);
    let v = note(&mut a, "v");
    let w = note(&mut a, "w");
    for (seq, from) in [(1, "b"), (2, "c")] {
        wire(&mut a, from, seq, WorkspaceEvent::Approved { id: ProposalId(v), by: from.into(), height: 1, sig: String::new() });
    }
    a.lock_start(0);
    let (base, round) = {
        let run = a.chain.lock.run.as_ref().expect("a leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    for (seq, from) in [(3, "b"), (4, "c")] {
        let promise = WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round, leader: "a".into(), accepted: None };
        wire(&mut a, from, seq, promise);
    }
    assert!(a.chain.lock.acceptor.holds_acceptance(1), "a accepted its value");
    let change_v = a.proposal_change(v).expect("v");
    let change_w = a.proposal_change(w).expect("w");
    let free = |a: &mut crate::State, from: &str, seq: u64, id: u64, change: &ChainChange| {
        let sig = identity_sign(b.key(from), &approval_bytes(&b.republic_id, 1, change));
        wire(a, from, seq, WorkspaceEvent::Approved { id: ProposalId(id), by: from.into(), height: 1, sig });
    };
    free(&mut a, "o", 5, w, &change_w);
    assert!(!a.chain.lock.acceptor.signed_freely(1), "o alone: a quorum with a's acceptance is still possible");
    free(&mut a, "b", 6, v, &change_v);
    assert!(a.chain.lock.acceptor.signed_freely(1), "o and b free leave two lock seats: a releases");
}

/// Review R5: a card every acceptor withholds (a wallet init while the
/// daemon is down) goes stale; the next ballot proposes the next ready
/// card instead of the same one again. Once every ready card went stale
/// the first comes round again.
#[test]
fn a_stale_proposal_rotates_to_the_next_card() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let first = note(&mut dora, "first");
    let second = note(&mut dora, "second");
    dora.lock_note_intent(first, "mara");
    dora.lock_note_intent(second, "mara");
    // dora's own local check withholds too (no acceptance anywhere)
    dora.chain.own_approvals.clear();
    let mut now = T;
    let mut seq = 0;
    let mut proposed = Vec::new();
    for attempt in 0..3 {
        if attempt == 0 {
            dora.lock_start(0);
        }
        let (base, round) = {
            let run = dora.chain.lock.run.as_ref().expect("dora leads");
            (run.leader.slot.base.clone(), run.leader.ballot.round)
        };
        seq += 1;
        wire(&mut dora, "mara", seq, WorkspaceEvent::LockPromise { height: 1, base, round, leader: "dora".into(), accepted: None });
        proposed.push(dora.chain.lock.run.as_ref().and_then(|r| r.value.as_ref()).map(|(id, _)| *id));
        // mara withholds: the run goes stale and the next ballot starts
        let started = dora.chain.lock.run.as_ref().map(|r| r.leader.ballot.round);
        while dora.chain.lock.run.as_ref().map(|r| r.leader.ballot.round) == started {
            now += 10;
            hear(&mut dora, now, &SEATS);
            dora.lock_tick();
        }
    }
    assert_eq!(proposed, vec![Some(first), Some(second), Some(first)]);
}

/// Review R6: mara signed a card freely at 1 on a lineage without the
/// enabling block, then re-based onto one that runs the lock. Her free
/// signature is marked before any lock reply: she promises nothing there.
#[test]
fn a_free_signature_from_before_the_lock_is_marked_on_the_head_move() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    mara.chain.lock.enabled_override = Some(false);
    note(&mut mara, "y");
    assert_eq!(own_sigs(&mara), 1, "a free signature at 1, no mark");
    assert!(!mara.chain.lock.acceptor.signed_freely(1));
    mara.chain.lock.enabled_override = Some(true);
    mara.lock_on_head_moved();
    assert!(mara.chain.lock.acceptor.signed_freely(1), "marked free");
    mara.chain.lock.outbox.clear();
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "walter", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 3 });
    assert!(!sent_kinds(&mara).contains(&"promise"), "no promise: {:?}", sent_kinds(&mara));
}

/// Review R7: an own card withdrawn after the lock chose it seals anyway;
/// the proposer is told, the record reads applied and withdrawn.
#[test]
fn an_own_card_sealed_after_its_withdrawal_is_reported() {
    let mut b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let id = note(&mut dora, "x");
    dora.cmd_withdraw(ProposalId(id), None).expect("withdrawn");
    let block = b.commit_card(&dora, id, &["mara", "walter"]);
    dora.receive_block_from("mara", block);
    let p = dora.proposals.get(&id).expect("record");
    assert_eq!(p.state, ProposalState::Applied);
    assert!(p.withdrawn);
    assert_eq!(dora.session.notice, format!("sealed-after-withdrawal:{id}"));
}

/// Review R8: an intent that outran its card survives a head move; a
/// retired card's intents do not.
#[test]
fn an_intent_before_its_card_survives_a_head_move() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let retired = note(&mut dora, "gone");
    dora.cmd_withdraw(ProposalId(retired), None).expect("withdrawn");
    dora.lock_note_intent(retired, "mara");
    dora.lock_note_intent(77, "mara");
    dora.lock_on_head_moved();
    assert!(dora.chain.lock.intents.contains_key(&77), "the card is still on its way");
    assert!(!dora.chain.lock.intents.contains_key(&retired), "a retired card's intent goes");
}

/// Review R9: the enabling vote's refusal names the seat to chase; and
/// the lock, once enabled, cannot be voted off (like the vault, a value
/// need not name it - the union keeps it).
#[test]
fn the_enabling_refusal_names_the_seat_and_the_lock_stays_on() {
    use molt_core::chain::{CHAIN_LOCK_CAP, CHAIN_LOCK_FEATURE};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&SEATS, 2);
    let (mut mara, _tmp, _dir) = crate::tests::support::stored_chain_signer(&b, "mara", &SEATS);
    mara.lock_force(true);
    let enable = json!({ "op": "set_features", "value": format!("{CHAIN_LOCK_FEATURE} memory") });
    let caps = |st: &mut crate::State, seat: &str, with: bool| {
        let list = if with { vec![CHAIN_LOCK_CAP.to_string()] } else { Vec::new() };
        st.active.as_mut().expect("active").prefs.seat_caps.insert(seat.to_string(), list);
    };
    caps(&mut mara, "dora", true);
    let err = mara.cmd_propose(Surface::Organization, enable.clone()).expect_err("refused");
    assert_eq!(err.to_string(), "bad payload: walter has not announced");
    caps(&mut mara, "walter", false);
    let err = mara.cmd_propose(Surface::Organization, enable).expect_err("refused");
    assert_eq!(err.to_string(), "bad payload: walter runs a build without the lock");

    let b = Builder::new_with_features(&SEATS, 2, &["memory", CHAIN_LOCK_FEATURE], false);
    let mut dora = genesis_seat("dora", &b, b.blocks.clone());
    let id = propose(&mut dora, Surface::Organization, json!({ "op": "set_features", "value": "memory quests" }));
    assert!(dora.proposals.contains_key(&id));
    assert!(dora.lock_enabled(), "still on");
}

/// Review T1: a withdrawn restore of z still held by the lock excuses z
/// for THAT restore only. z's old device is back and accepted w (an
/// ordinary change) with d and a; a leader hearing c, b (holding the
/// restore) and a (holding w) must count z as a voice that may hold w:
/// w is forced. Heard as free for every value, z let a second value in.
#[test]
fn a_restore_subject_is_excused_only_for_its_own_restore() {
    const FIVE: [&str; 5] = ["a", "b", "c", "d", "z"];
    let b = Builder::new(&FIVE, 3);
    let mut x = genesis_seat("a", &b, b.blocks.clone());
    x.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let kp_hex = kp_hex_for(&b, "z");
    let proof = crate::make_seat_proof(b.key("z"), "ticket", &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "z", "", &[], 0, &"cc".repeat(32));
    let rid_restore = x
        .verify_and_propose_restore("z", &b.pk("z"), &kp_hex, "ticket", &proof, "", &[], &consent, "")
        .expect("proposed");
    let restore = x.chain.proposal_changes.get(&rid_restore).cloned().expect("the restore");
    let w_id = note(&mut x, "w");
    let w = x.proposal_change(w_id).expect("w");

    let mut c = genesis_seat("c", &b, b.blocks.clone());
    c.chain.lock.enabled_override = Some(true);
    c.lock_force(true);
    hear(&mut c, T, &FIVE);
    let r_value = c.lock_value(1, &restore).expect("restore value");
    let w_value = c.lock_value(1, &w).expect("w value");
    // the withdrawn restore is still known here (its LockAccept came by)
    c.chain.lock.known.insert((1, r_value.digest.clone()), (rid_restore, restore.clone()));
    assert!(c.lock_restore_subjects().contains("z"));
    c.lock_start(0);
    let (base, round) = {
        let run = c.chain.lock.run.as_ref().expect("c leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    let held = |value: &Value, ballot: u64, leader: &str, id: u64, change: Option<ChainChange>| molt_core::chain::LockAcceptedValue {
        round: ballot,
        leader: leader.into(),
        digest: value.digest.clone(),
        need: u64::try_from(value.need).expect("small"),
        id,
        change,
    };
    let promises = [
        ("b", held(&r_value, 1, "a", rid_restore, Some(restore.clone()))),
        ("a", held(&w_value, 2, "d", w_id, None)),
    ];
    for (seq, (from, accepted)) in promises.into_iter().enumerate() {
        let promise = WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round, leader: "c".into(), accepted: Some(accepted) };
        wire(&mut c, from, u64::try_from(seq).expect("small") + 1, promise);
    }
    let run = c.chain.lock.run.as_ref().expect("still leading");
    assert!(run.leader.ready(), "c, b and a promised");
    assert_eq!(run.leader.forced(), Some(w_value), "z may hold w: w is forced");
}

/// Review T2: once every ready card went stale they take turns - the one
/// stale longest first - so a card blocked for good does not take every
/// later ballot from one that went stale only once.
#[test]
fn stale_cards_take_turns() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    let first = note(&mut dora, "first");
    let second = note(&mut dora, "second");
    dora.lock_note_intent(first, "mara");
    dora.lock_note_intent(second, "mara");
    dora.chain.own_approvals.clear();
    let mut now = T;
    let mut seq = 0;
    let mut proposed = Vec::new();
    dora.lock_start(0);
    for _ in 0..4 {
        let (base, round) = {
            let run = dora.chain.lock.run.as_ref().expect("dora leads");
            (run.leader.slot.base.clone(), run.leader.ballot.round)
        };
        seq += 1;
        wire(&mut dora, "mara", seq, WorkspaceEvent::LockPromise { height: 1, base, round, leader: "dora".into(), accepted: None });
        proposed.push(dora.chain.lock.run.as_ref().and_then(|r| r.value.as_ref()).map(|(id, _)| *id));
        let started = dora.chain.lock.run.as_ref().map(|r| r.leader.ballot.round);
        while dora.chain.lock.run.as_ref().map(|r| r.leader.ballot.round) == started {
            now += 10;
            hear(&mut dora, now, &SEATS);
            dora.lock_tick();
        }
    }
    assert_eq!(proposed, vec![Some(first), Some(second), Some(first), Some(second)]);
}

/// H1: walter is lost; mara alone accepted dora's card w, then dora
/// withdrew it. w is forced (walter may hold it). Any promiser accepts a
/// forced value, so the height decides: w seals and its proposer is told
/// - also a seat that never approved w completes it.
#[test]
fn a_forced_withdrawn_value_completes_with_any_promiser() {
    let b = Builder::new(&SEATS, 2);
    let mut dora = seat("dora", &b);
    hear(&mut dora, T, &["dora", "mara"]);
    let w = note(&mut dora, "w");
    dora.lock_note_intent(w, "mara");
    let change = dora.proposal_change(w).expect("w");
    let value = dora.lock_value(1, &change).expect("value");
    dora.cmd_withdraw(ProposalId(w), None).expect("withdrawn");
    dora.chain.lock.outbox.clear();
    dora.lock_start(0);
    let (base, round) = {
        let run = dora.chain.lock.run.as_ref().expect("dora leads");
        (run.leader.slot.base.clone(), run.leader.ballot.round)
    };
    let held = molt_core::chain::LockAcceptedValue {
        round: 1,
        leader: "walter".into(),
        digest: value.digest.clone(),
        need: u64::try_from(value.need).expect("small"),
        id: w,
        change: None,
    };
    wire(&mut dora, "mara", 1, WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round, leader: "dora".into(), accepted: Some(held) });
    // S3: one interval for a fuller hearing, walter stays silent
    dora.presence.clock_override = Some(T + 60);
    hear(&mut dora, T + 60, &["dora", "mara"]);
    dora.lock_tick();
    let forced_out = dora.chain.lock.outbox.iter().any(|e| matches!(&e.body, WorkspaceEvent::LockAccept { forced: true, digest, .. } if *digest == value.digest));
    assert!(forced_out, "w proposed as forced: {:?}", sent_kinds(&dora));
    assert!(sent_kinds(&dora).contains(&"accepted"), "dora's intent stood: she accepts the withdrawn w");
    let sig = identity_sign(b.key("mara"), &approval_bytes(&b.republic_id, 1, &change));
    wire(&mut dora, "mara", 2, WorkspaceEvent::LockAccepted { height: 1, base: base.clone(), round, leader: "dora".into(), digest: value.digest.clone(), sig });
    assert_eq!(dora.chain.head.as_ref().map(|h| h.height), Some(1), "the height decided");
    assert_eq!(dora.session.notice, format!("sealed-after-withdrawal:{w}"));

    // walter never approved w: a forced Accept is completed all the same
    let mut walter = seat("walter", &b);
    let mut x = seat("dora", &b);
    let w2 = note(&mut x, "w");
    let change2 = x.proposal_change(w2).expect("w");
    let ChainChange::Applied { payload, .. } = change2.clone() else { panic!("applied") };
    assert!(walter.receive_proposed(w2, Surface::Memory, payload, "dora"));
    wire(&mut walter, "dora", 1, WorkspaceEvent::Withdrawn { id: ProposalId(w2), by: "dora".into() });
    let slot = walter.lock_slot().expect("slot");
    let v2 = walter.lock_value(1, &change2).expect("value");
    walter.chain.lock.outbox.clear();
    wire(
        &mut walter,
        "dora",
        2,
        WorkspaceEvent::LockAccept { height: 1, base: slot.base, round: 5, digest: v2.digest, need: 2, id: ProposalId(w2), change: None, forced: true },
    );
    assert!(sent_kinds(&walter).contains(&"accepted"), "forced: accepted without an intent: {:?}", sent_kinds(&walter));
}

/// Review T3: a seat imported in pre-lock times carries the lock marker
/// but no lock file. Opened while its chain runs no lock, the marker is
/// cleared; after the enabling vote it reopens as a fresh acceptor, not
/// amnesiac, and promises - an n-of-n republic is not halted.
#[test]
fn a_marker_the_chain_does_not_back_is_cleared_at_open() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&SEATS, 3);
    let (mut mara, _tmp, _dir) = crate::tests::support::stored_chain_signer(&b, "mara", &SEATS);
    mara.lock_force(true);
    mara.active.as_mut().expect("active").prefs.chain_lock = true;
    mara.chain.lock.enabled_override = Some(false);
    mara.load_chain_lock(Ok(None), true);
    assert!(!mara.active.as_ref().expect("active").prefs.chain_lock, "cleared: the lock never wrote here");
    // the enabling vote sealed; the seat closed before its first lock write
    mara.chain.lock.enabled_override = Some(true);
    let marker = mara.active.as_ref().expect("active").prefs.chain_lock;
    mara.load_chain_lock(Ok(None), marker);
    assert_eq!(mara.chain.lock.amnesia, None, "a fresh acceptor, not amnesiac");
    hear(&mut mara, T, &SEATS);
    let base = mara.lock_slot().expect("slot").base;
    mara.chain.lock.outbox.clear();
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 1 });
    assert!(sent_kinds(&mara).contains(&"promise"), "{:?}", sent_kinds(&mara));
}

/// The switch off (the release build): lock frames from a peer change
/// nothing - no promise, no state, no file - and the enabling vote is
/// refused at both doors with the reason.
#[test]
fn a_dark_build_ignores_lock_frames_and_refuses_the_vote() {
    use molt_core::chain::CHAIN_LOCK_FEATURE;
    let b = Builder::new(&SEATS, 2);
    let mut mara = genesis_seat("mara", &b, b.blocks.clone());
    assert!(!mara.lock_cap_on(), "dark by default");
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "dora", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 1 });
    assert!(sent_kinds(&mara).is_empty(), "nothing sent");
    assert!(mara.chain.lock.acceptor.may_sign_freely(1), "no lock state");
    assert!(mara.chain.lock.disk.is_none(), "nothing written");
    let enable = json!({ "op": "set_features", "value": format!("{CHAIN_LOCK_FEATURE} memory") });
    let err = mara.cmd_propose(Surface::Organization, enable.clone()).expect_err("refused at propose");
    assert!(err.to_string().contains(LOCK_NOT_AVAILABLE), "{err}");
    let mut dora = genesis_seat("dora", &b, b.blocks.clone());
    dora.lock_force(true);
    dora.chain.lock.caps_override = Some(true);
    let id = propose(&mut dora, Surface::Organization, enable);
    let ChainChange::Applied { payload, .. } = dora.proposal_change(id).expect("change") else { panic!("org") };
    assert!(mara.receive_proposed(id, Surface::Organization, payload, "dora"));
    assert_eq!(mara.sign_blocker(id).as_deref(), Some(LOCK_NOT_AVAILABLE), "refused at approve");
}

/// The switch off on a chain that carries the key (a dev build founded
/// it): this build signs nothing there and says why; it still reads.
#[test]
fn a_dark_build_signs_nothing_on_a_locked_chain() {
    use molt_core::chain::CHAIN_LOCK_FEATURE;
    let b = Builder::new_with_features(&SEATS, 2, &[CHAIN_LOCK_FEATURE, "memory"], false);
    let mut mara = genesis_seat("mara", &b, b.blocks.clone());
    let id = note(&mut mara, "x");
    assert_eq!(mara.sign_blocker(id).as_deref(), Some(LOCK_CHAIN_UNSUPPORTED));
    assert_eq!(own_sigs(&mara), 0, "no free signature beside the lock");
    assert!(sent_kinds(&mara).is_empty(), "no lock frame: {:?}", sent_kinds(&mara));
    assert_eq!(mara.lock_status().2, LOCK_CHAIN_UNSUPPORTED, "the status says why");
}

/// dora leads height 1 and accepts her card with mara's promise; the
/// block for it then arrives. `(dora, slot, value)`.
fn accepted_then_sealed(b: &mut Builder, fail_chain: bool) -> (crate::State, Slot, Value) {
    let mut dora = seat("dora", b);
    let id = note(&mut dora, "x");
    dora.lock_note_intent(id, "mara");
    dora.lock_start(0);
    let (slot, round) = {
        let run = dora.chain.lock.run.as_ref().expect("dora leads");
        (run.leader.slot.clone(), run.leader.ballot.round)
    };
    wire(&mut dora, "mara", 1, WorkspaceEvent::LockPromise { height: 1, base: slot.base.clone(), round, leader: "dora".into(), accepted: None });
    let value = dora.lock_value(1, &dora.proposal_change(id).expect("change")).expect("value");
    assert!(dora.chain.lock.acceptor.holds(&slot, &value), "dora accepted x at 1");
    dora.chain.lock.fail_chain_writes = fail_chain;
    let block = b.commit_card(&dora, id, &["dora", "mara"]);
    dora.receive_block_from("mara", block);
    assert_eq!(dora.chain.head.as_ref().map(|h| h.height), Some(1));
    (dora, slot, value)
}

/// B-F1: the lock never forgets a slot the DURABLE chain does not cover:
/// the block at 1 did not reach the disk, so a crash would reopen at 0 -
/// the acceptance at (1, L) must still be on record.
#[test]
fn a_slot_above_the_durable_chain_is_never_forgotten() {
    let mut b = Builder::new(&SEATS, 2);
    let (dora, slot, value) = accepted_then_sealed(&mut b, true);
    assert!(dora.chain.lock.acceptor.holds(&slot, &value), "kept: the chain write failed");
}

/// D-F1: a slot below the head is kept for a window - a reorg may lead
/// back onto this lineage, and the seat must still report what it
/// accepted there.
#[test]
fn a_slot_below_the_head_is_kept_for_a_reorg_back() {
    let mut b = Builder::new(&SEATS, 2);
    let (dora, slot, value) = accepted_then_sealed(&mut b, false);
    assert!(dora.chain.lock.acceptor.holds(&slot, &value), "kept inside the window");
}

/// B-F1 at open: a lock file settled past the chain on disk (whatever
/// the cause) means the seat may have forgotten something: amnesia.
#[test]
fn a_lock_file_ahead_of_the_chain_means_amnesia() {
    let b = Builder::new(&SEATS, 2);
    let mara = seat("mara", &b);
    let mut file: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&LockFile::default()).expect("json")).expect("value");
    file["floor"] = serde_json::json!(5);
    let mut mara = mara;
    mara.load_chain_lock(Ok(Some(serde_json::to_vec(&file).expect("bytes"))), true);
    assert_eq!(mara.chain.lock.amnesia, Some(None), "settled past the chain: amnesia");
}

/// H2: a consented restore needs m acceptances where m < n - phase 1's
/// n - m + 1 then meets them even when the subject's old device promises
/// (with m - 1 a revived device and one survivor sealed a second value).
/// m - 1 only for m = n, where the consent stands in for the one seat
/// every other value needs.
#[test]
fn a_restore_needs_m_where_m_is_below_n() {
    for (n, m, need) in [(3, 2, 2), (5, 3, 3), (7, 4, 4), (3, 3, 2), (2, 2, 1)] {
        assert_eq!(Quorum { n, m }.restore_need(), need, "{m}-of-{n}");
    }
    let b = Builder::new(&SEATS, 2);
    let mut x = seat("dora", &b);
    x.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let kp_hex = kp_hex_for(&b, "walter");
    let proof = crate::make_seat_proof(b.key("walter"), "ticket", &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "walter", "", &[], 0, &"cc".repeat(32));
    let id = x
        .verify_and_propose_restore("walter", &b.pk("walter"), &kp_hex, "ticket", &proof, "", &[], &consent, "")
        .expect("proposed");
    let change = x.chain.proposal_changes.get(&id).cloned().expect("the restore");
    assert_eq!(x.lock_value(1, &change).expect("value").need, 2, "2-of-3: both survivors");
}

/// H2: a seat that is the subject of a restore in its own view never
/// leads - its old device would count its own promise.
#[test]
fn a_restore_subject_never_leads() {
    let b = Builder::new(&SEATS, 2);
    let mut walter = seat("walter", &b);
    let mut x = seat("dora", &b);
    x.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let kp_hex = kp_hex_for(&b, "walter");
    let proof = crate::make_seat_proof(b.key("walter"), "ticket", &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "walter", "", &[], 0, &"cc".repeat(32));
    let id = x
        .verify_and_propose_restore("walter", &b.pk("walter"), &kp_hex, "ticket", &proof, "", &[], &consent, "")
        .expect("proposed");
    let change = x.chain.proposal_changes.get(&id).cloned().expect("the restore");
    let value = walter.lock_value(1, &change).expect("value");
    walter.chain.lock.known.insert((1, value.digest), (id, change));
    assert!(walter.lock_restore_subjects().contains("walter"), "walter sees his own restore");
    walter.lock_start(0);
    assert!(walter.chain.lock.run.is_none(), "the subject does not lead");
}

/// A consented restore of `member` proposed on `x`: `(id, change)`.
fn restore_card(b: &Builder, x: &mut crate::State, member: &str) -> (u64, ChainChange) {
    x.seat_active.store(0, std::sync::atomic::Ordering::SeqCst);
    let rid = b.republic_id.clone();
    let kp_hex = kp_hex_for(b, member);
    let proof = crate::make_seat_proof(b.key(member), "ticket", &kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(b, member, "", &[], 0, &"cc".repeat(32));
    let id = x
        .verify_and_propose_restore(member, &b.pk(member), &kp_hex, "ticket", &proof, "", &[], &consent, "")
        .expect("proposed");
    let change = x.chain.proposal_changes.get(&id).cloned().expect("the restore");
    (id, change)
}

/// A-F1: an acceptor takes no `Prepare` from a seat that is a restore's
/// subject here - in 3-of-3 its old device would lead on its own promise
/// and seal a second value beside the restore.
#[test]
fn an_acceptor_refuses_a_restore_subjects_prepare() {
    let b = Builder::new(&SEATS, 3);
    let mut mara = seat("mara", &b);
    let (_, _) = restore_card(&b, &mut mara, "walter");
    assert!(mara.lock_restore_subjects().contains("walter"));
    mara.chain.lock.outbox.clear();
    let base = mara.lock_slot().expect("slot").base;
    wire(&mut mara, "walter", 1, WorkspaceEvent::LockPrepare { height: 1, base, round: 5 });
    assert!(!sent_kinds(&mara).contains(&"promise"), "no promise to the subject: {:?}", sent_kinds(&mara));
    let slot = mara.lock_slot().expect("slot");
    assert!(mara.chain.lock.acceptor.promised(&slot).is_none(), "no promise on record");
}

/// C-HIGH-1: a declined (or withdrawn) restore card no longer makes its
/// seat a subject - the seat is alive and its promises count.
#[test]
fn a_declined_restore_card_makes_no_subject() {
    let b = Builder::new(&SEATS, 3);
    let mut mara = seat("mara", &b);
    let (id, _) = restore_card(&b, &mut mara, "walter");
    assert!(mara.lock_restore_subjects().contains("walter"), "an open card");
    mara.proposals.get_mut(&id).expect("record").state = ProposalState::Rejected;
    assert!(!mara.lock_restore_subjects().contains("walter"), "a declined card does not take a live seat out");
}

/// A-F3: lock state at a height - on any lineage - forbids a free
/// signature there, also on a lineage without the enabling block: the
/// signature is base-agnostic and would count free on the locked one.
#[test]
fn lock_state_on_another_lineage_forbids_a_free_signature() {
    let b = Builder::new(&SEATS, 2);
    let mut mara = seat("mara", &b);
    let other = Slot { height: 1, base: "another-lineage".to_string() };
    let _ = mara.chain.lock.acceptor.prepare(&other, &Ballot { round: 1, leader: "dora".to_string() });
    mara.chain.lock.enabled_override = Some(false);
    assert!(!mara.lock_sign_freely(1), "a promise at 1 elsewhere: no free signature at 1");
}

/// A-F4: a legacy m = 1 chain does not enable Strict Order Mode - phase 1
/// would need every seat.
#[test]
fn the_mode_needs_two_approvals_per_decision() {
    use molt_core::chain::CHAIN_LOCK_FEATURE;
    let b = Builder::new(&SEATS, 1);
    let mut mara = seat("mara", &b);
    mara.chain.lock.caps_override = Some(true);
    let enable = json!({ "op": "set_features", "value": format!("{CHAIN_LOCK_FEATURE} memory") });
    let err = mara.cmd_propose(Surface::Organization, enable).expect_err("refused");
    assert!(err.to_string().contains(LOCK_NEEDS_TWO), "{err}");
}
