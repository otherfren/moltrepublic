// SPDX-License-Identifier: GPL-3.0-or-later

//! Temporary threads (`docs_archive/chat/chat_threads_lifecycle.md`): a topic channel
//! can be closed and reopened by any member (a system line every seat
//! folds the same way), forgotten on one seat, and it ends as a whole once
//! its window of silence passed. The general chat and the vote discussions
//! are untouched.

use std::collections::BTreeMap;

use molt_core::{ChannelRef, ChatKind, ChatMessage, MoltError, Reply, ThreadLife, ThreadOp};

use crate::{now_secs, State};

impl State {
    /// Every topic thread's lifecycle under the effective thread window.
    pub(crate) fn thread_lives(&self) -> BTreeMap<String, ThreadLife> {
        self.thread_lives_with(self.org_effective().thread_retention_days)
    }

    /// [`Self::thread_lives`] under a window the caller already folded.
    pub(crate) fn thread_lives_with(&self, window_days: u64) -> BTreeMap<String, ThreadLife> {
        molt_core::thread_lives(&self.chat, window_days.saturating_mul(86_400))
    }

    /// Whether the thread lifecycle hides `m` at `now`: its thread ended,
    /// the message belongs to an ended incarnation, or this seat forgot
    /// it. Never for the general chat or a vote discussion.
    pub(crate) fn thread_hides(
        &self,
        lives: &BTreeMap<String, ThreadLife>,
        m: &ChatMessage,
        now: u64,
    ) -> bool {
        let ChannelRef::Topic { name } = &m.channel else {
            return false;
        };
        if self.forgotten_hides(name, m) {
            return true;
        }
        lives.get(name).is_some_and(|life| life.expired(now) || life.predates(m))
    }

    /// Whether this seat forgot `m`: it sits at or before the forget mark
    /// in this seat's log. A mark whose message is gone (compacted) hides
    /// nothing - everything it covered went with it.
    pub(crate) fn forgotten_hides(&self, name: &str, m: &ChatMessage) -> bool {
        let Some(mark) = self.forgotten_threads.get(name) else {
            return false;
        };
        match (self.chat_pos.get(mark), self.chat_pos.get(&m.id)) {
            (Some(mark), Some(at)) => at <= mark,
            _ => false,
        }
    }

    /// The visible thread `name` (normalized) and its lifecycle, or
    /// [`MoltError::UnknownThread`]: a thread exists while a message of it
    /// is visible here.
    fn visible_thread(&self, name: String) -> Result<(String, ThreadLife), MoltError> {
        let channel = ChannelRef::Topic { name: name.clone() }
            .normalized()
            .map_err(|_| MoltError::UnknownThread(name.clone()))?;
        let ChannelRef::Topic { name } = channel else {
            return Err(MoltError::UnknownThread(name));
        };
        let visible = self
            .chat_visible()
            .any(|m| matches!(&m.channel, ChannelRef::Topic { name: n } if *n == name));
        let life = self.thread_lives().remove(&name).filter(|_| visible);
        life.map(|life| (name.clone(), life))
            .ok_or(MoltError::UnknownThread(name))
    }

    /// Refuse new speech in a closed thread (this seat's own sends only:
    /// the wire still lands a late message, so seats never diverge).
    pub(crate) fn refuse_closed_thread(&self, channel: &ChannelRef) -> Result<(), MoltError> {
        let ChannelRef::Topic { name } = channel else {
            return Ok(());
        };
        let closed = self
            .thread_lives()
            .get(name)
            .is_some_and(|life| life.closed_by.is_some() && !life.expired(now_secs()));
        if closed {
            return Err(MoltError::ThreadClosed(name.clone()));
        }
        Ok(())
    }

    /// `ThreadClose` / `ThreadReopen`: post the lifecycle line. A no-op
    /// when the thread is already in that state.
    pub(crate) fn cmd_thread_op(&mut self, name: String, op: ThreadOp) -> Result<Reply, MoltError> {
        self.ensure_demo_net();
        let (name, life) = self.visible_thread(name)?;
        if life.closed_by.is_some() == (op == ThreadOp::Close) {
            return Ok(Reply::Ack);
        }
        let body = match op {
            ThreadOp::Close => "closed the thread",
            ThreadOp::Reopen => "reopened the thread",
        };
        let id = crate::chat::mint_message_id()?;
        let from = self.member();
        // strictly after everything this seat saw in the thread: the fold
        // orders by (ts, id), and a close + reopen inside one second must
        // not be decided by the random id
        // …but never past the skew a receiver accepts: a stamp it clamps
        // would order the line differently there than here
        let now = now_secs();
        let ts = now
            .max(life.last_ts.saturating_add(1))
            .min(now.saturating_add(crate::net::WIRE_STAMP_SKEW_SECS));
        let msg = ChatMessage::text(id, from.clone(), body, ts)
            .with_channel(ChannelRef::Topic { name })
            .with_kind(ChatKind::System)
            .with_thread_op(op);
        self.post_chat_message(from, msg);
        Ok(Reply::Sent { id })
    }

    /// `ThreadForget`: hide the thread on this seat, through its newest
    /// message in this seat's log - a later one brings it back. Persisted
    /// in `prefs.toml`.
    pub(crate) fn cmd_thread_forget(&mut self, name: String) -> Result<Reply, MoltError> {
        let (name, _) = self.visible_thread(name)?;
        let newest = self
            .chat
            .iter()
            .rev()
            .find(|m| matches!(&m.channel, ChannelRef::Topic { name: n } if *n == name))
            .map(|m| m.id)
            .ok_or_else(|| MoltError::UnknownThread(name.clone()))?;
        self.forgotten_threads.insert(name, newest);
        self.persist_forgotten_threads();
        // a forget from MCP must reach an open window too
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Drop forget marks that hide nothing any more (the message was
    /// compacted away, or the thread ended anyway), then write the working
    /// copy through to `prefs.toml`.
    pub(crate) fn persist_forgotten_threads(&mut self) {
        let lives = self.thread_lives();
        let now = now_secs();
        let chat_pos = &self.chat_pos;
        self.forgotten_threads.retain(|name, mark| {
            chat_pos.contains_key(mark) && lives.get(name).is_some_and(|life| !life.expired(now))
        });
        let stored: BTreeMap<String, String> = self
            .forgotten_threads
            .iter()
            .map(|(name, id)| (name.clone(), hex::encode(id.0)))
            .collect();
        if let Some(a) = &mut self.active {
            if a.prefs.forgotten_threads != stored {
                a.prefs.forgotten_threads = stored;
                a.handle.set_prefs(a.prefs.clone());
            }
        }
    }
}
