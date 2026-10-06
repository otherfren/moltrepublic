// SPDX-License-Identifier: GPL-3.0-or-later

//! Receipts, the card counts and the re-seal (plan stage S3c, spec §7).
//!
//! A holder checks its share and the held payload once both are here and
//! reports `verified` or `complaint`; every member keeps the reports
//! last-wins per holder in `TransportState.vault_status`. They are status,
//! never consensus: the chain does not see them and a restored member
//! re-collects them from the re-sends on start.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use molt_core::vault::{
    secret_id, SecretHex, SecretText, VaultComplaintStatus, VaultComplaintView, VaultDeposit,
    VaultDepositState, VaultOp, VaultReceipt, VaultRefusal, VaultRevealOutcome, VaultStatusStore,
    VaultView,
};
use molt_core::{Command, MemberId, MoltError, ProposalState, Reply, SessionScope, Surface};
use molt_net::supervisor::StateStore as _;
use molt_net::vault_frames::{VaultFrame, VaultReceiptFrame, VaultVerdict, VAULT_V};

#[path = "complaint.rs"]
pub(crate) mod complaint;

/// The lab switch (feature `vault-lab`).
pub(crate) const LAB_ENV: &str = "MOLT_VAULT_LAB_COMPLAIN";

/// Own receipts and reveals go out again this often (and at every start).
const RESEND_SECS: u64 = 60 * 60;

/// A receipt for a deposit not seen here yet is kept only while the store
/// names fewer versions than this.
const UNKNOWN_MAX: usize = 256;

/// Whether the lab switch is on for this env value: only with the
/// feature compiled in.
pub(crate) fn lab_complain_from(value: Option<&str>) -> bool {
    cfg!(feature = "vault-lab") && value == Some("1")
}

/// Read once at spawn.
pub(crate) fn lab_complain_at_spawn() -> bool {
    lab_complain_from(std::env::var(LAB_ENV).ok().as_deref())
}

/// The handle-reachable dishonesty seams (plan S3c step 6, step 7).
#[derive(Default)]
pub(crate) struct LabSeams {
    /// This holder's receipt reports `complaint` whatever its check says.
    pub(crate) complain: AtomicBool,
    /// This depositor's reveals carry a wrong share.
    pub(crate) lie_on_reveal: AtomicBool,
    /// `(name, kind, text, holder)` to seal with a bad share for `holder`.
    bad_share: Mutex<Vec<(String, String, SecretText, MemberId)>>,
}

impl LabSeams {
    pub(crate) fn stage_bad_share(&self, name: &str, kind: &str, text: &str, holder: &str) {
        if let Ok(mut b) = self.bad_share.lock() {
            b.push((name.to_string(), kind.to_string(), SecretText(text.to_string()), holder.to_string()));
        }
    }

    fn take_bad_share(&self) -> Vec<(String, String, SecretText, MemberId)> {
        self.bad_share.lock().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default()
    }
}

/// The receipt plane of the open workspace. Holds shares (buffered
/// reveals, the seam's bad deal): no `Debug`.
#[derive(Default)]
pub(crate) struct ReceiptRuntime {
    /// Receipts and decided reveals (`TransportState.vault_status`).
    pub(crate) status: VaultStatusStore,
    /// This seat's own check per `secret_id`, this incarnation.
    pub(crate) checked: BTreeMap<String, bool>,
    /// When own receipts and reveals were last re-sent (0 = next tick).
    pub(crate) resent_at: u64,
    /// Revealed shares on the polynomial although the reveal lied.
    pub(crate) leaked: BTreeSet<(String, MemberId)>,
    /// The anchor height the store was last pruned at.
    pub(crate) pruned_at: Option<u64>,
    /// Reveals for a deposit not seen here yet.
    pub(crate) early: complaint::EarlyReveals,
    /// The seam's bad shares, by `(secret_id, holder)`.
    pub(crate) bad_dealt: BTreeMap<(String, MemberId), zeroize::Zeroizing<[u8; 32]>>,
    /// Persist order: the newest snapshot wins the store.
    persist_seq: u64,
    persisted: Arc<AtomicU64>,
}

impl ReceiptRuntime {
    /// The runtime of a workspace just opened.
    pub(crate) fn loaded(status: VaultStatusStore) -> Self {
        Self { status, ..Self::default() }
    }
}

/// Every deposit known here: each committed version above the anchor and
/// each open deposit card, by `secret_id`.
pub(crate) fn known_deposits(st: &crate::State) -> BTreeMap<String, VaultDeposit> {
    let rid = st.republic_id();
    let mut out: BTreeMap<String, VaultDeposit> =
        st.vault_state().versions.into_iter().map(|(sid, v)| (sid, v.deposit)).collect();
    for p in st.proposals.values() {
        if p.surface != Surface::Vault || p.state != ProposalState::Proposed || p.withdrawn {
            continue;
        }
        if let Ok(VaultOp::Deposit(dep)) = serde_json::from_value::<VaultOp>(p.payload.clone()) {
            out.entry(secret_id(&rid, &dep)).or_insert(dep);
        }
    }
    out
}

pub(crate) fn known_deposit(st: &crate::State, sid: &str) -> Option<VaultDeposit> {
    known_deposits(st).remove(sid)
}

/// Publish one vault frame on the group.
pub(crate) fn publish(st: &crate::State, frame: &VaultFrame) -> bool {
    st.group_net.as_ref().is_some_and(|g| g.handle.publish_control(frame.to_frame()))
}

/// Write the store back off the actor; a later snapshot is never
/// overwritten by an earlier one.
pub(crate) fn persist(st: &mut crate::State) {
    let Some(store) = st.file_store() else {
        return;
    };
    st.vault_rx.persist_seq += 1;
    let seq = st.vault_rx.persist_seq;
    let persisted = st.vault_rx.persisted.clone();
    let status = st.vault_rx.status.clone();
    tokio::spawn(async move {
        store
            .update(move |s| {
                if persisted.load(Ordering::SeqCst) > seq {
                    return false;
                }
                persisted.store(seq, Ordering::SeqCst);
                s.vault_status = status;
                true
            })
            .await;
    });
}

fn send_receipt(st: &crate::State, sid: &str, verified: bool, rev: u64) -> bool {
    let frame = VaultFrame::Receipt(VaultReceiptFrame {
        v: VAULT_V,
        by: st.member(),
        secret_id: sid.to_string(),
        verdict: if verified { VaultVerdict::Verified } else { VaultVerdict::Complaint },
        rev,
    });
    publish(st, &frame)
}

/// Record and send this holder's own verdict; the revision moves only
/// when the verdict does.
fn record_own(st: &mut crate::State, sid: &str, verified: bool) {
    let me = st.member();
    let now = crate::now_secs();
    let slot = st.vault_rx.status.receipts.entry(sid.to_string()).or_default();
    let rev = match slot.get(&me) {
        Some(r) if r.verified == verified => r.rev,
        Some(r) => now.max(r.rev.saturating_add(1)),
        None => now.max(1),
    };
    slot.insert(me, VaultReceipt { verified, rev });
    persist(st);
    send_receipt(st, sid, verified, rev);
    tracing::info!(secret_id = %sid, verified, "vault: receipt");
    st.emit_session(SessionScope::Full);
}

/// Check every deposit this holder has not checked yet whose payload is
/// here (spec §7 (a) and (b)); a seat without its seed stays silent.
fn sweep(st: &mut crate::State) {
    let Some(ctx) = st.vault_ctx() else {
        return;
    };
    let Some(seed) = st.vault_seed.clone() else {
        return;
    };
    let me = st.member();
    let rid = st.republic_id();
    let (sk, _) = molt_vault::vault_keypair(&seed);
    let complain = st.vault_seams.lab.complain.load(Ordering::SeqCst);
    for (sid, dep) in known_deposits(st) {
        if dep.depositor == me
            || !dep.holders.contains(&me)
            || st.vault_rx.checked.contains_key(&sid)
            || !st.vault_payload_held(&sid)
        {
            continue;
        }
        let ok = molt_vault::check_my_share(&dep, &rid, &ctx, &me, &sk).is_ok();
        st.vault_rx.checked.insert(sid.clone(), ok);
        if !ok {
            tracing::warn!(secret_id = %sid, depositor = %dep.depositor, "vault: own share fails");
        }
        record_own(st, &sid, ok && !complain);
    }
}

/// A deposit (pending or committed) appeared: check, send the receipt,
/// decide the reveals that came first.
pub(crate) fn on_deposit(st: &mut crate::State, secret_id: &str) {
    sweep(st);
    complaint::replay_early(st, secret_id);
}

/// A payload became held here: check what it completes.
pub(crate) fn on_payload_held(st: &mut crate::State) {
    sweep(st);
}

/// The presence beat: check, prune after a cut, and re-send own receipts
/// and reveals at start and every [`RESEND_SECS`].
pub(crate) fn tick(st: &mut crate::State, now: u64) {
    if !st.is_vault_republic() {
        return;
    }
    sweep(st);
    let anchor = st.chain.checkpoint_blob.as_ref().map(|b| b.upto);
    if anchor.is_some() && anchor != st.vault_rx.pruned_at && prune_at_cut(st) {
        st.vault_rx.pruned_at = anchor;
    }
    if st.group_net.is_none()
        || (st.vault_rx.resent_at != 0 && now.saturating_sub(st.vault_rx.resent_at) < RESEND_SECS)
    {
        return;
    }
    let me = st.member();
    let known = known_deposits(st);
    let mut sent = true;
    for (sid, by) in &st.vault_rx.status.receipts {
        if let (Some(r), true) = (by.get(&me), known.contains_key(sid)) {
            sent &= send_receipt(st, sid, r.verified, r.rev);
        }
    }
    sent &= complaint::resend_reveals(st, &known);
    if sent {
        st.vault_rx.resent_at = now.max(1);
    }
}

/// The payload beat: staged seam seals.
pub(crate) fn seam_tick(st: &mut crate::State) {
    for (name, kind, text, holder) in st.vault_seams.lab.take_bad_share() {
        if let Err(e) = complaint::seal_with_bad_share(st, &name, &kind, &text, &holder) {
            tracing::warn!(error = %e, "vault: seam seal refused");
        }
    }
}

/// At a cut: drop the entries of versions no longer current or pending
/// (plan 1.3.15: never while base-pending). Returns whether it ran.
pub(crate) fn prune_at_cut(st: &mut crate::State) -> bool {
    if st.vault_base_pending() {
        return false;
    }
    let vs = st.vault_state();
    let keep: BTreeSet<String> = known_deposits(st)
        .into_keys()
        .filter(|sid| vs.is_current(sid) || !vs.versions.contains_key(sid))
        .collect();
    let rx = &mut st.vault_rx;
    let before = (rx.status.receipts.len(), rx.status.reveals.len());
    rx.status.receipts.retain(|sid, _| keep.contains(sid));
    rx.status.reveals.retain(|sid, _| keep.contains(sid));
    rx.leaked.retain(|(sid, _)| keep.contains(sid));
    rx.checked.retain(|sid, _| keep.contains(sid));
    if before != (rx.status.receipts.len(), rx.status.reveals.len()) {
        persist(st);
    }
    true
}

/// The depositor re-opens its own payload for a re-seal.
pub(crate) fn reseal_text(
    dep: &VaultDeposit,
    republic_id: &str,
    seed: &[u8; 32],
    file: &[u8],
) -> Result<SecretText, MoltError> {
    let s = molt_vault::recover_secret(dep, republic_id, seed)
        .map_err(|_| MoltError::Vault(VaultRefusal::NotVerified))?;
    molt_vault::open_payload(dep, republic_id, &s, file).map_err(|_| MoltError::Vault(VaultRefusal::NotVerified))
}

impl crate::State {
    /// D15: the depositor re-seals a current version under the same name
    /// and kind - a fresh nonce, so a fresh secret and fresh shares (plan
    /// 1.3.3) - proposed like any deposit. The held payload is read off
    /// the actor; the seal answers the caller.
    pub(crate) fn cmd_vault_reseal(&mut self, secret_id: String) -> Result<Reply, MoltError> {
        if !self.is_vault_republic() {
            return Err(MoltError::Vault(VaultRefusal::NoVault));
        }
        let vs = self.vault_state();
        let me = self.member();
        let dep = vs
            .versions
            .get(&secret_id)
            .filter(|v| v.deposit.depositor == me && vs.is_current(&secret_id))
            .map(|v| v.deposit.clone())
            .ok_or_else(|| MoltError::BadPayload("no such deposit".to_string()))?;
        let seed = self.vault_seed.clone().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?;
        if !self.vault_payload_held(&secret_id) {
            return Err(MoltError::Vault(VaultRefusal::PayloadNotHeld));
        }
        let rid = self.republic_id();
        molt_vault::recover_secret(&dep, &rid, &seed).map_err(|_| MoltError::Vault(VaultRefusal::NotVerified))?;
        let storage = self.active.as_ref().ok_or(MoltError::Vault(VaultRefusal::PayloadNotHeld))?.handle.clone();
        let cmd_tx = self.cmd_tx.upgrade().ok_or_else(|| MoltError::Engine("engine stopped".to_string()))?;
        // taken LAST: every refusal above is answered by the actor loop
        let reply = self.deferred_reply.take().ok_or_else(|| MoltError::Engine("no reply channel".to_string()))?;
        tracing::info!(secret_id = %secret_id, "vault: re-seal");
        tokio::spawn(async move {
            let text = match storage.load_vault_payload(&secret_id).await {
                Some(file) => reseal_text(&dep, &rid, &seed, &file),
                None => Err(MoltError::Vault(VaultRefusal::PayloadNotHeld)),
            };
            match text {
                Ok(text) => {
                    let cmd = Command::VaultSeal { name: dep.name, kind: dep.kind, text };
                    let _ = cmd_tx.send(crate::Envelope { cmd, reply }).await;
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        });
        Ok(Reply::Ack)
    }

    /// A holder's authenticated receipt: last wins by revision, a non-holder
    /// is dropped; a complaint on one of this seat's deposits is answered
    /// with a reveal.
    pub(crate) fn cmd_net_vault_receipt(
        &mut self,
        from: &MemberId,
        secret_id: String,
        verdict: String,
        rev: u64,
    ) -> Result<Reply, MoltError> {
        let Some(ctx) = self.vault_ctx() else {
            return Ok(Reply::Ack);
        };
        let verified = match verdict.as_str() {
            "verified" => true,
            "complaint" => false,
            _ => return Ok(Reply::Ack),
        };
        if *from == self.member() || !ctx.holders_in_genesis_order.iter().any(|(n, _, _)| n == from) {
            return Ok(Reply::Ack);
        }
        let dep = known_deposit(self, &secret_id);
        match &dep {
            Some(d) if !d.holders.contains(from) => {
                tracing::debug!(secret_id = %secret_id, from = %from, "vault: receipt from a non-holder");
                return Ok(Reply::Ack);
            }
            None if !self.vault_rx.status.receipts.contains_key(&secret_id)
                && self.vault_rx.status.receipts.len() >= UNKNOWN_MAX =>
            {
                return Ok(Reply::Ack);
            }
            _ => {}
        }
        let slot = self.vault_rx.status.receipts.entry(secret_id.clone()).or_default();
        if slot.get(from).is_some_and(|r| r.rev >= rev) {
            return Ok(Reply::Ack);
        }
        slot.insert(from.clone(), VaultReceipt { verified, rev });
        persist(self);
        tracing::info!(secret_id = %secret_id, from = %from, verified, "vault: receipt landed");
        if let Some(dep) = dep.filter(|d| !verified && d.depositor == self.member()) {
            complaint::answer(self, &dep, &secret_id, from);
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_reveal(
        &mut self,
        from: &MemberId,
        secret_id: String,
        holder: MemberId,
        share: SecretHex,
        ikm: SecretHex,
    ) -> Result<Reply, MoltError> {
        let (Some(share), Some(ikm)) = (hex32(&share.0), hex32(&ikm.0)) else {
            return Ok(Reply::Ack);
        };
        complaint::take_reveal(self, from, &secret_id, &holder, share, ikm);
        Ok(Reply::Ack)
    }
}

fn hex32(s: &str) -> Option<zeroize::Zeroizing<[u8; 32]>> {
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(s, out.as_mut()).ok()?;
    Some(out)
}

/// Receipt counts, complaint lines and the re-seal offer (spec §7 table,
/// plan 1.3.18).
pub(crate) fn fill(st: &crate::State, view: &mut VaultView) {
    let Some(ctx) = st.vault_ctx() else {
        return;
    };
    let rx = &st.vault_rx;
    let replacing: BTreeSet<String> = view
        .deposits
        .iter()
        .filter(|d| d.proposal.is_some())
        .filter_map(|d| d.replaces.clone())
        .collect();
    for card in &mut view.deposits {
        let holders: Vec<&String> = ctx
            .holders_in_genesis_order
            .iter()
            .map(|(n, _, _)| n)
            .filter(|n| **n != card.depositor)
            .collect();
        let receipts = rx.status.receipts.get(&card.secret_id);
        let reveals = rx.status.reveals.get(&card.secret_id);
        let mut verified = 0usize;
        let mut valid = 0usize;
        let mut lines = Vec::new();
        for h in &holders {
            let receipt = receipts.and_then(|r| r.get(*h));
            let outcome = reveals.and_then(|r| r.get(*h)).copied();
            if receipt.is_some_and(|r| r.verified) {
                verified += 1;
            }
            if outcome == Some(VaultRevealOutcome::FalseComplaint)
                || rx.leaked.contains(&(card.secret_id.clone(), (*h).clone()))
            {
                valid += 1;
            }
            let status = match outcome {
                Some(VaultRevealOutcome::Lie) => VaultComplaintStatus::Lie,
                Some(VaultRevealOutcome::BadShare) => VaultComplaintStatus::BadShare,
                Some(VaultRevealOutcome::FalseComplaint) => VaultComplaintStatus::False,
                None if receipt.is_some_and(|r| !r.verified) => VaultComplaintStatus::Open,
                None => continue,
            };
            lines.push(VaultComplaintView { holder: (*h).clone(), status });
        }
        card.verified = u8::try_from(verified).unwrap_or(u8::MAX);
        card.readable_by = ctx.m.saturating_sub(u8::try_from(valid).unwrap_or(u8::MAX));
        card.complaints = lines;
        if card.state == VaultDepositState::Committed {
            if !holders.is_empty() && verified >= holders.len() {
                card.state = VaultDepositState::Hardened;
            } else if verified >= usize::from(ctx.m) {
                card.state = VaultDepositState::Sealed;
            }
        }
        card.reseal = card.mine && card.proposal.is_none() && valid > 0 && !replacing.contains(&card.secret_id);
    }
}

#[cfg(test)]
#[path = "receipts_tests.rs"]
mod tests;
