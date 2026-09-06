// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
#![cfg(feature = "embedded-tor")]

//! **Embedded Tor (arti), live** - the one T4 path no offline test can prove:
//! the in-process client bootstraps the Tor directory and a relay WebSocket
//! handshake completes through it, on the exact path the transport uses
//! (`Dialer::resolve("tor", "embedded", _)` + `RelayWs::connect`).
//!
//! `#[ignore]`d like every real-network twin: it needs the public internet
//! and a few minutes for a cold bootstrap. Run it with
//! `cargo test -p molt-net --features embedded-tor --test tor_embedded_smoke -- --ignored --nocapture`.
//! `MOLT_PROBE_RELAY` names the clearnet relay (default `wss://relay.damus.io`);
//! `MOLT_PROBE_ONION` names an optional onion relay to dial as well - never
//! written into this file, an onion address is the operator's.

use std::time::{Duration, Instant};

use molt_net::dial::Dialer;
use molt_net::relay_ws::RelayWs;

const BOOTSTRAP_AND_DIAL_MAX: Duration = Duration::from_secs(600);

/// Bootstrap the embedded client (first dial) and complete a relay
/// handshake through it; a second dial must reuse the bootstrapped client
/// and finish in seconds, not minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live tor: bootstraps arti over the public internet"]
async fn the_embedded_client_bootstraps_and_reaches_a_relay() {
    let url = std::env::var("MOLT_PROBE_RELAY").unwrap_or_else(|_| "wss://relay.damus.io".to_string());
    let dialer = Dialer::resolve("tor", "embedded", 0).expect("the embedded dialer resolves under the feature");
    assert!(
        !matches!(dialer, Dialer::Direct),
        "embedded must never resolve to a direct dial"
    );

    let cold = Instant::now();
    let first = tokio::time::timeout(BOOTSTRAP_AND_DIAL_MAX, RelayWs::connect(&dialer, &url))
        .await
        .expect("bootstrap + first dial within the deadline");
    let cold = cold.elapsed();
    let ws = first.unwrap_or_else(|e| panic!("first dial through embedded tor failed after {cold:?}: {e}"));
    ws.close().await;
    println!("REACHABLE {url} through embedded arti - cold bootstrap + handshake {cold:?}");

    let warm = Instant::now();
    let second = tokio::time::timeout(Duration::from_secs(120), RelayWs::connect(&dialer, &url))
        .await
        .expect("second dial within two minutes");
    let warm = warm.elapsed();
    let ws = second.unwrap_or_else(|e| panic!("second dial failed after {warm:?}: {e}"));
    ws.close().await;
    println!("REACHABLE again - warm dial {warm:?}");
    assert!(warm < cold, "the second dial must reuse the bootstrapped client ({warm:?} vs {cold:?})");

    if let Ok(onion) = std::env::var("MOLT_PROBE_ONION") {
        let at = Instant::now();
        let hs = tokio::time::timeout(Duration::from_secs(300), RelayWs::connect(&dialer, &onion)).await;
        match hs {
            Ok(Ok(ws)) => {
                ws.close().await;
                println!("REACHABLE onion relay through embedded arti - {:?}", at.elapsed());
            }
            Ok(Err(e)) => panic!("onion relay unreachable through embedded arti after {:?}: {e}", at.elapsed()),
            Err(_) => panic!("onion relay dial timed out after {:?}", at.elapsed()),
        }
    }
}
