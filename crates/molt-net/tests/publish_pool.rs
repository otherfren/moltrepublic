// SPDX-License-Identifier: GPL-3.0-or-later
//! The PERSISTENT publish channel (live incident 2026-08-09 §3): every
//! kind-445 publish used to dial a fresh relay connection (Tor circuit +
//! WS + TLS ≈ 2 s each), so resend rounds against a deaf peer starved
//! fresh sends. The group channel now keeps one unauthenticated publish
//! connection per relay and redials only when it broke.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use molt_net::dial::Dialer;
use molt_net::ritual_net::GroupChannel;
use nostr_relay_builder::MockRelay;
use tokio::net::{TcpListener, TcpStream};

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

const SEED: [u8; 32] = [7u8; 32];
const EXPORTER: [u8; 32] = [9u8; 32];

/// A TCP proxy that COUNTS accepted connections and can be cut: the only
/// way to observe, from outside, whether a publish dialed fresh.
struct CountingProxy {
    port: u16,
    connections: Arc<AtomicUsize>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    forwards: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl CountingProxy {
    async fn run(target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().expect("addr").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let forwards: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let count = connections.clone();
        let on = enabled.clone();
        let fw = forwards.clone();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !on.load(Ordering::SeqCst) {
                    drop(inbound); // refuse while cut
                    continue;
                }
                count.fetch_add(1, Ordering::SeqCst);
                let target = target.clone();
                fw.lock().await.push(tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                }));
            }
        });
        Self { port, connections, enabled, forwards }
    }

    fn count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
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

/// Strip `ws://host:port` down to `host:port` (the proxy's TCP target).
fn tcp_target(ws_url: &str) -> String {
    ws_url.trim_start_matches("ws://").trim_end_matches('/').to_string()
}

/// Two publishes ride ONE connection — the §3 churn (a fresh Tor circuit
/// per frame) is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_group_channel_reuses_one_publish_connection() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let proxy = CountingProxy::run(tcp_target(&relay.url().await.to_string())).await;
    let url = format!("ws://127.0.0.1:{}", proxy.port);
    let chan = GroupChannel::new(dialer(), vec![url], SEED);

    chan.publish_frame(&EXPORTER, b"first").await.expect("publish 1");
    chan.publish_frame(&EXPORTER, b"second").await.expect("publish 2");
    chan.publish_frame(&EXPORTER, b"third").await.expect("publish 3");

    assert_eq!(
        proxy.count(),
        1,
        "three publishes must share one persistent connection"
    );
}

/// A broken publish connection redials on the next publish — the pool
/// heals itself without giving up the ≥1-OK contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_publish_connection_redials_on_the_next_publish() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let proxy = CountingProxy::run(tcp_target(&relay.url().await.to_string())).await;
    let url = format!("ws://127.0.0.1:{}", proxy.port);
    let chan = GroupChannel::new(dialer(), vec![url], SEED);

    chan.publish_frame(&EXPORTER, b"before the cut").await.expect("publish 1");
    proxy.cut().await;
    proxy.restore();
    chan.publish_frame(&EXPORTER, b"after the cut").await.expect("publish 2");

    assert_eq!(proxy.count(), 2, "the dead connection redialed exactly once");
}
