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
use molt_net::ritual_net::{window_tags, GroupChannel, GroupRecv, RitualDelivery, RitualNet};
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
        .send_ritual(&third_pk, &RitualMsg::LinkSpent { seat: 0, reason: String::new() })
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

    let (stamp, report) = channel
        .publish_frame(&[9u8; 32], b"the mls ciphertext")
        .await
        .expect("publish the frame");
    assert_eq!(report.accepted.len(), 1, "the one relay took it: {report:?}");

    let GroupRecv::Frame { content, created_at } = sub.recv(RECV_TIMEOUT).await else {
        panic!("expected the frame");
    };
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
        matches!(sub.recv(Duration::from_secs(2)).await, GroupRecv::Idle),
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

/// Cluster C — `publish_frame` reports WHICH relay refused.
///
/// The per-relay outcome existed in `RelayRuntime::publish` and was thrown
/// away one layer up (`.map(|_report| ())`, and `publish_frame` returning a
/// bare stamp), so "landed on 1 of 2 relays" was indistinguishable from full
/// delivery — and a ritual leg that reached nobody looked the same as one
/// that reached everybody.
#[tokio::test]
async fn publish_frame_reports_the_relay_that_refused() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let live = relay.url().await.to_string();
    // a port nothing listens on (bound then dropped — never port 9, a host
    // running discard would silently invert this)
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let p = l.local_addr().expect("addr").port();
        drop(l);
        format!("ws://127.0.0.1:{p}")
    };

    let channel = GroupChannel::new(dialer(), vec![live.clone(), dead.clone()], [3u8; 32]);
    let (_stamp, report) = channel
        .publish_frame(&[9u8; 32], b"partial landing")
        .await
        .expect("≥1 relay accepted, so the publish succeeds");

    assert_eq!(report.accepted, vec![live], "the live relay took it");
    assert_eq!(report.failed.len(), 1, "…and the dead one is reported: {report:?}");
    assert_eq!(report.failed[0].0, dead, "named by url");
    assert!(
        !report.failed[0].1.is_empty(),
        "…with a reason, not just a flag: {report:?}"
    );
}

/// Cluster G — the ritual's SUBSCRIPTIONS must authenticate.
///
/// `with_auth_keys` had zero production callers, so every ritual subscription
/// was built unauthenticated. Against an auth-required relay the supervisor
/// drops the challenge (`let Some(keys) = … else { continue }`) and the
/// session stays connected-but-silent — no EOSE, no events — so the ritual
/// simply times out with no error anywhere.
///
/// `Read` mode on purpose: it leaves WRITES unauthenticated, which is what
/// the publish path relies on (see the guard below). Write/Both would refuse
/// the publishes and this would be red for the wrong reason.
#[tokio::test]
async fn ritual_endpoints_sync_and_deliver_on_an_auth_required_relay() {
    use nostr_relay_builder::builder::{RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode};
    use nostr_relay_builder::LocalRelay;

    let relay = LocalRelay::new(
        RelayBuilder::default().nip42(RelayBuilderNip42 { mode: RelayBuilderNip42Mode::Read }),
    );
    relay.run().await.expect("auth-required relay runs");
    let url = relay.url().await.to_string();

    let (founder_sk, founder_pk) = nostr_identity(b"founder-entropy", "aa11");
    let (joiner_sk, _) = nostr_identity(b"joiner-entropy", "bb22");
    let founder = RitualNet::new(dialer(), vec![url.clone()], &founder_sk).expect("founder");
    let joiner = RitualNet::new(dialer(), vec![url.clone()], &joiner_sk).expect("joiner");

    // the 1059 inbox must become readable — it authenticates with the anchor,
    // which the relay already learns from the `#p` filter anyway
    let mut inbox = founder.inbox().await.expect("inbox subscribes");
    assert!(
        inbox.live(RECV_TIMEOUT).await,
        "the ritual inbox must replay on an auth-required relay"
    );
    joiner
        .send_ritual(&founder_pk, &RitualMsg::LinkSpent { seat: 0, reason: String::new() })
        .await
        .expect("the wrap publishes");
    let got = inbox.recv(RECV_TIMEOUT).await.expect("the wrap is delivered");
    assert!(matches!(got, RitualDelivery::Msg(RitualMsg::LinkSpent { .. }, _)));

    // …and so must the 445 group channel
    let chan = GroupChannel::new(dialer(), vec![url], [4u8; 32]);
    let mut sub = chan.subscribe().await.expect("group subscribes");
    assert!(
        sub.live(RECV_TIMEOUT).await,
        "the group channel must replay on an auth-required relay"
    );
    chan.publish_frame(&[7u8; 32], b"authed frame").await.expect("frame publishes");
    assert!(
        matches!(sub.recv(RECV_TIMEOUT).await, GroupRecv::Frame { .. }),
        "the frame is delivered"
    );
}

/// Cluster G, the OTHER direction — the publish path must stay
/// UNAUTHENTICATED, and fail loudly rather than quietly authenticate.
///
/// Subscriptions authenticate; publishes must not. An authed publish channel
/// links every ephemeral-key event we send to the member behind it (§7.5).
/// This is the guard on step 2: adding `with_auth_keys` to `RitualNet::publish`
/// makes it go red.
#[tokio::test]
async fn the_publish_path_refuses_to_authenticate() {
    use nostr_relay_builder::builder::{RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode};
    use nostr_relay_builder::LocalRelay;

    // Write mode: the relay demands AUTH to PUBLISH
    let relay = LocalRelay::new(
        RelayBuilder::default().nip42(RelayBuilderNip42 { mode: RelayBuilderNip42Mode::Write }),
    );
    relay.run().await.expect("write-auth relay runs");
    let url = relay.url().await.to_string();

    let (sk, _) = nostr_identity(b"founder-entropy", "aa11");
    let (_, to) = nostr_identity(b"joiner-entropy", "bb22");
    let net = RitualNet::new(dialer(), vec![url], &sk).expect("endpoint");

    let err = net
        .send_ritual(&to, &RitualMsg::LinkSpent { seat: 0, reason: String::new() })
        .await
        .expect_err("the publish must NOT silently authenticate");
    let msg = err.to_string();
    assert!(
        msg.contains("auth") || msg.contains("refused"),
        "…and must say why, loudly: {msg}"
    );
}
