// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse's scanner (`docs/chain/wallet_treasury_design.md` §7, §9,
//! plan §7.9): one off-actor task per open workspace with a purse and its
//! view key, scanning from `max(birthday, cursor)` through the daemon
//! transport. It reports through [`molt_core::Command::NetWalletScan`];
//! the actor keeps the state, persists it and fills the view.

use std::time::Duration;

use molt_core::wallet::WalletTxView;
use molt_core::{Command, Event, MoltError, Reply, SessionScope};
use molt_net::monero_rpc::{self, DaemonError, DaemonTransport};
use molt_treasury::scan::{BlockScanner, ScanFault, ScanState, StandardScanner, Step};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::State;

/// Caught up, the daemon is asked again this often.
const POLL_SECS: u64 = 30;
/// A failed round waits this long, doubling up to [`RETRY_MAX_SECS`].
const RETRY_SECS: u64 = 10;
const RETRY_MAX_SECS: u64 = 600;
/// Paused at a fork this build cannot read: asked again this rarely.
const PAUSED_SECS: u64 = 3600;
/// Blocks asked for per round trip.
const BATCH: u64 = 100;
/// The paused line for a daemon that answered something wrong.
pub(crate) const DAEMON_FAULT: &str = "daemon fault";

/// The scanner side of the purse, in memory.
#[derive(Default)]
pub(crate) struct ScanRt {
    task: Option<tokio::task::JoinHandle<()>>,
    /// What the running task scans with (purse, view key, daemon).
    key: Option<[u8; 32]>,
    /// The incarnation; never reset, so a closed workspace's report is stale.
    generation: u64,
    /// The progress, from the file or the task.
    state: Option<ScanState>,
    /// The address `state` belongs to (a reorg may swap the purse).
    state_for: Option<String>,
    /// The bytes last handed to the writer.
    saved: Option<Zeroizing<Vec<u8>>>,
    daemon_height: Option<u64>,
    connected: bool,
    paused: Option<String>,
    error: String,
}

impl std::fmt::Debug for ScanRt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanRt")
            .field("running", &self.task.is_some())
            .field("generation", &self.generation)
            .field("next", &self.state.as_ref().map(ScanState::next_height))
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}

/// One [`Command::NetWalletScan`], unpacked.
pub(crate) struct ScanReport {
    pub(crate) scan_height: u64,
    pub(crate) daemon_height: u64,
    pub(crate) paused: Option<String>,
    pub(crate) error: String,
    pub(crate) connected: bool,
    pub(crate) state: Vec<u8>,
    pub(crate) generation: Option<u64>,
}

/// What the purse view shows of the scan.
pub(crate) struct ScanShown {
    pub(crate) balance: u64,
    pub(crate) pending: u64,
    pub(crate) scan_height: u64,
    pub(crate) daemon_height: Option<u64>,
    pub(crate) connected: Option<bool>,
    pub(crate) paused: Option<String>,
    pub(crate) history: Vec<WalletTxView>,
}

impl ScanRt {
    /// The task stops; its queued reports go stale.
    pub(crate) fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.key = None;
        self.generation += 1;
    }

    /// A workspace closed: nothing of its scan stays.
    pub(crate) fn reset(&mut self) {
        self.stop();
        *self = Self { generation: self.generation, ..Self::default() };
    }
}

impl State {
    /// The open path: the scan file read for the purse; damage costs a
    /// rescan from the birthday.
    pub(crate) fn wallet_scan_on_open(
        &mut self,
        file: Result<Option<Zeroizing<Vec<u8>>>, molt_storage::StorageError>,
    ) {
        let Some(purse) = self.wallet_purse() else {
            return;
        };
        let birthday = purse.created.birthday;
        self.purse.scan.state_for = Some(purse.created.address);
        let state = match file {
            Ok(None) => None,
            Ok(Some(bytes)) => match ScanState::decode(&bytes) {
                Ok(s) if s.birthday() == birthday => Some(s),
                Ok(_) => {
                    tracing::warn!("wallet_scan=foreign_birthday action=rescan");
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "wallet_scan=damaged action=rescan");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "wallet_scan=unreadable action=rescan");
                None
            }
        };
        self.purse.scan.state = state;
    }

    /// The beat: a scanner runs exactly while there is a purse, its view
    /// key and a daemon this node may dial.
    pub(crate) fn wallet_scan_tick(&mut self) {
        let want = self.wallet_scan_want();
        let key = want.as_ref().map(|w| w.key);
        if key == self.purse.scan.key {
            return;
        }
        self.purse.scan.stop();
        let Some(w) = want else {
            return;
        };
        let transport = match self.wallet_daemon_for_scan() {
            Ok(t) => t,
            Err(e) => {
                if self.purse.scan.error != e {
                    tracing::info!(error = %e, "wallet_scan=no_daemon");
                    self.purse.scan.error = e;
                    self.purse.scan.connected = false;
                    self.emit_session(SessionScope::Full);
                }
                // no key: the dial settings are asked again on the next beat
                return;
            }
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade().filter(|_| tokio::runtime::Handle::try_current().is_ok()) else {
            return;
        };
        let scanner = match StandardScanner::for_address(&w.address, w.network, &w.view) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "wallet_scan=no_scanner");
                self.purse.scan.key = key;
                return;
            }
        };
        let ours = self.purse.scan.state_for.as_deref() == Some(w.address.as_str());
        let state = match self.purse.scan.state.take() {
            Some(s) if ours && s.birthday() == w.birthday => s,
            _ => ScanState::new(w.birthday),
        };
        self.purse.scan.state_for = Some(w.address.clone());
        self.purse.scan.state = Some(state.clone());
        let generation = self.purse.scan.generation;
        let base = match self.wallet_seams.scan_poll_ms.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        tracing::info!(from = state.next_height(), "wallet_scan=started");
        self.purse.scan.key = key;
        self.purse.scan.task =
            Some(tokio::spawn(scan_loop(transport, scanner, state, Feed { cmd_tx, generation }, base)));
    }

    fn wallet_daemon_for_scan(&self) -> Result<DaemonTransport, String> {
        let s = &self.session.settings;
        if s.wallet_daemon_url.is_empty() {
            return Err(DaemonError::None.to_string());
        }
        let base = self.dialer_for().map_err(|e| format!("daemon: {e}"))?;
        DaemonTransport::new(
            &s.wallet_daemon_url,
            &s.wallet_daemon_login,
            s.wallet_daemon_confirmed,
            self.clearnet_session,
            &base,
        )
        .map_err(|e| e.to_string())
    }

    /// The purse, its view key and the daemon settings, fingerprinted.
    fn wallet_scan_want(&self) -> Option<Want> {
        self.active.as_ref()?;
        let purse = self.wallet_purse()?;
        let view = self.wallet_held_view()?;
        let s = &self.session.settings;
        let mut h = Sha256::new();
        for part in [
            purse.created.address.as_bytes(),
            &purse.created.birthday.to_le_bytes(),
            &view[..],
            s.wallet_daemon_url.as_bytes(),
            s.wallet_daemon_login.as_bytes(),
            &[u8::from(s.wallet_daemon_confirmed), u8::from(self.clearnet_session)],
        ] {
            h.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
            h.update(part);
        }
        Some(Want {
            key: h.finalize().into(),
            address: purse.created.address,
            network: purse.created.network,
            birthday: purse.created.birthday,
            view,
        })
    }

    /// [`Command::NetWalletScan`]: the scanner's round landed.
    pub(crate) fn cmd_net_wallet_scan(&mut self, r: ScanReport) -> Result<Reply, MoltError> {
        if r.generation != Some(self.purse.scan.generation) || self.purse.scan.task.is_none() {
            return Ok(Reply::Ack);
        }
        let before = self.wallet_scan_shown();
        let scan = &mut self.purse.scan;
        if !r.state.is_empty() {
            match ScanState::decode(&r.state) {
                Ok(s) => scan.state = Some(s),
                Err(e) => tracing::error!(error = %e, "wallet_scan=bad_report"),
            }
            if scan.saved.as_deref() != Some(&r.state) {
                let bytes = Zeroizing::new(r.state);
                if let Some(active) = self.active.as_ref() {
                    active.handle.save_wallet_scan(bytes.clone());
                }
                self.purse.scan.saved = Some(bytes);
            }
        }
        let scan = &mut self.purse.scan;
        if r.connected != scan.connected || r.error != scan.error {
            if r.error.is_empty() {
                tracing::info!(height = r.scan_height, daemon = r.daemon_height, "wallet_scan=ok");
            } else {
                tracing::warn!(error = %r.error, connected = r.connected, "wallet_scan=failed");
            }
        }
        scan.connected = r.connected;
        scan.error = r.error;
        if r.connected {
            scan.daemon_height = Some(r.daemon_height);
        }
        let newly_paused = r.paused.is_some() && scan.paused != r.paused;
        scan.paused = r.paused;
        if newly_paused {
            if let Some(reason) = scan.paused.clone().filter(|p| p != DAEMON_FAULT) {
                tracing::warn!(reason = %reason, "wallet_scan=paused");
                self.emit(Event::WalletScanPaused { reason });
            }
        }
        if self.wallet_scan_shown() != before {
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// The scan as the purse view shows it; balance against the daemon's
    /// height, or the scan's own while none answered.
    pub(crate) fn wallet_scan_shown(&self) -> ScanShown {
        let scan = &self.purse.scan;
        let Some(state) = scan.state.as_ref() else {
            return ScanShown {
                balance: 0,
                pending: 0,
                scan_height: 0,
                daemon_height: scan.daemon_height,
                connected: scan.task.as_ref().map(|_| scan.connected),
                paused: scan.paused.clone(),
                history: Vec::new(),
            };
        };
        let scan_height = state.next_height().saturating_sub(1);
        let tip = scan.daemon_height.unwrap_or(scan_height);
        let (balance, pending) = state.balance(tip);
        let mut txs: std::collections::BTreeMap<(u64, [u8; 32]), WalletTxView> = std::collections::BTreeMap::new();
        for o in state.outputs() {
            txs.entry((o.height, o.tx))
                .and_modify(|t| t.amount = t.amount.saturating_add(o.amount))
                .or_insert_with(|| WalletTxView {
                    txid: hex::encode(o.tx),
                    incoming: true,
                    amount: o.amount,
                    height: o.height,
                    at: Some(o.at),
                    confirmations: tip.saturating_sub(o.height),
                });
        }
        let history: Vec<WalletTxView> = txs.into_values().rev().collect();
        ScanShown {
            balance,
            pending,
            scan_height,
            daemon_height: scan.daemon_height,
            connected: scan.task.as_ref().map(|_| scan.connected),
            paused: scan.paused.clone(),
            history,
        }
    }
}

impl PartialEq for ScanShown {
    fn eq(&self, o: &Self) -> bool {
        (self.balance, self.pending, self.scan_height, self.daemon_height, self.connected, &self.paused, &self.history)
            == (o.balance, o.pending, o.scan_height, o.daemon_height, o.connected, &o.paused, &o.history)
    }
}

struct Want {
    key: [u8; 32],
    address: String,
    network: molt_treasury::Network,
    birthday: u64,
    view: Zeroizing<[u8; 32]>,
}

struct Feed {
    cmd_tx: tokio::sync::mpsc::Sender<crate::Envelope>,
    generation: u64,
}

impl Feed {
    /// `false` once the engine is gone.
    async fn report(&self, state: &ScanState, tip: u64, outcome: &Outcome) -> bool {
        let (paused, error, connected) = match outcome {
            Outcome::CaughtUp | Outcome::More => (None, String::new(), true),
            Outcome::Paused(f) => (f.paused().map(str::to_string), f.to_string(), true),
            Outcome::Fault(e) => (Some(DAEMON_FAULT.to_string()), e.clone(), true),
            Outcome::Down(e) => (None, e.clone(), false),
            Outcome::Syncing(e) => (None, e.clone(), true),
        };
        let cmd = Command::NetWalletScan {
            scan_height: state.next_height().saturating_sub(1),
            daemon_height: tip,
            paused,
            error,
            connected,
            state: molt_core::vault::SecretBytes(state.encode()),
            generation: Some(self.generation),
        };
        let reply = tokio::sync::oneshot::channel().0;
        self.cmd_tx.send(crate::Envelope { cmd, reply }).await.is_ok()
    }
}

/// How one round ended.
enum Outcome {
    /// Nothing left above the cursor.
    CaughtUp,
    /// Blocks left: go on at once.
    More,
    /// A fork this build cannot read.
    Paused(ScanFault),
    /// The daemon answered something wrong.
    Fault(String),
    /// The daemon did not answer.
    Down(String),
    /// The daemon has not caught up with the network.
    Syncing(String),
}

async fn scan_loop(
    transport: DaemonTransport,
    mut scanner: StandardScanner,
    mut state: ScanState,
    feed: Feed,
    base: Option<Duration>,
) {
    let poll = base.unwrap_or(Duration::from_secs(POLL_SECS));
    let retry = base.unwrap_or(Duration::from_secs(RETRY_SECS));
    let mut backoff = retry;
    let mut tip = 0;
    loop {
        let outcome = round(&transport, &mut scanner, &mut state, &mut tip).await;
        if !feed.report(&state, tip, &outcome).await {
            return;
        }
        let wait = match outcome {
            Outcome::More => continue,
            Outcome::CaughtUp => {
                backoff = retry;
                poll
            }
            Outcome::Paused(_) => base.unwrap_or(Duration::from_secs(PAUSED_SECS)),
            Outcome::Fault(_) | Outcome::Down(_) | Outcome::Syncing(_) => {
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(RETRY_MAX_SECS));
                w
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// One batch: the tip, the blocks above the cursor, each scanned and applied.
async fn round(transport: &DaemonTransport, scanner: &mut StandardScanner, state: &mut ScanState, tip: &mut u64) -> Outcome {
    let failed = |e: DaemonError| match e {
        DaemonError::Fault(_) => Outcome::Fault(e.to_string()),
        DaemonError::Syncing => Outcome::Syncing(e.to_string()),
        other => Outcome::Down(other.to_string()),
    };
    *tip = match monero_rpc::daemon_height(transport.clone()).await {
        Ok(h) => h,
        Err(e) => return failed(e),
    };
    let from = state.next_height();
    if from > *tip {
        return Outcome::CaughtUp;
    }
    let to = (*tip).min(from.saturating_add(BATCH - 1));
    let blocks = match monero_rpc::scannable_blocks(transport.clone(), from, to).await {
        Ok(b) => b,
        Err(e) => return failed(e),
    };
    for b in blocks {
        let Ok(height) = u64::try_from(b.block.number()) else {
            return Outcome::Fault("daemon fault: block number".to_string());
        };
        let (hash, previous) = (b.block.hash(), b.block.header.previous);
        let found = match scanner.scan_block(b) {
            Ok(found) => found,
            Err(f @ ScanFault::UpdateNeeded(_)) => return Outcome::Paused(f),
            Err(f @ ScanFault::Daemon(_)) => return Outcome::Fault(f.to_string()),
        };
        match state.apply(height, hash, previous, found) {
            Step::Applied(new) => {
                for o in new {
                    tracing::info!(height, amount = o.amount, "wallet_scan=received");
                }
            }
            Step::Reorg => {
                tracing::warn!(height, next = state.next_height(), "wallet_scan=reorg");
                return Outcome::More;
            }
            Step::OutOfOrder => return Outcome::Fault("daemon fault: out of order".to_string()),
        }
    }
    if state.next_height() > *tip {
        Outcome::CaughtUp
    } else {
        Outcome::More
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::test_support::{genesis_seat, Builder};
    use molt_treasury::scan::{Lock, Received};

    fn out(key: u8, tx: u8, height: u64, amount: u64) -> Received {
        Received { key: [key; 32], amount, height, at: 1_700_000_000 + height, tx: [tx; 32], index_in_tx: u64::from(key), lock: Lock::None }
    }

    /// One row per transaction, its outputs summed, newest first; balance at the daemon's tip.
    #[test]
    fn history_sums_a_transaction_and_lists_the_newest_first() {
        let b = Builder::new(&["a", "b", "c"], 2);
        let mut st = genesis_seat("a", &b, b.blocks.clone());
        let mut s = ScanState::new(100);
        s.apply(100, [1; 32], [0; 32], vec![out(1, 7, 100, 5), out(2, 7, 100, 6)]);
        s.apply(101, [2; 32], [1; 32], vec![out(3, 8, 101, 4)]);
        st.purse.scan.state = Some(s);
        st.purse.scan.daemon_height = Some(120);
        let shown = st.wallet_scan_shown();
        let rows: Vec<(u64, u64, u64)> = shown.history.iter().map(|t| (t.height, t.amount, t.confirmations)).collect();
        assert_eq!(rows, vec![(101, 4, 19), (100, 11, 20)]);
        assert_eq!((shown.balance, shown.pending, shown.scan_height), (11, 4, 101));
        assert!(shown.history.iter().all(|t| t.incoming));
        assert_eq!(shown.connected, None, "no scanner: the probe speaks");
    }
}
