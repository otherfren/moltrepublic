// SPDX-License-Identifier: GPL-3.0-or-later

//! `moltd` and the web door (`docs/web_gui.md` §5.3): `[web] port` opens a
//! loopback listener that serves the bundle; a bundle directory that is
//! not private opens nothing and the node stays up.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Node(Child);

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn bundle(dir: &Path) {
    std::fs::create_dir_all(dir).expect("web dir");
    std::fs::write(dir.join("index.html"), "<!doctype html>moltd").expect("index");
    std::fs::write(dir.join("molt_web.js"), "export default 1").expect("js");
    std::fs::write(dir.join("molt_web_bg.wasm"), [0u8, 97, 115, 109]).expect("wasm");
    std::fs::write(dir.join("page.js"), "import './molt_web.js'").expect("page js");
    std::fs::write(dir.join("page.css"), "body{margin:0}").expect("page css");
}

/// A headless node with a seat key and `[web] port`; the bundle directory
/// is the default, `web/` beside the config.
fn spawn(dir: &Path, web: u16) -> (Node, u16) {
    let mcp = free_port();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[node]\nheadless = true\n[storage]\nworkspace_dir = \"{}\"\n\
             [mcp]\nport = {mcp}\nallow = \"127.0.0.1\"\ntoken = \"svc\"\n[web]\nport = {web}\n",
            dir.join("ws").display()
        ),
    )
    .expect("write config");
    let child = Command::new(env!("CARGO_BIN_EXE_moltd"))
        .arg("--config")
        .arg(&config)
        .arg("--mcp-tcp")
        .arg(format!("127.0.0.1:{}", free_port()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn moltd");
    (Node(child), mcp)
}

fn get(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_configured_web_port_serves_the_bundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    bundle(&dir.path().join("web"));
    let web = free_port();
    let (_node, _) = spawn(dir.path(), web);
    let mut page = String::new();
    wait_for("the web door answers", || {
        page = get(web).unwrap_or_default();
        page.starts_with("HTTP/1.1 200")
    });
    assert!(page.ends_with("<!doctype html>moltd"), "{page}");
}

#[test]
fn a_bundle_others_can_write_opens_no_door_and_the_node_stays_up() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let web_dir = dir.path().join("web");
    bundle(&web_dir);
    std::fs::set_permissions(web_dir.join("index.html"), std::fs::Permissions::from_mode(0o666)).expect("chmod");
    let web = free_port();
    let (_node, mcp) = spawn(dir.path(), web);
    // MCP comes up: the node is alive
    wait_for("the MCP port answers", || TcpStream::connect(("127.0.0.1", mcp)).is_ok());
    std::thread::sleep(Duration::from_millis(500));
    assert!(TcpStream::connect(("127.0.0.1", web)).is_err(), "the web port must stay closed");
}
