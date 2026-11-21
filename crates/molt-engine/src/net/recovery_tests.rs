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

fn attempt(member: &str, touched: tokio::time::Instant) -> crate::chain::AnchorFirst {
    crate::chain::AnchorFirst {
        member: member.to_string(),
        attempt: String::new(),
        started: 0,
        identity_pk: String::new(),
        key_package: String::new(),
        seat_proof: String::new(),
        new_nostr_pk: "ab".repeat(32),
        relays: Vec::new(),
        consent: String::new(),
        reply: String::new(),
        frames: vec!["{}".to_string()],
        resent_at: None,
        tasks: Vec::new(),
        opened: touched,
        touched,
        logged_at: None,
        counts: std::sync::Arc::default(),
    }
}

/// An attempt that goes silent is bounded in memory, not in time: past
/// the cap the least recently heard rejoiner's attempt goes, and its
/// next Ready is answered `link outdated`.
#[test]
fn open_anchor_attempts_are_bounded_by_count() {
    let mut st = crate::tests::plain_state();
    let t0 = tokio::time::Instant::now();
    let cap = super::recovery::MAX_ANCHOR_ATTEMPTS;
    for i in 0..=cap {
        let at = t0 + std::time::Duration::from_secs(u64::try_from(i).expect("small"));
        st.recovery.attempts.note_minted(&format!("t{i}"));
        st.admit_anchor_attempt(&format!("t{i}"), attempt(&format!("m{i}"), at));
    }
    assert_eq!(st.recovery.anchor_first.len(), cap);
    assert!(!st.recovery.anchor_first.contains_key("t0"), "the least recently heard went");
    st.cmd_net_recover_ready("m0", "t0", "", &"ab".repeat(32), None).expect("ready");
    assert_eq!(st.recovery.anchor_first.len(), cap, "an evicted attempt proposes nothing");
    assert!(
        st.recovery.unsolicited_cooldown.keys().any(|(k, _)| k.starts_with("outdated:m0:")),
        "answered link outdated"
    );
}

/// The coordinator says once a minute what an open attempt waits for.
#[test]
fn an_open_attempt_logs_its_wait_every_minute() {
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
        let mut st = crate::tests::plain_state();
        let t0 = tokio::time::Instant::now();
        let a = attempt("petra", t0);
        a.counts.sent.store(3, std::sync::atomic::Ordering::Relaxed);
        a.counts.accepted.store(2, std::sync::atomic::Ordering::Relaxed);
        st.admit_anchor_attempt("t", a);
        st.recover_wait_tick(t0 + std::time::Duration::from_secs(61));
        st.recover_wait_tick(t0 + std::time::Duration::from_secs(91));
        st.recover_wait_tick(t0 + std::time::Duration::from_secs(121));
    });
    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    let lines: Vec<&str> = logs.lines().filter(|l| l.contains("recover_wait")).collect();
    assert_eq!(lines.len(), 2, "{logs}");
    assert!(
        lines[0].contains("for=\"ready\" member=petra since_s=61 sent=3 accepted=2"),
        "{}",
        lines[0]
    );
}

/// A seat restored anchor-first holds its anchor: a coordinator restart,
/// even a hard kill, must not turn on the group re-push for it.
#[test]
fn an_anchor_first_seat_survives_a_restart() {
    for with_snapshot in [false, true] {
        let b = crate::chain::test_support::Builder::new(&["petra", "walter"], 2);
        let (mut walter, _tmp, dir) = crate::tests::stored_chain_signer(&b, "walter", &["petra", "walter"]);
        let seat = ("petra".to_string(), "ab".repeat(32));
        walter.note_anchor_first_seat(seat.clone());
        let back = crate::tests::reopen_chain_signer(walter, &dir, &b, "walter", with_snapshot);
        assert!(back.recovery.anchor_first_seats.contains(&seat), "with_snapshot={with_snapshot}");
    }
}

/// Review R4: a Want/Ready for a ticket this node never minted is a
/// prober or a replay - dropped, no answer, no cooldown entry.
#[test]
fn a_want_for_a_ticket_never_minted_is_not_answered() {
    let mut st = crate::tests::plain_state();
    let sender = "ab".repeat(32);
    st.cmd_net_recover_want("petra", "never-minted", &[0], "a1", &sender, None).expect("want");
    st.cmd_net_recover_ready("petra", "never-minted", "a1", &sender, None).expect("ready");
    assert!(st.recovery.unsolicited_cooldown.is_empty(), "nothing answered");
    st.recovery.attempts.note_minted("minted-here");
    st.cmd_net_recover_ready("petra", "minted-here", "a1", &sender, None).expect("ready");
    assert_eq!(st.recovery.unsolicited_cooldown.len(), 1, "a ticket minted here is answered");
}

/// Review R4: the answer map is capped on every insert.
#[test]
fn the_answer_map_is_capped() {
    let mut st = crate::tests::plain_state();
    for i in 0..1_000 {
        assert!(st.answer_once((format!("k{i}"), "s".to_string())));
    }
    assert!(st.recovery.unsolicited_cooldown.len() <= 256, "{}", st.recovery.unsolicited_cooldown.len());
    assert!(!st.answer_once(("k999".to_string(), "s".to_string())), "the newest is still remembered");
}

/// Review R4: a minted ticket's fingerprint - never the ticket - survives
/// a restart, so its Want/Ready still earns `link outdated`.
#[test]
fn a_minted_ticket_survives_a_restart() {
    for with_snapshot in [false, true] {
        let b = crate::chain::test_support::Builder::new(&["petra", "walter"], 2);
        let (mut walter, _tmp, dir) = crate::tests::stored_chain_signer(&b, "walter", &["petra", "walter"]);
        walter.recovery.attempts.note_minted("t-secret");
        walter.snapshot_to_disk();
        let back = crate::tests::reopen_chain_signer(walter, &dir, &b, "walter", with_snapshot);
        assert!(back.recovery.attempts.minted("t-secret"), "with_snapshot={with_snapshot}");
        assert!(!back.recovery.attempts.minted.iter().any(|p| p.contains("t-secret")));
    }
}
