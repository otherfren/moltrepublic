// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! T4 Tor — deterministic egress no-leak harness (plan §4 B4, concept §7).
//!
//! The strongest T4 guarantee is *"a Tor-configured node makes zero direct
//! dials."* The nightly/manual CI tier proves it with an OS-level egress
//! firewall (see `crates/molt-engine/tests/tor_e2e.rs`); **this file is the
//! automatable proxy** for that claim — it runs in the default `cargo test`
//! with no network and no privileges, yet still proves both halves:
//!
//! 1. **Routing is fail-closed.** [`Dialer::resolve`] under `network = tor`
//!    never yields [`Dialer::Direct`] for *any* mode (incl. `embedded` without
//!    the feature → `TorMisconfigured`, and `nym` → error). Exactly one
//!    clearnet path exists — `network = none` — and only that.
//! 2. **Egress targets the proxy, never the server.** A Tor-configured dialer
//!    (and the `SmpTransport` built on it), pointed at a *blackhole* SOCKS
//!    proxy — a local `TcpListener` that accepts then closes — and given a
//!    server whose "host" is a *second* bound loopback port, connects to the
//!    **proxy** and never to the server port. A direct-dial leak would land on
//!    the server listener; SOCKS5h sends the host proxy-side (`DOMAINNAME`), so
//!    it never does. This is the "no direct egress" proof without an OS
//!    firewall: the proxy is the only socket the tor path is allowed to open.
//!
//! Run: `cargo test -p molt-net --test tor_no_leak`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use molt_net::smp::tls::Dialer;
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::{NetError, Transport};

/// A valid SMP fingerprint (konkin's) — only its 32-byte decode is exercised;
/// no server is ever actually reached in these deterministic tests.
const FP: &str = "f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=";

/// Bind a loopback `TcpListener` that counts and immediately closes every
/// connection it accepts (a "blackhole"). Returns its port and the live hit
/// counter. The accept loop is detached; it ends when the test runtime stops.
async fn blackhole_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole listener");
    let port = listener.local_addr().expect("listener addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream); // accept then close — a blackhole proxy/server
        }
    });
    (port, hits)
}

/// Deterministic, no-network: the routing decision is fail-closed — under
/// `network = tor` **no** mode ever resolves to `Direct`, and only
/// `network = none` is clearnet.
#[test]
fn resolve_under_network_tor_never_yields_direct() {
    // the one clearnet path.
    assert!(matches!(
        Dialer::resolve("none", "local", 9050).expect("none is clearnet"),
        Dialer::Direct
    ));

    // network=tor, EVERY mode: either a fail-closed error, or a non-Direct
    // dialer — never a silent clearnet fallback.
    for mode in ["local", "whonix", "embedded", "", "nonsense", "nym"] {
        match Dialer::resolve("tor", mode, 9050) {
            Ok(dialer) => assert!(
                !matches!(dialer, Dialer::Direct),
                "network=tor mode={mode} resolved to Direct — a clearnet leak"
            ),
            Err(NetError::TorMisconfigured(_)) => {}
            other => panic!("network=tor mode={mode}: unexpected {other:?}"),
        }
    }

    // embedded WITHOUT the feature is a clean config error, never Direct.
    #[cfg(not(feature = "embedded-tor"))]
    assert!(matches!(
        Dialer::resolve("tor", "embedded", 9050),
        Err(NetError::TorMisconfigured(_))
    ));

    // nym and any unknown network fail closed too (defence in depth).
    assert!(matches!(
        Dialer::resolve("nym", "local", 9050),
        Err(NetError::TorMisconfigured(_))
    ));
    assert!(matches!(
        Dialer::resolve("bogus", "local", 9050),
        Err(NetError::TorMisconfigured(_))
    ));
}

/// The raw chokepoint ([`Dialer::dial`], "the ONE place a TCP socket to an SMP
/// server opens"): a Tor-configured dial connects to the SOCKS proxy and never
/// to the server host:port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tor_dialer_targets_the_socks_proxy_never_the_server() {
    let (proxy_port, proxy_hits) = blackhole_listener().await;
    let (server_port, server_hits) = blackhole_listener().await;

    // resolved exactly as the app resolves it: tor + local → SOCKS 127.0.0.1:port
    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");
    assert!(!matches!(dialer, Dialer::Direct), "tor must not be Direct");

    // the "server" host is a bound loopback port: a direct-dial LEAK would land
    // here. Under SOCKS5h the host is only sent proxy-side, so it must not.
    let server = SmpServer::parse(&format!("smp://{FP}@127.0.0.1:{server_port}"))
        .expect("server url parses");

    // dial errors at the blackhole proxy; we assert on *where the bytes went*.
    let res = dialer.dial(&server).await;
    assert!(res.is_err(), "the blackhole proxy cannot complete a dial: {res:?}");

    // let both accept loops register any connection (a leak included).
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        proxy_hits.load(Ordering::SeqCst) >= 1,
        "the Tor dialer must reach the SOCKS proxy"
    );
    assert_eq!(
        server_hits.load(Ordering::SeqCst),
        0,
        "NO-LEAK VIOLATION: a Tor-configured dial reached the server host:port directly"
    );
}

/// The same proof one layer up: a whole [`SmpTransport`] built with a Tor
/// dialer routes its egress (here `create_queue` → `SmpConn::connect` →
/// `connect_tls` → `Dialer::dial`) through the proxy, never direct to the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tor_transport_routes_all_egress_through_the_proxy() {
    let (proxy_port, proxy_hits) = blackhole_listener().await;
    let (server_port, server_hits) = blackhole_listener().await;

    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");
    let server = SmpServer::parse(&format!("smp://{FP}@127.0.0.1:{server_port}"))
        .expect("server url parses");
    let transport = SmpTransport::with_dialer(server, dialer);

    // create_queue is the first thing the ritual does on the wire; it errors at
    // the blackhole proxy, but must have targeted the proxy, not the server.
    let res = transport.create_queue().await;
    assert!(res.is_err(), "blackhole proxy cannot complete a queue: {res:?}");

    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        proxy_hits.load(Ordering::SeqCst) >= 1,
        "the Tor transport must dial through the SOCKS proxy"
    );
    assert_eq!(
        server_hits.load(Ordering::SeqCst),
        0,
        "NO-LEAK VIOLATION: the Tor transport reached the server host:port directly"
    );
}
