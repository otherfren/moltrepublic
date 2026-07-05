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
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_set_theme(&mut self, theme: String) -> Result<Reply, MoltError> {
        self.session.theme = theme;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_save_settings(
        &mut self,
        settings: SessionSettings,
    ) -> Result<Reply, MoltError> {
        // Mock: store the values in the session; nothing is written to disk.
        self.session.settings = settings;
        self.session.notice = "saved".to_string();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
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
