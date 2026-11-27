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
    b.cmd_net_evidence_part(&pk("a"), &b.republic_id(), &"ab".repeat(32), (0, 1), payload).expect("ack");
    assert_eq!(b.seat_evidence("a").reason, "damaged");
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

/// When an offer is due: another branch or a catch-up served without
/// progress - never plain lag; once per (own head, peer's reported head).
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
    let due: Vec<String> = a.evidence_due().into_iter().map(|(m, _)| m).collect();
    assert_eq!(due, vec!["b".to_string(), "c".to_string()]);
    for (m, key) in a.evidence_due() {
        a.chain.evidence.offered.insert(m, key);
    }
    assert!(a.evidence_due().is_empty(), "once per key");
    a.chain.peer_heights.insert("b".to_string(), 2);
    assert_eq!(a.evidence_due().len(), 1, "the peer moved: offered again");
}

/// The parts of a payload with multi-byte text reassemble exactly.
#[test]
fn parts_cut_on_char_boundaries() {
    let text = "ä".repeat(evidence::PART_BYTES);
    let parts = evidence::split_parts(&text);
    assert!(parts.len() >= 2 && parts.iter().all(|p| p.len() <= evidence::PART_BYTES));
    assert_eq!(parts.concat(), text);
}
