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

/// G7 in-order hold: parked out-of-order envelopes per sender, before the
/// newest is shed onto the resend machinery.
const ORDERED_PARK_MAX: usize = 512;

/// G7 pathology valve: a parked envelope whose predecessor never arrived is
/// released UNORDERED (loudly) after this long. The resend backoff caps at
/// 600 s, so an honest predecessor always lands well inside the window.
pub(crate) const ORDERED_PARK_GIVEUP_SECS: u64 = 900;


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
        // parked successors chain into the OLD incarnation's seq space; the
        // new incarnation resends its own history anyway (G7)
        self.delivery.ordered_park.remove(member);
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
        // G7 in-order hold (delivery guarantee): an envelope whose stamped
        // predecessor is not in the accept window yet is PARKED, un-marked —
        // the sender keeps it unacked (its floor stalls below it) and the
        // resend machinery re-earns it after any crash. A resent predecessor
        // can therefore never become visible AFTER its successor. `prev_seq
        // == 0` (pre-G7 sender / chain start) delivers unordered as before.
        //
        // Fresh-incarnation rule (N4b §3.1a): a sender we hold NO accepted
        // history with cannot be ordered against that history. A rejoiner or
        // late joiner enters the broadcast mid-stream, and the predecessors
        // were published at epochs its exporter ring can never open — parking
        // would hold the whole catch-up hostage to frames that cannot exist
        // for it. The first envelope delivers as the ordering baseline.
        let has_history = self.delivery.accepted.get(&from).is_some_and(|w| w.high > 0);
        if envelope.prev_seq != 0
            && has_history
            && !self
                .delivery.accepted
                .get(&from)
                .is_some_and(|w| w.is_accepted(envelope.prev_seq))
        {
            self.park_ordered(&from, envelope);
            return Ok(Reply::Ack);
        }
        let reply = self.deliver_gated(from.clone(), envelope);
        // successors that were waiting on what just landed drain IN ORDER;
        // each drained envelope can itself unlock the next
        while let Some(next) = self.take_ready_parked(&from) {
            let _ = self.deliver_gated(from.clone(), next);
        }
        reply
    }

    /// G7: park an out-of-order envelope (bounded per sender, dup-tolerant),
    /// and arm an ACK — the reported window tells the sender exactly which
    /// predecessor is missing, so its resend closes the gap fast.
    pub(crate) fn park_ordered(&mut self, from: &MemberId, envelope: EventEnvelope) {
        let park = self.delivery.ordered_park.entry(from.clone()).or_default();
        if park.len() >= ORDERED_PARK_MAX && !park.contains_key(&envelope.seq) {
            // shed the NEWEST (furthest from deliverable) — the resend
            // machinery re-offers it once the chain caught up
            if let Some((&last, _)) = park.iter().next_back() {
                if envelope.seq > last {
                    tracing::debug!(%from, seq = envelope.seq, "ordered park full - shedding onto the resend");
                    return;
                }
                park.remove(&last);
            }
        }
        tracing::debug!(%from, seq = envelope.seq, prev = envelope.prev_seq, "holding an out-of-order envelope for its predecessor");
        let now = self.presence_now();
        self.delivery.ordered_park
            .entry(from.clone())
            .or_default()
            .entry(envelope.seq)
            .or_insert((envelope, now));
        let due = now + ACK_DEBOUNCE_SECS;
        self.delivery.ack_due.entry(from.clone()).or_insert(due);
    }

    /// G7: the next parked envelope from `from` whose predecessor is now
    /// accepted (ascending seq = chain order), if any.
    fn take_ready_parked(&mut self, from: &MemberId) -> Option<EventEnvelope> {
        let ready = {
            let park = self.delivery.ordered_park.get(from)?;
            park.iter()
                .find(|(_, (env, _))| {
                    env.prev_seq == 0
                        || self
                            .delivery.accepted
                            .get(from)
                            .is_some_and(|w| w.is_accepted(env.prev_seq))
                })
                .map(|(seq, _)| *seq)?
        };
        let park = self.delivery.ordered_park.get_mut(from)?;
        let (env, _) = park.remove(&ready)?;
        if park.is_empty() {
            self.delivery.ordered_park.remove(from);
        }
        Some(env)
    }

    /// G7 pathology valve (rides the delivery tick): a parked envelope whose
    /// predecessor never arrives — a buggy or lying chain — is released
    /// UNORDERED after [`ORDERED_PARK_GIVEUP_SECS`], loudly. Within the
    /// valve window a real predecessor always lands first (the resend
    /// backoff caps at 600 s), so honest chains never trip it.
    pub(crate) fn release_stale_parked(&mut self, now: u64) {
        let stale: Vec<(MemberId, u64)> = self
            .delivery.ordered_park
            .iter()
            .flat_map(|(m, park)| {
                park.iter()
                    .filter(|(_, (_, at))| now.saturating_sub(*at) > ORDERED_PARK_GIVEUP_SECS)
                    .map(|(seq, _)| (m.clone(), *seq))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (member, seq) in stale {
            let Some(park) = self.delivery.ordered_park.get_mut(&member) else { continue };
            let Some((env, _)) = park.remove(&seq) else { continue };
            if park.is_empty() {
                self.delivery.ordered_park.remove(&member);
            }
            tracing::warn!(%member, seq, prev = env.prev_seq, "a parked envelope's predecessor never arrived - releasing it unordered");
            let _ = self.deliver_gated(member.clone(), env);
        }
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
        // G7: release park entries whose predecessor pathologically never came
        self.release_stale_parked(now);
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
