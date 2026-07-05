// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
//! Live check: create a real queue on the server with a signed NEW
//! command (transport concept §3.2). `#[ignore]` (real network).

use molt_net::smp::{SmpConn, SmpServer};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

/// Checkpoint 3: NEW → IDS. A signed recipient command creates a real
/// queue on the live server and returns its recipient/sender ids. This
/// proves the Ed25519 command-signing path (signature over
/// sessionId ++ authorized) end to end.
#[tokio::test]
#[ignore = "creates a real queue on smp.konkin.io"]
async fn new_queue_returns_ids() {
    let s = SmpServer::parse(KONKIN).expect("parse");
    let mut conn = SmpConn::connect(&s).await.expect("handshake");
    let q = conn.new_queue().await.expect("NEW → IDS");
    assert!(
        (16..=24).contains(&q.recipient_id.len()),
        "recipient id {} bytes",
        q.recipient_id.len()
    );
    assert!((16..=24).contains(&q.sender_id.len()), "sender id");
    assert_ne!(q.recipient_id, q.sender_id);
    println!(
        "OK: NEW created a real queue on {} — recipientId={} senderId={}",
        s.host,
        hex::encode(&q.recipient_id),
        hex::encode(&q.sender_id)
    );
}
