// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N2 relay-runtime keystones (`docs/transport/nostr_n2_plan.md` §3), driven
//! against the in-process relay — the Nostr twin of the loopback hub. Steps
//! land here one by one, red first.

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::relay_ws::RelayWs;
use nostr::{
    Alphabet, ClientMessage, EventBuilder, Filter, Keys, Kind, RelayMessage, SingleLetterTag,
    SubscriptionId, Tag,
};
use nostr_relay_builder::MockRelay;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn h_tagged_event(keys: &Keys, h: &str, content: &str) -> nostr::Event {
    EventBuilder::new(Kind::Custom(445), content)
        .tag(Tag::parse(["h", h]).expect("h tag"))
        .sign_with_keys(keys)
        .expect("sign")
}

/// Step 1 — the typed single-connection edge (`relay_ws`): EVENT → OK(true),
/// REQ → the stored EVENT + EOSE, over a plain ws:// loopback connection
/// dialed through the REAL dialer (Direct mode — §10.14 admits loopback).
#[tokio::test]
async fn relay_ws_publishes_and_reads_back() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");

    let keys = Keys::generate();
    let event = h_tagged_event(&keys, "6d6f6c74", "opaque ciphertext");
    let id = event.id;

    let mut ws = RelayWs::connect(&dialer, &url).await.expect("connect");
    ws.send(ClientMessage::event(event)).await.expect("send EVENT");
    match ws.recv(RECV_TIMEOUT).await.expect("a frame arrives") {
        RelayMessage::Ok { event_id, status, message } => {
            assert!(status, "the relay must accept the event: {message}");
            assert_eq!(event_id, id);
        }
        other => panic!("expected OK, got {other:?}"),
    }

    // a SECOND connection reads it back: EVENT, then EOSE
    let mut reader = RelayWs::connect(&dialer, &url).await.expect("second connect");
    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");
    let sub = SubscriptionId::new("n2-step1");
    reader
        .send(ClientMessage::req(sub.clone(), vec![filter]))
        .await
        .expect("send REQ");
    match reader.recv(RECV_TIMEOUT).await.expect("stored event") {
        RelayMessage::Event { subscription_id, event } => {
            assert_eq!(subscription_id.as_ref(), &sub);
            assert_eq!(event.id, id, "the stored event comes back byte-identical");
        }
        other => panic!("expected EVENT, got {other:?}"),
    }
    match reader.recv(RECV_TIMEOUT).await.expect("end of stored events") {
        RelayMessage::EndOfStoredEvents(got) => assert_eq!(got.as_ref(), &sub),
        other => panic!("expected EOSE, got {other:?}"),
    }
}

/// Step 2 KEYSTONE — publish is ≥1-OK (concept §11 N2): one live relay among
/// dead ones is SUCCESS, with per-relay outcomes reported (never a silent
/// partial); no live relay is a typed failure. Publishing the same event
/// again stays success — a relay answering `OK:false "duplicate: …"` counts
/// as published (MDK port #3), which is what makes a rewind-resend safe.
#[tokio::test]
async fn publish_is_one_ok_with_per_relay_outcomes() {
    use molt_net::relay_runtime::RelayRuntime;

    let relay = MockRelay::run().await.expect("relay");
    let live = relay.url().await.to_string();
    let dead = "ws://127.0.0.1:9".to_string(); // discard port — nothing listens
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();
    let event = h_tagged_event(&keys, "6d6f6c74", "step2");

    let rt = RelayRuntime::new(dialer.clone(), vec![live.clone(), dead.clone()]);
    let report = rt.publish(&event).await.expect("one live relay is enough");
    assert_eq!(report.accepted, vec![live.clone()], "the live relay OK'd");
    assert_eq!(report.failed.len(), 1, "the dead relay is REPORTED, not hidden");
    assert_eq!(report.failed[0].0, dead);

    // the same event again: still success (duplicate-tolerant end to end)
    let again = rt.publish(&event).await.expect("re-publish is success");
    assert_eq!(again.accepted, vec![live]);

    // no relay at all / no live relay: a typed failure, never a silent drop
    let none = RelayRuntime::new(dialer.clone(), vec![]);
    assert!(none.publish(&event).await.is_err(), "empty pool cannot publish");
    let all_dead = RelayRuntime::new(dialer, vec![dead]);
    assert!(all_dead.publish(&event).await.is_err(), "no OK anywhere is failure");
}
