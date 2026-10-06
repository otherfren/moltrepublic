// SPDX-License-Identifier: GPL-3.0-or-later

//! Complaint reveals (plan stage S3c, spec §7): the depositor answers a
//! complaint with the holder's share and ephemeral, and every member
//! decides it against the record and the holder's FOUNDING vault key.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use molt_core::vault::{
    deposit_signing_bytes, secret_id, share_aad, SecretHex, SecretText, VaultDeposit, VaultRefusal,
    VaultRevealOutcome,
};
use molt_core::{MemberId, MoltError, Reply, SessionScope};
use molt_net::vault_frames::{VaultFrame, VaultRevealFrame, VAULT_V};
use zeroize::Zeroizing;

use super::{known_deposit, persist, publish};

/// Reveals buffered for deposits not seen here yet, at most.
const EARLY_MAX: usize = 64;

/// One reveal waiting for its deposit: `(from, holder, share, ikm)`.
type Early = (MemberId, MemberId, Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>);

/// A revealed share and its ephemeral ikm.
type Revealed = (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>);

/// Reveals that arrived before their deposit, by `secret_id`.
#[derive(Default)]
pub(crate) struct EarlyReveals(BTreeMap<String, Vec<Early>>);

impl EarlyReveals {
    fn len(&self) -> usize {
        self.0.values().map(Vec::len).sum()
    }
}

/// The share and ephemeral this depositor reveals for `holder`: the
/// re-dealt ones, or what the seams make of them.
fn reveal_of(
    st: &crate::State,
    dep: &VaultDeposit,
    sid: &str,
    holder: &str,
) -> Option<Revealed> {
    let ctx = st.vault_ctx()?;
    let seed = st.vault_seed.as_ref()?;
    let (share, ikm) = molt_vault::rederive_share(dep, &st.republic_id(), &ctx, seed, holder).ok()?;
    let mut bytes = Zeroizing::new(*share.as_bytes());
    if let Some(bad) = st.vault_rx.bad_dealt.get(&(sid.to_string(), holder.to_string())) {
        *bytes = **bad;
    }
    if st.vault_seams.lab.lie_on_reveal.load(Ordering::SeqCst) {
        bytes[0] ^= 1;
    }
    Some((bytes, ikm))
}

/// Answer `holder`'s complaint on this seat's deposit: publish the reveal
/// and decide it here too (no seat hears its own frames). Returns whether
/// it went out.
pub(crate) fn answer(st: &mut crate::State, dep: &VaultDeposit, sid: &str, holder: &str) -> bool {
    let Some((share, ikm)) = reveal_of(st, dep, sid, holder) else {
        tracing::warn!(secret_id = %sid, holder = %holder, "vault: complaint not answerable");
        return false;
    };
    let me = st.member();
    let frame = VaultFrame::Reveal(VaultRevealFrame {
        v: VAULT_V,
        by: me.clone(),
        secret_id: sid.to_string(),
        holder: holder.to_string(),
        share: SecretHex(hex::encode(*share)),
        ikm: SecretHex(hex::encode(*ikm)),
    });
    let sent = publish(st, &frame);
    take_reveal(st, &me, sid, holder, share, ikm);
    sent
}

/// Every reveal of this seat's deposits to re-send, `(secret_id, holder)`:
/// each holder that complained or was already answered (only ever on a
/// complaint).
pub(crate) fn reveals_to_resend(st: &crate::State, known: &BTreeMap<String, VaultDeposit>) -> Vec<(String, MemberId)> {
    let me = st.member();
    let mut out = Vec::new();
    for (sid, dep) in known.iter().filter(|(_, d)| d.depositor == me) {
        let receipts = st.vault_rx.status.receipts.get(sid);
        let reveals = st.vault_rx.status.reveals.get(sid);
        out.extend(
            dep.holders
                .iter()
                .filter(|h| {
                    receipts.and_then(|r| r.get(*h)).is_some_and(|r| !r.verified)
                        || reveals.is_some_and(|r| r.contains_key(*h))
                })
                .map(|h| (sid.clone(), h.clone())),
        );
    }
    out
}

/// Whether `share` lies on `dep`'s polynomial at `holder`'s genesis x.
fn on_polynomial(st: &crate::State, dep: &VaultDeposit, holder: &str, share: &molt_vault::Share) -> bool {
    let Some(ctx) = st.vault_ctx() else {
        return false;
    };
    let Some(x) = ctx
        .holders_in_genesis_order
        .iter()
        .position(|(n, _, _)| n == holder)
        .and_then(|i| u8::try_from(i + 1).ok())
    else {
        return false;
    };
    let commitments: Option<Vec<[u8; 32]>> = dep
        .commitments
        .iter()
        .map(|c| {
            let mut out = [0u8; 32];
            hex::decode_to_slice(c, &mut out).ok().map(|()| out)
        })
        .collect();
    commitments.is_some_and(|c| molt_vault::verify_share(&c, x, share))
}

/// Decide a reveal from the depositor and keep the outcome. A lie stays
/// a lie: no later reveal clears it.
pub(crate) fn take_reveal(
    st: &mut crate::State,
    from: &MemberId,
    sid: &str,
    holder: &str,
    share: Zeroizing<[u8; 32]>,
    ikm: Zeroizing<[u8; 32]>,
) {
    let Some(ctx) = st.vault_ctx() else {
        return;
    };
    let Some(dep) = known_deposit(st, sid) else {
        if st.vault_rx.early.len() < EARLY_MAX {
            st.vault_rx.early.0.entry(sid.to_string()).or_default().push((
                from.clone(),
                holder.to_string(),
                share,
                ikm,
            ));
        }
        return;
    };
    if dep.depositor != *from || !dep.holders.iter().any(|h| h == holder) {
        tracing::debug!(secret_id = %sid, from = %from, "vault: reveal not from the depositor");
        return;
    }
    let share = molt_vault::Share::from_bytes(*share);
    let outcome = match molt_vault::decide(&dep, &st.republic_id(), &ctx, holder, &share, &ikm) {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(secret_id = %sid, error = %e, "vault: reveal not decidable");
            return;
        }
    };
    let valid = outcome == VaultRevealOutcome::FalseComplaint || on_polynomial(st, &dep, holder, &share);
    let rx = &mut st.vault_rx.status;
    let newly_valid =
        valid && rx.valid_reveals.entry(sid.to_string()).or_default().insert(holder.to_string());
    let slot = rx.reveals.entry(sid.to_string()).or_default();
    let decided = match slot.get(holder) {
        Some(VaultRevealOutcome::Lie) => false,
        Some(o) => *o != outcome,
        None => true,
    };
    if decided {
        slot.insert(holder.to_string(), outcome);
    }
    if !decided && !newly_valid {
        return;
    }
    persist(st);
    tracing::info!(secret_id = %sid, holder = %holder, outcome = ?outcome, "vault: complaint decided");
    st.emit_session(SessionScope::Full);
}

/// Decide the reveals that came before their deposit.
pub(crate) fn replay_early(st: &mut crate::State, sid: &str) {
    let Some(early) = st.vault_rx.early.0.remove(sid) else {
        return;
    };
    for (from, holder, share, ikm) in early {
        take_reveal(st, &from, sid, &holder, share, ikm);
    }
}

/// The seam's dishonest dealer: an honest deposit whose share for
/// `holder` is off the polynomial, sealed with the derived ephemeral and
/// signed - and revealed as dealt.
pub(crate) fn seal_with_bad_share(
    st: &mut crate::State,
    name: &str,
    kind: &str,
    text: &SecretText,
    holder: &str,
) -> Result<Reply, MoltError> {
    let ctx = st.vault_ctx().ok_or(MoltError::Vault(VaultRefusal::NoVault))?;
    let seed = st.vault_seed.clone().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?;
    let sk = st.identity_sk.clone().ok_or(MoltError::Vault(VaultRefusal::NoVaultKey))?;
    let rid = st.republic_id();
    let me = st.member();
    let engine = |e: molt_vault::VaultError| MoltError::Engine(format!("vault: {e}"));
    let replaces = st.vault_slot_current(&me, name)?;
    let input = molt_vault::DepositInput { republic_id: &rid, depositor: &me, name, kind, replaces: &replaces, text, ctx: &ctx };
    let (mut dep, file) =
        molt_vault::build_deposit(&input, &seed, &sk, &mut crate::vault::deposit::os_rng()?).map_err(engine)?;
    let sid = secret_id(&rid, &dep);
    let (share, ikm) = molt_vault::rederive_share(&dep, &rid, &ctx, &seed, holder).map_err(engine)?;
    let mut bad = Zeroizing::new(*share.as_bytes());
    bad[0] ^= 1;
    let i = dep.holders.iter().position(|h| h == holder).ok_or_else(|| MoltError::Engine("vault: not a holder".into()))?;
    let pk = &ctx.holders_in_genesis_order.iter().find(|(n, _, _)| n == holder).ok_or_else(|| MoltError::Engine("vault: not a holder".into()))?.2;
    let sealed = molt_vault::seal_share(pk, &molt_vault::Share::from_bytes(*bad), &share_aad(&sid, holder), &ikm)
        .map_err(engine)?;
    dep.enc_share[i] = hex::encode(sealed);
    dep.sig_depositor = molt_storage::identity_sign(&sk, &deposit_signing_bytes(&rid, &dep));
    st.vault_rx.bad_dealt.insert((sid, holder.to_string()), bad);
    st.vault_propose_deposit(dep, &file)
}
