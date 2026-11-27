// SPDX-License-Identifier: GPL-3.0-or-later

//! The automatic evidence exchange (`docs/chain/chain_rescue_plan.md`
//! §5a) on live seats, without the relay: the parts go in as the seat
//! inbox hands them over.

use super::branch_tests::fork;
use super::test_support::genesis_seat;
use super::*;
use molt_core::{HealLoop, MoltError};

const PK: [(&str, &str); 3] = [("a", "a1"), ("b", "b2"), ("c", "c3")];

fn pk(m: &str) -> String {
    PK.iter().find(|(n, _)| *n == m).map(|(_, p)| p.repeat(32)).expect("a seat")
}

/// Every seat addresses every other by its own key.
fn keyed(st: &mut State) {
    for (m, _) in PK {
        st.chain.anchors.insert(m.to_string(), pk(m));
    }
}

/// `from`'s offer to `to`, delivered part by part.
fn deliver(from: &State, to: &mut State, peer: &str) -> String {
    let (offer, parts, _) = from.evidence_for(peer).expect("an offer");
    let total = u32::try_from(parts.len()).expect("few");
    keyed(to);
    for (i, data) in parts.into_iter().enumerate() {
        let index = u32::try_from(i).expect("few");
        to.cmd_net_evidence_part(&pk(&from.member()), &from.republic_id(), &offer, (index, total), data).expect("ack");
    }
    offer
}

fn head(st: &State) -> u64 {
    st.chain.head.as_ref().map_or(0, |h| h.height)
}

/// A seat that is merely behind on the same branch: the evidence extends
/// its chain, one confirmation catches it up, nothing drops.
#[test]
fn a_stuck_seat_adopts_what_extends_its_chain() {
    let (x, _) = fork();
    let a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks[..2].to_vec());
    let mut a = a;
    a.chain.peer_heights.insert("b".to_string(), 1);
    keyed(&mut a);
    deliver(&a, &mut b, "b");
    let seen = b.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.kind.as_str(), seen.blocks, seen.from_height), ("ready", "extends", 2, 2));
    assert_eq!(head(&b), 1, "held, not adopted");
    b.cmd_evidence_adopt("a").expect("adopt");
    assert_eq!(head(&b), head(&a));
    assert_eq!(b.seat_evidence("a").inbound, "adopted");
}

/// Another branch is kept and shown; nothing is adopted while the choice
/// is dark.
#[test]
fn another_branch_is_shown_never_adopted() {
    let (x, y) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut c = genesis_seat("c", &y, y.blocks.clone());
    a.chain.diverged.insert(
        "c".to_string(),
        molt_core::ChainDivergence { peer: "c".to_string(), since_height: 1, seen_ts: 0 },
    );
    keyed(&mut a);
    let held = c.chain.head.clone();
    deliver(&a, &mut c, "c");
    let seen = c.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.kind.as_str(), seen.blocks, seen.drops), ("ready", "other-branch", 2, 1));
    let e = c.cmd_evidence_adopt("a").expect_err("dark");
    assert_eq!(e.to_string(), "branch: another branch: the branch choice decides");
    assert_eq!(c.chain.head, held, "nothing adopted");
    assert_eq!(c.branch_status_view().forks.len(), 1, "both branches shown");
    assert!(c.chain.branch.file.provenance.values().any(|how| how == "seat:a"), "where it came from");
}

/// A forged block refuses the whole bundle; a damaged payload too.
#[test]
fn a_forged_or_damaged_bundle_is_refused() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks[..2].to_vec());
    keyed(&mut b);
    let mut run = x.blocks[2..].to_vec();
    run[1].sigs[0].sig = "00".repeat(64);
    let payload = serde_json::to_string(&run).expect("json");
    let offer = molt_storage::content_hash(payload.as_bytes());
    b.cmd_net_evidence_part(&pk("a"), &b.republic_id(), &offer, (0, 1), payload.clone()).expect("ack");
    let seen = b.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.reason.as_str()), ("refused", "threshold"));
    assert_eq!(head(&b), 1);
    b.cmd_net_evidence_part(&pk("c"), &b.republic_id(), &"ab".repeat(32), (0, 1), payload).expect("ack");
    assert_eq!(b.seat_evidence("c").reason, "damaged");
}

/// Bounds: too many parts or an oversized part are refused at once.
#[test]
fn an_oversized_bundle_is_refused() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    keyed(&mut b);
    let rid = b.republic_id();
    b.cmd_net_evidence_part(&pk("a"), &rid, "o1", (0, 100_000), "x".to_string()).expect("ack");
    assert_eq!(b.seat_evidence("a").reason, "too large: use the file");
    let big = "x".repeat(evidence::PART_BYTES + 1);
    b.cmd_net_evidence_part(&pk("c"), &rid, "o2", (0, 2), big).expect("ack");
    assert_eq!(b.seat_evidence("c").inbound, "refused");
}

/// Only a roster seat's working anchor may send; another republic's
/// parts are ignored.
#[test]
fn a_non_member_cannot_send() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    keyed(&mut b);
    let rid = b.republic_id();
    b.cmd_net_evidence_part(&"ee".repeat(32), &rid, "o", (0, 1), "[]".to_string()).expect("ack");
    b.cmd_net_evidence_part(&pk("b"), &rid, "o", (0, 1), "[]".to_string()).expect("ack");
    b.cmd_net_evidence_part(&pk("a"), "elsewhere", "o", (0, 1), "[]".to_string()).expect("ack");
    assert!(b.chain.evidence.inbound.is_empty(), "nothing held");
}

/// An adopted offer does not come back with a relay's replay.
#[test]
fn a_replayed_offer_stays_finished() {
    let (x, _) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks[..2].to_vec());
    a.chain.peer_heights.insert("b".to_string(), 1);
    keyed(&mut a);
    deliver(&a, &mut b, "b");
    b.cmd_evidence_adopt("a").expect("adopt");
    deliver(&a, &mut b, "b");
    assert_eq!(b.seat_evidence("a").inbound, "adopted");
    let e = b.cmd_evidence_adopt("a").expect_err("once");
    assert!(matches!(e, MoltError::Branch(w) if w == "nothing to adopt"));
}

/// When an automatic offer is due: a peer stuck in a catch-up this seat
/// serves without progress - never plain lag, never a diverged peer (its
/// anchor may sit on the other branch), and once per range: not again
/// until the peer's own head passed what was offered (M4, L1).
#[test]
fn an_offer_is_due_only_where_the_normal_path_fails() {
    let (x, _) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    a.chain.peer_heights.insert("b".to_string(), 1);
    assert!(a.evidence_due().is_empty(), "plain lag is the catch-up's");
    a.chain.heal_loops.push(HealLoop {
        peer: "b".to_string(),
        asker: false,
        height: 2,
        key: "k".to_string(),
        rounds: super::sync::HEAL_LOOP_ROUNDS,
        since: 1,
        rule: String::new(),
        loud: false,
    });
    a.chain.diverged.insert("c".to_string(), molt_core::ChainDivergence { peer: "c".to_string(), since_height: 1, seen_ts: 0 });
    assert_eq!(a.evidence_due(), vec!["b".to_string()], "a diverged peer waits for a manual offer");
    assert!(a.seat_evidence("c").anchor_outdated, "and says why");
    a.note_auto_offer("b");
    for h in [2, 1, 2, 0] {
        a.chain.peer_heights.insert("b".to_string(), h);
        assert!(a.evidence_due().is_empty(), "a peer cannot re-trigger by its claims: {h}");
    }
    a.chain.peer_heights.insert("b".to_string(), 3);
    assert_eq!(a.evidence_due(), vec!["b".to_string()], "past the offered range");
}

/// H1: a want is deduplicated, capped at the offer, bounded per offer and
/// by bytes.
#[test]
fn a_want_cannot_amplify() {
    let (x, _) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    a.chain.peer_heights.insert("b".to_string(), 0);
    keyed(&mut a);
    a.chain.evidence.no_net = true;
    a.cmd_evidence_offer("b").expect("offer");
    let offer = a.chain.evidence.out["b"].offer.clone();
    let rid = a.republic_id();
    let first = a.chain.evidence.out["b"].parts[0].len();
    let base = a.chain.evidence.out["b"].bytes;
    a.cmd_net_evidence_want(&pk("b"), &rid, &offer, &[0; 10_000]).expect("ack");
    assert_eq!(a.chain.evidence.out["b"].bytes, base + first, "one part, once");
    a.cmd_net_evidence_want(&pk("b"), &rid, &offer, &[7_000, 9_999]).expect("ack");
    assert_eq!(a.chain.evidence.out["b"].bytes, base + first, "beyond the offer is nothing");
    for _ in 0..10 {
        a.cmd_net_evidence_want(&pk("b"), &rid, &offer, &[0]).expect("ack");
    }
    assert!(a.chain.evidence.out["b"].bytes <= 4 * base, "a byte budget per offer");
    assert!(a.chain.evidence.out["b"].wants <= 3, "a want budget per offer");
}

/// H2: on a v1 chain an own block with a junk attestation, or another
/// honest m-subset of it, is the block this seat holds - never a fork.
#[test]
fn a_signature_variant_of_an_own_block_is_held() {
    let (x, _) = super::branch_tests::fork_v1();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    keyed(&mut b);
    let rid = b.republic_id();
    let mut junk = x.blocks[2].clone();
    junk.sigs.push(RosterAttestation { member: "c".to_string(), sig: "00".repeat(64) });
    let mut subset = x.blocks[2].clone();
    subset.sigs = x.seal(2, subset.change.clone(), &["b", "c"]).sigs;
    for (i, variant) in [junk, subset].into_iter().enumerate() {
        let payload = serde_json::to_string(&vec![variant]).expect("json");
        let offer = molt_storage::content_hash(payload.as_bytes());
        let from = if i == 0 { "a" } else { "c" };
        b.cmd_net_evidence_part(&pk(from), &rid, &offer, (0, 1), payload).expect("ack");
        assert_eq!(b.seat_evidence(from).inbound, "", "nothing new from {from}");
    }
    assert!(b.chain.branch.file.evidence.is_empty(), "no phantom fork");
}

/// H2: a held prefix (as a variant) plus a new suffix extends.
#[test]
fn a_variant_prefix_with_a_new_suffix_extends() {
    let (x, _) = super::branch_tests::fork_v1();
    let mut b = genesis_seat("b", &x, x.blocks[..3].to_vec());
    keyed(&mut b);
    let mut variant = x.blocks[2].clone();
    variant.sigs.push(RosterAttestation { member: "c".to_string(), sig: "00".repeat(64) });
    let run = vec![variant, x.blocks[3].clone()];
    let payload = serde_json::to_string(&run).expect("json");
    let offer = molt_storage::content_hash(payload.as_bytes());
    b.cmd_net_evidence_part(&pk("a"), &b.republic_id(), &offer, (0, 1), payload).expect("ack");
    let seen = b.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.kind.as_str(), seen.blocks), ("ready", "extends", 1));
    assert!(b.chain.branch.file.evidence.is_empty());
}

/// M1: a head that moved since arrival is re-checked at adopt - a block
/// of another branch at that height turns the bundle stale, nothing is
/// fed to the fork rule.
#[test]
fn adopt_rechecks_against_the_current_head() {
    let (x, y) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks[..2].to_vec());
    a.chain.peer_heights.insert("b".to_string(), 1);
    keyed(&mut a);
    deliver(&a, &mut b, "b");
    b.receive_block_from("c", y.blocks[2].clone());
    let held = b.chain.head.clone();
    let e = b.cmd_evidence_adopt("a").expect_err("stale");
    assert_eq!(e.to_string(), "branch: stale: the chain moved");
    assert_eq!(b.chain.head, held);
    assert_eq!(b.seat_evidence("a").kind, "other-branch", "re-judged");
    b.delivery.evicted = true;
    assert!(b.cmd_evidence_adopt("a").is_err(), "an evicted seat adopts nothing");
}

/// M2: a refused or incomplete offer is not final - a new pass of the
/// same payload is judged again.
#[test]
fn a_new_pass_reopens_a_refused_offer() {
    let (x, _) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks[..2].to_vec());
    // a believes b is further than it is: the run does not link yet
    a.chain.peer_heights.insert("b".to_string(), 2);
    keyed(&mut a);
    deliver(&a, &mut b, "b");
    assert_eq!(b.seat_evidence("a").reason, "does not link to this chain");
    b.receive_block_from("a", x.blocks[2].clone());
    deliver(&a, &mut b, "b");
    let seen = b.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.kind.as_str()), ("ready", "extends"));
}

/// M2 + the re-ask: a missing part is asked for a bounded number of
/// times, then the entry is incomplete but keeps its parts; a new pass
/// completes it.
#[test]
fn a_missing_part_is_asked_then_a_new_pass_completes() {
    let (x, _) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut b = genesis_seat("b", &x, x.blocks[..1].to_vec());
    a.chain.peer_heights.insert("b".to_string(), 0);
    keyed(&mut a);
    keyed(&mut b);
    let (offer, _, _) = a.evidence_for("b").expect("offer");
    let payload: String = serde_json::to_string(&x.blocks[1..].to_vec()).expect("json");
    assert_eq!(molt_storage::content_hash(payload.as_bytes()), offer);
    let (one, two) = payload.split_at(payload.len() / 2);
    let rid = b.republic_id();
    b.chain.evidence.no_net = true;
    b.presence.clock_override = Some(1_000);
    b.cmd_net_evidence_part(&pk("a"), &rid, &offer, (1, 2), two.to_string()).expect("ack");
    for round in 1..=4u64 {
        b.presence.clock_override = Some(1_000 + 31 * round);
        b.evidence_tick(1_000 + 31 * round);
    }
    let seen = b.seat_evidence("a");
    assert_eq!((seen.inbound.as_str(), seen.reason.as_str(), seen.in_parts), ("refused", "incomplete", 1));
    b.cmd_net_evidence_part(&pk("a"), &rid, &offer, (0, 2), one.to_string()).expect("ack");
    assert_eq!(b.seat_evidence("a").inbound, "ready", "the kept part plus the new pass");
}

/// M3: evidence already held is not written again; one judged bundle per
/// sender and head.
#[test]
fn judging_is_bounded_per_sender_and_head() {
    let (x, y) = fork();
    let mut a = genesis_seat("a", &x, x.blocks.clone());
    let mut c = genesis_seat("c", &y, y.blocks.clone());
    a.chain.peer_heights.insert("c".to_string(), 1);
    keyed(&mut a);
    deliver(&a, &mut c, "c");
    assert_eq!(c.seat_evidence("a").kind, "other-branch");
    c.cmd_evidence_dismiss("a").expect("dismiss");
    c.chain.branch.disk = None;
    a.chain.peer_heights.insert("c".to_string(), 0);
    deliver(&a, &mut c, "c");
    assert!(c.chain.branch.disk.is_none(), "nothing new, nothing written");
    assert_eq!(c.seat_evidence("a").reason, "one bundle per head", "the second judged bundle waits");
}

/// L1 test: the founding anchor of a restored seat is not its address.
#[test]
fn an_old_anchor_after_a_known_restore_is_refused() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    keyed(&mut b);
    b.chain.anchors.insert("a".to_string(), "a9".repeat(32));
    b.cmd_net_evidence_part(&pk("a"), &b.republic_id(), "o", (0, 1), "[]".to_string()).expect("ack");
    assert!(b.chain.evidence.inbound.is_empty(), "the replaced device");
}

/// Bounds of a part: an index past the total, a zero total, a duplicate.
#[test]
fn malformed_parts_are_refused_and_duplicates_count_once() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    keyed(&mut b);
    let rid = b.republic_id();
    b.cmd_net_evidence_part(&pk("a"), &rid, "o1", (2, 2), "x".to_string()).expect("ack");
    assert_eq!(b.seat_evidence("a").inbound, "refused");
    b.cmd_net_evidence_part(&pk("c"), &rid, "o2", (0, 0), "x".to_string()).expect("ack");
    assert_eq!(b.seat_evidence("c").inbound, "refused");
    let _ = b.cmd_evidence_dismiss("a");
    b.cmd_net_evidence_part(&pk("a"), &rid, "o3", (0, 3), "x".to_string()).expect("ack");
    b.cmd_net_evidence_part(&pk("a"), &rid, "o3", (0, 3), "x".to_string()).expect("ack");
    assert_eq!(b.seat_evidence("a").in_parts, 1);
}

/// L3: parts are cut by their ENCODED size - quotes escape to two bytes.
#[test]
fn parts_are_sized_by_their_encoding() {
    let text = "\"".repeat(evidence::PART_BYTES);
    for p in evidence::split_parts(&text) {
        assert!(serde_json::to_string(&p).expect("json").len() <= evidence::PART_BYTES + 2);
    }
}

/// The parts of a payload with multi-byte text reassemble exactly.
#[test]
fn parts_cut_on_char_boundaries() {
    let text = "ä".repeat(evidence::PART_BYTES);
    let parts = evidence::split_parts(&text);
    assert!(parts.len() >= 2 && parts.iter().all(|p| p.len() <= evidence::PART_BYTES));
    assert_eq!(parts.concat(), text);
}

/// H2 in the file import too: a v1 signature variant of an own block in
/// a bundle is held, never kept as another branch.
#[test]
fn a_file_import_of_a_signature_variant_keeps_nothing() {
    let (x, _) = super::branch_tests::fork_v1();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    let a = genesis_seat("a", &x, x.blocks.clone());
    let mut bundle = a.branch_bundle();
    bundle.runs[0][1].sigs.push(RosterAttestation { member: "c".to_string(), sig: "00".repeat(64) });
    let json = serde_json::to_string(&bundle).expect("json");
    assert_eq!(b.import_bundle(&json).expect("imports"), (0, 0));
    assert!(b.chain.branch.file.evidence.is_empty());
}

/// L5: the finished offers are bounded.
#[test]
fn finished_offers_are_bounded() {
    let (x, _) = fork();
    let mut b = genesis_seat("b", &x, x.blocks.clone());
    for i in 0..1_000 {
        b.chain.evidence.inbound.insert("a".to_string(), evidence::Incoming { offer: format!("o{i}"), ..Default::default() });
        b.cmd_evidence_dismiss("a").expect("dismiss");
    }
    assert!(b.chain.evidence.finished.len() <= 256);
}
