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
use sha2::{Digest, Sha256};

use crate::{now_secs, ReplicaState, State};

/// Write a snapshot every N events (plus one on clean close). Snapshots are
/// an optimization; the log holds the truth.
const SNAPSHOT_EVERY: u64 = 1000;

/// Domain-separation tag of the P4 legacy-id synthesis (chat bus). The
/// formula is a **cross-node contract** — every node must derive the same
/// id for the same legacy message, or id-addressed events stop converging:
/// `sha256(TAG ‖ le64(position) ‖ from ‖ le64(ts) ‖ body)[..16]`. Pinned by
/// a literal in `legacy_log_replay_synthesizes_stable_ids`.
const LEGACY_ID_TAG: &[u8] = b"molt-chat-legacy-id\0";

/// Synthesize the stable id of a legacy (pre-chat-bus, nil-id) chat message
/// at its ingest `position` (index in the chat log at insertion). Both
/// ingest choke points — [`State::apply`]'s `Chat` arm and
/// [`State::restore_dump`] — see the same positions and the same
/// at-insertion fields, so full replay and snapshot+tail agree on every id
/// (the determinism keystone).
fn legacy_message_id(position: usize, from: &str, ts: u64, body: &str) -> molt_core::MessageId {
    let mut h = Sha256::new();
    h.update(LEGACY_ID_TAG);
    h.update(u64::try_from(position).unwrap_or(u64::MAX).to_le_bytes());
    h.update(from.as_bytes());
    h.update(ts.to_le_bytes());
    h.update(body.as_bytes());
    let digest = h.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    molt_core::MessageId(id)
}

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

    /// Apply one envelope, hand it to the open workspace's writer (if
    /// any), and wake the transport. The reply waits for neither the disk
    /// nor the network: the writer group-commits on its own clock, and the
    /// net wakeup carries no data (the log/feed is the outbox — transport
    /// concept §2). A lagging or failing writer surfaces honestly in the
    /// session notice.
    pub(crate) fn record(&mut self, env: EventEnvelope) {
        self.apply(&env);
        // the demo mesh mirrors + wakes now; a real mesh's outbox is the storage
        // log, so it must be woken AFTER the append below (else its log read
        // could race ahead of the write and miss this event)
        if let Some(net) = &self.net {
            net.publish(&env);
        }
        let Some(active) = &self.active else {
            return;
        };
        let seq = env.seq;
        let wake_real = self.net.as_ref().is_some_and(|n| n.is_real());
        // keep a copy to wake the real mesh with after the append consumes `env`
        let wake_env = wake_real.then(|| env.clone());
        if !active.handle.append(env) {
            self.session.notice = "storage-lagging".to_string();
        }
        if let (Some(net), Some(env)) = (&self.net, &wake_env) {
            net.wake_appended(env);
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
                identities,
                attestations: _,
                republic_id,
                // the ratified charter lives in the genesis frame (immutable);
                // the ratified charter, surfaced by the Constitution surface
                agenda,
            } => {
                self.replica = Some(ReplicaState {
                    name: name.clone(),
                    member: member.clone(),
                    roster: roster.clone(),
                    rule_m: *rule_m,
                    identities: identities.clone(),
                    agenda: agenda.clone(),
                    republic_id: republic_id.clone(),
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
                let mut msg = msg.clone();
                // P4: a legacy message (pre-chat-bus, nil id) gets its
                // stable id synthesized deterministically from its ingest
                // position — the same formula as restore_dump, so replay
                // and snapshot+tail agree. After this line no message in
                // state carries a nil id, and chat_pos indexes the whole
                // log (the pre-B1 nil-skip is obsolete).
                if msg.id.is_nil() {
                    msg.id = legacy_message_id(self.chat.len(), &msg.from, msg.ts, &msg.body);
                }
                // a legacy numeric quote resolves to the (possibly just
                // synthesized) id of the message it pointed at — the index
                // is still well-defined at apply time; the legacy field
                // itself stays readable and is never written by new code
                if msg.quote_id.is_none() {
                    if let Some(q) = msg.quote {
                        msg.quote_id = usize::try_from(q)
                            .ok()
                            .and_then(|i| self.chat.get(i))
                            .map(|m| m.id);
                    }
                }
                self.chat_pos.insert(msg.id, self.chat.len());
                self.chat.push(msg);
            }
            WorkspaceEvent::ChatReacted {
                index,
                id,
                emoji,
                by,
            } => {
                let Some(msg) = self.chat_target(id, *index) else {
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
            WorkspaceEvent::ChatDeleted { index, id, by } => {
                let Some(msg) = self.chat_target(id, *index) else {
                    return;
                };
                msg.body.clear();
                msg.reactions.clear();
                // deleting the message drops a file share with it
                msg.file = None;
                msg.deleted_by = Some(by.clone());
            }
            WorkspaceEvent::FileRemoved { index, id, .. } => {
                if let Some(file) = self.chat_target(id, *index).and_then(|m| m.file.as_mut()) {
                    file.available = false;
                }
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
            WorkspaceEvent::Committed(_)
            | WorkspaceEvent::ChainRequest { .. }
            | WorkspaceEvent::MembershipProposed { .. }
            | WorkspaceEvent::MlsCommit { .. }
            | WorkspaceEvent::MeshAnnounced { .. } => {
                // chain transport/coordination frames (a broadcast block, a
                // catch-up request, a membership-proposal announcement, a raw MLS
                // re-key commit, a relayed mesh announce) ride the log only to
                // reach the outbox; the chain lives in chain.state, the MLS
                // ratchet in the group and the mesh in transport.state, none
                // rebuilt from the log, so apply/replay is a deliberate no-op
            }
        }
    }

    /// Resolve a chat event's target message: **prefer the stable id**
    /// (chat bus) and fall back to the legacy position only when the event
    /// predates ids (`id == None`). An id that is present but unknown means
    /// the target is missing here — the event is ignored (never re-routed
    /// through the untrusted index; B1 adds wire-side parking).
    fn chat_target(
        &mut self,
        id: &Option<molt_core::MessageId>,
        index: u64,
    ) -> Option<&mut molt_core::ChatMessage> {
        let pos = match id {
            Some(id) => *self.chat_pos.get(id)?,
            None => usize::try_from(index).ok()?,
        };
        self.chat.get_mut(pos)
    }

    /// Serialize the actor's workspace state — the snapshot payload.
    pub(crate) fn dump(&self) -> EngineStateDump {
        let replica = self.replica.clone().unwrap_or_default();
        EngineStateDump {
            name: replica.name,
            member: replica.member,
            rule_m: replica.rule_m,
            roster: replica.roster,
            identities: replica.identities,
            agenda: replica.agenda,
            republic_id: replica.republic_id,
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
            identities: dump.identities,
            agenda: dump.agenda,
            republic_id: dump.republic_id,
        });
        self.chat = dump.chat;
        // P4, the second ingest choke point: a LEGACY snapshot (written by
        // pre-chat-bus code) may still carry nil-id messages and unresolved
        // numeric quotes — synthesize/resolve exactly like apply's Chat arm,
        // over the same positions, so snapshot+tail equals full replay.
        // (Snapshots written after B1 already carry the synthesized ids and
        // pass through untouched. One inherent legacy edge: a pre-chat-bus
        // snapshot of an already-deleted legacy message hashes the wiped
        // body, so such a tombstone's id can differ from the full-replay
        // id — bounded to unaddressable legacy tombstones.)
        for i in 0..self.chat.len() {
            if self.chat.get(i).is_some_and(|m| m.id.is_nil()) {
                let id = self
                    .chat
                    .get(i)
                    .map(|m| legacy_message_id(i, &m.from, m.ts, &m.body));
                if let (Some(m), Some(id)) = (self.chat.get_mut(i), id) {
                    m.id = id;
                }
            }
            let unresolved_quote = self
                .chat
                .get(i)
                .filter(|m| m.quote_id.is_none())
                .and_then(|m| m.quote);
            if let Some(q) = unresolved_quote {
                // apply resolved against the log BEFORE this message, so
                // only a backward reference (j < i) resolves here too
                let qid = usize::try_from(q)
                    .ok()
                    .filter(|&j| j < i)
                    .and_then(|j| self.chat.get(j))
                    .map(|m| m.id);
                if let Some(m) = self.chat.get_mut(i) {
                    m.quote_id = qid;
                }
            }
        }
        // the id→position map is derived state — rebuilt, never persisted;
        // after the synthesis above every message has a non-nil id
        self.chat_pos = self
            .chat
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id, i))
            .collect();
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
        // the workspace scope advances: outstanding recovery recv loops and
        // mesh-extension tasks of the OLD workspace die at this boundary
        // (their commands carry the old scope) — a mere mesh rebuild within
        // one workspace deliberately does not pass through here
        self.net_scope += 1;
        self.recovery_tickets.clear();
        self.recovery_mesh_window.clear();
        self.mesh_extension_at.clear();
        self.replica = None;
        self.identity_sk = None;
        self.chain.clear();
        self.chain_head = None;
        self.chain_applied.clear();
        self.pending_sigs.clear();
        self.proposal_changes.clear();
        self.pending_blocks.clear();
        self.catchup_from = None;
        self.pending_recovery.clear();
        self.chat.clear();
        self.chat_pos.clear();
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
        // legacy-shaped messages (nil id, no channel): the keystones pin
        // that pre-chat-bus logs keep replaying identically, index fallback
        // included
        let msg = |from: &str, body: &str, ts: u64| {
            ChatMessage {
                id: molt_core::MessageId::NIL,
                from: from.to_string(),
                body: body.to_string(),
                ts,
                quote: None,
                quote_id: None,
                channel: molt_core::ChannelRef::Group,
                reactions: Default::default(),
                deleted_by: None,
                file: None,
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
                    identities: Vec::new(),
                    attestations: Vec::new(),
                    republic_id: String::new(),
                    agenda: String::new(),
                },
            ),
            e(2, "petra", WorkspaceEvent::Chat(msg("petra", "gm", 102))),
            e(
                3,
                "walter",
                WorkspaceEvent::ChatReacted {
                    index: 0,
                    id: None,
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
                    height: 0,
                    sig: String::new(),
                },
            ),
            e(
                6,
                "walter",
                WorkspaceEvent::Approved {
                    id: ProposalId(1),
                    by: "walter".to_string(),
                    height: 0,
                    sig: String::new(),
                },
            ),
            e(7, "walter", WorkspaceEvent::Applied { id: ProposalId(1) }),
            e(
                8,
                "petra",
                WorkspaceEvent::ChatDeleted {
                    index: 0,
                    id: None,
                    by: "petra".to_string(),
                },
            ),
            e(9, "petra", {
                let mut share = msg("petra", "", 109);
                share.file = Some(molt_core::FileMeta {
                    name: "charter.pdf".to_string(),
                    size: 48_000,
                    kind: "PDF".to_string(),
                    modified: 100,
                    available: true,
                });
                WorkspaceEvent::Chat(share)
            }),
            e(
                10,
                "petra",
                WorkspaceEvent::FileRemoved {
                    index: 1,
                    id: None,
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
        assert_eq!(a.chat.len(), 2);
        assert_eq!(a.chat[0].deleted_by.as_deref(), Some("petra"));
        assert!(a.chat[0].reactions.is_empty());
        let file = a.chat[1].file.as_ref().expect("share survives replay");
        assert_eq!(file.name, "charter.pdf");
        assert!(!file.available, "the removal replays too");
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

    /// A pre-chat-bus log, committed as **JSON literals in the exact wire
    /// shape old nodes wrote** (no `id`/`channel`/`quote_id`, a numeric
    /// `quote`, index-addressed reactions/deletes) — the old-log
    /// compatibility contract, pinned forever. Never regenerate these with
    /// current code: the test proves old bytes, not current serialization.
    const LEGACY_LOG: [&str; 7] = [
        r#"{"seq":1,"ts":101,"by":"petra","body":{"type":"founded","name":"Chess Club","rule_m":2,"rule_n":3,"member":"petra","roster":["petra","walter"]}}"#,
        r#"{"seq":2,"ts":102,"by":"petra","body":{"type":"chat","from":"petra","body":"gm","ts":102}}"#,
        r#"{"seq":3,"ts":103,"by":"walter","body":{"type":"chat","from":"walter","body":"re: gm","ts":103,"quote":0}}"#,
        r#"{"seq":4,"ts":104,"by":"walter","body":{"type":"chat_reacted","index":0,"emoji":"👍","by":"walter"}}"#,
        r#"{"seq":5,"ts":105,"by":"petra","body":{"type":"chat","from":"petra","body":"","ts":105,"file":{"name":"charter.pdf","size":48000,"kind":"PDF","modified":100,"available":true}}}"#,
        r#"{"seq":6,"ts":106,"by":"petra","body":{"type":"file_removed","index":2,"by":"petra"}}"#,
        r#"{"seq":7,"ts":107,"by":"petra","body":{"type":"chat_deleted","index":0,"by":"petra"}}"#,
    ];

    /// The legacy log above plus new-style (chat-bus) events on top — the
    /// mixed history a long-lived workspace actually has.
    fn mixed_envs() -> Vec<EventEnvelope> {
        let mut all: Vec<EventEnvelope> = LEGACY_LOG
            .iter()
            .map(|s| serde_json::from_str(s).expect("legacy envelope decodes"))
            .collect();
        let new_id = molt_core::MessageId([0x42; 16]);
        let mut msg = ChatMessage::text(new_id, "walter", "new era", 108).with_channel(
            molt_core::ChannelRef::Topic {
                name: "budget".to_string(),
            },
        );
        msg.quote_id = None;
        all.push(EventEnvelope {
            seq: 8,
            ts: 108,
            by: "walter".to_string(),
            body: WorkspaceEvent::Chat(msg),
        });
        all.push(EventEnvelope {
            seq: 9,
            ts: 109,
            by: "petra".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 3,
                id: Some(new_id),
                emoji: "🔥".to_string(),
                by: "petra".to_string(),
            },
        });
        all
    }

    /// P4: replaying a pre-chat-bus log synthesizes a **stable, non-nil id
    /// for every legacy message** (identical across replays and across the
    /// snapshot+tail path), resolves the legacy numeric quote to the right
    /// `quote_id`, and leaves `chat_pos` indexing the whole log.
    #[test]
    fn legacy_log_replay_synthesizes_stable_ids() {
        let run = || {
            let mut st = plain_state();
            for env in mixed_envs() {
                st.apply(&env);
            }
            st
        };
        // deterministic: two replays, identical dumps
        assert_eq!(run().dump(), run().dump());

        let st = run();
        assert_eq!(st.chat.len(), 4);
        for (i, m) in st.chat.iter().enumerate() {
            assert!(!m.id.is_nil(), "message {i} kept a nil id after ingest");
            assert_eq!(st.chat_pos.get(&m.id), Some(&i), "chat_pos indexes message {i}");
        }
        // the P4 formula is a cross-node contract: pin the synthesized id of
        // message 0 (position 0, from "petra", ts 102, body "gm") as a literal
        // so a formula change can never slip through silently
        assert_eq!(st.chat[0].id.to_string(), LEGACY_ID_OF_MSG_0);
        // the legacy numeric quote resolved to message 0's synthesized id;
        // the legacy field itself stays readable, untouched
        assert_eq!(st.chat[1].quote_id, Some(st.chat[0].id));
        assert_eq!(st.chat[1].quote, Some(0));
        // the index-addressed legacy events replayed as before
        assert_eq!(st.chat[0].deleted_by.as_deref(), Some("petra"));
        assert!(st.chat[0].reactions.is_empty(), "delete drops reactions");
        assert!(
            !st.chat[2].file.as_ref().expect("share survives").available,
            "the legacy file removal replays"
        );
        // the new-style events landed by id
        assert_eq!(
            st.chat[3].channel,
            molt_core::ChannelRef::Topic {
                name: "budget".to_string()
            }
        );
        assert_eq!(st.chat[3].reactions["🔥"], vec!["petra".to_string()]);

        // the determinism keystone over the MIXED log: snapshot at every k
        // plus tail equals full replay — both ingest choke points synthesize
        // the same ids
        let all = mixed_envs();
        let full = run().dump();
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

    /// The P4 pin for `legacy_log_replay_synthesizes_stable_ids`:
    /// `sha256("molt-chat-legacy-id\0" ‖ le64(0) ‖ "petra" ‖ le64(102) ‖ "gm")[..16]`.
    const LEGACY_ID_OF_MSG_0: &str = "bbb7bc990b87cf10ecf6ed59f31e8ce2";

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
                id: None,
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
