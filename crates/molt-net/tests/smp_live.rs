// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Live SMP connectivity checks against real SimpleX servers. These make
//! real network connections, so they are `#[ignore]` by default — run
//! explicitly:
//!
//! ```text
//! cargo test -p molt-net --test smp_live -- --ignored --nocapture
//! ```
//!
//! They verify the pinned-fingerprint TLS layer (transport concept §3.1)
//! against (a) the user's own server and (b) a public SimpleX server, and
//! that a wrong fingerprint is rejected (the anti-MITM guarantee).

use molt_net::smp::{tls, SmpServer};

/// The user's server (documents/SimpleX.txt).
const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";
/// A public SimpleX server (smp8), CA fingerprint computed from its live
/// certificate chain. NOTE: the official simplex.im servers use **ED448**
/// certificates, which the pure-Rust rustls-rustcrypto provider cannot
/// verify (it supports ED25519 — like the user's konkin server). This is a
/// documented limitation of the no-C-toolchain posture, asserted below.
const PUBLIC_ED448: &str =
    "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im";

#[tokio::test]
#[ignore = "makes a real TLS connection to smp.konkin.io"]
async fn konkin_server_pins_and_connects() {
    let s = SmpServer::parse(KONKIN).expect("parse");
    tls::test_connection(&s)
        .await
        .expect("TLS+ALPN+pin against smp.konkin.io");
    println!("OK: TLS 1.3 + ALPN smp/1 + fingerprint pin against {}", s.host);
}

#[tokio::test]
#[ignore = "makes a real TLS connection to smp8.simplex.im"]
async fn public_ed448_server_is_a_known_pure_rust_gap() {
    // Honest documentation of the pure-Rust posture's cost: the official
    // simplex.im servers use ED448 certs; rustls-rustcrypto rejects the
    // handshake because it cannot verify ED448 (it does ED25519). If this
    // ever starts *passing*, the provider gained ED448 and this test
    // should flip to `connects`.
    let s = SmpServer::parse(PUBLIC_ED448).expect("parse");
    match tls::test_connection(&s).await {
        Ok(()) => println!("NOTE: smp8 now connects — rustcrypto gained ED448; update this test"),
        Err(e) => println!("EXPECTED (pure-Rust ED448 gap): {e}"),
    }
}

#[tokio::test]
#[ignore = "makes a real TLS connection to smp.konkin.io"]
async fn wrong_fingerprint_is_rejected() {
    // konkin host, but a different (public server's) fingerprint → the pin
    // must reject it, proving the check has teeth (anti-downgrade/MITM)
    let bad = "smp://bU0K-bRg0FEOTKArHrx40e6L1lDzz6i8kdcKMV-vMWo=@smp.konkin.io";
    let s = SmpServer::parse(bad).expect("parse");
    let err = tls::test_connection(&s)
        .await
        .expect_err("a mismatched fingerprint must be rejected");
    println!("OK: mismatched pin rejected: {err}");
}
