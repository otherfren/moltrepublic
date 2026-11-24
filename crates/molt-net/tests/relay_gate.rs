// SPDX-License-Identifier: GPL-3.0-or-later
//! A withdrawn relay is not dialed (`docs_archive/transport/relay_pool.md`
//! §3 rule 3): the gate every engine dialer carries, at every path class
//! that opens a relay socket. Every "no dial" is read off a proxy's accept
//! counter after the withdrawal returned.

mod common;

use std::time::Duration;

use common::proxy::Counting;
use molt_net::dial::{Dialer, RelayGateKeeper, Route};
use molt_net::relay_runtime::RelayRuntime;
use molt_net::relay_ws::RelayWs;
use molt_net::ritual_net::GroupChannel;
use molt_net::NetError;
use nostr::{EventBuilder, Filter, Keys, Kind};
use nostr_relay_builder::MockRelay;

const RECV: Duration = Duration::from_secs(10);

async fn relay_behind_proxy() -> (MockRelay, Counting) {
    let relay = MockRelay::run().await.expect("relay");
    let target = relay
        .url()
        .await
        .to_string()
        .trim_start_matches("ws://")
        .trim_end_matches('/')
        .to_string();
    let proxy = Counting::run(target).await;
    (relay, proxy)
}

fn note(text: &str) -> nostr::Event {
    EventBuilder::new(Kind::Custom(4242), text)
        .sign_with_keys(&Keys::generate())
        .expect("sign")
}

fn gated(keeper: &RelayGateKeeper) -> Dialer {
    Dialer::direct().with_gate(keeper.gate())
}

/// One-shot publish (seat inbox, ritual legs): refused before any socket,
/// and switching back on dials again.
#[tokio::test]
async fn a_withdrawn_relay_gets_no_one_shot_publish_and_comes_back_on_consent() {
    let (_relay, proxy) = relay_behind_proxy().await;
    let keeper = RelayGateKeeper::default();
    keeper.set(&[proxy.url()]);
    let rt = RelayRuntime::new(gated(&keeper), vec![proxy.url()]);
    rt.publish(&note("allowed")).await.expect("allowed relay publishes");
    assert_eq!(proxy.accepts(), 1);

    keeper.set(&[]);
    let err = rt.publish(&note("withdrawn")).await.expect_err("withdrawn");
    assert!(matches!(err, NetError::Withdrawn), "{err:?}");
    assert_eq!(proxy.accepts(), 1, "withdrawn, yet the relay was dialed");

    keeper.set(&[proxy.url()]);
    rt.publish(&note("back on")).await.expect("consent given again");
    assert_eq!(proxy.accepts(), 2);
}

/// A dial begun before the withdrawal: its Tor/TCP connect completes after
/// it, and it sends nothing - no TLS hello, no WebSocket handshake. The
/// same dial still allowed does speak (the counter-case).
#[tokio::test]
async fn a_dial_in_flight_at_the_withdrawal_sends_no_handshake() {
    let socks = common::held_socks::run().await;
    let url = "wss://relay.example.org".to_string();
    let keeper = RelayGateKeeper::default();
    keeper.set(std::slice::from_ref(&url));
    let dialer = Dialer::on(Route::Socks5 {
        proxy: format!("127.0.0.1:{}", socks.port),
        session: "t".into(),
    })
    .with_gate(keeper.gate());

    for withdraw in [false, true] {
        let (d, u) = (dialer.clone(), url.clone());
        let dial = tokio::spawn(async move { RelayWs::connect(&d, &u).await.map(|_| ()) });
        let (release, sent) = socks.requests.lock().await.recv().await.expect("a CONNECT");
        if withdraw {
            keeper.set(&[]);
        }
        release.send(()).expect("release");
        let res = dial.await.expect("dial task");
        let bytes = sent.await.expect("socks verdict");
        if withdraw {
            assert!(matches!(res, Err(NetError::Withdrawn)), "{res:?}");
            assert_eq!(bytes, 0, "the in-flight dial spoke to a withdrawn relay");
        } else {
            assert!(bytes > 0, "an allowed dial starts its handshake");
        }
    }
}

/// The re-key delivery's shape: ONE task publishing over two relays loses
/// only the withdrawn one - the kept socket to it closes, no fresh dial
/// goes out, and the other relay still takes the frame.
#[tokio::test]
async fn a_publish_over_two_relays_loses_only_the_withdrawn_one() {
    let (_ra, a) = relay_behind_proxy().await;
    let (_rb, b) = relay_behind_proxy().await;
    let keeper = RelayGateKeeper::default();
    keeper.set(&[a.url(), b.url()]);
    let chan = GroupChannel::new(gated(&keeper), vec![a.url(), b.url()], [7u8; 32]);
    let (_, report) = chan.publish_frame(&[9u8; 32], b"before").await.expect("publish");
    assert_eq!(report.accepted.len(), 2);

    keeper.set(&[b.url()]);
    let (_, report) = chan.publish_frame(&[9u8; 32], b"after").await.expect("b still takes it");
    assert_eq!(report.accepted, vec![b.url()]);
    a.wait_closes(1).await;
    assert_eq!(a.accepts(), 1, "withdrawn, yet the relay was dialed");

    keeper.set(&[a.url(), b.url()]);
    let (_, report) = chan.publish_frame(&[9u8; 32], b"back").await.expect("publish");
    assert_eq!(report.accepted.len(), 2);
    assert_eq!(a.accepts(), 2);

    keeper.set(&[]);
    let err = chan.publish_frame(&[9u8; 32], b"none").await.expect_err("nothing allowed");
    assert!(matches!(err, NetError::Withdrawn), "{err:?}");
}

/// A subscription (downloads, base fetches, inboxes): the live socket to a
/// withdrawn relay closes at once, its supervisor dials nothing while it
/// stays withdrawn, the other relay keeps delivering, and consent given
/// again redials.
#[tokio::test]
async fn a_subscription_parks_a_withdrawn_relay_and_resumes_it() {
    let (_ra, a) = relay_behind_proxy().await;
    let (_rb, b) = relay_behind_proxy().await;
    let keeper = RelayGateKeeper::default();
    keeper.set(&[a.url(), b.url()]);
    let rt = RelayRuntime::new(gated(&keeper), vec![a.url(), b.url()])
        .with_backoff(Duration::from_millis(5), Duration::from_millis(20));
    let mut sub = rt
        .subscribe(Filter::new().kind(Kind::Custom(4242)))
        .await
        .expect("subscribe");
    assert_eq!((a.accepts(), b.accepts()), (1, 1));

    keeper.set(&[b.url()]);
    a.wait_closes(1).await;
    let plain = RelayRuntime::new(Dialer::direct(), vec![b.url()]);
    for i in 0..5 {
        let ev = note(&format!("live {i}"));
        plain.publish(&ev).await.expect("publish to b");
        assert_eq!(sub.recv(RECV).await.map(|e| e.id), Some(ev.id), "b keeps delivering");
    }
    assert_eq!(a.accepts(), 1, "withdrawn, yet the supervisor redialed");

    keeper.set(&[a.url(), b.url()]);
    a.wait_accepts(2).await;
}

/// Per engine, never per process: two keepers share nothing.
#[tokio::test]
async fn two_engines_do_not_share_a_gate() {
    let (_relay, proxy) = relay_behind_proxy().await;
    let allows = RelayGateKeeper::default();
    allows.set(&[proxy.url()]);
    let refuses = RelayGateKeeper::default();
    refuses.set(&["wss://other.example.org".to_string()]);

    let err = RelayWs::connect(&gated(&refuses), &proxy.url()).await.map(|_| ()).expect_err("refused");
    assert!(matches!(err, NetError::Withdrawn), "{err:?}");
    assert_eq!(proxy.accepts(), 0);
    RelayWs::connect(&gated(&allows), &proxy.url()).await.expect("allowed");
    assert_eq!(proxy.accepts(), 1);
}
