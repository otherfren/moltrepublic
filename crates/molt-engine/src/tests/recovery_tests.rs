// SPDX-License-Identifier: GPL-3.0-or-later

//! The rejoiner side of recovery: link and storage guards, materializing
//! from the served chain, the seat-anchor checks, incarnation scoping.

use super::support::*;
use crate::*;
use molt_core::Screen;

/// An actionable recovery link with a bogus host — parseable, so
/// `cmd_recover_start` arms the context (generation + link + phrase)
/// before its honest no-transport failure; the injected
/// `NetRecoverSealed` then materializes against that context (the same
/// seam the two-instance tests drive).
/// R2's recover leg: the refusal must NAME the relays it diagnosed —
/// the join leg extends its run log with one line per invite relay,
/// but the recover leg dropped `refusal.detail` and served only the
/// headline. Rule 3 makes re-join the routine path for a relay change,
/// so this is the leg where the naming matters most (rule 5).
#[test]
fn a_recover_refusal_names_the_relays_it_diagnosed() {
    let rt = rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().expect("tmp");
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
        true, // persist — the recover path needs storage to recover into
        None,
    );
    let link = crate::recovery::RecoveryInvite {
        republic: "Guild".to_string(),
        member: "bob".to_string(),
        ticket: "ab".repeat(32),
        server: String::new(),
        queue_id: String::new(),
        wrap: String::new(),
        republic_id: "f00d".to_string(),
        handover: Some(molt_net::invite::RecoveryHandoverV2 {
            ticket: "ab".repeat(32),
            npub: "12".repeat(32),
            relays: vec!["wss://coordinator.example".to_string()],
            republic_id: "f00d".to_string(),
            identity_pk: String::new(),
        }),
    }
    .render();
    st.cmd_recover_start(link, "brave mountain".to_string()).expect("acked");
    let notice = st.session.notice.clone();
    assert!(notice.starts_with("recover-failed:"), "{notice}");
    assert!(
        notice.contains("coordinator.example"),
        "the refusal names the relay the operator must add: {notice}"
    );
}

#[test]
fn recover_start_guards_the_link_and_the_storage() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        // a bare preview link carries no transport handover — not actionable
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        let err = w
            .execute(Command::RecoverStart {
                link: "molt://recover/Guild/bob/abcdef".to_string(),
                phrase: "some phrase".to_string(),
            })
            .await
            .expect_err("a preview link cannot start a recovery");
        assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");

        // a storage-less node has nowhere to materialize the recovery
        let w2 = spawn(GroupConfig::demo(), SessionView::default());
        let err = w2
            .execute(Command::RecoverStart {
                link: recover_link("bob", "f00d"),
                phrase: "some phrase".to_string(),
            })
            .await
            .expect_err("a storage-less node cannot recover");
        assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");
    });
}

/// **The A2 keystone:** a completed rejoin materializes the recovered
/// workspace from the verified chain — adopting the FULL chain (block 1's
/// gated `Applied` projects into the surface state), anchoring the seat's
/// phrase-derived identity, and entering the republic.
#[test]
fn recovery_materializes_the_workspace_from_the_full_verified_chain() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        let (chain, republic_id) = recovered_chain(&phrase);
        w.execute(Command::RecoverStart {
            link: recover_link("bob", &republic_id),
            phrase: phrase.clone(),
        })
        .await
        .expect("recover start");
        // the production path fails honestly (no transport in this build)
        // on the recovery notice channel — the armed context survives it
        let s = read_session(&w).await;
        assert_eq!(
            s.notice,
            format!("recover-failed:{}", crate::LEGACY_RECOVERY_LINK),
            "the honest N-demo gap error rides the recovery notice"
        );

        // a stale-generation result is dropped without a trace
        let chain_json = serde_json::to_string(&chain).expect("chain json");
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: chain_json.clone(),
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(999),
        })
        .await
        .expect("stale sealed");
        let s = read_session(&w).await;
        assert!(s.workspaces.is_empty(), "a stale result must not materialize");

        // the current-generation result materializes the workspace
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: chain_json,
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(1),
        })
        .await
        .expect("sealed");
        let s = read_session(&w).await;
        assert_eq!(s.screen, Screen::Main, "entered the recovered republic");
        let ws = s
            .workspaces
            .iter()
            .find(|x| x.name == "Guild")
            .expect("the recovered workspace is listed");
        assert_eq!(s.active_workspace, ws.id);
        assert_eq!(ws.agenda, "survive total loss");
        // the FULL chain was adopted, not just the genesis: block 1's
        // gated Applied projects into the surface state
        let mem = read_surface(&w, Surface::Memory).await;
        assert_eq!(mem.applied.len(), 1, "block 1 projected");
        assert_eq!(mem.applied[0]["title"], "survived the loss");
    });
}

/// **N4b step 6d: a recovered workspace comes back as a NOSTR workspace.**
///
/// Recovery used to materialize `TransportShape::default()` and a `None`
/// transport secret — the legacy queue shape — so a seat that recovered
/// into a Nostr republic came back unable to speak to it at all.
///
/// The three things it must hold, and where each honestly comes from:
///
/// - the **new** transport anchor's private half, re-derived on the actor
///   from `(phrase, recovery ticket)`. Not checked against the seat's
///   roster entry, which is the DEAD founding anchor — the returning seat
///   is re-anchored by its own `Restored` block, and comparing against
///   the roster would either always fail or, once "fixed" by deleting the
///   comparison, accept any key at all.
/// - the **chain-ratified** relay pool, from the verified chain rather
///   than from whatever the rejoin task reported. The pool is governed
///   (roster-v4), so the chain is the authority and the Welcome's copy is
///   only a hint.
/// - the group's rotation seed, which only the Welcome can supply.
#[test]
fn recovery_materializes_the_nostr_shape_and_the_new_anchor() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let ticket = "ab".repeat(8);
    // the anchor the rejoiner proves it holds: ticket-salted with THIS
    // recovery's ticket, exactly as the request signed it
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (new_sk, new_pk) = molt_net::nostr_identity(&entropy, &ticket);
    let pool = vec!["wss://relay.one.example/".to_string()];
    let (chain, republic_id) = recovered_chain_with(&phrase, pool.clone(), Some(new_pk.clone()));

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(new_sk),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert_eq!(
        st.session.screen,
        Screen::Main,
        "the recovery must enter the republic; notice = {:?}",
        st.session.notice
    );
    let dir = st.active.as_ref().expect("materialized").dir.clone();
    drop(st); // release the writer + flock before reopening

    let ts = reopen(&dir).read_transport_state();
    assert_eq!(
        ts.kind,
        Some(molt_core::TransportKind::Nostr),
        "a recovered Nostr seat must not come back as a queue workspace"
    );
    assert_eq!(ts.relays, pool, "the CHAIN-ratified pool, not the task's copy");
    assert_eq!(
        ts.rotation_seed.as_deref(),
        Some(&[0x5au8; 32][..]),
        "without the h-tag seed the seat can neither publish nor subscribe"
    );
    let sk = ts.nostr_sk.as_ref().expect("the new transport secret is sealed");
    assert_eq!(
        molt_net::nostr_pk_for_sk(sk).expect("valid scalar"),
        new_pk,
        "the sealed secret must be the private half of the anchor the Restored \
         block put in the chain"
    );
}

/// The rejoiner's status line (`NetRecoverNote`): a live incarnation's
/// note reaches the notice channel; a stale one says nothing (a
/// restarted recovery must not have the old task talking over it).
#[test]
fn a_recover_note_speaks_only_for_the_live_incarnation() {
    let mut st = tests::plain_state();
    st.recover_generation = 3;
    st.cmd_net_recover_note("waiting".to_string(), Some(2)).expect("ack");
    assert!(
        !st.session.notice.starts_with("recover-note:"),
        "a stale incarnation says nothing: {:?}",
        st.session.notice
    );
    st.cmd_net_recover_note(
        "waiting for the coordinator's Welcome (2 min)".to_string(),
        Some(3),
    )
    .expect("ack");
    assert_eq!(
        st.session.notice,
        "recover-note:waiting for the coordinator's Welcome (2 min)"
    );
}

/// The stuck-epoch self-heal is CAPPED and SPACED (`detached_reattach.md`
/// §2.4): a failed spawn still stamps the clock (no per-health-frame
/// retry), the session cap survives resets, and a running recovery task
/// is never stacked onto.
#[test]
fn the_self_heal_reattach_is_capped_and_spaced() {
    let mut st = tests::plain_state();
    // no chain, no seed — the spawn cannot start, but the clock stamps
    st.maybe_self_heal_reattach();
    assert_eq!(st.reattach_attempts, 0, "a spawn that cannot start counts no attempt");
    let first = st.last_reattach.expect("the try is stamped");
    // immediately again: inside the spacing window nothing happens
    st.maybe_self_heal_reattach();
    assert_eq!(st.last_reattach, Some(first), "spaced - no re-stamp inside the window");
    // at the session cap nothing ever fires again, even past the window
    st.reattach_attempts = 3;
    st.last_reattach = None;
    st.maybe_self_heal_reattach();
    assert_eq!(st.last_reattach, None, "the session cap is final (anti ping-pong)");
}

/// The rejoiner's checklist (`NetRecoverProgress`): a live incarnation's
/// report becomes the session's `RecoverState` with per-seat approval
/// flags; a stale incarnation's report is dropped like a stale note.
#[test]
fn a_recover_progress_builds_the_checklist_for_the_live_incarnation() {
    let mut st = tests::plain_state();
    st.recover_generation = 3;
    let roster = vec!["petra".to_string(), "vera".to_string(), "walter".to_string()];
    let approved = vec!["petra".to_string(), "walter".to_string()];
    st.cmd_net_recover_progress("petra".to_string(), 3, roster.clone(), approved.clone(), Some(2))
        .expect("ack");
    assert_eq!(
        st.session.recover,
        molt_core::RecoverState::default(),
        "a stale incarnation says nothing"
    );
    st.cmd_net_recover_progress("petra".to_string(), 3, roster, approved, Some(3))
        .expect("ack");
    let r = &st.session.recover;
    assert_eq!((r.member.as_str(), r.need), ("petra", 3));
    let flags: Vec<(&str, bool)> =
        r.seats.iter().map(|s| (s.member.as_str(), s.approved)).collect();
    assert_eq!(
        flags,
        vec![("petra", true), ("vera", false), ("walter", true)],
        "roster order, per-seat approval"
    );
}

/// The counter-case, and the reason the check cannot simply be deleted: a
/// rejoin task delivering a secret that is NOT this recovery's derived key
/// fails the recovery instead of sealing a workspace whose transport key
/// nobody addresses.
#[test]
fn recovery_refuses_a_transport_secret_that_is_not_the_seats_own() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (_, new_pk) = molt_net::nostr_identity(&entropy, &"ab".repeat(8));
    let (chain, republic_id) = recovered_chain_with(
        &phrase,
        vec!["wss://relay.one.example/".to_string()],
        Some(new_pk),
    );
    let (foreign_sk, _) = molt_net::nostr_identity(b"someone-else-entirely", "other");

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(foreign_sk),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(st.active.is_none(), "a foreign transport secret must not materialize");
    assert!(
        st.session.notice.starts_with("recover-failed:"),
        "the failure surfaces to the operator; notice = {:?}",
        st.session.notice
    );
}

/// **A context switch abandons an in-flight recovery.**
///
/// `cmd_recover_start` has always invalidated the join; the reverse
/// direction was missing and cost nothing while a recovery spawned
/// nothing. Since 6e it holds a 1059 inbox and a 445 subscription for up
/// to fifteen minutes, so a forgotten one sits on relay sockets long
/// after the human moved on — and its late result would materialize a
/// republic into whatever context replaced it.
///
/// The generation bump is the observable half (the socket release rides
/// with it): a result from the abandoned incarnation must land nowhere.
#[test]
fn a_context_switch_abandons_an_in_flight_recovery() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let (chain, republic_id) = recovered_chain(&phrase);
    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    assert!(st.recover_ctx.is_some(), "precondition: the context is armed");

    // starting a founding is a context switch like any other. (A join
    // start that is REFUSED — an unparseable link — deliberately is not
    // one: nothing replaced the recovery, so nothing should abandon it.)
    let _ = st.cmd_create_start(
        "Other Republic".to_string(),
        "bob".to_string(),
        2,
        2,
        Vec::new(),
    );
    assert!(st.recover_ctx.is_none(), "the abandoned recovery kept its context");

    // …and the abandoned incarnation's result lands nowhere
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        String::new(),
        String::new(),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(
        st.active.is_none(),
        "a superseded recovery must not materialize a republic into the new context"
    );
}

/// A coordinator that re-admits the seat under SOMEBODY ELSE'S transport
/// anchor is refused — the one thing the served chain can still say wrong
/// about our own key.
///
/// (It usually says nothing: a Nostr coordinator serves the chain ANCHOR,
/// and this seat's `Restored` block is at the head, arriving later over
/// catch-up. So the check is "if it speaks, it must agree" — demanding a
/// re-anchor here would refuse every real recovery, which is exactly what
/// the step-6 capstone caught.)
#[test]
fn recovery_refuses_a_chain_that_re_anchors_the_seat_elsewhere() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (ours, _) = molt_net::nostr_identity(&entropy, &"ab".repeat(8));
    // the chain re-anchors bob to a key that is NOT this recovery's
    let (_, hostile_pk) = molt_net::nostr_identity(b"a-key-we-do-not-hold", "elsewhere");
    let (chain, republic_id) = recovered_chain_with(
        &phrase,
        vec!["wss://relay.one.example/".to_string()],
        Some(hostile_pk),
    );

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(ours),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(st.active.is_none(), "a seat re-anchored elsewhere must not materialize");
    assert!(
        st.session.notice.contains("different transport key"),
        "the refusal must name what is wrong; notice = {:?}",
        st.session.notice
    );
}

/// Defence in depth on the actor: a chain whose roster does not anchor the
/// identity derived from THIS recovery's phrase is hard-rejected — a forged
/// internal command (or a coordinator serving someone else's chain) must
/// not materialize a workspace the seat cannot sign for.
#[test]
fn recovery_hard_rejects_a_chain_that_does_not_anchor_the_phrase() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        // the chain anchors an identity derived from a DIFFERENT phrase
        let other = molt_storage::generate_seed_phrase().expect("other phrase");
        let (chain, republic_id) = recovered_chain(&other);
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        w.execute(Command::RecoverStart {
            link: recover_link("bob", &republic_id),
            phrase,
        })
        .await
        .expect("recover start");
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: serde_json::to_string(&chain).expect("chain json"),
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(1),
        })
        .await
        .expect("sealed");
        let s = read_session(&w).await;
        assert!(s.workspaces.is_empty(), "an unanchored chain must not materialize");
        assert_ne!(s.screen, Screen::Main);
        assert!(
            s.notice.starts_with("recover-failed:"),
            "the failure surfaces to the operator; notice = {:?}",
            s.notice
        );
    });
}
