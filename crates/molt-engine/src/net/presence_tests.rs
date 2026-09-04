// SPDX-License-Identifier: GPL-3.0-or-later

//! Real presence: numeric stamps, aging, the activity trio, the send/link
//! pins and the net-health verdict.

use super::test_support::*;
use molt_core::{ChatMessage, EventEnvelope, MemberInfo, WorkspaceEvent};

/// A peer sighting stamps the member with the engine clock's REAL unix
/// time, and the activity trio counts it in every window it falls into
/// (ada, the local member, always counts — it is the one reading).
#[test]
fn a_peer_sighting_stamps_the_real_clock_and_feeds_the_trio() {
    let mut st = presence_fixture();
    st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
    let bob = pill(&st, "bob");
    assert_eq!(bob.last_seen, T, "the stamp is the engine clock's time");
    assert_eq!(bob.state, 0, "a fresh sighting is online");
    let s = st.status();
    assert_eq!((s.active_1h, s.active_24h, s.active_7d), (2, 2, 2));
    // two hours of silence: bob leaves the 1h window by pure clock
    // advance — no event needed, the trio reads the stamps
    st.presence.clock_override = Some(T + 7_200);
    let s = st.status();
    assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 2, 2));
    // eight days of silence: bob leaves every window
    st.presence.clock_override = Some(T + 8 * 86_400);
    let s = st.status();
    assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 1, 1));
}

/// This node never hears itself on the wire, so its own stamp would
/// age out — but it is the one running the app: self stays online
/// through every aging pass and read, and always counts in the trio.
#[test]
fn the_local_member_stays_online_through_aging() {
    let mut st = presence_fixture(); // ada is the local member
    // long after every threshold, with no traffic at all
    st.presence.clock_override = Some(T + 30 * 86_400);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "ada").state, 0, "self never ages offline");
    let ada = st
        .members_view()
        .into_iter()
        .find(|m| m.member == "ada")
        .expect("ada row");
    assert_eq!(ada.presence, 0, "the Members table shows self online");
    let s = st.status();
    assert_eq!(s.active_1h, 1, "self always counts active");
}

/// Stage B honest health: the supervisor's link/send signals drive
/// `session.net_health` Ok → Degraded (reason naming every troubled
/// peer) → Ok, and only when BOTH legs are clear again.
#[test]
fn link_and_send_signals_drive_ok_degraded_ok() {
    let mut st = presence_fixture();
    assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
    st.cmd_net_link_down("bob".to_string(), "subscription ended".to_string(), None)
        .expect("ack");
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains("bob"), "names the peer: {reason}");
            assert!(reason.contains("subscription ended"), "carries the cause: {reason}");
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
    // a stuck outbox on ANOTHER peer joins the reason
    st.cmd_net_send_failed("cid".to_string(), "SKEY rejected: ERR AUTH".to_string(), None)
        .expect("ack");
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains("bob") && reason.contains("cid"), "{reason}");
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
    // heal one leg: bob's subscription is back AND it delivers a frame (so
    // the leg is verified, not merely live-but-unverified) — still degraded
    // because the OTHER peer's outbox is stuck
    st.cmd_net_link_up("bob".to_string(), None).expect("ack");
    st.cmd_net_peer_seen("bob".to_string(), None).expect("bob delivers - leg verified");
    assert!(
        matches!(st.session.net_health, molt_core::NetHealth::Degraded { .. }),
        "cid's outbox is still stuck"
    );
    // heal the second: honest Ok again
    st.cmd_net_send_ok("cid".to_string(), None).expect("ack");
    assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
}

/// §6.5 (N5.5): presence over relays is traffic-derived and COARSE —
/// silence is not absence. On a Nostr workspace a stamped member ages
/// to stale and STAYS there; only never-heard shows dark. The mesh
/// keeps its keepalive-backed aging (a silent mesh member really is
/// unreachable — its keepalives stopped).
#[test]
fn a_quiet_nostr_republic_shows_last_seen_not_offline() {
    let mut st = presence_fixture();
    st.cmd_net_peer_seen("bob".to_string(), None).expect("stamp bob");
    // a quiet weekend later, on a MESH workspace: bob is honestly offline
    st.presence.clock_override = Some(T + 3 * 86_400);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 2, "mesh aging is unchanged");
    // the same silence on a NOSTR workspace: coarse, not dark
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![7u8; 32]),
        relays: vec!["ws://relay.example".to_string()],
        rotation_seed: [0u8; 32],
    });
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 1, "a stamped member is stale, never dark");
    assert_eq!(
        st.presence_of("bob", T, st.presence_now()),
        1,
        "the shared derivation agrees (co-equality)"
    );
    // …but a member NEVER heard from is honestly dark
    assert_eq!(pill(&st, "cid").state, 2, "never-heard stays dark");
}

/// The coarse-Nostr lift covers a QUIET republic, not an absent seat.
/// Since the founding date became a real stamp (nobody reads back as
/// never-seen), "stamped" alone would paint a member gone for months
/// the same yellow as one heard from this morning - so the lift ends
/// with [`MemberInfo::COARSE_SECS`] and the dot goes dark again.
#[test]
fn a_seat_silent_past_the_coarse_window_goes_dark_again() {
    let mut st = presence_fixture();
    st.cmd_net_peer_seen("bob".to_string(), None).expect("stamp bob");
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![7u8; 32]),
        relays: vec!["ws://relay.example".to_string()],
        rotation_seed: [0u8; 32],
    });
    // inside the window: coarse, not dark (the quiet-republic case)
    st.presence.clock_override = Some(T + MemberInfo::COARSE_SECS - 60);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 1, "a quiet week is still stale");
    // past it: this is not silence any more, it is absence
    st.presence.clock_override = Some(T + MemberInfo::COARSE_SECS + 60);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 2, "months of silence must read dark");
    assert_eq!(
        st.presence_of("bob", T, st.presence_now()),
        2,
        "the shared derivation agrees (co-equality)"
    );
}

/// N5.4 (G4 epoch-ring honesty) + N5.5: on a Nostr workspace the health
/// verdict is the GROUP CHANNEL's — relays, not members. A deaf channel
/// degrades with the relay reason; frames past the exporter ring are a
/// PERMANENT, named loss; a dead subscription is Down; a healthy
/// channel is an honest Ok again.
#[test]
fn group_channel_health_names_relays_and_ring_losses() {
    let mut st = presence_fixture();
    let h = |subscribed: bool, deaf: Option<&str>, opaque: u64| {
        molt_net::group_runtime::GroupHealth {
            subscribed,
            deaf: deaf.map(|s| s.to_string()),
            opaque_frames: opaque,
        }
    };
    st.apply_group_health(h(true, Some("relay ws://r refused the sub"), 0));
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains("relay"), "names the relay trouble: {reason}");
        }
        other => panic!("deaf must degrade, got {other:?}"),
    }
    // the deafness heals — honest Ok again
    st.apply_group_health(h(true, None, 0));
    assert_eq!(st.session.net_health, molt_core::NetHealth::Ok);
    // G4: a frame older than the exporter ring is unreadable BY
    // CONSTRUCTION — a named permanent loss, never silence
    st.apply_group_health(h(true, None, 3));
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains('3') && reason.contains("key ring"), "{reason}");
        }
        other => panic!("ring losses must be loud, got {other:?}"),
    }
    // a dead subscription cannot heal itself — Down, not Degraded
    st.apply_group_health(h(false, Some("subscribe: connection refused"), 0));
    assert!(
        matches!(st.session.net_health, molt_core::NetHealth::Down { .. }),
        "a dead inbox is Down: {:?}",
        st.session.net_health
    );
}

/// The group verdict also carries a stuck outbox (send_failed on
/// broadcast names no peer — the trouble is the channel).
#[test]
fn a_stuck_group_outbox_joins_the_channel_verdict() {
    let mut st = presence_fixture();
    st.delivery.send_stuck
        .insert("ada".to_string(), "no relay accepted the frame".to_string());
    st.apply_group_health(molt_net::group_runtime::GroupHealth {
        subscribed: true,
        deaf: None,
        opaque_frames: 0,
    });
    match &st.session.net_health {
        molt_core::NetHealth::Degraded { reason } => {
            assert!(reason.contains("no relay accepted"), "{reason}");
        }
        other => panic!("a stuck outbox must surface, got {other:?}"),
    }
}

/// `Down` is the open/config path's verdict (fail-closed dialer,
/// detached reopen) — runtime link signals must never lift it.
#[test]
fn a_down_verdict_is_never_lifted_by_link_signals() {
    let mut st = presence_fixture();
    st.session.net_health = molt_core::NetHealth::Down {
        reason: "resume failed - workspace opened detached".to_string(),
    };
    st.cmd_net_link_down("bob".to_string(), "x".to_string(), None).expect("ack");
    assert!(matches!(st.session.net_health, molt_core::NetHealth::Down { .. }));
    st.cmd_net_link_up("bob".to_string(), None).expect("ack");
    st.cmd_net_send_ok("bob".to_string(), None).expect("ack");
    assert!(
        matches!(st.session.net_health, molt_core::NetHealth::Down { .. }),
        "link signals must never lift a Down verdict"
    );
}

/// Link/send-stuck state is scoped to the workspace: the close/switch
/// boundary clears it (like the send-failure presence pins), so the
/// next workspace never inherits a Degraded pill.
#[test]
fn link_state_does_not_leak_past_a_workspace_reset() {
    let mut st = presence_fixture();
    st.cmd_net_link_down("bob".to_string(), "gone".to_string(), None).expect("ack");
    st.cmd_net_send_failed("cid".to_string(), "gone".to_string(), None).expect("ack");
    assert!(!st.delivery.link_down.is_empty() && !st.delivery.send_stuck.is_empty());
    st.reset_workspace_state();
    assert!(
        st.delivery.link_down.is_empty() && st.delivery.send_stuck.is_empty(),
        "the close/switch boundary clears the link state"
    );
}

/// A send-failure pin is scoped to the workspace: closing/resetting the
/// workspace drops it, so a same-named member in the next workspace is
/// not falsely shown unreachable.
#[test]
fn a_send_failure_pin_does_not_leak_past_a_workspace_reset() {
    let mut st = presence_fixture();
    st.cmd_net_send_failed("bob".to_string(), "gone".to_string(), None)
        .expect("ack");
    assert!(st.delivery.unreachable.contains("bob"));
    st.reset_workspace_state();
    assert!(
        st.delivery.unreachable.is_empty(),
        "the close/switch boundary clears the pins"
    );
}

/// A stuck BROADCAST outbox (the group runtime names the own seat)
/// flags the channel, never the operator's own presence: this node is
/// running, and no sighting could ever lift a pin on itself.
#[test]
fn a_stuck_broadcast_outbox_never_pins_the_own_seat_offline() {
    let mut st = presence_fixture();
    st.cmd_net_send_failed(
        "ada".to_string(),
        "no relay accepted the frame".to_string(),
        None,
    )
    .expect("ack");
    assert!(
        st.delivery.send_stuck.contains_key("ada"),
        "the channel trouble is recorded"
    );
    assert!(
        !st.delivery.unreachable.contains("ada"),
        "the own seat is never pinned unreachable"
    );
}

/// The presence ticker ages a silent member's pill: online → stale
/// after `ONLINE_SECS`, stale → offline after `STALE_SECS` — the stamp
/// itself never moves without real traffic.
#[test]
fn the_ticker_ages_a_silent_pill_stale_then_offline() {
    let mut st = presence_fixture();
    st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
    st.presence.clock_override = Some(T + MemberInfo::ONLINE_SECS + 1);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 1, "silence past ONLINE_SECS is stale");
    st.presence.clock_override = Some(T + MemberInfo::STALE_SECS + 1);
    st.cmd_net_presence_tick().expect("tick");
    let bob = pill(&st, "bob");
    assert_eq!(bob.state, 2, "silence past STALE_SECS is offline");
    assert_eq!(bob.last_seen, T, "aging never invents a sighting");
}

/// A member the transport never heard from stays honestly never-seen:
/// sentinel stamp, offline pill, counted in NO activity window — and
/// the ticker does not invent presence for it.
#[test]
fn a_member_without_traffic_stays_never_seen_and_counts_nowhere() {
    let mut st = presence_fixture();
    st.cmd_net_presence_tick().expect("tick");
    let cid = pill(&st, "cid");
    assert_eq!(cid.last_seen, MemberInfo::NEVER);
    assert_eq!(cid.state, 2);
    let view = st
        .members_view()
        .into_iter()
        .find(|m| m.member == "cid")
        .expect("cid row");
    assert_eq!(view.last_seen, MemberInfo::NEVER);
    assert_eq!(view.presence, 2);
    let s = st.status();
    // only ada (the local member) is active anywhere
    assert_eq!((s.active_1h, s.active_24h, s.active_7d), (1, 1, 1));
}

/// A silent workspace entry the operator has switched AWAY from must age
/// its pills too — the presence ticker cannot freeze a closed workspace's
/// members at "online" forever. Self-online and send-failure pins are
/// scoped to the ACTIVE workspace; a switched-away one ages purely from
/// each member's real stamp.
#[test]
fn a_switched_away_workspace_ages_out_instead_of_freezing_online() {
    let mut st = presence_fixture(); // active "w-presence" (ada/bob/cid)
    st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
    // a second workspace we last looked at when everyone was online
    // (fresh stamps), then switched away from and never touched again
    let closed_roster = vec!["ada".to_string(), "bob".to_string()];
    st.session.workspaces.push(molt_core::WorkspaceInfo {
        id: "w-closed".to_string(),
        name: "Closed".to_string(),
        detail: "1-of-2".to_string(),
        synced: false,
        state: 2,
        last_sync_min: 0,
        sync_queue: 0,
        s3: false,
        size_kib: 0,
        last_backup_min: molt_core::WorkspaceInfo::NEVER,
        backup_copies: 0,
        backup_error: String::new(),
        seed: String::new(),
        net: "none".to_string(),
        encrypted: false,
        members: molt_core::roster_members(&closed_roster, T, |_| T),
        agenda: String::new(),
        restored: false,
    });
    // 31 minutes of total silence pass everywhere
    st.presence.clock_override = Some(T + MemberInfo::STALE_SECS + 1);
    st.cmd_net_presence_tick().expect("tick");
    // the ACTIVE entry ages honestly (bob offline, ada self-online)
    assert_eq!(pill(&st, "bob").state, 2, "the active workspace's silent peer ages offline");
    assert_eq!(pill(&st, "ada").state, 0, "the active workspace keeps self online");
    // the CLOSED entry must age from its stamps, not freeze at online
    let closed = st
        .session
        .workspaces
        .iter()
        .find(|w| w.id == "w-closed")
        .expect("closed entry");
    let closed_pill = |name: &str| {
        closed.members.iter().find(|m| m.name == name).expect("closed pill").state
    };
    assert_eq!(closed_pill("bob"), 2, "a switched-away peer ages offline, not frozen online");
    assert_eq!(
        closed_pill("ada"),
        2,
        "self-online applies only to the ACTIVE workspace; a closed one ages self too"
    );
}

/// The pushed presence stamp must not freeze between state changes. A
/// re-stamp that renders an identical "N min ago" label (same displayed
/// minute) is not re-broadcast, but one that crosses a label-minute
/// boundary IS — otherwise a continuously-seen peer's pushed age drifts
/// upward against a still-green pill.
#[test]
fn a_restamp_crossing_a_label_minute_re_pushes_the_fresh_stamp() {
    let mut st = presence_fixture();
    // align to a label-minute boundary so the buckets are obvious
    let base = (T / 60) * 60;
    st.presence.clock_override = Some(base);
    // first sighting flips NEVER -> online: a state change, so it pushes
    st.cmd_net_peer_seen("bob".to_string(), None).expect("first sighting");
    // observe only pushes from here on
    let mut ev = st.subscribe_events();
    // a re-stamp still inside the same displayed minute renders identically
    st.presence.clock_override = Some(base + 59);
    st.cmd_net_peer_seen("bob".to_string(), None).expect("re-stamp, same minute");
    assert!(
        ev.try_recv().is_err(),
        "a re-stamp inside the same label-minute must not re-broadcast the session"
    );
    // a re-stamp crossing into the next displayed minute changes the label
    st.presence.clock_override = Some(base + 60);
    st.cmd_net_peer_seen("bob".to_string(), None).expect("re-stamp, next minute");
    assert!(
        matches!(ev.try_recv(), Ok(crate::Event::SessionChanged { .. })),
        "crossing a label-minute must push the refreshed stamp"
    );
}

/// A send-failure pins the member unreachable (state 2) WITHOUT
/// touching its last-seen stamp — the stamp records real sightings
/// only — and the pin outlives the ticker until the next sighting.
#[test]
fn a_send_failure_pins_unreachable_until_the_next_sighting() {
    let mut st = presence_fixture();
    st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
    st.cmd_net_send_failed("bob".to_string(), "queue gone".to_string(), None)
        .expect("ack");
    let bob = pill(&st, "bob");
    assert_eq!(bob.state, 2, "failing sends mark the member unreachable");
    assert_eq!(bob.last_seen, T, "a failure is not a sighting");
    // the ticker must not lift the pin while the stamp is still fresh
    st.presence.clock_override = Some(T + 10);
    st.cmd_net_presence_tick().expect("tick");
    assert_eq!(pill(&st, "bob").state, 2, "unreachable is sticky");
    assert_eq!(
        st.members_view()
            .into_iter()
            .find(|m| m.member == "bob")
            .expect("bob row")
            .presence,
        2,
        "reads see the pin too"
    );
    // real inbound traffic lifts it
    st.cmd_net_peer_seen("bob".to_string(), None).expect("ack");
    let bob = pill(&st, "bob");
    assert_eq!(bob.state, 0);
    assert_eq!(bob.last_seen, T + 10);
}

/// Upload availability ("sharer online?") derives from the same real
/// stamps: a never-seen sharer is offline, a sighting flips it.
#[test]
fn upload_availability_follows_the_real_stamps() {
    fn cid_online(st: &crate::State) -> bool {
        st.uploads_view()
            .into_iter()
            .find(|u| u.member == "cid")
            .expect("cid share")
            .online
    }
    let mut st = presence_fixture();
    // the share's ts must sit inside the retention window, which is
    // measured on the REAL clock (chat visibility is not presence)
    let ts = crate::now_secs();
    let mut msg = ChatMessage::text(id(9), "cid", "", ts);
    msg.file = Some(molt_core::FileMeta {
        name: "notes.pdf".to_string(),
        size: 10,
        kind: "PDF".to_string(),
        modified: ts,
        available: true,
        checksum: String::new(),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    st.apply(&EventEnvelope { prev_seq: 0,
        seq: 1,
        ts,
        by: "cid".to_string(),
        body: WorkspaceEvent::Chat(msg),
    });
    assert!(!cid_online(&st), "a never-seen sharer is honestly offline");
    st.cmd_net_peer_seen("cid".to_string(), None).expect("ack");
    assert!(cid_online(&st), "a sighting makes the sharer reachable");
}

/// `State::presence_of` and the ticker's pill refresh are ONE derivation:
/// for every member / pin / transport / stamp combination the on-demand
/// answer equals the pill the refresh writes on the active entry.
#[test]
fn presence_of_and_the_pill_refresh_agree_on_every_input() {
    let stamps = [
        MemberInfo::NEVER,
        T,
        T - MemberInfo::ONLINE_SECS - 1,
        T - MemberInfo::STALE_SECS - 1,
        T - MemberInfo::COARSE_SECS,
        T - MemberInfo::COARSE_SECS - 1,
    ];
    for coarse in [false, true] {
        for pinned in [false, true] {
            for stamp in stamps {
                let mut st = presence_fixture();
                if coarse {
                    st.nostr = Some(crate::NostrTransport {
                        sk: zeroize::Zeroizing::new(vec![7u8; 32]),
                        relays: vec!["ws://relay.example".to_string()],
                        rotation_seed: [0u8; 32],
                    });
                }
                if pinned {
                    st.delivery.unreachable.insert("bob".to_string());
                }
                let active = st.session.active_workspace.clone();
                let entry = st
                    .session
                    .workspaces
                    .iter_mut()
                    .find(|w| w.id == active)
                    .expect("active entry");
                for m in &mut entry.members {
                    m.last_seen = stamp;
                }
                st.cmd_net_presence_tick().expect("tick");
                for name in ["ada", "bob", "cid"] {
                    assert_eq!(
                        pill(&st, name).state,
                        st.presence_of(name, stamp, T),
                        "member={name} coarse={coarse} pinned={pinned} stamp={stamp}"
                    );
                }
            }
        }
    }
}
