// SPDX-License-Identifier: GPL-3.0-or-later

//! The gated surfaces: propose → threshold approvals → apply. A faithful
//! but *simulated* stand-in for the real FROST threshold machine.

use std::collections::HashMap;

use molt_core::{
    ChannelInfo, ChannelRef, Event, MemberView, MoltError, ProposalId, ProposalRecord,
    ProposalState, ProposalView, Reply, StatusView, Surface, SurfaceSnapshot, SurfaceStat,
    UploadView, WorkspaceEvent,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ReplicaState, State};

/// The "Ist-Stand / Soll-Stand" display pair of a proposal: what the
/// targeted state is now (from the genesis replica, for the Organization
/// edit ops) and what the change would make it (the payload's `value`).
/// Mock-grade display data, never consensus input — "" when unknown.
pub(crate) fn change_summary(
    replica: Option<&ReplicaState>,
    p: &ProposalRecord,
) -> (String, String) {
    let proposed = p
        .payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if p.surface != Surface::Organization {
        return (String::new(), proposed);
    }
    let op = p.payload.get("op").and_then(Value::as_str).unwrap_or("");
    let current = match op {
        "set_charter" => replica.map(|r| r.agenda.clone()).unwrap_or_default(),
        "set_name" => replica.map(|r| r.name.clone()).unwrap_or_default(),
        // the chat-retention setting is not yet engine state — its mock
        // default is the Ist-Stand until the real setting lands
        "set_chat_retention" => "7 days".to_string(),
        // no image / plugin state exists yet (mock) — nothing to show
        _ => String::new(),
    };
    (current, proposed)
}

impl State {
    pub(crate) fn cmd_propose(
        &mut self,
        surface: Surface,
        payload: Value,
    ) -> Result<Reply, MoltError> {
        if !surface.is_gated() {
            return Err(MoltError::ChatNotGated);
        }
        if !payload.is_object() {
            return Err(MoltError::BadPayload(
                "payload must be a JSON object".into(),
            ));
        }
        let me = self.member();
        let id = ProposalId(self.next_id);
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Proposed {
                id,
                surface,
                payload,
            },
        );
        self.record(env);
        self.emit(Event::Proposed { id, surface });
        if self.is_chain_governed() {
            // real threshold: the proposer co-signs their own proposal; the
            // other members' signatures arrive over the mesh
            if self.config.self_cosign {
                self.chain_sign_and_gossip_approval(id.0);
            }
        } else {
            if self.config.self_cosign {
                // legacy counted simulation — the proposer's own approval is an
                // event too, so replay must not depend on the config flag
                let env = self.make_env(
                    me.clone(),
                    WorkspaceEvent::Approved {
                        id,
                        by: me,
                        height: 0,
                        sig: String::new(),
                    },
                );
                self.record(env);
            }
            // A self-cosign may already satisfy a threshold of 1.
            self.try_apply(id);
        }
        Ok(Reply::Proposed { id })
    }

    pub(crate) fn cmd_approve(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
        }
        if self.is_chain_governed() {
            // real threshold: sign + gossip; a block seals once m distinct
            // members have signed (here or over the mesh)
            self.chain_sign_and_gossip_approval(proposal.0);
            let have = self.chain_approval_count(proposal.0);
            self.emit(Event::Approved {
                id: proposal,
                have,
                need: self.threshold(),
            });
        } else {
            let me = self.member();
            let env = self.make_env(
                me.clone(),
                WorkspaceEvent::Approved {
                    id: proposal,
                    by: me,
                    height: 0,
                    sig: String::new(),
                },
            );
            self.record(env);
            let have = self.proposals.get(&proposal.0).map(|p| p.approvals).unwrap_or(0);
            self.emit(Event::Approved {
                id: proposal,
                have,
                need: self.threshold(),
            });
            self.try_apply(proposal);
        }
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_decline(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
        }
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Declined {
                id: proposal,
                by: me,
            },
        );
        self.record(env);
        self.emit(Event::Rejected { id: proposal });
        Ok(Reply::Ack)
    }

    /// Record the `Applied` event once a proposal has reached the threshold.
    /// The threshold *decision* happens here, at event-creation time; the
    /// outcome is an event of its own, so replay never re-decides it.
    fn try_apply(&mut self, id: ProposalId) {
        let ready = matches!(
            self.proposals.get(&id.0),
            Some(p) if p.state == ProposalState::Proposed
                && p.approvals >= self.threshold()
        );
        if !ready {
            return;
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Applied { id });
        self.record(env);
        if let Some(surface) = self.proposals.get(&id.0).map(|p| p.surface) {
            self.emit(Event::Applied { id, surface });
        }
    }

    /// Re-decide thresholds after a replay: a crash between an `Approved`
    /// frame and its `Applied` frame must not leave a proposal stuck at
    /// `have >= need` forever. Called once per open, after the tail applied.
    ///
    /// Legacy path only: a chain-governed workspace never applies by counting —
    /// the replayed `Approved` frames are real signatures the chain already
    /// consumed (or not), so re-counting them here would double-apply.
    pub(crate) fn recover_pending_applies(&mut self) {
        if self.is_chain_governed() {
            return;
        }
        let ready: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed && p.approvals >= self.threshold())
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            self.try_apply(ProposalId(id));
        }
    }

    pub(crate) fn view(&self, id: u64, p: &ProposalRecord) -> ProposalView {
        // a chain-governed proposal's real progress is the count of distinct
        // collected signatures, not the legacy counter
        let approvals = if self.is_chain_governed() {
            self.chain_approval_count(id)
        } else {
            p.approvals
        };
        // reader-relative: chain governance knows exactly who signed; the
        // legacy counted simulation has one local operator standing in for
        // the whole group, where the FIRST approval is by definition ours
        // (self-cosign or the explicit approve) and repeats simulate peers
        let approved_by_me = if self.is_chain_governed() {
            let me = self.member();
            self.pending_sigs
                .get(&id)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == me))
        } else {
            p.approvals > 0
        };
        let (current, proposed) = change_summary(self.replica.as_ref(), p);
        ProposalView {
            id: ProposalId(id),
            surface: p.surface,
            payload: p.payload.clone(),
            approvals,
            threshold: self.threshold(),
            state: p.state,
            approved_by_me,
            current,
            proposed,
        }
    }

    /// Applied log of one surface, as wire values. Chat serializes its typed
    /// messages into the same JSON shape the log always had; a `channel`
    /// filter (chat only) keeps exactly the messages filing under that
    /// channel — exact [`ChannelRef`] equality, so Topic names match by
    /// exact string (pin P3). Filtered rows keep their embedded ids;
    /// position-in-`applied` is not an addressing scheme.
    fn applied_values(&self, surface: Surface, channel: Option<&ChannelRef>) -> Vec<Value> {
        if surface == Surface::Chat {
            self.chat
                .iter()
                .filter(|m| channel.map_or(true, |c| &m.channel == c))
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .collect()
        } else {
            // the surface's applied log is the legacy (counted-simulation)
            // projection plus the chain (real threshold) projection — one of the
            // two is always empty for a given workspace, so this is a concat
            let mut v = self.applied.get(&surface).cloned().unwrap_or_default();
            if let Some(chain) = self.chain_applied.get(&surface) {
                v.extend(chain.iter().cloned());
            }
            v
        }
    }

    /// Every distinct channel in the chat log, one pass (chat-bus pin P7):
    /// `Group` is always listed (even when empty); the rest follow in
    /// first-appearance order, which is deterministic because the log
    /// order is canonical. Deleted (tombstoned) messages still count for
    /// their channel — they are rows in the log, and a channel whose only
    /// message was deleted must not silently vanish from the sidebar.
    fn chat_channels(&self) -> Vec<ChannelInfo> {
        let mut infos = vec![ChannelInfo {
            channel: ChannelRef::Group,
            count: 0,
            last_ts: 0,
        }];
        let mut pos: HashMap<ChannelRef, usize> = HashMap::from([(ChannelRef::Group, 0)]);
        for m in &self.chat {
            let at = *pos.entry(m.channel.clone()).or_insert_with(|| {
                infos.push(ChannelInfo {
                    channel: m.channel.clone(),
                    count: 0,
                    last_ts: 0,
                });
                infos.len() - 1
            });
            infos[at].count += 1;
            infos[at].last_ts = infos[at].last_ts.max(m.ts);
        }
        infos
    }

    /// The read contract: the (possibly channel-filtered) applied log plus,
    /// on the chat surface, the always-unfiltered channel enumeration.
    /// Other surfaces ignore `channel` and keep `channels` empty.
    pub(crate) fn snapshot(&self, surface: Surface, channel: Option<ChannelRef>) -> SurfaceSnapshot {
        let pending: Vec<ProposalView> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == surface && p.state == ProposalState::Proposed)
            .map(|(id, p)| self.view(*id, p))
            .collect();
        SurfaceSnapshot {
            surface,
            gated: surface.is_gated(),
            applied: self.applied_values(surface, channel.as_ref()),
            pending,
            denied: self
                .proposals
                .values()
                .filter(|p| p.surface == surface && p.state == ProposalState::Rejected)
                .count(),
            channels: if surface == Surface::Chat {
                self.chat_channels()
            } else {
                Vec::new()
            },
        }
    }

    /// Whether a pending proposal still awaits `member`'s approval. Chain
    /// governance knows exactly who signed; the legacy counted simulation
    /// (one operator stands in for the group) treats the first approval as
    /// the local member's and cannot know about the simulated peers — for
    /// them every pending proposal counts as open (mock).
    fn waits_on(&self, id: u64, p: &ProposalRecord, member: &str) -> bool {
        if p.state != ProposalState::Proposed {
            return false;
        }
        if self.is_chain_governed() {
            !self
                .pending_sigs
                .get(&id)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == member))
        } else if member == self.member() {
            p.approvals == 0
        } else {
            true
        }
    }

    /// The Organization → Members table: one row per roster member. The
    /// identity anchor comes from the genesis (real on ritual-founded
    /// workspaces); presence is the session entry's mock label.
    pub(crate) fn members_view(&self) -> Vec<MemberView> {
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        self.roster()
            .into_iter()
            .map(|member| {
                let identity_pk = self
                    .replica
                    .as_ref()
                    .and_then(|r| r.identities.iter().find(|i| i.member == member))
                    .map(|i| i.identity_pk.clone())
                    .unwrap_or_default();
                // a human-scale fingerprint: the key's leading 16 hex chars
                let id = identity_pk.get(..16).unwrap_or_default().to_string();
                let (last_seen, presence) = entry
                    .and_then(|e| e.members.iter().find(|m| m.name == member))
                    .map(|m| (m.last.clone(), m.state))
                    .unwrap_or_default();
                MemberView {
                    open_proposals: self
                        .proposals
                        .iter()
                        .filter(|(pid, p)| self.waits_on(**pid, p, &member))
                        .count(),
                    uploads: self
                        .chat
                        .iter()
                        .filter(|m| m.from == member && m.file.is_some())
                        .count(),
                    member,
                    id,
                    identity_pk,
                    last_seen,
                    presence,
                }
            })
            .collect()
    }

    /// The Organization → Uploads table: every file shared into the chat,
    /// in log order. Only metadata — the bytes move user-to-user via the
    /// share link ([`molt_core::FileMeta`]), which is why a download needs
    /// the sharer online; the 14-day link expiry and the checksum are mocks
    /// like the fetch itself.
    pub(crate) fn uploads_view(&self) -> Vec<UploadView> {
        const MOCK_LINK_TTL_SECS: u64 = 14 * 86_400;
        let me = self.member();
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let presence = |member: &str| {
            entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.state)
                .unwrap_or(0)
        };
        self.chat
            .iter()
            .filter_map(|m| {
                m.file.as_ref().map(|f| {
                    // deterministic mock checksum: no bytes exist yet, so it
                    // hashes the share's identity — stable across reads/nodes
                    let mut h = Sha256::new_with_prefix(b"molt-upload-mock-checksum\0");
                    h.update(f.name.as_bytes());
                    h.update(f.size.to_le_bytes());
                    h.update(f.modified.to_le_bytes());
                    let checksum = h
                        .finalize()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();
                    UploadView {
                        id: m.id,
                        member: m.from.clone(),
                        ts: m.ts,
                        name: f.name.clone(),
                        kind: f.kind.clone(),
                        size: f.size,
                        available: f.available,
                        expires_ts: m.ts + MOCK_LINK_TTL_SECS,
                        online: m.from == me || presence(&m.from) != 2,
                        checksum,
                    }
                })
            })
            .collect()
    }

    pub(crate) fn status(&self) -> StatusView {
        // the activity trio is a mock presence projection (real presence is
        // transport work): synced = hour-active, syncing = day-active, the
        // whole roster = week-active
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let presence = |member: &str| {
            entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.state)
                .unwrap_or(0)
        };
        let roster = self.roster();
        let active_1h = roster.iter().filter(|m| presence(m) == 0).count();
        let active_24h = roster.iter().filter(|m| presence(m) <= 1).count();
        let active_7d = roster.len();
        let surfaces = Surface::ALL
            .into_iter()
            .map(|s| {
                let pending = self
                    .proposals
                    .values()
                    .filter(|p| p.surface == s && p.state == ProposalState::Proposed)
                    .count();
                let applied = if s == Surface::Chat {
                    self.chat.len()
                } else {
                    self.applied.get(&s).map(|v| v.len()).unwrap_or(0)
                        + self.chain_applied.get(&s).map(|v| v.len()).unwrap_or(0)
                };
                SurfaceStat {
                    surface: s,
                    gated: s.is_gated(),
                    applied,
                    pending,
                }
            })
            .collect();
        StatusView {
            member: self.member(),
            members: roster,
            threshold: self.threshold(),
            surfaces,
            founded_ts: self.replica.as_ref().map(|r| r.founded_ts).unwrap_or(0),
            active_1h,
            active_24h,
            active_7d,
        }
    }
}
