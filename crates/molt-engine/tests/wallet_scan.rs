// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §14 step 8 (§7.9, design §7, §9), end to end over a relay:
//! every seat scans the purse from its own stub daemon serving real
//! blocks. Pending until 20 confirmations, a reorg rewinds, a damaged scan
//! file rescans, the fork pauses, a decode error is a daemon fault, a close
//! stops the scanner. A payment is a block paying the purse's address.

use std::time::Duration;

use molt_core::wallet::{WalletPhase, WalletView};
use molt_core::{Command, Event, Reply, Surface};
use molt_engine::WalletHandle;
use molt_net::monero_rpc::stub::{StubBlock, StubConfig, StubDaemon};
use molt_treasury::testing::{block, chain, Pay, TestBlock, Timelock};
use molt_treasury::Network;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{found_n_prepared, read_session};

const DEADLINE: u64 = 30;
/// The chain the daemons serve before the purse exists; the birthday is
/// 10 below its tip.
const BASE: u64 = 4900;
const TIP: u64 = 5000;
const AMOUNT: u64 = 1_250_000_000_000;

async fn wallet(w: &WalletHandle) -> WalletView {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(s)) => *s.wallet.expect("wallet view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_wallet(w: &WalletHandle, what: &str, pred: impl Fn(&WalletView) -> bool) -> WalletView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    loop {
        let v = wallet(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\nlast view: {v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The blocks every daemon serves, from [`BASE`].
struct Chain {
    blocks: Vec<TestBlock>,
}

impl Chain {
    fn new() -> Self {
        Self { blocks: chain(BASE, TIP, [9; 32], 0, &[]) }
    }

    fn tip(&self) -> &TestBlock {
        self.blocks.last().expect("a tip")
    }

    /// Blocks on the tip, the first one paying `pay`.
    fn grow(&mut self, count: u64, pay: Option<Pay<'_>>) {
        let from = self.tip().number + 1;
        let pays: Vec<(u64, Pay<'_>)> = pay.into_iter().map(|p| (from, p)).collect();
        let more = chain(from, from + count - 1, self.tip().hash, 0, &pays);
        self.blocks.extend(more);
    }

    /// From `height` on, another branch of `count` blocks.
    fn fork(&mut self, height: u64, count: u64) {
        self.blocks.retain(|b| b.number < height);
        let more = chain(height, height + count - 1, self.tip().hash, 1, &[]);
        self.blocks.extend(more);
    }

    fn serve(&self, ds: &[StubDaemon]) {
        let blocks: Vec<StubBlock> =
            self.blocks.iter().map(|b| StubBlock { blob: b.blob.clone(), outputs: b.outputs }).collect();
        for d in ds {
            d.set_chain(BASE, blocks.clone());
        }
    }
}

fn pay(address: &str) -> Pay<'_> {
    Pay { address, network: Network::Mainnet, amount: AMOUNT, lock: Timelock::None }
}

/// Three seats, 2-of-3, wallet in the charter, a daemon each serving
/// [`Chain::new`]; back once every seat sees the purse.
async fn purse(url: &str, tmp: &std::path::Path) -> (Vec<WalletHandle>, Vec<StubDaemon>, Chain, String) {
    let c = Chain::new();
    let mut ds = Vec::new();
    let mut patches = Vec::new();
    for _ in 0..3 {
        let d = StubDaemon::start(TIP, StubConfig::default()).await;
        patches.push(serde_json::json!({
            "wallet_daemon_url": d.url, "wallet_daemon_confirmed": true, "wallet_network": "mainnet"
        }));
        ds.push(d);
    }
    c.serve(&ds);
    let all = found_n_prepared(tmp, url, 3, 2, &["memory", "wallet"], &patches).await;
    let mut address = String::new();
    for w in &all {
        w.__wallet_deadline(DEADLINE);
        w.__wallet_scan_poll(Duration::from_millis(200));
    }
    for w in &all {
        address = wait_wallet(w, "the purse", |v| v.phase == WalletPhase::Ready).await.address;
    }
    (all, ds, c, address)
}

/// Plan §7.9 (design §7): a payment is pending until 20 confirmations,
/// then balance, on every seat; a reorg that drops it rewinds the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payment_confirms_after_20_blocks_and_a_reorg_rewinds_it() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, ds, mut c, address) = purse(&url, tmp.path()).await;
    for w in &all {
        let v = wait_wallet(w, "caught up", |v| v.scan_height == TIP).await;
        assert!(v.connected);
        assert_eq!((v.balance, v.pending, v.daemon_height), (0, 0, TIP));
        assert!(v.history.is_empty());
    }
    c.grow(1, Some(pay(&address)));
    c.serve(&ds);
    let paid = TIP + 1;
    for w in &all {
        let v = wait_wallet(w, "the payment", |v| v.pending == AMOUNT).await;
        assert_eq!(v.balance, 0);
        assert_eq!(v.history.len(), 1);
        let tx = &v.history[0];
        assert!(tx.incoming);
        assert_eq!((tx.amount, tx.height, tx.confirmations), (AMOUNT, paid, 1), "the top block counts");
        assert_eq!(tx.txid.len(), 64);
        assert!(tx.at.is_some_and(|t| t > 1_600_000_000), "the block's time");
        assert!(!v.can_spend);
    }
    c.grow(18, None);
    c.serve(&ds);
    for w in &all {
        let v = wait_wallet(w, "18 blocks on", |v| v.scan_height == paid + 18).await;
        assert_eq!((v.balance, v.pending), (0, AMOUNT), "19 confirmations are not enough");
    }
    c.grow(1, None);
    c.serve(&ds);
    for w in &all {
        let v = wait_wallet(w, "20 confirmations", |v| v.balance == AMOUNT).await;
        assert_eq!(v.pending, 0);
        assert_eq!(v.history[0].confirmations, 20);
    }
    // the network drops the paying block: every seat forgets the payment
    c.fork(paid, 25);
    c.serve(&ds);
    let top = paid + 24;
    for w in &all {
        let v = wait_wallet(w, "the reorg", |v| v.scan_height == top).await;
        assert_eq!((v.balance, v.pending), (0, 0));
        assert!(v.history.is_empty(), "{:?}", v.history);
    }
}

/// Design §9: a block this build cannot read pauses scanning loudly and
/// keeps the funds; a block that does not decode is the daemon's fault,
/// and scanning resumes once the daemon answers well again; an
/// unreachable daemon shows as disconnected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fork_pauses_and_a_bad_daemon_is_a_fault() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, ds, mut c, address) = purse(&url, tmp.path()).await;
    let a = &all[0];
    c.grow(1, Some(pay(&address)));
    c.serve(&ds);
    wait_wallet(a, "the payment", |v| v.pending == AMOUNT).await;

    ds[0].set_garbage(true);
    c.grow(1, None);
    c.serve(&ds);
    let v = wait_wallet(a, "the fault", |v| v.scan_paused.as_deref() == Some("daemon fault")).await;
    assert!(v.connected, "the daemon answered, wrongly");
    assert_eq!(v.pending, AMOUNT, "funds kept");
    ds[0].set_garbage(false);
    let v = wait_wallet(a, "resumed", |v| v.scan_paused.is_none() && v.scan_height == TIP + 2).await;
    assert_eq!(v.pending, AMOUNT);

    let mut events = a.subscribe();
    let tip = c.tip().clone();
    c.blocks.push(block(tip.number + 1, tip.hash, 17, 0, None));
    c.serve(&ds);
    let v = wait_wallet(a, "the pause", |v| v.scan_paused.as_deref() == Some("update needed")).await;
    assert_eq!(v.pending, AMOUNT, "funds kept");
    assert_eq!(v.scan_height, TIP + 2, "nothing past the fork");
    let mut paused = false;
    loop {
        match events.try_recv() {
            Ok(e) => paused |= matches!(&e, Event::WalletScanPaused { reason } if reason == "update needed"),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    assert!(paused, "the pause reaches the frontends");

    let gone = StubDaemon::start(1, StubConfig::default()).await;
    let dead = gone.url.clone();
    drop(gone);
    a.execute(Command::PatchSettings {
        patch: serde_json::json!({ "wallet_daemon_url": dead, "wallet_daemon_confirmed": true }),
    })
    .await
    .expect("daemon moved");
    let v = wait_wallet(a, "disconnected", |v| !v.connected).await;
    assert_eq!(v.pending, AMOUNT, "funds kept");
}

/// Design §6: a damaged scan file costs a rescan from the birthday, an
/// intact one survives a reopen; a close stops the scanner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_scan_file_rescans_and_a_close_stops_the_scanner() {
    let relay = MockRelay::run().await.expect("relay");
    let tmp = tempfile::tempdir().expect("tmp");
    let url = relay.url().await.to_string();
    let (all, ds, mut c, address) = purse(&url, tmp.path()).await;
    let a = &all[0];
    c.grow(1, Some(pay(&address)));
    c.serve(&ds);
    wait_wallet(a, "the payment", |v| v.pending == AMOUNT).await;
    let ws = read_session(a).await.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&tmp.path().join("a"), &ws).expect("dir");
    let file = dir.join(molt_storage::WALLET_SCAN_FILE);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !file.exists() {
        assert!(tokio::time::Instant::now() < deadline, "the scan file is written");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    a.execute(Command::CloseWorkspace).await.expect("close");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let hits = ds[0].hits();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ds[0].hits(), hits, "a closed workspace scans nothing");

    // an intact file: the payment is there before the daemon answers
    ds[0].set_garbage(true);
    a.execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("reopen");
    let v = wallet(a).await;
    assert_eq!(v.pending, AMOUNT, "the scan state survives a reopen");
    assert_eq!(v.scan_height, TIP + 1);
    a.execute(Command::CloseWorkspace).await.expect("close");

    let mut bytes = std::fs::read(&file).expect("scan file");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&file, &bytes).expect("damage");
    a.execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("reopen");
    let v = wallet(a).await;
    assert_eq!((v.pending, v.history.len()), (0, 0), "nothing read from a damaged file");
    ds[0].set_garbage(false);
    let v = wait_wallet(a, "the rescan", |v| v.pending == AMOUNT && v.scan_height == TIP + 1).await;
    assert_eq!(v.history.len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.execute(Command::CloseWorkspace).await.expect("close");
    let (opened, _) = molt_storage::open_workspace(&dir).expect("open");
    let rewritten = opened.read_wallet_scan().expect("the rescan rewrote the file");
    assert!(rewritten.is_some());
}
