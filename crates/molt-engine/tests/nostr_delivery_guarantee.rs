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

mod common;

use common::{engine, read_chat, read_session};
use std::time::Duration;

use molt_core::{Command, SessionView};
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

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    common::wait_for_secs(w, what, 60, pred).await
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

/// A 2-of-2 republic; `pre_lock` founds it without the sealing lock (no
/// lock frames on the wire).
async fn found_two_of_two_as(root: &std::path::Path, url: &str, pre_lock: bool) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    a.__found_locked(!pre_lock);
    a.__lock_available(!pre_lock);
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
    b.__lock_available(!pre_lock);
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
        relays: Vec::new(),
    })
    .await
    .expect("join starts");

    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
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

    // no lock: a lock frame from walter right after petra's return would
    // expose the hole at once and the gap fetch would fill it - the resend
    // this keystone proves would never be needed
    let (a, b) = found_two_of_two_as(tmp.path(), &url, true).await;
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
    // no "it is not there yet" wait: the store's own answer above is the proof,
    // and a gap fetch may close the hole within seconds

    // …and the guarantee repairs it: walter's floor stopped below the gap, the
    // tail is fetched or re-offered, and it lands.
    assert!(
        wait_for_chat(&b, "into the void", 90).await,
        "the unacknowledged tail must be re-offered until it lands"
    );

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// The same gap under the sealing lock (every founding of this build): the
/// message missed while away LANDS, exactly once - by the gap fetch (a lock
/// frame from walter right after petra's return exposes the hole) or by
/// the resend, whichever comes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn under_the_lock_a_message_missed_while_away_lands_exactly_once() {
    let (relay, db) = prunable_relay().await;
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_two_of_two_as(tmp.path(), &url, false).await;
    let ws_b = read_session(&b).await.active_workspace.clone();
    a.execute(Command::Chat { body: "before the gap".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("walter speaks");
    assert!(wait_for_chat(&b, "before the gap", 30).await, "the live path works");
    tokio::time::sleep(Duration::from_secs(6)).await;
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::Chat { body: "into the void".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("walter speaks again");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let group_frames = Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP));
    assert!(!db.query(group_frames.clone()).await.expect("query").is_empty(), "the relay stored the traffic");
    db.wipe().await.expect("prune the relay");
    assert!(db.query(group_frames).await.expect("query").is_empty(), "nothing left to serve");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    assert!(wait_for_chat(&b, "into the void", 90).await, "the missed message lands");
    // a few more seconds for a late duplicate to show up
    tokio::time::sleep(Duration::from_secs(5)).await;
    let landed = read_chat(&b).await.iter().filter(|r| r.get("body").and_then(|v| v.as_str()) == Some("into the void")).count();
    assert_eq!(landed, 1, "exactly once");
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

#[derive(Clone)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The one buffer this binary logs into (a global subscriber installs once).
fn capture_logs() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static BUF: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(BUF.get_or_init(|| {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let _ = tracing_subscriber::fmt()
            .with_writer(Capture(std::sync::Arc::clone(&buf)))
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .with_ansi(false)
            .without_time()
            .try_init();
        buf
    }))
}

/// **WP1 step 2: a receiver whose window regressed after a hard stop** (the
/// window saved less recently than the ack went out). Its next sheet no
/// longer holds what walter's floor counts as acked: walter rewinds that
/// floor and re-offers the tail; petra applies none of it twice and
/// reports no message lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_receiver_whose_window_regressed_gets_the_tail_again_and_applies_it_once() {
    let logs = capture_logs();
    let (relay, _db) = prunable_relay().await;
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_two_of_two_as(tmp.path(), &url, true).await;
    for i in 0..20 {
        a.execute(Command::Chat { body: format!("tail {i}"), quote: None, channel: molt_core::ChannelRef::default() })
            .await
            .expect("walter sends");
    }
    assert!(wait_for_chat(&b, "tail 19", 60).await, "the tail reached petra");
    let ws = read_session(&a).await.workspaces[0].id.clone();
    let dir = molt_storage::find_workspace_dir(&tmp.path().join("founder"), &ws).expect("walter's dir");
    let cursor = |ts: &molt_core::TransportState| ts.outbound.get("petra").copied().unwrap_or_default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let acked = loop {
        let ts = molt_storage::peek_transport_state(&dir).unwrap_or_default();
        let head = ts.group.map_or(0, |g| g.log_seq);
        if cursor(&ts).acked_floor >= head && head > 0 {
            break cursor(&ts).acked_floor;
        }
        assert!(tokio::time::Instant::now() < deadline, "petra never acked the tail: {:?}", cursor(&ts));
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let lost_before = String::from_utf8_lossy(&logs.lock().expect("logs")).matches("inbound_gap_lost").count();
    b.__regress_window("walter", acked.saturating_sub(15));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ts = molt_storage::peek_transport_state(&dir).unwrap_or_default();
        if cursor(&ts).regressed_from > 0 && cursor(&ts).acked_floor < acked {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "walter never rewound: {:?}", cursor(&ts));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let ts = molt_storage::peek_transport_state(&dir).unwrap_or_default();
        if cursor(&ts).regressed_from == 0 && cursor(&ts).acked_floor >= acked {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the re-offered tail never landed: {:?}", cursor(&ts));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let bodies: Vec<String> =
        read_chat(&b).await.iter().filter_map(|r| r.get("body").and_then(|v| v.as_str()).map(str::to_string)).collect();
    for i in 0..20 {
        assert_eq!(bodies.iter().filter(|m| **m == format!("tail {i}")).count(), 1, "tail {i} applied twice or lost");
    }
    let text = String::from_utf8_lossy(&logs.lock().expect("logs")).to_string();
    assert_eq!(text.matches("inbound_gap_lost").count(), lost_before, "a false loss was reported");
    assert!(text.contains("ack_regressed"), "the rewind warned");
}
