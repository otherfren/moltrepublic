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

use molt_core::{Command, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod common;

use common::{adopt_relay, engine, read_session, wait_for, wait_for_secs};

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
        if let Reply::Proposals { proposals, .. } =
            w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("value").and_then(|v| v.as_str()) == Some(value)
            }) {
                w.execute(Command::Approve { proposal: p.id, note: None })
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
        relays: Vec::new(),
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
    every_seat_announced(&[&a, &b]).await;
    // B3: these tests restore a seat heard a moment ago (production waits
    // until it is silent for 120 s)
    for w in [&a, &b] {
        w.__seat_active_secs(0);
    }
    (a, b, petra_phrase, walter_phrase)
}

/// Each node has every other seat's status: a recovery link minted now
/// carries the v2 consent anchor (`recovery_auto_approval.md` §7e).
async fn every_seat_announced(nodes: &[&WalletHandle]) {
    for w in nodes {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let Reply::Mirror(v) = w.execute(Command::ReadMirror).await.expect("read mirror") else {
                panic!("read mirror");
            };
            if v.members.iter().all(|m| m.known) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the seats never announced: {v:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
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
        if let Reply::Proposals { proposals, .. } =
            a.execute(Command::LIST_ALL_PROPOSALS).await.expect("list")
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
        relays: Vec::new(),
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
    found_three_as(root, url, threshold, false).await
}

/// [`found_three_at`]; `pre_lock` founds a republic as before the sealing
/// lock existed (B6 only).
async fn found_three_as(
    root: &std::path::Path,
    url: &str,
    threshold: u8,
    pre_lock: bool,
) -> (WalletHandle, WalletHandle, WalletHandle, String) {
    let a = engine(&root.join("founder"));
    a.__found_locked(!pre_lock);
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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
    every_seat_announced(&[&a, &b, &v]).await;
    // B3: restored a moment after the loss (see `found_republic`)
    for w in [&a, &b, &v] {
        w.__seat_active_secs(0);
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
    // one survivor restores while the other sleeps: B6 only (under the
    // lock a 2-of-3 restore needs both survivors' promises)
    let (a, b, v, petra_phrase) = found_three_as(tmp.path(), &url, 2, true).await;

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
        relays: Vec::new(),
    })
    .await
    .expect("recover start");
    // vera, heard seconds ago, holds the sealing turn (B6): walter seals as
    // the fallback after one grace and the ask round
    let s = wait_for_secs(&c, "the recovery to open", 150, |s| {
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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
                anchor_first: false,
                version: String::new(),
                consent: String::new(),
                started: 0,
                caps: Vec::new(),
            }),
        )
        .await
        .expect("the impostor's wrap publishes — being refused is the coordinator's job");

    // the coordinator must not re-admit anyone: its chain stays at the genesis
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match a.execute(Command::ReadChain).await.expect("read chain") {
            Reply::Chain { blocks, .. } => assert_eq!(
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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
    c.execute(Command::RecoverStart { link, phrase: wrong, relays: Vec::new() }).await.expect("recover start");

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
        relays: Vec::new(),
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

/// **Field flow 2026-08-24: a local copy of the seat must not block its
/// recovery.** An S3-restored (detached) workspace shares the recovered
/// seat's id — `create_workspace` refused on the existing dir, so the very
/// path the detached notice recommends ("rejoin via a recovery link") died
/// with "already exists". A recovery now RETIRES the local copy to the
/// trash (recoverable 30 days) and materializes the verified state, closing
/// the copy first when it is the open one. Driven here as a SECOND recovery
/// on the same node — the same collision class as restore-then-recover.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_replaces_a_local_copy_of_the_seat() {
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

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    for round in 0..2u8 {
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
        c.execute(Command::RecoverStart {
            link,
            phrase: petra_phrase.clone(),
            relays: Vec::new(),
        })
        .await
        .expect("recover start");
        let s = wait_for(&c, "the recovery to open", |s| {
            (s.screen == molt_core::Screen::Main
                && s.notice.starts_with("recovered:"))
                || s.notice.starts_with("recover-failed:")
        })
        .await;
        assert!(
            !s.notice.starts_with("recover-failed:"),
            "round {round}: the existing local copy must not block the recovery: {:?}",
            s.notice
        );
        assert_eq!(
            s.workspaces.len(),
            1,
            "round {round}: one workspace, never a duplicate id: {:?}",
            s.workspaces.iter().map(|w| w.id.clone()).collect::<Vec<_>>()
        );
    }
}

/// Design I12/W5: the local copy a recovery retires may hold a key part
/// of the purse; its keys records move into the recovered workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_keeps_the_key_parts_of_the_local_copy() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let root = tmp.path().join("rejoiner");
    let c = engine(&root);
    adopt_relay(&c, &url).await;
    let record = b"a key part".to_vec();
    for round in 0..2u8 {
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
        c.execute(Command::RecoverStart {
            link,
            phrase: petra_phrase.clone(),
            relays: Vec::new(),
        })
        .await
        .expect("recover start");
        let s = wait_for(&c, "the recovery to open", |s| {
            (s.screen == molt_core::Screen::Main && s.notice.starts_with("recovered:"))
                || s.notice.starts_with("recover-failed:")
        })
        .await;
        assert!(s.notice.starts_with("recovered:"), "round {round}: {:?}", s.notice);
        let ws_id = s.workspaces[0].id.clone();
        c.execute(Command::CloseWorkspace).await.expect("close");
        let dir = molt_storage::find_workspace_dir(&root, &ws_id).expect("the recovered dir");
        let (opened, _) = molt_storage::open_workspace(&dir).expect("open");
        if round == 0 {
            opened.append_wallet_keys(&record).expect("a key part");
        } else {
            let held = opened.read_wallet_keys().expect("keys");
            assert!(
                held.iter().any(|r| r.as_slice() == record.as_slice()),
                "the retired copy's key part must survive the recovery"
            );
        }
    }
}

/// **B3 (was the detached-reattach keystone): a restored workspace asks
/// nobody for its own restore.** petra's workspace is exported, her device
/// dies, a fresh device restores the blob and opens it: it stays honestly
/// detached and sends no request - the field's self-service restores
/// re-keyed live seats. The operator rejoins with a link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_workspace_stays_detached_and_rejoins_with_a_link() {
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

    // petra's knowledge leaves the device as a manual export…
    let ws_id = read_session(&b).await.workspaces[0].id.clone();
    let blob = tmp.path().join("petra.molt.enc");
    b.execute(Command::ExportWorkspace {
        id: ws_id,
        dest: blob.display().to_string(),
        passphrase: "super-secret-pass".to_string(),
    })
    .await
    .expect("export kickoff");
    wait_for(&b, "the export to finish", |s| {
        !s.export.running && s.export.result == "ok"
    })
    .await;
    // …and the device dies
    drop(b);

    // a fresh device: restore the blob, then OPEN it — that is ALL
    let c = engine(&tmp.path().join("restored"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RestoreStart {
        way: "file".to_string(),
        target: blob.display().to_string(),
        secret: "super-secret-pass".to_string(),
        replace: false,
    })
    .await
    .expect("restore start");
    wait_for(&c, "the restore to stage and verify", |s| s.restore.run.outcome == 1).await;
    c.execute(Command::RestoreFinish).await.expect("restore finish");

    // the seat stays detached and sends nothing: walter proposes nothing
    let s = read_session(&c).await;
    assert_eq!(s.notice, "detached", "no reattach runs by itself");
    tokio::time::sleep(Duration::from_secs(3)).await;
    match a.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, .. } => assert_eq!(blocks.len(), 1, "no restore without a link: {blocks:?}"),
        other => panic!("unexpected: {other:?}"),
    }

    // the operator rejoins with a link
    a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await.expect("mint");
    let s = wait_for(&a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    let s = wait_for(&c, "the rejoin to go live", |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "the link rejoin: {:?}", s.notice);
}

/// A relay that stores events and counts the kind-445 frames it admits.
#[derive(Debug, Clone, Default)]
struct Count445(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl nostr_relay_builder::builder::WritePolicy for Count445 {
    fn admit_event<'a>(
        &'a self,
        event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a std::net::SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult> {
        Box::pin(async move {
            if event.kind == nostr_relay_builder::prelude::Kind::Custom(445) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

async fn counting_relay(count: &Count445) -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = nostr_relay_builder::LocalRelay::new(
        nostr_relay_builder::builder::RelayBuilder::default().database(db).write_policy(count.clone()),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

async fn status_of(w: &WalletHandle) -> molt_core::StatusView {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// **B2 (field R12): a seat restored while its old node still runs.** The
/// re-key removes the old node's leaf; the old node used to merge it as an
/// ordinary re-key and re-offer its tail forever. Now it says
/// `seat-evicted`, reports `evicted` in `status`, and publishes nothing;
/// the bystander sees an ordinary re-key and raises nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_restored_while_its_old_node_runs_is_told_and_goes_quiet() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let count = Count445::default();
    let (_relay, url) = counting_relay(&count).await;
    let tmp = tempfile::tempdir().expect("tmp");
    // the coordinator restores a seat it still hears: an older build's
    // behaviour, or the operator's mistake (the window is 0 here)
    let (a, b, v, petra_phrase) = found_three(tmp.path(), &url).await;
    // a stale or forged eviction report to a seat whose group is live
    v.execute(Command::NetEvicted { by: "walter".to_string(), generation: None }).await.expect("ack");
    assert!(!status_of(&v).await.evicted, "a live group is not evicted by a report");

    a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await.expect("mint");
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
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    let s = wait_for(&c, "the rejoin", |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()) || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "rejoin: {:?}", s.notice);

    // the old node learns it was replaced
    let s = wait_for(&b, "the old node to learn its eviction", |s| s.notice == "seat-evicted").await;
    assert_eq!(s.net_health, molt_core::NetHealth::Down { reason: "seat evicted".to_string() });
    assert!(status_of(&b).await.evicted, "status says evicted");
    // the bystander merged an ordinary re-key
    assert!(!status_of(&v).await.evicted);
    assert_ne!(read_session(&v).await.notice, "seat-evicted");

    // everyone else goes quiet; the old node alone keeps writing
    for w in [&a, &v, &c] {
        w.execute(Command::CloseWorkspace).await.expect("close");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = count.0.load(std::sync::atomic::Ordering::SeqCst);
    let chat = || Command::Chat {
        body: "from the replaced device".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    };
    let refused = b.execute(chat()).await;
    assert!(matches!(refused, Err(molt_core::MoltError::SeatEvicted)), "never sent, so refused: {refused:?}");
    // past the first resend round (10 s)
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(
        count.0.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "the evicted node published nothing"
    );

    // a restart of the evicted node stays quiet and says so again
    let id = read_session(&b).await.active_workspace;
    b.execute(Command::ClearNotice).await.expect("clear");
    b.execute(Command::CloseWorkspace).await.expect("close");
    b.execute(Command::OpenWorkspace { id }).await.expect("reopen");
    wait_for(&b, "the reopened node to name its eviction", |s| s.notice == "seat-evicted").await;
    assert!(status_of(&b).await.evicted, "still evicted after the restart");
    assert!(matches!(b.execute(chat()).await, Err(molt_core::MoltError::SeatEvicted)));
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(
        count.0.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "the reopened evicted node published nothing"
    );
}

/// **B3: a link-less request - what an older build's self-heal sends - is
/// refused with `rejoin needs a link` and answered**, never proposed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_less_request_from_an_older_build_is_refused_and_answered() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    // walter's working anchor, as the older node's chain names it
    a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await.expect("mint");
    let s = wait_for(&a, "a link naming walter's anchor", |s| s.notice.starts_with("recovery-link:")).await;
    let link = s.notice.strip_prefix("recovery-link:").expect("link").to_string();
    let h = molt_engine::RecoveryInvite::parse(&link).expect("link").handover.expect("v2 handover");

    // the older build's self-service request: a self-minted ticket
    let self_ticket = "self-minted-ticket";
    let (sk, identity_pk) = molt_engine::member_identity(&petra_phrase).expect("seat identity");
    let entropy = molt_storage::seed_entropy(&petra_phrase).expect("entropy");
    let (anchor_sk, anchor) = molt_net::nostr_identity(&entropy, self_ticket);
    let mls = molt_net::MlsMember::new(&sk, "petra").expect("mls identity");
    let kp_hex = hex::encode(mls.key_package().expect("key package"));
    let seat_proof = molt_engine::make_seat_proof(&sk, self_ticket, &kp_hex, &h.republic_id, &anchor, &[]);
    let dialer = molt_net::dial::Dialer::resolve("none", "local", 0).expect("direct dialer");
    let old = molt_net::ritual_net::RitualNet::new(dialer, vec![url.clone()], &anchor_sk).expect("transport");
    let mut inbox = old.inbox().await.expect("inbox");
    old.send_ritual(
        &h.npub,
        &molt_net::invite::RitualMsg::Recover(molt_net::invite::RecoverRequest {
            member: "petra".to_string(),
            identity_pk,
            key_package: kp_hex,
            ticket: self_ticket.to_string(),
            seat_proof,
            new_nostr_pk: anchor,
            relays: Vec::new(),
            reply: None,
            anchor_first: false,
            version: String::new(),
            consent: String::new(),
            started: 0,
            caps: Vec::new(),
        }),
    )
    .await
    .expect("publish");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let reason = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "the older node never got an answer");
        if let Some(molt_net::ritual_net::RitualDelivery::Msg(
            molt_net::invite::RitualMsg::RecoverRefused { reason, .. },
            _,
        )) = inbox.recv(left).await
        {
            break reason;
        }
    };
    assert_eq!(reason, "rejoin needs a link");
    match a.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, .. } => assert_eq!(blocks.len(), 1, "never proposed: {blocks:?}"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// **B3: no link for a seat heard seconds ago**, at the production window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_for_a_seat_heard_seconds_ago_is_refused() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, _, _) = found_republic(tmp.path(), &url, 2).await;
    a.__seat_active_secs(u64::MAX);
    b.execute(Command::Chat { body: "still here".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("petra speaks");
    wait_for(&a, "walter to hear petra", |s| {
        s.workspaces.iter().any(|w| w.members.iter().any(|m| m.name == "petra" && m.last_seen > 0))
    })
    .await;
    // D3: the command itself answers the refusal; the notice stays
    let refused = a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await;
    assert!(
        matches!(&refused, Err(molt_core::MoltError::Recover(why)) if why == "seat still active"),
        "{refused:?}"
    );
    assert_eq!(read_session(&a).await.notice, "recovery-link-failed:seat still active");
}

/// **Field failure 2026-09-12:** the rejoiner gave a relay a flat 10 s to
/// replay; over Tor the group channel took longer, and the run died after
/// the Welcome with the seat already re-keyed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoin_over_a_slowly_replaying_relay_still_comes_back() {
    let slow = common::SlowReplay::new(&[445, 1059]);
    let (_relay, url) = common::slow_relay(&slow).await;
    let tmp = tempfile::tempdir().expect("tmp");
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

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    slow.arm();
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
        relays: Vec::new(),
    })
    .await
    .expect("recover start");
    let s = wait_for_secs(&c, "the recovered republic to open", 120, |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "the recovery failed: {:?}",
        s.notice
    );
}

/// Findings R11 (live repro E3): the rejoiner waited for the end of the
/// group replay without reading it, so a relay holding more frames in its
/// read window than the subscription buffers never finished replaying, and
/// the run sat silent until the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoin_drains_a_backlog_larger_than_the_subscription_buffer() {
    let backlog = common::Backlog::new(12_000);
    let (_relay, url) = common::backlog_relay(&backlog).await;
    let tmp = tempfile::tempdir().expect("tmp");
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

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    backlog.arm();
    c.execute(Command::RecoverStart {
        link,
        phrase: petra_phrase,
        relays: Vec::new(),
    })
    .await
    .expect("recover start");
    let s = wait_for_secs(&c, "the recovered republic to open", 180, |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(
        !s.notice.starts_with("recover-failed:"),
        "the recovery failed: {:?}",
        s.notice
    );
}

/// The join twin of R11: founder and joiner both gated their group
/// subscription on the end of a replay nobody read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founding_completes_over_a_backlog_larger_than_the_subscription_buffer() {
    let backlog = common::Backlog::new(12_000);
    let (_relay, url) = common::backlog_relay(&backlog).await;
    let tmp = tempfile::tempdir().expect("tmp");
    backlog.arm();
    let (a, b, _, _) = found_republic(tmp.path(), &url, 2).await;
    for w in [&a, &b] {
        assert_eq!(read_session(w).await.screen, molt_core::Screen::Main, "the seat entered");
    }
}

// ---- B4: anchor first, then seal; no fixed deadlines ----------------------

async fn chain_len(w: &WalletHandle) -> usize {
    match w.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, .. } => blocks.len(),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn open_proposals(w: &WalletHandle) -> usize {
    match w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") {
        Reply::Proposals { proposals, .. } => {
            proposals.iter().filter(|p| p.state == molt_core::ProposalState::Proposed).count()
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Mint a link for petra on `a` and return it.
async fn mint_for_petra(a: &WalletHandle) -> String {
    a.execute(Command::RecoverInviteStart { member: "petra".to_string() }).await.expect("mint");
    let s = wait_for(a, "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    s.notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string()
}

/// The test plays the rejoiner over raw relays: its inbox, its request.
struct ManualRejoiner {
    net: molt_net::ritual_net::RitualNet,
    inbox: molt_net::ritual_net::RitualInbox,
    coordinator: String,
    ticket: String,
    request: molt_net::invite::RitualMsg,
    /// This run's id; frames naming another run are skipped.
    attempt: String,
    mls: molt_net::MlsMember,
}

/// How a [`ManualRejoiner`] run differs from the plain one.
#[derive(Default)]
struct RunShape {
    /// The relays the request declares (default: the test relay).
    declared: Option<Vec<String>>,
    /// The run's start stamp.
    started: u64,
}

impl ManualRejoiner {
    async fn start(url: &str, link: &str, phrase: &str, anchor_first: bool) -> Self {
        Self::start_as(url, link, phrase, anchor_first, RunShape::default()).await
    }

    async fn start_as(url: &str, link: &str, phrase: &str, anchor_first: bool, shape: RunShape) -> Self {
        let h = molt_engine::RecoveryInvite::parse(link).expect("link").handover.expect("v2 handover");
        let (sk, identity_pk) = molt_engine::member_identity(phrase).expect("seat identity");
        let entropy = molt_storage::seed_entropy(phrase).expect("entropy");
        let (anchor_sk, anchor) = molt_net::nostr_identity(&entropy, &h.ticket);
        let mls = molt_net::MlsMember::new(&sk, "petra").expect("mls identity");
        let kp_hex = hex::encode(mls.key_package().expect("key package"));
        let relays = shape.declared.unwrap_or_else(|| vec![url.to_string()]);
        let seat_proof = molt_engine::make_seat_proof(&sk, &h.ticket, &kp_hex, &h.republic_id, &anchor, &relays);
        let consent = molt_engine::make_restore_consent(
            &sk,
            &h.republic_id,
            "petra",
            &identity_pk,
            &anchor,
            &relays,
            h.consent_anchor.as_ref(),
        );
        let dialer = molt_net::dial::Dialer::resolve("none", "local", 0).expect("direct dialer");
        let net = molt_net::ritual_net::RitualNet::new(dialer, vec![url.to_string()], &anchor_sk).expect("transport");
        let mut inbox = net.inbox().await.expect("inbox");
        assert!(inbox.first_read(Duration::from_secs(10)).await.any(), "the inbox is readable");
        let attempt = molt_net::invite::recover_attempt_id(&kp_hex);
        let request = molt_net::invite::RitualMsg::Recover(molt_net::invite::RecoverRequest {
            member: "petra".to_string(),
            identity_pk,
            key_package: kp_hex,
            ticket: h.ticket.clone(),
            seat_proof,
            new_nostr_pk: anchor,
            relays,
            consent,
            reply: None,
            anchor_first,
            version: if anchor_first { "0.0.21".to_string() } else { String::new() },
            started: shape.started,
            caps: Vec::new(),
        });
        net.send_ritual(&h.npub, &request).await.expect("the request publishes");
        ManualRejoiner { net, inbox, coordinator: h.npub, ticket: h.ticket, request, attempt, mls }
    }

    async fn repeat_request(&self) {
        self.send(self.request.clone()).await;
    }

    async fn send(&self, msg: molt_net::invite::RitualMsg) {
        self.net.send_ritual(&self.coordinator, &msg).await.expect("publish");
    }

    /// This run's Ready.
    fn ready(&self) -> molt_net::invite::RitualMsg {
        molt_net::invite::RitualMsg::RecoverReady {
            member: "petra".to_string(),
            ticket: self.ticket.clone(),
            attempt: self.attempt.clone(),
        }
    }

    /// This run's withdrawal, as a cancelled rejoiner sends it.
    fn withdrawal(&self) -> molt_net::invite::RitualMsg {
        molt_net::invite::RitualMsg::RecoverFailed {
            member: "petra".to_string(),
            reason: "cancelled".to_string(),
            version: "0.0.21".to_string(),
            attempt: self.attempt.clone(),
        }
    }

    /// The next anchor piece / Welcome / refusal from the coordinator for
    /// this run (a stored frame of another run on the link is skipped).
    async fn next(&mut self, what: &str) -> molt_net::ritual_net::RitualDelivery {
        self.next_within(what, Duration::from_secs(30)).await.unwrap_or_else(|| panic!("nothing arrived: {what}"))
    }

    async fn next_within(&mut self, what: &str, within: Duration) -> Option<molt_net::ritual_net::RitualDelivery> {
        use molt_net::invite::RitualMsg as M;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.inbox.recv(left).await {
                Some(d @ molt_net::ritual_net::RitualDelivery::Welcome(..)) => return Some(d),
                Some(molt_net::ritual_net::RitualDelivery::Msg(M::RecoverProgress { .. }, _)) => {}
                Some(molt_net::ritual_net::RitualDelivery::Msg(
                    M::RecoverAnchor { attempt, .. } | M::RecoverRefused { attempt, .. },
                    _,
                )) if !attempt.is_empty() && attempt != self.attempt => {
                    tracing::debug!(what, "skipping another run's frame");
                }
                Some(d @ molt_net::ritual_net::RitualDelivery::Msg(..)) => return Some(d),
                None => {}
            }
        }
    }

    /// Every anchor piece, by index.
    async fn anchor(&mut self) -> std::collections::BTreeMap<u32, String> {
        let mut got = std::collections::BTreeMap::new();
        loop {
            match self.next("the anchor").await {
                molt_net::ritual_net::RitualDelivery::Msg(
                    molt_net::invite::RitualMsg::RecoverAnchor { index, total, event, .. },
                    _,
                ) => {
                    got.insert(index, event);
                    if got.len() == usize::try_from(total).expect("total") {
                        return got;
                    }
                }
                other => panic!("expected an anchor piece: {other:?}"),
            }
        }
    }

    /// The Welcome for this run, joined with this run's KeyPackage.
    async fn join_welcome(&mut self) -> Result<(), String> {
        loop {
            match self.next("the Welcome").await {
                molt_net::ritual_net::RitualDelivery::Welcome(w, _) => {
                    return self.mls.join_from_welcome(&w.welcome).map_err(|e| e.to_string());
                }
                molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverAnchor { .. }, _) => {}
                other => panic!("expected the Welcome: {other:?}"),
            }
        }
    }
}

/// **B4 keystone (findings R13):** the coordinator sends the anchor first
/// and changes nothing - head, MLS epoch, proposals - until the rejoiner's
/// Ready; a lost piece is re-sent on `RecoverWant`; only then one
/// `Restored` block seals, the group re-keys and the Welcome goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_coordinator_holds_still_until_the_rejoiner_is_ready() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let epoch = status_of(&a).await.mls_epoch;
    assert!(epoch > 0, "the coordinator runs a group");

    let mut r = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    let anchor = r.anchor().await;
    assert!(!anchor.is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(chain_len(&a).await, 1, "nothing sealed before Ready");
    assert_eq!(status_of(&a).await.mls_epoch, epoch, "no re-key before Ready");
    assert_eq!(open_proposals(&a).await, 0, "nothing proposed before Ready");

    // a piece lost on the way is asked for by index (a B4 build names no run)
    r.send(molt_net::invite::RitualMsg::RecoverWant {
        member: "petra".to_string(),
        ticket: r.ticket.clone(),
        missing: vec![0],
        attempt: String::new(),
    })
    .await;
    match r.next("the re-sent piece").await {
        molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverAnchor { index, event, .. }, _) => {
            assert_eq!(index, 0);
            assert_eq!(Some(&event), anchor.get(&0), "the same piece again");
        }
        other => panic!("expected the re-sent piece: {other:?}"),
    }
    assert_eq!(chain_len(&a).await, 1, "a Want seals nothing");

    r.send(r.ready()).await;
    match r.next("the Welcome").await {
        molt_net::ritual_net::RitualDelivery::Welcome(..) => {}
        other => panic!("expected the Welcome: {other:?}"),
    }
    assert_eq!(chain_len(&a).await, 2, "one Restored block");
    assert_eq!(status_of(&a).await.mls_epoch, epoch + 1, "one re-key");
    // F1: the spent link no longer sits in the coordinator's notice
    assert!(!read_session(&a).await.notice.contains("molt://recover/"), "the spent link is gone");
}

/// **A repeated request is the rejoiner asking again** (it got no piece):
/// the whole anchor goes out again, nothing is proposed, the ticket stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_request_gets_the_anchor_again() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let mut r = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    let first = r.anchor().await;
    // past the coordinator's debounce
    tokio::time::sleep(Duration::from_secs(11)).await;
    r.repeat_request().await;
    let again = r.anchor().await;
    assert_eq!(again, first, "the same anchor again");
    assert_eq!(chain_len(&a).await, 1, "nothing sealed");
    assert_eq!(open_proposals(&a).await, 0, "nothing proposed");
    // the ticket is not spent: Ready still admits
    r.send(r.ready()).await;
    loop {
        match r.next("the Welcome").await {
            molt_net::ritual_net::RitualDelivery::Welcome(..) => break,
            molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverAnchor { .. }, _) => {}
            other => panic!("expected the Welcome: {other:?}"),
        }
    }
    assert_eq!(chain_len(&a).await, 2);
}

/// **B4: an older rejoiner (no `anchor_first`) gets the seal-first order
/// unchanged** from a new coordinator: no anchor gift wrap, a sealed
/// block and the Welcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_rejoiner_still_gets_the_seal_first_order() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let mut r = ManualRejoiner::start(&url, &link, &petra_phrase, false).await;
    match r.next("the Welcome").await {
        molt_net::ritual_net::RitualDelivery::Welcome(..) => {}
        other => panic!("seal-first sends the Welcome, no anchor piece: {other:?}"),
    }
    assert_eq!(chain_len(&a).await, 2, "sealed before the Welcome, as before");
}

/// **B4: a new rejoiner against an older (seal-first) coordinator**
/// completes through the old order: the Welcome comes first, the anchor
/// over the group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_rejoiner_completes_against_a_seal_first_coordinator() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    a.__seal_first(true);
    let link = mint_for_petra(&a).await;
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() }).await.expect("recover start");
    let s = wait_for(&c, "the rejoin", |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "rejoin: {:?}", s.notice);
    assert!(
        s.recover.log.iter().any(|l| l == "welcomed back - fetching the chain anchor"),
        "the seal-first path: {:?}",
        s.recover.log
    );
}

/// **B4: a coordinator that restarted between the anchor and the Ready**
/// lost the attempt; the rejoiner hears `link outdated` instead of waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_coordinator_answers_link_outdated() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let mut r = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    r.anchor().await;
    let id = read_session(&a).await.workspaces[0].id.clone();
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id }).await.expect("reopen");
    r.send(r.ready()).await;
    loop {
        match r.next("the refusal").await {
            molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverRefused { reason, .. }, _) => {
                assert_eq!(reason, "link outdated");
                break;
            }
            // a piece re-sent before the restart may still trickle in
            molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverAnchor { .. }, _) => {}
            other => panic!("expected the refusal: {other:?}"),
        }
    }
    assert_eq!(chain_len(&a).await, 1, "nothing sealed");
}

/// A relay that refuses gift wraps to one key, as armed.
#[derive(Debug, Clone, Default)]
struct WrapGate {
    to: std::sync::Arc<std::sync::Mutex<String>>,
    /// Wraps to let through before refusing.
    pass: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Wraps to refuse after that (usize::MAX = until disarmed).
    refuse: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    refused: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl nostr_relay_builder::builder::WritePolicy for WrapGate {
    fn admit_event<'a>(
        &'a self,
        event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a std::net::SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult> {
        use std::sync::atomic::Ordering::SeqCst;
        Box::pin(async move {
            let to = self.to.lock().expect("lock").clone();
            let ours = event.kind == nostr_relay_builder::prelude::Kind::GiftWrap
                && !to.is_empty()
                && event.tags.public_keys().any(|p| p.to_hex() == to);
            if ours {
                if self.pass.load(SeqCst) > 0 {
                    self.pass.fetch_sub(1, SeqCst);
                } else if self.refuse.load(SeqCst) > 0 {
                    self.refuse.fetch_sub(1, SeqCst);
                    self.refused.fetch_add(1, SeqCst);
                    return nostr_relay_builder::builder::PolicyResult::Reject("blocked: test".to_string());
                }
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

async fn gated_relay(gate: &WrapGate) -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = nostr_relay_builder::LocalRelay::new(
        nostr_relay_builder::builder::RelayBuilder::default().database(db).write_policy(gate.clone()),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// The rejoiner's new anchor for `link`, as its phrase derives it.
fn rejoiner_anchor(link: &str, phrase: &str) -> String {
    let h = molt_engine::RecoveryInvite::parse(link).expect("link").handover.expect("v2 handover");
    let entropy = molt_storage::seed_entropy(phrase).expect("entropy");
    molt_net::nostr_identity(&entropy, &h.ticket).1
}

/// **B4: a cancel after the anchor arrived leaves the coordinator as it
/// was** - head, epoch, no open card. The rejoiner's Ready never gets
/// through (the relay refuses it), so the cancel lands in that window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_after_the_anchor_changes_nothing_on_the_coordinator() {
    use std::sync::atomic::Ordering::SeqCst;
    let gate = WrapGate::default();
    let (_relay, url) = gated_relay(&gate).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let epoch = status_of(&a).await.mls_epoch;
    // the request passes, everything after it to the coordinator is refused
    let h = molt_engine::RecoveryInvite::parse(&link).expect("link").handover.expect("v2 handover");
    *gate.to.lock().expect("lock") = h.npub.clone();
    gate.pass.store(1, SeqCst);
    gate.refuse.store(usize::MAX, SeqCst);

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() }).await.expect("recover start");
    wait_for(&c, "the anchor to verify on the rejoiner", |s| {
        s.recover.log.iter().any(|l| l.starts_with("chain verified up to block"))
    })
    .await;
    wait_for(&c, "the Ready to be refused at least once", |_| gate.refused.load(SeqCst) > 0).await;
    c.execute(Command::RecoverCancel).await.expect("cancel");
    gate.refuse.store(0, SeqCst);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain_len(&a).await, 1, "no Restored block");
    assert_eq!(status_of(&a).await.mls_epoch, epoch, "no re-key");
    assert_eq!(open_proposals(&a).await, 0, "no open Restored card");
}

/// **B4: a refused attempt (wrong phrase on a link without the seat's
/// anchored key) leaves the coordinator as it was.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_attempt_changes_nothing_on_the_coordinator() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, _, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let epoch = status_of(&a).await.mls_epoch;
    let wrong = molt_storage::generate_seed_phrase().expect("a valid but foreign phrase");
    let mut r = ManualRejoiner::start(&url, &link, &wrong, true).await;
    match r.next("the refusal").await {
        molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverRefused { reason, .. }, _) => {
            assert!(reason.contains("identity key"), "{reason}");
        }
        other => panic!("expected the refusal, no anchor: {other:?}"),
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(chain_len(&a).await, 1);
    assert_eq!(status_of(&a).await.mls_epoch, epoch);
    assert_eq!(open_proposals(&a).await, 0);
}

/// **B4: the Welcome survives a relay that refuses five publishes** - it
/// is published until a relay takes it, no attempt cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_welcome_survives_five_refused_publishes() {
    use std::sync::atomic::Ordering::SeqCst;
    let gate = WrapGate::default();
    let (_relay, url) = gated_relay(&gate).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    // the one anchor piece passes; the next five wraps to the rejoiner are
    // refused: at most the progress report and otherwise the Welcome
    *gate.to.lock().expect("lock") = rejoiner_anchor(&link, &petra_phrase);
    gate.pass.store(1, SeqCst);
    gate.refuse.store(5, SeqCst);

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() }).await.expect("recover start");
    let s = wait_for_secs(&c, "the rejoin", 180, |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "rejoin: {:?}", s.notice);
    assert_eq!(gate.refused.load(SeqCst), 5, "five publishes were refused");
}

// ---- review R1-R4: every frame of a run names the run --------------------

/// Poll `probe` until it returns `want`, at most `secs`.
async fn eventually<F, Fut>(what: &str, secs: u64, want: usize, probe: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let got = probe().await;
        if got == want {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {got}, want {want}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// **Review R1:** every run on one link derives the same transport key and
/// its inbox replays stored wraps. A refusal of an earlier run must not
/// end the retry - B3 made "refused, fix it, retry the same link" the
/// normal path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_on_the_same_link_is_not_ended_by_an_earlier_runs_refusal() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    // run 1 declares a relay nobody else uses: refused, the ticket stays
    let shape = RunShape { declared: Some(vec!["wss://nowhere.invalid".to_string()]), started: 1 };
    let mut first = ManualRejoiner::start_as(&url, &link, &petra_phrase, true, shape).await;
    match first.next("the refusal").await {
        molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverRefused { reason, .. }, _) => {
            assert!(reason.contains("nowhere.invalid"), "{reason}");
        }
        other => panic!("expected the refusal: {other:?}"),
    }
    drop(first);

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() }).await.expect("recover start");
    let s = wait_for_secs(&c, "the retry", 120, |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "the retry: {:?}", s.notice);
}

/// **Review R3:** a replay of an earlier run's request - one the
/// coordinator saw, or one it never saw that started earlier - never
/// replaces the running attempt, so the Welcome opens with the running
/// run's KeyPackage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_earlier_run_never_replaces_the_running_one() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let now = unix_now();
    let mut oldest = ManualRejoiner::start_as(&url, &link, &petra_phrase, true, RunShape { started: now - 200, ..RunShape::default() }).await;
    oldest.anchor().await;
    let mut running = ManualRejoiner::start_as(&url, &link, &petra_phrase, true, RunShape { started: now, ..RunShape::default() }).await;
    running.anchor().await;
    // the run it replaced, replayed; and a run never seen that started earlier
    oldest.repeat_request().await;
    let _unseen = ManualRejoiner::start_as(&url, &link, &petra_phrase, true, RunShape { started: now - 100, ..RunShape::default() }).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    running.send(running.ready()).await;
    running.join_welcome().await.expect("the Welcome is for the running run");
    assert_eq!(chain_len(&a).await, 2);
}

/// **Review R2:** a cancel after Ready, with the card still unsealed (vera
/// heard petra's old device and holds her approval): the rejoiner
/// withdraws its run, the coordinator withdraws the card through the
/// ordinary governance path - vera sees it withdrawn too - and the link
/// is live again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_after_ready_withdraws_the_unsealed_card() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    // a B6 race: vera seals with walter's pre-collected signature; under
    // the lock nothing seals without walter accepting in a ballot
    let (a, b, v, petra_phrase) = found_three_as(tmp.path(), &url, 3, true).await;
    v.__seat_active_secs(u64::MAX);
    b.execute(Command::Chat { body: "still here".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("petra speaks");
    wait_for(&v, "vera to hear petra", |s| {
        s.workspaces.iter().any(|w| w.members.iter().any(|m| m.name == "petra" && m.last_seen > 0))
    })
    .await;
    drop(b);
    let link = mint_for_petra(&a).await;

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link: link.clone(), phrase: petra_phrase.clone(), relays: Vec::new() })
        .await
        .expect("recover start");
    eventually("the card on the coordinator", 60, 1, || open_proposals(&a)).await;
    eventually("the card at vera", 60, 1, || open_proposals(&v)).await;
    c.execute(Command::RecoverCancel).await.expect("cancel");
    // after Ready the rejoiner cannot know whether the card sealed
    assert_eq!(
        read_session(&c).await.notice,
        format!("recover-failed:{}", molt_engine::RECOVER_CANCELLED_MAYBE_READMITTED)
    );
    eventually("the card withdrawn on the coordinator", 60, 0, || open_proposals(&a)).await;
    eventually("the card withdrawn at vera", 60, 0, || open_proposals(&v)).await;
    assert_eq!(chain_len(&a).await, 1, "nothing sealed");
    // D2: the live link is back on the coordinator's notice
    assert_eq!(read_session(&a).await.notice, format!("recovery-link:{link}"));

    // D1: vera's gate opens - her held approval stays unsigned, the
    // withdrawn card never seals (one presence tick and then some)
    v.__seat_active_secs(0);
    tokio::time::sleep(Duration::from_secs(35)).await;
    assert_eq!(chain_len(&v).await, 1, "vera never sealed the withdrawn card");
    assert_eq!(chain_len(&a).await, 1, "nothing sealed");

    // the ticket is live again: a new run on the same link gets the anchor
    let mut again = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    again.anchor().await;
}

/// **F1 (v0.1.1, acceptance E1):** a cancel right after Ready while vera
/// auto-approves at once (no gate): walter's own seal and vera's both wait
/// out the settle, the withdrawal wins on every seat, nothing seals and
/// the ticket is live again. The settles are held open, so this proves
/// the ordering, not a timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_right_after_ready_beats_an_auto_approving_bystander() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, v, petra_phrase) = found_three(tmp.path(), &url).await;
    drop(b);
    a.__restore_settle_secs(Some(600));
    v.__restore_settle_secs(Some(600));
    let link = mint_for_petra(&a).await;
    // past the seal pacing round after the genesis, as in the field: a
    // seal is held by nothing but the settle
    tokio::time::sleep(Duration::from_secs(6)).await;

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link: link.clone(), phrase: petra_phrase.clone(), relays: Vec::new() })
        .await
        .expect("recover start");
    eventually("the card open on the coordinator", 60, 1, || open_proposals(&a)).await;
    eventually("the card open on vera", 60, 1, || open_proposals(&v)).await;
    c.execute(Command::RecoverCancel).await.expect("cancel");
    eventually("the card withdrawn on the coordinator", 60, 0, || open_proposals(&a)).await;
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(open_proposals(&v).await, 0, "vera holds no open card");
    assert_eq!(chain_len(&v).await, 1, "vera sealed nothing");
    assert_eq!(chain_len(&a).await, 1, "nothing sealed");

    let mut again = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    again.anchor().await;
}

/// The red twin of the test above: with no settle the auto-approving
/// bystander signs on sight and the restore seals before the cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_settle_the_bystander_seals_before_the_cancel() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    // a B6 race: the bystander seals on sight with the coordinator's
    // collected signature; under the lock nothing seals without both
    // survivors accepting in one ballot (the settle twin covers the lock)
    let (a, b, v, petra_phrase) = found_three_as(tmp.path(), &url, 2, true).await;
    drop(b);
    a.__restore_settle_secs(Some(0));
    v.__restore_settle_secs(Some(0));
    let link = mint_for_petra(&a).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    eventually("the card (or its block) on the coordinator", 60, 1, || async {
        open_proposals(&a).await + chain_len(&a).await - 1
    })
    .await;
    c.execute(Command::RecoverCancel).await.expect("cancel");
    eventually("the restore sealed anyway", 60, 2, || chain_len(&v)).await;
}

/// **F2 (v0.1.1, acceptance E2):** the run is cancelled after its block
/// sealed and the group re-keyed, before the rejoiner joined (every wrap
/// to it after the anchor is refused). The coordinator reports it; the
/// rejoiner does not claim a clean cancel; a new link restores the seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_after_the_rekey_is_reported_and_a_new_link_heals_it() {
    use std::sync::atomic::Ordering::SeqCst;
    let gate = WrapGate::default();
    let (_relay, url) = gated_relay(&gate).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let epoch = status_of(&a).await.mls_epoch;
    *gate.to.lock().expect("lock") = rejoiner_anchor(&link, &petra_phrase);
    gate.pass.store(1, SeqCst);
    gate.refuse.store(usize::MAX, SeqCst);

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase.clone(), relays: Vec::new() })
        .await
        .expect("recover start");
    eventually("the seal", 90, 2, || chain_len(&a)).await;
    eventually("the re-key", 60, 1, || async {
        usize::from(status_of(&a).await.mls_epoch > epoch)
    })
    .await;
    c.execute(Command::RecoverCancel).await.expect("cancel");
    assert_eq!(
        read_session(&c).await.notice,
        format!("recover-failed:{}", molt_engine::RECOVER_CANCELLED_MAYBE_READMITTED),
        "not a clean cancel"
    );
    let s = wait_for_secs(&a, "the coordinator's report", 60, |s| s.notice == "restore-abandoned:petra").await;
    assert_eq!(s.notice, "restore-abandoned:petra");

    gate.refuse.store(0, SeqCst);
    let fresh = mint_for_petra(&a).await;
    let c2 = engine(&tmp.path().join("rejoiner-2"));
    adopt_relay(&c2, &url).await;
    c2.execute(Command::RecoverStart { link: fresh, phrase: petra_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    let s = wait_for_secs(&c2, "the second rejoin", 180, |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "the second rejoin: {:?}", s.notice);
    assert_eq!(chain_len(&a).await, 3, "a second Restored block");
}

/// A relay that refuses group messages (kind 445) while held.
#[derive(Debug, Clone, Default)]
struct GroupGate {
    hold: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl nostr_relay_builder::builder::WritePolicy for GroupGate {
    fn admit_event<'a>(
        &'a self,
        event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a std::net::SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult> {
        Box::pin(async move {
            if event.kind.as_u16() == 445 && self.hold.load(std::sync::atomic::Ordering::SeqCst) {
                return nostr_relay_builder::builder::PolicyResult::Reject("blocked: test".to_string());
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

/// **D1 (v0.1.0 acceptance):** a third seat seals the cancelled run's card
/// before the withdrawal reaches it. The block stands; the coordinator
/// does not re-key for the dead run, says so, and a new link heals the
/// seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restore_sealed_after_its_withdrawal_is_reported_and_a_new_link_heals_it() {
    use std::sync::atomic::Ordering::SeqCst;
    let gate = GroupGate::default();
    let (_relay, url) = {
        use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
        let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
        let relay = nostr_relay_builder::LocalRelay::new(
            nostr_relay_builder::builder::RelayBuilder::default().database(db).write_policy(gate.clone()),
        );
        relay.run().await.expect("relay runs");
        let url = relay.url().await.to_string();
        (relay, url)
    };
    let tmp = tempfile::tempdir().expect("tmp");
    // a B6 race: vera seals with walter's pre-collected signature; under
    // the lock nothing seals without walter accepting in a ballot
    let (a, b, v, petra_phrase) = found_three_as(tmp.path(), &url, 3, true).await;
    v.__seat_active_secs(u64::MAX);
    b.execute(Command::Chat { body: "still here".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("petra speaks");
    wait_for(&v, "vera to hear petra", |s| {
        s.workspaces.iter().any(|w| w.members.iter().any(|m| m.name == "petra" && m.last_seen > 0))
    })
    .await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let epoch = status_of(&a).await.mls_epoch;

    let c = engine(&tmp.path().join("rejoiner"));
    adopt_relay(&c, &url).await;
    c.execute(Command::RecoverStart { link, phrase: petra_phrase.clone(), relays: Vec::new() })
        .await
        .expect("recover start");
    eventually("the card at vera with walter's signature", 60, 1, || async {
        match v.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") {
            Reply::Proposals { proposals, .. } => proposals
                .iter()
                .filter(|p| p.state == molt_core::ProposalState::Proposed && p.approvals == 1)
                .count(),
            other => panic!("unexpected: {other:?}"),
        }
    })
    .await;

    // the group goes quiet on the relay: walter's withdrawal cannot reach vera
    gate.hold.store(true, SeqCst);
    c.execute(Command::RecoverCancel).await.expect("cancel");
    eventually("the card withdrawn on the coordinator", 60, 0, || open_proposals(&a)).await;
    // vera's gate opens: she signs and seals alone (walter + vera + consent)
    v.__seat_active_secs(0);
    eventually("vera seals", 90, 2, || chain_len(&v)).await;
    gate.hold.store(false, SeqCst);

    let s = wait_for_secs(&a, "the coordinator's report", 180, |s| {
        s.notice == "restore-sealed-after-withdrawal:petra"
    })
    .await;
    assert_eq!(s.notice, "restore-sealed-after-withdrawal:petra");
    assert_eq!(chain_len(&a).await, 2, "the sealed block stands");
    assert_eq!(status_of(&a).await.mls_epoch, epoch, "no re-key for the dead run");

    // a new link restores the seat for real
    let fresh = mint_for_petra(&a).await;
    let c2 = engine(&tmp.path().join("rejoiner-2"));
    adopt_relay(&c2, &url).await;
    c2.execute(Command::RecoverStart { link: fresh, phrase: petra_phrase, relays: Vec::new() })
        .await
        .expect("recover start");
    let s = wait_for_secs(&c2, "the second rejoin", 180, |s| {
        s.notice.starts_with("recovered:") || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "the second rejoin: {:?}", s.notice);
    assert_eq!(chain_len(&a).await, 3, "a second Restored block");
    assert_eq!(status_of(&a).await.mls_epoch, epoch + 1, "one re-key, for the live run");
}

/// **Review R2:** once the re-admission sealed, a withdrawal changes
/// nothing, and a new run on the spent link hears `link outdated` naming
/// that run - not silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_run_on_a_spent_link_hears_link_outdated() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let mut first = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    first.anchor().await;
    first.send(first.ready()).await;
    eventually("the seal", 60, 2, || chain_len(&a)).await;
    first.send(first.withdrawal()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(chain_len(&a).await, 2, "a sealed re-admission stays");
    drop(first);

    let mut second = ManualRejoiner::start(&url, &link, &petra_phrase, true).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "no answer to the run on the spent link");
        // anything else is the first run's Welcome, stored for the shared key
        if let Some(molt_net::ritual_net::RitualDelivery::Msg(
            molt_net::invite::RitualMsg::RecoverRefused { reason, attempt, .. },
            _,
        )) = second.next_within("the answer", left).await
        {
            assert_eq!(reason, "link outdated");
            assert_eq!(attempt, second.attempt, "names the run it answers");
            break;
        }
    }
}

/// **Review R4:** a link-less request whose seat proof does not verify is
/// dropped without an answer - only a sender that proved the seat learns
/// anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_less_request_without_a_seat_proof_gets_no_answer() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b, petra_phrase, _) = found_republic(tmp.path(), &url, 2).await;
    drop(b);
    let link = mint_for_petra(&a).await;
    let h = molt_engine::RecoveryInvite::parse(&link).expect("link").handover.expect("v2 handover");
    let self_ticket = "self-minted-ticket";
    let (_, identity_pk) = molt_engine::member_identity(&petra_phrase).expect("seat identity");
    let stranger = molt_storage::generate_seed_phrase().expect("a foreign phrase");
    let (wrong_sk, _) = molt_engine::member_identity(&stranger).expect("a foreign key");
    let entropy = molt_storage::seed_entropy(&stranger).expect("entropy");
    let (anchor_sk, anchor) = molt_net::nostr_identity(&entropy, self_ticket);
    let mls = molt_net::MlsMember::new(&wrong_sk, "petra").expect("mls identity");
    let kp_hex = hex::encode(mls.key_package().expect("key package"));
    let seat_proof = molt_engine::make_seat_proof(&wrong_sk, self_ticket, &kp_hex, &h.republic_id, &anchor, &[]);
    let dialer = molt_net::dial::Dialer::resolve("none", "local", 0).expect("direct dialer");
    let prober = molt_net::ritual_net::RitualNet::new(dialer, vec![url.clone()], &anchor_sk).expect("transport");
    let mut inbox = prober.inbox().await.expect("inbox");
    prober
        .send_ritual(
            &h.npub,
            &molt_net::invite::RitualMsg::Recover(molt_net::invite::RecoverRequest {
                member: "petra".to_string(),
                identity_pk,
                key_package: kp_hex,
                ticket: self_ticket.to_string(),
                seat_proof,
                new_nostr_pk: anchor,
                relays: Vec::new(),
                reply: None,
                anchor_first: false,
                version: String::new(),
                consent: String::new(),
                started: 0,
                caps: Vec::new(),
            }),
        )
        .await
        .expect("publish");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        if let Some(molt_net::ritual_net::RitualDelivery::Msg(molt_net::invite::RitualMsg::RecoverRefused { reason, .. }, _)) =
            inbox.recv(left).await
        {
            panic!("an unproven sender was answered: {reason}");
        }
    }
}
