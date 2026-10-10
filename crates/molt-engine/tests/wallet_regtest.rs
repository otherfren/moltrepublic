// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §10 "Manuell" + §16: a real monerod in regtest, offline on
//! 127.0.0.1, mines to a 3-seat purse; every seat sees the coinbase
//! outputs pending, then as balance once 20 confirmations and the 60-block
//! coinbase lock have passed; a close and reopen keeps the state.
//!
//! `MOLT_TEST_MONEROD=/path/to/monerod cargo test -p molt-engine --test
//! wallet_regtest -- --ignored --nocapture`

use std::time::{Duration, Instant};

use molt_core::wallet::{WalletPhase, WalletView};
use molt_core::{Command, Reply, Surface};
use molt_engine::WalletHandle;
use molt_treasury::testing::any_address;
use molt_treasury::Network;
use nostr_relay_builder::MockRelay;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod vault_support;
use vault_support::{found_n_prepared, read_session};

const TO_PURSE: u64 = 5;
const UNLOCK: u64 = 60;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

/// One JSON-RPC call to the regtest daemon.
async fn rpc(port: u16, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": "0", "method": method, "params": params }).to_string();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let req = format!(
        "POST /json_rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.ok()?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.ok()?;
    let text = String::from_utf8_lossy(&out);
    let json = text.split_once("\r\n\r\n")?.1;
    serde_json::from_str::<serde_json::Value>(json).ok()?.get("result").cloned()
}

async fn mine(port: u16, blocks: u64, address: &str) -> u64 {
    let r = rpc(port, "generateblocks", serde_json::json!({ "amount_of_blocks": blocks, "wallet_address": address }))
        .await
        .expect("generateblocks");
    r["height"].as_u64().expect("height")
}

async fn wallet(w: &WalletHandle) -> WalletView {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(s)) => *s.wallet.expect("wallet view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wallet(w: &WalletHandle, what: &str, pred: impl Fn(&WalletView) -> bool) -> WalletView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let v = wallet(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\nlast view: {v:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct Monerod(std::process::Child);

impl Drop for Monerod {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs MOLT_TEST_MONEROD (a monerod binary)"]
async fn a_regtest_daemon_mines_into_the_purse() {
    let Ok(bin) = std::env::var("MOLT_TEST_MONEROD") else {
        eprintln!("MOLT_TEST_MONEROD unset: skipped");
        return;
    };
    let started = Instant::now();
    let tmp = tempfile::tempdir().expect("tmp");
    let (rpc_port, p2p_port) = (free_port(), free_port());
    let data = tmp.path().join("monerod");
    let child = std::process::Command::new(&bin)
        .args(["--regtest", "--offline", "--fixed-difficulty", "1", "--non-interactive", "--no-igd", "--hide-my-port", "--no-zmq"])
        .args(["--rpc-bind-ip", "127.0.0.1", "--rpc-bind-port", &rpc_port.to_string()])
        .args(["--p2p-bind-ip", "127.0.0.1", "--p2p-bind-port", &p2p_port.to_string()])
        .arg("--data-dir")
        .arg(&data)
        .arg("--log-file")
        .arg(tmp.path().join("monerod.log"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("monerod starts");
    let _monerod = Monerod(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    while rpc(rpc_port, "get_info", serde_json::json!({})).await.is_none() {
        assert!(Instant::now() < deadline, "monerod never answered");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // regtest takes mainnet addresses
    let nobody = any_address(Network::Mainnet, b"regtest");
    let base = mine(rpc_port, 30, &nobody).await;
    eprintln!("regtest: up after {:?}, height {base}", started.elapsed());

    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let daemon = format!("http://127.0.0.1:{rpc_port}");
    let patch = serde_json::json!({ "wallet_daemon_url": daemon, "wallet_daemon_confirmed": true, "wallet_network": "mainnet" });
    let all = found_n_prepared(tmp.path(), &url, 3, 2, &["memory", "wallet"], &[patch.clone(), patch.clone(), patch]).await;
    for w in &all {
        w.__wallet_deadline(30);
        w.__wallet_scan_poll(Duration::from_millis(500));
    }
    let mut address = String::new();
    for w in &all {
        address = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await.address;
    }
    eprintln!("regtest: purse after {:?}", started.elapsed());

    let paid_to = mine(rpc_port, TO_PURSE, &address).await;
    let mut mined = 0;
    for w in &all {
        let v = wait_wallet(w, "the mined outputs", |v| v.history.len() == usize::try_from(TO_PURSE).expect("n") && v.scan_height >= paid_to).await;
        assert_eq!(v.balance, 0, "coinbase is locked");
        assert!(v.pending > 0);
        mined = v.pending;
    }
    eprintln!("regtest: {TO_PURSE} blocks to the purse, pending {mined} on every seat after {:?}", started.elapsed());

    let top = mine(rpc_port, UNLOCK, &nobody).await;
    for w in &all {
        let v = wait_wallet(w, "the unlocked balance", |v| v.scan_height >= top && v.balance == mined).await;
        assert_eq!(v.pending, 0);
        assert!(v.connected);
    }
    eprintln!("regtest: {UNLOCK} more blocks, balance {mined} on every seat after {:?}", started.elapsed());

    let a = &all[0];
    let ws = read_session(a).await.active_workspace.clone();
    tokio::time::sleep(Duration::from_secs(1)).await;
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");
    let v = wallet(a).await;
    assert_eq!((v.balance, v.scan_height), (mined, top), "the scan state survives a reopen");
    let v = wait_wallet(a, "scanning again", |v| v.connected && v.daemon_height >= top).await;
    assert_eq!(v.balance, mined);
    eprintln!("regtest: reopen keeps balance {mined} at {top}; total {:?}", started.elapsed());
}
