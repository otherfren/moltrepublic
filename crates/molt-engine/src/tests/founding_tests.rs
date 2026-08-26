// SPDX-License-Identifier: GPL-3.0-or-later

//! The founder side of the create lifecycle over the `WalletHandle`
//! surface: relay gating, the sim founding, the phrase-backup gate.

use super::support::*;
use crate::*;
use molt_core::{demo_workspace_id, Screen, SessionSettings};

/// seed_backup_confirmation.md keystone (❻½): an all-ratified founding
/// does NOT finalize — nothing lands on disk — until the founder's own
/// recovery-phrase backup is confirmed (sim members auto-confirm
/// theirs). A wrong re-typed phrase is refused; the right one seals.
#[test]
fn founding_waits_for_the_backup_confirmation_before_writing() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let ws_dir = tmp.path().join("workspaces");
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: ws_dir.display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Backup".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        // every sim seat ratifies AND auto-confirms its backup…
        let mut seen_all = false;
        for _ in 0..600 {
            let s = read_session(&w).await;
            assert_ne!(s.create.run.outcome, 2, "founding failed: {:?}", s.create.run.log);
            if s.create.seats.iter().all(|x| x.state >= 2) && !s.create.seats.is_empty() {
                seen_all = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(seen_all, "sim seats never ratified");
        // …but WITHOUT the founder's own confirmation nothing seals and
        // nothing is written (grace window, then re-check)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let s = read_session(&w).await;
        assert_eq!(
            s.create.run.outcome, 0,
            "the founding sealed without the founder's backup confirmation: {:?}",
            s.create.run.log
        );
        let disk_empty = !ws_dir.exists()
            || std::fs::read_dir(&ws_dir).map(|d| d.count() == 0).unwrap_or(true);
        assert!(disk_empty, "the ritual wrote to disk before the last confirmation");
        // a wrong re-typed phrase is refused, the ritual keeps waiting
        let wrong = w
            .execute(Command::ConfirmSeedBackup { phrase: "wrong words entirely".to_string() })
            .await;
        assert!(wrong.is_err(), "a wrong phrase must be refused");
        assert_eq!(read_session(&w).await.create.run.outcome, 0);
        // the right phrase confirms, the founding finalizes, disk exists
        let seed = read_session(&w).await.create.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed })
            .await
            .expect("confirm backup");
        await_founding(&w).await;
        let s = read_session(&w).await;
        assert_eq!(s.active_workspace.len(), 64);
    });
}

/// N4a: a PRODUCTION founding (no test seam) runs over Nostr — and on a
/// fresh node with an EMPTY relay pool (ADR-0004: nothing pre-configured)
/// it fails honestly, naming the missing prerequisite, before a single
/// ticket leaves the engine.
#[test]
fn create_start_without_a_confirmed_relay_fails_honestly() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let err = w
            .execute(Command::CreateStart {
                name: "Gap".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                relays: Vec::new(),
            })
            .await
            .expect_err("no confirmed relay → no founding");
        assert!(
            err.to_string().contains("no relay configured"),
            "the honest prerequisite error surfaces: {err}"
        );
    });
}

/// …and the SAME refusal must not misdiagnose the pool it is looking at.
/// A confirmed clearnet relay with non-onion dialing switched off (the
/// hand-written `confirmed = true` without `clearnet_enabled = true`) was
/// told to "add and confirm one" — the one thing the operator had already
/// done, while the switch that was actually off went unmentioned.
#[test]
fn create_start_names_the_clearnet_switch_when_that_is_what_blocks_it() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: "wss://relay.example.org".to_string() })
            .await
            .expect("add");
        w.execute(Command::RelayConfirm {
            url: "wss://relay.example.org".to_string(),
            accept_clearnet: true,
        })
        .await
        .expect("confirm");
        // B4: the confirmation lands on the PROBE's verdict, and this
        // relay does not exist — inject the verdict directly; the test
        // is about the clearnet SWITCH, not about reachability
        let url = match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => s.settings.relays[0].url.clone(),
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::NetRelayProbed {
            url,
            error: String::new(),
            unreachable: false,
            confirm: true,
        })
        .await
        .expect("verdict");
        // the operator (or their config file) leaves non-onion dialing off
        w.execute(Command::RelayClearnetSession { unlock: false })
            .await
            .expect("dark");
        let err = w
            .execute(Command::CreateStart {
                name: "Gap".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                relays: Vec::new(),
            })
            .await
            .expect_err("nothing dialable → no founding");
        let err = err.to_string();
        assert!(
            err.contains("clearnet_enabled") && !err.contains("no relay configured"),
            "the refusal names the switch, not a confirmation that exists: {err}"
        );
    });
}

#[test]
fn create_lifecycle_founds_a_republic() {
    rt().block_on(async {
        // the offline sim seam (session-only): simulated members seal the
        // ritual so the founder-side lifecycle can be tested without a
        // network — a production founding fails honestly until N4
        let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);

        // invalid configurations are rejected up front
        assert!(matches!(
            w.execute(Command::CreateStart {
                name: "X".to_string(),
                member: "me".to_string(),
                threshold: 4,
                members: 3,
                relays: Vec::new(),
            })
            .await,
            Err(MoltError::Create(_))
        ));
        for bad_n in [1_u8, 14] {
            assert!(matches!(
                w.execute(Command::CreateStart {
                    name: "X".to_string(),
                    member: "me".to_string(),
                    threshold: 1,
                    members: bad_n,
                    relays: Vec::new(),
                })
                .await,
                Err(MoltError::Create(_))
            ));
        }
        // threshold 1 is refused since 2026-08-08 (product decision) —
        // the engine gate, so MCP meets it too, not only the GUI stepper
        assert!(matches!(
            w.execute(Command::CreateStart {
                name: "X".to_string(),
                member: "me".to_string(),
                threshold: 1,
                members: 2,
                relays: Vec::new(),
            })
            .await,
            Err(MoltError::Create(_))
        ));

        // a valid founding runs the ritual: two seats, each activated
        // and sealed by a simulated member, then the workspace is born
        w.execute(Command::CreateStart {
            name: "Chess Club".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        // "Enter republic" is refused until every seat is sealed
        assert!(matches!(
            w.execute(Command::CreateFinish).await,
            Err(MoltError::Create(_))
        ));
        await_founding(&w).await;
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.create.run.outcome, 1);
                // sealed ≠ entered (2026-08-08): the founder backs its
                // phrase up on the wizard's last step first — the exact
                // twin of the joiner's JoinFinish gate
                assert_ne!(s.screen, Screen::Main, "sealing must not auto-enter");
                assert_eq!(s.create.seed.split(' ').count(), 24);
                assert_eq!(s.create.seats.len(), 2);
                for seat in &s.create.seats {
                    // 3 = sealed AND backup-confirmed (❻½): a finalized
                    // founding implies every seat attested its backup
                    assert_eq!(seat.state, 4, "every seat sealed + backup-confirmed");
                    assert!(!seat.member.is_empty(), "the member named itself");
                    // a SIMULATED founding mints preview-only links (the
                    // path shape, no handover) — the preview parser is
                    // the right reader; real links are neutral segments
                    let info =
                        molt_core::InviteInfo::parse(&seat.link).expect("invite parses");
                    assert_eq!(info.republic, "Chess Club");
                    assert_eq!(info.inviter, "petra");
                }
                // the log carries the real ritual events, not a fake anim
                assert!(s.create.run.log.iter().any(|l| l.contains("activated invite")));
                assert!(s.create.run.log.iter().any(|l| l.contains("signed the roster")));
                assert!(s.create.run.log.iter().any(|l| l.contains("workspace created")));
            }
            other => panic!("unexpected: {other:?}"),
        }
        w.execute(Command::CreateFinish).await.expect("finish");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Main);
                assert_eq!(s.active_workspace, demo_workspace_id("Chess Club"));
                let ws = s
                    .workspaces
                    .iter()
                    .find(|w| w.name == "Chess Club")
                    .expect("workspace added");
                assert_eq!(ws.detail, "2-of-3");
                assert_eq!(ws.members.len(), 3);
                assert_eq!(s.create, molt_core::CreateState::default());
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The persisted `WorkspaceInfo.net` label mirrors the EFFECTIVE global
/// anonymity setting — the ritual transport always comes from the global
/// settings (`resolve_dialer`), so the label must reflect those and never
/// a client-supplied string (tor_transport_implementation.md §P8).
#[test]
fn workspace_net_label_mirrors_the_global_anonymity_setting() {
    rt().block_on(async {
        // default settings (anonymity = "none") → the label says "none"
        let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);
        w.execute(Command::CreateStart {
            name: "Plain".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        // the run header shows the effective network while the ritual runs
        assert_eq!(read_session(&w).await.create.net, "none");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let s = read_session(&w).await;
        let ws = s.workspaces.iter().find(|x| x.name == "Plain").expect("entry");
        assert_eq!(ws.net, "none", "label = the effective global setting");

        // tor configured globally → the label says "tor"
        let session = SessionView {
            settings: SessionSettings {
                anonymity: "tor".to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, false);
        w.execute(Command::CreateStart {
            name: "Onioned".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start tor");
        assert_eq!(read_session(&w).await.create.net, "tor");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish tor");
        let s = read_session(&w).await;
        let ws = s.workspaces.iter().find(|x| x.name == "Onioned").expect("entry");
        assert_eq!(ws.net, "tor", "label = the effective global setting");
    });
}

#[test]
fn leaving_the_create_screen_abandons_an_in_flight_founding() {
    rt().block_on(async {
        // manual seam: the ritual opens but no member joins, so it stays
        // open (it cannot seal and hijack the session behind our back)
        let (w, _material_rx) =
            __spawn_manual_founding(GroupConfig::demo(), SessionView::default());
        w.execute(Command::CreateStart {
            name: "Duet".to_string(),
            member: "founder".to_string(),
            threshold: 2,
            members: 2,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_ne!(s.create, molt_core::CreateState::default(), "founding is open")
            }
            other => panic!("unexpected: {other:?}"),
        }
        // navigating away abandons it (the session is in-memory)
        w.execute(Command::Navigate { screen: Screen::Choice }).await.expect("nav");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Choice);
                assert_eq!(s.create, molt_core::CreateState::default(), "founding abandoned");
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}
