// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! `put_object` / `delete_object` against an in-process HTTP stub on
//! 127.0.0.1 (the `s3_probe.rs` posture): asserts the exact signed wire
//! shape — method, path, the body's REAL SHA-256 in `x-amz-content-sha256`,
//! an internally consistent SigV4 Authorization header — and the honest
//! status mapping (2xx confirmed, 403 credentials class, anything else an
//! honest HTTP error, connect failures the connect class). Plain HTTP on
//! loopback: TLS-in-test would prove nothing about signing.

use std::sync::{Arc, Mutex};

use molt_net::s3::{sigv4, S3Client, S3Config, S3Error};
use molt_net::smp::tls::Dialer;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One recorded request: request line, lowercased headers, raw body.
#[derive(Debug, Clone, Default)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A single-shot stub: accepts one connection, records head + body (by
/// Content-Length), answers `status`, closes.
async fn stub_server(status: u16) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let seen = Arc::new(Mutex::new(Seen::default()));
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
        let head_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..head_end - 4.min(head_end)]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = buf[head_end..].to_vec();
        while body.len() < content_length {
            let n = sock.read(&mut chunk).await.expect("read body");
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        {
            let mut seen = record.lock().expect("record lock");
            seen.request_line = request_line;
            seen.headers = headers;
            seen.body = body;
        }
        let resp = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
        sock.write_all(resp.as_bytes()).await.expect("write response");
        sock.shutdown().await.ok();
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn client_for(endpoint: &str) -> S3Client {
    let config = S3Config::from_settings(endpoint, "AKIAEXAMPLE", "secret-example", "molt-bucket")
        .expect("config parses");
    S3Client::new(config, Dialer::Direct)
}

#[tokio::test]
async fn put_object_sends_the_signed_body_and_maps_2xx_to_confirmed() {
    let (endpoint, seen) = stub_server(200).await;
    let body = b"molt-export-v1\0pretend-blob-bytes".to_vec();
    let key = format!("molt/{}/001752800000.molt.enc", "ab".repeat(32));
    client_for(&endpoint)
        .put_object(&key, &body)
        .await
        .expect("2xx is a confirmed upload");

    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.request_line,
        format!("PUT /molt-bucket/{key} HTTP/1.1"),
        "path-style object path"
    );
    assert_eq!(seen.body, body, "the blob arrives verbatim");
    // the payload hash is the body's REAL sha256 — and it is signed
    let payload_hash = hex::encode(Sha256::digest(&body));
    assert_eq!(seen.header("x-amz-content-sha256"), Some(payload_hash.as_str()));
    let datetime = seen.header("x-amz-date").expect("x-amz-date").to_string();
    let host = seen.header("host").expect("host header").to_string();
    let headers = vec![
        ("host".to_string(), host),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), datetime.clone()),
    ];
    let expect = sigv4::authorization_header(&sigv4::SignParams {
        method: "PUT",
        uri: &format!("/molt-bucket/{key}"),
        query: &[],
        headers: &headers,
        payload_hash: &payload_hash,
        datetime: &datetime,
        region: "us-east-1",
        service: "s3",
        access_key: "AKIAEXAMPLE",
        secret_key: "secret-example",
    });
    assert_eq!(
        seen.header("authorization"),
        Some(expect.as_str()),
        "authorization must match the signed request"
    );
}

#[tokio::test]
async fn put_object_maps_403_to_the_credentials_class_never_success() {
    let (endpoint, _seen) = stub_server(403).await;
    let err = client_for(&endpoint)
        .put_object("molt/x", b"data")
        .await
        .expect_err("403 must NOT read as a stored backup");
    let S3Error::Http { status: 403, hint } = err else {
        panic!("expected the http 403 class, got {err:?}");
    };
    assert!(hint.contains("access key"), "hint names the credentials: {hint}");
}

/// A 403 whose S3 error body says `RequestTimeTooSkewed` must not be blamed
/// on the credentials — the honest cause (a skewed local clock) is in the
/// `<Code>`, and the hint has to reflect it.
#[tokio::test]
async fn put_object_403_reports_the_s3_code_not_only_credentials() {
    let xml = "<?xml version=\"1.0\"?><Error><Code>RequestTimeTooSkewed</Code>\
               <Message>The difference between the request time and the current time is too large.</Message></Error>";
    let mut response =
        format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n", xml.len()).into_bytes();
    response.extend_from_slice(xml.as_bytes());
    let (endpoint, _seen) = raw_stub(response).await;
    let err = client_for(&endpoint)
        .put_object("molt/x", b"data")
        .await
        .expect_err("403 is an error");
    let S3Error::Http { status: 403, hint } = err else {
        panic!("expected http 403, got {err:?}");
    };
    let low = hint.to_ascii_lowercase();
    assert!(
        low.contains("clock") || low.contains("skew") || low.contains("time"),
        "the skew cause must surface, not just credentials: {hint}"
    );
    assert!(
        !hint.contains("access key"),
        "a clock-skew 403 must not blame the credentials: {hint}"
    );
}

/// A different S3 `<Code>` (here `SignatureDoesNotMatch`) is folded into the
/// credentials hint so the operator sees which check failed.
#[tokio::test]
async fn put_object_403_folds_the_s3_code_into_the_hint() {
    let xml = "<Error><Code>SignatureDoesNotMatch</Code></Error>";
    let mut response =
        format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n", xml.len()).into_bytes();
    response.extend_from_slice(xml.as_bytes());
    let (endpoint, _seen) = raw_stub(response).await;
    let err = client_for(&endpoint)
        .put_object("molt/x", b"data")
        .await
        .expect_err("403 is an error");
    let S3Error::Http { status: 403, hint } = err else {
        panic!("expected http 403, got {err:?}");
    };
    assert!(hint.contains("SignatureDoesNotMatch"), "the code is surfaced: {hint}");
}

#[tokio::test]
async fn put_object_maps_5xx_to_an_honest_http_error() {
    let (endpoint, _seen) = stub_server(503).await;
    let err = client_for(&endpoint)
        .put_object("molt/x", b"data")
        .await
        .expect_err("503 must NOT read as a stored backup");
    assert!(
        matches!(err, S3Error::Http { status: 503, .. }),
        "expected http 503, got {err:?}"
    );
}

#[tokio::test]
async fn delete_object_sends_a_signed_delete_and_accepts_204() {
    let (endpoint, seen) = stub_server(204).await;
    client_for(&endpoint)
        .delete_object("molt/aa/001.molt.enc")
        .await
        .expect("204 is a successful delete");
    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.request_line,
        "DELETE /molt-bucket/molt/aa/001.molt.enc HTTP/1.1"
    );
    assert!(seen.body.is_empty(), "a delete carries no body");
    assert_eq!(
        seen.header("x-amz-content-sha256"),
        Some(sigv4::EMPTY_PAYLOAD_SHA256),
        "empty payload hash is signed"
    );
    assert!(seen.header("authorization").is_some(), "delete is signed");
}

#[tokio::test]
async fn delete_object_surfaces_a_403_honestly() {
    let (endpoint, _seen) = stub_server(403).await;
    let err = client_for(&endpoint)
        .delete_object("molt/x")
        .await
        .expect_err("403 is an error, never a silent prune");
    assert!(matches!(err, S3Error::Http { status: 403, .. }), "{err:?}");
}

/// A single-shot stub answering one request with raw response bytes.
async fn raw_stub(response: Vec<u8>) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let seen = Arc::new(Mutex::new(Seen::default()));
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
        {
            let head = String::from_utf8_lossy(&buf);
            let mut lines = head.split("\r\n");
            let mut seen = record.lock().expect("record lock");
            seen.request_line = lines.next().unwrap_or_default().to_string();
        }
        sock.write_all(&response).await.expect("write response");
        sock.shutdown().await.ok();
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

/// The download is streamed, byte-exact, with monotonic progress that
/// ends at the declared total — and the request is a signed GET on the
/// path-style object path.
#[tokio::test]
async fn get_object_streams_the_body_with_honest_progress() {
    let body: Vec<u8> = (0..100_000u32).map(|i| u8::try_from(i % 251).expect("byte")).collect();
    let mut response =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    response.extend_from_slice(&body);
    let (endpoint, seen) = raw_stub(response).await;

    let mut sink: Vec<u8> = Vec::new();
    let mut seen_progress: Vec<(u64, Option<u64>)> = Vec::new();
    let bytes = client_for(&endpoint)
        .get_object(
            "molt/aa/001.molt.enc",
            &mut sink,
            10 * 1024 * 1024,
            &mut |done, total| seen_progress.push((done, total)),
        )
        .await
        .expect("download succeeds");
    assert_eq!(bytes, u64::try_from(body.len()).expect("len"));
    assert_eq!(sink, body, "byte-exact");
    assert_eq!(
        seen.lock().expect("seen").request_line,
        "GET /molt-bucket/molt/aa/001.molt.enc HTTP/1.1"
    );
    assert!(!seen_progress.is_empty(), "progress was reported");
    assert!(
        seen_progress.windows(2).all(|w| w[0].0 <= w[1].0),
        "progress is monotonic: {seen_progress:?}"
    );
    let last = seen_progress.last().expect("nonempty");
    assert_eq!(*last, (bytes, Some(bytes)), "progress ends at the total");
}

/// EOF before the declared Content-Length is a hard error — a partial
/// blob must never look like a completed download.
#[tokio::test]
async fn get_object_rejects_a_truncated_body() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n".to_vec();
    response.extend_from_slice(&[7u8; 40]); // 40 of 100 bytes, then close
    let (endpoint, _seen) = raw_stub(response).await;
    let mut sink: Vec<u8> = Vec::new();
    let err = client_for(&endpoint)
        .get_object("k", &mut sink, 1024, &mut |_, _| {})
        .await
        .expect_err("truncation must reject");
    assert!(
        matches!(&err, S3Error::Protocol(m) if m.contains("truncated")),
        "honest truncation error, got {err:?}"
    );
}

/// A declared length beyond the cap is refused BEFORE any byte lands.
#[tokio::test]
async fn get_object_refuses_a_body_beyond_the_size_cap() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\n\r\n".to_vec();
    response.extend_from_slice(&[0u8; 2048]);
    let (endpoint, _seen) = raw_stub(response).await;
    let mut sink: Vec<u8> = Vec::new();
    let err = client_for(&endpoint)
        .get_object("k", &mut sink, 1024, &mut |_, _| {})
        .await
        .expect_err("over-cap must reject");
    assert!(
        matches!(&err, S3Error::Protocol(m) if m.contains("cap")),
        "honest cap error, got {err:?}"
    );
    assert!(sink.is_empty(), "nothing was written");
}

/// A 404 names the OBJECT (not the bucket) — the honest class for a
/// restore pointed at a key that is not there.
#[tokio::test]
async fn get_object_maps_404_to_the_missing_object_class() {
    let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (endpoint, _seen) = raw_stub(response).await;
    let mut sink: Vec<u8> = Vec::new();
    let err = client_for(&endpoint)
        .get_object("molt/zz/9.molt.enc", &mut sink, 1024, &mut |_, _| {})
        .await
        .expect_err("404 is an error");
    let S3Error::Http { status: 404, hint } = err else {
        panic!("expected http 404, got {err:?}");
    };
    assert!(hint.contains("molt/zz/9.molt.enc"), "names the object: {hint}");
}

#[tokio::test]
async fn unreachable_endpoint_is_the_connect_class_for_both_ops() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let client = client_for(&format!("http://127.0.0.1:{port}"));
    assert!(matches!(
        client.put_object("k", b"x").await,
        Err(S3Error::Connect(_))
    ));
    assert!(matches!(
        client.delete_object("k").await,
        Err(S3Error::Connect(_))
    ));
}
