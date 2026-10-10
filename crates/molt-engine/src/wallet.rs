// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse (`docs/chain/wallet_treasury_design.md`): its read model, the
//! init vote (§3.1, the only door) and the command handlers. The run lives
//! in `wallet_run.rs`, this seat's standing (founding stage, status, view
//! key) in `wallet_seat.rs`, the scanner in `wallet_scan.rs`.

use std::collections::BTreeSet;

use molt_core::wallet::{WalletPhase, WalletRefusal, WalletView};
use molt_core::{Command, MoltError, ProposalId, ProposalState, Reply, SessionScope, Surface};
use serde_json::{json, Value};

use crate::State;

/// The one door's op (W4).
pub(crate) const WALLET_INIT: &str = "wallet_init";
/// The purse record's op (design §3.5).
pub(crate) const WALLET_CREATED: &str = "wallet_created";
/// The proposer's birthday lies this many blocks below its daemon.
pub(crate) const BIRTHDAY_MARGIN: u64 = 10;
/// An approver takes a birthday at most this far above its own daemon:
/// daemons lag each other.
pub(crate) const BIRTHDAY_SLACK: u64 = 30;
/// ...and at most this far below it (two days of blocks).
pub(crate) const BIRTHDAY_WINDOW: u64 = 1440;
/// A daemon height older than this is asked again before a birthday is
/// judged against it.
const PROBE_FRESH_SECS: u64 = 120;

fn op(payload: &Value) -> Option<&str> {
    payload.get("op").and_then(Value::as_str)
}

pub(crate) fn is_init(payload: &Value) -> bool {
    op(payload) == Some(WALLET_INIT)
}

/// The closed op set every door enforces (§3.6), each op in its one shape.
pub(crate) fn purse_op_ok(payload: &Value) -> bool {
    parse_init(payload).is_some() || crate::wallet_run::parse_created(payload).is_some()
}

/// A well-formed init: `(birthday_height, network)`.
pub(crate) fn parse_init(payload: &Value) -> Option<(u64, String)> {
    if !is_init(payload) {
        return None;
    }
    let birthday = payload.get("birthday_height")?.as_u64()?;
    let network = payload.get("network")?.as_str()?;
    molt_core::wallet::WALLET_NETWORKS
        .contains(&network)
        .then(|| (birthday, network.to_string()))
}

/// Does `birthday` fit a seat whose daemon stands at `height`?
pub(crate) fn birthday_ok(birthday: u64, height: u64) -> bool {
    birthday <= height.saturating_add(BIRTHDAY_SLACK) && birthday.saturating_add(BIRTHDAY_WINDOW) >= height
}

/// The applied init the projection counts: the first well-formed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedInit {
    pub(crate) id: Option<u64>,
    pub(crate) birthday: u64,
    pub(crate) network: String,
}

/// The daemon side of the purse, in memory only.
#[derive(Debug, Default)]
pub(crate) struct PurseRt {
    /// The last probe's height and when it landed (unix seconds).
    pub(crate) height: Option<(u64, u64)>,
    /// The last probe's failure, one line.
    pub(crate) probe_error: String,
    /// The newest probe started; older results only refresh nothing.
    pub(crate) probe_gen: u64,
    pub(crate) probing: bool,
    /// The probe a `WalletInit` caller waits on.
    pub(crate) init_gen: Option<u64>,
    /// Init probes dropped by a close or a daemon change: answered refused.
    pub(crate) init_cancelled: BTreeSet<u64>,
    /// Init cards this seat approved before it could check them.
    pub(crate) consent: BTreeSet<u64>,
    /// The run, its records and attestations.
    pub(crate) run: crate::wallet_run::RunRt,
    /// The founding stage, the key part statuses, a view key without a part.
    pub(crate) seat: crate::wallet_seat::SeatRt,
    /// The scanner.
    pub(crate) scan: crate::wallet_scan::ScanRt,
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

    /// The first applied, well-formed `wallet_init` (§3.1); any later one
    /// and every other Wallet op are ignored.
    pub(crate) fn wallet_init_applied(&self) -> Option<AppliedInit> {
        self.chain.applied.get(&Surface::Wallet)?.iter().find_map(|(id, v)| {
            parse_init(v).map(|(birthday, network)| AppliedInit { id: *id, birthday, network })
        })
    }

    pub(crate) fn wallet_init_card_ids(&self) -> Vec<u64> {
        self.wallet_init_cards().into_iter().map(|(id, _)| id).collect()
    }

    /// Open init cards, oldest id first.
    fn wallet_init_cards(&self) -> Vec<(u64, Value)> {
        self.proposals
            .iter()
            .filter(|(_, p)| {
                p.surface == Surface::Wallet
                    && p.state == ProposalState::Proposed
                    && !p.withdrawn
                    && parse_init(&p.payload).is_some()
            })
            .map(|(id, p)| (*id, p.payload.clone()))
            .collect()
    }

    /// The nav shows the purse while an init is open (§3.1).
    pub(crate) fn wallet_init_pending(&self) -> bool {
        !self.wallet_init_cards().is_empty()
    }

    /// W1, chain, no applied init: the gates of the door.
    fn wallet_gates(&self) -> Result<(), WalletRefusal> {
        let (m, n) = self.wallet_rule().ok_or(WalletRefusal::NotChain)?;
        if !molt_core::wallet::bounds_ok(m, n) {
            return Err(WalletRefusal::Bounds);
        }
        if self.wallet_init_applied().is_some() {
            return Err(WalletRefusal::InitExists);
        }
        Ok(())
    }

    /// `set_features` may name `wallet` only where it is effective (W4).
    pub(crate) fn wallet_feature_refusal(&self, surface: Surface, payload: &Value) -> Option<MoltError> {
        if surface != Surface::Organization || op(payload) != Some("set_features") {
            return None;
        }
        let value = payload.get("value").and_then(Value::as_str).unwrap_or_default();
        let wallet = Surface::Wallet.as_str();
        (value.split_whitespace().any(|k| k == wallet) && !self.effective_features().iter().any(|f| f == wallet))
            .then(|| MoltError::BadPayload("wallet: set up the purse".to_string()))
    }

    /// The daemon height, if probed recently.
    fn fresh_height(&self) -> Option<u64> {
        let (h, at) = self.purse.height?;
        (crate::now_secs().saturating_sub(at) <= PROBE_FRESH_SECS).then_some(h)
    }

    /// The approve arm of a Wallet op, on every signing path (§3.6).
    pub(crate) fn wallet_approve_check(&self, payload: &Value) -> Result<(), WalletRefusal> {
        match op(payload) {
            Some(WALLET_INIT) => {
                let (birthday, network) = parse_init(payload).ok_or(WalletRefusal::UnknownOp)?;
                self.wallet_gates()?;
                // a daemonless seat's network is only the default: it abstains
                if self.session.settings.wallet_daemon_url.is_empty() {
                    return Err(WalletRefusal::NoDaemon);
                }
                // …and so does one whose daemon has not answered (plan §14 step 6)
                let Some(height) = self.fresh_height() else {
                    return Err(if self.purse.probing || self.purse.probe_error.is_empty() {
                        WalletRefusal::Checking
                    } else {
                        WalletRefusal::Daemon(self.purse.probe_error.clone())
                    });
                };
                if network != self.session.settings.wallet_network {
                    return Err(WalletRefusal::Network);
                }
                if !birthday_ok(birthday, height) {
                    return Err(WalletRefusal::Birthday);
                }
                Ok(())
            }
            Some(WALLET_CREATED) => self.wallet_created_check(payload),
            _ => Err(WalletRefusal::UnknownOp),
        }
    }

    /// The configured daemon under this node's policy.
    fn wallet_daemon(&self) -> Result<molt_net::monero_rpc::DaemonTransport, WalletRefusal> {
        let s = &self.session.settings;
        if s.wallet_daemon_url.is_empty() {
            return Err(WalletRefusal::NoDaemon);
        }
        let base = self.dialer_for().map_err(|e| WalletRefusal::Daemon(format!("daemon: {e}")))?;
        molt_net::monero_rpc::DaemonTransport::new(
            &s.wallet_daemon_url,
            &s.wallet_daemon_login,
            s.wallet_daemon_confirmed,
            self.clearnet_session,
            &base,
        )
        .map_err(|e| WalletRefusal::Daemon(e.to_string()))
    }

    /// Ask the daemon for its height off the actor; the answer lands as
    /// [`Command::NetWalletProbe`] on `reply` (the caller's, or nobody's).
    pub(crate) fn start_wallet_probe(
        &mut self,
        reply: Option<tokio::sync::oneshot::Sender<Result<Reply, MoltError>>>,
    ) -> Result<u64, WalletRefusal> {
        let transport = self.wallet_daemon();
        let cmd_tx = self.cmd_tx.upgrade().filter(|_| tokio::runtime::Handle::try_current().is_ok());
        let Some(cmd_tx) = cmd_tx else {
            let refusal = WalletRefusal::Daemon("daemon: engine stopped".to_string());
            if let Some(r) = reply {
                let _ = r.send(Err(MoltError::Wallet(refusal.clone())));
            }
            return Err(refusal);
        };
        self.purse.probe_gen += 1;
        let generation = self.purse.probe_gen;
        self.purse.probing = true;
        let reply = reply.unwrap_or_else(|| tokio::sync::oneshot::channel().0);
        let network = self.session.settings.wallet_network.clone();
        tokio::spawn(async move {
            let (height, error) = match transport {
                Ok(t) => match molt_net::monero_rpc::daemon_height(t, &network).await {
                    Ok(h) => (Some(h), String::new()),
                    Err(e) => (None, e.to_string()),
                },
                Err(r) => (None, r.to_string()),
            };
            let cmd = Command::NetWalletProbe { height, error, generation: Some(generation) };
            let _ = cmd_tx.send(crate::Envelope { cmd, reply }).await;
        });
        Ok(generation)
    }

    /// The Wallet surface's read model.
    pub(crate) fn wallet_view(&self) -> WalletView {
        let rule = self.wallet_rule();
        let (m, n) = rule.unwrap_or((0, 0));
        let purse = self.wallet_purse();
        let phase = match rule {
            None => WalletPhase::Off,
            Some((m, n)) if !molt_core::wallet::bounds_ok(m, n) => WalletPhase::Bounds,
            Some(_) if purse.is_some() => WalletPhase::Ready,
            Some(_) if self.wallet_init_applied().is_some() => WalletPhase::Init,
            Some(_) => WalletPhase::NoPurse,
        };
        let me = self.member();
        let own = self.wallet_own_status();
        let shareholders = match &purse {
            Some(_) => self
                .vault_founding_table()
                .into_iter()
                .map(|i| {
                    let status = if i.member == me {
                        own
                    } else {
                        self.purse.seat.statuses.get(&i.member).copied().unwrap_or_default()
                    };
                    (i.member, status)
                })
                .collect(),
            None => Vec::new(),
        };
        let scan = self.wallet_scan_shown();
        let watch = purse.is_some() && self.wallet_held_view().is_some();
        WalletView {
            address: purse.as_ref().map(|p| p.created.address.clone()).unwrap_or_default(),
            // Stage 1 cannot spend: the address never travels without this
            address_warning: purse
                .as_ref()
                .map(|_| molt_core::wallet::ADDRESS_WARNING.to_string())
                .unwrap_or_default(),
            balance: if watch { scan.balance } else { 0 },
            pending: if watch { scan.pending } else { 0 },
            scan_height: scan.scan_height,
            scan_paused: scan.paused,
            history: if watch { scan.history } else { Vec::new() },
            network: purse.as_ref().map_or_else(
                || self.session.settings.wallet_network.clone(),
                |p| crate::wallet_run::network_word(p.created.network).to_string(),
            ),
            run: self.wallet_run_view(),
            can_start: phase == WalletPhase::Init && self.purse.run.run.is_none() && self.purse.run.auto_from.is_none(),
            shareholders,
            daemon_height: scan.daemon_height.or(self.purse.height.map(|(h, _)| h)).unwrap_or(0),
            connected: scan.connected.unwrap_or(self.purse.height.is_some()),
            threshold: u32::from(m),
            participants: u32::from(n),
            phase,
            can_watch: watch,
            founding: self.purse.seat.founding,
            ..WalletView::default()
        }
    }

    /// [`molt_core::Command::WalletInit`]: the gates, then a daemon probe
    /// off the actor; [`Command::NetWalletProbe`] proposes and answers.
    pub(crate) fn cmd_wallet_init(&mut self) -> Result<Reply, MoltError> {
        self.wallet_init_ready().map_err(MoltError::Wallet)?;
        // taken LAST: every refusal above is answered by the actor loop
        let reply = self
            .deferred_reply
            .take()
            .ok_or_else(|| MoltError::Engine("no reply channel".to_string()))?;
        self.wallet_init_probe(Some(reply)).map_err(MoltError::Wallet)?;
        Ok(Reply::Ack)
    }

    fn wallet_init_ready(&self) -> Result<(), WalletRefusal> {
        self.wallet_gates()?;
        if self.wallet_init_pending() || self.purse.init_gen.is_some() {
            return Err(WalletRefusal::InitPending);
        }
        self.wallet_daemon().map(|_| ())
    }

    /// The gates, then the probe whose answer proposes the init.
    pub(crate) fn wallet_init_probe(
        &mut self,
        reply: Option<tokio::sync::oneshot::Sender<Result<Reply, MoltError>>>,
    ) -> Result<u64, WalletRefusal> {
        if reply.is_none() {
            self.wallet_init_ready()?;
        }
        let generation = self.start_wallet_probe(reply)?;
        self.purse.init_gen = Some(generation);
        Ok(generation)
    }

    /// [`Command::NetWalletProbe`]: a daemon height landed.
    pub(crate) fn cmd_net_wallet_probe(
        &mut self,
        height: Option<u64>,
        error: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        let Some(generation) = generation else {
            return Ok(Reply::Ack);
        };
        let latest = generation == self.purse.probe_gen;
        if latest {
            self.purse.probing = false;
            self.purse.height = height.map(|h| (h, crate::now_secs()));
            self.purse.probe_error = if height.is_some() { String::new() } else { error.clone() };
            match height {
                Some(h) => tracing::debug!(height = h, "wallet_probe=ok"),
                None => tracing::info!(error = %error, "wallet_probe=failed"),
            }
        }
        let out = if self.purse.init_cancelled.remove(&generation) {
            Err(MoltError::Wallet(WalletRefusal::Cancelled))
        } else if self.purse.init_gen == Some(generation) {
            self.purse.init_gen = None;
            self.propose_wallet_init(height, &error)
        } else {
            Ok(Reply::Ack)
        };
        if latest {
            self.wallet_review();
            self.wallet_advance_now();
            self.emit_session(SessionScope::Full);
        }
        out
    }

    fn propose_wallet_init(&mut self, height: Option<u64>, error: &str) -> Result<Reply, MoltError> {
        self.wallet_gates().map_err(MoltError::Wallet)?;
        if self.wallet_init_pending() {
            return Err(MoltError::Wallet(WalletRefusal::InitPending));
        }
        let height = height.ok_or_else(|| MoltError::Wallet(WalletRefusal::Daemon(error.to_string())))?;
        let payload = json!({
            "op": WALLET_INIT,
            "birthday_height": height.saturating_sub(BIRTHDAY_MARGIN),
            "network": self.session.settings.wallet_network,
        });
        self.propose_payload(Surface::Wallet, payload)
    }

    /// Approve or decline the open init cards this seat can now judge:
    /// out of the window or on another network declines (the card dies,
    /// §3.1), a consent that waited on the daemon signs, no daemon abstains.
    pub(crate) fn wallet_review(&mut self) {
        self.wallet_founding_claim();
        let me = self.member();
        for (id, payload) in self.wallet_init_cards() {
            let approved = self.chain.own_approvals.contains(&id);
            let declined = self.proposals.get(&id).is_some_and(|p| p.decliners.contains(&me));
            let decided = approved || declined;
            let consent = self.purse.consent.contains(&id) || self.wallet_founding_consents(id);
            match self.wallet_approve_check(&payload) {
                Ok(()) if !decided && consent => {
                    tracing::info!(id, "wallet_init=approved");
                    self.chain_sign_and_gossip_approval(id);
                    self.purse.consent.remove(&id);
                }
                // a re-sign that waited on a stale height
                Ok(()) if approved && !self.own_signature_stands(id) => {
                    self.chain_sign_and_gossip_approval(id);
                }
                // an approved card that aged out could never re-sign: it dies
                Err(r @ (WalletRefusal::Birthday | WalletRefusal::Network | WalletRefusal::Bounds)) if !declined => {
                    tracing::warn!(id, reason = %r, "wallet_init=declined");
                    self.purse.consent.remove(&id);
                    if let Err(e) = self.cmd_decline(ProposalId(id), None) {
                        tracing::warn!(id, error = %e, "wallet_init decline failed");
                    }
                }
                Err(WalletRefusal::Checking) if !self.purse.probing => {
                    let _ = self.start_wallet_probe(None);
                }
                _ => {}
            }
        }
    }

    /// A peer's init card landed: judge it, asking the daemon first if the
    /// height is stale.
    pub(crate) fn after_wallet_proposed(&mut self, id: u64) {
        self.wallet_supersede_dead();
        if !self.proposals.get(&id).is_some_and(|p| p.state == ProposalState::Proposed && is_init(&p.payload)) {
            return;
        }
        self.wallet_review();
    }

    /// The daemon settings moved: forget the old height, ask the new one.
    pub(crate) fn wallet_daemon_changed(&mut self) {
        self.cancel_wallet_init();
        self.purse.probe_gen += 1;
        self.purse.probing = false;
        self.purse.height = None;
        self.purse.probe_error.clear();
        if self.wallet_init_pending() {
            self.wallet_review();
        }
    }

    /// The waiting `WalletInit` will not propose: its probe answers refused.
    pub(crate) fn cancel_wallet_init(&mut self) {
        if let Some(g) = self.purse.init_gen.take() {
            self.purse.init_cancelled.insert(g);
        }
    }

    /// The presence tick: a stale or failed height under an open card is
    /// asked again.
    pub(crate) fn wallet_tick(&mut self) {
        if self.wallet_init_pending()
            && self.wallet_gates().is_ok()
            && self.fresh_height().is_none()
            && !self.purse.probing
            && !self.session.settings.wallet_daemon_url.is_empty()
        {
            let _ = self.start_wallet_probe(None);
        }
    }

    /// `approve` on an init this seat cannot check yet is kept as consent:
    /// the seat signs once its daemon answers (§3.1). The answer says so.
    pub(crate) fn wallet_consent_waits(&mut self, id: u64, refusal: WalletRefusal) -> WalletRefusal {
        if !matches!(refusal, WalletRefusal::NoDaemon | WalletRefusal::Checking | WalletRefusal::Daemon(_)) {
            return refusal;
        }
        self.purse.consent.insert(id);
        if matches!(refusal, WalletRefusal::Checking | WalletRefusal::Daemon(_)) && !self.purse.probing {
            let _ = self.start_wallet_probe(None);
        }
        WalletRefusal::Held(Box::new(refusal))
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
        let refusal = match active.handle.set_aside_wallet_keys_blocking() {
            Ok(true) => None,
            Ok(false) => Some(WalletRefusal::NoKeysFile),
            Err(molt_storage::StorageError::WalletKeysIntact) => Some(WalletRefusal::KeysIntact),
            Err(e) => return Err(MoltError::Storage(e.to_string())),
        };
        match refusal {
            None => {
                // a file damaged after open: the parts still in memory go back to disk
                let held: Vec<_> = self.purse.run.records.iter().map(molt_treasury::keys::KeysRecord::encode).collect();
                let kept = !held.is_empty()
                    && held.into_iter().all(|r| active.handle.persist_wallet_keys_blocking(r));
                if kept {
                    tracing::warn!(id, "wallet_keys=set_aside seat=rewritten");
                } else {
                    tracing::warn!(id, "wallet_keys=set_aside seat=watch_only");
                    self.purse.run.records.clear();
                    if self.wallet_purse().is_some() {
                        self.wallet_view_only();
                    }
                }
                self.lift_keys_hold(&id);
            }
            // the damage is gone already: only a keys failure holds the ticker
            Some(r) => {
                if self.backup_hold.contains_key(&id) {
                    self.lift_keys_hold(&id);
                }
                return Err(MoltError::Wallet(r));
            }
        }
        Ok(Reply::Ack)
    }

    fn lift_keys_hold(&mut self, id: &str) {
        self.backup_hold.remove(id);
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.backup_error.clear();
        }
        self.emit_session(SessionScope::Full);
    }
}

#[cfg(test)]
#[path = "wallet_tests.rs"]
mod tests;
