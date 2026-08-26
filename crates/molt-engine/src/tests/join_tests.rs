// SPDX-License-Identifier: GPL-3.0-or-later

//! The joiner side: link gating, generation-guarded reports, sealing from
//! a valid roster and the persisted transport secret.

use super::support::*;
use crate::*;
use molt_core::Screen;

#[test]
fn join_requires_a_joinable_link() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());

        // empty, plain text, and a bare preview link (no transport
        // handover) are all rejected — a real join needs a link that
        // carries the transport handover
        for bad in [
            "  ",
            "not-an-invite",
            "molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa",
        ] {
            assert!(
                matches!(
                    w.execute(Command::JoinStart {
                        invite: bad.to_string(),
                        member: "petra".to_string(),
                    })
                    .await,
                    Err(MoltError::Join(_))
                ),
                "should reject `{bad}`"
            );
        }
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => assert_eq!(s.join, molt_core::JoinState::default()),
            other => panic!("unexpected: {other:?}"),
        }

        // a real founding link (with the transport handover) arms the
        // wizard — and then fails HONESTLY: this build has no network
        // relay-gate refusal: this node shares NO relay with the invite
        // (its pool is empty), so the run says exactly that — naming both
        // sides — instead of dialing somewhere the operator never approved
        let link = crate::FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "Chess Club".to_string(),
                threshold: 2,
                members: 2,
                inviter: "walter".to_string(),
                ticket: "ab".repeat(32),
            },
            handover: molt_net::invite::InviteHandoverV2 {
                seat: 0,
                ticket: "ab".repeat(32),
                npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
                relays: vec!["wss://no-such-relay.invalid".to_string()],
            },
        }
        .render()
        .expect("a well-formed handover renders");
        w.execute(Command::JoinStart {
            invite: link,
            member: "petra".to_string(),
        })
        .await
        .expect("a joinable link arms the wizard");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Join);
                assert_eq!(s.join.republic, "Chess Club");
                assert_eq!((s.join.rule_m, s.join.rule_n), (2, 2));
                assert!(!s.join.seed.is_empty(), "the joiner's recovery phrase is shown");
                assert_eq!(s.join.run.outcome, 2, "no shared relay → the run fails honestly");
                assert!(
                    s.join.run.log.iter().any(|l| l.contains("no relay in common")),
                    "the honest relay-gate error is in the run log: {:?}",
                    s.join.run.log
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        // cancel still clears the failed run
        w.execute(Command::JoinCancel).await.expect("cancel");
    });
}

/// A joinable link with an unreachable relay — parseable, so
/// `cmd_join_start` arms the wizard before it fails honestly.
fn joinable_link() -> String {
    crate::FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "R".to_string(),
            threshold: 2,
            members: 2,
            inviter: "walter".to_string(),
            ticket: "ab".repeat(32),
        },
        handover: molt_net::invite::InviteHandoverV2 {
            seat: 0,
            ticket: "ab".repeat(32),
            npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
            relays: vec!["wss://no-such-relay.invalid".to_string()],
        },
    }
    .render()
    .expect("a well-formed handover renders")
}

/// Petra's nostr identity for the join fixtures — a REAL derived pair,
/// so the sealed handler's sk↔anchored-pk cross-check has a genuine
/// secret to validate (the anchors must be real canonical curve points
/// anyway: `verify_sealed_roster` rejects anything else).
fn petra_nostr() -> ([u8; 32], String) {
    molt_net::nostr_identity(b"petra-entropy", "ticket-petra")
}

fn valid_sealed_roster() -> molt_core::SealedRoster {
    use molt_core::{MemberIdentity, RosterAttestation};
    let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
    let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
    let identities = vec![
        MemberIdentity {
            member: "founder".to_string(),
            identity_pk: pk_a,
            nostr_pk: molt_net::nostr_identity(b"founder-entropy", "ticket-f").1,
        },
        MemberIdentity {
            member: "petra".to_string(),
            identity_pk: pk_b,
            nostr_pk: petra_nostr().1,
        },
    ];
    let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
    let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "", &[], None);
    let attestations = vec![
        RosterAttestation { member: "founder".to_string(), sig: molt_storage::identity_sign(&sk_a, &table) },
        RosterAttestation { member: "petra".to_string(), sig: molt_storage::identity_sign(&sk_b, &table) },
    ];
    molt_core::SealedRoster {
        name: "R".to_string(),
        republic_id,
        rule_m: 2,
        rule_n: 2,
        roster: vec!["founder".to_string(), "petra".to_string()],
        identities,
        attestations,
        agenda: String::new(),
        relays: Vec::new(),
        features: None,
    }
}

/// An honest join failure (here: the ADR-0004 relay gate) rides the join
/// run's EXISTING failure surface (`cmd_net_join_failed`), and that
/// surface keeps its gates: a report after the run already failed is
/// dropped, not double-appended.
#[test]
fn join_fails_honestly_and_late_reports_stay_dropped() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::JoinStart { invite: joinable_link(), member: "petra".to_string() })
            .await
            .expect("start");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.join.run.outcome, 2, "no shared relay → honest failure");
                assert!(s.join.run.log.iter().any(|l| l.contains("no relay in common")));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // a late failure report (any generation) is dropped — the run is
        // already settled, its log must not grow a second failure line
        w.execute(Command::NetJoinFailed { error: "boom".to_string(), generation: Some(1) })
            .await
            .expect("late");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert!(
                    !s.join.run.log.iter().any(|l| l.contains("boom")),
                    "a settled run drops late failure reports: {:?}",
                    s.join.run.log
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The GENERATION clause of the join gates (`cmd_join_cancel` bumps the
/// generation to invalidate in-flight tasks): a report from a superseded
/// generation is dropped even while a run is LIVE, and the sealed handler
/// shares the clause — a stale-generation seal materializes nothing.
#[test]
fn join_reports_from_a_stale_generation_are_dropped_while_live() {
    let mut st = plain_state();
    st.join_generation = 2;
    assert_eq!(st.session.join.run.outcome, 0, "run starts live");
    st.cmd_net_join_failed("boom".to_string(), Some(1)).expect("stale gen");
    assert_eq!(st.session.join.run.outcome, 0, "stale-generation report ignored");
    st.cmd_net_join_failed("boom".to_string(), None).expect("no gen");
    assert_eq!(st.session.join.run.outcome, 0, "generation-less report ignored");
    st.cmd_net_join_failed("boom".to_string(), Some(2)).expect("current gen");
    assert_eq!(st.session.join.run.outcome, 2, "matching generation lands");
    assert!(st.session.join.run.log.iter().any(|l| l.contains("boom")));

    let mut st2 = plain_state();
    st2.join_generation = 2;
    let before = st2.session.workspaces.len();
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    st2.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("stale seal");
    assert_eq!(
        st2.session.workspaces.len(),
        before,
        "a stale-generation seal materializes nothing"
    );
}

/// `NetJoinSealed` stays on the surface (dormant — N4's Nostr join task
/// re-emits it), so its materialization is pinned by arming the join
/// context DIRECTLY: `cmd_join_start` fails honestly without a transport
/// (its run settles at outcome 2, which gates the sealed handler off).
#[test]
fn join_seals_into_the_republic_from_a_valid_roster() {
    let rt = rt();
    let _guard = rt.enter();
    // a verified sealed roster materializes the republic
    let mut st = plain_state();
    st.join_generation = 1;
    st.session.join = molt_core::JoinState {
        member: "petra".to_string(),
        seed: "wombat lattice orbit".to_string(),
        ..molt_core::JoinState::default()
    };
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    st.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("sealed");
    // sealed ≠ entered (2026-08-08): entry waits for the phrase-backup
    // confirmation — JoinFinish is the joiner's CreateFinish
    assert_ne!(st.session.screen, Screen::Main, "sealing must not auto-enter");
    assert_eq!(st.session.join.run.outcome, 1, "the run reports sealed");
    assert!(!st.session.join.sealed_id.is_empty(), "the sealed id is exposed");
    st.cmd_join_finish().expect("finish enters");
    assert_eq!(st.session.screen, Screen::Main, "entered the republic");
    assert_eq!(st.session.join, molt_core::JoinState::default(), "join reset");
    let ws = st.session.workspaces.iter().find(|ws| ws.name == "R").expect("workspace added");
    // the net label mirrors the joiner's own global anonymity setting
    // ("none" by default) — never a hardcoded "tor"
    assert_eq!(ws.net, "none", "label = the effective global setting");

    // a garbage roster fails the join rather than materialising anything
    let mut st2 = plain_state();
    st2.join_generation = 1;
    st2.session.join = molt_core::JoinState {
        member: "x".to_string(),
        ..molt_core::JoinState::default()
    };
    let before = st2.session.workspaces.len();
    st2.cmd_net_join_sealed("{".to_string(), String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("bad");
    assert_eq!(st2.session.join.run.outcome, 2, "garbage roster fails");
    assert_eq!(st2.session.workspaces.len(), before, "nothing materialized");
}

/// N1 PIN — the secret that pairs with the FOREVER-anchored third anchor
/// is validated before it is persisted: `cmd_net_join_sealed` must
/// refuse a nostr_sk that is not 32 bytes of hex, or whose x-only public
/// key is not OUR seat's anchored `nostr_pk` — in both directions the
/// join FAILS (like the corrupt-MLS arm), because sealing a genesis
/// whose transport secret the node does not actually hold surfaces only
/// when N4's transport first uses the key, with the salting ticket long
/// dead and no re-derivation path. The matching secret persists into
/// `transport.state.nostr_sk` byte-exactly.
#[test]
fn join_sealed_validates_the_persisted_nostr_secret() {
    let rt = rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().expect("tmp");
    let persist_state = || {
        let (ev_tx, _keep) = broadcast::channel::<Event>(8);
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
        let mut st = State::new(
            GroupConfig::demo(),
            SessionView {
                settings: molt_core::SessionSettings {
                    workspace_dir: tmp.path().display().to_string(),
                    ..molt_core::SessionSettings::default()
                },
                ..SessionView::default()
            },
            ev_tx,
            cmd_tx,
            None,
            true, // persist — the secret-lifecycle path under test
            None,
        );
        st.join_generation = 1;
        st.session.join = molt_core::JoinState {
            member: "petra".to_string(),
            seed: molt_storage::generate_seed_phrase().expect("seed"),
            ..molt_core::JoinState::default()
        };
        st
    };
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    let (petra_sk, petra_npk) = petra_nostr();

    // absent, truncated, and odd-length secrets all FAIL the join
    for bad in ["", "abcd", "ab", &hex::encode(&petra_sk[..16])] {
        let mut st = persist_state();
        st.cmd_net_join_sealed(sealed.clone(), String::new(), Vec::new(), bad.to_string(), Vec::new(), String::new(), Some(1))
            .expect("handler never errors");
        assert_eq!(
            st.session.join.run.outcome, 2,
            "a malformed nostr secret {bad:?} must fail the join"
        );
        assert!(st.active.is_none(), "nothing materialized for {bad:?}");
    }
    // a well-formed scalar that is NOT the private half of petra's
    // anchored nostr_pk fails too (the wrong-seat/wrong-derivation case)
    let (foreign_sk, _) = molt_net::nostr_identity(b"someone-else", "ticket-x");
    let mut st = persist_state();
    st.cmd_net_join_sealed(
        sealed.clone(),
        String::new(),
        Vec::new(),
        hex::encode(foreign_sk),
        Vec::new(),
        String::new(),
        Some(1),
    )
    .expect("handler never errors");
    assert_eq!(st.session.join.run.outcome, 2, "a mismatched secret must fail the join");
    assert!(st.active.is_none(), "nothing materialized for the mismatch");

    // the matching secret seals the join and persists byte-exactly
    let mut st = persist_state();
    st.cmd_net_join_sealed(
        sealed.clone(),
        String::new(),
        Vec::new(),
        hex::encode(petra_sk),
        Vec::new(),
        String::new(),
        Some(1),
    )
    .expect("handler never errors");
    // sealed (entering waits for the phrase-backup step; the secret's
    // validation + persistence is what THIS test pins)
    assert_eq!(st.session.join.run.outcome, 1, "the matching secret seals the join");
    let dir = st.active.as_ref().expect("materialized").dir.clone();
    drop(st); // release the writer + flock before reopening
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let ws = loop {
        match molt_storage::open_workspace(&dir) {
            Ok((ws, _)) => break ws,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("reopening the joined workspace: {e}"),
        }
    };
    let ts = ws.read_transport_state();
    assert_eq!(
        ts.nostr_sk.as_deref(),
        Some(&petra_sk[..]),
        "the validated secret is sealed into transport.state"
    );
    assert_eq!(
        molt_net::nostr_pk_for_sk(&petra_sk).expect("pk"),
        petra_npk,
        "…and it IS the private half of the anchored third anchor"
    );
}
