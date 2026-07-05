// SPDX-License-Identifier: GPL-3.0-or-later

//! The gated surfaces: propose → threshold approvals → apply. A faithful
//! but *simulated* stand-in for the real FROST threshold machine.

use molt_core::{
    Event, MoltError, ProposalId, ProposalState, ProposalView, Reply, StatusView, Surface,
    SurfaceSnapshot, SurfaceStat,
};
use serde_json::Value;

use crate::{Proposal, State};

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
        let id = self.next_id;
        self.next_id += 1;
        let approvals = if self.config.self_cosign { 1 } else { 0 };
        self.proposals.insert(
            id,
            Proposal {
                surface,
                payload,
                approvals,
                state: ProposalState::Proposed,
            },
        );
        self.emit(Event::Proposed {
            id: ProposalId(id),
            surface,
        });
        // A self-cosign may already satisfy a threshold of 1.
        self.try_apply(id);
        Ok(Reply::Proposed { id: ProposalId(id) })
    }

    pub(crate) fn cmd_approve(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        let pid = proposal.0;
        {
            let p = self
                .proposals
                .get_mut(&pid)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
            p.approvals += 1;
        }
        let have = self.proposals[&pid].approvals;
        self.emit(Event::Approved {
            id: proposal,
            have,
            need: self.threshold(),
        });
        self.try_apply(pid);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_decline(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        let p = self
            .proposals
            .get_mut(&proposal.0)
            .ok_or(MoltError::UnknownProposal(proposal))?;
        if p.state != ProposalState::Proposed {
            return Err(MoltError::AlreadyTerminal(proposal, p.state));
        }
        p.state = ProposalState::Rejected;
        self.emit(Event::Rejected { id: proposal });
        Ok(Reply::Ack)
    }

    /// Apply a proposal if it has reached the threshold.
    fn try_apply(&mut self, pid: u64) {
        let (surface, payload, ready) = match self.proposals.get(&pid) {
            Some(p) if p.state == ProposalState::Proposed => (
                p.surface,
                p.payload.clone(),
                p.approvals >= self.threshold(),
            ),
            _ => return,
        };
        if !ready {
            return;
        }
        self.applied.entry(surface).or_default().push(payload);
        if let Some(p) = self.proposals.get_mut(&pid) {
            p.state = ProposalState::Applied;
        }
        self.emit(Event::Applied {
            id: ProposalId(pid),
            surface,
        });
    }

    pub(crate) fn view(&self, id: u64, p: &Proposal) -> ProposalView {
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
            self.applied.get(&surface).cloned().unwrap_or_default()
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
            member: self.config.member.clone(),
            members: self.config.members.clone(),
            threshold: self.threshold(),
            surfaces,
        }
    }
}
