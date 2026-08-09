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
/// `sha256(TAG ‖ le64(sender_ordinal) ‖ from ‖ le64(ts) ‖ body)[..16]`.
/// Pinned by literals in `legacy_log_replay_synthesizes_stable_ids`.
const LEGACY_ID_TAG: &[u8] = b"molt-chat-legacy-id\0";

/// Synthesize the stable id of a legacy (pre-chat-bus, nil-id) chat message
/// from its **per-sender ordinal** — how many messages from the same `from`
/// preceded it at insertion. Pre-chat-bus delivery is in-order **per sender
/// only**, so the GLOBAL log position of a cross-sender interleaving differs
/// between nodes and must never enter the hash; the per-sender ordinal is
/// identical everywhere. Both ingest choke points — [`State::apply`]'s
/// `Chat` arm and [`State::restore_dump`] — count the same prefix, so full
/// replay and snapshot+tail agree on every id (the determinism keystone).
fn legacy_message_id(ordinal: usize, from: &str, ts: u64, body: &str) -> molt_core::MessageId {
    let mut h = Sha256::new();
    h.update(LEGACY_ID_TAG);
    h.update(u64::try_from(ordinal).unwrap_or(u64::MAX).to_le_bytes());
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
    /// ts from the engine clock — the one place a clock runs. An envelope
    /// this node AUTHORS also carries the in-order chain (G7): `prev_seq` =
    /// the seq of our previous own ackable envelope, so a receiver can hold
    /// it until the predecessor landed. Re-recorded PEER events (the wire
    /// receive path) stay unstamped — they are never fanned out, and a zero
    /// serializes away (byte-identical legacy log frames).
    pub(crate) fn make_env(&mut self, by: MemberId, body: WorkspaceEvent) -> EventEnvelope {
        let prev_seq = if by == self.member() { self.last_own_ackable } else { 0 };
        let env = EventEnvelope {
            seq: self.next_seq,
            ts: now_secs(),
            by,
            body,
            prev_seq,
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
        // `last_mesh_out` = "our own traffic crossed the wire since the last
        // MLS snapshot" — its ONLY reader is `persist_mls_if_due`, which uses
        // it to debounce the live ratchet persist (delivery guarantee §4.6).
        // Only events WE authored advance our sender ratchet (the outbox sends
        // authored events only — `NetRuntime::wants`), so a *received* peer
        // event (`env.by != me`) must NOT stamp it. Stamped before the
        // `active` borrow below so the mutation is conflict-free.
        if self.net.as_ref().is_some_and(|n| n.is_real())
            && crate::net::crosses_wire(&env.body)
            && env.by == self.member()
        {
            self.last_mesh_out = self.presence_now();
        }
        // delivery guarantee §4.6: the debounced live ratchet persist rides
        // RECORD (traffic-coupled), because the presence tick is a 30 s beat
        // — a hard kill between ticks would otherwise regress the ratchet by
        // a whole burst. Early-outs on the debounce; cheap in the hot path.
        self.persist_mls_if_due(self.presence_now());
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
        // …and the group runtime's outbox, which reads the same storage log
        // (the watch coalesces, so this never blocks the append path)
        if let Some(group) = &self.group_net {
            let _ = group.wakeup.send(seq);
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
                // read by verifiers recomputing the signed bytes from the log,
                // not by the applier — the live pool comes from transport.state
                relays: _,
            } => {
                self.replica = Some(ReplicaState {
                    name: name.clone(),
                    member: member.clone(),
                    roster: roster.clone(),
                    rule_m: *rule_m,
                    identities: identities.clone(),
                    agenda: agenda.clone(),
                    republic_id: republic_id.clone(),
                    founded_ts: env.ts,
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
                // stable id synthesized deterministically from its
                // PER-SENDER ordinal (cross-node stable — global positions
                // are not) — the same formula as restore_dump, so replay
                // and snapshot+tail agree. After this line no message in
                // state carries a nil id, and chat_pos indexes the whole
                // log (the pre-B1 nil-skip is obsolete).
                if msg.id.is_nil() {
                    let ordinal = self.sender_ordinal(&msg.from, self.chat.len());
                    msg.id = legacy_message_id(ordinal, &msg.from, msg.ts, &msg.body);
                }
                // a legacy numeric quote resolves to the (possibly just
                // synthesized) id of the message it pointed at — the index
                // is still well-defined at apply time; the legacy field
                // itself stays readable and is never written by new code
                // (WP4a: not on a node that has physically pruned — the
                // positions moved, so resolving would attribute the quote to
                // an innocent surviving message. It stays unresolved instead.)
                if msg.quote_id.is_none() && !self.chat_pruned {
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
                op,
            } => {
                let Some(msg) = self.chat_target(id, *index) else {
                    return;
                };
                // a reaction never lands on a tombstone (delete clears
                // reactions) — otherwise a concurrent react/delete pair
                // would converge differently per arrival order
                if msg.deleted_by.is_some() {
                    return;
                }
                let has_this = msg.reactions.get(emoji).is_some_and(|who| who.contains(by));
                // `Some(op)` is an idempotent set/unset — the SENDER already
                // resolved the toggle, so a redelivered duplicate is a
                // no-op. `None` is a legacy (pre-op) event and replays with
                // the original toggle semantics, bit-identically.
                let want_this = match op {
                    Some(molt_core::ReactOp::Add) => true,
                    Some(molt_core::ReactOp::Remove) => false,
                    None => !has_this,
                };
                if want_this == has_this {
                    return;
                }
                // one reaction per member: any previous emoji of theirs goes
                for who in msg.reactions.values_mut() {
                    who.retain(|w| w != by);
                }
                msg.reactions.retain(|_, who| !who.is_empty());
                if want_this {
                    msg.reactions.entry(emoji.clone()).or_default().push(by.clone());
                }
            }
            WorkspaceEvent::ChatDeleted { index, id, by } => {
                let Some(msg) = self.chat_target(id, *index) else {
                    return;
                };
                msg.body.clear();
                msg.reactions.clear();
                // a tombstone carries no read receipts either (the read/delete
                // pair commutes: a receipt arriving after the delete is
                // dropped by the ChatRead arm's tombstone guard)
                msg.read_by.clear();
                // deleting the message drops a file share with it
                msg.file = None;
                msg.deleted_by = Some(by.clone());
            }
            WorkspaceEvent::FileRemoved { index, id, .. } => {
                if let Some(file) = self.chat_target(id, *index).and_then(|m| m.file.as_mut()) {
                    file.available = false;
                }
            }
            WorkspaceEvent::ChatRead { ids, by } => {
                // read receipts address purely by stable id (post-chat-bus).
                // An unknown id is ignored here — the wire arm parks it until
                // the message arrives, then drains it back through here.
                for id in ids {
                    let Some(msg) = self.chat_target(&Some(*id), 0) else {
                        continue;
                    };
                    // never on a tombstone (delete clears read_by → commute),
                    // and the author never receipts their own message
                    if msg.deleted_by.is_some() || &msg.from == by {
                        continue;
                    }
                    // monotonic idempotent insert — at-least-once redelivery
                    // is a harmless no-op
                    msg.read_by.insert(by.clone());
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
                        declined_at: 0,
                        declined_by: MemberId::new(),
                        decliners: Vec::new(),
                    },
                );
                self.next_id = self.next_id.max(id.0 + 1);
                let _ = self.register_parked_declines(id.0);
            }
            WorkspaceEvent::Approved { id, .. } => {
                // Replay projection, deliberately a plain count: live
                // approvals are already deduplicated at their source
                // (`cmd_approve` refuses a second local approval; the chain
                // collects distinct signatures and ignores this counter), so
                // this arm only reconstructs what a log recorded — including
                // legacy pre-chain logs whose counter once simulated peers.
                if let Some(p) = self.proposals.get_mut(&id.0) {
                    if p.state == ProposalState::Proposed {
                        p.approvals += 1;
                    }
                }
            }
            WorkspaceEvent::Declined { id, by } => {
                // a decline is ONE member's voice, not a veto: the proposal
                // turns Rejected only when approval can no longer reach the
                // threshold (declines > n − m; pre-ritual keeps the
                // single-operator semantics — one decline is the exit). An
                // own-log decline whose FOREIGN proposal is not registered
                // yet parks instead of vanishing — the WP2 re-serve brings
                // the proposal back, and the vote must still be standing.
                // All in [`State::register_decline`]; the outcome is
                // ignored here, replay must not ring frontends.
                let _ = self.register_decline(id.0, by, env.ts);
            }
            WorkspaceEvent::Applied { id } => {
                if let Some(p) = self.proposals.get_mut(&id.0) {
                    if p.state == ProposalState::Proposed {
                        p.state = ProposalState::Applied;
                        let payload = p.payload.clone();
                        let surface = p.surface;
                        self.applied.entry(surface).or_default().push((Some(id.0), payload));
                    }
                }
            }
            WorkspaceEvent::MemberSeen { .. } => {
                // presence is runtime state owned by the transport; the
                // variant exists so checkpoints have a schema slot
            }
            WorkspaceEvent::MembershipProposed { id, op, member, .. } => {
                // the approval surface (recovery approval design, 2026-08-08):
                // the gossip that makes every member sign the SAME change also
                // creates the HUMAN-facing record — one applier for the
                // proposer, every receiver and replay. Idempotent (`entry`):
                // a re-gossip must not reset collected votes. The CHAIN-side
                // registration stays in the net ingest — the chain is not
                // rebuilt from the log — and `proposal_change` refuses to
                // resolve a membership record without it, so a half-restored
                // state can never sign fabricated bytes.
                self.proposals.entry(id.0).or_insert_with(|| ProposalRecord {
                    surface: Surface::Organization,
                    payload: serde_json::json!({
                        "op": match op {
                            molt_core::MembershipOp::Restored => "restore_member",
                            molt_core::MembershipOp::Joined => "add_member",
                        },
                        "member": member,
                    }),
                    approvals: 0,
                    state: ProposalState::Proposed,
                    declined_at: 0,
                    declined_by: MemberId::new(),
                    decliners: Vec::new(),
                });
                self.next_id = self.next_id.max(id.0 + 1);
                let _ = self.register_parked_declines(id.0);
            }
            WorkspaceEvent::Committed(_)
            | WorkspaceEvent::ChainRequest { .. }
            | WorkspaceEvent::CheckpointProposed { .. }
            | WorkspaceEvent::CheckpointServed { .. }
            | WorkspaceEvent::MlsCommit { .. }
            | WorkspaceEvent::MeshAnnounced { .. }
            | WorkspaceEvent::FileRequested { .. } => {
                // chain transport/coordination frames (a broadcast block, a
                // catch-up request, a raw MLS re-key commit, a relayed mesh
                // announce, a file fetch request) ride the log only to reach
                // the outbox; the chain lives in chain.state, the MLS ratchet
                // in the group, the mesh in transport.state and a file
                // transfer on its dedicated queue, none rebuilt from the log,
                // so apply/replay is a deliberate no-op (MembershipProposed
                // left this group: its RECORD is log state, see its arm)
            }
        }
        // G7 in-order chain bookkeeping: our own ackable envelopes form the
        // `prev_seq` chain the receivers order on. Derived HERE (the one
        // place both live records and the open-time replay pass through), and
        // AFTER the match so the founder's own `Founded` — which sets the
        // replica this check reads — joins its chain. `MlsCommit`s are
        // excluded: they never reach a peer's engine, so a chain through one
        // would wedge every successor at the receiver.
        if env.seq > self.last_own_ackable
            && env.by == self.member()
            && !matches!(env.body, WorkspaceEvent::MlsCommit { .. })
        {
            self.last_own_ackable = env.seq;
        }
    }

    /// The **sender ordinal** the legacy id formula hashes: how many messages
    /// from `from` this node has ever ingested before position `upto` —
    /// the ones still in the log, PLUS the ones compaction physically dropped
    /// ([`EngineStateDump::chat_pruned_counts`]). Without the pruned part a
    /// compacted node would restart the count and synthesize different ids
    /// than its peers for the very same message (WP4a keystone).
    fn sender_ordinal(&self, from: &str, upto: usize) -> usize {
        let live = self.chat.iter().take(upto).filter(|p| p.from == from).count();
        let pruned = self
            .chat_pruned_counts
            .get(from)
            .copied()
            .unwrap_or(0);
        live.saturating_add(usize::try_from(pruned).unwrap_or(usize::MAX))
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
            // WP4a: once expired content has been physically dropped, a
            // position no longer identifies the message the sender meant —
            // every entry below it moved up. An id-less (pre-chat-bus) op is
            // therefore ignored on a pruned node rather than mis-addressed
            // onto an innocent surviving message.
            None if self.chat_pruned => return None,
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
            founded_ts: replica.founded_ts,
            chat: self.chat.clone(),
            applied: self
                .applied
                .iter()
                .filter(|(_, log)| !log.is_empty())
                .map(|(s, log)| {
                    (
                        s.as_str().to_string(),
                        log.iter().map(|(_, v)| v.clone()).collect(),
                    )
                })
                .collect(),
            applied_ids: self
                .applied
                .iter()
                .filter(|(_, log)| !log.is_empty())
                .map(|(s, log)| {
                    (
                        s.as_str().to_string(),
                        log.iter().map(|(id, _)| *id).collect(),
                    )
                })
                .collect(),
            proposals: self
                .proposals
                .iter()
                .map(|(id, p)| (*id, p.clone()))
                .collect(),
            next_proposal_id: self.next_id,
            chat_pruned: self.chat_pruned,
            chat_pruned_counts: self.chat_pruned_counts.clone(),
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
        // sticky: a workspace that once pruned can never trust chat positions
        // again (WP4a). Restoring an older, un-pruned snapshot must not clear
        // it either — hence `|=`.
        self.chat_pruned |= dump.chat_pruned;
        for (from, n) in &dump.chat_pruned_counts {
            let slot = self.chat_pruned_counts.entry(from.clone()).or_insert(0);
            *slot = (*slot).max(*n);
        }
        self.replica = Some(ReplicaState {
            name: dump.name,
            member: dump.member,
            roster: dump.roster,
            rule_m: dump.rule_m,
            identities: dump.identities,
            agenda: dump.agenda,
            republic_id: dump.republic_id,
            founded_ts: dump.founded_ts,
        });
        self.chat = dump.chat;
        // P4, the second ingest choke point: a LEGACY snapshot (written by
        // pre-chat-bus code) may still carry nil-id messages and unresolved
        // numeric quotes — synthesize/resolve exactly like apply's Chat arm,
        // over the same per-sender ordinals (the count of earlier messages
        // from the same sender — what apply saw at insertion), so
        // snapshot+tail equals full replay. (Snapshots written after B1
        // already carry the synthesized ids and pass through untouched. One
        // inherent legacy edge: a pre-chat-bus snapshot of an
        // already-deleted legacy message hashes the wiped body, so such a
        // tombstone's id can differ from the full-replay id — bounded to
        // unaddressable legacy tombstones. `from` survives deletion, so
        // tombstones still count toward later ordinals, exactly as at
        // apply time.)
        for i in 0..self.chat.len() {
            if self.chat.get(i).is_some_and(|m| m.id.is_nil()) {
                let ordinal = self
                    .chat
                    .get(i)
                    .map(|m| m.from.clone())
                    .map(|from| self.sender_ordinal(&from, i));
                let id = self
                    .chat
                    .get(i)
                    .zip(ordinal)
                    .map(|(m, o)| legacy_message_id(o, &m.from, m.ts, &m.body));
                if let (Some(m), Some(id)) = (self.chat.get_mut(i), id) {
                    m.id = id;
                }
            }
            // (WP4a: a pruned node never resolves a position again — see the
            // same guard in `apply`'s Chat arm.)
            let unresolved_quote = self
                .chat
                .get(i)
                .filter(|_| !self.chat_pruned)
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
                // re-join the payloads with their id track (additive dump
                // field): a pre-id dump — or a short/absent id vec — yields
                // `None` (origin unknown), payloads untouched
                let ids = dump.applied_ids.get(&name).cloned().unwrap_or_default();
                let mut ids = ids.into_iter();
                self.applied.insert(
                    s,
                    log.into_iter()
                        .map(|v| (ids.next().unwrap_or(None), v))
                        .collect(),
                );
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
        // the accept windows belong to the OLD workspace's senders — leaking
        // them would dedup-drop the NEXT workspace's fresh envelopes
        self.accepted.clear();
        self.accepted_dirty = false;
        self.accepted_saved_at = 0;
        self.mls_persisted_at = 0;
        self.ack_due.clear();
        self.last_own_ackable = 0;
        self.ordered_park.clear();
        // send-failure presence pins belong to the OLD workspace's mesh —
        // dropping them stops a same-named member showing offline in the next
        self.net_unreachable.clear();
        self.net_link_down.clear();
        self.net_send_stuck.clear();
        // …as does the transport-kind affordance flag (§10.7/§6.5)
        self.session.transport = String::new();
        // …and the read cursors, which belong to the OLD workspace (B2)
        self.read_cursors.clear();
        self.last_mesh_out = 0;
        // a runtime-derived Degraded belongs to the mesh that just ended —
        // it resets with its backing maps (a Down verdict is the open/config
        // path's and stays until the next resolve)
        if matches!(self.session.net_health, molt_core::NetHealth::Degraded { .. }) {
            self.session.net_health = molt_core::NetHealth::Ok;
        }
        self.replica = None;
        self.identity_sk = None;
        self.transport_kind = None;
        self.nostr = None;
        // the recovery inboxes are INBOUND-only (they subscribe and read), so
        // aborting is safe — there is no outbound frame in flight to drop,
        // which is the one case the delivery guarantee forbids aborting
        for task in self.recovery_inboxes.drain(..) {
            task.abort();
        }
        // the group runtime's OUTBOX is DRAINED, not aborted: a frame between
        // seal and relay-OK would otherwise vanish silently. The drain awaits,
        // and the actor never awaits — so it rides a spawned task, the same
        // pattern every other off-actor teardown uses.
        if let Some(group) = self.group_net.take() {
            tokio::spawn(async move { group.handle.shutdown().await });
        }
        self.chain.clear();
        self.chain_head = None;
        self.set_checkpoint_blob(None);
        self.pending_served_blob = None;
        self.chain_applied.clear();
        self.pending_sigs.clear();
        self.proposal_changes.clear();
        self.pending_blocks.clear();
        self.catchup_from = None;
        self.pending_recovery.clear();
        self.chat.clear();
        self.chat_pos.clear();
        // the compaction state belongs to the workspace we are leaving:
        // carrying `chat_pruned` over would make the NEXT workspace refuse
        // legacy index-addressed ops it can still resolve, and carrying the
        // per-sender counts over would synthesize WRONG legacy ids there
        // (the ordinal would start above zero) — cross-workspace divergence
        self.chat_pruned = false;
        self.chat_pruned_counts.clear();
        self.compacted_at = 0;
        self.parked.clear();
        self.share_paths.clear();
        self.downloads.clear();
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
        let e = |seq: u64, by: &str, body: WorkspaceEvent| EventEnvelope { prev_seq: 0,
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
                kind: molt_core::ChatKind::User,
                reactions: Default::default(),
                read_by: Default::default(),
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
                    relays: Vec::new(),
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
                    op: None,
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
                    // legacy fixture: pre-transfer shares carry no checksum
                    checksum: String::new(),
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

    /// A decline is one member's voice, not a veto: in a 2-of-3 republic ONE
    /// decline leaves the threshold reachable (two other members can still
    /// approve), so the proposal stays pending; only when approval becomes
    /// impossible (declines > n − m) does it turn Rejected. A repeated
    /// decline by the same member stays one voice (at-least-once delivery).
    #[test]
    fn a_single_decline_in_two_of_three_is_not_a_veto() {
        let mut st = plain_state();
        let e = |seq: u64, by: &str, body: WorkspaceEvent| EventEnvelope { prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: by.to_string(),
            body,
        };
        st.apply(&e(
            1,
            "petra",
            WorkspaceEvent::Founded {
                name: "Trio".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "petra".to_string(),
                roster: vec!["petra".to_string(), "walter".to_string(), "ida".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
                relays: Vec::new(),
            },
        ));
        st.apply(&e(
            2,
            "petra",
            WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: molt_core::Surface::Organization,
                payload: json!({"op":"set_image"}),
            },
        ));
        // one decline: the threshold (2) is still reachable via petra + ida
        st.apply(&e(
            3,
            "walter",
            WorkspaceEvent::Declined {
                id: ProposalId(1),
                by: "walter".to_string(),
            },
        ));
        assert_eq!(
            st.dump().proposals[&1].state,
            molt_core::ProposalState::Proposed,
            "one decline in 2-of-3 must not reject"
        );
        // the same member declining again stays ONE voice
        st.apply(&e(
            4,
            "walter",
            WorkspaceEvent::Declined {
                id: ProposalId(1),
                by: "walter".to_string(),
            },
        ));
        assert_eq!(
            st.dump().proposals[&1].state,
            molt_core::ProposalState::Proposed,
            "a repeated decline is not a second voice"
        );
        // the second DISTINCT decline makes approval impossible → rejected
        st.apply(&e(
            5,
            "ida",
            WorkspaceEvent::Declined {
                id: ProposalId(1),
                by: "ida".to_string(),
            },
        ));
        let d = st.dump();
        assert_eq!(d.proposals[&1].state, molt_core::ProposalState::Rejected);
        assert_eq!(
            d.proposals[&1].declined_by, "ida",
            "the tipping decliner is the recorded one"
        );
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
        all.push(EventEnvelope { prev_seq: 0,
            seq: 8,
            ts: 108,
            by: "walter".to_string(),
            body: WorkspaceEvent::Chat(msg),
        });
        all.push(EventEnvelope { prev_seq: 0,
            seq: 9,
            ts: 109,
            by: "petra".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 3,
                id: Some(new_id),
                emoji: "🔥".to_string(),
                by: "petra".to_string(),
                op: None,
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
        // the P4 formula is a cross-node contract: pin the synthesized ids as
        // literals so a formula change can never slip through silently.
        // Message 0: petra's FIRST message (sender ordinal 0, ts 102, "gm").
        assert_eq!(st.chat[0].id.to_string(), LEGACY_ID_OF_MSG_0);
        // Message 1: walter's FIRST message (sender ordinal 0 — NOT its
        // global position 1, which differs between nodes; ts 103, "re: gm").
        assert_eq!(st.chat[1].id.to_string(), LEGACY_ID_OF_MSG_1);
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

    /// **WP4a keystone (F9): a PRUNED snapshot + continued replay must not
    /// disturb what the log still means.** This is the proof the compaction
    /// design stands on (`docs/chain/log_compaction.md` §A.5/F9) — it runs
    /// BEFORE any compactor code, and a red run is a design stop, not a bug to
    /// patch.
    ///
    /// The compactor cuts at the HEAD (it writes the trimmed state as the
    /// snapshot the log continues from), so the events that follow a prune are
    /// the ones still to come. Over several retention cutoffs this pins:
    /// 1. **Nothing inside the window is lost, everything past it is gone.**
    /// 2. **Surviving messages are byte-identical** to the same id on a node
    ///    that never pruned — the two legacy POSITIONAL fields may degrade to
    ///    "unresolved" (a position cannot be re-resolved once a prefix is
    ///    dropped) but never to a DIFFERENT value, which would be
    ///    mis-attribution.
    /// 3. **Ids synthesized after a prune still match the peers'.** A legacy
    ///    id hashes a per-sender ordinal; the pruned node carries the dropped
    ///    count forward, so the very next legacy message from that sender gets
    ///    the same id everywhere. This is the property that made pruning
    ///    dangerous, and the one `chat_pruned_counts` exists for.
    /// 4. **`chat_pos` stays consistent** with the shortened log.
    #[test]
    fn a_pruned_snapshot_plus_replay_keeps_every_surviving_message_identical() {
        let all = mixed_envs();
        // what still happens AFTER the compaction: an id-addressed reaction on
        // a surviving message, and a LEGACY (nil-id) message from a sender
        // whose earlier messages were pruned — its synthesized id must match
        // the never-pruned node's
        let after = |from_seq: u64| -> Vec<EventEnvelope> {
            let mut legacy = ChatMessage::text(molt_core::MessageId::NIL, "petra", "later", 200);
            legacy.id = molt_core::MessageId::NIL;
            vec![
                EventEnvelope { prev_seq: 0,
                    seq: from_seq,
                    ts: 200,
                    by: "petra".to_string(),
                    body: WorkspaceEvent::Chat(legacy),
                },
                EventEnvelope { prev_seq: 0,
                    seq: from_seq + 1,
                    ts: 201,
                    by: "petra".to_string(),
                    body: WorkspaceEvent::ChatReacted {
                        index: 0,
                        id: Some(molt_core::MessageId([0x42; 16])),
                        emoji: "🎉".to_string(),
                        by: "petra".to_string(),
                        op: Some(molt_core::ReactOp::Add),
                    },
                },
            ]
        };
        let full = {
            let mut st = plain_state();
            for env in all.iter().chain(after(10).iter()) {
                st.apply(env);
            }
            st.dump()
        };

        for cutoff in [103u64, 105, 108, 109] {
            let mut st = plain_state();
            for env in &all {
                st.apply(env);
            }
            let mut snap = st.dump();
            let dropped = snap.prune_chat_before(cutoff);
            assert!(
                snap.chat.iter().all(|m| !(m.ts != 0 && m.ts < cutoff)),
                "pruning left expired content behind (cutoff {cutoff})"
            );
            if dropped > 0 {
                assert!(snap.chat_pruned, "a prune marks the dump as pruned");
                assert_eq!(
                    snap.chat_pruned_counts.values().sum::<u64>(),
                    u64::try_from(dropped).expect("dropped count fits u64"),
                    "every dropped message is carried in the per-sender counts"
                );
            }
            let mut st2 = plain_state();
            st2.restore_dump(snap);
            for env in &after(10) {
                st2.apply(env);
            }
            let got = st2.dump();

            // 1. the window boundary held in both directions
            for f in full.chat.iter().filter(|f| f.ts == 0 || f.ts >= cutoff) {
                assert!(
                    got.chat.iter().any(|m| m.id == f.id),
                    "message {} inside the window was lost (cutoff {cutoff})",
                    f.id
                );
            }
            assert!(
                got.chat.iter().all(|m| m.ts == 0 || m.ts >= cutoff),
                "expired content came back (cutoff {cutoff})"
            );
            // 2. + 3. every message both nodes hold is identical, INCLUDING
            //    the legacy message ingested after the prune (its id proves
            //    the sender ordinal survived the compaction)
            for m in &got.chat {
                let same = full
                    .chat
                    .iter()
                    .find(|f| f.id == m.id)
                    .unwrap_or_else(|| panic!("invented message {} (cutoff {cutoff})", m.id));
                let mut normalized = m.clone();
                if m.quote.is_none() {
                    normalized.quote = same.quote;
                }
                if m.quote_id.is_none() {
                    normalized.quote_id = same.quote_id;
                }
                assert_eq!(
                    &normalized, same,
                    "message {} diverged from the never-pruned node (cutoff {cutoff})",
                    m.id
                );
            }
            assert!(
                got.chat.iter().any(|m| m.body == "later"),
                "the legacy message ingested after the prune kept the peers' id (cutoff {cutoff})"
            );
            // 4. the id→position map matches the shortened log
            for (i, m) in got.chat.iter().enumerate() {
                assert_eq!(
                    st2.chat_pos.get(&m.id),
                    Some(&i),
                    "chat_pos lost message {} (cutoff {cutoff})",
                    m.id
                );
            }
        }

        // the cross-node id contract, spelled out: a node that pruned before
        // ts 105 no longer HAS the two pinned legacy messages, and the ones it
        // keeps are untouched
        let mut st = plain_state();
        for env in &all {
            st.apply(env);
        }
        let mut snap = st.dump();
        assert_eq!(snap.prune_chat_before(105), 2, "gm + re: gm age out at 105");
        assert!(
            snap.chat.iter().all(|m| m.id.to_string() != LEGACY_ID_OF_MSG_0
                && m.id.to_string() != LEGACY_ID_OF_MSG_1),
            "the expired legacy messages are physically gone"
        );
        let mut st2 = plain_state();
        st2.restore_dump(snap);
        assert_eq!(st2.chat.len(), 2, "the share + the new-era message survive");
        assert_eq!(st2.chat[1].id, molt_core::MessageId([0x42; 16]));
    }

    /// **The compaction state must not cross the workspace boundary.** It
    /// describes ONE workspace's log: carrying `chat_pruned` into the next
    /// would refuse legacy index-addressed ops it can still resolve, and
    /// carrying the per-sender counts would start that workspace's legacy
    /// ordinals above zero — synthesizing ids no peer agrees with.
    #[test]
    fn closing_a_workspace_clears_its_compaction_state() {
        let mut st = plain_state();
        for env in &mixed_envs() {
            st.apply(env);
        }
        let mut snap = st.dump();
        assert!(snap.prune_chat_before(105) > 0, "something was dropped");
        st.restore_dump(snap);
        assert!(st.chat_pruned);
        assert!(!st.chat_pruned_counts.is_empty());

        st.reset_workspace_state();
        assert!(!st.chat_pruned, "the next workspace starts unpruned");
        assert!(st.chat_pruned_counts.is_empty(), "and with no carried ordinals");

        // proof it matters: the SAME legacy log now synthesizes the SAME ids
        // as on a node that never pruned anything
        for env in &mixed_envs() {
            st.apply(env);
        }
        assert_eq!(st.chat[0].id.to_string(), LEGACY_ID_OF_MSG_0);
        assert_eq!(st.chat[1].id.to_string(), LEGACY_ID_OF_MSG_1);
    }

    /// **A legacy INDEX-addressed op must be ignored once this node pruned.**
    /// Positions move when content is dropped, so honouring the index would
    /// react on / delete / un-share an innocent SURVIVING message — silent
    /// corruption. Dropping the op is the honest outcome (it addresses a
    /// message this node no longer has); an id-addressed op is unaffected.
    /// The same rule covers the legacy numeric `quote`, which would otherwise
    /// attribute a reply to the wrong message.
    #[test]
    fn legacy_index_addressed_ops_are_ignored_once_pruned() {
        let mut st = plain_state();
        for env in &mixed_envs() {
            st.apply(env);
        }
        let mut snap = st.dump();
        assert_eq!(snap.prune_chat_before(105), 2, "the two oldest go");
        let mut st = plain_state();
        st.restore_dump(snap);
        let survivor = st.chat[0].id;
        let before = st.chat.clone();

        // an index-addressed reaction/delete/file-removal at position 0 —
        // which USED to mean petra's "gm" and now would hit the share
        for body in [
            WorkspaceEvent::ChatReacted {
                index: 0,
                id: None,
                emoji: "👍".to_string(),
                by: "walter".to_string(),
                op: None,
            },
            WorkspaceEvent::ChatDeleted {
                index: 0,
                id: None,
                by: "walter".to_string(),
            },
            WorkspaceEvent::FileRemoved {
                index: 0,
                id: None,
                by: "walter".to_string(),
            },
        ] {
            st.apply(&EventEnvelope { prev_seq: 0,
                seq: 20,
                ts: 210,
                by: "walter".to_string(),
                body,
            });
        }
        assert_eq!(st.chat, before, "no id-less op touched a surviving message");

        // …while the SAME ops addressed by id still land
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 21,
            ts: 211,
            by: "walter".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 999,
                id: Some(survivor),
                emoji: "👍".to_string(),
                by: "walter".to_string(),
                op: Some(molt_core::ReactOp::Add),
            },
        });
        assert_eq!(
            st.chat[0].reactions["👍"],
            vec!["walter".to_string()],
            "an id-addressed op is unaffected by pruning"
        );

        // a legacy numeric quote is no longer resolved by position
        let mut quoting = ChatMessage::text(molt_core::MessageId([0x77; 16]), "walter", "re", 212);
        quoting.quote = Some(0);
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 22,
            ts: 212,
            by: "walter".to_string(),
            body: WorkspaceEvent::Chat(quoting),
        });
        let posted = st.chat.last().expect("the quoting message landed");
        assert_eq!(
            posted.quote_id, None,
            "a positional quote stays unresolved on a pruned node instead of pointing at the wrong message"
        );
    }

    /// A dump that still carries a **nil-id (pre-chat-bus) message** must not
    /// be pruned at all: the legacy id is synthesized from the per-sender
    /// ordinal counted over the dump, so dropping a prefix before synthesis
    /// would hand this node DIFFERENT ids than its peers — silent divergence.
    /// The compactor skips such a dump instead (it materializes on the next
    /// open, and the round after prunes normally).
    #[test]
    fn a_dump_with_unsynthesized_legacy_ids_refuses_to_prune() {
        let mut dump = plain_state().dump();
        let mut legacy = ChatMessage::text(molt_core::MessageId([1u8; 16]), "petra", "old", 100);
        legacy.id = molt_core::MessageId::NIL;
        dump.chat = vec![
            legacy,
            ChatMessage::text(molt_core::MessageId([7u8; 16]), "petra", "new", 200),
        ];
        assert_eq!(dump.prune_chat_before(150), 0, "nothing is dropped");
        assert_eq!(dump.chat.len(), 2, "the dump is left exactly as it was");
        assert!(!dump.chat_pruned, "and it is not marked pruned");
    }

    /// The P4 pins for `legacy_log_replay_synthesizes_stable_ids` — the
    /// hashed ordinal is the **per-sender** one (cross-node stable), so both
    /// pins hash `le64(0)`:
    /// `sha256("molt-chat-legacy-id\0" ‖ le64(0) ‖ "petra" ‖ le64(102) ‖ "gm")[..16]`.
    const LEGACY_ID_OF_MSG_0: &str = "bbb7bc990b87cf10ecf6ed59f31e8ce2";
    /// `sha256("molt-chat-legacy-id\0" ‖ le64(0) ‖ "walter" ‖ le64(103) ‖ "re: gm")[..16]`.
    const LEGACY_ID_OF_MSG_1: &str = "a69dfac27e835b034302877efee908ea";

    /// Explicit react ops are **idempotent** (an at-least-once transport
    /// may deliver the same frame twice — the transport redelivers un-acked
    /// frames after a hard crash, the MLS path has no wire-seq cursor), while a
    /// legacy op-less event keeps its original toggle semantics on replay.
    #[test]
    fn explicit_react_ops_are_idempotent_but_legacy_toggles() {
        let mut st = plain_state();
        let id = molt_core::MessageId([9u8; 16]);
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 101,
            by: "petra".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(id, "petra", "hi", 101)),
        });
        let mut seq = 1u64;
        let mut react = |st: &mut crate::State, emoji: &str, op: Option<molt_core::ReactOp>| {
            seq += 1;
            st.apply(&EventEnvelope { prev_seq: 0,
                seq,
                ts: 100 + seq,
                by: "walter".to_string(),
                body: WorkspaceEvent::ChatReacted {
                    index: 0,
                    id: Some(id),
                    emoji: emoji.to_string(),
                    by: "walter".to_string(),
                    op,
                },
            });
        };

        // a duplicated Add must NOT invert the state
        react(&mut st, "👍", Some(molt_core::ReactOp::Add));
        react(&mut st, "👍", Some(molt_core::ReactOp::Add));
        assert_eq!(
            st.chat[0].reactions["👍"],
            vec!["walter".to_string()],
            "Add twice stays reacted once"
        );
        // Add of another emoji switches (one reaction per member)
        react(&mut st, "🔥", Some(molt_core::ReactOp::Add));
        assert!(!st.chat[0].reactions.contains_key("👍"), "the old emoji is gone");
        assert_eq!(st.chat[0].reactions["🔥"], vec!["walter".to_string()]);
        // a duplicated Remove must NOT re-add anything
        react(&mut st, "🔥", Some(molt_core::ReactOp::Remove));
        react(&mut st, "🔥", Some(molt_core::ReactOp::Remove));
        assert!(
            st.chat[0].reactions.is_empty(),
            "Remove twice stays removed: {:?}",
            st.chat[0].reactions
        );
        // a LEGACY op-less event still toggles: on, then off again
        react(&mut st, "🎉", None);
        assert_eq!(st.chat[0].reactions["🎉"], vec!["walter".to_string()]);
        react(&mut st, "🎉", None);
        assert!(st.chat[0].reactions.is_empty(), "the legacy toggle un-reacts");
    }

    /// A concurrent react/delete pair must **commute**: whichever order the
    /// two events arrive in, both nodes end on the identical tombstone
    /// without reactions (a reaction never lands on a tombstone).
    #[test]
    fn react_and_delete_commute_to_a_tombstone_without_reactions() {
        let id = molt_core::MessageId([0x0bu8; 16]);
        let chat = EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 101,
            by: "petra".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(id, "petra", "fleeting", 101)),
        };
        let react = EventEnvelope { prev_seq: 0,
            seq: 2,
            ts: 102,
            by: "walter".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 0,
                id: Some(id),
                emoji: "👍".to_string(),
                by: "walter".to_string(),
                op: Some(molt_core::ReactOp::Add),
            },
        };
        let delete = EventEnvelope { prev_seq: 0,
            seq: 3,
            ts: 103,
            by: "petra".to_string(),
            body: WorkspaceEvent::ChatDeleted {
                index: 0,
                id: Some(id),
                by: "petra".to_string(),
            },
        };
        let run = |order: [&EventEnvelope; 3]| {
            let mut st = plain_state();
            for env in order {
                st.apply(env);
            }
            st.dump()
        };
        let react_then_delete = run([&chat, &react, &delete]);
        let delete_then_react = run([&chat, &delete, &react]);
        assert_eq!(
            react_then_delete, delete_then_react,
            "the pair must converge independent of arrival order"
        );
        assert_eq!(react_then_delete.chat[0].deleted_by.as_deref(), Some("petra"));
        assert!(
            react_then_delete.chat[0].reactions.is_empty(),
            "no reaction survives on the tombstone"
        );
    }

    /// P4 across NODES: pre-chat-bus delivery is in-order **per sender
    /// only**, so two nodes hold the same legacy messages at different
    /// global positions. The id synthesis must still agree — it hashes the
    /// per-sender ordinal, which IS identical everywhere.
    #[test]
    fn legacy_ids_are_stable_across_cross_sender_interleavings() {
        let msg = |from: &str, body: &str, ts: u64| ChatMessage {
            id: molt_core::MessageId::NIL,
            from: from.to_string(),
            body: body.to_string(),
            ts,
            quote: None,
            quote_id: None,
            channel: molt_core::ChannelRef::Group,
            kind: molt_core::ChatKind::User,
            reactions: Default::default(),
            read_by: Default::default(),
            deleted_by: None,
            file: None,
        };
        let env = |seq: u64, m: &ChatMessage| EventEnvelope { prev_seq: 0,
            seq,
            ts: m.ts,
            by: m.from.clone(),
            body: WorkspaceEvent::Chat(m.clone()),
        };
        let p1 = msg("petra", "p first", 101);
        let p2 = msg("petra", "p second", 102);
        let w1 = msg("walter", "w first", 103);
        let w2 = msg("walter", "w second", 104);
        let run = |order: [&ChatMessage; 4]| {
            let mut st = plain_state();
            for (i, m) in order.iter().enumerate() {
                st.apply(&env(u64::try_from(i).expect("tiny") + 1, m));
            }
            st.chat
                .iter()
                .map(|m| ((m.from.clone(), m.body.clone()), m.id))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        // node A interleaves the senders; node B sees petra's burst first —
        // both respect the per-sender order (p1<p2, w1<w2)
        let node_a = run([&p1, &w1, &p2, &w2]);
        let node_b = run([&p1, &p2, &w1, &w2]);
        assert_eq!(
            node_a, node_b,
            "the same legacy message must synthesize the same id on every node"
        );
        let distinct: std::collections::BTreeSet<_> = node_a.values().collect();
        assert_eq!(distinct.len(), 4, "all synthesized ids stay distinct");
    }

    /// Envelopes that no longer match the state (corrupted log) are ignored,
    /// never a panic.
    #[test]
    fn out_of_range_events_are_ignored() {
        let mut st = plain_state();
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 1,
            by: "x".to_string(),
            body: WorkspaceEvent::ChatDeleted {
                index: 99,
                id: None,
                by: "x".to_string(),
            },
        });
        st.apply(&EventEnvelope { prev_seq: 0,
            seq: 2,
            ts: 2,
            by: "x".to_string(),
            body: WorkspaceEvent::Applied { id: ProposalId(9) },
        });
        assert!(st.dump().chat.is_empty());
        assert!(st.dump().applied.is_empty());
    }
}
