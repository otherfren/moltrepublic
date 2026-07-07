// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! THE END GOAL: the founding ritual run between **two independent
//! instances** over a **real SMP server** (transport concept §3.3) — and
//! the member side is the engine's *actual* [`molt_engine::run_ritual_member`],
//! not a reimplementation. Founder and member each hold their own
//! [`SmpTransport`]; nothing is shared beyond the invite link material (as
//! it travels off-band) and the wire.
//!
//! The member creates the queue it receives on and advertises it inside the
//! `JoinRequest` (SMP's queue model). The founder side here mirrors the
//! engine's `spawn_founder_recv` + `maybe_seal` — those are `State`-coupled,
//! so the test drives the same steps against the same crypto
//! (`molt_net::invite`, `molt_storage` identity, `roster_canonical_bytes`).
//!
//! `#[ignore]` (real network):
//! `cargo test -p molt-engine --test ritual_over_smp -- --ignored --nocapture`

use molt_core::{roster_canonical_bytes, MemberIdentity};
use molt_net::invite::{self, RitualMsg};
use molt_net::smp::{SmpServer, SmpTransport};
use molt_net::{
    msg_id, send_framed, wrap, QueueId, Reassembler, SndQueueAddr, Transport, WrapKey,
};
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

/// Rebuild the founder's send handle to the member's reply queue from the
/// handover the member advertised in its `JoinRequest`.
fn reply_target(r: &invite::ReplyHandover) -> (SndQueueAddr, WrapKey) {
    let id = hex::decode(&r.queue_id).expect("queue id hex");
    let wrap_bytes: [u8; 32] = hex::decode(&r.wrap).expect("wrap hex").try_into().expect("32 bytes");
    (
        SndQueueAddr { server: r.server.clone(), id: QueueId::from_bytes(id) },
        WrapKey::from_bytes(wrap_bytes),
    )
}

async fn ritual(url: &str, label: &str) {
    let server = SmpServer::parse(url).expect("parse");
    let ws_id = "smp-ritual-workspace";
    let ticket = invite::mint_ticket().expect("ticket");

    // ---- FOUNDER instance: its own transport + identity + invite queue --
    let founder_t = SmpTransport::new(server.clone());
    let founder_seed =
        molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
            .expect("seed");
    let (founder_sk, founder_pk) = molt_storage::derive_identity_key(&founder_seed, ws_id);
    let invite_q = founder_t.create_queue().await.expect("invite NEW");
    let invite_wrap = WrapKey::fresh().expect("wrap");
    let mut founder_rx = founder_t.subscribe(&invite_q.rcv).await.expect("SUB invite");

    // the off-band invite link carries {invite_snd, invite_wrap, ticket}.
    // Hand them to the member instance as InviteMaterial.
    let member_t = SmpTransport::new(server.clone());
    let material = molt_engine::InviteMaterial {
        seat: 0,
        transport: member_t.clone(),
        invite_snd: invite_q.snd.clone(),
        invite_wrap: invite_wrap.clone(),
        ticket: ticket.clone(),
    };

    // ---- MEMBER instance: the REAL engine member code over its own SMP
    // transport. It derives its identity from its phrase, creates+subscribes
    // its reply queue, sends the MAC-bound JoinRequest advertising it, awaits
    // the table, signs, and returns its identity pk. ----
    let member_phrase = molt_storage::generate_seed_phrase().expect("phrase");
    // collect_genesis = false: the founder side here is hand-rolled and does
    // not distribute a genesis, so the member stops at its seal signature
    let member_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(
            material,
            "remote-member".into(),
            member_phrase,
            false,
            false,
            None,
            None,
        )
        .await
    });

    // FOUNDER: receive the JoinRequest, verify the ticket MAC, learn the
    // member's identity + reply queue
    let mut founder_reasm = Reassembler::new();
    let RitualMsg::Join(req) =
        recv_ritual(&mut founder_rx, &invite_wrap, &mut founder_reasm, 20).await
    else {
        panic!("[{label}] expected JoinRequest");
    };
    assert!(
        invite::verify_join_mac(&ticket, &req.name, &req.identity_pk, &req.mac),
        "[{label}] the join MAC must verify against the ticket"
    );
    let (reply_snd, reply_wrap) =
        reply_target(req.reply.as_ref().expect("member advertised a reply queue"));

    // FOUNDER: build the pre-attestation proposal (salted by the content-derived
    // republic id, not the local ws id) and send it for the member to recompute
    // and ratify
    let _ = ws_id;
    let identities = vec![
        MemberIdentity { member: "founder".into(), identity_pk: founder_pk.clone() },
        MemberIdentity { member: req.name.clone(), identity_pk: req.identity_pk.clone() },
    ];
    let republic_id = molt_storage::republic_id("SMP Duet", 2, 2, &identities);
    let table = roster_canonical_bytes(&republic_id, 2, 2, &identities, "");
    let founder_sig = molt_storage::identity_sign(&founder_sk, &table);
    let proposal = molt_core::SealedRoster {
        name: "SMP Duet".into(),
        republic_id: republic_id.clone(),
        rule_m: 2,
        rule_n: 2,
        roster: vec!["founder".into(), req.name.clone()],
        identities: identities.clone(),
        attestations: Vec::new(),
        agenda: String::new(),
    };
    let seal = RitualMsg::Seal {
        proposal: serde_json::to_string(&proposal).expect("encode proposal"),
    };
    send_framed(
        &founder_t,
        &reply_snd,
        &reply_wrap,
        msg_id("founder", "member", 1),
        &serde_json::to_vec(&seal).expect("encode seal"),
    )
    .await
    .expect("send table over SMP");

    // FOUNDER: receive the member's seal signature and verify it against the
    // anchored key — the ritual is sealed
    let RitualMsg::Signed(sealed) =
        recv_ritual(&mut founder_rx, &invite_wrap, &mut founder_reasm, 20).await
    else {
        panic!("[{label}] expected SealSigned");
    };
    assert!(
        molt_storage::identity_verify(&req.identity_pk, &table, &sealed.sig),
        "[{label}] the member's seal signature must verify over the roster table"
    );
    assert!(molt_storage::identity_verify(&founder_pk, &table, &founder_sig));

    // the real member code returns the pk it anchored — it must match
    let member_pk = member_task
        .await
        .expect("member task panicked")
        .expect("member ritual failed")
        .pk;
    assert_eq!(member_pk, req.identity_pk, "member's returned pk matches its join");

    println!(
        "OK [{label}]: founding ritual completed between two instances over SMP — \
         real run_ritual_member on the member side, both attestations verify"
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
