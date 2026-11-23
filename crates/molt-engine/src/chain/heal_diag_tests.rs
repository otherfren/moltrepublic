// SPDX-License-Identifier: GPL-3.0-or-later

//! WP3: a stale block does not re-mark a peer that left its branch; a
//! refused block and a catch-up round that moves nothing are visible on
//! the refusing seat without a debug log.

use super::*;
use crate::chain::test_support::*;
use molt_core::{HeightHash, Surface, WorkspaceEvent};
use serde_json::json;

const T: u64 = 1_751_000_000;

/// `st` heard every seat at `now`.
fn hear(st: &mut crate::State, now: u64) {
    st.presence.clock_override = Some(now);
    st.session.active_workspace = "w-heal".to_string();
    let roster = st.roster();
    let members = molt_core::roster_members(&roster, now, |_| now);
    st.session.workspaces.retain(|w| w.id != "w-heal");
    st.session.workspaces.push(molt_core::WorkspaceInfo {
        id: "w-heal".to_string(),
        name: "Chess Club".to_string(),
        detail: "2-of-2".to_string(),
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

fn note(id: u64, title: &str) -> ChainChange {
    ChainChange::Applied { proposal_id: id, surface: Surface::Memory, payload: json!({ "op": "add_note", "title": title }) }
}

/// walter on `g, x1, x2`; petra's old block `y1` contends at height 1 and
/// loses the fork rule (petra re-based, not walter).
fn walter_and_an_old_block() -> (crate::State, Builder, ChainBlock) {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let x1 = b.seal(1, note(1, "x1"), &["petra", "walter"]);
    let y1 = (0..64)
        .map(|i| b.seal(1, note(9, &format!("old {i}")), &["petra", "walter"]))
        .find(|y| block_hash(&b.republic_id, y) > block_hash(&b.republic_id, &x1))
        .expect("a losing block");
    b.push(x1);
    b.commit(note(2, "x2"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    (walter, b, y1)
}

fn head_report(b: &Builder, height: u64) -> WorkspaceEvent {
    let block = b.blocks.iter().find(|x| x.height == height).expect("a block");
    WorkspaceEvent::ChainRequest {
        from_height: height + 1,
        known: vec![HeightHash { height, hash: block_hash(&b.republic_id, block) }],
    }
}

fn alarmed_after_a_quiet_hour(st: &mut crate::State) -> bool {
    st.chain_heal_tick();
    for now in [T + 1, T + 1_000, T + 2_000] {
        hear(st, now);
        st.chain_heal_tick();
    }
    st.session.notice.starts_with("chain-heal-stuck")
}

/// F1: the peer re-based onto ours and said so; its old block arriving
/// late is history, not a divergence.
#[test]
fn a_late_block_of_a_left_branch_does_not_re_mark_the_peer() {
    let (mut walter, b, y1) = walter_and_an_old_block();
    wire(&mut walter, "petra", 20, head_report(&b, 2));
    wire(&mut walter, "petra", 10, WorkspaceEvent::Committed(y1));
    assert!(walter.chain.diverged.is_empty(), "petra stays healthy");
    assert!(walter.chain.fork_candidates.is_empty(), "nothing stashed");
    assert!(!alarmed_after_a_quiet_hour(&mut walter), "no alarm");
}

/// The counter-cases: a peer whose newest frame is the foreign block, or
/// whose last report names a block we do not hold, is on another branch.
#[test]
fn a_peer_really_on_another_branch_is_still_marked() {
    let (mut walter, b, y1) = walter_and_an_old_block();
    wire(&mut walter, "petra", 20, head_report(&b, 2));
    wire(&mut walter, "petra", 30, WorkspaceEvent::Committed(y1.clone()));
    assert!(walter.chain.diverged.contains_key("petra"), "the block is newer than the report");

    let (mut walter, _, y1) = walter_and_an_old_block();
    let foreign = WorkspaceEvent::ChainRequest {
        from_height: 2,
        known: vec![HeightHash { height: 1, hash: block_hash(&b.republic_id, &y1) }],
    };
    wire(&mut walter, "petra", 20, foreign);
    wire(&mut walter, "petra", 10, WorkspaceEvent::Committed(y1));
    assert!(walter.chain.diverged.contains_key("petra"), "its report names a foreign block");
}

fn rules(st: &crate::State) -> Vec<String> {
    st.chain_sync().refused.iter().map(|r| r.rule.clone()).collect()
}

/// H1: each refusal names its rule, its block and its sender in status.
#[test]
fn a_refused_block_names_its_rule_in_status() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit(note(1, "x1"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    let mut short = b.seal(2, note(2, "x2"), &["petra"]);
    short.prev = b.head_hash.clone();
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(short.clone()));
    let twice = b.seal(2, note(1, "again"), &["petra", "walter"]);
    wire(&mut walter, "petra", 2, WorkspaceEvent::Committed(twice));
    let genesis = b.seal(2, b.blocks[0].change.clone(), &["petra", "walter"]);
    wire(&mut walter, "petra", 3, WorkspaceEvent::Committed(genesis));
    assert_eq!(rules(&walter), vec!["threshold", "double_apply", "genesis_late"]);
    let first = &walter.chain_sync().refused[0];
    assert_eq!(first.from, "petra");
    assert_eq!(first.height, 2);
    assert_eq!(first.hash, block_hash(&b.republic_id, &short)[..12]);
    assert_eq!(first.at, T);
}

/// Every refusal reason in the verifier maps to its own code.
#[test]
fn every_refusal_reason_has_a_stable_code() {
    for (text, rule) in [
        ("block height 3 does not follow 1", "height"),
        ("block 2 does not link to its predecessor", "link"),
        ("a genesis cannot appear after height 0", "genesis_late"),
        ("proposal 4 is applied twice", "double_apply"),
        ("checkpoint upto 3 must be exactly its block height 5 minus one", "cut_upto"),
        ("block 2 has 1 valid approvals, threshold is 2", "threshold"),
        ("block 2: c's consent is spent", "consent_spent"),
        ("block 2: c's consent is stale", "consent_stale"),
        ("block 2: c's consent does not verify", "consent_invalid"),
        ("block 2: c's consent is malformed", "consent_malformed"),
        ("block 2 counts c twice - consent plus a roster signature", "consent_twice"),
        ("block 2 carries a consent on a non-restore membership change", "consent_misplaced"),
        ("cannot restore unknown member c", "unknown_member"),
        ("block 2 restores an unknown member", "unknown_member"),
        ("a Restored block must keep c's anchored identity key", "restore_key"),
        ("membership op `joined` is not supported: seats are fixed at founding", "joined"),
        ("checkpoint at upto 4 does not match this chain's own projection", "cut_state"),
        ("checkpoint upto 3 lies below the blob coverage 4", "cut_floor"),
        ("block 5: a legacy checkpoint after a folded cut", "cut_kind"),
        ("block 5: a vault checkpoint outside a vault republic", "cut_kind"),
        ("block 5: vault not enabled", "vault"),
        ("something new", "other"),
    ] {
        assert_eq!(crate::chain::refusal_rule(text), rule, "{text}");
    }
}

/// G3, the server's side: the same peer asks for the same range again and
/// again while its head stands still.
#[test]
fn a_repeated_request_reads_heal_stuck_on_the_server_until_the_peer_moves() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit(note(1, "x1"), &["petra", "walter"]);
    b.commit(note(2, "x2"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    for (i, seq) in [10u64, 11, 12].into_iter().enumerate() {
        assert!(walter.chain_sync().heal_stuck.is_none(), "not after {i} rounds");
        hear(&mut walter, T + 100 * seq);
        wire(&mut walter, "petra", seq, head_report(&b, 1));
    }
    let stuck = walter.chain_sync().heal_stuck.expect("stuck after three rounds");
    assert_eq!((stuck.peer.as_str(), stuck.height, stuck.since), ("petra", 2, T + 1_000));
    assert!(walter.session.notice.starts_with("chain-heal-stuck:petra"), "{}", walter.session.notice);
    wire(&mut walter, "petra", 13, head_report(&b, 2));
    assert!(walter.chain_sync().heal_stuck.is_none(), "the peer's head moved");
}

/// G3, the asker's side: the same block is refused again and again; a
/// head move clears it, and a restart re-derives rather than clears it.
#[test]
fn a_repeatedly_refused_block_reads_heal_stuck_on_the_asker() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit(note(1, "x1"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    let mut short = b.seal(2, note(2, "x2"), &["petra"]);
    short.prev = b.head_hash.clone();
    // frames repeating the block between two requests are one round
    for seq in [1u64, 2, 3] {
        wire(&mut walter, "petra", seq, WorkspaceEvent::Committed(short.clone()));
    }
    assert!(walter.chain_sync().heal_stuck.is_none(), "one round, not three");
    for seq in [11u64, 12] {
        walter.ask_for_height(2);
        wire(&mut walter, "petra", seq, WorkspaceEvent::Committed(short.clone()));
    }
    let stuck = walter.chain_sync().heal_stuck.expect("stuck");
    assert_eq!((stuck.peer.as_str(), stuck.height, stuck.rule.as_str()), ("petra", 2, "threshold"));

    // a restart: the evidence survives, the flag waits for one more round
    let dump = walter.snapshot_now().state;
    let mut back = chain_signer("walter", &b, b.blocks.clone());
    back.restore_dump(dump);
    hear(&mut back, T + 50);
    assert!(back.chain_sync().heal_stuck.is_none(), "re-derived, not trusted blindly");
    wire(&mut back, "petra", 13, WorkspaceEvent::Committed(short));
    let again = back.chain_sync().heal_stuck.expect("re-derived");
    assert_eq!(again.since, T, "the original start");

    let good = b.commit(note(2, "x2"), &["petra", "walter"]);
    wire(&mut back, "petra", 14, WorkspaceEvent::Committed(good));
    assert!(back.chain_sync().heal_stuck.is_none(), "the head moved");
}

/// A4: an identical line is logged once per key per minute; the next one
/// after the minute carries how many it folded.
#[test]
fn an_identical_log_line_is_throttled_per_minute() {
    let (mut walter, _, _) = walter_and_an_old_block();
    assert_eq!(walter.log_gate("served:petra", "from=1 blocks=2"), Some(0));
    assert_eq!(walter.log_gate("served:petra", "from=1 blocks=2"), None);
    assert_eq!(walter.log_gate("served:petra", "from=1 blocks=2"), None);
    assert_eq!(walter.log_gate("served:dora", "from=1 blocks=2"), Some(0), "per key");
    assert_eq!(walter.log_gate("served:petra", "from=2 blocks=1"), Some(2), "a new text");
    assert_eq!(walter.log_gate("served:petra", "from=2 blocks=1"), None);
    hear(&mut walter, T + 60);
    assert_eq!(walter.log_gate("served:petra", "from=2 blocks=1"), Some(1), "a minute later");
}

fn reported_head(st: &crate::State, member: &str) -> Option<u64> {
    let mut seats = st.seat_syncs(&std::collections::HashSet::new());
    st.fill_seat_reports(&mut seats);
    seats.into_iter().find(|s| s.member == member).and_then(|s| s.head_height)
}

fn committed_sent(st: &crate::State) -> usize {
    st.chain.lock.outbox.iter().filter(|e| matches!(e.body, WorkspaceEvent::Committed(_))).count()
}

/// Review H1/L3: a re-delivered run of a peer's old beacons is history -
/// no heal round, no serve, no divergence clear, no seat head move.
#[test]
fn a_re_delivered_run_of_old_beacons_is_history() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit(note(1, "x1"), &["petra", "walter"]);
    b.commit(note(2, "x2"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    wire(&mut walter, "petra", 20, head_report(&b, 2));
    assert_eq!(reported_head(&walter, "petra"), Some(2));
    walter.chain.diverged.insert(
        "petra".to_string(),
        molt_core::ChainDivergence { peer: "petra".to_string(), since_height: 2, seen_ts: T },
    );
    let served = committed_sent(&walter);
    for seq in [10u64, 11, 12] {
        hear(&mut walter, T + 100 * seq);
        wire(&mut walter, "petra", seq, head_report(&b, 1));
    }
    assert!(walter.chain_sync().heal_stuck.is_none(), "no rounds");
    assert_eq!(committed_sent(&walter), served, "nothing served");
    assert!(walter.chain.diverged.contains_key("petra"), "no clear");
    assert_eq!(reported_head(&walter, "petra"), Some(2), "the seat head stays");
}

/// walter on `g, x1, x2, x3`; petra's fork block `y2` on the shared `x1`
/// loses the fork rule.
fn walter_and_a_fork_above_a_shared_prefix() -> (crate::State, Builder, ChainBlock) {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit(note(1, "x1"), &["petra", "walter"]);
    let x2 = b.seal(2, note(2, "x2"), &["petra", "walter"]);
    let y2 = (0..64)
        .map(|i| b.seal(2, note(9, &format!("fork {i}")), &["petra", "walter"]))
        .find(|y| block_hash(&b.republic_id, y) > block_hash(&b.republic_id, &x2))
        .expect("a losing block");
    b.push(x2);
    b.commit(note(3, "x3"), &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    hear(&mut walter, T);
    (walter, b, y2)
}

/// Review R4: a newer duplicate of a SHARED block proves nothing about a
/// fork above it - the late fork block still marks its sender.
#[test]
fn a_shared_prefix_duplicate_does_not_excuse_a_later_fork_block() {
    let (mut walter, b, y2) = walter_and_a_fork_above_a_shared_prefix();
    wire(&mut walter, "petra", 11, WorkspaceEvent::Committed(b.blocks[1].clone()));
    wire(&mut walter, "petra", 10, WorkspaceEvent::Committed(y2));
    assert!(walter.chain.diverged.contains_key("petra"), "the fork block marks petra");
}

/// Review R4, the clearing twin: a duplicate below the fork height does
/// not clear the mark; one at it does.
#[test]
fn only_a_duplicate_at_the_fork_height_clears_a_divergence() {
    let (mut walter, b, y2) = walter_and_a_fork_above_a_shared_prefix();
    wire(&mut walter, "petra", 10, WorkspaceEvent::Committed(y2));
    assert!(walter.chain.diverged.contains_key("petra"));
    wire(&mut walter, "petra", 11, WorkspaceEvent::Committed(b.blocks[1].clone()));
    assert!(walter.chain.diverged.contains_key("petra"), "a shared block proves nothing");
    wire(&mut walter, "petra", 12, WorkspaceEvent::Committed(b.blocks[2].clone()));
    assert!(walter.chain.diverged.is_empty(), "our block at the fork height does");
}
