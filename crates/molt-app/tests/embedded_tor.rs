// SPDX-License-Identifier: GPL-3.0-or-later

//! The v0.0.2 field report: `tor.mode = "embedded"` aborted the node on the
//! first dial (rustls found no process-level CryptoProvider - this graph is
//! ring-free, and the molt-net test graph that hid it carries ring through
//! a dev-dependency). Pinned HERE, in the binary's own graph. Offline: the
//! panic fired before any bootstrap. Only in an `--features embedded-tor`
//! build - the plain suite compiles this file to nothing.
#![cfg(feature = "embedded-tor")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn an_embedded_tor_dial_never_panics_the_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    let mcp_tcp = free_port();
    let config = dir.path().join("config.toml");
    // a well-formed onion address that exists nowhere - the panic was before any dial
    let onion = "ws://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion";
    std::fs::write(
        &config,
        format!(
            "[node]\nheadless = true\n[storage]\nworkspace_dir = \"{}\"\n\
             [mcp]\nport = {port}\nallow = \"127.0.0.1\"\ntoken = \"t\"\n\
             [transport.anonymity]\nnetwork = \"tor\"\n\
             [transport.anonymity.tor]\nmode = \"embedded\"\nport = 9050\n\
             [transport.nostr]\nclearnet_enabled = false\n\
             [[transport.nostr.relay]]\nurl = \"{onion}\"\nconfirmed = true\n",
            dir.path().join("ws").display()
        ),
    )
    .expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_moltd"))
        .arg("--config")
        .arg(&config)
        .arg("--mcp-tcp")
        .arg(format!("127.0.0.1:{mcp_tcp}"))
        .env("HOME", dir.path()) // arti's state dir lives under $HOME
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moltd");
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut pipe = child.stderr.take().expect("stderr pipe");
    let sink = stderr.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().expect("stderr lock").extend_from_slice(&buf[..n]);
        }
    });

    let mut stream = connect(port);
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut call = |req: serde_json::Value| -> serde_json::Value {
        let mut line = req.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("write");
        let mut resp = String::new();
        reader.read_line(&mut resp).expect("read");
        serde_json::from_str(&resp).expect("json")
    };
    call(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"token":"t"}}));
    let r = call(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"relay_probe","arguments":{"url": onion}}}));
    assert!(r["result"].is_object(), "probe accepted: {r}");

    std::thread::sleep(Duration::from_secs(3));
    let log = String::from_utf8_lossy(&stderr.lock().expect("stderr lock")).into_owned();
    let alive = child.try_wait().expect("try_wait").is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(!log.contains("panicked"), "a dial must never panic:\n{log}");
    assert!(alive, "the node must stay up:\n{log}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe")
        .local_addr()
        .expect("probe addr")
        .port()
}

fn connect(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
                return s;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "mcp port never opened: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
