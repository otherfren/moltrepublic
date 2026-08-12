// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The N4a capstone** (`docs_archive/transport/nostr_n4_plan.md` §9 step 7): two
//! REAL engines found and join a republic over one in-process Nostr relay —
//! the first engine-level test to drive the relay runtime at all, and the
//! path that lights the long-dark actor-level `NetJoinSealed` lane (dispatch
//! + persist branch) WITHOUT command injection.
//!
//! Unlike `two_instances.rs` (where the member side is a hand-driven task
//! against the founder's loopback hub), BOTH sides here are storage-backed
//! engines driven purely through the public Command surface — exactly what
//! a GUI or MCP agent does in production.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView, WorkspaceEvent};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll the session until `pred` holds — the same no-event-bus pattern every
/// engine test uses.
async fn wait_for(w: &WalletHandle, what: &str, pred: impl Fn(&SessionView) -> bool) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

/// ADR-0004: nothing is pre-configured — each node adds and confirms the
/// relay itself. A `ws://127.0.0.1` relay is `Local` kind, gated like
/// clearnet (§10.14): confirm with the acknowledgement, then unlock the
/// session.
async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    // B4: the confirmation lands on the PROBE's verdict, off-actor — an
    // unusable relay never becomes a confirmed one
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// Adopt a relay with an INJECTED ok-verdict, bypassing the B4 probe: for
/// tests whose relay double is deliberately hostile in exactly the way the
/// probe would catch — their subject is the DEEPER line of defense (the
/// founding's own readable/publish gates), which must hold for a relay
/// that turns hostile AFTER it was vetted.
async fn adopt_relay_unprobed(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm { url: url.to_string(), accept_clearnet: true })
        .await
        .expect("relay confirm");
    let stored = read_session(w).await.settings.relays[0].url.clone();
    w.execute(Command::NetRelayProbed {
        url: stored,
        error: String::new(),
        unreachable: false,
        confirm: true,
    })
    .await
    .expect("verdict");
    wait_for(w, "the injected verdict to confirm the relay", |s| {
        s.settings.relays.iter().any(|r| r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// KEYSTONE — the full production founding+join choreography over Nostr:
/// CreateStart → (link v2 via the once-dormant NetRitualLinkReady) →
/// JoinStart on a second engine → founder sees the join → CreatePropose →
/// joiner ratifies → both seal, enter via the phrase-backup gate, and
/// persist the v4 transport
/// shape; the genesis anchors three verified anchors per seat; both reopen
/// honestly (not "detached").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_republic_founds_and_a_member_joins_over_one_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();

    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let root_b = tmp.path().join("joiner");

    // --- founder engine: real storage, NO test seams
    let a = engine(&root_a);
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("a production founding starts over the confirmed relay");

    // the invite link becomes JOINABLE (v2 handover) once the founder's
    // inbox subscription is live — the NetRitualLinkReady seam's first use
    let s = wait_for(&a, "the seat link to become a joinable v2 link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();
    let inv = molt_engine::FoundingInvite::parse(&link).expect("joinable");
    assert_eq!(inv.handover.relays, vec![url.clone()], "the link names the invite relay");

    // --- joiner engine: its own storage, its own relay adoption
    let b = engine(&root_b);
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    // the founder ingests the gift-wrapped JoinRequest (MAC v2 + PoP) and
    // unlocks deliberation
    wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose).await;

    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
    })
    .await
    .expect("charter proposed");

    // the joiner sees the charter (over the freshly born 445 group) and
    // ratifies it — the human gate
    let s = wait_for(&b, "petra to see the proposed charter", |s| s.join.awaiting_ratify).await;
    assert_eq!(s.join.proposed_name, "Chess Club");
    assert_eq!(s.join.proposed_agenda, "play chess, decide together");
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");

    // both sides seal; entering is gated on the phrase-backup step
    // (2026-08-08) — the founder finishes exactly like the joiner
    let s = wait_for(&a, "the founding to seal on the founder", |s| {
        s.create.run.outcome == 1
    })
    .await;
    assert_ne!(s.screen, molt_core::Screen::Main, "sealing must not auto-enter");
    let ws_id_a = s.active_workspace.clone();
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal on petra", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    let s = wait_for(&b, "petra to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    let ws_id_b = s.active_workspace.clone();

    // F1 — the FIRST session must be as honest as a reopen. `net_health` was
    // written on the open path only, so a freshly founded/joined Nostr
    // workspace kept the serde default and showed a green pill for its whole
    // first session. It is green again now, but for the opposite reason: the
    // group runtime (N5.2) is UP, and the founding path sets the pill from
    // that fact rather than from a default. The invariant F1 protects is
    // unchanged — what the first session claims must be what is true.
    for (who, s) in [("founder", read_session(&a).await), ("joiner", read_session(&b).await)] {
        assert!(
            matches!(&s.net_health, molt_core::NetHealth::Ok),
            "{who}: the runtime came up, so the first session must say so, got {:?}",
            s.net_health
        );
        assert_ne!(s.notice, "detached", "{who}: a live runtime is not 'detached'");
    }

    // --- disk truth, both ends (close to release the LOCKs)
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");

    let dir_a = molt_storage::find_workspace_dir(&root_a, &ws_id_a).expect("a dir");
    let dir_b = molt_storage::find_workspace_dir(&root_b, &ws_id_b).expect("b dir");
    let (ws_a, _) = molt_storage::open_workspace(&dir_a).expect("open a");
    let (ws_b, _) = molt_storage::open_workspace(&dir_b).expect("open b");

    // the genesis: 2 seats, 2 verifying attestations, three anchors each
    let log = ws_a.read_log_from(1).expect("genesis");
    let WorkspaceEvent::Founded {
        rule_m,
        rule_n,
        identities,
        attestations,
        republic_id,
        agenda,
        relays,
        features,
        ..
    } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!((*rule_m, *rule_n), (2, 2));
    assert_eq!(agenda, "play chess, decide together");
    assert_eq!(identities.len(), 2);
    assert_eq!(attestations.len(), 2);
    // the ratified pool travels in the genesis frame, so a reader holding only
    // the log can recompute exactly what the attestations were signed over
    assert_eq!(relays, &vec![url.clone()]);
    let table = molt_core::roster_canonical_bytes(
        republic_id,
        *rule_m,
        *rule_n,
        identities,
        agenda,
        relays,
        features.as_deref(),
    );
    for att in attestations {
        let identity = identities
            .iter()
            .find(|i| i.member == att.member)
            .expect("attestation names a member");
        assert!(
            molt_storage::identity_verify(&identity.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }
    for entry in identities {
        assert_eq!(
            molt_net::canonical_nostr_pk(&entry.nostr_pk).expect("valid third anchor"),
            entry.nostr_pk,
            "{} anchors the canonical form",
            entry.member
        );
    }

    // the joiner's genesis is the SAME republic knowledge (its `member`
    // field is the local writer by design — everything constitutional
    // must match byte-for-byte)
    let log_b = ws_b.read_log_from(1).expect("b genesis");
    let WorkspaceEvent::Founded {
        rule_m: b_m,
        rule_n: b_n,
        identities: b_identities,
        attestations: b_attestations,
        republic_id: b_republic_id,
        agenda: b_agenda,
        ..
    } = &log_b[0].body
    else {
        panic!("petra's first event is not Founded");
    };
    assert_eq!((b_m, b_n), (rule_m, rule_n));
    assert_eq!(b_identities, identities, "one identity table, both ends");
    assert_eq!(b_attestations, attestations, "one attestation set, both ends");
    assert_eq!(b_republic_id, republic_id, "one republic id, both ends");
    assert_eq!(b_agenda, agenda, "one ratified charter, both ends");

    // the v4 transport shape, both ends: kind Nostr, the relay list, one
    // shared rotation_seed, and each side's own paired nostr secret
    let ts_a = ws_a.read_transport_state();
    let ts_b = ws_b.read_transport_state();
    for (who, ts, member) in [("walter", &ts_a, "walter"), ("petra", &ts_b, "petra")] {
        assert_eq!(
            ts.kind,
            Some(molt_core::TransportKind::Nostr),
            "{who}: kind discriminator"
        );
        assert_eq!(ts.relays, vec![url.clone()], "{who}: the group relay list");
        let seed = ts.rotation_seed.as_ref().expect("rotation seed persisted");
        assert_eq!(seed.len(), 32, "{who}: a 32-byte h-tag seed");
        let sk = ts.nostr_sk.as_ref().expect("nostr secret persisted");
        let anchored = &identities
            .iter()
            .find(|i| i.member == member)
            .expect("anchored")
            .nostr_pk;
        assert_eq!(
            &molt_net::nostr_pk_for_sk(sk).expect("valid scalar"),
            anchored,
            "{who}: the persisted secret pairs with the anchored third anchor"
        );
        assert!(ts.mesh.is_empty(), "{who}: no queue-mesh on a Nostr workspace");
        assert!(ts.mls.is_some(), "{who}: the MLS group snapshot persisted");
    }
    assert_eq!(
        ts_a.rotation_seed, ts_b.rotation_seed,
        "ONE rotation seed, delivered inside the authenticated Welcome"
    );
    assert_ne!(ts_a.nostr_sk, ts_b.nostr_sk, "distinct per-seat secrets");

    // the MLS group interoperates from both PERSISTED snapshots
    let mut mls_a =
        molt_net::MlsMember::restore(ts_a.mls.as_ref().expect("a mls")).expect("restore a");
    let mut mls_b =
        molt_net::MlsMember::restore(ts_b.mls.as_ref().expect("b mls")).expect("restore b");
    drop(ws_a);
    drop(ws_b);
    let ct = mls_a.encrypt(b"first law: show up").expect("encrypt");
    match mls_b.decrypt(&ct).expect("decrypt") {
        molt_net::MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "walter");
            assert_eq!(plaintext, b"first law: show up");
        }
        other => panic!("expected an application message, got {other:?}"),
    }

    // reopen honesty (§7.5): a Nostr workspace is NOT "detached" — and since
    // N5.2 it comes back up on its own, so the reopened pill is green because
    // the runtime is running, not because nothing set it
    a.execute(Command::OpenWorkspace { id: ws_id_a }).await.expect("reopen a");
    let s = read_session(&a).await;
    assert_ne!(s.notice, "detached", "a Nostr workspace is not detached");
    assert!(
        matches!(&s.net_health, molt_core::NetHealth::Ok),
        "a reopened Nostr workspace rebuilds its runtime, got {:?}",
        s.net_health
    );
}

/// REGRESSION (user report, 2026-07-31) — **one relay in common is enough.**
/// A founder publishes its whole dialable pool into the invite; an invitee
/// almost never has that exact set (the founder runs an onion relay, the
/// invitee a clearnet one, …). Requiring EVERY named relay to be dialable
/// locally made joining impossible whenever the two pools merely overlapped
/// instead of matching — the invitee saw "the invite names relay X, which
/// this node has not confirmed" for a relay it never needed.
///
/// The rule: the join proceeds over the INTERSECTION and is refused only
/// when that is empty. The group's own relay list (what the Welcome
/// carries) is still persisted whole — this node just dials the subset it
/// has confirmed, which is exactly the §6 "a workspace relay the operator
/// has not confirmed is not dialed silently" contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_needs_only_one_relay_in_common_with_the_invite() {
    // an onion relay the founder can name but nobody in this test can dial
    const FOUNDER_ONLY: &str =
        "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";

    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    // the founder's pool: the shared relay PLUS one only it has
    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    adopt_relay(&a, FOUNDER_ONLY).await;
    a.execute(Command::CreateStart {
        name: "Overlap".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();
    let inv = molt_engine::FoundingInvite::parse(&link).expect("joinable");
    assert!(
        inv.handover.relays.len() == 2 && inv.handover.relays.contains(&FOUNDER_ONLY.to_string()),
        "the invite names BOTH founder relays: {:?}",
        inv.handover.relays
    );

    // the joiner has ONLY the shared one — the overlap is a single relay
    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    // it must simply work: founder sees the join, charter is ratified, both seal
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Overlap".to_string(),
        agenda: "meet on the relay we share".to_string(),
    })
    .await
    .expect("proposed");
    wait_for(&b, "petra to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    wait_for(&b, "the join to seal", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    let s = wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    let ws_id_b = s.active_workspace.clone();

    // the joiner persists the GROUP's whole relay list (policy), even though
    // it dialed only the subset it confirmed
    b.execute(Command::CloseWorkspace).await.expect("close b");
    let dir_b = molt_storage::find_workspace_dir(&tmp.path().join("joiner"), &ws_id_b)
        .expect("b dir");
    let (ws_b, _) = molt_storage::open_workspace(&dir_b).expect("open b");
    let ts_b = ws_b.read_transport_state();
    assert_eq!(
        ts_b.relays,
        inv.handover.relays,
        "the group's relay list is persisted whole — dialing is gated separately"
    );
}

/// NEGATIVE — with NO relay in common the join is refused, and the message
/// names both sides so the operator can actually act (the old message told
/// them to confirm a relay they may have already confirmed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_with_no_relay_in_common_is_refused_with_an_actionable_message() {
    // two REACHABLE relays with no overlap — each node's own pool works, the
    // two simply never meet
    let founder_relay = MockRelay::run().await.expect("founder relay");
    let founder_only = founder_relay.url().await.to_string();
    let joiner_relay = MockRelay::run().await.expect("joiner relay");
    let url = joiner_relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &founder_only).await;
    a.execute(Command::CreateStart {
        name: "Disjoint".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    // the joiner's only relay is one the invite does not name
    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("the wizard arms");
    let s = wait_for(&b, "the join to refuse", |s| s.join.run.outcome == 2).await;
    let log = s.join.run.log.join(" ");
    assert!(
        log.contains("no relay in common"),
        "the refusal names the real problem: {log}"
    );
    assert!(
        log.contains(&founder_only),
        "…and lists what the invite asks for: {log}"
    );
    assert!(
        log.contains(&url),
        "…and what this node can actually dial: {log}"
    );
}

/// A parseable invite naming exactly these relays — no founder needed, the
/// relay gate refuses long before anything is dialed.
fn link_naming(relays: &[String]) -> String {
    molt_engine::FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "Gated".to_string(),
            threshold: 2,
            members: 2,
            inviter: "walter".to_string(),
            ticket: "ab".repeat(32),
        },
        handover: molt_net::invite::InviteHandoverV2 {
            seat: 0,
            ticket: "ab".repeat(32),
            npub: molt_net::nostr_identity(b"founder-entropy", "self-ticket").1,
            relays: relays.to_vec(),
        },
    }
    .render()
    .expect("a well-formed handover renders")
}

/// The refusal line that mentions `url` — one per relay the invite names.
fn line_about<'a>(log: &'a [String], url: &str) -> &'a str {
    log.iter()
        .find(|l| l.contains(url))
        .unwrap_or_else(|| panic!("no line about {url} in the refusal: {log:?}"))
        .as_str()
}

/// REGRESSION (user report, 2026-08-01 — "config3 could join, config2 could
/// not; nothing in the log, the UI just said the invitation was refused").
///
/// A relay hand-written into `config.toml` as `confirmed = true` but WITHOUT
/// `clearnet_enabled = true` is silently undialable, and the old refusal
/// ("no relay in common … this node can dial [nothing]") told the operator to
/// confirm a relay they had already confirmed — while the one thing that
/// would have helped, "non-onion dialing is switched off", was never said.
///
/// The rule: when nothing is dialable, the refusal diagnoses EVERY relay the
/// invite names, individually, against this node's own pool — each with the
/// one action that would fix THAT relay. A flat "no relay in common" is only
/// honest for a relay this node has never heard of.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_refusal_diagnoses_every_invite_relay_individually() {
    let absent = "wss://never-added.example".to_string();
    let unconfirmed = "wss://added-but-unconfirmed.example".to_string();
    let dark = "wss://confirmed-but-dark.example".to_string();

    let tmp = tempfile::tempdir().expect("tmp");
    let b = engine(&tmp.path().join("joiner"));
    // in the pool, never confirmed
    b.execute(Command::RelayAdd { url: unconfirmed.clone() }).await.expect("add");
    // confirmed WITH the exposure acknowledgement — then non-onion dialing
    // switched off again (the hand-written `confirmed = true` without
    // `clearnet_enabled = true` reaches exactly this state)
    adopt_relay(&b, &dark).await;
    b.execute(Command::RelayClearnetSession { unlock: false })
        .await
        .expect("go dark");

    b.execute(Command::JoinStart {
        invite: link_naming(&[absent.clone(), unconfirmed.clone(), dark.clone()]),
        member: "petra".to_string(),
    })
    .await
    .expect("the wizard arms");
    let s = wait_for(&b, "the join to refuse", |s| s.join.run.outcome == 2).await;
    let log = &s.join.run.log;

    // one short fault per relay — no instructions in the per-relay lines
    let l = line_about(log, &absent);
    assert!(l.ends_with("not in relay pool"), "unknown relay: {l}");
    let l = line_about(log, &unconfirmed);
    assert!(l.ends_with("not confirmed"), "a relay the operator CAN see: {l}");
    let l = line_about(log, &dark);
    assert!(l.ends_with("clearnet/local dialing off"), "the switched-off gate: {l}");

    // the terminal line carries the fix ONCE, and no longer claims the pools
    // merely fail to overlap (two of these three ARE in common)
    let terminal = log.iter().find(|l| l.starts_with('✗')).expect("a ✗ line");
    assert!(!terminal.contains("no relay in common"), "{terminal}");
    assert!(terminal.contains("clearnet_enabled"), "the key that lifts it: {terminal}");
    assert!(
        log.iter().filter(|l| l.contains("clearnet_enabled")).count() == 1,
        "the remedy is stated exactly once, not per relay: {log:?}"
    );
}

/// NEGATIVE — the single-use ticket over the relay: a SECOND person
/// activating the same link (valid MAC, different member) is told the link
/// is spent — over its own gift-wrap inbox — and its join fails fast; the
/// first activator's anchored seat stays untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_activation_of_the_same_link_fails_as_spent() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Duo".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&tmp.path().join("joiner-b"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("b joins");
    wait_for(&a, "petra's seat to anchor", |s| s.create.can_propose).await;

    // the interloper pastes the SAME link
    let c = engine(&tmp.path().join("joiner-c"));
    adopt_relay(&c, &url).await;
    c.execute(Command::JoinStart {
        invite: link,
        member: "carol".to_string(),
    })
    .await
    .expect("c's wizard arms");
    let s = wait_for(&c, "carol's join to fail as spent", |s| s.join.run.outcome == 2).await;
    assert!(
        s.join.run.log.iter().any(|l| l.contains("already used")),
        "the spent-link reason reaches the second activator: {:?}",
        s.join.run.log
    );
    // …and it is the reason for HER situation. A second PERSON needs her own
    // link; the same person retrying after group birth needs the founding
    // re-minted, and telling her to ask for a fresh link would be wrong
    // advice. The frame used to carry no reason at all, so both got one text.
    assert!(
        s.join.run.log.iter().any(|l| l.contains("your own")),
        "a second PERSON is told to ask for her own link: {:?}",
        s.join.run.log
    );

    // the anchored seat is still petra's
    let s = read_session(&a).await;
    assert_eq!(s.create.seats[0].member, "petra", "the seat stays with the first activator");
}

/// NEGATIVE — a declined charter aborts the founding on BOTH sides, over
/// the 445 group: the joiner's run fails as declined, the founder's seat
/// flips to declined and the create run fails — nothing seals, nothing
/// materializes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declined_charter_aborts_both_sides() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Nope".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the founder to see petra", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Nope".to_string(),
        agenda: "unacceptable terms".to_string(),
    })
    .await
    .expect("proposed");

    wait_for(&b, "petra to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinDeclineCharter).await.expect("decline");

    let s = wait_for(&b, "petra's join to fail as declined", |s| s.join.run.outcome == 2).await;
    assert!(
        s.join.run.log.iter().any(|l| l.contains("declined")),
        "the decline reason is in petra's run log: {:?}",
        s.join.run.log
    );
    let s = wait_for(&a, "the founder to see the decline", |s| s.create.run.outcome == 2).await;
    assert_eq!(s.create.seats[0].state, 3, "the seat is marked declined");
    assert!(s.workspaces.is_empty(), "nothing materialized on the founder");
}

/// NEGATIVE — a decline ends the founding for the BYSTANDER too (2026-08-08):
/// in a 2-of-3, petra ratifies and then waits; dora declines. The founder
/// fails — and petra must fail WITH it, loudly, instead of idling in a
/// waiting posture on a founding that can never seal (the abort frame
/// travels to every member, not only the decliner's own screen).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_decline_ends_the_founding_for_the_waiting_co_member_too() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Nope".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "two joinable links", |s| {
        s.create.seats.len() >= 2
            && s.create
                .seats
                .iter()
                .all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let link_b = s.create.seats[0].link.clone();
    let link_c = s.create.seats[1].link.clone();

    let b = engine(&tmp.path().join("petra"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link_b,
        member: "petra".to_string(),
    })
    .await
    .expect("petra joins");
    let c = engine(&tmp.path().join("dora"));
    adopt_relay(&c, &url).await;
    c.execute(Command::JoinStart {
        invite: link_c,
        member: "dora".to_string(),
    })
    .await
    .expect("dora joins");

    wait_for(&a, "the founder to see both joiners", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Nope".to_string(),
        agenda: "unacceptable terms".to_string(),
    })
    .await
    .expect("proposed");

    // petra RATIFIES and settles into the waiting posture…
    wait_for(&b, "petra to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("petra ratifies");
    // …then dora declines
    wait_for(&c, "dora to see the charter", |s| s.join.awaiting_ratify).await;
    c.execute(Command::JoinDeclineCharter).await.expect("dora declines");

    wait_for(&a, "the founder to fail", |s| s.create.run.outcome == 2).await;
    // THE point: the bystander leaves its waiting modal with an honest
    // failure — not a hang until some timeout
    let s = wait_for(&b, "petra's join to fail too", |s| s.join.run.outcome == 2).await;
    assert!(
        s.join.run.log.iter().any(|l| l.contains("declined")),
        "petra's log names the decline: {:?}",
        s.join.run.log
    );
    assert!(s.workspaces.is_empty(), "nothing materialized on petra");
}

/// REGRESSION (cluster I) — a founder with MORE relays than an invite may
/// carry still founds, over its first eight.
///
/// The 8-relay cap is untrusted-INPUT enforcement: it bounds what a pasted
/// link may make this node dial. It was being applied to the FOUNDER'S OWN
/// pool, so an operator who confirmed nine relays got no link at all and the
/// founding aborted outright ("9 relays — more than the 8 an invite may
/// carry"). Cap what goes IN, in the pool's own priority order.
///
/// The cap must be applied EXACTLY ONCE, upstream of both consumers: the
/// joiner requires the invite's relay set and the Welcome's to be identical,
/// so a link-only fix would move the failure to group birth. That is why this
/// test drives the whole choreography through to a sealed join rather than
/// stopping at "a link rendered".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founder_pool_over_the_link_cap_still_founds_over_its_first_eight() {
    const CAP: usize = 8;
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    // the live relay FIRST (pool order is the dial priority), then eight
    // syntactically valid but unreachable v3 onions. Onion pads fail
    // fail-closed and INSTANTLY under the direct dialer — a clearnet or
    // .invalid pad would stall every publish on a real resolver.
    adopt_relay(&a, &url).await;
    let pads: Vec<String> = "bcdefghi"
        .chars()
        .map(|c| format!("wss://{}{}.onion", "a".repeat(55), c))
        .collect();
    for p in &pads {
        adopt_relay(&a, p).await;
    }

    a.execute(Command::CreateStart {
        name: "Overflow".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");

    // fail fast on the red run: surface the real refusal instead of a 30 s wait
    let s = wait_for(&a, "a joinable link (or an honest refusal)", |s| {
        s.create.run.outcome == 2
            || (!s.create.seats.is_empty()
                && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok())
    })
    .await;
    assert_ne!(
        s.create.run.outcome, 2,
        "the founding must not abort on the operator's own pool size: {:?}",
        s.create.run.log
    );
    let link = s.create.seats[0].link.clone();
    let inv = molt_engine::FoundingInvite::parse(&link).expect("joinable");

    // exactly the first eight, in pool order
    let mut expected: Vec<String> = vec![url.clone()];
    expected.extend(pads.iter().take(CAP - 1).cloned());
    assert_eq!(inv.handover.relays, expected, "the first {CAP} in priority order");
    assert!(
        !inv.handover.relays.contains(pads.last().expect("a ninth relay")),
        "the ninth relay is dropped, not carried"
    );
    // …and the operator is TOLD, so a silent truncation cannot read as
    // "the app is using my whole pool"
    assert!(
        s.create.run.log.iter().any(|l| l.contains("9") && l.contains(&CAP.to_string())),
        "the founding log names how many of the pool the invite carries: {:?}",
        s.create.run.log
    );

    // the whole choreography must still complete — this is what pins the
    // WELCOME leg (capped identically) and the joiner's invite-set ==
    // Welcome-set equality check
    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart { invite: link, member: "petra".to_string() })
        .await
        .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Overflow".to_string(),
        agenda: "found over a pool bigger than a link".to_string(),
    })
    .await
    .expect("proposed");
    wait_for(&b, "petra to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    wait_for(&b, "the join to seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty())
        .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
}

/// A sealed roster the joiner's own engine would accept, plus the matching
/// nostr secret — byte-identical in shape to what an abandoned member task
/// really emits (`spawn_member_join`'s `NetJoinSealed`).
fn injected_seal(member: &str) -> (String, String) {
    use molt_core::{MemberIdentity, RosterAttestation};
    let (sk_f, pk_f) = molt_storage::derive_identity_key(&[7u8; 32], "founder");
    let (sk_m, pk_m) = molt_storage::derive_identity_key(&[9u8; 32], member);
    let (nostr_sk, nostr_pk) = molt_net::nostr_identity(b"petra-entropy", "ticket-petra");
    let identities = vec![
        MemberIdentity {
            member: "founder".to_string(),
            identity_pk: pk_f,
            nostr_pk: molt_net::nostr_identity(b"founder-entropy", "ticket-f").1,
        },
        MemberIdentity {
            member: member.to_string(),
            identity_pk: pk_m,
            nostr_pk,
        },
    ];
    let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
    let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "", &[], None);
    let sealed = molt_core::SealedRoster {
        relays: Vec::new(),
        name: "R".to_string(),
        republic_id,
        rule_m: 2,
        rule_n: 2,
        roster: vec!["founder".to_string(), member.to_string()],
        identities,
        attestations: vec![
            RosterAttestation {
                member: "founder".to_string(),
                sig: molt_storage::identity_sign(&sk_f, &table),
            },
            RosterAttestation {
                member: member.to_string(),
                sig: molt_storage::identity_sign(&sk_m, &table),
            },
        ],
        agenda: String::new(),
        features: None,
    };
    (
        serde_json::to_string(&sealed).expect("sealed json"),
        hex::encode(nostr_sk),
    )
}

/// SECURITY (cluster D) — starting a FOUNDING must invalidate an in-flight
/// join, or the abandoned join's late seal hijacks the session.
///
/// `cmd_join_start` and `cmd_join_cancel` bump `join_generation` and abort the
/// task; the founding and recovery entry points were missed. The only gate on
/// `cmd_net_join_sealed` is that generation — so a `NetJoinSealed` arriving
/// from a join the user walked away from still materialized a workspace, set
/// `active_workspace` and flipped the screen to Main, out from under the
/// founding wizard. The user ends up inside a republic they never created.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_invalidates_an_in_flight_join() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    // a real founding on A, so B has a genuine live join to abandon
    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Other".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    // the join must be genuinely LIVE, or a later green proves nothing
    let s = read_session(&b).await;
    assert_eq!(s.join.run.outcome, 0, "the join is in flight");

    // …the user changes their mind and founds their own republic instead
    b.execute(Command::CreateStart {
        name: "Mine".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("b starts its own founding");

    // the abandoned join's task now reports in, exactly as it really would
    let (sealed, nostr_sk) = injected_seal("petra");
    b.execute(Command::NetJoinSealed {
        sealed,
        mls: String::new(),
        mesh: Vec::new(),
        nostr_sk,
        relays: Vec::new(),
        rotation_seed: String::new(),
        generation: Some(1),
    })
    .await
    .expect("the late report is accepted and dropped");

    let s = read_session(&b).await;
    assert!(
        !s.workspaces.iter().any(|w| w.name == "R"),
        "the abandoned join must not materialize a republic: {:?}",
        s.workspaces.iter().map(|w| &w.name).collect::<Vec<_>>()
    );
    assert_eq!(s.screen, molt_core::Screen::Create, "the founding wizard keeps the screen");
    assert_eq!(s.create.run.outcome, 0, "…and its own run is untouched");
}

/// …and the same for RECOVERY — the second entry point that switched the
/// session out of a join without invalidating it. A recovery additionally
/// abandons any in-flight FOUNDING (the symmetric hole: `maybe_finalize`
/// would otherwise seal a founding into the recovery session).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_invalidates_an_in_flight_join() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Other".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    assert_eq!(read_session(&b).await.join.run.outcome, 0, "the join is in flight");

    // the user abandons it for a recovery instead. It fails honestly (N4b is
    // not built) — but the CONTEXT is armed, which is all the hijack needed.
    let _ = b
        .execute(Command::RecoverStart {
            link: molt_engine::RecoveryInvite {
                republic: "Guild".to_string(),
                member: "petra".to_string(),
                ticket: "ab".repeat(8),
                server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid"
                    .to_string(),
                queue_id: "cd".repeat(12),
                wrap: "ef".repeat(32),
                republic_id: "f00d".to_string(),
                handover: None,
            }
            .render(),
            phrase: "abandon abandon abandon".to_string(),
        })
        .await;

    let (sealed, nostr_sk) = injected_seal("petra");
    b.execute(Command::NetJoinSealed {
        sealed,
        mls: String::new(),
        mesh: Vec::new(),
        nostr_sk,
        relays: Vec::new(),
        rotation_seed: String::new(),
        generation: Some(1),
    })
    .await
    .expect("the late report is accepted and dropped");

    let s = read_session(&b).await;
    assert!(
        !s.workspaces.iter().any(|w| w.name == "R"),
        "the abandoned join must not materialize a republic during a recovery: {:?}",
        s.workspaces.iter().map(|w| &w.name).collect::<Vec<_>>()
    );
    assert_ne!(s.screen, molt_core::Screen::Main, "the recovery keeps the screen");
}

// ---------------------------------------------------------------------------
// Cluster C — a relay that REFUSES kind-445, so a publish failure is a real
// wire outcome rather than an injected one.
// ---------------------------------------------------------------------------

use nostr_relay_builder::builder::{PolicyResult, RelayBuilder, WritePolicy};
use nostr_relay_builder::prelude::{BoxedFuture, Event, Kind};
use nostr_relay_builder::LocalRelay;

/// Rejects kind-445 frames after `accept_first` of them. Filtering strictly on
/// 445 keeps the 1059 gift-wrap traffic (join requests, Welcomes) out of the
/// counter, so the count means what the test says it means.
#[derive(Debug)]
struct Reject445After(std::sync::atomic::AtomicUsize, usize);

impl Reject445After {
    fn new(accept_first: usize) -> Self {
        Self(std::sync::atomic::AtomicUsize::new(0), accept_first)
    }
}

impl WritePolicy for Reject445After {
    fn admit_event<'a>(
        &'a self,
        event: &'a Event,
        _addr: &'a std::net::SocketAddr,
    ) -> BoxedFuture<'a, PolicyResult> {
        Box::pin(async move {
            if event.kind != Kind::Custom(445) {
                return PolicyResult::Accept;
            }
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.1 {
                PolicyResult::Accept
            } else {
                PolicyResult::Reject("test policy: no 445 for you".to_string())
            }
        })
    }
}

async fn relay_rejecting_445_after(accept_first: usize) -> (LocalRelay, String) {
    let relay = LocalRelay::new(RelayBuilder::default().write_policy(Reject445After::new(accept_first)));
    relay.run().await.expect("policy relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// REGRESSION (cluster C) — a Seal that NO relay accepts must fail the
/// founding, not hang it.
///
/// `spawn_publish_frame_with`'s failure sink had zero `Some(...)` callers, so
/// the one path that reports a refused publish was dead code. The founder sat
/// on "charter proposed" forever, every member waited for a frame that was
/// never accepted, and `NetRitualFailed` — which exists for exactly this —
/// was never reached. A seam that exists but is not wired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seal_that_no_relay_accepts_fails_the_founding_instead_of_hanging() {
    // accept the founder's Welcome/1059 traffic, refuse every 445 — the Seal
    // is the first 445 of the founding
    let (_relay, url) = relay_rejecting_445_after(0).await;
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay_unprobed(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Doomed".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay_unprobed(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;

    a.execute(Command::CreatePropose {
        name: "Doomed".to_string(),
        agenda: "this seal will never land".to_string(),
    })
    .await
    .expect("proposed");

    // the Seal publish is refused by the only relay: the founding must FAIL,
    // visibly, instead of sitting on "charter proposed" forever
    let s = wait_for(&a, "the founding to fail on the refused seal", |s| {
        s.create.run.outcome == 2
    })
    .await;
    assert!(
        s.create.run.log.iter().any(|l| l.contains("seal") && l.contains("publish")),
        "the log names the leg that could not be published: {:?}",
        s.create.run.log
    );
    assert!(s.workspaces.is_empty(), "nothing was founded");
}

/// REGRESSION (cluster F2) — a founder's cancel must REACH the members.
///
/// `cmd_create_cancel` only tore the ritual down locally. Every member sat in
/// an unbounded wait (`loop { recv(RECV_SLICE) }` with no deadline and no
/// progress surface), so a dead founding was indistinguishable from a slow
/// one — forever. The abort travels as a 445 group frame once the group is
/// born, and as a gift-wrap per anchored seat before that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founder_cancel_reaches_the_members_inside_the_born_group() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Abandoned".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    // all-joined ⇒ the group is BORN, so petra is now waiting on the 445
    // channel rather than her gift-wrap inbox
    wait_for(&a, "the group to be born", |s| s.create.can_propose).await;

    a.execute(Command::CreateCancel).await.expect("the founder gives up");

    let s = wait_for(&b, "petra to learn the founding is over", |s| {
        s.join.run.outcome == 2
    })
    .await;
    assert!(
        s.join.run.log.iter().any(|l| l.contains("founder ended this founding")),
        "she is told WHY, not just that it stopped: {:?}",
        s.join.run.log
    );
    assert!(s.workspaces.is_empty(), "nothing materialized on her side");
}

/// REGRESSION (cluster F3) — a legitimate RETRY of the same link by the same
/// person must keep the seat, not burn it.
///
/// `cmd_join_start` mints a fresh seed phrase on every start, so a retry after
/// a transport hiccup always derives a DIFFERENT identity — which the founder
/// read as "a second person activated this link" and refused with LinkSpent,
/// leaving the seat anchored to the joiner's dead first identity and the
/// founding wedged. (The backlog blamed the comparison; the comparison is
/// correct. The bug was that no re-activation path existed at all.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_of_the_same_link_by_the_same_joiner_keeps_the_seat() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    // 3 seats so the group is NOT born when petra retries (birth needs every
    // seat anchored) — that is the window a retry is resumable in
    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Retry".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "two joinable links", |s| {
        s.create.seats.len() == 2
            && s.create
                .seats
                .iter()
                .all(|x| molt_engine::FoundingInvite::parse(&x.link).is_ok())
    })
    .await;
    let link0 = s.create.seats[0].link.clone();
    let link1 = s.create.seats[1].link.clone();

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart { invite: link0.clone(), member: "petra".to_string() })
        .await
        .expect("first attempt");
    wait_for(&a, "petra's seat to anchor", |s| s.create.seats[0].member == "petra").await;

    // …the transport hiccups: petra abandons the stuck attempt and retries
    // with the same link. JoinCancel + JoinStart is the real user flow (the
    // engine refuses a second JoinStart while one is running), and the retry
    // mints a FRESH phrase, so the founder sees a different identity_pk.
    b.execute(Command::JoinCancel).await.expect("she gives up on the stuck attempt");
    b.execute(Command::JoinStart { invite: link0, member: "petra".to_string() })
        .await
        .expect("the retry arms");
    let s = wait_for(&a, "the founder to accept the re-activation", |s| {
        s.create.run.log.iter().any(|l| l.contains("re-activated"))
    })
    .await;
    assert_eq!(s.create.seats[0].member, "petra", "the seat is still hers");
    assert!(
        !s.create.run.log.iter().any(|l| l.contains("activated a second time")),
        "her own retry is not treated as a stranger: {:?}",
        s.create.run.log
    );

    // and the founding still completes with her SECOND identity
    let c = engine(&tmp.path().join("third"));
    adopt_relay(&c, &url).await;
    c.execute(Command::JoinStart { invite: link1, member: "carol".to_string() })
        .await
        .expect("carol joins");
    wait_for(&a, "all seats", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Retry".to_string(),
        agenda: "a hiccup must not cost the seat".to_string(),
    })
    .await
    .expect("proposed");
    wait_for(&b, "petra to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("petra ratifies");
    wait_for(&c, "carol to see the charter", |s| s.join.awaiting_ratify).await;
    c.execute(Command::JoinConfirmCharter).await.expect("carol ratifies");
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
}

/// Rejects every REQ, while still accepting writes — the relay that is
/// connected and silent, which is the shape auth-required / rate-limited
/// relays actually present.
#[derive(Debug)]
struct RejectEveryReq;

impl nostr_relay_builder::builder::QueryPolicy for RejectEveryReq {
    fn admit_query<'a>(
        &'a self,
        _query: &'a nostr_relay_builder::prelude::Filter,
        _addr: &'a std::net::SocketAddr,
    ) -> BoxedFuture<'a, PolicyResult> {
        Box::pin(async move { PolicyResult::Reject("no reqs".to_string()) })
    }
}

/// REGRESSION (cluster G) — a founding must REFUSE when its inbox is not
/// readable, instead of publishing invite links over it.
///
/// `subscribe()` succeeds as soon as a relay accepts the REQ; it says nothing
/// about whether the relay will ever REPLAY. The three `let _ = live(...)`
/// sites discarded exactly that answer, so "subscribe before advertise"
/// degraded to "advertise blind": links went out over an inbox nothing would
/// ever answer on, and the founding then timed out with no error anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founding_refuses_when_its_inbox_never_becomes_readable() {
    let relay = LocalRelay::new(RelayBuilder::default().query_policy(RejectEveryReq));
    relay.run().await.expect("silent relay runs");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay_unprobed(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Unreadable".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");

    let s = wait_for(&a, "the founding to refuse the unreadable inbox", |s| {
        s.create.run.outcome == 2
    })
    .await;
    assert!(
        s.create.run.log.iter().any(|l| l.contains("not readable")),
        "the refusal names the unreadable subscription: {:?}",
        s.create.run.log
    );
    // …and NO seat link was ever advertised over it
    assert!(
        s.create
            .seats
            .iter()
            .all(|x| molt_engine::FoundingInvite::parse(&x.link).is_err()),
        "no joinable link may be published over an inbox nothing replayed"
    );
}

/// SECURITY (adversarial swoop, 2026-08-01) — a joiner may NOT take the
/// founder's handle.
///
/// Every "is this the founder?" gate on the 445 channel is a string compare
/// against `info.inviter` — a handle printed in every invite link. The MLS
/// leaf credential is whatever the joiner typed, `key_package_binding` only
/// requires it to equal the CLAIMED member (satisfied by construction), and
/// OpenMLS enforces uniqueness of signature/encryption/init keys but never of
/// credential identities. So a legitimate invitee who simply types the
/// founder's name was welcomed into the group with a credential that
/// satisfies `frame_is_from_founder` — able to end every other seat's join
/// with one `Aborted` frame, or propose a charter as the founder.
///
/// The chain layer independently ASSUMES handle uniqueness (`valid_signers`
/// counts a set of NAMES), so duplicates also make a republic ungovernable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_may_not_claim_the_founders_handle() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Impersonation".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    // the invitee types the founder's handle, which the link tells them
    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "walter".to_string(),
    })
    .await
    .expect("the wizard arms");

    let s = wait_for(&a, "the founder to refuse the taken handle", |s| {
        s.create.run.log.iter().any(|l| l.contains("already taken"))
    })
    .await;
    assert!(
        s.create.seats[0].member.is_empty(),
        "the seat must NOT anchor an impersonator: {:?}",
        s.create.seats[0].member
    );
    assert!(!s.create.can_propose, "…and the founding must not proceed on it");
}

/// SECURITY (adversarial swoop, 2026-08-01) — a re-activation that FAILS must
/// not destroy the seat it tried to replace.
///
/// The first version of the F3 re-anchor cleared the seat's identity, key
/// package and reply handover BEFORE running the ingest ladder. Since
/// `verify_join_mac` is a pure HMAC over the ticket, any holder of a leaked
/// link can mint a request that passes the MAC while failing
/// proof-of-possession — and that failure left the seat EMPTY, evicting the
/// honest member and making `all_joined` unreachable forever.
///
/// Stage-then-commit: nothing is touched until every check has passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_re_activation_leaves_the_honest_seat_intact() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Eviction".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "two joinable links", |s| {
        s.create.seats.len() == 2
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link0 = s.create.seats[0].link.clone();
    let inv = molt_engine::FoundingInvite::parse(&link0).expect("parse");

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart { invite: link0, member: "petra".to_string() })
        .await
        .expect("petra joins");
    let s = wait_for(&a, "petra's seat to anchor", |s| s.create.seats[0].member == "petra").await;
    let honest_anchor = s.create.seats[0].member.clone();

    // an attacker with the leaked link mints a MAC-valid request under
    // petra's handle but claims a transport key it does not hold — the
    // gift-wrap's proven sealer will not match, so PoP must refuse it
    let (_evil_sk, evil_npk) = molt_net::nostr_identity(b"evil-entropy", "evil");
    let (attacker_sk, _) = molt_net::nostr_identity(b"attacker-entropy", "atk");
    let attacker = molt_net::ritual_net::RitualNet::new(
        molt_net::dial::Dialer::resolve("none", "local", 0).expect("dialer"),
        vec![url.clone()],
        &attacker_sk,
    )
    .expect("attacker endpoint");
    let (_, evil_idpk) = molt_storage::derive_identity_key(&[42u8; 32], "petra");
    let mac = molt_net::invite::join_mac(&inv.handover.ticket, "petra", &evil_idpk, &evil_npk);
    attacker
        .send_ritual(
            &inv.handover.npub,
            &molt_net::invite::RitualMsg::Join(molt_net::invite::JoinRequest {
                seat: 0,
                name: "petra".to_string(),
                identity_pk: evil_idpk,
                nostr_pk: evil_npk,
                mac,
                reply: None,
                key_package: String::new(),
                relays: Vec::new(),
            }),
        )
        .await
        .expect("the hostile request publishes");

    // give the founder time to process and refuse it
    tokio::time::sleep(Duration::from_secs(2)).await;

    // The session VIEW keeps the old name even when the ritual seat is
    // cleared, so asserting on it proves nothing. The observable damage is
    // that an emptied seat can never complete the founding: `all_joined`
    // counts ANCHORED seats, so `can_propose` never flips once one is lost.
    let c = engine(&tmp.path().join("third"));
    adopt_relay(&c, &url).await;
    c.execute(Command::JoinStart {
        invite: s.create.seats[1].link.clone(),
        member: "carol".to_string(),
    })
    .await
    .expect("carol joins");
    let s = wait_for(&a, "all seats anchored despite the hostile attempt", |s| {
        s.create.can_propose
    })
    .await;
    assert_eq!(
        s.create.seats[0].member, honest_anchor,
        "…and the seat is still the honest member's: {:?}",
        s.create.run.log
    );
}

/// REGRESSION (field report, 2026-08-01) — changing the relay pool during a
/// live founding must SAY that the already-minted invites are now stale.
///
/// The invites carry the pool as it was at `CreateStart`. An operator whose
/// joiner was refused with "no relay in common" added a shared relay, watched
/// both pools go green, and the SAME invite kept being refused — because the
/// link in their hand still named the old relays. Nothing said so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changing_the_pool_during_a_founding_says_the_invites_are_stale() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Stale".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    wait_for(&a, "a joinable link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    // the operator adds another relay mid-founding, trying to fix a refused join
    a.execute(Command::RelayAdd { url: "wss://later.example".to_string() })
        .await
        .expect("add");
    let s = read_session(&a).await;
    assert!(
        s.create.run.log.iter().any(|l| l.contains("already minted still name")),
        "the founding log must say the outstanding links are stale: {:?}",
        s.create.run.log
    );
    // …and confirming it does not stack a second identical line
    a.execute(Command::RelayConfirm {
        url: "wss://later.example".to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("confirm");
    let s = read_session(&a).await;
    assert_eq!(
        s.create.run.log.iter().filter(|l| l.contains("already minted still name")).count(),
        1,
        "the warning must not stack per pool edit: {:?}",
        s.create.run.log
    );
}

/// The failure an operator must act on gets a SHORT headline, not just a
/// sentence buried at the end of the log.
///
/// User instruction (2026-08-01): error messages must be short, to the point,
/// large and in the signal colour. The GUI renders `run.headline` at h1/h2 in
/// `Theme.bad`; the log keeps the detail. A headline that carries the
/// explanation is the wall of text again, one line higher up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_join_gets_a_short_headline_not_only_a_log_line() {
    let tmp = tempfile::tempdir().expect("tmp");

    // the joiner's only relay is one the invite does not name. No MockRelay:
    // the gate refuses before anything is dialed, and a relay standing by
    // that nothing ever connects to would suggest otherwise.
    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, "wss://mine.example").await;
    b.execute(Command::JoinStart {
        invite: link_naming(&["wss://never-added.example".to_string()]),
        member: "petra".to_string(),
    })
    .await
    .expect("the wizard arms");

    let s = wait_for(&b, "the join to refuse", |s| s.join.run.outcome == 2).await;
    let h = s.join.run.headline.clone();
    assert!(!h.is_empty(), "a failed run carries a headline: {:?}", s.join.run.log);
    assert!(
        h.split_whitespace().count() <= 5 && h.len() <= 32,
        "…and it is a few words, renderable large: {h:?}"
    );
    assert_eq!(h, "No shared relay", "…naming the missing thing, nothing else");
    // the detail is still there, just not the headline's job
    assert!(
        s.join.run.log.iter().any(|l| l.contains("no relay in common")),
        "the log keeps the detail: {:?}",
        s.join.run.log
    );
}

/// The founder PICKS the republic's relays; the invite carries exactly that
/// pick, and a relay the founder cannot dial is refused rather than dropped.
///
/// Before this, the invite's list was an accident: `start_ritual` took the
/// node's whole dialable pool and capped it at eight IN POOL ORDER, so nobody
/// chose what the republic would run on — and R3 is about to make that set
/// constitutional (every member signs it into the genesis). A joiner's refusal
/// can only say "the republic uses these" if the founder meant them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_founder_picks_the_republics_relays_and_the_invite_carries_the_pick() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    // a second confirmed relay the founder deliberately leaves OUT
    adopt_relay(&a, "wss://not-chosen.example").await;

    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: vec![url.clone()],
    })
    .await
    .expect("founding starts on the picked relay");

    let s = wait_for(&a, "the seat link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let inv = molt_engine::FoundingInvite::parse(&s.create.seats[0].link).expect("parses");
    assert_eq!(
        inv.handover.relays,
        vec![url.clone()],
        "the invite names the PICK, not the whole pool"
    );

    // …and a pick this node cannot dial is refused, with the relay named
    let b = engine(&tmp.path().join("other"));
    adopt_relay(&b, &url).await;
    let err = b
        .execute(Command::CreateStart {
            name: "Ghost".to_string(),
            member: "walter".to_string(),
            threshold: 2,
            members: 2,
            relays: vec!["wss://never-added.example".to_string()],
        })
        .await
        .expect_err("a relay this node cannot dial must refuse the founding");
    assert!(
        format!("{err}").contains("never-added.example"),
        "…naming the offending relay: {err}"
    );
}

/// **R3** — the relay pool is signed by everyone, not merely configured.
///
/// The pool decides who can reach whom (relays do not federate), so it is as
/// constitutional as the roster. Since `molt-roster-v4` it rides inside the
/// bytes every member signs at the seal, which means a founder cannot seal a
/// pool different from the one that was ratified — the same sign-what-you-see
/// property the charter already had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_pool_is_bound_into_what_every_member_signs() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");

    let a = engine(&root_a);
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: vec![url.clone()],
    })
    .await
    .expect("founding starts");

    let s = wait_for(&a, "the seat link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess".to_string(),
    })
    .await
    .expect("charter");
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let s = wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    let ws_id = s.active_workspace.clone();
    // entering is gated on the phrase-backup step now (2026-08-08)
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join seal", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");

    // …the genesis on disk carries the pool, and the chain still verifies —
    // which is the whole claim: the attestations were made over bytes that
    // INCLUDE these relays.
    let dir = molt_storage::find_workspace_dir(&root_a, &ws_id).expect("dir");
    let (ws, _) = molt_storage::open_workspace(&dir).expect("open");
    let (_blob, chain) = ws.read_chain();
    let genesis = chain.first().expect("a genesis block");
    let molt_core::chain::ChainChange::Genesis { relays, .. } = &genesis.change else {
        panic!("block 0 is not a genesis");
    };
    assert_eq!(
        relays,
        &vec![url.clone()],
        "the ratified pool must be in the genesis"
    );
    molt_engine::verify_chain(&chain).expect("the signed chain still verifies over v4 bytes");

    // …and a doctored pool breaks every attestation, which is what "signed"
    // has to mean. Without this the field could ride along unbound.
    let mut forged = chain.clone();
    let molt_core::chain::ChainChange::Genesis { relays, .. } = &mut forged[0].change else {
        unreachable!()
    };
    relays.push("wss://the-founder-added-this-later.example".to_string());
    assert!(
        molt_engine::verify_chain(&forged).is_err(),
        "a pool nobody ratified must not verify"
    );
}
