// SPDX-License-Identifier: GPL-3.0-or-later

//! Ingest of an accepted wire envelope: `deliver_gated` is the one match
//! over every `WorkspaceEvent` kind that crosses the wire - chat and its
//! id-addressed verbs, the governance gossip, the chain catch-up frames,
//! the relay file plane's announcements - each arm validating what the
//! link identity may claim before anything is recorded.

use super::*;

/// How far ahead of this node's clock a peer's stamp may claim to be
/// (`FileServed` and wire chat): clock skew, not a licence to date a
/// message into a retention-proof future.
const WIRE_STAMP_SKEW_SECS: u64 = 900;

/// One requester is served a chain catch-up at most this often (review C3).
pub(crate) const CHAIN_SERVE_DEBOUNCE_SECS: u64 = 30;

/// Unknown read-receipt targets one `ChatRead` frame may park: a frame of
/// random ids would otherwise sweep the whole P6 parking buffer (review E6).
pub(crate) const PARKED_READS_PER_FRAME: usize = 16;


impl State {
    /// The post-gate delivery body (the accept point + the kind match) —
    /// called for direct arrivals and for drained G7 park entries alike.
    pub(super) fn deliver_gated(
        &mut self,
        from: MemberId,
        envelope: EventEnvelope,
    ) -> Result<Reply, MoltError> {
        // D7: a decline the FULL park would shed must stay UNACKED — past
        // the accept below the at-least-once guarantee is spent on it and
        // the voice is gone for good; left unmarked, the sender's resend
        // re-earns it once the park has room (successors hold in the G7
        // ordered park, bounded by its give-up valve).
        if let WorkspaceEvent::Declined { id, by, .. } = &envelope.body {
            if by == &from && self.decline_would_shed(id.0, by) {
                tracing::warn!(%from, id = id.0, "decline park full - leaving the frame unacked for the resend");
                return Ok(Reply::Ack);
            }
        }
        // delivery guarantee G2/G3 (delivery_guarantee.md §4.2): THE accept
        // point. Past the generation + roster gates the envelope counts as
        // engine-accepted — mark it in the sender's window (that mark is what
        // the ACK frame reports back, and resends trim against it), and drop
        // it here if the window already holds it (a mesh-rebuild resend /
        // redelivery must never re-apply — the kind-level dedups below only
        // cover Chat and MeshAnnounced). Kind-level IGNORING further down is
        // semantics, not transport loss — it still counts as accepted.
        let fresh = self.accept_envelope(&from, envelope.seq);
        // fresh OR duplicate, the sender is owed a (debounced) ACK: a dup
        // usually means our previous ack was lost or lags, and re-acking is
        // what stops its resend loop (§4.3). `or_insert` keeps the earliest
        // deadline of a burst — one ack answers all of it.
        let due = self.presence_now() + ACK_DEBOUNCE_SECS;
        self.delivery.ack_due.entry(from.clone()).or_insert(due);
        if !fresh {
            tracing::debug!(%from, seq = envelope.seq, "dropping an already-accepted envelope (duplicate/resend)");
            return Ok(Reply::Ack);
        }
        match envelope.body {
            WorkspaceEvent::Chat(mut msg) => {
                msg.from = from.clone(); // defense in depth: the link decides
                msg.quote = None; // sender-local LEGACY index — does not transfer
                                  // (`quote_id`/`channel` are global refs and stay)
                // The channel tag is a CLAIM, not a fact (display routing,
                // never a boundary — nothing engine-side trusts it): run it
                // through the same normalization a local send gets, and
                // COERCE an unnormalizable claim (empty/oversized topic
                // name) to the all-hands `Group` channel instead of
                // dropping the message — a peer's mangled tag must not
                // suppress content anyone was meant to see, and the log
                // keeps its "every stored topic name is normalized"
                // invariant. Same posture for CLOSED discussions: the
                // local send guard (`ensure_channel_writable`) is NOT
                // applied here — a peer's message that was in flight while
                // the vote decided must still land identically on every
                // member (convergence over enforcement).
                msg.channel = msg
                    .channel
                    .normalized()
                    .unwrap_or(molt_core::ChannelRef::Group);
                // a FRESH message carries no stances: reactions, receipts
                // and the tombstone travel as their own link-authenticated
                // events, so inside a body they can only be forged
                // attributions to other members. The stamp is the sender's
                // claim: bound it like `FileServed` (no future beyond the
                // skew window, no "unknown age" that retention never
                // reaches) — reads add the retention to it
                msg.reactions.clear();
                msg.read_by.clear();
                msg.deleted_by = None;
                let now = self.presence_now();
                if msg.ts == 0 || msg.ts > now.saturating_add(WIRE_STAMP_SKEW_SECS) {
                    msg.ts = now;
                }
                // P5: the wire admits each message exactly once, by stable
                // id — a nil id (pre-chat-bus sender) or an already-known
                // id (duplicate / replay / mesh-rebuild resend) is dropped
                if msg.id.is_nil() {
                    tracing::warn!(%from, "dropping a wire chat message without a stable id");
                    return Ok(Reply::Ack);
                }
                if let Some(pos) = self.chat_pos.get(&msg.id) {
                    match self.chat.get(*pos).map(|stored| stored.from.clone()) {
                        // documented v1 limitation ("id squatting"): whoever
                        // lands an id first keeps it — but a cross-AUTHOR
                        // collision is either a bug or an attempt to occupy
                        // a foreign id, so leave an audit trail at WARN
                        Some(stored_author) if stored_author != from => tracing::warn!(
                            %from,
                            %stored_author,
                            id = %msg.id,
                            "dropping a wire chat message whose duplicate id belongs to another author"
                        ),
                        _ => tracing::debug!(%from, id = %msg.id, "dropping a wire chat message with a duplicate id"),
                    }
                    return Ok(Reply::Ack);
                }
                let id = msg.id;
                let channel = msg.channel.clone();
                let body = msg.body.clone();
                let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
                self.record(env);
                self.emit(molt_core::Event::Chat {
                    id,
                    from,
                    body,
                    channel,
                });
                // P6 drain: refs that outran this message were parked under
                // its id — the appliers re-evaluate the P5 rules NOW,
                // against the just-landed link-authenticated message
                // (delete only from its author, file-removal only from its
                // sharer, a reaction always — `by` was forced to the link
                // at park time), through the very same record/emit path a
                // live arrival takes.
                for r in self.parked.drain(&id) {
                    match r {
                        PendingRef::React { by, emoji, op } => self.wire_react(id, by, emoji, op),
                        PendingRef::Delete { by } => self.wire_delete(id, by),
                        PendingRef::FileRemove { by } => self.wire_file_remove(id, by),
                        PendingRef::Read { by } => self.wire_read(id, by),
                    }
                }
            }
            // chat-bus B1, the P5 receive-side matrix: the id-addressed chat
            // verbs. Defense in depth mirrors the Chat arm — the acting
            // member (`by`) is ALWAYS the authenticated link identity, the
            // target resolves by stable id only (a sender-local index never
            // transfers, same posture as the legacy quote), and the recorded
            // event writes the LOCAL position into the legacy `index` field
            // for older readers. An unknown target parks (P6) and re-applies
            // when its message lands — see the Chat arm's drain.
            WorkspaceEvent::ChatReacted { id, emoji, op, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire reaction without a message id");
                    return Ok(Reply::Ack);
                };
                // the local-send sanity check (cmd_react_chat's twin)
                let Some(emoji) = crate::chat::sanitize_emoji(&emoji) else {
                    tracing::warn!(%from, "dropping a wire reaction with a malformed emoji");
                    return Ok(Reply::Ack);
                };
                self.wire_react(id, from, emoji, op);
            }
            WorkspaceEvent::ChatDeleted { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire delete without a message id");
                    return Ok(Reply::Ack);
                };
                self.wire_delete(id, from);
            }
            WorkspaceEvent::FileRemoved { id, .. } => {
                let Some(id) = id else {
                    tracing::debug!(%from, "dropping a wire file-removal without a message id");
                    return Ok(Reply::Ack);
                };
                self.wire_file_remove(id, from);
            }
            // read receipts (batched, id-only). `by` is discarded — the acting
            // member is ALWAYS the link identity, so a peer cannot forge
            // another member's receipt. Known targets record in one batched
            // event; a target that has not arrived here yet parks (P6) and
            // re-applies when its message lands — see the Chat arm's drain.
            WorkspaceEvent::ChatRead { ids, .. } => {
                let mut known: Vec<MessageId> = Vec::new();
                let mut parked = 0usize;
                for id in ids {
                    match self.chat_by_id(&id) {
                        Ok((_, msg)) => {
                            // skip a tombstone or `from`'s own message (commute
                            // with the local apply guard); the rest are receiptable
                            if msg.deleted_by.is_none() && msg.from != from {
                                known.push(id);
                            }
                        }
                        Err(_) if parked < PARKED_READS_PER_FRAME => {
                            parked += 1;
                            self.parked.park(id, PendingRef::Read { by: from.clone() });
                        }
                        // past the per-frame cap: a receipt is ephemeral and
                        // its target may never arrive — dropped, not parked
                        Err(_) => {}
                    }
                }
                if !known.is_empty() {
                    self.record_read(known, from);
                }
            }
            // chain governance gossip + block broadcast — only a chain-governed
            // workspace acts on it (the transport carries it; the chain decides)
            WorkspaceEvent::Proposed { id, surface, payload } if self.is_chain_governed() => {
                // defense in depth: the same two gates the local propose path
                // applies — dropped, never recorded (convergence before
                // enforcement, like every wire guard). Both verdicts are
                // node-independent, so peers agree on what to drop.
                //
                // (1) a payload that cannot be framed inside the transport
                // budget could never become a publishable block; recording it
                // would only let this node approve a change that then wedges
                // the sealer's outbox
                if !crate::proposals::payload_fits(surface, &payload, &self.roster()) {
                    tracing::warn!(from = %from, surface = ?surface, "dropping a proposal too large to publish");
                    return Ok(Reply::Ack);
                }
                // a Files vote must have the shape every seat can check
                // (op, id, identity, stamp) - the approve door then
                // matches it against this seat's own share
                if surface == molt_core::Surface::Files {
                    if let Err(e) = crate::files_state::validate_files_payload(&payload) {
                        tracing::warn!(from = %from, error = %e, "dropping a malformed files proposal");
                        return Ok(Reply::Ack);
                    }
                }
                // (2) a set_image must decode as a picture (WP3); a
                // set_member_image must also be square (the wire twin of
                // the propose gate — one contract, both doors)
                if surface == molt_core::Surface::Organization
                    && payload.get("op").and_then(serde_json::Value::as_str) == Some("set_image")
                    && !crate::proposals::image_bytes(&payload)
                        .is_some_and(|b| crate::proposals::image_decodable(&b).is_ok())
                {
                    tracing::warn!(from = %from, "dropping a set_image proposal without valid, decodable bytes");
                    return Ok(Reply::Ack);
                }
                if surface == molt_core::Surface::Organization
                    && payload.get("op").and_then(serde_json::Value::as_str)
                        == Some("set_member_image")
                    && !crate::proposals::image_bytes(&payload)
                        .is_some_and(|b| crate::proposals::member_image_ok(&b).is_ok())
                {
                    tracing::warn!(from = %from, "dropping a set_member_image proposal without valid, square bytes");
                    return Ok(Reply::Ack);
                }
                // (3) a set_relays with no relay at all could only ever fold
                // as a no-op (the pool must never empty) — also
                // node-independent, so peers agree. The make-before-break
                // overlap rule deliberately does NOT sit here: its verdict
                // depends on this node's fold-state at ingest time, so it is
                // enforced where every holder passes deterministically — the
                // effective-pool fold itself (`fold_pool_edit`).
                if surface == molt_core::Surface::Organization
                    && payload.get("op").and_then(serde_json::Value::as_str) == Some("set_relays")
                    && payload
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .map_or(true, |v| v.split_whitespace().next().is_none())
                {
                    tracing::warn!(from = %from, "dropping a set_relays proposal with an empty pool");
                    return Ok(Reply::Ack);
                }
                // (4) a profile op must name a SEAT. That is the part every
                // holder decides identically — and it is ALL this door can
                // decide. Authorship is not checkable here: WP2's
                // `serve_open_governance` re-serves every open card under the
                // SERVING peer's identity (`make_env(me, body)`), so a
                // legitimate re-serve and a forged claim have the same shape
                // on the wire, and `ProposalRecord.by` is a display hint by
                // design. Dropping on `payload.member != from` therefore
                // blinded exactly the catching-up holder WP2 exists for: it
                // never saw another seat's open profile card, so it could
                // never vote on it. The self-edit rule lives where the seat
                // IS known — the propose gate (`cmd_propose`); past that,
                // the threshold governs, as it does for every org change.
                if surface == molt_core::Surface::Organization
                    && crate::proposals::is_member_profile_op(&payload)
                    && !payload
                        .get("member")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|m| self.roster().iter().any(|s| s == m))
                {
                    let claimed =
                        payload.get("member").and_then(serde_json::Value::as_str).unwrap_or("");
                    tracing::warn!(from = %from, claimed = %claimed, "dropping a profile proposal for an unknown seat");
                    return Ok(Reply::Ack);
                }
                // (5) the description cap is that same one contract: a
                // length the propose gate refuses must not walk in through
                // the wire door (node-independent, like every drop here)
                if surface == molt_core::Surface::Organization
                    && payload.get("op").and_then(serde_json::Value::as_str)
                        == Some("set_member_desc")
                    && crate::proposals::member_desc_ok(
                        payload.get("value").and_then(serde_json::Value::as_str).unwrap_or(""),
                    )
                    .is_err()
                {
                    tracing::warn!(from = %from, "dropping a set_member_desc proposal over the length cap");
                    return Ok(Reply::Ack);
                }
                // announce only a genuinely NEW proposal: a WP2 re-serve or
                // an id-collision refusal must not (re-)ring frontends
                if self.receive_proposed(id.0, surface, payload, &from) {
                    // wake a sleeping agent harness if this seat's vote is
                    // now awaited (debounced; no-op without a wake command)
                    self.maybe_wake_pending(&from);
                    self.emit(molt_core::Event::Proposed { id, surface, by: from });
                    // a retraction that outran the card stands now
                    if matches!(
                        self.register_parked_withdrawal(id.0),
                        crate::proposals::WithdrawOutcome::Withdrawn
                    ) {
                        self.emit(molt_core::Event::Withdrawn { id });
                    }
                    // votes that outran the card (parked declines) stand
                    // now — every drained voice speaks (D4), and a drain
                    // that tips posts the decision line too (D5)
                    let drain = self.register_parked_declines(id.0);
                    for by in drain.voices {
                        self.emit(molt_core::Event::Declined { id, by });
                    }
                    if drain.rejected {
                        self.emit(molt_core::Event::Rejected { id });
                        if let Some((payload, who)) = self
                            .proposals
                            .get(&id.0)
                            .map(|p| (p.payload.clone(), p.declined_by.clone()))
                        {
                            self.post_decision_summary(id.0, &payload, Some(&who));
                        }
                    }
                }
            }
            WorkspaceEvent::Approved { id, by, height, sig } if self.is_chain_governed() => {
                // D2: a LINK-AUTHENTICATED approve clears the member's
                // standing decline (newest stance wins). Gated on by ==
                // from — receive_approval never verifies the signature, so
                // an ungated clear would be a forged veto-cancel.
                if by == from {
                    if let Some(p) = self.proposals.get_mut(&id.0) {
                        if p.state == molt_core::ProposalState::Proposed {
                            p.decliners.retain(|d| d != &by);
                        }
                    }
                }
                self.receive_approval(id.0, &by, height, &sig);
                self.emit(molt_core::Event::Approved {
                    id,
                    have: self.chain_approval_count(id.0),
                    need: self.threshold(),
                });
                // a pending recovery this node coordinates: report the vote
                // to the waiting rejoiner (no-op for every other proposal;
                // a sealed block consumed the pending entry already, and the
                // Welcome supersedes any report)
                self.push_recover_progress(id.0);
            }
            // a decline is a VOTE and crosses the wire like one (see
            // `crosses_wire`) — without this arm it was acked and DROPPED,
            // so a majority-declined proposal stayed pending forever on
            // every node but the decliner's own (live incident 2026-08-09,
            // defect 6). Unlike an approval it carries no signature, so the
            // link identity is the only proof of authorship (`ChatRead`
            // posture): it counts for `from`, and a body claiming another
            // member is dropped, never re-attributed.
            WorkspaceEvent::Declined { id, by, hash } if self.is_chain_governed() => {
                if by != from {
                    tracing::warn!(%from, claimed = %by, "dropping a decline claiming another member");
                    return Ok(Reply::Ack);
                }
                match self.register_decline(id.0, &from, envelope.ts, &hash) {
                    crate::proposals::DeclineOutcome::Rejected => {
                        self.emit(molt_core::Event::Rejected { id });
                        // D5: the WIRE tip posts the decision line too —
                        // under the deterministic summary id, so a second
                        // poster collapses in the duplicate-id drop
                        // (pre-D5 this stayed silent and a vote tipped by
                        // a received decline had no line anywhere)
                        if let Some((payload, who)) = self
                            .proposals
                            .get(&id.0)
                            .map(|p| (p.payload.clone(), p.declined_by.clone()))
                        {
                            self.post_decision_summary(id.0, &payload, Some(&who));
                        }
                    }
                    crate::proposals::DeclineOutcome::Voice => {
                        self.emit(molt_core::Event::Declined { id, by: from });
                    }
                    _ => {}
                }
            }
            // the proposer's retraction crosses like a decline: no
            // signature, so the link identity is the only proof — it must
            // BE the claimed author, and the register checks the recorded
            // proposer on top (a withdraw is proposer-only)
            WorkspaceEvent::Withdrawn { id, by } if self.is_chain_governed() => {
                if by != from {
                    tracing::warn!(%from, claimed = %by, "dropping a withdraw claiming another member");
                    return Ok(Reply::Ack);
                }
                if matches!(
                    self.register_withdraw(id.0, &from, envelope.ts),
                    crate::proposals::WithdrawOutcome::Withdrawn
                ) {
                    self.emit(molt_core::Event::Withdrawn { id });
                }
            }
            WorkspaceEvent::Committed(block) if self.is_chain_governed() => {
                self.receive_block(block);
            }
            WorkspaceEvent::ChainRequest { from_height } if self.is_chain_governed() => {
                tracing::debug!(me = %self.member(), %from, from_height, "chain catch-up request arrived");
                // an AMPLIFIER (review C3): one frame makes every member record
                // the whole chain + blob + open cards. Nothing above the head
                // to serve, and one requester at most once per debounce
                let now = self.presence_now();
                let beyond_head = self
                    .chain.head
                    .as_ref()
                    .is_some_and(|h| from_height > h.height);
                let recently = self
                    .chain.served_at
                    .get(&from)
                    .is_some_and(|t| now.saturating_sub(*t) < CHAIN_SERVE_DEBOUNCE_SECS);
                if beyond_head || recently {
                    tracing::debug!(%from, from_height, beyond_head, recently, "chain catch-up request not served");
                } else {
                    self.chain.served_at.insert(from.clone(), now);
                    self.serve_chain_from(from_height);
                    // WP2: the requester is (re)joining the conversation — beyond
                    // the committed suffix it also lost the ephemeral open
                    // governance state with its RAM, so re-serve that too
                    self.serve_open_governance();
                }
            }
            WorkspaceEvent::MembershipProposed {
                id,
                op,
                nostr_pk,
                member,
                identity_pk,
                relays,
                consent,
            } if self.is_chain_governed() => {
                // D3: the applier (events.rs) mints the human-facing card,
                // but it runs only via record() — the proposer's own log. A
                // RECEIVER therefore held no card, cmd_approve refused with
                // UnknownProposal, and an m>=3 recovery stalled. Re-author
                // and record (the Chat arm's pattern) so the survivor can
                // vote; `by` stays the link identity, never the body's
                // claim. The GATES run first (review 2026-08-25): recording
                // before them persisted a phantom card per frame and let one
                // `id = u64::MAX - 1` poison `next_id` on every node.
                let change = molt_core::ChainChange::Membership {
                    op,
                    member: member.clone(),
                    identity_pk: identity_pk.clone(),
                    nostr_pk: nostr_pk.clone(),
                    relays: relays.clone(),
                    consent: consent.clone(),
                };
                if !self.admits_membership_proposal(id.0, &change) {
                    return Ok(Reply::Ack);
                }
                let env = self.make_env(
                    from.clone(),
                    WorkspaceEvent::MembershipProposed {
                        id,
                        op,
                        nostr_pk: nostr_pk.clone(),
                        member: member.clone(),
                        identity_pk: identity_pk.clone(),
                        relays: relays.clone(),
                        consent: consent.clone(),
                    },
                );
                self.record(env);
                self.receive_membership_proposal(
                    id.0,
                    op,
                    &member,
                    &identity_pk,
                    nostr_pk,
                    relays,
                    consent,
                );
            }
            // WP4b: a peer proposed a compaction cut — recompute the state
            // hash from OUR chain and auto-co-sign only on a match
            // (verify-before-sign; correctness attestation, not a product
            // decision, so no human round-trip)
            WorkspaceEvent::CheckpointProposed { id, upto, state_hash, folded }
                if self.is_chain_governed() =>
            {
                self.receive_checkpoint_proposal(id.0, upto, &state_hash, folded);
            }
            // WP4b: a pruned peer served its blob ahead of the anchor —
            // stash it; the adopt happens hard-verified once the anchor
            // block (and its suffix) arrive as Committed frames
            WorkspaceEvent::CheckpointServed { blob } if self.is_chain_governed() => {
                self.receive_checkpoint_blob(blob);
            }
            // dynamic mesh membership ❸: a relayed mesh announce — authenticate
            // the ANNOUNCER by MLS decryption (the event author is only the
            // relay) and extend this node's own mesh toward it. A nonce'd
            // announce was the retired self-heal rotate-relay broadcast: the
            // field stays parsed (additive-only rule — old logs carry it) but
            // the announce is IGNORED; every current writer mints nonce-less,
            // single-hop announces (recovery relay, bootstrap).
            WorkspaceEvent::MeshAnnounced { ct, nonce } if self.is_chain_governed() => {
                if nonce.is_none() {
                    let me = self.member();
                    if let Ok(raw) = hex::decode(&ct) {
                        if let Some((announcer, plain)) =
                            self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
                        {
                            if announcer != me && self.roster().contains(&announcer) {
                                if let Ok(a) =
                                    serde_json::from_slice::<molt_net::mesh::MeshAnnounce>(&plain)
                                {
                                    self.spawn_mesh_extension(announcer, &a);
                                }
                            }
                        }
                    }
                }
            }
            // a member wants a shared file's bytes: authenticate the
            // REQUESTER by MLS decryption (like a mesh announce), and only
            // the SHARER acts — everyone else in the group decrypts the
            // broadcast and drops it silently. The bytes then flow over the
            // advertised dedicated queue, never through this log.
            // RELAY file plane (`file_transfer_nostr.md`): a member asks
            // the sharer to publish a share's chunk series (lazy), and the
            // sharer's announcement names the series' publish stamp. Both
            // ride the encrypted group log — the link is the authenticator.
            WorkspaceEvent::FileWanted { id } if self.nostr.is_some() => {
                self.serve_file_wanted(id);
            }
            WorkspaceEvent::FileServed { id, at } if self.nostr.is_some() => {
                // trust gates (review 2026-08-10): only the SHARER's own
                // announcement counts — any member could otherwise poison
                // the group's stamp cache with one frame; a far-future
                // stamp names an h-window that holds nothing (forever);
                // and a redelivered OLD announcement must not regress a
                // newer stamp (at-least-once delivery)
                let from_sharer =
                    matches!(self.share_identity(&id), Ok((ident, _)) if ident.by == from);
                let plausible = at <= crate::now_secs().saturating_add(WIRE_STAMP_SKEW_SECS);
                let newer = self.files.series.get(&id).map_or(true, |old| at > *old);
                if from_sharer && plausible && newer {
                    self.files.series.insert(id, at);
                    if let Some((target, dest)) = self.files.pending.remove(&id) {
                        self.spawn_nostr_fetch(id, at, target, dest);
                    }
                    // the mirror worker waited for exactly this stamp
                    if self.files.mirror_pending.remove(&id).is_some() {
                        self.start_mirror(id, at);
                    }
                } else if !from_sharer {
                    tracing::warn!(%from, %id, "dropping a FileServed not from the sharer");
                }
            }
            WorkspaceEvent::FileRequested { ct } => {
                let me = self.member();
                if let Ok(raw) = hex::decode(&ct) {
                    if let Some((requester, plain)) =
                        self.net.as_ref().and_then(|n| n.decrypt_group_message(&raw))
                    {
                        if requester != me && self.roster().contains(&requester) {
                            if let Ok(req) =
                                serde_json::from_slice::<molt_net::transfer::FetchRequest>(&plain)
                            {
                                self.answer_file_request(req);
                            }
                        }
                    }
                }
            }
            other => {
                tracing::debug!(%from, kind = ?std::mem::discriminant(&other), "event over the wire not acted on here");
            }
        }
        Ok(Reply::Ack)
    }
}
