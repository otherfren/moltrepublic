// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Live SMP handshake + framing checks (transport concept §3.2), written
//! against the real server's bytes. `#[ignore]` (real network):
//!
//! ```text
//! cargo test -p molt-net --test smp_handshake -- --ignored --nocapture
//! ```

use molt_net::smp::{SmpConn, SmpServer};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

/// Checkpoint 2: TLS pin → SMP handshake → unsigned PING → PONG. Proves
/// the block framing (word16 length + content + '#' pad), the
/// clientHello (version + keyHash echo), and the transmission encoding
/// end to end, with no queue crypto.
#[tokio::test]
#[ignore = "makes a real connection to smp.konkin.io"]
async fn handshake_then_ping_pong() {
    let s = SmpServer::parse(KONKIN).expect("parse");
    let mut conn = SmpConn::connect(&s).await.expect("handshake");
    assert_eq!(conn.session_id.len(), 32, "32-byte session id");
    assert!(conn.version >= 7, "negotiated a modern SMP version");
    println!("handshaked v{} with {}", conn.version, s.host);
    conn.ping().await.expect("PING → PONG");
    println!("OK: PING → PONG against {}", s.host);
}
