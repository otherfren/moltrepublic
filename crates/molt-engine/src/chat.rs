// SPDX-License-Identifier: GPL-3.0-or-later

//! Chat: the one ungated surface. Messages are typed [`ChatMessage`]s —
//! the engine mutates and the GUI reads the same struct, and the wire
//! (`read_state.applied`) serializes to the same JSON as before (additive
//! chat-bus fields: `id`, `channel`, `quote_id`).
//!
//! Since the chat bus, every message carries a stable [`MessageId`] minted
//! here (the engine owns the CSPRNG — `molt-core` holds no I/O), and every
//! chat verb (react, delete, download, remove) addresses its target **by
//! id** through the `chat_pos` map — positional indices remain only as the
//! legacy fallback inside the event applier.
//!
//! Handlers follow the S0 shape: validate → build the [`WorkspaceEvent`] →
//! [`State::record`] (apply + persist). Nothing here mutates `self.chat`
//! directly. Fan-out to other members is not chat's business either:
//! `record` publishes to the transport feed and the outbox does the rest
//! (`net.rs`) — on a session-only context that means the loopback demo
//! mesh, whose peers answer through their own engines.

use molt_core::{
    ChannelRef, ChatKind, ChatMessage, Event, FileMeta, MemberId, MessageId, MoltError,
    ProposalState, Reply, WorkspaceEvent,
};

use crate::{now_secs, State};

/// Mint a fresh random message id (chat-bus pin P1: 128-bit CSPRNG, minted
/// by the engine — never `mockrand`, never in `molt-core`).
pub(crate) fn mint_message_id() -> Result<MessageId, MoltError> {
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id)
        .map_err(|e| MoltError::Engine(format!("os rng unavailable: {e}")))?;
    Ok(MessageId(id))
}

/// The one emoji sanity check both surfaces share: a local send
/// (`cmd_react_chat`) rejects with an error, a wire arrival
/// (`cmd_net_delivered`) logs and drops — but what counts as a valid
/// reaction is decided HERE, once. Returns the trimmed emoji, or `None`
/// when empty/oversized.
pub(crate) fn sanitize_emoji(emoji: &str) -> Option<String> {
    let emoji = emoji.trim();
    if emoji.is_empty() || emoji.chars().count() > 4 {
        return None;
    }
    Some(emoji.to_string())
}

impl State {
    /// Refuse a local write into the discussion of a DECIDED vote: a
    /// `Patch` channel is read-only iff its proposal is known here and no
    /// longer `Proposed` — the deliberation ended with the vote. UNKNOWN
    /// patch ids stay writable (chat-bus Q4: a ref may arrive before — or
    /// forever without — its referent, and must never error). Enforced on
    /// the LOCAL send paths only (`cmd_chat`, `cmd_share_file`); the wire
    /// receive path (`net.rs`) stays permissive so logs converge even when
    /// a peer's message was in flight while the vote decided.
    pub(crate) fn ensure_channel_writable(&self, channel: &ChannelRef) -> Result<(), MoltError> {
        if let ChannelRef::Patch { id } = channel {
            if let Some(p) = self.proposals.get(&id.0) {
                if p.state != ProposalState::Proposed {
                    return Err(MoltError::DiscussionClosed(*id, p.state));
                }
            }
        }
        Ok(())
    }

    /// Post as the local member.
    pub(crate) fn cmd_chat(
        &mut self,
        body: String,
        quote: Option<MessageId>,
        channel: ChannelRef,
    ) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let channel = channel.normalized().map_err(MoltError::BadPayload)?;
        self.ensure_channel_writable(&channel)?;
        let from = self.member();
        self.post_message(from, body, quote, channel)?;
        Ok(Reply::Ack)
    }

    /// Build, record and announce one chat message; returns its minted id.
    pub(crate) fn post_message(
        &mut self,
        from: MemberId,
        body: String,
        quote_id: Option<MessageId>,
        channel: ChannelRef,
    ) -> Result<MessageId, MoltError> {
        self.post_message_with_kind(from, body, quote_id, channel, ChatKind::User)
    }

    /// [`State::post_message`], carrying an explicit [`ChatKind`]. Only the
    /// engine mints non-`User` messages (first use: the recovery rejoin
    /// notice in `chain.rs`) — `Command::Chat` always posts `User`, so no
    /// operator can dress a message up as a system line.
    pub(crate) fn post_message_with_kind(
        &mut self,
        from: MemberId,
        body: String,
        quote_id: Option<MessageId>,
        channel: ChannelRef,
        kind: ChatKind,
    ) -> Result<MessageId, MoltError> {
        let id = mint_message_id()?;
        // a quote only sticks when it points at a known message
        let quote_id = quote_id.filter(|q| self.chat_pos.contains_key(q));
        let mut msg = ChatMessage::text(id, from.clone(), body.clone(), now_secs())
            .with_channel(channel.clone())
            .with_kind(kind);
        msg.quote_id = quote_id;
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.emit(Event::Chat {
            id,
            from,
            body,
            channel,
        });
        Ok(id)
    }

    // ---- B2: the seat's own read cursors (buzz_followups.md) ------------
    //
    // One cursor per channel, addressed by MessageId (positions shift under
    // WP4a compaction, ids do not), held by the engine and persisted in
    // prefs.toml — so a restart keeps what was read, and an MCP agent
    // driving the same seat sees the same "what is new" the GUI counts.

    /// Load the persisted cursors for the freshly opened workspace, and
    /// SEED a cursor-less one: the first observation marks everything read
    /// — opening a workspace must not present its whole history as one
    /// unread wall (the old GUI ledger's rule, now engine-side).
    pub(crate) fn adopt_read_cursors(&mut self) {
        self.read_cursors = self
            .active
            .as_ref()
            .map(|a| a.prefs.read_cursors.clone())
            .unwrap_or_default();
        if !self.read_cursors.is_empty() {
            return;
        }
        let newest: Vec<(String, MessageId)> = {
            let mut per: std::collections::HashMap<String, MessageId> =
                std::collections::HashMap::new();
            // log order — the LAST message per channel wins, i.e. the newest
            for m in self.chat_visible() {
                per.insert(m.channel.storage_key(), m.id);
            }
            per.into_iter().collect()
        };
        if newest.is_empty() {
            return;
        }
        for (k, id) in newest {
            self.read_cursors.insert(k, hex::encode(id.0));
        }
        self.persist_read_cursors();
    }

    /// Write the working cursors through to `prefs.toml` (a session-only
    /// workspace keeps them for the session).
    fn persist_read_cursors(&mut self) {
        if let Some(a) = &mut self.active {
            a.prefs.read_cursors = self.read_cursors.clone();
            a.handle.set_prefs(a.prefs.clone());
        }
    }

    /// The channel's read cursor as a LOG position. `None` = no cursor, or
    /// one whose message no longer exists — the seat has not read the
    /// channel since before the retention horizon, so everything visible
    /// counts unread (honest, and exactly what id-addressing buys: a
    /// pruned-away cursor never silently re-points at a shifted position).
    pub(crate) fn read_cursor_pos(&self, channel: &ChannelRef) -> Option<usize> {
        let hexid = self.read_cursors.get(&channel.storage_key())?;
        let raw = hex::decode(hexid).ok()?;
        let arr: [u8; 16] = raw.try_into().ok()?;
        self.chat_pos.get(&MessageId(arr)).copied()
    }

    /// B2: does `m` sit after its channel's read cursor — and is it even
    /// something this seat could have left unread?
    ///
    /// **A seat's OWN message is never unread to itself.** It is read by
    /// definition: the operator wrote it. Counting it put the author's own
    /// words in the channel badge, handed an agent asking "what is new" its
    /// own output back, and kept the GUI's read-marking permanently armed —
    /// every render of a channel this seat had spoken in issued a
    /// `MarkChannelRead`, whose engine event started another render.
    pub(crate) fn chat_msg_unread(&self, m: &molt_core::ChatMessage) -> bool {
        if m.from == self.member() {
            return false;
        }
        match self.read_cursor_pos(&m.channel) {
            None => true,
            Some(c) => self.chat_pos.get(&m.id).map_or(true, |p| *p > c),
        }
    }

    /// B2 — the seat marks a channel read ([`Command::MarkChannelRead`], a
    /// tool on both surfaces). `up_to` empty = through the channel's newest
    /// visible message. The cursor only ever advances — mark-unread is
    /// deliberately not built (B2 step 7: one machine per seat).
    pub(crate) fn cmd_mark_channel_read(
        &mut self,
        channel: ChannelRef,
        up_to: String,
    ) -> Result<Reply, MoltError> {
        let channel = channel.normalized().map_err(MoltError::BadPayload)?;
        let id = if up_to.trim().is_empty() {
            match self.chat_visible().filter(|m| m.channel == channel).last().map(|m| m.id) {
                // an empty channel has nothing to read
                None => return Ok(Reply::Ack),
                Some(id) => id,
            }
        } else {
            let raw = hex::decode(up_to.trim())
                .ok()
                .and_then(|b| <[u8; 16]>::try_from(b).ok())
                .ok_or_else(|| {
                    MoltError::BadPayload("up_to must be a 32-hex message id".into())
                })?;
            let id = MessageId(raw);
            // the id must BE a message of this channel — a foreign cursor
            // would silently read as "everything unread" forever
            let pos = self
                .chat_pos
                .get(&id)
                .copied()
                .ok_or_else(|| MoltError::BadPayload("up_to names no known message".into()))?;
            if self.chat.get(pos).map(|m| &m.channel) != Some(&channel) {
                return Err(MoltError::BadPayload(
                    "up_to is not a message of this channel".into(),
                ));
            }
            if let Some(cur) = self.read_cursor_pos(&channel) {
                if pos <= cur {
                    return Ok(Reply::Ack);
                }
            }
            id
        };
        self.read_cursors.insert(channel.storage_key(), hex::encode(id.0));
        self.persist_read_cursors();
        Ok(Reply::Ack)
    }

    /// Share a local file into the chat: kick the off-actor hash task —
    /// the share message posts (via [`State::cmd_net_file_shared`]) once
    /// the real metadata + sha256 exist. Only metadata enters the chat;
    /// the path stays this node's business (prefs, never wire/log).
    pub(crate) fn cmd_share_file(
        &mut self,
        path: String,
        channel: ChannelRef,
    ) -> Result<Reply, MoltError> {
        // §10.7 re-decided 2026-08-09: a relay republic shares over the
        // kind-447 chunk data plane (`file_transfer_nostr.md`) — the share
        // itself stays metadata-only on every transport, the bytes publish
        // lazily on the first download request
        self.ensure_demo_net();
        let channel = channel.normalized().map_err(MoltError::BadPayload)?;
        self.ensure_channel_writable(&channel)?;
        let path = path.trim().to_string();
        if path.is_empty() {
            return Err(MoltError::BadPayload(
                "the file path must not be empty".into(),
            ));
        }
        let p = std::path::PathBuf::from(&path);
        if p.file_name().is_none() {
            return Err(MoltError::BadPayload(format!(
                "{path:?} has no file name component"
            )));
        }
        // file_cap_bytes = 0 turns sharing off on every transport (FP4)
        let Some(cap) = self.effective_file_cap() else {
            return Err(MoltError::BadPayload(
                "file sharing is off (file_cap_bytes = 0)".into(),
            ));
        };
        // relay plane: an over-cap file is refused where the human who
        // picked it can act — admitting it would mint a share nobody can
        // ever download (the lazy publish refuses on the cap, silently for
        // the group; review 2026-08-10). Metadata only, no read.
        if self.nostr.is_some() {
            if let Ok(meta) = std::fs::metadata(&p) {
                if meta.len() > cap {
                    return Err(MoltError::BadPayload(format!(
                        "file is {} bytes — the share cap is {cap}",
                        meta.len()
                    )));
                }
            }
        }
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Engine("the engine is shutting down".into()));
        };
        crate::transfer::spawn_share_hash(p, channel, self.net_scope, cmd_tx);
        Ok(Reply::Ack)
    }

    /// The off-actor share hash finished: post the share message (the real
    /// metadata + checksum) and remember the source path so this node can
    /// serve downloads — across restarts, via the prefs sidecar. (The arm
    /// mirrors the command's fields one-to-one; bundling them into a struct
    /// would only rename the coupling.) Deliberately NOT re-checked against
    /// `ensure_channel_writable`: the operator's share was admitted at
    /// `cmd_share_file` time — a vote deciding during the hash must not
    /// retro-refuse it (same posture as a wire arrival).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_net_file_shared(
        &mut self,
        name: String,
        size: u64,
        kind: String,
        modified: u64,
        checksum: String,
        path: String,
        channel: ChannelRef,
    ) -> Result<Reply, MoltError> {
        let from = self.member();
        let id = mint_message_id()?;
        let mut msg = ChatMessage::text(id, from.clone(), String::new(), now_secs())
            .with_channel(channel.clone());
        msg.file = Some(FileMeta {
            name: name.clone(),
            size,
            kind,
            modified: if modified == 0 { now_secs() } else { modified },
            available: true,
            checksum,
        });
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.remember_share_path(id, &path);
        self.emit(Event::Chat {
            id,
            from,
            body: format!("📎 {name}"),
            channel,
        });
        Ok(Reply::Ack)
    }

    /// The off-actor share hash failed — surface the honest error.
    pub(crate) fn cmd_net_file_share_failed(
        &mut self,
        name: String,
        reason: String,
    ) -> Result<Reply, MoltError> {
        tracing::warn!(%name, %reason, "sharing a file failed");
        self.session.notice = format!("share-failed:{name}:{reason}");
        self.emit_session(molt_core::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Download a shared file: fetch the bytes peer-to-peer from the
    /// sharer's device (async kickoff — progress and the result arrive as
    /// [`Event::FileTransfer`]). The node's OWN share is an honest local
    /// copy; a workspace without a real mesh gets an honest error.
    pub(crate) fn cmd_download_file(
        &mut self,
        id: MessageId,
        dest: Option<String>,
    ) -> Result<Reply, MoltError> {
        let (from, target) = {
            let (_, msg) = self.chat_by_id(&id)?;
            let file = msg.file.as_ref().ok_or(MoltError::NoFile(id))?;
            if !file.available {
                return Err(MoltError::FileUnavailable(id));
            }
            // uploads are ephemeral like chat: a share that aged out of the
            // retention window left the read contract (it is not in the
            // uploads table any more), so downloading it is refused too —
            // before any task spawns or a download phase is recorded
            if self.chat_ts_aged_out(msg.ts) {
                return Err(MoltError::FileExpired(id));
            }
            (
                msg.from.clone(),
                crate::transfer::FetchTarget {
                    id_hex: id.to_string(),
                    name: file.name.clone(),
                    size: file.size,
                    checksum: file.checksum.clone(),
                },
            )
        };
        if self.downloads.get(&id).is_some_and(|d| {
            d.phase == "requested" || d.phase == "transferring"
        }) {
            return Err(MoltError::BadPayload(
                "this share is already downloading".into(),
            ));
        }
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Engine("the engine is shutting down".into()));
        };
        let dest = crate::transfer::DestSpec {
            explicit: dest,
            default_dir: self.session.settings.download_dir.clone(),
        };
        let me = self.member();
        if from == me {
            // my own share: no network involved — an honest local copy
            let source = self.share_paths.get(&id).cloned().ok_or_else(|| {
                MoltError::Engine(
                    "this node no longer knows the shared file's local path".into(),
                )
            })?;
            crate::transfer::spawn_local_copy(
                source,
                id,
                target,
                dest,
                self.net_scope,
                cmd_tx,
            );
        } else if self.nostr.is_some() {
            // a peer's share on the RELAY data plane: fetch the chunk
            // series when its publish stamp is known, else ask the sharer
            // to publish (lazy — `file_transfer_nostr.md`) and fetch when
            // the FileServed announcement lands
            self.nostr_download(id, target, dest);
        } else {
            // a peer's share: the transfer needs the real mesh
            let (Some(transport), Some(group)) = (
                self.net.as_ref().and_then(|n| n.runtime_transport()),
                self.net.as_ref().and_then(|n| n.group_arc()),
            ) else {
                return Err(MoltError::Engine(
                    "this workspace's members are simulated — no real node holds this file"
                        .into(),
                ));
            };
            crate::transfer::spawn_file_fetch(
                transport,
                group,
                id,
                target,
                dest,
                crate::transfer::FetchTimeouts::default(),
                self.net_scope,
                cmd_tx,
            );
        }
        self.set_download_phase(id, molt_core::TransferPhase::Requested);
        Ok(Reply::Ack)
    }

    /// The sharer deleted the local file: the share flips to unavailable
    /// for everyone, permanently (an event — replay reproduces it).
    pub(crate) fn cmd_remove_file(&mut self, id: MessageId) -> Result<Reply, MoltError> {
        let me = self.member();
        let index = {
            let (index, msg) = self.chat_by_id(&id)?;
            let file = msg.file.as_ref().ok_or(MoltError::NoFile(id))?;
            if msg.from != me {
                return Err(MoltError::NotYourFile(id));
            }
            if !file.available {
                return Err(MoltError::FileUnavailable(id));
            }
            index
        };
        // record_file_remove forgets the source path at the choke point
        self.record_file_remove(index, id, me);
        Ok(Reply::Ack)
    }

    /// Remember one of MY shares' local source path — runtime map + the
    /// per-workspace prefs sidecar (survives restarts; never wire/log).
    fn remember_share_path(&mut self, id: MessageId, path: &str) {
        self.share_paths.insert(id, std::path::PathBuf::from(path));
        if let Some(active) = &mut self.active {
            active
                .prefs
                .shared_files
                .insert(id.to_string(), path.to_string());
            active.handle.set_prefs(active.prefs.clone());
        }
    }

    /// Forget a share's source path (share removed).
    pub(crate) fn forget_share_path(&mut self, id: &MessageId) {
        self.share_paths.remove(id);
        if let Some(active) = &mut self.active {
            if active.prefs.shared_files.remove(&id.to_string()).is_some() {
                active.handle.set_prefs(active.prefs.clone());
            }
        }
    }

    /// Track a download's lifecycle for the uploads view + event stream.
    pub(crate) fn set_download_phase(&mut self, id: MessageId, phase: molt_core::TransferPhase) {
        let view = match &phase {
            molt_core::TransferPhase::Requested => molt_core::DownloadView {
                phase: "requested".to_string(),
                percent: 0,
                path: String::new(),
                error: String::new(),
            },
            molt_core::TransferPhase::Progress { percent } => molt_core::DownloadView {
                phase: "transferring".to_string(),
                percent: *percent,
                path: String::new(),
                error: String::new(),
            },
            molt_core::TransferPhase::Done { path } => molt_core::DownloadView {
                phase: "done".to_string(),
                percent: 100,
                path: path.clone(),
                error: String::new(),
            },
            molt_core::TransferPhase::Failed { reason } => molt_core::DownloadView {
                phase: "failed".to_string(),
                percent: 0,
                path: String::new(),
                error: reason.clone(),
            },
        };
        self.downloads.insert(id, view);
        self.emit(Event::FileTransfer { id, phase });
    }

    /// Toggle the local member's emoji reaction: the emoji you already
    /// picked un-reacts, any other emoji switches — one per member. The
    /// toggle is resolved HERE, against local state, and recorded as an
    /// explicit idempotent [`molt_core::ReactOp`] — the wire may deliver
    /// the event more than once (at-least-once transport), and a duplicate
    /// must never invert the reaction on a peer.
    pub(crate) fn cmd_react_chat(
        &mut self,
        id: MessageId,
        emoji: String,
    ) -> Result<Reply, MoltError> {
        let emoji = sanitize_emoji(&emoji).ok_or_else(|| {
            MoltError::BadPayload("the reaction must be a short emoji".into())
        })?;
        let me = self.member();
        let (index, msg) = self.chat_by_id(&id)?;
        // a tombstone takes no reactions (the applier ignores them anyway —
        // refuse here instead of recording a dead event for every peer)
        if msg.deleted_by.is_some() {
            return Err(MoltError::BadPayload(
                "the message was deleted — a tombstone takes no reactions".into(),
            ));
        }
        // resolve the toggle against local state: same emoji un-reacts,
        // anything else (re)sets — recorded as the explicit op
        let op = if msg.reactions.get(&emoji).is_some_and(|who| who.contains(&me)) {
            molt_core::ReactOp::Remove
        } else {
            molt_core::ReactOp::Add
        };
        self.record_react(index, id, me, emoji, Some(op));
        Ok(Reply::Ack)
    }

    /// Confirm the local member has read these chat messages (read receipts).
    /// Co-equal: the GUI issues it on channel open, an MCP agent explicitly.
    /// While read receipts are disabled locally the node reveals nothing (a
    /// silent no-op). Otherwise it filters to ids this node can honestly
    /// receipt — known, live, `User`-kind, authored by someone else, and not
    /// already read by the local member — and records ONE batched `ChatRead`.
    /// A repeat (same channel reopened) filters to empty and never
    /// re-broadcasts; unknown / own / deleted ids are skipped.
    pub(crate) fn cmd_mark_read(&mut self, ids: Vec<MessageId>) -> Result<Reply, MoltError> {
        if !self.session.settings.read_receipts {
            return Ok(Reply::Ack);
        }
        let me = self.member();
        let mut fresh: Vec<MessageId> = Vec::new();
        for id in ids {
            if fresh.contains(&id) {
                continue; // a duplicate id in the batch
            }
            if let Ok((_, msg)) = self.chat_by_id(&id) {
                if msg.deleted_by.is_none()
                    && msg.kind == molt_core::ChatKind::User
                    && msg.from != me
                    && !msg.read_by.contains(&me)
                {
                    fresh.push(id);
                }
            }
        }
        if fresh.is_empty() {
            return Ok(Reply::Ack); // nothing new to confirm
        }
        self.record_read(fresh, me);
        Ok(Reply::Ack)
    }

    /// Retrieval IS the reading (user decision 2026-08-16): a chat
    /// `ReadState` receipts the foreign messages it returns, so an MCP
    /// agent's poll sends the same honest receipts the GUI sends when it
    /// renders a channel — agents and humans behave the same. Reuses
    /// [`State::cmd_mark_read`]'s rules (own / deleted / system-kind /
    /// already-read are filtered there; silent no-op while this node's
    /// receipts are off). Only what was RETURNED is receipted — a filtered
    /// read (one channel, the unread slice) confirms exactly that slice.
    pub(crate) fn receipt_returned_chat(&mut self, snap: &molt_core::SurfaceSnapshot) {
        if snap.surface != molt_core::Surface::Chat {
            return;
        }
        let ids: Vec<MessageId> = snap
            .applied
            .iter()
            .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
            .filter_map(|s| s.parse().ok())
            .collect();
        if !ids.is_empty() {
            let _ = self.cmd_mark_read(ids);
        }
    }

    /// Wipe one of YOUR OWN messages for everyone; only the deletion notice
    /// remains. Only the author may delete (the P5 "no moderation" posture):
    /// peers enforce exactly this on the wire (`wire_delete` drops a foreign
    /// delete), so honoring a foreign delete locally would fork state —
    /// tombstone here, message intact everywhere else, permanently.
    pub(crate) fn cmd_delete_chat(&mut self, id: MessageId) -> Result<Reply, MoltError> {
        let me = self.member();
        let (index, msg) = self.chat_by_id(&id)?;
        // the caller must be the author in OUR log — wire_delete's mirror
        if msg.from != me {
            return Err(MoltError::NotYourMessage(id));
        }
        self.record_delete(index, id, me);
        Ok(Reply::Ack)
    }

    // ---- the one place each chat verb's event shape exists ---------------
    //
    // Local commands (validated above) and link-authenticated wire arrivals
    // (validated in `net.rs`) both come through these: build the
    // WorkspaceEvent, record it (apply + persist + outbox) and emit the
    // operator event. `by` is the acting identity the CALLER established —
    // the local member here, the link identity on the wire.

    /// Build, record and emit one `ChatReacted` event.
    pub(crate) fn record_react(
        &mut self,
        index: u64,
        id: MessageId,
        by: MemberId,
        emoji: String,
        op: Option<molt_core::ReactOp>,
    ) {
        let env = self.make_env(
            by.clone(),
            WorkspaceEvent::ChatReacted {
                index,
                id: Some(id),
                emoji: emoji.clone(),
                by: by.clone(),
                op,
            },
        );
        self.record(env);
        self.emit(Event::Reacted { id, emoji, by });
    }

    /// Build, record and emit one batched `ChatRead` event. Shared by the
    /// local command (`by` = the local member → self-authored, crosses the
    /// wire) and the wire arm (`by` = the authenticated peer → recorded and
    /// applied locally, never re-broadcast: the outbox feeds only
    /// self-authored events). The caller has already filtered to receiptable
    /// ids.
    pub(crate) fn record_read(&mut self, ids: Vec<MessageId>, by: MemberId) {
        let env = self.make_env(
            by.clone(),
            WorkspaceEvent::ChatRead {
                ids: ids.clone(),
                by: by.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Read { ids, by });
    }

    /// Build, record and emit one `ChatDeleted` event.
    pub(crate) fn record_delete(&mut self, index: u64, id: MessageId, by: MemberId) {
        let env = self.make_env(
            by.clone(),
            WorkspaceEvent::ChatDeleted {
                index,
                id: Some(id),
                by: by.clone(),
            },
        );
        self.record(env);
        // deleting a message drops its file share too — forget any source
        // path so it does not linger in prefs.toml (choke point: covers
        // local + wire deletes, no-op unless it is one of MY shares)
        self.forget_share_path(&id);
        self.emit(Event::Deleted { id, by });
    }

    /// Build, record and emit one `FileRemoved` event.
    pub(crate) fn record_file_remove(&mut self, index: u64, id: MessageId, by: MemberId) {
        let env = self.make_env(
            by.clone(),
            WorkspaceEvent::FileRemoved {
                index,
                id: Some(id),
                by: by.clone(),
            },
        );
        self.record(env);
        // the file is gone — forget its source path (choke point: local
        // remove_file AND a wire FileRemoved both pass through here)
        self.forget_share_path(&id);
        self.emit(Event::FileRemoved { id, by });
    }

    /// Resolve a message id through the id→position map: the position (as
    /// the legacy `index` new events still record for older readers) plus
    /// the message itself. Shared with the wire arms in `net.rs` — one
    /// lookup, one legacy-index derivation.
    pub(crate) fn chat_by_id(&self, id: &MessageId) -> Result<(u64, &ChatMessage), MoltError> {
        let pos = *self
            .chat_pos
            .get(id)
            .ok_or(MoltError::UnknownMessage(*id))?;
        let msg = self.chat.get(pos).ok_or(MoltError::UnknownMessage(*id))?;
        // usize→u64 cannot truncate on any supported target
        let index = u64::try_from(pos).map_err(|_| MoltError::UnknownMessage(*id))?;
        Ok((index, msg))
    }
}

#[cfg(test)]
mod tests {
    use molt_core::{ChatMessage, EventEnvelope, MessageId, MoltError, WorkspaceEvent};

    use crate::tests::plain_state;

    /// Land one peer chat message in the state (the applier path a wire
    /// arrival takes — `plain_state`'s own member is "me").
    fn land_chat(st: &mut crate::State, seq: u64, id: MessageId, from: &str, body: &str) {
        st.apply(&EventEnvelope { prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: from.to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(id, from, body, 100 + seq)),
        });
    }

    /// A member must not tombstone a FOREIGN message: peers reject exactly
    /// that on the wire (`wire_delete`), so honoring it locally would fork
    /// state permanently — one honest click, two diverged republics.
    #[test]
    fn deleting_a_foreign_message_is_refused_and_leaves_state_untouched() {
        let mut st = plain_state();
        let id = MessageId([7u8; 16]);
        land_chat(&mut st, 1, id, "peer-1", "not yours");

        let err = st
            .cmd_delete_chat(id)
            .expect_err("deleting a foreign message must be refused");
        assert!(
            matches!(err, MoltError::NotYourMessage(x) if x == id),
            "unexpected error: {err:?}"
        );
        let dump = st.dump();
        assert_eq!(dump.chat[0].body, "not yours", "the body is untouched");
        assert_eq!(dump.chat[0].deleted_by, None, "no tombstone was written");
    }

    /// The positive control: deleting your OWN message still works.
    #[test]
    fn deleting_your_own_message_still_works() {
        let mut st = plain_state();
        let id = MessageId([8u8; 16]);
        land_chat(&mut st, 1, id, "me", "mine to wipe");

        st.cmd_delete_chat(id).expect("deleting an own message works");
        let dump = st.dump();
        assert_eq!(dump.chat[0].deleted_by.as_deref(), Some("me"));
        assert_eq!(dump.chat[0].body, "", "the body is wiped");
    }

    /// The local toggle UX survives the explicit-op wire contract: the
    /// SENDER resolves the toggle against its own state (first click Add,
    /// second click Remove), so clicking twice un-reacts as before.
    #[test]
    fn reacting_twice_locally_still_toggles_off() {
        let mut st = plain_state();
        let id = MessageId([9u8; 16]);
        land_chat(&mut st, 1, id, "peer-1", "toggle me");

        st.cmd_react_chat(id, "👍".to_string()).expect("first react");
        assert_eq!(st.chat[0].reactions["👍"], vec!["me".to_string()]);
        st.cmd_react_chat(id, "👍".to_string()).expect("second react");
        assert!(
            st.chat[0].reactions.is_empty(),
            "the second click un-reacts: {:?}",
            st.chat[0].reactions
        );
    }

    /// Delivery guarantee G2 (delivery_guarantee.md §4.2): the wire admits
    /// each ENVELOPE exactly once per (sender, seq) — a mesh-rebuild resend
    /// of the same reaction must not toggle it back off (ChatReacted has no
    /// id-level dedup of its own; only the accept window catches this).
    /// A REAL second toggle (a new envelope, higher seq) must still work.
    #[test]
    fn a_resent_reaction_envelope_does_not_toggle_the_reaction_off() {
        let mut st = plain_state();
        let id = MessageId([31u8; 16]);
        land_chat(&mut st, 1, id, "peer-1", "react to me");
        let body = WorkspaceEvent::ChatReacted {
            index: 0,
            id: Some(id),
            emoji: "👍".to_string(),
            by: "peer-2".to_string(),
            op: None,
        };
        deliver(&mut st, "peer-2", 7, body.clone());
        assert_eq!(st.chat[0].reactions["👍"], vec!["peer-2".to_string()]);
        // the SAME envelope again (a resend after a mesh rebuild)
        deliver(&mut st, "peer-2", 7, body.clone());
        assert_eq!(
            st.chat[0].reactions.get("👍").map(|v| v.as_slice()),
            Some(["peer-2".to_string()].as_slice()),
            "a duplicate envelope must not re-toggle the reaction"
        );
        // a genuine second toggle arrives as a NEW envelope and still lands
        deliver(&mut st, "peer-2", 8, body);
        assert!(
            st.chat[0].reactions.is_empty(),
            "a fresh envelope still toggles off: {:?}",
            st.chat[0].reactions
        );
    }

    /// §4.3: every authenticated wire delivery — fresh or duplicate — arms a
    /// debounced ACK to its sender (a dup means the previous ack was lost;
    /// re-acking is what stops the sender's resend loop).
    #[test]
    fn a_wire_delivery_arms_a_debounced_ack_even_for_duplicates() {
        let mut st = plain_state();
        let id = MessageId([33u8; 16]);
        land_chat(&mut st, 1, id, "peer-1", "ack me");
        let body = WorkspaceEvent::ChatReacted {
            index: 0,
            id: Some(id),
            emoji: "👍".to_string(),
            by: "peer-2".to_string(),
            op: Some(molt_core::ReactOp::Add),
        };
        deliver(&mut st, "peer-2", 5, body.clone());
        assert!(st.ack_due.contains_key("peer-2"), "a fresh delivery arms an ack");
        st.ack_due.clear();
        deliver(&mut st, "peer-2", 5, body);
        assert!(st.ack_due.contains_key("peer-2"), "a duplicate re-arms the ack");
    }

    /// E7 finding 1, the semantic pin: a recovered seat's fresh incarnation
    /// re-mints seqs its old device already used — without the reset the
    /// window swallows the new envelopes as duplicates; after
    /// `reset_peer_accept_window` (the survivor's recovery-announce point)
    /// they land again.
    #[test]
    fn a_recovered_seats_reused_seqs_land_again_after_the_window_reset() {
        let mut st = plain_state();
        let id = MessageId([44u8; 16]);
        land_chat(&mut st, 1, id, "peer-1", "target");
        let react = |emoji: &str| WorkspaceEvent::ChatReacted {
            index: 0,
            id: Some(id),
            emoji: emoji.to_string(),
            by: "peer-2".to_string(),
            op: Some(molt_core::ReactOp::Add),
        };
        // the OLD incarnation used seq 6
        deliver(&mut st, "peer-2", 6, react("👍"));
        assert!(st.chat[0].reactions.contains_key("👍"));
        // the NEW incarnation (post-recovery) re-mints seq 6 — swallowed,
        // so the reaction does NOT change (one reaction per member: landing
        // would have swapped 👍 for 🎉)
        deliver(&mut st, "peer-2", 6, react("🎉"));
        assert!(
            st.chat[0].reactions.contains_key("👍") && !st.chat[0].reactions.contains_key("🎉"),
            "without a reset the window swallows the new incarnation's envelope"
        );
        // the survivor's recovery-announce point resets the window
        st.reset_peer_accept_window(&"peer-2".to_string());
        deliver(&mut st, "peer-2", 6, react("🎉"));
        assert!(
            st.chat[0].reactions.contains_key("🎉"),
            "after the reset the new incarnation's envelope lands: {:?}",
            st.chat[0].reactions
        );
    }

    /// G7 (the live 3-node evaluation finding): B must never become visible
    /// before A. A successor whose stamped predecessor is not accepted yet
    /// is parked un-marked (the sender keeps it unacked), a dup of the
    /// parked copy is absorbed, and the predecessor's arrival drains the
    /// park IN ORDER.
    #[test]
    fn a_successor_waits_for_its_predecessor_and_lands_in_order() {
        let mut st = plain_state();
        let chat = |seq: u64, prev: u64, id: u8, body: &str| EventEnvelope {
            seq,
            ts: 200 + seq,
            by: "peer-2".to_string(),
            body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                MessageId([id; 16]),
                "peer-2",
                body,
                200 + seq,
            )),
            prev_seq: prev,
        };
        let deliver_env = |st: &mut crate::State, env: EventEnvelope| {
            st.cmd_net_delivered("peer-2".to_string(), env, None)
                .expect("a wire delivery never errors");
        };
        // the chain start delivers immediately (prev 0)
        deliver_env(&mut st, chat(5, 0, 1, "start"));
        assert_eq!(st.chat.len(), 1);
        // B (seq 12, prev 10) arrives BEFORE A (seq 10) — parked, invisible
        deliver_env(&mut st, chat(12, 10, 3, "B"));
        assert_eq!(st.chat.len(), 1, "B must not become visible before A");
        assert!(
            !st.accepted["peer-2"].is_accepted(12),
            "a parked envelope is NOT accept-marked — the sender keeps resending it"
        );
        // a resent copy of B while parked is absorbed
        deliver_env(&mut st, chat(12, 10, 3, "B"));
        assert_eq!(st.chat.len(), 1);
        // A lands → A applies, then the park drains B — in order
        deliver_env(&mut st, chat(10, 5, 2, "A"));
        assert_eq!(st.chat.len(), 3, "A and the drained B are both visible");
        assert_eq!(st.chat[1].body, "A");
        assert_eq!(st.chat[2].body, "B", "order is the sender's, not arrival");
        assert!(st.accepted["peer-2"].is_accepted(10) && st.accepted["peer-2"].is_accepted(12));
        assert!(st.ordered_park.is_empty(), "the park drained");
    }

    /// A fresh, VISIBLE chat message (recent ts — `land_chat`'s epoch-old
    /// stamps age out of the retention window and the read contract).
    fn land_fresh(st: &mut crate::State, seq: u64, id: u8, body: &str) -> MessageId {
        let ts = crate::now_secs() - 60 + seq;
        let mid = MessageId([id; 16]);
        st.apply(&EventEnvelope {
            prev_seq: 0,
            seq,
            ts,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(mid, "peer-1", body, ts)),
        });
        mid
    }

    /// B2 — the engine-side read cursor: marking a channel read moves the
    /// per-channel unread count and the `"unread"` view slices exactly the
    /// messages AFTER the cursor, in order, by id; and the cursor only ever
    /// advances (mark-unread is deliberately not built).
    #[test]
    fn marking_a_channel_read_counts_and_slices_by_id() {
        let mut st = plain_state();
        let a = land_fresh(&mut st, 1, 1, "a");
        let b = land_fresh(&mut st, 2, 2, "b");
        let c = land_fresh(&mut st, 3, 3, "c");
        let group = molt_core::ChannelRef::Group;
        let unread_bodies = |st: &crate::State| -> Vec<String> {
            st.chat_visible_in(Some("unread")).map(|m| m.body.clone()).collect()
        };
        assert_eq!(unread_bodies(&st), vec!["a", "b", "c"], "no cursor - everything is new");

        st.cmd_mark_channel_read(group.clone(), hex::encode(a.0)).expect("mark a");
        assert_eq!(unread_bodies(&st), vec!["b", "c"], "exactly the messages after the cursor");
        let channels = st.snapshot(molt_core::Surface::Chat, None, None).channels;
        assert_eq!(channels[0].unread, 2, "the channel count agrees with the slice");

        // the cursor only advances: re-marking an OLDER message is a no-op
        st.cmd_mark_channel_read(group.clone(), hex::encode(b.0)).expect("mark b");
        st.cmd_mark_channel_read(group.clone(), hex::encode(a.0)).expect("older is a no-op");
        assert_eq!(unread_bodies(&st), vec!["c"]);

        // empty up_to = through the newest visible message
        st.cmd_mark_channel_read(group.clone(), String::new()).expect("mark all");
        assert!(unread_bodies(&st).is_empty());
        let channels = st.snapshot(molt_core::Surface::Chat, None, None).channels;
        assert_eq!(channels[0].unread, 0);
        let _ = c;
    }

    /// B2 step 3 — the pin that decides id-versus-index: prune BELOW the
    /// cursor and the cursor still resolves to the same logical position.
    /// A positional ledger would shift here (and count wrong in either
    /// direction); the id-addressed cursor does not.
    #[test]
    fn a_read_cursor_survives_compaction_by_id() {
        let mut st = plain_state();
        let _a = land_fresh(&mut st, 1, 1, "a");
        let b = land_fresh(&mut st, 2, 2, "b");
        let _c = land_fresh(&mut st, 3, 3, "c");
        st.cmd_mark_channel_read(molt_core::ChannelRef::Group, hex::encode(b.0))
            .expect("mark b");

        // the WP4a shape: prune below the cursor, rebuild from the dump
        let mut snap = st.dump();
        let cut = st.chat[0].ts + 1; // drops exactly "a"
        assert_eq!(snap.prune_chat_before(cut), 1, "the oldest goes");
        let cursors = st.read_cursors.clone();
        let mut st = plain_state();
        st.restore_dump(snap);
        // the reopen path loads the cursors from prefs (adopt_read_cursors);
        // the dump carries no prefs, so hand them over as the reopen would
        st.read_cursors = cursors;

        let unread: Vec<String> =
            st.chat_visible_in(Some("unread")).map(|m| m.body.clone()).collect();
        assert_eq!(
            unread,
            vec!["c"],
            "after the prune the cursor still means 'read through b' - \
             a positional cursor would have shifted"
        );
    }

    /// §10.7 re-decided 2026-08-09: a relay republic HAS a file data plane
    /// (the kind-447 chunk series, `file_transfer_nostr.md`) — the share is
    /// admitted, metadata-only like on every transport. The e2e proof is
    /// `file_over_relays.rs`; this pins the gate's removal.
    #[test]
    fn sharing_a_file_on_a_relay_republic_is_admitted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        // plain_state drops its command receiver — the share path needs a
        // live one for the off-actor hash task's report
        let (ev_tx, _keep) = tokio::sync::broadcast::channel(8);
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(8);
        // the engine actor holds the strong sender in production — the
        // state keeps only a weak one, so hold a strong clone here
        let _keep_tx = cmd_tx.clone();
        let mut st = crate::State::new(
            molt_core::GroupConfig::demo(),
            molt_core::SessionView::default(),
            ev_tx,
            cmd_tx,
            None,
            false,
            None,
        );
        st.nostr = Some(crate::NostrTransport {
            sk: zeroize::Zeroizing::new(vec![7u8; 32]),
            relays: vec!["ws://relay.example".to_string()],
            rotation_seed: [0u8; 32],
        });
        st.cmd_share_file("/tmp/x.pdf".to_string(), molt_core::ChannelRef::Group)
            .expect("the share is admitted — the hash task reports any IO truth");
    }

    /// G7's fresh-incarnation rule (N4b §3.1a): an envelope from a sender we
    /// hold NO accepted history with delivers immediately, even though its
    /// `prev_seq` points into that history. A rejoiner (or late joiner)
    /// enters the broadcast mid-stream, and the stamped predecessors were
    /// published at epochs its exporter ring can never open — parking would
    /// hold the whole catch-up hostage to frames that cannot exist for it.
    /// Ordering starts at what it CAN see; G7 holds from there on.
    #[test]
    fn a_first_contact_envelope_delivers_without_a_history_to_order_against() {
        let mut st = plain_state();
        let chat = |seq: u64, prev: u64, id: u8, body: &str| EventEnvelope {
            seq,
            ts: 200 + seq,
            by: "peer-2".to_string(),
            body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                MessageId([id; 16]),
                "peer-2",
                body,
                200 + seq,
            )),
            prev_seq: prev,
        };
        let deliver_env = |st: &mut crate::State, env: EventEnvelope| {
            st.cmd_net_delivered("peer-2".to_string(), env, None)
                .expect("a wire delivery never errors");
        };
        // first contact: seq 12 chained onto a history this node never saw
        deliver_env(&mut st, chat(12, 11, 1, "first contact"));
        assert_eq!(st.chat.len(), 1, "nothing to order against — it must deliver");
        assert!(
            st.accepted["peer-2"].is_accepted(12),
            "…and it seeds the window as the ordering baseline"
        );
        // from the baseline on, G7 is fully in force: a successor parks…
        deliver_env(&mut st, chat(14, 13, 3, "B"));
        assert_eq!(st.chat.len(), 1, "post-baseline ordering is not weakened");
        // …until its predecessor lands, then the park drains in order
        deliver_env(&mut st, chat(13, 12, 2, "A"));
        assert_eq!(st.chat.len(), 3);
        assert_eq!(st.chat[1].body, "A");
        assert_eq!(st.chat[2].body, "B");
    }

    /// G7: the chain `make_env` stamps — a second own event links to the
    /// first, an `MlsCommit` never joins the chain (receivers could never
    /// accept it), and a re-recorded PEER event carries no chain at all.
    #[test]
    fn make_env_chains_own_ackable_events_and_skips_commits() {
        let mut st = plain_state();
        let body = |id: u8| {
            WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                MessageId([id; 16]),
                "me",
                "x",
                1,
            ))
        };
        let e1 = st.make_env("me".to_string(), body(1));
        assert_eq!(e1.prev_seq, 0, "the chain starts at zero");
        st.apply(&e1);
        let commit = st.make_env(
            "me".to_string(),
            WorkspaceEvent::MlsCommit { commit: "aa".to_string() },
        );
        st.apply(&commit);
        let e2 = st.make_env("me".to_string(), body(2));
        assert_eq!(
            e2.prev_seq, e1.seq,
            "the chain skips the commit — a receiver can never accept one"
        );
        let foreign = st.make_env("peer-1".to_string(), body(3));
        assert_eq!(foreign.prev_seq, 0, "re-recorded peer events carry no chain");
    }

    /// G7: the recovery window reset also clears the park (its entries chain
    /// into the OLD incarnation's seq space), and the pathology valve
    /// releases a stale entry unordered instead of wedging the sender.
    #[test]
    fn the_park_clears_on_reset_and_releases_stale_entries() {
        let mut st = plain_state();
        st.clock_override = Some(1_750_000_000);
        // seed the window: with a history to order against, G7 parks (the
        // fresh-incarnation rule would otherwise deliver the orphan as a
        // first-contact baseline)
        st.cmd_net_delivered(
            "peer-2".to_string(),
            EventEnvelope {
                seq: 1,
                ts: 201,
                by: "peer-2".to_string(),
                body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    MessageId([6u8; 16]),
                    "peer-2",
                    "baseline",
                    201,
                )),
                prev_seq: 0,
            },
            None,
        )
        .expect("delivered");
        assert_eq!(st.chat.len(), 1);
        let orphan = EventEnvelope {
            seq: 12,
            ts: 212,
            by: "peer-2".to_string(),
            body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                MessageId([7u8; 16]),
                "peer-2",
                "orphan",
                212,
            )),
            prev_seq: 10,
        };
        st.cmd_net_delivered("peer-2".to_string(), orphan.clone(), None)
            .expect("delivered");
        assert_eq!(st.chat.len(), 1, "parked, not applied");
        // a recovery reset forgets the old incarnation's park
        st.reset_peer_accept_window(&"peer-2".to_string());
        assert!(st.ordered_park.is_empty(), "the reset clears the park");

        // the reset also forgot the history, so re-seed before parking again;
        // the valve releases it (loudly) after the giveup window
        st.cmd_net_delivered(
            "peer-2".to_string(),
            EventEnvelope {
                seq: 1,
                ts: 202,
                by: "peer-2".to_string(),
                body: WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                    MessageId([8u8; 16]),
                    "peer-2",
                    "baseline two",
                    202,
                )),
                prev_seq: 0,
            },
            None,
        )
        .expect("delivered");
        assert_eq!(st.chat.len(), 2);
        st.cmd_net_delivered("peer-2".to_string(), orphan, None).expect("delivered");
        st.release_stale_parked(1_750_000_000 + 10);
        assert_eq!(st.chat.len(), 2, "inside the window it stays held");
        st.release_stale_parked(1_750_000_000 + crate::net::ORDERED_PARK_GIVEUP_SECS + 1);
        assert_eq!(st.chat.len(), 3, "the valve releases rather than wedges");
        assert!(st.ordered_park.is_empty());
    }

    // ---- read receipts (Lesebestätigung) ---------------------------------

    /// Apply a read receipt straight through the applier (the recorded path).
    fn land_read(st: &mut crate::State, seq: u64, ids: Vec<MessageId>, by: &str) {
        st.apply(&EventEnvelope { prev_seq: 0,
            seq,
            ts: 200 + seq,
            by: by.to_string(),
            body: WorkspaceEvent::ChatRead {
                ids,
                by: by.to_string(),
            },
        });
    }

    /// Deliver an envelope over the authenticated wire path (the P5 receive
    /// arm). `from` must be a real roster peer (demo: peer-1 / peer-2).
    fn deliver(st: &mut crate::State, from: &str, seq: u64, body: WorkspaceEvent) {
        st.cmd_net_delivered(
            from.to_string(),
            EventEnvelope { prev_seq: 0,
                seq,
                ts: 200 + seq,
                by: from.to_string(),
                body,
            },
            None,
        )
        .expect("a wire delivery never errors");
    }

    fn read_by(st: &crate::State, id: &MessageId) -> Vec<String> {
        st.chat_by_id(id)
            .expect("message present")
            .1
            .read_by
            .iter()
            .cloned()
            .collect()
    }

    /// The send filter: marking read records the local member exactly once
    /// (monotonic), never receipts an OWN or UNKNOWN message, and a repeat
    /// (channel reopened) adds nothing.
    #[test]
    fn mark_read_records_me_once_and_skips_own_and_unknown() {
        let mut st = plain_state();
        let peer = MessageId([21u8; 16]);
        land_chat(&mut st, 1, peer, "peer-1", "read me");
        let mine = MessageId([22u8; 16]);
        land_chat(&mut st, 2, mine, "me", "my own");

        st.cmd_mark_read(vec![peer, mine, MessageId([99u8; 16])])
            .expect("mark read");
        assert_eq!(read_by(&st, &peer), vec!["me".to_string()]);
        assert!(
            read_by(&st, &mine).is_empty(),
            "an own message is never self-receipted"
        );

        st.cmd_mark_read(vec![peer]).expect("mark read again");
        assert_eq!(read_by(&st, &peer), vec!["me".to_string()], "idempotent");
    }

    /// Retrieval IS the reading (user decision 2026-08-16): a ReadState on
    /// the chat surface receipts the foreign messages it RETURNS — an MCP
    /// agent's poll behaves exactly like the GUI rendering the channel, so
    /// agents and humans light the same dots.
    #[test]
    fn a_chat_read_state_receipts_what_it_returns() {
        let mut st = plain_state();
        let peer = MessageId([25u8; 16]);
        land_chat(&mut st, 1, peer, "peer-1", "hello");
        // the harness stamps ancient timestamps — pin them to "unknown age"
        // so the retention window keeps them visible for the read
        for m in &mut st.chat {
            m.ts = 0;
        }
        let snap = st.snapshot(molt_core::Surface::Chat, None, None);
        st.receipt_returned_chat(&snap);
        assert_eq!(read_by(&st, &peer), vec!["me".to_string()]);

        // a non-chat snapshot receipts nothing
        let m2 = MessageId([26u8; 16]);
        land_chat(&mut st, 2, m2, "peer-1", "later");
        for m in &mut st.chat {
            m.ts = 0;
        }
        let org = st.snapshot(molt_core::Surface::Organization, None, None);
        st.receipt_returned_chat(&org);
        assert!(read_by(&st, &m2).is_empty(), "only a chat read receipts");
    }

    /// The local privacy switch: while read receipts are off this node
    /// records and sends nothing.
    #[test]
    fn mark_read_is_a_noop_while_disabled() {
        let mut st = plain_state();
        st.session.settings.read_receipts = false;
        let peer = MessageId([23u8; 16]);
        land_chat(&mut st, 1, peer, "peer-1", "read me");
        st.cmd_mark_read(vec![peer]).expect("mark read");
        assert!(read_by(&st, &peer).is_empty(), "disabled: no receipt");
    }

    /// The applier: a peer receipt inserts them (idempotent), the author
    /// never receipts their own message, a receipt never lands on a
    /// tombstone, and deleting the message clears the receipts with it.
    #[test]
    fn read_by_converges_commutes_with_delete_and_ignores_the_author() {
        let mut st = plain_state();
        let m = MessageId([24u8; 16]);
        land_chat(&mut st, 1, m, "peer-1", "the message");

        // the author's own receipt is ignored
        land_read(&mut st, 2, vec![m], "peer-1");
        assert!(read_by(&st, &m).is_empty());

        // a distinct member reads it — idempotent on redelivery
        land_read(&mut st, 3, vec![m], "peer-2");
        land_read(&mut st, 4, vec![m], "peer-2");
        assert_eq!(read_by(&st, &m), vec!["peer-2".to_string()]);

        // deleting the message (by its author) clears the receipts
        let index = st.chat_by_id(&m).expect("msg").0;
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 5,
            ts: 205,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::ChatDeleted {
                index,
                id: Some(m),
                by: "peer-1".to_string(),
            },
        });
        assert!(read_by(&st, &m).is_empty(), "a tombstone carries no receipts");

        // a receipt arriving AFTER the delete is dropped (commute)
        land_read(&mut st, 6, vec![m], "peer-2");
        assert!(read_by(&st, &m).is_empty());
    }

    /// Over the authenticated wire: a receipt binds to the LINK identity and
    /// converges; one that outruns its message is parked and drains when the
    /// message lands.
    #[test]
    fn a_wire_receipt_binds_to_the_link_and_parks_until_its_message_lands() {
        let mut st = plain_state();
        let m = MessageId([25u8; 16]);

        // the receipt outruns the message: parked, nothing applied yet
        deliver(
            &mut st,
            "peer-2",
            1,
            WorkspaceEvent::ChatRead {
                ids: vec![m],
                by: "peer-2".to_string(),
            },
        );
        assert!(st.chat_by_id(&m).is_err(), "the message has not arrived");

        // the message lands over the wire → the parked receipt drains
        deliver(
            &mut st,
            "peer-1",
            2,
            WorkspaceEvent::Chat(ChatMessage::text(m, "peer-1", "hi", 202)),
        );
        assert_eq!(
            read_by(&st, &m),
            vec!["peer-2".to_string()],
            "the parked receipt applied on arrival, bound to the link identity"
        );
    }
}
