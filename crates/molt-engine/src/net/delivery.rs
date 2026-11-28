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

/// The open probe (§13): one switch, independent of the `behind` signal.
pub(crate) const OPEN_PROBE: bool = false;

/// The probe's re-ask interval grows from [`GAP_ASK_EVERY_SECS`] to this.
const PROBE_EVERY_MAX_SECS: u64 = 600;

/// A stream counts as behind once its hold or gap is this old.
const BEHIND_AFTER_SECS: u64 = 10;

/// Repeats from one seat that make a hint.
const DUP_EPISODE_MIN: u32 = 32;

/// A hold warns first after this long, then at most once per
/// [`HOLD_WARN_EVERY_SECS`] - one INFO line followed by silence hid the
/// 2026-11-27 deadlock for four hours.
pub(crate) const HOLD_WARN_AFTER_SECS: u64 = 120;

/// The floor of the gap between two hold warnings.
pub(crate) const HOLD_WARN_EVERY_SECS: u64 = 600;

/// One seat's run of frames a legacy window drops as already seen, while
/// nothing of that seat was accepted this session.
#[derive(Debug, Default)]
pub(crate) struct DupEpisode {
    first_seq: u64,
    last_seq: u64,
    count: u32,
    warned: bool,
    /// A frame of the seat was accepted this session: no more counting.
    fresh_seen: bool,
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
    /// The last `inbound_held_stuck` warning of this episode.
    warned_at: Option<u64>,
    unordered: u32,
    shed: u32,
    lost: u32,
    /// The sender answers gap wants: its duplicates are targeted resends
    /// for other askers, never evidence about our gaps.
    answers: bool,
    /// A window reset for a restore: owed from seq 0.
    from_start: bool,
    /// Since this session opened, nothing proves the sender has no more:
    /// its frames read before a restart may be gone with the hold, and the
    /// relay copies no longer open. Ask for the listing until it ends.
    probe: bool,
    probe_asks: u32,
    /// The last listing was a full page: more is owed than listed so far.
    probe_more: bool,
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

    /// Known to be behind: a listed own seq not here, or held content or
    /// an owed gap older than [`BEHIND_AFTER_SECS`] (a declined seq is
    /// parked, not owed).
    fn behind(&self, window: &molt_core::AcceptedWindow, now: u64, probe_on: bool) -> bool {
        let old = |since: u64| now.saturating_sub(since) >= BEHIND_AFTER_SECS;
        let listed = probe_on
            && (self.probe_more || self.listing.as_ref().is_some_and(|l| l.own.iter().any(|s| !window.is_accepted(*s))));
        let held = !self.held.is_empty() && self.since.is_some_and(old);
        let owed = self.missing.iter().any(|(s, g)| !self.refused.contains(s) && old(g.since));
        listed || held || owed
    }

    /// Whether a missing seq is asked for now. Only inside `(frontier,
    /// cap]`: a sender reads one window below the highest seq named, and an
    /// older one answers `gone` for anything under that read (§13).
    fn askable(&self, seq: u64, gap: &Gap, now: u64) -> bool {
        seq <= self.cap()
            && now.saturating_sub(gap.since) >= GAP_ASK_AFTER_SECS
            && gap.asked_at.map_or(true, |t| now.saturating_sub(t) >= GAP_ASK_EVERY_SECS)
    }

    /// Whether the open probe asks now. Re-asks on a growing interval
    /// until a listing answers; a sender not known to answer listings
    /// gets one ask until it is.
    fn probe_due(&self, now: u64, capable: bool) -> bool {
        if !self.probe {
            return false;
        }
        let Some(at) = self.listing_asked_at else { return true };
        if !capable {
            return false;
        }
        let shift = self.probe_asks.saturating_sub(1).min(6);
        let every = GAP_ASK_EVERY_SECS.saturating_mul(1 << shift).min(PROBE_EVERY_MAX_SECS);
        now.saturating_sub(at) >= every
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
        self.delivery.seen_high.remove(member);
        self.forget_inbound(member);
    }

    /// The window, its hold, its ack deadline and its repeat count - not
    /// the request/claim gates, which belong to the incarnation.
    fn forget_inbound(&mut self, member: &MemberId) {
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

    /// Empty `member`'s window as owed from its start, and hand it to the
    /// writer before any ratchet persist (§11). A re-key merge forgets the
    /// incarnation with it; an operator resync keeps it.
    fn forget_incarnation(&mut self, member: &str, proof: molt_core::RestoreProof) {
        let member = member.to_string();
        let high = self.delivery.accepted.get(&member).map_or(0, |w| w.high);
        // a resync of a window an older build wrote may face another
        // incarnation than it describes: forget that one's gates too
        let legacy = self.delivery.accepted.get(&member).map_or(true, |w| w.proof == molt_core::RestoreProof::Unknown);
        if proof == molt_core::RestoreProof::Merge || legacy {
            self.reset_peer_accept_window(&member);
        } else {
            self.forget_inbound(&member);
        }
        self.delivery.accepted.insert(member.clone(), molt_core::AcceptedWindow { proof, ..Default::default() });
        self.delivery.reset_unsaved = true;
        self.save_accepted_now();
        tracing::info!(%member, high, via = proof.label(), "accept_window_reset");
    }

    /// Queue the windows now. A refused queue keeps them dirty, and a reset
    /// among them holds every ratchet persist back until it is queued: the
    /// writer is FIFO, so the reset reaches the disk first.
    pub(crate) fn save_accepted_now(&mut self) {
        self.delivery.accepted_dirty = true;
        let Some(active) = self.active.as_ref() else { return };
        if !self.drop_seam.refuses_window_saves() && active.handle.save_accepted(self.delivery.accepted.clone()) {
            self.delivery.accepted_dirty = false;
            self.delivery.reset_unsaved = false;
            self.delivery.accepted_saved_at = self.presence_now();
        }
    }

    /// Whether the live ratchet may be persisted: never while a window
    /// reset waits for the writer.
    pub(crate) fn ratchet_may_persist(&mut self) -> bool {
        if self.delivery.reset_unsaved {
            self.delivery.ratchet_held = self.delivery.ratchet_held.saturating_add(1);
            return false;
        }
        true
    }

    /// The live group ratchet, running or parked.
    pub(crate) fn group_mls(&self) -> Option<std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>> {
        self.group_net.as_ref().map(|g| g.mls.clone()).or_else(|| self.group_mls_parked.clone())
    }

    /// Reset every seat `mls` lists as re-keyed by a merged commit -
    /// received or this node's own. The list inside the group state is the
    /// source, so a lost `NetPeerRekeyed` loses nothing: every beat, every
    /// ratchet persist and every runtime rebuild drains it.
    pub(crate) fn drain_rekeys(&mut self, mls: &std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>) {
        let taken = mls.lock().map(|mut m| m.take_rekeyed()).unwrap_or_default();
        let me = self.member();
        for member in taken.into_iter().filter(|m| *m != me) {
            self.rekey_reset(&member);
        }
    }

    pub(crate) fn rekey_reset(&mut self, member: &str) {
        self.note_rekey_merged(member);
        if self.drop_seam.keeps_window_on_rekey() {
            // the seam: the window an older build kept after losing the reset
            if let Some(w) = self.delivery.accepted.get_mut(member) {
                w.proof = molt_core::RestoreProof::Unknown;
            }
            self.delivery.dups.remove(member);
            self.delivery.seen_high.remove(member);
        } else {
            self.reset_after_rekey(member);
        }
    }

    /// The ratchet to persist: none until every re-key it holds is drained
    /// and its window reset queued - checked under the lock the merge takes.
    pub(crate) fn ratchet_snapshot(&mut self, mls: &std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>) -> Option<Vec<u8>> {
        self.drain_rekeys(mls);
        if !self.ratchet_may_persist() {
            return None;
        }
        let g = mls.lock().ok()?;
        if g.rekeys_pending() {
            return None;
        }
        g.snapshot().ok()
    }

    /// Log a commit this seat made with the ratchet that merged it, in one
    /// writer step, through the gate of the debounced persist (re-keys
    /// drained, their resets queued first). No peer sends a seat its own
    /// commit back, so it never leaves before its epoch is on disk.
    /// `false` = the pair is not durable; the commit is logged regardless.
    pub(crate) fn record_own_commit(
        &mut self,
        env: EventEnvelope,
        mls: &std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>,
    ) -> bool {
        let snap = self.ratchet_snapshot(mls);
        let durable = self.record_with(env, snap);
        if durable {
            self.delivery.mls_persisted_at = self.presence_now();
        }
        durable
    }

    /// A re-key commit merged that re-admitted `member`: THE reset of a
    /// restore. The group runtime hands the merge over the same ordered
    /// sink as the frames, and no past epoch is kept, so every frame of
    /// `member` delivered after this is the new device's and none before it
    /// was. The `Restored` block resets nothing.
    pub(crate) fn reset_after_rekey(&mut self, member: &str) {
        self.forget_incarnation(member, molt_core::RestoreProof::Merge);
    }

    /// The highest seq accepted or held from `from` in its current
    /// incarnation this session - never a dropped duplicate, never a stored
    /// window's high (it may describe another device).
    pub(crate) fn seen_high(&self, from: &str) -> u64 {
        // after a restart: the stored high of a window that is this device's
        let stored = || {
            self.delivery
                .accepted
                .get(from)
                .filter(|w| w.proof != molt_core::RestoreProof::Unknown)
                .map_or(0, |w| w.high)
        };
        self.delivery.seen_high.get(from).copied().unwrap_or_else(stored)
    }

    /// Whether frame `seq` of `from` is the newest seen from it: only such
    /// a frame takes live effects (serve, divergence, notices); an older one
    /// re-delivered after a reset or a rewind is history.
    pub(crate) fn newest_seen(&self, from: &str, seq: u64) -> bool {
        seq >= self.seen_high(from)
    }

    pub(super) fn note_seen(&mut self, from: &MemberId, seq: u64) {
        let high = self.seen_high(from).max(seq);
        self.delivery.seen_high.insert(from.clone(), high);
    }

    /// The operator resets `member`'s window, as a restore would; the gap
    /// asks and the sender's rewind then re-deliver its retained history.
    /// While that runs a second call changes nothing - unless `give_up`,
    /// which takes what is held as the new start.
    pub(crate) fn cmd_resync_seat(&mut self, member: MemberId, give_up: bool) -> Result<Reply, MoltError> {
        if self.active.is_none() && self.replica.is_none() {
            return Err(MoltError::Engine("no open republic".to_string()));
        }
        if member == self.member() {
            return Err(MoltError::Engine("cannot resync the own seat".to_string()));
        }
        if !self.roster().contains(&member) {
            return Err(MoltError::Engine(format!("{member} is not a seat")));
        }
        let running = self.resync_running(&member);
        if give_up {
            if !running {
                return Err(MoltError::Engine(format!("no resync of {member} waiting")));
            }
            // the new start: below what is held, else what was accepted;
            // nothing at all - the next frame is the baseline
            let hold = self.delivery.inbound.get(&member);
            let to = hold.and_then(|h| h.held.keys().next().map(|s| s - 1)).or_else(|| {
                hold.map(|h| {
                    let listed = h.listing.as_ref().map_or(0, |l| l.through);
                    let missing = h.missing.keys().next_back().copied().unwrap_or(0);
                    self.seen_high(&member).max(listed).max(missing)
                })
            });
            let frontier = hold.map_or(0, |h| h.frontier);
            tracing::warn!(%member, count = to.unwrap_or(0).saturating_sub(frontier), first = frontier + 1, via = "operator", "inbound_gap_lost");
            if let Some(hold) = self.delivery.inbound.get_mut(&member) {
                hold.from_start = false;
            }
            if let Some(to) = to {
                self.give_up_through(&member, to, "operator");
            }
            // still nothing accepted: no longer owed from the start, the next
            // frame is the baseline
            if let Some(w) = self.delivery.accepted.get_mut(&member).filter(|w| w.high == 0) {
                w.proof = molt_core::RestoreProof::Current;
                self.save_accepted_now();
            }
            self.emit_session(crate::SessionScope::Full);
            return Ok(Reply::Ack);
        }
        if running {
            return Err(MoltError::Engine(format!("resync of {member} running - give_up ends the wait")));
        }
        self.forget_incarnation(&member, molt_core::RestoreProof::Operator);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A reset window still filling: nothing accepted yet, or a gap still
    /// asked for.
    fn resync_running(&self, member: &str) -> bool {
        let Some(w) = self.delivery.accepted.get(member) else { return false };
        w.proof.is_reset()
            && (w.high == 0
                || self
                    .delivery
                    .inbound
                    .get(member)
                    .is_some_and(|h| !h.held.is_empty() || !h.missing.is_empty() || h.listing.is_some()))
    }

    /// `member`'s repeat count once it warned, else 0.
    pub(crate) fn dup_streak(&self, member: &str) -> u32 {
        self.delivery.dups.get(member).filter(|e| e.warned).map_or(0, |e| e.count)
    }

    /// A hint, no action: a window an older build wrote dropping frames of
    /// `from` as already seen, at rising seqs, while nothing of `from` was
    /// accepted this session - a seat whose frames are accepted is not
    /// behind a stale window, or that window was overtaken (unprovable;
    /// the operator action stays).
    pub(super) fn note_dup(&mut self, from: &MemberId, seq: u64) {
        let legacy = self.delivery.accepted.get(from).is_some_and(|w| w.proof == molt_core::RestoreProof::Unknown);
        let e = self.delivery.dups.entry(from.clone()).or_default();
        if !legacy || e.fresh_seen || (e.count > 0 && seq <= e.last_seq) {
            return;
        }
        if e.count == 0 {
            e.first_seq = seq;
        }
        e.count = e.count.saturating_add(1);
        e.last_seq = seq;
        if e.count == DUP_EPISODE_MIN && !e.warned {
            e.warned = true;
            tracing::warn!(%from, first_seq = e.first_seq, last_seq = seq, count = e.count, "dup_streak");
        }
    }

    pub(super) fn note_fresh(&mut self, from: &MemberId) {
        let e = self.delivery.dups.entry(from.clone()).or_default();
        if e.fresh_seen {
            return;
        }
        if e.warned {
            tracing::info!(%from, count = e.count, "dup_streak_cleared");
        }
        *e = DupEpisode { fresh_seen: true, ..DupEpisode::default() };
    }

    /// Mark `seq` from `from` as engine-accepted (delivery guarantee §4.2 —
    /// the accept point's bookkeeping). `false` = the sender's window already
    /// held it: the envelope is a duplicate/resend and the caller drops it
    /// (G2). Freshness dirties the window for the debounced persist.
    pub(crate) fn accept_envelope(&mut self, from: &MemberId, seq: u64) -> bool {
        let window = self
            .delivery
            .accepted
            .entry(from.clone())
            .or_insert_with(|| molt_core::AcceptedWindow { proof: molt_core::RestoreProof::Current, ..Default::default() });
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
        if now.saturating_sub(self.delivery.mls_persisted_at) < self.drop_seam.mls_persist_secs(MLS_PERSIST_SECS)
            || !self.ratchet_may_persist()
        {
            return;
        }
        // A Nostr workspace has no `NetRuntime`, but it very much has a live
        // ratchet: the group runtime advances it on every publish. Without
        // this the reopen restores the founding blob and the next publish
        // REUSES sender generations — which every peer replay-rejects and
        // silently drops.
        if let Some(group) = self.group_net.as_ref() {
            let mls = group.mls.clone();
            let snap = self.ratchet_snapshot(&mls);
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
        // a reset the writer refused retries on every beat
        if !self.delivery.accepted_dirty
            || (!self.delivery.reset_unsaved && now.saturating_sub(self.delivery.accepted_saved_at) < ACCEPT_SAVE_SECS)
        {
            return;
        }
        self.save_accepted_now();
        self.delivery.accepted_saved_at = now;
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
        if self.drop_seam.cuts(&from) || self.drop_seam.drops(&from, envelope.seq) || self.drop_seam.drops_chat(&envelope.body) {
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
        // Above the cap only a chain link over an accepted prev passes: the
        // prev_seq names the sender's previous OWN ackable event, so an
        // accepted prev proves every seq between it and this one unowed -
        // the frontier skips them and nothing this receiver is owed can
        // age out unseen. Without that proof the hold stays (2026-11-27: a
        // flood of never-sent log entries between two owed envelopes
        // otherwise deadlocked the receiver behind the cap, its ack sheet
        // frozen, the sender's tail re-offered forever).
        // `prev_seq == 0`: a pre-G7 sender or a chain start, unordered as
        // ever - and never proof.
        let linked = envelope.prev_seq != 0 && !prev_missing;
        let wait = (envelope.seq > hold.cap() && !linked)
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
        self.note_seen(from, env.seq);
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
        hold.warned_at = None;
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
        if self.drop_seam.holds_wants() {
            return;
        }
        let replaying = replaying || now.saturating_sub(quiet_since) < GAP_ASK_AFTER_SECS;
        // the first replay read through: what it brought is not asked for
        let caught_up = self.group_net.as_ref().is_some_and(|g| {
            let h = g.health.borrow();
            h.subscribed && !h.catching_up
        });
        let probe_on = self.open_probe();
        if probe_on && caught_up && !replaying {
            self.open_probes(&me);
        }
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
            let capable = self.reported_build(&m);
            let Some(hold) = self.delivery.inbound.get_mut(&m) else { continue };
            let capable = capable || hold.answers;
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
                } else if (!probe_on || !listed || refused) && now.saturating_sub(gap.since) >= GAP_FALLBACK_SECS {
                    settle.push((seq, "fallback"));
                } else if !refused && hold.askable(seq, gap, now) {
                    ask.push(seq);
                }
            }
            if !hold.held.is_empty() {
                if let Some(since) = hold.since {
                    if !hold.logged && now.saturating_sub(since) >= GAP_ASK_AFTER_SECS {
                        hold.logged = true;
                        let missing_seq = hold.missing.keys().next().copied().unwrap_or(hold.frontier + 1);
                        tracing::info!(member = %m, missing_seq, held = hold.held.len(), frontier = hold.frontier, since_s = now - since, "inbound_held");
                    }
                    let warn_due = hold.warned_at.map_or(
                        now.saturating_sub(since) >= HOLD_WARN_AFTER_SECS,
                        |t| now.saturating_sub(t) >= HOLD_WARN_EVERY_SECS,
                    );
                    if warn_due {
                        hold.warned_at = Some(now);
                        tracing::warn!(member = %m, held = hold.held.len(), frontier = hold.frontier, since_s = now - since, "inbound_held_stuck");
                    }
                }
            }
            // anything known above the frontier: ask for the listing of it
            let above = !hold.ahead.is_empty() || !hold.held.is_empty() || !hold.missing.is_empty();
            let probing = !above && hold.listing.is_none() && hold.probe_due(now, capable);
            let list_due = probing
                || (above
                    && hold.listing.is_none()
                    && hold.listing_asked_at.map_or(true, |t| now.saturating_sub(t) >= GAP_ASK_EVERY_SECS)
                    && hold.since.map_or(true, |s| now.saturating_sub(s) >= GAP_ASK_AFTER_SECS));
            ask.truncate(molt_net::gap::GAP_MAX_SEQS);
            if let Some(group) = self.group_net.as_ref() {
                // the LISTING ask runs even while a replay is still coming
                // in: it re-publishes nothing, and a flood between two owed
                // envelopes needs it to move the frontier (2026-11-27); the
                // named asks stay paused - the replay fills those itself
                if list_due {
                    let want = molt_net::gap::GapWant::range(me.clone(), m.clone(), hold.frontier, hold.cap());
                    // the seam loses the frame on the wire: it went out
                    if self.drop_seam.drops_listing_ask() || group.handle.publish_control(want.to_frame()) {
                        tracing::debug!(member = %m, after = hold.frontier, probe = probing, "gap_list_ask");
                        hold.listing_asked_at = Some(now);
                        if probing {
                            hold.probe_asks = hold.probe_asks.saturating_add(1);
                        }
                    }
                }
                if !ask.is_empty() && !replaying {
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
            for (seq, via) in settle {
                self.settle_gap(&m, seq, via);
            }
            self.apply_ready(&m, "filled");
        }
        let behind = self.behind_peers();
        if behind != self.delivery.behind {
            self.delivery.behind = behind;
            self.recompute_net_health();
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
        // a `gone` outside what this seat asked is no answer: an older
        // sender says it for seqs it never read (§13)
        let asked = |h: &SenderHold, s: u64| s > h.frontier && s <= h.cap();
        let (valid, stray): (Vec<u64>, Vec<u64>) = gone.into_iter().partition(|s| asked(hold, *s));
        if !stray.is_empty() {
            tracing::debug!(%from, count = stray.len(), first = stray[0], "gap_gone_ignored");
        }
        for s in valid {
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
        // too wide, or a stale page beside a newer one held: changes nothing
        if through > after.saturating_add(molt_net::gap::GAP_RANGE_MAX)
            || hold.listing.as_ref().is_some_and(|l| l.through >= through)
        {
            return;
        }
        // a full page, or a range that reached its width: there may be more
        hold.probe = hold.probe
            && (own.len() >= molt_net::gap::GAP_LIST_MAX || through >= after.saturating_add(molt_net::gap::GAP_RANGE_MAX));
        hold.probe_more = hold.probe;
        hold.probe_asks = 0;
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

    /// Arm the open probe once per session for every sender this seat holds
    /// history with; a runtime rebuild keeps the holds, so it re-arms none.
    fn open_probes(&mut self, me: &MemberId) {
        let fresh: Vec<MemberId> = self
            .delivery
            .accepted
            .iter()
            .filter(|(m, w)| *m != me && w.high > 0 && !self.delivery.probed.contains(*m))
            .map(|(m, _)| m.clone())
            .collect();
        for m in fresh {
            self.delivery.probed.insert(m.clone());
            let Some(w) = self.delivery.accepted.get(&m) else { continue };
            let (start, links) = (if w.frontier > 0 { w.frontier } else { w.high }, w.links.clone());
            self.delivery.inbound.entry(m).or_insert_with(|| SenderHold::new(start, links)).probe = true;
        }
    }

    /// The senders whose stream this seat knows it is behind on, for at
    /// least the ask delay.
    pub(crate) fn behind_peers(&self) -> Vec<MemberId> {
        let now = self.presence_now();
        let empty = molt_core::AcceptedWindow::default();
        let probe_on = self.open_probe();
        let mut peers: Vec<MemberId> = self
            .delivery
            .inbound
            .iter()
            .filter(|(m, h)| h.behind(self.delivery.accepted.get(*m).unwrap_or(&empty), now, probe_on))
            .map(|(m, _)| m.clone())
            .collect();
        peers.sort();
        peers
    }

    /// The open probe: the release switch, or a test forcing it on.
    pub(crate) fn open_probe(&self) -> bool {
        OPEN_PROBE || self.drop_seam.forces_probe()
    }

    /// A seq of `member` delivered but left unaccepted (the D7 park).
    #[cfg(test)]
    pub(crate) fn park_for_test(&mut self, member: &str, seq: u64) {
        if let Some(h) = self.delivery.inbound.get_mut(member) {
            h.refused.insert(seq);
        }
    }

    /// The own outbox holds a tail these seats are owed: its hourly budget
    /// is spent (empty = a round went out again).
    pub(crate) fn cmd_net_resend_held(&mut self, owed: Vec<MemberId>, generation: Option<u64>) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            return Ok(Reply::Ack);
        }
        let now = self.presence_now();
        self.delivery.resend_held = if owed.is_empty() {
            None
        } else {
            Some((owed, self.delivery.resend_held.as_ref().map_or(now, |(_, since)| *since)))
        };
        self.recompute_net_health();
        Ok(Reply::Ack)
    }

    /// A hold whose only content is a missing seq (a D7-refused first frame).
    #[cfg(test)]
    pub(crate) fn note_missing_for_test(&mut self, member: &str, seq: u64) {
        let now = self.presence_now();
        let hold = self.delivery.inbound.entry(member.to_string()).or_insert_with(|| SenderHold::new(0, Vec::new()));
        hold.note_missing(seq, now);
    }

    #[cfg(test)]
    pub(crate) fn listing_through_for_test(&self, member: &str) -> Option<u64> {
        self.delivery.inbound.get(member).and_then(|h| h.listing.as_ref()).map(|l| l.through)
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
        let mut rows: Vec<molt_core::InboundHeld> = self
            .delivery
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
                from_start: h.from_start,
            })
            .collect();
        // a reset window nothing has reached yet waits all the same
        for (m, w) in &self.delivery.accepted {
            if w.proof.is_reset() && w.high == 0 && !self.delivery.inbound.contains_key(m) {
                rows.push(molt_core::InboundHeld { member: m.clone(), missing_seq: 1, from_start: true, ..Default::default() });
            }
        }
        rows
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
        if let Some(mls) = self.group_mls() {
            self.drain_rekeys(&mls);
        }
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
        if self.drop_seam.beacon_now.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.chain.beacon_at = 1;
        }
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
        let mut w = molt_core::AcceptedWindow { proof: old.proof, ..Default::default() };
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

#[cfg(test)]
mod probe_tests {
    use super::*;

    fn hold(probe: bool) -> SenderHold {
        SenderHold { probe, ..SenderHold::new(10, Vec::new()) }
    }

    /// An unanswered ask is judged when the next one is due, never on the
    /// tick after it; a sender known to answer is asked again on a
    /// growing interval and never given up on by time.
    #[test]
    fn the_probe_re_asks_a_capable_sender_on_a_growing_interval() {
        let mut h = hold(true);
        assert!(h.probe_due(100, true), "the first ask goes at once");
        h.listing_asked_at = Some(100);
        h.probe_asks = 1;
        assert!(!h.probe_due(101, true), "not on the next tick");
        assert!(!h.probe_due(109, true));
        assert!(h.probe_due(110, true), "due after the ask interval");
        h.listing_asked_at = Some(110);
        h.probe_asks = 2;
        assert!(!h.probe_due(129, true) && h.probe_due(130, true), "the interval doubles");
        h.probe_asks = 30;
        assert!(h.probe_due(110 + PROBE_EVERY_MAX_SECS, true), "capped, never given up");
        assert!(h.probe, "still armed");
    }

    /// A sender not known to answer listings (an older build) gets the one
    /// ask; silence after it asks nothing more until it is known.
    #[test]
    fn an_old_sender_gets_one_ask_then_silence() {
        let mut h = hold(true);
        assert!(h.probe_due(100, false));
        h.listing_asked_at = Some(100);
        h.probe_asks = 1;
        assert!(!h.probe_due(100 + PROBE_EVERY_MAX_SECS * 10, false), "no second ask");
        assert!(h.probe_due(100 + GAP_ASK_EVERY_SECS, true), "a reported build resumes it");
    }

    /// The probe ends on a short page or on a "nothing above" answer, and
    /// goes on after a full one.
    #[test]
    fn the_probe_pages_on_a_full_listing_and_ends_on_a_short_or_level_one() {
        let mut st = crate::net::test_support::presence_fixture();
        let bob = "bob".to_string();
        let me = st.member();
        st.delivery.inbound.insert(bob.clone(), hold(true));
        let full: Vec<u64> = (11..).take(molt_net::gap::GAP_LIST_MAX).collect();
        let through = *full.last().expect("full");
        st.cmd_net_gap_listed(&bob, &me, 10, through, full);
        assert!(st.delivery.inbound[&bob].probe && st.delivery.inbound[&bob].probe_more, "a full page: more to come");
        st.delivery.inbound.insert(bob.clone(), hold(true));
        st.cmd_net_gap_listed(&bob, &me, 10, 12, vec![11, 12]);
        assert!(!st.delivery.inbound[&bob].probe, "a short page ends it");
        st.delivery.inbound.insert(bob.clone(), hold(true));
        st.cmd_net_gap_listed(&bob, &me, 10, 10, Vec::new());
        assert!(!st.delivery.inbound[&bob].probe && !st.delivery.inbound[&bob].probe_more, "level: nothing above");
    }

    /// `behind` holds while a listed own seq is not here, whatever its age,
    /// and clears only when the window has it.
    #[test]
    fn behind_holds_until_the_listed_tail_is_accepted() {
        let mut h = hold(false);
        let mut w = molt_core::AcceptedWindow::default();
        assert!(w.accept(10));
        h.listing = Some(Listing { through: 12, own: [11, 12].into_iter().collect(), tries: Default::default() });
        assert!(h.behind(&w, 0, true), "listed and not here");
        assert!(w.accept(11));
        assert!(h.behind(&w, 0, true));
        assert!(w.accept(12));
        assert!(!h.behind(&w, 0, true), "level");
        h.probe_more = true;
        assert!(h.behind(&w, 0, true), "a full page promised more");
        assert!(!h.behind(&w, 0, false), "with the probe off a listing shows nothing");
    }

    /// H1: a named ask never reaches past the cap, however far above it a
    /// missing seq sits (the prev of a far-ahead frame, a shed one), so no
    /// sender's read window can leave a low seq unread.
    #[test]
    fn a_named_ask_stays_inside_the_cap() {
        let mut h = hold(false);
        h.note_missing(11, 0);
        h.note_missing(10 + molt_core::ACCEPT_WINDOW_BITS + 500, 0);
        let ask: Vec<u64> = h.missing.iter().filter(|(s, g)| h.askable(**s, g, 100)).map(|(s, _)| *s).collect();
        assert_eq!(ask, vec![11]);
    }

    /// A gap shorter than the behind delay, or a declined (parked) seq, is
    /// not "behind": ordinary reordering does not flap the health.
    #[test]
    fn a_short_gap_or_a_parked_seq_is_not_behind() {
        let mut h = hold(false);
        let w = molt_core::AcceptedWindow::default();
        h.note_missing(11, 100);
        assert!(!h.behind(&w, 100 + BEHIND_AFTER_SECS - 1, true));
        assert!(h.behind(&w, 100 + BEHIND_AFTER_SECS, true));
        h.refused.insert(11);
        assert!(!h.behind(&w, 1_000, true), "parked, not owed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::test_support::{T, id, presence_fixture};

    fn chat(seq: u64, prev: u64, body: &str) -> EventEnvelope {
        let n = usize::try_from(seq).expect("small");
        EventEnvelope {
            seq,
            ts: T,
            by: "bob".to_string(),
            body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(id(n), "bob", body, T)),
            prev_seq: prev,
        }
    }

    fn land(st: &mut State, env: EventEnvelope) {
        st.cmd_net_delivered("bob".to_string(), env, None).expect("delivery");
    }

    #[derive(Clone, Default)]
    struct AskSink;
    impl molt_net::EngineSink for AskSink {
        async fn deliver(&self, _from: &MemberId, _env: EventEnvelope) -> Result<(), molt_net::NetError> {
            Ok(())
        }
        async fn peer_seen(&self, _m: &MemberId) {}
        async fn send_failed(&self, _m: &MemberId, _r: &str) {}
    }

    /// The 2026-11-27 deadlock's second half: while a stored replay is
    /// still coming in, the LISTING ask (it re-publishes nothing) still
    /// goes, at most once per the ask interval; only the named asks stay
    /// paused - the replay fills those itself. After the listing's answer
    /// the frontier jumps.
    #[tokio::test]
    async fn a_listing_ask_also_runs_while_a_replay_is_coming_in() {
        let mut st = presence_fixture();
        land(&mut st, chat(1, 0, "base"));
        land(&mut st, chat(2, 1, "second"));
        land(&mut st, chat(5, 4, "hole")); // missing 4, held 5
        let far = 2 + molt_core::ACCEPT_WINDOW_BITS + 500;
        land(&mut st, chat(far, far - 1, "far")); // missing far-1, held far
        let (health_tx, health_rx) = tokio::sync::watch::channel(molt_net::group_runtime::GroupHealth {
            subscribed: true,
            replaying: true,
            ..molt_net::group_runtime::GroupHealth::default()
        });
        let mls = std::sync::Arc::new(std::sync::Mutex::new({
            let mut m = molt_net::MlsMember::new(&molt_storage::SigningKey::from_bytes(&[9u8; 32]), "ada")
                .expect("mls");
            m.create_group().expect("group");
            m
        }));
        // a dead relay (bound, then dropped - never a service port); the
        // runtime's tasks never run, only the control queue is observed
        let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("ws://127.0.0.1:{}", dead.local_addr().expect("addr").port());
        drop(dead);
        let channel = molt_net::ritual_net::GroupChannel::new(molt_net::dial::Dialer::direct(), vec![url], [7u8; 32]);
        let (wake, wake_rx) = tokio::sync::watch::channel(0u64);
        let handle = molt_net::group_runtime::spawn_group(
            channel.clone(),
            molt_net::supervisor::MlsChannel::from_shared(mls.clone()),
            molt_net::group_runtime::GroupNetConfig::fast("ada".to_string(), vec!["bob".to_string()]),
            molt_net::supervisor::MemLog::new(),
            molt_net::supervisor::MemStateStore::new(),
            AskSink,
            wake_rx,
            health_tx,
        );
        let trickle = molt_net::trickle::spawn_trickle(
            channel,
            molt_net::supervisor::MemStateStore::new(),
            handle.outbox_busy(),
            molt_net::trickle::TrickleConfig::new(3600, 1),
        );
        st.group_net = Some(crate::GroupNet {
            handle,
            mls,
            wakeup: wake,
            health: health_rx,
            trickle,
            lockout: crate::net::LockOut::default(),
            unreadable: crate::net::UnreadableEpisode::default(),
        });
        at(&mut st, GAP_ASK_AFTER_SECS);
        st.cmd_net_delivery_tick().expect("tick");
        let hold = st.delivery.inbound.get("bob").expect("hold");
        assert_eq!(
            hold.listing_asked_at,
            Some(T + GAP_ASK_AFTER_SECS),
            "the listing ask went out despite the replay"
        );
        assert_eq!(
            hold.missing.get(&4).and_then(|g| g.asked_at),
            None,
            "no named ask while the replay runs"
        );
        // the listing's answer (a range cannot reach the held far one) lets
        // the frontier jump
        let top = 2 + molt_core::ACCEPT_WINDOW_BITS;
        st.cmd_net_gap_listed(&"bob".to_string(), &"ada".to_string(), 2, top, vec![top]);
        assert_eq!(
            st.delivery.accepted["bob"].frontier,
            top - 1,
            "the listing raised the frontier while the replay ran"
        );
    }

    fn at(st: &mut State, secs: u64) {
        st.presence.clock_override = Some(T + secs);
    }
}
