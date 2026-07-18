// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The S3 probe against an in-process HTTP stub on 127.0.0.1: asserts a
//! correctly SigV4-signed request arrives on the wire and that the HTTP
//! status maps to the honest error classes (200 ok, 403 credentials,
//! 404 bucket). Plain HTTP on loopback — TLS-in-test would prove nothing
//! about signing, and the TLS path is the same rustls stack the SMP
//! transport already exercises. The `Direct` dialer allows loopback
//! (fail-closed only means: no Tor setting may leak to clearnet).

use std::sync::{Arc, Mutex};

use molt_net::s3::{sigv4, S3Client, S3Config, S3Error};
use molt_net::smp::tls::Dialer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One recorded request: the request line and the lowercased headers.
#[derive(Debug, Clone, Default)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A single-shot S3 stub: accepts one connection, records the request head,
/// answers with `status`, closes. Returns (endpoint URL, recorded request).
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
        {
            let head = String::from_utf8_lossy(&buf);
            let mut lines = head.split("\r\n");
            let mut seen = record.lock().expect("record lock");
            seen.request_line = lines.next().unwrap_or_default().to_string();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    seen.headers
                        .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
        }
        let phrase = match status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Status",
        };
        let resp = format!("HTTP/1.1 {status} {phrase}\r\nContent-Length: 0\r\n\r\n");
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
async fn probe_sends_a_correctly_signed_head_and_maps_200_to_ok() {
    let (endpoint, seen) = stub_server(200).await;
    client_for(&endpoint).probe_bucket().await.expect("200 is ok");

    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(seen.request_line, "HEAD /molt-bucket HTTP/1.1");
    // the signed headers arrived, host includes the non-default port
    let host = seen.header("host").expect("host header");
    assert_eq!(host, endpoint.trim_start_matches("http://"));
    assert_eq!(
        seen.header("x-amz-content-sha256"),
        Some(sigv4::EMPTY_PAYLOAD_SHA256)
    );
    let datetime = seen.header("x-amz-date").expect("x-amz-date").to_string();
    let auth = seen.header("authorization").expect("authorization");

    // recompute the signature over exactly what arrived on the wire — the
    // header must be internally consistent with the request the stub saw
    // (the sigv4 math itself is pinned by the AWS vectors in the unit tests)
    let headers = vec![
        ("host".to_string(), host.to_string()),
        (
            "x-amz-content-sha256".to_string(),
            sigv4::EMPTY_PAYLOAD_SHA256.to_string(),
        ),
        ("x-amz-date".to_string(), datetime.clone()),
    ];
    let expect = sigv4::authorization_header(&sigv4::SignParams {
        method: "HEAD",
        uri: "/molt-bucket",
        query: &[],
        headers: &headers,
        payload_hash: sigv4::EMPTY_PAYLOAD_SHA256,
        datetime: &datetime,
        region: "us-east-1",
        service: "s3",
        access_key: "AKIAEXAMPLE",
        secret_key: "secret-example",
    });
    assert_eq!(auth, expect, "authorization header must match the signed request");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"),
        "credential scope shape: {auth}"
    );
    assert!(
        auth.contains("/us-east-1/s3/aws4_request"),
        "scope region/service: {auth}"
    );
    assert!(
        auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"),
        "signed headers: {auth}"
    );
}

#[tokio::test]
async fn probe_maps_403_to_the_credentials_class() {
    let (endpoint, _seen) = stub_server(403).await;
    let err = client_for(&endpoint)
        .probe_bucket()
        .await
        .expect_err("403 is an error");
    let S3Error::Http { status: 403, hint } = err else {
        panic!("expected the http 403 class, got {err:?}");
    };
    assert!(hint.contains("access key"), "hint names the credentials: {hint}");
}

#[tokio::test]
async fn probe_maps_404_to_the_missing_bucket_class() {
    let (endpoint, _seen) = stub_server(404).await;
    let err = client_for(&endpoint)
        .probe_bucket()
        .await
        .expect_err("404 is an error");
    let S3Error::Http { status: 404, hint } = err else {
        panic!("expected the http 404 class, got {err:?}");
    };
    assert!(hint.contains("molt-bucket"), "hint names the bucket: {hint}");
}

#[tokio::test]
async fn unreachable_endpoint_is_the_connect_class() {
    // bind-then-drop: the port is closed, the dial is refused immediately
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let err = client_for(&format!("http://127.0.0.1:{port}"))
        .probe_bucket()
        .await
        .expect_err("refused connection is an error");
    assert!(
        matches!(err, S3Error::Connect(_)),
        "expected the connect class, got {err:?}"
    );
}

/// `#[ignore]` (real network): drives the https/WebPKI path end to end —
/// real DNS, TCP, TLS 1.3 against AWS's certificate chain verified through
/// webpki-roots + the rustcrypto provider. Bogus credentials on purpose:
/// reaching the HTTP error class at all proves transport + TLS + signing
/// were accepted as well-formed; the loopback tests can't cover this.
/// `cargo test -p molt-net --test s3_probe -- --ignored --nocapture`
#[tokio::test]
#[ignore = "dials the real s3.amazonaws.com"]
async fn live_https_probe_reaches_aws_through_webpki_tls() {
    let err = client_for("https://s3.amazonaws.com")
        .probe_bucket()
        .await
        .expect_err("bogus creds cannot probe successfully");
    let S3Error::Http { status, hint } = err else {
        panic!("expected an HTTP-class outcome (transport+TLS ok), got {err:?}");
    };
    println!("OK: live AWS answered http {status}: {hint}");
}

#[tokio::test]
async fn onion_endpoint_under_direct_dialer_fails_closed() {
    // no Tor configured + .onion endpoint: refused before any DNS/dial
    let err = client_for("http://abcdefexample.onion")
        .probe_bucket()
        .await
        .expect_err("onion without Tor must fail closed");
    let S3Error::Connect(reason) = err else {
        panic!("expected the connect class, got {err:?}");
    };
    assert!(reason.contains("Tor"), "reason names Tor: {reason}");
}
