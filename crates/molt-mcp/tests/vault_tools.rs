// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The vault over the MCP surface (vault build plan S4, spec §11): seal,
//! approve, grant and read driven only through tool calls on real nodes,
//! and the read-only key kept away from every vault tool.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SEAT: &str = "seat-key-0123456789";
const READ: &str = "read-key-0123456789";
const NAMES: [&str; 4] = ["a", "b", "c", "d"];

async fn session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(w: &WalletHandle, what: &str, pred: impl Fn(&SessionView) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !pred(&*session(w).await) {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let view = SessionView {
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            mirror_publish_interval_secs: 1,
            mcp_token: SEAT.to_string(),
            mcp_read_token: READ.to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), view)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    w.execute(Command::RelayConfirm { url: url.to_string(), accept_clearnet: true }).await.expect("confirm");
    wait_for(w, "the relay confirmed", |s| {
        s.settings.relays.iter().any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true }).await.expect("unlock");
}

/// A 2-of-4 republic with `features` over `url`, founder first.
async fn found(root: &std::path::Path, url: &str, features: &[&str]) -> Vec<WalletHandle> {
    let a = engine(&root.join(NAMES[0]));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart { name: "R".into(), member: "a".into(), threshold: 2, members: 4, relays: Vec::new() })
        .await
        .expect("founding starts");
    wait_for(&a, "seat links", |s| {
        s.create.seats.len() == 3 && s.create.seats.iter().all(|x| molt_engine::FoundingInvite::parse(&x.link).is_ok())
    })
    .await;
    let links: Vec<String> = session(&a).await.create.seats.iter().map(|x| x.link.clone()).collect();
    let mut all = vec![a];
    for (i, link) in links.into_iter().enumerate() {
        let w = engine(&root.join(NAMES[i + 1]));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: NAMES[i + 1].into() }).await.expect("join");
        all.push(w);
    }
    let a = &all[0];
    wait_for(a, "every join", |s| s.create.can_propose).await;
    let features = features.iter().map(|f| (*f).to_string()).collect();
    a.execute(Command::CreatePropose { name: "R".into(), agenda: "one".into(), features })
        .await
        .expect("charter");
    let seed = session(a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup");
    for w in &all[1..] {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup");
    }
    wait_for(a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("finish");
    for w in &all[1..] {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "entered", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    }
    all
}

/// One node's MCP endpoint on a free loopback port.
async fn serve(w: &WalletHandle) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let h = w.clone();
    tokio::spawn(async move {
        let _ = molt_mcp::serve_listener(h, listener, false, vec!["127.0.0.1".parse().expect("ip")], String::new(), String::new())
            .await;
    });
    port
}

/// One JSON-RPC response to `req` over a fresh connection keyed `token`.
async fn rpc(port: u16, token: &str, req: Value) -> Value {
    let sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "token": token } });
    for msg in [init, req] {
        w.write_all(format!("{msg}\n").as_bytes()).await.expect("write");
    }
    let mut out = Value::Null;
    for _ in 0..2 {
        let line = lines.next_line().await.expect("read").expect("a line");
        out = serde_json::from_str(&line).expect("json");
    }
    out
}

/// A tool call: `Ok(result json)` or `Err(error text)`.
async fn call(port: u16, token: &str, name: &str, args: Value) -> Result<Value, String> {
    let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": args } });
    let resp = rpc(port, token, req).await;
    if let Some(e) = resp.get("error") {
        return Err(e.to_string());
    }
    let text = resp["result"]["content"][0]["text"].as_str().expect("text").to_string();
    if resp["result"]["isError"] == json!(true) {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn vault(port: u16) -> Value {
    let v = call(port, SEAT, "read_state", json!({ "surface": "vault" })).await.expect("read_state");
    // absent while the vault is not enabled
    v.get("vault").cloned().unwrap_or(Value::Null)
}

async fn wait_vault(port: u16, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let v = vault(port).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\n{v}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn items<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v[key].as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

fn id_of(reply: &Value) -> u64 {
    reply.get("id").and_then(Value::as_u64).unwrap_or_else(|| panic!("no id in {reply}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_tools_drive_seal_grant_read() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found(tmp.path(), &url, &["vault"]).await;
    let mut ports = Vec::new();
    for w in &all {
        ports.push(serve(w).await);
    }
    let (a, b, d) = (ports[0], ports[1], ports[3]);
    let text = "violet-anchor-26";

    let sealed = call(a, SEAT, "vault_seal", json!({ "name": "n", "kind": "text", "text": text })).await.expect("seal");
    let deposit = id_of(&sealed);
    wait_vault(b, "the payload at b", |v| items(v, "deposits").iter().any(|x| x["proposal"] == json!(deposit) && x["held"] == json!(true)))
        .await;
    call(b, SEAT, "approve", json!({ "proposal_id": deposit })).await.expect("approve deposit");
    let v = wait_vault(d, "the deposit at d", |v| {
        items(v, "deposits").iter().any(|x| x["name"] == json!("n") && x["state"] != json!("pending") && x["held"] == json!(true))
    })
    .await;
    let sid = items(&v, "deposits")[0]["secret_id"].as_str().expect("secret_id").to_string();
    assert!(!v.to_string().contains(text), "the view never carries the text");

    let granted = call(b, SEAT, "vault_grant", json!({ "secret_id": sid, "reader": "d" })).await.expect("grant");
    let grant = id_of(&granted);
    wait_vault(a, "the grant card at a", |v| items(v, "grants").iter().any(|x| x["proposal"] == json!(grant))).await;
    call(a, SEAT, "approve", json!({ "proposal_id": grant })).await.expect("approve grant");
    wait_vault(d, "the committed grant", |v| items(v, "grants").iter().any(|x| x["state"] == json!("committed") && x["mine"] == json!(true)))
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let read = loop {
        let r = call(d, SEAT, "vault_read", json!({ "secret_id": sid })).await.expect("vault_read");
        if r["reply"] == json!("vault_text") {
            break r;
        }
        assert_eq!(r["reply"], json!("vault_pending"), "{r}");
        assert!(tokio::time::Instant::now() < deadline, "never readable: {r}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!((read["text"].as_str(), read["name"].as_str()), (Some(text), Some("n")));
    let refused = call(b, SEAT, "vault_read", json!({ "secret_id": sid })).await.expect_err("b is not the reader");
    assert!(refused.contains("not the reader"), "{refused}");
}

#[tokio::test]
async fn the_read_only_key_never_reaches_vault_read() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = engine(tmp.path());
    let port = serve(&w).await;
    let listed = rpc(port, READ, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(!names.is_empty());
    for tool in ["vault_seal", "vault_reseal", "vault_grant", "vault_read", "read_state", "approve"] {
        assert!(!names.contains(&tool), "the read key lists {tool}");
        let err = call(port, READ, tool, json!({ "secret_id": "ab".repeat(32) })).await.expect_err("refused");
        assert!(err.contains("read-only"), "{tool}: {err}");
    }
    // the seat key reaches it (and is refused on the merits: no vault here)
    let err = call(port, SEAT, "vault_read", json!({ "secret_id": "ab".repeat(32) })).await.expect_err("no vault");
    assert!(!err.contains("read-only"), "{err}");
}

/// E5 over MCP: a prepared vault refuses every vault tool and
/// `read_state(vault)` carries no `vault` until a `set_features` vote,
/// itself driven over MCP, switches it on (E4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_tools_refuse_until_enabled() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found(tmp.path(), &url, &["memory"]).await;
    let (a, b) = (serve(&all[0]).await, serve(&all[1]).await);
    let sid = "ab".repeat(32);
    for (tool, args) in [
        ("vault_seal", json!({ "name": "n", "kind": "text", "text": "one" })),
        ("vault_reseal", json!({ "secret_id": sid })),
        ("vault_grant", json!({ "secret_id": sid, "reader": "b" })),
        ("vault_read", json!({ "secret_id": sid })),
    ] {
        let err = call(a, SEAT, tool, args).await.expect_err("refused while disabled");
        assert!(err.contains("vault: not enabled"), "{tool}: {err}");
    }
    let state = call(a, SEAT, "read_state", json!({ "surface": "vault" })).await.expect("read_state");
    assert!(state.get("vault").is_none_or(Value::is_null), "no vault section: {state}");
    let status = call(a, SEAT, "status", json!({})).await.expect("status");
    assert_eq!(status["vault_enable"], json!("offer"), "{status}");

    let enable = call(
        a,
        SEAT,
        "propose",
        json!({ "surface": "organization", "payload": { "op": "set_features", "value": "memory vault" } }),
    )
    .await
    .expect("the enable vote");
    let id = id_of(&enable);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if call(b, SEAT, "approve", json!({ "proposal_id": id })).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the card never reached b");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    wait_vault(a, "the vault switched on", |v| v["real"] == json!(true)).await;
    let status = call(a, SEAT, "status", json!({})).await.expect("status");
    assert_eq!(status["vault_enable"], json!("on"), "{status}");
    call(a, SEAT, "vault_seal", json!({ "name": "n", "kind": "text", "text": "one" })).await.expect("seal once enabled");
}
