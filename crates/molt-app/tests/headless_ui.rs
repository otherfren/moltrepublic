// SPDX-License-Identifier: GPL-3.0-or-later

//! The `docs_archive/ui/gui_over_mcp.md` step-2 pin: a real `moltd` brings the GUI
//! up on the Slint testing backend (no display), its live mirror publishes,
//! and MCP reads the snapshot back. Runs only in a `--features ui-testing`
//! build — the plain suite compiles this file to nothing.
#![cfg(feature = "ui-testing")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[test]
fn a_testing_backend_moltd_publishes_a_ui_snapshot_over_mcp() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a free port by bind-then-drop — racy in theory, fine for a dev pin
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        l.local_addr().expect("probe addr").port()
    };
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[node]\nheadless = false\n[storage]\nworkspace_dir = \"{}\"\n\
             [mcp]\nport = {port}\nallow = \"127.0.0.1\"\ntoken = \"walk\"\n",
            dir.path().join("ws").display()
        ),
    )
    .expect("write config");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_moltd"))
        .arg("--config")
        .arg(&config)
        .env("MOLT_UI_TESTING", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn moltd");
    let result = drive(port);
    let _ = child.kill();
    let _ = child.wait();
    let snapshot = result.expect("driving moltd over MCP");
    assert!(snapshot["generation"].as_u64().expect("generation") >= 1);
    assert_eq!(snapshot["screen"].as_str().expect("screen"), "choice");
}

/// Connect to the node's MCP TCP port, authenticate, and poll
/// `read_ui_state` until the window's mirror has published a snapshot.
fn drive(port: u16) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("mcp port never opened: {e}")),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut call = |req: serde_json::Value| -> Result<serde_json::Value, String> {
        let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut resp = String::new();
        reader.read_line(&mut resp).map_err(|e| e.to_string())?;
        serde_json::from_str(&resp).map_err(|e| e.to_string())
    };
    call(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "token": "walk" }
    }))?;
    loop {
        let r = call(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "read_ui_state", "arguments": {} }
        }))?;
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        let snap = &v["snapshot"];
        if snap["generation"].as_u64().unwrap_or(0) >= 1 {
            return Ok(snap.clone());
        }
        if Instant::now() >= deadline {
            return Err(format!("no snapshot published; last: {text}"));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}
