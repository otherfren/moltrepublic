// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A polling MCP client is one INFO line a minute, not one per request; a
//! burst of bad tokens is one WARN (`docs_archive/security/mcp-security.md`).
//! The only test in its binary: a process-wide subscriber, so no other
//! test's callsites race its interest cache.

use std::sync::{Arc, Mutex};

use molt_core::{Command, GroupConfig, SessionView};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// One `initialize` POST; the status code.
async fn post(addr: std::net::SocketAddr, token: &str) -> u16 {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": "test", "version": "0" } } })
    .to_string();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\
         Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut raw))
        .await
        .expect("a reply")
        .expect("read");
    let raw = String::from_utf8_lossy(&raw);
    raw.split_whitespace().nth(1).and_then(|c| c.parse().ok()).expect("status line")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_polling_client_is_one_info_line_and_a_refusal_burst_one_warn() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(Capture(buf.clone()))
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber");
    let handle = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    handle
        .execute(Command::PatchSettings { patch: json!({ "mcp_token": "seat-secret" }) })
        .await
        .expect("seed the live key");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let allow = vec!["127.0.0.1".parse().expect("ip")];
        let _ = molt_mcp::serve_listener(handle, listener, false, allow, "seat-secret".into(), String::new()).await;
    });
    for _ in 0..20 {
        assert_eq!(post(addr, "seat-secret").await, 200);
    }
    for _ in 0..5 {
        assert_eq!(post(addr, "nope").await, 401);
    }
    let log = String::from_utf8_lossy(&buf.lock().expect("log buffer")).into_owned();
    let ours: Vec<&str> = log.lines().filter(|l| l.contains("molt_mcp")).collect();
    assert!(!ours.iter().any(|l| l.contains("MCP client connected")), "{log}");
    let clients: Vec<&&str> = ours.iter().filter(|l| l.contains("mcp_clients")).collect();
    assert_eq!(clients.len(), 1, "{log}");
    assert!(clients[0].contains(" INFO ") && clients[0].contains("via=\"http\"") && clients[0].contains("count=1"), "{log}");
    let refused: Vec<&&str> = ours.iter().filter(|l| l.contains("invalid bearer token")).collect();
    assert_eq!(refused.len(), 1, "{log}");
    assert!(refused[0].contains(" WARN ") && refused[0].contains("count=1"), "{log}");
}
