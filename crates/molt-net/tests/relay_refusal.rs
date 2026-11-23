// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A relay that refuses a publish is answering, not gone
//! (`docs_archive/transport/relay_pool.md` §5.2): a busy `error:` is asked
//! again on the same relay, a verdict (`invalid:`) is not, and only a dead
//! connection reads as unreachable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use molt_net::dial::Dialer;
use molt_net::relay_refusal::refusals;
use molt_net::relay_runtime::PublishPool;
use molt_net::timeouts::RelayTimeouts;
use nostr::{EventBuilder, Keys, Kind};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

fn event() -> nostr::Event {
    EventBuilder::new(Kind::Custom(445), "x").sign_with_keys(&Keys::generate()).expect("sign")
}

/// A relay whose n-th EVENT (0-based) is answered by `answer(n)`; counts
/// every EVENT it got and every one it took.
struct Scripted {
    url: String,
    events: Arc<AtomicUsize>,
    taken: Arc<AtomicUsize>,
}

impl Scripted {
    async fn run(answer: fn(usize) -> (bool, &'static str)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let events = Arc::new(AtomicUsize::new(0));
        let taken = Arc::new(AtomicUsize::new(0));
        let (ev, tk) = (events.clone(), taken.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (ev, tk) = (ev.clone(), tk.clone());
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                    while let Some(Ok(msg)) = ws.next().await {
                        let Message::Text(t) = msg else { continue };
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                        if v.get(0).and_then(|s| s.as_str()) != Some("EVENT") {
                            continue;
                        }
                        let id = v[1].get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let (ok, message) = answer(ev.fetch_add(1, Ordering::SeqCst));
                        if ok {
                            tk.fetch_add(1, Ordering::SeqCst);
                        }
                        let frame = serde_json::json!(["OK", id, ok, message]);
                        let _ = ws.send(Message::text(frame.to_string())).await;
                    }
                });
            }
        });
        Self { url: format!("ws://{addr}"), events, taken }
    }

    fn events(&self) -> usize {
        self.events.load(Ordering::SeqCst)
    }

    fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }
}

fn pool(urls: Vec<String>, publish: Duration) -> PublishPool {
    PublishPool::new(dialer(), urls).with_timeouts(RelayTimeouts { connect: Duration::from_secs(5), publish })
}

/// A busy refusal is asked again on the same relay and lands; the relay
/// is never reported unreachable and its count shows the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_refusal_is_retried_and_lands() {
    let relay = Scripted::run(|n| if n == 0 { (false, "error: database is locked") } else { (true, "") }).await;
    let report = pool(vec![relay.url.clone()], Duration::from_secs(10))
        .publish(&event())
        .await
        .expect("the retry lands");
    assert_eq!(report.accepted, vec![relay.url.clone()]);
    assert_eq!(relay.events(), 2, "asked twice");
    let seen = refusals().get(&relay.url).cloned().expect("counted");
    assert_eq!(seen.busy, 1);
    assert_eq!(seen.last_shape, "busy");
    assert!(seen.last_at > 0);
}

/// With a second relay taking the event at once, the busy one still gets
/// it: relays do not federate, a peer reading only that relay needs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_relay_gets_the_event_after_another_took_it() {
    let fine = Scripted::run(|_| (true, "")).await;
    let busy = Scripted::run(|n| if n == 0 { (false, "rate-limited: slow down") } else { (true, "") }).await;
    let report = pool(vec![fine.url.clone(), busy.url.clone()], Duration::from_secs(10))
        .publish(&event())
        .await
        .expect("published");
    assert!(report.accepted.contains(&fine.url));
    for _ in 0..100 {
        if busy.taken() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(busy.taken(), 1, "the busy relay took it on the retry");
}

/// The wait before asking again does not hold the relay's slot: another
/// frame goes out at once (a held slot stalled every frame for up to a
/// publish deadline behind one rate-limited event).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_wait_does_not_hold_other_frames() {
    let relay = Scripted::run(|n| if n == 0 { (false, "rate-limited: slow down") } else { (true, "") }).await;
    let pool = pool(vec![relay.url.clone()], Duration::from_secs(10));
    let first = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.publish(&event()).await })
    };
    while relay.events() == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let began = tokio::time::Instant::now();
    pool.publish(&event()).await.expect("the second frame lands");
    assert!(began.elapsed() < Duration::from_millis(700), "waited {:?} behind the busy one", began.elapsed());
    first.await.expect("task").expect("the first lands on its retry");
}

/// A verdict is final for this event: one EVENT, no retry, not unreachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_refusal_is_not_retried() {
    let relay = Scripted::run(|_| (false, "invalid: bad sig")).await;
    let err = pool(vec![relay.url.clone()], Duration::from_secs(10))
        .publish(&event())
        .await
        .expect_err("refused");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(relay.events(), 1, "not asked again");
    assert!(err.to_string().contains("relay refused: invalid"), "{err}");
    let seen = refusals().get(&relay.url).cloned().expect("counted");
    assert_eq!((seen.busy, seen.last_shape), (0, "invalid"));
}

/// A relay that always errors costs one publish deadline, not a spin; its
/// connection stays usable for the next event (no breaker).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_always_errors_does_not_spin() {
    let relay = Scripted::run(|_| (false, "error: database is locked")).await;
    let pool = pool(vec![relay.url.clone()], Duration::from_secs(4));
    let began = tokio::time::Instant::now();
    let err = pool.publish(&event()).await.expect_err("never accepted");
    assert!(began.elapsed() < Duration::from_secs(8), "bounded by the deadline");
    let asked = relay.events();
    assert!((2..=4).contains(&asked), "a few backed-off attempts, not a spin: {asked}");
    assert!(err.to_string().contains("relay refused: error: database is locked"), "{err}");
    let _ = pool.publish(&event()).await;
    assert!(relay.events() > asked, "the next event is asked at once - no breaker");
}

/// Connection-level failure is still unreachable, and counts no refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_connection_still_reads_unreachable() {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let p = l.local_addr().expect("addr").port();
        drop(l);
        format!("ws://127.0.0.1:{p}")
    };
    let err = pool(vec![dead.clone()], Duration::from_secs(2)).publish(&event()).await.expect_err("dead");
    assert!(err.to_string().starts_with("unreachable:"), "{err}");
    assert!(!refusals().contains_key(&dead));
}
