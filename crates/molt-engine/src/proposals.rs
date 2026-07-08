// SPDX-License-Identifier: GPL-3.0-or-later

//! The gated surfaces: propose → threshold approvals → apply. A faithful
//! but *simulated* stand-in for the real FROST threshold machine.

use molt_core::{
    Event, MoltError, ProposalId, ProposalRecord, ProposalState, ProposalView, Reply, StatusView,
    Surface, SurfaceSnapshot, SurfaceStat, WorkspaceEvent,
};
use serde_json::Value;

use crate::State;

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
        if self.config.self_cosign {
            // the proposer's own approval is an event too — replay must not
            // depend on the config flag
            let env = self.make_env(me.clone(), WorkspaceEvent::Approved { id, by: me });
            self.record(env);
        }
        // A self-cosign may already satisfy a threshold of 1.
        self.try_apply(id);
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
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Approved {
                id: proposal,
                by: me,
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
    pub(crate) fn recover_pending_applies(&mut self) {
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
        ProposalView {
            id: ProposalId(id),
            surface: p.surface,
            payload: p.payload.clone(),
            approvals: p.approvals,
            threshold: self.threshold(),
            state: p.state,
        }
    }

    /// Applied log of one surface, as wire values. Chat serializes its typed
    /// messages into the same JSON shape the log always had.
    fn applied_values(&self, surface: Surface) -> Vec<Value> {
        if surface == Surface::Chat {
            self.chat
                .iter()
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

    pub(crate) fn snapshot(&self, surface: Surface) -> SurfaceSnapshot {
        let pending: Vec<ProposalView> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == surface && p.state == ProposalState::Proposed)
            .map(|(id, p)| self.view(*id, p))
            .collect();
        SurfaceSnapshot {
            surface,
            gated: surface.is_gated(),
            applied: self.applied_values(surface),
            pending,
        }
    }

    pub(crate) fn status(&self) -> StatusView {
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
            members: self.roster(),
            threshold: self.threshold(),
            surfaces,
        }
    }
}
