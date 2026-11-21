// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **buzz B3 step 2 + chain divergence S2** — the stored-replay bound.
//!
//! A relay that replays forever costs a signature verification and a
//! dedup-ring slot per event, so one REQ may replay at most the bound. But a
//! healthy relay of an active group legitimately holds more than that, so
//! the bound ends the SESSION and the replay continues in pages backwards
//! with `until` (`docs/chain/chain_divergence_plan.md` S2, C6/C7). Only a
//! relay whose pages do not move back is backed off - and even then its
//! supervisor keeps running.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::relay_runtime::{RelayDownReason, RelayHealth, RelayRuntime};
use nostr::{Alphabet, EventBuilder, Filter, Keys, Kind, SingleLetterTag, Tag, Timestamp};
use nostr_relay_builder::MockRelay;

const H: &str = "6d6f6c74";

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

fn filter() -> Filter {
    Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), H)
}

/// `n` events, one per `step` seconds back from `newest`.
fn events(n: u64, newest: u64, step: u64) -> Vec<nostr::Event> {
    let keys = Keys::generate();
    (0..n)
        .map(|i| {
            EventBuilder::new(Kind::Custom(445), format!("e{i}"))
                .tag(Tag::parse(["h", H]).expect("h tag"))
                .custom_created_at(Timestamp::from_secs(newest - i * step))
                .sign_with_keys(&keys)
                .expect("sign")
        })
        .collect()
}

/// How a [`scripted_relay`] answers a REQ (newest first).
#[derive(Clone, Copy)]
enum Script {
    /// NIP-01: honours `since`/`until`, then EOSE.
    Honest,
    /// Ignores `until`: the same newest page on every REQ, never EOSE.
    SamePage,
    /// The FIRST connection sends this many events and drops; later ones
    /// are honest.
    CutAfter(usize),
}

struct Scripted {
    url: String,
    /// The filter of every REQ, in arrival order.
    reqs: Arc<Mutex<Vec<serde_json::Value>>>,
}

/// A hand-scripted relay over a fixed store (sorted newest first).
async fn scripted_relay(mut store: Vec<nostr::Event>, script: Script) -> Scripted {
    store.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    let store = Arc::new(store);
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!(
        "ws://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let log = reqs.clone();
    tokio::spawn(async move {
        let mut conn = 0usize;
        while let Ok((stream, _)) = listener.accept().await {
            conn += 1;
            let first = conn == 1;
            let store = store.clone();
            let log = log.clone();
            tokio::spawn(async move {
                use futures_util::{SinkExt, StreamExt};
                use tokio_tungstenite::tungstenite::Message;
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    let Ok(text) = msg.into_text() else { continue };
                    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
                        continue;
                    };
                    if parsed.get(0).and_then(|v| v.as_str()) != Some("REQ") {
                        continue;
                    }
                    let sub = parsed
                        .get(1)
                        .and_then(|v| v.as_str())
                        .unwrap_or("s")
                        .to_string();
                    let f = parsed.get(2).cloned().unwrap_or_default();
                    log.lock().expect("log").push(f.clone());
                    let since = f.get("since").and_then(|v| v.as_u64()).unwrap_or(0);
                    let until = f.get("until").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                    let honest = |e: &&nostr::Event| {
                        let t = e.created_at.as_secs();
                        t >= since && t <= until
                    };
                    let (page, eose): (Vec<&nostr::Event>, bool) = match script {
                        Script::Honest => (store.iter().filter(honest).collect(), true),
                        Script::SamePage => (store.iter().collect(), false),
                        Script::CutAfter(k) if first => {
                            (store.iter().filter(honest).take(k).collect(), false)
                        }
                        Script::CutAfter(_) => (store.iter().filter(honest).collect(), true),
                    };
                    for e in page {
                        let frame = serde_json::json!(["EVENT", sub, e]).to_string();
                        if ws.send(Message::text(frame)).await.is_err() {
                            return;
                        }
                    }
                    if eose {
                        let frame = serde_json::json!(["EOSE", sub]).to_string();
                        let _ = ws.send(Message::text(frame)).await;
                    } else if matches!(script, Script::CutAfter(_)) {
                        return; // drop the connection mid-replay
                    }
                }
            });
        }
    });
    Scripted { url, reqs }
}

async fn drain(sub: &mut molt_net::relay_runtime::Subscription) -> Vec<nostr::EventId> {
    let mut got = Vec::new();
    while let Some(ev) = sub.recv(Duration::from_millis(800)).await {
        got.push(ev.id);
    }
    got
}

/// C6: a relay holding three times the bound is READ, in pages, and stays
/// up; its cursor lands on the newest event once the replay completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_relay_is_paged_through_not_dropped() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let now = Timestamp::now().as_secs();
    let store = events(60, now - 10, 10);
    let rt = RelayRuntime::new(dialer(), vec![url.clone()])
        .with_history_bound(20)
        .with_backoff(Duration::from_millis(20), Duration::from_millis(200));
    for e in &store {
        rt.publish(e).await.expect("seed");
    }

    let mut sub = rt.clone().subscribe(filter()).await.expect("subscribe");
    let got = drain(&mut sub).await;
    for e in &store {
        assert!(
            got.contains(&e.id),
            "missing {} of {} delivered",
            e.created_at.as_secs(),
            got.len()
        );
    }
    assert!(
        sub.synced(Duration::from_secs(2)).await,
        "a completed paged replay syncs"
    );
    assert_eq!(rt.health().await.get(&url), Some(&RelayHealth::Up));
    assert_eq!(
        rt.down_reasons().await.get(&url),
        None,
        "a completed replay clears the reason"
    );
    assert_eq!(rt.cursors().await.get(&url), Some(&(now - 10)));
}

/// A relay whose page never moves back is backed off with the long
/// backoff - and the subscription survives: it is retried later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_stuck_on_one_page_is_backed_off_not_dropped() {
    let now = Timestamp::now().as_secs();
    let relay = scripted_relay(events(30, now - 10, 10), Script::SamePage).await;
    let rt = RelayRuntime::new(dialer(), vec![relay.url.clone()])
        .with_history_bound(10)
        .with_backoff(Duration::from_millis(20), Duration::from_secs(2));
    let mut sub = rt.clone().subscribe(filter()).await.expect("subscribe");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while rt.down_reasons().await.get(&relay.url) != Some(&RelayDownReason::ReplayStuck) {
        assert!(tokio::time::Instant::now() < deadline, "never marked stuck");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rt.health().await.get(&relay.url), Some(&RelayHealth::Down));
    let stuck_at = relay.reqs.lock().expect("log").len();
    // page one; page two does not move back, so one second is skipped;
    // the next page brings events past its `until`: the relay ignores it
    assert_eq!(stuck_at, 4, "{:?}", relay.reqs.lock().expect("log"));

    // backed off: nothing during most of the cap...
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        relay.reqs.lock().expect("log").len(),
        stuck_at,
        "hammered while backed off"
    );
    // ...and retried after it: the supervisor is alive
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while relay.reqs.lock().expect("log").len() == stuck_at {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the supervisor died"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        rt.cursors().await.get(&relay.url),
        None,
        "no EOSE, no cursor"
    );
    assert_eq!(drain(&mut sub).await.len(), 10, "exactly the bound, once");
}

/// C7: a session cut mid-replay (newest first) must not move the cursor,
/// or the resumed REQ starts past the part it never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_cut_midway_resumes_without_a_gap() {
    let now = Timestamp::now().as_secs();
    // spans five days: wider than the 48 h resubscribe overlap
    let store = events(10, now - 10, 12 * 3600);
    let relay = scripted_relay(store.clone(), Script::CutAfter(3)).await;
    let rt = RelayRuntime::new(dialer(), vec![relay.url.clone()])
        .with_backoff(Duration::from_millis(20), Duration::from_millis(200));
    let mut sub = rt.clone().subscribe(filter()).await.expect("subscribe");
    let got = drain(&mut sub).await;
    let reqs = relay.reqs.lock().expect("log").clone();
    assert!(reqs.len() >= 2, "the cut session was resumed: {reqs:?}");
    assert_eq!(
        reqs[1].get("since"),
        None,
        "the cursor moved before EOSE: {reqs:?}"
    );
    for e in &store {
        assert!(got.contains(&e.id), "gap at {}", e.created_at.as_secs());
    }
    assert_eq!(rt.cursors().await.get(&relay.url), Some(&(now - 10)));
}

/// Findings R11: a replay of more pages than the fan-in channel holds parks
/// the relay task on a full channel before its EOSE. The readability gate
/// must not need a consumer; the end of the replay does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_larger_than_the_channel_is_readable_before_anyone_drains_it() {
    let now = Timestamp::now().as_secs();
    let store = events(300, now - 10, 1);
    let relay = scripted_relay(store.clone(), Script::Honest).await;
    let rt = RelayRuntime::new(dialer(), vec![relay.url.clone()])
        .with_history_bound(50)
        .with_backoff(Duration::from_millis(20), Duration::from_millis(200));
    assert!(rt.channel_capacity() < store.len());
    let mut sub = rt.clone().subscribe(filter()).await.expect("subscribe");

    let st = sub.read_state(Duration::from_secs(5)).await;
    assert!(st.full(), "the relay answered, nothing drained yet: {st:?}");
    assert!(
        !sub.synced(Duration::from_secs(1)).await,
        "the end of a multi-page replay sits behind a full channel"
    );

    let got: std::collections::HashSet<_> = drain(&mut sub).await.into_iter().collect();
    assert!(store.iter().all(|e| got.contains(&e.id)), "{} of 300", got.len());
    assert!(sub.synced(Duration::from_secs(2)).await, "drained, the replay ends");
}

/// A connected relay that never answers keeps the gate shut, and the
/// first-answer wait returns on the one that does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_answer_opens_the_gate_a_silent_relay_does_not() {
    let now = Timestamp::now().as_secs();
    let answering = scripted_relay(events(5, now - 10, 1), Script::Honest).await;
    // accepts the REQ, then neither events nor EOSE
    let silent = scripted_relay(Vec::new(), Script::SamePage).await;
    let rt = RelayRuntime::new(dialer(), vec![answering.url.clone(), silent.url.clone()]);
    let mut sub = rt.subscribe(filter()).await.expect("subscribe");

    let began = tokio::time::Instant::now();
    let st = sub.first_read(Duration::from_secs(20)).await;
    assert_eq!((st.answered, st.connected), (1, 2));
    assert!(began.elapsed() < Duration::from_secs(5), "returned at the first answer");
    let st = sub.read_state(Duration::from_millis(500)).await;
    assert!(st.any() && !st.full(), "the silent relay never answered: {st:?}");
}

/// The plan's scale: 12 000 stored events against the production bound
/// (MockRelay answers at most 500 per REQ, so this one is scripted).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twelve_thousand_events_page_through_the_production_bound() {
    let now = Timestamp::now().as_secs();
    let store = events(12_000, now - 10, 1);
    let relay = scripted_relay(store.clone(), Script::Honest).await;
    let rt = RelayRuntime::new(dialer(), vec![relay.url.clone()])
        .with_history_bound(5_000)
        .with_backoff(Duration::from_millis(20), Duration::from_millis(200));
    let mut sub = rt.clone().subscribe(filter()).await.expect("subscribe");
    let got: std::collections::HashSet<_> = drain(&mut sub).await.into_iter().collect();
    assert_eq!(got.len(), 12_000);
    assert!(store.iter().all(|e| got.contains(&e.id)));
    let reqs = relay.reqs.lock().expect("log").clone();
    assert_eq!(reqs.len(), 4, "three pages, then the live tail: {reqs:?}");
    assert_eq!(reqs[3].get("until"), None, "the tail is live");
    // the tail keeps the 48 h overlap: back-dated wraps and resent
    // commits still arrive
    assert_eq!(
        reqs[3].get("since").and_then(|v| v.as_u64()),
        Some((now - 10).saturating_sub(48 * 3600))
    );
    assert_eq!(rt.health().await.get(&relay.url), Some(&RelayHealth::Up));
}
