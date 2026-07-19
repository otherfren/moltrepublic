// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
//! SmpTransport through the Transport trait, against real servers: create
//! a queue, subscribe, send blocks through the trait, receive them.
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::{PaddedBlock, Transport, PADDED_BLOCK_LEN};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";
const SMP8: &str = "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im";

fn block(tag: u8) -> PaddedBlock {
    let mut b = vec![tag; PADDED_BLOCK_LEN];
    b[..5].copy_from_slice(b"MARK\0");
    PaddedBlock::from_bytes(b).expect("size")
}

async fn trait_round_trip(url: &str, label: &str) {
    let s = SmpServer::parse(url).expect("parse");
    // recipient node creates a queue and subscribes
    let recipient = SmpTransport::new(s.clone());
    let pair = recipient.create_queue().await.expect("create_queue");
    let mut rx = recipient.subscribe(&pair.rcv).await.expect("subscribe");
    // an independent sender node sends two blocks to the queue's send addr
    let sender = SmpTransport::new(s.clone());
    for tag in [7u8, 9u8] {
        sender.send(&pair.snd, block(tag)).await.expect("send");
    }
    // both arrive through the trait, in order, intact
    for tag in [7u8, 9u8] {
        let d = tokio::time::timeout(std::time::Duration::from_secs(6), rx.recv())
            .await
            .expect("timeout")
            .expect("delivery");
        assert_eq!(d.block.as_slice()[10], tag, "[{label}] block {tag} content");
        assert_eq!(&d.block.as_slice()[..5], b"MARK\0");
    }
    println!("OK [{label}]: SmpTransport delivered 2 blocks through the Transport trait");
}

#[tokio::test]
#[ignore = "live network"]
async fn smp_transport_over_ed25519() { trait_round_trip(KONKIN, "konkin").await; }

#[tokio::test]
#[ignore = "live network"]
async fn smp_transport_over_ed448() { trait_round_trip(SMP8, "smp8 official").await; }

/// Regression (T4 review): a subscription's idle long-poll must NOT be killed
/// by the per-block request/response deadline (30 s). Subscribe, stay quiet
/// well past that deadline, THEN send — the message must still arrive, proving
/// the subscription survived the idle window (else the node goes deaf after
/// 30 s of quiet — breaking recovery / runtime delivery / late joins).
async fn subscription_survives_idle_past_the_deadline(url: &str) {
    let s = SmpServer::parse(url).expect("parse");
    let recipient = SmpTransport::new(s.clone());
    let pair = recipient.create_queue().await.expect("create_queue");
    let mut rx = recipient.subscribe(&pair.rcv).await.expect("subscribe");
    // idle for longer than BLOCK_IO_TIMEOUT (30 s) with nothing pushed
    tokio::time::sleep(std::time::Duration::from_secs(40)).await;
    // only now does a sender enqueue — the subscription must still be alive
    let sender = SmpTransport::new(s.clone());
    sender.send(&pair.snd, block(5)).await.expect("send after idle");
    let d = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("subscription still delivers after a >30s idle")
        .expect("delivery");
    assert_eq!(d.block.as_slice()[10], 5, "block content after idle");
    println!("OK: subscription survived a 40s idle and still delivered");
}

#[tokio::test]
#[ignore = "live network (slow: 40s idle)"]
async fn subscription_survives_idle_ed25519() {
    subscription_survives_idle_past_the_deadline(KONKIN).await;
}

/// The 2026-07-19 restart fix, pinned against the real server's SKEY
/// semantics: T1 creates + subscribes a queue and exports its creds BEFORE
/// any send (the mesh-up persist moment — no send key exists yet, only the
/// seed). T2 (a "reopened" node) imports them and sends — its FIRST send
/// SKEYs the queue with the seed-derived key. T3 (a second reopen, same
/// creds) sends again — the server either accepts the idempotent same-key
/// re-SKEY or rejects it, in which case the D3 fallback (SEND anyway, the
/// server's verdict is authoritative) must still deliver. Both blocks must
/// arrive on T1's subscription. Before the fix T3's fresh random key drew
/// `SKEY rejected: ERR AUTH` and the leg died forever.
#[tokio::test]
#[ignore = "live network"]
async fn skey_rederivation_after_reopen_keeps_sending() {
    let s = SmpServer::parse(KONKIN).expect("parse");
    let t1 = SmpTransport::new(s.clone());
    let pair = t1.create_queue().await.expect("create_queue");
    let mut rx = t1.subscribe(&pair.rcv).await.expect("subscribe");
    // the mesh-up moment: creds exported before ANY send happened
    let creds = t1.export_creds().expect("creds export");

    // first incarnation of the sender: derives + SKEYs, then sends
    let t2 = SmpTransport::new(s.clone());
    t2.import_creds(&creds);
    t2.send(&pair.snd, block(21)).await.expect("first-incarnation send");

    // second incarnation, SAME creds (a restart): must re-derive the SAME
    // key and keep sending — re-SKEY idempotence or the SEND fallback
    let t3 = SmpTransport::new(s.clone());
    t3.import_creds(&creds);
    t3.send(&pair.snd, block(22)).await.expect("post-reopen send");

    for tag in [21u8, 22u8] {
        let d = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("delivery within 10s")
            .expect("delivery");
        assert_eq!(d.block.as_slice()[10], tag, "block {tag} delivered");
    }
    println!("OK: both incarnations sent with the same derived sender key");
}
