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
