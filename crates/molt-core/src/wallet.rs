// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse's read model and refusals (`docs_archive/chain/wallet_treasury_design.md`,
//! plan §5.2). Additive: every field defaults, so an older snapshot decodes.

use serde::{Deserialize, Serialize};

use crate::MemberId;

/// The networks a purse can live on (design §7, W11).
pub const WALLET_NETWORKS: [&str; 3] = ["mainnet", "stagenet", "testnet"];

/// The default network (W11).
#[must_use]
pub fn default_wallet_network() -> String {
    "mainnet".to_string()
}

/// The daemon login rides an HTTP header: one bounded line.
#[must_use]
pub fn daemon_login_ok(login: &str) -> bool {
    login.len() <= 512 && !login.chars().any(char::is_control)
}

/// Where this republic's purse stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalletPhase {
    /// Not a chain-governed republic: no founding rule to read.
    #[default]
    Off,
    /// The founding rule is outside `2 <= m <= n-1` (W1).
    Bounds,
    /// Within the bounds, no init applied: "Set up the purse".
    NoPurse,
    /// The init is applied, no purse yet: runs happen here.
    Init,
    /// The purse exists.
    Ready,
}

/// One run's stage (plan §7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStage {
    /// Waiting for every seat's readiness and consent.
    #[default]
    Ready,
    /// Round 1 frames arriving.
    Round1,
    /// Round 2 frames arriving.
    Round2,
    /// Attestations arriving.
    Attest,
    /// The terminal block is being sealed.
    Sealing,
    /// The purse committed.
    Done,
    /// The run ended without a purse.
    Aborted,
}

/// A seat's key part as its status frame last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    /// It holds its key part.
    Held,
    /// It sees the purse but holds no key part.
    WatchOnly,
    /// Nothing heard.
    #[default]
    Unknown,
}

/// The progress of the current run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalletRunView {
    /// Where it stands.
    pub stage: RunStage,
    /// Seats done with this stage.
    pub done: u32,
    /// Seats in the run.
    pub of: u32,
    /// Seats not ready yet.
    pub missing: Vec<MemberId>,
    /// Why it aborted, one line.
    pub reason: Option<String>,
    /// This seat still has to consent.
    pub needs_consent: bool,
    /// A daemon URL another seat offered; "" for none.
    pub daemon_hint: String,
}

/// One incoming or outgoing transfer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalletTxView {
    /// Transaction id, hex.
    pub txid: String,
    /// Received (Stage 1 sees nothing else).
    pub incoming: bool,
    /// Piconero.
    pub amount: u64,
    /// Block height; 0 while in the pool.
    pub height: u64,
    /// Block time, unix seconds.
    pub at: Option<u64>,
    /// Confirmations so far.
    pub confirmations: u64,
}

/// The Wallet surface's read model (`SurfaceSnapshot.wallet`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalletView {
    /// The deposit address; "" until the purse exists.
    pub address: String,
    /// [`ADDRESS_WARNING`] beside an address that cannot spend (W11).
    pub address_warning: String,
    /// `mainnet` | `stagenet` | `testnet`.
    pub network: String,
    /// Confirmed balance, piconero.
    pub balance: u64,
    /// Not yet confirmed or unlocked, piconero.
    pub pending: u64,
    /// The scanner's height.
    pub scan_height: u64,
    /// The daemon's height.
    pub daemon_height: u64,
    /// The daemon answers.
    pub connected: bool,
    /// Why scanning stopped: `update needed` (the fork) or `daemon fault`.
    pub scan_paused: Option<String>,
    /// `rule_m`.
    pub threshold: u32,
    /// `rule_n`.
    pub participants: u32,
    /// Where the purse stands.
    pub phase: WalletPhase,
    /// The current run.
    pub run: Option<WalletRunView>,
    /// Set-up voted, no run in memory and none about to start: a seat
    /// starts one by hand (`wallet_retry`).
    pub can_start: bool,
    /// Each seat's key part status.
    pub shareholders: Vec<(MemberId, ShareStatus)>,
    /// Transfers, newest first.
    pub history: Vec<WalletTxView>,
    /// Stage 1: always false.
    pub can_spend: bool,
    /// This seat holds the view key: it can see the balance.
    pub can_watch: bool,
    /// This session founded the republic with the purse: the wizard ends
    /// with the purse stage.
    pub founding: bool,
}

/// W11: what every reader of the address is told while spending is not built.
pub const ADDRESS_WARNING: &str = "spending does not work yet - money sent here is gone";

/// W1: a purse needs `2 <= m <= n-1`.
#[must_use]
pub fn bounds_ok(rule_m: u8, rule_n: u8) -> bool {
    rule_m >= 2 && rule_m < rule_n
}

/// Why a wallet command was refused (`MoltError::Wallet`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalletRefusal {
    /// The step that builds it has not landed.
    #[error("not available yet")]
    NotYet,
    /// Nothing to set aside.
    #[error("no keys file")]
    NoKeysFile,
    /// An intact keys file is never set aside.
    #[error("keys file intact")]
    KeysIntact,
    /// No chain, no founding rule (W1).
    #[error("not a chain republic")]
    NotChain,
    /// The founding rule is outside `2 <= m <= n-1` (W1).
    #[error("needs 2 <= m <= n-1")]
    Bounds,
    /// The purse was set up already.
    #[error("already set up")]
    InitExists,
    /// A set-up vote is open.
    #[error("set-up vote pending")]
    InitPending,
    /// This seat has no daemon: it abstains.
    #[error("no daemon")]
    NoDaemon,
    /// The daemon is being asked.
    #[error("checking the daemon")]
    Checking,
    /// The daemon could not be asked, one line.
    #[error("{0}")]
    Daemon(String),
    /// The birthday lies outside this seat's window.
    #[error("birthday out of range")]
    Birthday,
    /// The vote is for another network than this seat's.
    #[error("other network")]
    Network,
    /// Not a purse op.
    #[error("unknown op")]
    UnknownOp,
    /// Purse votes come only from their commands.
    #[error("use wallet_init")]
    UseInit,
    /// An approve this seat cannot check yet: kept, it signs once it can.
    #[error("{0} - approval held")]
    Held(Box<WalletRefusal>),
    /// The set-up was dropped before its daemon answered.
    #[error("set-up cancelled")]
    Cancelled,
    /// No set-up is running.
    #[error("no set-up running")]
    NoRun,
    /// A set-up is running.
    #[error("set-up running")]
    RunActive,
    /// The purse record is not what this seat computed.
    #[error("not this seat's result")]
    NotMine,
    /// The OS RNG failed: nothing secret is drawn.
    #[error("no randomness")]
    Rng,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The view key lets outsiders watch the purse: wiped on drop.
    #[test]
    fn the_view_answer_wipes_its_key() {
        fn wipes<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        let answer = crate::Command::NetWalletViewAnswer {
            from: "a".to_string(),
            view: crate::vault::SecretBytes(vec![1; 32]),
            generation: None,
        };
        let crate::Command::NetWalletViewAnswer { view, .. } = &answer else {
            panic!("built above");
        };
        wipes(view);
    }

    /// The UI's and MCP's contract: a filled view serializes to exactly this.
    #[test]
    fn wallet_view_json_shape_is_pinned() {
        let view = WalletView {
            address: "4a".to_string(),
            address_warning: ADDRESS_WARNING.to_string(),
            network: "mainnet".to_string(),
            balance: 5,
            pending: 6,
            scan_height: 7,
            daemon_height: 8,
            connected: true,
            scan_paused: Some("update needed".to_string()),
            threshold: 2,
            participants: 3,
            phase: WalletPhase::Ready,
            run: Some(WalletRunView {
                stage: RunStage::Attest,
                done: 1,
                of: 3,
                missing: vec!["b".to_string()],
                reason: None,
                needs_consent: true,
                daemon_hint: "http://x.onion".to_string(),
            }),
            can_start: false,
            shareholders: vec![("a".to_string(), ShareStatus::Held), ("b".to_string(), ShareStatus::WatchOnly)],
            history: vec![WalletTxView {
                txid: "ab".to_string(),
                incoming: true,
                amount: 9,
                height: 10,
                at: Some(11),
                confirmations: 3,
            }],
            can_spend: false,
            can_watch: true,
            founding: true,
        };
        let want = json!({
            "address": "4a",
            "address_warning": "spending does not work yet - money sent here is gone",
            "network": "mainnet",
            "balance": 5,
            "pending": 6,
            "scan_height": 7,
            "daemon_height": 8,
            "connected": true,
            "scan_paused": "update needed",
            "threshold": 2,
            "participants": 3,
            "phase": "ready",
            "run": {
                "stage": "attest",
                "done": 1,
                "of": 3,
                "missing": ["b"],
                "reason": null,
                "needs_consent": true,
                "daemon_hint": "http://x.onion"
            },
            "can_start": false,
            "shareholders": [["a", "held"], ["b", "watch_only"]],
            "history": [{
                "txid": "ab",
                "incoming": true,
                "amount": 9,
                "height": 10,
                "at": 11,
                "confirmations": 3
            }],
            "can_spend": false,
            "can_watch": true,
            "founding": true
        });
        assert_eq!(serde_json::to_value(&view).expect("serializes"), want);
        let back: WalletView = serde_json::from_value(want).expect("deserializes");
        assert_eq!(back, view);
        for (phase, word) in [
            (WalletPhase::Off, "off"),
            (WalletPhase::Bounds, "bounds"),
            (WalletPhase::NoPurse, "no_purse"),
            (WalletPhase::Init, "init"),
        ] {
            assert_eq!(serde_json::to_value(phase).expect("serializes"), json!(word));
        }
        for (stage, word) in [
            (RunStage::Ready, "ready"),
            (RunStage::Round1, "round1"),
            (RunStage::Round2, "round2"),
            (RunStage::Sealing, "sealing"),
            (RunStage::Done, "done"),
            (RunStage::Aborted, "aborted"),
        ] {
            assert_eq!(serde_json::to_value(stage).expect("serializes"), json!(word));
        }
        assert_eq!(serde_json::to_value(ShareStatus::Unknown).expect("serializes"), json!("unknown"));
    }

    /// Additive: an older or partial snapshot decodes with defaults.
    #[test]
    fn an_empty_wallet_view_decodes() {
        let back: WalletView = serde_json::from_value(json!({})).expect("deserializes");
        assert_eq!(back, WalletView::default());
        let run: WalletRunView = serde_json::from_value(json!({"stage": "round2"})).expect("deserializes");
        assert_eq!(run.stage, RunStage::Round2);
    }

    /// W1 at its edges.
    #[test]
    fn bounds_are_two_to_n_minus_one() {
        assert!(!bounds_ok(1, 3), "m = 1 hands every seat the key");
        assert!(bounds_ok(2, 3));
        assert!(!bounds_ok(3, 3), "m = n freezes on one loss");
        assert!(bounds_ok(4, 5));
        assert!(!bounds_ok(0, 0));
    }

    /// The daemon login is write-only: no settings read carries it.
    #[test]
    fn the_daemon_login_never_serializes() {
        let s = crate::SessionSettings {
            wallet_daemon_login: "u:hunter2".to_string(),
            ..crate::SessionSettings::default()
        };
        let v = serde_json::to_value(&s).expect("serializes");
        assert!(!v.to_string().contains("hunter2"));
        assert_eq!(v["wallet_network"], json!("mainnet"));
        let back: crate::SessionSettings = serde_json::from_value(v).expect("deserializes");
        assert!(back.wallet_daemon_login.is_empty());
    }

    /// The refusals read as one compact line.
    #[test]
    fn refusals_are_compact() {
        assert_eq!(
            crate::MoltError::Wallet(WalletRefusal::NotYet).to_string(),
            "purse: not available yet"
        );
    }
}
