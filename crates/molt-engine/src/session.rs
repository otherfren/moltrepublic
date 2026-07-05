// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared app/session state: navigation, language, theme, settings and the
//! locally known workspaces. All of it is co-equal — the GUI and MCP drive
//! the same commands and mirror the same session.
//!
//! The workspace verbs are storage-backed on a persistent engine
//! ([`crate::spawn_with_config`] / [`crate::spawn_with_storage`]): open
//! loads snapshot + log tail through the event applier, close writes a
//! closing snapshot and releases the LOCK, delete moves the directory to
//! the recoverable `.trash`. On a storage-less engine ([`crate::spawn`])
//! they keep the original session-only behavior.

use std::path::PathBuf;

use molt_core::{
    roster_members, MoltError, Reply, Screen, SessionScope, SessionSettings, Surface,
    WorkspaceEvent, WorkspaceId, WorkspaceInfo,
};

use crate::{ActiveStorage, State};

impl State {
    pub(crate) fn cmd_navigate(&mut self, screen: Screen) -> Result<Reply, MoltError> {
        self.session.screen = screen;
        self.session.notice = String::new();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_select_surface(&mut self, surface: Surface) -> Result<Reply, MoltError> {
        self.session.surface = surface;
        self.session.view = surface.default_view().to_string();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_select_view(
        &mut self,
        surface: Surface,
        view: String,
    ) -> Result<Reply, MoltError> {
        if !surface.views().iter().any(|(k, _)| *k == view) {
            return Err(MoltError::UnknownView(surface, view));
        }
        self.session.surface = surface;
        self.session.view = view;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_set_language(&mut self, lang: String) -> Result<Reply, MoltError> {
        self.session.language = lang;
        self.persist_settings(false);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_set_theme(&mut self, theme: String) -> Result<Reply, MoltError> {
        self.session.theme = theme;
        self.persist_settings(false);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_save_settings(
        &mut self,
        settings: SessionSettings,
    ) -> Result<Reply, MoltError> {
        validate_settings(&settings)?;
        self.session.settings = settings;
        self.mark_restart_required();
        if self.store.is_some() {
            // The reply must not wait for the disk; the write outcome comes
            // back as a ConfigNotice ("saved" / "save-failed: …").
            self.session.notice = String::new();
            self.persist_settings(true);
        } else {
            // No config file attached (tests, ephemeral node): session-only.
            self.session.notice = "saved".to_string();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Mirror an externally edited `config.toml` into the shared session.
    /// Applied exactly like a save minus the persist step (the file already
    /// holds these values); invalid values are rejected back to the watcher,
    /// which keeps the last good state and raises `config-conflict`.
    pub(crate) fn cmd_reload_settings(
        &mut self,
        settings: SessionSettings,
        language: String,
        theme: String,
    ) -> Result<Reply, MoltError> {
        validate_settings(&settings)?;
        if self.session.settings == settings
            && self.session.language == language
            && self.session.theme == theme
        {
            return Ok(Reply::Ack); // no visible change, no notice churn
        }
        self.session.settings = settings;
        self.session.language = language;
        self.session.theme = theme;
        self.mark_restart_required();
        self.session.notice = "config-reloaded".to_string();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Surface a config-persistence outcome (sent by the ConfigStore task).
    pub(crate) fn cmd_config_notice(&mut self, notice: String) -> Result<Reply, MoltError> {
        self.session.notice = notice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Queue the current session settings for persistence (no-op without a
    /// config store). `notify` puts the outcome into the session notice.
    fn persist_settings(&mut self, notify: bool) {
        if let Some(store) = &self.store {
            store.persist(
                crate::configstore::file_settings(
                    &self.session.settings,
                    &self.session.language,
                    &self.session.theme,
                ),
                notify,
            );
        }
    }

    /// Compare the live settings against the boot snapshot and record which
    /// restart-required keys changed — shared session state, so the GUI can
    /// warn persistently and an MCP agent sees the same list. Keys are the
    /// config-file names. Hot keys (workspace/S3/UI) never appear here;
    /// `mcp.allow` / `mcp.token` move off this list once live rotation of the
    /// MCP acceptor lands (concept-config-bidirection §3.5, C4).
    fn mark_restart_required(&mut self) {
        let boot = &self.boot_settings;
        let live = &self.session.settings;
        let mut keys = Vec::new();
        if live.headless != boot.headless {
            keys.push("node.headless".to_string());
        }
        if live.mcp_port != boot.mcp_port {
            keys.push("mcp.port".to_string());
        }
        if live.mcp_allow != boot.mcp_allow {
            keys.push("mcp.allow".to_string());
        }
        if live.mcp_token != boot.mcp_token {
            keys.push("mcp.token".to_string());
        }
        if live.anonymity != boot.anonymity {
            keys.push("transport.anonymity.network".to_string());
        }
        if live.tor_mode != boot.tor_mode {
            keys.push("transport.anonymity.tor.mode".to_string());
        }
        if live.tor_port != boot.tor_port {
            keys.push("transport.anonymity.tor.port".to_string());
        }
        self.session.restart_required = keys;
    }

    /// The resolved workspace root (`storage.workspace_dir`, `~` expanded).
    pub(crate) fn workspace_root(&self) -> PathBuf {
        molt_storage::expand_tilde(&self.session.settings.workspace_dir)
    }

    pub(crate) fn cmd_open_workspace(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        if !self.session.workspaces.iter().any(|w| w.id == id) {
            return Err(MoltError::UnknownWorkspace(id));
        }
        // reopening the already-open workspace is a navigation no-op — a
        // second open would collide with our own flock and report Busy
        let already_open = self.active.as_ref().is_some_and(|a| a.id == id);
        if self.persist && !already_open {
            self.open_stored_workspace(&id)?;
        }
        // the transport context changes with the workspace: tear the old
        // mesh down and stand the new one up right away (presence pills
        // are live before the first chat; persisted opens no-op — their
        // seats are real and empty until T2)
        self.teardown_net();
        self.session.active_workspace = id;
        self.ensure_demo_net();
        self.session.screen = Screen::Main;
        self.session.notice = String::new();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Load a workspace from disk into the actor: LOCK, snapshot + tail
    /// through the event applier, then hand the append side to a writer
    /// task. Every validation runs *before* the previously open workspace
    /// is torn down — any failure leaves it untouched (the freshly taken
    /// LOCK releases when `opened` drops on the error paths).
    fn open_stored_workspace(&mut self, id: &str) -> Result<(), MoltError> {
        let root = self.workspace_root();
        let dir = molt_storage::find_workspace_dir(&root, id).ok_or_else(|| {
            MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            ))
        })?;
        let (opened, loaded) =
            molt_storage::open_workspace(&dir).map_err(molt_storage::StorageError::into_molt)?;
        if loaded.unknown_events > 0 {
            return Err(MoltError::Storage(format!(
                "{} event(s) were written by a newer version — update this \
                 node to open the workspace (writing with a partial history \
                 would fork it)",
                loaded.unknown_events
            )));
        }
        // the loaded history must carry its genesis: a snapshot only exists
        // after Founded was applied (it records the acting member), a bare
        // log must start with the Founded frame
        let has_genesis = match &loaded.snapshot {
            Some(snap) => !snap.state.member.is_empty(),
            None => matches!(
                loaded.tail.first(),
                Some(env) if env.seq == 1 && matches!(env.body, WorkspaceEvent::Founded { .. })
            ),
        };
        if !has_genesis {
            return Err(MoltError::Storage(
                "workspace history has no Founded genesis".to_string(),
            ));
        }

        // point of no return: swap the actor state to the new workspace
        self.close_active_storage();
        self.reset_workspace_state();
        if let Some(snap) = loaded.snapshot {
            self.restore_dump(snap.state);
        }
        for env in &loaded.tail {
            self.apply(env);
        }
        self.next_seq = opened.next_seq;
        let prefs = opened.prefs.clone();
        self.active = Some(ActiveStorage {
            id: id.to_string(),
            dir,
            prefs,
            handle: molt_storage::start_writer(opened),
        });
        // a crash may have separated an Approved frame from its Applied
        // frame; re-decide thresholds that were already met
        self.recover_pending_applies();
        self.refresh_active_entry();
        Ok(())
    }

    /// Mirror the replayed genesis identity into the session's list entry:
    /// the manifest copies feed the undecrypted Open screen, the event
    /// stream is the authority once the workspace is open.
    fn refresh_active_entry(&mut self) {
        let Some(replica) = self.replica.clone() else {
            return;
        };
        let Some(active) = &self.active else {
            return;
        };
        let Some(ws) = self
            .session
            .workspaces
            .iter_mut()
            .find(|w| w.id == active.id)
        else {
            return;
        };
        ws.name = replica.name;
        ws.detail = WorkspaceInfo::rule_detail(replica.rule_m, replica.roster.len());
        // members are a projection of the replayed roster — always rebuilt,
        // so a roster grown by MemberJoined never leaves a stale list
        ws.members = roster_members(&replica.roster, |m| m == replica.member, "not seen yet");
    }

    /// Flush + closing snapshot + LOCK release for the open workspace (if
    /// any), then drop its in-memory state. No-op on a session-only open.
    pub(crate) fn close_active_storage(&mut self) {
        if let Some(active) = self.active.take() {
            let snap = self.snapshot_now();
            active.handle.close(Some(snap));
            self.reset_workspace_state();
        }
    }

    pub(crate) fn cmd_close_workspace(&mut self) -> Result<Reply, MoltError> {
        self.teardown_net();
        self.close_active_storage();
        self.session.active_workspace = String::new();
        // back on the boot group: its mesh stands up for the next chat
        self.ensure_demo_net();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_set_workspace_backup(
        &mut self,
        id: WorkspaceId,
        enabled: bool,
    ) -> Result<Reply, MoltError> {
        let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) else {
            return Err(MoltError::UnknownWorkspace(id));
        };
        ws.s3 = enabled;
        if enabled {
            // enabling runs a first backup right away (the uploader itself
            // is milestone S5; the stamp keeps list and prefs consistent)
            ws.last_backup_min = 0;
        }
        if self.persist {
            self.persist_backup_pref(&id, enabled);
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Write the auto-backup switch into the workspace's `prefs.toml`. For
    /// the open workspace the engine-held copy is authoritative and the
    /// write goes through the writer task (one writer per directory —
    /// re-reading the file here would race the writer's queued updates);
    /// a closed workspace's file is read, patched and rewritten directly.
    pub(crate) fn persist_backup_pref(&mut self, id: &str, enabled: bool) {
        if let Some(a) = &mut self.active {
            if a.id == id {
                a.prefs.s3_backup = enabled;
                if enabled {
                    a.prefs.last_backup = Some(crate::now_secs());
                }
                a.handle.set_prefs(a.prefs.clone());
                return;
            }
        }
        let Some(dir) = molt_storage::find_workspace_dir(&self.workspace_root(), id) else {
            // the promise is persistence — a missing directory must not
            // look like success
            tracing::warn!(id, "backup pref not persisted: workspace directory missing");
            self.session.notice = "storage-failed".to_string();
            return;
        };
        let mut prefs = molt_storage::read_prefs(&dir);
        prefs.s3_backup = enabled;
        if enabled {
            prefs.last_backup = Some(crate::now_secs());
        }
        if let Err(e) = molt_storage::write_prefs(&dir, &prefs) {
            tracing::warn!(error = %e, "persisting backup pref failed");
            self.session.notice = "storage-failed".to_string();
        }
    }

    /// Record in the workspace's `prefs.toml` that its other members are
    /// in-process simulations (founded before the real network exists) —
    /// so a later open knows to run their loopback peer engines. Same
    /// writer-vs-direct discipline as [`Self::persist_backup_pref`].
    pub(crate) fn persist_simulated_members(&mut self, id: &str, simulated: bool) {
        if let Some(a) = &mut self.active {
            if a.id == id {
                a.prefs.simulated_members = simulated;
                a.handle.set_prefs(a.prefs.clone());
                return;
            }
        }
        let Some(dir) = molt_storage::find_workspace_dir(&self.workspace_root(), id) else {
            tracing::warn!(id, "simulated-members pref not persisted: directory missing");
            return;
        };
        let mut prefs = molt_storage::read_prefs(&dir);
        prefs.simulated_members = simulated;
        if let Err(e) = molt_storage::write_prefs(&dir, &prefs) {
            tracing::warn!(error = %e, "persisting simulated-members pref failed");
        }
    }

    pub(crate) fn cmd_delete_workspace(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        if !self.session.workspaces.iter().any(|w| w.id == id) {
            return Err(MoltError::UnknownWorkspace(id));
        }
        // deleting the open workspace closes it first (flush, LOCK release)
        // and immediately stops calling it open — even if the trash move
        // below fails, the session must not claim an open workspace whose
        // actor state is gone
        let mut dir = None;
        if self.active.as_ref().is_some_and(|a| a.id == id) {
            dir = self.active.as_ref().map(|a| a.dir.clone());
            self.close_active_storage();
            self.session.active_workspace = String::new();
        }
        if self.session.active_workspace == id {
            // a session-only workspace being deleted takes its mesh along
            self.teardown_net();
        }
        if self.persist {
            let root = self.workspace_root();
            let dir = dir.or_else(|| molt_storage::find_workspace_dir(&root, &id));
            if let Some(dir) = dir {
                if let Err(e) = molt_storage::trash_workspace(&root, &dir) {
                    // the entry stays listed (the dir still exists); the
                    // close above is already visible
                    self.emit_session(SessionScope::Full);
                    return Err(e.into_molt());
                }
            }
        }
        self.session.workspaces.retain(|w| w.id != id);
        if self.session.active_workspace == id {
            self.session.active_workspace = String::new();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }
}

/// Value validation shared by save and reload: nothing invalid reaches the
/// session or the file. (The MCP schema and the GUI widgets narrow most of
/// this already; the engine is the authority.)
fn validate_settings(s: &SessionSettings) -> Result<(), MoltError> {
    if !matches!(s.anonymity.as_str(), "tor" | "nym" | "none") {
        return Err(MoltError::Settings(format!(
            "anonymity network must be tor | nym | none, not `{}`",
            s.anonymity
        )));
    }
    if !matches!(s.tor_mode.as_str(), "local" | "embedded" | "whonix") {
        return Err(MoltError::Settings(format!(
            "tor mode must be local | embedded | whonix, not `{}`",
            s.tor_mode
        )));
    }
    if s.mcp_port == 0 {
        return Err(MoltError::Settings("mcp.port must not be 0".to_string()));
    }
    if s.anonymity == "tor" && s.tor_mode == "local" && s.tor_port == 0 {
        return Err(MoltError::Settings(
            "tor.port must not be 0 when mode = \"local\"".to_string(),
        ));
    }
    if s.workspace_dir.trim().is_empty() {
        return Err(MoltError::Settings(
            "workspace_dir must not be empty".to_string(),
        ));
    }
    Ok(())
}
