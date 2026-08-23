// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N4b step 5** (`docs_archive/transport/nostr_n4_plan.md` §8.8): the coordinator
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

/// Found a real 2-of-2 "Chess Club" over one in-process relay, exactly as
/// `nostr_founding.rs`'s capstone does, and hand back both live engines.
async fn found_two_of_two(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let (a, b, _, _) = found_republic(root, url, 2).await;
    (a, b)
}

/// Found a real `threshold`-of-2 "Chess Club" over one in-process relay and
/// hand back both live engines plus **petra's recovery phrase** — which a
/// total-loss rejoiner is the only thing that still has.
/// The second voice: wait until `w` sees the open proposal whose payload
/// `value` matches, then approve it through the public command surface.
async fn approve_value(w: &WalletHandle, value: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals } =
            w.execute(Command::ListProposals).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("value").and_then(|v| v.as_str()) == Some(value)
            }) {
                w.execute(Command::Approve { proposal: p.id })
                    .await
                    .expect("approve");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the proposal {value:?} never reached the second voice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn found_republic(
    root: &std::path::Path,
    url: &str,
    threshold: u8,
) -> (WalletHandle, WalletHandle, String, String) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold,
        members: 2,
        relays: Vec::new(),
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
    // the phrase the engine minted for petra: shown once during the join, and
    // after a total loss it is all that is left of the seat
    let petra_phrase = wait_for(&b, "petra's recovery phrase to be minted", |s| {
        !s.join.seed.is_empty()
    })
    .await
    .join
    .seed
    .clone();

    wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    let walter_phrase = {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_.clone() })
            .await
            .expect("founder backup confirm");
        seed_
    };

    wait_for(&b, "petra to see the proposed charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }

    wait_for(&a, "the founding to seal on the founder", |s| s.create.run.outcome == 1).await;
    // entering is gated on the phrase-backup step now (2026-08-08) — both ends
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal on petra", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "petra to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b, petra_phrase, walter_phrase)
}

/// Field keystone (found 2026-08-17, recovery attempt 3g of the incident
/// rerun): a seat can still be recovered AFTER the republic sealed a
/// `set_relays` edit. The recovery gate compared the Welcome's relay set
/// against the GENESIS roster's — but the Welcome (rightly) carries the
/// GOVERNED pool, so on any republic that ever voted its pool the gate
/// refused every recovery forever ("names a different relay set than the
/// republic ratified"). The authority must be the served chain's own
/// fold: genesis (or blob) pool plus every applied `set_relays` block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_still_verifies_after_a_sealed_pool_edit() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let relay2 = MockRelay::run().await.expect("second relay");
    let url2 = relay2.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;

    // the republic GOVERNS its pool: both voices seal url + url2. walter
    // never confirms url2 locally (the field case) — his dialable subset
    // stays [url], while the ratified pool is [url, url2].
    a.execute(Command::Propose {
        surface: molt_core::Surface::Organization,
        payload: serde_json::json!({"op": "set_relays", "value": format!("{url} {url2}")}),
    })
    .await
    .expect("propose pool edit");
    approve_value(&b, &format!("{url} {url2}")).await;
    wait_for(&a, "the pool edit to seal", |s| {
        s.workspaces
            .first()
            .is_some_and(|w| w.detail.contains("2-of-2") || !w.name.is_empty())
    })
    .await;
    // the seal is observable as the applied entry reaching the proposals
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals } =
            a.execute(Command::ListProposals).await.expect("list")
        {
            if proposals.iter().any(|p| p.state == molt_core::ProposalState::Applied) {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pool edit never sealed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // petra's device dies AFTER the pool vote
    drop(b);
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint");
    let s = wait_for(&a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
    })
    .await
    .expect("recover start");
    let s = wait_for(&c, "the recovery to open", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "a recovery after a sealed pool edit must verify: {:?}",
        s.notice
    );
}

/// Found a real 2-of-3 "Chess Club" over one in-process relay: walter
/// (founder), petra and vera. Hands back the three live engines plus
/// petra's recovery phrase.
async fn found_three(
    root: &std::path::Path,
    url: &str,
) -> (WalletHandle, WalletHandle, WalletHandle, String) {
    found_three_at(root, url, 2).await
}

/// [`found_three`] with a chosen threshold (m-of-3).
async fn found_three_at(
    root: &std::path::Path,
    url: &str,
    threshold: u8,
) -> (WalletHandle, WalletHandle, WalletHandle, String) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("founding starts over the confirmed relay");

    let s = wait_for(&a, "both seat links to become joinable v2 links", |s| {
        s.create.seats.len() == 2
            && s.create
                .seats
                .iter()
                .all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();

    let b = engine(&root.join("joiner-petra"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: links[0].clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("petra joins");
    let petra_phrase = wait_for(&b, "petra's recovery phrase", |s| !s.join.seed.is_empty())
        .await
        .join
        .seed
        .clone();

    let v = engine(&root.join("joiner-vera"));
    adopt_relay(&v, url).await;
    v.execute(Command::JoinStart {
        invite: links[1].clone(),
        member: "vera".to_string(),
    })
    .await
    .expect("vera joins");

    wait_for(&a, "the founder to accept both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    for w in [&b, &v] {
        wait_for(w, "the proposed charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed_ = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }

    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in [&b, &v] {
        wait_for(w, "the join to seal", |s| {
            s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
        })
        .await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the member to enter", |s| {
            s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
        })
        .await;
    }
    (a, b, v, petra_phrase)
}

/// **Field keystone (live incident 2026-08-09 §2, field rerun 2026-08-17):
/// a bystander that was OFFLINE during a seat's recovery must still hear
/// the rejoiner after catching up.** The trap: the bystander's persisted
/// accept window for the seat still carries the LOST incarnation's seq
/// marks, and on Nostr nothing on the bystander resets it — the
/// coordinator resets its own window in `restore_member`, the mesh reset
/// rides the recovery announce, but a bystander that merely replays the
/// backlog classifies every fresh-incarnation envelope as a duplicate,
/// drops it and ACKS it — the at-least-once guarantee is spent on a
/// message nobody applied (the field's veronica lost exactly one message
/// this way, permanently).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bystander_that_slept_through_a_recovery_still_hears_the_rejoiner() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, v, petra_phrase) = found_three(tmp.path(), &url).await;

    // petra's FIRST incarnation burns a SPAN of its seq space (seqs are log
    // positions, so a batch of own sends occupies a consecutive stretch),
    // and vera's accept window records every one of those marks. The new
    // incarnation's shorter recovered log restarts below that span, so its
    // batch re-uses seqs the old incarnation actually sent — the collision
    // the field hit on one message, made unavoidable here.
    const BATCH: usize = 30;
    for i in 0..BATCH {
        b.execute(Command::Chat {
            body: format!("petra before the loss {i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("petra sends");
    }
    for w in [&a, &v] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let seen = read_chat_bodies(w).await;
            if (0..BATCH).all(|i| seen.iter().any(|m| m.contains(&format!("before the loss {i}")))) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "pre-loss chat never arrived");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // vera goes offline — a clean close persists her window marks
    let vera_ws = read_session(&v).await.workspaces[0].id.clone();
    v.execute(Command::CloseWorkspace).await.expect("vera closes");

    // petra's device dies; walter coordinates the recovery while vera sleeps
    drop(b);
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint");
    let s = wait_for(&a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
    })
    .await
    .expect("recover start");
    let s = wait_for(&c, "the recovery to open", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery: {:?}", s.notice);

    // the new incarnation speaks a batch of its own — its seq span overlaps
    // the lost incarnation's — and the coordinator hears every message
    // (its window reset in restore_member)
    for i in 0..BATCH {
        c.execute(Command::Chat {
            body: format!("petra from the comeback {i}"),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("petra sends again");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let seen = read_chat_bodies(&a).await;
        if (0..BATCH).all(|i| seen.iter().any(|m| m.contains(&format!("from the comeback {i}")))) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the coordinator must hear the rejoiner's whole batch"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // vera wakes and catches up — EVERY rejoiner message must arrive; any
    // seq that collides with the lost incarnation's marks would be
    // swallowed as a duplicate (and falsely acked) without the reset
    v.execute(Command::OpenWorkspace { id: vera_ws }).await.expect("vera reopens");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let seen = read_chat_bodies(&v).await;
        let missing: Vec<usize> = (0..BATCH)
            .filter(|i| !seen.iter().any(|m| m.contains(&format!("from the comeback {i}"))))
            .collect();
        if missing.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sleeping bystander swallowed rejoiner messages as the lost \
             incarnation's duplicates (accept window never reset) — missing {missing:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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
        url: onion.clone(),
        accept_clearnet: false,
    })
    .await
    .expect("onion relay confirm");
    // B4: settle the probe's verdict FIRST — it lands on the same notice
    // channel the mint's refusal below is read from, and an unreachable
    // onion (Tor is off in this fixture) would otherwise clobber it
    wait_for(&a, "the onion relay's probe verdict to settle", |s| {
        s.settings.relays.iter().any(|r| r.url.contains(".onion") && r.confirmed)
    })
    .await;
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

    let health_before = read_session(&a).await.net_health.clone();
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
    // …and minting did not TOUCH the network pill. That is the property, not
    // any particular value: the mint resolves a dialer, and a resolver that
    // wrote `Ok` on success would relabel the whole session's network state as
    // a side effect of an unrelated action.
    assert_eq!(
        s.net_health, health_before,
        "minting a recovery link must not change the network health"
    );

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// **N4b step 6 capstone: a total-loss seat really comes back, over relays.**
///
/// Everything 6a–6e built, driven end to end through the public Command
/// surface and nothing else — no injected transport material, no hand-built
/// chain. A fresh node holding only petra's recovery phrase and the link
/// walter minted rejoins the republic:
///
/// 1. the rejoiner gift-wraps a `RecoverRequest` carrying a NEW transport
///    anchor and a seat proof over it (6e);
/// 2. walter's recovery inbox hands it to the actor, which proposes and — at
///    m=1, the only threshold a republic with one survivor can reach —
///    commits the `Restored` block;
/// 3. that commit fires the coordinator's **Nostr** re-key (6c): the MLS
///    commit rides a 445 at a pinned stamp, the Welcome is gift-wrapped to
///    the anchor the chain just ratified, and the chain ANCHOR is served;
/// 4. the rejoiner assembles the anchor until it verifies standalone and
///    materializes (6d) as a Nostr workspace.
///
/// Threshold **2-of-2** — the case that used to be a structural dead end
/// (the lost seat's own signature would have been needed). Since the
/// recovery approval design (2026-08-08) the rejoiner's CONSENT counts as
/// one distinct signer, so walter's surviving signature plus petra's
/// consent reach the threshold: m = n republics recover.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_seat_rejoins_the_republic_over_relays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    // 2-of-2: walter's signature + petra's consent seal the Restored block
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;

    // …and the republic keeps governing meanwhile, so the recovery's own
    // Restored block does NOT land at height 1. That gap is the point: the
    // coordinator serves the ANCHOR (height 0) while its outbox also carries
    // the new head, so the rejoiner sees a non-consecutive pair and must not
    // try to verify across the hole.
    // At 2-of-2 each rename needs BOTH voices, so they commit while petra's
    // device still lives — through the PUBLIC approve surface.
    for name in ["Chess Club Reloaded", "Chess Club Again"] {
        a.execute(Command::Propose {
            surface: molt_core::Surface::Organization,
            payload: serde_json::json!({"op": "set_name", "value": name}),
        })
        .await
        .expect("propose");
        approve_value(&b, name).await;
        wait_for(&a, "the rename to commit on both voices", |s| {
            s.workspaces.iter().any(|w| w.name == name)
        })
        .await;
    }
    // NOW petra's device is gone
    drop(b);

    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("the mint is acked");
    let s = wait_for(&a, "the recovery link to be minted", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed over relays, got {:?}", s.notice))
        .to_string();

    // a FRESH node: it holds the phrase and the link, and nothing else
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
    })
    .await
    .expect("recover start");

    let s = wait_for(&c, "the recovered republic to open", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "the recovery failed: {:?}",
        s.notice
    );
    // It materializes from the ANCHOR, so it comes back under the FOUNDING
    // name — the two later renames are above the anchor and arrive over the
    // ordinary catch-up (§3.1), which is the next assertion.
    let ws = s
        .workspaces
        .iter()
        .find(|w| w.name == "Chess Club")
        .expect("the recovered republic is listed under its founding name");
    assert_eq!(ws.agenda, "play chess, decide together", "the ratified charter came back");
    assert_eq!(ws.members.len(), 2, "the whole roster came back from the chain");

    // …and everything above the anchor really does arrive that way: no second
    // rail, no bespoke fetch, which is the whole reason the coordinator serves
    // an anchor rather than a chain.
    // The runtime is UP, which is what makes it a live seat rather than a
    // frozen snapshot — and was not true before this capstone existed.
    assert_eq!(
        s.net_health,
        molt_core::NetHealth::Ok,
        "a recovered Nostr seat with no group runtime is deaf: no 445s in, no outbox out"
    );
    // …and the two renames above the anchor arrive over the ordinary
    // catch-up (§3.1a).
    wait_for(&c, "the renames above the anchor to arrive", |s| {
        s.workspaces.iter().any(|w| w.name == "Chess Club Again")
    })
    .await;

    // …and walter sees the return, so the two ends agree the seat is back
    wait_for(&a, "walter to record petra's return", |s| {
        s.workspaces
            .iter()
            .any(|w| w.name == "Chess Club Again" && w.members.len() == 2)
    })
    .await;
}

/// **Live-incident 2026-08-09 §2/§3 repro:** TWO recoveries of the same
/// seat, back to back, must leave BOTH chat directions converging. The
/// field evidence (SecretReuseError storms on consumed ratchet
/// generations, an inbound-deaf rejoiner, churning survivor outboxes)
/// points at MLS divergence out of the SECOND re-key — this drives the
/// whole flow through the public command surface and then insists on
/// bidirectional chat, which is exactly what the live republic lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_recovery_of_the_same_seat_still_converges_both_ways() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);

    // ---- recovery ONE ----
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint 1");
    let s = wait_for(&a, "link 1", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link1 = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("mint 1 must succeed: {:?}", s.notice))
        .to_string();
    let c1 = engine(&tmp.path().join("rejoiner-1"));
    adopt_relay(&c1, &url).await;
    c1.execute(Command::RecoverStart {
        link: link1,
        phrase: petra_phrase.clone(),
    })
    .await
    .expect("recover 1 start");
    let s = wait_for(&c1, "recovery 1 to open", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery 1: {:?}", s.notice);
    wait_for(&a, "walter to record return 1", |s| {
        s.workspaces.iter().any(|w| w.members.len() == 2)
    })
    .await;

    // …and petra's replacement device dies TOO (the live sequence)
    drop(c1);

    // ---- recovery TWO, fresh link, fresh device ----
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint 2");
    let s = wait_for(&a, "link 2", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link2 = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("mint 2 must succeed: {:?}", s.notice))
        .to_string();
    let c2 = engine(&tmp.path().join("rejoiner-2"));
    adopt_relay(&c2, &url).await;
    c2.execute(Command::RecoverStart {
        link: link2,
        phrase: petra_phrase,
    })
    .await
    .expect("recover 2 start");
    let s = wait_for(&c2, "recovery 2 to open", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery 2: {:?}", s.notice);

    // ---- BOTH directions converge (what the live republic lost) ----
    a.execute(Command::Chat {
        body: "from walter after the double recovery".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("walter sends");
    c2.execute(Command::Chat {
        body: "from petra's second comeback".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("petra sends");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let a_sees = read_chat_bodies(&a).await;
        let c_sees = read_chat_bodies(&c2).await;
        let a_ok = a_sees.iter().any(|b| b.contains("second comeback"));
        let c_ok = c_sees.iter().any(|b| b.contains("after the double recovery"));
        if a_ok && c_ok {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chat did not converge after the double recovery — \
             walter sees {a_sees:?}, petra sees {c_sees:?} \
             (the live incident's §2 deafness)"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The chat bodies a node currently reads (public surface).
async fn read_chat_bodies(w: &WalletHandle) -> Vec<String> {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s
            .applied
            .iter()
            .filter_map(|v| v.get("body").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect(),
        other => panic!("unexpected: {other:?}"),
    }
}

/// **N4b step 6f: the wrap-author gate, finally pinnable** (the test step 5
/// owed).
///
/// The coordinator refuses a recovery request whose claimed transport anchor
/// is not the key that sealed the gift wrap. Step 5 could not test it: every
/// forged request it could build failed the SEAT PROOF first, so the test
/// would have gone green with the gate deleted.
///
/// 6e is what makes it testable — the request here is **correctly signed**,
/// with a real seat proof over a real anchor, and differs from an honest one
/// in exactly one respect: somebody else wrapped it. That is the
/// relay-level attacker the gate exists for, re-addressing the Welcome to a
/// key nobody holds so the returning seat is stranded.
///
/// And the assertion is not "nothing happened", which any bug also
/// satisfies: the ticket must still be UNSPENT afterwards, proven by the
/// honest recovery going on to succeed over the very same link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_wrapped_by_another_key_is_refused_and_leaves_the_ticket_unspent() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    // 2-of-2: the honest tail below still succeeds, because the rejoiner's
    // consent is the second voice (recovery approval design, 2026-08-08)
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);

    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("the mint is acked");
    let s = wait_for(&a, "the recovery link to be minted", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed, got {:?}", s.notice))
        .to_string();
    let inv = molt_engine::RecoveryInvite::parse(&link).expect("parseable link");
    let h = inv.handover.clone().expect("a v2 handover");

    // Everything an honest rejoiner would produce…
    let (sk, identity_pk) = molt_engine::member_identity(&petra_phrase).expect("seat identity");
    let entropy = molt_storage::seed_entropy(&petra_phrase).expect("entropy");
    let (_, anchor) = molt_net::nostr_identity(&entropy, &h.ticket);
    let mls = molt_net::MlsMember::new(&sk, "petra").expect("mls identity");
    let kp_hex = hex::encode(mls.key_package().expect("key package"));
    let seat_proof =
        molt_engine::make_seat_proof(&sk, &h.ticket, &kp_hex, &h.republic_id, &anchor, &[]);

    // …sent from a key that is NOT that anchor.
    let (impostor_sk, impostor_pk) = molt_net::nostr_identity(b"a relay-level attacker", "x");
    assert_ne!(impostor_pk, anchor, "the whole point is that these differ");
    let dialer = molt_net::dial::Dialer::resolve("none", "local", 0).expect("direct dialer");
    let impostor = molt_net::ritual_net::RitualNet::new(dialer, vec![url.clone()], &impostor_sk)
        .expect("impostor transport");
    impostor
        .send_ritual(
            &h.npub,
            &molt_net::invite::RitualMsg::Recover(molt_net::invite::RecoverRequest {
                member: "petra".to_string(),
                identity_pk,
                key_package: kp_hex,
                ticket: h.ticket.clone(),
                seat_proof,
                new_nostr_pk: anchor,
                relays: Vec::new(),
                reply: None,
                consent: String::new(),
            }),
        )
        .await
        .expect("the impostor's wrap publishes — being refused is the coordinator's job");

    // the coordinator must not re-admit anyone: its chain stays at the genesis
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match a.execute(Command::ReadChain).await.expect("read chain") {
            Reply::Chain { blocks } => assert_eq!(
                blocks.len(),
                1,
                "a request the claimed anchor did not sign must not re-admit a seat: {blocks:?}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // …and the ticket is still spendable, which the honest return proves
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
    })
    .await
    .expect("recover start");
    let s = wait_for(&c, "the honest recovery to complete", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "the refused impostor spent the ticket — the real seat can no longer return: {:?}",
        s.notice
    );
}

/// **Auto-approval keystone (recovery_auto_approval.md §3): at 3-of-3 a
/// recovery completes with NO human approval anywhere.** Before this, the
/// coordinator's signature + the rejoiner's consent reached 2 and the run
/// hung on a proposal card the third member never saw — the field defect of
/// 2026-08-23. Now the third member verifies the consent itself on receipt
/// of the gossiped proposal and signs automatically; no `Command::Approve`
/// is issued by anyone in this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_needs_no_human_approval_when_survivors_are_online() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    // 3-of-3: every voice is needed — walter + vera + petra's consent
    let (a, b, v, petra_phrase) = found_three_at(tmp.path(), &url, 3).await;

    // petra's device dies; walter mints while vera merely stays online
    drop(b);
    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint");
    let s = wait_for(&a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
    })
    .await
    .expect("recover start");

    // the whole point: nobody calls Command::Approve, and the seat returns
    let s = wait_for(&c, "the recovery to open with no human vote", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery: {:?}", s.notice);

    // the rejoiner SAW the vote: the coordinator's progress frames built the
    // checklist (recovery_auto_approval.md §4), and by Done every voice is
    // counted (the seal finalizes the list even if the last frame lost the
    // race against the Welcome)
    let s = wait_for(&c, "the finished checklist", |s| {
        s.recover.member == "petra"
            && s.recover.need == 3
            && s.recover.seats.len() == 3
            && s.recover.seats.iter().all(|seat| seat.approved)
    })
    .await;
    let names: Vec<&str> = s.recover.seats.iter().map(|x| x.member.as_str()).collect();
    assert_eq!(names, vec!["walter", "petra", "vera"], "roster order");

    // both survivors agree the seat is back
    for w in [&a, &v] {
        wait_for(w, "the survivor to record petra's return", |s| {
            s.workspaces.iter().any(|w| w.members.len() == 3)
        })
        .await;
    }
}

/// **WP6 (recovery_auto_approval.md, field log 2026-08-23): a refused
/// request fails the rejoiner FAST, with the reason.** Before this, only the
/// coordinator saw `recover-refused:` while the rejoiner sat out the full
/// 15-minute timeout — a wrong phrase was indistinguishable from a dead
/// coordinator. The refusal answer goes only to a sender that passed the
/// ticket + PoP gates (an unknown ticket stays a silent drop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_phrase_fails_the_rejoiner_fast_with_the_reason() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, _petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);

    a.execute(Command::RecoverInviteStart {
        member: "petra".to_string(),
    })
    .await
    .expect("mint");
    let s = wait_for(&a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    let wrong = molt_storage::generate_seed_phrase().expect("a valid but foreign phrase");
    c.execute(Command::RecoverStart { link, phrase: wrong }).await.expect("recover start");

    // the refusal must arrive as an honest failure within the ordinary wait —
    // never as a silent quarter-hour timeout
    let s = wait_for(&c, "the refusal to reach the rejoiner", |s| {
        s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        s.notice.contains("identity key"),
        "the coordinator's reason travels verbatim: {:?}",
        s.notice
    );
    // …and the coordinator still surfaced its own refused notice or kept the
    // link armed: the ticket is NOT spent by a failed proof, so the same link
    // with the RIGHT phrase can still recover the seat.
}

/// **The FOUNDER's seat recovers too (field bug 2026-08-23).** The founding
/// ritual salts the founder's identity with a name-derived workspace id
/// (`start_ritual`), while every joiner uses the fixed "member" tag — and
/// the rejoin task only ever derived the member convention, so a founder's
/// correct phrase was refused with "recovery must re-derive the seat's own
/// identity key" (the field's lnInks). The link now carries the seat's
/// anchored identity pk and the rejoiner resolves the matching convention —
/// and rejects a genuinely wrong phrase LOCALLY, before any network round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_founders_own_seat_recovers_over_relays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, _petra_phrase, walter_phrase) = found_republic(tmp.path(), &url, 2).await;

    // the FOUNDER's device dies; the surviving joiner coordinates
    drop(a);
    b.execute(Command::RecoverInviteStart {
        member: "walter".to_string(),
    })
    .await
    .expect("mint");
    let s = wait_for(&b, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart {
        link,
        phrase: walter_phrase,
    })
    .await
    .expect("recover start");
    let s = wait_for(&c, "the founder's seat to recover", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "the founder's correct phrase must recover the seat: {:?}",
        s.notice
    );
    // and the survivor records the return
    wait_for(&b, "petra to record walter's return", |s| {
        s.workspaces.iter().any(|w| w.members.len() == 2)
    })
    .await;
}
