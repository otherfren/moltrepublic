// SPDX-License-Identifier: GPL-3.0-or-later
//! The window's entry point: build the `AppWindow`, wire every callback
//! group, start the live mirror, run the event loop. [`Ctx`] is what the
//! callbacks capture; the `issue*` helpers are the three ways a click
//! reaches the engine.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use molt_core::{Command, SessionSettings};
use molt_engine::WalletHandle;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::i18n::error_toast;
use crate::mirror::{push_surfaces, spawn_mirror};
use crate::net_tor::tor_mode_enabled;
use crate::settings::issue_draft;
use crate::surfaces::ChatUiState;
use crate::wiki_bridge::{
    wire_patch_view, wire_wiki, wire_wiki_draft, wire_wiki_export, wire_wiki_vote,
};
use crate::{actions, AppWindow};

/// Open the GUI and run the Slint event loop on the calling (main) thread.
///
/// `config_path` is shown in the settings panel as the location a real save
/// *would* target. `embedded_tor_available` is the compile-time truth of the
/// binary's `embedded-tor` feature (P3): when false, the tor-mode dropdown greys
/// its "embedded" row (the in-process arti dialer was not built in). Returns
/// when the window closes, or an error if the GUI cannot start (e.g. no display)
/// — in which case the caller falls back to headless.
pub fn run_app(
    wallet: WalletHandle,
    rt: Handle,
    config_path: PathBuf,
    embedded_tor_available: bool,
) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    // the settings footer shows the full config.toml location: directory
    // greyed, file name in text color. Absolutize a relative discovery path
    // (e.g. "config.toml" found in the cwd) so it reads as a full path.
    let abs = if config_path.is_absolute() {
        config_path.clone()
    } else {
        std::env::current_dir()
            .map(|d| d.join(&config_path))
            .unwrap_or_else(|_| config_path.clone())
    };
    let file = abs
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = abs
        .parent()
        .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
        .unwrap_or_default();
    ui.set_config_dir(dir.into());
    ui.set_config_file(file.into());
    // Surface the compile-time embedded-tor availability into the tor-mode
    // dropdown's per-row enabled flags (a constant for the process lifetime).
    ui.set_tor_mode_enabled(ModelRc::new(VecModel::from(
        tor_mode_enabled(embedded_tor_available).to_vec(),
    )));

    // Clipboard (copy the seed out, paste a phrase in). arboard's X11 backend
    // serves the selection only while the `Clipboard` object is alive, and
    // dropping it stalls ~2 s trying to hand the contents to a clipboard
    // manager this setup may not have (then the contents are gone). So: create
    // ONE clipboard on first use and deliberately leak it — the X11 selection
    // dies with the process either way.
    let clip: Rc<RefCell<Option<&'static mut arboard::Clipboard>>> = Rc::new(RefCell::new(None));
    fn with_clipboard<R>(
        slot: &Rc<RefCell<Option<&'static mut arboard::Clipboard>>>,
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>,
    ) -> Option<R> {
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
    }
    {
        let clip = clip.clone();
        ui.on_copy_text(move |text| {
            let _ = with_clipboard(&clip, |cb| cb.set_text(text.to_string()));
        });
    }
    {
        let clip = clip.clone();
        ui.on_paste_clipboard(move || {
            with_clipboard(&clip, arboard::Clipboard::get_text)
                .unwrap_or_default()
                .into()
        });
    }

    // Copy one of the (session-mirrored) run logs as one text block.
    {
        let clip = clip.clone();
        let weak = ui.as_weak();
        ui.on_copy_log(move |which| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let log = match which.as_str() {
                "create" => ui.get_cw_log(),
                "join" => ui.get_jw_log(),
                _ => ui.get_rw_log(),
            };
            let text = log
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = with_clipboard(&clip, |cb| cb.set_text(text));
        });
    }

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
        rt: rt.clone(),
        wallet: wallet.clone(),
        weak: ui.as_weak(),
        last_settings: last_settings.clone(),
        chat_ui: chat_ui.clone(),
    };

    // The Multisig-Wiki mock's state machine + its WikiState bridge —
    // UI-local by design, EXCEPT the changeset vote: that one proposes on
    // the real gated Memory surface, so it is wired with the handles.
    let (wiki_model, wiki_last) = wire_wiki(&ui);
    wire_wiki_vote(&ui, &ctx, &wiki_model, &wiki_last);
    wire_patch_view(&ui);
    wire_wiki_export(&ui, &ctx);
    wire_wiki_draft(&ui, &ctx);

    // --- actions: each becomes a Command on the shared engine ---
    actions::workspace::wire(&ui, &ctx);
    actions::settings::wire(&ui, &ctx);
    actions::relays::wire(&ui, &ctx);
    actions::ritual::wire(&ui, &ctx);
    actions::chat::wire(&ui, &ctx);
    actions::org::wire(&ui, &ctx);
    // Quit confirmed from the modal: end the Slint event loop so `ui.run()`
    // returns and the process shuts down.
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // --- live-mirror: re-read and re-render on every engine change ---
    spawn_mirror(&ctx);

    ui.run()
}

/// What every callback captures: the runtime, the shared engine handle,
/// the window, and the two UI-local states the mirror task shares with the
/// callbacks. One clone per closure; the methods are the three ways a
/// click reaches the engine plus the surfaces re-read a view-local change
/// needs (sort, filter, page, channel - state the engine never sees).
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) rt: Handle,
    pub(crate) wallet: WalletHandle,
    pub(crate) weak: slint::Weak<AppWindow>,
    pub(crate) last_settings: Arc<Mutex<Option<SessionSettings>>>,
    pub(crate) chat_ui: Arc<Mutex<ChatUiState>>,
}

impl Ctx {
    /// Fire a command; an engine error surfaces as a toast ([`issue`]).
    pub(crate) fn issue(&self, cmd: Command) {
        issue(&self.rt, &self.wallet, &self.weak, cmd);
    }

    /// [`issue_then_toast`]: the success toast fires only on success.
    pub(crate) fn issue_then_toast(&self, cmd: Command, toast: String) {
        issue_then_toast(&self.rt, &self.wallet, &self.weak, cmd, toast);
    }

    /// [`issue_draft`]: the settings draft through its three doors.
    pub(crate) fn issue_draft(&self, wake: String, settings: SessionSettings) {
        issue_draft(&self.rt, &self.wallet, &self.weak, wake, settings);
    }

    /// Re-read and re-push the surfaces after a UI-local change the engine
    /// does not announce (a sort, a filter, a page, the channel selection).
    pub(crate) fn refresh_surfaces(&self) {
        let cx = self.clone();
        self.rt.spawn(async move {
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
    rt: &Handle,
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    cmd: Command,
    toast: String,
) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
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
fn issue(rt: &Handle, wallet: &WalletHandle, weak: &slint::Weak<AppWindow>, cmd: Command) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
        if let Err(e) = w.execute(cmd).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_show_toast_error(error_toast(&ui, &e));
                }
            });
        }
    });
}
