// SPDX-License-Identifier: GPL-3.0-or-later

//! The delivery beat: due ACK deadlines.

use super::test_support::*;

/// §4.3: the ACK flush takes only DUE deadlines (future ones stay armed),
/// and a (re)established leg arms an immediate ack only when there is a
/// window to report.
#[test]
fn the_ack_flush_takes_only_due_deadlines_and_link_up_arms_one() {
    let mut st = presence_fixture();
    let win = {
        let mut w = molt_core::AcceptedWindow::default();
        assert!(w.accept(3));
        w
    };
    st.delivery.accepted.insert("bob".to_string(), win);
    st.delivery.ack_due.insert("bob".to_string(), T);
    st.delivery.ack_due.insert("cid".to_string(), T + 100);
    st.flush_due_acks(T + 1);
    assert!(
        !st.delivery.ack_due.contains_key("bob"),
        "the due deadline is consumed (no mesh here: dropped - resends re-arm)"
    );
    assert!(st.delivery.ack_due.contains_key("cid"), "a future deadline stays armed");

    // link-up arms an immediate ack — but only with a window to report
    st.delivery.ack_due.clear();
    st.cmd_net_link_up("cid".to_string(), None).expect("ack");
    assert!(st.delivery.ack_due.is_empty(), "no window for cid - nothing to report");
    st.cmd_net_link_up("bob".to_string(), None).expect("ack");
    assert_eq!(st.delivery.ack_due.get("bob"), Some(&T), "bob's window arms a due-now ack");
}

// ---- G7 inbound gap: hold by need, close actively, never deafen --------

#[derive(Clone, Default)]
struct AskSink;
impl molt_net::EngineSink for AskSink {
    async fn deliver(&self, _from: &molt_core::MemberId, _env: molt_core::EventEnvelope) -> Result<(), molt_net::NetError> {
        Ok(())
    }
    async fn peer_seen(&self, _m: &molt_core::MemberId) {}
    async fn send_failed(&self, _m: &molt_core::MemberId, _r: &str) {}
}

/// Attach a group runtime on a dead relay (bound, then dropped - never a
/// service port): its tasks never deliver, only the control queue takes the
/// asks. Needs a tokio runtime.
pub(super) fn attach_dead_group(st: &mut crate::State, replaying: bool) {
    let (health_tx, health_rx) = tokio::sync::watch::channel(molt_net::group_runtime::GroupHealth {
        subscribed: true,
        replaying,
        ..molt_net::group_runtime::GroupHealth::default()
    });
    let mls = std::sync::Arc::new(std::sync::Mutex::new({
        let mut m = molt_net::MlsMember::new(&molt_storage::SigningKey::from_bytes(&[9u8; 32]), "ada").expect("mls");
        m.create_group().expect("group");
        m
    }));
    let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("ws://127.0.0.1:{}", dead.local_addr().expect("addr").port());
    drop(dead);
    let channel = molt_net::ritual_net::GroupChannel::new(molt_net::dial::Dialer::direct(), vec![url], [7u8; 32]);
    let (wake, wake_rx) = tokio::sync::watch::channel(0u64);
    let handle = molt_net::group_runtime::spawn_group(
        channel.clone(),
        molt_net::supervisor::MlsChannel::from_shared(mls.clone()),
        molt_net::group_runtime::GroupNetConfig::fast("ada".to_string(), vec!["bob".to_string()]),
        molt_net::supervisor::MemLog::new(),
        molt_net::supervisor::MemStateStore::new(),
        AskSink,
        wake_rx,
        health_tx,
    );
    let trickle = molt_net::trickle::spawn_trickle(
        channel,
        molt_net::supervisor::MemStateStore::new(),
        handle.outbox_busy(),
        molt_net::trickle::TrickleConfig::new(3600, 1),
    );
    st.group_net = Some(crate::GroupNet {
        handle,
        mls,
        wakeup: wake,
        health: health_rx,
        trickle,
        lockout: crate::net::LockOut::default(),
        unreadable: crate::net::UnreadableEpisode::default(),
    });
}


fn chat_env(seq: u64, prev: u64, body: &str) -> molt_core::EventEnvelope {
    let n = usize::try_from(seq).expect("small");
    molt_core::EventEnvelope {
        seq,
        ts: T,
        by: "bob".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(id(n), "bob", body, T)),
        prev_seq: prev,
    }
}

fn request_env(seq: u64, prev: u64) -> molt_core::EventEnvelope {
    molt_core::EventEnvelope {
        seq,
        ts: T,
        by: "bob".to_string(),
        body: molt_core::WorkspaceEvent::ChainRequest { from_height: 1, known: Vec::new() },
        prev_seq: prev,
    }
}

fn land(st: &mut crate::State, env: molt_core::EventEnvelope) {
    st.cmd_net_delivered("bob".to_string(), env, None).expect("a wire delivery never errors");
}

fn bodies(st: &crate::State) -> Vec<String> {
    st.chat.iter().map(|m| m.body.clone()).collect()
}

fn at(st: &mut crate::State, secs: u64) {
    st.presence.clock_override = Some(T + secs);
}

/// A gap holds only what needs the sender's order: a frame whose effect
/// does not depend on its predecessor is accepted at once, the chat behind
/// the gap waits, and the late predecessor drains it in order.
#[test]
fn a_gap_holds_only_order_sensitive_kinds() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, request_env(5, 4));
    assert!(
        st.delivery.accepted["bob"].is_accepted(5),
        "an order-free frame is accepted behind a gap"
    );
    land(&mut st, chat_env(6, 5, "after"));
    assert_eq!(bodies(&st), ["base"], "chat waits for the missing seq 4");
    land(&mut st, chat_env(4, 1, "before"));
    assert_eq!(bodies(&st), ["base", "before", "after"], "the sender's order");
}

/// The sender says the missing seq is not owed: the held stream is
/// released at once. One it re-published must land within the skew grace.
#[test]
fn a_gap_the_sender_disowns_releases_at_once() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(3, 2, "held"));
    assert_eq!(bodies(&st), ["base"]);
    st.cmd_net_gap_answered("bob".to_string(), "cid".to_string(), Vec::new(), vec![2]);
    assert_eq!(bodies(&st), ["base"], "an answer to another asker is not ours");
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), vec![2]);
    assert_eq!(bodies(&st), ["base", "held"]);
    assert!(st.inbound_held_view().iter().all(|h| h.held == 0), "nothing is held any more");

    land(&mut st, chat_env(5, 4, "held two"));
    at(&mut st, 3);
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), vec![4], Vec::new());
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base", "held"], "a resent seq gets its grace");
    at(&mut st, 3 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base", "held", "held two"], "resent yet absent: settled");
}

/// A peer that cannot answer (an older build): the hold ends after a short
/// bound, never after 900 s; the predecessor arriving later still lands
/// once, and a settled gap never holds new traffic again.
#[test]
fn an_unanswered_gap_falls_back_far_below_the_old_valve() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(3, 2, "held"));
    at(&mut st, 30);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base"], "inside the bound it stays held");
    let view = st.inbound_held_view();
    assert_eq!(
        (view.len(), view[0].missing_seq, view[0].held),
        (1, 2, 1),
        "the hold is visible"
    );
    at(&mut st, crate::net::GAP_FALLBACK_SECS + 1);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base", "held"], "released at the fallback bound");
    const { assert!(crate::net::GAP_FALLBACK_SECS * 10 < 900, "far below the old valve") };
    land(&mut st, chat_env(2, 1, "late"));
    assert_eq!(bodies(&st), ["base", "held", "late"], "the late one lands once");
    land(&mut st, chat_env(2, 1, "late"));
    land(&mut st, chat_env(4, 3, "next"));
    assert_eq!(bodies(&st), ["base", "held", "late", "next"], "no new hold");
}

/// An older sender's resend round passing over the gap without the gap
/// proves it unfillable: released a few seconds later, not at the bound.
#[test]
fn a_resend_round_passing_the_gap_settles_it() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, request_env(3, 2));
    land(&mut st, chat_env(4, 3, "held"));
    assert_eq!(bodies(&st), ["base"]);
    at(&mut st, 20);
    land(&mut st, request_env(3, 2)); // the round resends 3, not 2
    at(&mut st, 20 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base", "held"]);
}

/// Envelope `seq` of the G1 replay: every third one chat, the rest
/// order-free; the newest (`top`) order-free.
fn replay_env(seq: u64) -> molt_core::EventEnvelope {
    if seq % 3 == 1 {
        chat_env(seq, seq - 1, &format!("m{seq}"))
    } else {
        request_env(seq, seq - 1)
    }
}

fn replay_chats(upto: u64, without: &[u64]) -> Vec<String> {
    std::iter::once("base".to_string())
        .chain((2..=upto).filter(|s| s % 3 == 1 && !without.contains(s)).map(|s| format!("m{s}")))
        .collect()
}

fn accepted(st: &crate::State, seq: u64) -> bool {
    st.delivery.accepted["bob"].is_accepted(seq)
}

/// G1: a newest-first replay over far more than the accept window, with an
/// order-free head. Nothing may be accepted beyond the window above the
/// last gap-free point, so no older seq ever ages out unseen: everything
/// lands exactly once, in the sender's order, with no resend at all.
#[test]
fn a_newest_first_replay_past_the_window_loses_nothing() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    let top = 3000;
    for seq in (2..=top).rev() {
        land(&mut st, replay_env(seq));
        let high = st.delivery.accepted["bob"].high;
        assert!(seq == 2 || high <= 1 + molt_core::ACCEPT_WINDOW_BITS, "seq {seq}: high {high}");
    }
    assert_eq!(bodies(&st), replay_chats(top, &[]), "every chat once, in order");
    assert!((1..=top).all(|s| accepted(&st, s)), "every seq accepted");
    assert!(st.inbound_held_view().is_empty());
}

/// G1 with a real hole and a sender that never answers a gap want (an
/// older build): the window keeps reporting the hole unseen - so the
/// sender's ack floor stays below it and its resend rounds reach back to
/// it - and nothing above the window over the hole is accepted. Its next
/// round passing over the hole proves it unfillable; then everything held
/// lands in order.
#[test]
fn an_older_senders_hole_is_never_acked_and_closes_on_its_round() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    let top = 3000;
    for seq in (2..=top).rev().filter(|s| *s != 5) {
        land(&mut st, replay_env(seq));
    }
    let w = st.delivery.accepted["bob"].clone();
    assert!(!w.is_accepted(5), "the hole reads unseen: the sender's floor stops below it");
    assert!(w.is_accepted(4));
    assert!(w.high <= 4 + molt_core::ACCEPT_WINDOW_BITS, "nothing far above the hole: high {}", w.high);
    assert!(!accepted(&st, top), "the head waits, unaccepted");
    // the sender's round re-offers from its floor, which the hole pinned
    at(&mut st, 30);
    for seq in 6..=20 {
        land(&mut st, replay_env(seq));
    }
    at(&mut st, 30 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), replay_chats(top, &[]), "everything held lands, once, in order");
    assert!((6..=top).all(|s| accepted(&st, s)));
}

/// G1 with a sender that answers: the hole is listed, asked for and resent;
/// the frontier then walks up page by page.
#[test]
fn a_listed_hole_is_resent_and_the_frontier_walks_up() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    let top = 3000;
    for seq in (2..=top).rev().filter(|s| *s != 5) {
        land(&mut st, replay_env(seq));
    }
    let own: Vec<u64> = (2..=1 + molt_core::ACCEPT_WINDOW_BITS).collect();
    st.cmd_net_gap_listed(&"bob".to_string(), &"ada".to_string(), 1, 1 + molt_core::ACCEPT_WINDOW_BITS, own);
    assert!(!accepted(&st, top));
    land(&mut st, replay_env(5)); // the targeted resend
    assert_eq!(bodies(&st), replay_chats(top, &[]));
    assert!((1..=top).all(|s| accepted(&st, s)));
}

/// The byte bound: what it sheds stays owed (never accepted), is listed
/// and resent, and lands in order.
#[test]
fn a_shed_envelope_stays_owed_and_is_recovered() {
    let mut st = presence_fixture();
    st.delivery.hold_bytes_cap = Some(20_000);
    land(&mut st, chat_env(1, 0, "base"));
    let top = 400;
    for seq in (3..=top).rev() {
        land(&mut st, replay_env(seq));
    }
    let shed: Vec<u64> = (3..=top).filter(|s| !accepted(&st, *s) && !st.inbound_held_has("bob", *s)).collect();
    assert!(!shed.is_empty(), "the bound shed something");
    assert!(shed.iter().all(|s| !accepted(&st, *s)), "shed is never accepted");
    land(&mut st, replay_env(2));
    for seq in shed {
        land(&mut st, replay_env(seq)); // the sender's resend
    }
    assert_eq!(bodies(&st), replay_chats(top, &[]));
    assert!((1..=top).all(|s| accepted(&st, s)));
}

/// A restart in the middle of the catch-up: the held envelopes are gone
/// (their frames are spent), the frontier comes back with the window, the
/// listing names what is still owed, and the resends close it.
#[test]
fn a_restart_mid_catch_up_loses_nothing_owed() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    let top = 2000;
    for seq in (1500..=top).rev() {
        land(&mut st, replay_env(seq));
    }
    for seq in (2..=10).rev() {
        land(&mut st, replay_env(seq));
    }
    let saved = st.delivery.accepted.clone();
    let mut st = presence_fixture();
    st.delivery.accepted = saved;
    let owed: Vec<u64> = (2..=top).filter(|s| !st.delivery.accepted["bob"].is_accepted(*s)).collect();
    assert!(owed.contains(&top), "the spent head stays owed");
    let f = st.delivery.accepted["bob"].frontier;
    assert!(f >= 10, "the frontier came back: {f}");
    // the sender's listings and resends, bottom up
    for seq in owed {
        land(&mut st, replay_env(seq));
    }
    assert!((1..=top).all(|s| accepted(&st, s)));
    let chats: Vec<String> = bodies(&st);
    let want: Vec<String> = replay_chats(top, &[]).into_iter().filter(|b| {
        b.strip_prefix('m').and_then(|n| n.parse::<u64>().ok()).map_or(true, |n| n > 10)
    }).filter(|b| b != "base").collect();
    assert_eq!(chats, want, "after the restart: the rest, once, in order");
}

/// A sender that answers gap wants re-publishes for other askers too: its
/// duplicates say nothing about our gap.
#[test]
fn an_answering_senders_duplicates_are_no_evidence() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, request_env(3, 2));
    land(&mut st, chat_env(4, 3, "held"));
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), Vec::new());
    land(&mut st, request_env(3, 2)); // a resend for another asker
    at(&mut st, 20);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(bodies(&st), ["base"], "still waiting for the asked seq");
}

/// Gap 30: a sender that answered once, then leaves our asks unanswered,
/// is again judged by its rounds (§10(c)) - a tail held at the cap behind a
/// predecessor no round re-offers heals on the next round, not on a listing
/// that never comes.
#[tokio::test]
async fn a_sender_whose_hold_has_answered_still_self_heals_a_cap_held_tail() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(2, 1, "second"));
    let tail = 2 + molt_core::ACCEPT_WINDOW_BITS + 500;
    land(&mut st, request_env(tail, tail - 1)); // its prev was acked before a restart
    assert!(st.inbound_held_has("bob", tail), "held at the cap");
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), Vec::new());
    attach_dead_group(&mut st, false);
    for secs in [2, 12, 22, 32] {
        at(&mut st, secs);
        st.cmd_net_delivery_tick().expect("tick");
    }
    at(&mut st, 40);
    land(&mut st, request_env(tail, tail - 1)); // the round re-offers the tail
    at(&mut st, 40 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert!(accepted(&st, tail), "the round healed the tail: frontier {}", st.delivery.accepted["bob"].frontier);
}

/// The safety the flag keeps: while the sender answers our asks - even
/// only every second one - it is filling the gap, and a repeat (a resend
/// for another asker) never moves the frontier over the owed seq.
#[tokio::test]
async fn an_answering_sender_in_progress_never_lets_a_repeat_jump_the_frontier() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, request_env(3, 2));
    land(&mut st, chat_env(4, 3, "held"));
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), Vec::new());
    attach_dead_group(&mut st, false);
    for secs in [2, 12, 22] {
        at(&mut st, secs);
        st.cmd_net_delivery_tick().expect("tick");
        if secs == 12 {
            st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), Vec::new());
        }
    }
    land(&mut st, request_env(3, 2)); // a resend for another asker
    at(&mut st, 22 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(st.delivery.accepted["bob"].frontier, 1, "2 stays owed");
    assert!(!accepted(&st, 2));
    land(&mut st, chat_env(2, 1, "late"));
    assert_eq!(st.delivery.accepted["bob"].frontier, 4, "the real 2 closes it");
}

/// More than the hold's cap behind one gap: nothing is shed - the overflow
/// is delivered (out of order) instead of being dropped onto a resend that
/// may never come.
#[test]
fn a_flood_behind_a_gap_loses_nothing() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    for seq in 3..=702 {
        land(&mut st, chat_env(seq, seq - 1, &format!("m{seq}")));
    }
    land(&mut st, chat_env(2, 1, "m2"));
    assert_eq!(st.chat.len(), 702, "every message landed exactly once");
    for seq in 2..=702 {
        assert!(st.delivery.accepted["bob"].is_accepted(seq), "seq {seq} accepted");
    }
}

/// G5: a want is answered only for this node's own stream and only from a
/// roster member.
#[test]
fn a_gap_want_is_ours_only_from_the_roster_for_our_stream() {
    let st = presence_fixture();
    let s = |x: &str| x.to_string();
    assert!(st.gap_want_is_ours(&s("bob"), &s("ada")));
    assert!(!st.gap_want_is_ours(&s("mallory"), &s("ada")), "not on the roster");
    assert!(!st.gap_want_is_ours(&s("bob"), &s("cid")), "a third member's stream");
    assert!(!st.gap_want_is_ours(&s("ada"), &s("ada")), "our own want echoed");
}

/// G1 evidence: a seq accepted in an earlier session arriving again (a
/// stored frame re-read after a restart from a ratchet persisted late) is
/// no resend round - the frontier stays below the hole.
#[test]
fn a_frame_re_read_after_a_restart_is_no_round() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    for seq in 2..=4 {
        land(&mut st, request_env(seq, seq - 1));
    }
    land(&mut st, request_env(6, 5));
    let saved = st.delivery.accepted.clone();
    assert_eq!(saved["bob"].frontier, 4);
    let mut st = presence_fixture();
    st.delivery.accepted = saved;
    land(&mut st, request_env(6, 5)); // re-read after the restart, not re-sent
    at(&mut st, 60);
    st.cmd_net_delivery_tick().expect("tick");
    assert!(!accepted(&st, 5));
    assert_eq!(st.delivery.accepted["bob"].frontier, 4, "the frontier stays below the hole");
    land(&mut st, request_env(5, 4));
    assert_eq!(st.delivery.accepted["bob"].frontier, 6, "the real 5 closes it");
}

/// G1 evidence: a round seen only from its middle (cut short) proves
/// nothing above its lowest re-offer - only the hole below it is given up.
#[test]
fn a_cut_round_passes_only_what_lies_below_it() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, request_env(3, 2));
    land(&mut st, request_env(4, 3));
    land(&mut st, request_env(6, 5));
    at(&mut st, 10);
    land(&mut st, request_env(4, 3)); // the round re-offers 4, then stops
    at(&mut st, 10 + crate::net::GAP_PASS_GRACE_SECS);
    st.cmd_net_delivery_tick().expect("tick");
    assert_eq!(st.delivery.accepted["bob"].frontier, 4, "2 given up, 5 still owed");
    assert!(!accepted(&st, 5));
    land(&mut st, request_env(5, 4));
    assert_eq!(st.delivery.accepted["bob"].frontier, 6);
}

/// B7 behind an inbound gap: an intent is a stance (an `Approved` without
/// a signature) and waits like one; the five lock frames carry their
/// ballot and are idempotent in any order, so none of them waits.
#[test]
fn intents_wait_behind_a_gap_and_lock_frames_never_do() {
    use molt_core::{ProposalId, WorkspaceEvent};
    use super::delivery::order_sensitive;
    let intent = WorkspaceEvent::Approved { id: ProposalId(1), by: "a".into(), height: 1, sig: String::new() };
    assert!(order_sensitive(&intent));
    let base = "ab".to_string();
    let lock = [
        WorkspaceEvent::LockPrepare { height: 1, base: base.clone(), round: 1 },
        WorkspaceEvent::LockPromise { height: 1, base: base.clone(), round: 1, leader: "a".into(), accepted: None },
        WorkspaceEvent::LockAccept {
            height: 1,
            base: base.clone(),
            round: 1,
            digest: "d".into(),
            need: 2,
            id: ProposalId(1),
            change: None,
            forced: false,
            evidence: Vec::new(),
        },
        WorkspaceEvent::LockAccepted {
            height: 1,
            base: base.clone(),
            round: 1,
            leader: "a".into(),
            digest: "d".into(),
            sig: "s".into(),
        },
        WorkspaceEvent::LockNack { height: 1, base, round: 1, leader: "a".into(), promised: None, head: 0 },
    ];
    assert!(lock.iter().all(|f| !order_sensitive(f)));
}

fn bob_env(seq: u64, prev: u64, body: molt_core::WorkspaceEvent) -> molt_core::EventEnvelope {
    molt_core::EventEnvelope { seq, ts: T, by: "bob".to_string(), body, prev_seq: prev }
}

/// B7 behind a real gap (the frontier, 2026-11-22): a lock frame behind a
/// missing seq is accepted at once; an intent (an unsigned `Approved`)
/// waits like any stance until the hole closes. Far ahead of the frontier
/// (more than the accept window) a lock frame is buffered like every
/// envelope and applied bottom-up - stale by then and ignored, never
/// reordered into a decision.
#[test]
fn behind_a_gap_a_lock_frame_passes_and_an_intent_waits() {
    use molt_core::{ProposalId, WorkspaceEvent};
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, bob_env(3, 2, WorkspaceEvent::LockPrepare { height: 1, base: "ab".into(), round: 1 }));
    assert!(accepted(&st, 3), "a lock frame does not wait for seq 2");
    let intent = WorkspaceEvent::Approved { id: ProposalId(1), by: "bob".into(), height: 1, sig: String::new() };
    land(&mut st, bob_env(4, 3, intent));
    assert!(!accepted(&st, 4), "the intent waits behind the hole");
    land(&mut st, chat_env(2, 1, "the hole"));
    assert!(accepted(&st, 4), "the hole closed, the intent applied");
    let far = 3 + 2000;
    land(&mut st, bob_env(far, far - 1, WorkspaceEvent::LockPrepare { height: 9, base: "ab".into(), round: 1 }));
    assert!(!accepted(&st, far), "far ahead of the frontier even a lock frame is buffered");
}

/// D-F3: a lock frame is unowed, so it shares its `prev_seq` with the
/// next owed envelope. Arriving after that envelope above a gap, it must
/// not take the envelope's link: the frontier reaches the owed envelope
/// once the hole closes.
#[test]
fn an_unowed_lock_frame_never_takes_an_owed_link() {
    use molt_core::WorkspaceEvent;
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    // seq 2 is missing; the owed seq 4 and the unowed seq 3 both name it
    land(&mut st, bob_env(4, 2, WorkspaceEvent::Withdrawn { id: molt_core::ProposalId(7), by: "bob".into() }));
    assert!(accepted(&st, 4), "an owed, order-free envelope is accepted above the gap");
    land(&mut st, bob_env(3, 2, WorkspaceEvent::LockPrepare { height: 1, base: "ab".into(), round: 1 }));
    land(&mut st, chat_env(2, 1, "the hole"));
    let frontier = st.delivery.accepted["bob"].frontier;
    assert!(frontier >= 4, "the frontier passes the owed envelope: {frontier}");
}

/// The spent resend budget has its own field: it names the owed seats in
/// the health and the status, keeps its start when the seats change,
/// leaves a real publish failure alone, clears on an empty report and
/// never counts toward the locked-out verdict.
#[test]
fn a_held_resend_is_its_own_note_and_never_a_lockout() {
    let mut st = presence_fixture();
    at(&mut st, 0);
    let healthy = molt_net::group_runtime::GroupHealth { subscribed: true, ..Default::default() };
    st.delivery.send_stuck.insert("ada".to_string(), "no relay accepted the frame".to_string());
    st.cmd_net_resend_held(vec!["bob".to_string()], None).expect("ack");
    st.apply_group_health(healthy.clone());
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains("resend budget spent, owed: bob"), "{reason}");
            assert!(reason.contains("no relay accepted"), "the publish failure stays: {reason}");
        }
        other => panic!("{other:?}"),
    }
    let status = st.status();
    assert_eq!((status.resend_held, status.resend_held_since), (vec!["bob".to_string()], T));
    at(&mut st, 50);
    st.cmd_net_resend_held(vec!["bob".to_string(), "cid".to_string()], None).expect("ack");
    assert_eq!(st.delivery.resend_held, Some((vec!["bob".to_string(), "cid".to_string()], T)), "the start stays");
    st.delivery.send_stuck.clear();
    st.apply_group_health(molt_net::group_runtime::GroupHealth { live_opaque: 9, ..healthy.clone() });
    assert!(!st.delivery.locked_out_noted, "a spent budget is no lockout");
    st.cmd_net_resend_held(Vec::new(), None).expect("ack");
    st.apply_group_health(healthy);
    assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
    assert!(st.status().resend_held.is_empty());
}

/// A listing wider than the asked range is refused, and a stale page
/// never replaces the newer one already held.
#[test]
fn a_lying_or_stale_listing_changes_nothing() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, replay_env(3));
    let (bob, ada) = ("bob".to_string(), "ada".to_string());
    st.cmd_net_gap_listed(&bob, &ada, 1, 2 + molt_core::ACCEPT_WINDOW_BITS, vec![2, 3]);
    assert_eq!(st.listing_through_for_test("bob"), None, "a range wider than a window is refused");
    st.cmd_net_gap_listed(&bob, &ada, 1, 400, vec![2, 3, 400]);
    st.cmd_net_gap_listed(&bob, &ada, 1, 200, vec![2, 3]);
    assert_eq!(st.listing_through_for_test("bob"), Some(400), "the stale page did not replace the newer one");
}

/// With the probe off (v0.1.3), a listed hole settles at the 60 s order
/// bound exactly as in v0.1.2; with it on, a listed hole holds - but a
/// parked (declined) one never holds its sender's stream.
#[test]
fn the_switch_off_keeps_v0_1_2_gap_timing_and_a_parked_seq_never_holds() {
    let listed_hole = |force: bool, park: bool| {
        let mut st = presence_fixture();
        st.drop_seam.force_probe.store(force, std::sync::atomic::Ordering::SeqCst);
        land(&mut st, chat_env(1, 0, "base"));
        land(&mut st, chat_env(3, 2, "held"));
        let (bob, ada) = ("bob".to_string(), "ada".to_string());
        st.cmd_net_gap_listed(&bob, &ada, 1, 3, vec![2, 3]);
        if park {
            st.park_for_test("bob", 2);
        }
        at(&mut st, crate::net::GAP_FALLBACK_SECS - 1);
        st.cmd_net_delivery_tick().expect("tick");
        assert_eq!(bodies(&st), ["base"], "held before the bound");
        at(&mut st, crate::net::GAP_FALLBACK_SECS);
        st.cmd_net_delivery_tick().expect("tick");
        bodies(&st)
    };
    assert_eq!(listed_hole(false, false), ["base", "held"], "off: settled at the bound");
    assert_eq!(listed_hole(true, false), ["base"], "on: a listed hole holds");
    assert_eq!(listed_hole(true, true), ["base", "held"], "a parked seq does not hold");
}

/// H1, the asker's side against an older sender: a `gone` for a seq
/// outside `(frontier, cap]` is no answer - the seq stays owed.
#[test]
fn a_gone_outside_the_asked_window_is_ignored() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(3, 2, "held"));
    let far = 2 + molt_core::ACCEPT_WINDOW_BITS + 500;
    land(&mut st, chat_env(far + 1, far, "far"));
    let missing = |st: &crate::State| st.inbound_held_view().iter().map(|h| h.missing).sum::<u32>();
    assert_eq!(missing(&st), 2, "2 and the far prev");
    st.cmd_net_gap_answered("bob".to_string(), "ada".to_string(), Vec::new(), vec![2, far]);
    assert_eq!(bodies(&st), ["base", "held"], "the asked seq is settled");
    assert_eq!(missing(&st), 1, "the far one stays owed");
}

/// The 2026-11-27 flood deadlock (gap 25): ~1,700 entries of the sender's
/// log sit between two envelopes owed to this seat - entries that never
/// cross the wire (re-recorded peer traffic). The next owed one arrives
/// ~1,700 positions above the accept cap, but its `prev_seq` IS an accepted
/// seq. The prev chain proves nothing owed hides between prev and seq, so
/// the envelope is accepted, the frontier moves to it, nothing between is
/// marked missing, the ack sheet's high rises and the sender's acknowledged
/// floor follows - no re-offer round (the keystone over the loopback hub
/// pins that last part end to end).
#[test]
fn an_envelope_chained_to_an_accepted_prev_passes_the_cap() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(2, 1, "second"));
    let far = 2 + 1700;
    land(&mut st, chat_env(far, 2, "after the flood"));
    let w = st.delivery.accepted["bob"].clone();
    assert!(w.is_accepted(far), "accepted although above frontier + window");
    assert_eq!(w.high, far, "the ack sheet's high rises");
    assert_eq!(w.frontier, far, "the frontier moved to it");
    assert!(st.inbound_held_view().is_empty(), "nothing held, nothing missing");
    // the sheet's high is what the sender's floor reads (§4.4): that it
    // follows past the flood's unowed span is the keystone's assertion
    // (`an_envelope_chained_to_an_accepted_prev_crosses_a_log_flood_
    // without_a_re_offer`)
}

/// The protection stays pinned: without an accepted prev (`prev_seq == 0`,
/// or a prev the window never took) an envelope above the cap is held as
/// before, and a duplicate or already-held one is refused without a second
/// delivery. Everything else about the hold is untouched - the byte shed
/// (`a_shed_envelope_stays_owed_and_is_recovered`) and the session ring
/// (`SESSION_MAX`) among it.
#[test]
fn a_far_envelope_without_an_accepted_prev_stays_held() {
    let mut st = presence_fixture();
    land(&mut st, chat_env(1, 0, "base"));
    land(&mut st, chat_env(2, 1, "second"));
    let far = 2 + molt_core::ACCEPT_WINDOW_BITS + 500;
    land(&mut st, chat_env(far, 0, "no chain"));
    assert!(!accepted(&st, far), "prev_seq 0 is never proof");
    assert!(st.inbound_held_has("bob", far));
    land(&mut st, chat_env(far + 1, 500, "chained into the void"));
    assert!(!accepted(&st, far + 1), "an unaccepted prev is no proof");
    assert!(st.inbound_held_has("bob", far + 1));
    land(&mut st, chat_env(far, 0, "no chain"));
    assert_eq!(bodies(&st), ["base", "second"], "the duplicate is refused again");
}

/// Visibility (the 2026-11-27 deadlock ran silent for 4 hours behind one
/// INFO line): a hold stuck for minutes repeats a WARN at a bounded rate -
/// first at 120 s, then every 600 s - naming held, frontier and how long.
#[test]
fn a_long_hold_warns_again_at_a_bounded_rate() {
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut b) = self.0.lock() {
                b.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || sink.clone()).finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut st = presence_fixture();
        land(&mut st, chat_env(1, 0, "base"));
        land(&mut st, chat_env(2, 1, "second"));
        land(&mut st, chat_env(2000, 1999, "held"));
        for secs in [119, 120, 121, 719, 720] {
            at(&mut st, secs);
            st.cmd_net_delivery_tick().expect("tick");
        }
    });
    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    let warns: Vec<&str> = logs.lines().filter(|l| l.contains("inbound_held_stuck")).collect();
    assert_eq!(warns.len(), 2, "{logs}");
    assert!(
        warns[0].contains("held=1") && warns[0].contains("frontier=2") && warns[0].contains("since_s=120"),
        "{}",
        warns[0]
    );
    assert!(warns[1].contains("since_s=720"), "{}", warns[1]);
}

/// A runtime rebuild drops a spent-budget note: the new outbox reports
/// afresh.
#[test]
fn a_rebuild_drops_the_held_resend_note() {
    let mut st = presence_fixture();
    st.cmd_net_resend_held(vec!["bob".to_string()], None).expect("ack");
    assert!(st.delivery.resend_held.is_some());
    st.rebuild_group_net_for_test();
    assert!(st.delivery.resend_held.is_none());
}
