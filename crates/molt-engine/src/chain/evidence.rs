// SPDX-License-Identifier: GPL-3.0-or-later

//! **The automatic evidence exchange** (`docs/chain/chain_rescue_plan.md`
//! §5a): a seat sends the blocks a peer lacks over the seats' own wrapped
//! channel (the ritual gift wrap to the peer's working anchor, outside the
//! group), the peer verifies them like a file import and holds them; one
//! confirmation adopts what extends its chain. Another branch is only
//! shown - the choice that could adopt it is dark.

use std::collections::{BTreeMap, VecDeque};

use molt_core::branch::SeatEvidence;
use molt_core::{MemberId, MoltError, Reply};

use super::*;

/// One part's ENCODED size: the recovery anchor's piece size.
pub(crate) const PART_BYTES: usize = super::membership::RECOVER_PART_MAX;
/// The most one offer carries; a larger run goes by file.
pub(crate) const OFFER_MAX: usize = 4 * 1024 * 1024;
/// Re-asks for missing parts per offer, on either side.
const WANT_ROUNDS: u32 = 3;
/// A gap this long without a part is asked for again (presence clock).
const WANT_EVERY_SECS: u64 = 30;
/// Between two parts on the wire.
const PART_PACE_MS: u64 = 200;
/// What one offer may put on the wire, resends included, per payload byte.
const BYTES_FACTOR: usize = 3;
/// Finished offers remembered against relay replays.
const FINISHED_MAX: usize = 256;
/// The most content evaluations (hash + JSON parse) of one sender's
/// offers per chain head; a head that moved refills the count.
const JUDGE_MAX_PER_HEAD: u32 = 3;
/// Between two automatic-offer attempts at one peer.
const AUTO_RETRY_SECS: u64 = 30;

/// Refusals a human reads.
pub(crate) const TOO_LARGE: &str = "too large: use the file";
pub(crate) const NOTHING_TO_ADOPT: &str = "nothing to adopt";
pub(crate) const OTHER_BRANCH: &str = "another branch: the branch choice decides";
pub(crate) const STALE: &str = "stale: the chain moved";
const NO_LINK: &str = "does not link to this chain";
const ONE_PER_HEAD: &str = "one bundle per head";
const INCOMPLETE: &str = "incomplete";

/// A refusal that may pass once the chain or the store changes.
fn transient(reason: &str) -> bool {
    matches!(reason, NO_LINK | ONE_PER_HEAD | "evidence store full")
}

/// What this seat sends one peer.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Outgoing {
    pub(crate) offer: String,
    pub(crate) parts: Vec<String>,
    pub(crate) sent: u32,
    pub(crate) state: &'static str,
    pub(crate) reason: String,
    /// Bytes handed to the wire for this offer, resends included.
    pub(crate) bytes: usize,
    /// Wants answered for this offer.
    pub(crate) wants: u32,
}

/// What one peer sent this seat.
#[derive(Debug, Clone, Default)]
pub(crate) struct Incoming {
    pub(crate) offer: String,
    pub(crate) total: u32,
    pub(crate) parts: BTreeMap<u32, String>,
    pub(crate) state: &'static str,
    /// The complete payload, kept to judge it again.
    pub(crate) payload: String,
    /// The blocks to adopt (another branch: shown only).
    pub(crate) blocks: Vec<ChainBlock>,
    pub(crate) from_height: u64,
    pub(crate) kind: &'static str,
    pub(crate) drops: u32,
    pub(crate) reason: String,
    pub(crate) asked: u32,
    /// The last part or re-ask (presence clock).
    pub(crate) last_at: u64,
    /// The head this payload was last judged at.
    pub(crate) judged_head: String,
    /// The distinct parts held (`total` once the payload is complete -
    /// the parts map is cleared then).
    pub(crate) have: u32,
    /// The decoded bytes held in `parts` (`max_parts` alone assumes the
    /// worst-case escaping; this is the bound the offer cap needs).
    pub(crate) bytes: usize,
}

/// What was spent judging one sender's bundles at one chain head.
#[derive(Debug, Clone)]
pub(crate) struct JudgedHead {
    pub(crate) head: String,
    pub(crate) offer: String,
    pub(crate) count: u32,
}

/// The exchange's runtime (memory only; another branch is kept in
/// `branch.state` as evidence).
#[derive(Debug, Default)]
pub(crate) struct EvidenceRuntime {
    pub(crate) out: BTreeMap<MemberId, Outgoing>,
    pub(crate) inbound: BTreeMap<MemberId, Incoming>,
    /// Per peer, this seat's head height at its last automatic offer: no
    /// new one until the peer's own head passed it.
    pub(crate) auto_top: BTreeMap<MemberId, u64>,
    /// Per peer, the last tick an automatic offer was ATTEMPTED: the gap
    /// keeps a peer without a relay channel from a build every tick.
    pub(crate) auto_tried: BTreeMap<MemberId, u64>,
    /// Per peer, the head and the peer's claimed height at which an
    /// automatic offer failed to BUILD (`nothing to offer`, `too large`):
    /// no rebuild until one of them moves.
    pub(crate) auto_failed: BTreeMap<MemberId, (String, Option<u64>, String)>,
    /// Per sender, what was judged at the current head: one evaluation
    /// per offer and head, at most [`JUDGE_MAX_PER_HEAD`] of any offers.
    pub(crate) judged: BTreeMap<MemberId, JudgedHead>,
    /// Offers adopted or dismissed, oldest first: a relay replay does not
    /// bring them back.
    pub(crate) finished: VecDeque<String>,
    /// One send task per peer; a new send replaces it.
    pub(crate) tasks: BTreeMap<MemberId, tokio::task::JoinHandle<()>>,
    /// Tests: no relay channel even where a transport exists.
    #[cfg(test)]
    pub(crate) no_net: bool,
}

impl EvidenceRuntime {
    fn is_finished(&self, offer: &str) -> bool {
        self.finished.iter().any(|f| f == offer)
    }

    fn finish(&mut self, offer: &str) {
        if !self.is_finished(offer) {
            self.finished.push_back(offer.to_string());
            while self.finished.len() > FINISHED_MAX {
                self.finished.pop_front();
            }
        }
    }
}

/// What one char takes inside a JSON string.
fn encoded_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        '\u{0}'..='\u{1f}' => 6,
        _ => ch.len_utf8(),
    }
}

/// Cut `payload` into parts whose JSON-encoded size is at most
/// [`PART_BYTES`], on char boundaries.
pub(crate) fn split_parts(payload: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut cur, mut size) = (String::new(), 0);
    for ch in payload.chars() {
        let n = encoded_len(ch);
        if size + n > PART_BYTES {
            parts.push(std::mem::take(&mut cur));
            size = 0;
        }
        cur.push(ch);
        size += n;
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
        self.offer_evidence(member)?;
        Ok(Reply::Ack)
    }

    /// The body of `evidence_offer`; `true` = the send task started (the
    /// offer is on its way), `false` = no relay channel (the out entry
    /// says so).
    pub(crate) fn offer_evidence(&mut self, member: &str) -> Result<bool, MoltError> {
        if !self.roster().iter().any(|m| m == member) || member == self.member() {
            return Err(err(format!("not a seat: {member}")));
        }
        let (offer, parts, from) = match self.evidence_for(member) {
            Ok(v) => v,
            Err(why) => {
                self.set_out_refused(member, &why);
                return Err(err(why));
            }
        };
        let to = self.working_nostr_pk(member);
        let total = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        tracing::info!(peer = %member, from, parts = total, "evidence=offered");
        let bytes = parts.iter().map(String::len).sum();
        self.chain.evidence.out.insert(
            member.to_string(),
            Outgoing { offer: offer.clone(), parts: parts.clone(), state: "offered", bytes, ..Outgoing::default() },
        );
        let all: Vec<u32> = (0..total).collect();
        let started = self.send_evidence_parts(member, &to, &offer, &parts, &all);
        self.emit_session(crate::SessionScope::Full);
        Ok(started)
    }

    /// Record on the out entry that nothing went out, and say so once.
    fn set_out_refused(&mut self, member: &str, why: &str) {
        let fresh = Outgoing { state: "refused", reason: why.to_string(), ..Outgoing::default() };
        if self.chain.evidence.out.get(member) != Some(&fresh) {
            self.chain.evidence.out.insert(member.to_string(), fresh);
            self.emit_session(crate::SessionScope::Full);
        }
    }

    /// Publish `which` parts off the actor, paced, in this peer's ONE send
    /// task (a running one is replaced); progress comes back as
    /// `NetEvidenceSent`. `true` = the task started. Without a relay
    /// channel an UNSENT offer is refused - an answered want must not
    /// reset one already `sent`.
    fn send_evidence_parts(&mut self, member: &str, to: &str, offer: &str, parts: &[String], which: &[u32]) -> bool {
        let net = self.evidence_net();
        let Some((net, cmd_tx)) = net else {
            if let Some(o) = self.chain.evidence.out.get_mut(member) {
                if o.state == "offered" {
                    o.state = "refused";
                    o.reason = "no relay channel".to_string();
                }
            }
            return false;
        };
        if let Some(task) = self.chain.evidence.tasks.remove(member) {
            task.abort();
        }
        let (peer, to, offer, rid) = (member.to_string(), to.to_string(), offer.to_string(), self.republic_id());
        let total = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        let parts: Vec<(u32, String)> =
            which.iter().filter_map(|i| parts.get(usize::try_from(*i).ok()?).map(|p| (*i, p.clone()))).collect();
        let task = tokio::spawn(async move {
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
            let cmd = molt_core::Command::NetEvidenceSent { member: peer, offer, sent, total, error };
            crate::send_internal(&cmd_tx, cmd).await;
        });
        self.chain.evidence.tasks.insert(member.to_string(), task);
        true
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
        self.chain.evidence.tasks.remove(member);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A peer asks for parts again: deduplicated, within the offer, a few
    /// times per offer and within a byte budget.
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
        let wanted: std::collections::BTreeSet<u32> =
            missing.iter().copied().filter(|i| usize::try_from(*i).is_ok_and(|i| i < out.parts.len())).collect();
        if wanted.is_empty() {
            return Ok(Reply::Ack);
        }
        let add: usize = wanted.iter().filter_map(|i| out.parts.get(usize::try_from(*i).ok()?)).map(String::len).sum();
        let payload: usize = out.parts.iter().map(String::len).sum();
        if out.wants >= WANT_ROUNDS || out.bytes + add > BYTES_FACTOR * payload {
            tracing::info!(peer = %member, wants = out.wants, "evidence=want_refused");
            return Ok(Reply::Ack);
        }
        if let Some(o) = self.chain.evidence.out.get_mut(&member) {
            o.wants += 1;
            o.bytes += add;
        }
        let to = self.working_nostr_pk(&member);
        let which: Vec<u32> = wanted.into_iter().collect();
        tracing::info!(peer = %member, missing = which.len(), "evidence=resend");
        self.send_evidence_parts(&member, &to, offer, &out.parts, &which);
        Ok(Reply::Ack)
    }

    /// One part from the seat inbox: only a roster seat's working anchor,
    /// one offer per sender, bounded; complete -> verified and held. A new
    /// pass (part 0) of an offer that was refused or incomplete reopens it.
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
        if self.chain.evidence.is_finished(offer) {
            return Ok(Reply::Ack);
        }
        let now = self.presence_now();
        let max_parts = u32::try_from(OFFER_MAX.div_ceil(PART_BYTES / 6)).unwrap_or(u32::MAX);
        let entry = self.chain.evidence.inbound.entry(member.clone()).or_default();
        if entry.offer != offer {
            *entry = Incoming { offer: offer.to_string(), total, state: "receiving", last_at: now, ..Incoming::default() };
            self.emit_session(crate::SessionScope::Full);
        } else if entry.state == "refused" && index == 0 {
            // a new pass: what was missing may come now, what did not link
            // may link now - the re-judge counts against the per-head
            // budget, and the frame passes the same bounds as a first part
            entry.state = "receiving";
            entry.asked = 0;
            entry.last_at = now;
            if entry.reason != INCOMPLETE {
                entry.parts.clear();
                entry.bytes = 0;
                entry.have = 0;
            }
            entry.reason.clear();
            self.emit_session(crate::SessionScope::Full);
        }
        let entry = self.chain.evidence.inbound.entry(member.clone()).or_default();
        if total == 0 || total > max_parts || index >= total || data.len() > PART_BYTES || entry.total != total {
            entry.state = "refused";
            entry.reason = TOO_LARGE.to_string();
            entry.parts.clear();
            entry.bytes = 0;
            entry.have = 0;
            tracing::warn!(peer = %member, total, reason = "bounds", "evidence=refused");
            self.emit_session(crate::SessionScope::Full);
            return Ok(Reply::Ack);
        }
        if entry.state != "receiving" {
            return Ok(Reply::Ack);
        }
        let Some(add) = data.len().checked_sub(entry.parts.get(&index).map_or(0, String::len)) else {
            // a shorter replacement part: dropped, the stored bytes stand
            tracing::warn!(peer = %member, index, reason = "bounds", "evidence=refused");
            return Ok(Reply::Ack);
        };
        if entry.bytes.saturating_add(add) > OFFER_MAX {
            entry.state = "refused";
            entry.reason = TOO_LARGE.to_string();
            entry.parts.clear();
            entry.bytes = 0;
            entry.have = 0;
            tracing::warn!(peer = %member, total, reason = "bounds", "evidence=refused");
            self.emit_session(crate::SessionScope::Full);
            return Ok(Reply::Ack);
        }
        if !entry.payload.is_empty() {
            // a well-formed new pass of a bundle already held: judge the
            // kept payload, nothing is stored beside it
            let payload = std::mem::take(&mut entry.payload);
            self.take_evidence(&member, offer, payload, true);
            self.emit_session(crate::SessionScope::Full);
            return Ok(Reply::Ack);
        }
        entry.parts.insert(index, data);
        entry.bytes += add;
        entry.have = u32::try_from(entry.parts.len()).unwrap_or(u32::MAX);
        entry.last_at = now;
        if entry.have == total {
            let payload: String = entry.parts.values().map(String::as_str).collect();
            entry.parts.clear();
            entry.bytes = 0;
            entry.have = total;
            self.take_evidence(&member, offer, payload, false);
            self.emit_session(crate::SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// A complete payload: verified as strictly as a file import and
    /// held (a run that extends this chain, or another branch); `again`
    /// = a re-judge of a payload already counted.
    ///
    /// The budget is per sender and head: one evaluation per offer and
    /// head, at most [`JUDGE_MAX_PER_HEAD`] evaluations of ANY offers per
    /// head - network input cannot run hash + parse over 4 MiB
    /// unboundedly, and a corrected offer still gets its evaluation; a
    /// head that moved refills the count.
    fn take_evidence(&mut self, member: &str, offer: &str, payload: String, again: bool) {
        let head = self.chain.head.as_ref().map(|h| h.hash.clone()).unwrap_or_default();
        let spent = match self.chain.evidence.judged.get(member) {
            Some(j) if j.head == head => j.count >= JUDGE_MAX_PER_HEAD || (j.offer == offer && !again),
            _ => false,
        };
        let outcome = if spent { Err(ONE_PER_HEAD.to_string()) } else { self.judge_evidence(member, offer, &payload) };
        if !spent {
            let count = self
                .chain
                .evidence
                .judged
                .get(member)
                .filter(|j| j.head == head)
                .map_or(1, |j| j.count.saturating_add(1));
            self.chain.evidence.judged.insert(member.to_string(), JudgedHead { head: head.clone(), offer: offer.to_string(), count });
        }
        let entry = self.chain.evidence.inbound.entry(member.to_string()).or_default();
        entry.judged_head = head;
        match outcome {
            Ok(None) => {
                *entry = Incoming::default();
                self.chain.evidence.finish(offer);
            }
            Ok(Some((kind, blocks, from_height, drops))) => {
                entry.state = "ready";
                entry.reason.clear();
                entry.kind = kind;
                entry.from_height = from_height;
                entry.drops = drops;
                entry.payload = payload;
                entry.have = entry.total;
                tracing::info!(peer = %member, kind, blocks = blocks.len(), from = from_height, "evidence=ready");
                entry.blocks = blocks;
            }
            Err(why) => {
                tracing::warn!(peer = %member, reason = %why, "evidence=refused");
                entry.state = "refused";
                // kept to judge again once the chain or the store moved
                if transient(&why) {
                    entry.payload = payload;
                }
                entry.reason = why;
            }
        }
    }

    /// The new part of a run of another seat, linked onto this chain: the
    /// leading blocks whose DECISION this chain holds at their height are
    /// held - a signature variant (on a v1 chain its identity differs) is
    /// the same block, never a fork. What follows such a variant is
    /// re-linked onto this chain's own blocks (`prev` is not signed).
    /// `None` = nothing new; `Err` = it does not link.
    pub(crate) fn new_part(&self, run: &[ChainBlock]) -> Result<Option<Vec<ChainBlock>>, String> {
        let first = self.chain.blocks.first().map_or(0, |b| b.height);
        let own = |h: u64| usize::try_from(h.checked_sub(first)?).ok().and_then(|i| self.chain.blocks.get(i));
        let held = run.iter().take_while(|b| own(b.height).is_some_and(|o| o.change == b.change)).count();
        let mut rest = run[held..].to_vec();
        let Some(head) = rest.first().map(|b| b.height) else {
            return Ok(None);
        };
        let base = head.checked_sub(1).ok_or(NO_LINK)?;
        let link = self.own_hash(base).ok_or(NO_LINK)?;
        if rest[0].prev != link {
            if held == 0 {
                return Err(NO_LINK.to_string());
            }
            rest[0].prev = link;
            for i in 1..rest.len() {
                rest[i].prev = self.block_id_of(&rest[i - 1]);
            }
        }
        Ok(Some(rest))
    }

    /// `(kind, blocks, first new height, applied blocks a fork would
    /// drop)`; `None` = nothing new.
    #[allow(clippy::type_complexity)] // one verdict, named in the doc
    fn judge_evidence(&mut self, member: &str, offer: &str, payload: &str) -> Result<Option<(&'static str, Vec<ChainBlock>, u64, u32)>, String> {
        if molt_storage::content_hash(payload.as_bytes()) != offer {
            return Err("damaged".to_string());
        }
        let run: Vec<ChainBlock> = serde_json::from_str(payload).map_err(|_| "not blocks".to_string())?;
        let Some(rest) = self.new_part(&run)? else {
            return Ok(None);
        };
        self.verify_run(&rest).map_err(|e| refusal_rule(&e).to_string())?;
        let from = rest[0].height;
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        if from == head + 1 {
            return Ok(Some(("extends", rest, from, 0)));
        }
        let drops = self
            .chain
            .blocks
            .iter()
            .filter(|b| b.height >= from && matches!(b.change, ChainChange::Applied { .. }))
            .count();
        if self.keep_verified_evidence(&rest, &format!("seat:{member}"))? {
            self.persist_branch();
        }
        Ok(Some(("other-branch", rest, from, u32::try_from(drops).unwrap_or(u32::MAX))))
    }

    /// `evidence_adopt`: what extends this chain takes the catch-up path,
    /// re-checked against the head NOW; the ingest gates hold here too.
    pub(crate) fn cmd_evidence_adopt(&mut self, member: &str) -> Result<Reply, MoltError> {
        if self.delivery.evicted {
            return Err(MoltError::SeatEvicted);
        }
        if self.branch_abandoned() {
            return Err(err(super::branch::ABANDONED));
        }
        if self.chain.head.is_none() {
            return Err(err("no chain yet"));
        }
        let Some(entry) = self.chain.evidence.inbound.get(member).cloned() else {
            return Err(err(NOTHING_TO_ADOPT));
        };
        if entry.state != "ready" {
            return Err(err(NOTHING_TO_ADOPT));
        }
        if entry.kind != "extends" {
            return Err(err(OTHER_BRANCH));
        }
        let head = self.chain.head.as_ref().map_or(0, |h| h.height);
        let fresh = self.new_part(&entry.blocks);
        let blocks = match fresh {
            Ok(Some(rest)) if rest[0].height == head + 1 => rest,
            Ok(None) => {
                self.chain.evidence.inbound.remove(member);
                self.chain.evidence.finish(&entry.offer);
                self.emit_session(crate::SessionScope::Full);
                return Err(err(NOTHING_TO_ADOPT));
            }
            _ => {
                // the head moved: what this bundle is NOW decides
                self.take_evidence(member, &entry.offer, entry.payload.clone(), true);
                self.emit_session(crate::SessionScope::Full);
                return Err(err(STALE));
            }
        };
        for b in blocks {
            self.receive_block_at(member, None, b);
        }
        let after = self.chain.head.as_ref().map_or(0, |h| h.height);
        tracing::info!(peer = %member, from = head, to = after, "evidence=adopted");
        self.chain.evidence.finish(&entry.offer);
        if let Some(e) = self.chain.evidence.inbound.get_mut(member) {
            e.state = "adopted";
            e.blocks.clear();
            e.payload.clear();
        }
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// `evidence_dismiss`.
    pub(crate) fn cmd_evidence_dismiss(&mut self, member: &str) -> Result<Reply, MoltError> {
        let Some(entry) = self.chain.evidence.inbound.remove(member) else {
            return Err(err(NOTHING_TO_ADOPT));
        };
        self.chain.evidence.finish(&entry.offer);
        self.emit_session(crate::SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// On the heal tick: re-ask a gap (bounded), judge again what was
    /// refused for a reason the moved head may have cured, and offer to a
    /// peer the normal path is not fixing.
    pub(crate) fn evidence_tick(&mut self, now: u64) {
        let wants: Vec<(MemberId, String, Vec<u32>)> = self
            .chain
            .evidence
            .inbound
            .iter_mut()
            .filter(|(_, e)| e.state == "receiving" && now.saturating_sub(e.last_at) >= WANT_EVERY_SECS)
            .filter_map(|(m, e)| {
                if e.asked >= WANT_ROUNDS {
                    // the parts stay: a new pass completes them
                    e.state = "refused";
                    e.reason = INCOMPLETE.to_string();
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
        let head = self.chain.head.as_ref().map(|h| h.hash.clone()).unwrap_or_default();
        let again: Vec<(MemberId, String, String)> = self
            .chain
            .evidence
            .inbound
            .iter()
            .filter(|(_, e)| e.state == "refused" && transient(&e.reason) && !e.payload.is_empty() && e.judged_head != head)
            .map(|(m, e)| (m.clone(), e.offer.clone(), e.payload.clone()))
            .collect();
        let rejudged = again.len();
        for (member, offer, payload) in again {
            self.take_evidence(&member, &offer, payload, true);
        }
        if rejudged > 0 {
            self.emit_session(crate::SessionScope::Full);
        }
        if self.branch_abandoned() {
            return;
        }
        for peer in self.evidence_due() {
            if self.chain.evidence.auto_tried.get(&peer).is_some_and(|at| now.saturating_sub(*at) < AUTO_RETRY_SECS) {
                continue;
            }
            let claim = self.chain.peer_heights.get(&peer).copied();
            if self.chain.evidence.auto_failed.get(&peer).is_some_and(|(h, c, _)| *h == head && *c == claim) {
                continue;
            }
            self.chain.evidence.auto_tried.insert(peer.clone(), now);
            match self.offer_evidence(&peer) {
                // the latch waits for the send: a tick without a relay
                // channel must not switch the automatic path off; a build
                // refusal holds until the head or the claim moves
                Ok(true) => {
                    self.chain.evidence.auto_failed.remove(&peer);
                    self.note_auto_offer(&peer);
                }
                Ok(false) => {
                    self.chain.evidence.auto_failed.remove(&peer);
                    tracing::info!(peer = %peer, reason = "no relay channel", "evidence=not_offered");
                }
                Err(why) => {
                    self.chain.evidence.auto_failed.insert(peer.clone(), (head.clone(), claim, why.to_string()));
                    tracing::info!(peer = %peer, reason = %why, "evidence=not_offered");
                }
            }
        }
    }

    /// Remember an automatic offer to `peer`: the next waits until the
    /// peer's own head passed this seat's head now.
    pub(crate) fn note_auto_offer(&mut self, peer: &str) {
        let top = self.chain.head.as_ref().map_or(0, |h| h.height);
        self.chain.evidence.auto_top.insert(peer.to_string(), top);
    }

    /// The peers an automatic offer is due to: stuck in a catch-up this
    /// seat serves without progress - never plain lag, never a diverged
    /// peer (a restore on its branch may have moved its key: the manual
    /// offer only), and not again until the peer's head passed the last
    /// offer's range.
    pub(crate) fn evidence_due(&self) -> Vec<MemberId> {
        let me = self.member();
        let stuck: std::collections::BTreeSet<&str> = self
            .chain
            .heal_loops
            .iter()
            .filter(|l| !l.asker && l.rounds >= super::sync::HEAL_LOOP_ROUNDS)
            .map(|l| l.peer.as_str())
            .collect();
        self.roster()
            .into_iter()
            .filter(|m| *m != me && stuck.contains(m.as_str()) && !self.chain.diverged.contains_key(m))
            .filter(|m| {
                self.chain.evidence.auto_top.get(m).map_or(true, |top| self.chain.peer_heights.get(m).is_some_and(|h| h >= top))
            })
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
        let mut v = SeatEvidence { anchor_outdated: self.chain.diverged.contains_key(member), ..SeatEvidence::default() };
        if let Some(o) = self.chain.evidence.out.get(member) {
            v.out = if o.state == "offered" && o.sent > 0 { "sending" } else { o.state }.to_string();
            v.out_parts = o.sent;
            v.out_total = u32::try_from(o.parts.len()).unwrap_or(u32::MAX);
            v.reason.clone_from(&o.reason);
        }
        if let Some(e) = self.chain.evidence.inbound.get(member) {
            v.inbound = e.state.to_string();
            v.in_parts = e.have.min(e.total);
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
