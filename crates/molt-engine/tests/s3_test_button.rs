// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The backup settings panel's "Test connection" button drives
//! [`molt_core::Command::NetTestS3`]: the engine builds the S3 config from
//! the request (falling back to the saved settings for empty fields), runs a
//! SigV4-signed `HEAD /bucket` probe **off the actor** through the resolved
//! dialer, and feeds the outcome back as `NetTestS3Result` into
//! `session.s3_test`. The GUI button and the `net_test_s3` MCP tool are thin
//! wrappers over this.
//!
//! No real network: the probe runs against an in-process HTTP stub on
//! 127.0.0.1 (default settings resolve the `Direct` dialer, which allows
//! loopback — fail-closed only constrains Tor configurations).

use std::sync::{Arc, Mutex};

use molt_core::{Command, GroupConfig, Reply, SessionView};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A single-shot S3 stub answering `status`; records the request head.
async fn stub_server(status: u16) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let seen = Arc::new(Mutex::new(String::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        *record.lock().expect("record lock") = String::from_utf8_lossy(&buf).to_string();
        let resp = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
        sock.write_all(resp.as_bytes()).await.expect("write response");
        sock.shutdown().await.ok();
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

/// Issue one NetTestS3 and poll `session.s3_test` until it settles.
async fn run_test(cmd: Command) -> String {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    w.execute(cmd).await.expect("NetTestS3 accepted");
    for _ in 0..150 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !sv.s3_test.is_empty() && sv.s3_test != "testing" {
            return sv.s3_test;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("s3_test never settled");
}

fn cmd_for(endpoint: &str) -> Command {
    Command::NetTestS3 {
        endpoint: endpoint.to_string(),
        access_key: "AKIAEXAMPLE".to_string(),
        secret_key: "secret-example".to_string(),
        bucket: "molt-bucket".to_string(),
    }
}

#[tokio::test]
async fn test_button_reports_ok_and_sends_a_signed_probe() {
    let (endpoint, seen) = stub_server(200).await;
    let result = run_test(cmd_for(&endpoint)).await;
    assert_eq!(result, "ok", "a 200 probe must land as ok");
    let seen = seen.lock().expect("seen lock").clone();
    assert!(
        seen.starts_with("HEAD /molt-bucket HTTP/1.1"),
        "the probe is a path-style HEAD: {seen}"
    );
    assert!(
        seen.contains("Authorization: AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"),
        "the probe is SigV4-signed: {seen}"
    );
}

#[tokio::test]
async fn test_button_reports_the_credentials_class_on_403() {
    let (endpoint, _seen) = stub_server(403).await;
    let result = run_test(cmd_for(&endpoint)).await;
    assert!(
        result.starts_with("error:") && result.contains("403"),
        "403 → honest error, got {result:?}"
    );
    assert!(
        result.contains("access key"),
        "the 403 class names the credentials: {result:?}"
    );
}

#[tokio::test]
async fn test_button_fails_fast_without_an_endpoint_and_no_network() {
    // nothing configured, nothing passed: the config validation fails
    // in-actor, no dial ever happens
    let result = run_test(Command::NetTestS3 {
        endpoint: String::new(),
        access_key: String::new(),
        secret_key: String::new(),
        bucket: String::new(),
    })
    .await;
    assert!(
        result.starts_with("error:") && result.contains("endpoint"),
        "missing endpoint → in-actor error, got {result:?}"
    );
}

#[tokio::test]
async fn empty_fields_fall_back_to_the_saved_settings() {
    let (endpoint, _seen) = stub_server(200).await;
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    // save the S3 settings, then test with every field empty
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    let mut settings = sv.settings.clone();
    settings.s3_endpoint = endpoint;
    settings.s3_access_key = "AKIAEXAMPLE".to_string();
    settings.s3_secret_key = "secret-example".to_string();
    settings.s3_bucket = "molt-bucket".to_string();
    w.execute(Command::SaveSettings { settings }).await.expect("settings saved");
    w.execute(Command::NetTestS3 {
        endpoint: String::new(),
        access_key: String::new(),
        secret_key: String::new(),
        bucket: String::new(),
    })
    .await
    .expect("NetTestS3 accepted");
    for _ in 0..150 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !sv.s3_test.is_empty() && sv.s3_test != "testing" {
            assert_eq!(sv.s3_test, "ok", "saved settings drive the probe");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("s3_test never settled");
}
