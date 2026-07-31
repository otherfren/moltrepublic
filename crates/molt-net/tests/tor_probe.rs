// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The Tor connectivity probe (Settings → Anonymity network → "Test Tor").
//!
//! The probe's ONLY job is to be honest about which rung of the evidence
//! ladder it actually reached — a green light that means "a socket answered"
//! is worse than no light at all. The ladder, weakest first:
//!
//! | rung            | what it PROVES                                        |
//! |-----------------|-------------------------------------------------------|
//! | `off`           | nothing — Tor is not enabled, no packet was sent        |
//! | `misconfigured` | nothing — the fail-closed dialer refused to resolve     |
//! | `no_proxy`      | nothing is listening at the configured SOCKS address    |
//! | `proxy_only`    | a socket answers there — no traffic was routed through it |
//! | `circuit_failed`| the proxy answers but the dial through it failed        |
//! | `circuit`       | a real relay was reached END TO END through Tor         |
//!
//! No real network: every rung is exercised against loopback listeners
//! (`tor_no_leak.rs` posture) — a dead port, a blackhole listener, and a
//! fake SOCKS5 proxy that really completes the RFC 1928/1929 handshake.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use molt_core::relay::RelayEntry;
use molt_core::TorTestState;
use molt_net::dial::Dialer;
use molt_net::tor_probe;

/// A confirmed clearnet relay (the only kind that can prove a circuit — a
/// LOCAL relay is dialed directly and never touches Tor).
const RELAY: &str = "wss://relay.example.test";

fn entry(url: &str, confirmed: bool) -> RelayEntry {
    RelayEntry {
        url: url.to_string(),
        confirmed,
    }
}

/// A port nothing listens on: bind, read the port, drop the listener.
async fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    l.local_addr().expect("addr").port()
}

/// Accept-and-close listener with a hit counter (the `tor_no_leak` helper).
async fn blackhole_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind blackhole");
    let port = listener.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, hits)
}

/// A listener that accepts and holds every connection open forever without
/// ever answering — the "something is listening but says nothing" case.
async fn silent_listener() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind silent");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    port
}

/// A fake SOCKS5 proxy that really performs the RFC 1928 method negotiation
/// and the RFC 1929 username/password sub-negotiation, then answers the
/// CONNECT with `reply_code` (0x00 = success). Returns its port and the host
/// it was asked to reach.
async fn fake_socks5(reply_code: u8) -> (u16, Arc<tokio::sync::Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake socks");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let record = record.clone();
            tokio::spawn(async move {
                // greeting: VER NMETHODS METHODS…
                let mut head = [0u8; 2];
                if s.read_exact(&mut head).await.is_err() {
                    return;
                }
                let mut methods = vec![0u8; usize::from(head[1])];
                if s.read_exact(&mut methods).await.is_err() {
                    return;
                }
                if s.write_all(&[0x05, 0x02]).await.is_err() {
                    return;
                }
                // RFC 1929: VER ULEN USER PLEN PASS
                let mut ah = [0u8; 2];
                if s.read_exact(&mut ah).await.is_err() {
                    return;
                }
                let mut user = vec![0u8; usize::from(ah[1])];
                if s.read_exact(&mut user).await.is_err() {
                    return;
                }
                let mut pl = [0u8; 1];
                if s.read_exact(&mut pl).await.is_err() {
                    return;
                }
                let mut pass = vec![0u8; usize::from(pl[0])];
                if s.read_exact(&mut pass).await.is_err() {
                    return;
                }
                if s.write_all(&[0x01, 0x00]).await.is_err() {
                    return;
                }
                // CONNECT: VER CMD RSV ATYP …
                let mut ch = [0u8; 4];
                if s.read_exact(&mut ch).await.is_err() {
                    return;
                }
                let mut hl = [0u8; 1];
                if s.read_exact(&mut hl).await.is_err() {
                    return;
                }
                let mut host_port = vec![0u8; usize::from(hl[0]) + 2];
                if s.read_exact(&mut host_port).await.is_err() {
                    return;
                }
                *record.lock().await = Some(
                    String::from_utf8_lossy(&host_port[..usize::from(hl[0])]).into_owned(),
                );
                let _ = s
                    .write_all(&[0x05, reply_code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        }
    });
    (port, seen)
}

/// KEYSTONE — the pure ladder. `verdict` never claims more than the rungs
/// that actually ran reported; every combination maps to exactly one rung.
#[test]
fn the_verdict_claims_only_what_the_rungs_proved() {
    // proxy rung failed → no daemon there. The circuit rung never ran.
    let v = tor_probe::verdict(&tor_probe::RungReport {
        proxy: "127.0.0.1:9050".to_string(),
        proxy_answered: Some(Err("connection refused".to_string())),
        ..Default::default()
    });
    assert_eq!(v.state, TorTestState::NoProxy);
    assert_eq!(v.proxy, "127.0.0.1:9050");
    assert!(v.detail.contains("connection refused"), "{v:?}");
    assert!(v.target.is_empty(), "nothing was dialed: {v:?}");

    // proxy answered, but there was nothing to route through.
    let v = tor_probe::verdict(&tor_probe::RungReport {
        proxy: "127.0.0.1:9050".to_string(),
        proxy_answered: Some(Ok(())),
        ..Default::default()
    });
    assert_eq!(v.state, TorTestState::ProxyOnly);
    assert!(v.target.is_empty());

    // proxy answered and the dial through it failed.
    let v = tor_probe::verdict(&tor_probe::RungReport {
        proxy: "127.0.0.1:9050".to_string(),
        proxy_answered: Some(Ok(())),
        target: RELAY.to_string(),
        circuit: Some(Err("host unreachable".to_string())),
    });
    assert_eq!(v.state, TorTestState::CircuitFailed);
    assert_eq!(v.target, RELAY);
    assert!(v.detail.contains("host unreachable"), "{v:?}");

    // the full end-to-end proof.
    let v = tor_probe::verdict(&tor_probe::RungReport {
        proxy: "127.0.0.1:9050".to_string(),
        proxy_answered: Some(Ok(())),
        target: RELAY.to_string(),
        circuit: Some(Ok(1234)),
    });
    assert_eq!(v.state, TorTestState::Circuit);
    assert_eq!(v.ms, 1234);
    assert_eq!(v.target, RELAY);

    // no proxy rung at all (embedded arti) and nothing to dial: NOTHING was
    // tested — never dressed up as "a proxy answered".
    let v = tor_probe::verdict(&tor_probe::RungReport::default());
    assert_eq!(v.state, TorTestState::NoTarget);
    assert!(v.proxy.is_empty());
}

/// The weakest real rung: nothing is listening at the configured SOCKS port,
/// so there is no Tor daemon there.
#[tokio::test]
async fn a_dead_socks_port_is_the_no_daemon_rung() {
    let port = dead_port().await;
    let dialer = Dialer::resolve("tor", "local", port).expect("tor+local");
    let v = tor_probe::probe(&dialer, Some(RELAY)).await;
    assert_eq!(v.state, TorTestState::NoProxy, "{v:?}");
    assert_eq!(v.proxy, format!("127.0.0.1:{port}"));
    assert!(
        v.target.is_empty(),
        "no relay was dialed once the proxy rung failed: {v:?}"
    );
}

/// The partial rung, stated honestly: a socket answers at the SOCKS address,
/// but with no dialable relay nothing was routed through it — so no circuit
/// is proven. ADR-0004: the probe never invents a host to dial.
#[tokio::test]
async fn a_listening_proxy_without_a_relay_stops_at_the_partial_rung() {
    let (port, hits) = blackhole_listener().await;
    let dialer = Dialer::resolve("tor", "local", port).expect("tor+local");
    let v = tor_probe::probe(&dialer, None).await;
    assert_eq!(v.state, TorTestState::ProxyOnly, "{v:?}");
    assert_eq!(v.proxy, format!("127.0.0.1:{port}"));
    assert!(v.target.is_empty(), "{v:?}");
    // let the accept loop register the connection the probe really opened
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        hits.load(Ordering::SeqCst) >= 1,
        "the proxy rung really connected"
    );
}

/// The strongest rung: a relay dialed THROUGH the SOCKS proxy, end to end.
/// The relay host is a name that resolves nowhere — SOCKS5h keeps it
/// proxy-side, which is exactly what a real circuit does.
#[tokio::test]
async fn a_relay_reached_through_tor_proves_a_circuit() {
    let (port, seen) = fake_socks5(0x00).await;
    let dialer = Dialer::resolve("tor", "local", port).expect("tor+local");
    let v = tor_probe::probe(&dialer, Some(RELAY)).await;
    assert_eq!(v.state, TorTestState::Circuit, "{v:?}");
    assert_eq!(v.target, RELAY);
    assert_eq!(
        seen.lock().await.clone(),
        Some("relay.example.test".to_string()),
        "the relay host went proxy-side (SOCKS5h), never through local DNS"
    );
}

/// A proxy that speaks SOCKS5 but cannot build the circuit is NOT a working
/// Tor — the verdict must say so instead of stopping at "the proxy answered".
#[tokio::test]
async fn a_socks_refusal_is_reported_as_a_failed_circuit() {
    let (port, _seen) = fake_socks5(0x04).await; // host unreachable
    let dialer = Dialer::resolve("tor", "local", port).expect("tor+local");
    let v = tor_probe::probe(&dialer, Some(RELAY)).await;
    assert_eq!(v.state, TorTestState::CircuitFailed, "{v:?}");
    assert_eq!(v.target, RELAY);
    assert!(!v.detail.is_empty(), "the failure names a real reason: {v:?}");
}

/// Fail-closed: without Tor the probe sends NOTHING. A `Direct` dialer must
/// never make the probe dial the relay on the clearnet.
#[tokio::test]
async fn without_tor_the_probe_refuses_and_dials_nothing() {
    let (relay_port, relay_hits) = blackhole_listener().await;
    let url = format!("ws://127.0.0.1:{relay_port}");
    let dialer = Dialer::resolve("none", "local", 9050).expect("direct");
    let v = tor_probe::probe(&dialer, Some(&url)).await;
    assert_eq!(v.state, TorTestState::Off, "{v:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        relay_hits.load(Ordering::SeqCst),
        0,
        "the Tor test must not open a clearnet connection when Tor is off"
    );
}

/// Target selection is the relay pool's own policy, plus one extra rule: a
/// LOCAL relay is reached DIRECTLY and never through Tor, so it can never
/// prove a circuit and is never the probe's target.
#[test]
fn the_target_is_a_confirmed_tor_routable_relay_or_nothing() {
    let onion = format!("wss://{}.onion", "a".repeat(56));
    // a fresh install has nothing to probe with
    assert_eq!(tor_probe::probe_target(&[], false), None);
    // unconfirmed relays are not dialable, so they are not probe targets
    assert_eq!(
        tor_probe::probe_target(&[entry(RELAY, false), entry(&onion, false)], true),
        None
    );
    // a local relay is dialed directly — it can never prove a Tor circuit
    assert_eq!(
        tor_probe::probe_target(&[entry("ws://192.168.1.5:7777", true)], true),
        None
    );
    // clearnet needs the session unlock, exactly like every other dial
    assert_eq!(tor_probe::probe_target(&[entry(RELAY, true)], false), None);
    assert_eq!(
        tor_probe::probe_target(&[entry(RELAY, true)], true),
        Some(RELAY.to_string())
    );
    // an onion relay needs no unlock, and pool ORDER is the priority
    assert_eq!(
        tor_probe::probe_target(&[entry(&onion, true)], false),
        Some(onion.clone())
    );
    assert_eq!(
        tor_probe::probe_target(&[entry("ws://127.0.0.1:7777", true), entry(&onion, true)], true),
        Some(onion),
        "the local relay is skipped, the next dialable one wins"
    );
}

/// A hung proxy must not hang the probe: both rungs are deadline-bounded, so
/// the verdict always arrives.
#[tokio::test(start_paused = true)]
async fn every_rung_is_deadline_bounded() {
    // a listener that accepts and then says nothing at all: the TCP rung
    // succeeds, the SOCKS negotiation never completes.
    let port = silent_listener().await;
    let dialer = Dialer::resolve("tor", "local", port).expect("tor+local");
    let started = tokio::time::Instant::now();
    let v = tor_probe::probe(&dialer, Some(RELAY)).await;
    let elapsed = started.elapsed();
    assert_eq!(v.state, TorTestState::CircuitFailed, "{v:?}");
    assert!(
        elapsed <= tor_probe::PROXY_TIMEOUT + tor_probe::CIRCUIT_TIMEOUT,
        "the probe ran {elapsed:?}, past its own deadlines"
    );
}
