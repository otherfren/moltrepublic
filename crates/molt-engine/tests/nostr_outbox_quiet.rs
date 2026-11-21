// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The outbox goes quiet for peers that are not there** (field
//! 2026-11-21: seats re-offered their whole unacked tail every ten
//! minutes, for ever, to gone or split peers - tens of thousands of frames
//! on the relay). Three real engines over a relay; the stall backoff is
//! shortened process-wide, so this file holds only these tests.

mod common;

use common::{adopt_relay, engine, read_chat, read_session, wait_for, wait_for_secs};
use std::time::Duration;

use molt_core::Command;
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

/// Past the first presence tick, whose one-off `MirrorWho` is not a
/// re-offer.
const FIRST_TICK: Duration = Duration::from_secs(35);

fn fast_resends() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    molt_net::group_runtime::__set_resend_backoff(Duration::from_millis(500), Duration::from_secs(2));
}

async fn store_relay() -> (LocalRelay, std::sync::Arc<MemoryDatabase>, String) {
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = LocalRelay::new(RelayBuilder::default().database(db.clone()));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, db, url)
}

async fn group_frames(db: &MemoryDatabase) -> usize {
    db.query(Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP)))
        .await
        .expect("query")
        .len()
}

/// A real 2-of-3 republic: mara founds, walter and dora join.
async fn found_trio(root: &std::path::Path, url: &str) -> [WalletHandle; 3] {
    let a = engine(&root.join("mara"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Drei".to_string(),
        member: "mara".to_string(),
        members: 3,
        threshold: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "two seat links", |s| {
        s.create.seats.len() >= 2
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["walter", "dora"].iter().zip(links) {
        let w = engine(&root.join(name));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    wait_for(&a, "both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Drei".to_string(), agenda: String::new(), features: Vec::new() })
        .await
        .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the joiner to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            .await;
    }
    let mut it = joiners.into_iter();
    [a, it.next().expect("walter"), it.next().expect("dora")]
}

async fn say(w: &WalletHandle, body: &str) {
    w.execute(Command::Chat { body: body.to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("chat");
}

async fn has_chat(w: &WalletHandle, body: &str, secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if read_chat(w).await.iter().any(|r| r.get("body").and_then(|v| v.as_str()) == Some(body)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Case (b): dora is gone, mara and walter are healthy. The relay's group
/// traffic goes flat; dora returns to a relay that kept nothing and still
/// receives what she missed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gone_seat_costs_no_frames_and_gets_everything_back() {
    fast_resends();
    let (_relay, db, url) = store_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let [a, b, c] = found_trio(tmp.path(), &url).await;
    let up = tokio::time::Instant::now();
    for w in [&a, &b, &c] {
        w.__seat_active_secs(5);
    }
    let ws_c = read_session(&c).await.active_workspace.clone();

    say(&a, "before the gap").await;
    assert!(has_chat(&c, "before the gap", 30).await, "the live path works");
    tokio::time::sleep(Duration::from_secs(6)).await; // the sheets land

    c.execute(Command::CloseWorkspace).await.expect("dora leaves");
    say(&a, "into the void").await;
    assert!(has_chat(&b, "into the void", 30).await, "walter hears it");
    tokio::time::sleep(Duration::from_secs(3)).await;
    db.wipe().await.expect("prune the relay");
    assert_eq!(group_frames(&db).await, 0, "nothing a later REQ could serve");

    // open rounds, then dora falls silent (5 s window); and past every
    // seat's one-off first-tick frame (`MirrorWho`)
    tokio::time::sleep(Duration::from_secs(12)).await;
    tokio::time::sleep_until(up + FIRST_TICK).await;
    let flat_from = group_frames(&db).await;
    tokio::time::sleep(Duration::from_secs(15)).await; // ~7 rounds at the 2 s ceiling
    let flat_to = group_frames(&db).await;
    eprintln!("group frames: {flat_from} -> {flat_to} over 15 s");
    assert_eq!(flat_from, flat_to, "re-offers went out to a gone seat");

    c.execute(Command::OpenWorkspace { id: ws_c }).await.expect("dora returns");
    assert!(
        has_chat(&c, "into the void", 30).await,
        "the returning seat must be re-offered what it missed"
    );
    for w in [&a, &b, &c] {
        w.execute(Command::CloseWorkspace).await.expect("close");
    }
}

/// Case (a): mara concludes she is locked out (her own outbox stalled,
/// live frames she cannot read; nobody she can read is up). From then on
/// she publishes nothing, although her peers still count as heard from
/// the replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locked_out_seat_publishes_nothing() {
    fast_resends();
    let backlog = common::Backlog::sealed(600);
    let (_relay, url) = common::backlog_relay(&backlog).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let [a, b, c] = found_trio(tmp.path(), &url).await;
    let ws_a = read_session(&a).await.active_workspace.clone();

    say(&a, "before the gap").await;
    assert!(has_chat(&c, "before the gap", 30).await, "the live path works");
    tokio::time::sleep(Duration::from_secs(6)).await; // dora's sheet proves a floor
    c.execute(Command::CloseWorkspace).await.expect("dora leaves");
    b.execute(Command::CloseWorkspace).await.expect("walter leaves");
    say(&a, "into the void").await;
    a.execute(Command::CloseWorkspace).await.expect("mara closes");
    // her reopened subscription meets unreadable live frames
    backlog.arm();
    a.execute(Command::OpenWorkspace { id: ws_a }).await.expect("mara reopens");
    let up = tokio::time::Instant::now();
    wait_for_secs(&a, "the lock-out", 90, |s| s.notice == "seat-locked-out").await;
    tokio::time::sleep_until(up + FIRST_TICK).await;

    let db = backlog.db();
    let from = group_frames(&db).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let to = group_frames(&db).await;
    eprintln!("group frames after the lock-out: {from} -> {to} over 10 s");
    assert_eq!(from, to, "a locked-out seat kept re-offering");
    a.execute(Command::CloseWorkspace).await.expect("close");
}
