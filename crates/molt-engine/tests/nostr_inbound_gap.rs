// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A missing envelope is asked for, never waited out** (field 2026-11:
//! one lost predecessor per sender held a restarted node deaf for the full
//! 900 s valve). Two real engines over a relay; the sender's stall backoff
//! is pinned at the 600 s ceiling process-wide, so only the gap request
//! can close a gap inside these tests - this file holds only them.

mod common;

use common::{found_pair, read_chat, read_session};
use std::time::{Duration, Instant};

use molt_core::Command;
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

/// A gap must close well inside this; the old hold needed 600..900 s.
const PROMPT: Duration = Duration::from_secs(20);

fn backed_off_senders() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    molt_net::group_runtime::__set_resend_backoff(Duration::from_secs(600), Duration::from_secs(600));
}

async fn store_relay() -> (LocalRelay, String) {
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = LocalRelay::new(RelayBuilder::default().database(db).rate_limit(
        nostr_relay_builder::builder::RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 },
    ));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

async fn say(w: &WalletHandle, body: &str) {
    w.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("chat");
}

async fn bodies(w: &WalletHandle) -> Vec<String> {
    read_chat(w)
        .await
        .iter()
        .filter_map(|r| r.get("body").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// Wait until `w` shows every one of `want`; the elapsed time, or `None`.
async fn shows_all(w: &WalletHandle, want: &[String], within: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < within {
        let have = bodies(w).await;
        if want.iter().all(|b| have.contains(b)) {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

fn in_order(have: &[String], want: &[String]) {
    let pos: Vec<usize> = want
        .iter()
        .map(|b| {
            assert_eq!(have.iter().filter(|h| *h == b).count(), 1, "{b} landed exactly once: {have:?}");
            have.iter().position(|h| h == b).expect("present")
        })
        .collect();
    assert!(pos.windows(2).all(|w| w[0] < w[1]), "the sender's order: {have:?}");
}

/// A live envelope lost on the way: its successor is not held for the
/// sender's next resend round - the receiver asks, the sender re-publishes
/// exactly it, both land in order within seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_envelope_is_asked_for_not_waited_out() {
    backed_off_senders();
    let (_relay, url) = store_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(tmp.path(), &[&url], "Gap").await;
    say(&a, "warm").await;
    assert!(shows_all(&b, &["warm".to_string()], Duration::from_secs(30)).await.is_some());

    b.__drop_inbound_chat("one");
    say(&a, "one").await;
    say(&a, "two").await;
    let want = ["warm".to_string(), "one".to_string(), "two".to_string()];
    let took = shows_all(&b, &want, PROMPT).await;
    eprintln!("gap_closed_after={took:?}");
    assert!(took.is_some(), "the stream behind the lost envelope must flow within {PROMPT:?}");
    assert_eq!(b.__dropped_inbound(), 1, "the loss was real");
    in_order(&bodies(&b).await, &want);

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// The field shape: a node restarts while its peer spoke; one EARLY
/// envelope of the replay never reaches it. The peer's stream is processed
/// within seconds of the reopen, in order, nothing lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_with_one_withheld_early_envelope_is_not_deafened() {
    backed_off_senders();
    let (_relay, url) = store_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(tmp.path(), &[&url], "Gap").await;
    say(&a, "warm").await;
    assert!(shows_all(&b, &["warm".to_string()], Duration::from_secs(30)).await.is_some());
    tokio::time::sleep(Duration::from_secs(5)).await; // b's sheet lands

    let ws_b = read_session(&b).await.active_workspace.clone();
    b.execute(Command::CloseWorkspace).await.expect("close b");
    let want: Vec<String> = (1..=20).map(|i| format!("m{i}")).collect();
    for body in &want {
        say(&a, body).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    b.__drop_inbound_chat("m3");
    let reopened = Instant::now();
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    let first = shows_all(&b, &want[..1], PROMPT).await;
    let all = shows_all(&b, &want, PROMPT).await;
    eprintln!("first_processed_after={first:?} all_after={all:?} since_reopen={:?}", reopened.elapsed());
    assert!(all.is_some(), "the whole stream must flow within {PROMPT:?} of the reopen");
    assert_eq!(b.__dropped_inbound(), 1, "the withheld envelope was real");
    in_order(&bodies(&b).await, &want);

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
