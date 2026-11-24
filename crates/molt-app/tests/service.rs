// SPDX-License-Identifier: GPL-3.0-or-later

//! `moltd` as a service: SIGTERM is a clean stop (the durable close runs,
//! exit 0), and `[node] open_on_start` opens a workspace without a client -
//! a phrase-sealed one only warns and the node stays up for whoever decides.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "svc";

struct Node {
    child: Child,
    port: u16,
    mcp_tcp: u16,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Node {
    fn spawn(dir: &Path, open_on_start: &str) -> Node {
        Node::spawn_with(dir, open_on_start, &format!("token = \"{TOKEN}\"\n"))
    }

    /// `token_line`: the `[mcp]` token entry verbatim, `""` for none.
    fn spawn_with(dir: &Path, open_on_start: &str, token_line: &str) -> Node {
        let port = free_port();
        let mcp_tcp = free_port();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[node]\nheadless = true\nopen_on_start = \"{open_on_start}\"\n\
                 [storage]\nworkspace_dir = \"{}\"\n\
                 [mcp]\nport = {port}\nallow = \"127.0.0.1\"\n{token_line}",
                dir.join("ws").display()
            ),
        )
        .expect("write config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_moltd"))
            .arg("--config")
            .arg(&config)
            .arg("--mcp-tcp")
            .arg(format!("127.0.0.1:{mcp_tcp}"))
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
        Node { child, port, mcp_tcp, stderr }
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("stderr lock")).into_owned()
    }

    fn sigterm(&self) {
        let ok = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .expect("kill")
            .success();
        assert!(ok, "kill -TERM");
    }

    /// The exit code, or `None` when the process died by a signal.
    fn wait_exit(&mut self, secs: u64) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status.code();
            }
            assert!(Instant::now() < deadline, "moltd still running after {secs}s\n{}", self.stderr());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `read_session` over MCP, as the seat.
    fn read_session(&self) -> serde_json::Value {
        let mut stream = connect(self.port);
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut call = |req: serde_json::Value| -> serde_json::Value {
            let mut line = req.to_string();
            line.push('\n');
            stream.write_all(line.as_bytes()).expect("write");
            let mut resp = String::new();
            reader.read_line(&mut resp).expect("read");
            serde_json::from_str(&resp).expect("json")
        };
        call(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"token":TOKEN}}));
        let r = call(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_session","arguments":{}}}));
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
        serde_json::from_str(text).unwrap_or_else(|_| panic!("read_session: {r}"))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

/// A workspace on disk under `root`, closed. Returns its id and phrase.
fn workspace(root: &Path) -> (String, String) {
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let seed = molt_storage::seed_entropy(&phrase).expect("entropy");
    let genesis = molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 1,
        ts: 1_751_000_000,
        by: "ada".to_string(),
        body: molt_core::WorkspaceEvent::Founded {
            name: "Service".to_string(),
            rule_m: 2,
            rule_n: 2,
            member: "ada".to_string(),
            roster: vec!["ada".to_string(), "ben".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
            relays: Vec::new(),
            features: None,
        },
    };
    let ws = molt_storage::create_workspace(root, &seed, &genesis).expect("create");
    (ws.manifest.workspace.id.clone(), phrase)
}

fn workspace_dir(root: &Path) -> std::path::PathBuf {
    let scan = molt_storage::scan_workspaces(root);
    assert_eq!(scan.len(), 1, "one workspace under the root");
    scan[0].dir.clone()
}

fn snapshot_count(ws_dir: &Path) -> usize {
    std::fs::read_dir(ws_dir.join("snapshots")).map(|d| d.count()).unwrap_or(0)
}

#[test]
fn sigterm_stops_a_headless_node_with_exit_0() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = Node::spawn(dir.path(), "");
    drop(connect(node.port));
    node.sigterm();
    assert_eq!(node.wait_exit(15), Some(0), "{}", node.stderr());
}

#[test]
fn open_on_start_opens_the_workspace_and_sigterm_closes_it_durably() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("ws");
    let (id, _phrase) = workspace(&root);
    let ws_dir = workspace_dir(&root);
    let before = snapshot_count(&ws_dir);

    let mut node = Node::spawn(dir.path(), &id);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = node.read_session();
        if s["active_workspace"] == serde_json::json!(id) {
            break;
        }
        assert!(Instant::now() < deadline, "never opened: {s}\n{}", node.stderr());
        std::thread::sleep(Duration::from_millis(200));
    }

    node.sigterm();
    assert_eq!(node.wait_exit(15), Some(0), "{}", node.stderr());
    assert!(
        snapshot_count(&ws_dir) > before,
        "the closing snapshot proves the durable close ran\n{}",
        node.stderr()
    );
}

#[test]
fn a_phrase_sealed_open_on_start_only_warns_and_the_node_stays_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("ws");
    let (id, phrase) = workspace(&root);
    molt_storage::seal_at_rest(&workspace_dir(&root), &phrase).expect("seal");

    let mut node = Node::spawn(dir.path(), &id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !node.stderr().contains("open_on_start") {
        assert!(Instant::now() < deadline, "no open_on_start log line\n{}", node.stderr());
        std::thread::sleep(Duration::from_millis(100));
    }
    let log = node.stderr();
    let line = log.lines().find(|l| l.contains("open_on_start")).expect("line");
    assert!(line.contains("WARN"), "a refused open is a warning, not more: {line}");
    assert!(line.contains("encrypted"), "the warning names the cause: {line}");

    let s = node.read_session();
    assert_eq!(s["active_workspace"], serde_json::json!(""), "{s}");
    assert!(node.child.try_wait().expect("try_wait").is_none(), "the node stays up");
    node.sigterm();
    assert_eq!(node.wait_exit(15), Some(0));
}

/// No seat token, no TCP port - neither the always-on one nor `--mcp-tcp` -
/// and the node stays up until stopped.
#[test]
fn no_token_binds_no_mcp_port_and_the_node_stays_up() {
    for token_line in ["token = \"\"\n", ""] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut node = Node::spawn_with(dir.path(), "", token_line);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !node.stderr().contains("mode: headless") {
            assert!(Instant::now() < deadline, "never reached headless\n{}", node.stderr());
            assert!(node.child.try_wait().expect("try_wait").is_none(), "exited\n{}", node.stderr());
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_secs(2));
        assert!(node.child.try_wait().expect("try_wait").is_none(), "the node stays up\n{}", node.stderr());
        assert!(node.stderr().contains("[mcp].token empty - TCP MCP off"), "{}", node.stderr());
        for port in [node.port, node.mcp_tcp] {
            assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "port {port} bound\n{}", node.stderr());
        }
        node.sigterm();
        assert_eq!(node.wait_exit(15), Some(0), "{}", node.stderr());
    }
}

/// One MCP session to a seat, kept open across calls.
struct Mcp {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    id: u64,
}

impl Mcp {
    fn open(port: u16) -> Mcp {
        let stream = connect(port);
        stream.set_read_timeout(Some(Duration::from_secs(60))).expect("timeout");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut mcp = Mcp { stream, reader, id: 0 };
        mcp.rpc("initialize", serde_json::json!({ "token": TOKEN }));
        mcp
    }

    fn rpc(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.id += 1;
        let mut line = serde_json::json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params}).to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).expect("write");
        loop {
            let mut resp = String::new();
            self.reader.read_line(&mut resp).expect("read");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("json");
            if v["id"] == serde_json::json!(self.id) {
                return v;
            }
        }
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> serde_json::Value {
        self.tool(name, args).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn wait(&mut self, what: &str, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let s = self.call("read_session", serde_json::json!({}));
            if pred(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// `Ok(value)` or the refusal text.
    fn tool(&mut self, name: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let r = self.rpc("tools/call", serde_json::json!({ "name": name, "arguments": args }));
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        if r["result"]["isError"] == serde_json::json!(true) || r.get("error").is_some() {
            return Err(format!("{text}{}", r["error"]));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    }
}

/// A headless seat with `relay` confirmed and clearnet on.
fn relay_seat(dir: &Path, relay: &str) -> Node {
    let port = free_port();
    let mcp_tcp = free_port();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[node]\nheadless = true\n\
             [storage]\nworkspace_dir = \"{}\"\n\
             [mcp]\nport = {port}\nallow = \"127.0.0.1\"\ntoken = \"{TOKEN}\"\n\
             [transport.nostr]\nclearnet_enabled = true\n\
             [[transport.nostr.relay]]\nurl = \"{relay}\"\nconfirmed = true\n",
            dir.join("ws").display()
        ),
    )
    .expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_moltd"))
        .arg("--config")
        .arg(&config)
        .arg("--mcp-tcp")
        .arg(format!("127.0.0.1:{mcp_tcp}"))
        .env("RUST_LOG", "info")
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
    Node { child, port, mcp_tcp, stderr }
}

/// A TCP proxy in front of `upstream`; once `swallow` is set it keeps
/// every connection open and forwards nothing more, in either direction.
fn freezable_proxy(upstream: String, swallow: Arc<std::sync::atomic::AtomicBool>) -> String {
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(&upstream) else { continue };
            for (mut from, mut to) in [
                (client.try_clone().expect("clone"), server.try_clone().expect("clone")),
                (server, client),
            ] {
                let swallow = swallow.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16 * 1024];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        if !swallow.load(Ordering::SeqCst) && to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        }
    });
    url
}

/// Ada and ben found a 2-of-2 republic over `relay`, both entered.
fn found_pair(ada: &mut Mcp, ben: &mut Mcp) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while let Err(e) = ada.tool(
        "create_start",
        serde_json::json!({ "name": "Club", "member": "ada", "threshold": 2, "members": 2 }),
    ) {
        assert!(Instant::now() < deadline, "create_start: {e}");
        std::thread::sleep(Duration::from_secs(1));
    }
    // the seat link becomes joinable once its transport details are in
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let s = ada.call("read_session", serde_json::json!({}));
        let link = s["create"]["seats"][0]["link"].as_str().unwrap_or("").to_string();
        if !link.is_empty() && ben.tool("join_start", serde_json::json!({ "invite": link, "member": "ben" })).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the invite never became joinable");
        std::thread::sleep(Duration::from_millis(500));
    }
    ada.wait("the join", |s| s["create"]["can_propose"] == serde_json::json!(true));
    ada.call("create_propose", serde_json::json!({ "name": "Club", "agenda": "", "features": ["memory"] }));
    let s = ada.wait("ada's phrase", |s| s["create"]["seed"].as_str().is_some_and(|p| !p.is_empty()));
    ada.call("confirm_seed_backup", serde_json::json!({ "phrase": s["create"]["seed"] }));
    ben.wait("the charter", |s| s["join"]["awaiting_ratify"] == serde_json::json!(true));
    ben.call("join_confirm_charter", serde_json::json!({}));
    let s = ben.wait("ben's phrase", |s| s["join"]["seed"].as_str().is_some_and(|p| !p.is_empty()));
    ben.call("confirm_seed_backup", serde_json::json!({ "phrase": s["join"]["seed"] }));
    let sealed = |side: &'static str| {
        move |s: &serde_json::Value| {
            s[side]["outcome"] == serde_json::json!(1) || s[side]["run"]["outcome"] == serde_json::json!(1)
        }
    };
    ada.wait("ada's seal", sealed("create"));
    ada.call("create_finish", serde_json::json!({}));
    ben.wait("ben's seal", sealed("join"));
    ben.call("join_finish", serde_json::json!({}));
    let open = |s: &serde_json::Value| s["active_workspace"].as_str().is_some_and(|w| !w.is_empty());
    ada.wait("ada's workspace", open);
    ben.wait("ben's workspace", open);
}

/// **A second SIGTERM ends a close that waits.** Ada's republic is open
/// over a relay that then freezes (it keeps the socket, answers nothing);
/// a message she sends is unpublished at the stop, so the close waits on
/// the relay. The first SIGTERM leaves her in the durable close, the
/// second ends it at once with exit 1.
#[test]
fn a_second_sigterm_ends_a_waiting_close() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let relay = rt.block_on(nostr_relay_builder::MockRelay::run()).expect("relay");
    let upstream = rt.block_on(relay.url()).to_string().trim_start_matches("ws://").trim_end_matches('/').to_string();
    let swallow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let url = freezable_proxy(upstream, swallow.clone());
    let (dir_a, dir_b) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
    let mut ada = relay_seat(dir_a.path(), &url);
    let ben = relay_seat(dir_b.path(), &url);
    let (mut a, mut b) = (Mcp::open(ada.port), Mcp::open(ben.port));
    found_pair(&mut a, &mut b);

    swallow.store(true, std::sync::atomic::Ordering::SeqCst);
    a.call("chat_send", serde_json::json!({ "body": "into the freeze" }));
    drop(a);
    ada.sigterm();
    std::thread::sleep(Duration::from_secs(3));
    assert!(ada.child.try_wait().expect("try_wait").is_none(), "the close did not wait\n{}", ada.stderr());
    let second = Instant::now();
    ada.sigterm();
    assert_eq!(ada.wait_exit(10), Some(1), "{}", ada.stderr());
    assert!(second.elapsed() < Duration::from_secs(5), "the second SIGTERM did not end it at once");
    // the pipe reader may still be draining what the process wrote last
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ada.stderr().contains("second stop signal - exiting without the durable close") {
        assert!(Instant::now() < deadline, "no forced-stop line\n{}", ada.stderr());
        std::thread::sleep(Duration::from_millis(50));
    }
}
