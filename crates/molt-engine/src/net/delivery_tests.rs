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
    assert!(st.inbound_held_view().is_empty(), "nothing is held any more");

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
