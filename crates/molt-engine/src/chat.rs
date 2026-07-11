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
    ChannelRef, ChatKind, ChatMessage, Event, FileMeta, MemberId, MessageId, MoltError, Reply,
    WorkspaceEvent,
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

impl State {
    /// Post as the local member.
    pub(crate) fn cmd_chat(
        &mut self,
        body: String,
        quote: Option<MessageId>,
        channel: ChannelRef,
    ) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let channel = channel.normalized().map_err(MoltError::BadPayload)?;
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

    /// Share a file into the chat: a message carrying only the metadata.
    /// The bytes stay on this node's disk — participants download from
    /// there while the file exists (mocked until the transport story).
    pub(crate) fn cmd_share_file(
        &mut self,
        name: String,
        size: u64,
        kind: String,
        modified: u64,
    ) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(MoltError::BadPayload(
                "the file name must not be empty".into(),
            ));
        }
        let kind = kind.trim().to_string();
        let from = self.member();
        let id = mint_message_id()?;
        let mut msg = ChatMessage::text(id, from.clone(), String::new(), now_secs());
        msg.file = Some(FileMeta {
            name: name.clone(),
            size,
            kind: if kind.is_empty() {
                "File".to_string()
            } else {
                kind
            },
            modified: if modified == 0 { now_secs() } else { modified },
            available: true,
        });
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.emit(Event::Chat {
            id,
            from,
            body: format!("📎 {name}"),
            channel: ChannelRef::Group,
        });
        Ok(Reply::Ack)
    }

    /// (Mock-)download a shared file from the sharer's disk: validates that
    /// the share exists and is still available; no bytes move until the
    /// transport exists.
    pub(crate) fn cmd_download_file(&self, id: MessageId) -> Result<Reply, MoltError> {
        let (_, msg) = self.chat_by_id(&id)?;
        let file = msg.file.as_ref().ok_or(MoltError::NoFile(id))?;
        if !file.available {
            return Err(MoltError::FileUnavailable(id));
        }
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
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::FileRemoved {
                index,
                id: Some(id),
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::FileRemoved { id, by: me });
        Ok(Reply::Ack)
    }

    /// Toggle the local member's emoji reaction: the emoji you already
    /// picked un-reacts, any other emoji switches — one per member.
    pub(crate) fn cmd_react_chat(
        &mut self,
        id: MessageId,
        emoji: String,
    ) -> Result<Reply, MoltError> {
        let emoji = emoji.trim().to_string();
        if emoji.is_empty() || emoji.chars().count() > 4 {
            return Err(MoltError::BadPayload(
                "the reaction must be a short emoji".into(),
            ));
        }
        let (index, _) = self.chat_by_id(&id)?;
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::ChatReacted {
                index,
                id: Some(id),
                emoji: emoji.clone(),
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Reacted { id, emoji, by: me });
        Ok(Reply::Ack)
    }

    /// Wipe a message for everyone; only the deletion notice remains.
    pub(crate) fn cmd_delete_chat(&mut self, id: MessageId) -> Result<Reply, MoltError> {
        let (index, _) = self.chat_by_id(&id)?;
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::ChatDeleted {
                index,
                id: Some(id),
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Deleted { id, by: me });
        Ok(Reply::Ack)
    }

    /// Resolve a message id through the id→position map: the position (as
    /// the legacy `index` new events still record for older readers) plus
    /// the message itself.
    fn chat_by_id(&self, id: &MessageId) -> Result<(u64, &ChatMessage), MoltError> {
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
