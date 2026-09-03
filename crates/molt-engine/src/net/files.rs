// SPDX-License-Identifier: GPL-3.0-or-later

//! The RELAY file plane on the engine side (`file_transfer_nostr.md`):
//! the requester's fetch (announced stamp or `FileWanted`), the sharer's
//! lazy series publish and its announcement, the queue-plane request
//! answer, and the size cap derived from the publish budget. The
//! off-actor tasks live in `crate::transfer`.

use super::*;

/// The relay file plane's working set: `(channel, secrets-to-OPEN,
/// current-secret-to-SEAL)` — the seal half is `None` when the current
/// epoch's exporter is unavailable (a serve must then refuse, never fall
/// back to a stale ring secret).
type FilePlaneContext = (
    molt_net::ritual_net::GroupChannel,
    Vec<[u8; 32]>,
    Option<[u8; 32]>,
);


impl State {
    /// The operative per-file cap (`file_transfer_nostr.md` §5.1):
    /// `None` = file sharing is OFF (`file_cap_bytes = 0`, FP4 2026-08-16),
    /// otherwise the configured value (an absent config key serde-defaults
    /// to 4 MiB before it ever reaches here).
    pub(crate) fn effective_file_cap(&self) -> Option<u64> {
        let cap = self.session.settings.file_cap_bytes;
        (cap != 0).then_some(cap)
    }

    /// The RELAY file plane's channel + exporter material, if this
    /// workspace can carry one (`file_transfer_nostr.md`): the same dial
    /// list and rotation seed the group runtime uses, plus the MLS
    /// exporter ring (open) and its head (seal).
    fn nostr_file_context(&self) -> Option<FilePlaneContext> {
        let nostr = self.nostr.as_ref()?;
        let relays = self.dialable_group_relays();
        if relays.is_empty() {
            return None;
        }
        let dialer = self.dialer_for().ok()?;
        let channel =
            molt_net::ritual_net::GroupChannel::new(dialer, relays, nostr.rotation_seed);
        // the CURRENT epoch's secret leads (it seals new series), the ring
        // follows (it opens series sealed before a re-key) — a fresh seat's
        // ring is empty until the first rotation, so the current secret is
        // what makes the plane work at all. The current secret travels
        // SEPARATELY too: a serve must refuse when it is unavailable rather
        // than seal a fresh series under a stale ring epoch nobody past the
        // ring horizon could open (review 2026-08-10).
        let (ring, current) = {
            let g = self.group_net.as_ref()?;
            let m = g.mls.lock().ok()?;
            (m.exporter_ring().to_vec(), m.exporter_secret().ok())
        };
        let mut secrets: Vec<[u8; 32]> = Vec::with_capacity(ring.len() + 1);
        if let Some(c) = current {
            secrets.push(c);
        }
        for s in ring {
            if !secrets.contains(&s) {
                secrets.push(s);
            }
        }
        if secrets.is_empty() {
            return None;
        }
        Some((channel, secrets, current))
    }

    /// Download a peer's share over the relay plane: fetch when the
    /// series' publish stamp is known, else park the download and ask the
    /// sharer to publish (lazy) — the `FileServed` announcement resumes it.
    pub(crate) fn nostr_download(
        &mut self,
        id: MessageId,
        target: crate::transfer::FetchTarget,
        dest: crate::transfer::DestSpec,
    ) {
        if let Some(at) = self.files.series.get(&id).copied() {
            self.spawn_nostr_fetch(id, at, target, dest);
        } else {
            self.files.pending.insert(id, (target, dest));
            let me = self.member();
            let env = self.make_env(me, WorkspaceEvent::FileWanted { id });
            self.record(env);
            // a parked download must not wait forever: if no FileServed
            // drains it within the window, it fails honestly and the
            // operator can retry (review 2026-08-10 — the park had no
            // timeout and the phase guard blocked every retry)
            if let Some(cmd_tx) = self.cmd_tx.upgrade() {
                crate::transfer::spawn_want_timeout(id, self.net_scope, cmd_tx);
            }
        }
    }

    /// The parked download's watchdog fired: if the `FileServed` answer
    /// never came (the id still parks), fail the download honestly — a
    /// drained park means the fetch is running and the watchdog is stale.
    pub(crate) fn cmd_net_file_wanted_timeout(
        &mut self,
        id: MessageId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        if self.files.pending.remove(&id).is_some() {
            self.set_download_phase(
                id,
                molt_core::TransferPhase::Failed {
                    reason: "the sharer did not answer".to_string(),
                },
            );
        }
        Ok(Reply::Ack)
    }

    /// Spawn the off-actor series fetch (reports back over the same
    /// `NetFileDone`/`NetFileFailed` path the queue-plane download uses).
    pub(super) fn spawn_nostr_fetch(
        &mut self,
        id: MessageId,
        at: u64,
        target: crate::transfer::FetchTarget,
        dest: crate::transfer::DestSpec,
    ) {
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        let Some((channel, ring, _)) = self.nostr_file_context() else {
            crate::transfer::spawn_file_verdict(
                id,
                Err("no dialable relay or group ring for the file plane".to_string()),
                self.net_scope,
                cmd_tx,
            );
            return;
        };
        let fetch = crate::transfer::spawn_nostr_fetch(
            channel,
            ring,
            id,
            at,
            target,
            dest,
            // sharing-off still allows PULLING a peer's share; the default
            // bounds what a hostile series claim may allocate
            self.effective_file_cap()
                .unwrap_or_else(molt_core::default_file_cap_bytes),
            self.net_scope,
            cmd_tx,
        );
        self.files.fetches.retain(|h| !h.is_finished());
        self.files.fetches.push(fetch);
    }

    /// A `FileWanted` broadcast landed: ONLY the sharer answers (every
    /// member receives it), by lazily publishing the chunk series — or by
    /// re-announcing a fresh enough stamp, so a burst of requests does not
    /// publish the series N times.
    pub(super) fn serve_file_wanted(&mut self, id: MessageId) {
        let me = self.member();
        let (is_mine, size) = match self.share_identity(&id) {
            Ok((ident, available)) => (ident.by == me && available, ident.size),
            Err(_) => return,
        };
        if !is_mine || self.share_expired(&id) || self.files.serving.contains(&id) {
            return;
        }
        // the size is known here — an over-cap share must not cost a full
        // disk read per request only to be refused inside the publish
        // (review 2026-08-10; the share-time gate makes this an edge)
        let Some(cap) = self.effective_file_cap() else {
            tracing::warn!(%id, "not serving: file sharing off (file_cap_bytes=0)");
            return;
        };
        if size > cap {
            tracing::warn!(%id, size, "not serving a share beyond the file cap");
            return;
        }
        let now = crate::now_secs();
        if let Some(at) = self.files.series.get(&id).copied() {
            // a standing series re-announces instead of re-publishing (one
            // stored copy serves everyone within relay retention) — UNLESS
            // this requester evidently just saw the stamp and still asks
            // again: then the series is unfetchable for it (pruned, or
            // sealed under an epoch it cannot open) and only a FRESH
            // publish under the current secret converges (review 2026-08-10)
            let recently_announced = self
                .files.announced
                .get(&id)
                .is_some_and(|t| now.saturating_sub(*t) < 300);
            if now.saturating_sub(at) < 86_400 && !recently_announced {
                self.files.announced.insert(id, now);
                let env = self.make_env(me, WorkspaceEvent::FileServed { id, at });
                self.record(env);
                return;
            }
        }
        let Some(path) = self.files.share_paths.get(&id).cloned() else {
            return;
        };
        let Some((channel, _, current)) = self.nostr_file_context() else {
            return;
        };
        // a fresh series seals under the CURRENT epoch only — publishing
        // under a stale ring secret would hand out a series fresh seats
        // and post-re-key members can never open
        let Some(exporter) = current else {
            tracing::warn!(%id, "no current exporter secret - not publishing the series");
            return;
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return;
        };
        // §5.4: the publish is metered on the SAME persisted hourly budget
        // the resend rounds draw from — the store is how the consumption
        // lands in transport.state
        let Some(store) = self
            .active
            .as_ref()
            .map(|a| crate::net::FileStateStore::new(a.handle.clone()))
        else {
            return;
        };
        self.files.serving.insert(id);
        crate::transfer::spawn_series_publish(
            channel,
            exporter,
            id,
            path,
            cap,
            store,
            self.net_scope,
            cmd_tx,
        );
    }

    /// The off-actor series publish reported back: clear the in-flight
    /// mark and, on success, announce the stamp to the group (that
    /// announcement is what resumes the requesters' parked fetches).
    pub(crate) fn cmd_net_file_series_published(
        &mut self,
        id: MessageId,
        at: u64,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_scope_current(generation) {
            return Ok(Reply::Ack);
        }
        self.files.serving.remove(&id);
        if at == 0 {
            return Ok(Reply::Ack); // the publish failed — honest silence, the
                                   // requester's park runs into its watchdog
        }
        self.files.series.insert(id, at);
        self.files.announced.insert(id, crate::now_secs());
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::FileServed { id, at });
        self.record(env);
        Ok(Reply::Ack)
    }

    /// A group-authenticated fetch request landed. The broadcast reaches
    /// EVERY member, so **only the sharer answers** — a member that does
    /// not (yet) hold the share, or whose share it isn't, stays completely
    /// silent: a `Refused` from a non-sharer would abort the requester's
    /// fetch of the REAL sharer's bytes (a laggard's refusal racing the
    /// sharer's manifest). Once this node is established as the sharer,
    /// honest refusals (unavailable, path lost) are correct.
    pub(super) fn answer_file_request(&mut self, req: molt_net::transfer::FetchRequest) {
        let Some(transport) = self.net.as_ref().and_then(|n| n.runtime_transport()) else {
            return; // no real mesh → nothing to serve on
        };
        if req.expires < crate::now_secs() {
            tracing::debug!(share = %req.id, "dropping an expired file request");
            return; // the requester is long gone — nobody listens for a refusal
        }
        let Ok(id) = req.id.parse::<MessageId>() else {
            return;
        };
        let me = self.member();
        // silent unless this node is the sharer — never refuse a share we
        // simply don't have; the actual sharer answers
        let is_my_share = matches!(self.chat_by_id(&id), Ok((_, msg)) if msg.from == me)
            || matches!(self.share_identity(&id), Ok((ident, _)) if ident.by == me);
        if !is_my_share {
            return;
        }
        let refuse = |reason: &str| {
            let frame = molt_net::transfer::TransferFrame::Refused {
                id: req.id.clone(),
                reason: reason.to_string(),
            };
            crate::transfer::spawn_send_refusal(transport.clone(), req.reply.clone(), frame);
        };
        let Ok((ident, available)) = self.share_identity(&id) else {
            refuse("the message carries no file");
            return;
        };
        if !available {
            refuse("the sharer removed the file - no longer available");
            return;
        }
        // a share past its window is not served, even to a requester whose
        // local check lagged (an honest refusal near the boundary, not a
        // hang); a persisted share has no window
        if self.share_expired(&id) {
            refuse("the share aged out of the chat retention window");
            return;
        }
        let size = ident.size;
        let Some(path) = self.files.share_paths.get(&id).cloned() else {
            refuse("this node no longer knows the shared file's local path");
            return;
        };
        crate::transfer::spawn_file_serve(
            transport,
            path,
            size,
            req.id,
            req.reply,
            self.files.serve_slots.clone(),
        );
    }

    /// The fetch task's request is ready: record the `FileRequested` event
    /// (the outbox ships it to every peer; the sharer answers).
    pub(crate) fn cmd_net_file_request_ready(
        &mut self,
        id: MessageId,
        ct: String,
    ) -> Result<Reply, MoltError> {
        // the share must still exist and be available — the honest guard
        // before broadcasting a request every member will decrypt
        let (_, available) = self.share_identity(&id)?;
        if !available {
            return Err(MoltError::FileUnavailable(id));
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::FileRequested { ct });
        self.record(env);
        Ok(Reply::Ack)
    }
}
