// SPDX-License-Identifier: GPL-3.0-or-later
//! The window's entry point: build the `AppWindow`, wire every callback
//! group, start the live mirror, run the event loop. [`Ctx`] is what the
//! callbacks capture; the `issue*` helpers are the three ways a click
//! reaches the engine.

#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use molt_core::{Command, SessionSettings};
use crate::engine::Engine;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use crate::platform::Spawner;

use crate::i18n::error_toast;
use crate::mirror::{push_surfaces, spawn_mirror};
use crate::net_tor::tor_mode_enabled;
use crate::settings::issue_draft;
use crate::surfaces::ChatUiState;
use crate::wiki_bridge::{
    wire_patch_view, wire_wiki, wire_wiki_draft, wire_wiki_export,
    wire_wiki_file_picker, wire_wiki_index, wire_wiki_vote,
};
use crate::{actions, AppWindow};

/// A config path as the settings footer shows it: the directory with its
/// trailing separator, and the file name. A string split, not `Path`: a
/// window behind the web door shows the SERVER's path, whose separator is
/// not this target's.
pub(crate) fn config_location(path: &str) -> (String, String) {
    let cut = path.rfind(['/', '\\']).map_or(0, |at| at + 1);
    (path[..cut].to_string(), path[cut..].to_string())
}

/// The LOCAL config path for the footer, split the way this target's
/// paths split (on Unix a backslash is a file-name character).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn local_config_location(path: &std::path::Path) -> (String, String) {
    let full = path.to_string_lossy();
    let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    match full.strip_suffix(file.as_str()) {
        Some(dir) => (dir.to_string(), file),
        None => (full.into_owned(), String::new()),
    }
}

/// What a window behind the web door takes from the server instead of
/// from its process (`docs/web_gui.md` §6.6): the embedded-Tor row from
/// the welcome, the settings footer from the engine's own config path,
/// and no folder dialog.
pub(crate) fn apply_remote_startup(ui: &AppWindow, caps: molt_core::wire::Caps, config_path: &str) {
    // a path here names the server's disk: the typed forms stay, the
    // folder dialogs of this machine go (§7)
    ui.set_folder_browse(false);
    if !ui.get_web() {
        ui.set_web(true);
        crate::i18n::apply_web_strings(ui, ui.get_lang_index());
    }
    let rows = tor_mode_enabled(caps.embedded_tor);
    if !ui.get_tor_mode_enabled().iter().eq(rows) {
        ui.set_tor_mode_enabled(ModelRc::new(VecModel::from(rows.to_vec())));
    }
    if !config_path.is_empty() {
        let (dir, file) = config_location(config_path);
        ui.set_config_dir(dir.into());
        ui.set_config_file(file.into());
    }
}

/// Open the GUI and run the Slint event loop on the calling (main) thread.
///
/// `config_path` is shown in the settings panel as the location a real save
/// *would* target. `embedded_tor_available` is the compile-time truth of the
/// binary's `embedded-tor` feature (P3): when false, the tor-mode dropdown greys
/// its "embedded" row (the in-process arti dialer was not built in). Returns
/// when the window closes, or an error if the GUI cannot start (e.g. no display)
/// — in which case the caller falls back to headless.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_app(
    wallet: impl Into<Engine>,
    rt: impl Into<Spawner>,
    config_path: PathBuf,
    embedded_tor_available: bool,
) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    // the settings footer shows the full config.toml location: directory
    // greyed, file name in text color. Absolutize a relative discovery path
    // (e.g. "config.toml" found in the cwd) so it reads as a full path.
    let abs = crate::native::absolute(&config_path);
    let (dir, file) = local_config_location(&abs);
    ui.set_config_dir(dir.into());
    ui.set_config_file(file.into());
    ui.set_app_version(version_label(env!("CARGO_PKG_VERSION"), cfg!(debug_assertions)).into());
    // Surface the compile-time embedded-tor availability into the tor-mode
    // dropdown's per-row enabled flags (a constant for the process lifetime).
    ui.set_tor_mode_enabled(ModelRc::new(VecModel::from(
        tor_mode_enabled(embedded_tor_available).to_vec(),
    )));
    wire_window(&ui, wallet.into(), rt.into());
    ui.run()
}

/// Open the GUI on an engine behind the web door and run the event loop:
/// the entry of a browser build (`docs/web_gui.md` §6.6), where `run`
/// does not return. There are no process inputs - the config location and
/// the embedded-Tor row arrive with the first session
/// ([`apply_remote_startup`]). `lang` (0 = English, 1 = German) labels the
/// window until that session names the language.
///
/// `opened` is handed the window's [`Sizer`] before the loop starts: a
/// browser has no window manager, so the page sizes the window itself.
pub fn run_remote(
    engine: impl Into<Engine>,
    spawner: impl Into<Spawner>,
    lang: i32,
    opened: impl FnOnce(Sizer),
) -> Result<(), slint::PlatformError> {
    let ui = open_remote(engine, spawner, lang)?;
    opened(Sizer(ui.as_weak()));
    ui.run()
}

/// Sets the size of an open window from outside it (the browser page
/// follows its viewport with this). Call it on the UI thread.
pub struct Sizer(slint::Weak<AppWindow>);

impl Sizer {
    /// Size the window to `width` x `height` logical pixels. The
    /// window's own minimum still holds. `false`: the window is gone.
    pub fn set(&self, width: f32, height: f32) -> bool {
        let Some(ui) = self.0.upgrade() else { return false };
        ui.window().set_size(slint::LogicalSize::new(width, height));
        true
    }
}

/// [`run_remote`] up to the event loop: the window, wired and mirroring.
pub(crate) fn open_remote(
    engine: impl Into<Engine>,
    spawner: impl Into<Spawner>,
    lang: i32,
) -> Result<AppWindow, slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_app_version(version_label(env!("CARGO_PKG_VERSION"), cfg!(debug_assertions)).into());
    // a path names the server's disk from the first frame on (§7), and
    // the web-only behaviour is there before the labels are (§6.5, §9)
    ui.set_folder_browse(false);
    ui.set_web(true);
    // the mirror applies the strings with each session; until the first
    // one lands the window would stand unlabelled
    crate::i18n::apply_strings(&ui, lang);
    wire_window(&ui, engine.into(), spawner.into());
    Ok(ui)
}

/// Wire every callback group of `ui` to `wallet` and start the live
/// mirror: what both entries share.
fn wire_window(ui: &AppWindow, wallet: Engine, spawner: Spawner) {
    // NOTE: the old duplicate-name check is gone by design — display names
    // may repeat, the workspace id disambiguates (the same DAO opened twice
    // locally is a supported setup).

    // The previously applied session settings: the mirror uses it to refresh
    // the settings draft only on real changes, the leave-guard to detect a
    // dirty draft.
    let last_settings: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    // Chat-bus UI state (selected channel, proposal
    // first-seen times) — UI-local by design, see [`ChatUiState`].
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));

    // what every callback captures: one clone per closure
    let ctx = Ctx {
        spawner,
        wallet,
        weak: ui.as_weak(),
        last_settings,
        chat_ui,
    };

    // The Multisig-Wiki mock's state machine + its WikiState bridge —
    // UI-local by design, EXCEPT the changeset vote: that one proposes on
    // the real gated Memory surface, so it is wired with the handles.
    let (wiki_model, wiki_last) = wire_wiki(ui);
    wire_wiki_vote(ui, &ctx, &wiki_model, &wiki_last);
    wire_patch_view(ui);
    wire_wiki_index(ui, &ctx);
    wire_wiki_file_picker(ui, &ctx);
    wire_wiki_export(ui, &ctx);
    wire_wiki_draft(ui, &ctx);

    // --- actions: each becomes a Command on the shared engine ---
    actions::clipboard::wire(ui, &ctx);
    actions::workspace::wire(ui, &ctx);
    actions::settings::wire(ui, &ctx);
    actions::relays::wire(ui, &ctx);
    actions::ritual::wire(ui, &ctx);
    actions::chat::wire(ui, &ctx);
    actions::org::wire(ui, &ctx);
    actions::vault::wire(ui, &ctx);
    actions::wallet::wire(ui, &ctx);
    actions::kanban::wire(ui, &ctx);
    // Quit confirmed from the modal: end the Slint event loop so `ui.run()`
    // returns and the process shuts down. (The modal opens on the window
    // manager's close request, which a browser tab never raises.)
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // --- live-mirror: re-read and re-render on every engine change ---
    spawn_mirror(&ctx);
}

/// What every callback captures: the spawner, the shared engine handle,
/// the window, and the two UI-local states the mirror task shares with the
/// callbacks. One clone per closure; the methods are the three ways a
/// click reaches the engine plus the surfaces re-read a view-local change
/// needs (sort, filter, page, channel - state the engine never sees).
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) spawner: Spawner,
    pub(crate) wallet: Engine,
    pub(crate) weak: slint::Weak<AppWindow>,
    pub(crate) last_settings: Arc<Mutex<Option<SessionSettings>>>,
    pub(crate) chat_ui: Arc<Mutex<ChatUiState>>,
}

impl Ctx {
    /// Run `task` to completion off the callback that started it. The one
    /// place the window starts a future: the target decides what runs it.
    pub(crate) fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.spawner.spawn(task);
    }

    /// Fire a command; an engine error surfaces as a toast ([`issue`]).
    pub(crate) fn issue(&self, cmd: Command) {
        issue(&self.spawner, &self.wallet, &self.weak, cmd);
    }

    /// [`issue_then_toast`]: the success toast fires only on success.
    pub(crate) fn issue_then_toast(&self, cmd: Command, toast: String) {
        issue_then_toast(&self.spawner, &self.wallet, &self.weak, cmd, toast);
    }

    /// [`issue_draft`]: the settings draft through its three doors.
    pub(crate) fn issue_draft(&self, wake: String, settings: SessionSettings) {
        let shown = crate::settings::stored_settings(&self.last_settings);
        issue_draft(&self.spawner, &self.wallet, &self.weak, wake, settings, shown);
    }

    /// Re-read and re-push the surfaces after a UI-local change the engine
    /// does not announce (a sort, a filter, a page, the channel selection).
    pub(crate) fn refresh_surfaces(&self) {
        let cx = self.clone();
        self.spawn(async move {
            push_surfaces(&cx.wallet, &cx.weak, &cx.chat_ui).await;
        });
    }
}

/// [`issue`], plus a success toast that only fires if the command SUCCEEDED.
///
/// The click sites used to toast on the way in: "Proposed" appeared the
/// moment the button was pressed, and a command that then failed showed the
/// user success followed by an error, for a proposal that never existed.
/// The confirmation belongs to the outcome, not to the intent.
fn issue_then_toast(
    spawner: &Spawner,
    wallet: &Engine,
    weak: &slint::Weak<AppWindow>,
    cmd: Command,
    toast: String,
) {
    let w = wallet.clone();
    let weak = weak.clone();
    spawner.spawn(async move {
        let outcome = w.execute(cmd).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match outcome {
                Ok(_) => ui.invoke_show_toast(toast.into()),
                Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
            }
        });
    });
}

/// Fire a command on the shared handle; the resulting event drives the
/// live-mirror, so callers do not await a reply — but an engine error is
/// surfaced as a toast instead of vanishing silently.
fn issue(spawner: &Spawner, wallet: &Engine, weak: &slint::Weak<AppWindow>, cmd: Command) {
    let w = wallet.clone();
    let weak = weak.clone();
    spawner.spawn(async move {
        if let Err(e) = w.execute(cmd).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_show_toast_error(error_toast(&ui, &e));
                }
            });
        }
    });
}

/// The version as the GUI shows it: a debug build says so.
pub(crate) fn version_label(version: &str, debug: bool) -> String {
    if debug {
        format!("{version} debug")
    } else {
        version.to_string()
    }
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn a_debug_build_is_marked_beside_the_version() {
        assert_eq!(super::version_label("1.2.3", true), "1.2.3 debug");
        assert_eq!(super::version_label("1.2.3", false), "1.2.3");
    }
}
