// SPDX-License-Identifier: GPL-3.0-or-later

//! Chat: the one ungated surface. Messages are typed [`ChatMessage`]s —
//! the engine mutates and the GUI reads the same struct, and the wire
//! (`read_state.applied`) serializes to the same JSON as before.
//!
//! Handlers follow the S0 shape: validate → build the [`WorkspaceEvent`] →
//! [`State::record`] (apply + persist). Nothing here mutates `self.chat`
//! directly. Fan-out to other members is not chat's business either:
//! `record` publishes to the transport feed and the outbox does the rest
//! (`net.rs`) — on a session-only context that means the loopback demo
//! mesh, whose peers answer through their own engines.

use std::collections::BTreeMap;

use molt_core::{ChatMessage, Event, FileMeta, MemberId, MoltError, Reply, WorkspaceEvent};

use crate::{now_secs, State};

impl State {
    /// Post as the local member.
    pub(crate) fn cmd_chat(
        &mut self,
        body: String,
        quote: Option<u64>,
    ) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let from = self.member();
        self.post_message(from, body, quote);
        Ok(Reply::Ack)
    }

    pub(crate) fn post_message(&mut self, from: MemberId, body: String, quote: Option<u64>) {
        // a quote only sticks when it points at an existing message
        let quote = quote.filter(|q| usize::try_from(*q).is_ok_and(|q| q < self.chat.len()));
        let msg = ChatMessage {
            from: from.clone(),
            body: body.clone(),
            ts: now_secs(),
            quote,
            reactions: BTreeMap::new(),
            deleted_by: None,
            file: None,
        };
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.emit(Event::Chat { from, body });
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
        let msg = ChatMessage {
            from: from.clone(),
            body: String::new(),
            ts: now_secs(),
            quote: None,
            reactions: BTreeMap::new(),
            deleted_by: None,
            file: Some(FileMeta {
                name: name.clone(),
                size,
                kind: if kind.is_empty() {
                    "File".to_string()
                } else {
                    kind
                },
                modified: if modified == 0 { now_secs() } else { modified },
                available: true,
            }),
        };
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.emit(Event::Chat {
            from,
            body: format!("📎 {name}"),
        });
        Ok(Reply::Ack)
    }

    /// (Mock-)download a shared file from the sharer's disk: validates that
    /// the share exists and is still available; no bytes move until the
    /// transport exists.
    pub(crate) fn cmd_download_file(&self, index: u64) -> Result<Reply, MoltError> {
        let msg = usize::try_from(index)
            .ok()
            .and_then(|i| self.chat.get(i))
            .ok_or(MoltError::UnknownMessage(index))?;
        let file = msg.file.as_ref().ok_or(MoltError::NoFile(index))?;
        if !file.available {
            return Err(MoltError::FileUnavailable(index));
        }
        Ok(Reply::Ack)
    }

    /// The sharer deleted the local file: the share flips to unavailable
    /// for everyone, permanently (an event — replay reproduces it).
    pub(crate) fn cmd_remove_file(&mut self, index: u64) -> Result<Reply, MoltError> {
        let me = self.member();
        {
            let msg = usize::try_from(index)
                .ok()
                .and_then(|i| self.chat.get(i))
                .ok_or(MoltError::UnknownMessage(index))?;
            let file = msg.file.as_ref().ok_or(MoltError::NoFile(index))?;
            if msg.from != me {
                return Err(MoltError::NotYourFile(index));
            }
            if !file.available {
                return Err(MoltError::FileUnavailable(index));
            }
        }
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::FileRemoved {
                index,
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::FileRemoved { index, by: me });
        Ok(Reply::Ack)
    }

    /// Toggle the local member's emoji reaction: the emoji you already
    /// picked un-reacts, any other emoji switches — one per member.
    pub(crate) fn cmd_react_chat(&mut self, index: u64, emoji: String) -> Result<Reply, MoltError> {
        let emoji = emoji.trim().to_string();
        if emoji.is_empty() || emoji.chars().count() > 4 {
            return Err(MoltError::BadPayload(
                "the reaction must be a short emoji".into(),
            ));
        }
        self.check_chat_index(index)?;
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::ChatReacted {
                index,
                emoji: emoji.clone(),
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Reacted {
            index,
            emoji,
            by: me,
        });
        Ok(Reply::Ack)
    }

    /// Wipe a message for everyone; only the deletion notice remains.
    pub(crate) fn cmd_delete_chat(&mut self, index: u64) -> Result<Reply, MoltError> {
        self.check_chat_index(index)?;
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::ChatDeleted {
                index,
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Deleted { index, by: me });
        Ok(Reply::Ack)
    }

    fn check_chat_index(&self, index: u64) -> Result<(), MoltError> {
        if usize::try_from(index).is_ok_and(|i| i < self.chat.len()) {
            Ok(())
        } else {
            Err(MoltError::UnknownMessage(index))
        }
    }
}
