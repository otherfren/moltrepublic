// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Gap wants wait for the stored replay** (follow-up to G1): a slow,
//! paged replay delivers older pages seconds apart; asking the sender for
//! what the relay is about to deliver anyway is the re-publication v0.1.0
//! removed. Process-wide counters, so this file holds only this test.

mod common;

use common::{found_pair, read_chat, read_session};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::Arc;
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

/// Delays every PAGED 445 REQ (one carrying `until`) once armed, and
/// remembers when the last one was let through (ms since `origin`).
#[derive(Debug, Clone)]
struct SlowPages {
    armed: Arc<AtomicBool>,
    origin: Instant,
    last_page_ms: Arc<AtomicU64>,
}

impl nostr_relay_builder::builder::QueryPolicy for SlowPages {
    fn admit_query<'a>(
        &'a self,
        query: &'a Filter,
        _addr: &'a std::net::SocketAddr,
    ) -> BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult> {
        Box::pin(async move {
            if self.armed.load(SeqCst) && query.until.is_some() {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.last_page_ms.fetch_max(ms, SeqCst);
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

/// A slow paged replay with one real hole: no gap want reaches the sender
/// before the last page was served, and the hole is asked for right after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gap_wants_wait_for_the_replay_and_then_close_a_real_hole() {
    backed_off_senders();
    molt_net::ritual_net::__set_group_history_bound(100);
    let origin = Instant::now();
    let slow = SlowPages { armed: Arc::new(AtomicBool::new(false)), origin, last_page_ms: Arc::new(AtomicU64::new(0)) };
    let db = Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = LocalRelay::new(
        RelayBuilder::default()
            .database(db.clone())
            .rate_limit(nostr_relay_builder::builder::RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 })
            .query_policy(slow.clone()),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
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
    let want: Vec<String> = (1..=500).map(|i| format!("m{i}")).collect();
    for body in &want {
        say(&a, body).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(densest_second(&db).await < 100, "precondition: no second holds a page");
    let asked_before = molt_net::group_runtime::__gap_asked();
    let republished_before = molt_net::group_runtime::__gap_republished();
    b.__drop_inbound_chat("m250");
    slow.armed.store(true, SeqCst);

    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut first_ask_ms = None;
    loop {
        if first_ask_ms.is_none() && molt_net::group_runtime::__gap_asked() > asked_before {
            first_ask_ms = Some(u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
        if shows_all(&b, &want, Duration::from_millis(1)).await.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the backlog never landed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let last_page = slow.last_page_ms.load(SeqCst);
    let first_ask = first_ask_ms.expect("the real hole was asked for");
    eprintln!("last_page_ms={last_page} first_ask_ms={first_ask}");
    assert!(last_page > 0, "the replay was paged");
    assert!(first_ask > last_page, "no want before the last page was served");
    assert!(first_ask - last_page < 20_000, "the hole is asked for right after the replay");
    assert_eq!(
        molt_net::group_runtime::__gap_republished() - republished_before,
        1,
        "only the real hole is re-published"
    );
    in_order(&bodies(&b).await, &want);

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
