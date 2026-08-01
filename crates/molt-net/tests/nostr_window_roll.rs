// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Cluster E — the §4.4 window roll under a relay that is not there.
//!
//! OWN TEST BINARY on purpose: `shift_window_clock_for_tests` is a
//! process-global seam, so it would leak into every other test sharing a
//! binary.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::envelope::H_WINDOW;
use molt_net::ritual_net::{shift_window_clock_for_tests, GroupChannel, GroupRecv};
use nostr_relay_builder::MockRelay;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

/// A cuttable TCP proxy that also COUNTS accepts — the count is how we prove
/// the retry is backoff-gated rather than a busy-spin.
struct Cuttable {
    port: u16,
    enabled: Arc<AtomicBool>,
    accepts: Arc<AtomicUsize>,
    forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Cuttable {
    async fn run(target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().expect("addr").port();
        let enabled = Arc::new(AtomicBool::new(true));
        let accepts = Arc::new(AtomicUsize::new(0));
        let forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let on = enabled.clone();
        let n = accepts.clone();
        let fw = forwards.clone();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                n.fetch_add(1, Ordering::SeqCst);
                if !on.load(Ordering::SeqCst) {
                    drop(inbound); // refuse while cut
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
        Self { port, enabled, accepts, forwards }
    }

    async fn cut(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        for f in self.forwards.lock().await.drain(..) {
            f.abort();
        }
    }

    fn restore(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }
}

/// KEYSTONE (cluster E) — a failed window-roll resubscribe is reported DEAF,
/// retried on a backoff, and heals.
///
/// `recv` returned `None` on a failed re-placement, which every caller reads
/// as "idle" — so a node went PERMANENTLY DEAF at a UTC midnight boundary
/// while looking perfectly healthy, and (because the caller loops straight
/// back) burned CPU doing it. Three properties, all previously absent:
/// deafness is distinguishable from quiet, the retry is rate-limited, and a
/// healed relay is heard again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_window_roll_is_reported_deaf_and_retried_on_a_backoff() {
    let relay = MockRelay::run().await.expect("relay");
    let direct = relay.url().await.to_string();
    let target = direct.trim_start_matches("ws://").to_string();
    let proxy = Cuttable::run(target).await;
    let url = format!("ws://127.0.0.1:{}", proxy.port);

    let seed = [5u8; 32];
    let chan = GroupChannel::new(dialer(), vec![url], seed);
    let mut sub = chan.subscribe().await.expect("subscribe");
    assert!(sub.live(Duration::from_secs(5)).await, "the initial REQ replayed");

    // the relay goes away, and the clock crosses a window boundary
    proxy.cut().await;
    shift_window_clock_for_tests(i64::try_from(H_WINDOW).expect("window fits"));

    // (a) DEAF, never Idle — the whole point
    let got = sub.recv(Duration::from_secs(3)).await;
    match &got {
        GroupRecv::Deaf(why) => assert!(!why.is_empty(), "the reason is carried"),
        other => panic!("a failed roll must report Deaf, got {other:?}"),
    }

    // (b) the retry is BACKOFF-gated, not per-iteration. Without the gate the
    // resubscribe fails in ~1 ms and the caller loops instantly, so a few
    // seconds means hundreds of connection attempts.
    let before = proxy.accepts.load(Ordering::SeqCst);
    let until = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < until {
        let _ = sub.recv(Duration::from_millis(500)).await;
    }
    let attempts = proxy.accepts.load(Ordering::SeqCst) - before;
    assert!(
        attempts <= 12,
        "the retry must be rate-limited, saw {attempts} connection attempts in 6 s"
    );

    // (c) it HEALS: the same seed under the NEW window reaches us again
    proxy.restore();
    let publisher = GroupChannel::new(dialer(), vec![direct], seed);
    publisher
        .publish_frame(&[9u8; 32], b"after the roll")
        .await
        .expect("publish under the new window");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut healed = false;
    while tokio::time::Instant::now() < deadline {
        if let GroupRecv::Frame { .. } = sub.recv(Duration::from_secs(2)).await {
            healed = true;
            break;
        }
    }
    assert!(healed, "the channel must be heard again once the relay returns");

    shift_window_clock_for_tests(0);
}
