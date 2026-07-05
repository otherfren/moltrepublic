// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
//! TWO independent SMP connections exchange a message through a real
//! server: instance R creates+subscribes a queue, instance S sends to its
//! sender id, R receives and decrypts. The transport-level proof of two
//! instances communicating over SMP.
use molt_net::smp::{SmpConn, SmpServer};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";
const SMP8: &str = "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im";

async fn round_trip(url: &str, label: &str) {
    let s = SmpServer::parse(url).expect("parse");
    // Instance R: create + subscribe a queue
    let mut r = SmpConn::connect(&s).await.expect("R connect");
    let q = r.new_queue(true).await.expect("NEW+SUB");
    // Instance S: an independent connection, send to the queue's sender id
    let mut sender = SmpConn::connect(&s).await.expect("S connect");
    let payload = b"molt ritual: hello from instance S";
    sender.send_to(&q.sender_id, payload).await.expect("SEND");
    // Instance R: receive + decrypt the server->recipient layer
    let (msg_id, plain) = r.recv_msg(&q).await.expect("recv MSG");
    // plaintext = timestamp(8) | msgFlags(2) | SP | body
    assert!(plain.len() > 11, "msg body present");
    let body = &plain[11..];
    assert!(
        body.windows(payload.len()).any(|w| w == payload),
        "[{label}] delivered body must contain the sent payload; got {:?}",
        String::from_utf8_lossy(&body[..body.len().min(64)])
    );
    r.ack(&q, &msg_id).await.expect("ACK");
    println!("OK [{label}]: two instances exchanged a message over SMP (msgId {} bytes)", msg_id.len());
}

#[tokio::test]
#[ignore = "live network"]
async fn two_instances_over_ed25519() { round_trip(KONKIN, "konkin ED25519").await; }

#[tokio::test]
#[ignore = "live network"]
async fn two_instances_over_ed448() { round_trip(SMP8, "smp8 ED448").await; }
