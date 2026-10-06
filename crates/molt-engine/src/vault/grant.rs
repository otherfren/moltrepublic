// SPDX-License-Identifier: GPL-3.0-or-later

//! Grant: propose, approve check, answer on commit and on request, the
//! audit (plan stage S4, spec §8, D17).

use std::collections::{BTreeMap, BTreeSet};

use molt_core::vault::{
    grant_id, resp_aad, SecretHex, VaultCtx, VaultDeposit, VaultDisplacedGrant, VaultGrant,
    VaultGrantState, VaultGrantView, VaultOp, VaultRefusal, VaultView,
};
use molt_core::{MemberId, MoltError, ProposalState, Reply, Surface};
use molt_net::vault_frames::{VaultAskFrame, VaultFrame, VaultRespFrame, VAULT_V};
use molt_vault::Share;
use serde_json::Value;

/// A reader's ask is answered at most this often per (grant, asker).
const ASK_EVERY_SECS: u64 = 10;

/// One answer at the reader: a share that checked, or a seat that failed.
pub(crate) enum Answer {
    Good(Share),
    Bad,
}

/// Grant bookkeeping of the open workspace. Answers live here only, never
/// on disk (spec §8); the displaced audit is persisted (D17).
#[derive(Default)]
pub(crate) struct GrantRuntime {
    /// The workspace incarnation (`net_scope`) this belongs to.
    scope: Option<u64>,
    /// Grants this seat answered on commit, or found applied at open.
    answered: BTreeSet<String>,
    /// `(grant_id, asker)` -> when last answered on request.
    asked: BTreeMap<(String, MemberId), u64>,
    /// Reader side: `grant_id` -> when this seat last asked.
    last_ask: BTreeMap<String, u64>,
    /// Committed grants waiting for the vault base.
    queued: BTreeSet<String>,
    /// Reader side: `grant_id` -> answering seat -> answer.
    answers: BTreeMap<String, BTreeMap<MemberId, Answer>>,
    /// Versions `Event::VaultReadable` was emitted for.
    readable: BTreeSet<String>,
    /// Grants applied here, for the audit line if a reorg drops one.
    seen: BTreeMap<String, VaultDisplacedGrant>,
    /// Applied here, then displaced by a reorg (persisted).
    pub(crate) displaced: Vec<VaultDisplacedGrant>,
    /// Every vault frame this node published.
    #[cfg(test)]
    pub(crate) sent: Vec<VaultFrame>,
    /// Payload bytes for reads in unit tests (no storage there).
    #[cfg(test)]
    pub(crate) payloads: BTreeMap<String, Vec<u8>>,
    /// Unit tests: the publish queue refuses.
    #[cfg(test)]
    pub(crate) publish_fails: bool,
}

/// Fold `lines` into the persisted audit; whether anything changed. A
/// merge: one reorg's persist tasks may land in any order.
pub(crate) fn merge_displaced(s: &mut molt_core::TransportState, lines: &[VaultDisplacedGrant]) -> bool {
    let mut changed = false;
    for d in lines {
        if !s.vault_displaced.iter().any(|x| x.grant_id == d.grant_id) {
            s.vault_displaced.push(d.clone());
            changed = true;
        }
    }
    changed
}

/// A committed grant as this seat sees it now.
pub(crate) struct LiveGrant {
    pub(crate) grant: VaultGrant,
    pub(crate) deposit: VaultDeposit,
    /// On the current version (not void, not replaced since).
    pub(crate) valid: bool,
}

/// A seat's Shamir x: its 1-based position in the founding table.
pub(crate) fn seat_x(ctx: &VaultCtx, seat: &str) -> Option<u8> {
    let i = ctx.holders_in_genesis_order.iter().position(|(n, _, _)| n == seat)?;
    u8::try_from(i + 1).ok()
}

/// A seat's FOUNDING vault key (plan 1.3.13).
fn founding_vault_pk<'a>(ctx: &'a VaultCtx, seat: &str) -> Option<&'a str> {
    ctx.holders_in_genesis_order
        .iter()
        .find(|(n, _, _)| n == seat)
        .map(|(_, _, pk)| pk.as_str())
}

pub(crate) fn commitments(dep: &VaultDeposit) -> Option<Vec<[u8; 32]>> {
    dep.commitments
        .iter()
        .map(|c| hex::decode(c).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()))
        .collect()
}

fn os_rng() -> Option<rand_chacha::ChaCha20Rng> {
    use rand_chacha::rand_core::SeedableRng as _;
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::getrandom(seed.as_mut()).ok()?;
    Some(rand_chacha::ChaCha20Rng::from_seed(*seed))
}

impl crate::State {
    /// The grant runtime of this workspace incarnation.
    pub(crate) fn grants_rt(&mut self) -> &mut GrantRuntime {
        if self.vault_grants.scope != Some(self.net_scope) {
            self.vault_grants = GrantRuntime { scope: Some(self.net_scope), ..GrantRuntime::default() };
        }
        &mut self.vault_grants
    }

    fn grants_rt_ref(&self) -> Option<&GrantRuntime> {
        (self.vault_grants.scope == Some(self.net_scope)).then_some(&self.vault_grants)
    }

    /// At open, after the chain: the persisted displaced audit, and every
    /// grant already applied counts as answered, so a reorg re-applying one
    /// does not answer it again (Q1).
    pub(crate) fn vault_load_displaced(&mut self, displaced: Vec<VaultDisplacedGrant>) {
        let applied: BTreeSet<String> = self.vault_state().grants.iter().map(|g| g.grant.grant_id.clone()).collect();
        let rt = self.grants_rt();
        rt.displaced = displaced;
        rt.answered.extend(applied);
    }

    /// The committed grant `grant_id` and its version.
    pub(crate) fn live_grant(&self, grant_id: &str) -> Option<LiveGrant> {
        let vs = self.vault_state();
        let g = vs.grants.iter().rev().find(|g| g.grant.grant_id == grant_id)?;
        let v = vs.versions.get(&g.grant.secret_id)?;
        Some(LiveGrant {
            valid: !g.void && vs.is_current(&g.grant.secret_id),
            grant: g.grant.clone(),
            deposit: v.deposit.clone(),
        })
    }

    fn vault_publish(&mut self, frame: VaultFrame) -> bool {
        let bytes = frame.to_frame();
        #[cfg(test)]
        {
            self.grants_rt().sent.push(frame);
            if self.group_net.is_none() {
                return !self.vault_grants.publish_fails;
            }
        }
        self.group_net.as_ref().is_some_and(|g| g.handle.publish_control(bytes))
    }

    /// This holder's share of `dep`, opened and checked; never stored.
    pub(crate) fn vault_my_share(&self, dep: &VaultDeposit, ctx: &VaultCtx) -> Option<Share> {
        let me = self.member();
        if !dep.holders.contains(&me) {
            return None;
        }
        let seed = self.vault_seed.as_ref()?;
        let (sk, _) = molt_vault::vault_keypair(seed);
        molt_vault::check_my_share(dep, &self.republic_id(), ctx, &me, &sk).ok()
    }

    pub(crate) fn cmd_vault_grant(&mut self, secret_id: String, reader: MemberId) -> Result<Reply, MoltError> {
        let ctx = self.vault_ctx().ok_or(MoltError::Vault(VaultRefusal::NoVault))?;
        if self.vault_base_pending() {
            return Err(self.vault_base_pending_error());
        }
        if self.vault_seed.is_none() {
            return Err(MoltError::Vault(VaultRefusal::NoVaultKey));
        }
        if !self.vault_state().is_current(&secret_id) {
            return Err(MoltError::BadPayload("not the current version".to_string()));
        }
        if seat_x(&ctx, &reader).is_none() {
            return Err(MoltError::BadPayload(format!("{reader}: not a seat")));
        }
        // the id the propose path mints next; grant_id binds it
        let floor = self.next_id;
        let id = self.mint_proposal_id();
        self.next_id = floor;
        let grant = VaultGrant { grant_id: grant_id(&secret_id, &reader, id), secret_id, reader };
        let payload =
            serde_json::to_value(VaultOp::Grant(grant)).map_err(|e| MoltError::Engine(format!("vault: {e}")))?;
        let reply = self.propose_payload(Surface::Vault, payload)?;
        if !matches!(&reply, Reply::Proposed { id: got, .. } if got.0 == id) {
            return Err(MoltError::Engine("vault: grant id drifted".to_string()));
        }
        tracing::info!(id, "vault: grant proposed");
        Ok(reply)
    }

    /// The grant arm of `approve`: `grant_id` recomputes, the target is the current version, the base is here, and
    /// this seat has its key.
    pub(crate) fn approve_check_grant(&self, payload: &Value) -> Result<(), MoltError> {
        let ctx = self.vault_ctx().ok_or(MoltError::Vault(VaultRefusal::NoVault))?;
        let Ok(VaultOp::Grant(g)) = serde_json::from_value::<VaultOp>(payload.clone()) else {
            return Err(MoltError::Vault(VaultRefusal::NotVerified));
        };
        // every card carrying this payload must recompute: the signed block
        // binds the card's id, which this check does not see
        let rid = self.republic_id();
        let mut cards = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == Surface::Vault && &p.payload == payload)
            .peekable();
        if cards.peek().is_none()
            || !cards.all(|(id, _)| super::deposit::check_vault_op(&rid, *id, payload, &ctx).is_ok())
        {
            return Err(MoltError::Vault(VaultRefusal::NotVerified));
        }
        if self.vault_base_pending() {
            return Err(self.vault_base_pending_error());
        }
        if self.vault_seed.is_none() {
            return Err(MoltError::Vault(VaultRefusal::NoVaultKey));
        }
        if !self.vault_state().is_current(&g.secret_id) {
            return Err(MoltError::Vault(VaultRefusal::NotVerified));
        }
        Ok(())
    }

    /// Seal this holder's share of a valid grant to its reader's FOUNDING
    /// key and publish it. Returns whether an answer was produced.
    fn vault_answer(&mut self, live: &LiveGrant) -> bool {
        let Some(ctx) = self.vault_ctx() else {
            return false;
        };
        let me = self.member();
        if !live.valid || live.grant.reader == me || !live.deposit.holders.contains(&me) {
            return false;
        }
        if self.vault_seed.is_none() {
            tracing::warn!(grant_id = %live.grant.grant_id, "vault_seed=missing");
            return false;
        }
        let Some(share) = self.vault_my_share(&live.deposit, &ctx) else {
            tracing::warn!(grant_id = %live.grant.grant_id, "vault: own share does not verify");
            return false;
        };
        let Some(reader_pk) = founding_vault_pk(&ctx, &live.grant.reader) else {
            return false;
        };
        let aad = resp_aad(&self.republic_id(), &live.grant.grant_id, &me);
        let Some(enc) = os_rng().and_then(|mut rng| molt_vault::seal_resp(reader_pk, &share, &aad, &mut rng).ok())
        else {
            return false;
        };
        let frame = VaultFrame::Resp(VaultRespFrame {
            v: VAULT_V,
            by: me,
            grant_id: live.grant.grant_id.clone(),
            enc: SecretHex(hex::encode(enc)),
        });
        if !self.vault_publish(frame) {
            tracing::warn!(grant_id = %live.grant.grant_id, "vault: answer not queued");
            return false;
        }
        tracing::info!(grant_id = %live.grant.grant_id, reader = %live.grant.reader, "vault: answered");
        true
    }

    pub(crate) fn cmd_net_vault_resp(
        &mut self,
        from: &MemberId,
        grant_id: String,
        enc: SecretHex,
    ) -> Result<Reply, MoltError> {
        let me = self.member();
        let (Some(ctx), Some(live)) = (self.vault_ctx(), self.live_grant(&grant_id)) else {
            return Ok(Reply::Ack);
        };
        if live.grant.reader != me || !live.valid || !live.deposit.holders.contains(from) {
            return Ok(Reply::Ack);
        }
        let (Some(x), Some(seed)) = (seat_x(&ctx, from), self.vault_seed.as_ref()) else {
            return Ok(Reply::Ack);
        };
        // seat, x and AAD come from the MLS sender, never the frame
        let (sk, _) = molt_vault::vault_keypair(seed);
        let aad = resp_aad(&self.republic_id(), &grant_id, from);
        let opened = hex::decode(&enc.0).ok().and_then(|b| molt_vault::open_resp(&sk, &b, &aad).ok());
        let answer = match (opened, commitments(&live.deposit)) {
            (Some(share), Some(c)) if molt_vault::verify_share(&c, x, &share) => Answer::Good(share),
            _ => {
                tracing::warn!(grant_id = %grant_id, from = %from, "vault: bad answer");
                Answer::Bad
            }
        };
        let rt = self.grants_rt();
        let seats = rt.answers.entry(grant_id).or_default();
        if !(matches!(answer, Answer::Bad) && matches!(seats.get(from), Some(Answer::Good(_)))) {
            seats.insert(from.clone(), answer);
        }
        let sid = live.grant.secret_id.clone();
        if self.vault_have(&sid) >= live.deposit.m && self.grants_rt().readable.insert(sid.clone()) {
            self.emit(molt_core::Event::VaultReadable { secret_id: sid });
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_ask(&mut self, from: &MemberId, grant_id: String) -> Result<Reply, MoltError> {
        self.vault_flush_queued();
        let Some(live) = self.live_grant(&grant_id) else {
            return Ok(Reply::Ack);
        };
        if &live.grant.reader != from || !live.valid {
            return Ok(Reply::Ack);
        }
        let now = crate::now_secs();
        let rt = self.grants_rt();
        rt.asked.retain(|_, t| now.saturating_sub(*t) < ASK_EVERY_SECS);
        let key = (grant_id.clone(), from.clone());
        if rt.asked.contains_key(&key) {
            return Ok(Reply::Ack);
        }
        if self.vault_base_pending() {
            self.grants_rt().queued.insert(grant_id);
            return Ok(Reply::Ack);
        }
        if self.vault_answer(&live) {
            self.grants_rt().asked.insert(key, now);
        }
        Ok(Reply::Ack)
    }

    /// Valid shares this reader holds for `secret_id`: its own plus every
    /// answer that checked, one per seat.
    pub(crate) fn vault_have(&self, secret_id: &str) -> u8 {
        u8::try_from(self.vault_reader_shares(secret_id).len()).unwrap_or(u8::MAX)
    }

    /// `(seat, x, share)` for every valid share at this reader.
    pub(crate) fn vault_reader_shares(&self, secret_id: &str) -> Vec<molt_vault::SeatShare> {
        let vs = self.vault_state();
        let (Some(ctx), Some(v)) = (self.vault_ctx(), vs.versions.get(secret_id)) else {
            return Vec::new();
        };
        let me = self.member();
        let mut out: Vec<molt_vault::SeatShare> = Vec::new();
        if let (Some(share), Some(x)) = (self.vault_my_share(&v.deposit, &ctx), seat_x(&ctx, &me)) {
            out.push(molt_vault::SeatShare { seat: me.clone(), x, share });
        }
        let Some(rt) = self.grants_rt_ref() else {
            return out;
        };
        for g in vs.grants.iter().filter(|g| g.grant.secret_id == secret_id && g.grant.reader == me) {
            for (seat, a) in rt.answers.get(&g.grant.grant_id).into_iter().flatten() {
                let (Answer::Good(share), Some(x)) = (a, seat_x(&ctx, seat)) else {
                    continue;
                };
                if !out.iter().any(|s| &s.seat == seat) {
                    out.push(molt_vault::SeatShare { seat: seat.clone(), x, share: share.clone() });
                }
            }
        }
        out
    }

    /// Ask the holders to answer every grant of `secret_id` to this seat,
    /// at most once per `ASK_EVERY_SECS` per grant.
    pub(crate) fn vault_ask(&mut self, secret_id: &str) {
        let now = crate::now_secs();
        let me = self.member();
        let grants: Vec<String> = self
            .vault_state()
            .grants
            .iter()
            .filter(|g| !g.void && g.grant.secret_id == secret_id && g.grant.reader == me)
            .map(|g| g.grant.grant_id.clone())
            .collect();
        let rt = self.grants_rt();
        rt.last_ask.retain(|_, t| now.saturating_sub(*t) < ASK_EVERY_SECS);
        let due: Vec<String> = grants.into_iter().filter(|g| !rt.last_ask.contains_key(g)).collect();
        for grant_id in due {
            if self.vault_publish(VaultFrame::Ask(VaultAskFrame { v: VAULT_V, by: me.clone(), grant_id: grant_id.clone() })) {
                self.grants_rt().last_ask.insert(grant_id, now);
            }
        }
    }

    /// Answers queued while the vault base was pending (S5 calls this when
    /// the base arrives).
    pub(crate) fn vault_flush_queued(&mut self) {
        if self.vault_base_pending() {
            return;
        }
        for grant_id in std::mem::take(&mut self.grants_rt().queued) {
            if let Some(live) = self.live_grant(&grant_id) {
                if self.vault_answer(&live) {
                    self.grants_rt().answered.insert(grant_id);
                }
            }
        }
    }

    fn persist_vault_displaced(&self) {
        let Some(store) = self.file_store() else {
            return;
        };
        let displaced = self.vault_grants.displaced.clone();
        tokio::spawn(async move {
            use molt_net::supervisor::StateStore as _;
            store
                .update(|s| merge_displaced(s, &displaced))
                .await;
        });
    }
}

/// A grant committed: answer it if this seat holds a share (once per
/// `grant_id`, D17), queue it while the base is pending.
pub(crate) fn on_commit(st: &mut crate::State, grant_id: &str) {
    st.vault_flush_queued();
    let Some(live) = st.live_grant(grant_id) else {
        return;
    };
    let name = live.deposit.name.clone();
    let reader = live.grant.reader.clone();
    let rt = st.grants_rt();
    // `answered` is per incarnation: a displaced line this incarnation never
    // committed was answered (or dropped) by an earlier one
    let earlier = !rt.seen.contains_key(grant_id) && rt.displaced.iter().any(|d| d.grant_id == grant_id);
    rt.seen.insert(
        grant_id.to_string(),
        VaultDisplacedGrant { grant_id: grant_id.to_string(), name, reader },
    );
    let done = rt.answered.contains(grant_id) || earlier;
    if !live.valid {
        tracing::info!(grant_id, "vault: grant void");
        return;
    }
    if done {
        tracing::info!(grant_id, "vault: grant already answered");
        return;
    }
    if st.vault_base_pending() {
        st.grants_rt().queued.insert(grant_id.to_string());
        return;
    }
    if st.vault_answer(&live) {
        st.grants_rt().answered.insert(grant_id.to_string());
    }
    let sid = live.grant.secret_id;
    if live.grant.reader == st.member() && st.vault_have(&sid) >= live.deposit.m && st.grants_rt().readable.insert(sid.clone()) {
        st.emit(molt_core::Event::VaultReadable { secret_id: sid });
    }
}

/// A reorg displaced an applied grant (plan 1.4): keep the audit line.
pub(crate) fn on_displaced(st: &mut crate::State, grant_id: &str) {
    let line = st.grants_rt().seen.get(grant_id).cloned().or_else(|| {
        let g = st.proposals.values().find_map(|p| match serde_json::from_value::<VaultOp>(p.payload.clone()) {
            Ok(VaultOp::Grant(g)) if p.surface == Surface::Vault && g.grant_id == grant_id => Some(g),
            _ => None,
        })?;
        let name = st.vault_state().versions.get(&g.secret_id).map(|v| v.deposit.name.clone()).unwrap_or_default();
        Some(VaultDisplacedGrant { grant_id: grant_id.to_string(), name, reader: g.reader })
    });
    let Some(line) = line else {
        return;
    };
    let rt = st.grants_rt();
    if rt.displaced.iter().any(|d| d.grant_id == grant_id) {
        return;
    }
    rt.displaced.push(line);
    tracing::warn!(grant_id, "vault: grant displaced");
    st.persist_vault_displaced();
    st.emit_session(crate::SessionScope::Full);
}

/// The `secret_id` and proposal of an open grant card.
fn grant_card(p: &molt_core::ProposalRecord) -> Option<VaultGrant> {
    if p.surface != Surface::Vault {
        return None;
    }
    match serde_json::from_value::<VaultOp>(p.payload.clone()) {
        Ok(VaultOp::Grant(g)) => Some(g),
        _ => None,
    }
}

/// Grant cards: open votes, the committed audit (void included), and the
/// displaced lines.
pub(crate) fn fill(st: &crate::State, view: &mut VaultView) {
    let Some(ctx) = st.vault_ctx() else {
        return;
    };
    let me = st.member();
    let vs = st.vault_state();
    let rt = st.grants_rt_ref();
    let card = |g: &VaultGrant, state: VaultGrantState| {
        let v = vs.versions.get(&g.secret_id);
        let mut c = VaultGrantView {
            grant_id: g.grant_id.clone(),
            secret_id: g.secret_id.clone(),
            depositor: v.map(|v| v.deposit.depositor.clone()).unwrap_or_default(),
            name: v.map(|v| v.deposit.name.clone()).unwrap_or_default(),
            reader: g.reader.clone(),
            state,
            mine: g.reader == me,
            need: v.map_or(ctx.m, |v| v.deposit.m),
            ..VaultGrantView::default()
        };
        if c.mine && state == VaultGrantState::Committed {
            c.answers = st.vault_have(&g.secret_id);
            c.bad_answers = rt
                .and_then(|rt| rt.answers.get(&g.grant_id))
                .map(|a| a.iter().filter(|(_, a)| matches!(a, Answer::Bad)).map(|(s, _)| s.clone()).collect())
                .unwrap_or_default();
        }
        c
    };
    for (id, p) in &st.proposals {
        if p.state != ProposalState::Proposed || p.withdrawn {
            continue;
        }
        if let Some(g) = grant_card(p) {
            let mut c = card(&g, VaultGrantState::Pending);
            c.proposal = Some(*id);
            view.grants.push(c);
        }
    }
    for g in &vs.grants {
        let state = if g.void || !vs.is_current(&g.grant.secret_id) {
            VaultGrantState::Void
        } else {
            VaultGrantState::Committed
        };
        let mut c = card(&g.grant, state);
        c.at = st
            .proposals
            .values()
            .find(|p| grant_card(p).is_some_and(|x| x.grant_id == g.grant.grant_id))
            .map(|p| p.applied_at)
            .filter(|t| *t > 0);
        view.grants.push(c);
    }
    for d in rt.map(|rt| rt.displaced.as_slice()).unwrap_or_default() {
        if vs.grants.iter().any(|g| g.grant.grant_id == d.grant_id) {
            continue;
        }
        view.grants.push(VaultGrantView {
            grant_id: d.grant_id.clone(),
            name: d.name.clone(),
            reader: d.reader.clone(),
            state: VaultGrantState::Displaced,
            mine: d.reader == me,
            need: ctx.m,
            ..VaultGrantView::default()
        });
    }
}

#[cfg(test)]
#[path = "grant_tests.rs"]
mod tests;
