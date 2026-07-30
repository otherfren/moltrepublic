// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! ListObjectsV2 against an in-process HTTP stub on 127.0.0.1 (the
//! `s3_probe.rs` posture): asserts the wire shape (a SigV4-signed GET with
//! the canonical, sorted query), continuation-token pagination, and the
//! honest error classes on failure. Plain HTTP on loopback — the TLS path
//! is the same stack the probe tests cover.

use std::sync::{Arc, Mutex};

use molt_net::s3::{S3Client, S3Config, S3Error, S3Object};
use molt_net::dial::Dialer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A multi-shot S3 stub: serves one canned `(status, body)` response per
/// accepted connection, in order; records each request head.
async fn stub_server(pages: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        for (status, body) in pages {
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
            record
                .lock()
                .expect("record lock")
                .push(String::from_utf8_lossy(&buf).to_string());
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.expect("write response");
            sock.shutdown().await.ok();
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn client_for(endpoint: &str) -> S3Client {
    let config = S3Config::from_settings(endpoint, "AKIAEXAMPLE", "secret-example", "molt-bucket")
        .expect("config parses");
    S3Client::new(config, Dialer::Direct)
}

fn contents(key: &str, size: u64, modified: &str) -> String {
    format!(
        "<Contents><Key>{key}</Key><LastModified>{modified}</LastModified>\
         <Size>{size}</Size></Contents>"
    )
}

#[tokio::test]
async fn list_sends_a_signed_canonical_get_and_parses_the_objects() {
    let body = format!(
        "<ListBucketResult><IsTruncated>false</IsTruncated>{}{}</ListBucketResult>",
        contents("molt/aa/001.molt.enc", 4096, "2013-05-24T00:00:00.000Z"),
        contents("molt/bb/002.molt.enc", 7, "2015-08-30T12:36:00Z"),
    );
    let (endpoint, seen) = stub_server(vec![(200, body)]).await;
    let objects = client_for(&endpoint)
        .list_objects("molt/")
        .await
        .expect("listing succeeds");
    assert_eq!(
        objects,
        vec![
            S3Object {
                key: "molt/aa/001.molt.enc".to_string(),
                size: 4096,
                modified: 1_369_353_600,
            },
            S3Object {
                key: "molt/bb/002.molt.enc".to_string(),
                size: 7,
                modified: 1_440_938_160,
            },
        ]
    );
    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(seen.len(), 1, "one page, one request");
    // wire path/query must be the canonical (sorted, encoded) form the
    // signature covers: list-type before prefix, the slash percent-encoded
    assert!(
        seen[0].starts_with("GET /molt-bucket?list-type=2&prefix=molt%2F HTTP/1.1"),
        "canonical query on the wire: {}",
        seen[0]
    );
    assert!(
        seen[0].contains("Authorization: AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"),
        "the listing is SigV4-signed: {}",
        seen[0]
    );
}

#[tokio::test]
async fn list_follows_the_continuation_token_across_pages() {
    let page1 = format!(
        "<ListBucketResult><IsTruncated>true</IsTruncated>\
         <NextContinuationToken>tok+1=</NextContinuationToken>{}</ListBucketResult>",
        contents("molt/aa/001.molt.enc", 1, "1970-01-01T00:00:10Z"),
    );
    let page2 = format!(
        "<ListBucketResult><IsTruncated>false</IsTruncated>{}</ListBucketResult>",
        contents("molt/aa/002.molt.enc", 2, "1970-01-01T00:00:20Z"),
    );
    let (endpoint, seen) = stub_server(vec![(200, page1), (200, page2)]).await;
    let objects = client_for(&endpoint)
        .list_objects("molt/")
        .await
        .expect("paged listing succeeds");
    assert_eq!(objects.len(), 2, "both pages' objects arrive: {objects:?}");
    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(seen.len(), 2, "two pages, two requests");
    assert!(
        seen[1].starts_with(
            "GET /molt-bucket?continuation-token=tok%2B1%3D&list-type=2&prefix=molt%2F HTTP/1.1"
        ),
        "the second request carries the encoded token: {}",
        seen[1]
    );
}

#[tokio::test]
async fn list_maps_403_to_the_credentials_class() {
    let (endpoint, _seen) = stub_server(vec![(403, String::new())]).await;
    let err = client_for(&endpoint)
        .list_objects("molt/")
        .await
        .expect_err("403 is an error");
    let S3Error::Http { status: 403, hint } = err else {
        panic!("expected the http 403 class, got {err:?}");
    };
    assert!(hint.contains("access key"), "hint names the credentials: {hint}");
}

#[tokio::test]
async fn a_body_that_is_not_a_listing_is_a_protocol_error() {
    // a captive portal / proxy answering 200 with HTML must never read as
    // a valid empty listing
    let (endpoint, _seen) =
        stub_server(vec![(200, "<html>captive portal</html>".to_string())]).await;
    let err = client_for(&endpoint)
        .list_objects("molt/")
        .await
        .expect_err("garbage body is an error");
    assert!(
        matches!(err, S3Error::Protocol(_)),
        "expected the protocol class, got {err:?}"
    );
}
