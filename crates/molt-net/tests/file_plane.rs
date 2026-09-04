// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F2 keystones — the file data plane over relays**
//! (`docs_archive/transport/file_transfer_nostr.md`): a chunk series published
//! once is fetched and verified by a member that shares nothing with the
//! sharer but the group's rotation seed and exporter ring — and every
//! refusal (cap, checksum, absent series) is honest, never silence.

mod common;

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::file_plane::{fetch_series, publish_series};

/// The v1 (legacy) cap the old series were published under.
const FILE_CAP_DEFAULT_BYTES: u64 = 4 * 1024 * 1024;
use molt_net::ritual_net::GroupChannel;
use nostr_relay_builder::MockRelay;
use sha2::{Digest, Sha256};

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

const SEED: [u8; 32] = [7u8; 32];
const EXPORTER: [u8; 32] = [9u8; 32];

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A 150-KiB series round-trips: published by one channel handle, fetched
/// by an independent one (the downloader was never online with the
/// sharer), verified against the share checksum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunk_series_roundtrips_over_the_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let sharer = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let bytes = pattern(150 * 1024);

    let (stamp, chunks) =
        publish_series(&sharer, &EXPORTER, "aa01", &bytes, Some(FILE_CAP_DEFAULT_BYTES))
            .await
            .expect("publishes");
    assert_eq!(usize::from(chunks), 4, "150 KiB at the relay chunk size");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let got = fetch_series(
        &fetcher,
        &[EXPORTER],
        "aa01",
        &sha_hex(&bytes),
        stamp,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(10)),
    )
    .await
    .expect("fetches");
    assert_eq!(got, bytes);
}

/// Two interleaved series stay separate — the header's series id routes
/// every chunk, and each download verifies its own checksum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_series_interleave_without_mixing() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let a = pattern(60 * 1024);
    let b: Vec<u8> = pattern(90 * 1024).into_iter().rev().collect();

    let (stamp_a, _) = publish_series(&chan, &EXPORTER, "aa02", &a, Some(FILE_CAP_DEFAULT_BYTES))
        .await
        .expect("a publishes");
    let (stamp_b, _) = publish_series(&chan, &EXPORTER, "bb02", &b, Some(FILE_CAP_DEFAULT_BYTES))
        .await
        .expect("b publishes");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let got_b = fetch_series(
        &fetcher,
        &[EXPORTER],
        "bb02",
        &sha_hex(&b),
        stamp_b,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(10)),
    )
    .await
    .expect("b fetches");
    assert_eq!(got_b, b);
    let got_a = fetch_series(
        &fetcher,
        &[EXPORTER],
        "aa02",
        &sha_hex(&a),
        stamp_a,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(10)),
    )
    .await
    .expect("a fetches");
    assert_eq!(got_a, a);
}

/// The cap refuses at publish time (nothing reaches the relay), and a
/// fetched series that does not hash to the share checksum is refused —
/// the log-anchored checksum is the trust root, not the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cap_and_the_checksum_refuse_honestly() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);

    // over-cap: refused locally
    let big = vec![1u8; 1024];
    assert!(
        publish_series(&chan, &EXPORTER, "cc03", &big, Some(1023)).await.is_err(),
        "the cap refuses at publish time"
    );

    // published bytes that do not match the claimed checksum: refused
    let bytes = pattern(10 * 1024);
    let (stamp, _) = publish_series(&chan, &EXPORTER, "dd04", &bytes, Some(FILE_CAP_DEFAULT_BYTES))
        .await
        .expect("publishes");
    let err = fetch_series(
        &chan,
        &[EXPORTER],
        "dd04",
        &sha_hex(b"different bytes"),
        stamp,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(10)),
    )
    .await;
    assert!(err.is_err(), "a checksum mismatch is refused, never returned");
}

/// A series the relays do not hold ends in the honest miss (the F3 caller
/// falls back to a FileRequested round) — within the quiet budget, not
/// forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_absent_series_misses_honestly() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url], SEED);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let err = fetch_series(
        &chan,
        &[EXPORTER],
        "ee05",
        &sha_hex(b"whatever"),
        now,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(2)),
    )
    .await;
    assert!(err.is_err(), "an absent series is a miss, not a hang");
}

/// FP2: a subscription whose every relay connection ENDED must surface as
/// a transport fault, never as the honest miss — "not stored" sends the
/// user to the sharer, "no relay reachable" sends them to their network,
/// and conflating the two burns the wrong hour.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_relay_mid_fetch_is_a_transport_error_not_a_miss() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};

    // minimal cuttable TCP proxy (the nostr_window_roll.rs shape)
    let relay = MockRelay::run().await.expect("in-process relay");
    let target = relay.url().await.to_string().trim_start_matches("ws://").to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let port = listener.local_addr().expect("addr").port();
    let enabled = Arc::new(AtomicBool::new(true));
    let accepts = Arc::new(AtomicUsize::new(0));
    let forwards: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    {
        let (on, n, fw) = (enabled.clone(), accepts.clone(), forwards.clone());
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                n.fetch_add(1, Ordering::SeqCst);
                if !on.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                let target = target.clone();
                fw.lock().await.push(tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                }));
            }
        });
    }

    let url = format!("ws://127.0.0.1:{port}");
    let chan = GroupChannel::new(dialer(), vec![url], SEED);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let fetch = tokio::spawn(async move {
        fetch_series(
            &chan,
            &[EXPORTER],
            "ff06",
            &sha_hex(b"never published"),
            now,
            FILE_CAP_DEFAULT_BYTES,
            // the deaf verdict is evaluated at the END of a quiet slice, so
            // the slice must outlive the cut below but keep the test fast
            Some(Duration::from_secs(3)),
        )
        .await
    });

    // wait until the fetch's subscription actually dialed through, give the
    // REQ a moment to place, then the relay goes away
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while accepts.load(Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the fetch never dialed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    enabled.store(false, Ordering::SeqCst);
    for f in forwards.lock().await.drain(..) {
        f.abort();
    }

    let err = tokio::time::timeout(Duration::from_secs(15), fetch)
        .await
        .expect("a deaf subscription must end the fetch at the quiet slice")
        .expect("join")
        .expect_err("a dead relay is an error");
    let msg = err.to_string();
    assert!(msg.contains("deaf"), "the error names the transport fault: {msg}");
    assert!(
        !msg.contains("no chunk of this series arrived"),
        "a dead relay must not read as a miss: {msg}"
    );
}

/// FP1 (§5.4): a chunk-series publish rides the SAME hourly budget as the
/// resend rounds — a spent budget holds the upload with a named refusal
/// (nothing reaches the relays), a fresh one consumes exactly one round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spent_publish_budget_holds_the_upload() {
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct MemStore(Arc<tokio::sync::Mutex<molt_core::TransportState>>);
    impl molt_net::supervisor::StateStore for MemStore {
        async fn load(&self) -> molt_core::TransportState {
            self.0.lock().await.clone()
        }
        async fn save(&self, state: molt_core::TransportState) {
            *self.0.lock().await = state;
        }
    }

    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url], SEED);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let bytes = pattern(64 * 1024);

    // spent: the window is current and every round is consumed
    let spent = MemStore::default();
    {
        let mut s = spent.0.lock().await;
        s.group = Some(molt_core::GroupCursor {
            resend_rounds: u32::MAX, // any value >= the hourly cap
            resend_window_start: now,
            ..molt_core::GroupCursor::default()
        });
    }
    let err = molt_net::file_plane::publish_series_metered(
        &chan, &EXPORTER, "aa10", &bytes, Some(FILE_CAP_DEFAULT_BYTES), &spent, now,
    )
    .await
    .expect_err("a spent budget holds the upload");
    assert!(err.to_string().contains("budget"), "{err}");
    // …and nothing reached the relay
    let miss = fetch_series(
        &chan,
        &[EXPORTER],
        "aa10",
        &sha_hex(&bytes),
        now,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(2)),
    )
    .await;
    assert!(miss.is_err(), "the held series must not be fetchable");

    // fresh: the publish goes through and consumes exactly one round
    let fresh = MemStore::default();
    let (stamp, chunks) = molt_net::file_plane::publish_series_metered(
        &chan, &EXPORTER, "bb10", &bytes, Some(FILE_CAP_DEFAULT_BYTES), &fresh, now,
    )
    .await
    .expect("a fresh budget publishes");
    assert!(stamp > 0 && chunks > 0);
    let cur = fresh.0.lock().await.group.expect("cursor persisted");
    assert_eq!(cur.resend_rounds, 1, "one series = one round");
    let got = fetch_series(
        &chan,
        &[EXPORTER],
        "bb10",
        &sha_hex(&bytes),
        stamp,
        FILE_CAP_DEFAULT_BYTES,
        Some(Duration::from_secs(5)),
    )
    .await
    .expect("the granted series fetches");
    assert_eq!(got, bytes);
}

// ---------------------------------------------------------------------------
// Series v2 (`docs_archive/files/mirroring.md` §3.1): sealed under the FILE's key
// ---------------------------------------------------------------------------

use molt_net::file_plane::{
    fetch_series_v2, manifest_of_reader, publish_series_v2, seal_piece, Manifest, SeriesExpect,
    PIECE_PAYLOAD_LEN,
};

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

const KEY: [u8; 32] = [42u8; 32];

fn write_tmp(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).expect("write");
    path
}

fn expect_of(bytes: &[u8]) -> (Manifest, SeriesExpect, String) {
    let (m, sha) = manifest_of_reader(bytes).expect("manifest");
    let expect = SeriesExpect { count: m.count, size: m.size, root: m.root() };
    (m, expect, sha)
}

/// A three-piece file (two full blocks, one short) round-trips: published
/// from disk slice by slice, fetched by an independent channel under the
/// file's key alone - no exporter secret involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_v2_series_roundtrips_under_the_files_key() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(2 * PIECE_PAYLOAD_LEN + 1234);
    let path = write_tmp(&tmp, "three.bin", &bytes);
    let (manifest, expect, sha) = expect_of(&bytes);
    assert_eq!(manifest.count, 3);
    assert_eq!(sha, sha_hex(&bytes));

    let sharer = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (start, count) = publish_series_v2(&sharer, &KEY, &path, &manifest, None)
        .await
        .expect("publishes");
    assert_eq!(count, 3);
    assert!(start > 0);

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("fetches");
    assert_eq!(m, manifest);
    got.truncate(usize::try_from(m.size).expect("fits"));
    assert_eq!(got, bytes);
}

/// A forged piece under the right key (wrong bytes at index 1) fails its
/// manifest hash and is dropped - the honest piece still lands; a series
/// published under another key never opens at all (the honest miss).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_piece_is_refused_by_hash_and_a_foreign_key_never_opens() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(2 * PIECE_PAYLOAD_LEN + 7);
    let path = write_tmp(&tmp, "tampered.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);

    // the forgery goes out FIRST, so a replay serves it before the truth
    let forged = seal_piece(&KEY, 1, manifest.count, b"not the real slice").expect("seal");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    chan.publish_file_content_at(&forged, now).await.expect("forged publish");
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, None)
        .await
        .expect("publishes");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    fetch_series_v2(&fetcher, &KEY, start.min(now), &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("the honest pieces complete the series");
    got.truncate(bytes.len());
    assert_eq!(got, bytes, "the forged slice never landed");

    // another key: nothing opens, no manifest - the honest miss
    let mut nothing: Vec<u8> = Vec::new();
    let err = fetch_series_v2(&fetcher, &[43u8; 32], start, &expect, &mut nothing, Some(Duration::from_secs(2)))
        .await
        .expect_err("a foreign key opens nothing");
    assert!(err.to_string().contains("no manifest"), "{err}");
    assert!(nothing.is_empty());
}

/// A series whose pieces sit in two day windows (the manifest a day
/// earlier than the data - a trickle) fetches from its START stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_v2_series_spanning_two_day_windows_fetches_from_its_start() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 99);
    let path = write_tmp(&tmp, "two-days.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let yesterday = now - 86_400;
    let count = manifest.count;
    // manifest pieces (index >= count) yesterday, data pieces today
    let stamp_for = move |index: u32| if index >= count { yesterday } else { now };

    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");
    assert_eq!(start, yesterday, "the start is the earliest stamp");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("both windows are subscribed");
    got.truncate(bytes.len());
    assert_eq!(got, bytes);
}

/// Relays replay stored events NEWEST first: a series published manifest
/// first (the oldest stamps) delivers every data slice BEFORE its manifest.
/// Seventy pieces - more than any buffer - must still complete: slices
/// land as they arrive and verify once the manifest lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seventy_slices_replayed_before_their_manifest_still_complete() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(69 * PIECE_PAYLOAD_LEN + 17);
    let path = write_tmp(&tmp, "seventy.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    assert_eq!(manifest.count, 70);
    let now = unix_now();
    let count = manifest.count;
    // manifest pieces an hour earlier than the data: newest-first replay
    // hands the fetcher all seventy slices before a single manifest piece
    let stamp_for = move |index: u32| if index >= count { now - 3_600 } else { now };
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("every slice completes");
    assert_eq!(m, manifest);
    got.truncate(bytes.len());
    assert_eq!(got, bytes);
}

/// A forged manifest chunk and a forged top record, both published
/// NEWEST so a replay serves them first, are dropped by hash - the honest
/// manifest wins and the series completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_manifest_is_dropped_by_hash_and_the_series_completes() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 5);
    let path = write_tmp(&tmp, "forged-manifest.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let layout = Manifest::layout_for(manifest.count).expect("layout");
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let now = unix_now();
    // the honest series a minute ago…
    let stamp_for = move |_: u32| now - 60;
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");
    // …the forgeries now: a chunk naming wrong slice hashes, a record with a
    // wrong chunk hash (its root cannot match), and a slice
    let wrong_chunk = vec![0xEEu8; 32 * 2];
    let forged_chunk = seal_piece(&KEY, manifest.count, manifest.count, &wrong_chunk).expect("seal");
    chan.publish_file_content_at(&forged_chunk, now).await.expect("forged chunk");
    let mut wrong_top = manifest.top_bytes();
    if let Some(last) = wrong_top.last_mut() {
        *last ^= 0xFF;
    }
    let forged_top = seal_piece(&KEY, layout.top, manifest.count, &wrong_top).expect("seal");
    chan.publish_file_content_at(&forged_top, now).await.expect("forged top");
    let forged_slice = seal_piece(&KEY, 0, manifest.count, b"not slice zero").expect("seal");
    chan.publish_file_content_at(&forged_slice, now).await.expect("forged slice");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("the honest manifest and slices complete the series");
    assert_eq!(m, manifest);
    got.truncate(bytes.len());
    assert_eq!(got, bytes, "no forgery landed in the end");
}

/// A relay without the 60-notes-per-minute default, for series of a
/// hundred-odd pieces: the tests below measure the fetcher, not the pacing.
async fn fast_relay() -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::builder::{RateLimit, RelayBuilder};
    use nostr_relay_builder::LocalRelay;
    let relay = LocalRelay::new(RelayBuilder::default().rate_limit(RateLimit {
        max_reqs: 500,
        notes_per_minute: 1_000_000,
    }));
    relay.run().await.expect("fast relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// The kind-447 REQ replays EVERY file's pieces under the day's tag: a
/// one-piece fetch must complete beside a foreign series far larger than
/// anything a per-series bound would size for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_series_fetches_beside_a_large_foreign_one() {
    let (_relay, url) = fast_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    // the foreign series: 120 pieces under another key, published NEWEST
    let foreign_key = [77u8; 32];
    let now = unix_now();
    for i in 0..120u32 {
        let fill = u8::try_from(i).unwrap_or(0);
        let piece = seal_piece(&foreign_key, i, 120, &[fill; 100]).expect("seal");
        chan.publish_file_content_at(&piece, now).await.expect("foreign piece");
    }
    let bytes = pattern(1234);
    let path = write_tmp(&tmp, "small.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let stamp_for = move |_: u32| now - 60;
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("the small series completes beside the large one");
    assert_eq!(m, manifest);
    assert_eq!(got, bytes);
}

/// A forged LAST slice of the full block length (the honest one is short)
/// is dropped by geometry before it can land - the sink ends at the
/// file's size, no trailing garbage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_full_length_last_slice_is_dropped_by_geometry() {
    let (_relay, url) = fast_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 5);
    let path = write_tmp(&tmp, "short-tail.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let now = unix_now();
    let stamp_for = move |_: u32| now - 60;
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");
    // the forgery: index 1 (the last) at the full block length, NEWEST
    let forged = seal_piece(&KEY, 1, manifest.count, &vec![0xAAu8; PIECE_PAYLOAD_LEN]).expect("seal");
    chan.publish_file_content_at(&forged, now).await.expect("forged tail");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("completes");
    assert_eq!(got.len(), bytes.len(), "no full-length forgery grew the sink");
    assert_eq!(got, bytes);
}

/// A slice that landed unverified (its chunk still missing) is never
/// overwritten by a later forgery at the same index: the honest slice
/// arrives NEWEST, the forgery next, the manifest last - the honest one
/// must still be what verifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forgery_never_evicts_a_landed_slice() {
    let (_relay, url) = fast_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 5);
    let path = write_tmp(&tmp, "evict.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let now = unix_now();
    let count = manifest.count;
    // manifest oldest, honest slices newest
    let stamp_for = move |index: u32| if index >= count { now - 3_600 } else { now };
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");
    // the forgery at index 0, full length, stamped BETWEEN honest data and manifest
    let forged = seal_piece(&KEY, 0, count, &vec![0xBBu8; PIECE_PAYLOAD_LEN]).expect("seal");
    chan.publish_file_content_at(&forged, now - 60).await.expect("forged slice");

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("the honest slice is what verifies");
    assert_eq!(m, manifest);
    assert_eq!(got, bytes, "the forgery did not evict the landed slice");
}

/// Twelve forged manifest chunks for one slot, all published NEWEST so a
/// replay serves them before the honest chunk: none may evict it - the
/// candidates wait for the top record, and the one it names wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twelve_forged_chunks_for_one_slot_do_not_evict_the_honest_one() {
    let (_relay, url) = fast_relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 5);
    let path = write_tmp(&tmp, "twelve.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);
    let chan = GroupChannel::new(dialer(), vec![url.clone()], SEED);
    let now = unix_now();
    // the honest series an hour ago (record oldest of all)
    let count = manifest.count;
    let layout = Manifest::layout_for(count).expect("layout");
    let stamp_for = move |index: u32| if index == layout.top { now - 7_200 } else { now - 3_600 };
    let (start, _) = publish_series_v2(&chan, &KEY, &path, &manifest, Some(&stamp_for))
        .await
        .expect("publishes");
    for i in 0..12u8 {
        let wrong_chunk = vec![i; 32 * 2];
        let forged = seal_piece(&KEY, count, count, &wrong_chunk).expect("seal");
        chan.publish_file_content_at(&forged, now - u64::from(i)).await.expect("forged chunk");
    }

    let fetcher = GroupChannel::new(dialer(), vec![url], SEED);
    let mut got: Vec<u8> = Vec::new();
    let m = fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("the honest chunk survives twelve forgeries");
    assert_eq!(m, manifest);
    assert_eq!(got, bytes);
}

/// A relay's verdict is permanent: a `blocked:` answer ends the series
/// on the first attempt instead of a hundred seconds of retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_verdict_ends_the_series_on_the_first_attempt() {
    use nostr_relay_builder::builder::{PolicyResult, RelayBuilder, WritePolicy};
    #[derive(Debug)]
    struct NoFiles;
    impl WritePolicy for NoFiles {
        fn admit_event<'a>(
            &'a self,
            event: &'a nostr::Event,
            _addr: &'a std::net::SocketAddr,
        ) -> nostr::util::BoxedFuture<'a, PolicyResult> {
            Box::pin(async move {
                if event.kind.as_u16() == 447 {
                    PolicyResult::Reject("blocked: no files here".to_string())
                } else {
                    PolicyResult::Accept
                }
            })
        }
    }
    let relay = nostr_relay_builder::LocalRelay::new(RelayBuilder::default().write_policy(NoFiles));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(100);
    let path = write_tmp(&tmp, "blocked.bin", &bytes);
    let (manifest, _, _) = expect_of(&bytes);
    let sharer = GroupChannel::new(dialer(), vec![url], SEED);
    let started = std::time::Instant::now();
    let err = publish_series_v2(&sharer, &KEY, &path, &manifest, None)
        .await
        .expect_err("the verdict ends the series");
    assert!(err.to_string().contains("blocked"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(20), "no retry storm: {:?}", started.elapsed());
}

/// A transient publish failure - the relay unreachable for seconds, past
/// the pool breaker's first trips - is retried on a backoff, never fatal
/// for the series: the sharer's round is spent, so a dropped socket must
/// not waste it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_publish_failure_is_retried_not_fatal() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let direct = relay.url().await.to_string();
    let target = direct.trim_start_matches("ws://").to_string();
    let proxy = common::proxy::Cuttable::run(target).await;
    let proxied = format!("ws://127.0.0.1:{}", proxy.port);
    let tmp = tempfile::tempdir().expect("tmp");
    let bytes = pattern(PIECE_PAYLOAD_LEN + 5);
    let path = write_tmp(&tmp, "retried.bin", &bytes);
    let (manifest, expect, _) = expect_of(&bytes);

    let sharer = GroupChannel::new(dialer(), vec![proxied], SEED);
    proxy.cut().await;
    let proxy_ref = &proxy;
    let restore = async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        proxy_ref.restore();
    };
    let (published, ()) = tokio::join!(
        publish_series_v2(&sharer, &KEY, &path, &manifest, None),
        restore
    );
    let (start, _) = published.expect("the series completes once the relay is back");

    let fetcher = GroupChannel::new(dialer(), vec![direct], SEED);
    let mut got: Vec<u8> = Vec::new();
    fetch_series_v2(&fetcher, &KEY, start, &expect, &mut got, Some(Duration::from_secs(10)))
        .await
        .expect("every piece landed on the relay");
    assert_eq!(got, bytes);
}
