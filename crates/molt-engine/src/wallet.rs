// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse (`docs/chain/wallet_treasury_design.md`): its read model and
//! command handlers. Runs, the init vote and the scanner land with plan
//! §14 steps 6-8; until then their doors refuse with one line.

use molt_core::wallet::{WalletPhase, WalletRefusal, WalletView};
use molt_core::{MoltError, Reply, SessionScope};

use crate::State;

/// What a not-yet-built door answers.
fn not_yet() -> Result<Reply, MoltError> {
    Err(MoltError::Wallet(WalletRefusal::NotYet))
}

impl State {
    /// The adopted chain's founding rule `(rule_m, rule_n)`: the genesis, or
    /// after a cut the anchor's - never [`State::threshold`] (plan §15).
    pub(crate) fn wallet_rule(&self) -> Option<(u8, u8)> {
        self.chain.head.as_ref()?;
        if let Some(blob) = &self.chain.checkpoint_blob {
            return Some((blob.rule_m, blob.rule_n));
        }
        match self.chain.blocks.first().map(|b| &b.change) {
            Some(molt_core::ChainChange::Genesis { rule_m, rule_n, .. }) => Some((*rule_m, *rule_n)),
            _ => None,
        }
    }

    /// The Wallet surface's read model.
    pub(crate) fn wallet_view(&self) -> WalletView {
        let rule = self.wallet_rule();
        let (m, n) = rule.unwrap_or((0, 0));
        // TODO(step 6): Init from the applied wallet_init; TODO(step 7): Ready
        // with the purse, run, shareholders; TODO(step 8): balance, history.
        let phase = match rule {
            None => WalletPhase::Off,
            Some((m, n)) if !molt_core::wallet::bounds_ok(m, n) => WalletPhase::Bounds,
            Some(_) => WalletPhase::NoPurse,
        };
        WalletView {
            network: self.session.settings.wallet_network.clone(),
            threshold: u32::from(m),
            participants: u32::from(n),
            phase,
            ..WalletView::default()
        }
    }

    /// [`molt_core::Command::WalletInit`].
    pub(crate) fn cmd_wallet_init(&mut self) -> Result<Reply, MoltError> {
        // TODO(step 6): gates W1/chain/no init, daemon probe, the proposal.
        not_yet()
    }

    /// [`molt_core::Command::WalletConsent`].
    pub(crate) fn cmd_wallet_consent(&mut self, _accept: bool) -> Result<Reply, MoltError> {
        // TODO(step 7): readiness/consent frame of the current run.
        not_yet()
    }

    /// [`molt_core::Command::WalletRetry`].
    pub(crate) fn cmd_wallet_retry(&mut self) -> Result<Reply, MoltError> {
        // TODO(step 7): a fresh run nonce for the applied init.
        not_yet()
    }

    /// The INTERNAL wallet feeds (probe, frames, scanner, status, view answer).
    pub(crate) fn cmd_net_wallet_unbuilt(&mut self) -> Result<Reply, MoltError> {
        // TODO(step 6): NetWalletProbe; TODO(step 7): NetWalletFrame,
        // NetWalletStatus, NetWalletViewAnswer; TODO(step 8): NetWalletScan.
        not_yet()
    }

    /// [`molt_core::Command::WalletAcknowledgeLoss`] (W5): the open
    /// workspace's damaged keys file moves aside, the seat is watch-only,
    /// and the backup ticker's hold is lifted.
    pub(crate) fn cmd_wallet_acknowledge_loss(&mut self) -> Result<Reply, MoltError> {
        let Some(active) = self.active.as_ref() else {
            return Err(MoltError::Storage("no workspace open".to_string()));
        };
        let id = active.id.clone();
        // an export in flight already read the file; its late failure would re-arm the hold
        if self.backup_inflight.contains(&id) {
            return Err(MoltError::WorkspaceBusy(
                "backup running - retry once it completes".to_string(),
            ));
        }
        match active.handle.set_aside_wallet_keys_blocking() {
            Ok(true) => {}
            Ok(false) => return Err(MoltError::Wallet(WalletRefusal::NoKeysFile)),
            Err(molt_storage::StorageError::WalletKeysIntact) => {
                return Err(MoltError::Wallet(WalletRefusal::KeysIntact));
            }
            Err(e) => return Err(MoltError::Storage(e.to_string())),
        }
        tracing::warn!(id, "wallet_keys=set_aside seat=watch_only");
        self.backup_hold.remove(&id);
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.backup_error.clear();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }
}
