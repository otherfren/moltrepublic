// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F2 keystones — the file data plane over relays**
//! (`docs/transport/file_transfer_nostr.md`): a chunk series published
//! once is fetched and verified by a member that shares nothing with the
//! sharer but the group's rotation seed and exporter ring — and every
//! refusal (cap, checksum, absent series) is honest, never silence.

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::file_plane::{fetch_series, publish_series, FILE_CAP_DEFAULT_BYTES};
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
        publish_series(&sharer, &EXPORTER, "aa01", &bytes, FILE_CAP_DEFAULT_BYTES)
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

    let (stamp_a, _) = publish_series(&chan, &EXPORTER, "aa02", &a, FILE_CAP_DEFAULT_BYTES)
        .await
        .expect("a publishes");
    let (stamp_b, _) = publish_series(&chan, &EXPORTER, "bb02", &b, FILE_CAP_DEFAULT_BYTES)
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
        publish_series(&chan, &EXPORTER, "cc03", &big, 1023).await.is_err(),
        "the cap refuses at publish time"
    );

    // published bytes that do not match the claimed checksum: refused
    let bytes = pattern(10 * 1024);
    let (stamp, _) = publish_series(&chan, &EXPORTER, "dd04", &bytes, FILE_CAP_DEFAULT_BYTES)
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
