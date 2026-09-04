// SPDX-License-Identifier: GPL-3.0-or-later

//! The mirror gossip on the engine side (`docs/files/mirroring.md` §3.4):
//! this seat's declaration and hold status go out as control frames -
//! at runtime start, on change, periodically - and every member's come in
//! and persist in `transport.state`, so "who mirrors what" reads locally.

use std::collections::HashMap;

use super::*;
use molt_core::{MirrorDecl, MirrorFileView, MirrorHold, MirrorMemberView, MirrorView};
use molt_net::mirror_gossip::{MirrorDeclFrame, MirrorStatusFrame, MirrorWhoFrame};
use molt_net::supervisor::StateStore as _;

/// How often the declaration repeats unchanged.
const DECL_REPEAT_SECS: u64 = 6 * 3_600;
/// The least time between two status frames (a change sends the next one
/// after this; an unchanged status repeats every fifth of an hour).
const STATUS_MIN_SECS: u64 = 60;
const STATUS_REPEAT_SECS: u64 = 5 * 60;
/// A holder answers a `MirrorWho` at most this often.
const WHO_ANSWER_SECS: u64 = 3_600;

impl State {
    /// Adopt the persisted gossip at open.
    pub(crate) fn load_mirror(&mut self, state: &molt_core::TransportState) {
        self.files.mirror = state.mirror.clone();
    }

    /// Write the gossip copy back (off the actor; the storage merge
    /// carries `mirror` beside the cursors).
    fn persist_mirror(&self) {
        let Some(store) = self.file_store() else {
            return;
        };
        let mirror = self.files.mirror.clone();
        tokio::spawn(async move {
            store
                .update(|s| {
                    s.mirror = mirror;
                    true
                })
                .await;
        });
    }

    fn publish_mirror_frame(&self, frame: Vec<u8>) -> bool {
        match self.group_net.as_ref() {
            Some(group) => {
                group.handle.publish_control(frame);
                true
            }
            None => false,
        }
    }

    /// This seat's declaration, as the members see it.
    fn own_decl(&self) -> MirrorDecl {
        MirrorDecl {
            on: self.files.mirror.on,
            quota: self.files.mirror.quota,
            rev: self.files.mirror.rev,
        }
    }

    fn send_mirror_decl(&mut self, now: u64) {
        let decl = self.own_decl();
        let frame = MirrorDeclFrame {
            v: molt_net::mirror_gossip::MIRROR_V,
            by: self.member(),
            on: decl.on,
            quota: decl.quota,
            rev: decl.rev,
        };
        if self.publish_mirror_frame(frame.to_frame()) {
            self.files.mirror_decl_sent = now;
        }
    }

    /// What this seat holds: its own available v2 shares, whole (M4 adds
    /// the mirrored series).
    pub(crate) fn own_mirror_holds(&self) -> Vec<MirrorHold> {
        let me = self.member();
        let mut ids: Vec<MessageId> = self.files.share_paths.keys().copied().collect();
        ids.sort_unstable_by_key(|id| id.to_string());
        ids.into_iter()
            .filter_map(|id| {
                let (ident, available) = self.share_identity(&id).ok()?;
                (ident.by == me && available && !ident.key_b64.is_empty())
                    .then_some(MirrorHold { id, held: ident.pieces, of: ident.pieces })
            })
            .collect()
    }

    fn send_mirror_status(&mut self, now: u64) {
        let holds = self.own_mirror_holds();
        let frame = MirrorStatusFrame {
            v: molt_net::mirror_gossip::MIRROR_V,
            by: self.member(),
            holds: holds
                .iter()
                .map(|h| (h.id.to_string(), h.held, h.of))
                .collect(),
        };
        if self.publish_mirror_frame(frame.to_frame()) {
            self.files.mirror_status_sent = now;
            self.files.mirror_status_last = holds;
        }
    }

    /// The periodic beat (rides the presence tick): the declaration at
    /// start and every six hours, the status when it changed (at most
    /// once a minute) and unchanged every five minutes, ONE ask at start.
    pub(crate) fn mirror_gossip_tick(&mut self, now: u64) {
        if self.group_net.is_none() {
            return;
        }
        if now.saturating_sub(self.files.mirror_decl_sent) >= DECL_REPEAT_SECS {
            self.send_mirror_decl(now);
        }
        let holds = self.own_mirror_holds();
        let since = now.saturating_sub(self.files.mirror_status_sent);
        let changed = holds != self.files.mirror_status_last;
        if (changed && since >= STATUS_MIN_SECS) || (!holds.is_empty() && since >= STATUS_REPEAT_SECS) {
            self.send_mirror_status(now);
        }
        if !self.files.mirror_who_asked {
            let frame = MirrorWhoFrame { v: molt_net::mirror_gossip::MIRROR_V, by: self.member() };
            self.files.mirror_who_asked = self.publish_mirror_frame(frame.to_frame());
        }
    }

    /// An own share changed (added, removed): say so at once when the
    /// minute allows, else the tick catches it.
    pub(crate) fn note_mirror_holds_changed(&mut self, now: u64) {
        if self.group_net.is_none() {
            return;
        }
        if now.saturating_sub(self.files.mirror_status_sent) >= STATUS_MIN_SECS
            && self.own_mirror_holds() != self.files.mirror_status_last
        {
            self.send_mirror_status(now);
        }
    }

    /// The consent switch and the quota (`set_mirror`, both surfaces).
    pub(crate) fn cmd_set_mirror(&mut self, on: bool, quota_bytes: u64) -> Result<Reply, MoltError> {
        if self.active.is_none() {
            return Err(MoltError::Engine("no open workspace".into()));
        }
        let now = crate::now_secs();
        let m = &mut self.files.mirror;
        m.on = on;
        m.quota = quota_bytes;
        m.rev = now.max(m.rev.saturating_add(1));
        self.persist_mirror();
        self.send_mirror_decl(now);
        Ok(Reply::Ack)
    }

    /// A member's declaration landed: last wins by revision.
    pub(crate) fn cmd_net_mirror_decl(
        &mut self,
        from: &MemberId,
        on: bool,
        quota: u64,
        rev: u64,
    ) -> Result<Reply, MoltError> {
        if *from == self.member() || !self.roster().contains(from) {
            return Ok(Reply::Ack);
        }
        let newer = self.files.mirror.decls.get(from).map_or(true, |d| rev >= d.rev);
        if newer {
            self.files.mirror.decls.insert(from.clone(), MirrorDecl { on, quota, rev });
            self.persist_mirror();
        }
        Ok(Reply::Ack)
    }

    /// A member's hold status landed: replaces what it said before.
    pub(crate) fn cmd_net_mirror_status(
        &mut self,
        from: &MemberId,
        holds: Vec<MirrorHold>,
    ) -> Result<Reply, MoltError> {
        if *from == self.member() || !self.roster().contains(from) {
            return Ok(Reply::Ack);
        }
        self.files.mirror.status.insert(from.clone(), holds);
        self.persist_mirror();
        Ok(Reply::Ack)
    }

    /// A member asks who holds what: answer with this seat's status, at
    /// most once an hour.
    pub(crate) fn cmd_net_mirror_who(&mut self, from: &MemberId) -> Result<Reply, MoltError> {
        if *from == self.member() {
            return Ok(Reply::Ack);
        }
        let now = crate::now_secs();
        if now.saturating_sub(self.files.mirror_who_answered) >= WHO_ANSWER_SECS
            && !self.own_mirror_holds().is_empty()
        {
            self.files.mirror_who_answered = now;
            self.send_mirror_status(now);
        }
        Ok(Reply::Ack)
    }

    /// Who holds a share whole: every member whose status says `held ==
    /// of` for it, plus this seat for its own shares.
    pub(crate) fn mirror_holders(&self) -> HashMap<MessageId, Vec<MemberId>> {
        let mut out: HashMap<MessageId, Vec<MemberId>> = HashMap::new();
        for hold in self.own_mirror_holds() {
            out.entry(hold.id).or_default().push(self.member());
        }
        for (member, holds) in &self.files.mirror.status {
            for hold in holds {
                if hold.held == hold.of && hold.of > 0 {
                    let holders = out.entry(hold.id).or_default();
                    if !holders.contains(member) {
                        holders.push(member.clone());
                    }
                }
            }
        }
        for holders in out.values_mut() {
            holders.sort_unstable();
        }
        out
    }

    /// `read_mirror`: this seat's switch and quota, every member's
    /// declaration, and per share who holds it.
    pub(crate) fn mirror_view(&self) -> MirrorView {
        let me = self.member();
        let members = self
            .roster()
            .into_iter()
            .map(|member| {
                let decl = if member == me {
                    Some(self.own_decl())
                } else {
                    self.files.mirror.decls.get(&member).cloned()
                };
                MirrorMemberView {
                    known: decl.is_some(),
                    on: decl.as_ref().is_some_and(|d| d.on),
                    quota: decl.as_ref().map_or(0, |d| d.quota),
                    rev: decl.as_ref().map_or(0, |d| d.rev),
                    member,
                }
            })
            .collect();
        let holders = self.mirror_holders();
        let files = self
            .uploads_view()
            .into_iter()
            .map(|u| MirrorFileView {
                holders: holders.get(&u.id).cloned().unwrap_or_default(),
                held: u.mirror_held,
                of: u.mirror_of,
                id: u.id,
                name: u.name,
            })
            .collect();
        MirrorView {
            on: self.files.mirror.on,
            quota: self.files.mirror.quota,
            used: 0,
            members,
            files,
        }
    }
}
