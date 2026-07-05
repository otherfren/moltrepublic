// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
//! Full SMP stack (handshake + NEW→IDS) against BOTH an Ed25519 server
//! (the user's konkin.io) and an official Ed448 server (smp8.simplex.im).
use molt_net::smp::{SmpConn, SmpServer};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";
const SMP8: &str = "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im";

async fn full_stack(url: &str, label: &str) {
    let s = SmpServer::parse(url).expect("parse");
    let mut conn = SmpConn::connect(&s).await.expect("handshake");
    conn.ping().await.expect("PING/PONG");
    let q = conn.new_queue(false).await.expect("NEW→IDS");
    assert!((16..=24).contains(&q.recipient_id.len()));
    println!("OK [{label}] v{} — PING/PONG + NEW created queue rid={}", conn.version, hex::encode(&q.recipient_id));
}

#[tokio::test]
#[ignore = "live network"]
async fn ed25519_server_full_stack() { full_stack(KONKIN, "konkin ED25519").await; }

#[tokio::test]
#[ignore = "live network"]
async fn ed448_official_server_full_stack() { full_stack(SMP8, "smp8 ED448").await; }
