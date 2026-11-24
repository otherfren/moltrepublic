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

    /// `Ok(value)` or the refusal text.
    fn tool(&mut self, name: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let r = self.rpc("tools/call", serde_json::json!({ "name": name, "arguments": args }));
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        if r["result"]["isError"] == serde_json::json!(true) || r.get("error").is_some() {
            return Err(format!("{text}{}", r["error"]));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> serde_json::Value {
        self.tool(name, args).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn wait(&mut self, what: &str, secs: u64, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let s = self.call("read_session", serde_json::json!({}));
            if pred(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn chat_bodies(&mut self) -> Vec<String> {
        let v = self.call("read_state", serde_json::json!({ "surface": "chat" }));
        v["applied"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["body"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
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

/// **A SIGTERM right after the last `chat_send` delivers every message**
/// (delivery guarantee §12): the close's final pass publishes what was
/// recorded before the stop, so the other seat holds all five while the
/// stopped seat stays down.
#[test]
fn a_sigterm_right_after_sending_delivers_every_message() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let relay = rt.block_on(nostr_relay_builder::MockRelay::run()).expect("relay");
    let url = rt.block_on(relay.url()).to_string();
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let mut ada = relay_seat(dir_a.path(), &url);
    let ben = relay_seat(dir_b.path(), &url);
    let mut a = Mcp::open(ada.port);
    let mut b = Mcp::open(ben.port);
    let confirmed = |s: &serde_json::Value| {
        s["settings"]["relays"].as_array().is_some_and(|r| r.iter().any(|r| r["confirmed"] == serde_json::json!(true)))
    };
    a.wait("ada's relay", 60, confirmed);
    b.wait("ben's relay", 60, confirmed);

    let deadline = Instant::now() + Duration::from_secs(60);
    while let Err(e) = a.tool(
        "create_start",
        serde_json::json!({ "name": "Club", "member": "ada", "threshold": 2, "members": 2 }),
    ) {
        assert!(Instant::now() < deadline, "create_start: {e}");
        std::thread::sleep(Duration::from_secs(1));
    }
    let s = a.wait("the invite", 60, |s| {
        s["create"]["seats"][0]["link"].as_str().is_some_and(|l| l.starts_with("molt://invite/"))
    });
    let link = s["create"]["seats"][0]["link"].as_str().expect("link").to_string();
    b.call("join_start", serde_json::json!({ "invite": link, "member": "ben" }));
    a.wait("the join", 120, |s| s["create"]["can_propose"] == serde_json::json!(true));
    a.call("create_propose", serde_json::json!({ "name": "Club", "agenda": "", "features": ["memory"] }));
    b.wait("the charter", 120, |s| s["join"]["awaiting_ratify"] == serde_json::json!(true));
    b.call("join_confirm_charter", serde_json::json!({}));
    let s = b.wait("ben's backup step", 120, |s| s["join"]["awaiting_backup"] == serde_json::json!(true));
    b.call("confirm_seed_backup", serde_json::json!({ "phrase": s["join"]["seed"] }));
    let s = a.wait("ada's backup step", 120, |s| s["create"]["seed"].as_str().is_some_and(|p| !p.is_empty()));
    a.call("confirm_seed_backup", serde_json::json!({ "phrase": s["create"]["seed"] }));
    let sealed = |side: &'static str| {
        move |s: &serde_json::Value| {
            s[side]["outcome"] == serde_json::json!(1) || s[side]["run"]["outcome"] == serde_json::json!(1)
        }
    };
    a.wait("ada's seal", 120, sealed("create"));
    a.call("create_finish", serde_json::json!({}));
    b.wait("ben's seal", 120, sealed("join"));
    b.call("join_finish", serde_json::json!({}));
    let open = |s: &serde_json::Value| s["active_workspace"].as_str().is_some_and(|w| !w.is_empty());
    a.wait("ada's workspace", 60, open);
    b.wait("ben's workspace", 60, open);

    a.call("chat_send", serde_json::json!({ "body": "warm" }));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !b.chat_bodies().iter().any(|m| m == "warm") {
        assert!(Instant::now() < deadline, "the live path never worked\n{}", ada.stderr());
        std::thread::sleep(Duration::from_millis(200));
    }

    let bodies: Vec<String> = (0..5).map(|i| format!("last {i}")).collect();
    for body in &bodies {
        a.call("chat_send", serde_json::json!({ "body": body }));
    }
    ada.sigterm();
    assert_eq!(ada.wait_exit(180), Some(0), "{}", ada.stderr());

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = b.chat_bodies();
        let landed = bodies.iter().filter(|m| got.contains(m)).count();
        if landed == bodies.len() {
            break;
        }
        assert!(Instant::now() < deadline, "{landed} of {} landed\n{}", bodies.len(), ada.stderr());
        std::thread::sleep(Duration::from_millis(200));
    }
}
