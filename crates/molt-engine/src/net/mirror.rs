// SPDX-License-Identifier: GPL-3.0-or-later

//! The mirror gossip on the engine side (`docs_archive/files/mirroring.md` §3.4):
//! this seat's declaration and hold status go out as control frames -
//! at runtime start, on change, periodically - and every member's come in
//! and persist in `transport.state`, so "who mirrors what" reads locally.

use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::files_state::FileState;
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
/// A half-collected status generation is dropped after this.
const PAGES_STALE_SECS: u64 = 600;
/// A `FileWanted` the worker sent for a series' stamp is asked again
/// after this if no announcement came.
const PENDING_RETRY_SECS: u64 = 600;
/// A failed mirror fetch is not retried before this has passed.
const FAIL_BACKOFF_SECS: u64 = 600;
/// The worker plans this often (rides the 1 s delivery tick).
const PLAN_EVERY_SECS: u64 = 5;
/// Mirror fetches running at once: the relays, not the disk, are the
/// bottleneck, and one at a time IS the pull pacing.
const MIRROR_FETCHES_AT_ONCE: usize = 1;

impl State {
    /// Adopt the persisted gossip at open.
    pub(crate) fn load_mirror(&mut self, state: &molt_core::TransportState) {
        self.files.mirror = state.mirror.clone();
    }

    /// Write the gossip copy back (off the actor; the storage merge
    /// carries these fields beside the cursors - the jobs have their own
    /// messages and are left alone).
    fn persist_mirror(&self) {
        let Some(store) = self.file_store() else {
            return;
        };
        let mirror = self.files.mirror.clone();
        tokio::spawn(async move {
            store
                .update(|s| {
                    s.mirror.on = mirror.on;
                    s.mirror.quota = mirror.quota;
                    s.mirror.rev = mirror.rev;
                    s.mirror.decls = mirror.decls;
                    s.mirror.status = mirror.status;
                    true
                })
                .await;
        });
    }

    fn publish_mirror_frame(&self, frame: Vec<u8>) -> bool {
        self.group_net
            .as_ref()
            .is_some_and(|group| group.handle.publish_control(frame))
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

    /// What this seat holds: its own available v2 shares, whole, and the
    /// series it mirrors (verified pieces of the count).
    pub(crate) fn own_mirror_holds(&self) -> Vec<MirrorHold> {
        let me = self.member();
        let mut ids: Vec<MessageId> = self.files.share_paths.keys().copied().collect();
        ids.sort_unstable_by_key(|id| id.to_string());
        let mut out: Vec<MirrorHold> = ids
            .into_iter()
            .filter_map(|id| {
                let (ident, available) = self.share_identity(&id).ok()?;
                (ident.by == me && available && !ident.key_b64.is_empty())
                    .then_some(MirrorHold { id, held: ident.pieces, of: ident.pieces })
            })
            .collect();
        for (series, job) in &self.files.mirror.jobs {
            let Ok(id) = series.parse::<MessageId>() else {
                continue;
            };
            let held = if job.complete {
                job.count
            } else {
                self.files.mirror_progress.get(&id).copied().unwrap_or_else(|| job.held_count())
            };
            out.push(MirrorHold { id, held, of: job.count });
        }
        out
    }

    /// This seat's copy of a share: `(held, of)` - whole for an own share,
    /// the job's progress for a mirrored one, nothing otherwise.
    pub(crate) fn mirror_held_of(&self, id: &MessageId, mine: bool, pieces: u32) -> (u32, u32) {
        if mine {
            return (pieces, pieces);
        }
        match self.files.mirror.jobs.get(&id.to_string()) {
            Some(job) if job.complete => (job.count, job.count),
            Some(job) => (
                self.files.mirror_progress.get(id).copied().unwrap_or_else(|| job.held_count()),
                job.count,
            ),
            None => (0, pieces),
        }
    }

    /// Bytes the mirror folder holds (the quota's counter).
    fn mirror_used(&self) -> u64 {
        self.files.mirror.jobs.values().map(|j| j.bytes).sum()
    }

    /// The mirror folder of the open republic (`prefs.mirror_dir`, else
    /// `<workspace root>/../mirror/<republic-id>/`).
    pub(crate) fn mirror_dir(&self) -> Option<PathBuf> {
        let active = self.active.as_ref()?;
        if !active.prefs.mirror_dir.is_empty() {
            return Some(molt_storage::expand_tilde(&active.prefs.mirror_dir));
        }
        let root = molt_storage::expand_tilde(&self.session.settings.workspace_dir);
        Some(root.join("..").join("mirror").join(&active.id))
    }

    fn member_online(&self, member: &str) -> bool {
        if *member == self.member() {
            return true;
        }
        let now = self.presence_now();
        let last_seen = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .and_then(|e| e.members.iter().find(|mi| mi.name == member))
            .map(|mi| mi.last_seen)
            .unwrap_or(molt_core::MemberInfo::NEVER);
        self.presence_of(member, last_seen, now) != 2
    }

    /// Whether THIS seat answers a want for `id` (the lowest-named online
    /// holder does), and from where: the shared file, or the mirror's
    /// piece directory (`stored`).
    pub(crate) fn piece_source_if_elected(
        &self,
        id: &MessageId,
        ident: &crate::files_state::ShareIdentity,
    ) -> Option<(PathBuf, bool)> {
        let me = self.member();
        let holders = self.mirror_holders();
        let elected = holders
            .get(id)?
            .iter()
            .find(|m| self.member_online(m))?;
        if *elected != me {
            return None;
        }
        if ident.by == me {
            return self.files.share_paths.get(id).cloned().map(|p| (p, false));
        }
        let complete = self.files.mirror.jobs.get(&id.to_string()).is_some_and(|j| j.complete);
        if !complete {
            return None;
        }
        Some((self.mirror_dir()?.join(id.to_string()), true))
    }

    /// The worker's planning beat (`docs_archive/files/mirroring.md` §3.3), every
    /// five seconds: drop what an unpersist freed, resume or start the
    /// least-mirrored persistent share this seat does not hold (one fetch
    /// at a time), stop at the quota with ONE notice.
    pub(crate) fn mirror_worker_tick(&mut self, now: u64) {
        if self.group_net.is_none() || now.saturating_sub(self.files.mirror_planned_at) < PLAN_EVERY_SECS {
            return;
        }
        self.files.mirror_planned_at = now;
        self.files.mirror_fetches.retain(|_, h| !h.is_finished());
        // an unanswered ask is asked again; a failure's backoff expires
        self.files
            .mirror_pending
            .retain(|_, asked| now.saturating_sub(*asked) < PENDING_RETRY_SECS);
        self.files.mirror_failed.retain(|_, until| *until > now);
        let states = self.files_state();
        // an unpersisted share whose window ended, or one the fold no
        // longer knows: its pieces go (the sharer's own file is never
        // touched - it is not in this folder)
        let gone: Vec<String> = self
            .files
            .mirror
            .jobs
            .keys()
            .filter(|series| {
                series.parse::<MessageId>().map_or(true, |id| match states.get(&id) {
                    Some(FileState::Persistent(_)) => false,
                    Some(FileState::Unpersisted(..)) => self.share_expired_in(&states, &id),
                    None => true,
                })
            })
            .cloned()
            .collect();
        for series in gone {
            self.drop_mirror(&series);
        }
        if !self.files.mirror.on {
            return;
        }
        if self.files.mirror_fetches.len() >= MIRROR_FETCHES_AT_ONCE {
            return;
        }
        let me = self.member();
        let holders = self.mirror_holders();
        // candidates: persistent v2 shares of others this seat does not
        // hold complete and is not fetching; the least mirrored first,
        // the older share on a tie
        let mut candidates: Vec<(usize, u64, MessageId, crate::files_state::ShareIdentity)> = states
            .iter()
            .filter_map(|(id, st)| match st {
                FileState::Persistent(ident) => Some((*id, ident.clone())),
                FileState::Unpersisted(..) => None,
            })
            .filter(|(id, ident)| {
                ident.by != me
                    && !ident.key_b64.is_empty()
                    && !self.files.mirror.jobs.get(&id.to_string()).is_some_and(|j| j.complete)
                    && !self.files.mirror_fetches.contains_key(id)
                    && !self.files.mirror_pending.contains_key(id)
                    && !self.files.mirror_failed.contains_key(id)
            })
            .map(|(id, ident)| {
                let mirrors = holders.get(&id).map_or(0, Vec::len);
                (mirrors, ident.shared_ts, id, ident)
            })
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.to_string().cmp(&b.2.to_string())));
        let Some((_, _, id, ident)) = candidates.into_iter().next() else {
            return;
        };
        let used = self.mirror_used();
        let already = self.files.mirror.jobs.get(&id.to_string()).map_or(0, |j| j.bytes);
        if used.saturating_sub(already).saturating_add(ident.size) > self.files.mirror.quota {
            if !self.files.mirror_quota_noted {
                self.files.mirror_quota_noted = true;
                self.session.notice = format!("mirror-quota:{used}:{}", self.files.mirror.quota);
                self.emit_session(SessionScope::Full);
            }
            return;
        }
        match self.files.series.get(&id).copied() {
            Some(at) => self.start_mirror(id, at),
            None => {
                // the sharer's stamp is what the fetch subscribes from
                self.files.mirror_pending.insert(id, now);
                let env = self.make_env(me, WorkspaceEvent::FileWanted { id });
                self.record(env);
            }
        }
    }

    /// Run (or resume) the mirror fetch of `id` from the series start `at`.
    pub(crate) fn start_mirror(&mut self, id: MessageId, at: u64) {
        if self.files.mirror_fetches.contains_key(&id) {
            return;
        }
        let Ok((ident, _)) = self.share_identity(&id) else {
            return;
        };
        let Some(key) = crate::files_state::decode_share_key(&ident.key_b64) else {
            return;
        };
        let (Some(cmd_tx), Some(store), Some(dir)) = (self.cmd_tx.upgrade(), self.file_store(), self.mirror_dir())
        else {
            return;
        };
        let Some(channel) = self.nostr_file_channel() else {
            return;
        };
        let series = id.to_string();
        let job = self
            .files
            .mirror
            .jobs
            .get(&series)
            .cloned()
            .unwrap_or(molt_core::MirrorJob {
                count: ident.pieces,
                size: ident.size,
                root: ident.root.clone(),
                key: key.to_vec(),
                started_at: at,
                held: Vec::new(),
                complete: false,
                bytes: 0,
            });
        self.files.mirror.jobs.insert(series.clone(), job.clone());
        let handle = crate::transfer::spawn_mirror_fetch(
            channel,
            id,
            dir.join(&series),
            job,
            store,
            self.net_scope,
            cmd_tx,
        );
        self.files.mirror_fetches.insert(id, handle);
    }

    /// Forget a mirrored series: its fetch, its pieces, its job.
    fn drop_mirror(&mut self, series: &str) {
        if let Ok(id) = series.parse::<MessageId>() {
            if let Some(h) = self.files.mirror_fetches.remove(&id) {
                h.abort();
            }
            self.files.mirror_progress.remove(&id);
        }
        self.files.mirror.jobs.remove(series);
        self.files.mirror_quota_noted = false;
        if let Some(dir) = self.mirror_dir() {
            let dir = dir.join(series);
            tokio::task::spawn_blocking(move || {
                let _ = std::fs::remove_dir_all(dir);
            });
        }
        if let Some(active) = self.active.as_ref() {
            active.handle.remove_mirror_job(series);
        }
    }

    /// The running fetch verified more pieces.
    pub(crate) fn cmd_net_mirror_progress(&mut self, id: MessageId, held: u32, bytes: u64) -> Result<Reply, MoltError> {
        self.files.mirror_progress.insert(id, held);
        if let Some(job) = self.files.mirror.jobs.get_mut(&id.to_string()) {
            job.bytes = bytes;
        }
        Ok(Reply::Ack)
    }

    /// The fetch ended: complete (say so to the members) or failed (gone).
    pub(crate) fn cmd_net_mirror_done(
        &mut self,
        id: MessageId,
        ok: bool,
        reason: String,
        bytes: u64,
    ) -> Result<Reply, MoltError> {
        self.files.mirror_fetches.remove(&id);
        let series = id.to_string();
        if ok {
            if let Some(job) = self.files.mirror.jobs.get_mut(&series) {
                job.complete = true;
                job.bytes = bytes;
            }
            self.files.mirror_progress.remove(&id);
            let now = crate::now_secs();
            self.files.mirror_status_sent = 0; // a completion is always news
            self.note_mirror_holds_changed(now);
        } else {
            tracing::warn!(%id, reason = %reason, "mirror: fetch failed");
            self.files.mirror.jobs.remove(&series);
            self.files.mirror_progress.remove(&id);
            self.files
                .mirror_failed
                .insert(id, crate::now_secs().saturating_add(FAIL_BACKOFF_SECS));
        }
        Ok(Reply::Ack)
    }

    fn send_mirror_status(&mut self, now: u64) {
        let holds = self.own_mirror_holds();
        let wire: Vec<(String, u32, u32)> = holds
            .iter()
            .map(|h| (h.id.to_string(), h.held, h.of))
            .collect();
        let mut sent = true;
        for page in MirrorStatusFrame::pages(self.member(), &wire, now) {
            sent &= self.publish_mirror_frame(page.to_frame());
        }
        if sent {
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
        // a generation whose pages stopped coming is forgotten
        self.files
            .mirror_pages
            .retain(|_, (_, _, since, _)| now.saturating_sub(*since) < PAGES_STALE_SECS);
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
        let now = crate::now_secs();
        let m = &mut self.files.mirror;
        m.on = on;
        m.quota = quota_bytes;
        m.rev = now.max(m.rev.saturating_add(1));
        self.files.mirror_quota_noted = false;
        self.files.mirror_planned_at = 0;
        // consent withdrawn: the running fetch stops now (what is stored
        // stays; the switch back on resumes it at its bitmap)
        if !on {
            for (_, fetch) in self.files.mirror_fetches.drain() {
                fetch.abort();
            }
            self.files.mirror_pending.clear();
        }
        self.persist_mirror();
        self.send_mirror_decl(now);
        Ok(Reply::Ack)
    }

    /// The mirror folder (GUI/config-only): the series folders move with
    /// it - one rename each, BEFORE the worker re-plans, so the new folder
    /// is never half-built by a fetch while the move runs; a folder that
    /// already exists at the target stays, the job re-checks its pieces.
    pub(crate) fn cmd_set_mirror_dir(&mut self, path: String) -> Result<Reply, MoltError> {
        let old = self.mirror_dir();
        let Some(active) = self.active.as_mut() else {
            return Err(MoltError::Engine("no open workspace".into()));
        };
        active.prefs.mirror_dir = path.trim().to_string();
        active.handle.set_prefs(active.prefs.clone());
        let new = self.mirror_dir();
        if let (Some(old), Some(new)) = (old, new) {
            if old != new {
                for (_, fetch) in self.files.mirror_fetches.drain() {
                    fetch.abort();
                }
                if old.is_dir() {
                    let _ = std::fs::create_dir_all(&new);
                    let entries = std::fs::read_dir(&old).map(|it| it.flatten().collect::<Vec<_>>()).unwrap_or_default();
                    for entry in entries {
                        let target = new.join(entry.file_name());
                        if target.exists() {
                            continue;
                        }
                        if let Err(e) = std::fs::rename(entry.path(), &target) {
                            tracing::warn!(error = %e, series = %entry.file_name().to_string_lossy(), "mirror: a series folder did not move - it starts over");
                        }
                    }
                    let _ = std::fs::remove_dir(&old);
                }
            }
        }
        self.files.mirror_planned_at = 0;
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

    /// A member's hold status landed - one page of a generation: the
    /// generation replaces what the member said before once every page
    /// arrived; a newer generation drops a half-collected older one.
    pub(crate) fn cmd_net_mirror_status(
        &mut self,
        from: &MemberId,
        holds: Vec<MirrorHold>,
        gen: u64,
        page: u16,
        pages: u16,
    ) -> Result<Reply, MoltError> {
        if *from == self.member() || !self.roster().contains(from) || pages == 0 || page >= pages {
            return Ok(Reply::Ack);
        }
        let complete = if pages == 1 {
            self.files.mirror_pages.remove(from);
            Some(holds)
        } else {
            let now = crate::now_secs();
            let entry = self
                .files
                .mirror_pages
                .entry(from.clone())
                .or_insert_with(|| (gen, pages, now, std::collections::BTreeMap::new()));
            if entry.0 > gen {
                return Ok(Reply::Ack); // a straggler of an older generation
            }
            if entry.0 < gen || entry.1 != pages {
                *entry = (gen, pages, now, std::collections::BTreeMap::new());
            }
            entry.3.insert(page, holds);
            if entry.3.len() == usize::from(pages) {
                let (_, _, _, collected) = self.files.mirror_pages.remove(from).unwrap_or_default();
                Some(collected.into_values().flatten().collect())
            } else {
                None
            }
        };
        if let Some(holds) = complete {
            self.files.mirror.status.insert(from.clone(), holds);
            self.persist_mirror();
        }
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
        // own shares are whole; a running mirror job is not a holder yet
        for hold in self.own_mirror_holds() {
            if hold.held == hold.of && hold.of > 0 {
                out.entry(hold.id).or_default().push(self.member());
            }
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
            used: self.mirror_used(),
            dir: self.mirror_dir().map(|d| d.display().to_string()).unwrap_or_default(),
            members,
            files,
        }
    }
}
