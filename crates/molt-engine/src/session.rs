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

/// A workspace directory's real on-disk size, clamped into the list
/// entry's `u32` KiB field. One recursive walk of a single directory —
/// called only at the quiescent entry choke points (materialize, open,
/// clean close), never per message and never on the org-apply path: a
/// wire-driven block apply must not stat-walk on the actor, and the
/// async writer may not have flushed the change yet anyway.
pub(crate) fn entry_size_kib(dir: &std::path::Path) -> u32 {
    u32::try_from(molt_storage::workspace_size_kib(dir)).unwrap_or(u32::MAX)
}

impl State {
    pub(crate) fn cmd_navigate(&mut self, screen: Screen) -> Result<Reply, MoltError> {
        // leaving an in-flight founding abandons it (the session is in-memory):
        // its recv loops must not seal a workspace and hijack the session — and
        // materialising it would even close the user's active workspace. This
        // also covers walking away while a post-seal mesh bootstrap is still
        // running (net_ritual is already gone by then, so check the bootstrap
        // too) — teardown_ritual reaps it.
        if screen != Screen::Create
            && (self.net_ritual.is_some() || self.founder_mesh_in.is_some())
        {
            self.teardown_ritual();
            self.ritual_attestations.clear();
            self.session.create = molt_core::CreateState::default();
        }
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

    /// Flip this node's read-receipts preference (a hot local pref — no
    /// restart). Silent persist to `config.toml`, then re-emit the session so
    /// the GUI reflects the new state (and, symmetrically, hides/shows others'
    /// receipts).
    pub(crate) fn cmd_set_read_receipts(&mut self, enabled: bool) -> Result<Reply, MoltError> {
        self.session.settings.read_receipts = enabled;
        self.persist_settings(false);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_save_settings(
        &mut self,
        settings: SessionSettings,
    ) -> Result<Reply, MoltError> {
        validate_settings(&settings)?;
        self.invalidate_backup_listing_on_target_change(&settings);
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
        self.invalidate_backup_listing_on_target_change(&settings);
        self.session.settings = settings;
        self.session.language = language;
        self.session.theme = theme;
        self.mark_restart_required();
        self.session.notice = "config-reloaded".to_string();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A changed backup target (endpoint/credentials/bucket) invalidates
    /// the backup table's bucket side: the orphan rows and the listing
    /// verdict described the OLD bucket, and an in-flight listing against
    /// it must not land either (the generation bump drops it). Shared by
    /// save and reload — the honest reset happens however the settings
    /// change.
    fn invalidate_backup_listing_on_target_change(&mut self, new: &SessionSettings) {
        let old = &self.session.settings;
        let changed = old.s3_endpoint != new.s3_endpoint
            || old.s3_access_key != new.s3_access_key
            || old.s3_secret_key != new.s3_secret_key
            || old.s3_bucket != new.s3_bucket;
        if !changed {
            return;
        }
        // the "Test connection" verdict described the OLD endpoint — a
        // changed target is unprobed until the next Test, so the UI must not
        // keep claiming the new endpoint is reachable ("ok") or unreachable.
        self.session.s3_test = String::new();
        // the orphan rows + listing verdict also described the OLD bucket;
        // an in-flight listing against it must not land either (the gen bump
        // drops it).
        if !(self.session.s3_list.is_empty() && self.session.backup_orphans.is_empty()) {
            self.s3_list_gen += 1;
            self.session.s3_list = String::new();
            self.session.backup_orphans.clear();
        }
    }

    /// Surface a config-persistence outcome (sent by the ConfigStore task).
    pub(crate) fn cmd_config_notice(&mut self, notice: String) -> Result<Reply, MoltError> {
        self.session.notice = notice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Test connectivity to an SMP server (the settings panel's Test button).
    /// Resolves the target (explicit `url`, else the configured custom or
    /// public server), marks the test in flight, and runs the real TLS
    /// handshake **off the actor** — the outcome returns as
    /// [`molt_core::Command::NetTestResult`] so the actor never blocks on the
    /// network.
    pub(crate) fn cmd_net_test_server(
        &mut self,
        url: String,
        anonymity: String,
        tor_mode: String,
        tor_port: u16,
    ) -> Result<Reply, MoltError> {
        let url = if url.trim().is_empty() {
            if self.session.settings.smp_server == "custom" {
                self.session.settings.smp_url.clone()
            } else {
                molt_config::default_public_smp()
            }
        } else {
            url
        };
        // parse in-actor so an obviously malformed URL fails fast
        let server = match molt_net::smp::SmpServer::parse(url.trim()) {
            Ok(s) => s,
            Err(e) => {
                self.session.smp_test = format!("error: {e}");
                self.emit_session(SessionScope::Full);
                return Ok(Reply::Ack);
            }
        };
        // route the probe through the resolved dialer (T4 §P7): over Tor it
        // uses the same routing the app will, and an onion-only target under a
        // Direct dialer reports "requires Tor" instead of leaking a clearnet
        // dial. A misconfigured Tor setting is itself the test failure.
        // Draft overrides (the settings form's live values) win over the
        // saved config, field-by-field — empty/0 falls back to saved, so a
        // bare MCP `test_smp_server` still probes the configured transport.
        let s = &self.session.settings;
        let anonymity = if anonymity.trim().is_empty() {
            s.anonymity.clone()
        } else {
            anonymity.trim().to_string()
        };
        let tor_mode = if tor_mode.trim().is_empty() {
            s.tor_mode.clone()
        } else {
            tor_mode.trim().to_string()
        };
        let tor_port = if tor_port == 0 { s.tor_port } else { tor_port };
        let dialer = match molt_net::smp::tls::Dialer::resolve(&anonymity, &tor_mode, tor_port) {
            Ok(dialer) => dialer,
            Err(e) => {
                self.session.smp_test = format!("error: {e}");
                self.emit_session(SessionScope::Full);
                return Ok(Reply::Ack);
            }
        };
        self.session.smp_test = "testing".to_string();
        self.emit_session(SessionScope::Full);
        if let Some(cmd_tx) = self.cmd_tx.upgrade() {
            tokio::spawn(async move {
                let result = match molt_net::smp::test_connection(&dialer, &server).await {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {e}"),
                };
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx
                    .send(crate::Envelope {
                        cmd: molt_core::Command::NetTestResult { result },
                        reply,
                    })
                    .await;
            });
        }
        Ok(Reply::Ack)
    }

    /// Record an SMP connection-test outcome into the session (fed back from
    /// the off-actor probe task).
    pub(crate) fn cmd_net_test_result(&mut self, result: String) -> Result<Reply, MoltError> {
        self.session.smp_test = result;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Test the configured S3 backup target (the backup panel's Test button).
    /// Empty fields fall back to the saved settings (the GUI passes its
    /// draft, an MCP call may pass nothing to test what is persisted); the
    /// config is validated in-actor so a missing/malformed endpoint fails
    /// fast, then the SigV4-signed `HEAD /bucket` probe runs **off the
    /// actor** through the resolved dialer — over Tor exactly like the SMP
    /// traffic when Tor is configured, and a misconfigured Tor setting is
    /// itself the test failure (fail-closed, T4 §P7). The outcome returns as
    /// [`molt_core::Command::NetTestS3Result`].
    pub(crate) fn cmd_net_test_s3(
        &mut self,
        endpoint: String,
        access_key: String,
        secret_key: String,
        bucket: String,
    ) -> Result<Reply, MoltError> {
        let s = &self.session.settings;
        let or_saved = |v: String, saved: &str| if v.trim().is_empty() { saved.to_string() } else { v };
        let endpoint = or_saved(endpoint, &s.s3_endpoint);
        let access_key = or_saved(access_key, &s.s3_access_key);
        let secret_key = or_saved(secret_key, &s.s3_secret_key);
        let bucket = or_saved(bucket, &s.s3_bucket);
        let config =
            match molt_net::s3::S3Config::from_settings(&endpoint, &access_key, &secret_key, &bucket)
            {
                Ok(c) => c,
                Err(e) => {
                    self.session.s3_test = format!("error: {e}");
                    self.emit_session(SessionScope::Full);
                    return Ok(Reply::Ack);
                }
            };
        let dialer = match self.dialer_for() {
            Ok(dialer) => dialer,
            Err(e) => {
                self.session.s3_test = format!("error: {e}");
                self.emit_session(SessionScope::Full);
                return Ok(Reply::Ack);
            }
        };
        self.session.s3_test = "testing".to_string();
        self.emit_session(SessionScope::Full);
        if let Some(cmd_tx) = self.cmd_tx.upgrade() {
            tokio::spawn(async move {
                let client = molt_net::s3::S3Client::new(config, dialer);
                let result = match client.probe_bucket().await {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {e}"),
                };
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx
                    .send(crate::Envelope {
                        cmd: molt_core::Command::NetTestS3Result { result },
                        reply,
                    })
                    .await;
            });
        }
        Ok(Reply::Ack)
    }

    /// Record an S3 probe outcome into the session (fed back from the
    /// off-actor probe task).
    pub(crate) fn cmd_net_test_s3_result(&mut self, result: String) -> Result<Reply, MoltError> {
        self.session.s3_test = result;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// List the configured bucket's backup objects (the settings backup
    /// table's refresh, mock_todo story 8). Always driven by the SAVED
    /// settings — the table reflects the configured backup target, not a
    /// draft. The config is validated in-actor (no backup target configured
    /// fails fast with an honest note and an EMPTY orphan table — never
    /// invented rows), then the SigV4-signed ListObjectsV2 under the
    /// `molt/` prefix (`backup_restore_design.md` §6.2) runs **off the
    /// actor** through the resolved dialer — fail-closed like every dial.
    /// The outcome returns as [`molt_core::Command::NetListBackupsResult`].
    pub(crate) fn cmd_net_list_backups(&mut self) -> Result<Reply, MoltError> {
        let s = &self.session.settings;
        let target = molt_net::s3::S3Config::from_settings(
            &s.s3_endpoint,
            &s.s3_access_key,
            &s.s3_secret_key,
            &s.s3_bucket,
        )
        .map_err(|e| e.to_string())
        .and_then(|config| {
            let dialer = self.dialer_for().map_err(|e| e.to_string())?;
            Ok((config, dialer))
        });
        let (config, dialer) = match target {
            Ok(pair) => pair,
            Err(e) => {
                // honest empty state: an unusable target lists nothing
                self.s3_list_gen += 1; // a stale in-flight result must not resurrect rows
                self.session.s3_list = format!("error: {e}");
                self.session.backup_orphans.clear();
                self.emit_session(SessionScope::Full);
                return Ok(Reply::Ack);
            }
        };
        // "listing" is only shown once the task really runs — a failed
        // upgrade (actor shutting down) must not wedge the state
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Ok(Reply::Ack);
        };
        self.s3_list_gen += 1;
        let generation = self.s3_list_gen;
        self.session.s3_list = "listing".to_string();
        self.emit_session(SessionScope::Full);
        tokio::spawn(async move {
            let client = molt_net::s3::S3Client::new(config, dialer);
            let (result, objects) = match client
                .list_objects(molt_core::BACKUP_OBJECT_PREFIX)
                .await
            {
                Ok(listed) => (
                    "ok".to_string(),
                    listed
                        .into_iter()
                        .map(|o| molt_core::BackupObject {
                            key: o.key,
                            size: o.size,
                            modified: o.modified,
                        })
                        .collect(),
                ),
                Err(e) => (format!("error: {e}"), Vec::new()),
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx
                .send(crate::Envelope {
                    cmd: molt_core::Command::NetListBackupsResult {
                        result,
                        objects,
                        generation: Some(generation),
                    },
                    reply,
                })
                .await;
        });
        Ok(Reply::Ack)
    }

    /// Record a bucket-listing outcome into the session (fed back from the
    /// off-actor listing task): classify the objects against the locally
    /// known workspaces — entries with no local counterpart become the REAL
    /// `backup_orphans` (foreign keys survive as unknown entries); on
    /// failure the table shows no bucket rows at all rather than stale or
    /// invented ones. A stale generation (an older request resolving after
    /// a newer one, or after the backup target changed) is dropped.
    pub(crate) fn cmd_net_list_backups_result(
        &mut self,
        result: String,
        objects: Vec<molt_core::BackupObject>,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.s3_list_gen) {
            return Ok(Reply::Ack);
        }
        if result == "ok" {
            let local_ids: Vec<String> = self
                .session
                .workspaces
                .iter()
                .map(|w| w.id.clone())
                .collect();
            self.session.backup_orphans =
                molt_core::backup_orphans_from_listing(&objects, &local_ids, crate::now_secs());
            // reconcile the LOCAL rows' bucket-side cells with the real
            // listing: how many backup copies of each local workspace the
            // bucket actually holds (0 = none seen — never invented)
            for ws in &mut self.session.workspaces {
                ws.backup_copies = u32::try_from(
                    objects
                        .iter()
                        .filter(|o| {
                            molt_core::parse_backup_key(&o.key)
                                .is_some_and(|(id, _)| id == ws.id)
                        })
                        .count(),
                )
                .unwrap_or(u32::MAX);
            }
        } else {
            // a failed listing knows nothing about the bucket — no rows,
            // no per-workspace copy counts
            self.session.backup_orphans.clear();
            for ws in &mut self.session.workspaces {
                ws.backup_copies = 0;
            }
        }
        self.session.s3_list = result;
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

    /// The config→dialer bridge (T4 §P1): resolve the SMP dialer from the live
    /// anonymity settings. `network=none`→Direct, `network=tor`→SOCKS/arti,
    /// and every misconfiguration (`embedded` without the feature, unknown
    /// mode, `nym`) is a fail-closed [`molt_net::NetError::TorMisconfigured`] —
    /// never a silent clearnet fallback.
    pub(crate) fn dialer_for(&self) -> Result<molt_net::smp::tls::Dialer, molt_net::NetError> {
        let s = &self.session.settings;
        molt_net::smp::tls::Dialer::resolve(&s.anonymity, &s.tor_mode, s.tor_port)
    }

    /// The display label for the EFFECTIVE global anonymity network — what
    /// `WorkspaceInfo.net` / `CreateState.net` show. Derived from the same
    /// settings [`Self::dialer_for`] resolves, never from a client-supplied
    /// string (tor_transport_implementation.md §P8). The normalization is
    /// the shared [`molt_core::effective_net_label`].
    pub(crate) fn effective_net_label(&self) -> String {
        molt_core::effective_net_label(&self.session.settings.anonymity).to_string()
    }

    /// Resolve the dialer for a flow about to open SMP connections, updating
    /// the transport-health pill (T4 §P6): success clears it to `Ok`, a
    /// fail-closed error sets it `Down` with the reason and returns that reason
    /// so the caller aborts the flow (fail-closed). Does **not** emit — the
    /// caller's own `emit_session` carries the new health state.
    pub(crate) fn resolve_dialer(&mut self) -> Result<molt_net::smp::tls::Dialer, String> {
        match self.dialer_for() {
            Ok(dialer) => {
                self.session.net_health = molt_core::NetHealth::Ok;
                Ok(dialer)
            }
            Err(e) => {
                let reason = e.to_string();
                self.session.net_health = molt_core::NetHealth::Down {
                    reason: reason.clone(),
                };
                Err(reason)
            }
        }
    }

    pub(crate) fn cmd_open_workspace(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        if !self.session.workspaces.iter().any(|w| w.id == id) {
            return Err(MoltError::UnknownWorkspace(id));
        }
        // an at-rest-encrypted workspace is inactive until decrypted
        if self.session.workspaces.iter().any(|w| w.id == id && w.encrypted) {
            return Err(MoltError::WorkspaceEncrypted(id));
        }
        // reopening the already-open workspace is a navigation no-op — a
        // second open would collide with our own flock and report Busy, and a
        // running mesh (real or demo) must not be torn down under it
        if self.active.as_ref().is_some_and(|a| a.id == id) {
            self.session.active_workspace = id;
            self.session.screen = Screen::Main;
            self.session.notice = String::new();
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }
        // clean close of the workspace we are leaving: persist its running mesh
        // crypto so IT resumes if reopened later
        self.persist_net_crypto_on_close();
        let transport_state = if self.persist {
            self.open_stored_workspace(&id)?
        } else {
            molt_core::TransportState::default()
        };
        // the transport context changes with the workspace: tear the old mesh
        // down and abandon any still-running founder bootstrap for the workspace
        // we are leaving (its ready would be ws-id-rejected anyway; this reaps
        // the task)
        self.teardown_net();
        self.founder_mesh_in = None;
        self.runtime_transport = None;
        self.session.active_workspace = id;
        // RESUME the real mesh from the clean-close snapshot: re-adopt the queue
        // credentials into a fresh transport (recv keys + secured sender keys, so
        // it can subscribe AND send) and load the advanced MLS ratchet. A
        // workspace never cleanly closed (crash, or founded on another node) has
        // no `smp_queues` → no mesh (offline until a recovery/rejoin; only the
        // demo-mesh test seam stands simulated peers up here).
        // fail-closed resume: resolve the dialer first. A misconfigured Tor
        // setting sets the health pill Down and skips the real mesh rather
        // than resuming over an unintended clearnet path.
        let dialer = self.resolve_dialer().ok();
        let resumed = match (&transport_state.mls, &transport_state.smp_queues, &dialer) {
            (Some(mls), Some(creds), Some(dialer)) if !transport_state.mesh.is_empty() => {
                // the reopen seam (tests): a transport on the still-running
                // loopback hub replaces the fresh-SmpTransport build — same
                // import contract
                let transport = if let Some(seam) = self.reopen_seam.clone() {
                    molt_net::Transport::import_creds(&seam, creds);
                    Some(seam)
                } else {
                    crate::founding::reopen_transport(&transport_state.mesh, creds, dialer.clone())
                };
                // DIAGNOSTIC (MOLT_MESH_PROBE): instead of standing up the real
                // mesh, run a raw per-leg SMP self-test to tell server-side queue
                // expiry apart from a moltrepublic resume/delivery bug on a
                // workspace that reopens deaf. Replaces the mesh for this session
                // (one subscription per queue). See `crate::probe`.
                if crate::probe::armed() {
                    if let Some(t) = transport {
                        crate::probe::spawn_mesh_probe(
                            t,
                            transport_state.mesh.clone(),
                            self.member(),
                        );
                    }
                    None
                } else {
                    transport.and_then(|t| self.build_real_net(t, &transport_state.mesh, mls))
                }
            }
            _ => None,
        };
        let resumed_real = resumed.is_some();
        if let Some(net) = resumed {
            self.net = Some(net);
        } else {
            self.ensure_demo_net();
        }
        self.session.screen = Screen::Main;
        // the honest OFFLINE state (2026-07-19 incident): a workspace whose
        // transport.state carries real-mesh evidence (MLS/creds/links) but
        // whose mesh did NOT resume must never look healthy — net_health
        // goes Down with the exact gap, and the persistent "detached"
        // notice is set. A dialer failure keeps its own fail-closed Down
        // reason from resolve_dialer (the workspace is not detached —
        // fixing the setting + reopening resumes).
        let offline = if resumed_real || !self.persist || dialer.is_none() {
            None
        } else if transport_state.mls.is_some()
            || transport_state.smp_queues.is_some()
            || !transport_state.mesh.is_empty()
        {
            Some(if transport_state.smp_queues.is_none() {
                "offline: no queue credentials on disk — the mesh cannot resume on \
                 this seat (hard shutdown before the mesh came up, or a pre-fix \
                 build); local reads/writes work, nothing reaches the peers; rejoin \
                 via a recovery link"
            } else if transport_state.mls.is_none() {
                "offline: no MLS group snapshot on disk — the mesh cannot resume; \
                 rejoin via a recovery link"
            } else if transport_state.mesh.is_empty() {
                "offline: no mesh links on disk — the mesh cannot resume; rejoin \
                 via a recovery link"
            } else {
                "offline: resuming the persisted mesh failed — local reads/writes \
                 work, nothing reaches the peers"
            })
        } else {
            None
        };
        self.session.notice = if let Some(reason) = offline {
            self.session.net_health = molt_core::NetHealth::Down {
                reason: reason.to_string(),
            };
            "detached".to_string()
        } else if self.persist
            && self.chain_head.is_some()
            && transport_state.mls.is_none()
            && transport_state.mesh.is_empty()
            && transport_state.smp_queues.is_none()
        {
            // the honest DETACHED state (backup_restore_design.md §4.4): an
            // imported workspace carries a verified chain but deliberately NO
            // live crypto — no MLS snapshot, no mesh links, no queue creds
            // (never exported, §3.3). Reading works; the mesh does not come
            // up; membership comes back via the recovery ritual, and the
            // notice says exactly that instead of pretending a healthy mesh.
            "detached".to_string()
        } else {
            String::new()
        };
        // diagnostics: make it unmistakable that the mesh is intentionally not
        // running (the probe replaced it), so the offline banner isn't mistaken
        // for the bug under investigation.
        if crate::probe::armed() {
            self.session.notice =
                "mesh-probe: diagnostics only — the real mesh is NOT running; read the \
                 molt_mesh_probe log"
                    .to_string();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Load a workspace from disk into the actor: LOCK, snapshot + tail
    /// through the event applier, then hand the append side to a writer
    /// task. Every validation runs *before* the previously open workspace
    /// is torn down — any failure leaves it untouched (the freshly taken
    /// LOCK releases when `opened` drops on the error paths).
    fn open_stored_workspace(&mut self, id: &str) -> Result<molt_core::TransportState, MoltError> {
        let root = self.workspace_root();
        let dir = molt_storage::find_workspace_dir(&root, id).ok_or_else(|| {
            MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            ))
        })?;
        let (mut opened, loaded) =
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
        // read the persisted transport state (MLS group + mesh + queue creds)
        // and the persistent commit-block chain NOW, while we still hold
        // `opened` directly — after start_writer only the async load path
        // remains, which the sync open handler can't await
        let transport_state = opened.read_transport_state();
        let (checkpoint_blob, chain) = opened.read_chain();
        // heal the crash window between a pruned chain write and its
        // manifest bump: a pruned workspace must never sit at v1
        if checkpoint_blob.is_some() {
            if let Err(e) = opened.bump_pruned_version() {
                tracing::warn!(error = %e, "manifest bump on reopen failed");
            }
        }
        self.active = Some(ActiveStorage {
            id: id.to_string(),
            dir,
            prefs,
            handle: molt_storage::start_writer(opened),
        });
        // adopt + verify the persistent chain (re-projects the gated surfaces
        // from it) and restore the runtime identity signing key so this node
        // can keep signing governance approvals after a reopen. This runs
        // BEFORE recover_pending_applies so the legacy threshold recovery knows
        // it is a chain workspace and stays out of the way.
        if !chain.is_empty() {
            // WP4b: a pruned holder re-anchors on its persisted blob BEFORE
            // adopting — verify_own then runs the suffix rules
            self.checkpoint_blob = checkpoint_blob;
            self.adopt_chain(chain);
        }
        self.identity_sk = transport_state
            .identity_sk
            .as_deref()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .map(|arr| molt_storage::SigningKey::from_bytes(&arr));
        // a crash may have separated an Approved frame from its Applied frame;
        // re-decide thresholds that were already met (legacy path only)
        self.recover_pending_applies();
        self.note_governance_readiness();
        // pull any blocks committed while we were away — a broadcast request the
        // outbox delivers once the resumed mesh connects; any survivor re-serves
        // its chain suffix (no-op when we are already current)
        if let Some(height) = self.chain_head.as_ref().map(|h| h.height) {
            self.request_catchup(height + 1);
        }
        self.refresh_active_entry();
        // the size is stamped outside refresh_active_entry on purpose: at
        // open the directory is quiescent (nothing queued on the writer yet)
        // so the walk is exact; the org-apply refresh path skips it
        if let Some((id, dir)) = self
            .active
            .as_ref()
            .map(|a| (a.id.clone(), a.dir.clone()))
        {
            self.set_entry_size(&id, &dir);
        }
        // rebuild the logo file from the replayed log if it went missing
        // (crash, restore) — deterministic, the bytes live in the payload
        self.sync_logo_file();
        // re-adopt MY shares' source paths from prefs, filtered to shares
        // that replayed as mine and still available — this node keeps
        // serving downloads across restarts
        self.adopt_share_paths();
        Ok(transport_state)
    }

    /// Load `prefs.shared_files` into the runtime share-path map, dropping
    /// entries whose share no longer exists in the replayed log, is not
    /// ours, or was removed (their serve would be refused anyway).
    fn adopt_share_paths(&mut self) {
        let entries: Vec<(String, String)> = match &self.active {
            Some(active) => active
                .prefs
                .shared_files
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            None => return,
        };
        let me = self.member();
        for (id_hex, path) in entries {
            let Ok(id) = id_hex.parse::<molt_core::MessageId>() else {
                continue;
            };
            let Ok((_, msg)) = self.chat_by_id(&id) else {
                continue;
            };
            if msg.from == me && msg.file.as_ref().is_some_and(|f| f.available) {
                self.share_paths.insert(id, std::path::PathBuf::from(path));
            }
        }
    }

    /// Mirror the replayed identity into the session's list entry: the
    /// manifest copies feed the undecrypted Open screen, the event stream
    /// is the authority once the workspace is open. Name and agenda are
    /// the EFFECTIVE values (genesis folded with the applied Organization
    /// ops) — an applied `set_name`/`set_charter` shows up everywhere.
    pub(crate) fn refresh_active_entry(&mut self) {
        let Some(replica) = self.replica.clone() else {
            return;
        };
        let eff = self.org_effective();
        let now = self.presence_now();
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
        ws.name = eff.name;
        ws.detail = WorkspaceInfo::rule_detail(replica.rule_m, replica.roster.len());
        ws.agenda = eff.agenda;
        // members are a projection of the replayed roster — always rebuilt,
        // so a roster grown by MemberJoined never leaves a stale list. The
        // rebuild PRESERVES the real last-seen stamps: the local member is
        // trivially present, everyone else keeps their stamp (a member new
        // to the roster honestly starts never-seen).
        let prev = std::mem::take(&mut ws.members);
        ws.members = roster_members(&replica.roster, now, |m| {
            if m == replica.member {
                now
            } else {
                prev.iter()
                    .find(|p| p.name == m)
                    .map(|p| p.last_seen)
                    .unwrap_or(molt_core::MemberInfo::NEVER)
            }
        });
        // a send-failure pin survives the rebuild until the next sighting
        for m in &mut ws.members {
            if self.net_unreachable.contains(&m.name) {
                m.state = 2;
            }
        }
    }

    /// An Organization change was applied: ripple the effective identity
    /// into every mirror that shows it — the session entry (header + Open
    /// list), the plaintext manifest on disk (the undecrypted Open-screen
    /// scan must agree after a restart), the materialized logo file and
    /// the session broadcast.
    pub(crate) fn after_org_applied(&mut self) {
        self.refresh_active_entry();
        if let Some(active) = &self.active {
            let name = self.org_effective().name;
            if !name.is_empty() {
                active.handle.set_display_name(name);
            }
        }
        self.sync_logo_file();
        self.emit_session(SessionScope::Full);
    }

    /// Reconcile the workspace's `logo.<ext>` file with the applied
    /// Organization log: the last applied `set_image`'s embedded bytes are
    /// what the file must hold, an applied `remove_image` (or no image op
    /// at all) means no file. Idempotent — also run at open, so a crashed
    /// or restored workspace rebuilds the logo deterministically from the
    /// log. The write itself happens on the storage writer thread.
    pub(crate) fn sync_logo_file(&self) {
        let Some(active) = &self.active else {
            return;
        };
        // scan the BORROWED entries in reverse for the last image op — decode
        // exactly one payload (not every historical set_image), and only when
        // it is actually the current image
        let mut want: Option<(String, Vec<u8>)> = None;
        for v in self.applied_org_entries().collect::<Vec<_>>().into_iter().rev() {
            match v.get("op").and_then(serde_json::Value::as_str) {
                Some("remove_image") => break, // the current image was cleared
                Some("set_image") => {
                    let value =
                        v.get("value").and_then(serde_json::Value::as_str).unwrap_or_default();
                    if let Some(bytes) = crate::proposals::image_bytes(v) {
                        want = Some((crate::proposals::logo_ext(value), bytes));
                    }
                    break; // the last image op wins
                }
                _ => {}
            }
        }
        active.handle.set_logo(want);
    }

    /// Flush + closing snapshot + LOCK release for the open workspace (if
    /// any), then drop its in-memory state. No-op on a session-only open.
    /// Stamp the real on-disk size into the list entry for `id` (no-op on
    /// an unknown id). Callers pick the quiescent moments — open and clean
    /// close — where the directory matches what the writer has flushed.
    fn set_entry_size(&mut self, id: &str, dir: &std::path::Path) {
        let size = entry_size_kib(dir);
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.size_kib = size;
        }
    }

    pub(crate) fn close_active_storage(&mut self) {
        if let Some(active) = self.active.take() {
            let snap = self.snapshot_now();
            // close is synchronous (acked by the writer thread), so the
            // flushed log + closing snapshot are on disk — the moment the
            // list entry's size is exact
            active.handle.close(Some(snap));
            self.set_entry_size(&active.id, &active.dir);
            self.reset_workspace_state();
        }
    }

    pub(crate) fn cmd_close_workspace(&mut self) -> Result<Reply, MoltError> {
        // clean close: persist the running mesh's crypto so a reopen resumes it
        self.persist_net_crypto_on_close();
        self.teardown_net();
        self.close_active_storage();
        self.session.active_workspace = String::new();
        // back on the boot context: no transport there (production runs no
        // fake peers; only the demo-mesh test seam re-arms its loopback mesh)
        self.ensure_demo_net();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Seal a workspace at rest under its recovery phrase (S6 — real,
    /// durable): `molt_storage::seal_at_rest` verifies the phrase against
    /// the encrypted genesis FIRST (proof the caller holds the credential),
    /// then removes the device-sealed key material and marks the manifest —
    /// the state is derived from the directory and survives restarts. The
    /// ACTIVE workspace refuses: it would be sealed from under its own open
    /// storage/mesh. Synchronous on the actor by design decision (§7.4):
    /// the phrase has 256-bit entropy, so verification is one HKDF + one
    /// AEAD open — no memory-hard KDF, no I/O worth a task.
    pub(crate) fn cmd_encrypt_workspace(
        &mut self,
        id: WorkspaceId,
        phrase: String,
    ) -> Result<Reply, MoltError> {
        if self.session.active_workspace == id {
            return Err(MoltError::WorkspaceBusy(
                "close the workspace before encrypting it".to_string(),
            ));
        }
        if !self.session.workspaces.iter().any(|w| w.id == id) {
            return Err(MoltError::UnknownWorkspace(id));
        }
        // a backup in flight is reading the very device-sealed key material
        // this command deletes: sealing mid-upload can leave a confirmed but
        // unrestorable blob (and retention would prune the good copies).
        // Refuse until the upload settles — mirrors `cmd_backup_now`.
        if self.backup_inflight.contains(&id) {
            return Err(MoltError::WorkspaceBusy(
                "a backup of this workspace is in flight — encrypt once it \
                 completes"
                    .to_string(),
            ));
        }
        if phrase.trim().is_empty() {
            return Err(MoltError::BadPayload(
                "the recovery phrase is required to encrypt — it is verified \
                 before the device-sealed keys are removed"
                    .to_string(),
            ));
        }
        // a storage-less node has no at-rest bytes and nothing to verify a
        // phrase against — claiming to seal would be exactly the fake
        // behavior this command used to be; refuse honestly instead
        if !self.persist {
            return Err(MoltError::Storage(
                "this node runs without workspace storage — there is nothing \
                 on disk to encrypt"
                    .to_string(),
            ));
        }
        let root = self.workspace_root();
        let dir = molt_storage::find_workspace_dir(&root, &id).ok_or_else(|| {
            MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            ))
        })?;
        molt_storage::seal_at_rest(&dir, &phrase)
            .map_err(molt_storage::StorageError::into_molt)?;
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.encrypted = true;
            // sealed = no key material: the details panel must not keep
            // showing the phrase or the genesis roster from session memory
            ws.seed = String::new();
            ws.members = Vec::new();
            ws.agenda = String::new();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Decrypt an at-rest-sealed workspace: `molt_storage::unseal_at_rest`
    /// REALLY verifies the phrase (BIP-39 checksum, then the genesis
    /// frame's Poly1305 tag under the derived key) — a wrong phrase is a
    /// hard error that changes nothing on disk. On success the key
    /// material is re-sealed under the local device key and the list entry
    /// gets its hidden details (phrase, roster, charter) back.
    pub(crate) fn cmd_decrypt_workspace(
        &mut self,
        id: WorkspaceId,
        phrase: String,
    ) -> Result<Reply, MoltError> {
        if !self.session.workspaces.iter().any(|w| w.id == id) {
            return Err(MoltError::UnknownWorkspace(id));
        }
        // symmetric with encrypt: a backup in flight is mid-read of this
        // dir's key material — decrypting under it would race the re-seal.
        if self.backup_inflight.contains(&id) {
            return Err(MoltError::WorkspaceBusy(
                "a backup of this workspace is in flight — decrypt once it \
                 completes"
                    .to_string(),
            ));
        }
        if phrase.trim().is_empty() {
            return Err(MoltError::BadPayload(
                "a recovery phrase is required to decrypt".to_string(),
            ));
        }
        // mirror of the encrypt honesty rule: nothing on disk, nothing to
        // verify the phrase against — refuse instead of pretending
        if !self.persist {
            return Err(MoltError::Storage(
                "this node runs without workspace storage — there is nothing \
                 on disk to decrypt"
                    .to_string(),
            ));
        }
        let root = self.workspace_root();
        let dir = molt_storage::find_workspace_dir(&root, &id).ok_or_else(|| {
            MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            ))
        })?;
        molt_storage::unseal_at_rest(&root, &dir, &phrase)
            .map_err(molt_storage::StorageError::into_molt)?;
        // the key material is back — refill the details-panel facts the
        // sealed state hid (same sources the boot scan uses)
        let seed = molt_storage::read_sealed_seed(&root, &dir, &id).unwrap_or_default();
        let mut members = Vec::new();
        let mut agenda = String::new();
        if let Some(genesis) = molt_storage::peek_genesis(&root, &dir, &id) {
            if let WorkspaceEvent::Founded {
                roster, agenda: a, ..
            } = genesis.body
            {
                // a closed workspace has no presence knowledge — every
                // member is honestly never-seen (same as the boot scan)
                members = roster_members(&roster, 0, |_| molt_core::MemberInfo::NEVER);
                agenda = a;
            }
        }
        if let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) {
            ws.encrypted = false;
            ws.seed = seed;
            ws.members = members;
            ws.agenda = agenda;
            // the sealed-at-rest skip status ("sealed — backup skipped") no
            // longer describes anything: this workspace can back up again
            // (mirrors cmd_set_workspace_backup's reset).
            ws.backup_error = String::new();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Manual workspace export (story 9): synchronous validation, then the
    /// blocking blob build (Argon2 + file I/O) runs OFF the actor via
    /// `spawn_blocking`; the real outcome returns as
    /// [`molt_core::Command::NetExportDone`] / `NetExportFailed` — the
    /// engine never fakes a success. Exporting the OPEN workspace is fine:
    /// log segments are append-only and the state files atomic-rename, so a
    /// concurrent copy is crash-consistent (design §6.1).
    pub(crate) fn cmd_export_workspace(
        &mut self,
        id: WorkspaceId,
        dest: String,
        passphrase: String,
    ) -> Result<Reply, MoltError> {
        let Some(entry) = self.session.workspaces.iter().find(|w| w.id == id) else {
            return Err(MoltError::UnknownWorkspace(id));
        };
        if entry.encrypted {
            return Err(MoltError::WorkspaceEncrypted(id));
        }
        if dest.trim().is_empty() {
            return Err(MoltError::BadPayload(
                "an export needs a target file path".to_string(),
            ));
        }
        // the engine-enforced passphrase policy (design §3.4) — fail fast,
        // before any state changes
        molt_storage::export::check_passphrase_policy(&passphrase)
            .map_err(|e| MoltError::BadPayload(e.to_string()))?;
        if self.session.export.running {
            return Err(MoltError::WorkspaceBusy(
                "an export is already running".to_string(),
            ));
        }
        let root = self.workspace_root();
        let Some(dir) = molt_storage::find_workspace_dir(&root, &id) else {
            return Err(MoltError::Storage(format!(
                "workspace {id} has no directory under {}",
                root.display()
            )));
        };
        let dest_path = molt_storage::expand_tilde(dest.trim());
        let dest_str = dest_path.display().to_string();
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Engine("engine is shutting down".to_string()));
        };
        self.session.export = molt_core::ExportState {
            running: true,
            workspace: id.clone(),
            dest: dest_str.clone(),
            result: String::new(),
            bytes: 0,
            skipped: Vec::new(),
        };
        self.emit_session(SessionScope::Full);
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                export_to_file(&root, &dir, &dest_path, zeroize::Zeroizing::new(passphrase))
            })
            .await;
            let cmd = match res {
                Ok(Ok(outcome)) => molt_core::Command::NetExportDone {
                    id,
                    dest: dest_str,
                    bytes: outcome.bytes,
                    skipped: outcome.skipped,
                },
                Ok(Err(e)) => molt_core::Command::NetExportFailed { id, error: e.to_string() },
                Err(e) => molt_core::Command::NetExportFailed {
                    id,
                    error: format!("export task failed: {e}"),
                },
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(crate::Envelope { cmd, reply }).await;
        });
        Ok(Reply::Ack)
    }

    /// The export task confirmed the blob on disk, fsynced (engine-internal).
    pub(crate) fn cmd_net_export_done(
        &mut self,
        id: WorkspaceId,
        dest: String,
        bytes: u64,
        skipped: Vec<String>,
    ) -> Result<Reply, MoltError> {
        self.session.export = molt_core::ExportState {
            running: false,
            workspace: id,
            dest,
            result: "ok".to_string(),
            bytes,
            skipped,
        };
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The export task failed (engine-internal) — the reason is surfaced
    /// verbatim; the stamp of a previous success does not survive (the
    /// state always describes the LAST attempt).
    pub(crate) fn cmd_net_export_failed(
        &mut self,
        id: WorkspaceId,
        error: String,
    ) -> Result<Reply, MoltError> {
        let ex = &mut self.session.export;
        ex.running = false;
        ex.workspace = id;
        ex.result = format!("error: {error}");
        ex.bytes = 0;
        ex.skipped = Vec::new();
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
        // honest stamps: enabling persists the pref and nothing else — the
        // next BackupTick runs a real first upload, and last_backup moves
        // ONLY on NetBackupDone. A stale failure note from the previous
        // toggle state no longer describes anything.
        ws.backup_error = String::new();
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
        if let Err(e) = molt_storage::write_prefs(&dir, &prefs) {
            tracing::warn!(error = %e, "persisting backup pref failed");
            self.session.notice = "storage-failed".to_string();
        }
    }

    /// Record in the workspace's `prefs.toml` whether its other members
    /// are in-process simulations (a sim-seam founding). The flag is a
    /// truthful legacy label with no production effect — no engine spawns
    /// fake peers and governance never counts for peers; only the
    /// demo-mesh test seam reads it. Same writer-vs-direct discipline as
    /// [`Self::persist_backup_pref`].
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
        // drop the in-memory last-backup stamp: a later restore that reuses
        // this id must not inherit a stale age from the deleted workspace
        self.backup_last_done.remove(&id);
        if self.session.active_workspace == id {
            self.session.active_workspace = String::new();
        }
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }
}

/// Blocking half of the manual export (runs inside `spawn_blocking`): build
/// the blob into `<name>.part` next to the destination, fsync it, then
/// atomically rename onto `dest` — a crash or failure never leaves a
/// half-written file under the target name. Missing parent directories are
/// created; an existing target file is replaced.
fn export_to_file(
    root: &std::path::Path,
    ws_dir: &std::path::Path,
    dest: &std::path::Path,
    passphrase: zeroize::Zeroizing<String>,
) -> Result<molt_storage::export::ExportOutcome, molt_storage::StorageError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export.molt.enc".to_string());
    let part = dest.with_file_name(format!("{name}.part"));
    // any failure past this point removes the partial file — a failed
    // export leaves nothing behind under any name
    let write = || -> Result<molt_storage::export::ExportOutcome, molt_storage::StorageError> {
        let file = std::fs::File::create(&part)?;
        let mut out = std::io::BufWriter::new(file);
        let key = molt_storage::export::ExportKey::Passphrase(passphrase);
        let outcome = molt_storage::export::export_dir(root, ws_dir, &key, &mut out)?;
        use std::io::Write as _;
        out.flush()?;
        let file = out
            .into_inner()
            .map_err(|e| molt_storage::StorageError::Io(e.into_error()))?;
        file.sync_all()?;
        std::fs::rename(&part, dest)?;
        // fsync the containing directory, or a power loss can undo the
        // rename even though the data blocks are on disk (same rule as
        // molt-storage's write_atomic)
        if let Some(parent) = dest.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        Ok(outcome)
    };
    let res = write();
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
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
    // Fail CLOSED on a custom SMP server with no URL (audit finding #3): the
    // resolver falls back to the bundled PUBLIC server otherwise, so a user who
    // picks a private server but leaves the field blank would silently route over
    // the public one — a metadata surprise. A `urls` list satisfies it too.
    if s.smp_server == "custom" && s.smp_url.trim().is_empty() && s.smp_urls.is_empty() {
        return Err(MoltError::Settings(
            "a custom SMP server needs a URL (or a redundant-server list)".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit finding #3: a custom SMP server with a BLANK url (and no redundant
    /// list) must be REJECTED — otherwise the resolver silently falls back to the
    /// bundled public server, routing a user who picked a private server over the
    /// public one. A url OR a urls list satisfies it; "public" needs neither.
    #[test]
    fn validate_rejects_a_custom_smp_server_with_no_url() {
        let base = molt_core::SessionSettings::default();
        // public default: fine with no url
        assert!(validate_settings(&base).is_ok());

        // custom + blank url + no list → rejected (fail closed)
        let blank = molt_core::SessionSettings {
            smp_server: "custom".to_string(),
            smp_url: "   ".to_string(),
            smp_urls: Vec::new(),
            ..base.clone()
        };
        assert!(validate_settings(&blank).is_err(), "custom + blank url must fail closed");

        // custom + a url → fine
        let with_url = molt_core::SessionSettings {
            smp_server: "custom".to_string(),
            smp_url: "smp://AAAA@host".to_string(),
            ..base.clone()
        };
        assert!(validate_settings(&with_url).is_ok());

        // custom + a redundant list (but blank url) → fine (the list is the source)
        let with_list = molt_core::SessionSettings {
            smp_server: "custom".to_string(),
            smp_url: String::new(),
            smp_urls: vec!["smp://AAAA@host".to_string()],
            ..base
        };
        assert!(validate_settings(&with_list).is_ok());
    }
}
