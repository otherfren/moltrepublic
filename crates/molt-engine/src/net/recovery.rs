// SPDX-License-Identifier: GPL-3.0-or-later

//! Recovery over the net, coordinator side: the recovery-inbox tasks a
//! minted link listens on, the standing seat inbox, the request ingest
//! (a minted link; a link-less request is refused), the progress frames to the
//! rejoiner, the post-re-key mesh announce/extension, and the link-mint
//! result handlers. The rejoiner's own task is `crate::recovery`.

use super::*;

/// Minimum seconds between accepted mesh (re-)announces per member — each
/// costs every peer a supervisor teardown+rebuild+fsync (see
/// `State::spawn_mesh_extension`).
const MESH_EXTENSION_COOLDOWN_SECS: u64 = 60;

/// B4: least time between two anchor re-sends for one attempt.
const WANT_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);
/// A repeated request re-sends the whole anchor at most this often: relays
/// replay stored request wraps in bursts on every resubscribe.
const REASK_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(10);
/// Open anchor-first attempts kept per workspace; the one whose rejoiner
/// was heard least recently goes first. Each holds a copy of the anchor,
/// and one per seat coming back at once is plenty.
pub(crate) const MAX_ANCHOR_ATTEMPTS: usize = 4;
/// The coordinator's wait line while an attempt waits for Ready.
const WAIT_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Links kept in memory to show again (D2).
const LINKS_KEPT: usize = 16;
/// Requests whose declared capabilities are kept for their seal (item 7).
const REJOINER_CAPS_MAX: usize = 64;

/// B4: the answer to a Want/Ready for an attempt this coordinator does
/// not hold (it restarted, or the link was re-minted).
pub(crate) const LINK_OUTDATED: &str = "link outdated";


impl State {
    /// A returning member's recovery request reached this coordinator (recovery
    /// step ❸): verify the seat proof against the anchored roster identity and
    /// propose the threshold re-admission, remembering the fresh KeyPackage +
    /// reply queue for the MLS re-key once the `Restored` block commits.
    ///
    /// Nostr third anchor: this IS the choke point. `RecoverRequest` carries
    /// the rejoiner's NEW anchor (N4b step 1), and it is canonicalized,
    /// checked for cross-seat collision and proven-possessed HERE — before
    /// the ticket is spent and before it can reach a `Restored` block —
    /// exactly like `cmd_net_join_requested`.
    ///
    /// Every run on one link shares its transport key, so the run itself is
    /// told apart by its attempt id (a hash of its KeyPackage): a replayed
    /// earlier run never replaces a later one, and every answer names the
    /// run it answers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_net_recover_requested(
        &mut self,
        member: MemberId,
        identity_pk: String,
        key_package: String,
        ticket: String,
        seat_proof: String,
        new_nostr_pk: String,
        relays: Vec<String>,
        consent: String,
        reply: String,
        sender_npub: String,
        (anchor_first, started, caps): (bool, u64, Vec<String>),
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let attempt = molt_net::invite::recover_attempt_id(&key_package);
        // the ticket must be a live one this node minted for THIS seat
        // (review R8): a member holding its own phrase must not spend
        // another seat's link
        let ticketed = self
            .recovery.tickets
            .get(&ticket)
            .is_some_and(|minted_for| *minted_for == member);
        // NORMALIZE-OR-REJECT at the choke point, exactly like the founding
        // ingest: an anchor that is not a canonical curve point must never
        // reach a chain block, and it must not spend the ticket either. Empty
        // stays empty (the loopback path has no transport anchor).
        let canonical = if new_nostr_pk.is_empty() {
            String::new()
        } else {
            match molt_net::canonical_nostr_pk(&new_nostr_pk) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(%member, error = %e, "recovery request with a malformed transport anchor - dropped");
                    return Ok(Reply::Ack);
                }
            }
        };
        // PROOF-OF-POSSESSION (§2.1, the founding twin at `cmd_net_join_requested`):
        // the anchor claimed here must BE the key that sealed the gift wrap.
        // The seat proof already binds it under the identity key, so this does
        // not gate authenticity — it gates DELIVERABILITY: without it a
        // relay-level attacker could re-address the coordinator's Welcome to a
        // key nobody holds and strand the seat.
        //
        // Gated on THIS node's transport kind, which no remote can influence —
        // not on the field being non-empty, which would let a missing proof
        // read as "loopback, nothing to check".
        if self.transport_kind == Some(molt_core::TransportKind::Nostr)
            && (sender_npub.is_empty() || canonical != sender_npub)
        {
            tracing::warn!(
                %member,
                "recovery request claims a transport key it did not sign with - refused (possible impersonation)"
            );
            return Ok(Reply::Ack);
        }
        // the seat proved: only such a sender is ever answered (review R4)
        let proven = || {
            self.seat_proof_holds(&member, &identity_pk, &key_package, &ticket, &seat_proof, &canonical, &relays)
        };
        // …and it must not collide with a seat that already holds it: the
        // complete register (review C8) - founding anchors, every Restored
        // block's anchor and the blob's working anchors. Relays replay every
        // stored request wrap, and an accepted request's anchor is in here.
        // A NEW run on a link whose re-admission sealed shares that anchor:
        // it hears `link outdated` (review R2), a replay stays silent.
        if !canonical.is_empty() && self.anchor_seen_in_chain(&canonical) {
            if !self.recovery.attempts.is_spent(&attempt) && proven() {
                self.answer_link_outdated(&member, &attempt, &canonical);
            } else {
                tracing::debug!(%member, "recovery request reuses an anchored transport key - dropped");
            }
            return Ok(Reply::Ack);
        }
        // B3: a seat never restores itself - an older build's link-less
        // request is answered, never proposed
        if !ticketed {
            let spent = self.recovery.spent_tickets.contains(&ticket);
            if self.recovery.attempts.is_spent(&attempt) {
                tracing::debug!(%member, "recover_request=replayed");
            } else if !proven() {
                tracing::debug!(%member, "recover_request=unproven");
            } else if anchor_first || spent {
                // a build that asks anchor-first never self-services: its
                // link is one this node forgot (a restart) or spent
                self.answer_link_outdated(&member, &attempt, &canonical);
            } else {
                self.refuse_unsolicited(&member, &attempt, &canonical, &sender_npub);
            }
            return Ok(Reply::Ack);
        }
        // one re-admission at a time (review R3): a pending Restored proposal
        // for this member means another receiver already coordinates — a
        // second request would re-key with its KeyPackage while the first
        // block's Welcome goes to a dead anchor, stranding the seat
        if self.readmission_in_flight(&member) {
            tracing::warn!(%member, "recovery request while a re-admission is pending - dropped");
            return Ok(Reply::Ack);
        }
        // A-F2: no seat enters a republic in Strict Order Mode on a build
        // without it - it would sign freely beside the lock
        if self.lock_enabled() && !caps.iter().any(|c| c == molt_core::chain::CHAIN_LOCK_CAP) {
            if !self.recovery.attempts.is_spent(&attempt) && proven() {
                let why = crate::chain::LOCK_CHAIN_UNSUPPORTED.to_string();
                self.refuse_recover_request(&member, &sender_npub, why, &attempt);
            }
            return Ok(Reply::Ack);
        }
        // item 7: what the device runs decides at the seal whether it is
        // welcomed (the mode may be enabled while the card is open)
        self.recovery.rejoiner_caps.retain(|(a, _)| *a != attempt);
        self.recovery.rejoiner_caps.push_back((attempt.clone(), caps));
        while self.recovery.rejoiner_caps.len() > REJOINER_CAPS_MAX {
            self.recovery.rejoiner_caps.pop_front();
        }
        // B4: the anchor first, the seal only on the rejoiner's Ready
        if anchor_first
            && self.transport_kind == Some(molt_core::TransportKind::Nostr)
            && !self.seal_first.load(std::sync::atomic::Ordering::SeqCst)
        {
            let held = self.recovery.anchor_first.get(&ticket).map(|a| (a.attempt.clone(), a.started));
            // the same run again: a relay's replay, or a rejoiner that got
            // no piece and cannot name the missing ones
            if held.as_ref().is_some_and(|(a, _)| *a == attempt) {
                self.resend_anchor(&ticket, None, REASK_DEBOUNCE);
                return Ok(Reply::Ack);
            }
            // review R3: an earlier run replayed never replaces a later one
            let older = held.as_ref().is_some_and(|(_, s)| started > 0 && *s > 0 && started < *s);
            if self.recovery.attempts.was_seen(&ticket, &attempt) || older {
                self.note_attempt_seen(&ticket, &attempt);
                tracing::debug!(%member, "recover_request=superseded");
                return Ok(Reply::Ack);
            }
            self.note_attempt_seen(&ticket, &attempt);
            if let Err(e) = self.check_restore_request(
                &member,
                &identity_pk,
                &key_package,
                &ticket,
                &seat_proof,
                &canonical,
                &relays,
                &consent,
            ) {
                // the same rejoiner's newer run, refused: its open attempt is dead
                if self.recovery.anchor_first.get(&ticket).is_some_and(|a| a.new_nostr_pk == canonical) {
                    if let Some(a) = self.recovery.anchor_first.remove(&ticket) {
                        a.abort();
                    }
                }
                self.refuse_recover_request(&member, &sender_npub, e, &attempt);
                return Ok(Reply::Ack);
            }
            let now = tokio::time::Instant::now();
            let run = crate::chain::AnchorFirst {
                member: member.to_string(),
                attempt,
                started,
                identity_pk,
                key_package,
                seat_proof,
                new_nostr_pk: canonical,
                relays,
                consent,
                reply,
                frames: Vec::new(),
                resent_at: Some(now),
                tasks: Vec::new(),
                opened: now,
                touched: now,
                logged_at: Some(now),
                counts: std::sync::Arc::default(),
            };
            // the anchor cannot travel as gift wraps: seal-first
            if let Some(a) = self.start_anchor_first(&ticket, run) {
                self.propose_restore(
                    &member,
                    &a.identity_pk,
                    &a.key_package,
                    &ticket,
                    &a.seat_proof,
                    &a.new_nostr_pk,
                    &a.relays,
                    &a.consent,
                    &a.reply,
                    &sender_npub,
                );
            }
            return Ok(Reply::Ack);
        }
        self.propose_restore(
            &member,
            &identity_pk,
            &key_package,
            &ticket,
            &seat_proof,
            &canonical,
            &relays,
            &consent,
            &reply,
            &sender_npub,
        );
        Ok(Reply::Ack)
    }

    /// Review R10: a frame of `member` stamped `ts` arrived (the stamp is
    /// the sender's, bounded by now).
    pub(crate) fn note_frame_heard(&mut self, member: &str, ts: u64) {
        let at = ts.min(self.presence_now());
        let e = self.recovery.frame_heard.entry(member.to_string()).or_insert(0);
        *e = (*e).max(at);
    }

    /// Review R10: when `member` was last heard, as the restore gate must
    /// read it. A receipt stamped within the catch-up grace after this
    /// open may be a replayed backlog frame of a lost device, so there only
    /// the frame's own stamp counts; otherwise the later of the two.
    pub(crate) fn restore_heard_at(&self, member: &str) -> u64 {
        const CATCH_UP_GRACE_SECS: u64 = 120;
        let framed = self.recovery.frame_heard.get(member).copied().unwrap_or(molt_core::MemberInfo::NEVER);
        let seen = self.member_last_seen(member);
        let opened = self.recovery.opened_at;
        let replayed = opened > 0 && seen >= opened && seen < opened.saturating_add(CATCH_UP_GRACE_SECS);
        if replayed {
            framed
        } else {
            framed.max(seen)
        }
    }

    /// Whether the request's seat proof verifies against the seat's
    /// anchored identity - for any ticket, minted here or not.
    #[allow(clippy::too_many_arguments)] // one request's fields, not a bag
    fn seat_proof_holds(
        &self,
        member: &str,
        identity_pk: &str,
        key_package: &str,
        ticket: &str,
        seat_proof: &str,
        anchor: &str,
        relays: &[String],
    ) -> bool {
        let anchored = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone());
        anchored.is_some_and(|pk| {
            pk == identity_pk
                && crate::founding::verify_seat_proof(
                    &pk,
                    ticket,
                    key_package,
                    &self.republic_id(),
                    anchor,
                    relays,
                    seat_proof,
                )
        })
    }

    fn note_attempt_seen(&mut self, ticket: &str, attempt: &str) {
        let live = self.recovery.tickets.clone();
        self.recovery.attempts.note_seen(ticket, attempt, &live);
    }

    /// Propose a verified request's `Restored` card now (the seal-first
    /// order, or an anchor-first attempt at its Ready), spending the
    /// ticket; a refusal answers the rejoiner. `true` = proposed.
    #[allow(clippy::too_many_arguments)] // one request's fields, not a bag
    fn propose_restore(
        &mut self,
        member: &MemberId,
        identity_pk: &str,
        key_package: &str,
        ticket: &str,
        seat_proof: &str,
        canonical: &str,
        relays: &[String],
        consent: &str,
        reply: &str,
        sender_npub: &str,
    ) -> bool {
        let attempt = molt_net::invite::recover_attempt_id(key_package);
        // NB: on a verified request, verify_and_propose_restore registers the
        // pending recovery BEFORE proposing (a lone coordinator commits the
        // block synchronously inside the propose, which consumes that entry)
        match self.verify_and_propose_restore(
            member,
            identity_pk,
            key_package,
            ticket,
            seat_proof,
            canonical,
            relays,
            consent,
            reply,
        ) {
            Ok(id) => {
                // spend the ticket only on a verified request, so a legitimate
                // member whose first attempt failed (e.g. a truncated proof) can
                // retry on the still-live queue
                self.recovery.tickets.remove(ticket);
                self.recovery.spent_tickets.insert(ticket.to_string());
                // a relay re-delivers the request with the ticket spent: it
                // is the same run, not one to answer
                self.recovery.attempts.note_spent(&attempt);
                self.clear_link_notice(ticket);
                tracing::info!(%member, "recovery seat proof verified - proposing re-admission");
                // the first checklist frame: the rejoiner learns the roster,
                // the threshold and the voices already counted
                self.push_recover_progress(id);
                true
            }
            Err(e) => {
                self.refuse_recover_request(member, sender_npub, e, &attempt);
                false
            }
        }
    }

    /// F1: a link is a bearer secret - once its ticket is spent, withdrawn
    /// or its attempt evicted, the notice stops showing it.
    pub(crate) fn clear_link_notice(&mut self, ticket: &str) {
        let shows = self
            .session
            .notice
            .strip_prefix("recovery-link:")
            .and_then(crate::recovery::RecoveryInvite::parse)
            .is_some_and(|inv| inv.ticket == ticket);
        if shows {
            self.session.notice.clear();
            self.emit_session(SessionScope::Full);
        }
    }

    /// D2: a ticket live again (its run withdrawn) shows its link again.
    pub(crate) fn show_link_notice(&mut self, ticket: &str) {
        if !self.recovery.tickets.contains_key(ticket) {
            return;
        }
        let Some(link) = self.recovery.links.iter().find(|(t, _)| t == ticket).map(|(_, l)| l.clone()) else {
            return;
        };
        let notice = format!("recovery-link:{link}");
        if self.session.notice != notice {
            self.session.notice = notice;
            self.emit_session(SessionScope::Full);
        }
    }

    /// A verified-sender request refused: the operator sees it, and so does
    /// the rejoiner (WP6) - answered only behind the ticket + PoP gates, so
    /// an unknown ticket stays a silent drop; the ticket is NOT spent (the
    /// same link with the right phrase still works).
    /// Item 7: a restore sealed under Strict Order Mode for a device
    /// without it: the device is not welcomed (it would sign freely beside
    /// the lock). Both sides are told; the seat needs a new link once the
    /// device runs a newer build. `true` = withheld.
    pub(crate) fn restore_unwelcomed(&mut self, member: &str, pending: &crate::chain::PendingRecovery) -> bool {
        if !self.lock_enabled() {
            return false;
        }
        let attempt = molt_net::invite::recover_attempt_id(&pending.key_package);
        let caps = self.recovery.rejoiner_caps.iter().find(|(a, _)| *a == attempt).map(|(_, c)| c.clone());
        if caps.is_some_and(|c| c.iter().any(|c| c == molt_core::chain::CHAIN_LOCK_CAP)) {
            return false;
        }
        tracing::warn!(%member, reason = crate::chain::LOCK_CHAIN_UNSUPPORTED, "restore=unwelcomed");
        self.recovery.rejoiner_caps.retain(|(a, _)| *a != attempt);
        self.recovery.tickets.remove(&pending.ticket);
        self.recovery.spent_tickets.insert(pending.ticket.clone());
        let npub = match &pending.card {
            molt_core::ChainChange::Membership { nostr_pk: Some(npk), .. } => npk.clone(),
            _ => String::new(),
        };
        self.refuse_recover_request(member, &npub, crate::chain::LOCK_CHAIN_UNSUPPORTED.to_string(), &attempt);
        self.session.notice = format!("restore-unwelcomed:{member}");
        self.emit_session(SessionScope::Full);
        true
    }

    fn refuse_recover_request(&mut self, member: &str, sender_npub: &str, e: String, attempt: &str) {
        self.session.notice = format!("recover-refused:{member}:{e}");
        self.emit_session(SessionScope::Full);
        tracing::warn!(%member, error = %e, "dropping an invalid recovery request");
        if !sender_npub.is_empty() {
            self.send_recover_frame(
                sender_npub.to_string(),
                molt_net::invite::RitualMsg::RecoverRefused {
                    member: member.to_string(),
                    reason: e,
                    attempt: attempt.to_string(),
                },
            );
        }
    }

    /// B4 step 2: send the anchor to the verified rejoiner's new key and
    /// hold the request until its Ready. Returns the attempt back when the
    /// anchor cannot travel as gift wraps (that request runs seal-first).
    fn start_anchor_first(
        &mut self,
        ticket: &str,
        mut attempt: crate::chain::AnchorFirst,
    ) -> Option<crate::chain::AnchorFirst> {
        let member = attempt.member.clone();
        let Some(frames) = self.recover_anchor_frames() else {
            tracing::warn!(%member, "recover_order=seal_first reason=anchor_too_large");
            return Some(attempt);
        };
        let Some(net) = self.recover_net() else {
            tracing::warn!(%member, "recover_order=seal_first reason=no_relay");
            return Some(attempt);
        };
        let total = u32::try_from(frames.len()).unwrap_or(u32::MAX);
        let msgs = frames
            .iter()
            .zip(0u32..)
            .map(|(event, index)| molt_net::invite::RitualMsg::RecoverAnchor {
                member: member.clone(),
                index,
                total,
                event: event.clone(),
                attempt: attempt.attempt.clone(),
            })
            .collect();
        attempt.frames = frames;
        attempt.tasks.push(crate::nostr_ritual::spawn_recover_frames(
            net,
            attempt.new_nostr_pk.clone(),
            msgs,
            "anchor",
            attempt.counts.clone(),
            self.cmd_tx.clone(),
        ));
        self.admit_anchor_attempt(ticket, attempt);
        tracing::info!(%member, parts = total, waiting_for = "ready", "recover_anchor=sent");
        None
    }

    /// Hold an attempt, replacing the link's earlier one and evicting the
    /// least recently heard beyond [`MAX_ANCHOR_ATTEMPTS`]. An evicted
    /// attempt's next Want/Ready is answered `link outdated`.
    pub(crate) fn admit_anchor_attempt(&mut self, ticket: &str, attempt: crate::chain::AnchorFirst) {
        if let Some(old) = self.recovery.anchor_first.insert(ticket.to_string(), attempt) {
            old.abort();
        }
        while self.recovery.anchor_first.len() > MAX_ANCHOR_ATTEMPTS {
            let Some(oldest) = self
                .recovery
                .anchor_first
                .iter()
                .filter(|(t, _)| t.as_str() != ticket)
                .min_by_key(|(_, a)| a.touched)
                .map(|(t, _)| t.clone())
            else {
                break;
            };
            if let Some(a) = self.recovery.anchor_first.remove(&oldest) {
                tracing::info!(member = %a.member, open = MAX_ANCHOR_ATTEMPTS, "recover_attempt=evicted");
                a.abort();
                self.clear_link_notice(&oldest);
            }
        }
    }

    /// Re-send anchor pieces of an attempt (`None` = all of them), at most
    /// once per `debounce`. The rejoiner was heard: it moves to the back of
    /// the eviction order.
    fn resend_anchor(&mut self, ticket: &str, wanted: Option<&[u32]>, debounce: std::time::Duration) {
        let now = tokio::time::Instant::now();
        let Some(a) = self.recovery.anchor_first.get_mut(ticket) else {
            return;
        };
        a.touched = now;
        if a.resent_at.is_some_and(|t| now.duration_since(t) < debounce) {
            return;
        }
        let total = u32::try_from(a.frames.len()).unwrap_or(u32::MAX);
        let wanted: std::collections::BTreeSet<u32> = match wanted {
            Some(w) => w.iter().copied().filter(|i| *i < total).collect(),
            None => (0..total).collect(),
        };
        let member = a.member.clone();
        let run = a.attempt.clone();
        let msgs: Vec<molt_net::invite::RitualMsg> = wanted
            .iter()
            .filter_map(|i| {
                let event = a.frames.get(usize::try_from(*i).ok()?)?.clone();
                Some(molt_net::invite::RitualMsg::RecoverAnchor {
                    member: member.clone(),
                    index: *i,
                    total,
                    event,
                    attempt: run.clone(),
                })
            })
            .collect();
        if msgs.is_empty() {
            return;
        }
        let to = a.new_nostr_pk.clone();
        let counts = a.counts.clone();
        let Some(net) = self.recover_net() else {
            return;
        };
        let parts = msgs.len();
        let task = crate::nostr_ritual::spawn_recover_frames(net, to, msgs, "anchor", counts, self.cmd_tx.clone());
        if let Some(a) = self.recovery.anchor_first.get_mut(ticket) {
            a.resent_at = Some(now);
            a.tasks.retain(|t| !t.is_finished());
            a.tasks.push(task);
        }
        tracing::info!(%member, parts, "recover_anchor=resent");
    }

    /// Every open anchor-first attempt says, at least once a minute, that
    /// it still waits for the rejoiner's Ready.
    pub(crate) fn recover_wait_tick(&mut self, now: tokio::time::Instant) {
        use std::sync::atomic::Ordering::Relaxed;
        for a in self.recovery.anchor_first.values_mut() {
            if a.logged_at.is_some_and(|t| now.saturating_duration_since(t) < WAIT_LOG_EVERY) {
                continue;
            }
            a.logged_at = Some(now);
            tracing::info!(
                "for" = "ready",
                member = %a.member,
                since_s = now.saturating_duration_since(a.opened).as_secs(),
                sent = a.counts.sent.load(Relaxed),
                accepted = a.counts.accepted.load(Relaxed),
                "recover_wait"
            );
        }
    }

    /// The attempt a Want/Ready names, held for `ticket`: `None` when none
    /// is held, `Some(false)` when the frame is not from that run.
    fn held_run(&self, ticket: &str, member: &str, attempt: &str, sender: &str) -> Option<bool> {
        self.recovery.anchor_first.get(ticket).map(|a| {
            a.member == member && a.new_nostr_pk == sender && (attempt.is_empty() || a.attempt == attempt)
        })
    }

    /// A Want/Ready for an attempt this node does not hold: answered only
    /// for a ticket it minted and has not spent (review R4); anything else
    /// is a relay's replay or a prober, dropped.
    fn answer_unheld(&mut self, member: &str, ticket: &str, attempt: &str, sender: &str) {
        if self.recovery.attempts.minted(ticket) && !self.recovery.spent_tickets.contains(ticket) {
            self.answer_link_outdated(member, attempt, sender);
        } else {
            tracing::debug!(%member, "recover_frame=unknown_ticket");
        }
    }

    /// B4 step 3: the rejoiner misses anchor pieces - re-send them,
    /// debounced. An attempt this node does not hold is `link outdated`.
    pub(crate) fn cmd_net_recover_want(
        &mut self,
        member: &str,
        ticket: &str,
        missing: &[u32],
        attempt: &str,
        sender: &str,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let sender = molt_net::canonical_nostr_pk(sender).unwrap_or_default();
        match self.held_run(ticket, member, attempt, &sender) {
            None => self.answer_unheld(member, ticket, attempt, &sender),
            Some(false) => tracing::debug!(%member, "recover_want=not_from_the_rejoiner"),
            Some(true) => self.resend_anchor(ticket, Some(missing), WANT_DEBOUNCE),
        }
        Ok(Reply::Ack)
    }

    /// B4 step 4: the rejoiner verified the anchor - propose, seal and
    /// re-key now. An attempt this node does not hold is `link outdated`.
    pub(crate) fn cmd_net_recover_ready(
        &mut self,
        member: &str,
        ticket: &str,
        attempt: &str,
        sender: &str,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let sender = molt_net::canonical_nostr_pk(sender).unwrap_or_default();
        match self.held_run(ticket, member, attempt, &sender) {
            None => {
                self.answer_unheld(member, ticket, attempt, &sender);
                return Ok(Reply::Ack);
            }
            Some(false) => {
                tracing::debug!(%member, "recover_ready=not_from_the_rejoiner");
                return Ok(Reply::Ack);
            }
            Some(true) => {}
        }
        let Some(a) = self.recovery.anchor_first.remove(ticket) else {
            return Ok(Reply::Ack);
        };
        let member = member.to_string();
        tracing::info!(%member, "recover_ready=received");
        if !self.recovery.tickets.get(ticket).is_some_and(|m| *m == member) {
            let run = a.attempt.clone();
            a.abort();
            self.answer_link_outdated(&member, &run, &sender);
            return Ok(Reply::Ack);
        }
        if self.readmission_in_flight(&member) {
            let run = a.attempt.clone();
            a.abort();
            self.refuse_recover_request(&member, &sender, "another rejoin of this seat is running".to_string(), &run);
            return Ok(Reply::Ack);
        }
        let seat = (member.clone(), a.new_nostr_pk.clone());
        self.note_anchor_first_seat(seat.clone());
        let proposed = self.propose_restore(
            &member,
            &a.identity_pk,
            &a.key_package,
            ticket,
            &a.seat_proof,
            &a.new_nostr_pk,
            &a.relays,
            &a.consent,
            &a.reply,
            &sender,
        );
        if !proposed {
            self.recovery.anchor_first_seats.remove(&seat);
        }
        a.abort();
        Ok(Reply::Ack)
    }

    /// Remember a seat restored anchor-first, on disk at once.
    pub(crate) fn note_anchor_first_seat(&mut self, seat: (String, String)) {
        if self.recovery.anchor_first_seats.insert(seat) {
            self.snapshot_to_disk();
        }
    }

    /// One answer per key and window: relays replay 1059 wraps on every
    /// resubscribe. The map is pruned and capped on every insert.
    pub(crate) fn answer_once(&mut self, key: (String, String)) -> bool {
        const ANSWER_COOLDOWN_SECS: u64 = 1_800;
        const ANSWERS_MAX: usize = 256;
        let now = crate::now_secs();
        let map = &mut self.recovery.unsolicited_cooldown;
        map.retain(|_, t| now.saturating_sub(*t) < ANSWER_COOLDOWN_SECS);
        if map.contains_key(&key) {
            return false;
        }
        while map.len() >= ANSWERS_MAX {
            let Some(oldest) = map.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone()) else {
                break;
            };
            map.remove(&oldest);
        }
        map.insert(key, now);
        true
    }

    /// `link outdated` to one run: a coordinator that restarted lost its
    /// attempt, or the link's ticket is spent. Callers have authenticated
    /// the run (a seat proof, or a ticket this node minted).
    fn answer_link_outdated(&mut self, member: &str, attempt: &str, sender: &str) {
        if sender.is_empty() || !self.answer_once((format!("outdated:{member}:{attempt}"), sender.to_string())) {
            return;
        }
        tracing::warn!(%member, reason = LINK_OUTDATED, "recover_refused");
        self.send_recover_frame(
            sender.to_string(),
            molt_net::invite::RitualMsg::RecoverRefused {
                member: member.to_string(),
                reason: LINK_OUTDATED.to_string(),
                attempt: attempt.to_string(),
            },
        );
    }

    /// B3: answer a link-less (self-service) request - an older build
    /// restoring itself, its seat proof verified - with `rejoin needs a
    /// link`, once per `(seat, anchor)` and window. Never proposed, so a
    /// live seat is never re-keyed by it.
    fn refuse_unsolicited(&mut self, member: &str, attempt: &str, anchor: &str, sender_npub: &str) {
        let proposed = |p: &crate::chain::PendingRecovery| {
            matches!(&p.card, molt_core::ChainChange::Membership { nostr_pk: Some(a), .. } if a == anchor)
        };
        let ours = self.recovery.pending.get(member).is_some_and(proposed)
            || self.recovery.swept.get(member).is_some_and(proposed);
        if ours || !self.answer_once((member.to_string(), anchor.to_string())) {
            tracing::debug!(%member, "recovery request already answered or proposed - dropped");
            return;
        }
        tracing::warn!(%member, reason = crate::chain::REJOIN_NEEDS_A_LINK, "recover_refused");
        if !sender_npub.is_empty() {
            self.send_recover_frame(
                sender_npub.to_string(),
                molt_net::invite::RitualMsg::RecoverRefused {
                    member: member.to_string(),
                    reason: crate::chain::REJOIN_NEEDS_A_LINK.to_string(),
                    attempt: attempt.to_string(),
                },
            );
        }
    }

    /// A returning member's run failed or was cancelled. With an attempt id
    /// from that run's key it is withdrawn (review R2): an open anchor-first
    /// attempt is dropped, an unsealed card withdrawn. Without one (an
    /// older rejoiner, after its re-admission) it is only logged.
    pub(crate) fn cmd_net_recover_peer_failed(
        &mut self,
        member: &str,
        reason: &str,
        version: &str,
        attempt: &str,
        sender: &str,
    ) -> Result<Reply, MoltError> {
        let canonical = molt_net::canonical_nostr_pk(sender).unwrap_or_default();
        if !attempt.is_empty() && !canonical.is_empty() {
            let open = self
                .recovery
                .anchor_first
                .iter()
                .find(|(_, a)| a.member == member && a.attempt == attempt && a.new_nostr_pk == canonical)
                .map(|(t, _)| t.clone());
            if let Some(ticket) = open {
                if let Some(a) = self.recovery.anchor_first.remove(&ticket) {
                    a.abort();
                }
                self.show_link_notice(&ticket);
                tracing::info!(%member, stage = "anchor", "recover_withdrawn");
                return Ok(Reply::Ack);
            }
            if self.withdraw_restore(member, attempt, &canonical) {
                tracing::info!(%member, stage = "card", "recover_withdrawn");
                return Ok(Reply::Ack);
            }
            if self.restore_abandoned(member, attempt, &canonical) {
                return Ok(Reply::Ack);
            }
        }
        let anchor = self.chain.blocks.iter().rev().find_map(|b| match &b.change {
            molt_core::ChainChange::Membership {
                op: molt_core::MembershipOp::Restored,
                member: m,
                nostr_pk,
                ..
            } if m == member => Some(nostr_pk.clone().unwrap_or_default()),
            _ => None,
        });
        if !anchor.is_some_and(|a| !a.is_empty() && a == canonical) {
            tracing::debug!(member = ?member, "recovery_failed frame=not_from_the_seat");
            return Ok(Reply::Ack);
        }
        let version = if version.is_empty() { "unknown" } else { version };
        tracing::warn!(%member, version, reason, "recovery_failed");
        Ok(Reply::Ack)
    }

    /// F2: remember the run a sealed restore re-keyed for.
    pub(crate) fn note_rekeyed_run(&mut self, member: &str, p: &crate::chain::PendingRecovery) {
        const REKEYED_KEPT: usize = 16;
        let molt_core::ChainChange::Membership { nostr_pk: Some(anchor), .. } = &p.card else {
            return;
        };
        let attempt = molt_net::invite::recover_attempt_id(&p.key_package);
        let at = self.presence_now();
        self.recovery.rekeyed.push_back((member.to_string(), attempt, anchor.clone(), at));
        while self.recovery.rekeyed.len() > REKEYED_KEPT {
            self.recovery.rekeyed.pop_front();
        }
    }

    /// F2: the run this node re-keyed for gave up before its device was
    /// heard - the seat is re-admitted with no device holding its leaf.
    /// Reported once; a new link restores it.
    fn restore_abandoned(&mut self, member: &str, attempt: &str, sender: &str) -> bool {
        let Some(at) = self
            .recovery
            .rekeyed
            .iter()
            .position(|(m, a, k, _)| m == member && a == attempt && k == sender)
        else {
            return false;
        };
        let Some((_, _, _, rekeyed_at)) = self.recovery.rekeyed.remove(at) else {
            return false;
        };
        if self.restore_heard_at(member) > rekeyed_at {
            return false;
        }
        let height = self.chain.blocks.iter().rev().find_map(|b| match &b.change {
            molt_core::ChainChange::Membership { op: molt_core::MembershipOp::Restored, member: m, .. }
                if m == member =>
            {
                Some(b.height)
            }
            _ => None,
        });
        tracing::warn!(%member, height = height.unwrap_or(0), "restore_abandoned");
        self.session.notice = format!("restore-abandoned:{member}");
        self.emit_session(SessionScope::Full);
        true
    }

    /// Report a coordinated recovery's vote state to its waiting rejoiner
    /// (`recovery_auto_approval.md` §4): gift-wrap a `RecoverProgress` frame
    /// to the seat's NEW transport anchor. A no-op unless `id` is a pending
    /// recovery this node coordinates on a Nostr republic (the loopback test
    /// transport carries no progress frames). Best-effort display data —
    /// a lost frame costs a stale checklist, never the recovery.
    pub(crate) fn push_recover_progress(&mut self, id: u64) {
        let Some(report) = self.recover_progress_for(id) else {
            return;
        };
        self.send_recover_progress_frame(report);
    }

    /// The send tail shared by [`Self::push_recover_progress`] (live vote
    /// updates) and the sealed block's completion report
    /// (`after_block_applied`).
    pub(crate) fn send_recover_progress_frame(&mut self, report: crate::chain::RecoverProgressReport) {
        let Some(to) = report.to.clone().filter(|t| !t.is_empty()) else {
            return;
        };
        let msg = molt_net::invite::RitualMsg::RecoverProgress {
            member: report.member,
            need: report.need,
            roster: report.roster,
            approved: report.approved,
            version: env!("CARGO_PKG_VERSION").to_string(),
            attempt: report.attempt,
            sealed: report.sealed,
        };
        self.send_recover_frame(to, msg);
    }

    /// Gift-wrap one recovery-side ritual frame to `to` over the group's
    /// dialable relays — the shared tail of the progress report and the
    /// refusal answer. Nostr only (the loopback test transport carries no
    /// recovery side-channel); best-effort, off the actor.
    fn send_recover_frame(&mut self, to: String, msg: molt_net::invite::RitualMsg) {
        let Some(net) = self.recover_net() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = net.send_ritual(&to, &msg).await {
                tracing::debug!(error = %e, "recovery frame did not publish");
            }
        });
    }

    /// This seat's gift-wrap endpoint over the group's dialable relays;
    /// `None` off Nostr (the loopback test transport has no side channel).
    pub(crate) fn recover_net(&self) -> Option<molt_net::ritual_net::RitualNet> {
        if self.transport_kind != Some(molt_core::TransportKind::Nostr) {
            return None;
        }
        let nostr = self.nostr.as_ref()?;
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return None;
        }
        let dialer = self.dialer_for().ok()?;
        molt_net::ritual_net::RitualNet::new(dialer, relays, &nostr.sk).ok()
    }

    /// Stand the STANDING seat inbox up for the open Nostr workspace
    /// (`detached_reattach.md` §2.1): subscribe this seat's own 1059 anchor
    /// so a restored seat can announce itself without a minted link. Called
    /// wherever the group runtime comes up (open, materialize); replaces a
    /// previous incarnation. A refusal to spawn (no relays, no key) is
    /// quiet — the ticketed link path is unaffected.
    pub(crate) fn spawn_seat_inbox_if_nostr(&mut self) {
        if let Some(task) = self.recovery.seat_inbox.take() {
            task.abort();
        }
        if self.transport_kind != Some(molt_core::TransportKind::Nostr)
            || !self.is_chain_governed()
        {
            return;
        }
        let Some(nostr) = self.nostr.as_ref() else {
            return;
        };
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return;
        }
        let Ok(dialer) = self.dialer_for() else {
            return;
        };
        let Ok(net) = molt_net::ritual_net::RitualNet::new(dialer, relays, &nostr.sk) else {
            return;
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        if self.recovery.opened_at == 0 {
            self.recovery.opened_at = self.presence_now();
        }
        self.recovery.seat_inbox = Some(crate::nostr_ritual::spawn_seat_inbox(
            net,
            self.net_scope,
            cmd_tx.downgrade(),
        ));
    }

    /// A surviving coordinator mints a recovery link for a member who lost its
    /// device (`recovery_ritual.md` §3) — a manually-granted re-admission for an
    /// existing seat. Validate the request against the open chain-governed
    /// workspace, mint a single-use ticket (the spend-once guard registers it),
    /// then provision the dedicated recovery queue off the actor and report the
    /// link. Caller errors (no republic, unknown seat, no chain) reject hard;
    /// OPERATIONAL states report on the recovery notice channel instead — the
    /// mint's real outcome (the link, or a failure) always arrives there, and
    /// the RETURNING member's presence is never involved: the link exists
    /// precisely because that member is unreachable.
    pub(crate) fn cmd_recover_invite_start(
        &mut self,
        member: MemberId,
    ) -> Result<Reply, MoltError> {
        // recovery only exists for a chain-governed republic (the returning
        // member re-verifies the handed-over chain from genesis)
        if !self.is_chain_governed() {
            return Err(MoltError::Recover(
                "recovery needs an open, chain-governed republic".to_string(),
            ));
        }
        // pool-settled gate (the founding/join twin): the minted link names
        // this node's relay pool — minting while a confirmation probe is in
        // flight hands out a link naming a pool about to change
        if !self.pending_relay_confirms.is_empty() {
            return Err(MoltError::Recover(
                crate::relay_msg::pool_verifying_reason().to_string(),
            ));
        }
        let Some(replica) = self.replica.as_ref() else {
            return Err(MoltError::Recover("no republic is open".to_string()));
        };
        // the returning member must be an anchored seat (the seat proof will be
        // checked against this key when the request arrives)
        if !replica.identities.iter().any(|i| i.member == member) {
            return Err(MoltError::Recover(format!(
                "{member} is not a member of this republic"
            )));
        }
        let republic = replica.name.clone();
        let republic_id = replica.republic_id.clone();
        // B3: no link for a seat that is running, nor from a fork
        // D3: a verdict known now answers the command too, not only the notice
        if let Some(why) = self.restore_gate(&member) {
            self.cmd_net_recover_link_failed(member, why.to_string(), String::new(), None)?;
            return Err(MoltError::Recover(why.to_string()));
        }
        // announce the attempt on the recovery notice channel: the frontends
        // render a calm pending state until the real outcome (`recovery-link:`
        // or `recovery-link-failed:`) replaces it — and because pending and
        // outcome always differ, a REPEATED identical outcome still
        // edge-triggers on every attempt
        self.session.notice = format!("recovery-link-pending:{member}");
        self.emit_session(molt_core::SessionScope::Full);
        // N4b step 5: a Nostr republic has no mesh and needs none — the mint
        // wants only a dialer, this seat's transport secret and the group's
        // relays. The discriminator is read FIRST, so a Nostr workspace is
        // never pushed down the queue-shaped path (whose absence of creds is
        // by design, not damage) and never refused with "mesh-not-running".
        if self.transport_kind == Some(molt_core::TransportKind::Nostr) {
            return self.mint_recovery_link_over_relays(member, republic, republic_id);
        }
        // the recovery queue is minted on the RUNTIME transport (a clone shares
        // its Arc, so this node can both create the queue and subscribe to it).
        // No runtime mesh (e.g. the workspace was reopened without a resumable
        // transport) is an operational state of THIS node, not a caller error:
        // ack the decision and report the calm outcome on the notice channel.
        let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) else {
            return self.cmd_net_recover_link_failed(
                member,
                "mesh-not-running".to_string(),
                String::new(),
                None,
            );
        };
        let ticket = molt_net::invite::mint_ticket().map_err(|e| MoltError::Recover(e.to_string()))?;
        let wrap = molt_net::wrap::WrapKey::fresh().map_err(|e| MoltError::Recover(e.to_string()))?;
        // register the ticket BEFORE the queue can carry a request, so the
        // spend-once guard is armed the moment the returning member answers
        self.recovery.tickets.insert(ticket.clone(), member.clone());
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Recover("engine stopped".to_string()));
        };
        let consent_anchor = self.consent_anchor_for(&member);
        crate::recovery::spawn_recovery_provisioning(
            transport,
            member,
            republic,
            republic_id,
            consent_anchor,
            ticket,
            wrap,
            // recovery loops are scoped to the open WORKSPACE (a mesh rebuild
            // mid-recovery must not orphan the minted link)
            self.net_scope,
            cmd_tx,
            self.recovery.material_sink.clone(),
        );
        Ok(Reply::Ack)
    }

    /// The Nostr half of [`Self::cmd_recover_invite_start`] (N4b §8.8 step 5).
    ///
    /// The relay set is the group's list INTERSECTED with what this node may
    /// actually dial. Advertising a relay this coordinator cannot reach would
    /// hand the returning member an address nobody is listening on — and
    /// relays do not federate (`relay_pool.md` §2.6), so "the group uses it"
    /// is not the same question as "I am reachable there". Capped like every
    /// other advertised list.
    ///
    /// Every refusal is an operational state of THIS node, not a caller error:
    /// it rides the recovery notice, never a command error.
    fn mint_recovery_link_over_relays(
        &mut self,
        member: MemberId,
        republic: String,
        republic_id: String,
    ) -> Result<Reply, MoltError> {
        let Some(nostr) = self.nostr.as_ref() else {
            // the kind says Nostr but the material did not load — its own
            // fault, not "no mesh"
            return self.cmd_net_recover_link_failed(
                member,
                "no transport key for this seat".to_string(),
                String::new(),
                None,
            );
        };
        let group_relays = nostr.relays.clone();
        let sk = nostr.sk.clone();
        // `dialer_for`, NOT `resolve_dialer`: the latter writes
        // `session.net_health = Ok` on success, and a Nostr workspace sits at
        // `Down { NOSTR_RUNTIME_PENDING }` on purpose until N5 exists.
        // Minting a link would have turned the pill green for the rest of the
        // session — promising a runtime that is not there. Same choice the
        // founding and join paths already make.
        let dialer = match self.dialer_for() {
            Ok(d) => d,
            Err(e) => {
                return self.cmd_net_recover_link_failed(
                    member,
                    format!("transport: {e}"),
                    String::new(),
                    None,
                )
            }
        };
        let verdicts = molt_core::relay::diagnose_invite_relays(
            &group_relays,
            &self.session.settings.relays,
            self.clearnet_session,
        );
        let relays: Vec<String> = verdicts
            .iter()
            .filter(|v| v.blocked.is_none())
            .map(|v| v.url.clone())
            .take(molt_net::welcome::MAX_PAYLOAD_RELAYS)
            .collect();
        if relays.is_empty() {
            // classified from THESE relays' verdicts — "my pool is empty" and
            // "my pool shares nothing with this republic" are different
            // faults with different fixes, and the whole-pool verdict cannot
            // tell them apart
            let reason = crate::relay_msg::republic_relay_reason(&verdicts);
            return self.cmd_net_recover_link_failed(member, reason, String::new(), None);
        }
        let net = match molt_net::ritual_net::RitualNet::new(dialer, relays, &sk) {
            Ok(n) => n,
            Err(e) => {
                return self.cmd_net_recover_link_failed(
                    member,
                    format!("transport keys: {e}"),
                    String::new(),
                    None,
                )
            }
        };
        // the sender is taken BEFORE the ticket is minted: every lane that
        // registers a ticket must go on to either use it or unregister it,
        // and this one cannot do either
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Recover("engine stopped".to_string()));
        };
        let ticket =
            molt_net::invite::mint_ticket().map_err(|e| MoltError::Recover(e.to_string()))?;
        // register BEFORE the inbox can carry a request, so the spend-once
        // guard is armed the moment the returning member answers
        self.recovery.tickets.insert(ticket.clone(), member.clone());
        self.recovery.attempts.note_minted(&ticket);
        self.snapshot_to_disk();
        // ONE inbox per open workspace. Every mint subscribes the same filter
        // on the same anchor (kind 1059, #p = this seat), so a second
        // subscription would duplicate every delivery and add another set of
        // forever-redialing relay supervisors. The actor validates by TICKET,
        // not by which task delivered the request, so one inbox serves every
        // outstanding link.
        for old in self.recovery.inboxes.drain(..) {
            old.abort();
        }
        // the seat's anchored identity pk rides the link (WP7): the rejoiner
        // needs it to resolve the founder-vs-joiner derivation convention
        let anchored_pk = self
            .replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == member))
            .map(|i| i.identity_pk.clone())
            .unwrap_or_default();
        let consent_anchor = self.consent_anchor_for(&member);
        let task = crate::nostr_ritual::spawn_recovery_inbox(
            net,
            member,
            ticket,
            republic,
            republic_id,
            anchored_pk,
            consent_anchor,
            // recovery loops are scoped to the open WORKSPACE
            self.net_scope,
            cmd_tx.downgrade(),
        );
        // parked so the close path can abort it — a relay subscription does
        // not end on its own the way a dead queue does
        self.recovery.inboxes.push(task);
        Ok(Reply::Ack)
    }

    /// A minted recovery link became available (from the off-actor provisioning
    /// task). Surface it to the operator so it can be shared off-band with the
    /// returning member.
    pub(crate) fn cmd_net_recover_link_ready(
        &mut self,
        member: MemberId,
        link: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        // the link itself is single-use secret material — it goes to the
        // operator surface only, never into the log
        tracing::info!(%member, "recovery link ready");
        if let Some(inv) = crate::recovery::RecoveryInvite::parse(&link) {
            self.recovery.links.retain(|(t, _)| *t != inv.ticket);
            self.recovery.links.push_back((inv.ticket, link.clone()));
            while self.recovery.links.len() > LINKS_KEPT {
                self.recovery.links.pop_front();
            }
        }
        self.session.notice = format!("recovery-link:{link}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Review R8: the minted link's inbox is still not readable - the
    /// pending notice says why, while it is still the one showing.
    pub(crate) fn cmd_net_recover_link_waiting(
        &mut self,
        member: &str,
        reason: &str,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let pending = format!("recovery-link-pending:{member}");
        let shows = self.session.notice == pending
            || self.session.notice.strip_prefix(&pending).is_some_and(|r| r.starts_with(':'));
        let notice = format!("{pending}:{reason}");
        if shows && self.session.notice != notice {
            self.session.notice = notice;
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// A recovery-link mint failed — either synchronously (no runtime mesh,
    /// from [`Self::cmd_recover_invite_start`] itself) or from the off-actor
    /// provisioning task (`Command::NetRecoverLinkFailed`, e.g. the queue
    /// mint failed). Surface the calm `recovery-link-failed:` notice on the
    /// same channel the minted link rides — the operator asked for a link, so
    /// silence would leave it waiting forever — and unregister the dead mint's
    /// ticket (it never left this node; nothing of the attempt stays armed).
    pub(crate) fn cmd_net_recover_link_failed(
        &mut self,
        member: MemberId,
        reason: String,
        ticket: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        if !ticket.is_empty() {
            self.recovery.tickets.remove(&ticket);
            self.recovery.links.retain(|(t, _)| *t != ticket);
        }
        tracing::warn!(%member, %reason, "recovery link mint failed");
        self.session.notice = format!("recovery-link-failed:{reason}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A rejoiner's **mesh announce** arrived on the recovery queue (dynamic
    /// mesh membership, `docs_archive/transport/dynamic_mesh.md` ❷): authenticate the
    /// announcer by MLS decryption and check it is the member whose re-key
    /// just completed, then relay the ciphertext **verbatim** over the runtime
    /// mesh (every survivor authenticates + extends itself) and extend this
    /// node's own mesh toward the rejoiner.
    pub(crate) fn cmd_net_recover_announced(
        &mut self,
        ct: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let Ok(raw) = hex::decode(&ct) else {
            return Ok(Reply::Ack);
        };
        let Some((announcer, plain)) =
            self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
        else {
            tracing::warn!("a recovery-queue mesh announce did not decrypt - dropped");
            return Ok(Reply::Ack);
        };
        // parse BEFORE spending the one-shot window: a malformed (but
        // authentic) announce must degrade to a dropped frame, not burn the
        // rejoiner's only chance to re-mesh (version skew / client bug)
        let Ok(announce) = serde_json::from_slice::<molt_net::mesh::MeshAnnounce>(&plain) else {
            tracing::warn!(%announcer, "mesh announce is malformed - dropped (window kept)");
            return Ok(Reply::Ack);
        };
        // only the member whose re-key JUST completed may (re)announce here —
        // the recovery queue can never re-point another member's links
        if !self.recovery.mesh_window.remove(&announcer) {
            tracing::warn!(%announcer, "mesh announce outside a recovery window - dropped");
            return Ok(Reply::Ack);
        }
        // E7 review finding 1: the rejoiner's NEW incarnation restarts its
        // log seq space (materialize_workspace), while our accept window for
        // it still holds the OLD device's marks — every fresh envelope would
        // read as already-accepted (set bit or aged) and be silently
        // swallowed AND falsely acked. This authenticated, one-shot recovery
        // announce IS the incarnation boundary: forget the old window.
        self.reset_peer_accept_window(&announcer);
        // relay VERBATIM: each survivor decrypts (and thereby authenticates)
        // the announcer itself, exactly like the founding star relay. A
        // recovery re-announce is single-hop over the live mesh (nonce-less —
        // nonce'd announces are the retired rotate relay and are ignored).
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::MeshAnnounced { ct, nonce: None });
        self.record(env);
        // a recovery re-announce is targeted at every survivor — no queue
        // for us here is a real anomaly, so spawn_mesh_extension warns
        self.spawn_mesh_extension(announcer, &announce);
        Ok(Reply::Ack)
    }

    /// Extend this node's running mesh toward `member` (dynamic mesh
    /// membership ❹): create a fresh per-pair inbound queue, reply with our
    /// own MLS-encrypted announce **directly onto the queue `member` announced
    /// for us** (per-queue FIFO puts it ahead of any runtime traffic), and
    /// report the assembled link back as [`Command::NetMeshExtended`]. Off the
    /// actor — queue creation is a live round-trip.
    pub(crate) fn spawn_mesh_extension(
        &mut self,
        member: MemberId,
        announce: &molt_net::mesh::MeshAnnounce,
    ) {
        let me = self.member();
        // send side FIRST — before the cooldown: an announce that carries no
        // queue for this node must not burn the announcer's cooldown slot, or
        // the follow-up announce that IS for us would bounce off "inside the
        // cooldown" for a full window (delivery_guarantee.md V1 — the live
        // 3-node deaf-leg loop). The cooldown guards the expensive path
        // (queue mint + rebuild), which a no-queue-for-us announce never
        // reaches.
        let (snds, wrap_out) = match molt_net::mesh::send_targets(announce, &me) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%member, reason = %e, "mesh announce carries no usable queue for this node");
                return;
            }
        };
        // per-member cooldown: an extension costs a full supervisor
        // teardown+rebuild+fsync on THIS node, so a member re-announcing
        // within the window is ignored (its first announce always passes —
        // recovery and honest rotation are one-shot; only rapid repeats are
        // capped, bounding the churn a misbehaving member can inflict)
        let now = self.presence_now();
        if let Some(last) = self.recovery.mesh_extension_at.get(&member) {
            if now.saturating_sub(*last) < MESH_EXTENSION_COOLDOWN_SECS {
                tracing::warn!(%member, "mesh announce inside the cooldown - ignored");
                return;
            }
        }
        self.recovery.mesh_extension_at.insert(member.clone(), now);
        let Some(net) = self.net.as_ref() else {
            return;
        };
        let (Some(transport), Some(group)) = (net.runtime_transport(), net.group_arc()) else {
            tracing::warn!(%member, "no real runtime mesh to extend");
            return;
        };
        // workspace scope, not mesh generation: a CONCURRENT extension's
        // rebuild must not drop this one's result (both fold into the live net)
        let generation = self.net_scope;
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            // one fresh per-pair inbound queue for the new leg
            let pair = match transport.create_queue().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%member, error = %e, "mesh-extension queue creation failed");
                    return;
                }
            };
            let Ok(wrap_in) = molt_net::WrapKey::fresh() else {
                let _ = transport.delete_queue(&pair.rcv).await;
                return;
            };
            let mut queues = std::collections::BTreeMap::new();
            queues.insert(
                member.clone(),
                molt_net::mesh::QueueHandover::of(&pair.snd, &wrap_in),
            );
            let reply = molt_net::mesh::MeshAnnounce { queues };
            let Ok(bytes) = serde_json::to_vec(&reply) else {
                return;
            };
            // encrypt with the SHARED runtime group (same Arc as the
            // supervisor — one ratchet, used in sequence)
            let Some(ct) = group.lock().ok().and_then(|mut g| g.encrypt(&bytes).ok()) else {
                tracing::warn!(%member, "encrypting the mesh reply failed");
                return;
            };
            let msg = molt_net::invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
            let Ok(payload) = serde_json::to_vec(&msg) else {
                return;
            };
            // the reply goes to the announcer's queue (per-queue FIFO puts it
            // ahead of runtime traffic)
            if let Err(e) = supervisor::send_framed(
                &transport,
                &snds[0],
                &wrap_out,
                molt_net::msg_id(&me, &member, 3),
                &payload,
            )
            .await
            {
                tracing::warn!(%member, error = %e, "sending the mesh reply failed");
                return;
            }
            let link = PeerLink {
                member: member.clone(),
                snds,
                wrap_out,
                rcvs: vec![pair.rcv],
                wrap_in,
            }
            .to_mesh();
            crate::send_internal(
                &cmd_tx,
                Command::NetMeshExtended {
                    link,
                    generation: Some(generation),
                },
            )
            .await;
        });
    }

    /// Fold a freshly assembled per-pair link into the **running** mesh
    /// (dynamic mesh membership ❺): rebuild the supervisor over
    /// `old mesh + link` — replacing any stale link to the same member (a
    /// recovered seat's old queues are dead) — and persist the grown mesh +
    /// crypto so a reopen resumes it. The rebuild IS the reopen path: per-peer
    /// cursors live in `transport.state` and survive it.
    pub(crate) fn cmd_net_mesh_extended(
        &mut self,
        link: molt_core::MeshLink,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        let Some(net) = self.net.as_ref() else {
            return Ok(Reply::Ack);
        };
        if !net.is_real() {
            return Ok(Reply::Ack);
        }
        // everything fallible is hoisted BEFORE the teardown: a failed
        // precondition must leave the old, working mesh standing (the rebuild
        // itself cannot start until the old supervisor is down — a second
        // subscriber on the same queues would supersede the first)
        let member = link.member.clone();
        if PeerLink::from_mesh(&link).is_none() {
            tracing::warn!(%member, "mesh extension link is malformed - keeping the old mesh");
            return Ok(Reply::Ack);
        }
        let mut mesh = net.mesh().to_vec();
        // V8 (delivery_guarantee.md §4.9): the replaced leg's OWN inbound
        // queues die here — collect them for a best-effort server-side delete
        // AFTER the rebuild (they are ours; their undelivered content is
        // covered by the acked-floor rewind, so deleting loses nothing)
        let replaced_rcvs: Vec<molt_net::RcvQueue> = mesh
            .iter()
            .filter(|l| l.member == link.member)
            .filter_map(PeerLink::from_mesh)
            .flat_map(|p| p.rcvs)
            .collect();
        mesh.retain(|l| l.member != link.member);
        mesh.push(link);
        let (Some(transport), Some(group)) = (net.runtime_transport(), net.group_arc()) else {
            return Ok(Reply::Ack);
        };
        if self.active.is_none() {
            return Ok(Reply::Ack);
        }
        // stop the old supervisor, then rebuild over the grown mesh SHARING
        // the live group Arc — no snapshot→restore: a late encrypt by a dying
        // outbox task advances the same ratchet the new supervisor continues
        // from, so sender generations are never rewound/reused (the snapshot
        // variant silently lost one message per peer in that race)
        self.teardown_net();
        if let Some(new_net) = self.build_real_net_shared(transport, &mesh, group.clone()) {
            // the grown mesh must survive a reopen — a LIVE merge (the rebuilt
            // supervisor keeps saving its cursors afterwards, so no seal),
            // snapshotted AFTER the rebuild from the shared group
            let crypto = new_net.crypto_for_close();
            self.net = Some(new_net);
            if let (Some(active), Some((mls, creds))) = (self.active.as_ref(), crypto) {
                if !active.handle.persist_mesh_crypto_blocking(mls, creds, mesh) {
                    tracing::error!("the grown mesh did not reach the disk");
                }
            }
            self.session.notice = format!("mesh-extended:{member}");
            self.emit_session(SessionScope::Full);
            tracing::info!(%member, "mesh extended");
            // V8 queue hygiene: the replaced leg's queues never carried a
            // delete before — every rotate leaked N queues on their servers
            // until idle expiry. Best-effort, off the actor, only after the
            // rebuild committed to the new leg.
            if !replaced_rcvs.is_empty() {
                let transport = self
                    .net
                    .as_ref()
                    .and_then(|n| n.runtime_transport());
                if let Some(transport) = transport {
                    tokio::spawn(async move {
                        for rcv in replaced_rcvs {
                            if let Err(e) = transport.delete_queue(&rcv).await {
                                tracing::debug!(error = %e, "deleting a replaced mesh queue failed (best-effort)");
                            }
                        }
                    });
                }
            }
        } else {
            tracing::warn!(%member, "mesh extension rebuild failed");
        }
        Ok(Reply::Ack)
    }
}
