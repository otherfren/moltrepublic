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

    /// Test connectivity to an SMP server (the settings panel's Test button).
    /// Resolves the target (explicit `url`, else the configured custom or
    /// public server), marks the test in flight, and runs the real TLS
    /// handshake **off the actor** — the outcome returns as
    /// [`molt_core::Command::NetTestResult`] so the actor never blocks on the
    /// network.
    pub(crate) fn cmd_net_test_server(&mut self, url: String) -> Result<Reply, MoltError> {
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
        let dialer = match self.dialer_for() {
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
                crate::founding::reopen_transport(&transport_state.mesh, creds, dialer.clone())
                    .and_then(|t| self.build_real_net(t, &transport_state.mesh, mls))
            }
            _ => None,
        };
        if let Some(net) = resumed {
            self.net = Some(net);
        } else {
            self.ensure_demo_net();
        }
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
        // so a roster grown by MemberJoined never leaves a stale list
        ws.members = roster_members(&replica.roster, |m| m == replica.member, "not seen yet");
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

    /// Encrypt a workspace at rest (mock flag flip — real at-rest crypto is
    /// the storage encryption story). The ACTIVE workspace refuses: it would
    /// be encrypted from under its own open storage/mesh.
    pub(crate) fn cmd_encrypt_workspace(&mut self, id: WorkspaceId) -> Result<Reply, MoltError> {
        if self.session.active_workspace == id {
            return Err(MoltError::WorkspaceBusy(
                "close the workspace before encrypting it".to_string(),
            ));
        }
        let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) else {
            return Err(MoltError::UnknownWorkspace(id));
        };
        ws.encrypted = true;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Decrypt an at-rest-encrypted workspace. Mock: the phrase is required
    /// but not yet verified against the workspace key.
    pub(crate) fn cmd_decrypt_workspace(
        &mut self,
        id: WorkspaceId,
        phrase: String,
    ) -> Result<Reply, MoltError> {
        if phrase.trim().is_empty() {
            return Err(MoltError::BadPayload(
                "a recovery phrase is required to decrypt".to_string(),
            ));
        }
        let Some(ws) = self.session.workspaces.iter_mut().find(|w| w.id == id) else {
            return Err(MoltError::UnknownWorkspace(id));
        };
        ws.encrypted = false;
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
