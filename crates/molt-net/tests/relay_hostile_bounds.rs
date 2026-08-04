// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **buzz B3 steps 1 and 3** — the two hostile-relay bounds that already
//! existed but nothing observed (`docs/reviews/buzz_followups.md` §B3).
//!
//! We are the client against relays we do not control, which is buzz's problem
//! mirrored. Two of their four bounds were already in the code when B3 was
//! re-verified (2026-08-02): the WebSocket frame cap and the read timeout that
//! kills a connection making no progress. Both were UNPINNED — deleting either
//! left the suite green — so these tests pin what exists rather than adding a
//! bound.
//!
//! The third bound, the per-REQ stored-event cap, is in
//! `relay_flood_bound.rs`. The fourth (subscriptions per connection) is
//! buzz's server-side concern and has no client mirror: we place one REQ per
//! connection.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use molt_net::dial::Dialer;
use molt_net::relay_runtime::RelayRuntime;
use nostr::{Filter, Kind};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

/// A relay double that answers every REQ with EOSE and then sends ONE text
/// message of `blob_bytes` junk — the shape a hostile relay uses to make a
/// client allocate. It counts accepted connections, which is what tells a
/// refused frame (the session dies and the supervisor redials) apart from a
/// merely unparseable one (the session lives on).
async fn blob_double(blob_bytes: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let counter = counter.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return; // the best-effort NIP-11 HTTP GET
                };
                counter.fetch_add(1, Ordering::SeqCst);
                while let Some(Ok(msg)) = ws.next().await {
                    let Message::Text(t) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
                        continue;
                    };
                    if v.get(0).and_then(|s| s.as_str()) != Some("REQ") {
                        continue;
                    }
                    let sub = v.get(1).and_then(|s| s.as_str()).unwrap_or("").to_string();
                    let _ = ws
                        .send(Message::text(serde_json::json!(["EOSE", sub]).to_string()))
                        .await;
                    // …and then the blob. Junk, deliberately: the cap must
                    // refuse it at the WebSocket layer, before anything looks
                    // at what it says.
                    let _ = ws.send(Message::text("x".repeat(blob_bytes))).await;
                }
            });
        }
    });
    (format!("ws://{addr}"), connections)
}

/// **A relay may not push a message past the frame cap** (`relay_ws.rs`
/// `MAX_WS_MESSAGE`, 1 MiB on both `max_message_size` and `max_frame_size`).
///
/// tungstenite's defaults are 64 MiB per message and 16 MiB per frame, ~500×
/// anything this client exchanges — a hostile relay could force that
/// allocation per connection, per pool relay.
///
/// The observable is the RECONNECT, and it has to be: a client that silently
/// dropped an oversized message would look identical to one that refused it.
/// A refused frame is a protocol error, so the session dies and the
/// supervisor redials; the control below proves this is the cap's doing and
/// not "any junk kills a session".
///
/// **Prove red:** drop the `.max_message_size(…)`/`.max_frame_size(…)` pair
/// in `RelayWs::connect` — tungstenite then accepts the 2 MiB blob, `recv`
/// returns a Framing error instead of a broken stream, the ping still
/// succeeds, and the session never dies (connections stay at 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_past_the_frame_cap_kills_the_session() {
    // 2 MiB: past the 1 MiB cap, nowhere near tungstenite's own default
    let (url, connections) = blob_double(2 * 1024 * 1024).await;
    let filter = Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP));
    let mut sub = RelayRuntime::new(dialer(), vec![url])
        .with_backoff(Duration::from_millis(50), Duration::from_millis(200))
        .subscribe(filter)
        .await
        .expect("subscribe");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while connections.load(Ordering::SeqCst) < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the oversized message was swallowed instead of refused: the session \
             survived it ({} connection(s))",
            connections.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        sub.recv(Duration::from_millis(200)).await.is_none(),
        "nothing in that blob is an event — it must never reach the consumer"
    );
}

/// The control: the SAME junk, under the cap, does NOT kill the session.
///
/// Without this the test above would pass on a client that dies on any
/// unparseable frame, which is a different (and worse) property — a relay's
/// stray NOTICE would then cost a reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn junk_under_the_frame_cap_leaves_the_session_alive() {
    let (url, connections) = blob_double(64 * 1024).await;
    let filter = Filter::new().kind(Kind::Custom(molt_net::kinds::KIND_GROUP));
    let _sub = RelayRuntime::new(dialer(), vec![url])
        .with_backoff(Duration::from_millis(50), Duration::from_millis(200))
        .subscribe(filter)
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "an unparseable but BOUNDED frame is skipped, not a session death"
    );
}

/// A relay double that completes the handshake, takes our EVENT, and then
/// makes no progress: it pings forever and never sends the OK. Transport-live,
/// application-dead — the dribbler B3 names.
async fn dribbler() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut tx, mut rx) = ws.split();
                // keep reading so the client's send never blocks…
                tokio::spawn(async move { while let Some(Ok(_)) = rx.next().await {} });
                // …and answer nothing but pings, forever
                loop {
                    if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }
    });
    format!("ws://{addr}")
}

/// **A relay that stays live and answers nothing cannot pin a publish**
/// (`relay_runtime.rs` `PUBLISH_TIMEOUT`, inside `publish_one`'s recv loop).
///
/// The subtlety is in `RelayWs::recv`: its deadline is computed ONCE, before
/// the skip loop, so transport frames the loop steps over — pings here, a
/// NOTICE stream in the wild — cannot extend it. A deadline re-armed per
/// frame would let a chatty relay hold the attempt open forever while making
/// no progress.
///
/// **Prove red:** move `let deadline = …` inside `RelayWs::recv`'s loop, or
/// drop the `PUBLISH_TIMEOUT` from `publish_one`'s `recv` call — this publish
/// then never returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_answers_nothing_cannot_pin_a_publish() {
    let url = dribbler().await;
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(Kind::Custom(molt_net::kinds::KIND_GROUP), "hello")
        .sign_with_keys(&keys)
        .expect("sign");

    let started = tokio::time::Instant::now();
    let outcome = RelayRuntime::new(dialer(), vec![url.clone()])
        .publish(&event)
        .await;
    let elapsed = started.elapsed();

    // a pool where NO relay accepted is an error, not a report of zero — the
    // caller must not be able to read it as a successful publish
    let Err(e) = outcome else {
        panic!("a relay that never acknowledged anything must not count as accepting");
    };
    assert!(
        e.to_string().contains("timed out"),
        "the reason must name the timeout, not a refusal the relay never gave: {e}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the publish took {elapsed:?} — a dribbling relay pinned the attempt"
    );
}

/// A double that accepts the TCP connection and then says NOTHING — the
/// WebSocket upgrade never completes. `_held` keeps the sockets alive: a
/// dropped stream would close the connection and hand the client an error it
/// is not supposed to get here.
async fn silent_tcp() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut _held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            _held.push(stream);
        }
    });
    format!("ws://{addr}")
}

/// **…and neither can one that never finishes the handshake**
/// (`relay_runtime.rs`, the `tokio::time::timeout(PUBLISH_TIMEOUT, …)`
/// wrapper around the whole `publish_one`).
///
/// This is the shape the inner bound cannot see. `publish_one` calls
/// `RelayWs::connect` directly, and the WebSocket upgrade has no deadline of
/// its own there — the dial and TLS carry theirs, but a relay that accepts
/// TCP and then stays mute is past both. Only the outer timeout ends it.
///
/// **Prove red** (verified 2026-08-04): drop that wrapper and the publish
/// never returns — the test hangs until the harness kills it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_never_finishes_the_handshake_cannot_pin_a_publish() {
    let url = silent_tcp().await;
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(Kind::Custom(molt_net::kinds::KIND_GROUP), "hello")
        .sign_with_keys(&keys)
        .expect("sign");

    let started = tokio::time::Instant::now();
    let outcome = RelayRuntime::new(dialer(), vec![url])
        .publish(&event)
        .await;
    let elapsed = started.elapsed();

    assert!(
        outcome.is_err(),
        "a relay that never spoke must not count as accepting"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the publish took {elapsed:?} — the handshake pinned the attempt"
    );
}
