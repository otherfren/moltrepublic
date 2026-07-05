// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared app/session state: navigation, language, theme, settings and the
//! locally known workspaces. All of it is co-equal — the GUI and MCP drive
//! the same commands and mirror the same session.

use molt_core::{MoltError, Reply, Screen, SessionScope, SessionSettings, Surface};

use crate::State;

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

    pub(crate) fn cmd_open_workspace(&mut self, name: String) -> Result<Reply, MoltError> {
        if !self.session.workspaces.iter().any(|w| w.name == name) {
            return Err(MoltError::UnknownWorkspace(name));
        }
        self.session.active_workspace = name;
        self.session.screen = Screen::Main;
        self.session.notice = String::new();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_close_workspace(&mut self) -> Result<Reply, MoltError> {
        self.session.active_workspace = String::new();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_set_workspace_backup(
        &mut self,
        name: String,
        enabled: bool,
    ) -> Result<Reply, MoltError> {
        let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.name == name) else {
            return Err(MoltError::UnknownWorkspace(name));
        };
        ws.s3 = enabled;
        if enabled {
            // mock: enabling runs a first backup right away
            ws.last_backup_min = 0;
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_delete_workspace(&mut self, name: String) -> Result<Reply, MoltError> {
        let before = self.session.workspaces.len();
        self.session.workspaces.retain(|w| w.name != name);
        if self.session.workspaces.len() == before {
            return Err(MoltError::UnknownWorkspace(name));
        }
        if self.session.active_workspace == name {
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
