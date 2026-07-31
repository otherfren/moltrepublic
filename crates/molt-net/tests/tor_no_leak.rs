// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! T4 Tor — deterministic egress no-leak harness (plan §4 B4, concept §7).
//!
//! The strongest T4 guarantee is *"a Tor-configured node makes zero direct
//! dials."* The OS-level egress-firewall harness that proved it end-to-end
//! returns with N2's WebSocket twin; **this file is the automatable proxy**
//! for that claim — it runs in the default `cargo test` with no network and
//! no privileges, yet still proves both halves:
//!
//! 1. **Routing is fail-closed.** [`Dialer::resolve`] under `network = tor`
//!    never yields [`Dialer::Direct`] for *any* mode (incl. `embedded` without
//!    the feature → `TorMisconfigured`, and `nym` → error). Exactly one
//!    clearnet path exists — `network = none` — and only that.
//! 2. **Egress targets the proxy, never the server.** A Tor-configured dialer,
//!    pointed at a *blackhole* SOCKS proxy — a local `TcpListener` that
//!    accepts then closes — and given a "server" host that is a *second*
//!    bound loopback port, connects to the **proxy** and never to the server
//!    port. A direct-dial leak would land on the server listener; SOCKS5h
//!    sends the host proxy-side (`DOMAINNAME`), so it never does. This is the
//!    "no direct egress" proof without an OS firewall: the proxy is the only
//!    socket the tor path is allowed to open.
//!
//! Run: `cargo test -p molt-net --test tor_no_leak`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use molt_net::dial::Dialer;
use molt_net::NetError;

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

/// The raw chokepoint ([`Dialer::dial_host`], the ONE place a TCP socket to a
/// remote host opens): a Tor-configured dial connects to the SOCKS proxy and
/// never to the server host:port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tor_dialer_targets_the_socks_proxy_never_the_server() {
    let (proxy_port, proxy_hits) = blackhole_listener().await;
    let (server_port, server_hits) = blackhole_listener().await;

    // resolved exactly as the app resolves it: tor + local → SOCKS 127.0.0.1:port
    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");
    assert!(!matches!(dialer, Dialer::Direct), "tor must not be Direct");

    // the "server" host is a bound loopback port: a direct-dial LEAK would land
    // here. Under SOCKS5h the host is only sent proxy-side, so it must not.
    // dial errors at the blackhole proxy; we assert on *where the bytes went*.
    let res = dialer.dial_host("127.0.0.1", server_port).await;
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

/// KEYSTONE — the SOCKS handshake carries the PER-HOST stream-isolation
/// credential (Tor's `IsolateSOCKSAuth`): username `molt-<session>-<host>`,
/// non-empty password, and `DOMAINNAME` addressing (proxy-side DNS). Pinned
/// across SOCKS client implementations (hand-rolled, then `tokio-socks` —
/// `mdk_evaluation.md` §7.7): losing the credential would silently put every
/// remote host on ONE Tor circuit, a fingerprinting regression no other test
/// would notice.
#[tokio::test]
async fn socks_handshake_carries_the_per_host_isolation_username() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake socks");
    let proxy_port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(tokio::sync::Mutex::new(None::<(String, usize)>));
    let seen_srv = seen.clone();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        // method negotiation: VER NMETHODS METHODS…
        let mut head = [0u8; 2];
        s.read_exact(&mut head).await.expect("greeting head");
        assert_eq!(head[0], 0x05, "SOCKS5");
        let mut methods = vec![0u8; usize::from(head[1])];
        s.read_exact(&mut methods).await.expect("methods");
        assert!(methods.contains(&0x02), "userpass must be offered: {methods:?}");
        s.write_all(&[0x05, 0x02]).await.expect("select userpass");
        // RFC 1929: VER ULEN USER PLEN PASS
        let mut ah = [0u8; 2];
        s.read_exact(&mut ah).await.expect("auth head");
        assert_eq!(ah[0], 0x01, "auth sub-negotiation version");
        let mut user = vec![0u8; usize::from(ah[1])];
        s.read_exact(&mut user).await.expect("username");
        let mut pl = [0u8; 1];
        s.read_exact(&mut pl).await.expect("plen");
        let mut pass = vec![0u8; usize::from(pl[0])];
        s.read_exact(&mut pass).await.expect("password");
        *seen_srv.lock().await =
            Some((String::from_utf8_lossy(&user).into_owned(), pass.len()));
        s.write_all(&[0x01, 0x00]).await.expect("auth ok");
        // CONNECT: VER CMD RSV ATYP …
        let mut ch = [0u8; 4];
        s.read_exact(&mut ch).await.expect("connect head");
        assert_eq!(ch[1], 0x01, "CONNECT");
        assert_eq!(ch[3], 0x03, "DOMAINNAME addressing — DNS stays proxy-side");
        let mut hl = [0u8; 1];
        s.read_exact(&mut hl).await.expect("host len");
        let mut host_port = vec![0u8; usize::from(hl[0]) + 2];
        s.read_exact(&mut host_port).await.expect("host+port");
        assert_eq!(&host_port[..usize::from(hl[0])], b"relay.example.test");
        // success reply, bound 0.0.0.0:0 — then keep the tunnel open briefly
        s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .expect("connect reply");
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");
    let res = dialer.dial_host("relay.example.test", 443).await;
    assert!(res.is_ok(), "a fully negotiated SOCKS dial succeeds: {res:?}");
    let (user, pass_len) = seen.lock().await.clone().expect("the proxy saw the auth frame");
    assert!(
        user.starts_with("molt-") && user.ends_with("-relay.example.test"),
        "isolation credential must be per-host: {user:?}"
    );
    assert!(pass_len > 0, "RFC 1929 servers may reject an empty password");
}

/// N2 step 8 KEYSTONE — the WS twin of the T4 no-leak harness: a
/// Tor-configured dial of a CLEARNET relay reaches only the SOCKS proxy,
/// for both schemes (TLS is layered above a dialed stream and never opens a
/// socket of its own — otherwise a second connection would appear here).
/// The relay host is a name, so a direct-dial regression would resolve it
/// locally: the proxy counter is the positive proof, and SOCKS5h keeps the
/// name off this machine's resolver.
#[tokio::test]
async fn relay_ws_under_tor_targets_the_socks_proxy_never_the_relay() {
    let (proxy_port, proxy_hits) = blackhole_listener().await;
    let (leak_port, leak_hits) = blackhole_listener().await;
    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");

    for scheme in ["ws", "wss"] {
        let url = format!("{scheme}://relay.example.test:{leak_port}");
        let res = molt_net::relay_ws::RelayWs::connect(&dialer, &url).await;
        assert!(res.is_err(), "the blackhole proxy cannot complete {url}");
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        proxy_hits.load(Ordering::SeqCst) >= 2,
        "both relay dials must reach the SOCKS proxy"
    );
    assert_eq!(
        leak_hits.load(Ordering::SeqCst),
        0,
        "NO-LEAK VIOLATION: a Tor-configured relay dial reached the relay directly"
    );
}

/// The deliberate counterpart (§10.14, review finding): a LOCAL relay is
/// dialed DIRECTLY even with Tor configured — a Tor proxy refuses private
/// addresses, and routing one through it would disclose the local address
/// while never connecting. That is exactly why a local relay sits behind
/// the same explicit per-session gate as clearnet.
#[tokio::test]
async fn a_local_relay_is_dialed_directly_even_under_tor() {
    let (proxy_port, proxy_hits) = blackhole_listener().await;
    let (relay_port, relay_hits) = blackhole_listener().await;
    let dialer = Dialer::resolve("tor", "local", proxy_port).expect("tor+local dialer");

    let url = format!("ws://127.0.0.1:{relay_port}");
    let res = molt_net::relay_ws::RelayWs::connect(&dialer, &url).await;
    assert!(res.is_err(), "the blackhole relay cannot complete the upgrade");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        relay_hits.load(Ordering::SeqCst) >= 1,
        "a local relay must be reached directly"
    );
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "…and must not be pushed through the Tor proxy, which would refuse it"
    );
}

/// …and the other fail-closed edge: an onion relay with Tor OFF is refused
/// outright — never a clearnet dial, never a DNS leak.
#[tokio::test]
async fn an_onion_relay_without_tor_is_refused() {
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let onion = format!("ws://{}.onion", "a".repeat(56));
    let res = molt_net::relay_ws::RelayWs::connect(&dialer, &onion).await;
    let err = match res {
        Err(e) => format!("{e}"),
        Ok(_) => panic!("an onion dial without Tor must fail closed"),
    };
    assert!(err.contains("Tor"), "the refusal names the reason: {err}");
}
