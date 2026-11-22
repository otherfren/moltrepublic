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

/// G7: held envelopes per sender; past it the oldest is delivered out of
/// order rather than shed (§10).
const HOLD_MAX: usize = 512;

/// A seq missing this long is asked for; younger gaps are mostly a
/// newest-first relay replay filling itself.
const GAP_ASK_AFTER_SECS: u64 = 2;

/// Re-ask a sender's still-missing seqs this often.
const GAP_ASK_EVERY_SECS: u64 = 10;

/// Relay skew after evidence that the gap will not fill (the sender
/// re-published it, or a resend round passed over it): settled after this.
pub(crate) const GAP_PASS_GRACE_SECS: u64 = 5;

/// The bound for a sender that answers nothing (an older build): six times
/// the first rung of its resend ladder, so its first rounds land inside it.
pub(crate) const GAP_FALLBACK_SECS: u64 = 60;

/// Missing and settled seqs tracked per sender (each); past it the lowest
/// missing one is settled.
const SETTLED_MAX: usize = 1024;

/// One missing seq of a sender's stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gap {
    since: u64,
    asked_at: Option<u64>,
    /// Evidence it will not fill: settled at this instant.
    settle_at: Option<u64>,
}

/// G7 per sender (§10): what is missing and what waits for it.
#[derive(Debug, Default)]
pub(crate) struct SenderHold {
    missing: std::collections::BTreeMap<u64, Gap>,
    held: std::collections::BTreeMap<u64, EventEnvelope>,
    settled: std::collections::BTreeSet<u64>,
    /// The current hold episode: start, logged, envelopes released out of order.
    since: Option<u64>,
    logged: bool,
    unordered: u32,
}

impl SenderHold {
    /// Whether an order-sensitive envelope at `seq` must wait.
    fn blocks(&self, seq: u64) -> bool {
        self.missing.range(..seq).next().is_some() || self.held.range(..seq).next().is_some()
    }

    /// Evidence that every missing seq below `seq` was passed over.
    fn passed(&mut self, seq: u64, now: u64) {
        for gap in self.missing.range_mut(..seq).map(|(_, g)| g) {
            gap.settle_at.get_or_insert(now + GAP_PASS_GRACE_SECS);
        }
    }

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

    fn idle(&self) -> bool {
        self.missing.is_empty() && self.held.is_empty()
    }
}

/// Whether an envelope's effect depends on its sender's earlier envelopes:
/// the chat stream (arrival is display order; a reaction toggles) and a
/// member's stances (newest wins). Everything else is keyed, buffered or
/// idempotent on its own and never waits.
fn order_sensitive(body: &WorkspaceEvent) -> bool {
    matches!(
        body,
        WorkspaceEvent::Chat(_)
            | WorkspaceEvent::ChatReacted { .. }
            | WorkspaceEvent::ChatDeleted { .. }
            | WorkspaceEvent::ChatRead { .. }
            | WorkspaceEvent::FileRemoved { .. }
            | WorkspaceEvent::Approved { .. }
            | WorkspaceEvent::Declined { .. }
            | WorkspaceEvent::Withdrawn { .. }
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
        if self.delivery.accepted.remove(member).is_some() {
            self.delivery.accepted_dirty = true;
        }
        // any pending ack deadline refers to the OLD window — drop it too
        self.delivery.ack_due.remove(member);
        // held successors chain into the OLD incarnation's seq space; the
        // new incarnation resends its own history anyway (G7)
        self.delivery.inbound.remove(member);
    }

    /// Mark `seq` from `from` as engine-accepted (delivery guarantee §4.2 —
    /// the accept point's bookkeeping). `false` = the sender's window already
    /// held it: the envelope is a duplicate/resend and the caller drops it
    /// (G2). Freshness dirties the window for the debounced persist.
    pub(crate) fn accept_envelope(&mut self, from: &MemberId, seq: u64) -> bool {
        let fresh = self.delivery.accepted.entry(from.clone()).or_default().accept(seq);
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
        let has_history = window.is_some_and(|w| w.high > 0);
        let is_dup = window.is_some_and(|w| w.is_accepted(envelope.seq));
        let prev_missing = envelope.prev_seq != 0
            && has_history
            && !window.is_some_and(|w| w.is_accepted(envelope.prev_seq));
        if !has_history {
            return self.deliver_gated(from, envelope);
        }
        let now = self.presence_now();
        let hold = self.delivery.inbound.entry(from.clone()).or_default();
        if is_dup || hold.held.contains_key(&envelope.seq) {
            // a resend round passed here: whatever is still missing below
            // was not re-published
            hold.passed(envelope.seq, now);
            if is_dup {
                return self.deliver_gated(from, envelope);
            }
            self.delivery.ack_due.entry(from).or_insert(now + ACK_DEBOUNCE_SECS);
            return Ok(Reply::Ack);
        }
        hold.missing.remove(&envelope.seq);
        let prev = envelope.prev_seq;
        if prev_missing && !hold.held.contains_key(&prev) && !hold.settled.contains(&prev) {
            hold.missing.entry(prev).or_insert(Gap { since: now, asked_at: None, settle_at: None });
            if hold.missing.len() > SETTLED_MAX {
                if let Some((&lowest, _)) = hold.missing.first_key_value() {
                    hold.settle(lowest);
                }
            }
        }
        if order_sensitive(&envelope.body) && hold.blocks(envelope.seq) {
            hold.since.get_or_insert(now);
            hold.held.insert(envelope.seq, envelope);
            let overflow = if hold.held.len() > HOLD_MAX { hold.held.pop_first() } else { None };
            self.delivery.ack_due.entry(from.clone()).or_insert(now + ACK_DEBOUNCE_SECS);
            if let Some((_, oldest)) = overflow {
                self.release_unordered(&from, oldest);
            }
            return Ok(Reply::Ack);
        }
        let reply = self.deliver_gated(from.clone(), envelope);
        self.drain_held(&from, "arrived");
        reply
    }

    /// Deliver a held envelope past its gap (cap overflow), counted for
    /// the episode's summary line.
    fn release_unordered(&mut self, from: &MemberId, env: EventEnvelope) {
        if let Some(hold) = self.delivery.inbound.get_mut(from) {
            if hold.unordered == 0 {
                tracing::warn!(member = %from, held = HOLD_MAX, "inbound_hold_full");
            }
            hold.unordered = hold.unordered.saturating_add(1);
        }
        let _ = self.deliver_gated(from.clone(), env);
    }

    /// Deliver, in seq order, every held envelope from `from` that no
    /// longer has a missing seq below it; close the episode once nothing
    /// is held.
    fn drain_held(&mut self, from: &MemberId, via: &str) {
        loop {
            let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
            let next = match hold.held.first_key_value() {
                Some((&seq, _)) if hold.missing.range(..seq).next().is_none() => hold.held.remove(&seq),
                _ => None,
            };
            match next {
                Some(env) => {
                    let _ = self.deliver_gated(from.clone(), env);
                }
                None => break,
            }
        }
        let now = self.presence_now();
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        if !hold.held.is_empty() {
            return;
        }
        if let Some(since) = hold.since.take() {
            if hold.logged || hold.unordered > 0 {
                tracing::info!(member = %from, since_s = now.saturating_sub(since), via, unordered = hold.unordered, "inbound_held_cleared");
            }
        }
        hold.logged = false;
        hold.unordered = 0;
        if hold.idle() && hold.settled.is_empty() {
            self.delivery.inbound.remove(from);
        }
    }

    /// Settle `seq` of `from`'s stream as never coming, then drain.
    fn settle_gap(&mut self, from: &MemberId, seq: u64, via: &str) {
        let Some(hold) = self.delivery.inbound.get_mut(from) else { return };
        if hold.settle(seq) {
            if via == "fallback" {
                tracing::warn!(member = %from, missing_seq = seq, held = hold.held.len(), via, "inbound_gap_skipped");
            } else {
                tracing::info!(member = %from, missing_seq = seq, held = hold.held.len(), via, "inbound_gap_skipped");
            }
        }
        self.drain_held(from, via);
    }

    /// G7 beat (§10): forget gaps the window filled or aged out, settle
    /// the ones with evidence or past the bound, ask for the rest.
    pub(crate) fn inbound_gap_tick(&mut self, now: u64) {
        let me = self.member();
        let members: Vec<MemberId> = self.delivery.inbound.keys().cloned().collect();
        for m in members {
            let window = self.delivery.accepted.get(&m).cloned().unwrap_or_default();
            let Some(hold) = self.delivery.inbound.get_mut(&m) else { continue };
            let filled: Vec<u64> =
                hold.missing.keys().copied().filter(|s| window.is_accepted(*s)).collect();
            for s in filled {
                hold.missing.remove(&s);
            }
            let mut settle: Vec<(u64, &str)> = Vec::new();
            let mut ask: Vec<u64> = Vec::new();
            for (&seq, gap) in &hold.missing {
                if gap.settle_at.is_some_and(|t| now >= t) {
                    settle.push((seq, "resend_passed"));
                } else if now.saturating_sub(gap.since) >= GAP_FALLBACK_SECS {
                    settle.push((seq, "fallback"));
                } else if now.saturating_sub(gap.since) >= GAP_ASK_AFTER_SECS
                    && gap.asked_at.map_or(true, |t| now.saturating_sub(t) >= GAP_ASK_EVERY_SECS)
                {
                    ask.push(seq);
                }
            }
            if !hold.held.is_empty() && !hold.logged {
                if let (Some(since), Some((&missing_seq, _))) = (hold.since, hold.missing.first_key_value()) {
                    if now.saturating_sub(since) >= GAP_ASK_AFTER_SECS {
                        hold.logged = true;
                        tracing::info!(member = %m, missing_seq, held = hold.held.len(), since_s = now - since, "inbound_held");
                    }
                }
            }
            ask.truncate(molt_net::gap::GAP_MAX_SEQS);
            if !ask.is_empty() {
                if let Some(group) = self.group_net.as_ref() {
                    let want = molt_net::gap::GapWant::new(me.clone(), m.clone(), ask.clone());
                    if group.handle.publish_control(want.to_frame()) {
                        tracing::debug!(member = %m, seqs = ask.len(), "gap_want");
                        for s in &ask {
                            if let Some(g) = hold.missing.get_mut(s) {
                                g.asked_at = Some(now);
                            }
                        }
                    }
                }
            }
            for (seq, via) in settle {
                self.settle_gap(&m, seq, via);
            }
            self.drain_held(&m, "filled");
        }
    }

    /// A gap want reached this node: only the named sender answers, with a
    /// targeted resend off the actor (§10).
    pub(crate) fn cmd_net_gap_wanted(&mut self, from: &MemberId, of: &MemberId, seqs: Vec<u64>) {
        if *of != self.member() || !self.roster().contains(from) {
            return;
        }
        if let Some(group) = self.group_net.as_ref() {
            if !group.handle.answer_gap(from.clone(), seqs) {
                tracing::debug!(%from, "gap_want=dropped reason=queue_full");
            }
        }
    }

    /// The sender answered our gap want: `gone` never comes, `resent` must
    /// land within the skew grace or it never will.
    pub(crate) fn cmd_net_gap_answered(&mut self, from: MemberId, to: MemberId, resent: Vec<u64>, gone: Vec<u64>) {
        if to != self.member() {
            return;
        }
        let now = self.presence_now();
        let Some(hold) = self.delivery.inbound.get_mut(&from) else { return };
        for s in &resent {
            if let Some(g) = hold.missing.get_mut(s) {
                g.settle_at.get_or_insert(now + GAP_PASS_GRACE_SECS);
            }
        }
        for s in gone {
            self.settle_gap(&from, s, "gone");
        }
    }

    /// The held streams, for the status view.
    pub(crate) fn inbound_held_view(&self) -> Vec<molt_core::InboundHeld> {
        self.delivery
            .inbound
            .iter()
            .filter(|(_, h)| !h.held.is_empty())
            .map(|(m, h)| molt_core::InboundHeld {
                member: m.clone(),
                missing_seq: h.missing.keys().next().copied().unwrap_or(0),
                held: u32::try_from(h.held.len()).unwrap_or(u32::MAX),
                since: h.since.unwrap_or(0),
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
                claims.insert(m, win.clone());
            }
        }
        if claims.is_empty() {
            return;
        }
        let ack = molt_net::group_ack::GroupAck::new(self.member(), claims);
        if self.delivery.last_group_ack.as_ref() == Some(&ack) {
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
        // A2.2: held seals land once their round has passed
        self.drain_held_seals();
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
            let Ok(mut payload) = serde_json::to_vec(window) else {
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
