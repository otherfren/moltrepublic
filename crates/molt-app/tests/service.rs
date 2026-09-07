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
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Node {
    fn spawn(dir: &Path, open_on_start: &str) -> Node {
        let port = free_port();
        let mcp_tcp = free_port();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[node]\nheadless = true\nopen_on_start = \"{open_on_start}\"\n\
                 [storage]\nworkspace_dir = \"{}\"\n\
                 [mcp]\nport = {port}\nallow = \"127.0.0.1\"\ntoken = \"{TOKEN}\"\n",
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
        Node { child, port, stderr }
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
