// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A replay longer than the accept window lands whole** (G1): two real
//! engines over a relay paging at 400 stored events, the sender's stall
//! backoff pinned at 600 s, and a process-wide count of gap re-publishes -
//! so this file holds only this test.

mod common;

use common::{found_pair, read_chat, read_session};
use std::time::{Duration, Instant};

use molt_core::Command;
use molt_engine::WalletHandle;
use nostr_relay_builder::prelude::*;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

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

async fn store_relay() -> (LocalRelay, std::sync::Arc<MemoryDatabase>, String) {
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = LocalRelay::new(RelayBuilder::default().database(db.clone()).rate_limit(
        nostr_relay_builder::builder::RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 },
    ));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, db, url)
}

/// The densest second of stored 445 frames. At a page bound's worth in
/// one second the relay replay skips the rest of that second (`relay replay
/// skipped one second`) and those frames come only by a gap want - not
/// what these tests measure.
async fn densest_second(db: &MemoryDatabase) -> usize {
    let events = db.query(Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP))).await.expect("query");
    let mut per: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for e in events.iter() {
        *per.entry(e.created_at.as_secs()).or_default() += 1;
    }
    per.values().copied().max().unwrap_or(0)
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

/// G1 at the engine: a node away while its peer spoke far more than the
/// accept window reads a paged, newest-first replay whose head is not a
/// chat. Everything lands once, in the sender's order, from what the relay
/// sent - the sender re-publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_paged_replay_lands_whole_without_a_resend() {
    backed_off_senders();
    molt_net::ritual_net::__set_group_history_bound(400);
    let (_relay, db, url) = store_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(tmp.path(), &[&url], "Gap").await;
    say(&a, "warm").await;
    assert!(shows_all(&b, &["warm".to_string()], Duration::from_secs(30)).await.is_some());
    // the founding-era frames this seat never could read are asked for and
    // re-published once, live - before the measured part
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = match b.execute(Command::Status).await.expect("status") {
            molt_core::Reply::Status(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        if status.inbound_held.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "held before the test: {:?}", status.inbound_held);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    let ws_b = read_session(&b).await.active_workspace.clone();
    b.execute(Command::CloseWorkspace).await.expect("close b");
    let want: Vec<String> = (1..=1300).map(|i| format!("m{i}")).collect();
    for body in &want {
        say(&a, body).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // the head is not a chat
    a.execute(Command::Propose {
        surface: molt_core::Surface::Organization,
        payload: serde_json::json!({ "op": "set_name", "value": "Gap two" }),
    })
    .await
    .expect("propose");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = molt_net::group_runtime::__gap_republished();
    assert!(densest_second(&db).await < 400, "precondition: no second holds a page");

    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    let took = shows_all(&b, &want, Duration::from_secs(120)).await;
    eprintln!("long_replay_caught_up_after={took:?}");
    assert!(took.is_some(), "the whole backlog must land");
    in_order(&bodies(&b).await, &want);
    let republished = molt_net::group_runtime::__gap_republished() - before;
    eprintln!("long_replay_republished={republished}");
    assert_eq!(republished, 0, "the backlog came from the replay, not from resends");

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
