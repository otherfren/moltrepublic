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
