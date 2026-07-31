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

/// Step 3 KEYSTONE — one pooled subscription, event-id dedup: the SAME event
/// held by two relays reaches the consumer ONCE; a new live event also
/// arrives exactly once. The dedup is the runtime's job (concept §11 N2) —
/// the consumer must never see relay-count copies.
#[tokio::test]
async fn subscription_dedups_across_two_relays() {
    use molt_net::relay_runtime::RelayRuntime;

    let r1 = MockRelay::run().await.expect("relay 1");
    let r2 = MockRelay::run().await.expect("relay 2");
    let urls = vec![r1.url().await.to_string(), r2.url().await.to_string()];
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();

    // seed the same event on BOTH relays before subscribing
    let rt = RelayRuntime::new(dialer, urls);
    let seeded = h_tagged_event(&keys, "6d6f6c74", "step3 stored");
    let report = rt.publish(&seeded).await.expect("publish");
    assert_eq!(report.accepted.len(), 2, "both relays hold the event");

    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");
    let mut sub = rt.subscribe(filter).await.expect("subscribe");

    let got = sub.recv(RECV_TIMEOUT).await.expect("the stored event, once");
    assert_eq!(got.id, seeded.id);
    assert!(
        sub.recv(Duration::from_millis(400)).await.is_none(),
        "the second relay's copy is deduplicated"
    );

    // a LIVE event published to both relays also arrives exactly once
    let live = h_tagged_event(&keys, "6d6f6c74", "step3 live");
    rt.publish(&live).await.expect("publish live");
    let got = sub.recv(RECV_TIMEOUT).await.expect("the live event, once");
    assert_eq!(got.id, live.id);
    assert!(
        sub.recv(Duration::from_millis(400)).await.is_none(),
        "no duplicate of the live event either"
    );
}

/// Step 4 KEYSTONE (concept §4.3/§11 N2) — "a peer publishing +24h
/// `created_at` does not blind the receiver after reopen": the per-relay
/// cursor advances on delivered events but is CLAMPED to local now + skew,
/// and a runtime reopened from stored cursors subscribes with the 172 800 s
/// overlap — so a normal now-event still arrives where an unclamped cursor
/// would have skipped everything until tomorrow.
#[tokio::test]
async fn a_future_dated_event_does_not_blind_the_cursor() {
    use molt_net::relay_runtime::RelayRuntime;
    use nostr::Timestamp;

    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();
    let now = Timestamp::now();

    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");

    // a peer publishes a +24h future-dated event
    let future = EventBuilder::new(Kind::Custom(445), "from tomorrow")
        .tag(Tag::parse(["h", "6d6f6c74"]).expect("h tag"))
        .custom_created_at(Timestamp::from_secs(now.as_secs() + 86_400))
        .sign_with_keys(&keys)
        .expect("sign");
    let rt = RelayRuntime::new(dialer.clone(), vec![url.clone()]);
    rt.publish(&future).await.expect("publish future-dated");

    let mut sub = rt.subscribe(filter.clone()).await.expect("subscribe");
    let got = sub.recv(RECV_TIMEOUT).await.expect("future event delivered");
    assert_eq!(got.id, future.id);

    // the cursor is CLAMPED: never beyond local now + the skew margin
    let cursors = rt.cursors().await;
    let cursor = *cursors.get(&url).expect("a cursor for the relay");
    assert!(
        cursor <= now.as_secs() + 3_600 + 5,
        "cursor {cursor} ran into the future (now {})",
        now.as_secs()
    );
    drop(sub);

    // "reopen": a fresh runtime seeded with the stored cursors. A normal
    // now-event published meanwhile must still be delivered — the clamped
    // cursor minus the overlap reaches it, an unclamped one would not.
    let normal = h_tagged_event(&keys, "6d6f6c74", "from today");
    rt.publish(&normal).await.expect("publish normal");

    let rt2 = RelayRuntime::new(dialer, vec![url]).with_cursors(cursors);
    let mut sub2 = rt2.subscribe(filter).await.expect("resubscribe");
    let mut seen = Vec::new();
    while let Some(ev) = sub2.recv(Duration::from_secs(2)).await {
        seen.push(ev.id);
    }
    assert!(
        seen.contains(&normal.id),
        "the now-event must survive the reopen (seen: {seen:?})"
    );
}

/// Step 5 — the EOSE gate (MDK port #6): "synced" means every CONNECTED
/// relay finished its stored-events replay. A dead relay in the pool never
/// connected, so it must NOT wedge the gate.
#[tokio::test]
async fn synced_means_every_connected_relay_sent_eose() {
    use molt_net::relay_runtime::RelayRuntime;

    let r1 = MockRelay::run().await.expect("relay 1");
    let r2 = MockRelay::run().await.expect("relay 2");
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();

    let rt = RelayRuntime::new(
        dialer,
        vec![
            r1.url().await.to_string(),
            r2.url().await.to_string(),
            "ws://127.0.0.1:9".to_string(), // dead — never connects
        ],
    );
    rt.publish(&h_tagged_event(&keys, "6d6f6c74", "stored"))
        .await
        .expect("seed");

    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");
    let mut sub = rt.subscribe(filter).await.expect("subscribe");
    assert!(
        sub.synced(RECV_TIMEOUT).await,
        "both LIVE relays sent EOSE; the dead one is not part of the gate"
    );
    // and the stored event arrived (once)
    assert!(sub.recv(RECV_TIMEOUT).await.is_some());
    assert!(sub.recv(Duration::from_millis(300)).await.is_none());
}

/// Step 6 KEYSTONE — the NIP-11 size budget: an event over the smallest
/// relay cap is refused LOUDLY before any relay sees it (the oversized-
/// CheckpointServed case, concept §7 wire-size cliff). The relay must never
/// receive it — a partial publish that advances some cursor is exactly the
/// silent divergence the budget exists to prevent.
#[tokio::test]
async fn an_oversized_event_is_refused_before_any_relay_sees_it() {
    use molt_net::relay_runtime::RelayRuntime;

    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();

    let rt = RelayRuntime::new(dialer, vec![url]).with_size_budget(Some(1024));
    let big = h_tagged_event(&keys, "6d6f6c74", &"x".repeat(2000));
    let err = rt.publish(&big).await.expect_err("over budget must refuse");
    let msg = format!("{err}");
    assert!(msg.contains("1024"), "the refusal names the cap: {msg}");

    // …and nothing reached the relay
    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");
    let mut sub = rt.subscribe(filter).await.expect("subscribe");
    assert!(sub.synced(RECV_TIMEOUT).await);
    assert!(
        sub.recv(Duration::from_millis(300)).await.is_none(),
        "the oversized event must never have been published"
    );
}

/// Step 6 — the NIP-11 probe: one HTTP GET (Accept: application/nostr+json)
/// over the SAME dial path reads the relay's advertised max_message_length.
#[tokio::test]
async fn nip11_probe_reads_the_relay_cap() {
    use molt_net::relay_runtime::probe_nip11_max_message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 2048];
        let n = s.read(&mut buf).await.expect("request");
        let req = String::from_utf8_lossy(&buf[..n]);
        assert!(req.contains("application/nostr+json"), "NIP-11 Accept header: {req}");
        let body = r#"{"name":"test","limitation":{"max_message_length":4096}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/nostr+json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).await.expect("response");
    });

    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let cap = probe_nip11_max_message(&dialer, &format!("ws://127.0.0.1:{port}"))
        .await
        .expect("probe succeeds");
    assert_eq!(cap, Some(4096));
}

/// A cuttable TCP proxy in front of a relay: while enabled it forwards
/// byte-for-byte; "cut" aborts every live forward and refuses new ones —
/// the only way to take a MockRelay down and bring "it" back on the SAME
/// port (the relay itself cannot rebind).
mod proxy {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    pub struct Cuttable {
        pub port: u16,
        enabled: Arc<AtomicBool>,
        forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    }

    impl Cuttable {
        pub async fn run(target: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
            let port = listener.local_addr().expect("addr").port();
            let enabled = Arc::new(AtomicBool::new(true));
            let forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
                Arc::new(Mutex::new(Vec::new()));
            let on = enabled.clone();
            let fw = forwards.clone();
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    if !on.load(Ordering::SeqCst) {
                        drop(inbound); // refuse while cut
                        continue;
                    }
                    let target = target.clone();
                    fw.lock().await.push(tokio::spawn(async move {
                        if let Ok(mut outbound) = TcpStream::connect(&target).await {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
                                .await;
                        }
                    }));
                }
            });
            Self { port, enabled, forwards }
        }

        pub async fn cut(&self) {
            self.enabled.store(false, Ordering::SeqCst);
            for f in self.forwards.lock().await.drain(..) {
                f.abort();
            }
        }

        pub fn restore(&self) {
            self.enabled.store(true, Ordering::SeqCst);
        }
    }
}

/// Step 7 KEYSTONE — supervision: a relay that dies mid-subscription goes
/// Down in `health()`, the supervisor reconnects with backoff when it
/// returns, RE-SUBSCRIBES from the cursor (overlap redeliveries absorbed by
/// the dedup ring), and live events flow again. The gap event — published
/// while the relay was cut — arrives after the heal: nothing is lost.
#[tokio::test]
async fn a_dying_relay_reconnects_and_the_gap_is_healed() {
    use molt_net::relay_runtime::{RelayHealth, RelayRuntime};

    let relay = MockRelay::run().await.expect("relay");
    let direct = relay.url().await.to_string();
    let target = direct.trim_start_matches("ws://").to_string();
    let proxy = proxy::Cuttable::run(target).await;
    let proxied = format!("ws://127.0.0.1:{}", proxy.port);

    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();
    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");

    // the runtime sees ONLY the proxied relay; publishing rides a direct
    // second runtime (a different "device") so the proxy only carries the
    // subscription under test
    let rt = RelayRuntime::new(dialer.clone(), vec![proxied.clone()])
        .with_backoff(Duration::from_millis(100), Duration::from_millis(400));
    let publisher = RelayRuntime::new(dialer, vec![direct]);

    let mut sub = rt.subscribe(filter).await.expect("subscribe via proxy");
    assert!(sub.synced(RECV_TIMEOUT).await, "initial sync through the proxy");

    let e1 = h_tagged_event(&keys, "6d6f6c74", "before the cut");
    publisher.publish(&e1).await.expect("publish e1");
    assert_eq!(
        sub.recv(RECV_TIMEOUT).await.expect("e1 arrives").id,
        e1.id
    );

    // the relay "dies"
    proxy.cut().await;
    let e2 = h_tagged_event(&keys, "6d6f6c74", "during the gap");
    publisher.publish(&e2).await.expect("publish e2 while cut");
    // the runtime notices: the proxied relay is not Up
    let mut waited = 0;
    loop {
        let health = rt.health().await;
        if health.get(&proxied) != Some(&RelayHealth::Up) {
            break;
        }
        waited += 1;
        assert!(waited < 100, "the dead relay never left Up: {health:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // the relay "returns" — the supervisor reconnects and resubscribes from
    // the cursor, so the gap event arrives; the dedup ring absorbs overlap
    // redeliveries of e1
    proxy.restore();
    let mut got_gap = false;
    for _ in 0..100 {
        if let Some(ev) = sub.recv(Duration::from_millis(200)).await {
            if ev.id == e2.id {
                got_gap = true;
                break;
            }
            assert_eq!(ev.id, e1.id, "only e1 may be redelivered (dedup lets it through once at most)");
        }
    }
    assert!(got_gap, "the gap event must arrive after the heal");
    assert_eq!(
        rt.health().await.get(&proxied),
        Some(&RelayHealth::Up),
        "the healed relay reads Up"
    );
}

/// Step 9 KEYSTONE — NIP-42 on the SUBSCRIBE connection: an auth-required
/// relay never replays to an unauthenticated REQ (the blind runtime
/// honestly never syncs); with the transport anchor's keys the reader
/// answers the challenge, re-places the REQ, and the backlog flows.
/// Publishing stays unauthenticated by design (mdk_eval §5 — an
/// authenticated publish channel would link every ephemeral-key event to
/// the member).
#[tokio::test]
async fn nip42_auth_unlocks_an_auth_required_relay() {
    use molt_net::relay_runtime::RelayRuntime;
    use nostr_relay_builder::builder::{RelayBuilderNip42, RelayBuilderNip42Mode};
    use nostr_relay_builder::{LocalRelay, RelayBuilder};

    let relay = LocalRelay::new(
        RelayBuilder::default()
            .nip42(RelayBuilderNip42 { mode: RelayBuilderNip42Mode::Read }),
    );
    relay.run().await.expect("run auth relay");
    let url = relay.url().await.to_string();
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let keys = Keys::generate();
    let anchor = Keys::generate(); // the per-republic transport anchor

    let filter = Filter::new()
        .kind(Kind::Custom(445))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "6d6f6c74");

    // mode Read: writing needs no auth — seed an event
    let plain = RelayRuntime::new(dialer.clone(), vec![url.clone()]);
    let ev = h_tagged_event(&keys, "6d6f6c74", "step9");
    plain.publish(&ev).await.expect("unauthenticated write in Read mode");

    // without auth keys: the REQ is placed but never replayed — no sync
    let mut blind = plain.subscribe(filter.clone()).await.expect("blind subscribe");
    assert!(
        !blind.synced(Duration::from_secs(2)).await,
        "an auth-required relay must not sync without keys"
    );
    drop(blind);

    // with the anchor: challenge answered, REQ re-placed, backlog arrives
    let authed = RelayRuntime::new(dialer, vec![url]).with_auth_keys(Some(anchor));
    let mut sub = authed.subscribe(filter).await.expect("authed subscribe");
    assert!(sub.synced(RECV_TIMEOUT).await, "authed sync completes");
    assert_eq!(sub.recv(RECV_TIMEOUT).await.expect("the event").id, ev.id);
}
