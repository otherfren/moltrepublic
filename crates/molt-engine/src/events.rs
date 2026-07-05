// SPDX-License-Identifier: GPL-3.0-or-later

//! The event applier — milestone S0 of concept-workspace-storage.md.
//!
//! Every handler follows one shape: **validate → build a
//! [`WorkspaceEvent`] → [`State::apply`]**. `apply` is the *only* thing that
//! mutates workspace state (chat, surface logs, proposals, roster), and it
//! reads nothing but the envelope — timestamps and identities come from the
//! envelope, clocks run only at event creation. That makes replaying a
//! persisted log byte-for-byte equivalent to having executed the commands
//! live, which is what the keystone determinism test pins.

use molt_core::{
    EngineStateDump, EventEnvelope, MemberId, ProposalRecord, ProposalState, Surface,
    WorkspaceEvent, WorkspaceSnapshot,
};

use crate::{now_secs, ReplicaState, State};

/// Write a snapshot every N events (plus one on clean close). Snapshots are
/// an optimization; the log holds the truth.
const SNAPSHOT_EVERY: u64 = 1000;

impl State {
    /// Stamp the next envelope: seq from the workspace's monotonic counter,
    /// ts from the engine clock — the one place a clock runs.
    pub(crate) fn make_env(&mut self, by: MemberId, body: WorkspaceEvent) -> EventEnvelope {
        let env = EventEnvelope {
            seq: self.next_seq,
            ts: now_secs(),
            by,
            body,
        };
        self.next_seq += 1;
        env
    }

    /// Apply one envelope, then hand it to the open workspace's writer (if
    /// any). The reply never waits for the disk; a lagging or failing
    /// writer surfaces honestly in the session notice.
    pub(crate) fn record(&mut self, env: EventEnvelope) {
        self.apply(&env);
        let Some(active) = &self.active else {
            return;
        };
        let seq = env.seq;
        if !active.handle.append(env) {
            self.session.notice = "storage-lagging".to_string();
        }
        if active.handle.failed() {
            self.session.notice = "storage-failed".to_string();
        } else if seq % SNAPSHOT_EVERY == 0 {
            let snap = self.snapshot_now();
            if let Some(active) = &self.active {
                active.handle.snapshot(snap);
            }
        }
    }

    /// **The only mutator of workspace state.** No clock, no config, no
    /// randomness — everything it needs is in the envelope, so replay is
    /// deterministic. Validation happened before the event was created;
    /// an envelope that no longer matches (e.g. an out-of-range index from
    /// a corrupted log) is ignored rather than panicking.
    pub(crate) fn apply(&mut self, env: &EventEnvelope) {
        match &env.body {
            WorkspaceEvent::Founded {
                name,
                rule_m,
                rule_n: _,
                member,
                roster,
            } => {
                self.replica = Some(ReplicaState {
                    name: name.clone(),
                    member: member.clone(),
                    roster: roster.clone(),
                    rule_m: *rule_m,
                });
            }
            WorkspaceEvent::MemberJoined { member } => {
                if let Some(r) = &mut self.replica {
                    if !r.roster.contains(member) {
                        r.roster.push(member.clone());
                    }
                }
            }
            WorkspaceEvent::Chat(msg) => {
                self.chat.push(msg.clone());
            }
            WorkspaceEvent::ChatReacted { index, emoji, by } => {
                let Some(msg) = usize::try_from(*index)
                    .ok()
                    .and_then(|i| self.chat.get_mut(i))
                else {
                    return;
                };
                let had_this = msg.reactions.get(emoji).is_some_and(|who| who.contains(by));
                for who in msg.reactions.values_mut() {
                    who.retain(|w| w != by);
                }
                msg.reactions.retain(|_, who| !who.is_empty());
                if !had_this {
                    msg.reactions.entry(emoji.clone()).or_default().push(by.clone());
                }
            }
            WorkspaceEvent::ChatDeleted { index, by } => {
                let Some(msg) = usize::try_from(*index)
                    .ok()
                    .and_then(|i| self.chat.get_mut(i))
                else {
                    return;
                };
                msg.body.clear();
                msg.reactions.clear();
                msg.deleted_by = Some(by.clone());
            }
            WorkspaceEvent::Proposed {
                id,
                surface,
                payload,
            } => {
                self.proposals.insert(
                    id.0,
                    ProposalRecord {
                        surface: *surface,
                        payload: payload.clone(),
                        approvals: 0,
                        state: ProposalState::Proposed,
                    },
                );
                self.next_id = self.next_id.max(id.0 + 1);
            }
            WorkspaceEvent::Approved { id, .. } => {
                // Deliberately no per-member dedup yet: the threshold
                // machine is a simulation (one local operator stands in for
                // the whole group, engine doc header) — a repeated Approve
                // *is* the next member's co-signature. Real dedup arrives
                // with real member identities (FROST/MLS, molt-identity);
                // the envelope's `by` already records what it will need.
                if let Some(p) = self.proposals.get_mut(&id.0) {
                    if p.state == ProposalState::Proposed {
                        p.approvals += 1;
                    }
                }
            }
            WorkspaceEvent::Declined { id, .. } => {
                if let Some(p) = self.proposals.get_mut(&id.0) {
                    if p.state == ProposalState::Proposed {
                        p.state = ProposalState::Rejected;
                    }
                }
            }
            WorkspaceEvent::Applied { id } => {
                if let Some(p) = self.proposals.get_mut(&id.0) {
                    if p.state == ProposalState::Proposed {
                        p.state = ProposalState::Applied;
                        let payload = p.payload.clone();
                        let surface = p.surface;
                        self.applied.entry(surface).or_default().push(payload);
                    }
                }
            }
            WorkspaceEvent::MemberSeen { .. } => {
                // presence is runtime state owned by the transport; the
                // variant exists so checkpoints have a schema slot
            }
        }
    }

    /// Serialize the actor's workspace state — the snapshot payload.
    pub(crate) fn dump(&self) -> EngineStateDump {
        let replica = self.replica.clone().unwrap_or_default();
        EngineStateDump {
            name: replica.name,
            member: replica.member,
            rule_m: replica.rule_m,
            roster: replica.roster,
            chat: self.chat.clone(),
            applied: self
                .applied
                .iter()
                .filter(|(_, log)| !log.is_empty())
                .map(|(s, log)| (s.as_str().to_string(), log.clone()))
                .collect(),
            proposals: self
                .proposals
                .iter()
                .map(|(id, p)| (*id, p.clone()))
                .collect(),
            next_proposal_id: self.next_id,
        }
    }

    /// A snapshot of the current state at the last recorded seq.
    pub(crate) fn snapshot_now(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq: self.next_seq.saturating_sub(1),
            state: self.dump(),
        }
    }

    /// Load a snapshot dump back into the actor (the open path; the log
    /// tail is then replayed through [`State::apply`]).
    pub(crate) fn restore_dump(&mut self, dump: EngineStateDump) {
        self.replica = Some(ReplicaState {
            name: dump.name,
            member: dump.member,
            roster: dump.roster,
            rule_m: dump.rule_m,
        });
        self.chat = dump.chat;
        self.applied.clear();
        for s in Surface::ALL {
            self.applied.insert(s, Vec::new());
        }
        for (name, log) in dump.applied {
            if let Some(s) = Surface::parse(&name) {
                self.applied.insert(s, log);
            }
        }
        self.proposals = dump.proposals.into_iter().collect();
        self.next_id = dump.next_proposal_id.max(1);
    }

    /// Drop all workspace state (close path). The next open starts clean.
    pub(crate) fn reset_workspace_state(&mut self) {
        self.replica = None;
        self.chat.clear();
        self.applied.clear();
        for s in Surface::ALL {
            self.applied.insert(s, Vec::new());
        }
        self.proposals.clear();
        self.next_id = 1;
        self.next_seq = 1;
    }
}

#[cfg(test)]
mod tests {
    use molt_core::{ChatMessage, EventEnvelope, ProposalId, WorkspaceEvent};
    use serde_json::json;

    use crate::tests::plain_state;

    fn envs() -> Vec<EventEnvelope> {
        let e = |seq: u64, by: &str, body: WorkspaceEvent| EventEnvelope {
            seq,
            ts: 100 + seq,
            by: by.to_string(),
            body,
        };
        let msg = |from: &str, body: &str, ts: u64| {
            ChatMessage {
                from: from.to_string(),
                body: body.to_string(),
                ts,
                quote: None,
                reactions: Default::default(),
                deleted_by: None,
            }
        };
        vec![
            e(
                1,
                "petra",
                WorkspaceEvent::Founded {
                    name: "Chess Club".to_string(),
                    rule_m: 2,
                    rule_n: 3,
                    member: "petra".to_string(),
                    roster: vec!["petra".to_string(), "walter".to_string()],
                },
            ),
            e(2, "petra", WorkspaceEvent::Chat(msg("petra", "gm", 102))),
            e(
                3,
                "walter",
                WorkspaceEvent::ChatReacted {
                    index: 0,
                    emoji: "👍".to_string(),
                    by: "walter".to_string(),
                },
            ),
            e(
                4,
                "petra",
                WorkspaceEvent::Proposed {
                    id: ProposalId(1),
                    surface: molt_core::Surface::Memory,
                    payload: json!({"op":"add_note","title":"t"}),
                },
            ),
            e(
                5,
                "petra",
                WorkspaceEvent::Approved {
                    id: ProposalId(1),
                    by: "petra".to_string(),
                },
            ),
            e(
                6,
                "walter",
                WorkspaceEvent::Approved {
                    id: ProposalId(1),
                    by: "walter".to_string(),
                },
            ),
            e(7, "walter", WorkspaceEvent::Applied { id: ProposalId(1) }),
            e(
                8,
                "petra",
                WorkspaceEvent::ChatDeleted {
                    index: 0,
                    by: "petra".to_string(),
                },
            ),
        ]
    }

    /// The keystone: replaying the same envelope stream twice produces the
    /// same dump — `apply` reads nothing but the envelope.
    #[test]
    fn replay_is_deterministic() {
        let run = || {
            let mut st = plain_state();
            for env in envs() {
                st.apply(&env);
            }
            st.dump()
        };
        let a = run();
        let b = run();
        assert_eq!(a, b);
        assert_eq!(a.name, "Chess Club");
        assert_eq!(a.roster, vec!["petra", "walter"]);
        assert_eq!(a.chat.len(), 1);
        assert_eq!(a.chat[0].deleted_by.as_deref(), Some("petra"));
        assert!(a.chat[0].reactions.is_empty());
        assert_eq!(a.applied["memory"].len(), 1);
        assert_eq!(a.proposals[&1].approvals, 2);
        assert_eq!(a.next_proposal_id, 2);
    }

    /// `replay(log) == replay(snapshot at k) + replay(tail)` for every k.
    #[test]
    fn snapshot_at_every_k_plus_tail_equals_full_replay() {
        let all = envs();
        let full = {
            let mut st = plain_state();
            for env in &all {
                st.apply(env);
            }
            st.dump()
        };
        for k in 0..all.len() {
            let mut st = plain_state();
            for env in &all[..k] {
                st.apply(env);
            }
            let snap = st.dump();
            let mut st2 = plain_state();
            st2.restore_dump(snap);
            for env in &all[k..] {
                st2.apply(env);
            }
            assert_eq!(st2.dump(), full, "diverged at k={k}");
        }
    }

    /// Envelopes that no longer match the state (corrupted log) are ignored,
    /// never a panic.
    #[test]
    fn out_of_range_events_are_ignored() {
        let mut st = plain_state();
        st.apply(&EventEnvelope {
            seq: 1,
            ts: 1,
            by: "x".to_string(),
            body: WorkspaceEvent::ChatDeleted {
                index: 99,
                by: "x".to_string(),
            },
        });
        st.apply(&EventEnvelope {
            seq: 2,
            ts: 2,
            by: "x".to_string(),
            body: WorkspaceEvent::Applied { id: ProposalId(9) },
        });
        assert!(st.dump().chat.is_empty());
        assert!(st.dump().applied.is_empty());
    }
}
