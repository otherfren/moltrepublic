// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **M2 keystones - the trickle sender** (`docs_archive/files/mirroring.md` §3.2):
//! one kind-447 event per tick, in the order top, manifest, data; a
//! persisted cursor resumes instead of restarting; the sender waits while
//! the group outbox has a frame pending or the hourly budget lacks
//! headroom; the daily byte cap stops it and a new day resumes it; a
//! `PieceWanted` range republishes exactly those pieces.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::file_plane::{manifest_of_reader, open_piece, Manifest, PIECE_PAYLOAD_LEN};
use molt_net::ritual_net::GroupChannel;
use molt_net::supervisor::{MemStateStore, StateStore};
use molt_net::trickle::{
    enqueue_publish, spawn_trickle, whole_series_ranges, TrickleConfig, PIECE_WIRE_BYTES,
};
use nostr_relay_builder::MockRelay;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

const SEED: [u8; 32] = [7u8; 32];
const KEY: [u8; 32] = [42u8; 32];

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// A three-piece file on disk: `(path, manifest)`.
fn three_pieces(tmp: &tempfile::TempDir) -> (std::path::PathBuf, Manifest) {
    let bytes = pattern(2 * PIECE_PAYLOAD_LEN + 1234);
    let path = tmp.path().join("three.bin");
    std::fs::write(&path, &bytes).expect("write");
    let (m, _) = manifest_of_reader(bytes.as_slice()).expect("manifest");
    assert_eq!(m.count, 3);
    (path, m)
}

fn job(path: &std::path::Path, m: &Manifest, ranges: Vec<(u32, u32)>) -> molt_core::PublishJob {
    molt_core::PublishJob {
        series: "aa01".to_string(),
        key: KEY.to_vec(),
        path: path.display().to_string(),
        count: m.count,
        size: m.size,
        root: m.root(),
        ranges,
        next: 0,
        started_at: unix_now(),
        stored: false,
        wiki_base: false,
    }
}

fn config(interval: Duration, clock: Arc<AtomicU64>) -> TrickleConfig {
    TrickleConfig {
        interval,
        daily_bytes: u64::MAX,
        clock: Arc::new(move || clock.load(Ordering::SeqCst)),
    }
}

/// Collect `(index, arrival instant)` of every piece under `KEY` that a
/// fresh subscription sees within `budget`, in arrival order.
async fn arrivals(url: &str, budget: Duration) -> Vec<(u32, tokio::time::Instant)> {
    let chan = GroupChannel::new(dialer(), vec![url.to_string()], SEED);
    let mut sub = chan
        .subscribe_files_from(unix_now() - 60, 3)
        .await
        .expect("subscribes");
    let deadline = tokio::time::Instant::now() + budget;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        if let molt_net::ritual_net::GroupRecv::Frame { content, .. } = sub.recv(left).await {
            if let Ok((index, _, _)) = open_piece(&KEY, &content) {
                out.push((index, tokio::time::Instant::now()));
            }
        }
    }
    out
}

/// The whole series drains ONE event per interval, top record first, then
/// the manifest chunk, then the data in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_series_drains_one_piece_per_tick_top_and_manifest_first() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let store = MemStateStore::new();
    let layout = Manifest::layout_for(m.count).expect("layout");
    store
        .update(|s| enqueue_publish(s, job(&path, &m, whole_series_ranges(layout))))
        .await;
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (_busy_tx, busy) = tokio::sync::watch::channel(false);
    let interval = Duration::from_millis(300);
    let clock = Arc::new(AtomicU64::new(unix_now()));
    let handle = spawn_trickle(chan, store.clone(), busy, config(interval, clock));
    handle.wake();
    let seen = arrivals(&url, Duration::from_secs(6)).await;
    let indices: Vec<u32> = seen.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![layout.top, m.count, 0, 1, 2], "top, manifest, data");
    for pair in seen.windows(2) {
        let gap = pair[1].1.duration_since(pair[0].1);
        assert!(gap >= Duration::from_millis(200), "one event per tick, gap {gap:?}");
    }
    assert!(store.load().await.file_jobs.publish.is_empty(), "a drained job leaves the queue");
    handle.shutdown().await;
}

/// A persisted cursor at position 2 publishes only the rest - the data
/// pieces - never the top record and manifest again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_resumes_at_the_persisted_cursor() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let store = MemStateStore::new();
    let layout = Manifest::layout_for(m.count).expect("layout");
    let mut resumed = job(&path, &m, whole_series_ranges(layout));
    resumed.next = 2;
    store.update(|s| enqueue_publish(s, resumed)).await;
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (_busy_tx, busy) = tokio::sync::watch::channel(false);
    let clock = Arc::new(AtomicU64::new(unix_now()));
    let handle = spawn_trickle(chan, store.clone(), busy, config(Duration::from_millis(20), clock));
    handle.wake();
    let seen = arrivals(&url, Duration::from_secs(3)).await;
    let indices: Vec<u32> = seen.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![0, 1, 2]);
    handle.shutdown().await;
}

/// While the outbox has a frame pending, or the hour's resend budget has
/// less than two rounds of headroom, nothing is published; both gates
/// lifted, the series flows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sender_waits_for_the_outbox_and_the_budget_headroom() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let store = MemStateStore::new();
    let layout = Manifest::layout_for(m.count).expect("layout");
    store
        .update(|s| enqueue_publish(s, job(&path, &m, whole_series_ranges(layout))))
        .await;
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (busy_tx, busy) = tokio::sync::watch::channel(true);
    let clock = Arc::new(AtomicU64::new(unix_now()));
    let handle = spawn_trickle(chan, store.clone(), busy, config(Duration::from_millis(20), clock));
    handle.wake();
    assert!(arrivals(&url, Duration::from_millis(600)).await.is_empty(), "outbox busy: silent");

    // the outbox went idle, but the budget is one round from spent
    let now = unix_now();
    store
        .update(|s| {
            s.group = Some(molt_core::GroupCursor {
                resend_rounds: 11,
                resend_window_start: now,
                ..Default::default()
            });
            true
        })
        .await;
    busy_tx.send_replace(false);
    handle.wake();
    assert!(arrivals(&url, Duration::from_millis(600)).await.is_empty(), "no headroom: silent");

    store
        .update(|s| {
            s.group = Some(molt_core::GroupCursor {
                resend_rounds: 10,
                resend_window_start: now,
                ..Default::default()
            });
            true
        })
        .await;
    handle.wake();
    let seen = arrivals(&url, Duration::from_secs(3)).await;
    assert_eq!(seen.len(), 5, "both gates lifted: the whole series");
    handle.shutdown().await;
}

/// The daily cap stops the sender after the first piece; the next UTC day
/// (the clock seam) resumes it, and the counter restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daily_cap_stops_the_sender_and_a_new_day_resumes_it() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let store = MemStateStore::new();
    let layout = Manifest::layout_for(m.count).expect("layout");
    store
        .update(|s| enqueue_publish(s, job(&path, &m, whole_series_ranges(layout))))
        .await;
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (_busy_tx, busy) = tokio::sync::watch::channel(false);
    let start = unix_now();
    let clock = Arc::new(AtomicU64::new(start));
    let mut cfg = config(Duration::from_millis(20), clock.clone());
    cfg.daily_bytes = PIECE_WIRE_BYTES + 1;
    let handle = spawn_trickle(chan, store.clone(), busy, cfg);
    handle.wake();
    let seen = arrivals(&url, Duration::from_secs(1)).await;
    assert_eq!(seen.len(), 1, "one piece fits the day, the second does not");
    let jobs = store.load().await.file_jobs;
    assert_eq!(jobs.sent_bytes, PIECE_WIRE_BYTES);
    assert_eq!(jobs.publish[0].next, 1);

    clock.store(start + 86_400, Ordering::SeqCst);
    handle.wake();
    // a fresh subscription replays the day's stored events: the first
    // piece again, plus exactly one more
    let seen = arrivals(&url, Duration::from_secs(1)).await;
    let mut indices: Vec<u32> = seen.iter().map(|(i, _)| *i).collect();
    indices.sort_unstable();
    assert_eq!(indices, vec![m.count, layout.top], "a new day grants one more");
    assert_eq!(store.load().await.file_jobs.sent_bytes, PIECE_WIRE_BYTES, "the counter restarted");
    handle.shutdown().await;
}

/// A `PieceWanted` for the range (1, 1) republishes exactly data index 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wanted_range_republishes_exactly_those_pieces() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let store = MemStateStore::new();
    store.update(|s| enqueue_publish(s, job(&path, &m, vec![(1, 1)]))).await;
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (_busy_tx, busy) = tokio::sync::watch::channel(false);
    let clock = Arc::new(AtomicU64::new(unix_now()));
    let handle = spawn_trickle(chan, store.clone(), busy, config(Duration::from_millis(20), clock));
    handle.wake();
    let seen = arrivals(&url, Duration::from_secs(2)).await;
    let indices: Vec<u32> = seen.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![1]);
    handle.shutdown().await;
}

/// The queue dedups by series: a whole-series job absorbs a later range
/// (the running job covers it), a range job merges with a range job, and
/// a whole-series job replaces a range job.
#[test]
fn the_publish_queue_dedups_by_series() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (path, m) = three_pieces(&tmp);
    let layout = Manifest::layout_for(m.count).expect("layout");
    let mut s = molt_core::TransportState::default();
    assert!(enqueue_publish(&mut s, job(&path, &m, vec![(1, 1)])));
    assert!(enqueue_publish(&mut s, job(&path, &m, vec![(2, 2)])));
    assert_eq!(s.file_jobs.publish.len(), 1);
    assert_eq!(s.file_jobs.publish[0].ranges, vec![(1, 1), (2, 2)]);
    assert!(!enqueue_publish(&mut s, job(&path, &m, vec![(2, 2)])), "already queued");
    assert!(enqueue_publish(&mut s, job(&path, &m, whole_series_ranges(layout))));
    assert_eq!(s.file_jobs.publish.len(), 1);
    assert_eq!(s.file_jobs.publish[0].ranges, whole_series_ranges(layout));
    assert!(!enqueue_publish(&mut s, job(&path, &m, vec![(0, 0)])), "the whole job covers it");
    assert_eq!(whole_series_ranges(layout), vec![(layout.top, layout.top), (3, 3), (0, 2)]);
}
