// SPDX-License-Identifier: GPL-3.0-or-later

//! The ConfigStore task: single owner of the node's `config.toml` file.
//!
//! The engine actor owns the *values*; this task owns the *file* — both the
//! writes (`SaveSettings` persists through here) and the watcher half that
//! picks up external edits. One owner for the file means no cross-task file
//! races by construction, and the engine never blocks on a disk.
//! Design: `docs/build/concept-config-bidirection.md`.
//!
//! * **App → file**: the engine queues [`StoreRequest::Persist`]; bursts are
//!   coalesced (250 ms debounce, newest wins) into one format-preserving,
//!   atomic write (`molt_config::update` + temp-and-rename). The outcome is
//!   reported back into the session notice via the engine-internal
//!   [`Command::ConfigNotice`].
//! * **File → app**: the same task polls the file (2 s; byte comparison
//!   against the last known-good content — polling by path is immune to
//!   editor rename/truncate strategies, and comparing the actual bytes is
//!   the echo suppression: a write of our own never loops back as a reload).
//!   A valid external edit is sent to the engine as the internal
//!   [`Command::ReloadSettings`]; an invalid one raises `config-conflict`
//!   and polling simply continues until the user's edit becomes valid.
//! * **Cross-instance safety**: a `<config>.lock` file carrying the owner's
//!   PID makes a second node on the same config fail fast at startup.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use molt_config::Settings;
use molt_core::{Command, MoltError, Reply, SessionSettings};
use tokio::sync::{mpsc, oneshot};

use crate::Envelope;

/// How long a burst of Persist requests is collected before the one write.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often the file is checked for external edits.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Capacity of the store's request queue.
const STORE_QUEUE: usize = 32;

/// A cheap, cloneable handle to the running ConfigStore task. The engine
/// holds one next to its command queue; `moltd` holds another for the clean
/// shutdown flush.
#[derive(Clone)]
pub struct ConfigStoreHandle {
    tx: mpsc::Sender<StoreRequest>,
}

/// One request into the store task.
enum StoreRequest {
    /// Write these settings to disk (coalesced; newest wins). When `notify`
    /// is set the outcome lands in the session notice — an explicit Save
    /// wants the toast, a language/theme click persists silently (failures
    /// are always reported).
    Persist {
        settings: Box<Settings>,
        notify: bool,
    },
    /// Run one poll pass now and ack — the deterministic test hook.
    PollNow(oneshot::Sender<()>),
    /// Flush any pending write, release the lock file, and stop.
    Shutdown(oneshot::Sender<()>),
}

impl ConfigStoreHandle {
    /// Queue a persist (fire-and-forget; the engine must never block on disk).
    pub(crate) fn persist(&self, settings: Settings, notify: bool) {
        if self
            .tx
            .try_send(StoreRequest::Persist {
                settings: Box::new(settings),
                notify,
            })
            .is_err()
        {
            // Queue full or store gone — coalescing makes a full queue mean
            // dozens of unwritten saves already; the newest one wins anyway.
            tracing::warn!("config store queue unavailable; save not persisted");
        }
    }

    /// Force one watcher poll and wait for it — deterministic testing and
    /// "reload now" ops, instead of waiting out the 2 s interval.
    pub async fn poll_now(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(StoreRequest::PollNow(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Flush any pending write, release the config lock, and stop the task.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(StoreRequest::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

/// Start the ConfigStore for `path`. Sweeps stale temp files, takes the
/// `<config>.lock` (failing fast — with the owner's PID — when another node
/// already runs on this config), reads the initial content, and spawns the
/// owning task. Must be called from within a tokio runtime.
pub(crate) fn spawn(
    path: PathBuf,
    engine_tx: mpsc::Sender<Envelope>,
) -> std::io::Result<ConfigStoreHandle> {
    sweep_stale_tmp(&path);
    let lock_path = acquire_lock(&path)?;
    // The node just strictly parsed this file to boot; it is the known-good base.
    let last_good = std::fs::read_to_string(&path)?;
    let (tx, rx) = mpsc::channel(STORE_QUEUE);
    tokio::spawn(store_task(Store {
        path,
        lock_path,
        last_good,
        conflict: None,
        engine_tx,
        rx,
    }));
    Ok(ConfigStoreHandle { tx })
}

/// The task-owned state.
struct Store {
    path: PathBuf,
    lock_path: PathBuf,
    /// Content of the file as last written by us or last successfully
    /// reloaded — the echo-suppression reference and the fallback write base.
    last_good: String,
    /// Content (or the `<missing>` marker) already complained about, so a
    /// broken file raises one `config-conflict` per distinct state, not one
    /// per 2 s poll.
    conflict: Option<String>,
    engine_tx: mpsc::Sender<Envelope>,
    rx: mpsc::Receiver<StoreRequest>,
}

async fn store_task(mut store: Store) {
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.reset(); // the first tick fires immediately otherwise
    loop {
        let deferred = tokio::select! {
            req = store.rx.recv() => match req {
                None => break,
                Some(StoreRequest::Persist { settings, notify }) => {
                    let (settings, notify, deferred) =
                        coalesce(&mut store.rx, settings, notify).await;
                    store.write(&settings, notify).await;
                    deferred
                }
                Some(other) => vec![other],
            },
            _ = poll.tick() => {
                store.poll_once().await;
                Vec::new()
            }
        };
        for req in deferred {
            match req {
                StoreRequest::Persist { .. } => unreachable!("persists are coalesced"),
                StoreRequest::PollNow(ack) => {
                    store.poll_once().await;
                    let _ = ack.send(());
                }
                StoreRequest::Shutdown(ack) => {
                    let _ = std::fs::remove_file(&store.lock_path);
                    let _ = ack.send(());
                    return;
                }
            }
        }
    }
    // Queue closed without an explicit shutdown (engine dropped): still
    // release the lock so a restart does not trip over our own PID.
    let _ = std::fs::remove_file(&store.lock_path);
}

/// Collect a burst of Persist requests for [`DEBOUNCE`], keeping only the
/// newest settings (and whether anyone wanted a notice). Non-persist requests
/// arriving inside the window are returned to be handled after the write.
async fn coalesce(
    rx: &mut mpsc::Receiver<StoreRequest>,
    mut settings: Box<Settings>,
    mut notify: bool,
) -> (Box<Settings>, bool, Vec<StoreRequest>) {
    let mut deferred = Vec::new();
    let deadline = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            req = rx.recv() => match req {
                Some(StoreRequest::Persist { settings: s, notify: n }) => {
                    settings = s;
                    notify = notify || n;
                }
                Some(other) => deferred.push(other),
                None => break,
            }
        }
    }
    (settings, notify, deferred)
}

impl Store {
    /// One format-preserving, atomic write; reports the outcome as a notice.
    async fn write(&mut self, settings: &Settings, notify: bool) {
        match self.write_inner(settings) {
            Ok(()) => {
                if notify {
                    self.notice("saved").await;
                }
            }
            Err(detail) => {
                // A failed write must be visible, notify or not.
                tracing::warn!(path = %self.path.display(), %detail, "config save failed");
                self.notice(&format!("save-failed: {detail}")).await;
            }
        }
    }

    fn write_inner(&mut self, settings: &Settings) -> Result<(), String> {
        // Base: the on-disk file (someone may have hand-edited since our last
        // write — their comments must survive). A deleted file is recreated
        // from the last known-good content; a *broken* file is never guessed
        // at or clobbered — the user is probably mid-edit.
        let base = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.last_good.clone(),
            Err(e) => return Err(format!("reading {}: {e}", self.path.display())),
        };
        let new_text = molt_config::update(&base, settings).map_err(|_| {
            "config.toml on disk is invalid; fix it or run --repair-config".to_string()
        })?;
        atomic_write(&self.path, &new_text).map_err(|e| e.to_string())?;
        self.last_good = new_text;
        self.conflict = None;
        Ok(())
    }

    /// One watcher pass: detect an external change, validate it, mirror it.
    async fn poll_once(&mut self) {
        const MISSING: &str = "<missing>";
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Externally deleted: keep the session values; the next save
                // recreates the file. Complain once.
                if self.conflict.as_deref() != Some(MISSING) {
                    tracing::warn!(path = %self.path.display(), "config.toml disappeared");
                    self.conflict = Some(MISSING.to_string());
                    self.notice("config-conflict").await;
                }
                return;
            }
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "config poll failed");
                return;
            }
        };
        if text == self.last_good {
            // Unchanged — or the echo of our own write coming back around.
            return;
        }
        match molt_config::parse(&text) {
            Err(e) => {
                // Broken TOML or invalid schema: keep the last good values,
                // keep polling — when the edit becomes valid it applies.
                if self.conflict.as_deref() != Some(text.as_str()) {
                    tracing::warn!(path = %self.path.display(), error = %e, "external config edit rejected");
                    self.conflict = Some(text);
                    self.notice("config-conflict").await;
                }
            }
            Ok(config) => {
                let settings = Settings::from(&config);
                let cmd = Command::ReloadSettings {
                    settings: session_settings(&settings),
                    language: settings.lang.clone(),
                    theme: settings.theme.clone(),
                };
                match self.send(cmd).await {
                    Ok(_) => {
                        self.last_good = text;
                        self.conflict = None;
                    }
                    Err(e) => {
                        // Well-formed file, but the engine's value validation
                        // rejected it — same treatment as broken TOML.
                        if self.conflict.as_deref() != Some(text.as_str()) {
                            tracing::warn!(path = %self.path.display(), error = %e, "external config edit rejected");
                            self.conflict = Some(text);
                            self.notice("config-conflict").await;
                        }
                    }
                }
            }
        }
    }

    /// Surface a notice in the shared session (via the internal command).
    async fn notice(&self, notice: &str) {
        let _ = self
            .send(Command::ConfigNotice {
                notice: notice.to_string(),
            })
            .await;
    }

    /// Send one command into the engine actor and await its reply.
    async fn send(&self, cmd: Command) -> Result<Reply, MoltError> {
        let (reply, rx) = oneshot::channel();
        self.engine_tx
            .send(Envelope { cmd, reply })
            .await
            .map_err(|_| MoltError::Engine("engine stopped".into()))?;
        rx.await
            .map_err(|_| MoltError::Engine("no reply from engine".into()))?
    }
}

// ---------------------------------------------------------------------------
// Settings mapping: config.toml <-> session
// ---------------------------------------------------------------------------

/// The file-shaped settings for a session state (the session keeps language
/// and theme outside [`SessionSettings`], the file carries them in `[ui]`).
pub(crate) fn file_settings(s: &SessionSettings, language: &str, theme: &str) -> Settings {
    Settings {
        headless: s.headless,
        workspace_dir: s.workspace_dir.clone(),
        s3_backup: s.s3_backup,
        s3_endpoint: s.s3_endpoint.clone(),
        s3_access_key: s.s3_access_key.clone(),
        s3_secret_key: s.s3_secret_key.clone(),
        s3_bucket: s.s3_bucket.clone(),
        s3_interval_min: s.s3_interval_min,
        s3_keep_copies: s.s3_keep_copies,
        sound_message: s.sound_message.clone(),
        sound_vote: s.sound_vote.clone(),
        read_receipts: s.read_receipts,
        anonymity: s.anonymity.clone(),
        tor_mode: s.tor_mode.clone(),
        tor_port: s.tor_port,
        smp_server: s.smp_server.clone(),
        smp_url: s.smp_url.clone(),
        smp_urls: s.smp_urls.clone(),
        download_dir: s.download_dir.clone(),
        mcp_port: s.mcp_port,
        mcp_allow: s.mcp_allow.clone(),
        mcp_token: s.mcp_token.clone(),
        lang: language.to_string(),
        theme: theme.to_string(),
    }
}

/// The session-shaped settings for a file value (language/theme travel
/// separately in [`Command::ReloadSettings`]).
fn session_settings(s: &Settings) -> SessionSettings {
    SessionSettings {
        headless: s.headless,
        workspace_dir: s.workspace_dir.clone(),
        s3_backup: s.s3_backup,
        s3_endpoint: s.s3_endpoint.clone(),
        s3_access_key: s.s3_access_key.clone(),
        s3_secret_key: s.s3_secret_key.clone(),
        s3_bucket: s.s3_bucket.clone(),
        s3_interval_min: s.s3_interval_min,
        s3_keep_copies: s.s3_keep_copies,
        sound_message: s.sound_message.clone(),
        sound_vote: s.sound_vote.clone(),
        read_receipts: s.read_receipts,
        mcp_port: s.mcp_port,
        mcp_allow: s.mcp_allow.clone(),
        mcp_token: s.mcp_token.clone(),
        anonymity: s.anonymity.clone(),
        tor_mode: s.tor_mode.clone(),
        tor_port: s.tor_port,
        smp_server: s.smp_server.clone(),
        smp_url: s.smp_url.clone(),
        smp_urls: s.smp_urls.clone(),
        download_dir: s.download_dir.clone(),
    }
}

// ---------------------------------------------------------------------------
// File plumbing: atomic write, temp sweep, PID lock
// ---------------------------------------------------------------------------

/// The sibling temp file this process writes before the rename.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

/// `config.toml` → `config.toml.lock` (keeps the original name intact).
fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

/// Standard temp-and-rename in the same directory: a crash leaves either the
/// old file or the new file, never a torn one. The temp file gets the
/// original's permissions (the file carries the MCP token — it must not
/// widen), 0600 when there is no original.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(path)
                .map(|m| m.permissions())
                .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o600));
            f.set_permissions(perms)?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // fsync the directory so the rename itself survives a crash (best effort;
    // not every filesystem lets you sync a directory handle).
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Remove leftover `config.toml.tmp-*` siblings from a previous crash (the
/// write path never leaves one behind on a clean run).
fn sweep_stale_tmp(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let prefix = format!("{name}.tmp-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(n) = entry.file_name().to_str() {
            if n.starts_with(&prefix) {
                tracing::warn!(file = %entry.path().display(), "removing stale config temp file");
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Take the advisory config lock: create `<config>.lock` exclusively and
/// write our PID into it. A live holder makes this node refuse to start
/// read-write (naming the PID); a stale lock (holder no longer running) is
/// swept and retried. This also protects the echo-suppression assumption:
/// only *we* and humans write this file.
fn acquire_lock(path: &Path) -> std::io::Result<PathBuf> {
    let lock = lock_path_for(path);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut f) => {
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                return Ok(lock);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&lock).unwrap_or_default();
                let holder_pid: Option<u32> = holder.trim().parse().ok();
                let alive = holder_pid
                    .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
                    .unwrap_or(false);
                if alive {
                    return Err(std::io::Error::other(format!(
                        "another moltd (pid {}) already runs on {} — \
                         two nodes must not share one config ({})",
                        holder.trim(),
                        path.display(),
                        lock.display(),
                    )));
                }
                // Stale lock from a crashed run: sweep and retry (the
                // create_new above arbitrates if two nodes race here).
                tracing::warn!(lock = %lock.display(), "removing stale config lock");
                let _ = std::fs::remove_file(&lock);
            }
            Err(e) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn_with_config;
    use crate::WalletHandle;
    use molt_core::{GroupConfig, SessionView};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A fresh config file (default values + a hand-written comment that a
    /// runtime save must keep) in a temp dir, plus the node on top of it.
    fn node(dir: &tempfile::TempDir) -> (PathBuf, WalletHandle, ConfigStoreHandle) {
        let path = dir.path().join("config.toml");
        let text = format!(
            "# hands off, this is my config\n{}",
            molt_config::render(&Settings::default())
        );
        std::fs::write(&path, text).expect("seed config");
        let (wallet, store) =
            spawn_with_config(GroupConfig::demo(), SessionView::default(), path.clone())
                .expect("spawn with config");
        (path, wallet, store)
    }

    /// Await a file state (the write path debounces 250 ms).
    async fn wait_for_file(path: &Path, pred: impl Fn(&str) -> bool) -> String {
        for _ in 0..200 {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if pred(&text) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("file never reached the expected state");
    }

    async fn session(wallet: &WalletHandle) -> molt_core::SessionView {
        match wallet
            .execute(Command::ReadSession)
            .await
            .expect("read session")
        {
            Reply::Session(s) => *s,
            other => panic!("unexpected: {other:?}"),
        }
    }

    async fn wait_for_notice(wallet: &WalletHandle, want: &str) {
        for _ in 0..200 {
            if session(wallet).await.notice == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("notice never became `{want}`");
    }

    #[test]
    fn save_persists_format_preserving_and_flags_restart_keys() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, wallet, _store) = node(&dir);

            let settings = SessionSettings {
                mcp_port: 5555,
                anonymity: "nym".to_string(),
                ..SessionSettings::default()
            };
            wallet
                .execute(Command::SaveSettings { settings })
                .await
                .expect("save");

            let text = wait_for_file(&path, |t| t.contains("port = 5555")).await;
            // the user's comment and the strict schema both survive
            assert!(text.contains("# hands off, this is my config"));
            let cfg = molt_config::parse(&text).expect("saved file parses strictly");
            let s = Settings::from(&cfg);
            assert_eq!(s.mcp_port, 5555);
            assert_eq!(s.anonymity, "nym");

            // the async write outcome lands in the notice…
            wait_for_notice(&wallet, "saved").await;
            // …and the restart-required keys are flagged as shared state
            let sv = session(&wallet).await;
            assert_eq!(
                sv.restart_required,
                vec![
                    "mcp.port".to_string(),
                    "transport.anonymity.network".to_string()
                ]
            );
        });
    }

    #[test]
    fn our_own_write_does_not_echo_back_as_reload() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, wallet, store) = node(&dir);

            let settings = SessionSettings {
                tor_port: 9150,
                ..SessionSettings::default()
            };
            wallet
                .execute(Command::SaveSettings { settings })
                .await
                .expect("save");
            wait_for_file(&path, |t| t.contains("port = 9150")).await;
            wait_for_notice(&wallet, "saved").await;

            // force the watcher over its own write: it must NOT reload
            store.poll_now().await;
            let sv = session(&wallet).await;
            assert_eq!(sv.notice, "saved", "self-write echoed back as a reload");
            assert_eq!(sv.settings.tor_port, 9150);
        });
    }

    #[test]
    fn external_edit_reloads_invalid_conflicts_then_recovers() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, wallet, store) = node(&dir);

            // external valid edit -> mirrored into the session
            let edited = molt_config::update(
                &std::fs::read_to_string(&path).expect("read"),
                &Settings {
                    lang: "de".to_string(),
                    mcp_port: 6060,
                    ..Settings::default()
                },
            )
            .expect("edit");
            std::fs::write(&path, &edited).expect("write");
            store.poll_now().await;
            let sv = session(&wallet).await;
            assert_eq!(sv.notice, "config-reloaded");
            assert_eq!(sv.language, "de");
            assert_eq!(sv.settings.mcp_port, 6060);
            assert_eq!(sv.restart_required, vec!["mcp.port".to_string()]);

            // external broken edit -> conflict notice, session keeps values
            std::fs::write(&path, "this is not::: toml").expect("break");
            store.poll_now().await;
            let sv = session(&wallet).await;
            assert_eq!(sv.notice, "config-conflict");
            assert_eq!(sv.settings.mcp_port, 6060, "session must keep last good");

            // the user fixes the file -> next poll applies it
            std::fs::write(&path, &edited).expect("fix");
            store.poll_now().await;
            assert_eq!(session(&wallet).await.settings.mcp_port, 6060);
        });
    }

    #[test]
    fn valid_toml_with_invalid_values_is_a_conflict() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, wallet, store) = node(&dir);

            // well-formed TOML, but a value the engine's validation rejects
            let broken = std::fs::read_to_string(&path)
                .expect("read")
                .replace("network = \"none\"", "network = \"bogus\"");
            std::fs::write(&path, broken).expect("write");
            store.poll_now().await;
            let sv = session(&wallet).await;
            assert_eq!(sv.notice, "config-conflict");
            assert_eq!(sv.settings.anonymity, "none", "session must keep last good");
        });
    }

    #[test]
    fn a_second_node_on_the_same_config_fails_fast() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, _wallet, _store) = node(&dir);
            let Err(err) = spawn_with_config(GroupConfig::demo(), SessionView::default(), path)
            else {
                panic!("second node must be refused");
            };
            let msg = err.to_string();
            assert!(msg.contains("already runs"), "unexpected error: {msg}");
            assert!(
                msg.contains(&std::process::id().to_string()),
                "error must name the holding pid: {msg}"
            );
        });
    }

    #[test]
    fn shutdown_releases_the_lock() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, _wallet, store) = node(&dir);
            store.shutdown().await;
            assert!(!lock_path_for(&path).exists());
            // a fresh node can start again on the same config
            let (_w2, s2) =
                spawn_with_config(GroupConfig::demo(), SessionView::default(), path.clone())
                    .expect("restart after shutdown");
            s2.shutdown().await;
        });
    }

    #[test]
    fn language_and_theme_persist_silently() {
        rt().block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, wallet, _store) = node(&dir);
            wallet
                .execute(Command::SetLanguage {
                    lang: "de".to_string(),
                })
                .await
                .expect("set language");
            let text = wait_for_file(&path, |t| t.contains("lang = \"de\"")).await;
            assert!(molt_config::parse(&text).is_ok());
            // silent: no "saved" toast for a language click
            assert_eq!(session(&wallet).await.notice, "");
        });
    }
}
