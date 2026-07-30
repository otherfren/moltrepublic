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
use std::time::{Duration, Instant};

use molt_net::s3::http::{roundtrip_download, roundtrip_upload, DownloadBounds};
use molt_net::s3::{sigv4, S3Client, S3Config, S3Error};
use molt_net::dial::Dialer;
use sha2::{Digest, Sha256};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

// ---------------------------------------------------------------------------
// Streaming-upload timeout semantics (review finding 1): a large PUT must ride
// per-write idle windows, never one whole-exchange cap it would blow through
// over a slow circuit. Driven through `roundtrip_upload` over an in-memory
// `duplex` pipe so the pacing is deterministic (no OS socket-buffer guessing).
// ---------------------------------------------------------------------------

/// A stalled server (never drains the body) fails at the per-write idle
/// window — promptly, and via the WRITE path, not a whole-exchange cap.
#[tokio::test]
async fn upload_stall_fails_with_a_per_write_idle_timeout() {
    let (mut client, server) = duplex(8 * 1024); // small pipe: writes block once full
    let idle = Duration::from_millis(200);
    let headers = vec![("host".to_string(), "x".to_string())];
    let body = vec![0u8; 128 * 1024]; // >> the pipe, so the write must stall
    // hold the server end open WITHOUT ever reading, so the client's body
    // write fills the pipe and then blocks
    let held = tokio::spawn(async move {
        let _server = server;
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let started = Instant::now();
    let err = roundtrip_upload(&mut client, "PUT", "/molt-bucket/k", &headers, &body, idle)
        .await
        .expect_err("a stalled upload must fail, never hang");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "failed at the idle window, not a whole-body cap"
    );
    let S3Error::Protocol(msg) = err else {
        panic!("expected a protocol error, got {err:?}");
    };
    assert!(
        msg.contains("write") && msg.contains("timed out"),
        "the failure is the per-write idle timeout, not a whole-exchange cap: {msg}"
    );
    held.abort();
}

/// A slow-but-progressing large upload SUCCEEDS even though the whole transfer
/// spans longer than one idle window — proving the bound is per-write (idle),
/// not a single cap over the (size-dependent) whole exchange.
#[tokio::test]
async fn upload_survives_a_large_slow_but_progressing_transfer() {
    const BODY_LEN: usize = 768 * 1024; // 12 slices of 64 KiB
    let (mut client, mut server) = duplex(8 * 1024);
    let idle = Duration::from_millis(300);
    let headers = vec![("host".to_string(), "x".to_string())];
    let body: Vec<u8> = (0..BODY_LEN).map(|i| u8::try_from(i % 251).expect("byte")).collect();
    let expected = body.clone();

    // the server drains ~8 KiB every 5 ms: each 64 KiB slice completes well
    // within the 300 ms idle window, but the whole ~480 ms transfer exceeds it
    let server_task = tokio::spawn(async move {
        let mut got = Vec::new();
        let mut b = [0u8; 8192];
        let mut head_end: Option<usize> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let n = match server.read(&mut b).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            got.extend_from_slice(&b[..n]);
            if head_end.is_none() {
                if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    head_end = Some(p + 4);
                }
            }
            if let Some(he) = head_end {
                if got.len() - he >= BODY_LEN {
                    break;
                }
            }
        }
        server
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .ok();
        server.shutdown().await.ok();
        let he = head_end.expect("head terminator seen");
        got[he..].to_vec()
    });

    let started = Instant::now();
    let resp = roundtrip_upload(&mut client, "PUT", "/molt-bucket/k", &headers, &body, idle)
        .await
        .expect("a slow-but-progressing upload must succeed");
    assert_eq!(resp.status, 200);
    assert!(
        started.elapsed() > idle,
        "the transfer must genuinely outlast one idle window (else it proves nothing)"
    );
    let received = server_task.await.expect("server task");
    assert_eq!(received, expected, "the body arrives byte-exact");
}

// ---------------------------------------------------------------------------
// Streaming-download bounds (review findings 2 & 3), driven through
// `roundtrip_download` over a real loopback TCP stub.
// ---------------------------------------------------------------------------

/// A server dribbling below the minimum-throughput floor is bounded by the
/// overall deadline — not held effectively unbounded by an idle-reset-only
/// timeout (finding 2).
#[tokio::test]
async fn download_below_the_throughput_floor_is_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut c = [0u8; 1024];
        let mut head = Vec::new();
        loop {
            let n = sock.read(&mut c).await.expect("read");
            if n == 0 {
                break;
            }
            head.extend_from_slice(&c[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // announce a body, then dribble far below the floor (each gap stays
        // under the idle window, so idle alone would never trip)
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15000\r\n\r\n")
            .await
            .expect("head");
        for _ in 0..300 {
            if sock.write_all(&[0u8; 50]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });

    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    let headers = vec![("host".to_string(), "x".to_string())];
    let mut sink: Vec<u8> = Vec::new();
    let bounds = DownloadBounds {
        idle: Duration::from_millis(500),  // 30 ms gaps never trip the idle window
        grace: Duration::from_millis(50),
        min_throughput_bps: 100_000, // makes the overall deadline ~250 ms
    };
    let started = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        roundtrip_download(&mut tcp, "/b/k", &headers, &mut sink, 20_000, bounds, &mut |_, _| {}),
    )
    .await
    .expect("must not hang");
    let err = res.expect_err("a dribble below the floor must be bounded");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "bounded by the throughput floor, not idle-only"
    );
    assert!(
        matches!(&err, S3Error::Protocol(m) if m.contains("floor") || m.contains("too slow")),
        "honest throughput-floor error, got {err:?}"
    );
}

/// A server dribbling the response HEAD below the throughput floor (never
/// completing `\r\n\r\n`, staying under the 64 KiB head cap) is bounded by the
/// overall floor too — the head phase is not an idle-only escape hatch.
#[tokio::test]
async fn download_head_dribble_is_bounded_by_the_floor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut c = [0u8; 1024];
        let n = sock.read(&mut c).await.expect("read");
        let _ = n;
        // send a partial head, then dribble header bytes forever without ever
        // sending the terminator — each gap under the idle window
        sock.write_all(b"HTTP/1.1 200 OK\r\n").await.expect("partial head");
        for _ in 0..300 {
            if sock.write_all(b"x-molt-pad: y\r\n").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });

    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    let headers = vec![("host".to_string(), "x".to_string())];
    let mut sink: Vec<u8> = Vec::new();
    let bounds = DownloadBounds {
        idle: Duration::from_millis(500),
        grace: Duration::from_millis(50),
        min_throughput_bps: 100_000, // overall ~250 ms for 20_000 bytes
    };
    let started = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        roundtrip_download(&mut tcp, "/b/k", &headers, &mut sink, 20_000, bounds, &mut |_, _| {}),
    )
    .await
    .expect("must not hang");
    let err = res.expect_err("a dribbling head must be bounded");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "bounded by the floor, not held by the idle-only head read"
    );
    assert!(
        matches!(&err, S3Error::Protocol(m) if m.contains("head") && m.contains("floor")),
        "honest head-floor error, got {err:?}"
    );
}

/// A non-2xx from a keep-alive server (ignoring `Connection: close`) returns
/// the honest status as soon as the Content-Length-framed error body is
/// complete — the drain never masks the status with a read timeout (finding 3).
#[tokio::test]
async fn download_nonc2xx_against_a_keepalive_server_returns_the_status() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut c = [0u8; 1024];
        let mut head = Vec::new();
        loop {
            let n = sock.read(&mut c).await.expect("read");
            if n == 0 {
                break;
            }
            head.extend_from_slice(&c[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // a framed 403 error body, then HOLD the socket open — a naive
        // drain-to-EOF would block here until the idle timeout
        sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 5\r\n\r\nabcde")
            .await
            .expect("resp");
        tokio::time::sleep(Duration::from_secs(30)).await; // keep-alive: never close
    });

    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    let headers = vec![("host".to_string(), "x".to_string())];
    let mut sink: Vec<u8> = Vec::new();
    let bounds = DownloadBounds {
        idle: Duration::from_millis(300),
        grace: Duration::from_millis(300),
        min_throughput_bps: 1024,
    };
    let started = Instant::now();
    let (status, bytes) = tokio::time::timeout(
        Duration::from_secs(5),
        roundtrip_download(&mut tcp, "/b/k", &headers, &mut sink, 1024, bounds, &mut |_, _| {}),
    )
    .await
    .expect("must not hang")
    .expect("returns the honest status, not a drain timeout");
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "returned when the framed error body was complete, not at the idle timeout"
    );
    assert_eq!(status, 403);
    assert_eq!(bytes, 0);
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
