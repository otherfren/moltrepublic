// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N4a keystones for the engine-facing ritual facade
//! (`docs/transport/nostr_n4_plan.md` §2/§4/§5): gift-wrapped ritual legs
//! and the kind-445 group channel, driven against the in-process relay.

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::envelope::{h_tag, open_outer, H_WINDOW};
use molt_net::invite::RitualMsg;
use molt_net::nostr_identity;
use molt_net::ritual_net::{window_tags, GroupChannel, RitualDelivery, RitualNet};
use molt_net::welcome::WelcomePayload;
use nostr_relay_builder::MockRelay;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

/// A relay + two endpoints with derived (ticket-salted) transport keys —
/// the shape every founder↔joiner leg starts from.
async fn two_endpoints() -> (MockRelay, RitualNet, RitualNet) {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let (founder_sk, _) = nostr_identity(b"founder-entropy", "aa11");
    let (joiner_sk, _) = nostr_identity(b"joiner-entropy", "bb22");
    let founder =
        RitualNet::new(dialer(), vec![url.clone()], &founder_sk).expect("founder endpoint");
    let joiner = RitualNet::new(dialer(), vec![url], &joiner_sk).expect("joiner endpoint");
    (relay, founder, joiner)
}

/// Keystone 1 — a kind-446 ritual message round-trips between two
/// endpoints, and the sender the inbox reports is the PROVEN NIP-59 seal
/// author (§2.1), not an asserted field.
#[tokio::test]
async fn a_ritual_msg_round_trips_between_two_endpoints() {
    let (_relay, founder, joiner) = two_endpoints().await;
    // pk_hex agrees with the one anchor derivation
    let (founder_sk, founder_pk) = nostr_identity(b"founder-entropy", "aa11");
    assert_eq!(founder.pk_hex(), founder_pk);
    assert_eq!(
        founder.pk_hex(),
        molt_net::nostr_pk_for_sk(&founder_sk).expect("derivable")
    );

    let mut inbox = founder.inbox().await.expect("founder inbox");
    assert!(inbox.live(RECV_TIMEOUT).await, "the inbox REQ replayed");

    let msg = RitualMsg::JoinAccepted { seat: 1 };
    joiner
        .send_ritual(&founder.pk_hex(), &msg)
        .await
        .expect("send the ritual msg");

    match inbox.recv(RECV_TIMEOUT).await.expect("a delivery") {
        RitualDelivery::Msg(got, sender) => {
            assert_eq!(got, msg, "the RitualMsg vocabulary rides verbatim");
            assert_eq!(
                sender,
                joiner.pk_hex(),
                "the sender is the verified seal author — proof of possession"
            );
        }
        other => panic!("expected a ritual msg, got {other:?}"),
    }
}

/// Keystone 2 — the kind-444 Welcome payload v2 (welcome + rotation seed +
/// relays) round-trips intact, with the proven sender.
#[tokio::test]
async fn a_welcome_round_trips_with_its_payload() {
    let (relay, founder, joiner) = two_endpoints().await;
    let url = relay.url().await.to_string();

    let mut inbox = joiner.inbox().await.expect("joiner inbox");
    assert!(inbox.live(RECV_TIMEOUT).await, "the inbox REQ replayed");

    let payload = WelcomePayload {
        welcome: b"the mls welcome bytes".to_vec(),
        rotation_seed: [7u8; 32],
        relays: vec![url],
    };
    founder
        .send_welcome(&joiner.pk_hex(), &payload)
        .await
        .expect("send the welcome");

    match inbox.recv(RECV_TIMEOUT).await.expect("a delivery") {
        RitualDelivery::Welcome(got, sender) => {
            assert_eq!(got, payload, "welcome + seed + relays arrive intact");
            assert_eq!(sender, founder.pk_hex(), "the inviter is proven");
        }
        other => panic!("expected a welcome, got {other:?}"),
    }
}

/// Keystone 3 — foreign traffic is SKIPPED, never fatal: a wrap addressed
/// to a third key (filtered by `#p`) and a forged kind-1059 that matches
/// the filter but is not a gift wrap both leave the inbox waiting (None on
/// timeout), and a subsequent addressed wrap still arrives.
#[tokio::test]
async fn foreign_traffic_is_skipped_not_fatal() {
    use molt_net::relay_runtime::RelayRuntime;
    use nostr::{EventBuilder, Keys, Kind, PublicKey, Tag};

    let (relay, founder, joiner) = two_endpoints().await;
    let url = relay.url().await.to_string();
    let (_, third_pk) = nostr_identity(b"third-party-entropy", "cc33");

    let mut inbox = founder.inbox().await.expect("founder inbox");
    assert!(inbox.live(RECV_TIMEOUT).await, "the inbox REQ replayed");

    // a wrap addressed to a THIRD key — the founder's #p filter excludes it
    joiner
        .send_ritual(&third_pk, &RitualMsg::LinkSpent { seat: 0 })
        .await
        .expect("send to the third key");
    // …and a 1059 that MATCHES the filter but does not peel (junk content):
    // it reaches the loop and must be skipped there, not kill it
    let junk = EventBuilder::new(Kind::Custom(1_059), "not a gift wrap")
        .tag(Tag::public_key(
            PublicKey::from_hex(&founder.pk_hex()).expect("founder anchor parses"),
        ))
        .sign_with_keys(&Keys::generate())
        .expect("sign the junk 1059");
    RelayRuntime::new(dialer(), vec![url])
        .publish(&junk)
        .await
        .expect("publish the junk");

    assert!(
        inbox.recv(Duration::from_secs(2)).await.is_none(),
        "foreign and unreadable traffic times out instead of erroring"
    );

    // a subsequent addressed wrap still arrives — the loop survived
    let msg = RitualMsg::JoinAccepted { seat: 3 };
    joiner
        .send_ritual(&founder.pk_hex(), &msg)
        .await
        .expect("send the addressed wrap");
    match inbox.recv(RECV_TIMEOUT).await.expect("the addressed wrap") {
        RitualDelivery::Msg(got, sender) => {
            assert_eq!(got, msg);
            assert_eq!(sender, joiner.pk_hex());
        }
        other => panic!("expected a ritual msg, got {other:?}"),
    }
}

/// Keystone 4 — a 445 frame round-trips through the group channel: the
/// exporter secret opens the delivered content, and the returned
/// `created_at` is EXACTLY the stamp `publish_frame` reported (the carrier
/// stamp both ends must agree on).
#[tokio::test]
async fn a_445_frame_round_trips_through_the_group_channel() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let channel = GroupChannel::new(dialer(), vec![url], [5u8; 32]);

    let mut sub = channel.subscribe().await.expect("group subscription");
    assert!(sub.live(RECV_TIMEOUT).await, "the group REQ replayed");

    let stamp = channel
        .publish_frame(&[9u8; 32], b"the mls ciphertext")
        .await
        .expect("publish the frame");

    let (content, created_at) = sub.recv(RECV_TIMEOUT).await.expect("the frame");
    assert_eq!(created_at, stamp, "one carrier stamp on both ends");
    assert_eq!(
        open_outer(&[[9u8; 32]], &content).expect("the exporter opens it"),
        b"the mls ciphertext"
    );
}

/// Keystone 5 — a frame published under a FOREIGN rotation seed never
/// reaches a subscriber: the h tags disagree, so the filter (and the
/// recv-side gate behind it) keeps the channels disjoint.
#[tokio::test]
async fn a_frame_under_a_foreign_seed_is_not_delivered() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let ours = GroupChannel::new(dialer(), vec![url.clone()], [1u8; 32]);
    let theirs = GroupChannel::new(dialer(), vec![url], [2u8; 32]);

    let mut sub = ours.subscribe().await.expect("our subscription");
    assert!(sub.live(RECV_TIMEOUT).await, "our REQ replayed");

    theirs
        .publish_frame(&[9u8; 32], b"not for us")
        .await
        .expect("their publish succeeds");
    assert!(
        sub.recv(Duration::from_secs(2)).await.is_none(),
        "a foreign-seed frame must not be delivered"
    );
}

/// Keystone 6 — the pure §4.4 skew-margin logic with injected time:
/// mid-window exactly one tag; within Δ=1h before a UTC boundary also the
/// NEXT window's tag; within Δ=1h after also the PREVIOUS window's — each
/// tag exactly the `envelope::h_tag` of its window.
#[test]
fn window_tags_cover_the_skew_margin() {
    let seed = [3u8; 32];
    let boundary = 20_000 * H_WINDOW;

    // mid-window: exactly the current window's tag
    let mid = boundary + H_WINDOW / 2;
    assert_eq!(window_tags(&seed, mid), vec![h_tag(&seed, mid)]);

    // 30 min before the next boundary: current + NEXT
    let before = boundary + H_WINDOW - 1_800;
    assert_eq!(
        window_tags(&seed, before),
        vec![h_tag(&seed, before), h_tag(&seed, boundary + H_WINDOW)],
        "shortly before midnight the next window is covered too"
    );

    // 30 min after the boundary: current + PREVIOUS
    let after = boundary + 1_800;
    assert_eq!(
        window_tags(&seed, after),
        vec![h_tag(&seed, after), h_tag(&seed, boundary - H_WINDOW)],
        "shortly after midnight the previous window is covered too"
    );
}
