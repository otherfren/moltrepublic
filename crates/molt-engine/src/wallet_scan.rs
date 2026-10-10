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
    /// The progress, from the file or the task; bound to its purse.
    state: Option<ScanState>,
    /// The bytes last handed to the writer.
    saved: Option<Zeroizing<Vec<u8>>>,
    /// The writer dropped the last save.
    save_due: bool,
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
    pub(crate) daemon_height: Option<u64>,
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
    /// The task stops; its queued reports go stale, its daemon's word too.
    pub(crate) fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.key = None;
        self.generation += 1;
        self.daemon_height = None;
        self.connected = false;
        self.paused = None;
    }

    /// What the view shows apart from the state's content.
    fn mark(&self) -> (bool, Option<u64>, bool, bool, Option<String>) {
        (self.state.is_some(), self.daemon_height, self.connected, self.task.is_some(), self.paused.clone())
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
        let state = match file {
            Ok(None) => None,
            Ok(Some(bytes)) => match ScanState::decode(&bytes) {
                Ok(s) if for_purse(&s, &purse.created) => Some(s),
                Ok(_) => {
                    tracing::warn!("wallet_scan=foreign_purse action=rescan");
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
        // the step drops or keeps the state, never edits it: the cheap mark says
        let before = self.purse.scan.mark();
        self.wallet_scan_step();
        if self.purse.scan.mark() != before {
            self.emit_session(SessionScope::Full);
        }
    }

    fn wallet_scan_step(&mut self) {
        let purse = self.wallet_purse();
        let foreign = self.purse.scan.state.as_ref().is_some_and(|s| !purse.as_ref().is_some_and(|p| for_purse(s, &p.created)));
        if foreign {
            tracing::info!("wallet_scan=purse_changed action=rescan");
            self.purse.scan.state = None;
            self.purse.scan.saved = None;
        }
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
        let state = self.purse.scan.state.take().unwrap_or_else(|| ScanState::new(&w.address, w.birthday));
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
        let before = self.purse.scan.mark();
        let mut changed = false;
        let scan = &mut self.purse.scan;
        if !r.state.is_empty() {
            match ScanState::decode(&r.state) {
                Ok(s) => {
                    changed = scan.state.as_ref() != Some(&s);
                    scan.state = Some(s);
                }
                Err(e) => tracing::error!(error = %e, "wallet_scan=bad_report"),
            }
        }
        // an unchanged state comes as empty: a dropped save is retried from memory
        let bytes = if !r.state.is_empty() {
            Some(Zeroizing::new(r.state))
        } else {
            scan.save_due.then(|| scan.state.as_ref().map(|s| Zeroizing::new(s.encode()))).flatten()
        };
        if let Some(bytes) = bytes.filter(|b| scan.saved.as_ref() != Some(b)) {
            let ok = self.active.as_ref().is_some_and(|a| a.handle.save_wallet_scan(bytes.clone()));
            self.purse.scan.save_due = !ok;
            if ok {
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
        if let Some(h) = r.daemon_height.filter(|_| r.connected) {
            scan.daemon_height = Some(h);
        }
        let newly_paused = r.paused.is_some() && scan.paused != r.paused;
        scan.paused = r.paused;
        if newly_paused {
            if let Some(reason) = scan.paused.clone().filter(|p| p != DAEMON_FAULT) {
                tracing::warn!(reason = %reason, "wallet_scan=paused");
                self.emit(Event::WalletScanPaused { reason });
            }
        }
        if changed || self.purse.scan.mark() != before {
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// The scan as the purse view shows it; balance against the daemon's
    /// chain height, or the scan's own while none answered.
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
        // the daemon's top block number; a chain holds one block more. Never
        // past the blocks it served and the scan linked: a claimed tip is cheap
        let chain = scan.daemon_height.map_or(state.next_height(), |h| h.saturating_add(1).min(state.next_height()));
        let (balance, pending) = state.balance(chain);
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
                    confirmations: chain.saturating_sub(o.height),
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

/// `state` was scanned for this purse.
fn for_purse(state: &ScanState, purse: &crate::wallet_run::Created) -> bool {
    state.address() == purse.address && state.birthday() == purse.birthday
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
    /// `last`: the bytes sent before; unchanged ones go as empty.
    async fn report(&self, state: &ScanState, last: &mut Zeroizing<Vec<u8>>, tip: Option<u64>, outcome: &Outcome) -> bool {
        let (paused, error, connected) = outcome.verdict();
        let bytes = state.encode();
        let bytes = if bytes == **last {
            Vec::new()
        } else {
            (**last).clone_from(&bytes);
            bytes
        };
        let cmd = Command::NetWalletScan {
            scan_height: state.next_height().saturating_sub(1),
            daemon_height: outcome.tip(tip),
            paused,
            error,
            connected,
            state: molt_core::vault::SecretBytes(bytes),
            generation: Some(self.generation),
        };
        let reply = tokio::sync::oneshot::channel().0;
        self.cmd_tx.send(crate::Envelope { cmd, reply }).await.is_ok()
    }
}

impl Outcome {
    /// The round's tip, where the round could trust it.
    fn tip(&self, tip: Option<u64>) -> Option<u64> {
        match self {
            Self::CaughtUp | Self::More | Self::Paused(_) => tip,
            Self::Fault(_) | Self::Down(_) | Self::Syncing(_) => None,
        }
    }

    /// `(paused, error, connected)` as reported.
    fn verdict(&self) -> (Option<String>, String, bool) {
        match self {
            Self::CaughtUp | Self::More => (None, String::new(), true),
            Self::Paused(f) => (f.paused().map(str::to_string), f.to_string(), true),
            Self::Fault(e) => (Some(DAEMON_FAULT.to_string()), e.clone(), true),
            Self::Down(e) => (None, e.clone(), false),
            Self::Syncing(e) => (None, e.clone(), true),
        }
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
    let mut pace = Pace::new(base);
    let mut last = Zeroizing::new(Vec::new());
    loop {
        let mut tip = None;
        let outcome = round(&transport, &mut scanner, &mut state, &mut tip).await;
        if !feed.report(&state, &mut last, tip, &outcome).await {
            return;
        }
        if let Some(wait) = pace.after(&outcome) {
            tokio::time::sleep(wait).await;
        }
    }
}

/// The waits between rounds.
struct Pace {
    poll: Duration,
    retry: Duration,
    paused: Duration,
    backoff: Duration,
}

impl Pace {
    /// `base` (the test seam) replaces every wait.
    fn new(base: Option<Duration>) -> Self {
        let retry = base.unwrap_or(Duration::from_secs(RETRY_SECS));
        Self {
            poll: base.unwrap_or(Duration::from_secs(POLL_SECS)),
            retry,
            paused: base.unwrap_or(Duration::from_secs(PAUSED_SECS)),
            backoff: retry,
        }
    }

    /// The wait after `outcome`; none to go on at once.
    fn after(&mut self, outcome: &Outcome) -> Option<Duration> {
        match outcome {
            Outcome::More => None,
            Outcome::CaughtUp => {
                self.backoff = self.retry;
                Some(self.poll)
            }
            Outcome::Paused(_) => Some(self.paused),
            Outcome::Fault(_) | Outcome::Down(_) | Outcome::Syncing(_) => {
                let w = self.backoff;
                self.backoff = (w * 2).min(Duration::from_secs(RETRY_MAX_SECS));
                Some(w)
            }
        }
    }
}

/// One batch: the tip, the blocks above the cursor, each scanned and applied.
async fn round(
    transport: &DaemonTransport,
    scanner: &mut StandardScanner,
    state: &mut ScanState,
    tip_out: &mut Option<u64>,
) -> Outcome {
    let failed = |e: DaemonError| match e {
        DaemonError::Fault(_) => Outcome::Fault(e.to_string()),
        DaemonError::Syncing => Outcome::Syncing(e.to_string()),
        other => Outcome::Down(other.to_string()),
    };
    let tip = match monero_rpc::daemon_height(transport.clone()).await {
        Ok(h) => h,
        Err(e) => return failed(e),
    };
    *tip_out = Some(tip);
    let from = state.next_height();
    if from > tip {
        return Outcome::CaughtUp;
    }
    let to = tip.min(from.saturating_add(BATCH - 1));
    let blocks = match monero_rpc::scannable_blocks(transport.clone(), from, to).await {
        Ok(b) => b,
        Err(e) => return failed(e),
    };
    for b in blocks {
        let Ok(height) = u64::try_from(b.block.number()) else {
            return Outcome::Fault("daemon fault: block number".to_string());
        };
        let (hash, previous, at) = (b.block.hash(), b.block.header.previous, b.block.header.timestamp);
        let found = match scanner.scan_block(b) {
            Ok(found) => found,
            Err(f @ ScanFault::UpdateNeeded(_)) => return Outcome::Paused(f),
            Err(f @ ScanFault::Daemon(_)) => return Outcome::Fault(f.to_string()),
        };
        match state.apply(height, hash, previous, at, found) {
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
            Step::Full => return Outcome::Fault("daemon fault: too many outputs".to_string()),
        }
    }
    if state.next_height() > tip {
        Outcome::CaughtUp
    } else {
        Outcome::More
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::test_support::{genesis_seat, Builder};
    use crate::wallet_run::tests::{purse_run, seat_with_purse, with_init};
    use molt_treasury::scan::{Lock, Received};
    use molt_treasury::Network;

    fn out(key: u8, tx: u8, height: u64, amount: u64) -> Received {
        Received { key: [key; 32], amount, height, at: 1_700_000_000 + height, tx: [tx; 32], index_in_tx: u64::from(key), lock: Lock::None }
    }

    /// A state for `address` from `birthday` with one paid block.
    fn paid(address: &str, birthday: u64) -> ScanState {
        let mut s = ScanState::new(address, birthday);
        s.apply(birthday, [1; 32], [0; 32], 1_700_000_000, vec![out(1, 7, birthday, 5)]);
        s
    }

    /// Empty linked blocks on top of `s` through `to`.
    fn scanned_to(mut s: ScanState, to: u64) -> ScanState {
        while s.next_height() <= to {
            let h = s.next_height();
            let hash = |h: u64| Sha256::digest(h.to_le_bytes()).into();
            let previous = if h == s.birthday() + 1 { [1; 32] } else { hash(h - 1) };
            s.apply(h, hash(h), previous, 1_700_000_000, Vec::new());
        }
        s
    }

    /// One row per transaction, its outputs summed, newest first; the
    /// daemon's top block counts as a confirmation (Monero's convention).
    #[test]
    fn history_sums_a_transaction_and_lists_the_newest_first() {
        let b = Builder::new(&["a", "b", "c"], 2);
        let mut st = genesis_seat("a", &b, b.blocks.clone());
        let mut s = ScanState::new("p", 100);
        s.apply(100, [1; 32], [0; 32], 0, vec![out(1, 7, 100, 5), out(2, 7, 100, 6)]);
        s.apply(101, [2; 32], [1; 32], 0, vec![out(3, 8, 101, 4)]);
        s.apply(102, Sha256::digest(102u64.to_le_bytes()).into(), [2; 32], 0, Vec::new());
        st.purse.scan.state = Some(scanned_to(s, 119));
        st.purse.scan.daemon_height = Some(119);
        let shown = st.wallet_scan_shown();
        let rows: Vec<(u64, u64, u64)> = shown.history.iter().map(|t| (t.height, t.amount, t.confirmations)).collect();
        assert_eq!(rows, vec![(101, 4, 19), (100, 11, 20)]);
        assert_eq!((shown.balance, shown.pending, shown.scan_height), (11, 4, 119));
        assert!(shown.history.iter().all(|t| t.incoming));
        assert_eq!(shown.connected, None, "no scanner: the probe speaks");
    }

    /// A scan file is the progress of one purse: another address or
    /// birthday costs a rescan.
    #[test]
    fn a_scan_file_of_another_purse_is_not_adopted() {
        let (mut st, c) = seat_with_purse("a", &[]);
        let file = |s: ScanState| Ok(Some(Zeroizing::new(s.encode())));
        st.wallet_scan_on_open(file(paid("another purse", c.birthday)));
        assert!(st.purse.scan.state.is_none(), "another address");
        st.wallet_scan_on_open(file(paid(&c.address, c.birthday + 1)));
        assert!(st.purse.scan.state.is_none(), "another birthday");
        st.wallet_scan_on_open(file(paid(&c.address, c.birthday)));
        assert!(st.purse.scan.state.is_some(), "its own");
    }

    /// A purse swapped in session drops the old progress at once, with no
    /// daemon to start a scanner for the new one.
    #[test]
    fn a_swapped_purse_drops_the_old_progress_without_a_daemon() {
        let b = with_init();
        let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
        let (mut st, c) = seat_with_purse("a", &records[..1]);
        st.session.settings.wallet_daemon_url = String::new();
        st.purse.scan.state = Some(scanned_to(paid("another purse", c.birthday), c.birthday + 100));
        st.purse.scan.daemon_height = Some(c.birthday + 100);
        assert_eq!(st.wallet_view().balance, 5, "shown before the beat");
        st.wallet_scan_tick();
        assert!(st.purse.scan.state.is_none());
        assert_eq!((st.wallet_view().balance, st.wallet_view().history.len()), (0, 0));
    }

    /// A restarted scanner starts without the old daemon's word.
    #[test]
    fn a_stop_forgets_the_old_daemons_word() {
        let (mut st, c) = seat_with_purse("a", &[]);
        st.purse.scan.state = Some(paid(&c.address, c.birthday));
        st.purse.scan.key = Some([1; 32]);
        st.purse.scan.paused = Some("update needed".to_string());
        st.purse.scan.daemon_height = Some(c.birthday + 100);
        st.purse.scan.connected = true;
        st.wallet_scan_tick();
        let v = st.wallet_view();
        assert_eq!(v.scan_paused, None);
        assert_ne!(v.daemon_height, c.birthday + 100);
        assert_eq!((v.balance, v.pending), (0, 0), "no view key");
    }

    /// The balance shows only where the view key is: a key part's, or one
    /// handed over.
    #[test]
    fn only_a_seat_with_the_view_key_shows_the_balance() {
        let b = with_init();
        let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
        let (mut c, created) = seat_with_purse("c", &[]);
        c.purse.scan.state = Some(scanned_to(paid(&created.address, created.birthday), created.birthday + 100));
        c.purse.scan.daemon_height = Some(created.birthday + 100);
        let v = c.wallet_view();
        assert_eq!((v.balance, v.pending, v.history.len(), v.can_watch), (0, 0, 0, false), "a stale file shows nothing");
        c.cmd_net_wallet_view_answer(&"a".to_string(), &records[0].view[..]).expect("ack");
        let v = c.wallet_view();
        assert_eq!((v.balance, v.history.len(), v.can_watch), (5, 1, true));
    }

    /// Design §7: a claimed tip never confirms more than the blocks the
    /// daemon served and the scan linked.
    #[test]
    fn confirmations_count_only_scanned_blocks() {
        let b = with_init();
        let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
        let (mut st, c) = seat_with_purse("a", &records[..1]);
        st.purse.scan.state = Some(paid(&c.address, c.birthday));
        st.purse.scan.daemon_height = Some(c.birthday + 10_000);
        let v = st.wallet_view();
        assert_eq!((v.balance, v.pending), (0, 5));
        assert_eq!(v.history[0].confirmations, 1);
    }

    /// A round that got no trusted tip (syncing, a fault, down) reports
    /// none, and the last known height stands.
    #[tokio::test]
    async fn a_round_without_a_tip_keeps_the_known_height() {
        assert_eq!(Outcome::CaughtUp.tip(Some(7)), Some(7));
        assert_eq!(Outcome::Paused(molt_treasury::scan::ScanFault::UpdateNeeded(1)).tip(Some(7)), Some(7));
        assert_eq!(Outcome::Fault("bad".into()).tip(Some(7)), None);
        assert_eq!(Outcome::Syncing("syncing".into()).tip(None), None);
        let (mut st, c) = seat_with_purse("a", &[]);
        st.purse.scan.task = Some(tokio::spawn(async {}));
        let generation = Some(st.purse.scan.generation);
        let report = |daemon_height| ScanReport {
            scan_height: c.birthday,
            daemon_height,
            paused: None,
            error: "daemon: syncing".to_string(),
            connected: true,
            state: Vec::new(),
            generation,
        };
        let first = report(None);
        st.cmd_net_wallet_scan(first).expect("ack");
        assert_eq!(st.purse.scan.daemon_height, None);
        st.purse.scan.daemon_height = Some(c.birthday + 50);
        let again = report(None);
        st.cmd_net_wallet_scan(again).expect("ack");
        assert_eq!(st.purse.scan.daemon_height, Some(c.birthday + 50));
    }

    /// A syncing daemon answers: connected, not paused; a fault is not the fork.
    #[test]
    fn a_syncing_daemon_is_connected_and_a_fault_is_not_the_fork() {
        assert_eq!(Outcome::Syncing("syncing".into()).verdict(), (None, "syncing".to_string(), true));
        assert_eq!(Outcome::Down("down".into()).verdict(), (None, "down".to_string(), false));
        let (paused, _, connected) = Outcome::Fault("bad".into()).verdict();
        assert_eq!((paused.as_deref(), connected), (Some(DAEMON_FAULT), true));
        let (paused, _, _) = Outcome::Paused(molt_treasury::scan::ScanFault::UpdateNeeded(17)).verdict();
        assert_eq!(paused.as_deref(), Some("update needed"));
    }

    /// Failures back off doubling to the cap; a good round resets it.
    #[test]
    fn a_failing_daemon_backs_off_to_the_cap() {
        let mut p = Pace::new(None);
        let down = Outcome::Down(String::new());
        let waits: Vec<u64> = (0..8).filter_map(|_| p.after(&down)).map(|d| d.as_secs()).collect();
        assert_eq!(waits, [10, 20, 40, 80, 160, 320, 600, 600]);
        assert_eq!(p.after(&Outcome::CaughtUp), Some(Duration::from_secs(POLL_SECS)));
        assert_eq!(p.after(&Outcome::Syncing(String::new())), Some(Duration::from_secs(RETRY_SECS)));
        assert_eq!(p.after(&Outcome::More), None);
    }
}
