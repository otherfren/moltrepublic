// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A connection that fails must SAY why.**
//!
//! Reported from real use (2026-08-01): Tor was running and the proxy worked,
//! but the client "could not connect" and the console showed nothing at all.
//! The cause was one line in `relay_runtime::supervise` — the connect result
//! was matched with a bare `_`, so every failure (proxy refused, TLS
//! rejected, WS upgrade 4xx, auth required) was discarded and the loop
//! retried forever in silence.
//!
//! Silence is the bug. These tests pin the diagnostic itself: the relay, the
//! ROUTE it was dialed over, and the reason must reach the log. This file is
//! its own test binary, so it may install a global subscriber.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::relay_runtime::RelayRuntime;

/// A `MakeWriter` that appends every formatted event into a shared buffer.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The ONE buffer this binary captures into. A global subscriber can be
/// installed exactly once, so every test must read the same buffer — three
/// tests each making their own would leave two of them permanently empty
/// (the first `try_init` wins and binds to its own).
fn capture_logs() -> Arc<Mutex<Vec<u8>>> {
    static BUF: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    let buf = BUF.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let _ = tracing_subscriber::fmt()
            .with_writer(Capture(Arc::clone(&buf)))
            .with_max_level(tracing::Level::DEBUG)
            // no colour: the buffer is matched as plain text, and ANSI codes
            // sit between the field name and its `=`
            .with_ansi(false)
            .without_time()
            .try_init();
        buf
    });
    Arc::clone(buf)
}

fn logged(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().expect("log buffer")).to_string()
}

/// A port that is bound and immediately dropped — nothing listens there.
/// (Never port 9: a host running the discard service would invert the test.)
fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let p = l.local_addr().expect("addr").port();
    drop(l);
    p
}

/// KEYSTONE — a relay that cannot be reached logs the RELAY, the ROUTE and
/// the REASON on ONE line. Verified red by deleting the `Ok(Err(e))` arm in
/// `subscribe`'s first-connect; before the fix that arm was
/// `.ok().and_then(Result::ok)` and nothing was emitted at all.
///
/// KNOWN GAP: this pins the FIRST connect (what an operator meets at
/// startup). `supervise`'s reconnect reports through the same shape but is
/// not pinned here — that needs a relay that connects and then dies, i.e.
/// the cuttable proxy in `tests/nostr_relay_runtime.rs::proxy`. Deleting the
/// supervise warn alone leaves this test green.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_relay_logs_the_reason_not_silence() {
    let buf = capture_logs();
    let url = format!("ws://127.0.0.1:{}", dead_port());

    let rt = RelayRuntime::new(Dialer::Direct, vec![url.clone()])
        .with_backoff(Duration::from_millis(20), Duration::from_millis(40));
    let sub = rt
        .subscribe(nostr::Filter::new().kind(nostr::Kind::Custom(445)))
        .await;
    // the subscribe itself may fail typed (nothing accepted the REQ); either
    // way the supervisor must have tried and reported. Hold the stream so the
    // supervisors are not aborted before they log.
    let _keep = sub;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ONE line must carry relay + route + cause TOGETHER. Asserting the three
    // separately is how a diagnostics test lies about itself: the url and
    // `via=` also appear in unrelated DEBUG dial lines, so the weak form
    // stays green with the connect-failure report deleted.
    let log = logged(&buf);
    let reported = log.lines().find(|l| {
        (l.contains("relay connect failed") || l.contains("relay connect timed out"))
            && l.contains(&url)
            && l.contains("via=direct")
            && (l.contains("error=") || l.contains("after_s="))
    });
    assert!(
        reported.is_some(),
        "no single line carries relay + route + cause, got:\n{log}"
    );
}

/// An empty pool connects to nothing BY DESIGN (ADR-0004), which from the
/// outside looks exactly like a broken connection. It must say so, or the
/// operator debugs Tor for an hour over a config that was never going to dial.
#[test]
fn an_empty_pool_says_it_will_contact_nothing() {
    let buf = capture_logs();
    let _rt = RelayRuntime::new(Dialer::Direct, Vec::new());
    let log = logged(&buf);
    assert!(
        log.contains("no dialable relay"),
        "an empty pool is announced, not silent: {log}"
    );
}

/// The route is the field that separates "Tor is broken" from "this never
/// went through Tor at all" — the operator's actual question.
#[test]
fn the_route_names_the_proxy_it_would_use() {
    assert_eq!(Dialer::Direct.route(), "direct");
    let socks = Dialer::resolve("tor", "local", 9050).expect("socks dialer");
    assert_eq!(socks.route(), "socks5://127.0.0.1:9050");
    let whonix = Dialer::resolve("tor", "whonix", 0).expect("whonix dialer");
    assert_eq!(whonix.route(), "socks5://10.152.152.10:9050");
}
