// SPDX-License-Identifier: GPL-3.0-or-later

//! Recovery over the net, coordinator side: the recovery-inbox tasks a
//! minted link listens on, the standing seat inbox, the request ingest
//! (ticketed and self-service lanes), the progress frames to the
//! rejoiner, the post-re-key mesh announce/extension, and the link-mint
//! result handlers. The rejoiner's own task is `crate::recovery`.

use super::*;

/// Minimum seconds between accepted mesh (re-)announces per member — each
/// costs every peer a supervisor teardown+rebuild+fsync (see
/// `State::spawn_mesh_extension`).
const MESH_EXTENSION_COOLDOWN_SECS: u64 = 60;


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
    #[allow(clippy::too_many_arguments)]
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
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        // TWO lanes (`detached_reattach.md` §2.2). Ticketed: the ticket must
        // be a live one this node minted via a recovery link. Self-service:
        // an unknown ticket is a restored seat announcing itself — accepted
        // only on an open chain-governed group, only WITH a consent (it is
        // the authorization), and every failure past here stays a SILENT
        // drop (no refusal frame — an unauthenticated prober gets no oracle).
        // a ticket is bound to the seat it was minted for (review R8): a
        // member holding its own phrase must not spend ANOTHER seat's link
        let ticketed = self
            .recovery.tickets
            .get(&ticket)
            .is_some_and(|minted_for| *minted_for == member);
        if !ticketed {
            if !self.is_chain_governed() || self.group_net.is_none() {
                tracing::debug!(%member, "unsolicited recovery request without an open group - dropped");
                return Ok(Reply::Ack);
            }
            if consent.is_empty() {
                tracing::warn!(%member, "unsolicited recovery request without a consent - dropped");
                return Ok(Reply::Ack);
            }
        }
        // one re-admission at a time, on BOTH lanes (review R3): a pending
        // Restored proposal for this member means another receiver (or an
        // earlier frame of this broadcast) already coordinates — a second,
        // ticketed request would re-key with its KeyPackage while the first
        // block's Welcome goes to a dead anchor, stranding the seat
        if self.chain.proposal_changes.values().any(|c| {
            matches!(c, molt_core::ChainChange::Membership {
                op: molt_core::MembershipOp::Restored,
                member: m,
                ..
            } if m == &member)
        }) {
            tracing::warn!(%member, ticketed, "recovery request while a re-admission is pending - dropped");
            return Ok(Reply::Ack);
        }
        // NB: on a verified request, verify_and_propose_restore registers the
        // pending recovery BEFORE proposing (a lone coordinator commits the
        // block synchronously inside the propose, which consumes that entry)
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
        // …and it must not collide with a seat that already holds it
        if !canonical.is_empty() {
            // the complete register (review C8): founding anchors, every
            // Restored block's anchor and the blob's working anchors
            let taken = self.anchor_seen_in_chain(&canonical);
            if taken {
                tracing::warn!(%member, "recovery request reuses an anchored transport key - dropped");
                return Ok(Reply::Ack);
            }
        }
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
        // self-service cooldown: relays replay 1059 wraps on every
        // resubscribe, and the accept window does not cover them — an
        // accepted (member, anchor) pair is served once per window
        const UNSOLICITED_COOLDOWN_SECS: u64 = 1_800;
        if !ticketed {
            // a request whose "new" anchor ALREADY is the member's working
            // anchor is the LIVE incarnation, replayed by a relay after the
            // cooldown — there is nothing to restore, and re-keying a live
            // seat is pure epoch churn
            if !canonical.is_empty() && self.working_nostr_pk(&member) == canonical {
                tracing::debug!(%member, "unsolicited recovery request for the live anchor - dropped");
                return Ok(Reply::Ack);
            }
            // THE CHAIN IS THE REPLAY REGISTER (field storm 2026-08-24):
            // relays replay every stored request wrap on each resubscribe,
            // and each once-ACCEPTED old request re-keyed the seat onto a
            // DEAD incarnation's anchor — kicking the live one out, forever,
            // in a loop. An anchor that was EVER anchored in this chain
            // (genesis, any Restored block, the checkpoint's summary) can
            // only be a replay: a genuine reattach mints a fresh salt.
            if self.anchor_seen_in_chain(&canonical) {
                tracing::debug!(%member, "unsolicited recovery request replays a chain-known anchor - dropped");
                return Ok(Reply::Ack);
            }
            let now = crate::now_secs();
            let key = (member.to_string(), canonical.clone());
            if self
                .recovery.unsolicited_cooldown
                .get(&key)
                .is_some_and(|t| now.saturating_sub(*t) < UNSOLICITED_COOLDOWN_SECS)
            {
                tracing::debug!(%member, "unsolicited recovery request within the cooldown - dropped");
                return Ok(Reply::Ack);
            }
        }
        match self.verify_and_propose_restore(
            ticketed,
            &member,
            &identity_pk,
            &key_package,
            &ticket,
            &seat_proof,
            &canonical,
            &relays,
            &consent,
            &reply,
        ) {
            Ok(id) => {
                // spend the ticket only on a verified request, so a legitimate
                // member whose first attempt failed (e.g. a truncated proof) can
                // retry on the still-live queue
                if ticketed {
                    self.recovery.tickets.remove(&ticket);
                }
                let now = crate::now_secs();
                self.recovery.unsolicited_cooldown
                    .retain(|_, t| now.saturating_sub(*t) < UNSOLICITED_COOLDOWN_SECS);
                self.recovery.unsolicited_cooldown
                    .insert((member.to_string(), canonical.clone()), now);
                tracing::info!(%member, ticketed, "recovery seat proof verified - proposing re-admission");
                // the first checklist frame: the rejoiner learns the roster,
                // the threshold and the voices already counted
                self.push_recover_progress(id);
            }
            Err(e) if ticketed => {
                // the operator must SEE the refusal (relay-pool mismatch is
                // the common honest cause — R5 names the relay to add); a
                // tracing-only drop left the coordinator staring at a silent
                // screen while the rejoiner waited out its timeout
                self.session.notice = format!("recover-refused:{member}:{e}");
                self.emit_session(SessionScope::Full);
                tracing::warn!(%member, error = %e, "dropping an invalid recovery request");
                // …and so must the REJOINER (WP6, field log 2026-08-23): a
                // wrong phrase looked like a dead coordinator for 15 minutes.
                // Answered only here — behind the ticket + PoP gates — so an
                // unknown ticket stays a silent drop, and the ticket is NOT
                // spent (the same link with the right phrase still works).
                if !sender_npub.is_empty() {
                    self.send_recover_frame(
                        sender_npub.clone(),
                        molt_net::invite::RitualMsg::RecoverRefused {
                            member: member.to_string(),
                            reason: e,
                        },
                    );
                }
            }
            Err(e) => {
                // self-service lane: silent toward the wire (no oracle), one
                // structured line for the operator's log
                tracing::warn!(%member, error = %e, "dropping an invalid unsolicited recovery request");
            }
        }
        Ok(Reply::Ack)
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
        };
        self.send_recover_frame(to, msg);
    }

    /// Gift-wrap one recovery-side ritual frame to `to` over the group's
    /// dialable relays — the shared tail of the progress report and the
    /// refusal answer. Nostr only (the loopback test transport carries no
    /// recovery side-channel); best-effort, off the actor.
    fn send_recover_frame(&mut self, to: String, msg: molt_net::invite::RitualMsg) {
        if self.transport_kind != Some(molt_core::TransportKind::Nostr) {
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
        tokio::spawn(async move {
            if let Err(e) = net.send_ritual(&to, &msg).await {
                tracing::debug!(error = %e, "recovery frame did not publish");
            }
        });
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
        crate::recovery::spawn_recovery_provisioning(
            transport,
            member,
            republic,
            republic_id,
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
        let task = crate::nostr_ritual::spawn_recovery_inbox(
            net,
            member,
            ticket,
            republic,
            republic_id,
            anchored_pk,
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
        self.session.notice = format!("recovery-link:{link}");
        self.emit_session(molt_core::SessionScope::Full);
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
            let (reply_tx, _rx) = oneshot::channel();
            let _ = cmd_tx
                .send(Envelope {
                    cmd: Command::NetMeshExtended {
                        link,
                        generation: Some(generation),
                    },
                    reply: reply_tx,
                })
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
