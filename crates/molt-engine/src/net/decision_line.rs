// SPDX-License-Identifier: GPL-3.0-or-later

//! The wire gate for decision lines. Their id is deterministic and so
//! predictable: a peer's copy takes it only when it is the line this seat
//! would post itself; one that outruns the decision is held until then.

use molt_core::{ChannelRef, ChatKind, ChatMessage, MemberId, MessageId, ProposalState};
use std::collections::HashMap;

use crate::proposals::PARKED_DECLINE_ID_WINDOW;
use crate::State;

const HELD_MAX: usize = 64;
const HELD_PER_MEMBER: usize = 8;
const HOLD_SECS: u64 = 600;
/// Ids hashed at most: a corrupt `next_id` must not stall the actor.
const ID_SPAN: u64 = 1 << 16;

struct Held {
    from: MemberId,
    msg: ChatMessage,
    until: u64,
}

/// Runtime-only: the id lookup (proposal ids up to the decline window past
/// `next_id`, so an id minted ahead is recognized too) and the held lines.
#[derive(Default)]
pub(crate) struct DecisionLines {
    republic: String,
    upto: u64,
    ids: HashMap<MessageId, (u64, bool)>,
    held: Vec<Held>,
}

pub(super) enum Verdict {
    Genuine,
    Pending,
    Forged,
}

impl State {
    /// The (proposal, declined) whose decision line owns `id`, if any.
    pub(super) fn decision_id_owner(&mut self, id: &MessageId) -> Option<(u64, bool)> {
        let republic = self.replica.as_ref().map(|r| r.republic_id.clone()).unwrap_or_default();
        let lines = &mut self.decision_lines;
        if lines.republic != republic {
            lines.republic = republic;
            lines.upto = 0;
            lines.ids.clear();
        }
        let top = self.next_id.saturating_add(PARKED_DECLINE_ID_WINDOW);
        lines.upto = lines.upto.max(top.saturating_sub(ID_SPAN));
        while lines.upto < top {
            lines.upto += 1;
            for declined in [false, true] {
                let sid = crate::chat::decision_summary_id(&lines.republic, lines.upto, declined);
                lines.ids.insert(sid, (lines.upto, declined));
            }
        }
        lines.ids.get(id).copied()
    }

    pub(super) fn decision_line_verdict(&self, msg: &ChatMessage, proposal: u64, declined: bool) -> Verdict {
        let in_place = msg.kind == ChatKind::System
            && matches!(msg.channel, ChannelRef::Patch { id } if id.0 == proposal);
        if !in_place {
            return Verdict::Forged;
        }
        let Some(p) = self.proposals.get(&proposal) else {
            return Verdict::Pending;
        };
        let matches = match (p.state, declined) {
            (ProposalState::Applied, false) => msg.body == Self::decision_summary(proposal, &p.payload, None),
            (ProposalState::Rejected, true) => p
                .decliners
                .iter()
                .chain(std::iter::once(&p.declined_by))
                .filter(|w| !w.is_empty())
                .any(|w| msg.body == Self::decision_summary(proposal, &p.payload, Some(w))),
            (ProposalState::Applied, true) | (ProposalState::Rejected, false) => false,
            _ => return Verdict::Pending,
        };
        if matches {
            Verdict::Genuine
        } else {
            Verdict::Forged
        }
    }

    pub(super) fn hold_decision_line(&mut self, from: MemberId, msg: ChatMessage) {
        let held = &mut self.decision_lines.held;
        let mine = held.iter().filter(|h| h.from == from).count();
        if held.len() >= HELD_MAX || mine >= HELD_PER_MEMBER {
            tracing::warn!(%from, id = %msg.id, "dropping a decision line - hold full");
            return;
        }
        let until = self.presence_now().saturating_add(HOLD_SECS);
        self.decision_lines.held.push(Held { from, msg, until });
    }

    /// Delivery-tick beat: land the held lines this seat now agrees with.
    pub(super) fn release_decision_lines(&mut self, now: u64) {
        if self.decision_lines.held.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.decision_lines.held);
        for h in held {
            if self.chat_pos.contains_key(&h.msg.id) {
                continue;
            }
            let Some((proposal, declined)) = self.decision_id_owner(&h.msg.id) else {
                continue;
            };
            match self.decision_line_verdict(&h.msg, proposal, declined) {
                Verdict::Genuine => self.land_wire_chat(h.from, h.msg),
                Verdict::Forged => {
                    tracing::warn!(from = %h.from, id = %h.msg.id, proposal, "dropping a forged decision line");
                }
                Verdict::Pending if h.until > now => self.decision_lines.held.push(h),
                Verdict::Pending => {
                    tracing::debug!(from = %h.from, id = %h.msg.id, proposal, "dropping an undecided decision line");
                }
            }
        }
    }
}
