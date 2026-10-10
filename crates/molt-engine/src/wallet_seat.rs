// SPDX-License-Identifier: GPL-3.0-or-later

//! This seat's standing in the purse (`docs_archive/chain/wallet_treasury_design.md`
//! §3.2, §5, plan §7.3, §7.8): the purse stage after a founding with
//! `wallet` in its charter, the key part status frames, and the view key's
//! ask and answer for a seat that holds no key part.

use std::collections::{BTreeMap, BTreeSet};

use molt_core::wallet::ShareStatus;
use molt_core::{MemberId, MoltError, Reply, SessionScope, Surface};
use molt_net::wallet_frames::{WalletFrame, WalletStatusFrame, WalletViewAskFrame, WalletViewRespFrame, WALLET_V};
use molt_treasury::keys;
use zeroize::Zeroizing;

use crate::State;

/// Own status goes out again this often.
pub(crate) const STATUS_RESEND_SECS: u64 = 30;
/// A seat without the view key asks again this often.
pub(crate) const ASK_SECS: u64 = 15;
/// One answer per asker in this window.
pub(crate) const ANSWER_SECS: u64 = 10;

/// In memory; `statuses` and `view` mirror `TransportState`.
#[derive(Default)]
pub(crate) struct SeatRt {
    /// This session founded the republic with `wallet` ratified: the
    /// ratification is this seat's consent (§3.2), never persisted.
    pub(crate) founding: bool,
    /// The auto-init is armed since then, until an init is visible.
    armed_at: Option<u64>,
    /// The inits visible when arming ended: the founding consented to these only.
    founding_inits: BTreeSet<u64>,
    pub(crate) tried_at: u64,
    /// The others' key part status, last frame wins.
    pub(crate) statuses: BTreeMap<MemberId, ShareStatus>,
    /// The view key, held without a key part.
    view: Option<Zeroizing<[u8; 32]>>,
    pub(crate) status_sent: Option<(ShareStatus, u64)>,
    asked_at: u64,
    answered: BTreeMap<MemberId, u64>,
}

impl std::fmt::Debug for SeatRt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatRt")
            .field("founding", &self.founding)
            .field("statuses", &self.statuses)
            .field("view", &self.view.is_some())
            .finish_non_exhaustive()
    }
}

impl State {
    /// A create or join finished (never a recovery, never a reopen): with
    /// `wallet` ratified and W1 holding, the purse stage follows.
    pub(crate) fn wallet_arm_founding(&mut self, features: Option<&[String]>) {
        let wallet = features.is_some_and(|f| f.iter().any(|x| x == Surface::Wallet.as_str()));
        let bounds = self.wallet_rule().is_some_and(|(m, n)| molt_core::wallet::bounds_ok(m, n));
        if wallet && bounds {
            self.purse.seat.founding = true;
            self.purse.seat.armed_at = Some(self.presence_now());
            tracing::info!("wallet_stage=armed");
            self.wallet_founding_claim();
        }
    }

    /// The first inits seen after arming are the founding's; arming ends.
    pub(crate) fn wallet_founding_claim(&mut self) {
        if self.purse.seat.armed_at.is_none() {
            return;
        }
        let applied = self.wallet_init_applied();
        let mut ids: BTreeSet<u64> = self.wallet_init_card_ids().into_iter().collect();
        ids.extend(applied.as_ref().and_then(|i| i.id));
        if applied.is_some() || !ids.is_empty() {
            self.purse.seat.armed_at = None;
            self.purse.seat.founding_inits = ids;
        }
    }

    /// The ratification consents to the founding's init, unless this seat declined its card.
    pub(crate) fn wallet_founding_consents(&self, init: u64) -> bool {
        let me = self.member();
        self.purse.seat.founding_inits.contains(&init) && !self.wallet_declined(init, &me)
    }

    pub(crate) fn wallet_declined(&self, init: u64, me: &MemberId) -> bool {
        self.proposals.get(&init).is_some_and(|p| p.decliners.contains(me))
    }

    /// Load what a reopen keeps.
    pub(crate) fn wallet_load_seat(&mut self, ts: &molt_core::TransportState) {
        self.purse.seat.statuses = ts.wallet_status.clone();
        self.purse.seat.view = ts.wallet_view.as_ref().and_then(|v| <[u8; 32]>::try_from(v.0.as_slice()).ok()).map(Zeroizing::new);
    }

    fn wallet_persist_seat(&self) {
        if let Some(active) = self.active.as_ref() {
            let view = self.purse.seat.view.as_ref().map(|v| molt_core::vault::SecretBytes(v.to_vec()));
            active.handle.save_wallet_seat(self.purse.seat.statuses.clone(), view);
        }
    }

    /// The view key this seat holds for the purse: its record's, or one
    /// it was handed that opens the address.
    pub(crate) fn wallet_held_view(&self) -> Option<Zeroizing<[u8; 32]>> {
        let p = self.wallet_purse()?;
        if let Some(k) = self.wallet_purse_record(&p.created) {
            return Some(self.purse.run.records[k].view.clone());
        }
        self.purse
            .seat
            .view
            .clone()
            .filter(|v| keys::view_matches(&p.created.address, p.created.network, v))
    }

    /// This seat's key part: held, or view only; unknown without a purse.
    pub(crate) fn wallet_own_status(&self) -> ShareStatus {
        match self.wallet_purse() {
            Some(p) if self.wallet_purse_record(&p.created).is_some() => ShareStatus::Held,
            Some(_) => ShareStatus::WatchOnly,
            None => ShareStatus::Unknown,
        }
    }

    /// The beat: the founding's auto-init, own status, the view ask.
    pub(crate) fn wallet_seat_tick(&mut self, now: u64) {
        self.wallet_founding_tick(now);
        let Some(purse) = self.wallet_purse() else {
            return;
        };
        // a purse adopted without its block's hook (a recovery's chain)
        if self.purse.run.settled != Some(purse.id) {
            self.wallet_on_commit();
        }
        let init = purse.created.init;
        let status = self.wallet_own_status();
        let due = self
            .purse
            .seat
            .status_sent
            .map_or(true, |(s, at)| s != status || now.saturating_sub(at) >= STATUS_RESEND_SECS);
        if due {
            self.purse.seat.status_sent = Some((status, now));
            let held = status == ShareStatus::Held;
            self.wallet_send(&WalletFrame::Status(WalletStatusFrame { v: WALLET_V, init, held }));
        }
        if self.wallet_held_view().is_none() && now.saturating_sub(self.purse.seat.asked_at) >= ASK_SECS {
            self.purse.seat.asked_at = now;
            tracing::info!(init, "wallet_view=asked");
            self.wallet_send(&WalletFrame::ViewAsk(WalletViewAskFrame { v: WALLET_V, init }));
        }
    }

    /// The lowest position with a daemon proposes the init; the next one
    /// after a step if none is visible (§3.2).
    fn wallet_founding_tick(&mut self, now: u64) {
        self.wallet_founding_claim();
        let Some(at) = self.purse.seat.armed_at else {
            return;
        };
        if self.purse.init_gen.is_some() || self.session.settings.wallet_daemon_url.is_empty() {
            return;
        }
        let Some(me) = self.wallet_pos(&self.member()) else {
            return;
        };
        let step = self.wallet_step();
        let wait = u64::from(me - 1).saturating_mul(step);
        if now.saturating_sub(at) < wait || now.saturating_sub(self.purse.seat.tried_at) < step {
            return;
        }
        self.purse.seat.tried_at = now;
        if let Err(e) = self.wallet_auto_init() {
            tracing::info!(error = %e, "wallet_init=not_proposed");
        }
    }

    /// [`State::cmd_wallet_init`] without a caller: the probe proposes.
    fn wallet_auto_init(&mut self) -> Result<(), molt_core::wallet::WalletRefusal> {
        let generation = self.wallet_init_probe(None)?;
        tracing::info!(generation, "wallet_init=auto");
        Ok(())
    }

    /// [`molt_core::Command::NetWalletStatus`]: a seat's own word on its key part.
    pub(crate) fn cmd_net_wallet_status(&mut self, from: &MemberId, status: ShareStatus, init: u64) -> Result<Reply, MoltError> {
        let peer = *from != self.member() && self.wallet_pos(from).is_some();
        let ours = self.wallet_purse().is_some_and(|p| p.created.init == init);
        if !peer || status == ShareStatus::Unknown || !ours {
            return Ok(Reply::Ack);
        }
        if self.purse.seat.statuses.insert(from.clone(), status) != Some(status) {
            tracing::info!(seat = %from, status = ?status, "wallet_status=changed");
            self.wallet_persist_seat();
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// A seat asks for the view key: answered over the group by any holder.
    pub(crate) fn wallet_on_view_ask(&mut self, from: &MemberId, init: u64) {
        let peer = *from != self.member() && self.wallet_pos(from).is_some();
        if !peer || self.wallet_purse().map_or(true, |p| p.created.init != init) {
            return;
        }
        let Some(view) = self.wallet_held_view() else {
            return;
        };
        let now = self.presence_now();
        if self.purse.seat.answered.get(from).is_some_and(|at| now.saturating_sub(*at) < ANSWER_SECS) {
            return;
        }
        self.purse.seat.answered.insert(from.clone(), now);
        tracing::info!(seat = %from, "wallet_view=answered");
        let view = molt_core::vault::SecretHex(hex::encode(*view));
        self.wallet_send(&WalletFrame::ViewResp(WalletViewRespFrame { v: WALLET_V, init, view }));
    }

    /// [`molt_core::Command::NetWalletViewAnswer`]: kept only when it
    /// opens the purse's address (`view·G`), never on the chain or the log.
    pub(crate) fn cmd_net_wallet_view_answer(&mut self, from: &MemberId, view: &[u8]) -> Result<Reply, MoltError> {
        let peer = *from != self.member() && self.wallet_pos(from).is_some();
        let Some(purse) = self.wallet_purse().filter(|_| peer) else {
            return Ok(Reply::Ack);
        };
        if self.wallet_held_view().is_some() {
            return Ok(Reply::Ack);
        }
        let Ok(view) = <[u8; 32]>::try_from(view).map(Zeroizing::new) else {
            return Ok(Reply::Ack);
        };
        if !keys::view_matches(&purse.created.address, purse.created.network, &view) {
            tracing::warn!(seat = %from, "wallet_view=mismatch");
            return Ok(Reply::Ack);
        }
        self.purse.seat.view = Some(view);
        tracing::info!(seat = %from, "wallet_view=received");
        self.wallet_persist_seat();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }
}
