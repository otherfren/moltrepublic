// SPDX-License-Identifier: GPL-3.0-or-later

//! The delivery guarantee's engine side
//! (`docs_archive/transport/delivery_guarantee.md`): the per-sender accept
//! windows and their debounced persists, the G7 in-order park, the ACK
//! flush and the group claim sheet, the live MLS-ratchet persist, and the
//! delivery tick that drives all of it. `cmd_net_delivered` is the accept
//! point; what an accepted envelope DOES is [`super::ingest`].

use super::*;

/// Debounce for persisting the receive-side accept windows (delivery
/// guarantee §4.7): at most one `transport.state` merge per this many
/// seconds, riding the presence tick; a clean close always flushes.
const ACCEPT_SAVE_SECS: u64 = 5;

/// Delivery-ACK debounce (§4.3): an accepted or duplicate delivery arms an
/// ack to its sender no later than this many seconds out; a burst inside
/// the window is answered by ONE ack frame.
pub(super) const ACK_DEBOUNCE_SECS: u64 = 3;

/// Debounce for the live MLS-ratchet persist (§4.6 / E6): with mesh traffic,
/// the current ratchet reaches `transport.state` at least this often — the
/// hard-kill regression window.
const MLS_PERSIST_SECS: u64 = 10;

/// G7 (§10): bytes of decrypted, not yet applied envelopes kept across all
/// senders; past it the highest seq of the largest buffer is shed (it stays
/// owed: the listing and the sender's rounds bring it back).
pub(crate) const HOLD_BYTES_MAX: usize = 64 * 1024 * 1024;

/// A seq missing this long is asked for; younger gaps are mostly a
/// newest-first relay replay filling itself.
const GAP_ASK_AFTER_SECS: u64 = 2;

/// Re-ask a sender's still-missing seqs (and its listing) this often.
const GAP_ASK_EVERY_SECS: u64 = 10;

/// Relay skew after evidence that the gap will not fill (the sender
/// re-published it, or a resend round passed over it): settled after this.
pub(crate) const GAP_PASS_GRACE_SECS: u64 = 5;

/// The ordering bound for a sender that answers nothing (an older build):
/// six times the first rung of its resend ladder. Order only - never the
/// frontier.
pub(crate) const GAP_FALLBACK_SECS: u64 = 60;

/// Missing and settled seqs tracked per sender (each); past it the lowest
/// missing one is settled.
const SETTLED_MAX: usize = 1024;

/// Seqs above the frontier remembered as seen this session, per sender.
const SESSION_MAX: usize = 65_536;

/// Re-publications of a listed seq that never landed before it is given up.
const LISTED_TRIES: u8 = 3;

/// Dropped frames newer than anything accepted that make one episode, and
/// consecutive fresh accepts that end it.
const DUP_EPISODE_MIN: u32 = 32;

/// One sender's run of frames dropped as already seen although authored
/// after everything accepted from it.
#[derive(Debug, Default)]
pub(crate) struct DupEpisode {
    first_seq: u64,
    last_seq: u64,
    count: u32,
    warned: bool,
    /// Fresh accepts since the last counted drop.
    clean: u32,
}

/// One missing seq of a sender's stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gap {
    since: u64,
    asked_at: Option<u64>,
    /// Evidence it will not fill: settled at this instant.
    settle_at: Option<u64>,
}

/// The sender's complete list of its own seqs in `(frontier, through]`.
#[derive(Debug, Default)]
struct Listing {
    through: u64,
    own: std::collections::BTreeSet<u64>,
    /// Per listed seq: re-publications that did not land.
    tries: std::collections::BTreeMap<u64, u8>,
}

/// G7 per sender (§10).
///
/// `frontier`: every envelope the sender owes at or below it is accepted or
/// proven unfillable, so nothing is accepted above `frontier +
/// ACCEPT_WINDOW_BITS` - the window can never age out an owed seq, and the
/// ack sheet never reports one as accepted. `held` keeps decrypted
/// envelopes that may not be applied yet, unaccepted: above that cap, or
/// order-sensitive behind a missing seq.
#[derive(Debug, Default)]
pub(crate) struct SenderHold {
    frontier: u64,
    /// Accepted envelopes above the frontier, `prev_seq -> seq`.
    ahead: std::collections::BTreeMap<u64, u64>,
    missing: std::collections::BTreeMap<u64, Gap>,
    held: std::collections::BTreeMap<u64, (EventEnvelope, usize)>,
    bytes: usize,
    settled: std::collections::BTreeSet<u64>,
    /// Delivered but left unaccepted (the D7 decline park).
    refused: std::collections::BTreeSet<u64>,
    listing: Option<Listing>,
    listing_asked_at: Option<u64>,
    /// Seqs above the frontier received in THIS session. A second arrival
    /// of one is a fresh encryption (MLS refuses to open a consumed frame
    /// twice), so only the sender's resend round can produce it - unlike a
    /// stored frame re-read after a restart from a ratchet persisted late.
    session: std::collections::BTreeSet<u64>,
    /// An older sender's resend round passed (a repeat arrived), and the
    /// lowest repeat seen since.
    passed_at: Option<u64>,
    passed_low: u64,
    /// The current hold episode: start, logged, released out of order,
    /// shed, given up.
    since: Option<u64>,
    logged: bool,
    unordered: u32,
    shed: u32,
    lost: u32,
    /// The sender answers gap wants: its duplicates are targeted resends
    /// for other askers, never evidence about our gaps.
    answers: bool,
    /// A window reset for a restore: owed from seq 0.
    from_start: bool,
}

impl SenderHold {
    fn new(frontier: u64, links: Vec<(u64, u64)>) -> SenderHold {
        let mut hold = SenderHold { frontier, ..SenderHold::default() };
        hold.ahead = links.into_iter().filter(|(_, s)| *s > frontier).collect();
        hold.raise(frontier);
        hold
    }

    fn cap(&self) -> u64 {
        self.frontier.saturating_add(molt_core::ACCEPT_WINDOW_BITS)
    }

    /// Whether an order-sensitive envelope at `seq` must wait.
    fn blocks(&self, seq: u64) -> bool {
        self.missing.range(..seq).next().is_some() || self.held.range(..seq).next().is_some()
    }

    /// Track `seq` as missing unless held or settled already.
    fn note_missing(&mut self, seq: u64, now: u64) {
        if seq <= self.frontier || self.held.contains_key(&seq) || self.settled.contains(&seq) {
            return;
        }
        self.missing.entry(seq).or_insert(Gap { since: now, asked_at: None, settle_at: None });
        if self.missing.len() > SETTLED_MAX {
            if let Some((&lowest, _)) = self.missing.first_key_value() {
                self.settle(lowest);
            }
        }
    }

    /// Evidence from an older sender's resend round.
    fn passed(&mut self, seq: u64, now: u64) {
        if self.answers || seq <= self.frontier || !self.session.contains(&seq) {
            return;
        }
        if self.passed_at.is_none() {
            self.passed_low = seq;
        }
        self.passed_at.get_or_insert(now);
        self.passed_low = self.passed_low.min(seq);
        for gap in self.missing.range_mut(..seq).map(|(_, g)| g) {
            gap.settle_at.get_or_insert(now + GAP_PASS_GRACE_SECS);
        }
    }

    /// Order only: `seq` no longer holds what is behind it.
    fn settle(&mut self, seq: u64) -> bool {
        if self.missing.remove(&seq).is_none() {
            return false;
        }
        self.settled.insert(seq);
        while self.settled.len() > SETTLED_MAX {
            self.settled.pop_first();
        }
        true
    }

    /// Chain `seq` (accepted, predecessor `prev`) onto the frontier.
    fn link(&mut self, prev: u64, seq: u64) {
        if prev == 0 || prev <= self.frontier {
            self.raise(seq);
        } else {
            self.ahead.insert(prev, seq);
        }
    }

    /// Move the frontier up to `to` and on through every accepted envelope
    /// chained to it.
    fn raise(&mut self, to: u64) {
        self.frontier = self.frontier.max(to);
        while let Some((&p, &s)) = self.ahead.first_key_value() {
            if p > self.frontier {
                break;
            }
            self.ahead.pop_first();
            self.frontier = self.frontier.max(s);
        }
        let f = self.frontier;
        self.missing.retain(|s, _| *s > f);
        self.session.retain(|s| *s > f);
        self.refused.retain(|s| *s > f);
        if self.listing.as_ref().is_some_and(|l| l.through <= f) {
            self.listing = None;
        }
    }
}

/// Whether `next` claims what `last` claimed: the stamp alone changes on
/// every sheet, and a duplicate burst must not republish an unchanged one.
pub(super) fn same_sheet(last: Option<&molt_net::group_ack::GroupAck>, next: &molt_net::group_ack::GroupAck) -> bool {
    last.is_some_and(|l| l.by == next.by && l.claims == next.claims)
}

/// Whether an envelope's effect depends on its sender's earlier envelopes:
/// the chat stream (arrival is display order; a reaction toggles) and a
/// member's stances (newest wins). A withdrawal is terminal and parks on
/// its own card's id, so it needs no order beyond that.
pub(super) fn order_sensitive(body: &WorkspaceEvent) -> bool {
    matches!(
        body,
        WorkspaceEvent::Chat(_)
            | WorkspaceEvent::ChatReacted { .. }
            | WorkspaceEvent::ChatDeleted { .. }
            | WorkspaceEvent::ChatRead { .. }
            | WorkspaceEvent::FileRemoved { .. }
            | WorkspaceEvent::Approved { .. }
            | WorkspaceEvent::Declined { .. }
    )
}


impl State {
    /// An authenticated peer event arrived. Validation failures are
    /// ack-and-skip (returning an error would wedge the supervisor on a
    /// poison event); T1's wire scope is [`crosses_wire`] — everything
    /// else is logged and ignored until MLS lands (T2).
    /// Forget a peer's accept window (delivery guarantee, E7 finding 1): a
    /// recovery re-key hands the seat to a NEW incarnation whose log seq
    /// space restarts — the old window's marks would swallow every fresh
    /// envelope as a duplicate. Called at the survivor's authenticated
    /// recovery-announce point; the next save persists the reset.
    pub(crate) fn reset_peer_accept_window(&mut self, member: &MemberId) {
        // a fresh incarnation asks for its catch-up anew: the C3 debounce
        // keyed by name must not swallow the rejoiner's request because the
        // lost device asked within the last thirty seconds
        self.chain.served_at.remove(member);
        self.forget_peer_claim(member);
        if self.delivery.accepted.remove(member).is_some() {
            self.delivery.accepted_dirty = true;
        }
        // any pending ack deadline refers to the OLD window — drop it too
        self.delivery.ack_due.remove(member);
        // held successors chain into the OLD incarnation's seq space; the
        // new incarnation resends its own history anyway (G7)
        self.delivery.inbound.remove(member);
        self.delivery.dups.remove(member);
    }

    /// [`Self::reset_peer_accept_window`], leaving an empty window keyed to
    /// `restore` with `proof`, written at once: a reset a kill can lose is
    /// the fault this exists for (§11).
    fn forget_incarnation(&mut self, member: &str, restore: &str, proof: molt_core::RestoreProof) {
        let member = member.to_string();
        let high = self.delivery.accepted.get(&member).map_or(0, |w| w.high);
        self.reset_peer_accept_window(&member);
        let window = molt_core::AcceptedWindow { restore: restore.to_string(), proof, ..Default::default() };
        self.delivery.accepted.insert(member.clone(), window);
        self.save_accepted_now();
        tracing::info!(%member, high, via = proof.label(), "accept_window_reset");
    }

    fn save_accepted_now(&mut self) {
        self.delivery.reset_saves = self.delivery.reset_saves.saturating_add(1);
        self.delivery.accepted_dirty = true;
        if let Some(active) = self.active.as_ref() {
            active.handle.save_accepted(self.delivery.accepted.clone());
            self.delivery.accepted_dirty = false;
            self.delivery.accepted_saved_at = self.presence_now();
        }
    }

    /// A re-key commit merged that re-admitted `member`. Always safe: before
    /// the merge no frame of the new device can be decrypted here, so the
    /// window holds the old incarnation only. It is the merge of the
    /// restore whose block was applied here last, if that one's merge is
    /// still due; else of one whose block is still to come.
    pub(crate) fn reset_after_rekey(&mut self, member: &str) {
        use molt_core::RestoreProof as P;
        let (restore, proof) = match self.delivery.accepted.get(member) {
            Some(w) if matches!(w.proof, P::Adopted | P::Block) => (w.restore.clone(), P::Merge),
            _ => (self.chain.anchors.get(member).cloned().unwrap_or_default(), P::MergePending),
        };
        self.forget_incarnation(member, &restore, proof);
    }

    /// A `Restored` block for `member` installing `anchor` is applied. A
    /// window keyed to an earlier known restore holds that device's stream:
    /// reset. A first-contact or unkeyed window is only keyed - the evidence
    /// rule resets it if it predates the restore - and a merged one whose
    /// block this is adopts the anchor.
    pub(crate) fn reset_for_restore(&mut self, member: &str, anchor: &str) {
        use molt_core::RestoreProof as P;
        if anchor.is_empty() || member == self.member() || self.drop_seam.keeps_window_on_block() {
            return;
        }
        let Some(w) = self.delivery.accepted.get(member) else { return };
        // our own seal is stamped only after it settles: it is now
        let sealed = match self.restore_sealed_at(member) {
            0 => self.presence_now(),
            t => t,
        };
        let next = match w.proof {
            P::MergePending if w.restore != anchor => P::Merge,
            _ if w.restore == anchor => return,
            // a first contact made after the restore holds only the new
            // device; one made clearly before it holds the old one
            P::Pending if w.born > sealed => P::Merge,
            P::Pending if w.born > 0 && w.born.saturating_add(self.drop_seam.evidence_skew()) <= sealed => {
                self.forget_incarnation(member, anchor, P::Block);
                return;
            }
            P::Pending => P::Adopted,
            P::Unknown if w.restore.is_empty() => P::Adopted,
            _ => {
                self.forget_incarnation(member, anchor, P::Block);
                return;
            }
        };
        if let Some(w) = self.delivery.accepted.get_mut(member) {
            w.restore = anchor.to_string();
            w.proof = next;
            self.delivery.accepted_dirty = true;
        }
    }

    /// At open: an unkeyed window of a restored seat is keyed to the chain's
    /// latest restore of it, never reset - the evidence rule finds the ones
    /// that predate it; a first-contact window is judged by its birth.
    pub(crate) fn reconcile_restore_windows(&mut self) {
        let me = self.member();
        let mut anchors: Vec<(MemberId, String)> =
            self.chain.anchors.iter().filter(|(m, _)| **m != me).map(|(m, a)| (m.clone(), a.clone())).collect();
        anchors.sort();
        for (member, anchor) in anchors {
            let Some((proof, unkeyed)) = self.delivery.accepted.get(&member).map(|w| (w.proof, w.restore.is_empty())) else {
                continue;
            };
            match proof {
                // a first contact the block never reached: judged as the block would
                molt_core::RestoreProof::Pending => self.reset_for_restore(&member, &anchor),
                molt_core::RestoreProof::Unknown if unkeyed => {
                    if let Some(w) = self.delivery.accepted.get_mut(&member) {
                        w.restore = anchor;
                    }
                    self.delivery.accepted_dirty = true;
                }
                _ => {}
            }
        }
    }

    /// The seal stamp of `member`'s latest `Restored` block (0 = unknown).
    fn restore_sealed_at(&self, member: &str) -> u64 {
        self.chain
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(&b.change, molt_core::ChainChange::Membership { op: molt_core::MembershipOp::Restored, member: m, .. } if m == member))
            .and_then(|b| self.chain.block_ts.get(&b.height).copied())
            .unwrap_or(0)
    }

    /// Whether a dropped duplicate from `from` stamped `ts` proves its window
    /// predates the seat's latest restore: the frame was written after the
    /// restore and after everything the window accepted, so its mark can
    /// only be the old incarnation's. With no accepted stamp known, the
    /// open time plus the skew bound stands in for it.
    fn dup_proves_stale(&self, from: &str, seq: u64, ts: u64) -> Option<String> {
        if self.drop_seam.keeps_window_on_block() {
            return None;
        }
        let anchor = self.chain.anchors.get(from)?;
        let w = self.delivery.accepted.get(from)?;
        let proven = matches!(
            w.proof,
            molt_core::RestoreProof::Block | molt_core::RestoreProof::Merge | molt_core::RestoreProof::Evidence | molt_core::RestoreProof::Operator
        );
        if w.restore == *anchor && proven {
            return None;
        }
        let newer = if w.stamp > 0 {
            (ts, seq) > (w.stamp, w.stamp_seq)
        } else if self.delivery.opened_at > 0 {
            ts > self.delivery.opened_at.saturating_add(self.drop_seam.evidence_skew())
        } else {
            return None;
        };
        (newer && ts > self.restore_sealed_at(from)).then(|| anchor.clone())
    }

    /// The operator resets `member`'s inbound state, as the evidence rule
    /// does; the gap asks and the sender's rewind then re-deliver its
    /// retained history. A second call while that runs changes nothing.
    pub(crate) fn cmd_resync_seat(&mut self, member: MemberId) -> Result<Reply, MoltError> {
        if self.active.is_none() && self.replica.is_none() {
            return Err(MoltError::Engine("no open republic".to_string()));
        }
        if member == self.member() {
            return Err(MoltError::Engine("cannot resync the own seat".to_string()));
        }
        if !self.roster().contains(&member) {
            return Err(MoltError::Engine(format!("{member} is not a seat")));
        }
        if self.resync_running(&member) {
            return Err(MoltError::Engine(format!("resync of {member} running")));
        }
        let anchor = self.chain.anchors.get(&member).cloned().unwrap_or_default();
        self.forget_incarnation(&member, &anchor, molt_core::RestoreProof::Operator);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// An operator resync still filling: nothing accepted yet, or a gap
    /// still asked for.
    fn resync_running(&self, member: &str) -> bool {
        let Some(w) = self.delivery.accepted.get(member) else { return false };
        w.proof == molt_core::RestoreProof::Operator
            && (w.high == 0
                || self
                    .delivery
                    .inbound
                    .get(member)
                    .is_some_and(|h| !h.held.is_empty() || !h.missing.is_empty() || h.listing.is_some()))
    }

    /// `member`'s current drop episode size once it warned, else 0.
    pub(crate) fn dup_streak(&self, member: &str) -> u32 {
        self.delivery.dups.get(member).filter(|e| e.warned).map_or(0, |e| e.count)
    }

    /// A dropped duplicate. `true` = it proved the window stale: the window
    /// was reset and the caller delivers the frame again.
    pub(super) fn note_dup(&mut self, from: &MemberId, seq: u64, ts: u64) -> bool {
        if let Some(anchor) = self.dup_proves_stale(from, seq, ts) {
            self.forget_incarnation(from, &anchor, molt_core::RestoreProof::Evidence);
            return true;
        }
        let (stamp, stamp_seq) = self.delivery.accepted.get(from).map_or((0, 0), |w| (w.stamp, w.stamp_seq));
        let e = self.delivery.dups.entry(from.clone()).or_default();
        if stamp == 0 || (ts, seq) <= (stamp, stamp_seq) || (e.count > 0 && seq <= e.last_seq) {
            return false;
        }
        if e.count == 0 {
            e.first_seq = seq;
        }
        e.count = e.count.saturating_add(1);
        e.last_seq = seq;
        e.clean = 0;
        if e.count == DUP_EPISODE_MIN && !e.warned {
            e.warned = true;
            tracing::warn!(%from, first_seq = e.first_seq, last_seq = seq, count = e.count, "dup_streak");
        }
        false
    }

    pub(super) fn note_fresh(&mut self, from: &MemberId, seq: u64, ts: u64) {
        if let Some(w) = self.delivery.accepted.get_mut(from) {
            if w.born == 0 {
                w.born = ts;
            }
            // a window not proven to postdate the seat's restore keeps the
            // stamp it had: a frame of the new device accepted out of order
            // must not lift the evidence bar over the ones it dropped
            let unproven = w.proof == molt_core::RestoreProof::Adopted
                || (w.proof == molt_core::RestoreProof::Unknown && !w.restore.is_empty());
            if !unproven && (ts, seq) > (w.stamp, w.stamp_seq) {
                w.stamp = ts;
                w.stamp_seq = seq;
            }
        }
        let Some(e) = self.delivery.dups.get_mut(from) else { return };
        e.clean = e.clean.saturating_add(1);
        if e.clean >= DUP_EPISODE_MIN {
            if e.warned {
                tracing::info!(%from, count = e.count, "dup_streak_cleared");
            }
            self.delivery.dups.remove(from);
        }
    }

    /// Mark `seq` from `from` as engine-accepted (delivery guarantee §4.2 —
    /// the accept point's bookkeeping). `false` = the sender's window already
    /// held it: the envelope is a duplicate/resend and the caller drops it
    /// (G2). Freshness dirties the window for the debounced persist.
    pub(crate) fn accept_envelope(&mut self, from: &MemberId, seq: u64) -> bool {
        // first contact: the chain may not have caught up with this seat's
        // restores yet, so the next one applied adopts the window
        let window = self
            .delivery
            .accepted
            .entry(from.clone())
            .or_insert_with(|| molt_core::AcceptedWindow { proof: molt_core::RestoreProof::Pending, ..Default::default() });
        let fresh = window.accept(seq);
        if fresh {
            self.delivery.accepted_dirty = true;
        }
        fresh
    }

    /// Debounced MLS-ratchet persist (delivery guarantee §4.6 / E6, the
    /// "per-drain persist" hardening in its pragmatic form): every
    /// [`MLS_PERSIST_SECS`] with mesh traffic since the last snapshot, merge
    /// the CURRENT ratchet into `transport.state`. A hard kill then regresses
    /// the ratchet by seconds, not by everything since mesh-up — so the
    /// reopen's rewind-resend re-encrypts a step or two past what the peers
    /// consumed instead of replaying a whole session into replay rejection.
    pub(crate) fn persist_mls_if_due(&mut self, now: u64) {
        if now.saturating_sub(self.delivery.mls_persisted_at) < MLS_PERSIST_SECS {
            return;
        }
        // A Nostr workspace has no `NetRuntime`, but it very much has a live
        // ratchet: the group runtime advances it on every publish. Without
        // this the reopen restores the founding blob and the next publish
        // REUSES sender generations — which every peer replay-rejects and
        // silently drops.
        if let Some(group) = self.group_net.as_ref() {
            let snap = group.mls.lock().ok().and_then(|g| g.snapshot().ok());
            if let (Some(snap), Some(active)) = (snap, self.active.as_ref()) {
                if active.handle.merge_mls_async(snap) {
                    self.delivery.mls_persisted_at = now;
                }
            }
            return;
        }
        let Some(net) = self.net.as_ref() else { return };
        if !net.is_real() {
            return;
        }
        // only when the ratchet could have moved: something went out, or
        // something was heard (receive ratchets advance on decrypt)
        let heard = self
            .roster()
            .iter()
            .filter(|m| **m != self.member())
            .map(|m| self.member_last_seen(m))
            .max()
            .unwrap_or(0);
        if self.delivery.last_mesh_out < self.delivery.mls_persisted_at && heard < self.delivery.mls_persisted_at {
            return;
        }
        let Some((Some(mls), _creds)) = net.crypto_for_close() else {
            return;
        };
        if let Some(active) = self.active.as_ref() {
            // only a really-enqueued merge advances the debounce stamp — a
            // dropped one (writer backpressure) retries on the next beat
            if active.handle.merge_mls_async(mls) {
                tracing::debug!("live MLS ratchet persist (debounced)");
                self.delivery.mls_persisted_at = now;
            }
        }
    }

    /// Debounced accept-window persist (rides the presence tick): at most one
    /// `transport.state` merge per [`ACCEPT_SAVE_SECS`], only when dirty.
    /// Fire-and-forget like the supervisor's cursor saves — a lost save only
    /// regresses the window, which resends + re-dedup absorb (§4.7).
    pub(crate) fn save_accepted_if_due(&mut self, now: u64) {
        if !self.delivery.accepted_dirty
            || now.saturating_sub(self.delivery.accepted_saved_at) < ACCEPT_SAVE_SECS
        {
            return;
        }
        if let Some(active) = self.active.as_ref() {
            active.handle.save_accepted(self.delivery.accepted.clone());
            self.delivery.accepted_dirty = false;
            self.delivery.accepted_saved_at = now;
        }
    }

    pub(crate) fn cmd_net_delivered(
        &mut self,
        from: MemberId,
        envelope: EventEnvelope,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            tracing::debug!(%from, "dropping a delivery from a torn-down mesh");
            return Ok(Reply::Ack);
        }
        if self.drop_seam.drops(&from, envelope.seq) || self.drop_seam.drops_chat(&envelope.body) {
            return Ok(Reply::Ack);
        }
        let known = match &self.net {
            Some(net) => net.peer_names.contains(&from),
            None => self.roster().contains(&from),
        };
        if !known || from == self.member() {
            tracing::warn!(%from, "dropping a delivery from an unknown or impersonated member");
            return Ok(Reply::Ack);
        }
        if envelope.by != from {
            tracing::warn!(%from, claimed = %envelope.by, "dropping a delivery whose author does not match its link");
            return Ok(Reply::Ack);
        }
        // G7 in-order hold (§10). A sender we hold NO accepted history
        // with cannot be ordered against that history (N4b §3.1a: a
        // rejoiner enters mid-stream): its first envelope is the baseline.
        let window = self.delivery.accepted.get(&from);
        // a window reset for a restore owes the new incarnation from its start
        let from_start = window.is_some_and(|w| w.high == 0 && w.proof.is_reset());
        let has_history = window.is_some_and(|w| w.high > 0) || from_start;
        let is_dup = window.is_some_and(|w| w.is_accepted(envelope.seq));
        let prev_missing = envelope.prev_seq != 0
            && has_history
            && !window.is_some_and(|w| w.is_accepted(envelope.prev_seq));
        let start = window.map_or(0, |w| if w.frontier > 0 { w.frontier } else { w.high });
        let links = window.map(|w| w.links.clone()).unwrap_or_default();
        if !has_history {
            return self.deliver_gated(from, envelope);
        }
        let now = self.presence_now();
        let hold = self.delivery.inbound.entry(from.clone()).or_insert_with(|| SenderHold {
            from_start,
            ..SenderHold::new(start, links)
        });
        if is_dup || hold.held.contains_key(&envelope.seq) {
            hold.passed(envelope.seq, now);
            if is_dup {
                return self.deliver_gated(from, envelope);
            }
            self.delivery.ack_due.entry(from).or_insert(now + ACK_DEBOUNCE_SECS);
            return Ok(Reply::Ack);
        }
        hold.missing.remove(&envelope.seq);
        hold.session.insert(envelope.seq);
        // a sender naming endless far-ahead seqs keeps only its lowest
        while hold.session.len() > SESSION_MAX {
            hold.session.pop_last();
        }
        if prev_missing {
            hold.note_missing(envelope.prev_seq, now);
        }
        // `prev_seq == 0`: a pre-G7 sender or a chain start, unordered as ever
        let wait = envelope.seq > hold.cap()
            || (envelope.prev_seq != 0 && order_sensitive(&envelope.body) && hold.blocks(envelope.seq));
        if wait {
            self.hold_envelope(&from, envelope, now);
            return Ok(Reply::Ack);
        }
        let reply = self.deliver_tracked(&from, envelope);
        self.apply_ready(&from, "arrived");
        reply
    }

    /// Keep a decrypted envelope unaccepted until it may be applied; past
    /// [`HOLD_BYTES_MAX`] shed the highest seq of the largest buffer.
    fn hold_envelope(&mut self, from: &MemberId, env: EventEnvelope, now: u64) {
        let size = serde_json::to_vec(&env).map_or(1024, |b| b.len());
        if let Some(hold) = self.delivery.inbound.get_mut(from) {
            hold.since.get_or_insert(now);
            hold.bytes += size;
            hold.held.insert(env.seq, (env, size));
        }
        self.delivery.ack_due.entry(from.clone()).or_insert(now + ACK_DEBOUNCE_SECS);
        let max = self.delivery.hold_bytes_cap.unwrap_or(HOLD_BYTES_MAX);
        while self.delivery.inbound.values().map(|h| h.bytes).sum::<usize>() > max {
            let Some((member, hold)) = self.delivery.inbound.iter_mut().max_by_key(|(_, h)| h.bytes) else { break };
            let Some((seq, (_, size))) = hold.held.pop_last() else { break };
            hold.bytes = hold.bytes.saturating_sub(size);
            // owed again: what it held back keeps waiting for it
            hold.note_missing(seq, now);
            if hold.shed == 0 {
                tracing::warn!(member = %member, seq, held = hold.held.len(), "inbound_hold_shed");
            }
            hold.shed = hold.shed.saturating_add(1);
        }
    }

    /// [`Self::deliver_gated`], then chain the seq onto the frontier; one
    /// left unaccepted (the D7 decline park) stays missing and refused.
    fn deliver_tracked(&mut self, from: &MemberId, env: EventEnvelope) -> Result<Reply, MoltError> {
        let (seq, prev) = (env.seq, env.prev_seq);
        // D-F3: an unowed frame shares its prev with the next owed envelope;
        // linking it would take that envelope's link and stall the frontier
        let unowed = molt_core::unowed_on_the_wire(&env.body);
        let reply = self.deliver_gated(from.clone(), env);
        let accepted = self.delivery.accepted.get(from).is_some_and(|w| w.is_accepted(seq));
        let now = self.presence_now();
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return reply };
        if accepted {
            hold.refused.remove(&seq);
            if !unowed {
                hold.link(prev, seq);
            }
            self.store_frontier(from);
        } else {
            hold.refused.insert(seq);
            hold.note_missing(seq, now);
        }
        reply
    }

    /// Persist the frontier and the links above it with the window they
    /// guard.
    fn store_frontier(&mut self, from: &MemberId) {
        let Some((f, links)) = self
            .delivery
            .inbound
            .get(from)
            .map(|h| (h.frontier, h.ahead.iter().map(|(p, s)| (*p, *s)).collect::<Vec<_>>()))
        else {
            return;
        };
        if let Some(w) = self.delivery.accepted.get_mut(from) {
            if w.frontier != f || w.links != links {
                w.frontier = f;
                w.links = links;
                self.delivery.accepted_dirty = true;
            }
        }
    }

    /// Apply, in seq order, every held envelope from `from` that may be
    /// applied now: at or below the cap, and - if order-sensitive - with no
    /// missing seq below it. Close the episode once nothing is held.
    fn apply_ready(&mut self, from: &MemberId, via: &str) {
        loop {
            let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
            let cap = hold.cap();
            let first_missing = hold.missing.keys().next().copied().unwrap_or(u64::MAX);
            let open = cap.min(first_missing.saturating_sub(1));
            let next = match hold.held.range(..=open).next() {
                Some((&s, _)) => Some(s),
                // past a missing seq only what needs no order
                None => hold
                    .held
                    .range(..=cap)
                    .find(|(_, (e, _))| e.prev_seq == 0 || !order_sensitive(&e.body))
                    .map(|(s, _)| *s),
            };
            let Some(seq) = next else { break };
            let Some((env, size)) = hold.held.remove(&seq) else { break };
            hold.bytes = hold.bytes.saturating_sub(size);
            let _ = self.deliver_tracked(from, env);
        }
        let now = self.presence_now();
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        if !hold.held.is_empty() {
            return;
        }
        if let Some(since) = hold.since.take() {
            if hold.logged || hold.unordered > 0 || hold.shed > 0 || hold.lost > 0 {
                tracing::info!(
                    member = %from,
                    since_s = now.saturating_sub(since),
                    via,
                    unordered = hold.unordered,
                    shed = hold.shed,
                    lost = hold.lost,
                    "inbound_held_cleared"
                );
            }
        }
        hold.logged = false;
        hold.unordered = 0;
        hold.shed = 0;
        hold.lost = 0;
    }

    /// Order only: `seq` of `from`'s stream no longer holds what is behind
    /// it, then apply.
    fn settle_gap(&mut self, from: &MemberId, seq: u64, via: &str) {
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        if hold.settle(seq) {
            if hold.held.range(seq..).next().is_some() {
                hold.unordered = hold.unordered.saturating_add(1);
            }
            if via == "fallback" {
                tracing::warn!(member = %from, missing_seq = seq, held = hold.held.len(), via, "inbound_gap_skipped");
            } else {
                tracing::info!(member = %from, missing_seq = seq, held = hold.held.len(), via, "inbound_gap_skipped");
            }
        }
        self.apply_ready(from, via);
    }

    /// Give up every owed seq of `from` at or below `to` that never came,
    /// loudly, and raise the frontier there.
    fn give_up_through(&mut self, from: &MemberId, to: u64, via: &str) {
        let window = self.delivery.accepted.get(from).cloned().unwrap_or_default();
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        if to <= hold.frontier {
            return;
        }
        let gone: Vec<u64> = hold
            .missing
            .range(..=to)
            .map(|(s, _)| *s)
            .chain(hold.listing.iter().flat_map(|l| l.own.range(..=to).copied()))
            .filter(|s| !window.is_accepted(*s) && !hold.held.contains_key(s))
            .collect::<std::collections::BTreeSet<u64>>()
            .into_iter()
            .collect();
        if !gone.is_empty() {
            let n = u32::try_from(gone.len()).unwrap_or(u32::MAX);
            hold.lost = hold.lost.saturating_add(n);
            tracing::warn!(member = %from, count = gone.len(), first = gone[0], via, "inbound_gap_lost");
        }
        hold.raise(to);
        self.store_frontier(from);
        self.apply_ready(from, via);
    }

    /// Where a sender's listing lets the frontier go: up to the first
    /// listed seq that is neither accepted nor held.
    fn listing_reach(&self, from: &MemberId) -> Option<u64> {
        let hold = self.delivery.inbound.get(from)?;
        let listing = hold.listing.as_ref()?;
        let window = self.delivery.accepted.get(from)?;
        let open = listing
            .own
            .iter()
            .find(|s| !window.is_accepted(**s) && !hold.held.contains_key(s))
            .copied();
        Some(open.map_or(listing.through, |s| s - 1))
    }

    /// G7 beat (§10): advance frontiers on listings and resend evidence,
    /// settle ordering gaps with evidence or past the bound, ask for the
    /// rest - named seqs, and the listing of what lies above the frontier.
    pub(crate) fn inbound_gap_tick(&mut self, now: u64) {
        let me = self.member();
        // a stored replay still coming in fills its gaps itself, newest
        // first: ask only once every relay's replay ended and was read
        let replaying = self.group_net.as_ref().is_some_and(|g| g.health.borrow().replaying);
        let quiet_since = if replaying {
            self.delivery.replay_quiet_since = None;
            now
        } else {
            *self.delivery.replay_quiet_since.get_or_insert(now)
        };
        let replaying = replaying || now.saturating_sub(quiet_since) < GAP_ASK_AFTER_SECS;
        let members: Vec<MemberId> = self.delivery.inbound.keys().cloned().collect();
        for m in members {
            // a listing complete up to some seq: raise there
            if let Some(reach) = self.listing_reach(&m) {
                if let Some(hold) = self.delivery.inbound.get_mut(&m) {
                    if reach > hold.frontier {
                        hold.raise(reach);
                        self.store_frontier(&m);
                        self.apply_ready(&m, "listed");
                    }
                }
            }
            let window = self.delivery.accepted.get(&m).cloned().unwrap_or_default();
            let Some(hold) = self.delivery.inbound.get_mut(&m) else { continue };
            // an older sender's round passed: below its top, what did not
            // come will not; the frontier may go as far as the held ones
            let mut jump = None;
            if let Some(at) = hold.passed_at {
                if now >= at + GAP_PASS_GRACE_SECS {
                    if !hold.answers {
                        let lowest_held = hold.held.keys().next().map_or(u64::MAX, |s| s - 1);
                        let lowest_refused = hold.refused.iter().next().map_or(u64::MAX, |s| s - 1);
                        // a round starts at the sender's floor and goes up:
                        // only below its lowest re-offer is a seq proven
                        // outside it
                        let below_round = hold.passed_low.saturating_sub(1);
                        jump = Some(hold.cap().min(below_round).min(lowest_held).min(lowest_refused));
                    }
                    hold.passed_at = None;
                }
            }
            // a reset window of a sender that answers no want (an older
            // build) cannot be filled from its start: past the fallback
            // what is held becomes the baseline, loudly
            let baseline = if hold.from_start && !hold.answers {
                hold.since
                    .filter(|s| now.saturating_sub(*s) >= GAP_FALLBACK_SECS)
                    .and(hold.held.keys().next().map(|s| s - 1))
            } else {
                None
            };
            // listed seqs whose re-publication did not land count a try
            let mut given_up = Vec::new();
            if let Some(l) = hold.listing.as_mut() {
                for (&seq, gap) in hold.missing.iter_mut() {
                    if l.own.contains(&seq) && gap.settle_at.is_some_and(|t| now >= t) && !window.is_accepted(seq) {
                        gap.settle_at = None;
                        let tries = l.tries.entry(seq).or_insert(0);
                        *tries += 1;
                        if *tries >= LISTED_TRIES {
                            given_up.push(seq);
                        }
                    }
                }
            }
            let filled: Vec<u64> = hold.missing.keys().copied().filter(|s| window.is_accepted(*s)).collect();
            for s in filled {
                hold.missing.remove(&s);
            }
            let mut settle: Vec<(u64, &str)> = Vec::new();
            let mut ask: Vec<u64> = Vec::new();
            for (&seq, gap) in &hold.missing {
                let refused = hold.refused.contains(&seq);
                let listed = hold.listing.as_ref().is_some_and(|l| l.own.contains(&seq));
                if !refused && !listed && gap.settle_at.is_some_and(|t| now >= t) {
                    settle.push((seq, "resend_passed"));
                } else if now.saturating_sub(gap.since) >= GAP_FALLBACK_SECS {
                    settle.push((seq, "fallback"));
                } else if !refused
                    && now.saturating_sub(gap.since) >= GAP_ASK_AFTER_SECS
                    && gap.asked_at.map_or(true, |t| now.saturating_sub(t) >= GAP_ASK_EVERY_SECS)
                {
                    ask.push(seq);
                }
            }
            if !hold.held.is_empty() && !hold.logged {
                if let Some(since) = hold.since {
                    if now.saturating_sub(since) >= GAP_ASK_AFTER_SECS {
                        hold.logged = true;
                        let missing_seq = hold.missing.keys().next().copied().unwrap_or(hold.frontier + 1);
                        tracing::info!(member = %m, missing_seq, held = hold.held.len(), frontier = hold.frontier, since_s = now - since, "inbound_held");
                    }
                }
            }
            // anything known above the frontier: ask for the listing of it
            let above = !hold.ahead.is_empty() || !hold.held.is_empty() || !hold.missing.is_empty();
            let list_due = above
                && hold.listing.is_none()
                && hold.listing_asked_at.map_or(true, |t| now.saturating_sub(t) >= GAP_ASK_EVERY_SECS)
                && hold.since.map_or(true, |s| now.saturating_sub(s) >= GAP_ASK_AFTER_SECS);
            ask.truncate(molt_net::gap::GAP_MAX_SEQS);
            if let Some(group) = self.group_net.as_ref().filter(|_| !replaying) {
                if list_due {
                    let want = molt_net::gap::GapWant::range(me.clone(), m.clone(), hold.frontier, hold.cap());
                    if group.handle.publish_control(want.to_frame()) {
                        hold.listing_asked_at = Some(now);
                    }
                }
                if !ask.is_empty() {
                    let want = molt_net::gap::GapWant::new(me.clone(), m.clone(), ask.clone());
                    if group.handle.publish_control(want.to_frame()) {
                        tracing::debug!(member = %m, seqs = ?ask, "gap_want");
                        for s in &ask {
                            if let Some(g) = hold.missing.get_mut(s) {
                                g.asked_at = Some(now);
                            }
                        }
                    }
                }
            }
            for seq in given_up {
                if let Some(hold) = self.delivery.inbound.get_mut(&m) {
                    if let Some(l) = hold.listing.as_mut() {
                        l.own.remove(&seq);
                    }
                    hold.missing.remove(&seq);
                    hold.lost = hold.lost.saturating_add(1);
                }
                tracing::warn!(member = %m, count = 1, first = seq, via = "resent_absent", "inbound_gap_lost");
            }
            if let Some(to) = jump {
                self.give_up_through(&m, to, "resend_passed");
            }
            if let Some(to) = baseline {
                if let Some(hold) = self.delivery.inbound.get_mut(&m) {
                    hold.from_start = false;
                }
                self.give_up_through(&m, to, "baseline");
            }
            for (seq, via) in settle {
                self.settle_gap(&m, seq, via);
            }
            self.apply_ready(&m, "filled");
        }
    }

    /// A gap want reached this node: only the named sender answers, off
    /// the actor (§10).
    pub(crate) fn cmd_net_gap_wanted(
        &mut self,
        from: &MemberId,
        of: &MemberId,
        seqs: Vec<u64>,
        range: Option<(u64, u64)>,
    ) {
        if !self.gap_want_is_ours(from, of) {
            return;
        }
        if let Some(group) = self.group_net.as_ref() {
            group.handle.answer_gap(from.clone(), seqs, range);
        }
    }

    /// Only a roster member's want, and only for this node's own stream.
    pub(crate) fn gap_want_is_ours(&self, from: &MemberId, of: &MemberId) -> bool {
        *of == self.member() && *from != self.member() && self.roster().contains(from)
    }

    /// The sender answered our gap want: `gone` never comes, `resent` must
    /// land within the skew grace.
    pub(crate) fn cmd_net_gap_answered(&mut self, from: MemberId, to: MemberId, resent: Vec<u64>, gone: Vec<u64>) {
        if to != self.member() {
            return;
        }
        let now = self.presence_now();
        let Some(hold) = self.delivery.inbound.get_mut(&from) else { return };
        hold.answers = true;
        hold.passed_at = None;
        for s in &resent {
            if let Some(g) = hold.missing.get_mut(s) {
                g.settle_at.get_or_insert(now + GAP_PASS_GRACE_SECS);
            }
        }
        for s in gone {
            if let Some(l) = self.delivery.inbound.get_mut(&from).and_then(|h| h.listing.as_mut()) {
                l.own.remove(&s);
            }
            self.settle_gap(&from, s, "gone");
        }
    }

    /// The sender listed its own seqs in `(after, through]`: what is not
    /// listed is not owed, what is listed and not here is asked for.
    pub(crate) fn cmd_net_gap_listed(&mut self, from: &MemberId, to: &MemberId, after: u64, through: u64, own: Vec<u64>) {
        if *to != self.member() {
            return;
        }
        let now = self.presence_now();
        let window = self.delivery.accepted.get(from).cloned().unwrap_or_default();
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        hold.answers = true;
        hold.passed_at = None;
        if after > hold.frontier || through <= hold.frontier {
            return;
        }
        let own: std::collections::BTreeSet<u64> = own.into_iter().filter(|s| *s > hold.frontier && *s <= through).collect();
        let unlisted: Vec<u64> = hold.missing.range(..=through).map(|(s, _)| *s).filter(|s| !own.contains(s)).collect();
        for s in &own {
            if !window.is_accepted(*s) && !hold.refused.contains(s) {
                hold.note_missing(*s, now);
            }
        }
        hold.listing = Some(Listing { through, own, tries: std::collections::BTreeMap::new() });
        for s in unlisted {
            self.settle_gap(from, s, "gone");
        }
        if let Some(reach) = self.listing_reach(from) {
            if let Some(hold) = self.delivery.inbound.get_mut(from) {
                if reach > hold.frontier {
                    hold.raise(reach);
                    self.store_frontier(from);
                }
            }
        }
        self.apply_ready(from, "listed");
    }

    #[cfg(test)]
    pub(crate) fn inbound_held_has(&self, member: &str, seq: u64) -> bool {
        self.delivery.inbound.get(member).is_some_and(|h| h.held.contains_key(&seq))
    }

    /// Whether `member`'s inbound stream is held behind a gap here.
    pub(crate) fn inbound_held_from(&self, member: &str) -> bool {
        self.delivery.inbound.get(member).is_some_and(|h| !h.held.is_empty() || !h.missing.is_empty())
    }

    /// The held streams, for the status view.
    pub(crate) fn inbound_held_view(&self) -> Vec<molt_core::InboundHeld> {
        self.delivery
            .inbound
            .iter()
            .filter(|(_, h)| !h.held.is_empty() || !h.missing.is_empty() || !h.ahead.is_empty())
            .map(|(m, h)| molt_core::InboundHeld {
                member: m.clone(),
                missing_seq: h.missing.keys().next().copied().unwrap_or(h.frontier + 1),
                held: u32::try_from(h.held.len()).unwrap_or(u32::MAX),
                since: h.since.unwrap_or(0),
                frontier: h.frontier,
                bytes: u64::try_from(h.bytes).unwrap_or(u64::MAX),
                missing: u32::try_from(h.missing.len()).unwrap_or(u32::MAX),
            })
            .collect()
    }

    /// Publish ONE claim sheet covering every member we owe an ack.
    ///
    /// The mesh emits per leg and skips a member it has no link to; on a
    /// broadcast that member is precisely the one the ack must still reach, so
    /// the per-peer loop is deleted rather than ported.
    ///
    /// The sheet is FULL STATE, so an unchanged one is not republished: on a
    /// quiet republic the steady state is zero frames.
    fn flush_group_ack(&mut self, due: &[MemberId]) {
        for m in due {
            self.delivery.ack_due.remove(m);
        }
        // every subject we owe, plus everyone the last sheet already spoke
        // about — dropping a subject from the sheet would read as "no longer
        // accepting", and the receiver would keep resending what we hold
        let mut claims: std::collections::BTreeMap<MemberId, molt_core::AcceptedWindow> =
            std::collections::BTreeMap::new();
        let subjects: Vec<MemberId> = due
            .iter()
            .cloned()
            .chain(
                self.delivery.last_group_ack
                    .as_ref()
                    .map(|a| a.claims.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
            .collect();
        for m in subjects {
            if let Some(win) = self.delivery.accepted.get(&m) {
                claims.insert(m, win.for_sheet());
            }
        }
        if claims.is_empty() {
            return;
        }
        let ack = molt_net::group_ack::GroupAck::new(self.member(), claims);
        if same_sheet(self.delivery.last_group_ack.as_ref(), &ack) {
            return; // nothing changed — say nothing
        }
        if let Some(group) = self.group_net.as_ref() {
            group.handle.publish_ack(ack.clone());
            self.delivery.last_group_ack = Some(ack);
        }
    }

    /// The delivery-guarantee beat (1 s ticker, E7 review): flush the due
    /// delivery ACKs and run the debounced accept-window / live-ratchet
    /// persists. Its own fast tick — riding the 30 s presence tick alone
    /// stretched the 3 s ack debounce into a 33 s latency that lost the race
    /// against the sender's 30 s resend timer, and widened the accept-window
    /// crash-regression from seconds to half a minute.
    pub(crate) fn cmd_net_delivery_tick(&mut self) -> Result<Reply, MoltError> {
        let now = self.presence_now();
        if let Some((member, seq)) = self.drop_seam.take_regress() {
            self.regress_window(&member, seq, now);
        }
        self.save_accepted_if_due(now);
        self.persist_mls_if_due(now);
        self.flush_due_acks(now);
        // a peer falling silent has no event of its own: this beat carries it
        self.push_outbox_reach();
        // G7: settle or ask for the gaps a held stream waits on
        self.inbound_gap_tick(now);
        self.release_decision_lines(now);
        // the mirror worker's planning beat (mirroring §3.3; gated inside)
        self.mirror_worker_tick(now);
        self.download_tick(crate::now_secs());
        self.mirror_decl_tick(now);
        self.wiki_base_tick();
        self.vault_payload_tick();
        self.vault_base_tick();
        self.retry_settled_restore_approvals();
        // A2.2: held seals land once their round has passed
        self.drain_held_seals();
        self.lock_tick();
        self.chain_heal_tick();
        self.sign_held_approvals();
        self.wake_tick();
        self.wallet_run_tick(now);
        Ok(Reply::Ack)
    }

    /// Send every delivery ACK that has come due (§4.3): one control frame
    /// per owed sender, carrying this node's accept window over THAT
    /// sender's events. Best-effort off the actor (the send_ping path); a
    /// lost ack is re-armed by the sender's next resend arriving as a dup.
    /// Test seam body: keep only `member`'s accept marks at or below `seq`.
    fn regress_window(&mut self, member: &MemberId, seq: u64, now: u64) {
        let Some(old) = self.delivery.accepted.get(member).cloned() else { return };
        let mut w = molt_core::AcceptedWindow { restore: old.restore.clone(), proof: old.proof, ..Default::default() };
        for s in seq.saturating_sub(molt_core::ACCEPT_WINDOW_BITS)..=seq.min(old.high) {
            if s > 0 && old.is_accepted(s) {
                let _ = w.accept(s);
            }
        }
        w.frontier = old.frontier.min(w.high);
        w.links = old.links.iter().copied().filter(|(_, s)| *s <= w.high).collect();
        self.delivery.accepted.insert(member.clone(), w);
        self.delivery.inbound.remove(member);
        self.delivery.accepted_dirty = true;
        self.delivery.ack_due.insert(member.clone(), now);
    }

    pub(crate) fn flush_due_acks(&mut self, now: u64) {
        if self.delivery.ack_due.is_empty() {
            return;
        }
        let due: Vec<MemberId> = self
            .delivery.ack_due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(m, _)| m.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        // N5.3: a Nostr workspace acks over the group channel — ONE 445 for
        // the whole republic. Read FIRST, because everything below is a
        // queue-mesh concept and the mesh branch would otherwise drop these
        // deadlines on the floor, which is how the guarantee was silently
        // 100% off over broadcast.
        if self.group_net.is_some() {
            self.flush_group_ack(&due);
            return;
        }
        let (transport, group, peers) = {
            let Some(net) = self.net.as_ref() else {
                // no live mesh to ack over — drop the deadlines (the sender's
                // resend after the mesh returns re-arms them)
                for m in &due {
                    self.delivery.ack_due.remove(m);
                }
                return;
            };
            if !net.is_real() {
                for m in &due {
                    self.delivery.ack_due.remove(m);
                }
                return;
            }
            let (Some(transport), Some(group)) = (net.runtime_transport(), net.group_arc())
            else {
                return;
            };
            let peers: Vec<PeerLink> =
                net.mesh().iter().filter_map(PeerLink::from_mesh).collect();
            (transport, group, peers)
        };
        for member in due {
            self.delivery.ack_due.remove(&member);
            let Some(window) = self.delivery.accepted.get(&member) else {
                continue; // nothing ever accepted — nothing to report
            };
            let Some(peer) = peers.iter().find(|p| p.member == member) else {
                continue; // no leg to this member right now
            };
            let Ok(mut payload) = serde_json::to_vec(&window.for_sheet()) else {
                continue;
            };
            let mut frame = molt_net::MESH_ACK_TAG.to_vec();
            frame.append(&mut payload);
            tokio::spawn(Self::send_ping(
                transport.clone(),
                group.clone(),
                peer.clone(),
                frame,
            ));
        }
    }

    /// Encrypt one control `payload` on the SHARED group and send it onto
    /// `peer`'s inbound queue (best-effort). The single send path for the
    /// control frames — today [`molt_net::MESH_ACK_TAG`]`‖window` (the
    /// delivery guarantee's ACK, §4.3).
    pub(crate) async fn send_ping(
        transport: molt_net::LoopbackTransport,
        group: Arc<Mutex<molt_net::MlsMember>>,
        peer: PeerLink,
        tag: Vec<u8>,
    ) {
        // one ratchet advance per ping, on the shared group (same Arc the
        // supervisor uses — locked in sequence)
        let Some(ct) = group.lock().ok().and_then(|mut g| g.encrypt(&tag).ok()) else {
            tracing::debug!(member = %peer.member, "encrypting the mesh ping failed");
            return;
        };
        // a fresh random id per ping: the receiver's reassembler dedups by
        // message id, so a reused id would be dropped before it could stamp
        // liveness (and a random 16 bytes never collides with the outbox's
        // derived ids)
        let mut idb = [0u8; 16];
        if getrandom::getrandom(&mut idb).is_err() {
            return;
        }
        if let Err(e) =
            supervisor::send_framed(&transport, peer.snd0(), &peer.wrap_out, molt_net::MsgId(idb), &ct)
                .await
        {
            tracing::debug!(member = %peer.member, error = %e, "mesh ping send failed");
        }
    }
}
