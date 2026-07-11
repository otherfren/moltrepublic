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
    /// A share IS a chat message (concept Q8), so it files under the given
    /// channel view exactly like `cmd_chat` — never hardcoded `Group`.
    pub(crate) fn cmd_share_file(
        &mut self,
        name: String,
        size: u64,
        kind: String,
        modified: u64,
        channel: ChannelRef,
    ) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let channel = channel.normalized().map_err(MoltError::BadPayload)?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(MoltError::BadPayload(
                "the file name must not be empty".into(),
            ));
        }
        let kind = kind.trim().to_string();
        let from = self.member();
        let id = mint_message_id()?;
        let mut msg = ChatMessage::text(id, from.clone(), String::new(), now_secs())
            .with_channel(channel.clone());
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
            channel,
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
        self.record_file_remove(index, id, me);
        Ok(Reply::Ack)
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
        st.apply(&EventEnvelope {
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
}
