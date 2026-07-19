// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The settings panel's "Test connection" button drives
//! [`molt_core::Command::NetTestServer`]. This exercises that exact command
//! path on a real engine: parse the URL, run the TLS handshake off the
//! actor, feed the result back as `NetTestResult`, and land it in
//! `session.smp_test`. The GUI button is a thin wrapper over this.
//!
//! `#[ignore]` (real network):
//! `cargo test -p molt-engine --test smp_test_button -- --ignored --nocapture`

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

/// Drive one test and poll `session.smp_test` until it settles (not "" and
/// not "testing"), up to ~15s.
async fn run_test(url: &str) -> String {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    w.execute(Command::NetTestServer {
        url: url.to_string(),
        anonymity: String::new(),
        tor_mode: String::new(),
        tor_port: 0,
    })
    .await
    .expect("NetTestServer accepted");
    for _ in 0..150 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !sv.smp_test.is_empty() && sv.smp_test != "testing" {
            return sv.smp_test;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("smp_test never settled");
}

#[tokio::test]
#[ignore = "dials the real smp.konkin.io"]
async fn test_button_reports_reachable_for_a_live_server() {
    let result = run_test(KONKIN).await;
    assert_eq!(result, "ok", "a reachable server must report ok");
    println!("OK: live server → session.smp_test = {result:?}");
}

#[tokio::test]
#[ignore = "resolves a bogus host (no network dependency on a real server)"]
async fn test_button_reports_error_for_an_unreachable_server() {
    // valid URL shape, 32-byte fingerprint, but a host that will not connect
    let bogus = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid";
    let result = run_test(bogus).await;
    assert!(result.starts_with("error:"), "unreachable → error, got {result:?}");
    println!("OK: unreachable server → session.smp_test = {result:?}");
}

/// The settings form's Test button must probe with the DRAFT anonymity
/// values, not the saved ones: a user who just flipped tor→none in the form
/// expects the probe to go direct, even though the saved config still says
/// tor. Saved = a misconfigured tor (fails the dialer resolve synchronously);
/// the draft override "none" must bypass that failure entirely.
#[tokio::test]
async fn test_button_uses_the_draft_anonymity_over_the_saved_one() {
    let session = SessionView {
        settings: SessionSettings {
            anonymity: "tor".to_string(),
            tor_mode: "broken-mode".to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::spawn(GroupConfig::demo(), session);
    let url = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid";

    // without a draft override the saved (broken) tor settings fail the
    // probe before any dial
    w.execute(Command::NetTestServer {
        url: url.to_string(),
        anonymity: String::new(),
        tor_mode: String::new(),
        tor_port: 0,
    })
    .await
    .expect("NetTestServer accepted");
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    assert!(
        sv.smp_test.starts_with("error:") && sv.smp_test.contains("tor"),
        "saved broken tor must fail the probe, got {:?}",
        sv.smp_test
    );

    // the draft override "none" routes direct: the probe passes the dialer
    // stage and actually starts (the async outcome itself needs no network
    // assertion here — not hitting the tor error is the point)
    w.execute(Command::NetTestServer {
        url: url.to_string(),
        anonymity: "none".to_string(),
        tor_mode: String::new(),
        tor_port: 0,
    })
    .await
    .expect("NetTestServer accepted");
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    assert!(
        !sv.smp_test.contains("tor"),
        "draft anonymity=none must bypass the saved tor config, got {:?}",
        sv.smp_test
    );
    println!("OK: draft none overrode saved broken tor → {:?}", sv.smp_test);
}

#[tokio::test]
async fn test_button_rejects_a_malformed_url_without_network() {
    // not #[ignore]: a syntactically invalid URL fails in-actor, no dialing
    let result = run_test("not-an-smp-url").await;
    assert!(result.starts_with("error:"), "malformed → error, got {result:?}");
    println!("OK: malformed url → session.smp_test = {result:?}");
}
