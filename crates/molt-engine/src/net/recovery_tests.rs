// SPDX-License-Identifier: GPL-3.0-or-later

//! Coordinator-side recovery: the link-mint failure report and the
//! announce cooldown.

use super::test_support::*;

/// The provisioning task's failure report lands as the calm
/// `recovery-link-failed:` session notice (the same channel the minted
/// link rides), and the dead mint's ticket is unregistered — nothing of
/// the failed attempt stays armed.
#[test]
fn a_recover_link_failure_report_sets_the_notice_and_kills_the_ticket() {
    let mut st = crate::tests::plain_state();
    st.recovery.tickets.insert("t-1".to_string(), "bob".to_string());
    st.cmd_net_recover_link_failed(
        "bob".to_string(),
        "boom".to_string(),
        "t-1".to_string(),
        None,
    )
    .expect("the report acks");
    assert_eq!(st.session.notice, "recovery-link-failed:boom");
    assert!(
        st.recovery.tickets.is_empty(),
        "the failed mint's ticket must not stay armed"
    );
}

/// V1 (delivery_guarantee.md): an announce that carries no queue for THIS
/// node must not burn the announcer's extension cooldown — the follow-up
/// announce that IS for us (moments later) must still adopt. A repeated
/// VALID announce inside the window stays capped as before.
#[test]
fn an_announce_without_our_queue_does_not_burn_the_cooldown() {
    let mut st = presence_fixture();
    st.presence.clock_override = Some(T);
    let handover = |queue: &str| molt_net::mesh::QueueHandover {
        server: String::new(),
        queue: queue.to_string(),
        wrap: hex::encode([7u8; 32]),
    };
    // bob's announce reaches ada without any queue for ada
    let mut queues = std::collections::BTreeMap::new();
    queues.insert("cid".to_string(), handover("aa"));
    let for_cid = molt_net::mesh::MeshAnnounce { queues };
    st.spawn_mesh_extension("bob".to_string(), &for_cid);
    assert!(
        !st.recovery.mesh_extension_at.contains_key("bob"),
        "an announce carrying nothing for us must not stamp the cooldown"
    );
    // moments later bob's announce FOR ada arrives — it must pass the gate
    st.presence.clock_override = Some(T + 5);
    let mut queues = std::collections::BTreeMap::new();
    queues.insert("ada".to_string(), handover("bb"));
    let for_ada = molt_net::mesh::MeshAnnounce { queues };
    st.spawn_mesh_extension("bob".to_string(), &for_ada);
    assert_eq!(
        st.recovery.mesh_extension_at.get("bob"),
        Some(&(T + 5)),
        "the announce for us passes the cooldown gate and stamps it"
    );
    // a REPEATED valid announce inside the window is still ignored (churn cap)
    st.presence.clock_override = Some(T + 10);
    st.spawn_mesh_extension("bob".to_string(), &for_ada);
    assert_eq!(
        st.recovery.mesh_extension_at.get("bob"),
        Some(&(T + 5)),
        "a rapid repeat stays capped - the stamp is not refreshed"
    );
}
