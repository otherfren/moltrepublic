// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The N4a capstone** (`docs/transport/nostr_n4_plan.md` §9 step 7): two
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
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// KEYSTONE — the full production founding+join choreography over Nostr:
/// CreateStart → (link v2 via the once-dormant NetRitualLinkReady) →
/// JoinStart on a second engine → founder sees the join → CreatePropose →
/// joiner ratifies → both seal, auto-enter, and persist the v4 transport
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

    // both sides seal and auto-enter
    let s = wait_for(&a, "the founding to seal on the founder", |s| {
        s.create.run.outcome == 1 && s.screen == molt_core::Screen::Main
    })
    .await;
    let ws_id_a = s.active_workspace.clone();
    let s = wait_for(&b, "the join to seal on petra", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    let ws_id_b = s.active_workspace.clone();

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
        ..
    } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!((*rule_m, *rule_n), (2, 2));
    assert_eq!(agenda, "play chess, decide together");
    assert_eq!(identities.len(), 2);
    assert_eq!(attestations.len(), 2);
    let table =
        molt_core::roster_canonical_bytes(republic_id, *rule_m, *rule_n, identities, agenda);
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

    // reopen honesty (§7.5): a Nostr workspace is NOT "detached" — it pends
    // on its N5 runtime, and the health reason says so
    a.execute(Command::OpenWorkspace { id: ws_id_a }).await.expect("reopen a");
    let s = read_session(&a).await;
    assert_ne!(s.notice, "detached", "a Nostr workspace is not detached");
    assert!(
        matches!(&s.net_health, molt_core::NetHealth::Down { reason } if reason.contains("N5")),
        "honest pending-runtime health, got {:?}",
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
    let s = wait_for(&b, "the join to seal", |s| {
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
    wait_for(&b, "the join to seal", |s| {
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
    let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "");
    let sealed = molt_core::SealedRoster {
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
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Doomed".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
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
