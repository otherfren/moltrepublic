// SPDX-License-Identifier: GPL-3.0-or-later

//! The automatic S3 backup (mock_todo story 12, `backup_restore_design.md`
//! §6): a slow engine ticker ([`molt_core::Command::BackupTick`]) whose
//! **synchronous** handler only *decides* — for every workspace whose
//! auto-backup pref is on, whose interval elapsed, and whose key material is
//! accessible it spawns an off-actor task that builds the crash-consistent
//! `molt-export-v1` blob in `workspace` key mode (restorable from recovery
//! phrase + workspace id — no prompt) and PUTs it to the configured bucket
//! through the fail-closed dialer, then prunes copies beyond
//! `s3_keep_copies`.
//!
//! Honesty is the contract: `prefs.last_backup` (and the list entry's
//! stamp) move **only** on a confirmed upload ([`Command::NetBackupDone`]);
//! failures land verbatim in the entry's `backup_error` and the session
//! notice; a sealed-at-rest workspace is skipped with an honest status
//! (design P6) — no key is accessible and a ticker cannot prompt.

use molt_core::{Command, MoltError, Reply, SessionScope, WorkspaceId};

use crate::{now_secs, Envelope, State};

/// The backup ticker period: the decide pass is cheap (a few manifest/prefs
/// reads), so one pass a minute keeps `s3_interval_min` honest at its
/// minute granularity.
pub(crate) const BACKUP_TICK_MS: u64 = 60_000;

/// The honest per-workspace status of a sealed-at-rest skip (design P6).
const SEALED_SKIP: &str = "sealed at rest — backup skipped until decrypted";

impl State {
    /// One decide pass of the backup ticker (engine-internal). Synchronous:
    /// never awaits I/O — due workspaces spawn their upload task and report
    /// back as [`Command::NetBackupDone`] / [`Command::NetBackupFailed`].
    pub(crate) fn cmd_backup_tick(&mut self) -> Result<Reply, MoltError> {
        if !self.persist {
            return Ok(Reply::Ack);
        }
        // re-age the "last backup" label every pass, even when nothing is
        // due: a confirmed upload stamps ~0 minutes and without this the UI
        // would show "gerade eben" forever until a restart (review finding).
        let mut changed = self.reage_backup_labels();
        // the GLOBAL "Automatic S3 backup" switch is a MASTER gate: off =
        // this ticker decides nothing, regardless of per-workspace prefs
        // (unchecking the settings box must actually stop the automation —
        // 2026-07-19 report; the per-workspace pref picks WHICH republics
        // back up while the switch is on)
        if !self.session.settings.s3_backup {
            if changed {
                self.emit_session(SessionScope::Full);
            }
            return Ok(Reply::Ack);
        }
        // candidates: the per-workspace pref (mirrored in the entry) is on
        let candidates: Vec<WorkspaceId> = self
            .session
            .workspaces
            .iter()
            .filter(|w| w.s3 && !self.backup_inflight.contains(&w.id))
            .map(|w| w.id.clone())
            .collect();
        // clamp to the tick period: interval 0 must not mean "a full blob
        // upload every single tick, forever"
        let interval_secs = u64::from(self.session.settings.s3_interval_min.max(1)) * 60;
        // no configured target → nothing to spawn; the ticker stays silent
        // (the settings panel / backup table already surface the state, and
        // a per-minute notice would be spam, not honesty). The re-aging
        // above still ran, so emit if it moved a label.
        let s = &self.session.settings;
        let config = molt_net::s3::S3Config::from_settings(
            &s.s3_endpoint,
            &s.s3_access_key,
            &s.s3_secret_key,
            &s.s3_bucket,
        );
        let config = match config {
            Ok(config) if !candidates.is_empty() => config,
            _ => {
                if changed {
                    self.emit_session(SessionScope::Full);
                }
                return Ok(Reply::Ack);
            }
        };
        let now = now_secs();
        let root = self.workspace_root();
        for id in candidates {
            let sealed = self
                .session
                .workspaces
                .iter()
                .any(|w| w.id == id && w.encrypted);
            if sealed {
                // design P6: skip with an honest status — no key on disk,
                // and prompting from a ticker is not a thing
                changed |= self.set_backup_error(&id, SEALED_SKIP);
                continue;
            }
            let Some(dir) = molt_storage::find_workspace_dir(&root, &id) else {
                changed |= self.set_backup_error(&id, "workspace directory missing");
                continue;
            };
            if let Some(reason) = backup_refusal_reason(&dir) {
                // a chainless (legacy) dir would ship a blob restore always
                // refuses — skip it with an honest status, never a doomed
                // upload the user only discovers at disaster time
                changed |= self.set_backup_error(&id, reason);
                continue;
            }
            // last completed backup: engine-held prefs for the open
            // workspace, plus the in-memory last-done as a fallback so a
            // stamp that could not be persisted does not re-upload every tick
            let last_backup = self.effective_last_backup(&id, &dir);
            if last_backup.is_some_and(|t| now.saturating_sub(t) < interval_secs) {
                continue; // not due yet
            }
            match self.dialer_for() {
                Ok(dialer) => {
                    self.spawn_backup_task(id, dir, config.clone(), dialer);
                }
                Err(e) => {
                    // fail-closed: a Tor misconfiguration never falls back
                    // to clearnet — it surfaces as the honest failure
                    changed |= self.set_backup_error(&id, &e.to_string());
                }
            }
        }
        if changed {
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// The last CONFIRMED backup timestamp for a workspace: the durable
    /// `prefs.last_backup` (engine-held for the open workspace, on-disk for
    /// a closed one) OR the in-memory last-done, whichever is newer. The
    /// in-memory copy survives a `prefs` write that could not land, so a
    /// non-persistable stamp does not force a full re-upload every minute.
    fn effective_last_backup(&self, id: &str, dir: &std::path::Path) -> Option<u64> {
        let persisted = match &self.active {
            Some(a) if a.id == id => a.prefs.last_backup,
            _ => molt_storage::read_prefs(dir).last_backup,
        };
        let in_mem = self.backup_last_done.get(id).copied();
        match (persisted, in_mem) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Refresh every entry's `last_backup_min` from its effective stamp so
    /// the "letztes Backup" age keeps advancing between uploads. Returns
    /// whether any label moved.
    fn reage_backup_labels(&mut self) -> bool {
        let now = now_secs();
        let root = self.workspace_root();
        let ids: Vec<WorkspaceId> = self
            .session
            .workspaces
            .iter()
            .map(|w| w.id.clone())
            .collect();
        let mut changed = false;
        for id in ids {
            let Some(dir) = molt_storage::find_workspace_dir(&root, &id) else {
                continue;
            };
            let Some(ts) = self.effective_last_backup(&id, &dir) else {
                continue;
            };
            let minutes = u32::try_from(now.saturating_sub(ts) / 60).unwrap_or(u32::MAX - 1);
            if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
                if ws.last_backup_min != minutes {
                    ws.last_backup_min = minutes;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The manual "backup now to S3" trigger (a tool on both surfaces):
    /// same task as the ticker, interval ignored — but every precondition
    /// is a loud, honest refusal instead of a silent skip.
    pub(crate) fn cmd_backup_now(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        let Some(entry) = self.session.workspaces.iter().find(|w| w.id == id) else {
            return Err(MoltError::UnknownWorkspace(id));
        };
        if entry.encrypted {
            return Err(MoltError::WorkspaceEncrypted(id));
        }
        if !self.persist {
            return Err(MoltError::Storage(
                "this node has no workspace storage to back up".to_string(),
            ));
        }
        if self.backup_inflight.contains(&id) {
            return Err(MoltError::WorkspaceBusy(
                "a backup of this workspace is already running".to_string(),
            ));
        }
        let s = &self.session.settings;
        let config = molt_net::s3::S3Config::from_settings(
            &s.s3_endpoint,
            &s.s3_access_key,
            &s.s3_secret_key,
            &s.s3_bucket,
        )
        .map_err(|e| MoltError::Settings(format!("backup target: {e}")))?;
        let root = self.workspace_root();
        let dir = molt_storage::find_workspace_dir(&root, &id).ok_or_else(|| {
            MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            ))
        })?;
        if let Some(reason) = backup_refusal_reason(&dir) {
            // a chainless (legacy) dir exports a blob restore always refuses
            // — refuse loudly here rather than upload a blob that only fails
            // at disaster time
            return Err(MoltError::Storage(reason.to_string()));
        }
        let dialer = self
            .dialer_for()
            .map_err(|e| MoltError::Settings(e.to_string()))?;
        self.spawn_backup_task(id, dir, config, dialer);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// S7 (`backup_restore_design.md` §10): fetch the NEWEST bucket backup
    /// of `id` onto this device as a SEALED stub — no secret asked, nothing
    /// decrypted. The off-actor task lists `molt/<id>/`, downloads the
    /// newest object VERBATIM and lands it via
    /// [`molt_storage::write_restored_stub`]; the outcome returns as the
    /// engine-internal [`Command::NetBackupFetched`].
    pub(crate) fn cmd_backup_fetch(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        if !self.persist {
            return Err(MoltError::Storage(
                "this node has no workspace storage to fetch into".to_string(),
            ));
        }
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(MoltError::Storage(
                "pick a backup: the workspace id from the backup table (64 hex chars)"
                    .to_string(),
            ));
        }
        let root = self.workspace_root();
        if molt_storage::find_workspace_dir(&root, &id).is_some() {
            return Err(MoltError::Storage(format!(
                "workspace {id} already exists locally"
            )));
        }
        let s = &self.session.settings;
        let config = molt_net::s3::S3Config::from_settings(
            &s.s3_endpoint,
            &s.s3_access_key,
            &s.s3_secret_key,
            &s.s3_bucket,
        )
        .map_err(|e| MoltError::Settings(format!("backup target: {e}")))?;
        let dialer = self
            .dialer_for()
            .map_err(|e| MoltError::Settings(e.to_string()))?;
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Storage("engine stopped".to_string()));
        };
        tokio::spawn(async move {
            let done = |id: WorkspaceId, error: String| Command::NetBackupFetched { id, error };
            let client = molt_net::s3::S3Client::new(config, dialer);
            let outcome = async {
                let prefix = format!("molt/{id}/");
                let objects = client
                    .list_objects(&prefix)
                    .await
                    .map_err(|e| format!("s3: list {prefix}: {e}"))?;
                let newest = objects
                    .iter()
                    .filter_map(|o| molt_core::parse_backup_key(&o.key).map(|(_, ts)| (ts, &o.key)))
                    .max_by_key(|(ts, _)| *ts)
                    .map(|(ts, key)| (ts, key.clone()))
                    .ok_or_else(|| format!("no backup for {id} in the bucket"))?;
                let (ts, key) = newest;
                let mut blob: Vec<u8> = Vec::new();
                client
                    .get_object(
                        &key,
                        &mut blob,
                        crate::lifecycles::RESTORE_MAX_BYTES,
                        &mut |_done, _total| {},
                    )
                    .await
                    .map_err(|e| format!("s3: GET {key}: {e}"))?;
                let stub_id = id.clone();
                tokio::task::spawn_blocking(move || {
                    molt_storage::write_restored_stub(&root, &stub_id, ts, &blob)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| format!("stub write: {e}"))??;
                Ok::<(), String>(())
            }
            .await;
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx
                .send(crate::Envelope {
                    cmd: done(id, outcome.err().unwrap_or_default()),
                    reply,
                })
                .await;
        });
        Ok(Reply::Ack)
    }

    /// The fetch task's outcome: on success the sealed stub joins the
    /// workspace list (the Open screen shows it); on failure the honest
    /// reason rides the notice.
    pub(crate) fn cmd_net_backup_fetched(
        &mut self,
        id: WorkspaceId,
        error: String,
    ) -> Result<Reply, MoltError> {
        if !error.is_empty() {
            self.session.notice = format!("backup-fetch-failed:{error}");
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }
        let root = self.workspace_root();
        let net = self.effective_net_label();
        if let Some(entry) = molt_storage::scan_workspaces(&root)
            .iter()
            .find(|e| e.manifest.workspace.id == id)
        {
            let mut info = entry.info();
            info.net = net;
            // replace-or-push: idempotent against a re-fetch after a delete
            self.session.workspaces.retain(|w| w.id != id);
            self.session.workspaces.push(info);
            // its orphan row retires against the last listing
            self.reclassify_backups();
        }
        self.session.notice = format!("backup-fetched:{id}");
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Spawn the off-actor backup task for one workspace: blob build
    /// (blocking — Argon2-free in workspace key mode, but file I/O) on the
    /// blocking pool, then PUT + retention over the network. Marks the
    /// workspace in flight; the result returns as the engine-internal
    /// `NetBackupDone` / `NetBackupFailed`.
    fn spawn_backup_task(
        &mut self,
        id: WorkspaceId,
        dir: std::path::PathBuf,
        config: molt_net::s3::S3Config,
        dialer: molt_net::dial::Dialer,
    ) {
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return; // actor shutting down — no task, no inflight mark
        };
        // the blob is built by COPYING the directory, so everything the
        // caller already considers written has to be on disk first — the
        // writer group-commits (fsync at most every 50 ms), so a just-sent
        // chat message can still be in its buffer and would silently be
        // missing from the backup (an intermittent, load-dependent hole in
        // the one copy that exists for disaster recovery). The flush itself
        // waits on an fsync, so it belongs in the task, NOT on the actor —
        // a slow disk must never stall command handling.
        let flush = self
            .active
            .as_ref()
            .filter(|a| a.id == id)
            .map(|a| a.handle.clone());
        self.backup_inflight.insert(id.clone());
        let root = self.workspace_root();
        let keep = usize::from(self.session.settings.s3_keep_copies.max(1));
        let max_bytes = self.session.settings.s3_max_bytes;
        tokio::spawn(async move {
            let ts = now_secs();
            let build_dir = dir.clone();
            let build = tokio::task::spawn_blocking(move || {
                if let Some(handle) = flush {
                    if !handle.flush_blocking() {
                        // the backup would capture a log the disk does not
                        // hold yet — say so rather than shipping it quietly
                        tracing::error!("flush before backup failed — the copy may lag the log");
                    }
                }
                let mut blob = Vec::new();
                molt_storage::export::export_dir(
                    &root,
                    &build_dir,
                    &molt_storage::export::ExportKey::Workspace,
                    &mut blob,
                )
                .map(|outcome| (blob, outcome))
            })
            .await;
            let cmd = match build {
                Ok(Ok((blob, _outcome))) => {
                    let bytes = u64::try_from(blob.len()).unwrap_or(u64::MAX);
                    if bytes > crate::lifecycles::RESTORE_MAX_BYTES {
                        // enforce the restore path's own size cap here: a blob
                        // beyond it would be refused at restore time, so an
                        // "upload" of it is a false backup, not a real one
                        Command::NetBackupFailed {
                            id,
                            error: format!(
                                "workspace export is {bytes} bytes — beyond the \
                                 {}-byte cap the restore path enforces; not uploaded",
                                crate::lifecycles::RESTORE_MAX_BYTES
                            ),
                        }
                    } else {
                        let object = molt_core::backup_key(&id, ts);
                        let client = molt_net::s3::S3Client::new(config, dialer);
                        match client.put_object(&object, &blob).await {
                            Ok(()) => {
                                // retention only AFTER the confirmed upload;
                                // a failure is surfaced, never blocks the
                                // backup (the next success re-prunes). The
                                // byte quota runs on top of the COUNT
                                // retention and reports separately - the two
                                // tell different stories.
                                let prune_error =
                                    prune_old_copies(&client, &id, keep, &object).await;
                                let quota_error =
                                    enforce_quota(&client, max_bytes, &object).await;
                                Command::NetBackupDone {
                                    id,
                                    ts,
                                    object,
                                    bytes,
                                    prune_error,
                                    quota_error,
                                }
                            }
                            Err(e) => Command::NetBackupFailed {
                                id,
                                error: e.to_string(),
                            },
                        }
                    }
                }
                Ok(Err(e)) => Command::NetBackupFailed {
                    id,
                    error: e.to_string(),
                },
                Err(e) => Command::NetBackupFailed {
                    id,
                    error: format!("backup task failed: {e}"),
                },
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(Envelope { cmd, reply }).await;
        });
    }

    /// A confirmed upload (engine-internal): ONLY here does the stamp move.
    pub(crate) fn cmd_net_backup_done(
        &mut self,
        id: WorkspaceId,
        ts: u64,
        object: String,
        bytes: u64,
        prune_error: String,
        quota_error: String,
    ) -> Result<Reply, MoltError> {
        self.backup_inflight.remove(&id);
        // the in-memory last-done is the authoritative fallback: it is set
        // even if the durable prefs stamp below cannot be written, so the
        // due-check never re-uploads on a loop
        self.backup_last_done.insert(id.clone(), ts);
        // clear the last failure FIRST (this upload succeeded), then stamp:
        // if the durable stamp cannot persist, `stamp_backup_time` re-sets
        // an honest error, which must survive rather than be cleared here
        let minutes = u32::try_from(now_secs().saturating_sub(ts) / 60).unwrap_or(u32::MAX - 1);
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.last_backup_min = minutes;
            ws.backup_error = String::new();
        }
        self.stamp_backup_time(&id, ts);
        tracing::info!(id, object, bytes, "backup uploaded");
        // one notice slot, so the count retention wins when both spoke —
        // its failure is the older, more basic one
        if !prune_error.is_empty() {
            tracing::warn!(id, error = %prune_error, "backup retention pruning failed");
            self.note_backup(format!("backup-prune-failed:{prune_error}"));
        } else if !quota_error.is_empty() {
            tracing::warn!(id, error = %quota_error, "backup bucket quota");
            self.note_backup(format!("backup-quota:{quota_error}"));
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A failed backup (engine-internal): stamp untouched, reason verbatim.
    pub(crate) fn cmd_net_backup_failed(
        &mut self,
        id: WorkspaceId,
        error: String,
    ) -> Result<Reply, MoltError> {
        self.backup_inflight.remove(&id);
        tracing::warn!(id, error = %error, "backup failed");
        self.set_backup_error(&id, &error);
        self.note_backup(format!("backup-failed:{error}"));
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Set a backup notice WITHOUT clobbering a foreign one: the backup
    /// tasks are the first timer-driven writers of the single-slot session
    /// notice, and overwriting e.g. a freshly minted `recovery-link:…`
    /// before the GUI's edge-trigger consumed it would eat a one-shot
    /// dialog. Backup notices only replace an empty slot or each other;
    /// the per-workspace `backup_error` state carries the failure anyway.
    fn note_backup(&mut self, notice: String) {
        let current = &self.session.notice;
        if current.is_empty() || current.starts_with("backup-") {
            self.session.notice = notice;
        }
    }

    /// Persist `prefs.last_backup` for one workspace — the same
    /// writer-vs-direct discipline as `persist_backup_pref`.
    fn stamp_backup_time(&mut self, id: &str, ts: u64) {
        if let Some(a) = &mut self.active {
            if a.id == id {
                a.prefs.last_backup = Some(ts);
                a.handle.set_prefs(a.prefs.clone());
                return;
            }
        }
        let Some(dir) = molt_storage::find_workspace_dir(&self.workspace_root(), id) else {
            tracing::warn!(id, "backup stamp not persisted: workspace directory missing");
            return;
        };
        let mut prefs = molt_storage::read_prefs(&dir);
        prefs.last_backup = Some(ts);
        if let Err(e) = molt_storage::write_prefs(&dir, &prefs) {
            // surface it: the upload happened, but the durable stamp did not
            // land — the in-memory last-done (set in cmd_net_backup_done)
            // keeps the due-check from re-uploading, and the user sees why
            // the on-disk age will look stale after a restart
            tracing::warn!(error = %e, "persisting the backup stamp failed");
            self.set_backup_error(id, &format!("backup uploaded but the stamp could not persist: {e}"));
            self.note_backup(format!("backup-stamp-not-persisted:{e}"));
        }
    }

    /// Set one entry's `backup_error`; returns whether anything changed
    /// (the ticker emits only when a pass changed visible state).
    fn set_backup_error(&mut self, id: &str, error: &str) -> bool {
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            if ws.backup_error != error {
                ws.backup_error = error.to_string();
                return true;
            }
        }
        false
    }
}

/// Prune this workspace's bucket copies beyond `keep` (design §6.3): list
/// the prefix, sort keys (lexicographic == age, §6.2), DELETE the oldest
/// beyond the keep window. Every failure is reported (first one wins the
/// message), none is retried here — the next successful backup re-prunes.
/// `just_uploaded` is the object this backup just confirmed — it is NEVER
/// a prune candidate, so a clock regression that makes its timestamp sort
/// "oldest" cannot delete the very copy `NetBackupDone` is confirming.
/// Returns `""` on success.
async fn prune_old_copies(
    client: &molt_net::s3::S3Client,
    id: &WorkspaceId,
    keep: usize,
    just_uploaded: &str,
) -> String {
    let prefix = format!("{}{id}/", molt_core::BACKUP_OBJECT_PREFIX);
    let listed = match client.list_objects(&prefix).await {
        Ok(objects) => objects,
        Err(e) => return format!("listing for retention failed: {e}"),
    };
    // only keys that follow the backup naming scheme for THIS workspace are
    // ours to prune — foreign objects under the prefix are left alone
    let keys: Vec<String> = listed
        .into_iter()
        .filter(|o| molt_core::parse_backup_key(&o.key).is_some_and(|(kid, _)| kid == *id))
        .map(|o| o.key)
        .collect();
    let mut first_error = String::new();
    for key in prune_candidates(keys, keep, just_uploaded) {
        if let Err(e) = client.delete_object(&key).await {
            if first_error.is_empty() {
                first_error = format!("deleting {key} failed: {e}");
            }
        }
    }
    first_error
}

/// Pure retention decision: from a workspace's backup object keys, the
/// oldest to delete so at most `keep` remain — but NEVER `just_uploaded`,
/// the fresh copy this backup just confirmed. Keys sort lexicographically =
/// age (§6.2 zero-padded timestamps). Filtering the just-uploaded key out
/// of the delete set (rather than out of the candidates) keeps retention
/// exact in the normal case and only ever over-retains by one under a clock
/// regression — the rare case where our new object's timestamp sorts
/// "oldest" and would otherwise be deleted while `NetBackupDone` confirms it.
fn prune_candidates(mut keys: Vec<String>, keep: usize, just_uploaded: &str) -> Vec<String> {
    keys.sort_unstable();
    let excess = keys.len().saturating_sub(keep);
    keys.into_iter()
        .take(excess)
        .filter(|k| k != just_uploaded)
        .collect()
}

/// One listed backup object, reduced to what the quota decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuotaObject {
    /// The full object key.
    key: String,
    /// The workspace the object belongs to.
    id: molt_core::WorkspaceId,
    /// The object's backup timestamp (parsed from the key, = its age).
    ts: u64,
    /// Size in bytes as the listing reported it.
    size: u64,
}

/// Hold the backup bucket under `max_bytes` (`docs/storage/s3_buckets.md`
/// §4), bucket-wide across every workspace this node backs up. `0` = no
/// limit and no listing round-trip.
///
/// Only objects that [`molt_core::parse_backup_key`] accepts are counted or
/// deleted: a foreign object in the bucket is not ours to prune, and
/// counting it would make someone else's upload delete our backups.
/// Returns `""` on success, otherwise the first honest failure - including
/// the case where the quota cannot be met without eating a workspace's last
/// copy, which is reported rather than done.
async fn enforce_quota(
    client: &molt_net::s3::S3Client,
    max_bytes: u64,
    just_uploaded: &str,
) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let listed = match client.list_objects(molt_core::BACKUP_OBJECT_PREFIX).await {
        Ok(objects) => objects,
        Err(e) => return format!("listing failed: {e}"),
    };
    let objects: Vec<QuotaObject> = listed
        .into_iter()
        .filter_map(|o| {
            molt_core::parse_backup_key(&o.key).map(|(id, ts)| QuotaObject {
                key: o.key,
                id,
                ts,
                size: o.size,
            })
        })
        .collect();
    let (delete, remaining) = quota_candidates(objects, max_bytes, just_uploaded);
    let mut first_error = String::new();
    for key in delete {
        if let Err(e) = client.delete_object(&key).await {
            if first_error.is_empty() {
                first_error = format!("deleting {key} failed: {e}");
            }
        }
    }
    if !first_error.is_empty() {
        return first_error;
    }
    if remaining > max_bytes {
        return format!("{remaining} B of {max_bytes} B, nothing left to prune");
    }
    String::new()
}

/// Pure byte-quota decision (`docs/storage/s3_buckets.md` §4): from every
/// backup object this node wrote, the oldest to delete so the total fits
/// `max_bytes`, plus the byte total that remains afterwards.
///
/// Three rules make the difference between retention and data loss:
/// `max_bytes == 0` is NO limit; the **newest copy of every workspace** is
/// never a candidate (a byte quota must not leave a republic with zero
/// backups); and neither is `just_uploaded`, the copy the confirming
/// backup is about — under a clock regression its timestamp sorts oldest.
/// Age is the PARSED timestamp, not the key order: a key carries the
/// workspace id before the stamp, so lexicographic order across workspaces
/// is not age order. An unfittable quota deletes every candidate and
/// returns a remainder still above the limit — the caller says so rather
/// than eating into the newest copies.
fn quota_candidates(
    objects: Vec<QuotaObject>,
    max_bytes: u64,
    just_uploaded: &str,
) -> (Vec<String>, u64) {
    let used: u64 = objects.iter().fold(0u64, |acc, o| acc.saturating_add(o.size));
    if max_bytes == 0 || used <= max_bytes {
        return (Vec::new(), used);
    }
    // the newest copy per workspace, deterministic on a timestamp tie
    let mut newest: std::collections::HashMap<&str, &QuotaObject> =
        std::collections::HashMap::new();
    for o in &objects {
        newest
            .entry(o.id.as_str())
            .and_modify(|cur| {
                if (o.ts, o.key.as_str()) > (cur.ts, cur.key.as_str()) {
                    *cur = o;
                }
            })
            .or_insert(o);
    }
    let mut candidates: Vec<&QuotaObject> = objects
        .iter()
        .filter(|o| o.key != just_uploaded)
        .filter(|o| newest.get(o.id.as_str()).map_or(true, |n| n.key != o.key))
        .collect();
    // oldest first; the key breaks a timestamp tie so the pick is stable
    candidates.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.key.cmp(&b.key)));
    let mut delete = Vec::new();
    let mut remaining = used;
    for c in candidates {
        if remaining <= max_bytes {
            break;
        }
        remaining = remaining.saturating_sub(c.size);
        delete.push(c.key.clone());
    }
    (delete, remaining)
}

/// A dir the auto-backup path must NOT ship: a chainless (legacy,
/// pre-chain) workspace has no `chain.state`, so its export carries no
/// verifiable chain and the restore path rejects it outright
/// (`lifecycles.rs::cmd_net_restore_staged`). Returns the honest reason, or
/// `None` when the dir is backup-able.
fn backup_refusal_reason(dir: &std::path::Path) -> Option<&'static str> {
    if !dir.join("chain.state").exists() {
        return Some(
            "no persistent chain — a backup of this workspace could not be \
             restored (chainless legacy directory)",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{prune_candidates, quota_candidates, QuotaObject};

    fn key(ts: u64) -> String {
        let id = "ab".repeat(32);
        molt_core::backup_key(&id, ts)
    }

    /// A listed backup object of workspace `n`, timestamp `ts`, `size` bytes.
    fn obj(n: u8, ts: u64, size: u64) -> QuotaObject {
        let id = format!("{n:02x}").repeat(32);
        QuotaObject {
            key: molt_core::backup_key(&id, ts),
            id,
            ts,
            size,
        }
    }

    #[test]
    fn prune_keeps_the_newest_and_deletes_the_oldest() {
        let keys = vec![key(1), key(2), key(3), key(4), key(5)];
        let just = key(5);
        let del = prune_candidates(keys, 3, &just);
        // 5 keys, keep 3 (excluding the just-uploaded newest) → delete oldest 2
        assert_eq!(del, vec![key(1), key(2)]);
    }

    #[test]
    fn the_just_uploaded_key_is_never_pruned_under_clock_regression() {
        // the clock regressed: our fresh object's ts (5) is SMALLER than the
        // existing generations (10..12), so lexicographically it sorts as the
        // "oldest" — yet it is the copy the confirmation is about and must
        // never be deleted.
        let just = key(5);
        let keys = vec![key(10), key(11), key(12), just.clone()];
        let del = prune_candidates(keys, 2, &just);
        assert!(
            !del.contains(&just),
            "the just-confirmed upload is never a prune candidate: {del:?}"
        );
        // of the OTHER three, keep 2 newest → only the oldest other (10) goes
        assert_eq!(del, vec![key(10)]);
    }

    #[test]
    fn a_quota_of_zero_means_no_limit() {
        let objects = vec![obj(1, 1, 1_000), obj(1, 2, 1_000), obj(2, 1, 1_000)];
        let (del, used) = quota_candidates(objects, 0, "");
        assert!(del.is_empty(), "0 = no limit, nothing is pruned: {del:?}");
        assert_eq!(used, 3_000);
    }

    #[test]
    fn under_the_quota_nothing_is_pruned() {
        let objects = vec![obj(1, 1, 100), obj(1, 2, 100)];
        let (del, used) = quota_candidates(objects, 1_000, "");
        assert!(del.is_empty(), "{del:?}");
        assert_eq!(used, 200);
    }

    #[test]
    fn over_the_quota_the_oldest_go_first_and_it_stops_once_it_fits() {
        // three generations of one workspace, 100 bytes each, plus a second
        // workspace's single copy = 400 bytes. Quota 350 → one 100er goes.
        let objects = vec![
            obj(1, 1, 100),
            obj(1, 2, 100),
            obj(1, 3, 100),
            obj(2, 9, 100),
        ];
        let just = molt_core::backup_key(&"01".repeat(32), 3);
        let (del, used) = quota_candidates(objects, 350, &just);
        assert_eq!(del, vec![molt_core::backup_key(&"01".repeat(32), 1)]);
        assert_eq!(used, 300, "one 100-byte object deleted from 400");
    }

    #[test]
    fn age_is_the_timestamp_not_the_key_order() {
        // workspace 02's copy is OLDER than workspace 01's, but sorts AFTER
        // it lexicographically (the key carries the id before the stamp). A
        // key-order pruner would delete the wrong one.
        let objects = vec![obj(1, 500, 100), obj(1, 501, 100), obj(2, 1, 100)];
        // 02 has only one copy → it is that workspace's newest and protected,
        // so the oldest DELETABLE object is 01@500.
        let (del, _) = quota_candidates(objects, 100, "");
        assert_eq!(del, vec![molt_core::backup_key(&"01".repeat(32), 500)]);
    }

    #[test]
    fn the_newest_copy_of_every_workspace_survives_the_quota() {
        // far over quota, but each workspace's newest copy is untouchable —
        // a byte limit must never leave a republic with zero backups.
        let objects = vec![obj(1, 1, 100), obj(1, 2, 100), obj(2, 1, 100), obj(2, 2, 100)];
        let (del, used) = quota_candidates(objects, 1, "");
        assert_eq!(
            del,
            vec![
                molt_core::backup_key(&"01".repeat(32), 1),
                molt_core::backup_key(&"02".repeat(32), 1),
            ]
        );
        assert_eq!(used, 200, "the two newest copies stay, quota or not");
    }

    #[test]
    fn the_just_uploaded_object_is_never_a_quota_candidate() {
        // clock regression again: the fresh upload sorts oldest by timestamp
        // and would be the first byte-quota victim.
        let just = molt_core::backup_key(&"01".repeat(32), 5);
        let objects = vec![obj(1, 5, 100), obj(1, 10, 100), obj(1, 11, 100)];
        let (del, _) = quota_candidates(objects, 1, &just);
        assert!(!del.contains(&just), "{del:?}");
    }

    #[test]
    fn an_unfittable_quota_deletes_what_it_can_and_reports_the_remainder() {
        let objects = vec![obj(1, 1, 100), obj(1, 2, 100), obj(2, 1, 100)];
        let (del, used) = quota_candidates(objects, 50, "");
        assert_eq!(del, vec![molt_core::backup_key(&"01".repeat(32), 1)]);
        assert_eq!(used, 200, "still over 50 — only the newest copies remain");
    }
}
