// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N5.3b — the Nostr twin of `delivery_guarantee.rs`.**
//!
//! The mesh keystone takes a queue deaf and proves the tail is re-offered. Its
//! broadcast twin needs the relay to LOSE something — the marmot spec's "a
//! relay dies or prunes".
//!
//! `MockRelay` alone would make this test meaningless: its store holds 75 000
//! events and a 445 subscription carries no `since`/`until`, so a returning
//! member's fresh placement is a full history query. The message would arrive
//! because the RELAY kept it, and the keystone would pass with the entire
//! guarantee absent.
//!
//! A relay that forgets from the START does not work either — the founding
//! ritual itself needs the replay (subscribe-before-advertise leans on it). So
//! the relay stores normally, and its history is WIPED at the exact moment the
//! gap is created. After that nothing can arrive from history, and anything
//! that does arrive was genuinely re-sent.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

/// A relay whose store we hold, so the test can prune it mid-flight.
async fn prunable_relay() -> (LocalRelay, std::sync::Arc<MemoryDatabase>) {
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = LocalRelay::new(RelayBuilder::default().database(db.clone()));
    relay.run().await.expect("relay runs");
    (relay, db)
}

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nnotice={:?} create={:?} join={:?}",
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

async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn found_two_of_two(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("founding starts");

    let s = wait_for(&a, "the seat link", |s| {
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

    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess".to_string(),
        features: Vec::new(),
    })
    .await
    .expect("charter");
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    // entering is gated on the phrase-backup step now (2026-08-08) — both ends
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
    (a, b)
}

async fn wait_for_chat(w: &WalletHandle, body: &str, secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if read_chat(w)
            .await
            .iter()
            .any(|r| r.get("body").and_then(|v| v.as_str()) == Some(body))
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// A message spoken while a member is away, over a relay that keeps nothing,
/// is re-offered until it lands.
///
/// The gap is REAL and asserted before the repair: without that assertion the
/// test could not tell "the resend worked" from "the relay served it from
/// history", which is the one way this keystone can be inert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_missed_while_away_is_resent_until_it_lands() {
    let (relay, db) = prunable_relay().await;
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;
    let ws_b = read_session(&b).await.active_workspace.clone();

    // petra must have acked at least once, or walter has no proven floor and
    // — correctly — never resends: absence of evidence is not evidence of loss
    a.execute(Command::Chat {
        body: "before the gap".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks");
    assert!(
        wait_for_chat(&b, "before the gap", 30).await,
        "the live path must work before the gap is meaningful"
    );
    tokio::time::sleep(Duration::from_secs(6)).await; // let petra's sheet land

    // …petra leaves, and walter speaks into a relay that forgets
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::Chat {
        body: "into the void".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks again");
    tokio::time::sleep(Duration::from_secs(3)).await;
    // …and the relay prunes. Proven, not assumed: the store held walter's 445s
    // before the wipe and holds none after, so "the gap is real" rests on the
    // database's own answer rather than on a sleep having been long enough.
    let group_frames = Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP));
    let before = db
        .query(group_frames.clone())
        .await
        .expect("query before")
        .len();
    assert!(before > 0, "the relay must have stored the traffic to begin with");
    db.wipe().await.expect("prune the relay");
    let after = db.query(group_frames).await.expect("query after").len();
    assert_eq!(after, 0, "the wipe must leave nothing a later REQ could serve");

    // …petra returns. THE GAP IS REAL: the relay kept nothing, so nothing can
    // arrive from history.
    b.execute(Command::OpenWorkspace { id: ws_b.clone() })
        .await
        .expect("reopen b");
    assert!(
        !wait_for_chat(&b, "into the void", 5).await,
        "the pruned relay must not serve it from history — otherwise this test \
         proves nothing about the resend"
    );

    // …and the guarantee repairs it: walter's floor stopped below the gap, the
    // stall clock re-offers the tail, and it lands.
    assert!(
        wait_for_chat(&b, "into the void", 90).await,
        "the unacknowledged tail must be re-offered until it lands"
    );

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
