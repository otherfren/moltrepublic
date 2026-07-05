// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! THE END GOAL: the founding ritual run between **two independent
//! instances** that communicate over a **real SMP server** (transport
//! concept §3.3). Founder and member each hold their own `SmpTransport`;
//! nothing is shared beyond the invite link material (as it would travel
//! off-band) and the wire.
//!
//! Uses the real ritual crypto verbatim — `molt_net::invite` (ticket,
//! MAC, `RitualMsg`), `molt_storage` identity keys, and
//! `molt_core::roster_canonical_bytes` — over `molt_net::send_framed` /
//! per-queue wrapping, exactly as the engine's ritual does, only the
//! transport is `SmpTransport` instead of the loopback hub.
//!
//! `#[ignore]` (real network):
//! `cargo test -p molt-engine --test ritual_over_smp -- --ignored --nocapture`

use molt_core::{roster_canonical_bytes, MemberIdentity};
use molt_net::invite::{self, RitualMsg};
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::{send_framed, msg_id, wrap, Reassembler, SndQueueAddr, Transport, WrapKey};
use molt_net::chunk::PushOutcome;

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";
const SMP8: &str = "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im";

/// Receive one framed [`RitualMsg`] on a subscribed queue (unwrap +
/// reassemble), waiting up to `secs`.
async fn recv_ritual(
    rx: &mut tokio::sync::mpsc::Receiver<molt_net::Delivery>,
    wrap_key: &WrapKey,
    reasm: &mut Reassembler,
    secs: u64,
) -> RitualMsg {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let d = tokio::time::timeout(deadline - tokio::time::Instant::now(), rx.recv())
            .await
            .expect("ritual recv timed out")
            .expect("delivery");
        let Ok(plain) = wrap::unwrap_block(wrap_key, &d.block) else {
            d.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        d.ack.ack();
        if let Ok(PushOutcome::Complete(_, bytes)) = outcome {
            return serde_json::from_slice(&bytes).expect("decode RitualMsg");
        }
    }
}

async fn ritual(url: &str, label: &str) {
    let server = SmpServer::parse(url).expect("parse");
    let ws_id = "smp-ritual-workspace";
    let ticket = invite::mint_ticket().expect("ticket");

    // ---- FOUNDER instance: its own transport + identity + invite queue --
    let founder_t = SmpTransport::new(server.clone());
    let founder_seed = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().unwrap())
        .expect("seed");
    let (founder_sk, founder_pk) = molt_storage::derive_identity_key(&founder_seed, ws_id);
    // the queue the founder RECEIVES the join/seal on
    let invite_q = founder_t.create_queue().await.expect("invite NEW");
    let invite_wrap = WrapKey::fresh().expect("wrap");
    let mut founder_rx = founder_t.subscribe(&invite_q.rcv).await.expect("SUB invite");

    // the off-band invite link carries: {invite_snd, invite_wrap, ticket,
    // ws_id}. Hand them to the member instance.
    let link_invite_snd: SndQueueAddr = invite_q.snd.clone();
    let link_invite_wrap = invite_wrap.clone();
    let link_ticket = ticket.clone();

    // ---- MEMBER instance: a *separate* transport, own identity, own
    // reply queue (in SMP each party creates the queue it receives on) ----
    let member_t = SmpTransport::new(server.clone());
    let member_seed = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().unwrap())
        .expect("seed");
    // same workspace id as the founder, the member's own seed → a distinct
    // key that still belongs to this workspace's roster
    let (member_sk, member_pk) = molt_storage::derive_identity_key(&member_seed, ws_id);
    let reply_q = member_t.create_queue().await.expect("reply NEW");
    let reply_wrap = WrapKey::fresh().expect("wrap");
    let mut member_rx = member_t.subscribe(&reply_q.rcv).await.expect("SUB reply");

    // MEMBER → FOUNDER: activate the invite. The JoinRequest carries the
    // member's name+key and (SMP-specific) where to reach it: its reply
    // queue address + wrap key travel alongside (here as extra fields the
    // founder reads out of band, mirroring the concept's "reply pair").
    let name = "remote-member".to_string();
    let join = RitualMsg::Join(invite::JoinRequest {
        seat: 0,
        name: name.clone(),
        identity_pk: member_pk.clone(),
        mac: invite::join_mac(&link_ticket, &name, &member_pk),
    });
    send_framed(
        &member_t,
        &link_invite_snd,
        &link_invite_wrap,
        msg_id(&name, "founder", 1),
        &serde_json::to_vec(&join).unwrap(),
    )
    .await
    .expect("send JoinRequest over SMP");

    // FOUNDER: receive the JoinRequest, verify the ticket MAC, anchor key
    let mut founder_reasm = Reassembler::new();
    let RitualMsg::Join(req) =
        recv_ritual(&mut founder_rx, &invite_wrap, &mut founder_reasm, 15).await
    else {
        panic!("[{label}] expected JoinRequest");
    };
    assert!(
        invite::verify_join_mac(&ticket, &req.name, &req.identity_pk, &req.mac),
        "[{label}] the join MAC must verify against the ticket"
    );
    assert_eq!(req.identity_pk, member_pk, "member's real key arrived");

    // FOUNDER: build the sealed roster table and sign it
    let identities = vec![
        MemberIdentity { member: "founder".into(), identity_pk: founder_pk.clone() },
        MemberIdentity { member: req.name.clone(), identity_pk: req.identity_pk.clone() },
    ];
    let table = roster_canonical_bytes(ws_id, 2, 2, &identities);
    let founder_sig = molt_storage::identity_sign(&founder_sk, &table);

    // FOUNDER → MEMBER: send the canonical table to sign (over the member's
    // reply queue, which the member created and subscribed to)
    let seal = RitualMsg::Seal { table: hex::encode(&table) };
    send_framed(
        &founder_t,
        &reply_q.snd,
        &reply_wrap,
        msg_id("founder", "member", 1),
        &serde_json::to_vec(&seal).unwrap(),
    )
    .await
    .expect("send table over SMP");

    // MEMBER: receive the table, sign it, send the signature back
    let mut member_reasm = Reassembler::new();
    let RitualMsg::Seal { table: got } =
        recv_ritual(&mut member_rx, &reply_wrap, &mut member_reasm, 15).await
    else {
        panic!("[{label}] expected Seal table");
    };
    let table_bytes = hex::decode(&got).unwrap();
    let member_sig = molt_storage::identity_sign(&member_sk, &table_bytes);
    let signed = RitualMsg::Signed(invite::SealSigned { seat: 0, sig: member_sig });
    send_framed(
        &member_t,
        &link_invite_snd,
        &link_invite_wrap,
        msg_id(&name, "founder", 2),
        &serde_json::to_vec(&signed).unwrap(),
    )
    .await
    .expect("send SealSigned over SMP");

    // FOUNDER: receive the member's signature, verify it against the
    // anchored key — the ritual is sealed
    let RitualMsg::Signed(sealed) =
        recv_ritual(&mut founder_rx, &invite_wrap, &mut founder_reasm, 15).await
    else {
        panic!("[{label}] expected SealSigned");
    };
    assert!(
        molt_storage::identity_verify(&req.identity_pk, &table, &sealed.sig),
        "[{label}] the member's seal signature must verify over the roster table"
    );
    // and the founder's own attestation verifies too
    assert!(molt_storage::identity_verify(&founder_pk, &table, &founder_sig));

    println!(
        "OK [{label}]: founding ritual completed between two instances over SMP — \
         both roster attestations verify"
    );
}

#[tokio::test]
#[ignore = "runs the ritual over the real smp.konkin.io"]
async fn founding_ritual_two_instances_over_ed25519_smp() {
    ritual(KONKIN, "konkin ED25519").await;
}

#[tokio::test]
#[ignore = "runs the ritual over the official Ed448 server"]
async fn founding_ritual_two_instances_over_ed448_smp() {
    ritual(SMP8, "smp8 official ED448").await;
}
