// SPDX-License-Identifier: GPL-3.0-or-later

//! **The automatic evidence exchange** (`docs/chain/chain_rescue_plan.md`
//! §5a): a seat sends the blocks a peer lacks over the seats' own wrapped
//! channel (the ritual gift wrap to the peer's working anchor, outside the
//! group), the peer verifies them like a file import and holds them; one
//! confirmation adopts what extends its chain. Another branch is only
//! shown - the choice that could adopt it is dark.

use std::collections::{BTreeMap, BTreeSet};

use molt_core::branch::SeatEvidence;
use molt_core::{MemberId, MoltError, Reply};

use super::*;

/// One part's payload: the recovery anchor's piece size.
pub(crate) const PART_BYTES: usize = super::membership::RECOVER_PART_MAX;
/// The most one offer carries; a larger run goes by file.
pub(crate) const OFFER_MAX: usize = 4 * 1024 * 1024;
/// Re-asks for missing parts per offer.
const WANT_ROUNDS: u32 = 3;
/// Between two re-asks (presence clock).
const WANT_EVERY_SECS: u64 = 30;
/// Between two parts on the wire.
const PART_PACE_MS: u64 = 200;

/// Refusals a human reads.
pub(crate) const TOO_LARGE: &str = "too large: use the file";
pub(crate) const NOTHING_TO_ADOPT: &str = "nothing to adopt";
pub(crate) const OTHER_BRANCH: &str = "another branch: the branch choice decides";

/// What this seat sends one peer.
#[derive(Debug, Clone, Default)]
pub(crate) struct Outgoing {
    pub(crate) offer: String,
    pub(crate) parts: Vec<String>,
    pub(crate) sent: u32,
    pub(crate) state: &'static str,
    pub(crate) reason: String,
}

/// What one peer sent this seat.
#[derive(Debug, Clone, Default)]
pub(crate) struct Incoming {
    pub(crate) offer: String,
    pub(crate) total: u32,
    pub(crate) parts: BTreeMap<u32, String>,
    pub(crate) state: &'static str,
    /// The blocks that extend this chain, held until adopted.
    pub(crate) blocks: Vec<ChainBlock>,
    pub(crate) from_height: u64,
    pub(crate) kind: &'static str,
    pub(crate) drops: u32,
    pub(crate) reason: String,
    pub(crate) asked: u32,
    /// The last part or re-ask (presence clock).
    pub(crate) last_at: u64,
}

/// The exchange's runtime (memory only; another branch is kept in
/// `branch.state` as evidence).
#[derive(Debug, Default)]
pub(crate) struct EvidenceRuntime {
    pub(crate) out: BTreeMap<MemberId, Outgoing>,
    pub(crate) inbound: BTreeMap<MemberId, Incoming>,
    /// Auto-offer key per peer: (own head, the peer's reported head).
    pub(crate) offered: BTreeMap<MemberId, (String, u64)>,
    /// Offers adopted or dismissed: a relay replay does not bring them back.
    pub(crate) finished: BTreeSet<String>,
    /// Tests: no relay channel even where a transport exists.
    #[cfg(test)]
    pub(crate) no_net: bool,
}

/// Cut `payload` into parts of at most [`PART_BYTES`], on char boundaries.
pub(crate) fn split_parts(payload: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    for ch in payload.chars() {
        if cur.len() + ch.len_utf8() > PART_BYTES {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() || parts.is_empty() {
        parts.push(cur);
    }
    parts
}

fn err(why: impl Into<String>) -> MoltError {
    MoltError::Branch(why.into())
}

impl State {
    /// The roster seat whose working anchor is `pk` (never this seat).
    fn seat_of_anchor(&self, pk: &str) -> Option<MemberId> {
        let me = self.member();
        self.roster().into_iter().find(|m| *m != me && !pk.is_empty() && self.working_nostr_pk(m) == pk)
    }

    /// The run `member` evidently lacks: from the fork point when it is on
    /// another branch, else from its reported head + 1.
    pub(crate) fn evidence_for(&self, member: &str) -> Result<(String, Vec<String>, u64), String> {
        let head = self.chain.head.as_ref().ok_or("no chain yet")?;
        let anchor = self.chain.blocks.first().map_or(0, |b| b.height);
        let from = match self.chain.diverged.get(member) {
            Some(d) => d.since_height,
            None => self.chain.peer_heights.get(member).map_or(anchor, |h| h.saturating_add(1)),
        }
        .max(anchor);
        if from > head.height {
            return Err("nothing to offer".to_string());
        }
        let run: Vec<&ChainBlock> = self.chain.blocks.iter().filter(|b| b.height >= from).collect();
        let payload = serde_json::to_string(&run).map_err(|e| e.to_string())?;
        if payload.len() > OFFER_MAX {
            return Err(TOO_LARGE.to_string());
        }
        let offer = molt_storage::content_hash(payload.as_bytes());
        Ok((offer, split_parts(&payload), from))
    }

    /// `evidence_offer`, and the automatic offer: build, remember, send.
    pub(crate) fn cmd_evidence_offer(&mut self, member: &str) -> Result<Reply, MoltError> {
        if !self.roster().iter().any(|m| m == member) || member == self.member() {
            return Err(err(format!("not a seat: {member}")));
        }
        let (offer, parts, from) = self.evidence_for(member).map_err(err)?;
        let to = self.working_nostr_pk(member);
        let total = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        tracing::info!(peer = %member, from, parts = total, "evidence=offered");
        self.chain.evidence.out.insert(
            member.to_string(),
            Outgoing { offer: offer.clone(), parts: parts.clone(), sent: 0, state: "offered", reason: String::new() },
        );
        let all: Vec<u32> = (0..total).collect();
        self.send_evidence_parts(member, &to, &offer, &parts, &all);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Publish `which` parts off the actor, paced; progress comes back as
    /// `NetEvidenceSent`. Without a relay channel the offer is refused.
    fn send_evidence_parts(&mut self, member: &str, to: &str, offer: &str, parts: &[String], which: &[u32]) {
        let net = self.evidence_net();
        let Some((net, cmd_tx)) = net else {
            if let Some(o) = self.chain.evidence.out.get_mut(member) {
                o.state = "refused";
                o.reason = "no relay channel".to_string();
            }
            return;
        };
        let (member, to, offer, rid) = (member.to_string(), to.to_string(), offer.to_string(), self.republic_id());
        let total = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        let parts: Vec<(u32, String)> =
            which.iter().filter_map(|i| parts.get(usize::try_from(*i).ok()?).map(|p| (*i, p.clone()))).collect();
        tokio::spawn(async move {
            let mut sent = 0u32;
            let mut error = String::new();
            for (index, data) in parts {
                let msg = molt_net::invite::RitualMsg::EvidencePart {
                    republic_id: rid.clone(),
                    offer: offer.clone(),
                    index,
                    total,
                    data,
                };
                if let Err(e) = net.send_ritual(&to, &msg).await {
                    error = e.to_string();
                    break;
                }
                sent += 1;
                tokio::time::sleep(std::time::Duration::from_millis(PART_PACE_MS)).await;
            }
            let cmd = molt_core::Command::NetEvidenceSent { member, offer, sent, total, error };
            crate::send_internal(&cmd_tx, cmd).await;
        });
    }

    /// The seat's own wrapped channel: its anchor key over the consented
    /// group relays.
    fn evidence_net(&self) -> Option<(molt_net::ritual_net::RitualNet, tokio::sync::mpsc::Sender<crate::Envelope>)> {
        #[cfg(test)]
        if self.chain.evidence.no_net {
            return None;
        }
        let nostr = self.nostr.as_ref()?;
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return None;
        }
        let dialer = self.dialer_for().ok()?;
        let net = molt_net::ritual_net::RitualNet::new(dialer, relays, &nostr.sk).ok()?;
        Some((net, self.cmd_tx.upgrade()?))
    }

    pub(crate) fn cmd_net_evidence_sent(&mut self, member: &str, offer: &str, sent: u32, total: u32, error: &str) -> Result<Reply, MoltError> {
        if let Some(o) = self.chain.evidence.out.get_mut(member).filter(|o| o.offer == offer) {
            o.sent = o.sent.max(sent).min(total);
            if error.is_empty() {
                o.state = "sent";
                tracing::info!(peer = %member, parts = sent, "evidence=sent");
            } else {
                o.state = "refused";
                o.reason = error.to_string();
                tracing::warn!(peer = %member, sent, total, %error, "evidence=send_failed");
            }
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A peer asks for parts again.
    pub(crate) fn cmd_net_evidence_want(&mut self, sender: &str, rid: &str, offer: &str, missing: &[u32]) -> Result<Reply, MoltError> {
        if rid != self.republic_id() {
            return Ok(Reply::Ack);
        }
        let Some(member) = self.seat_of_anchor(sender) else {
            return Ok(Reply::Ack);
        };
        let Some(out) = self.chain.evidence.out.get(&member).filter(|o| o.offer == offer).cloned() else {
            return Ok(Reply::Ack);
        };
        let to = self.working_nostr_pk(&member);
        let missing: Vec<u32> = missing.iter().copied().filter(|i| usize::try_from(*i).is_ok_and(|i| i < out.parts.len())).collect();
        tracing::info!(peer = %member, missing = missing.len(), "evidence=resend");
        self.send_evidence_parts(&member, &to, offer, &out.parts, &missing);
        Ok(Reply::Ack)
    }

    /// One part from the seat inbox: only a roster seat's working anchor,
    /// one offer per sender, bounded; complete -> verified and held.
    pub(crate) fn cmd_net_evidence_part(
        &mut self,
        sender: &str,
        rid: &str,
        offer: &str,
        (index, total): (u32, u32),
        data: String,
    ) -> Result<Reply, MoltError> {
        if rid != self.republic_id() || !self.is_chain_governed() {
            return Ok(Reply::Ack);
        }
        let Some(member) = self.seat_of_anchor(sender) else {
            tracing::warn!(reason = "not_a_seat", "evidence=refused");
            return Ok(Reply::Ack);
        };
        if self.chain.evidence.finished.contains(offer) {
            return Ok(Reply::Ack);
        }
        let max_parts = u32::try_from(OFFER_MAX.div_ceil(PART_BYTES)).unwrap_or(u32::MAX);
        let now = self.presence_now();
        let entry = self.chain.evidence.inbound.entry(member.clone()).or_default();
        if entry.offer != offer {
            *entry = Incoming { offer: offer.to_string(), total, state: "receiving", ..Incoming::default() };
        }
        if total == 0 || total > max_parts || index >= total || data.len() > PART_BYTES || entry.total != total {
            entry.state = "refused";
            entry.reason = TOO_LARGE.to_string();
            entry.parts.clear();
            tracing::warn!(peer = %member, total, reason = "bounds", "evidence=refused");
            self.emit_session(crate::SessionScope::Full);
            return Ok(Reply::Ack);
        }
        if entry.state != "receiving" {
            return Ok(Reply::Ack);
        }
        entry.parts.insert(index, data);
        entry.last_at = now;
        if entry.parts.len() == usize::try_from(total).unwrap_or(usize::MAX) {
            let payload: String = entry.parts.values().map(String::as_str).collect();
            self.take_evidence(&member, offer, &payload);
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A complete payload: verify it as strictly as a file import, then
    /// hold it - a run that extends this chain, or another branch.
    fn take_evidence(&mut self, member: &str, offer: &str, payload: &str) {
        let outcome = self.judge_evidence(offer, payload);
        let entry = self.chain.evidence.inbound.entry(member.to_string()).or_default();
        entry.parts.clear();
        match outcome {
            Ok(None) => {
                *entry = Incoming::default();
                self.chain.evidence.finished.insert(offer.to_string());
            }
            Ok(Some((kind, blocks, from_height, drops))) => {
                entry.state = "ready";
                entry.kind = kind;
                entry.from_height = from_height;
                entry.drops = drops;
                tracing::info!(peer = %member, kind, blocks = blocks.len(), from = from_height, "evidence=ready");
                entry.blocks = blocks;
            }
            Err(why) => {
                tracing::warn!(peer = %member, reason = %why, "evidence=refused");
                entry.state = "refused";
                entry.reason = why;
            }
        }
    }

    /// `(kind, blocks to adopt, first new height, applied blocks a fork
    /// would drop)`; `None` = nothing new.
    #[allow(clippy::type_complexity)] // one verdict, named in the doc
    fn judge_evidence(&mut self, offer: &str, payload: &str) -> Result<Option<(&'static str, Vec<ChainBlock>, u64, u32)>, String> {
        if molt_storage::content_hash(payload.as_bytes()) != offer {
            return Err("damaged".to_string());
        }
        let run: Vec<ChainBlock> = serde_json::from_str(payload).map_err(|_| "not blocks".to_string())?;
        let rest = self.unheld(&run).to_vec();
        let Some(first) = rest.first() else {
            return Ok(None);
        };
        let base = first.height.checked_sub(1).ok_or("does not link to this chain")?;
        if self.own_hash(base).as_deref() != Some(first.prev.as_str()) {
            return Err("does not link to this chain".to_string());
        }
        self.verify_run(&rest).map_err(|e| refusal_rule(&e).to_string())?;
        let from = first.height;
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        if base == head {
            return Ok(Some(("extends", rest, from, 0)));
        }
        let drops = self
            .chain
            .blocks
            .iter()
            .filter(|b| b.height >= from && matches!(b.change, ChainChange::Applied { .. }))
            .count();
        self.keep_evidence(&rest)?;
        self.persist_branch();
        Ok(Some(("other-branch", rest, from, u32::try_from(drops).unwrap_or(u32::MAX))))
    }

    /// `evidence_adopt`: what extends this chain takes the catch-up path.
    pub(crate) fn cmd_evidence_adopt(&mut self, member: &str) -> Result<Reply, MoltError> {
        let Some(entry) = self.chain.evidence.inbound.get(member).cloned() else {
            return Err(err(NOTHING_TO_ADOPT));
        };
        if entry.state != "ready" {
            return Err(err(NOTHING_TO_ADOPT));
        }
        if entry.kind != "extends" {
            return Err(err(OTHER_BRANCH));
        }
        let before = self.chain.head.as_ref().map_or(0, |h| h.height);
        for b in entry.blocks {
            self.receive_block_at(member, None, b);
        }
        let after = self.chain.head.as_ref().map_or(0, |h| h.height);
        tracing::info!(peer = %member, from = before, to = after, "evidence=adopted");
        self.chain.evidence.finished.insert(entry.offer.clone());
        if let Some(e) = self.chain.evidence.inbound.get_mut(member) {
            e.state = "adopted";
            e.blocks.clear();
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// `evidence_dismiss`.
    pub(crate) fn cmd_evidence_dismiss(&mut self, member: &str) -> Result<Reply, MoltError> {
        let Some(entry) = self.chain.evidence.inbound.remove(member) else {
            return Err(err(NOTHING_TO_ADOPT));
        };
        self.chain.evidence.finished.insert(entry.offer);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// On the heal tick: re-ask missing parts (bounded), and offer to a
    /// peer the normal path is not fixing - diverged from this seat, or
    /// one whose catch-up this seat served without progress. Once per
    /// (own head, peer's reported head).
    pub(crate) fn evidence_tick(&mut self, now: u64) {
        // a gap a quiet half minute long is asked for again, a bounded
        // number of times
        let wants: Vec<(MemberId, String, Vec<u32>)> = self
            .chain
            .evidence
            .inbound
            .iter_mut()
            .filter(|(_, e)| e.state == "receiving" && now.saturating_sub(e.last_at) >= WANT_EVERY_SECS)
            .filter_map(|(m, e)| {
                if e.asked >= WANT_ROUNDS {
                    e.state = "refused";
                    e.reason = "incomplete".to_string();
                    e.parts.clear();
                    return None;
                }
                e.asked += 1;
                e.last_at = now;
                let missing: Vec<u32> = (0..e.total).filter(|i| !e.parts.contains_key(i)).collect();
                Some((m.clone(), e.offer.clone(), missing))
            })
            .collect();
        for (member, offer, missing) in wants {
            self.send_evidence_want(&member, &offer, missing);
        }
        if self.nostr.is_none() || self.branch_abandoned() {
            return;
        }
        for (peer, key) in self.evidence_due() {
            self.chain.evidence.offered.insert(peer.clone(), key);
            if let Err(e) = self.cmd_evidence_offer(&peer) {
                tracing::info!(peer = %peer, reason = %e, "evidence=not_offered");
            }
        }
    }

    /// The peers an automatic offer is due to, with their offer key: on
    /// another branch than this seat, or stuck in a catch-up this seat
    /// serves without progress - never plain lag; once per key.
    pub(crate) fn evidence_due(&self) -> Vec<(MemberId, (String, u64))> {
        let head = self.chain.head.as_ref().map(|h| h.hash.clone()).unwrap_or_default();
        let stuck: BTreeSet<&str> = self
            .chain
            .heal_loops
            .iter()
            .filter(|l| !l.asker && l.rounds >= super::sync::HEAL_LOOP_ROUNDS)
            .map(|l| l.peer.as_str())
            .collect();
        let me = self.member();
        self.roster()
            .into_iter()
            .filter(|m| *m != me && (self.chain.diverged.contains_key(m) || stuck.contains(m.as_str())))
            .map(|m| {
                let key = (head.clone(), self.chain.peer_heights.get(&m).copied().unwrap_or(0));
                (m, key)
            })
            .filter(|(m, key)| self.chain.evidence.offered.get(m) != Some(key))
            .collect()
    }

    fn send_evidence_want(&mut self, member: &str, offer: &str, missing: Vec<u32>) {
        let to = self.working_nostr_pk(member);
        let Some((net, _)) = self.evidence_net() else {
            return;
        };
        let msg = molt_net::invite::RitualMsg::EvidenceWant { republic_id: self.republic_id(), offer: offer.to_string(), missing };
        tokio::spawn(async move {
            if let Err(e) = net.send_ritual(&to, &msg).await {
                tracing::info!(error = %e, "evidence=want_failed");
            }
        });
    }

    /// The seat table's column for `member`.
    pub(crate) fn seat_evidence(&self, member: &str) -> SeatEvidence {
        let mut v = SeatEvidence::default();
        if let Some(o) = self.chain.evidence.out.get(member) {
            v.out = if o.state == "offered" && o.sent > 0 { "sending" } else { o.state }.to_string();
            v.out_parts = o.sent;
            v.out_total = u32::try_from(o.parts.len()).unwrap_or(u32::MAX);
            v.reason.clone_from(&o.reason);
        }
        if let Some(e) = self.chain.evidence.inbound.get(member) {
            v.inbound = e.state.to_string();
            v.in_parts = u32::try_from(e.parts.len()).unwrap_or(u32::MAX);
            v.in_total = e.total;
            v.blocks = u32::try_from(e.blocks.len()).unwrap_or(u32::MAX);
            v.from_height = e.from_height;
            v.kind = e.kind.to_string();
            v.drops = e.drops;
            if !e.reason.is_empty() {
                v.reason.clone_from(&e.reason);
            }
        }
        v
    }
}
