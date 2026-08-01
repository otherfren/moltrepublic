// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N4b step 5** (`docs/transport/nostr_n4_plan.md` §8.8): the coordinator
//! mints a recovery link over RELAYS.
//!
//! The recovery twin of `nostr_founding.rs`. Both engines are storage-backed
//! and driven purely through the public Command surface — a mint that only
//! works when a test injects transport material would pin nothing (the N3
//! lesson: a keystone driving an API the product does not call).

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
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
/// relay itself, then unlocks the clearnet/local session.
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

/// Found a real 2-of-2 "Chess Club" over one in-process relay, exactly as
/// `nostr_founding.rs`'s capstone does, and hand back both live engines.
async fn found_two_of_two(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
    })
    .await
    .expect("a production founding starts over the confirmed relay");

    let s = wait_for(&a, "the seat link to become a joinable v2 link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("joiner"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
    })
    .await
    .expect("charter proposed");

    wait_for(&b, "petra to see the proposed charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");

    wait_for(&a, "the founding to seal on the founder", |s| {
        s.create.run.outcome == 1 && s.screen == molt_core::Screen::Main
    })
    .await;
    wait_for(&b, "the join to seal on petra", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// A survivor mints a recovery link for a lost seat, over the relays — no
/// mesh, no queue.
///
/// Before N4b step 5 this reported `recovery-link-failed:mesh-not-running`:
/// `cmd_recover_invite_start` required `runtime_transport()` to mint a queue,
/// which no Nostr republic has. The link must instead carry the **v2
/// handover** — the coordinator's own transport anchor plus the relays it
/// listens on — because that is the only thing a total-loss rejoiner can
/// address a gift wrap to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn minting_a_recovery_link_over_relays_renders_a_v2_link() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;

    // petra lost her device; walter mints her a fresh recovery link
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("the human's decision is acked, never a command error");

    let s = wait_for(&a, "the recovery link to be minted", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed over relays, got {:?}", s.notice))
        .to_string();

    let inv = molt_engine::RecoveryInvite::parse(&link).expect("a parseable recovery link");
    assert_eq!(inv.member, "petra", "the link names the returning seat");
    let h = inv
        .handover
        .as_ref()
        .expect("a v2 handover — the queue shape cannot reach a rejoiner over relays");
    assert_eq!(h.relays, vec![url.clone()], "…the relays the coordinator listens on");
    assert!(!h.npub.is_empty(), "…and the coordinator's own transport anchor");
    assert_eq!(
        h.republic_id, inv.republic_id,
        "…and the republic id the seat proof binds"
    );
    assert_eq!(h.ticket, inv.ticket, "the handover carries the FULL ticket");

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// Having SOME dialable relay is not the same as sharing one with the
/// republic — and the mint must know the difference.
///
/// This is the rule the intersection exists for: the coordinator's pool holds
/// a perfectly dialable relay that the group does not use, while the group's
/// own relay is blocked here. A mint that advertised "whatever I can dial"
/// would hand out a link naming a relay no other member listens on; relays do
/// not federate, so the returning member would reach nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dialable_relay_the_group_does_not_use_is_not_a_shared_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;

    // an onion relay is dialable without the clearnet acknowledgement, so
    // after locking the session this node still has something it CAN reach —
    // just not anything this republic uses
    let onion = "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion".to_string();
    a.execute(Command::RelayAdd { url: onion.clone() })
        .await
        .expect("onion relay add");
    // …confirmed, but with no clearnet acknowledgement: an onion relay needs
    // none, which is the whole point of the fixture
    a.execute(Command::RelayConfirm {
        url: onion,
        accept_clearnet: false,
    })
    .await
    .expect("onion relay confirm");
    // the republic's relay leaves the pool entirely — this must be the
    // "shares nothing with this republic" case, NOT "blocked by a switch",
    // or the assertion below would pin a reason the fixture does not produce
    a.execute(Command::RelayRemove { url: url.clone() })
        .await
        .expect("drop the republic's relay");

    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("acked");

    let s = wait_for(&a, "the mint to refuse", |s| {
        s.notice.starts_with("recovery-link-failed:") || s.notice.starts_with("recovery-link:")
    })
    .await;
    let reason = s.notice.strip_prefix("recovery-link-failed:").unwrap_or_else(|| {
        panic!("a relay the group does not use must not mint a link, got {:?}", s.notice)
    });
    assert_eq!(
        reason, "no relay in common with this republic",
        "…and it says so, rather than blaming the operator's own pool"
    );

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// A coordinator that cannot dial any of the republic's relays says WHICH of
/// the three reasons it is — it does not advertise an address nobody is
/// listening on, and it does not fall back to "mesh-not-running".
///
/// Relays do not federate (`relay_pool.md` §2.6), so "the group uses this
/// relay" and "I am reachable there" are different questions. Minting a link
/// naming a relay this node cannot reach would strand the returning member at
/// a dead address — the one person who has already lost their device and has
/// no second channel to notice on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coordinator_that_cannot_reach_the_group_relays_says_which_switch() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;

    // the operator locks the clearnet/local session again: the republic's
    // relay is still in the pool and still confirmed, but no longer dialable
    a.execute(Command::RelayClearnetSession { unlock: false })
        .await
        .expect("session lock");

    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("the decision is still acked — this is an operational state");

    let s = wait_for(&a, "the mint to refuse", |s| {
        s.notice.starts_with("recovery-link-failed:") || s.notice.starts_with("recovery-link:")
    })
    .await;
    let reason = s
        .notice
        .strip_prefix("recovery-link-failed:")
        .unwrap_or_else(|| panic!("an unreachable coordinator must not mint, got {:?}", s.notice));
    assert!(
        reason.contains("clearnet_enabled"),
        "…naming the switch that actually blocked it, got {reason:?}"
    );
    assert_ne!(
        reason, "mesh-not-running",
        "a Nostr republic has no mesh — that answer sends the operator nowhere"
    );
    // …and the honest "no relay runtime yet" state survives the mint. The
    // dialer resolves fine here, and a resolver that wrote `Ok` on success
    // would turn the network pill green for the rest of the session on a
    // republic whose traffic still goes nowhere (N5 is not built).
    assert!(
        matches!(&s.net_health, molt_core::NetHealth::Down { reason } if reason.contains("N5")),
        "minting must not promise a runtime that does not exist, got {:?}",
        s.net_health
    );

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
