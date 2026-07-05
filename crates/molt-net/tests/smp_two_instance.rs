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
    // Instance S: an independent connection, secure the queue then send
    // three messages to its sender id
    let mut sender = SmpConn::connect(&s).await.expect("S connect");
    let key = sender.secure_as_sender(&q.sender_id).await.expect("SKEY");
    for i in 0..3u8 {
        let payload = format!("molt ritual message {i} from instance S");
        sender.send_to(&q.sender_id, &key, payload.as_bytes()).await.expect("SEND");
    }
    // Instance R: receive + decrypt each, in order
    for i in 0..3u8 {
        let body = r.recv_next(&q).await.expect("recv");
        let want = format!("molt ritual message {i} from instance S");
        assert!(
            body.windows(want.len()).any(|w| w == want.as_bytes()),
            "[{label}] message {i} must arrive intact; got {:?}",
            String::from_utf8_lossy(&body[..body.len().min(64)])
        );
    }
    println!("OK [{label}]: two instances exchanged 3 secured messages over SMP");
}

#[tokio::test]
#[ignore = "live network"]
async fn two_instances_over_ed25519() { round_trip(KONKIN, "konkin ED25519").await; }

#[tokio::test]
#[ignore = "live network"]
async fn two_instances_over_ed448() { round_trip(SMP8, "smp8 ED448").await; }
