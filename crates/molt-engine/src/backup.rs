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
        // candidates: the per-workspace pref (mirrored in the entry) is on
        let candidates: Vec<WorkspaceId> = self
            .session
            .workspaces
            .iter()
            .filter(|w| w.s3 && !self.backup_inflight.contains(&w.id))
            .map(|w| w.id.clone())
            .collect();
        if candidates.is_empty() {
            return Ok(Reply::Ack);
        }
        // no configured target → nothing to do; the ticker stays silent
        // (the settings panel / backup table already surface the state, and
        // a per-minute notice would be spam, not honesty)
        let s = &self.session.settings;
        let Ok(config) = molt_net::s3::S3Config::from_settings(
            &s.s3_endpoint,
            &s.s3_access_key,
            &s.s3_secret_key,
            &s.s3_bucket,
        ) else {
            return Ok(Reply::Ack);
        };
        // clamp to the tick period: interval 0 must not mean "a full blob
        // upload every single tick, forever"
        let interval_secs = u64::from(s.s3_interval_min.max(1)) * 60;
        let now = now_secs();
        let root = self.workspace_root();
        let mut changed = false;
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
            // last completed backup: the engine-held prefs are authoritative
            // for the open workspace (the writer applies updates in order)
            let last_backup = match &self.active {
                Some(a) if a.id == id => a.prefs.last_backup,
                _ => molt_storage::read_prefs(&dir).last_backup,
            };
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
        let dialer = self
            .dialer_for()
            .map_err(|e| MoltError::Settings(e.to_string()))?;
        self.spawn_backup_task(id, dir, config, dialer);
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
        dialer: molt_net::smp::tls::Dialer,
    ) {
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return; // actor shutting down — no task, no inflight mark
        };
        self.backup_inflight.insert(id.clone());
        let root = self.workspace_root();
        let keep = usize::from(self.session.settings.s3_keep_copies.max(1));
        tokio::spawn(async move {
            let ts = now_secs();
            let build_dir = dir.clone();
            let build = tokio::task::spawn_blocking(move || {
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
                    let object = molt_core::backup_key(&id, ts);
                    let bytes = u64::try_from(blob.len()).unwrap_or(u64::MAX);
                    let client = molt_net::s3::S3Client::new(config, dialer);
                    match client.put_object(&object, &blob).await {
                        Ok(()) => {
                            // retention only AFTER the confirmed upload; a
                            // prune failure is surfaced, never blocks the
                            // backup (the next success re-prunes)
                            let prune_error = prune_old_copies(&client, &id, keep).await;
                            Command::NetBackupDone {
                                id,
                                ts,
                                object,
                                bytes,
                                prune_error,
                            }
                        }
                        Err(e) => Command::NetBackupFailed {
                            id,
                            error: e.to_string(),
                        },
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
    ) -> Result<Reply, MoltError> {
        self.backup_inflight.remove(&id);
        self.stamp_backup_time(&id, ts);
        let minutes = u32::try_from(now_secs().saturating_sub(ts) / 60).unwrap_or(u32::MAX - 1);
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.last_backup_min = minutes;
            ws.backup_error = String::new();
        }
        tracing::info!(id, object, bytes, "backup uploaded");
        if !prune_error.is_empty() {
            tracing::warn!(id, error = %prune_error, "backup retention pruning failed");
            self.note_backup(format!("backup-prune-failed:{prune_error}"));
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
            tracing::warn!(error = %e, "persisting the backup stamp failed");
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
/// Returns `""` on success.
async fn prune_old_copies(
    client: &molt_net::s3::S3Client,
    id: &WorkspaceId,
    keep: usize,
) -> String {
    let prefix = format!("{}{id}/", molt_core::BACKUP_OBJECT_PREFIX);
    let listed = match client.list_objects(&prefix).await {
        Ok(objects) => objects,
        Err(e) => return format!("listing for retention failed: {e}"),
    };
    // only keys that follow the backup naming scheme for THIS workspace are
    // ours to prune — foreign objects under the prefix are left alone
    let mut keys: Vec<String> = listed
        .into_iter()
        .filter(|o| molt_core::parse_backup_key(&o.key).is_some_and(|(kid, _)| kid == *id))
        .map(|o| o.key)
        .collect();
    keys.sort_unstable();
    let excess = keys.len().saturating_sub(keep);
    let mut first_error = String::new();
    for key in &keys[..excess] {
        if let Err(e) = client.delete_object(key).await {
            if first_error.is_empty() {
                first_error = format!("deleting {key} failed: {e}");
            }
        }
    }
    first_error
}
