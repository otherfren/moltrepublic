// SPDX-License-Identifier: GPL-3.0-or-later

//! Chat: the one ungated surface. Messages are typed [`ChatMessage`]s —
//! the engine mutates and the GUI reads the same struct, and the wire
//! (`read_state.applied`) serializes to the same JSON as before.
//!
//! Handlers follow the S0 shape: validate → build the [`WorkspaceEvent`] →
//! [`State::record`] (apply + persist). Nothing here mutates `self.chat`
//! directly.

use std::collections::BTreeMap;

use molt_core::{
    mockrand, ChatMessage, Command, Event, MemberId, MoltError, Reply, WorkspaceEvent,
};

use crate::{now_secs, Envelope, State};

impl State {
    /// Post as the local member; the demo answers with 0–2 simulated replies.
    pub(crate) fn cmd_chat(
        &mut self,
        body: String,
        quote: Option<u64>,
    ) -> Result<Reply, MoltError> {
        let from = self.member();
        let trigger = self.chat.len();
        self.post_message(from, body, quote);
        self.spawn_sim_replies(trigger);
        Ok(Reply::Ack)
    }

    /// Post as another member. Engine-internal (the reply simulator); not an
    /// MCP tool — exposing it would allow member impersonation.
    pub(crate) fn cmd_chat_from(
        &mut self,
        from: MemberId,
        body: String,
        quote: Option<u64>,
    ) -> Result<Reply, MoltError> {
        self.post_message(from, body, quote);
        Ok(Reply::Ack)
    }

    fn post_message(&mut self, from: MemberId, body: String, quote: Option<u64>) {
        // a quote only sticks when it points at an existing message
        let quote = quote.filter(|q| usize::try_from(*q).is_ok_and(|q| q < self.chat.len()));
        let msg = ChatMessage {
            from: from.clone(),
            body: body.clone(),
            ts: now_secs(),
            quote,
            reactions: BTreeMap::new(),
            deleted_by: None,
        };
        let env = self.make_env(from.clone(), WorkspaceEvent::Chat(msg));
        self.record(env);
        self.emit(Event::Chat { from, body });
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

    /// Simulate the rest of the republic: a few seconds after the local
    /// member posts, 0–2 other members answer with a canned line each (half
    /// of them quoting the message they answer). The repliers come from the
    /// active workspace's roster (offline members stay silent), or from the
    /// group config when nothing is open.
    ///
    /// **Session-only workspaces only.** A persisted workspace's log is the
    /// authoritative shared history — a canned reply recorded there would
    /// replay forever as a real message from a member who never spoke.
    pub(crate) fn spawn_sim_replies(&self, trigger: usize) {
        if self.active.is_some() {
            return;
        }
        const LINES: [&str; 16] = [
            "sounds good to me",
            "can someone double-check the numbers?",
            "+1",
            "i'll take that quest tomorrow",
            "did anyone hear back from the notary?",
            "lol",
            "agreed, let's move on",
            "wait — which invite was that?",
            "backing this",
            "brb, checking the vault",
            "nice, ship it",
            "hmm, not sure about that",
            "we should propose it properly",
            "who's online later tonight?",
            "good morning everyone",
            "that fence isn't going to fix itself 🙂",
        ];
        let me = self.member();
        let mut pool: Vec<String> = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .filter(|w| !w.members.is_empty())
            .map(|w| {
                w.members
                    .iter()
                    .filter(|m| m.state != 2) // offline members stay silent
                    .map(|m| m.name.clone())
                    .collect()
            })
            .unwrap_or_else(|| self.roster());
        pool.retain(|n| *n != me);
        if pool.is_empty() {
            return;
        }
        let mut seed = crate::entropy_for(&me) | 1;
        let count = usize::try_from(mockrand::xorshift(&mut seed) % 3).unwrap_or_default();
        let mut replies: Vec<(u64, String, String, Option<u64>)> = (0..count)
            .map(|_| {
                let who = pool[usize::try_from(mockrand::xorshift(&mut seed)).unwrap_or_default()
                    % pool.len()]
                .clone();
                let line = LINES[usize::try_from(mockrand::xorshift(&mut seed))
                    .unwrap_or_default()
                    % LINES.len()]
                .to_string();
                let delay_ms = 1500 + mockrand::xorshift(&mut seed) % 5000; // 1.5–6.5 s
                                                                            // half of the replies quote the message they answer
                let quote = (mockrand::xorshift(&mut seed) % 2 == 0)
                    .then_some(u64::try_from(trigger).unwrap_or_default());
                (delay_ms, who, line, quote)
            })
            .collect();
        replies.sort_by_key(|(delay, _, _, _)| *delay);
        if replies.is_empty() {
            return;
        }
        let tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            let mut elapsed = 0;
            for (delay_ms, from, body, quote) in replies {
                tokio::time::sleep(std::time::Duration::from_millis(
                    delay_ms.saturating_sub(elapsed),
                ))
                .await;
                elapsed = delay_ms;
                let (reply, _rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(Envelope {
                        cmd: Command::ChatFrom { from, body, quote },
                        reply,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}
