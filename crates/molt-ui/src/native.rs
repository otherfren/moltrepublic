// SPDX-License-Identifier: GPL-3.0-or-later
//! **Native only.** What the window takes from tokio and from the
//! operating system lives here, so the rest of the crate names neither
//! (`docs/web_gui.md` §6.2, §6.3). A browser build replaces this module:
//! its [`Spawner`] is `slint::spawn_local`, its [`off_thread`] runs inline
//! on the one thread there is, and its [`platform`] is the browser's.
//!
//! [`NativePlatform`] is the native half of the platform seam
//! (`molt_ui_kit::platform::Platform`): the clipboard, the alert player,
//! the clocks, the file dialogs and the local file reads.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use molt_ui_kit::platform::{FilePick, Platform, Save, Task};

use std::future::Future;

use molt_core::Event;
use molt_ui_kit::client::NoEvent;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;

/// Where the window's futures run: the node's tokio runtime.
#[derive(Clone)]
pub struct Spawner(Handle);

impl From<Handle> for Spawner {
    fn from(rt: Handle) -> Self {
        Spawner(rt)
    }
}

impl Spawner {
    /// Run `task` to completion, detached.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn(task);
    }
}

/// Run blocking or CPU-heavy `work` off the caller's thread. `None`: the
/// worker died before it answered.
pub(crate) async fn off_thread<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    tokio::task::spawn_blocking(work).await.ok()
}

fn worker_lost() -> std::io::Error {
    std::io::Error::other("worker lost")
}

/// The native window's platform, for [`crate::platform::platform`].
pub(crate) fn platform() -> Box<dyn Platform> {
    Box::new(NativePlatform)
}

/// The operating system behind the platform seam.
pub(crate) struct NativePlatform;

thread_local! {
    /// arboard's X11 backend serves the selection only while the
    /// `Clipboard` object is alive, and dropping it stalls ~2 s trying to
    /// hand the contents to a clipboard manager this setup may not have
    /// (then the contents are gone). So: ONE clipboard, created on first
    /// use on the UI thread and deliberately leaked - the X11 selection
    /// dies with the process either way.
    static CLIPBOARD: RefCell<Option<&'static mut arboard::Clipboard>> = const { RefCell::new(None) };
}

fn with_clipboard<R>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>) -> Option<R> {
    CLIPBOARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => *slot = Some(Box::leak(Box::new(cb))),
                Err(e) => {
                    tracing::warn!(error = %e, "clipboard unavailable");
                    return None;
                }
            }
        }
        let cb = slot.as_mut()?;
        match f(cb) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "clipboard access failed");
                None
            }
        }
    })
}

/// The directory a folder dialog should start in, given a hand-editable
/// draft: the draft - its leading `~` expanded the same way the engine
/// resolves the setting (`molt_storage::expand_tilde`, so the config
/// default "~/…" starts at the real folder) - when it names an existing
/// directory, otherwise `None`: an empty draft or a typo must not derail
/// the dialog (rfd then opens at its platform default). Runs a stat -
/// call it off the UI thread (the draft may point at a slow mount).
pub(crate) fn browse_start_dir(draft: &str) -> Option<PathBuf> {
    let path = molt_storage::expand_tilde(draft);
    path.is_dir().then_some(path)
}

/// `path` as a full path: a relative one is read against the process's
/// working directory (the config file `moltd` found beside itself), and
/// stays as it is when that directory cannot be read.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |dir| dir.join(path))
}

/// Write `bytes` to the path a save dialog returned (it asked before
/// replacing a file). With `refuse_link` a link there is refused, never
/// followed.
pub(crate) fn write_saved(path: &Path, bytes: &[u8], refuse_link: bool) -> std::io::Result<PathBuf> {
    if refuse_link && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "a link"));
    }
    std::fs::write(path, bytes)?;
    Ok(path.to_path_buf())
}

/// A per-process-start random tag for the alert files: unguessable, unlike
/// the pid.
fn alert_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| molt_config::random_token().unwrap_or_else(|_| "0".to_string()))
}

/// Create `path` as a NEW file - never through one that already exists
/// (a planted file or symlink there is not ours to write).
fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    std::io::Write::write_all(&mut f, bytes)
}

impl Platform for NativePlatform {
    fn copy(&self, text: String) {
        let _ = with_clipboard(|cb| cb.set_text(text));
    }

    /// Read at once, on the calling (UI) thread, where the one clipboard
    /// lives; the future is ready.
    fn paste(&self) -> Task<Option<String>> {
        let text = with_clipboard(arboard::Clipboard::get_text);
        Box::pin(async move { text })
    }

    /// The sample is rendered once per process (a tiny WAV, cached under
    /// the runtime dir) and handed to the system player - pw-play, paplay
    /// or aplay, runtime-detected, silently a no-op when none exists.
    /// Deliberately NO compiled audio stack: cpal/rodio would pull ALSA's
    /// C bindings, and the pure-Rust posture stands (CLAUDE.md).
    ///
    /// ALL work - the first-play render and the player spawn - runs on a
    /// detached thread, never the caller's, and the spawned player is
    /// REAPED there (its `wait()`), so no zombies accumulate.
    fn play_alert(&self, kind: &str, wav: fn(&str) -> Vec<u8>) {
        let kind = kind.to_string();
        std::thread::spawn(move || {
            // under the per-user runtime dir (0700) when there is one, else
            // the shared temp dir - and never a file this process did not
            // write: a pre-planted file (pids are enumerable) would feed the
            // player attacker bytes (review F8). One random tag per process
            // start.
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|d| d.is_dir())
                .unwrap_or_else(std::env::temp_dir);
            let path = dir.join(format!("molt-alert-{}-{kind}.wav", alert_nonce()));
            if !path.exists() && write_new_file(&path, &wav(&kind)).is_err() {
                return;
            }
            for player in ["pw-play", "paplay", "aplay"] {
                match std::process::Command::new(player)
                    .arg(&path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(mut child) => {
                        let _ = child.wait(); // reap - no zombie
                        return;
                    }
                    Err(_) => continue,
                }
            }
        });
    }

    fn unix_now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    fn monotonic(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }

    fn sleep(&self, time: Duration) -> Task<()> {
        // built inside the future: tokio arms a timer only on its runtime
        Box::pin(async move { tokio::time::sleep(time).await })
    }

    fn random_token(&self) -> Option<String> {
        molt_config::random_token().ok()
    }

    fn pick_folder(&self, start: String) -> Task<Option<String>> {
        Box::pin(async move {
            let start_dir = off_thread(move || browse_start_dir(&start)).await.flatten();
            let mut picker = rfd::AsyncFileDialog::new();
            if let Some(dir) = start_dir {
                picker = picker.set_directory(dir);
            }
            picker.pick_folder().await.map(|folder| folder.path().display().to_string())
        })
    }

    fn pick_file(&self, pick: FilePick) -> Task<Option<String>> {
        Box::pin(async move {
            let mut picker = rfd::AsyncFileDialog::new();
            if let Some((label, extensions)) = &pick.filter {
                picker = picker.add_filter(label.as_str(), extensions);
            }
            picker.pick_file().await.map(|file| file.path().display().to_string())
        })
    }

    fn pick_save(&self, name: String) -> Task<Option<String>> {
        Box::pin(async move {
            let mut picker = rfd::AsyncFileDialog::new();
            if !name.is_empty() {
                picker = picker.set_file_name(name.as_str());
            }
            picker.save_file().await.map(|dest| dest.path().display().to_string())
        })
    }

    fn read_file(&self, file: String) -> Task<std::io::Result<Vec<u8>>> {
        Box::pin(async move { off_thread(move || std::fs::read(file)).await.unwrap_or_else(|| Err(worker_lost())) })
    }

    fn save_file(&self, save: Save) -> Task<std::io::Result<Option<String>>> {
        Box::pin(async move {
            let Some(dest) = rfd::AsyncFileDialog::new().set_file_name(save.name.as_str()).save_file().await else {
                return Ok(None); // cancelled
            };
            let path = dest.path().to_path_buf();
            off_thread(move || write_saved(&path, &save.bytes, save.refuse_link))
                .await
                .unwrap_or_else(|| Err(worker_lost()))
                .map(|path| Some(path.display().to_string()))
        })
    }

    fn local_bytes(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn local_stamp(&self, path: &str) -> Option<(u64, u128)> {
        let meta = std::fs::metadata(path).ok()?;
        let stamp = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        Some((meta.len(), stamp))
    }
}

/// The in-process engine's event stream.
pub(crate) struct EngineEvents(pub(crate) Receiver<Event>);

impl EngineEvents {
    /// The next event; `Lagged` means "re-read everything".
    pub(crate) async fn recv(&mut self) -> Result<Event, NoEvent> {
        self.0.recv().await.map_err(|e| match e {
            RecvError::Lagged(_) => NoEvent::Lagged,
            RecvError::Closed => NoEvent::Closed,
        })
    }
}
