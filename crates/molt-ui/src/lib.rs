// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
// The handwritten GUI logic casts small ints to Slint's `i32`, does float
// label math, and drives Slint APIs that return `Option`s we unwrap; the
// allows are scoped to this UI crate only, so the rest of the workspace
// keeps the strict posture. (Slint's GENERATED code lives in molt-ui-window
// with its own allow header.)
#![allow(
    clippy::as_conversions,
    clippy::unwrap_used,
    clippy::float_arithmetic,
    clippy::todo
)]

//! `molt-ui`: the GUI operator.
//!
//! This crate hosts the multi-stage front of the node — a first-run wizard
//! (create / open / join / restore), a shared completion screen, the main
//! surfaces view, and a settings panel. The settings are real (they persist
//! to the node's `config.toml` and mirror external edits of it); the
//! workspace lifecycles are real — create/open/join/close write to disk.
//!
//! The GUI is a **live-mirror of the engine's shared session**, not a holder of
//! its own state. Every action (navigate, switch language, save settings, finish
//! a wizard) is turned into a [`molt_core::Command`] on the shared
//! [`WalletHandle`]; a background task re-reads the session on each
//! [`molt_core::Event::SessionChanged`] and pushes it back into the Slint
//! properties. An MCP agent issuing the *same* commands drives this *same* state,
//! so the GUI and the MCP operator are co-equal — exactly as for the surfaces.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use molt_core::relay::{RelayBlock, RelayKind, RelayStatus, RelayUrlError};
use molt_core::{
    ChannelRef, Command, Event, MessageId, ProposalId, Reply, SessionScope, SessionSettings,
    SessionView, Surface,
};
use molt_engine::WalletHandle;
use slint::{Model, ModelRc, VecModel};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

// The Slint-generated window (AppWindow, the Strings/Theme globals, every
// row struct) lives in its own crate as a compile-time firewall — see
// molt-ui-window's crate docs. The glob keeps this crate's code reading as
// if the module were still injected here.
pub use molt_ui_window::*;

mod alerts;
mod channels;
mod chat_log;
mod i18n;
mod images;
mod labels;
mod models;
mod net_tor;
mod patchview;
mod surfaces;
mod wiki;
mod wiki_bridge;

use alerts::*;
use channels::*;
#[cfg(test)]
use chat_log::*;
use i18n::*;
use images::*;
use labels::*;
use models::*;
use net_tor::*;
use surfaces::*;
use wiki_bridge::*;

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

    // The join preview: the same molt-core invite parser the engine's join
    // run uses, so the preview and the run can never disagree.
    // The create wizard's live folder preview: the same molt-core slug
    // rule the storage layer builds the real directory name from, so the
    // preview and the disk can never disagree. The trailing short id is
    // elided — it derives from the seed, which only exists at finish.
    ui.on_folder_preview(|dir, name| {
        if name.trim().is_empty() {
            return "".into();
        }
        format!(
            "{}/{}.…",
            dir.trim_end_matches('/'),
            molt_core::slugify(&name)
        )
        .into()
    });

    {
        let weak = ui.as_weak();
        // the FULL parser, not the preview one: since the neutral link shape
        // (2026-08-08) the preview data rides inside the handover segment,
        // and only a JOINABLE link should ever preview as valid
        ui.on_parse_invite(move |s| match molt_engine::FoundingInvite::parse(&s).map(|i| i.info) {
            Ok(i) => {
                // how many of the republic's relays this node does not have.
                // The invite carries them, so a refused joiner never has to
                // copy them out of a chat message by hand.
                let missing = weak
                    .upgrade()
                    .map(|ui| invite_relays_missing(&ui, &s))
                    .unwrap_or(0);
                InvitePreview {
                    valid: true,
                    republic: i.republic.as_str().into(),
                    rule: format!("{}-of-{}", i.threshold, i.members).into(),
                    inviter: i.inviter.as_str().into(),
                    missing_relays: missing,
                }
            }
            Err(_) => InvitePreview::default(),
        });
        // the Restore wizard's one link field: which of the two flows is
        // this link for? Pure, like parse_invite, so the panel re-reads it
        // on every keystroke without any state to keep in sync. The relay
        // deviation rides along: relays do not federate, so a link whose
        // pool this node does not share is a hard blocker worth showing
        // BEFORE the run can fail on it.
        let weak_cl = ui.as_weak();
        ui.on_classify_link(move |s| {
            let missing = |relays: &[String]| -> (i32, slint::SharedString) {
                let Some(ui) = weak_cl.upgrade() else { return (0, "".into()) };
                let have: Vec<String> =
                    ui.get_relay_rows().iter().map(|r| r.url.to_string()).collect();
                let miss: Vec<&String> =
                    relays.iter().filter(|u| !have.contains(u)).collect();
                (
                    i32::try_from(miss.len()).unwrap_or(0),
                    miss.first().map(|u| u.as_str()).unwrap_or("").into(),
                )
            };
            match link_kind(&s) {
                LinkKind::Invite { republic, inviter } => {
                    let (n, first) = molt_engine::FoundingInvite::parse(s.trim())
                        .map(|inv| missing(&inv.handover.relays))
                        .unwrap_or((0, "".into()));
                    LinkPreview {
                        kind: 1,
                        republic: republic.into(),
                        who: inviter.into(),
                        missing: n,
                        missing_first: first,
                    }
                }
                LinkKind::Recovery { republic, member } => {
                    let (n, first) = molt_engine::RecoveryInvite::parse(s.trim())
                        .and_then(|inv| inv.handover.map(|h| missing(&h.relays)))
                        .unwrap_or((0, "".into()));
                    LinkPreview {
                        kind: 2,
                        republic: republic.into(),
                        who: member.into(),
                        missing: n,
                        missing_first: first,
                    }
                }
                LinkKind::Unrecognized => LinkPreview::default(),
            }
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

    // The Multisig-Wiki mock's state machine + its WikiState bridge —
    // UI-local by design, EXCEPT the changeset vote: that one proposes on
    // the real gated Memory surface, so it is wired with the handles.
    let (wiki_model, wiki_last) = wire_wiki(&ui);
    wire_wiki_vote(&ui, &rt, &wallet, &wiki_model, &wiki_last);
    wire_patch_view(&ui);
    wire_wiki_export(&ui, &rt, &wallet);

    // --- actions: each becomes a Command on the shared engine ---
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_navigate(move |screen| {
            issue(
                &rt,
                &w,
                &weak,
                Command::Navigate {
                    screen: to_screen(screen),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_set_language(move |idx| {
            let lang = if idx == 1 { "de" } else { "en" }.to_string();
            issue(&rt, &w, &weak, Command::SetLanguage { lang });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_set_fonts(move |app, nav, editor| {
            issue(
                &rt,
                &w,
                &weak,
                Command::SetFonts {
                    app: u16::try_from(app).unwrap_or(14),
                    nav: u16::try_from(nav).unwrap_or(13),
                    editor: u16::try_from(editor).unwrap_or(14),
                },
            );
        });
    }
    {
        // The in-app ThemeSwitch fires Theme.picked; route it to a command so the
        // theme change round-trips through the engine (co-equal with MCP).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.global::<Theme>().on_picked(move |i| {
            issue(
                &rt,
                &w,
                &weak,
                Command::SetTheme {
                    theme: theme_name(i),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.on_save_settings(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui, &stored_settings(&last));
            let wake = ui.get_cfg_poke_wake().to_string();
            issue_draft(&rt, &w, &ui.as_weak(), wake, settings);
        });
    }
    {
        // Rotate the MCP token: mint a fresh one, drop it into the draft, and
        // persist the settings in one go (Slint cannot generate randomness).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.on_rotate_token(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // a failed mint leaves the OLD token in place: overwriting it
            // with "" would silently disable MCP authentication on save
            let Ok(token) = molt_config::random_token() else {
                ui.invoke_show_toast_error(Strings::get(&ui).get_set_token_failed());
                return;
            };
            ui.set_cfg_mcp_token(token.into());
            let settings = read_settings_draft(&ui, &stored_settings(&last));
            let wake = ui.get_cfg_poke_wake().to_string();
            issue_draft(&rt, &w, &ui.as_weak(), wake, settings);
        });
    }
    {
        // Probe the S3 backup target in the draft (not the saved settings),
        // so the user can validate endpoint + credentials before saving.
        // The engine runs a real SigV4-signed HEAD over the configured
        // dialer; the verdict streams back into `cfg-s3-test`.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_test_s3(move |target| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // one account for every bucket — only the bucket differs, and
            // all of it comes from the DRAFT so an unsaved edit is probed
            let (target, bucket) = if target == "media" {
                (molt_core::S3Target::Media, ui.get_cfg_media_s3_bucket())
            } else {
                (molt_core::S3Target::Workspaces, ui.get_cfg_s3_bucket())
            };
            issue(
                &rt,
                &w,
                &ui.as_weak(),
                Command::NetTestS3 {
                    target,
                    endpoint: ui.get_cfg_s3_endpoint().to_string(),
                    access_key: ui.get_cfg_s3_access().to_string(),
                    secret_key: ui.get_cfg_s3_secret().to_string(),
                    bucket: bucket.to_string(),
                },
            );
        });
    }
    {
        // Probe Tor with the anonymity values in the DRAFT (not the saved
        // settings): changing them is restart-required, so the user asking
        // "is Tor actually there?" has normally not saved yet. The engine
        // runs the two-rung ladder off-actor and streams the rung it reached
        // back into `cfg-tor-test`.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_test_tor(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let (network, mode, port) = tor_probe_args(
                ui.get_cfg_network_index(),
                ui.get_cfg_tor_mode_index(),
                ui.get_cfg_tor_port(),
            );
            issue(
                &rt,
                &w,
                &ui.as_weak(),
                Command::NetTestTor {
                    network,
                    mode,
                    port,
                },
            );
        });
    }
    {
        // Refresh the backup table's bucket side: a real ListObjectsV2
        // against the SAVED backup target (never a draft — the table shows
        // the configured bucket). Fired on opening the backup tab and by
        // the explicit refresh button; the honest outcome streams back into
        // `cfg-bk-list` and the orphan rows.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_list_backups(move || {
            issue(&rt, &w, &weak, Command::NetListBackups);
        });
    }
    {
        // S7: fetch a bucket-only workspace onto this device — sealed; the
        // outcome arrives on the session notice (backup-fetched/-failed)
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_backup_fetch(move |id| {
            if let Some(ui) = weak.upgrade() {
                ui.set_bk_fetched("".into());
                ui.set_bk_fetch_error("".into());
            }
            issue(&rt, &w, &weak, Command::BackupFetch { id: id.to_string() });
        });
    }
    {
        // Add a relay to the pool. The URL is pre-validated with molt-core's
        // own parser so the message under the field is localized; the engine
        // re-validates and stays the gate. The draft is cleared only once the
        // engine actually accepted the entry.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_add(move |url| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let pool: Vec<String> = ui
                .get_relay_rows()
                .iter()
                .map(|r| r.url.to_string())
                .collect();
            if let Err(msg) = relay_add_check(ui.get_lang_index(), url.as_str(), &pool) {
                ui.set_relay_error(msg.into());
                return;
            }
            ui.set_relay_error("".into());
            let w = w.clone();
            let weak = ui.as_weak();
            rt.spawn(async move {
                let res = w
                    .execute(Command::RelayAdd {
                        url: url.to_string(),
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    match res {
                        Ok(_) => {
                            ui.set_relay_draft("".into());
                            ui.set_relay_error("".into());
                        }
                        // an engine refusal the local check did not foresee
                        // (a concurrent MCP edit, a future rule) belongs
                        // under the field verbatim, never nowhere
                        Err(e) => ui.set_relay_error(e.to_string().into()),
                    }
                });
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_remove(move |url| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RelayRemove {
                    url: url.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_move(move |url, up| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RelayMove {
                    url: url.to_string(),
                    up,
                },
            );
        });
    }
    {
        // `accept_clearnet` rides in from the GUI's warning dialog — the
        // engine enforces it either way, so an MCP agent faces the same gate.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_confirm(move |url, accept_clearnet| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RelayConfirm {
                    url: url.to_string(),
                    accept_clearnet,
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_revoke(move |url| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RelayRevoke {
                    url: url.to_string(),
                },
            );
        });
    }
    {
        // Session-only clearnet activation: never persisted, so a restart
        // re-arms the gate by itself.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_relay_clearnet_session(move |unlock| {
            issue(&rt, &w, &weak, Command::RelayClearnetSession { unlock });
        });
    }
    {
        // Leaving settings is guarded: a clean draft navigates straight back;
        // a dirty one raises the unsaved-changes modal (save / discard / stay).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.on_close_settings(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let dirty = last
                .lock()
                .ok()
                .and_then(|l| l.clone())
                .is_some_and(|s| settings_draft_differs(&s, &ui));
            if dirty {
                ui.set_confirm_leave_open(true);
            } else {
                issue(
                    &rt,
                    &w,
                    &ui.as_weak(),
                    Command::Navigate {
                        screen: to_screen(ui.get_settings_return()),
                    },
                );
            }
        });
    }
    {
        // Modal "Save & continue": persist the draft, then leave — but only
        // when the engine accepted it (a validation error keeps the user on
        // the settings screen, with the error as a toast).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.on_save_and_leave(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui, &stored_settings(&last));
            let wake = ui.get_cfg_poke_wake().to_string();
            let screen = to_screen(ui.get_settings_return());
            let w = w.clone();
            let weak = ui.as_weak();
            rt.spawn(async move {
                match save_draft(&w, wake, settings).await {
                    Ok(()) => {
                        let _ = w.execute(Command::Navigate { screen }).await;
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_show_toast_error(error_toast(&ui, &e));
                            }
                        });
                    }
                }
            });
        });
    }
    {
        // Modal "Discard & continue": reset the draft to the live session
        // values, then leave.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.on_discard_and_leave(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if let Some(s) = last.lock().ok().and_then(|l| l.clone()) {
                apply_settings_fields(&ui, &s);
            }
            issue(
                &rt,
                &w,
                &ui.as_weak(),
                Command::Navigate {
                    screen: to_screen(ui.get_settings_return()),
                },
            );
        });
    }
    // Quit confirmed from the modal: end the Slint event loop so `ui.run()`
    // returns and the process shuts down.
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    // Intercept the OS/WM window close: with a workspace open OR any engine
    // run (restore / founding / join) in flight, keep the window and raise
    // the quit-confirm modal instead of closing outright (the in-app × is
    // disabled during a run; the WM button must not be a silent bypass).
    // Likewise, quitting from the settings screen with unsaved draft edits
    // raises the save/discard/stay modal instead of silently dropping them.
    {
        let weak = ui.as_weak();
        let last = last_settings.clone();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_screen() == AppScreen::Main || ui.get_run_active() {
                    ui.set_confirm_quit_open(true);
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
                let dirty = ui.get_screen() == AppScreen::Settings
                    && last
                        .lock()
                        .ok()
                        .and_then(|l| l.clone())
                        .is_some_and(|s| settings_draft_differs(&s, &ui));
                if dirty {
                    ui.set_confirm_leave_open(true);
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_open_workspace(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::OpenWorkspace { id: id.to_string() },
            );
        });
    }
    // real at-rest sealing (S6) — same commands as the MCP
    // encrypt_/decrypt_workspace tools; the engine verifies the phrase
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_encrypt_workspace(move |id, phrase| {
            issue(
                &rt,
                &w,
                &weak,
                Command::EncryptWorkspace {
                    id: id.to_string(),
                    phrase: phrase.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_decrypt_workspace(move |id, phrase| {
            issue(
                &rt,
                &w,
                &weak,
                Command::DecryptWorkspace {
                    id: id.to_string(),
                    phrase: phrase.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_close_workspace(move || {
            issue(&rt, &w, &weak, Command::CloseWorkspace);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_delete_workspace(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::DeleteWorkspace { id: id.to_string() },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_set_ws_backup(move |id, enabled| {
            issue(
                &rt,
                &w,
                &weak,
                Command::SetWorkspaceBackup {
                    id: id.to_string(),
                    enabled,
                },
            );
        });
    }
    // the real manual export — same command as the MCP export_workspace
    // tool; the honest outcome streams back via the session's export state
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_export_workspace(move |id, dest, passphrase| {
            issue(
                &rt,
                &w,
                &weak,
                Command::ExportWorkspace {
                    id: id.to_string(),
                    dest: dest.to_string(),
                    passphrase: passphrase.to_string(),
                },
            );
        });
    }
    // Sort the Open list by a header column (view-local: only the mirrored
    // model is reordered; push_session re-applies the sort on every refresh).
    {
        let weak = ui.as_weak();
        ui.on_sort_workspaces(move |key, desc| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut items: Vec<WorkspaceItem> = ui.get_ws_list().iter().collect();
            sort_ws_items(&mut items, key.as_str(), desc);
            ui.set_ws_list(ModelRc::new(VecModel::from(items)));
        });
    }
    // Same idiom for the settings backup table.
    {
        let weak = ui.as_weak();
        ui.on_sort_backups(move |key, desc| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut items: Vec<BackupRow> = ui.get_bk_rows().iter().collect();
            sort_bk_rows(&mut items, key.as_str(), desc);
            ui.set_bk_rows(ModelRc::new(VecModel::from(items)));
        });
    }
    // The Organization tables' sort/filter. View-local presentation like
    // the Open/backup lists — but these mirrored rows are rebuilt from the
    // engine on every push, so the state lives in ChatUiState (toggle in
    // Rust, single writer) and push_surfaces re-applies it each time; the
    // engine's ReadMembers/ReadUploads stay the full projections for MCP.
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_sort_members(move |column| {
            if let Ok(mut st) = chat_ui.lock() {
                st.sort_members_by(column.as_str());
            }
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_sort_uploads(move |column| {
            if let Ok(mut st) = chat_ui.lock() {
                st.sort_uploads_by(column.as_str());
            }
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_filter_uploads(move |needle| {
            if let Ok(mut st) = chat_ui.lock() {
                st.set_uploads_filter(needle.to_string());
            }
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        // The proposal-outcome lists' pager (Organization → Declined, the
        // gated surfaces' applied log): step the UI-local page, then
        // re-push — the push clamps against the list's current length and
        // echoes "page x of y" back into the surface tab.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_page_list(move |surface, list, delta| {
            if let Ok(mut st) = chat_ui.lock() {
                st.page_list_by(surface.as_str(), list.as_str(), delta);
            }
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        // A member row's uploads count: jump to Organization → Uploads
        // pre-filtered to that member. The view switch is the same engine
        // command the nav issues; the filter itself stays single-writer in
        // ChatUiState and the push echoes it into the filter box.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_jump_member_uploads(move |member| {
            if let Ok(mut st) = chat_ui.lock() {
                st.set_uploads_filter(member.to_string());
            }
            issue(
                &rt,
                &w,
                &weak,
                Command::SelectView {
                    surface: Surface::Organization,
                    view: "uploads".to_string(),
                },
            );
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_restore_start(move |way, target, secret| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RestoreStart {
                    way: way.to_string(),
                    target: target.to_string(),
                    secret: secret.to_string(),
                    // the GUI's default collision policy is the safe refuse
                    // (design P2); an explicit replace goes through MCP
                    replace: false,
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_restore_cancel(move || {
            issue(&rt, &w, &weak, Command::RestoreCancel);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_restore_finish(move || {
            issue(&rt, &w, &weak, Command::RestoreFinish);
        });
    }
    {
        let rt_adopt = rt.clone();
        let w_adopt = wallet.clone();
        let weak_adopt = ui.as_weak();
        ui.on_adopt_invite_relays(move |link| {
            let Ok(inv) = molt_engine::FoundingInvite::parse(&link) else {
                return;
            };
            for url in inv.handover.relays {
                issue(&rt_adopt, &w_adopt, &weak_adopt, Command::RelayAdd { url: url.clone() });
                // an ONION relay needs no exposure decision — confirm it
                // outright. A clearnet one keeps its acknowledgement: making
                // the convenient path the less private one is exactly what
                // this button must not do.
                if molt_core::relay::relay_kind(&url) == molt_core::relay::RelayKind::Onion {
                    issue(
                        &rt_adopt,
                        &w_adopt,
                        &weak_adopt,
                        Command::RelayConfirm { url, accept_clearnet: false },
                    );
                }
            }
        });
    }
    {
        let st_toggle = chat_ui.clone();
        let weak_toggle = ui.as_weak();
        ui.on_cw_toggle_relay(move |idx| {
            let Ok(mut st) = st_toggle.lock() else { return };
            let rows = st.create_pick_rows();
            let Some((url, _)) = rows.get(usize::try_from(idx).unwrap_or(0)) else {
                return;
            };
            st.toggle_create_relay(url.clone());
            let rows: Vec<RelayPick> = st
                .create_pick_rows()
                .into_iter()
                .map(|(url, picked)| RelayPick { url: url.into(), picked })
                .collect();
            if let Some(ui) = weak_toggle.upgrade() {
                ui.set_cw_relay_picks(slint::ModelRc::new(slint::VecModel::from(rows)));
            }
        });
    }
    {
        // the phrase-backup gates (create step 4, join finish): the re-typed
        // phrase must MATCH, but whitespace runs and letter case never block
        // an honest re-type
        ui.on_seed_matches(|typed, expected| {
            let norm = |s: &str| {
                s.split_whitespace()
                    .map(str::to_lowercase)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            !typed.trim().is_empty() && norm(&typed) == norm(&expected)
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let st_pick = chat_ui.clone();
        ui.on_create_start(move |name, member, threshold, members| {
            // the founder's pick: every dialable relay the wizard did not
            // deselect. Empty means "no explicit choice" to the engine, which
            // is exactly right when nothing was deselected.
            let picked = st_pick
                .lock()
                .ok()
                .map(|st| st.create_pick())
                .unwrap_or_default();
            issue(
                &rt,
                &w,
                &weak,
                Command::CreateStart {
                    name: name.to_string(),
                    member: member.to_string(),
                    threshold: u8::try_from(threshold).unwrap_or(0),
                    members: u8::try_from(members).unwrap_or(0),
                    relays: picked,
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_create_cancel(move || {
            issue(&rt, &w, &weak, Command::CreateCancel);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_create_finish(move || {
            issue(&rt, &w, &weak, Command::CreateFinish);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        // the debounced wiki-draft persist (WP-D) — an opaque blob hop
        ui.on_wiki_draft_save(move |draft| {
            issue(
                &rt,
                &w,
                &weak,
                Command::WikiDraftSave {
                    draft: draft.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        // workspace entry: fetch the stored draft, hand it to the wiki
        // model over the WikiState bridge (the completion is Send-bound)
        ui.on_wiki_draft_load(move || {
            let w = w.clone();
            let weak2 = weak.clone();
            rt.spawn(async move {
                let draft = match w.execute(Command::WikiDraftLoad).await {
                    Ok(Reply::WikiDraft { draft }) => draft,
                    _ => String::new(),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak2.upgrade() {
                        ui.global::<WikiState>().invoke_draft_loaded(draft.into());
                    }
                });
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        // the ❻½ phrase-backup confirmation — founder or joiner, the
        // engine routes by the running ritual (a mismatch surfaces as an
        // honest error toast)
        ui.on_confirm_seed_backup(move |phrase| {
            issue(
                &rt,
                &w,
                &weak,
                Command::ConfirmSeedBackup {
                    phrase: phrase.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_create_propose(move |name, agenda| {
            // the wizard's checkbox selection; the engine canonicalizes
            let features = weak
                .upgrade()
                .map(|ui| {
                    // quests/vault/wallet have no wizard checkbox (locked
                    // off, not built) — no property to read until they ship
                    [(ui.get_cw_feat_memory(), "memory")]
                    .into_iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, key)| key.to_string())
                    .collect()
                })
                .unwrap_or_default();
            issue(
                &rt,
                &w,
                &weak,
                Command::CreatePropose {
                    name: name.to_string(),
                    agenda: agenda.to_string(),
                    features,
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_start(move |invite, member| {
            // not the plain issue(): a REFUSED start (bad link, no relay,
            // already running) must re-arm the optimistic jw-starting latch,
            // or the join button stays dead with nothing running. An accepted
            // start needs no reset here — the engine session flips jw-step
            // and the form is gone.
            let w = w.clone();
            let weak = weak.clone();
            let cmd = Command::JoinStart {
                invite: invite.to_string(),
                member: member.to_string(),
            };
            rt.spawn(async move {
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_jw_starting(false);
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_cancel(move || {
            // leaving the run re-arms the start latch: the form comes back
            // with a clickable button
            if let Some(ui) = weak.upgrade() {
                ui.set_jw_starting(false);
            }
            issue(&rt, &w, &weak, Command::JoinCancel);
        });
    }
    // recovery (total-loss rejoin): the coordinator mints a link for an
    // anchored seat; the returning member rejoins from link + phrase. Both
    // are human decisions — tools on both surfaces, co-equal with MCP.
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_recover_invite(move |member| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RecoverInviteStart {
                    member: member.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_recover_start(move |link, phrase| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RecoverStart {
                    link: link.to_string(),
                    phrase: phrase.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_confirm_charter(move || {
            issue(&rt, &w, &weak, Command::JoinConfirmCharter);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_decline_charter(move || {
            issue(&rt, &w, &weak, Command::JoinDeclineCharter);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_finish(move || {
            issue(&rt, &w, &weak, Command::JoinFinish);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_select_surface(move |key| {
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            issue(&rt, &w, &weak, Command::SelectSurface { surface });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_select_view(move |key, view| {
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            issue(
                &rt,
                &w,
                &weak,
                Command::SelectView {
                    surface,
                    view: view.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_send_chat(move |body, quote| {
            let body = body.trim().to_string();
            if body.is_empty() {
                return;
            }
            // "" = no quote; a legacy row without an id can't be quoted
            let quote = quote.parse::<MessageId>().ok();
            // compose files into the channel this window has selected
            let channel = chat_ui
                .lock()
                .ok()
                .map(|s| s.selected.clone())
                .unwrap_or_default();
            issue(&rt, &w, &weak, Command::Chat { body, quote, channel });
        });
    }
    {
        // Channel selection (chat bus). UI-LOCAL state, not a session
        // command: the filter itself is engine-side (`ReadState{channel}`),
        // so co-equality holds — an MCP agent passes its own filter and
        // neither operator can hijack the other's view. The canonical key
        // is echoed back into `selected-channel` (single writer: Rust).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_select_channel(move |key| {
            let Some(ch) = parse_channel_key(&key) else {
                return;
            };
            // topics normalize on selection exactly as on send (trim, cap);
            // a rejected name is told, not silently swallowed
            let ch = match ch.normalized() {
                Ok(ch) => ch,
                Err(e) => {
                    if let Some(ui) = weak.upgrade() {
                        // a normalization refusal is a plain String — wrap
                        // it as the payload error it is, so it localizes
                        ui.invoke_show_toast_error(error_toast(
                            &ui,
                            &molt_core::MoltError::BadPayload(e),
                        ));
                    }
                    return;
                }
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_selected_channel(channel_key(&ch).as_str().into());
                ui.set_selected_channel_votable(matches!(ch, ChannelRef::Patch { .. }));
                // instant banner feedback — for a fresh (still empty) topic
                // this is the only visible signal until its first message
                // exists; the next push refreshes it with the lazy title
                ui.set_selected_channel_label(
                    channel_display_label(&ch, &HashMap::new()).as_str().into(),
                );
                // …and the read-only flag from the proposal cache, so the
                // compose row collapses on the click, not a push later (the
                // push then re-decides from the engine's annotation)
                let (closed, org) = chat_ui
                    .lock()
                    .map(|st| {
                        (
                            selected_channel_closed(&ch, &[], &st.proposals),
                            selected_channel_org(&ch, &st.proposals),
                        )
                    })
                    .unwrap_or((false, false));
                ui.set_selected_channel_closed(closed);
                // instant, like `closed`: the nav must not collapse the
                // section the click came from while the push is in flight
                ui.set_selected_channel_org(org);
            }
            if let Ok(mut st) = chat_ui.lock() {
                // bumps the push generation: every in-flight push read
                // for the previous selection is stale from this moment
                st.select(ch);
            }
            // re-read through the engine filter (the point of the bus)
            let w = w.clone();
            let weak = weak.clone();
            let chat_ui = chat_ui.clone();
            rt.spawn(async move {
                push_surfaces(&w, &weak, &chat_ui).await;
            });
        });
    }
    {
        // "back to the vote" from a patch channel's banner: the selected
        // channel names the proposal, the proposal cache names its hosting
        // surface — the jump reuses the sidebar's own SelectView /
        // SelectSurface commands (no new engine verb).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_jump_to_vote(move || {
            let Some(cmd) = chat_ui
                .lock()
                .ok()
                .and_then(|st| vote_jump_command(&st.selected, &st.proposals))
            else {
                return;
            };
            issue(&rt, &w, &weak, cmd);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_delete_chat(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            issue(&rt, &w, &weak, Command::DeleteChat { id });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        let chat_ui = chat_ui.clone();
        ui.on_share_pick(move || {
            let w = w.clone();
            let weak = weak.clone();
            // a share files into the channel this window has selected —
            // captured at click time (the view the sharer was looking at),
            // same source as compose (concept Q8)
            let channel = chat_ui
                .lock()
                .ok()
                .map(|s| s.selected.clone())
                .unwrap_or_default();
            // the native picker runs async (XDG portal) off the UI thread;
            // the engine derives the metadata + real sha256 from this path
            // and posts the share when hashing completes
            rt.spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new().pick_file().await else {
                    return; // cancelled
                };
                let cmd = Command::ShareFile {
                    path: file.path().display().to_string(),
                    channel,
                };
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_download_file(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            let w = w.clone();
            let weak = weak.clone();
            // save-dialog per download (product decision): the user picks
            // the destination, then the engine fetches peer-to-peer;
            // completion/failure surfaces via Event::FileTransfer
            rt.spawn(async move {
                let Some(dest) = rfd::AsyncFileDialog::new().save_file().await else {
                    return; // cancelled
                };
                let cmd = Command::DownloadFile {
                    id,
                    dest: Some(dest.path().display().to_string()),
                };
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_remove_file(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            let msg = weak
                .upgrade()
                .map(|ui| ui.global::<Strings>().get_toast_file_removed().to_string())
                .unwrap_or_default();
            issue_then_toast(&rt, &w, &weak, Command::RemoveFile { id }, msg);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_toggle_reaction(move |id, emoji| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            issue(
                &rt,
                &w,
                &weak,
                Command::ReactChat {
                    id,
                    emoji: emoji.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_propose(move |key, title| {
            let title = title.trim().to_string();
            if title.is_empty() {
                return;
            }
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            if !surface.is_gated() {
                return;
            }
            let payload = serde_json::json!({ "op": default_op(surface), "title": title });
            issue(&rt, &w, &weak, Command::Propose { surface, payload });
        });
    }
    // R6 pool-edit modal: the draft is a row table. Seed copies the
    // effective pool, add validates through molt-core's own parser (the
    // same gate the engine applies — and against the DRAFT, so a queued
    // duplicate is caught too), remove drops a row. The space-joined
    // `org-relays-draft` string stays the org-propose payload, so the
    // propose path below is untouched.
    {
        let weak = ui.as_weak();
        ui.on_relays_draft_seed(move || {
            let Some(ui) = weak.upgrade() else { return };
            let rows: Vec<String> = ui.get_org_relays().iter().map(|s| s.to_string()).collect();
            set_relay_draft_rows(&ui, &rows);
            ui.set_org_relay_add_draft("".into());
            ui.set_org_relay_add_error("".into());
        });
        let weak = ui.as_weak();
        ui.on_relays_draft_add(move |url| {
            let Some(ui) = weak.upgrade() else { return };
            let mut rows: Vec<String> = ui
                .get_org_relays_draft_rows()
                .iter()
                .map(|s| s.to_string())
                .collect();
            match relay_add_check(ui.get_lang_index(), url.as_str(), &rows) {
                Err(msg) => ui.set_org_relay_add_error(msg.into()),
                Ok(canon) => {
                    rows.push(canon);
                    set_relay_draft_rows(&ui, &rows);
                    ui.set_org_relay_add_draft("".into());
                    ui.set_org_relay_add_error("".into());
                }
            }
        });
        let weak = ui.as_weak();
        ui.on_relays_draft_remove(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let mut rows: Vec<String> = ui
                .get_org_relays_draft_rows()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let Ok(i) = usize::try_from(i) else { return };
            if i < rows.len() {
                rows.remove(i);
                set_relay_draft_rows(&ui, &rows);
                // a removed row may end the condition an add error named
                ui.set_org_relay_add_error("".into());
            }
        });
    }
    // an Organization change from the status screen's edit modals (charter /
    // image): the same Command::Propose the MCP propose tool drives — the
    // drafted value rides along under "value", the display title under
    // "title" (what the pending cards summarize). A set_image reads the
    // picked file OFF the UI thread and embeds the bytes as base64
    // (sign-what-you-see: members vote on the actual image; the engine
    // refuses anything over its cap with an honest error toast).
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_org_propose(move |op, value| {
            if op.as_str() == "set_image" {
                let w = w.clone();
                let weak = weak.clone();
                let path = value.to_string();
                rt.spawn(async move {
                    let read = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || std::fs::read(&path)
                    })
                    .await;
                    let payload = match read {
                        Ok(Ok(bytes)) => {
                            use base64::Engine as _;
                            // WP3 pre-check with the REAL preview decoder:
                            // instant, localized feedback instead of an
                            // engine-error round-trip. The engine's co-equal
                            // sniff (molt-engine proposals.rs
                            // `image_decodable`) still guards the command
                            // path for every frontend — deliberate
                            // duplication, each side references the other.
                            if image_from_bytes(&bytes).is_none() {
                                let weak = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = weak.upgrade() {
                                        let msg =
                                            ui.global::<Strings>().get_pc_img_missing();
                                        ui.invoke_show_toast(msg);
                                    }
                                });
                                return;
                            }
                            let name = std::path::Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.clone());
                            // no baked display title: the op is the
                            // language-neutral placeholder every UI
                            // translates at render time (display_title)
                            serde_json::json!({
                                "op": "set_image",
                                "value": name,
                                "bytes_b64":
                                    base64::engine::general_purpose::STANDARD.encode(bytes),
                            })
                        }
                        _ => {
                            // no Debug dump at the user: the one important
                            // thing is WHICH file did not read
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    let msg = format!(
                                        "\u{26a0} {} {path}",
                                        ui.global::<Strings>().get_toast_file_unreadable()
                                    );
                                    ui.invoke_show_toast_error(msg.into());
                                }
                            });
                            return;
                        }
                    };
                    // the confirmation belongs to the OUTCOME: this path
                    // can still fail on the engine's own decode sniff or
                    // the payload cap, and a "Proposed" toast on the click
                    // followed by an error described a proposal that never
                    // existed
                    let outcome = w
                        .execute(Command::Propose {
                            surface: Surface::Organization,
                            payload,
                        })
                        .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        match outcome {
                            Ok(_) => {
                                let msg = ui.global::<Strings>().get_toast_proposed();
                                ui.invoke_show_toast(msg);
                            }
                            Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
                        }
                    });
                });
                return;
            }
            let payload = serde_json::json!({
                "op": op.as_str(),
                "value": value.as_str(),
            });
            let msg = weak
                .upgrade()
                .map(|ui| ui.global::<Strings>().get_toast_proposed().to_string())
                .unwrap_or_default();
            issue_then_toast(
                &rt,
                &w,
                &weak,
                Command::Propose {
                    surface: Surface::Organization,
                    payload,
                },
                msg,
            );
        });
    }
    // Organization → Members: the OWN seat's profile. `set_member_image`
    // reads the picked file OFF the UI thread, fits it to what this
    // republic still carries (square + budget) and embeds the bytes;
    // everything else is a plain payload. The engine refuses a profile op
    // proposed for another seat, so `member` is always the own one.
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_member_propose(move |op, member, value| {
            if op.as_str() != "set_member_image" {
                // a removal carries no value at all - the payload is what
                // the members sign, so it says only what it changes
                let payload = if op.as_str() == "remove_member_image" {
                    serde_json::json!({ "op": op.as_str(), "member": member.as_str() })
                } else {
                    serde_json::json!({
                        "op": op.as_str(),
                        "member": member.as_str(),
                        "value": value.as_str(),
                    })
                };
                let msg = weak
                    .upgrade()
                    .map(|ui| ui.global::<Strings>().get_toast_proposed().to_string())
                    .unwrap_or_default();
                issue_then_toast(
                    &rt,
                    &w,
                    &weak,
                    Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    },
                    msg,
                );
                return;
            }
            let budget = weak
                .upgrade()
                .map(|ui| usize::try_from(ui.get_mp_img_budget()).unwrap_or(0))
                .unwrap_or(0);
            let w = w.clone();
            let weak = weak.clone();
            let member = member.to_string();
            let path = value.to_string();
            rt.spawn(async move {
                let read = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || std::fs::read(&path)
                })
                .await;
                let Ok(Ok(bytes)) = read else {
                    // no Debug dump at the user: the one important thing is
                    // WHICH file did not read
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let msg = format!(
                                "\u{26a0} {} {path}",
                                ui.global::<Strings>().get_toast_file_unreadable()
                            );
                            ui.invoke_show_toast_error(msg.into());
                        }
                    });
                    return;
                };
                // the crop/downscale is CPU work on a picture up to 8192²
                let fitted =
                    tokio::task::spawn_blocking(move || fit_member_image(&bytes, budget)).await;
                let fitted = match fitted {
                    Ok(Ok(fitted)) => fitted,
                    Ok(Err(why)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let s = ui.global::<Strings>();
                                ui.invoke_show_toast_error(match why {
                                    ImageFitError::Undecodable => s.get_pc_img_missing(),
                                    ImageFitError::TooLarge => s.get_mp_img_too_big(),
                                });
                            }
                        });
                        return;
                    }
                    Err(_) => return,
                };
                use base64::Engine as _;
                // the name must match the bytes: the engine derives the
                // avatar file's extension from this display value
                let stem = std::path::Path::new(&path)
                    .file_stem()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| member.clone());
                let payload = serde_json::json!({
                    "op": "set_member_image",
                    "member": member,
                    "value": format!("{stem}.{}", fitted.ext),
                    "bytes_b64":
                        base64::engine::general_purpose::STANDARD.encode(fitted.bytes),
                });
                // the confirmation belongs to the OUTCOME: the engine's own
                // gates (square, budget, the seat) still run after this
                let outcome = w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match outcome {
                        Ok(_) => {
                            let msg = ui.global::<Strings>().get_toast_proposed();
                            ui.invoke_show_toast(msg);
                        }
                        Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
                    }
                });
            });
        });
    }
    // pick the own seat's picture — same picker set as the republic image
    {
        let rt = rt.clone();
        let weak = ui.as_weak();
        ui.on_mp_img_pick(move || {
            let weak = weak.clone();
            rt.spawn(async move {
                let picker = rfd::AsyncFileDialog::new()
                    // no "svg": the engine refuses it, and a square check
                    // on a vector is meaningless
                    .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif", "bmp"]);
                let Some(file) = picker.pick_file().await else {
                    return; // cancelled
                };
                let path = file.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_mp_img_draft(path.into());
                    }
                });
            });
        });
    }
    // sound preview in the settings panel — plays the picked alert once
    {
        ui.on_test_sound(move |kind| {
            play_alert(kind.as_str());
        });
    }
    // pick a new republic image via the native file dialog (async XDG
    // portal, like the chat share picker) — only the path lands in the
    // draft; proposing it ships the file REFERENCE, never bytes
    {
        let rt = rt.clone();
        let weak = ui.as_weak();
        ui.on_org_logo_pick(move || {
            let weak = weak.clone();
            rt.spawn(async move {
                let picker = rfd::AsyncFileDialog::new()
                    // no "svg" (L1, 2026-08-16): the engine refuses SVG proposals —
                    // offering a format the vote will bounce is a trap
                    .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif", "bmp"]);
                let Some(file) = picker.pick_file().await else {
                    return; // cancelled
                };
                let path = file.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_org_logo_draft(path.into());
                    }
                });
            });
        });
    }
    // browse for the workspace folder via the native dialog (async XDG
    // portal, like the logo picker) — the picked path lands in the modal's
    // draft field, which stays hand-editable as a fallback
    {
        let rt = rt.clone();
        let weak = ui.as_weak();
        ui.on_ws_dir_pick(move || {
            let weak = weak.clone();
            // only the property read happens on the UI thread; the stat in
            // browse_start_dir moves to a blocking task (a draft pointing at
            // a hung mount must not freeze the event loop)
            let draft = weak
                .upgrade()
                .map(|ui| ui.get_ws_dir_draft().to_string())
                .unwrap_or_default();
            rt.spawn(async move {
                let start_dir = tokio::task::spawn_blocking(move || browse_start_dir(&draft))
                    .await
                    .ok()
                    .flatten();
                let mut picker = rfd::AsyncFileDialog::new();
                if let Some(dir) = start_dir {
                    picker = picker.set_directory(dir);
                }
                let Some(folder) = picker.pick_folder().await else {
                    return; // cancelled
                };
                let path = folder.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_ws_dir_draft(path.into());
                    }
                });
            });
        });
    }
    // the proposed image behind a pending set_image: the bytes RODE the
    // proposal payload (sign-what-you-see), so the viewer decodes them
    // locally on every member's device — no transfer, no proposer needed.
    // Shown INLINE in the proposal's card; the same id toggles it off.
    {
        let weak = ui.as_weak();
        ui.on_view_proposal_image(move |id, img_b64| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_img_inline_id() == id {
                ui.set_img_inline_id(-1);
                return;
            }
            match proposal_image_from_b64(img_b64.as_str()) {
                Some(img) => {
                    ui.set_img_inline_src(img);
                    ui.set_img_inline_id(id);
                }
                None => {
                    let s = ui.global::<Strings>();
                    ui.invoke_show_toast_error(s.get_pc_img_missing());
                }
            }
        });
    }
    // save the ORIGINAL proposed-image bytes (no re-encode) wherever the
    // save dialog points; the suggested name is the proposal's file-name
    // value. Local bytes → the write happens right here, no engine hop.
    {
        let rt = rt.clone();
        let weak = ui.as_weak();
        ui.on_save_proposal_image(move |img_b64, name| {
            use base64::Engine as _;
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // an empty/absent payload decodes to zero bytes — that is a
            // missing image, not a file worth a save dialog (a minimal
            // MCP proposal may carry no bytes_b64 at all)
            let bytes = match base64::engine::general_purpose::STANDARD.decode(img_b64.as_str()) {
                Ok(b) if !b.is_empty() => b,
                _ => {
                    let s = ui.global::<Strings>();
                    ui.invoke_show_toast_error(s.get_pc_img_missing());
                    return;
                }
            };
            let saved_prefix = ui.global::<Strings>().get_toast_dl_done();
            let weak = weak.clone();
            rt.spawn(async move {
                let Some(dest) = rfd::AsyncFileDialog::new()
                    .set_file_name(name.as_str())
                    .save_file()
                    .await
                else {
                    return; // cancelled
                };
                let path = dest.path().to_path_buf();
                let write = tokio::task::spawn_blocking(move || {
                    std::fs::write(&path, &bytes).map(|()| path)
                })
                .await;
                let msg = match write {
                    Ok(Ok(path)) => (format!("{saved_prefix} {}", path.display()), true),
                    Ok(Err(e)) => (format!("⚠ {e}"), false),
                    Err(e) => (format!("⚠ {e}"), false),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let (msg, ok) = msg;
                        if ok {
                            ui.invoke_show_toast(msg.into());
                        } else {
                            ui.invoke_show_toast_error(msg.into());
                        }
                    }
                });
            });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_approve(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::Approve {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_decline(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::Decline {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        // right-click on a member: the directed nudge (co-equal MCP tool: poke).
        // One door for all nine name sites — the Poke global (theme.slint).
        ui.global::<Poke>().on_go(move |member| {
            issue(
                &rt,
                &w,
                &weak,
                Command::Poke {
                    member: member.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        // closing the recovery-link dialog acknowledges the notice that
        // opened it — otherwise the one-shot link re-opens it on the next
        // fresh window (co-equal MCP tool: clear_notice)
        ui.on_clear_notice(move || {
            issue(&rt, &w, &weak, Command::ClearNotice);
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_withdraw(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::Withdraw {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }

    // --- live-mirror: re-read and re-render on every engine change ---
    {
        let weak = ui.as_weak();
        let w = wallet.clone();
        let last_settings = last_settings.clone();
        let chat_ui = chat_ui.clone();
        rt.spawn(async move {
            let mut rx = w.subscribe();
            push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
            push_surfaces(&w, &weak, &chat_ui).await;
            loop {
                match rx.recv().await {
                    Ok(Event::SessionChanged { scope }) => {
                        push_session(&w, &weak, &last_settings, scope, &chat_ui).await;
                        // A Full session change can mean a workspace was
                        // opened or closed — the surface state (replayed
                        // chat history!) changed with it, without any
                        // chat/proposal event firing. Run-scoped ticks
                        // (90 ms) deliberately skip this.
                        if scope == SessionScope::Full {
                            // a Full change can be a workspace open/close:
                            // proposal ids are per-workspace counters, so a
                            // stale inline-viewer id would light up an id-
                            // colliding card in the NEXT workspace with the
                            // previous one's decoded image — drop it
                            let weak2 = weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak2.upgrade() {
                                    ui.set_img_inline_id(-1);
                                }
                            });
                            push_surfaces(&w, &weak, &chat_ui).await;
                        }
                    }
                    // Any surface event (chat / propose / approve / …) re-reads
                    // the surfaces, so the GUI mirrors what an MCP agent did.
                    // An Event::Chat carries id+channel and could tick unread
                    // counters directly, but the re-read stays the single
                    // source of truth — event payloads never drive state.
                    // A finished download additionally toasts its outcome
                    // (the table repaints via the same re-read).
                    // alert sounds: an INCOMING chat message (never our own
                    // echo) and a new vote play the configured alert — read
                    // from the last APPLIED settings, so an unsaved draft
                    // never changes behavior
                    Ok(Event::Chat { from, .. }) => {
                        alert_unless_own(&last_settings, |s| s.sound_message.clone(), &weak, from);
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // only a vote somebody ELSE initiated rings — the
                    // proposer already knows what they just did
                    Ok(Event::Proposed { by, .. }) => {
                        alert_unless_own(&last_settings, |s| s.sound_vote.clone(), &weak, by);
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // a poke addressed to THIS seat toasts who poked and
                    // rings its own sound (the engine already gated opt-in +
                    // cooldown); the sender side confirms quietly. No
                    // push_surfaces — a poke changes no surface state.
                    Ok(Event::Poked { by, to }) => {
                        alert_unless_own(&last_settings, |s| s.sound_poke.clone(), &weak, by.clone());
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let st = ui.global::<Strings>();
                            let me = ui.get_node_member();
                            if to.as_str() == me.as_str() {
                                ui.invoke_show_toast(format!("{by} {}", st.get_toast_poked()).into());
                            } else if by.as_str() == me.as_str() {
                                ui.invoke_show_toast(
                                    format!("{} {to}", st.get_toast_poke_sent()).into(),
                                );
                            }
                        });
                    }
                    // WP4b: checkpoint lifecycle closure for the operator —
                    // sealed toasts the height, stale tells them to re-cut
                    Ok(Event::CheckpointSealed { height, .. }) => {
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let msg = ui.global::<Strings>().get_toast_checkpoint_sealed();
                            ui.invoke_show_toast(format!("{msg} #{height}").into());
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // CheckpointStale is NOT toasted: the automation re-cuts
                    // by itself on the very next commit — a "propose again"
                    // instruction would be noise (the event stays on the
                    // stream for MCP observers)
                    Ok(Event::CheckpointStale { .. }) => {
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(Event::FileTransfer { phase, .. }) => {
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let st = ui.global::<Strings>();
                            match &phase {
                                molt_core::TransferPhase::Done { path } => {
                                    ui.invoke_show_toast(
                                        format!("{} {path}", st.get_toast_dl_done()).into(),
                                    );
                                }
                                molt_core::TransferPhase::Failed { reason } => {
                                    ui.invoke_show_toast_error(
                                        format!("{} {reason}", st.get_toast_dl_failed()).into(),
                                    );
                                }
                                _ => {}
                            }
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(Event::UiActionRequested { action }) => {
                        // gui_over_mcp.md, the drive half: perform the verb
                        // through the SAME callbacks a human's click takes,
                        // then publish — both queue on the event loop, so
                        // the publish claim is post-perform. A pure view
                        // change may not ring the engine, hence the
                        // explicit publish.
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                perform_ui_action(&ui, &action);
                            }
                        });
                        publish_ui_state(&w, &weak).await;
                    }
                    Ok(_) => push_surfaces(&w, &weak, &chat_ui).await,
                    Err(RecvError::Lagged(_)) => {
                        push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    ui.run()
}

/// The directory the workspace-folder browse dialog should start in, given
/// the modal's hand-editable draft: the draft — its leading `~` expanded the
/// same way the engine resolves the setting (`molt_storage::expand_tilde`,
/// so the config default "~/…" starts at the real folder) — when it names an
/// existing directory, otherwise `None`: an empty draft or a typo must not
/// derail the dialog (rfd then opens at its platform default). Runs a stat —
/// call it off the UI thread (the draft may point at a slow mount).
fn browse_start_dir(draft: &str) -> Option<std::path::PathBuf> {
    let path = molt_storage::expand_tilde(draft);
    path.is_dir().then_some(path)
}

/// Map a session workspace into the Slint-side row struct. Member chips
/// render the relative age from the real stamp; a never-seen member (all
/// of them, on a closed workspace) shows a bare chip.
fn workspace_item(lang: i32, now: u64, w: &molt_core::WorkspaceInfo) -> WorkspaceItem {
    let members: Vec<MemberSync> = w
        .members
        .iter()
        .map(|m| MemberSync {
            name: m.name.as_str().into(),
            last: seen_label(lang, now, m.last_seen, "").into(),
            state: i32::from(m.state),
        })
        .collect();
    WorkspaceItem {
        id: w.id.as_str().into(),
        name: w.name.as_str().into(),
        detail: w.detail.as_str().into(),
        status: sync_status_label(lang, w.state, w.last_sync_min, w.sync_queue).into(),
        synced: w.synced,
        state: i32::from(w.state),
        last_sync_min: w.last_sync_min as i32,
        s3: w.s3,
        backup: backup_when_label(lang, w.last_backup_min).into(),
        encrypted: w.encrypted,
        seed: w.seed.as_str().into(),
        net: w.net.as_str().into(),
        members: ModelRc::new(VecModel::from(members)),
    }
}

/// The settings backup table: every local workspace mapped to its bucket
/// backup (if auto-backup is on), then the bucket-only orphans from the
/// last real listing (none until one ran).
fn backup_rows(sv: &SessionView) -> Vec<BackupRow> {
    let lang = i32::from(sv.language == "de");
    // machine sort key for the last-backup column ("never" sorts last)
    fn last_key(min: u32) -> i32 {
        if min == molt_core::WorkspaceInfo::NEVER {
            i32::MAX
        } else {
            i32::try_from(min).unwrap_or(i32::MAX)
        }
    }
    let mut rows: Vec<BackupRow> = sv
        .workspaces
        .iter()
        .map(|w| BackupRow {
            id: w.id.as_str().into(),
            local: w.name.as_str().into(),
            // the bucket-side cell is honest: the last backup attempt's
            // real failure, else what the last real listing saw in the
            // bucket (copies × the id pseudonym — the bucket stores no
            // names), else nothing. The auto-toggle alone claims nothing.
            remote: if !w.backup_error.is_empty() {
                w.backup_error.clone()
            } else if w.backup_copies > 0 {
                format!("{}\u{00d7} \u{00b7} {}", w.backup_copies, short_hex_id(&w.id))
            } else {
                String::new()
            }
            .into(),
            has_local: true,
            auto: w.s3,
            size: size_label(w.size_kib).into(),
            last: backup_when_label(lang, w.last_backup_min).into(),
            size_kib: i32::try_from(w.size_kib).unwrap_or(i32::MAX),
            last_min: last_key(w.last_backup_min),
        })
        .collect();
    rows.extend(sv.backup_orphans.iter().map(|o| BackupRow {
        // the FULL workspace-id pseudonym rides along (the label below is
        // shortened): restore-from-S3 starts from exactly this id
        id: o.id.as_str().into(),
        local: "".into(),
        remote: orphan_remote_label(o).into(),
        has_local: false,
        auto: false,
        size: size_label(o.size_kib).into(),
        last: backup_when_label(lang, o.last_backup_min).into(),
        size_kib: i32::try_from(o.size_kib).unwrap_or(i32::MAX),
        last_min: last_key(o.last_backup_min),
    }));
    rows
}

/// Sort the backup table by a header column ("local"/"remote"/"auto"/
/// "size"/"last"); an empty key keeps the session order. Empty name cells
/// and never-backed-up rows sort last (ascending).
fn sort_bk_rows(items: &mut [BackupRow], key: &str, desc: bool) {
    match key {
        "local" => items.sort_by_key(|r| (r.local.is_empty(), r.local.to_lowercase())),
        "remote" => items.sort_by_key(|r| (r.remote.is_empty(), r.remote.to_lowercase())),
        "auto" => items.sort_by_key(|r| !r.auto),
        "size" => items.sort_by_key(|r| r.size_kib),
        "last" => items.sort_by_key(|r| r.last_min),
        _ => return,
    }
    if desc {
        items.reverse();
    }
}

/// Sort the Open screen's workspace rows by a header column ("name" or
/// "sync"); an empty key keeps the session order. Ascending "sync" puts the
/// most recently synced republic first.
fn sort_ws_items(items: &mut [WorkspaceItem], key: &str, desc: bool) {
    match key {
        "name" => items.sort_by_key(|w| w.name.to_lowercase()),
        "sync" => items.sort_by_key(|w| w.last_sync_min),
        _ => return,
    }
    if desc {
        items.reverse();
    }
}

/// The recovery-flow reading of the transient session notice — the engine's
/// contract for the recovery ritual (`recovery_ritual.md`): a coordinator's
/// mint lifecycle (pending → link | failed), and the rejoiner's
/// started/failed/done lifecycle.
#[derive(Debug, PartialEq, Eq)]
enum RecoverNotice {
    /// Not a recovery notice (every other notice, e.g. "saved").
    None,
    /// Rejoiner: the recovery task's live status line (a bounded wait must
    /// not look dead — `NetRecoverNote`).
    Note(String),
    /// Coordinator: a link mint started for this member — the dialog opens in
    /// its calm pending state until the outcome notice replaces it.
    LinkPending(String),
    /// Coordinator: the engine minted a single-use `molt://recover/…` link.
    Link(String),
    /// Coordinator: the mint failed for an operational reason of THIS node —
    /// `mesh-not-running` on the legacy queue shape, or (on Nostr) a relay
    /// reason naming the missing piece. The returning member's presence is
    /// never involved. Rendered as the calm failed state of the same link
    /// dialog, not as an error toast.
    LinkFailed(String),
    /// Rejoiner: the engine accepted link + phrase; the rejoin runs off the
    /// actor (it can span the survivors' human approval).
    Started(String),
    /// Rejoiner: the rejoin failed with this reason (retry = a fresh start,
    /// usually with a freshly minted link).
    Failed(String),
    /// Rejoiner: the seat is recovered — the engine flips to Main itself.
    Done(String),
    /// Coordinator: a returning member's request arrived and was REFUSED
    /// (`member:reason` — e.g. the R5 relay gate naming the relay to add).
    /// Without this the coordinator stares at a silent screen while the
    /// rejoiner waits out its timeout.
    Refused(String),
}

/// Split a session notice into its recovery reading (verbatim payload —
/// an error may itself contain colons).
fn parse_recover_notice(notice: &str) -> RecoverNotice {
    if let Some(member) = notice.strip_prefix("recovery-link-pending:") {
        RecoverNotice::LinkPending(member.to_string())
    } else if let Some(reason) = notice.strip_prefix("recovery-link-failed:") {
        RecoverNotice::LinkFailed(reason.to_string())
    } else if let Some(link) = notice.strip_prefix("recovery-link:") {
        RecoverNotice::Link(link.to_string())
    } else if let Some(member) = notice.strip_prefix("recover-started:") {
        RecoverNotice::Started(member.to_string())
    } else if let Some(line) = notice.strip_prefix("recover-note:") {
        RecoverNotice::Note(line.to_string())
    } else if let Some(error) = notice.strip_prefix("recover-failed:") {
        RecoverNotice::Failed(error.to_string())
    } else if let Some(member) = notice.strip_prefix("recovered:") {
        RecoverNotice::Done(member.to_string())
    } else if let Some(rest) = notice.strip_prefix("recover-refused:") {
        RecoverNotice::Refused(rest.to_string())
    } else {
        RecoverNotice::None
    }
}

/// Does the settings form hold unsaved edits against `stored`?
///
/// The relay pool is deliberately excluded: it is NOT part of the settings
/// draft (it is edited live through the `Relay*` commands, and
/// [`read_settings_draft`] therefore always yields an empty pool). Comparing
/// it would make every node that has a relay look permanently "dirty" — the
/// leave-guard would fire on every exit and an external settings change would
/// be suppressed as "the user is editing".
fn settings_draft_differs(stored: &SessionSettings, ui: &AppWindow) -> bool {
    let mut stored = stored.clone();
    stored.relays = Vec::new();
    // same reason as the pool: not a config-tab field, so comparing it would
    // make every clearnet-enabled node look permanently "dirty"
    stored.clearnet_relays_enabled = false;
    // the font sizes are edited live through set_fonts, never the draft —
    // neutralize them to the draft's carried constants for the same reason
    let d = SessionSettings::default();
    stored.font_app = d.font_app;
    stored.font_nav = d.font_nav;
    stored.font_editor = d.font_editor;
    stored != read_settings_draft(ui, &stored)
}

/// The stored settings out of the mirror — the draft needs them for the
/// fields it does not own outright: the file cap it echoes verbatim (0 is a
/// VALUE now, FP4), and the byte quotas whose MiB rendering must not round a
/// hand-written config value on an unrelated save.
fn stored_settings(last: &Arc<Mutex<Option<SessionSettings>>>) -> SessionSettings {
    last.lock()
        .ok()
        .and_then(|l| l.clone())
        .unwrap_or_default()
}

/// One MiB in bytes — the unit the GUI edits byte quotas in. A realistic
/// bucket size is far out of reach of a +/- stepper, so the field is typed
/// text and this is the unit it means.
const MIB: u64 = 1024 * 1024;

/// A byte quota as the whole MiB the field shows. Rounds UP, so the number
/// displayed is never smaller than the limit actually in force.
fn mib_label(bytes: u64) -> String {
    bytes.div_ceil(MIB).to_string()
}

/// The field's text back to bytes — keeping the STORED byte value whenever
/// it still renders as the same MiB. A `s3_max_bytes = 500000000` written by
/// hand shows as 477 MiB and stays 500000000 unless the operator actually
/// changes the number; without this, saving an unrelated setting would
/// silently round every quota onto the MiB grid. An emptied field is 0 (no
/// limit); text that is not a number at all keeps the stored value rather
/// than inventing one.
fn mib_text_to_bytes(text: &str, stored: u64) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    let Ok(mib) = text.parse::<u64>() else {
        return stored;
    };
    if mib == stored.div_ceil(MIB) {
        return stored;
    }
    mib.saturating_mul(MIB)
}

/// Gather the config-tab draft properties into a [`SessionSettings`].
fn read_settings_draft(ui: &AppWindow, stored: &SessionSettings) -> SessionSettings {
    let d = SessionSettings::default();
    SessionSettings {
        headless: ui.get_cfg_headless(),
        // not a config-tab field: the relay pool and the clearnet decision
        // are edited through the Relay* commands, never the settings draft.
        // Carried as false here and re-merged by the engine, exactly like
        // `relays` below (save_settings can neither inject nor wipe them).
        clearnet_relays_enabled: false,
        // edited live through set_fonts, never the draft; the engine
        // re-merges the stored sizes on save
        font_app: d.font_app,
        font_nav: d.font_nav,
        font_editor: d.font_editor,
        // not a config-tab field: the draft echoes the STORED cap so a
        // wholesale save keeps it (0 is a VALUE now — sharing off, FP4)
        file_cap_bytes: stored.file_cap_bytes,
        workspace_dir: ui.get_cfg_workspace_dir().to_string(),
        download_dir: ui.get_cfg_download_dir().to_string(),
        sound_message: sound_name(ui.get_cfg_sound_message_index()),
        sound_vote: sound_name(ui.get_cfg_sound_vote_index()),
        sound_poke: sound_name(ui.get_cfg_sound_poke_index()),
        poke_enabled: ui.get_cfg_poke_enabled(),
        poke_wake_command: ui.get_cfg_poke_wake().to_string(),
        read_receipts: ui.get_cfg_read_receipts(),
        s3_backup: ui.get_cfg_s3_backup(),
        s3_endpoint: ui.get_cfg_s3_endpoint().to_string(),
        s3_access_key: ui.get_cfg_s3_access().to_string(),
        s3_secret_key: ui.get_cfg_s3_secret().to_string(),
        s3_bucket: ui.get_cfg_s3_bucket().to_string(),
        s3_interval_min: ui.get_cfg_s3_interval() as u16,
        s3_keep_copies: ui.get_cfg_s3_copies() as u16,
        s3_max_bytes: mib_text_to_bytes(&ui.get_cfg_s3_max(), stored.s3_max_bytes),
        media_s3_bucket: ui.get_cfg_media_s3_bucket().to_string(),
        media_s3_max_bytes: mib_text_to_bytes(
            &ui.get_cfg_media_s3_max(),
            stored.media_s3_max_bytes,
        ),
        mcp_port: ui.get_cfg_mcp_port() as u16,
        mcp_allow: ui.get_cfg_mcp_allow().to_string(),
        mcp_token: ui.get_cfg_mcp_token().to_string(),
        anonymity: net_name(ui.get_cfg_network_index()),
        tor_mode: mode_name(ui.get_cfg_tor_mode_index()),
        tor_port: ui.get_cfg_tor_port() as u16,
        // The relay pool is not part of the settings draft: it is edited live
        // through the `Relay*` commands (URL validation + the clearnet
        // acknowledgement live there), and the engine keeps the live pool on
        // save regardless of what a draft carries.
        relays: Vec::new(),
    }
}

/// [`issue`], plus a success toast that only fires if the command SUCCEEDED.
///
/// The click sites used to toast on the way in: "Proposed" appeared the
/// moment the button was pressed, and a command that then failed showed the
/// user success followed by an error, for a proposal that never existed.
/// The confirmation belongs to the outcome, not to the intent.
/// Copy one run log into its Slint model, localized line-wise (E5). The
/// TONE model keeps reading the engine lines — the glyph survives
/// localization (pinned) — so both stay in step.
fn sync_log_localized(
    lang: i32,
    current: &ModelRc<slint::SharedString>,
    items: &[String],
    set: impl FnOnce(ModelRc<slint::SharedString>),
) {
    sync_rows(
        current,
        items.iter().map(|l| localize_log_line(lang, l).into()).collect(),
        set,
    );
}

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
/// The settings draft's three doors, in ORDER and in ONE task: the wake
/// command (a local shell hook — its own door, so no other surface can
/// plant one), the host posture with both secrets (`SetNodePosture` —
/// the GUI's door; MCP operates the seat, not the machine), then the
/// wholesale save, which re-merges the stored posture and so must land
/// LAST. "Save & continue" and "Rotate token" used to skip the wake door
/// and lose an edited command (review 2026-08-25 F3).
async fn save_draft(
    w: &WalletHandle,
    wake: String,
    settings: SessionSettings,
) -> Result<(), molt_core::MoltError> {
    w.execute(Command::SetWakeCommand { command: wake }).await?;
    w.execute(Command::SetNodePosture {
        posture: molt_core::NodePosture::of(&settings),
    })
    .await?;
    w.execute(Command::SaveSettings { settings }).await?;
    Ok(())
}

/// [`save_draft`] fire-and-forget, an error as a toast.
fn issue_draft(
    rt: &Handle,
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    wake: String,
    settings: SessionSettings,
) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
        if let Err(e) = save_draft(&w, wake, settings).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_show_toast_error(error_toast(&ui, &e));
                }
            });
        }
    });
}

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

/// Read the shared session and push it into the Slint properties on the UI
/// thread. `last_settings` remembers the previously applied settings so the
/// draft form is only refreshed when they really changed.
/// Perform one requested GUI interaction (`gui_over_mcp.md`): domain
/// verbs mapped onto the SAME Slint callbacks a human's click invokes,
/// so the action surface cannot drift from what a person can do. An
/// unknown verb is a logged no-op — the next snapshot's generation still
/// answers the caller.
fn perform_ui_action(ui: &AppWindow, action: &molt_core::UiAction) {
    let s = |k: &str| {
        action
            .args
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match action.verb.as_str() {
        "select_view" => ui.invoke_select_view(s("surface").into(), s("view").into()),
        "select_channel" => ui.invoke_select_channel(s("channel").into()),
        "open_workspace" => ui.invoke_open_workspace(s("id").into()),
        "close_workspace" => ui.invoke_close_workspace(),
        "chat_send" => ui.invoke_send_chat(s("body").into(), String::new().into()),
        other => {
            tracing::warn!(verb = %other, "ui_action: unknown verb — performed as a no-op");
        }
    }
}

/// The GUI's monotone publish counter (`gui_over_mcp.md`): bumped per
/// snapshot so an agent can await "my action landed" by polling
/// `read_ui_state` for a larger generation.
static UI_PUBLISH_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the window ACTUALLY holds right now (`gui_over_mcp.md`): read
/// from the UI models/properties on the UI thread — deliberately not from
/// the engine bundle, because "does the pane hold what the engine holds"
/// is exactly the question this exists to answer.
fn build_ui_snapshot(ui: &AppWindow) -> molt_core::UiSnapshot {
    use slint::Model as _;
    let screen = match ui.get_screen() {
        AppScreen::Choice => "choice",
        AppScreen::Create => "create",
        AppScreen::Open => "open",
        AppScreen::Join => "join",
        AppScreen::Restore => "restore",
        AppScreen::Settings => "settings",
        AppScreen::Main => "main",
    };
    let surfaces = ui.get_surfaces();
    let chat = surfaces.iter().find(|s| s.key == "chat");
    let (chat_rows, chat_last) = chat
        .map(|s| {
            let n = s.log.row_count();
            let last = (n.saturating_sub(3)..n)
                .filter_map(|i| s.log.row_data(i))
                .map(|l| l.text.to_string())
                .collect();
            (u32::try_from(n).unwrap_or(u32::MAX), last)
        })
        .unwrap_or((0, Vec::new()));
    let wizard = match screen {
        "create" => format!("create:{}", ui.get_cw_step()),
        "join" => format!("join:{}", ui.get_jw_step()),
        _ => String::new(),
    };
    molt_core::UiSnapshot {
        screen: screen.to_string(),
        surface: ui.get_selected_surface().to_string(),
        view: ui.get_selected_view().to_string(),
        channel: ui.get_selected_channel().to_string(),
        chat_rows,
        chat_last,
        compose_visible: screen == "main" && ui.get_selected_surface() == "chat",
        chat_in_view: ui.get_chat_log_in_view(),
        nav: surfaces.iter().map(|s| s.key.to_string()).collect(),
        pending_count: surfaces
            .iter()
            .map(|s| u32::try_from(s.pending_count.max(0)).unwrap_or(0))
            .sum(),
        wizard,
        toast: ui.get_toast_text().to_string(),
        generation: UI_PUBLISH_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
    }
}

/// Build the snapshot on the UI thread and hand it to the engine — the
/// publish half of `gui_over_mcp.md`, run at the end of every mirror pass
/// (what it publishes is what it just rendered).
async fn publish_ui_state(wallet: &WalletHandle, weak: &slint::Weak<AppWindow>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = tx.send(build_ui_snapshot(&ui));
        }
    });
    if let Ok(snapshot) = rx.await {
        let _ = wallet.execute(Command::UiPublish { snapshot }).await;
    }
}

async fn push_session(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    last_settings: &Arc<Mutex<Option<SessionSettings>>>,
    scope: SessionScope,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let chat_ui_for_apply = chat_ui.clone();
    let sv = match wallet.execute(Command::ReadSession).await {
        Ok(Reply::Session(sv)) => *sv,
        _ => return,
    };
    // a run tick only advanced one lifecycle — repaint just that mirror
    if scope != SessionScope::Full {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_runs(&ui, &sv);
            }
        });
        return;
    }
    // THE workspace switch, and the only place it happens. It used to ride
    // `begin_push`, keyed on whatever session copy that push had read — so a
    // push that read the session BEFORE an open could re-enter the state as
    // "no workspace" AFTER the open, bump the epoch past the good push, and
    // land its own empty bundle. That is what showed an empty chat on the
    // first open, until switching surfaces forced a fresh one.
    //
    // Here it is ordered by the event stream instead: the session mirror
    // runs on every SessionChanged, before the surfaces do.
    {
        let mut st = match chat_ui.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = st.workspace.clone();
        st.enter_workspace(&sv.active_workspace);
        if before != st.workspace {
            tracing::debug!(from = %before, to = %st.workspace, gen = st.generation, "ui: workspace switch");
        }
    }
    let (changed, prev) = {
        // POISON-TOLERANT, like every other lock site here. A panic in some
        // other callback must not stop the live mirror for the rest of the
        // session: this cache only decides whether the settings FORM is
        // refreshed, and reading a possibly-stale copy is a redraw, while
        // panicking here ends the task that redraws anything at all.
        let mut last = match last_settings.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prev = last.clone();
        let changed = prev.as_ref() != Some(&sv.settings);
        if changed {
            *last = Some(sv.settings.clone());
        }
        (changed, prev)
    };
    let weak2 = weak.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            // Draft protection: a settings change arriving while the user has
            // unsaved edits open (an external config reload, an MCP save) must
            // not wipe what they typed. The notice/leave-guard tells them; the
            // draft stays until they Save or discard.
            let editing = ui.get_screen() == AppScreen::Settings
                && prev.is_some_and(|p| settings_draft_differs(&p, &ui));
            apply_session(&ui, &sv, changed && !editing, &chat_ui_for_apply);
        }
    });
    // gui_over_mcp.md: publish what this pass rendered (queued behind the
    // apply above on the event loop, so the claim is post-render)
    publish_ui_state(wallet, &weak2).await;
}

/// Render one session snapshot into the window: screen, language (and strings),
/// the transient notice, the last-wizard outcome, and the settings draft. The
/// draft fields are only overwritten when the session's settings actually
/// changed — otherwise an unrelated change (language, theme, navigation)
/// would wipe what the user is typing in the settings form.
fn apply_session(
    ui: &AppWindow,
    sv: &SessionView,
    settings_changed: bool,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let lang = i32::from(sv.language == "de");
    ui.set_screen(from_screen(sv.screen));
    ui.set_selected_surface(sv.surface.as_str().into());
    ui.set_selected_view(sv.view.as_str().into());
    // What the poke menus gate on: the APPLIED value, never the settings
    // draft. A ticked-but-unsaved checkbox would otherwise offer a menu whose
    // command the engine refuses (and an unticked one would hide a menu that
    // still works).
    ui.global::<Poke>().set_on(sv.settings.poke_enabled);

    // the Open screen's list mirrors the session's workspaces, re-applying
    // whatever column sort the user picked
    let now = unix_now();
    let mut items: Vec<WorkspaceItem> = sv
        .workspaces
        .iter()
        .map(|w| workspace_item(lang, now, w))
        .collect();
    sort_ws_items(
        &mut items,
        ui.get_ws_sort_key().as_str(),
        ui.get_ws_sort_desc(),
    );
    sync_rows(&ui.get_ws_list(), items, |m| ui.set_ws_list(m));

    // the settings backup tab mirrors the same workspaces as a local↔bucket
    // mapping table
    let mut bk = backup_rows(sv);
    sort_bk_rows(
        &mut bk,
        ui.get_bk_sort_key().as_str(),
        ui.get_bk_sort_desc(),
    );
    sync_rows(&ui.get_bk_rows(), bk, |m| ui.set_bk_rows(m));

    // the main header shows the active workspace (+ its sync status); the
    // chat's members strip mirrors the active workspace's roster/presence.
    // `active_workspace` is an id; the header wants the display name.
    let active = sv.workspaces.iter().find(|w| w.id == sv.active_workspace);
    ui.set_active_workspace(active.map(|w| w.name.as_str()).unwrap_or_default().into());
    // drafts are PER WORKSPACE: entering another republic resets the wiki
    // model and loads that workspace's stored draft (WP-D)
    {
        let g = ui.global::<WikiState>();
        let ws: slint::SharedString = sv.active_workspace.as_str().into();
        if g.get_ws_id() != ws {
            g.set_ws_id(ws);
            g.invoke_workspace_changed();
        }
    }
    let (a_state, a_status) = active
        .map(|w| {
            (
                i32::from(w.state),
                sync_status_label(lang, w.state, w.last_sync_min, w.sync_queue),
            )
        })
        .unwrap_or((0, String::new()));
    ui.set_active_state(a_state);
    ui.set_active_status(a_status.into());
    // the ratified founding charter for the Constitution surface (empty until a
    // deliberated workspace is open) — plus its balanced display columns
    let agenda = active.map(|w| w.agenda.as_str()).unwrap_or_default();
    ui.set_active_agenda(agenda.into());
    let cols: Vec<slint::SharedString> = charter_columns(agenda, 3)
        .into_iter()
        .map(slint::SharedString::from)
        .collect();
    ui.set_charter_cols(ModelRc::new(VecModel::from(cols)));
    let roster: Vec<MemberSync> = active
        .map(|w| {
            w.members
                .iter()
                .map(|m| MemberSync {
                    name: m.name.as_str().into(),
                    last: seen_label(lang, now, m.last_seen, never_seen_label(lang)).into(),
                    state: i32::from(m.state),
                })
                .collect()
        })
        .unwrap_or_default();
    sync_rows(&ui.get_active_members(), roster, |m| {
        ui.set_active_members(m)
    });
    // Organization → Status/Members: reachable count from the roster (a member
    // is reachable unless offline, state 2)
    let total = active.map(|w| w.members.len()).unwrap_or(0);
    let online = active
        .map(|w| w.members.iter().filter(|m| m.state != 2).count())
        .unwrap_or(0);
    ui.set_org_online(i32::try_from(online).unwrap_or(0));
    ui.set_org_total(i32::try_from(total).unwrap_or(0));

    // the create wizard's folder preview roots at the configured
    // workspace dir (shown raw, as configured — `~` and all)
    ui.set_cw_dir(sv.settings.workspace_dir.clone().into());

    // the manual-export status (Open-detail note): honest mirror of the
    // engine's export state — running / real success / real failure
    {
        let s = ui.global::<Strings>();
        let ex = &sv.export;
        let (note, failed) = if ex.running {
            (format!("{} {}", s.get_ow_export_running(), ex.dest), false)
        } else if ex.result == "ok" {
            let mut note =
                format!("{} {} ({})", s.get_ow_export_note(), ex.dest, file_size_label(ex.bytes));
            if !ex.skipped.is_empty() {
                note.push_str(&format!(
                    " - {} {}",
                    s.get_ow_export_skipped(),
                    ex.skipped.join(", ")
                ));
            }
            (note, false)
        } else if let Some(err) = ex.result.strip_prefix("error: ") {
            (format!("{} {}", s.get_ow_export_failed(), err), true)
        } else {
            (String::new(), false)
        };
        ui.set_export_note(note.into());
        ui.set_export_failed(failed);
        ui.set_export_ws(ex.workspace.as_str().into());
    }

    // the wiki export's honest outcome, EDGE-triggered: the session
    // re-pushes unchanged, so a toast per push would repeat the same result
    // forever. A started export clears the mark, which is what lets the very
    // same export toast a second time.
    {
        let ex = &sv.wiki_export;
        let mark = if ex.running {
            String::new()
        } else {
            format!("{}|{}|{}|{}", ex.dest, ex.result, ex.files, ex.bytes)
        };
        if ui.get_wiki_export_seen() != mark.as_str() {
            ui.set_wiki_export_seen(mark.as_str().into());
            if let Some((msg, failed)) = wiki_export_toast(lang, ex) {
                if failed {
                    ui.invoke_show_toast_error(msg.into());
                } else {
                    ui.invoke_show_toast(msg.into());
                }
            }
        }
    }

    apply_runs(ui, sv);
    ui.global::<Theme>().set_theme_index(theme_index(&sv.theme));
    ui.global::<Theme>()
        .set_fs_app(f32::from(sv.settings.font_app));
    ui.global::<Theme>()
        .set_fs_nav(f32::from(sv.settings.font_nav));
    ui.global::<Theme>()
        .set_fs_editor(f32::from(sv.settings.font_editor));
    ui.set_lang_index(lang);
    ui.set_notice(sv.notice.clone().into());
    // what §10.7 file gating and the §6.5 coarse-presence hint key on
    ui.set_session_transport(sv.transport.as_str().into());
    // a failed write carries its detail in the notice; split it off so the
    // settings footer can render it in the error tone without string ops
    ui.set_notice_failed(
        match sv.notice.strip_prefix("save-failed: ") {
            Some(d) if lang == 1 => format!("Speichern fehlgeschlagen: {d}"),
            _ if sv.notice.starts_with("save-failed") => sv.notice.clone(),
            _ => String::new(),
        }
        .into(),
    );
    // The recovery flow rides the same transient notice, but EDGE-triggered:
    // the notice lingers in the session until something replaces it, and the
    // link dialog / rejoin state must react once per NEW notice — a re-push of
    // an unchanged session must not re-open a dismissed link dialog. The
    // last-handled notice is mirrored into the window (like the settings
    // draft cache) and compared before acting.
    if ui.get_recover_notice_seen() != sv.notice.as_str() {
        ui.set_recover_notice_seen(sv.notice.clone().into());
        // S7: the fetch outcome rides the notice — the backup tab shows it
        if let Some(fetched) = sv.notice.strip_prefix("backup-fetched:") {
            ui.set_bk_fetched(fetched.into());
            ui.set_bk_fetch_error("".into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-fetch-failed:") {
            ui.set_bk_fetch_error(err.into());
        }
        match parse_recover_notice(&sv.notice) {
            RecoverNotice::LinkPending(member) => {
                // coordinator: a mint attempt started — open the dialog in
                // its calm pending state (the outcome notice fills it in)
                ui.set_recover_link_member(member.into());
                ui.set_recovery_link("".into());
                ui.set_recovery_link_error("".into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::Link(link) => {
                // coordinator: present the freshly minted single-use link
                ui.set_recovery_link(link.into());
                ui.set_recovery_link_error("".into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::LinkFailed(reason) => {
                // coordinator: the mint failed for an operational reason of
                // THIS node — same dialog, calm failed state (never a toast;
                // the returning member's presence is irrelevant to a mint)
                ui.set_recovery_link("".into());
                ui.set_recovery_link_error(reason.into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::Started(member) => {
                ui.set_rv_member(member.into());
                ui.set_rv_running(true);
                ui.set_rv_error("".into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Failed(error) => {
                ui.set_rv_running(false);
                ui.set_rv_error(localize_recover_failed(lang, &error).into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Note(line) => {
                ui.set_rv_note(localize_recover_note(lang, &line).into());
            }
            RecoverNotice::Done(_) => {
                // the engine flips to Main itself — just clear the peer-way
                // state so a later return to the Restore screen starts clean
                ui.set_rv_running(false);
                ui.set_rv_error("".into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Refused(what) => {
                // coordinator: the request was refused — same link dialog,
                // loud error slot (the reason names what to fix, e.g. the
                // relay the pool must gain)
                ui.set_recovery_link_error(what.into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::None => {}
        }
        // the same edge-triggered channel carries the backup/restore
        // honesty notices (story 12/13): toast them once per NEW notice
        let s = ui.global::<Strings>();
        if sv.notice == "detached" {
            // §4.4: knowledge restored, membership not — say exactly that
            ui.invoke_show_toast_error(s.get_toast_detached());
        } else if sv.notice == "reattaching" {
            // detached_reattach.md: the seat announces itself — a calm
            // in-progress note, not an error
            ui.invoke_show_toast(s.get_toast_reattaching());
        } else if let Some(err) = sv.notice.strip_prefix("backup-failed:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_failed()).into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-prune-failed:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_prune()).into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-quota:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_quota()).into());
        } else if let Some(rest) = sv.notice.strip_prefix("relay-refused:") {
            // B4: the probe's one-line verdict — the entry stays unconfirmed
            ui.invoke_show_toast_error(format!("{} {rest}", s.get_toast_relay_refused()).into());
        } else if let Some(rest) = sv.notice.strip_prefix("relay-unverified:") {
            // …and the honest middle class: confirmed on the operator's
            // consent, but the relay could not be judged right now
            ui.invoke_show_toast(format!("{} {rest}", s.get_toast_relay_unverified()).into());
        } else if let Some(err) = sv.notice.strip_prefix("genesis-undelivered:") {
            // The republic EXISTS here but its members were never told: the
            // genesis 445 reached no relay. Copy lives Rust-side (the
            // tor_verdict_copy_for precedent) so this needs no Strings entry
            // and no ~6 GiB molt-ui-window rebuild. Without a surface the
            // engine's notice would itself be an inert seam — the exact sin
            // this fix is about.
            ui.invoke_show_toast_error(format!("{} {err}", genesis_undelivered_copy(lang)).into());
        }
    }
    // persistent restart warning: which changed keys only apply on restart
    ui.set_restart_keys(sv.restart_required.join(", ").into());
    // the S3 test status is transient and lives outside the settings draft,
    // so push it on every update — even while the user has an unsaved field
    // open and `settings_changed` is suppressed
    ui.set_cfg_s3_test(localize_s3_verdict(lang, &sv.s3_test).into());
    ui.set_cfg_media_s3_test(localize_s3_verdict(lang, &sv.s3_media_test).into());
    ui.set_cfg_bk_list(localize_s3_verdict(lang, &sv.s3_list).into());
    // …and the same for the Tor probe's verdict. The rung's key drives the
    // "testing" affordance, the sentence and its tone come from Rust so the
    // "only a proven circuit is green" rule stays a tested statement, and the
    // technical specifics ride along untranslated.
    ui.set_cfg_tor_test(sv.tor_test.state.as_str().into());
    // a confirmed-but-switched-off pool is a DIFFERENT problem from an empty
    // one, and telling the user to "confirm a relay" they already confirmed
    // is advice that cannot help (review finding). The classification is
    // molt-core's single `pool_gap` — this used to hand-roll the same
    // predicate, which is how three copies of it came to exist.
    // the create wizard's relay picker: what this node can dial, and the
    // founder's choice among it. Refreshed from the session so a relay
    // confirmed while the wizard is open shows up.
    {
        let dialable = molt_core::relay::dialable(&sv.settings.relays, sv.clearnet_session);
        if let Ok(mut st) = chat_ui.lock() {
            st.set_create_relays(dialable);
            let rows: Vec<RelayPick> = st
                .create_pick_rows()
                .into_iter()
                .map(|(url, picked)| RelayPick { url: url.into(), picked })
                .collect();
            ui.set_cw_relay_picks(slint::ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    let session_locked = molt_core::relay::pool_gap(&sv.settings.relays, sv.clearnet_session)
        == Some(molt_core::relay::PoolGap::NonOnionOff);
    ui.set_cfg_tor_test_text(
        tor_verdict_copy_for(lang, sv.tor_test.state, session_locked).into(),
    );
    ui.set_cfg_tor_test_tone(tor_test_tone(sv.tor_test.state));
    ui.set_cfg_tor_test_detail(tor_test_detail(lang, &sv.tor_test).into());

    // transport health for the header "chat" pill: tone (green/amber/red) plus
    // the engine's reason string as the hover tooltip (P6). Pushed on every
    // update so a dial outcome repaints the pill regardless of settings edits.
    let (net_tone, net_reason) = net_health_pill(&sv.net_health);
    ui.set_net_health_tone(net_tone);
    ui.set_net_health_reason(localize_net_reason(lang, &net_reason).into());

    // the create screen's read-only "Network" line: the EFFECTIVE global
    // anonymity network. NOT a draft field (the user never types it), so it
    // is pushed on every update — inside the settings_changed guard a GUI
    // save would leave it stale (the draft-protection `editing` flag
    // suppresses the mirror exactly then)
    ui.set_cw_net(molt_core::effective_net_label(&sv.settings.anonymity).into());

    // the relay pool is edited LIVE through the Relay* commands, so it is not
    // a draft field: push it on every update, even while an unsaved form edit
    // suppresses the draft mirror
    apply_relays(ui, sv);

    if !settings_changed {
        apply_strings(ui, lang);
        return;
    }
    apply_settings_fields(ui, &sv.settings);

    apply_strings(ui, lang);
}

/// The relay pool as the Network panel renders it: the entries in priority
/// order, each carrying the ENGINE's derived verdict. The GUI never
/// re-evaluates the dial policy — it turns `blocked` into words, nothing more
/// (`docs_archive/transport/relay_pool.md` §3).
fn relay_rows(relays: &[RelayStatus]) -> Vec<RelayItem> {
    let last = relays.len().saturating_sub(1);
    relays
        .iter()
        .enumerate()
        .map(|(i, r)| RelayItem {
            url: r.url.as_str().into(),
            kind: match r.kind {
                RelayKind::Onion => 0,
                RelayKind::Clearnet => 1,
                RelayKind::Local => 2,
            },
            confirmed: r.confirmed,
            blocked: match r.blocked {
                None => 0,
                Some(RelayBlock::Unconfirmed) => 1,
                Some(RelayBlock::ClearnetSessionLocked) => 2,
            },
            pos: i32::try_from(i.saturating_add(1)).unwrap_or(i32::MAX),
            first: i == 0,
            last: i == last,
        })
        .collect()
}

/// Mirror the relay pool into Settings → Nostr relays: the rows, how many relays
/// are dialable right now (0 = this node is connected to nothing), whether a
/// confirmed clearnet relay exists at all (only then is there anything for
/// the session activation to unlock), and the session unlock itself.
fn apply_relays(ui: &AppWindow, sv: &SessionView) {
    sync_rows(&ui.get_relay_rows(), relay_rows(&sv.relays), |m| {
        ui.set_relay_rows(m);
    });
    let dialable = sv.relays.iter().filter(|r| r.blocked.is_none()).count();
    ui.set_relay_dialable(i32::try_from(dialable).unwrap_or(i32::MAX));
    // the session toggle concerns everything reached outside Tor: clearnet
    // AND local relays (§10.14) share the one per-session activation
    ui.set_relay_clearnet_confirmed(
        sv.relays
            .iter()
            .any(|r| r.confirmed && r.kind != RelayKind::Onion),
    );
    ui.set_cfg_clearnet_session(sv.clearnet_session);
    // the clearnet warning tailors its sentence to the SAVED anonymity
    // setting — never to the unsaved dropdown draft, which would promise a
    // Tor circuit the node does not have yet
    ui.set_net_tor_active(sv.settings.anonymity == "tor");
}

/// The R6 pool-edit modal's one draft setter: rows are the editing truth,
/// the space-joined string is the org-propose payload — set together so the
/// two can never disagree. The overlap flag mirrors the engine's
/// make-before-break gate into `confirm-enabled`, so the modal never sends
/// a draft the engine would refuse after the rows are already gone.
fn set_relay_draft_rows(ui: &AppWindow, rows: &[String]) {
    let current: Vec<String> = ui.get_org_relays().iter().map(|s| s.to_string()).collect();
    let overlap = current.is_empty() || rows.iter().any(|r| current.contains(r));
    ui.set_org_relays_draft_overlap(overlap);
    ui.set_org_relays_draft(rows.join(" ").into());
    sync_strings(&ui.get_org_relays_draft_rows(), rows, |m| {
        ui.set_org_relays_draft_rows(m)
    });
}

/// Validate a pool-add URL: `Ok` carries the CANONICAL spelling to store,
/// `Err` the localized reason the pool refuses it. Validation runs through
/// molt-core's OWN parser (the very function the engine gates on, so the
/// field message and the gate can never disagree); the engine still
/// re-validates and stays the authority.
fn relay_add_check(lang: i32, raw: &str, pool: &[String]) -> Result<String, &'static str> {
    let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
    match molt_core::relay::normalize_relay_url(raw) {
        Err(RelayUrlError::Scheme) => Err(l.rp_err_scheme),
        Err(RelayUrlError::Host) => Err(l.rp_err_host),
        Err(RelayUrlError::PlaintextClearnet) => Err(l.rp_err_plain),
        Err(RelayUrlError::Junk) => Err(l.rp_err_junk),
        Err(RelayUrlError::OnionAddress) => Err(l.rp_err_onion),
        Err(RelayUrlError::Userinfo) => Err(l.rp_err_userinfo),
        Err(RelayUrlError::Fragment) => Err(l.rp_err_fragment),
        Err(RelayUrlError::TooLong) => Err(l.rp_err_toolong),
        Err(RelayUrlError::NonCanonical) => Err(l.rp_err_noncanon),
        Ok(url) if pool.contains(&url) => Err(l.rp_err_dup),
        Ok(url) => Ok(url),
    }
}

/// Push one settings value into the draft form fields (the mirror on real
/// changes, and the leave-guard's "discard" reset).
fn apply_settings_fields(ui: &AppWindow, s: &SessionSettings) {
    ui.set_cfg_headless(s.headless);
    ui.set_cfg_workspace_dir(s.workspace_dir.clone().into());
    ui.set_cfg_download_dir(s.download_dir.clone().into());
    ui.set_cfg_s3_backup(s.s3_backup);
    ui.set_cfg_s3_endpoint(s.s3_endpoint.clone().into());
    ui.set_cfg_s3_access(s.s3_access_key.clone().into());
    ui.set_cfg_s3_secret(s.s3_secret_key.clone().into());
    ui.set_cfg_s3_bucket(s.s3_bucket.clone().into());
    ui.set_cfg_s3_interval(i32::from(s.s3_interval_min));
    ui.set_cfg_s3_copies(i32::from(s.s3_keep_copies));
    ui.set_cfg_s3_max(mib_label(s.s3_max_bytes).into());
    ui.set_cfg_media_s3_bucket(s.media_s3_bucket.clone().into());
    ui.set_cfg_media_s3_max(mib_label(s.media_s3_max_bytes).into());
    ui.set_cfg_mcp_port(s.mcp_port as i32);
    ui.set_cfg_mcp_allow(s.mcp_allow.clone().into());
    ui.set_cfg_mcp_token(s.mcp_token.clone().into());
    ui.set_cfg_sound_message_index(sound_index(&s.sound_message));
    ui.set_cfg_sound_vote_index(sound_index(&s.sound_vote));
    ui.set_cfg_sound_poke_index(sound_index(&s.sound_poke));
    ui.set_cfg_poke_enabled(s.poke_enabled);
    ui.set_cfg_poke_wake(s.poke_wake_command.clone().into());
    ui.set_cfg_read_receipts(s.read_receipts);
    ui.set_cfg_network_index(net_index(&s.anonymity));
    ui.set_cfg_tor_mode_index(mode_index(&s.tor_mode));
    ui.set_cfg_tor_port(s.tor_port as i32);
}

/// Mirror the three engine-run lifecycles (the engine ticks them at 90 ms;
/// a `SessionChanged` with a run scope re-renders ONLY this, so the rest of
/// the window keeps its focus/scroll state untouched).
fn apply_runs(ui: &AppWindow, sv: &SessionView) {
    let lang = i32::from(sv.language == "de");
    // restore
    ui.set_rw_step(i32::from(sv.restore.run.step));
    ui.set_rw_way(sv.restore.way.clone().into());
    ui.set_rw_target(sv.restore.target.clone().into());
    ui.set_rw_progress(f32::from(sv.restore.run.progress_pct) / 100.0);
    ui.set_rw_outcome(i32::from(sv.restore.run.outcome));
    sync_log_localized(ui.get_lang_index(), &ui.get_rw_log(), &sv.restore.run.log, |m| {
        ui.set_rw_log(m)
    });
    sync_log_tones(&ui.get_rw_log_tone(), &sv.restore.run.log, |m| ui.set_rw_log_tone(m));
    ui.set_rw_headline(localize_headline(ui.get_lang_index(), &sv.restore.run.headline).into());

    // founding ritual; the run header is composed here so an MCP-started
    // founding shows real values even with an empty local form
    ui.set_cw_step(i32::from(sv.create.run.step));
    ui.set_cw_outcome(i32::from(sv.create.run.outcome));
    ui.set_cw_seed(sv.create.seed.clone().into());
    ui.set_cw_backup_confirmed(sv.create.backup_confirmed);
    ui.set_cw_run_name(sv.create.name.clone().into());
    ui.set_cw_run_detail(
        format!(
            "{}-of-{} · {}",
            sv.create.threshold, sv.create.members, sv.create.net
        )
        .into(),
    );
    sync_log_localized(ui.get_lang_index(), &ui.get_cw_log(), &sv.create.run.log, |m| {
        ui.set_cw_log(m)
    });
    sync_log_tones(&ui.get_cw_log_tone(), &sv.create.run.log, |m| ui.set_cw_log_tone(m));
    ui.set_cw_headline(localize_headline(ui.get_lang_index(), &sv.create.run.headline).into());
    // a declined seat switches the failure banner to "the founding is over"
    ui.set_cw_declined(sv.create.seats.iter().any(|s| s.state == 3));
    // the ritual member list: founder plus one row per seat. The founder
    // row is sealed by construction (2) and fully green (4) once their own
    // phrase backup is confirmed — same scale as the member seats.
    let mut seats: Vec<RitualSeat> = vec![RitualSeat {
        member: sv.create.member.as_str().into(),
        detail: strings_founder(lang).into(),
        state: if sv.create.backup_confirmed { 4 } else { 2 },
    }];
    for (i, s) in sv.create.seats.iter().enumerate() {
        let (member, detail) = if s.member.is_empty() {
            // only offer the link once it is genuinely joinable (carries the
            // transport handover); until the queue is provisioned it is a
            // non-joinable preview, so show nothing to copy yet
            let detail = if molt_engine::FoundingInvite::parse(&s.link).is_ok() {
                s.link.clone()
            } else {
                String::new()
            };
            (
                if lang == 1 {
                    format!("Einladung {}", i + 1)
                } else {
                    format!("Invite {}", i + 1)
                },
                detail,
            )
        } else {
            (s.member.clone(), seat_state_label(lang, s.state))
        };
        seats.push(RitualSeat {
            member: member.into(),
            detail: detail.into(),
            state: i32::from(s.state),
        });
    }
    // sealed = the roster signature is in (state 2), key secured or not
    // (state 4) — counting only 2 read as a regression at the very end
    let sealed = seats.iter().filter(|s| s.state == 2 || s.state == 4).count();
    ui.set_cw_sealed(i32::try_from(sealed).unwrap_or(0));
    ui.set_cw_total(i32::try_from(seats.len()).unwrap_or(0));
    // every MEMBER has ratified the charter (signature delivered, 2/4) —
    // gates the founder's own phrase-backup prompt: it comes only after
    // all others accepted the charter and features
    ui.set_cw_all_ratified(
        !sv.create.seats.is_empty()
            && sv
                .create
                .seats
                .iter()
                .all(|s| s.state == 2 || s.state == 4),
    );
    ui.set_cw_simulated(sv.create.simulated);
    // the deliberation step: once every seat has joined, the founder proposes
    // the final name + charter for the members to ratify (the agenda itself is
    // a local editable draft in the wizard, like the name)
    ui.set_cw_can_propose(sv.create.can_propose);
    sync_rows(&ui.get_cw_seats(), seats, |m| ui.set_cw_seats(m));

    // the rejoiner's re-admission checklist (recovery_auto_approval.md §5):
    // roster seats with their counted voices, toward `need` approvals
    let rv_seats: Vec<RecoverSeatRow> = sv
        .recover
        .seats
        .iter()
        .map(|s| RecoverSeatRow {
            member: s.member.as_str().into(),
            approved: s.approved,
        })
        .collect();
    ui.set_rv_have(
        i32::try_from(sv.recover.seats.iter().filter(|s| s.approved).count()).unwrap_or(0),
    );
    ui.set_rv_need(i32::try_from(sv.recover.need).unwrap_or(0));
    sync_rows(&ui.get_rv_seats(), rv_seats, |m| ui.set_rv_seats(m));

    // join
    ui.set_jw_step(i32::from(sv.join.run.step));
    // the engine returned the join session to the idle form (join_finish
    // after success, or an invalidation by create/recover/open) — re-arm
    // the optimistic start latch, or the NEXT join in this app run has a
    // dead button (review 2026-08-12). While a start is in flight the
    // engine's own re-entry guard still refuses a double, so the worst a
    // racing pre-start push can do is re-show the toast this latch avoids.
    if sv.join.run.step == 0 {
        ui.set_jw_starting(false);
    }
    ui.set_jw_progress(f32::from(sv.join.run.progress_pct) / 100.0);
    ui.set_jw_outcome(i32::from(sv.join.run.outcome));
    ui.set_jw_republic(sv.join.republic.clone().into());
    ui.set_jw_rule(format!("{}-of-{}", sv.join.rule_m, sv.join.rule_n).into());
    ui.set_jw_inviter(sv.join.inviter.clone().into());
    // the joiner's own recovery phrase, generated + shown once (like the
    // founder's on the create screen)
    ui.set_jw_seed(sv.join.seed.clone().into());
    // the ratification step: the founder's proposed charter, which the joiner
    // must confirm before its signature is released and the workspace opens
    ui.set_jw_awaiting_ratify(sv.join.awaiting_ratify);
    ui.set_jw_awaiting_backup(sv.join.awaiting_backup);
    ui.set_jw_sealed(!sv.join.sealed_id.is_empty());
    ui.set_jw_proposed_name(sv.join.proposed_name.clone().into());
    ui.set_jw_proposed_agenda(sv.join.proposed_agenda.clone().into());
    // the proposed feature selection, exactly as it will be signed; a
    // pre-v5 founder (None) renders the legacy baseline
    let jw_feat = |key: &str| match &sv.join.proposed_features {
        Some(f) => f.iter().any(|k| k == key),
        None => molt_core::Surface::LEGACY_FEATURES.contains(&key),
    };
    ui.set_jw_feat_memory(jw_feat("memory"));
    ui.set_jw_feat_quests(jw_feat("quests"));
    ui.set_jw_feat_vault(jw_feat("vault"));
    ui.set_jw_feat_wallet(jw_feat("wallet"));
    sync_log_localized(ui.get_lang_index(), &ui.get_jw_log(), &sv.join.run.log, |m| {
        ui.set_jw_log(m)
    });
    sync_log_tones(&ui.get_jw_log_tone(), &sv.join.run.log, |m| ui.set_jw_log_tone(m));
    ui.set_jw_headline(localize_headline(ui.get_lang_index(), &sv.join.run.headline).into());
}

/// What a `molt://…` link in the Restore wizard's one link field turns out
/// to be — the whole of the Join/Restore merge (`docs_archive/ui/welcome_rework.md`).
///
/// The two shapes are unambiguous by prefix and both already have a parser in
/// `molt-engine`; the panel just asks which one it is holding, because that
/// decides which FIELD is required and which existing command a click issues.
/// No new engine surface: `join_start` and `recover_start` stay exactly the
/// co-equal tools they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// A founding invite (`molt://invite/…`) — needs a NAME, mints its own
    /// recovery phrase, runs `Command::JoinStart`.
    Invite {
        /// The republic's display name, for the panel's confirmation line.
        republic: String,
        /// Who minted the link.
        inviter: String,
    },
    /// A recovery link (`molt://recover/…`) — names its own seat, needs the
    /// PHRASE, runs `Command::RecoverStart`.
    Recovery {
        /// The republic's display name.
        republic: String,
        /// The seat coming back; the name field shows this read-only.
        member: String,
    },
    /// Empty, malformed, or actionable-looking but missing its transport
    /// handover (a preview-only link nothing can be done with).
    Unrecognized,
}

/// Classify a pasted link. Empty input is [`LinkKind::Unrecognized`] like any
/// other unusable value — the panel simply stays unarmed rather than
/// complaining at someone who has not typed anything yet.
#[must_use]
pub fn link_kind(link: &str) -> LinkKind {
    let trimmed = link.trim();
    if trimmed.is_empty() {
        return LinkKind::Unrecognized;
    }
    // both parsers reject a link whose handover is missing or damaged, which
    // is what keeps a preview-only link from arming a flow that cannot run
    if let Ok(inv) = molt_engine::FoundingInvite::parse(trimmed) {
        return LinkKind::Invite {
            republic: inv.info.republic,
            inviter: inv.info.inviter,
        };
    }
    if let Some(rec) = molt_engine::RecoveryInvite::parse(trimmed) {
        return LinkKind::Recovery {
            republic: rec.republic,
            member: rec.member,
        };
    }
    LinkKind::Unrecognized
}

/// The invite's relays this node does not hold yet.
fn invite_relays_missing(ui: &AppWindow, link: &str) -> i32 {
    let Ok(inv) = molt_engine::FoundingInvite::parse(link) else {
        return 0;
    };
    let have: Vec<String> = ui.get_relay_rows().iter().map(|r| r.url.to_string()).collect();
    i32::try_from(
        inv.handover
            .relays
            .iter()
            .filter(|u| !have.contains(u))
            .count(),
    )
    .unwrap_or(0)
}

/// Per-line tone of a run log (0 neutral, 1 good, 2 bad) from the ✓/✗
/// prefix convention every engine log line follows — lets the Slint side
/// highlight terminal lines without string surgery it cannot do.
// Mirrored in place via sync_rows: the log modal's per-line colour bindings
// consume these models, and a fresh ModelRc per engine tick would reset the
// repeater on every tick while the modal is open.
fn sync_log_tones(current: &ModelRc<i32>, log: &[String], set: impl FnOnce(ModelRc<i32>)) {
    sync_rows(
        current,
        log.iter()
            .map(|l| match l.chars().next() {
                Some('✓') => 1,
                Some('✗') => 2,
                _ => 0,
            })
            .collect::<Vec<i32>>(),
        set,
    );
}

/// Read every surface and push it into the window.
///
/// The gathering ([`gather_surfaces`]) is separate so it can be TESTED: it
/// is all of the decisions — which workspace, which channel, whether this
/// pass is still current — and it needs no window and no event loop. What
/// stays here is the hop onto the UI thread, which is Slint's own.
async fn push_surfaces(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let Some((my_gen, bundle)) = gather_surfaces(wallet, chat_ui).await else {
        return;
    };
    let weak2 = weak.clone();
    let chat_ui = chat_ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let weak = weak2;
        // the generation may have moved between the bundle build and this
        // closure running on the UI thread — a stale bundle must not land
        // (it would revert the visible pane until the next engine event)
        if !chat_ui.lock().map(|st| st.is_current(my_gen)).unwrap_or(false) {
            tracing::debug!(gen = my_gen, "ui: bundle DROPPED as stale");
            return;
        }
        if let Some(ui) = weak.upgrade() {
            apply_surfaces(&ui, &bundle);
            tracing::debug!(
                gen = my_gen,
                chat_rows = ui
                    .get_surfaces()
                    .iter()
                    .find(|s| s.key == "chat")
                    .map_or(0, |s| s.log.row_count()),
                "ui: bundle applied"
            );
        }
    });
    // the snapshot must claim what this pass just rendered
    // (gui_over_mcp.md): the publish closure queues AFTER the apply
    // closure above, so it reads the fresh models — without this, a
    // surfaces-only change (a chat message arriving) left the published
    // snapshot stale until the next SESSION push
    publish_ui_state(wallet, weak).await;
}

/// Build the Slint surface models from a bundle (on the UI thread). The rows
/// of the surfaces model are updated IN PLACE when possible: replacing the
/// whole model would recreate every main-view element on each engine event —
/// and with it drop the keyboard focus out of the chat compose box mid-typing.
fn apply_surfaces(ui: &AppWindow, b: &SurfacesBundle) {
    ui.set_node_member(b.member.clone().into());
    // the poke menus need the own seat too: every site gates "never myself"
    // through Poke.can()
    ui.global::<Poke>().set_me(b.member.clone().into());
    // the Chain-History panel: paged at 20, newest first (rows arrive
    // newest-first from read_chain)
    {
        let page = page_of(&b.list_pages, "chain", "history");
        let (start, end, page, pages) = page_slice(b.chain_rows.len(), page, LIST_PAGE_SIZE);
        let rows: Vec<ChainRow> = b.chain_rows[start..end]
            .iter()
            .map(|r| chain_row(b.lang, r))
            .collect();
        ui.set_chain_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_chain_page(i32::try_from(page + 1).unwrap_or(1));
        ui.set_chain_pages(i32::try_from(pages).unwrap_or(1));
    }
    ui.set_threshold_badge(b.threshold_badge.clone().into());
    let tabs: Vec<SurfaceTab> = b
        .surfaces
        .iter()
        .map(|s| {
            // the paged lists (page size LIST_PAGE_SIZE, pager row in the
            // .slint side): a gated surface's log IS its applied/accepted
            // history, so it pages; chat's log is the chat pane — full.
            // Counts stay full-list so the status strip and the nav badges
            // never shrink to a page.
            let a_page = page_of(&b.list_pages, &s.key, "applied");
            let (a_start, a_end, a_page, a_pages) = if s.gated {
                page_slice(s.log.len(), a_page, LIST_PAGE_SIZE)
            } else {
                (0, s.log.len(), 0, 1)
            };
            let log: Vec<LogLine> = s.log[a_start..a_end]
                .iter()
                .map(|l| {
                    let reactions: Vec<ReactionItem> = l
                        .reactions
                        .iter()
                        .map(|r| ReactionItem {
                            emoji: r.emoji.as_str().into(),
                            count: r.count,
                            mine: r.mine,
                        })
                        .collect();
                    let receipts: Vec<ReceiptItem> = l
                        .receipts
                        .iter()
                        .map(|r| ReceiptItem {
                            name: r.name.as_str().into(),
                            read: r.read,
                        })
                        .collect();
                    LogLine {
                        id: l.id.clone().into(),
                        lead: l.lead.clone().into(),
                        text: l.text.clone().into(),
                        when: l.when.clone().into(),
                        quote: l.quote,
                        system: l.system,
                        quote_label: l.quote_label.clone().into(),
                        quote_indent: l.quote_indent,
                        deleted_by: l.deleted_by.clone().into(),
                        first: l.first,
                        own: l.own,
                        alt: l.alt,
                        mine_emoji: l.mine_emoji.clone().into(),
                        reactions: ModelRc::new(VecModel::from(reactions)),
                        receipts: ModelRc::new(VecModel::from(receipts)),
                        has_file: l.has_file,
                        file_name: l.file_name.clone().into(),
                        file_meta: l.file_meta.clone().into(),
                        file_available: l.file_available,
                        // -1 = no known proposal origin: the row offers no
                        // 💬 jump (feedback honesty, like the id guards)
                        patch_id: l
                            .proposal_id
                            .and_then(|i| i32::try_from(i).ok())
                            .unwrap_or(-1),
                    }
                })
                .collect();
            let to_row = to_proposal_row;
            // pending pages too (ask 2026-08-15: proposal cards are tall,
            // a long list needs pages) — the nav badges and
            // `pending-my-vote` alerts stay full-list, so an open vote on
            // a later page still alarms
            let p_page = page_of(&b.list_pages, &s.key, "pending");
            let (p_start, p_end, p_page, p_pages) = if s.gated {
                page_slice(s.pending.len(), p_page, LIST_PAGE_SIZE)
            } else {
                (0, s.pending.len(), 0, 1)
            };
            let pending: Vec<ProposalRow> = s.pending[p_start..p_end].iter().map(to_row).collect();
            let d_page = page_of(&b.list_pages, &s.key, "declined");
            let (d_start, d_end, d_page, d_pages) =
                page_slice(s.declined.len(), d_page, LIST_PAGE_SIZE);
            let declined: Vec<ProposalRow> =
                s.declined[d_start..d_end].iter().map(to_decided_row).collect();
            // the Accepted table pages in lockstep with the applied log
            // (same source list, same length — `accepted` is its
            // newest-first projection)
            let (ac_start, ac_end, _, _) = if s.gated {
                page_slice(s.accepted.len(), a_page, LIST_PAGE_SIZE)
            } else {
                (0, 0, 0, 1)
            };
            let accepted: Vec<ProposalRow> =
                s.accepted[ac_start..ac_end].iter().map(to_decided_row).collect();
            // the surface's sub-views come straight from the shared
            // molt-core vocabulary (same list select_view validates against)
            let views: Vec<ViewItem> = Surface::parse(&s.key)
                .map(|sf| {
                    sf.views()
                        .iter()
                        .map(|(key, label)| ViewItem {
                            key: (*key).into(),
                            name: view_label(b.lang, key, label).into(),
                            icon: view_icon(key).into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            SurfaceTab {
                key: s.key.clone().into(),
                name: s.name.clone().into(),
                gated: s.gated,
                applied_count: s.log.len() as i32,
                pending_count: s.pending.len() as i32,
                pending_voted_count: s.pending_voted as i32,
                pending_my_vote_count: (s.pending.len() - s.pending_voted) as i32,
                denied_count: s.denied as i32,
                declined_count: s.declined.len() as i32,
                // the pager echo, 1-based for the "page x of y" label
                applied_page: (a_page + 1) as i32,
                applied_pages: a_pages as i32,
                declined_page: (d_page + 1) as i32,
                declined_pages: d_pages as i32,
                pending_page: (p_page + 1) as i32,
                pending_pages: p_pages as i32,
                log: ModelRc::new(VecModel::from(log)),
                pending: ModelRc::new(VecModel::from(pending)),
                declined: ModelRc::new(VecModel::from(declined)),
                accepted: ModelRc::new(VecModel::from(accepted)),
                views: ModelRc::new(VecModel::from(views)),
            }
        })
        .collect();
    sync_rows(&ui.get_surfaces(), tabs, |m| ui.set_surfaces(m));

    // the Shared-Memory base: hand the folded tree to the wiki model over
    // the WikiState bridge (this apply runs from a Send-bound closure that
    // cannot hold the UI-thread Rc model; the base-arrived handler can)
    if let Some(mem) = b.surfaces.iter().find(|s| s.key == "memory") {
        let g = ui.global::<WikiState>();
        let docs: Vec<WikiBase> = mem
            .wiki_tree
            .iter()
            .map(|(p, c)| WikiBase {
                path: p.as_str().into(),
                content: c.as_str().into(),
            })
            .collect();
        if let Some(fresh) = sync_vec_model(&g.get_base_docs(), docs) {
            g.set_base_docs(fresh);
        }
        g.set_base_rev(i32::try_from(mem.wiki_rev).unwrap_or(i32::MAX));
        g.invoke_base_arrived();
    }

    // the chat sidebar's channel rows + the canonical selection echo (so
    // the highlight always names the channel the engine filtered by)
    let channels: Vec<ChannelItem> = b
        .channels
        .iter()
        .map(|c| ChannelItem {
            key: c.key.as_str().into(),
            label: c.label.as_str().into(),
            icon: c.icon.as_str().into(),
            unread: c.unread,
        })
        .collect();
    // does the CURRENT filter have a row of its own? The Gruppe nav row
    // stands in for every filter that does not (the group itself, a decided
    // vote's read-only discussion), and it must NOT stand in for one that
    // does - a lit Gruppe row while the pane shows a topic tells the user
    // they are somewhere they are not.
    let selected_listed = b.channels.iter().any(|c| c.key == b.selected_key);
    sync_rows(&ui.get_chat_channels(), channels, |m| ui.set_chat_channels(m));
    ui.set_selected_channel_listed(selected_listed);
    ui.set_selected_channel(b.selected_key.as_str().into());
    ui.set_selected_channel_votable(b.selected_key.starts_with("patch:"));
    ui.set_selected_channel_label(b.selected_label.as_str().into());
    ui.set_selected_channel_closed(b.selected_closed);
    ui.set_selected_channel_org(b.selected_org);
    // the wiki-patch diff viewer: a SELECTED patch decision owns the
    // global; otherwise the first pending wiki patch on Shared Memory
    // does (its card in the proposals view carries the inline viewer).
    // Either way the parse refreshes only when the OWNING id changes, so
    // the user's file selection survives the mirror ticks.
    let decision_changed = ui.get_selected_decision().id != b.selected_decision.id;
    ui.set_selected_decision(to_proposal_row(&b.selected_decision));
    if b.selected_decision.patch_op {
        if decision_changed {
            patch_view_sync(
                ui,
                &b.selected_decision.proposed,
                0,
                b.selected_decision.id,
            );
        }
    } else {
        let mem_patch = b
            .surfaces
            .iter()
            .find(|s| s.key == "memory")
            .and_then(|s| s.pending.iter().find(|p| p.patch_op));
        let held = ui.global::<PatchView>().get_for_id();
        match mem_patch {
            Some(p) if held != p.id => patch_view_sync(ui, &p.proposed, 0, p.id),
            None if held != 0 => patch_view_sync(ui, "", 0, 0),
            _ => {}
        }
    }

    // the Organization tables (Members / Uploads). The avatars go through
    // the path-keyed cache: `sync_rows` below rewrites EVERY row on EVERY
    // push, so decoding here would re-decode the whole roster per tick
    let members: Vec<MemberRow> = AVATARS.with_borrow_mut(|cache| {
        cache.retain_live(&b.members.iter().map(|m| m.image_key.as_str()).collect());
        b.members
            .iter()
            .map(|m| {
                // the picture rode the applied proposal, so the engine
                // materialized the file on every device; decode by CONTENT
                // (the name's extension comes from a peer-supplied value)
                let avatar = (!m.image.is_empty())
                    .then(|| {
                        cache.get(&m.image_key, |_| {
                            std::fs::read(&m.image)
                                .ok()
                                .and_then(|b| image_from_bytes(&b))
                        })
                    })
                    .flatten();
                MemberRow {
                    name: m.name.as_str().into(),
                    id: m.id.as_str().into(),
                    pk: m.pk.as_str().into(),
                    last: m.last.as_str().into(),
                    state: m.state,
                    uploads: m.uploads,
                    split: m.split.as_str().into(),
                    avatar_set: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    avatar_path: m.image.as_str().into(),
                    desc: m.desc.as_str().into(),
                }
            })
            .collect()
    });
    sync_rows(&ui.get_org_members(), members, |m| ui.set_org_members(m));
    let uploads: Vec<UploadRow> = b
        .uploads
        .iter()
        .map(|u| UploadRow {
            id: u.id.as_str().into(),
            user: u.user.as_str().into(),
            date: u.date.as_str().into(),
            name: u.name.as_str().into(),
            kind: u.kind.as_str().into(),
            size: u.size.as_str().into(),
            available: u.available,
            online: u.online,
            checksum: u.checksum.as_str().into(),
            expires: u.expires.as_str().into(),
            status: u.status.as_str().into(),
            status_kind: u.status_kind,
            availability: u.availability.as_str().into(),
        })
        .collect();
    sync_rows(&ui.get_org_uploads(), uploads, |m| ui.set_org_uploads(m));

    // the tables' sort/filter echo: the headers render the ▲/▼ arrow from
    // these; the filter box only refreshes when Rust owns the change (a
    // workspace-switch reset or the members-table uploads-jump) — a live
    // keystroke bumps the push generation, so a stale echo never lands
    ui.set_om_sort_column(b.members_sort.as_str().into());
    ui.set_om_sort_ascending(b.members_asc);
    ui.set_ou_sort_column(b.uploads_sort.as_str().into());
    ui.set_ou_sort_ascending(b.uploads_asc);
    if ui.get_ou_filter().as_str() != b.uploads_filter {
        ui.set_ou_filter(b.uploads_filter.as_str().into());
    }

    ui.set_group_unread(b.group_unread);

    // the status info strip (founding date + mock activity trio)
    ui.set_org_founded(b.org_stats.founded.as_str().into());
    ui.set_org_chat_retention(b.org_stats.retention_days);
    // the Members table offers "recovery link" only where recovery exists
    ui.set_org_chain_governed(b.org_stats.chain_governed);
    let feat_on = |key: &str| b.org_stats.features.iter().any(|f| f == key);
    ui.set_org_feat_memory(feat_on("memory"));
    ui.set_org_feat_quests(feat_on("quests"));
    ui.set_org_feat_vault(feat_on("vault"));
    ui.set_org_feat_wallet(feat_on("wallet"));
    sync_strings(&ui.get_org_relays(), &b.org_stats.relays, |m| ui.set_org_relays(m));
    // the R6 pencil's draft prefill: the same pool, space-joined
    ui.set_org_relays_joined(b.org_stats.relays.join(" ").into());

    // the republic's image: (re)load the picture only when the file
    // reference changes. The bytes rode the applied set_image proposal, so
    // the engine materializes the logo file on EVERY device; decode by
    // CONTENT (image_from_bytes) — the reference's extension comes from a
    // peer-supplied display value and must not decide the format. On a
    // session-only workspace the reference is no local file — the read
    // fails quietly and the placeholder mark stays.
    if let Some(key) = logo_needs_reload(
        &LOGO_KEY.with_borrow(std::clone::Clone::clone),
        &b.org_stats.image,
    ) {
        LOGO_KEY.with_borrow_mut(|k| *k = key);
        ui.set_org_img_path(b.org_stats.image.as_str().into());
        let loaded = (!b.org_stats.image.is_empty())
            .then(|| std::fs::read(&b.org_stats.image).ok())
            .flatten()
            .and_then(|bytes| image_from_bytes(&bytes));
        ui.set_org_img_set(loaded.is_some());
        ui.set_org_img(loaded.unwrap_or_default());
    }
    // the picture budget this republic still carries: the engine stays the
    // authority, this is what the member-picture fit aims at
    ui.set_mp_img_budget(i32::try_from(b.org_stats.image_budget).unwrap_or(i32::MAX));
}

#[cfg(test)]
mod tests {
    /// E5 coverage: the German log table covers EXACTLY the engine's
    /// shape inventory (set-equal both ways); every rendering keeps the
    /// tone glyph and the slot count, a synthesized line round-trips
    /// with its slots intact, and unknown lines / non-German languages
    /// pass through verbatim.
    #[test]
    fn every_log_shape_has_a_german_rendering() {
        use std::collections::BTreeSet;
        let engine: BTreeSet<Vec<&str>> = molt_engine::known_log_shapes()
            .iter()
            .map(|s| s.to_vec())
            .collect();
        let gui: BTreeSet<Vec<&str>> = super::LOG_SHAPES_DE
            .iter()
            .map(|(en, _)| en.to_vec())
            .collect();
        assert_eq!(engine, gui, "engine shapes and the German table diverge");
        for (en, de) in super::LOG_SHAPES_DE {
            assert_eq!(en.len(), de.len(), "slot count differs: {en:?}");
            assert_eq!(
                en[0].chars().next(),
                de[0].chars().next(),
                "tone glyph lost: {en:?}"
            );
            let mut line = String::new();
            let mut want = String::new();
            for (i, (e, d)) in en.iter().zip(de.iter()).enumerate() {
                line.push_str(e);
                want.push_str(d);
                if i + 1 < en.len() {
                    let slot = format!("S{i}");
                    line.push_str(&slot);
                    want.push_str(&slot);
                }
            }
            assert_eq!(
                super::localize_log_line(1, &line),
                want,
                "round-trip failed for {en:?}"
            );
            assert_ne!(want, line, "German rendering equals English: {en:?}");
            assert_eq!(super::localize_log_line(0, &line), line);
        }
        assert_eq!(
            super::localize_log_line(1, "→ some brand new line"),
            "→ some brand new line"
        );
    }

    /// E6: the transport-pill reason, S3 verdicts, Tor details and the
    /// recovery status lines render German part-wise; machine states and
    /// free-text error tails ride verbatim.
    #[test]
    fn e6_maps_render_german_and_keep_tails() {
        use super::{
            localize_net_reason, localize_recover_failed, localize_recover_note,
            localize_s3_verdict, localize_tor_detail, tor_gap_de,
        };
        // net reason: compound parts — member, count and free tail survive
        let r = "link to walter: connecting; sends to mara: io: broken pipe; \
                 relays: no relay accepted the subscription; 3 frames past the key ring";
        assert_eq!(
            localize_net_reason(1, r),
            "Verbindung zu walter: verbinde; Zustellung an mara: io: broken pipe; \
             Relays: kein Relay nahm die Subscription an; 3 Frames jenseits des Schlüsselrings"
        );
        assert_eq!(localize_net_reason(0, r), r);
        assert_eq!(
            localize_net_reason(1, "no live relay connection (0 of 3 up, reconnecting)"),
            "keine lebende Relay-Verbindung (0 von 3 erreichbar, verbinde neu)"
        );
        // the offline statics match by prefix (the engine wraps their tails)
        assert!(localize_net_reason(
            1,
            "offline: no mesh links on disk - rejoin via a recovery link"
        )
        .starts_with("offline: keine Mesh-Links"));
        // s3: machine states untouched; shells + hints localized, code rides
        assert_eq!(localize_s3_verdict(1, "testing"), "testing");
        assert_eq!(localize_s3_verdict(1, "ok"), "ok");
        assert_eq!(
            localize_s3_verdict(1, "error: endpoint: no bucket configured"),
            "Fehler: Endpunkt: kein Bucket konfiguriert"
        );
        assert_eq!(
            localize_s3_verdict(
                1,
                "error: http 403: access denied - check access key and secret (AccessDenied)"
            ),
            "Fehler: HTTP 403: Zugriff verweigert - Access-Key und Secret prüfen (AccessDenied)"
        );
        assert_eq!(
            localize_s3_verdict(1, "error: http 404: bucket `media` not found"),
            "Fehler: HTTP 404: Bucket `media` nicht gefunden"
        );
        // tor: the four gap clauses stay distinct; rung tails verbatim
        let gaps = [
            "no relay is configured",
            "no relay is confirmed yet",
            "the confirmed relays need non-onion dialing, which is switched off",
            "only local relays are configured, and those bypass Tor",
        ];
        let mut des: Vec<String> = gaps.iter().map(|g| tor_gap_de(g)).collect();
        for (g, d) in gaps.iter().zip(&des) {
            assert_ne!(d, g, "gap clause without a German arm: {g}");
        }
        des.sort();
        des.dedup();
        assert_eq!(des.len(), 4, "gap renderings collide");
        assert_eq!(
            localize_tor_detail(1, "no circuit was proven - no relay is confirmed yet"),
            "kein Circuit bewiesen - noch kein Relay bestätigt"
        );
        assert_eq!(
            localize_tor_detail(1, "no relay handshake through Tor to x.onion: timed out"),
            "no relay handshake through Tor to x.onion: timed out"
        );
        // recovery: known notes + failure prefixes, tails verbatim
        assert_eq!(
            localize_recover_note(1, "waiting for the coordinator's Welcome (7 min)"),
            "warte auf das Welcome des Koordinators (7 min)"
        );
        assert_eq!(
            localize_recover_failed(1, "recovery request: relay refused"),
            "Recovery-Anfrage: relay refused"
        );
    }

    /// E6: every wiki-side refusal literal renders German — pinned against
    /// the SOURCE, so a new `Err("…")` in wiki.rs goes red here until it
    /// gets an arm in `localize_wiki_err`.
    #[test]
    fn every_wiki_error_renders_german() {
        let src = include_str!("wiki.rs");
        let mut found = 0;
        for part in src.split("Err(\"").skip(1) {
            let lit = part.split('"').next().expect("literal terminates");
            found += 1;
            let de = super::localize_wiki_err(1, lit);
            assert_ne!(de, lit, "wiki error without a German arm: {lit:?}");
            assert!(!de.is_empty());
        }
        assert!(found >= 20, "the wiki.rs error scan found only {found} sites");
        // honest fallback + non-German identity
        assert_eq!(super::localize_wiki_err(1, "some new error"), "some new error");
        assert_eq!(super::localize_wiki_err(0, "unknown folder"), "unknown folder");
    }

    /// E3 coverage: every headline phrase the engine can emit has a
    /// German rendering — a new phrase without one goes red here instead
    /// of silently showing English in the German UI. (The engine pins the
    /// inventory producible; this pins it translated.)
    #[test]
    fn every_engine_headline_has_a_german_rendering() {
        for phrase in molt_engine::known_headlines() {
            let de = super::localize_headline(1, phrase);
            assert_ne!(
                &de, phrase,
                "phrase without a German arm: {phrase}"
            );
            assert!(!de.is_empty());
        }
        // …and the honest fallback: unknown phrases render as themselves
        assert_eq!(super::localize_headline(1, "Brand new phrase"), "Brand new phrase");
        assert_eq!(super::localize_headline(0, "No shared relay"), "No shared relay");
    }

    /// E2: the error toast renders in the active language, and the match
    /// carries NO wildcard — a new MoltError variant fails compilation in
    /// `localize_error` until it gets a German arm.
    #[test]
    fn engine_errors_render_in_the_active_language() {
        let e = molt_core::MoltError::UnknownProposal(molt_core::ProposalId(7));
        assert_eq!(super::localize_error(0, &e), e.to_string(), "EN = engine Display (MCP parity)");
        assert_eq!(super::localize_error(1, &e), "Unbekannter Vorschlag #7");
        let e = molt_core::MoltError::WorkspaceEncrypted("R".to_string());
        assert!(super::localize_error(1, &e).contains("versiegelt"));
    }

    /// R1 (relay_topology_plan): the create wizard states rule 1 — ONE
    /// relay every member can reach (the join runs over the INTERSECTION;
    /// "identical pool" was a stricter, false rule that contradicted the
    /// engine's own gate) — plus the self-hosted branch.
    #[test]
    fn the_create_wizard_states_the_one_shared_relay_rule() {
        for l in [Lexicon::en(), Lexicon::de()] {
            let h = l.cw_relays_hint;
            assert!(
                h.contains("ONE relay") || h.contains("EIN Relay"),
                "branch 1 - one shared relay: {h}"
            );
            assert!(
                h.to_lowercase().contains("pool"),
                "branch 2 - the self-hosted relay in every pool: {h}"
            );
            assert!(
                !h.contains("identical") && !h.contains("identischen"),
                "the pool need not be identical - the join runs over the intersection: {h}"
            );
        }
    }

    /// L10: the retention pair renders its unit in the ACTIVE language —
    /// the payload carries the machine value, and a legacy "30 days"
    /// normalizes by its leading number instead of leaking English into
    /// the German card.
    #[test]
    fn the_retention_pair_renders_its_unit_in_the_active_language() {
        assert_eq!(super::retention_value(0, "7"), "7 days");
        assert_eq!(super::retention_value(1, "7"), "7 Tage");
        assert_eq!(super::retention_value(1, "30 days"), "30 Tage");
        assert_eq!(super::retention_value(0, ""), "", "unknown stays untouched");
    }

    use super::*;
    use molt_core::{ChannelInfo, ChatMessage, ProposalState, ProposalView};

    /// The set_relays vote card shows the CHANGES: every pool member of the
    /// union, marked kept / added / removed, in current-then-added order.
    /// Review 2026-08-12: a set_features card must never paint a red
    /// "removed" row - the union fold cannot remove, and `current` is
    /// recomputed live, so a racing enable would otherwise show an
    /// impossible removal on a governance card. Keys render as display
    /// labels (one vocabulary with nav and wizard).
    #[test]
    fn a_feature_diff_never_shows_a_removal_and_renders_labels() {
        let pv = ProposalView {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            approvals: 1,
            threshold: 2,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            // a racing enable made "vault" effective AFTER this was proposed
            current: "memory vault".to_string(),
            proposed: "memory quests".to_string(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let row = proposal_row(0, &pv);
        assert!(
            row.relay_changes.iter().all(|(sign, _)| *sign != RELAY_ROW_REMOVED),
            "a feature diff row claimed a removal: {:?}",
            row.relay_changes
        );
        assert!(
            row.relay_changes
                .iter()
                .any(|(sign, label)| *sign == RELAY_ROW_KEPT && label == "Vault"),
            "the racing enable renders as kept, labelled: {:?}",
            row.relay_changes
        );
        assert!(
            row.relay_changes
                .iter()
                .any(|(sign, label)| *sign == RELAY_ROW_ADDED && label == "Kanban"),
            "the addition renders with its display label: {:?}",
            row.relay_changes
        );
    }

    #[test]
    fn relay_pool_diff_marks_added_removed_kept() {
        let rows = relay_pool_diff("wss://a wss://b", "wss://b wss://c");
        assert_eq!(
            rows,
            vec![
                (RELAY_ROW_REMOVED, "wss://a".to_string()),
                (RELAY_ROW_KEPT, "wss://b".to_string()),
                (RELAY_ROW_ADDED, "wss://c".to_string()),
            ]
        );
        // identical pools: everything kept, nothing invented
        assert_eq!(
            relay_pool_diff("wss://a", "wss://a"),
            vec![(RELAY_ROW_KEPT, "wss://a".to_string())]
        );
        // duplicates in a hand-written proposal collapse
        assert_eq!(
            relay_pool_diff("", "wss://x wss://x"),
            vec![(RELAY_ROW_ADDED, "wss://x".to_string())]
        );
        // an empty proposed pool folds as a no-op engine-side, so the card
        // must NOT promise removals — no rows, generic fallback
        assert_eq!(relay_pool_diff("wss://a", ""), Vec::<(i32, String)>::new());
    }

    fn line(lead: &str, text: &str) -> LogLineData {
        LogLineData {
            id: String::new(),
            lead: lead.to_string(),
            text: text.to_string(),
            when: String::new(),
            quote: -1,
            quote_id: String::new(),
            system: false,
            quote_label: String::new(),
            quote_indent: 0,
            deleted_by: String::new(),
            first: true,
            own: false,
            alt: false,
            mine_emoji: String::new(),
            reactions: Vec::new(),
            receipts: Vec::new(),
            has_file: false,
            file_name: String::new(),
            file_meta: String::new(),
            file_available: false,
            proposal_id: None,
        }
    }

    /// A deterministic 32-char hex id for tests.
    fn hex_id(b: u8) -> String {
        MessageId([b; 16]).to_string()
    }

    fn qsrc(lead: &str, text: &str, deleted: bool) -> QuoteSrc {
        QuoteSrc {
            lead: lead.to_string(),
            text: text.to_string(),
            deleted,
        }
    }

    /// The pending-card image preview decodes the payload bytes that rode
    /// the `set_image` proposal — for EVERY format the propose-side picker
    /// offers (png, jpg, jpeg, webp, gif, svg, bmp). The decode must key on
    /// the CONTENT, never on a file extension: the payload is raw bytes, no
    /// name travels with it. (This pins the bug where the bytes were staged
    /// as a `.img` temp file and `slint::Image::load_from_path` — which
    /// trusts extensions — failed for every proposal, so "Click to view the
    /// proposed image" only ever produced the failure toast.)
    #[test]
    fn a_proposed_image_decodes_from_the_payload_for_every_picker_format() {
        // real minimal files, one per picker format (2x2 red, PIL-generated)
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        let gif = "R0lGODdhAgACAIEAAMgeHgAAAAAAAAAAACwAAAAAAgACAAAIBgABCAQQEAA7";
        let bmp = "Qk1GAAAAAAAAADYAAAAoAAAAAgAAAAIAAAABABgAAAAAABAAAADEDgAAxA4AAAAAAAAAAAAAHh7IHh7IAAAeHsgeHsgAAA==";
        let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
        let jpeg = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAACAAIDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDkKKKK8U/TD//Z";
        for (fmt, b64) in [
            ("png", png),
            ("gif", gif),
            ("bmp", bmp),
            ("webp", webp),
            ("jpeg", jpeg),
        ] {
            let img = proposal_image_from_b64(b64);
            assert!(img.is_some(), "the {fmt} payload must decode");
            let img = img.expect("checked above");
            assert_eq!(img.size().width, 2, "{fmt} decodes to the real picture");
            assert_eq!(img.size().height, 2, "{fmt} decodes to the real picture");
        }
        // svg travels as its source text
        use base64::Engine as _;
        let svg = base64::engine::general_purpose::STANDARD.encode(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
        );
        assert!(
            proposal_image_from_b64(&svg).is_some(),
            "an svg payload must decode"
        );
    }

    /// Undecodable payloads answer `None` — the caller shows the honest
    /// "could not be decoded" toast, never a broken image.
    #[test]
    fn an_undecodable_image_payload_is_none_not_a_panic() {
        assert!(proposal_image_from_b64("").is_none(), "empty payload");
        assert!(
            proposal_image_from_b64("not base64 at all!").is_none(),
            "not base64"
        );
        use base64::Engine as _;
        let garbage = base64::engine::general_purpose::STANDARD.encode([0x00u8; 64]);
        assert!(
            proposal_image_from_b64(&garbage).is_none(),
            "valid base64, but not an image"
        );
    }

    /// **The `Strings`/`lexicon!` pairing is guarded in ONE direction
    /// only**: an entry whose field has no property fails to compile, but
    /// a property with no entry compiles and renders as an EMPTY string in
    /// both languages. This scans the two sources against each other, so a
    /// forgotten pair goes red here instead of shipping a blank label.
    #[test]
    fn every_strings_property_has_an_english_and_a_german_arm() {
        let theme = include_str!("../../molt-ui-window/ui/theme.slint");
        let lex = include_str!("i18n.rs");
        // the Strings global alone - Theme, HintTip and Poke declare
        // string properties of their own
        let block = theme
            .split("export global Strings {")
            .nth(1)
            .expect("the Strings global")
            .split("\n}")
            .next()
            .expect("the global closes");
        let mut keys = 0;
        for line in block.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.split_once("property <string> ") else {
                continue;
            };
            let key = rest
                .1
                .split([';', ':'])
                .next()
                .expect("a property name")
                .trim();
            keys += 1;
            let field = key.replace('-', "_");
            assert!(
                lex.contains(&format!("\n    {field}: \"")),
                "Strings.{key} has no lexicon! entry - it renders EMPTY"
            );
        }
        assert!(keys > 500, "the Strings scan found only {keys} properties");
    }

    // ---------------------------------------------------------------
    // Member profiles (`member_profiles_plan.md` §5): the picture a seat
    // proposes for itself is fitted HERE - square and inside this
    // republic's served budget - before the engine ever sees it.
    // ---------------------------------------------------------------

    /// A `w x h` picture with incompressible content: a flat colour would
    /// fit any budget at any edge and prove nothing about the downscale.
    pub(super) fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        let mut seed: u32 = 0x1234_5678;
        for p in img.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *p = image::Rgb([(seed >> 16) as u8, (seed >> 8) as u8, seed as u8]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    /// The engine refuses a non-square member picture (every frontend
    /// renders it in a square box), so the fit crops from the CENTRE -
    /// a top-left crop would behead every portrait.
    #[test]
    fn a_wide_picture_is_center_cropped_to_a_square() {
        use image::GenericImageView as _;
        let wide = noisy_png(40, 20);
        let fitted = fit_member_image(&wide, 1 << 20).expect("a small picture fits");
        let out = image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
        assert_eq!(
            out.width(),
            out.height(),
            "the engine refuses a non-square picture"
        );
        assert_eq!(out.width(), 20, "the square is the shorter edge");
        let src = image::load_from_memory(&wide).expect("source decodes");
        assert_eq!(
            out.get_pixel(0, 0),
            src.get_pixel(10, 0),
            "the crop starts at the middle, not at the left edge"
        );
    }

    /// The served budget is the promise the engine keeps; a picture over
    /// it is stepped down until it fits, not sent to be refused.
    #[test]
    fn an_oversized_picture_lands_inside_the_budget() {
        let big = noisy_png(1024, 1024);
        let budget = 40 * 1024;
        assert!(big.len() > budget, "the fixture must actually be oversized");
        let fitted = fit_member_image(&big, budget).expect("a downscale fits it");
        assert!(
            fitted.bytes.len() <= budget,
            "{} bytes over a {budget} byte budget",
            fitted.bytes.len()
        );
        image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
    }

    /// A picture that is already square and already small travels as the
    /// bytes the user picked - a re-encode would only lose quality.
    #[test]
    fn a_picture_that_already_fits_is_proposed_untouched() {
        let small = noisy_png(64, 64);
        let fitted = fit_member_image(&small, 1 << 20).expect("it fits");
        assert_eq!(fitted.bytes, small, "no re-encode when none is needed");
        assert_eq!(fitted.ext, "png", "the name must not lie about the format");
    }

    /// Below the floor the honest answer is a refusal: a 128px avatar that
    /// still does not fit means the republic has no room for a picture.
    #[test]
    fn a_budget_below_the_floor_is_refused_honestly() {
        let big = noisy_png(1024, 1024);
        assert!(
            matches!(fit_member_image(&big, 400), Err(ImageFitError::TooLarge)),
            "an unreachable budget must refuse, never ship a 1px avatar"
        );
    }

    /// Undecodable bytes are caught by the frontend's real decoder, the
    /// same pre-check `on_org_propose` runs for the logo.
    #[test]
    fn undecodable_bytes_never_reach_the_proposal() {
        assert!(matches!(
            fit_member_image(b"not an image at all", 1 << 20),
            Err(ImageFitError::Undecodable)
        ));
    }

    /// A seat that REPLACES its picture keeps the same file name
    /// (`avatar-<stem>.<ext>`), so a path-only cache key would keep
    /// showing the old face until the app restarts. The key carries the
    /// file's identity, not just its name.
    /// The republic's picture must survive a REPLACEMENT: same file name,
    /// new content. A path compare says "unchanged" and the window keeps the
    /// old logo until a restart - the bug this rule replaced.
    #[test]
    fn a_replaced_logo_forces_a_reload_although_its_path_is_unchanged() {
        let tmp = tempfile::tempdir().expect("tmp");
        let logo = tmp.path().join("logo.png");
        let path = logo.display().to_string();
        std::fs::write(&logo, noisy_png(8, 8)).expect("write the first logo");

        let first = super::logo_needs_reload("", &path).expect("a first picture always loads");
        assert_eq!(
            super::logo_needs_reload(&first, &path),
            None,
            "an unchanged picture must not be decoded again on every push"
        );

        std::fs::write(&logo, noisy_png(16, 16)).expect("replace the logo");
        let second = super::logo_needs_reload(&first, &path)
            .expect("a replaced picture must reload behind its unchanged path");
        assert_ne!(first, second, "the key moves with the content");

        assert_eq!(
            super::logo_needs_reload("", ""),
            None,
            "a republic without a picture never reloads one"
        );
    }

    #[test]
    fn the_avatar_cache_key_moves_when_the_file_content_does() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("avatar-walter.png");
        std::fs::write(&path, noisy_png(8, 8)).expect("write");
        let p = path.display().to_string();
        let first = avatar_cache_key(&p);
        assert!(first.starts_with(&p), "the key still names the file: {first}");
        assert_eq!(first, avatar_cache_key(&p), "an untouched file keys the same");
        // the same NAME, a different picture
        std::fs::write(&path, noisy_png(16, 16)).expect("rewrite");
        assert_ne!(
            first,
            avatar_cache_key(&p),
            "a replaced picture must invalidate the cached decode"
        );
        assert_eq!(avatar_cache_key(""), "", "no picture, no key");
    }

    /// `sync_rows` rewrites EVERY row on EVERY mirror push, so a decode
    /// inside the row mapping would re-decode the whole roster per tick.
    #[test]
    fn an_avatar_decodes_once_per_path_and_forgets_the_gone_ones() {
        let mut cache = AvatarCache::default();
        let loads = std::cell::Cell::new(0);
        let load = |_p: &str| {
            loads.set(loads.get() + 1);
            Some(slint::Image::default())
        };
        assert!(cache.get("/w/avatar-a.png", load).is_some());
        assert!(cache.get("/w/avatar-a.png", load).is_some());
        assert_eq!(loads.get(), 1, "one decode per path, not per push");
        // a miss is remembered too - a picture whose file is not on this
        // device must not re-stat on every tick either
        let missing = |_p: &str| {
            loads.set(loads.get() + 1);
            None
        };
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert_eq!(loads.get(), 2, "the miss is cached like the hit");
        let live: std::collections::HashSet<&str> = ["/w/avatar-a.png"].into_iter().collect();
        cache.retain_live(&live);
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert_eq!(loads.get(), 3, "a dropped path decodes again");
    }

    /// One `ProposalView` carrying a member-profile payload.
    fn profile_view(op: &str, member: &str) -> ProposalView {
        let mut v = view_of(1, "", ProposalState::Proposed);
        v.surface = Surface::Organization;
        v.payload = serde_json::json!({ "op": op, "member": member });
        v
    }

    /// A member picture rides the same inline-preview and save path the
    /// org logo has - the bytes are in the payload either way.
    #[test]
    fn a_member_picture_proposal_offers_the_preview() {
        for op in ["set_member_image", "remove_member_image"] {
            assert!(
                proposal_row(0, &profile_view(op, "walter")).image_op,
                "{op} must render as a picture change"
            );
        }
        assert!(
            !proposal_row(0, &profile_view("set_member_desc", "walter")).image_op,
            "a description carries no picture"
        );
        let mut v = profile_view("set_member_image", "walter");
        v.payload["bytes_b64"] = serde_json::json!("QUJD");
        assert_eq!(
            proposal_row(0, &v).img_b64,
            "QUJD",
            "the bytes reach the preview"
        );
    }

    /// A profile change is about ONE seat - the card says whose.
    #[test]
    fn member_profile_titles_name_the_seat_in_both_languages() {
        for (op, en, de) in [
            ("set_member_image", "Picture: walter", "Bild: walter"),
            (
                "set_member_desc",
                "Description: walter",
                "Beschreibung: walter",
            ),
            (
                "remove_member_image",
                "Remove picture: walter",
                "Bild entfernen: walter",
            ),
        ] {
            let payload = serde_json::json!({ "op": op, "member": "walter" });
            assert_eq!(display_title(0, &payload), en);
            assert_eq!(display_title(1, &payload), de);
        }
        // a profile payload without a seat cannot claim one
        let anon = serde_json::json!({ "op": "set_member_desc", "value": "hi" });
        assert!(!display_title(0, &anon).contains("Description:"));
    }

    /// An engine-authored System-kind message maps onto the same per-line
    /// `system` flag the governance rows use — one quiet rendering path,
    /// never a second style; a User message stays a normal card.
    #[test]
    fn a_system_kind_message_maps_onto_the_quiet_line_flag() {
        let user = ChatMessage::text(MessageId([1; 16]), "petra", "gm", 100);
        assert!(!chat_line(0, &user, "me", &[]).system);
        let notice = ChatMessage::text(MessageId([2; 16]), "petra", "🔑 back", 101)
            .with_kind(molt_core::ChatKind::System);
        assert!(chat_line(0, &notice, "me", &[]).system);
    }

    /// Read receipts show ONLY on the local member's own messages (the sender
    /// wants delivery confirmation) — one dot per OTHER member, green once in
    /// read_by; an incoming message carries no receipt row at all.
    #[test]
    fn read_receipts_render_only_on_own_messages() {
        let roster = vec!["me".to_string(), "ada".to_string(), "bo".to_string()];

        // my own message: a dot per OTHER member, ada green (read), bo yellow
        let mut mine = ChatMessage::text(MessageId([3; 16]), "me", "hi", 100);
        mine.read_by.insert("ada".to_string());
        let r = chat_line(0, &mine, "me", &roster).receipts;
        assert_eq!(r.len(), 2, "one dot per other member");
        assert_eq!(r.iter().find(|x| x.name == "ada").map(|x| x.read), Some(true));
        assert_eq!(r.iter().find(|x| x.name == "bo").map(|x| x.read), Some(false));
        assert!(r.iter().all(|x| x.name != "me"), "the author gets no self-dot");

        // an incoming message (not mine): NO receipt row
        let mut theirs = ChatMessage::text(MessageId([4; 16]), "ada", "yo", 101);
        theirs.read_by.insert("me".to_string());
        assert!(
            chat_line(0, &theirs, "me", &roster).receipts.is_empty(),
            "a received message shows no receipts"
        );
    }

    /// The recovery flow rides the transient session notice (the engine's
    /// contract: `recovery-link-pending:` / `recovery-link:` /
    /// `recovery-link-failed:` / `recover-started:` / `recover-failed:` /
    /// `recovered:`); the parser must split each prefix off verbatim and
    /// treat everything else — including the existing notices — as none.
    #[test]
    fn recover_notices_parse_into_their_ui_effects() {
        assert_eq!(
            parse_recover_notice("recovery-link:molt://recover/abc"),
            RecoverNotice::Link("molt://recover/abc".to_string())
        );
        assert_eq!(
            parse_recover_notice("recover-started:ashi"),
            RecoverNotice::Started("ashi".to_string())
        );
        assert_eq!(
            parse_recover_notice("recover-failed:the survivors declined"),
            RecoverNotice::Failed("the survivors declined".to_string())
        );
        assert_eq!(
            parse_recover_notice("recovered:ashi"),
            RecoverNotice::Done("ashi".to_string())
        );
        // the coordinator's mint lifecycle: pending on the attempt, then the
        // outcome — a calm failed state (the flip side of Link) whose payload
        // is a reason the dialog maps onto localized text
        assert_eq!(
            parse_recover_notice("recovery-link-pending:ashi"),
            RecoverNotice::LinkPending("ashi".to_string())
        );
        assert_eq!(
            parse_recover_notice("recovery-link-failed:mesh-not-running"),
            RecoverNotice::LinkFailed("mesh-not-running".to_string())
        );
        // `recovery-link-failed:` must not be swallowed by the shorter
        // `recovery-link:` prefix — order in the parser matters
        assert_eq!(
            parse_recover_notice("recovery-link-failed:transport: queue gone"),
            RecoverNotice::LinkFailed("transport: queue gone".to_string())
        );
        // the non-recovery notices stay untouched by this path
        assert_eq!(parse_recover_notice("saved"), RecoverNotice::None);
        assert_eq!(parse_recover_notice("save-failed: disk"), RecoverNotice::None);
        assert_eq!(parse_recover_notice(""), RecoverNotice::None);
        // an error that itself contains a colon survives whole
        assert_eq!(
            parse_recover_notice("recover-failed:transport: queue gone"),
            RecoverNotice::Failed("transport: queue gone".to_string())
        );
    }

    /// Rewrite of the pre-chat-bus author-block/teaser tests, meaning
    /// preserved: header once per block, zebra flips on author change,
    /// quotes tease "author: body", dangling quotes are dropped — but the
    /// quotes are now id-addressed, resolve their teaser through the
    /// full-log map (so a cross-channel quote teases without a jump row)
    /// and deleted targets tease with an ellipsis.
    #[test]
    fn annotate_chat_log_resolves_quotes_by_id() {
        let mut log = vec![
            line("me", "first"),
            line("me", "second"),
            line("ashi", "answer"),
            line("me", "back"),
        ];
        for (i, l) in log.iter_mut().enumerate() {
            l.id = hex_id(u8::try_from(i).expect("tiny") + 1);
        }
        log[2].quote_id = hex_id(1); // in view → teaser + jump row
        log[3].quote_id = hex_id(99); // dangling id → dropped
        let quotes = HashMap::from([(hex_id(1), qsrc("me", "first", false))]);
        annotate_chat_log(&mut log, &quotes);
        // the header shows once per author block …
        assert_eq!(
            log.iter().map(|l| l.first).collect::<Vec<_>>(),
            [true, false, true, true]
        );
        // … and the zebra flips exactly on author changes
        assert_eq!(
            log.iter().map(|l| l.alt).collect::<Vec<_>>(),
            [false, false, true, false]
        );
        assert_eq!(log[2].quote_label, "me: first");
        assert_eq!(log[2].quote, 0, "the jump target is the quoted row");
        assert_eq!(log[3].quote, -1, "dangling quotes are dropped");
        assert_eq!(log[3].quote_label, "");

        // a deleted target teases with an ellipsis; a target OUTSIDE the
        // displayed log (cross-channel quote — the sanctioned cross-post)
        // teases from the full-log map but offers no jump row
        let mut log = vec![line("ashi", "reply")];
        log[0].id = hex_id(2);
        log[0].quote_id = hex_id(1);
        let quotes = HashMap::from([(hex_id(1), qsrc("me", "", true))]);
        annotate_chat_log(&mut log, &quotes);
        assert_eq!(log[0].quote_label, "me: …");
        assert_eq!(log[0].quote, -1, "not in view: teaser without a jump");

        // legacy numeric quotes (pre-chat-bus rows) still resolve by row
        let mut log = vec![line("me", "first"), line("ashi", "answer"), line("me", "back")];
        log[1].quote = 0;
        log[2].quote = 99; // out of range
        annotate_chat_log(&mut log, &HashMap::new());
        assert_eq!(log[1].quote_label, "me: first");
        assert_eq!(log[2].quote, -1, "out-of-range legacy quotes are dropped");
    }

    #[test]
    fn derive_channels_lists_only_open_vote_discussions() {
        let known_of = |title: &str, fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": title}),
            surface: Surface::Memory,
            approvals: 1,
            threshold: 2,
            fate,
        };
        let infos = vec![
            ChannelInfo {
                channel: ChannelRef::Topic { name: "zeta".into() },
                count: 4,
                last_ts: 40,
                state: None,
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(7) },
                count: 1,
                last_ts: 30,
                state: None,
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(5) },
                count: 2,
                last_ts: 20,
                state: Some(ProposalState::Applied),
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(3) },
                count: 5,
                last_ts: 10,
                state: Some(ProposalState::Proposed),
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Group,
                count: 9,
                last_ts: 50,
                state: None,
                unread: 0,
            },
        ];
        let known = HashMap::from([
            (3u64, known_of("raise budget", KnownFate::Pending)),
            (5u64, known_of("sealed one", KnownFate::Applied)),
        ]);
        let unread = HashMap::from([("patch:3".to_string(), 2usize), ("group".to_string(), 1)]);
        let rows = derive_channels(0, &infos, &known, &unread);
        // topics first (a human named them), then the discussions of OPEN
        // votes. No group row - the Gruppe nav view covers it - and no
        // sealed/closed votes or unknown proposals: a discussion is
        // vote-bound and dies with its vote.
        //
        // The TOPIC row is the one this list lost once, and losing it made
        // the New-topic button a trapdoor: the channel existed and held
        // messages with nowhere to click back to.
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["topic:zeta", "patch:3"],
            "the topic keeps its row; only the open vote's discussion survives"
        );
        assert_eq!(rows[0].label, "zeta", "a topic is labelled by its name");
        assert_eq!(rows[1].label, "raise budget", "patch title from proposal state");
        assert_eq!(rows[1].unread, 2);
        // nothing open → no rows (the sidebar hides the whole section)
        let rows = derive_channels(0, &[], &HashMap::new(), &HashMap::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn vote_jump_targets_the_hosting_surface_and_fate_view() {
        let known_of = |surface: Surface, fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": "t"}),
            surface,
            approvals: 0,
            threshold: 2,
            fate,
        };
        let known = HashMap::from([
            (5u64, known_of(Surface::Organization, KnownFate::Pending)),
            (6u64, known_of(Surface::Organization, KnownFate::Closed)),
            (7u64, known_of(Surface::Memory, KnownFate::Pending)),
        ]);
        // only a patch channel has a vote to jump back to
        assert!(vote_jump_command(&ChannelRef::Group, &known).is_none());
        let topic = ChannelRef::Topic { name: "zeta".to_string() };
        assert!(vote_jump_command(&topic, &known).is_none());
        // an open Organization vote → its card sits in the pending view
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(5) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
        ));
        // a closed one moved to the declined view
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(6) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "declined"
        ));
        // a gated surface hosts its cards on its main view — plain surface
        // selection, exactly like the sidebar row
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(7) }, &known),
            Some(Command::SelectSurface { surface: Surface::Memory })
        ));
        // a cache miss (this UI never saw the proposal) falls back to the
        // Organization pending view — never a dead button
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(99) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
        ));
        // WP1: an APPLIED Organization vote's row lives in the accepted view
        let known = HashMap::from([(8u64, {
            let mut k = known_of(Surface::Organization, KnownFate::Applied);
            k.approvals = 2;
            k
        })]);
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(8) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "accepted"
        ));
    }

    /// Discussion/card titles must never mix languages: an org governance
    /// payload carries the machine `op` as its placeholder and the UI
    /// translates it AT RENDER TIME in the active language — never a
    /// pre-rendered string frozen in whatever language the proposer's UI
    /// happened to be in. User content (note titles) passes through.
    #[test]
    fn org_titles_render_in_the_active_language_from_the_op_placeholder() {
        let payload = serde_json::json!({"op": "set_name", "value": "Neu"});
        assert_eq!(display_title(0, &payload), "Rename");
        assert_eq!(display_title(1, &payload), "Name ändern");
        // a legacy payload with a baked, possibly foreign-language title:
        // the op placeholder still wins for governance ops
        let legacy =
            serde_json::json!({"op": "set_image", "title": "Logo ändern", "value": "x.png"});
        // short noun labels: the sidebar channel list elides long titles,
        // and a leading "Change …" verb is redundant on a proposal anyway
        assert_eq!(display_title(0, &legacy), "Logo");
        // user content is the title — untouched, in any language
        let note = serde_json::json!({"op": "add_note", "title": "budget"});
        assert_eq!(display_title(0, &note), "budget");
        assert_eq!(display_title(1, &note), "budget");
    }

    /// WP1: an applied log line carries the id of the proposal that produced
    /// it (the snapshot's parallel id track), so the row can offer the 💬
    /// jump into the vote's discussion. A row with no known origin (legacy
    /// dump, pre-id peer) carries none and must offer no jump.
    #[test]
    fn applied_log_lines_carry_their_patch_id() {
        let snap = molt_core::SurfaceSnapshot {
            surface: Surface::Memory,
            gated: true,
            applied: vec![
                serde_json::json!({"op": "add_note", "title": "a"}),
                serde_json::json!({"op": "add_note", "title": "b"}),
            ],
            applied_ids: vec![Some(7), None],
            pending: Vec::new(),
            denied: 0,
            declined: Vec::new(),
            accepted: vec![ProposalView {
                id: ProposalId(7),
                surface: Surface::Memory,
                payload: serde_json::json!({"op": "add_note", "title": "a"}),
                approvals: 2,
                threshold: 2,
                state: molt_core::ProposalState::Applied,
                approved_by_me: true,
                declined_by_me: false,
                current: String::new(),
                proposed: String::new(),
                votes: vec![
                    molt_core::MemberVote {
                        member: "petra".to_string(),
                        vote: molt_core::VoteState::Approved,
                    },
                    molt_core::MemberVote {
                        member: "walter".to_string(),
                        vote: molt_core::VoteState::Approved,
                    },
                ],
                declined_at: 0,
                declined_by: String::new(),
                by: String::new(),
                mine: false,
                superseded: false,
                withdrawn: false,
            }],
            channels: Vec::new(),
            has_archive: false,
            wiki_tree: Vec::new(),
            wiki_rev: 0,
        };
        let data = surface_data(0, Surface::Memory, &snap, "petra", None, &HashMap::new());
        assert_eq!(data.log.len(), 2);
        assert_eq!(data.log[0].proposal_id, Some(7));
        assert_eq!(data.log[1].proposal_id, None);
        // the Accepted table: newest first, the proposal-backed row carries
        // its voters, the legacy row (unknown origin) only its title
        assert_eq!(data.accepted.len(), 2);
        assert_eq!(data.accepted[0].id, -1, "legacy row, no discussion jump");
        assert_eq!(data.accepted[1].id, 7);
        assert_eq!(data.accepted[1].votes.len(), 2, "the block-proven voters");
    }

    #[test]
    fn system_lines_interleave_by_time_and_tolerate_unknown_proposals() {
        let pv = ProposalView {
            id: ProposalId(4),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
            approvals: 2,
            threshold: 3,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(0, 4, &[pv], &HashMap::new(), &first_seen);
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0].0, 150, "stamped with the UI-side first-seen time");
        assert!(sys[0].1.system, "system lines carry the quiet-style flag");
        assert!(sys[0].1.lead.is_empty(), "system lines have no author");
        assert!(sys[0].1.id.is_empty(), "no id → no id-requiring actions");
        let text = &sys[0].1.text;
        assert!(
            text.contains("#4") && text.contains("budget") && text.contains("2/3"),
            "{text}"
        );

        // an unknown/already-materialized proposal renders as a bare
        // handle, never an error (concept Q4)
        let sys_unknown = patch_system_lines(0, 9, &[], &HashMap::new(), &first_seen);
        assert!(sys_unknown[0].1.text.contains("#9"), "{}", sys_unknown[0].1.text);
        assert_eq!(sys_unknown[0].0, 0, "never seen → sorts to the top");

        // merged by time into the chat lines; the chat order itself is
        // never disturbed and a tie puts the system line first
        let chat = vec![
            (100u64, line("me", "a")),
            (200, line("me", "b")),
            (300, line("me", "c")),
        ];
        let system = vec![
            (200u64, system_line_data("s2".into())),
            (150, system_line_data("s1".into())),
        ];
        let merged = merge_by_time(chat, system);
        assert_eq!(
            merged.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "s1", "s2", "b", "c"]
        );
    }

    /// Review finding: the read contract's `pending` is Proposed-only, so
    /// the moment a proposal seals (or closes) it vanishes from every read
    /// and the patch channel degraded to "#id" with no state line. The
    /// UI-side cache must keep the title and resolve the fate from the
    /// applied log the UI already reads.
    #[test]
    fn patch_title_and_state_survive_the_proposal_leaving_pending() {
        let pv = ProposalView {
            id: ProposalId(4),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
            approvals: 2,
            threshold: 3,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let mut known = HashMap::new();
        // while pending: cached with title + progress
        update_known_proposals(&mut known, std::slice::from_ref(&pv), &[], &HashMap::new());
        assert_eq!(display_title(0, &known[&4].payload), "budget", "human title, no op-code prefix");
        assert_eq!(known[&4].fate, KnownFate::Pending);

        // the proposal leaves the Proposed-only window and its payload
        // shows up in the surface's applied log → Applied
        let applied = HashMap::from([(Surface::Memory, vec![pv.payload.clone()])]);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&4].fate, KnownFate::Applied);

        // the system line keeps the title and renders the sealed state
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(0, 4, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("budget") && text.contains('✓'), "{text}");
        assert!(text.contains("3/3"), "sealed at the threshold: {text}");

        // a sealed vote's discussion leaves the sidebar (discussions exist
        // to decide something — once decided there is nothing to vote on)
        let infos = vec![ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(4) },
            count: 1,
            last_ts: 10,
            state: None,
            unread: 0,
        }];
        let rows = derive_channels(0, &infos, &known, &HashMap::new());
        assert!(rows.is_empty(), "an Applied vote's discussion is hidden");

        // vanished WITHOUT an applied trace: the read contract cannot tell
        // Rejected from expired — neutral closed marker, title kept, no
        // fabricated verdict
        let pv9 = ProposalView {
            id: ProposalId(9),
            payload: serde_json::json!({ "title": "drop the fee" }),
            ..pv.clone()
        };
        update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &applied);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&9].fate, KnownFate::Closed);
        let sys = patch_system_lines(0, 9, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("drop the fee") && text.contains('⊘'), "{text}");
        assert!(!text.contains('✓') && !text.contains('✗'), "{text}");

        // an id never seen anywhere still tolerates (concept Q4)
        let sys = patch_system_lines(0, 77, &[], &known, &first_seen);
        assert_eq!(sys[0].1.text, "⚖ #77");

        // a Closed verdict corrects itself when the applied value shows up
        // in a later read (an out-of-order pass must not stick a wrong fate)
        let applied9 = HashMap::from([(
            Surface::Memory,
            vec![serde_json::json!({ "title": "drop the fee" })],
        )]);
        update_known_proposals(&mut known, &[], &[], &applied9);
        assert_eq!(known[&9].fate, KnownFate::Applied);
        // … while an already-Applied fate is sticky even if the surface
        // read is missing this pass
        update_known_proposals(&mut known, &[], &[], &HashMap::new());
        assert_eq!(known[&4].fate, KnownFate::Applied);
        assert_eq!(known[&9].fate, KnownFate::Applied);
    }

    /// One `ProposalView` for the cache tests, minimal noise.
    fn view_of(id: u64, title: &str, state: ProposalState) -> ProposalView {
        ProposalView {
            id: ProposalId(id),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": title }),
            approvals: 0,
            threshold: 3,
            state,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: if state == ProposalState::Rejected { 100 } else { 0 },
            declined_by: if state == ProposalState::Rejected {
                "ashi".to_string()
            } else {
                String::new()
            },
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        }
    }

    /// The snapshots' `declined` lists fold into the proposal cache: a veto
    /// this UI never saw pending (fresh open, another member's decline)
    /// still titles its discussion channel and flags it closed — and an
    /// Applied fate is never downgraded by the fold.
    #[test]
    fn declined_votes_fold_into_the_cache_as_closed() {
        let mut known = HashMap::new();
        // never seen pending: the decline inserts a Closed entry, titled
        let dv7 = view_of(7, "vetoed", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv7), &HashMap::new());
        assert_eq!(known[&7].fate, KnownFate::Closed);
        assert_eq!(display_title(0, &known[&7].payload), "vetoed", "human title from the summary");

        // a cached Pending refreshes to Closed when its decline shows up
        let pv8 = view_of(8, "late veto", ProposalState::Proposed);
        update_known_proposals(&mut known, std::slice::from_ref(&pv8), &[], &HashMap::new());
        assert_eq!(known[&8].fate, KnownFate::Pending);
        let dv8 = view_of(8, "late veto", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv8), &HashMap::new());
        assert_eq!(known[&8].fate, KnownFate::Closed);

        // an Applied fate is sticky against the fold (the applied-log probe
        // proved the seal; byte-identical-twin ambiguity must not un-seal)
        let pv9 = view_of(9, "sealed", ProposalState::Proposed);
        update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &HashMap::new());
        let applied = HashMap::from([(Surface::Memory, vec![pv9.payload.clone()])]);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&9].fate, KnownFate::Applied);
        let dv9 = view_of(9, "sealed", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv9), &applied);
        assert_eq!(known[&9].fate, KnownFate::Applied, "never downgraded");

        // …and the derive_channels contract holds over the folded cache:
        // the closed discussion stays OFF the sidebar
        let infos = vec![ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(7) },
            count: 2,
            last_ts: 20,
            state: Some(ProposalState::Rejected),
            unread: 0,
        }];
        assert!(
            derive_channels(0, &infos, &known, &HashMap::new()).is_empty(),
            "a declined vote's discussion is not a sidebar row"
        );
    }

    /// The decision-panel flag: only an ORGANIZATION decision's discussion.
    ///
    /// The ask is explicit that other surfaces' decisions are handled
    /// differently, so the panel must not appear for them. And it must not
    /// appear for the group chat or a free topic either — there is no
    /// decision to head those with.
    #[test]
    fn selected_channel_org_flags_only_organization_decisions() {
        let known_of = |surface: Surface| KnownProposal {
            payload: serde_json::json!({"op": "set_name", "value": "x"}),
            surface,
            approvals: 1,
            threshold: 2,
            fate: KnownFate::Pending,
        };
        let known = HashMap::from([
            (1u64, known_of(Surface::Organization)),
            (2u64, known_of(Surface::Memory)),
        ]);
        let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };

        assert!(selected_channel_org(&patch(1), &known), "an Organization decision");
        assert!(
            !selected_channel_org(&patch(2), &known),
            "another surface's decision is handled differently - no panel"
        );
        assert!(
            !selected_channel_org(&patch(9), &known),
            "an unknown referent heads nothing"
        );
        assert!(!selected_channel_org(&ChannelRef::Group, &known));
        assert!(!selected_channel_org(
            &ChannelRef::Topic { name: "budget".into() },
            &known
        ));
    }

    /// The compose-collapse flag: only a DECIDED vote's patch channel is
    /// read-only. The engine's enumeration annotation is authoritative when
    /// present; otherwise the proposal cache decides; group/topic, open
    /// votes and unknown referents (Q4) stay writable.
    #[test]
    fn selected_channel_closed_flags_only_decided_patch_votes() {
        let known_of = |fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": "t"}),
            surface: Surface::Memory,
            approvals: 1,
            threshold: 2,
            fate,
        };
        let info = |id: u64, state: Option<ProposalState>| ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(id) },
            count: 1,
            last_ts: 10,
            state,
            unread: 0,
        };
        let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };
        let known = HashMap::from([
            (1u64, known_of(KnownFate::Pending)),
            (2u64, known_of(KnownFate::Closed)),
            (3u64, known_of(KnownFate::Applied)),
        ]);

        // group/topic are never closed
        assert!(!selected_channel_closed(&ChannelRef::Group, &[], &known));
        assert!(!selected_channel_closed(
            &ChannelRef::Topic { name: "x".into() },
            &[],
            &known
        ));

        // the engine annotation decides when present …
        let infos = vec![
            info(1, Some(ProposalState::Proposed)),
            info(2, Some(ProposalState::Rejected)),
            info(3, Some(ProposalState::Applied)),
        ];
        assert!(!selected_channel_closed(&patch(1), &infos, &HashMap::new()));
        assert!(selected_channel_closed(&patch(2), &infos, &HashMap::new()));
        assert!(selected_channel_closed(&patch(3), &infos, &HashMap::new()));
        // … and wins over a stale cache
        let stale = HashMap::from([(2u64, known_of(KnownFate::Pending))]);
        assert!(selected_channel_closed(&patch(2), &infos, &stale));

        // no (or unannotated) enumeration entry → the cache decides — the
        // instant-feedback path on selection passes no infos at all
        assert!(!selected_channel_closed(&patch(1), &[], &known));
        assert!(selected_channel_closed(&patch(2), &[], &known));
        assert!(selected_channel_closed(&patch(3), &[], &known));
        assert!(selected_channel_closed(&patch(2), &[info(2, None)], &known));

        // unknown everywhere stays writable (chat-bus Q4)
        assert!(!selected_channel_closed(&patch(99), &infos, &known));
    }

    /// The epoch invalidates a bundle read for a selection the user has
    /// LEFT — that is the whole job. It used to invalidate on every newer
    /// push start as well, which starved the pane (see
    /// `an_overlapping_push_does_not_starve_the_one_it_overlaps`): a stale
    /// bundle landing is a cosmetic revert one push later, an empty pane is
    /// the user losing their chat.
    #[test]
    fn push_generation_guard_invalidates_stale_pushes() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let g1 = st.begin_push("ws-1").expect("current");
        assert!(st.is_current(g1), "a push for the current selection lands");
        // a selection change invalidates every in-flight push …
        st.select(ChannelRef::Topic {
            name: "budget".into(),
        });
        assert!(!st.is_current(g1));
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".into()
            }
        );
        // … and the counter moves across the workspace-switch reset, so an
        // old push can never match a freshly reset state
        let g2 = st.begin_push("ws-1").expect("current");
        st.enter_workspace("ws-2");
        let g3 = st.begin_push("ws-2").expect("current");
        assert!(g3 > g2, "monotonic across enter_workspace resets");
        assert!(st.is_current(g3));
        assert!(!st.is_current(g2));
    }


    /// A workspace switch must not leak the previous workspace's channel
    /// state into the next one: a stale Patch/Topic selection would filter
    /// the new workspace's log until manually cleared, and the first-seen
    /// stamps would misplace system lines. Same workspace → everything is
    /// kept. (Unread counts live engine-side since B2 and reset with the
    /// workspace there.)
    #[test]
    fn chat_ui_state_resets_on_workspace_switch() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        st.selected = ChannelRef::Topic {
            name: "budget".to_string(),
        };
        st.first_seen.insert(4, 100);

        // the same workspace: selection and stamps survive
        st.enter_workspace("ws-1");
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".to_string()
            }
        );
        assert_eq!(st.first_seen.get(&4), Some(&100));

        // a switch: back to Group, stamps gone, and the new identity sticks
        st.enter_workspace("ws-2");
        assert_eq!(st.selected, ChannelRef::Group);
        assert!(st.first_seen.is_empty());
        st.selected = ChannelRef::Group;
        st.enter_workspace("ws-2");
        assert!(st.first_seen.is_empty(), "no reset without a switch");
    }

    #[test]
    fn channel_keys_round_trip() {
        for c in [
            ChannelRef::Group,
            ChannelRef::Patch { id: ProposalId(42) },
            ChannelRef::Topic { name: "Budget 2026".into() },
        ] {
            assert_eq!(parse_channel_key(&channel_key(&c)), Some(c));
        }
        assert_eq!(parse_channel_key("patch:xyz"), None, "junk never panics");
        assert_eq!(parse_channel_key(""), None);
    }

    #[test]
    fn charter_splits_into_balanced_columns_at_word_boundaries() {
        // a short charter stays single-column
        assert_eq!(
            charter_columns("kurz und knapp", 3),
            vec!["kurz und knapp".to_string()]
        );
        // empty → no columns (the UI shows its no-agenda line)
        assert!(charter_columns("   ", 3).is_empty());
        // ~450 chars → 2 columns; nothing lost, split at word boundaries
        let mid = "wort ".repeat(90);
        let cols = charter_columns(&mid, 3);
        assert_eq!(cols.len(), 2);
        assert!(
            cols.join(" ")
                .split_whitespace()
                .eq(mid.split_whitespace()),
            "columns are a display split - every word survives"
        );
        // a long charter caps at the column maximum
        let long = "wort ".repeat(300);
        assert_eq!(charter_columns(&long, 3).len(), 3);
        // umlauts near the cut never split a character
        let umlaut = "ä".repeat(400);
        let cols = charter_columns(&umlaut, 3);
        assert_eq!(cols.concat(), umlaut);
    }

    #[test]
    fn expires_labels_render_the_retention_deadline() {
        assert_eq!(expires_label(0, 100, 100 + 13 * 86_400, true), "in 13 days");
        assert_eq!(expires_label(0, 100, 100 + 86_400, true), "in 1 day");
        assert_eq!(expires_label(0, 100, 100 + 7_200, true), "in 2 h");
        assert_eq!(expires_label(0, 100, 100 + 120, true), "in 2 min");
        assert_eq!(expires_label(0, 500, 100, true), "expired");
        assert_eq!(
            expires_label(0, 100, 0, true),
            "-",
            "0 = unknown share age, no deadline (the engine keeps it forever)"
        );
        assert_eq!(
            expires_label(0, 100, 100 + 86_400, false),
            "-",
            "an unavailable share has nothing left to expire"
        );
        // the cell renders in the active language, like the tables around it
        assert_eq!(expires_label(1, 100, 100 + 13 * 86_400, true), "in 13 Tagen");
        assert_eq!(expires_label(1, 100, 100 + 86_400, true), "in 1 Tag");
        assert_eq!(expires_label(1, 500, 100, true), "abgelaufen");
    }

    #[test]
    fn quote_indent_groups_by_target_and_alternates_between_neighbors() {
        let mut log = vec![
            line("a", "question 1"),
            line("b", "reply 1"),
            line("c", "reply 2"),
            line("d", "reply to something else"),
            line("e", "plain"),
            line("f", "late reply"),
        ];
        log[1].quote_id = hex_id(1);
        log[2].quote_id = hex_id(1);
        log[3].quote_id = hex_id(2);
        log[5].quote_id = hex_id(3);
        let quotes = HashMap::from([
            (hex_id(1), qsrc("a", "question 1", false)),
            (hex_id(2), qsrc("x", "question 2", false)),
            (hex_id(3), qsrc("y", "question 3", false)),
        ]);
        annotate_chat_log(&mut log, &quotes);
        assert_eq!(log[0].quote_indent, 0, "no quote, no indent");
        assert_eq!(log[1].quote_indent, 1, "a fresh reply group starts at depth 1");
        assert_eq!(log[2].quote_indent, 1, "same target keeps the depth");
        assert_eq!(log[3].quote_indent, 2, "a neighboring different target alternates");
        assert_eq!(log[4].quote_indent, 0, "plain rows sit flush and end the run");
        assert_eq!(log[5].quote_indent, 1, "after a break the next group restarts at 1");
    }

    // ---- the chat pane's push epoch -----------------------------------

    /// **Two overlapping pushes must BOTH be able to land.**
    ///
    /// `push_surfaces` issues `MarkChannelRead` whenever the channel on
    /// screen has unread messages; the engine event that causes starts the
    /// next push while the current one is still reading. While `begin_push`
    /// bumped the epoch, that made the reading push stale and it threw its
    /// finished bundle away — so opening a chat with anything unread left
    /// the pane EMPTY until some later burst happened to leave one push
    /// unoverlapped. That is the bug this pins, and it is invisible to any
    /// test that pushes one at a time.
    #[test]
    fn an_overlapping_push_does_not_starve_the_one_it_overlaps() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let a = st.begin_push("ws-1").expect("the active workspace");
        let b = st.begin_push("ws-1").expect("the MarkChannelRead echo");
        assert!(st.is_current(b), "the newer push lands");
        assert!(
            st.is_current(a),
            "…and so does the one it overlapped: both read the same selection, \
             so dropping either renders nothing at all"
        );
    }

    /// **THE first-open bug, from the user's own log.**
    ///
    /// ```text
    /// ui: workspace switch from= to=752… gen=2
    /// ui: bundle gathered ws=752… gen=2 channel=group chat_rows=9
    /// ui: bundle DROPPED as stale gen=2
    /// ```
    ///
    /// The bundle was RIGHT — nine rows — and was thrown away 38 ms later
    /// because the epoch had moved. What moved it was the session mirror
    /// refreshing the CREATE WIZARD's relay picker: opening a workspace
    /// changes the dialable pool, `set_create_relays` bumped, and the
    /// surfaces bundle in flight died of it. Only on the first open,
    /// because the pool only changes once — which is exactly the reported
    /// symptom.
    ///
    /// The epoch is the SELECTION epoch. It exists so a bundle read for a
    /// channel or workspace the user has left cannot land. A relay picker
    /// the bundle does not even carry must not be able to invalidate it.
    #[test]
    fn unrelated_ui_state_cannot_stale_a_surfaces_bundle() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let in_flight = st.begin_push("ws-1").expect("current");

        // the session mirror refreshes the create wizard's relay picker —
        // which the surfaces bundle does not carry at all
        st.set_create_relays(vec!["wss://relay.example".to_string()]);
        assert!(
            st.is_current(in_flight),
            "the relay picker is not part of the bundle - it must not stale it"
        );

        // …and the things the bundle DOES carry still do
        let in_flight = st.begin_push("ws-1").expect("current");
        st.sort_members_by("name");
        assert!(
            !st.is_current(in_flight),
            "the members order IS in the bundle - a stale one would revert it"
        );
    }

    /// **A push reading for a workspace that is no longer open must not
    /// land, and must not drag the state back to it.**
    ///
    /// This is the empty chat on a first open. The workspace switch used to
    /// ride `begin_push`, keyed on whatever session copy that push had read
    /// — so a push that read the session BEFORE the open re-entered the
    /// state as "no workspace" AFTER it, bumped the epoch past the good
    /// push (whose bundle was then discarded) and landed its own empty one.
    /// Switching surfaces forced a fresh push, which is why it looked like
    /// the chat needed a nudge.
    #[test]
    fn a_push_that_read_the_session_before_an_open_cannot_land_after_it() {
        let mut st = ChatUiState::default();
        // …a push that read the session while nothing was open
        let stale = st.begin_push("").expect("nothing open is a state too");
        // …then the open lands, through the SESSION mirror
        st.enter_workspace("ws-1");
        let fresh = st.begin_push("ws-1").expect("the open workspace");

        assert!(st.is_current(fresh), "the push that read the open workspace lands");
        assert!(!st.is_current(stale), "…and the one from before it does not");
        // the decisive part: the stale push cannot re-enter the old state
        assert_eq!(
            st.begin_push(""),
            None,
            "a push for a workspace that is not open renders nothing at all"
        );
        assert_eq!(st.workspace, "ws-1", "…and it did not drag the state back");
    }

    /// The epoch exists for ONE thing: a bundle read for a selection the
    /// user has left must never land on the one they are looking at (it
    /// would also mark the wrong channel read).
    #[test]
    fn a_push_read_for_another_selection_never_lands() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let in_flight = st.begin_push("ws-1").expect("current");
        st.select(ChannelRef::Topic { name: "budget".into() });
        assert!(
            !st.is_current(in_flight),
            "a bundle read for the previous channel must not land"
        );
        // …and a workspace switch is the same rule one level up
        let in_flight = st.begin_push("ws-1").expect("current");
        st.enter_workspace("ws-2");
        assert!(
            !st.is_current(in_flight),
            "a bundle read against another workspace's log must not land"
        );
    }

    // ---- the Restore wizard's one link field (welcome_rework.md) -------

    /// The two link shapes are rendered by the ENGINE's own `render()`,
    /// never hand-written here: a hand-built string pins the test's idea of
    /// the format, and the day the real one changes the test keeps passing
    /// while the panel stops recognizing anything.
    /// A real x-only anchor - the handover encoders validate the key, so a
    /// made-up hex string cannot stand in for one.
    fn anchor(seed: u8) -> String {
        molt_net::nostr_identity(&[seed; 32], "fixture").1
    }

    fn invite_link() -> String {
        molt_engine::FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "Chess Club".to_string(),
                threshold: 2,
                members: 3,
                inviter: "walter".to_string(),
                ticket: "a".repeat(64),
            },
            handover: molt_net::invite::InviteHandoverV2 {
                seat: 1,
                ticket: "a".repeat(64),
                npub: anchor(1),
                relays: vec!["ws://127.0.0.1:7777".to_string()],
            },
        }
        .render()
        .expect("the engine renders its own link")
    }

    fn recovery_link() -> String {
        molt_engine::RecoveryInvite {
            republic: "Chess Club".to_string(),
            member: "petra".to_string(),
            ticket: "c".repeat(64),
            server: String::new(),
            queue_id: String::new(),
            wrap: String::new(),
            republic_id: "d".repeat(64),
            handover: Some(molt_net::invite::RecoveryHandoverV2 {
                identity_pk: String::new(),
                ticket: "c".repeat(64),
                npub: anchor(2),
                relays: vec!["ws://127.0.0.1:7777".to_string()],
                republic_id: "d".repeat(64),
            }),
        }
        .render()
    }

    /// One field, two flows: an invite link asks for a NAME and joins, a
    /// recovery link brings its own seat and needs the PHRASE. Getting this
    /// wrong sends someone through the founding ritual to recover a seat
    /// they already hold, so it is pinned rather than eyeballed.
    #[test]
    fn one_link_field_tells_a_join_from_a_recovery() {
        assert_eq!(
            link_kind(&invite_link()),
            LinkKind::Invite {
                republic: "Chess Club".to_string(),
                inviter: "walter".to_string(),
            },
            "a founding invite routes to the join"
        );
        assert_eq!(
            link_kind(&recovery_link()),
            LinkKind::Recovery {
                republic: "Chess Club".to_string(),
                member: "petra".to_string(),
            },
            "a recovery link routes to the ritual, and names its own seat"
        );
        // whitespace is what a paste actually carries
        assert_eq!(link_kind(&format!("  {}\n", invite_link())), link_kind(&invite_link()));
    }

    /// Everything else arms nothing. A PREVIEW-only invite link is the
    /// interesting case: it parses as a human-readable invite and carries no
    /// transport handover at all, so a panel that armed on "looks like an
    /// invite" would start a join that cannot reach anybody.
    #[test]
    fn a_link_that_cannot_act_arms_nothing() {
        let full = invite_link();
        let preview = full.rsplit_once('/').expect("the handover is the last segment").0;
        assert_eq!(
            link_kind(preview),
            LinkKind::Unrecognized,
            "a preview link has no transport handover - nothing can be done with it"
        );
        let damaged = format!("{}zz", recovery_link());
        assert_eq!(
            link_kind(&damaged),
            LinkKind::Unrecognized,
            "a damaged recovery handover is not an actionable link"
        );
        for junk in ["", "   ", "hello", "molt://", "molt://invite/", "https://example.com"] {
            assert_eq!(link_kind(junk), LinkKind::Unrecognized, "junk: {junk:?}");
        }
    }

    /// The chat offers exactly ONE view, and it is writable. The nav used
    /// to carry two more: an Archive (the older half of the retention
    /// window - an invisible cliff a conversation fell over at 3.5 days)
    /// and the agent-facing "unread" slice, which broke the pane outright:
    /// the GUI marks the on-screen channel read on every refresh, so it
    /// emptied itself on sight, and the compose row is gated on the general
    /// view, so there was nothing to write into either.
    #[test]
    fn the_chat_offers_one_writable_view() {
        assert_eq!(
            Surface::Chat.views().iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["today"],
            "a second chat view is a place a user can get stranded in"
        );
        assert_eq!(Surface::Chat.default_view(), "today");
        // …and the read slice stays available to an agent, off the nav
        assert!(molt_core::CHAT_READ_SLICES.contains(&"unread"));
    }

    #[test]
    fn when_label_relative_part() {
        let ts = 1_750_000_000_u64;
        let at = |offset: i64| when_label_at(0, ts, 1_750_000_000 + offset);
        assert!(at(5).ends_with("(just now)"), "{}", at(5));
        assert!(at(60).ends_with("(~1 minute ago)"), "{}", at(60));
        assert!(at(20 * 60).ends_with("(~20 minutes ago)"), "{}", at(1200));
        assert!(at(2 * 3600).ends_with("(~2 hours ago)"), "{}", at(7200));
        assert!(at(3 * 86_400).ends_with("(~3 days ago)"), "{}", at(259_200));
    }

    /// The presence cell reads a REAL stamp: fresh sightings stay relative,
    /// and past a week the DATE takes over - "34 d ago" is arithmetic the
    /// reader should not have to do. Only a seat this install has never had
    /// any evidence for says so.
    #[test]
    fn the_last_seen_cell_goes_from_relative_to_a_plain_date() {
        let now = 1_787_000_000_u64;
        assert_eq!(seen_label(0, now, molt_core::MemberInfo::NEVER, "never seen"), "never seen");
        assert_eq!(seen_label(0, now, now, ""), "just now");
        assert_eq!(seen_label(0, now, now - 3 * 3600, ""), "3 h ago");
        assert_eq!(seen_label(1, now, now - 2 * 86_400, ""), "vor 2 Tagen");
        // the week boundary: one side relative, the other the date itself
        assert_eq!(seen_label(0, now, now - 6 * 86_400, ""), "6 d ago");
        let old = now - 30 * 86_400;
        assert_eq!(seen_label(0, now, old, ""), date_label(0, old));
        assert_eq!(seen_label(1, now, old, ""), date_label(1, old));
        // the two spellings, pinned against the same instant
        let iso = date_label(0, old);
        let de = date_label(1, old);
        assert_eq!(iso.len(), 10, "ISO date: {iso}");
        assert_eq!(de.len(), 10, "German date: {de}");
        assert_eq!(
            de,
            format!("{}.{}.{}", &iso[8..10], &iso[5..7], &iso[0..4]),
            "the German date is the same day, written the German way"
        );
    }

    #[test]
    fn sync_status_label_matches_the_demo_prose() {
        assert_eq!(sync_status_label(0, 0, 0, 0), "Synced · just now");
        assert_eq!(sync_status_label(0, 0, 2, 0), "Synced · 2 min ago");
        assert_eq!(sync_status_label(0, 0, 60, 0), "Synced · 1 h ago");
        assert_eq!(sync_status_label(0, 1, 0, 80), "Syncing… 80 items left");
        assert_eq!(sync_status_label(0, 2, 4320, 0), "Offline · last sync 3 d ago");
    }

    #[test]
    fn nav_labels_speak_german() {
        assert_eq!(surface_name(1, Surface::Organization), "Organisation");
        assert_eq!(surface_name(0, Surface::Organization), "Organization");
        assert_eq!(view_label(1, "members", "Members"), "Mitglieder");
        assert_eq!(view_label(1, "archive", "Archive"), "Archiv");
        assert_eq!(view_label(1, "pending", "Pending"), "Ausstehend");
        // the Kanban views (kanban_workflows.md §6.0): "plan" is new,
        // "my-quests" keeps its wire key under the "Mine" label
        assert_eq!(view_label(1, "plan", "Planning"), "Planung");
        assert_eq!(view_label(0, "plan", "Planning"), "Planning");
        assert_eq!(view_label(1, "my-quests", "Mine"), "Meine");
        // unmapped keys fall back to the shared English vocabulary
        assert_eq!(view_label(1, "status", "Status"), "Status");
        assert_eq!(view_label(0, "members", "Members"), "Members");
    }

    #[test]
    fn sync_status_label_speaks_german() {
        assert_eq!(sync_status_label(1, 0, 0, 0), "Synchronisiert · gerade eben");
        assert_eq!(sync_status_label(1, 0, 2, 0), "Synchronisiert · vor 2 Min.");
        assert_eq!(sync_status_label(1, 0, 60, 0), "Synchronisiert · vor 1 Std.");
        assert_eq!(sync_status_label(1, 1, 0, 80), "Synchronisiere… 80 ausstehend");
        assert_eq!(
            sync_status_label(1, 2, 4320, 0),
            "Offline · letzter Sync vor 3 Tagen"
        );
        assert_eq!(sync_status_label(1, 0, 1440, 0), "Synchronisiert · vor 1 Tag");
    }

    fn ws(name: &str, minutes: i32) -> WorkspaceItem {
        WorkspaceItem {
            id: molt_core::demo_workspace_id(name).into(),
            name: name.into(),
            detail: "".into(),
            status: "".into(),
            synced: true,
            state: 0,
            last_sync_min: minutes,
            s3: false,
            backup: "".into(),
            encrypted: false,
            seed: "".into(),
            net: "".into(),
            members: ModelRc::new(VecModel::from(Vec::new())),
        }
    }

    #[test]
    fn size_and_backup_labels() {
        assert_eq!(size_label(920), "920 KiB");
        assert_eq!(size_label(1840), "1.8 MiB");
        assert_eq!(backup_when_label(0, molt_core::WorkspaceInfo::NEVER), "never");
        assert_eq!(backup_when_label(0, 0), "just now");
        assert_eq!(backup_when_label(0, 30), "30 min ago");
        assert_eq!(backup_when_label(0, 129_600), "90 d ago");
        assert_eq!(backup_when_label(1, molt_core::WorkspaceInfo::NEVER), "nie");
        assert_eq!(backup_when_label(1, 0), "gerade eben");
        assert_eq!(backup_when_label(1, 30), "vor 30 Min.");
        assert_eq!(backup_when_label(1, 129_600), "vor 90 Tagen");
    }

    /// A session with bucket-only entries, as a real listing would produce
    /// them: one true orphan (id only, no name) and one foreign key. The
    /// production DEFAULT has none — molt-core pins that.
    fn sv_with_orphans() -> SessionView {
        SessionView {
            backup_orphans: vec![
                molt_core::BackupOrphan {
                    id: "ab".repeat(32),
                    name: String::new(),
                    size_kib: 480,
                    last_backup_min: 129_600,
                },
                molt_core::BackupOrphan {
                    id: String::new(),
                    name: "molt/leftover.bin".to_string(),
                    size_kib: 75,
                    last_backup_min: 43_200,
                },
            ],
            // the demo republics are a fixture, not the default (review K6)
            workspaces: molt_core::WorkspaceInfo::demo_set(),
            ..SessionView::default()
        }
    }

    #[test]
    fn sort_bk_rows_by_size_and_names_with_empties_last() {
        let sv = sv_with_orphans();
        let mut rows = backup_rows(&sv);
        sort_bk_rows(&mut rows, "size", false);
        let sizes: Vec<i32> = rows.iter().map(|r| r.size_kib).collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{sizes:?}");
        sort_bk_rows(&mut rows, "local", false);
        assert!(
            rows.last().expect("rows").local.is_empty(),
            "orphans sort last on the local column"
        );
        sort_bk_rows(&mut rows, "last", false);
        assert_eq!(
            rows.last().expect("rows").last.as_str(),
            "never",
            "never-backed-up rows sort last"
        );
    }

    #[test]
    fn backup_rows_map_locals_then_orphans() {
        let sv = sv_with_orphans();
        let rows = backup_rows(&sv);
        assert_eq!(rows.len(), sv.workspaces.len() + sv.backup_orphans.len());
        // locals first: name on the left, bucket side only when auto is on
        for (row, w) in rows.iter().zip(&sv.workspaces) {
            assert!(row.has_local);
            assert_eq!(row.local.as_str(), w.name);
            assert_eq!(row.auto, w.s3);
            // the bucket cell claims nothing the bucket didn't confirm: a
            // real backup error, else really listed copies, else empty —
            // never derived from the auto toggle alone (story 12 honesty)
            if w.backup_error.is_empty() && w.backup_copies == 0 {
                assert!(row.remote.is_empty());
            } else {
                assert!(!row.remote.is_empty());
            }
        }
        // orphans last: bucket side only, no toggle. A true orphan shows
        // its shortened workspace-id pseudonym (no name exists in the
        // bucket — never invent one); a foreign key shows its raw key.
        let orphans = &rows[sv.workspaces.len()..];
        for row in orphans {
            assert!(!row.has_local);
            assert_eq!(row.local.as_str(), "");
            assert!(!row.auto);
        }
        assert_eq!(orphans[0].remote.as_str(), "abababababab…");
        // the row keeps the FULL pseudonym (restore starts from it)
        assert_eq!(orphans[0].id.as_str(), "ab".repeat(32));
        assert_eq!(orphans[1].remote.as_str(), "molt/leftover.bin");
        assert_eq!(orphans[1].id.as_str(), "", "a foreign key has no workspace id");
    }

    /// The production default renders a table with ONLY the local rows —
    /// no invented bucket entries (story 8's regression fence, UI side).
    #[test]
    fn backup_rows_default_has_no_bucket_only_rows() {
        let sv = SessionView::default();
        let rows = backup_rows(&sv);
        assert_eq!(rows.len(), sv.workspaces.len());
        assert!(rows.iter().all(|r| r.has_local));
    }

    #[test]
    fn sort_ws_items_by_name_and_recency() {
        let mut items = vec![ws("beta", 60), ws("Alpha", 5), ws("gamma", 0)];
        sort_ws_items(&mut items, "name", false);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"], "case-insensitive");
        sort_ws_items(&mut items, "sync", false);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["gamma", "Alpha", "beta"], "most recent first");
        sort_ws_items(&mut items, "sync", true);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["beta", "Alpha", "gamma"]);
    }

    /// Uploads-table row for the presentation tests. The DISPLAY strings
    /// are deliberately misleading (they would sort the other way round),
    /// pinning that date/size/expiry sort by the underlying numeric keys
    /// and never the rendered labels.
    fn upload(user: &str, name: &str, checksum: &str, ts: u64, bytes: u64) -> UploadRowData {
        UploadRowData {
            id: String::new(),
            user: user.to_string(),
            date: format!("{}", u64::MAX - ts),
            name: name.to_string(),
            kind: String::new(),
            size: format!("{} KiB", u64::MAX - bytes),
            available: true,
            online: true,
            // the cell shows a shortened prefix — the filter must still
            // match on the full value
            checksum: checksum.get(..4).unwrap_or(checksum).to_string(),
            expires: String::new(),
            status: String::new(),
            status_kind: 0,
            availability: String::new(),
            ts,
            bytes,
            expires_ts: ts,
            checksum_full: checksum.to_string(),
        }
    }

    #[test]
    fn sort_uploads_text_columns_case_insensitive() {
        let mut rows = vec![
            upload("bob", "zeta.pdf", "CC99", 1, 1),
            upload("Alice", "Alpha.PDF", "0b11", 2, 2),
            upload("carol", "beta.txt", "aa22", 3, 3),
        ];
        rows[0].kind = "PDF".to_string();
        rows[1].kind = "zip".to_string();
        rows[2].kind = "Txt".to_string();
        rows[0].status = "\u{2713}".to_string();
        rows[1].status = "42 %".to_string();
        let users = |rows: &[UploadRowData]| -> Vec<String> {
            rows.iter().map(|r| r.user.clone()).collect()
        };
        sort_uploads(&mut rows, "user", true);
        assert_eq!(users(&rows), ["Alice", "bob", "carol"], "case-insensitive");
        sort_uploads(&mut rows, "user", false);
        assert_eq!(users(&rows), ["carol", "bob", "Alice"], "descending flips");
        sort_uploads(&mut rows, "file", true);
        assert_eq!(users(&rows), ["Alice", "carol", "bob"], "Alpha < beta < zeta");
        sort_uploads(&mut rows, "type", true);
        assert_eq!(users(&rows), ["bob", "carol", "Alice"], "pdf < txt < zip");
        sort_uploads(&mut rows, "checksum", true);
        assert_eq!(users(&rows), ["Alice", "carol", "bob"], "0b < aa < cc");
        sort_uploads(&mut rows, "download", true);
        assert_eq!(users(&rows), ["carol", "Alice", "bob"], "idle < 42 % < ✓");
    }

    #[test]
    fn sort_uploads_numeric_columns_use_underlying_values() {
        // the rendered date/size labels would sort exactly the other way
        // round (see `upload`) — only the numeric keys give this order
        let mut rows = vec![
            upload("a", "x", "", 30, 10_240),
            upload("b", "y", "", 10, 2_048),
            upload("c", "z", "", 20, 900),
        ];
        let users = |rows: &[UploadRowData]| -> Vec<String> {
            rows.iter().map(|r| r.user.clone()).collect()
        };
        sort_uploads(&mut rows, "date", true);
        assert_eq!(users(&rows), ["b", "c", "a"], "oldest share first");
        sort_uploads(&mut rows, "date", false);
        assert_eq!(users(&rows), ["a", "c", "b"], "newest share first");
        sort_uploads(&mut rows, "size", true);
        assert_eq!(users(&rows), ["c", "b", "a"], "900 B < 2 KiB < 10 KiB");
        sort_uploads(&mut rows, "expires", true);
        assert_eq!(users(&rows), ["b", "c", "a"], "soonest expiry first");
        // an unknown/empty column keeps the current order
        sort_uploads(&mut rows, "", false);
        assert_eq!(users(&rows), ["b", "c", "a"]);
    }

    #[test]
    fn filter_uploads_matches_user_name_or_checksum_case_insensitively() {
        let all = || {
            vec![
                upload("Alice", "report.pdf", "aabb1122", 1, 1),
                upload("bob", "photo.png", "ccdd3344", 2, 2),
            ]
        };
        assert_eq!(filter_uploads(all(), "").len(), 2, "empty needle = all");
        let f = filter_uploads(all(), "LICE");
        assert_eq!(f.len(), 1, "user match, case-insensitive");
        assert_eq!(f[0].user, "Alice");
        let f = filter_uploads(all(), "PHOTO");
        assert_eq!(f.len(), 1, "filename match");
        assert_eq!(f[0].user, "bob");
        // beyond the 4-char display prefix — must match the FULL checksum
        let f = filter_uploads(all(), "DD33");
        assert_eq!(f.len(), 1, "full-checksum match");
        assert_eq!(f[0].user, "bob");
        assert!(filter_uploads(all(), "zzz").is_empty(), "no match = empty");
    }

    /// Members-table row for the sort tests.
    fn member(name: &str, id: &str, last_ts: u64, state: i32, uploads: i32) -> MemberRowData {
        MemberRowData {
            name: name.to_string(),
            id: id.to_string(),
            pk: id.to_string(),
            last: String::new(),
            last_ts,
            state,
            uploads,
            split: String::new(),
            image: String::new(),
            image_key: String::new(),
            desc: String::new(),
        }
    }

    #[test]
    fn sort_members_by_name_uploads_and_presence() {
        let mut rows = vec![
            member("bob", "0b", 10_000, 0, 3),
            member("Alice", "aa", 9_700, 1, 10),
            member("carol", "", 0, 2, 2),
        ];
        let names = |rows: &[MemberRowData]| -> Vec<String> {
            rows.iter().map(|r| r.name.clone()).collect()
        };
        sort_members(&mut rows, "name", true);
        assert_eq!(names(&rows), ["Alice", "bob", "carol"], "case-insensitive");
        sort_members(&mut rows, "uploads", true);
        assert_eq!(names(&rows), ["carol", "bob", "Alice"], "2 < 3 < 10 numeric");
        sort_members(&mut rows, "uploads", false);
        assert_eq!(names(&rows), ["Alice", "bob", "carol"]);
        // "last" is the REAL stamp: most recent first, never-seen (0) at
        // the end — regardless of pill state
        sort_members(&mut rows, "last", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"]);
        // unanchored (empty) identity cells sort last ascending
        sort_members(&mut rows, "id", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"], "0b < aa < empty");
        sort_members(&mut rows, "", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"], "unknown = keep");
    }

    /// The Organization tables' view state: clicking the active column
    /// flips the direction, a new column starts ascending, and every
    /// change bumps the push generation (stales in-flight bundles).
    #[test]
    fn org_sort_state_toggles_and_bumps_generation() {
        let mut st = ChatUiState::default();
        let g = st.generation;
        st.sort_uploads_by("size");
        assert_eq!(st.uploads_sort, "size");
        assert!(st.uploads_asc, "a fresh column starts ascending");
        st.sort_uploads_by("size");
        assert!(!st.uploads_asc, "the same column flips the direction");
        st.sort_uploads_by("user");
        assert_eq!(st.uploads_sort, "user");
        assert!(st.uploads_asc, "switching columns resets to ascending");
        st.sort_members_by("uploads");
        assert_eq!(st.members_sort, "uploads");
        assert!(st.members_asc);
        st.set_uploads_filter("alice".to_string());
        assert_eq!(st.uploads_filter, "alice");
        assert_eq!(st.generation, g + 5, "every change stales in-flight pushes");
    }

    /// The pure paging window behind the proposal-outcome lists
    /// (Declined / the applied log): 20 rows per page, the page clamps
    /// into range (a shrunk list must never show an empty page), and a
    /// list of at most one page reports `page_count == 1` — the pager
    /// row hides on that.
    #[test]
    fn page_slice_windows_and_clamps() {
        // empty list: one (empty) page, never a panic range
        assert_eq!(page_slice(0, 0, 20), (0, 0, 0, 1));
        // exactly one page: untouched
        assert_eq!(page_slice(20, 0, 20), (0, 20, 0, 1));
        // one entry over: a second page holding the remainder
        assert_eq!(page_slice(21, 0, 20), (0, 20, 0, 2));
        assert_eq!(page_slice(21, 1, 20), (20, 21, 1, 2));
        // an out-of-range page clamps to the last one (the list shrank)
        assert_eq!(page_slice(21, 9, 20), (20, 21, 1, 2));
        // a full second page ends at the list end
        assert_eq!(page_slice(40, 1, 20), (20, 40, 1, 2));
        assert_eq!(page_slice(61, 3, 20), (60, 61, 3, 4));
    }

    /// The pager's UI-local state (ChatUiState, like the table sorts):
    /// prev/next step per (surface, list) independently, below-zero
    /// clamps at the first page, the push-time clamp re-bases a stored
    /// page against the list's current length (and writes it back, so
    /// the next step moves from the visible page), every step bumps the
    /// push generation, and a workspace switch resets everything.
    #[test]
    fn list_page_state_steps_clamps_and_resets() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-a");
        let g = st.generation;
        st.page_list_by("organization", "declined", 1);
        st.page_list_by("organization", "declined", 1);
        assert_eq!(st.clamp_list_page("organization", "declined", 100), 2);
        assert_eq!(st.generation, g + 2, "every step stales in-flight pushes");
        // stepping below the first page clamps at zero
        st.page_list_by("organization", "declined", -9);
        assert_eq!(st.clamp_list_page("organization", "declined", 100), 0);
        // the clamp writes back: page 3 on a 2-page list re-bases to the
        // last page, and the next "prev" moves from THERE
        st.page_list_by("organization", "declined", 3);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 1);
        st.page_list_by("organization", "declined", -1);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
        // per-(surface, list) independence
        st.page_list_by("memory", "applied", 1);
        assert_eq!(st.clamp_list_page("memory", "applied", 100), 1);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
        // a workspace switch resets the pages with the rest of the state
        st.enter_workspace("ws-b");
        assert_eq!(st.clamp_list_page("memory", "applied", 100), 0);
    }

    /// Guard: every nav sub-view of every surface has a real icon — the
    /// "▪️" fallback showing up in the sidebar means someone added a view
    /// without extending `view_icon`.
    #[test]
    fn every_view_has_an_icon() {
        for surface in Surface::ALL {
            for (key, _) in surface.views() {
                assert_ne!(view_icon(key), "▪️", "view `{key}` has no icon");
            }
        }
    }

    /// The tor-mode dropdown greys "embedded" unless the binary was built with
    /// the `embedded-tor` feature (P3). local + whonix are always selectable;
    /// only the middle (embedded) row tracks the compile-time truth passed
    /// through the app→ui seam.
    #[test]
    fn embedded_row_is_disabled_when_feature_off() {
        // model is ["local", "embedded", "whonix"]
        assert_eq!(tor_mode_enabled(false), [true, false, true]);
        assert_eq!(tor_mode_enabled(true), [true, true, true]);
    }

    /// The header "chat" pill mirrors transport health (P6): Ok → good/green
    /// with no tooltip; Degraded → warn/amber; Down → bad/red — the latter two
    /// carrying the engine's reason string as the hover tooltip.
    #[test]
    fn net_health_maps_to_pill_tone() {
        use molt_core::NetHealth;
        // tone index: 0 = good (green), 1 = warn (amber), 2 = bad (red)
        assert_eq!(net_health_pill(&NetHealth::Ok), (0, String::new()));
        assert_eq!(
            net_health_pill(&NetHealth::Degraded {
                reason: "Tor circuit timed out".to_string(),
            }),
            (1, "Tor circuit timed out".to_string()),
        );
        assert_eq!(
            net_health_pill(&NetHealth::Down {
                reason: "embedded Tor not built into this binary".to_string(),
            }),
            (2, "embedded Tor not built into this binary".to_string()),
        );
    }

    /// The workspace-folder browse dialog starts where the hand-editable
    /// draft points ONLY when that (after the engine's own `~` expansion —
    /// the config default is "~/…") is a real directory; anything else
    /// (empty draft, typo, a file) must yield no start dir so the dialog
    /// opens at its platform default instead of failing.
    #[test]
    fn ws_dir_browse_starts_at_the_draft_only_when_it_is_a_real_directory() {
        let dir = tempfile::tempdir().expect("create a temp directory");
        let dir_path = dir.path().display().to_string();
        assert_eq!(
            browse_start_dir(&dir_path),
            Some(dir.path().to_path_buf()),
            "an existing directory is a usable start dir"
        );
        // a "~" draft expands against $HOME exactly like the engine resolves
        // the setting — pinning the config default's "~/…" form to a REAL
        // start dir, not a literal "~" path that never exists
        let home = std::env::var_os("HOME").expect("HOME is set in the test env");
        assert_eq!(
            browse_start_dir("~"),
            Some(std::path::PathBuf::from(home)),
            "a tilde draft starts at the expanded home directory"
        );
        // a FILE is not a directory to start browsing in
        let file_path = dir.path().join("config.toml");
        std::fs::write(&file_path, b"x").expect("write a probe file");
        assert_eq!(browse_start_dir(&file_path.display().to_string()), None);
        assert_eq!(browse_start_dir(""), None, "empty draft → dialog default");
        assert_eq!(
            browse_start_dir(&format!("{dir_path}/definitely-missing")),
            None,
            "a stale/typoed draft → dialog default"
        );
    }
    /// The relay panel renders the ENGINE's verdict, never its own: every
    /// `blocked` reason becomes exactly one row state, and the position /
    /// end-of-list flags follow the pool order (which IS the priority).
    #[test]
    fn relay_rows_mirror_the_engine_verdict_and_the_priority_order() {
        let status = |url: &str, kind, confirmed, blocked| RelayStatus {
            url: url.to_string(),
            kind,
            confirmed,
            blocked,
        };
        let rows = relay_rows(&[
            // in use: a confirmed onion relay dials by itself
            status("wss://aaa.onion", RelayKind::Onion, true, None),
            // in the pool, but the user has not confirmed it
            status(
                "wss://relay.example.org",
                RelayKind::Clearnet,
                false,
                Some(RelayBlock::Unconfirmed),
            ),
            // confirmed local (LAN self-host), but this session has not
            // activated it — same gate as clearnet, own badge (kind 2)
            status(
                "ws://192.168.1.5:7777",
                RelayKind::Local,
                true,
                Some(RelayBlock::ClearnetSessionLocked),
            ),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pos, 1);
        assert_eq!(rows[0].kind, 0, "onion badge");
        assert!(rows[0].confirmed);
        assert_eq!(rows[0].blocked, 0, "no block = in use right now");
        assert!(rows[0].first, "position 0 cannot move up");
        assert!(!rows[0].last);
        assert_eq!(rows[1].pos, 2);
        assert_eq!(rows[1].kind, 1, "clearnet badge");
        assert_eq!(rows[1].blocked, 1, "unconfirmed");
        assert!(!rows[1].first && !rows[1].last, "the middle row moves both ways");
        assert_eq!(rows[2].pos, 3);
        assert_eq!(rows[2].kind, 2, "local badge - never presented as clearnet");
        assert_eq!(rows[2].blocked, 2, "outside Tor, not activated this session");
        assert!(rows[2].confirmed, "…yet confirmed: the two are independent");
        assert!(rows[2].last, "the bottom row cannot move down");
        // a single relay is BOTH ends — neither arrow may promise a move
        let one = relay_rows(&[status("wss://aaa.onion", RelayKind::Onion, false, Some(RelayBlock::Unconfirmed))]);
        assert!(one[0].first && one[0].last);
        assert!(relay_rows(&[]).is_empty(), "a fresh install shows no rows");
    }

    /// Every way the pool refuses a URL reaches the user as a readable line
    /// under the field — in their language, never as a silent no-op. The
    /// classification comes from molt-core's own parser, so the message and
    /// the engine's gate can never drift apart.
    #[test]
    fn a_refused_relay_url_gets_a_localized_message_under_the_field() {
        let pool = vec!["wss://relay.example.org".to_string()];
        for lang in [0, 1] {
            assert_eq!(
                relay_add_check(lang, "wss://fresh.example.org", &pool).as_deref(),
                Ok("wss://fresh.example.org")
            );
            assert!(
                relay_add_check(
                    lang,
                    "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
                    &pool
                )
                .is_ok(),
                "plaintext to an onion service is fine - Tor encrypts it"
            );
            // …and every refusal names its reason
            for bad in [
                "https://relay.example.org",
                "relay.example.org",
                "wss://",
                "ws://relay.example.org",
                "wss://relay example.org",
                // a .onion host that is not a real v3 address
                "wss://aaa.onion",
                // already in the pool (normalized: same relay, other spelling)
                "WSS://Relay.Example.ORG/",
            ] {
                let msg = relay_add_check(lang, bad, &pool)
                    .err()
                    .unwrap_or_else(|| panic!("{bad:?} must be refused with a message"));
                assert!(!msg.is_empty());
            }
        }
        // the five parser verdicts and the duplicate are DISTINCT messages,
        // so the user learns what to fix
        let msgs = [
            relay_add_check(0, "https://relay.example.org", &pool).err(),
            relay_add_check(0, "wss://", &pool).err(),
            relay_add_check(0, "ws://relay.example.org", &pool).err(),
            relay_add_check(0, "wss://relay example.org", &pool).err(),
            relay_add_check(0, "wss://aaa.onion", &pool).err(),
            relay_add_check(0, "wss://relay.example.org", &pool).err(),
        ];
        for (i, a) in msgs.iter().enumerate() {
            for b in msgs.iter().skip(i + 1) {
                assert_ne!(a, b, "each refusal reads differently");
            }
        }
        // German is a real translation, not the English string
        assert_ne!(
            relay_add_check(0, "wss://", &pool).err(),
            relay_add_check(1, "wss://", &pool).err(),
        );
    }

    /// The honesty invariant of the Tor probe, in colour: ONLY a proven
    /// circuit may read as "good". A SOCKS port that merely answers is amber
    /// (something is there, nothing is proven), and every rung that failed or
    /// refused is red or neutral — never green.
    #[test]
    fn only_a_proven_tor_circuit_is_toned_good() {
        use molt_core::TorTestState as S;
        assert_eq!(tor_test_tone(S::Circuit), TONE_GOOD);
        assert_eq!(tor_test_tone(S::ProxyOnly), TONE_WARN, "a listening port proves no circuit");
        for s in [S::Idle, S::Testing, S::Off] {
            assert_eq!(tor_test_tone(s), TONE_NEUTRAL, "{s:?} is not a verdict");
        }
        for s in [S::Misconfigured, S::NoProxy, S::NoTarget, S::CircuitFailed] {
            assert_eq!(tor_test_tone(s), TONE_BAD, "{s:?} is a failure");
        }
        for s in [
            S::Idle,
            S::Testing,
            S::Off,
            S::Misconfigured,
            S::NoProxy,
            S::ProxyOnly,
            S::NoTarget,
            S::CircuitFailed,
        ] {
            assert_ne!(tor_test_tone(s), TONE_GOOD, "{s:?} must never read as success");
        }
    }

    /// Every rung of the ladder reaches the user in their own language, and no
    /// two rungs share a sentence — the whole point is that the user learns
    /// WHICH rung was reached. The partial rung must say out loud that no
    /// circuit is proven.
    #[test]
    fn every_tor_rung_has_its_own_honest_copy_in_both_languages() {
        use molt_core::TorTestState as S;
        let all = [
            S::Idle,
            S::Testing,
            S::Off,
            S::Misconfigured,
            S::NoProxy,
            S::ProxyOnly,
            S::NoTarget,
            S::CircuitFailed,
            S::Circuit,
        ];
        for lang in [0, 1] {
            for (i, a) in all.iter().enumerate() {
                assert!(!tor_verdict_copy(lang, *a).is_empty(), "{a:?} needs copy");
                for b in all.iter().skip(i + 1) {
                    assert_ne!(
                        tor_verdict_copy(lang, *a),
                        tor_verdict_copy(lang, *b),
                        "{a:?} and {b:?} must not read the same"
                    );
                }
            }
            // German is a real translation, not the English string
            assert_ne!(tor_verdict_copy(0, *all.last().expect("non-empty")), tor_verdict_copy(1, *all.last().expect("non-empty")));
        }
        // the partial rung states the missing proof, in both languages
        assert!(
            tor_verdict_copy(0, S::ProxyOnly).contains("no circuit"),
            "EN must deny the circuit outright"
        );
        assert!(
            tor_verdict_copy(1, S::ProxyOnly).contains("Circuit"),
            "DE must deny the circuit outright"
        );
        // …and no rung short of Circuit may claim Tor works
        for s in all.iter().filter(|s| **s != S::Circuit) {
            let en = tor_verdict_copy(0, *s).to_lowercase();
            assert!(!en.contains("tor works"), "{s:?} must not claim Tor works");
        }
    }

    /// The technical second line never invents anything: it names only what
    /// the engine actually reported. A duration is shown for the rung it is
    /// meaningful on (the completed circuit) and nowhere else.
    #[test]
    fn the_tor_detail_line_states_only_what_was_probed() {
        use molt_core::{TorTest, TorTestState as S};
        assert_eq!(tor_test_detail(0, &TorTest::default()), "");
        let probed = TorTest {
            state: S::ProxyOnly,
            detail: "no confirmed relay to dial".into(),
            proxy: "127.0.0.1:9050".into(),
            target: String::new(),
            ms: 0,
        };
        let line = tor_test_detail(0, &probed);
        assert!(line.contains("127.0.0.1:9050"), "the probed SOCKS address is named");
        assert!(line.contains("no confirmed relay to dial"), "the engine's reason rides along");
        assert!(!line.contains("ms"), "no duration where none was measured");
        let circuit = TorTest {
            state: S::Circuit,
            detail: String::new(),
            proxy: "127.0.0.1:9050".into(),
            target: "wss://relay.onion".into(),
            ms: 812,
        };
        let line = tor_test_detail(0, &circuit);
        assert!(line.contains("wss://relay.onion"), "the relay that was reached is named");
        assert!(line.contains("812 ms"), "the circuit's dial time");
        // a duration measured on a rung that never completed a circuit is NOT
        // shown — it would read as a working connection
        let failed = TorTest { state: S::CircuitFailed, ms: 812, ..circuit.clone() };
        assert!(!tor_test_detail(0, &failed).contains("812 ms"));
    }

    /// The panel's button tests the DRAFT, not the saved settings: changing
    /// the anonymity network is restart-required, so the user will usually not
    /// have saved yet. The port is clamped into the wire type instead of
    /// wrapping — a garbage port must not silently become a valid one.
    #[test]
    fn the_tor_button_probes_the_draft_the_user_is_looking_at() {
        assert_eq!(tor_probe_args(0, 0, 9050), ("tor".to_string(), "local".to_string(), 9050));
        assert_eq!(
            tor_probe_args(0, 1, 9050),
            ("tor".to_string(), "embedded".to_string(), 9050)
        );
        assert_eq!(tor_probe_args(0, 2, 9050), ("tor".to_string(), "whonix".to_string(), 9050));
        // "none" is answered honestly by the engine (Off) — the GUI does not
        // silently rewrite it into a tor probe
        assert_eq!(tor_probe_args(1, 0, 9050), ("none".to_string(), "local".to_string(), 9050));
        // out-of-range drafts clamp to the "not given" marker, never wrap
        assert_eq!(tor_probe_args(0, 0, -1).2, 0);
        assert_eq!(tor_probe_args(0, 0, 70000).2, 0);
        assert_eq!(tor_probe_args(0, 0, 0).2, 0);
    }
}

#[cfg(test)]
mod s3_target_tests {
    //! The two S3 targets (`docs/storage/s3_buckets.md`): the byte quotas
    //! are edited in MiB but stored in bytes, and the two targets share no
    //! field on the way through the settings draft.

    use super::*;

    /// A quota the operator wrote by hand in bytes must survive a settings
    /// save that did not touch it - the MiB stepper is a VIEW of the value,
    /// not a re-quantization of it.
    #[test]
    fn an_untouched_byte_quota_is_not_rounded_onto_the_mib_grid() {
        // rounded UP, so the displayed limit is never smaller than the real one
        assert_eq!(mib_label(500_000_000), "477");
        assert_eq!(
            mib_text_to_bytes("477", 500_000_000),
            500_000_000,
            "the field still shows 477 - keep the exact stored bytes"
        );
        // …but a real edit converts
        assert_eq!(mib_text_to_bytes("1000", 500_000_000), 1000 * 1024 * 1024);
        // 0 is "no limit" on both sides, and clearing one really clears it
        assert_eq!(mib_label(0), "0");
        assert_eq!(mib_text_to_bytes("0", 0), 0);
        assert_eq!(mib_text_to_bytes("0", 500_000_000), 0);
        // an emptied field means no limit; garbage keeps the stored value
        // rather than inventing one
        assert_eq!(mib_text_to_bytes("  ", 500_000_000), 0);
        assert_eq!(mib_text_to_bytes("-5", 500_000_000), 500_000_000);
        assert_eq!(mib_text_to_bytes("abc", 500_000_000), 500_000_000);
        // an absurd number saturates instead of wrapping
        assert_eq!(mib_text_to_bytes(&u64::MAX.to_string(), 0), u64::MAX);
    }

    /// Push the account and both buckets into a real headless window and read
    /// the draft back: the two buckets stay distinct, and the quotas survive.
    #[test]
    fn both_buckets_round_trip_through_the_settings_draft() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let stored = SessionSettings {
            s3_endpoint: "https://backup.example.org".to_string(),
            s3_access_key: "BAK".to_string(),
            s3_secret_key: "bak-secret".to_string(),
            s3_bucket: "media-archive".to_string(),
            s3_max_bytes: 500_000_000,
            media_s3_bucket: "clips".to_string(),
            media_s3_max_bytes: 3 * 1024 * 1024 * 1024,
            ..SessionSettings::default()
        };
        apply_settings_fields(&ui, &stored);
        let draft = read_settings_draft(&ui, &stored);
        assert_eq!(draft.s3_endpoint, "https://backup.example.org");
        assert_eq!(draft.s3_bucket, "media-archive");
        assert_eq!(draft.media_s3_bucket, "clips");
        assert_eq!(
            draft.s3_access_key, "BAK",
            "one account: the credentials are not per bucket"
        );
        assert_eq!(
            draft.s3_max_bytes, 500_000_000,
            "the hand-written byte quota survives an untouched round trip"
        );
        assert_eq!(draft.media_s3_max_bytes, 3 * 1024 * 1024 * 1024);
        // and the form reports itself clean: an unedited draft must not make
        // the leave-guard fire
        assert!(
            !settings_draft_differs(&stored, &ui),
            "an untouched draft equals the stored settings"
        );
    }
}

#[cfg(test)]
mod gui_tests {
    //! **The GUI's own logic, run headless.**
    //!
    //! Everything here drives the REAL `AppWindow` against a REAL engine
    //! through the same live-mirror functions the running app uses — with
    //! `i-slint-backend-testing` there is no display and no window, so these
    //! belong in the ordinary suite.
    //!
    //! They exist because three chat bugs in a row were diagnosed by reading
    //! code instead of by evidence: the engine was provably right each time
    //! (checked against a live `moltd` over MCP), and the fault was in this
    //! layer, where nothing could observe it.

    use super::*;
    use molt_core::{ChannelRef, GroupConfig, SessionView};

    /// `gui_over_mcp.md` step 1's pin: the published snapshot claims what
    /// the WINDOW's models hold — screen, selection, the chat surface's
    /// row count and last bodies, the nav keys and the pending sum. The
    /// snapshot is the read half agents test the window through, so a
    /// drift here would make every such test lie.
    #[test]
    fn the_ui_snapshot_claims_what_the_window_holds() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_selected_view("today".into());
        ui.set_selected_channel("group".into());
        let log = ModelRc::new(VecModel::from(vec![
            LogLine { text: "erste".into(), ..LogLine::default() },
            LogLine { text: "zweite".into(), ..LogLine::default() },
            LogLine { text: "dritte".into(), ..LogLine::default() },
            LogLine { text: "vierte".into(), ..LogLine::default() },
        ]));
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![
            SurfaceTab {
                key: "chat".into(),
                log,
                pending_count: 0,
                ..SurfaceTab::default()
            },
            SurfaceTab { key: "organization".into(), pending_count: 2, ..SurfaceTab::default() },
        ])));
        let snap = build_ui_snapshot(&ui);
        assert_eq!(
            (snap.screen.as_str(), snap.surface.as_str(), snap.view.as_str(), snap.channel.as_str()),
            ("main", "chat", "today", "group")
        );
        assert_eq!(snap.chat_rows, 4, "the model's row count, not the engine's");
        assert_eq!(
            snap.chat_last,
            vec!["zweite".to_string(), "dritte".to_string(), "vierte".to_string()],
            "the last three rendered bodies"
        );
        assert_eq!(snap.nav, vec!["chat".to_string(), "organization".to_string()]);
        assert_eq!(snap.pending_count, 2);
        assert!(snap.compose_visible);
        let again = build_ui_snapshot(&ui);
        assert!(again.generation > snap.generation, "every publish bumps");
    }

    /// The wiki bridge drives the REAL generated `WikiState` face headless:
    /// open → edit → close → delete through the same callbacks the pane
    /// fires, asserting the models follow. This is the layer the unit tests
    /// in `wiki.rs` cannot see (types, models, borrow discipline).
    #[test]
    fn wiki_bridge_opens_edits_closes_and_deletes_headless() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let _wiki = wire_wiki(&ui);
        let g = ui.global::<WikiState>();
        // production starts EMPTY; the engine base arrives over the real
        // bridge (base-docs + base-arrived), exactly like the surfaces
        // mirror delivers it
        assert_eq!(g.get_tabs().row_count(), 0);
        assert!(!g.get_doc_open());
        g.set_base_docs(ModelRc::new(VecModel::from(vec![
            WikiBase {
                path: "charter.md".into(),
                content: "# Charter\n\nWhat we agreed to.".into(),
            },
            WikiBase {
                path: "glossary.md".into(),
                content: "# Glossary\n\nThe words we keep using.".into(),
            },
        ])));
        g.set_base_rev(1);
        g.invoke_base_arrived();
        assert_eq!(g.get_nav_rows().row_count(), 2, "the folded base lands");
        assert_eq!(g.get_cs_rows().row_count(), 0, "a clean tree has no panel");
        // open the charter via the open route so a tab exists
        let rows = g.get_nav_rows();
        let charter = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "charter.md")
            .expect("charter row");
        g.invoke_nav_open(charter.id);
        assert_eq!(g.get_tabs().row_count(), 1);
        assert!(g.get_doc_open());
        assert_eq!(g.get_doc_path().as_str(), "charter.md");
        // open glossary.md via the open route
        let rows = g.get_nav_rows();
        let glossary = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "glossary.md")
            .expect("glossary row");
        // a mark must PATCH the row model, never replace it: a swap
        // re-creates the row elements mid-double-click, which is exactly
        // how "double-click does not open" happened live
        g.invoke_nav_mark(glossary.id);
        let rows_after = g.get_nav_rows();
        assert!(
            std::ptr::eq(
                rows.as_any()
                    .downcast_ref::<VecModel<WikiNavRow>>()
                    .expect("nav rows are a VecModel") as *const _,
                rows_after
                    .as_any()
                    .downcast_ref::<VecModel<WikiNavRow>>()
                    .expect("still a VecModel") as *const _,
            ),
            "the nav model must survive a mark (rows patch in place)"
        );
        g.invoke_nav_open(glossary.id);
        assert_eq!(g.get_tabs().row_count(), 2);
        assert_eq!(g.get_doc_path().as_str(), "glossary.md");
        // a base refresh with the SAME content is a no-op for the models
        g.invoke_base_arrived();
        assert_eq!(g.get_tabs().row_count(), 2);
        assert_eq!(g.get_doc_path().as_str(), "glossary.md");
        // an edit turns up on the changeset stack, the tab status and the
        // preview diff
        g.invoke_edit_toggle();
        let edited = format!("{}\n\nA new closing thought.", g.get_raw());
        g.invoke_edited(edited.into());
        assert_eq!(g.get_cs_rows().row_count(), 1);
        let row = g.get_cs_rows().row_data(0).expect("stack row");
        assert_eq!(row.kind, 5, "an edit row");
        assert_eq!(row.label.as_str(), "glossary.md");
        assert!(g.get_cs_lines() > 0, "touched lines are counted");
        let tabs = g.get_tabs();
        let gtab = (0..tabs.row_count())
            .filter_map(|i| tabs.row_data(i))
            .find(|t| t.label.as_str() == "glossary.md")
            .expect("glossary tab");
        assert_eq!(gtab.status, 2, "the tab paints modified");
        g.invoke_edit_toggle();
        let blocks = g.get_blocks();
        assert!(
            (0..blocks.row_count())
                .filter_map(|i| blocks.row_data(i))
                .any(|b| b.status == 1 && b.text.as_str().contains("closing thought")),
            "the appended paragraph previews as Added"
        );
        // Ctrl+W closes glossary; focus falls back to the charter tab
        g.invoke_close_active();
        assert_eq!(g.get_tabs().row_count(), 1);
        assert_eq!(g.get_doc_path().as_str(), "charter.md");
        // Del on the marked (still glossary) row: a pending deletion — the
        // row stays, struck, and the chip carries both changes
        g.invoke_delete_marked();
        let rows = g.get_nav_rows();
        let struck = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "glossary.md")
            .expect("the deleted row stays listed");
        assert_eq!(struck.status, 3);
        // the stack narrates both actions, the NET counts only the delete
        assert_eq!(g.get_cs_rows().row_count(), 2);
        assert_eq!(g.get_cs_deleted(), 1);
        assert_eq!(g.get_cs_lines(), 0, "a deleted file's edits are not lines");
        // undo takes back the deletion (the edit stays pending) …
        g.invoke_cs_undo();
        assert_eq!(g.get_cs_rows().row_count(), 1);
        assert_eq!(g.get_cs_deleted(), 0);
        assert!(g.get_cs_lines() > 0);
        // … a per-file revert clears the file without touching others …
        g.invoke_nav_revert(struck.id);
        assert_eq!(g.get_cs_rows().row_count(), 0, "the panel is gone");
        // … and after fresh changes, revert-all clears everything at once
        g.invoke_new_file();
        g.invoke_new_folder();
        assert_eq!(g.get_cs_rows().row_count(), 2);
        assert_eq!(g.get_cs_added(), 1);
        g.invoke_cs_revert();
        assert_eq!(g.get_cs_rows().row_count(), 0);
        assert_eq!(g.get_cs_added(), 0);
    }

    /// **The 0px-collapse trap, pinned.** Seven of the nine poke sites wrap
    /// an existing Text in a `ContextMenuArea`, and three of those wrappers
    /// sit inside a LAYOUT — where an element contributes its children's
    /// size constraints or nothing at all. Nothing at all means an
    /// invisible, unclickable name. This measures the real geometry of the
    /// chat author's name after a live mirror pass.
    ///
    /// **Runs on the dev-ui chain only** — `ElementHandle` queries need the
    /// element names Slint keeps under `SLINT_EMIT_DEBUG_INFO`, which the
    /// interpreter path carries anyway while the code generator would put
    /// them into the ~400k-line module (a build that already peaks at ~9 GiB).
    /// The layout engine under test is the same in both paths. Run it with
    /// `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test
    /// -p molt-ui --lib --features molt-ui/live-preview`.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_chat_author_name_keeps_its_width_inside_the_poke_menu_wrapper() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        ui.window()
            .set_size(slint::PhysicalSize::new(1200, 800));

        rt.block_on(async {
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();
            mirror(&w, &ui, &last, &chat_ui).await;
        });
        assert!(chat_rows(&ui) > 0, "no chat row, nothing to measure");
        // the repeaters only materialize on a shown window in the main screen
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.show().expect("show headless");

        let names: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-name")
                .collect();
        let menus: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-menu")
                .collect();
        assert!(!names.is_empty(), "the author header must render");
        assert_eq!(names.len(), menus.len(), "every name carries its menu area");
        for (n, m) in names.iter().zip(menus.iter()) {
            assert!(
                n.size().width > 1.0,
                "author name collapsed to {}px",
                n.size().width
            );
            // the CLICK area is what breaks silently: a wrapper that
            // contributes no size constraint is invisible to the pointer
            assert!(
                m.size().width >= n.size().width && m.size().height >= n.size().height,
                "menu area {}x{} does not cover the name {}x{}",
                m.size().width,
                m.size().height,
                n.size().width,
                n.size().height
            );
        }
    }
    /// **A right-click must actually OPEN the menu.** Every poke site is a
    /// `ContextMenuArea`; the operator reported that right-clicking does
    /// nothing anywhere, while the engine path is provably fine (a poke
    /// issued over MCP toasts on both nodes). This dispatches a REAL right
    /// press onto the chat author's menu area and looks for the menu item
    /// that must appear — checked to be ABSENT before the click, so a
    /// find that always matches cannot pass for a menu.
    ///
    /// **Runs on the dev-ui chain only** (element ids), like its geometry
    /// sibling above.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_right_click_on_a_poke_site_opens_the_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        rt.block_on(async {
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();
            mirror(&w, &ui, &last, &chat_ui).await;
        });
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        apply_strings(&ui, 0);
        ui.show().expect("show headless");
        // the author is the own seat in this fixture — make it a POKABLE
        // name so the area is enabled (the gate is what `Poke.can` decides)
        ui.global::<Poke>().set_me("petra".into());
        ui.global::<Poke>().set_on(true);

        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let menu = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "ChatRow::author-menu",
        )
        .next()
        .expect("the author menu area must render");
        right_click(&ui, &menu, 0.5);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click opened no menu carrying {label:?}"
        );
    }

    /// The SAME right-click, on the site the operator actually uses:
    /// Organization → Members. Its `ContextMenuArea` wraps the whole row,
    /// so the press must reach it wherever the row is not covered by a
    /// control of its own.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_right_click_on_a_member_row_opens_the_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(true);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        assert_eq!(rows.len(), 2, "one menu area per member row");
        // row 1 is petra — the pokable seat
        let area = &rows[1];
        assert!(
            area.size().width > 1.0 && area.size().height > 1.0,
            "the menu area collapsed to {:?}",
            area.size()
        );
        right_click(&ui, area, 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click on the member row opened no menu"
        );
    }

    /// **Poking off must not make the feature vanish.** An entry that is
    /// simply absent reads as a dead right-click (that is how the operator
    /// met it). With the switch off the menu still opens and names the
    /// action - greyed, so it says "this exists, it is off" instead of
    /// nothing at all.
    #[cfg(feature = "live-preview")]
    #[test]
    fn with_poking_off_the_member_row_still_offers_the_entry_greyed() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(false);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        right_click(&ui, &rows[1], 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "the entry must still be offered, greyed - `Poke.on` is what the \
             MenuItem binds its `enabled` to, and `can()` (pinned separately) \
             is what refuses the command"
        );
    }

    /// The own seat is never a poke target, switch or no switch: its row
    /// offers no menu at all.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_own_seats_row_offers_no_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(true);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        // row 0 is walter — this node's own seat
        right_click(&ui, &rows[0], 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "the own seat must not offer the entry"
        );
    }

    /// **Organization → Accepted: the Value column must never overrun its
    /// cell.** A description change carries a whole sentence as its value;
    /// an unwrapped `Text` reports that whole line as its PREFERRED width,
    /// which pushes the row past the table instead of eliding inside it.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_long_accepted_value_elides_inside_its_cell() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("accepted".into());
        // a seat description is typed into a multi-line box, so its value
        // can carry NEWLINES — a table cell that renders them is three
        // lines tall inside a 40px row and paints over its neighbours
        let long = "Baut an der Autistenzentrale\nund schreibt die Protokolle,\n\
             erreichbar meistens nachts, Zeitzone egal, und noch ein Satz \
             damit die Zelle ganz sicher zu schmal wird";
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            applied_count: 1,
            accepted: ModelRc::new(VecModel::from(vec![ProposalRow {
                id: 1,
                text: "Member description".into(),
                proposed: long.into(),
                ..ProposalRow::default()
            }])),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        ui.show().expect("show headless");

        let table = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            &ui,
            "DecisionTable",
        )
        .next()
        .expect("the decided-votes table must render");
        let cell = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "DecisionTable::dt-value",
        )
        .next()
        .expect("the value cell must render");
        let right = cell.absolute_position().x + cell.size().width;
        let edge = table.absolute_position().x + table.size().width;
        eprintln!(
            "value cell {:?} w={} right={right} table right={edge}",
            cell.absolute_position(),
            cell.size().width
        );
        assert!(
            right <= edge,
            "the value ran {}px past the table",
            right - edge
        );
    }

    /// **The chat presence strip: the pill follows the name, and the
    /// last-seen label never leaves it.** A seat name is free text, and an
    /// unelided `Text` reports the WHOLE name as its preferred width - so a
    /// long name pushed itself and the last-seen label straight out of the
    /// fixed 150px pill (reported 2026-08-22). Two things are pinned: the
    /// column follows the longest name, and inside a pill the NAME is what
    /// gives way - the last-seen label stays visible.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_long_member_name_grows_its_pill_and_keeps_the_last_seen_label() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "chat".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);

        let seat = |name: &str, last: &str| MemberSync {
            name: name.into(),
            last: last.into(),
            state: 0,
        };
        let measure = |ui: &AppWindow| -> Vec<(f32, f32, f32)> {
            let pills: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                ui,
                "MemberPill",
            )
            .collect();
            let names: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-name")
                    .collect();
            let lasts: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-last")
                    .collect();
            assert_eq!(pills.len(), names.len(), "every pill renders its name");
            assert_eq!(pills.len(), lasts.len(), "every pill renders its last-seen");
            pills
                .iter()
                .zip(names.iter())
                .zip(lasts.iter())
                .map(|((p, n), l)| {
                    let edge = p.absolute_position().x + p.size().width;
                    let name_right = n.absolute_position().x + n.size().width;
                    let last_right = l.absolute_position().x + l.size().width;
                    eprintln!(
                        "pill w={} right={edge} | name w={} right={name_right} | last w={} right={last_right}",
                        p.size().width,
                        n.size().width,
                        l.size().width
                    );
                    assert!(
                        name_right <= edge,
                        "the name ran {}px past its pill",
                        name_right - edge
                    );
                    assert!(
                        l.size().width > 1.0 && last_right <= edge,
                        "the last-seen label is {}px wide and ends {}px past the pill",
                        l.size().width,
                        last_right - edge
                    );
                    // and it is PARKED at the right edge, so the labels line
                    // up down the grid instead of drifting with the name
                    assert!(
                        last_right >= edge - 12.0,
                        "the last-seen label floats {}px short of the pill edge",
                        edge - last_right
                    );
                    (p.size().width, n.size().width, l.size().width)
                })
                .collect()
        };

        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat("ada", "2 min ago"),
            seat("bob", "just now"),
        ])));
        ui.show().expect("show headless");
        let short = measure(&ui);

        // a name of ordinary length still fits, but the pill has to GROW for
        // it instead of cutting it off at the 150px column
        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat("bartholomaeus-von-habsburg", "2 min ago"),
            seat("bob", "just now"),
        ])));
        let grown = measure(&ui);
        assert!(
            grown[0].0 > short[0].0 + 10.0,
            "the pill did not follow the name: {}px vs {}px",
            grown[0].0,
            short[0].0
        );

        // past every sane cap the NAME elides - the last-seen label stays
        // (measure() asserts it for every pill)
        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat(&"x".repeat(300), "2 min ago"),
            seat("bob", "just now"),
        ])));
        let huge = measure(&ui);
        assert!(
            huge[0].1 < 2000.0,
            "the name was not elided: {}px",
            huge[0].1
        );

        // a cut name is not a lost name: hovering the pill spells it out in
        // the window-topmost hint overlay
        let pill = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            &ui,
            "MemberPill",
        )
        .next()
        .expect("the strip renders its pills");
        let at = slint::LogicalPosition::new(
            pill.absolute_position().x + pill.size().width / 2.0,
            pill.absolute_position().y + pill.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        assert_eq!(
            ui.global::<HintTip>().get_label().to_string(),
            "x".repeat(300),
            "the elided name must read in full on hover"
        );
    }

    /// **Every button must survive a bigger font.** The app font is a
    /// setting (9-28px); a button whose height or width is a hardcoded
    /// pixel count keeps the box of the 14px default while its label grows
    /// inside it - which is how the operator met a cut-off "Entschlüsseln"
    /// on the Open screen. Two invariants, measured on the real layout:
    /// a button's label stays INSIDE the button, and no two buttons
    /// overlap (a button taller than its fixed row lands on its neighbour).
    #[cfg(feature = "live-preview")]
    #[test]
    fn every_button_keeps_its_label_at_the_largest_font() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        apply_strings(&ui, 1); // German: the longest labels in the app
        // the biggest size the stepper offers
        ui.global::<Theme>().set_fs_app(28.0);
        ui.set_screen(AppScreen::Open);
        ui.set_ws_list(ModelRc::new(VecModel::from(vec![
            WorkspaceItem {
                id: "a".into(),
                name: "Erste Republik".into(),
                detail: "2-of-3".into(),
                status: "Synchronisiert".into(),
                synced: true,
                backup: "vor 30 Min.".into(),
                ..WorkspaceItem::default()
            },
            WorkspaceItem {
                id: "b".into(),
                name: "Zweite Republik".into(),
                detail: "3-of-5".into(),
                status: "Offline".into(),
                encrypted: true,
                backup: "nie".into(),
                ..WorkspaceItem::default()
            },
        ])));
        ui.show().expect("show headless");
        let mut checked = assert_buttons_scale(&ui, "open");
        assert!(checked > 3, "the Open screen must render its buttons");

        // the choice screen and the three wizards, step by step
        ui.set_screen(AppScreen::Choice);
        checked += assert_buttons_scale(&ui, "choice");
        for (screen, steps, set) in [
            (AppScreen::Create, 4, 0),
            (AppScreen::Join, 3, 1),
            (AppScreen::Restore, 4, 2),
        ] {
            ui.set_screen(screen);
            for step in 0..steps {
                match set {
                    0 => ui.set_cw_step(step),
                    1 => ui.set_jw_step(step),
                    _ => ui.set_rw_step(step),
                }
                checked += assert_buttons_scale(&ui, &format!("{screen:?} step {step}"));
            }
        }

        // the main screen, one pass per surface - WITH rows in them: the
        // buttons that sit inside chat rows, proposal cards and the members
        // table are exactly the ones a fixed row height would squash
        let log = ModelRc::new(VecModel::from(vec![
            LogLine {
                id: "aa".repeat(16).into(),
                lead: "bartholomaeus".into(),
                text: "Erste Nachricht in der Republik".into(),
                when: "2026-08-22 13:37 (gerade eben)".into(),
                first: true,
                quote: -1,
                patch_id: -1,
                ..LogLine::default()
            },
            LogLine {
                id: "bb".repeat(16).into(),
                lead: "petra".into(),
                text: "Zweite Nachricht".into(),
                when: "2026-08-22 13:38 (gerade eben)".into(),
                first: true,
                own: true,
                quote: -1,
                patch_id: -1,
                ..LogLine::default()
            },
        ]));
        let votes = ModelRc::new(VecModel::from(vec![
            ProposalRow {
                id: 1,
                text: "Mitgliedsbeschreibung ändern".into(),
                proposed: "ein neuer Satz".into(),
                ..ProposalRow::default()
            },
            ProposalRow {
                id: 2,
                text: "Relais aufnehmen".into(),
                proposed: "wss://relay.example".into(),
                ..ProposalRow::default()
            },
        ]));
        let surfaces: Vec<SurfaceTab> = ["chat", "organization", "memory", "vault", "kanban"]
            .iter()
            .map(|k| SurfaceTab {
                key: (*k).into(),
                log: log.clone(),
                pending: votes.clone(),
                accepted: votes.clone(),
                pending_count: 2,
                applied_count: 2,
                ..SurfaceTab::default()
            })
            .collect();
        ui.set_surfaces(ModelRc::new(VecModel::from(surfaces.clone())));
        ui.set_org_members(ModelRc::new(VecModel::from(vec![
            MemberRow {
                name: "bartholomaeus".into(),
                last: "vor 2 Min.".into(),
                ..MemberRow::default()
            },
            MemberRow {
                name: "petra".into(),
                last: "22.07.2026".into(),
                ..MemberRow::default()
            },
        ])));
        // the tables whose rows carry buttons - uploads, backups, the relay
        // pickers - are exactly the fixed-height rows a bigger font bursts
        ui.set_org_uploads(ModelRc::new(VecModel::from(vec![UploadRow {
            id: "cc".repeat(16).into(),
            name: "protokoll.pdf".into(),
            user: "bartholomaeus".into(),
            date: "2026-08-22".into(),
            kind: "PDF".into(),
            size: "1.2 MiB".into(),
            available: true,
            online: true,
            expires: "in 13 Tagen".into(),
            ..UploadRow::default()
        }])));
        ui.set_bk_rows(ModelRc::new(VecModel::from(vec![BackupRow {
            id: "a".into(),
            local: "Erste Republik".into(),
            remote: "erste.molt.enc".into(),
            has_local: true,
            size: "1.8 MiB".into(),
            ..BackupRow::default()
        }])));
        ui.set_cw_relay_picks(ModelRc::new(VecModel::from(vec![RelayPick {
            url: "wss://relay.example".into(),
            picked: true,
        }])));
        ui.set_screen(AppScreen::Main);
        for s in &surfaces {
            ui.set_selected_surface(s.key.clone());
            for view in ["", "members", "pending", "accepted", "today"] {
                ui.set_selected_view(view.into());
                checked += assert_buttons_scale(&ui, &format!("main/{}/{view}", s.key));
            }
        }

        // and the settings screen
        ui.set_screen(AppScreen::Settings);
        checked += assert_buttons_scale(&ui, "settings");
        // a sweep that silently stopped finding buttons proves nothing
        assert!(checked > 40, "only {checked} buttons were measured");
    }

    /// The measured invariants, so every screen can be checked the same
    /// way: a label inside its button, and no two buttons overlapping.
    #[cfg(feature = "live-preview")]
    fn assert_buttons_scale(ui: &AppWindow, screen: &str) -> usize {
        let buttons: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "AppButton")
                .filter(|b| b.size().width > 0.0 && b.size().height > 0.0)
                .collect();
        // the other controls sit in the SAME rows and scale by the same
        // token: a field that outgrows its row lands on the button next to
        // it, so they all go into the overlap check
        let controls: Vec<_> = ["AppField", "AppDropdown", "AppStepper", "AppCheck"]
            .iter()
            .flat_map(|t| {
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, t)
                    .filter(|c| c.size().width > 0.0 && c.size().height > 0.0)
                    .collect::<Vec<_>>()
            })
            .chain(buttons.iter().cloned())
            .collect();
        let labels: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppButton::abtn-label")
                .filter(|l| l.size().width > 0.0)
                .collect();
        let rect = |e: &i_slint_backend_testing::ElementHandle| {
            let p = e.absolute_position();
            let s = e.size();
            (p.x, p.y, p.x + s.width, p.y + s.height)
        };
        for l in &labels {
            let (lx0, ly0, lx1, ly1) = rect(l);
            // the button this label belongs to: the label sits on its
            // line, so take the button whose vertical span holds the
            // label's middle and whose left edge is the nearest one left
            // of the label (an overflowing label still STARTS inside)
            let mid = (ly0 + ly1) / 2.0;
            let owner = buttons
                .iter()
                .filter(|b| {
                    let (bx0, by0, _, by1) = rect(b);
                    bx0 <= lx0 + 0.5 && by0 <= mid && mid <= by1
                })
                .max_by(|a, b| {
                    rect(a)
                        .0
                        .partial_cmp(&rect(b).0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            let Some(owner) = owner else { continue };
            let (bx0, by0, bx1, by1) = rect(owner);
            assert!(
                lx1 <= bx1 + 0.5 && ly1 <= by1 + 0.5 && lx0 >= bx0 - 0.5 && ly0 >= by0 - 0.5,
                "{screen}: the label \"{}\" ({lx0},{ly0})-({lx1},{ly1}) breaks out of its \
                 button ({bx0},{by0})-({bx1},{by1})",
                l.accessible_label().unwrap_or_default()
            );
        }
        for (i, a) in controls.iter().enumerate() {
            for b in controls.iter().skip(i + 1) {
                let (ax0, ay0, ax1, ay1) = rect(a);
                let (bx0, by0, bx1, by1) = rect(b);
                let overlap = ax0 < bx1 - 0.5
                    && bx0 < ax1 - 0.5
                    && ay0 < by1 - 0.5
                    && by0 < ay1 - 0.5;
                assert!(
                    !overlap,
                    "{screen}: two controls overlap - {} ({ax0},{ay0})-({ax1},{ay1}) and \
                     {} ({bx0},{by0})-({bx1},{by1})",
                    a.type_name().unwrap_or_default(),
                    b.type_name().unwrap_or_default()
                );
            }
        }
        buttons.len()
    }

    /// **A hint must not outlive the pointer.** The nav rows write the
    /// window-topmost `HintTip` overlay on hover and clear it on leave -
    /// but the clear was guarded by comparing the tip's ANCHOR to the
    /// row's current position, so a row that moved while hovered (the nav
    /// expands its sub-views on a click, a list scrolls) could never
    /// recognize its own tip again and the bubble stayed on screen for
    /// good. Pinned here: leaving the row clears it, and so does leaving
    /// the window - even after the row moved underneath the pointer.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_nav_hint_disappears_when_the_pointer_leaves_the_row() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1000, 700));
        apply_strings(&ui, 1);
        // big font + long names: the nav label elides, which is what makes
        // the expanded row write a hint at all
        ui.global::<Theme>().set_fs_app(24.0);
        let views = ModelRc::new(VecModel::from(vec![
            ViewItem {
                key: "status".into(),
                name: "Status".into(),
                ..ViewItem::default()
            },
            ViewItem {
                key: "members".into(),
                name: "Mitglieder".into(),
                ..ViewItem::default()
            },
        ]));
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![
            SurfaceTab {
                key: "organization".into(),
                name: "Organisation der Republik".into(),
                views: views.clone(),
                ..SurfaceTab::default()
            },
            SurfaceTab {
                key: "chat".into(),
                name: "Unterhaltung und Beschlüsse".into(),
                views: views.clone(),
                ..SurfaceTab::default()
            },
        ])));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.show().expect("show headless");

        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
                .filter(|r| r.size().width > 0.0)
                .collect();
        assert!(rows.len() >= 2, "the nav must render its rows");
        // a hover change reaches the `changed` handlers on the next frame -
        // headless, that frame is `mock_elapsed_time` (it runs the change
        // trackers), which the real app gets for free from its render loop
        let frame = || {
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let hover = |ui: &AppWindow, e: &i_slint_backend_testing::ElementHandle| {
            let p = e.absolute_position();
            let s = e.size();
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(
                        p.x + s.width / 2.0,
                        p.y + s.height / 2.0,
                    ),
                });
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let leave = |ui: &AppWindow| {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(900.0, 400.0),
                });
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let tip = |ui: &AppWindow| ui.global::<HintTip>().get_label().to_string();

        // 1. plain enter/leave
        hover(&ui, &rows[1]);
        assert!(!tip(&ui).is_empty(), "hovering a cut nav label shows its hint");
        leave(&ui);
        assert_eq!(tip(&ui), "", "the hint must go when the pointer leaves");

        // 2. the row MOVES while hovered - a bigger font resizes every nav
        //    row, so the hovered one is somewhere else by the time the
        //    pointer leaves. This is the case the old anchor-guard could
        //    not clear (it compared the tip's anchor to the row's CURRENT
        //    position), and it is deliberately not one of the navigations
        //    that drop the hint outright.
        let before = rows[1].absolute_position().y;
        hover(&ui, &rows[1]);
        assert!(!tip(&ui).is_empty(), "hint is up again");
        ui.global::<Theme>().set_fs_app(20.0);
        frame();
        let moved: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
                .filter(|r| r.size().width > 0.0)
                .collect();
        assert_ne!(
            moved[1].absolute_position().y,
            before,
            "the fixture must actually move the row"
        );
        leave(&ui);
        assert_eq!(tip(&ui), "", "a hint whose row moved must still clear");

        // 3. the pointer leaves the WINDOW (the nav sits at the left edge,
        //    so this is the ordinary way out of it)
        hover(&ui, &moved[1]);
        assert!(!tip(&ui).is_empty(), "hint is up again");
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerExited);
        frame();
        assert_eq!(tip(&ui), "", "leaving the window must clear the hint");
    }

    /// **Organization -> Status: the gated-settings card.** Its rows are
    /// label + value + pencil inside a 300px card; an unelided label
    /// reports its whole line as the row's preferred width and shoves the
    /// pencil through the card's border (reported 2026-08-23). The label
    /// is what gives way, and the pencils line up on one right edge.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_org_settings_pencils_stay_inside_the_card_and_line_up() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        apply_strings(&ui, 1); // German: the long "Chat löschen nach" line
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("status".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            name: "Organisation".into(),
            ..SurfaceTab::default()
        }])));
        ui.set_org_chat_retention(30);
        ui.set_org_relays(ModelRc::new(VecModel::from(vec![
            slint::SharedString::from("wss://relay.example"),
        ])));
        ui.show().expect("show headless");

        for font in [14.0_f32, 24.0] {
            ui.global::<Theme>().set_fs_app(font);
            let card = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                &ui,
                "OrgSettingsCard",
            )
            .find(|c| c.size().width > 0.0)
            .expect("the gated-settings card must render");
            let edge = card.absolute_position().x + card.size().width;
            let pencils: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                    .filter(|b| {
                        let p = b.absolute_position();
                        b.size().width > 0.0
                            && p.x >= card.absolute_position().x - 1.0
                            && p.y >= card.absolute_position().y - 1.0
                            && p.y <= card.absolute_position().y + card.size().height + 1.0
                    })
                    .collect();
            assert_eq!(pencils.len(), 2, "font {font}: relays + retention pencil");
            let mut rights = Vec::new();
            for b in &pencils {
                let right = b.absolute_position().x + b.size().width;
                assert!(
                    right <= edge,
                    "font {font}: the pencil ran {}px through the card border",
                    right - edge
                );
                rights.push(right);
            }
            assert!(
                (rights[0] - rights[1]).abs() < 1.0,
                "font {font}: the pencils are not aligned: {rights:?}"
            );

            // and EVERY pencil in the pane's right-hand column shares that
            // edge - they read as one column, so a panel with its own
            // padding staggers visibly against its neighbours
            let column: Vec<f32> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                    .filter(|b| {
                        let (w, h) = (b.size().width, b.size().height);
                        let right = b.absolute_position().x + w;
                        // square = one of the ✎ pencils, not a labelled button
                        w > 0.0 && (w - h).abs() < 1.0 && (edge - right).abs() < 60.0
                    })
                    .map(|b| b.absolute_position().x + b.size().width)
                    .collect();
            assert!(column.len() >= 4, "font {font}: found {} pencils", column.len());
            let (lo, hi) = column.iter().fold((f32::MAX, f32::MIN), |(lo, hi), r| {
                (lo.min(*r), hi.max(*r))
            });
            assert!(
                hi - lo < 1.0,
                "font {font}: the pencil column staggers by {}px: {column:?}",
                hi - lo
            );
        }
    }

    /// The pill CLIPS - a pane too narrow for even the elided name must not
    /// paint over its neighbour - and a clipped element must still hand its
    /// right-click to the poke menu, which renders in the window's popup
    /// layer rather than inside the pill.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_clipped_presence_pill_still_opens_its_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "chat".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        let poke = ui.global::<Poke>();
        poke.set_on(true);
        poke.set_me("walter".into());
        ui.set_active_members(ModelRc::new(VecModel::from(vec![MemberSync {
            name: "ada".into(),
            last: "2 min ago".into(),
            state: 0,
        }])));
        ui.show().expect("show headless");

        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let pills: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "MemberPill")
                .collect();
        assert_eq!(pills.len(), 1, "the strip renders the seat");
        right_click(&ui, &pills[0], 0.5);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click on the presence pill opened no menu"
        );
    }

    /// The decided-votes table is an INDEX: one line per decision. A seat
    /// description is typed into a multi-line box (`Ich bin der Peter\n!`
    /// is what the operator's node actually holds), and rendering that
    /// newline made the 40px row two lines tall. The CARD keeps the real
    /// shape — sign-what-you-see reads the value as it will be applied.
    #[test]
    fn a_decided_rows_value_reads_as_one_line_while_the_card_keeps_the_shape() {
        let data = ProposalRowData {
            current: "Ich bin der Peter\n!".to_string(),
            proposed: "erste\n\nzweite   dritte".to_string(),
            ..ProposalRowData::default()
        };
        let row = to_decided_row(&data);
        assert_eq!(row.current.as_str(), "Ich bin der Peter !");
        assert_eq!(row.proposed.as_str(), "erste zweite dritte");
        let card = to_proposal_row(&data);
        assert!(
            card.proposed.as_str().contains('\n'),
            "the vote card must show the value as it will be applied"
        );
    }

    /// **The settings tab bar wraps, the titles do not.** A tab title must
    /// never break inside its own tab; when the row cannot hold them all,
    /// the BAR takes a second row instead. Measured on the real geometry at
    /// two window widths.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_settings_tabs_stay_one_line_and_the_bar_wraps_when_it_must() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Settings);
        ui.set_active_workspace("w".into());
        apply_strings(&ui, 1); // German — the widest titles
        ui.window().set_size(slint::PhysicalSize::new(1600, 900));
        ui.show().expect("show headless");

        let rows_at = |ui: &AppWindow| -> Vec<f32> {
            let mut ys: Vec<f32> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "SettingsTab")
                    .map(|t| t.absolute_position().y)
                    .collect();
            ys.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            ys.dedup_by(|a, b| (*a - *b).abs() < 1.0);
            ys
        };
        let tabs: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
                .collect();
        assert_eq!(tabs.len(), 9, "nine tabs with a workspace open");
        for t in &tabs {
            assert!(
                t.size().height <= 30.0,
                "a tab grew to {}px - its title wrapped inside the tab",
                t.size().height
            );
        }
        assert_eq!(rows_at(&ui).len(), 1, "1600px holds every tab in one row");

        // …and narrow enough, the BAR breaks instead of the titles
        ui.window().set_size(slint::PhysicalSize::new(700, 900));
        let narrow: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
                .collect();
        assert_eq!(narrow.len(), 9, "no tab may disappear when the bar wraps");
        for t in &narrow {
            assert!(
                t.size().height <= 30.0,
                "a tab grew to {}px in the narrow bar",
                t.size().height
            );
        }
        assert_eq!(rows_at(&ui).len(), 2, "700px needs a second row");
    }

    /// **Settings: the S3 credentials have their own tab.** "Backup" used
    /// to carry two errands at once - WHEN/WHICH workspace is backed up,
    /// and WHERE the bucket is. The endpoint, keys and bucket moved to
    /// "S3 config"; the schedule stayed. Driven by real clicks on the real
    /// bar (the tabs are found by type and ordered left to right - the
    /// bar's transparent measuring texts carry the same titles, so looking
    /// tabs up by their label would hit those instead).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_s3_endpoint_moved_out_of_the_backup_tab_onto_its_own() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Settings);
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1600, 900));
        ui.show().expect("show headless");

        let shown = |ui: &AppWindow, label: &str| {
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label)
                .next()
                .is_some()
        };
        let click_tab = |ui: &AppWindow, index: usize| {
            let mut tabs: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                ui,
                "SettingsTab",
            )
            .collect();
            tabs.sort_by(|a, b| {
                a.absolute_position()
                    .x
                    .partial_cmp(&b.absolute_position().x)
                    .expect("no NaN")
            });
            let tab = tabs.get(index).expect("the tab must render");
            let pos = tab.absolute_position();
            let size = tab.size();
            let at = slint::LogicalPosition::new(
                pos.x + size.width / 2.0,
                pos.y + size.height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
        };

        click_tab(&ui, 2); // Backup
        assert!(shown(&ui, "Automatic S3 backup"), "the schedule stays on Backup");
        assert!(!shown(&ui, "S3 endpoint"), "the endpoint left the Backup tab");
        assert!(!shown(&ui, "Access key"), "so did the keys");

        click_tab(&ui, 3); // S3 config
        assert!(shown(&ui, "S3 endpoint"), "the endpoint lives on the S3 tab");
        assert!(shown(&ui, "Access key"), "and so do the keys");
        assert!(shown(&ui, "Bucket"), "and the bucket");
        assert!(!shown(&ui, "Automatic S3 backup"), "the schedule did not follow");
    }

    /// The Vault mock's secrets list, headless: the deposits render, and a
    /// click on "Seal a secret" opens the dialog it was given (the button
    /// used to be dead, with a "not yet" tooltip).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_vault_seal_button_opens_its_dialog() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("vault".into());
        ui.set_selected_view("secrets".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "vault".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        ui.show().expect("show headless");

        let cards: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SecretCard")
                .collect();
        eprintln!("secret cards: {}", cards.len());
        assert!(cards.len() >= 6, "the sample deposits must render");
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
                .next()
                .is_none(),
            "no dialog before the click"
        );

        let button = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "VaultPane::vt-seal-btn",
        )
        .next()
        .expect("the seal button must render");
        let pos = button.absolute_position();
        let size = button.size();
        let at = slint::LogicalPosition::new(
            pos.x + size.width / 2.0,
            pos.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
                .next()
                .is_some(),
            "the seal dialog must open"
        );
    }

    /// Organization → Members with two seats, rendered headless. `on` is
    /// the applied poke switch the menus gate on.
    #[cfg(feature = "live-preview")]
    fn members_window(on: bool) -> AppWindow {
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("members".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            ..SurfaceTab::default()
        }])));
        ui.set_node_member("walter".into());
        ui.set_org_members(ModelRc::new(VecModel::from(vec![
            MemberRow { name: "walter".into(), ..MemberRow::default() },
            MemberRow { name: "petra".into(), ..MemberRow::default() },
        ])));
        ui.global::<Poke>().set_me("walter".into());
        ui.global::<Poke>().set_on(on);
        apply_strings(&ui, 0);
        ui.show().expect("show headless");
        ui
    }

    /// One real right press inside `area`, `fx` across its width (1.0 = the
    /// right edge) and vertically centred.
    #[cfg(feature = "live-preview")]
    fn right_click(ui: &AppWindow, area: &i_slint_backend_testing::ElementHandle, fx: f32) {
        let pos = area.absolute_position();
        let size = area.size();
        let at = slint::LogicalPosition::new(
            pos.x + (size.width * fx).min(size.width - 2.0),
            pos.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Right,
        });
    }

    /// The open poke menu, found by the title its single item carries.
    #[cfg(feature = "live-preview")]
    fn poke_menu_open(
        ui: &AppWindow,
        label: &str,
    ) -> Option<i_slint_backend_testing::ElementHandle> {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label).next()
    }

    /// The poke gate lives in ONE place (`Poke.can`, theme.slint) because
    /// nine sites render a member name and each offers the menu. This pins
    /// what every one of them inherits: off means no menu anywhere, the own
    /// seat is never a target, and an empty name (system lines, tombstone-
    /// free rows) never is either.
    #[test]
    fn the_poke_gate_refuses_the_own_seat_the_empty_name_and_the_off_switch() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let poke = ui.global::<Poke>();
        poke.set_me("walter".into());

        poke.set_on(false);
        assert!(!poke.invoke_can("petra".into()), "off: no menu anywhere");

        poke.set_on(true);
        assert!(poke.invoke_can("petra".into()), "on: another seat pokable");
        assert!(!poke.invoke_can("walter".into()), "never the own seat");
        assert!(!poke.invoke_can("".into()), "no name, no target");
    }

    /// The menus gate on the APPLIED switch, never the settings draft: a
    /// ticked-but-unsaved checkbox would offer a menu the engine refuses.
    #[test]
    fn the_poke_gate_follows_the_applied_setting_not_the_draft() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            settings: molt_core::SessionSettings {
                poke_enabled: true,
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert!(ui.global::<Poke>().get_on(), "applied switch reaches the menus");

        // the draft alone must not move it
        ui.set_cfg_poke_enabled(false);
        assert!(ui.global::<Poke>().get_on(), "the draft does not gate the menu");
    }

    /// The rejoiner's checklist (recovery_auto_approval.md §5): the session's
    /// `RecoverState` becomes per-seat rows plus the have/need counters, and
    /// an empty state clears the rows again.
    #[test]
    fn the_recover_checklist_maps_seats_and_counts() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            recover: molt_core::RecoverState {
                member: "petra".to_string(),
                need: 3,
                seats: vec![
                    molt_core::RecoverSeat { member: "walter".to_string(), approved: true },
                    molt_core::RecoverSeat { member: "petra".to_string(), approved: true },
                    molt_core::RecoverSeat { member: "vera".to_string(), approved: false },
                ],
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (2, 3));
        let rows = ui.get_rv_seats();
        let got: Vec<(String, bool)> = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .map(|r| (r.member.to_string(), r.approved))
            .collect();
        assert_eq!(
            got,
            vec![
                ("walter".to_string(), true),
                ("petra".to_string(), true),
                ("vera".to_string(), false)
            ],
            "roster order, per-seat approval"
        );

        // a fresh recovery clears the list (RecoverStart resets the state)
        apply_session(&ui, &SessionView::default(), true, &chat_ui);
        assert_eq!(ui.get_rv_seats().row_count(), 0);
        assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (0, 0));
    }

    /// Restore-from-backup (recovery_auto_approval.md §7): the Settings ›
    /// Backup modal's state machine — a confirm without a phrase starts
    /// nothing; with one it hands ("s3", the orphan's id, the phrase) to the
    /// real restore pipeline, leads to the Restore screen, and drops the
    /// phrase. Runs on the dev-ui chain (`ElementHandle` needs the
    /// interpreter's debug info).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_backup_restore_modal_drives_the_s3_pipeline() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let calls: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let c = calls.clone();
            ui.on_restore_start(move |way, target, secret| {
                c.borrow_mut().push((way.to_string(), target.to_string(), secret.to_string()));
            });
        }
        let navs: Rc<RefCell<Vec<AppScreen>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let n = navs.clone();
            ui.on_navigate(move |s| n.borrow_mut().push(s));
        }
        let label = ui.global::<Strings>().get_bk_restore().to_string();
        assert!(!label.is_empty(), "the label must be applied before searching");
        ui.show().expect("show headless");
        // control BEFORE the modal: nothing wears the label on this screen
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label.as_str())
                .next()
                .is_none(),
            "no restore affordance before the modal opens"
        );
        // an orphan row opened the modal
        ui.set_bk_restore_id("cafe01".into());
        ui.set_bk_restore_open(true);
        let click = |ui: &AppWindow| {
            let h = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                ui,
                label.as_str(),
            )
            .next()
            .expect("the modal's confirm button renders");
            let at = slint::LogicalPosition::new(
                h.absolute_position().x + h.size().width / 2.0,
                h.absolute_position().y + h.size().height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
        };
        // no phrase yet: the confirm is disarmed
        click(&ui);
        assert!(calls.borrow().is_empty(), "no phrase, no pipeline");
        assert!(ui.get_bk_restore_open(), "the modal stays up");
        // with the phrase: the REAL pipeline is asked, the modal closes,
        // the phrase is dropped, and the run view is next
        ui.set_bk_restore_seed("brave mole over the hills".into());
        click(&ui);
        assert_eq!(
            calls.borrow().as_slice(),
            &[(
                "s3".to_string(),
                "cafe01".to_string(),
                "brave mole over the hills".to_string()
            )],
            "confirm hands way/target/phrase to restore-start"
        );
        assert!(!ui.get_bk_restore_open(), "confirm closes the modal");
        assert_eq!(ui.get_bk_restore_seed().as_str(), "", "every way out drops the phrase");
        assert_eq!(navs.borrow().last(), Some(&AppScreen::Restore), "the run view is next");
    }

    /// One orphan-row session for the backup-table tests (field bug
    /// 2026-08-24): a bucket-only workspace plus one foreign key.
    fn sv_backup_orphan() -> (SessionView, String) {
        let id = "ab".repeat(32);
        let sv = SessionView {
            backup_orphans: vec![
                molt_core::BackupOrphan {
                    id: id.clone(),
                    name: String::new(),
                    size_kib: 480,
                    last_backup_min: 60,
                },
                molt_core::BackupOrphan {
                    id: String::new(),
                    name: "molt/leftover.bin".to_string(),
                    size_kib: 75,
                    last_backup_min: 43_200,
                },
            ],
            // no demo locals: exactly the two bucket rows render
            workspaces: Vec::new(),
            ..SessionView::default()
        };
        (sv, id)
    }

    /// The orphan row's restore affordance lives IN the local column (user
    /// decision 2026-08-24) and must FIT the row — the old trailing button
    /// sat beyond the table's column budget and was clipped invisible on
    /// every build (the "kein Knopf zu sehen" field report), worse under
    /// ui-scale. Measured at a scaled app font, dev-ui chain.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_orphan_restore_button_sits_in_the_local_column_and_fits() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.global::<Theme>().set_fs_app(20.0); // ui-scale ≈ 1.43, the field setup
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let (sv, _id) = sv_backup_orphan();
        apply_session(&ui, &sv, true, &chat_ui);
        ui.set_screen(AppScreen::Settings);
        ui.set_set_tab(2);
        ui.show().expect("show headless");
        let btn = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bkr-btn",
        )
        .next()
        .expect("the orphan row renders its restore button");
        let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bk-row",
        )
        .collect();
        assert_eq!(rows.len(), 2, "orphan + foreign row render");
        let row = &rows[0];
        let btn_right = btn.absolute_position().x + btn.size().width;
        let row_right = row.absolute_position().x + row.size().width;
        assert!(
            btn.size().width > 0.0 && btn_right <= row_right + 0.5,
            "the restore button must fit inside its row: button right {btn_right} vs row right {row_right}"
        );
        // …and the last COLUMN stays inside too (the pre-fix budget ignored
        // the ui-scale of the fixed columns, clipping the row's tail)
        for r in &rows {
            assert!(
                r.absolute_position().x + r.size().width <= 1200.0,
                "a row never overflows the window"
            );
        }
    }

    /// A double-click anywhere on an orphan row arms the same restore modal
    /// as the button (user decision 2026-08-24); a foreign-key row (no
    /// workspace id) stays inert.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_double_click_on_an_orphan_row_arms_the_restore_modal() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let (sv, id) = sv_backup_orphan();
        apply_session(&ui, &sv, true, &chat_ui);
        ui.set_screen(AppScreen::Settings);
        ui.set_set_tab(2);
        ui.show().expect("show headless");
        let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bk-row",
        )
        .collect();
        let dclick = |row: &i_slint_backend_testing::ElementHandle| {
            let at = slint::LogicalPosition::new(
                // between the columns, not on the button
                row.absolute_position().x + row.size().width * 0.6,
                row.absolute_position().y + row.size().height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            for _ in 0..2 {
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: at,
                    button: slint::platform::PointerEventButton::Left,
                });
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: at,
                    button: slint::platform::PointerEventButton::Left,
                });
            }
        };
        // the foreign-key row (sorted last) stays inert
        dclick(&rows[1]);
        assert!(!ui.get_bk_restore_open(), "a foreign key has nothing to restore");
        // the orphan row arms the modal with ITS id
        dclick(&rows[0]);
        assert!(ui.get_bk_restore_open(), "double-click arms the restore modal");
        assert_eq!(ui.get_bk_restore_id().as_str(), id, "the row's own id");
    }

    /// A node with storage, a founded workspace, and chat in it — the state
    /// a user's second launch starts from.
    fn node_with_chat(root: &std::path::Path) -> (WalletHandle, String) {
        // exactly the session `moltd` hands the engine at startup: the
        // workspaces are what is ON DISK. `SessionView::default()` carries
        // the demo fixtures, which would list six republics that do not
        // exist and hide the one that does.
        let session = SessionView {
            workspaces: molt_storage::scan_workspaces(root)
                .iter()
                .map(molt_storage::ScanEntry::info)
                .collect(),
            settings: molt_core::SessionSettings {
                workspace_dir: root.display().to_string(),
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = molt_engine::spawn_with_storage(GroupConfig::demo(), session);
        (w, String::new())
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// How many chat rows the window is showing right now.
    fn chat_rows(ui: &AppWindow) -> usize {
        ui.get_surfaces()
            .iter()
            .find(|s| s.key == "chat")
            .map_or(0, |s| s.log.row_count())
    }

    /// A sealed workspace ON DISK, demo-grade (empty identities and
    /// attestations), plus the unix `now` its appended events should stamp
    /// — NOW, not a fixed stamp: chat older than the retention window is
    /// correctly invisible, and a fixture from last year would "reproduce"
    /// a bug that is the product working as specified.
    fn workspace_on_disk(
        root: &std::path::Path,
        rule_m: u8,
        roster: &[&str],
        agenda: &str,
    ) -> (molt_storage::OpenedWorkspace, u64) {
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        let seed = molt_storage::seed_entropy(&phrase).expect("entropy");
        let sealed = molt_core::SealedRoster {
            name: "DevTest".to_string(),
            republic_id: "d0".repeat(32),
            rule_m,
            rule_n: u8::try_from(roster.len()).expect("roster fits u8"),
            roster: roster.iter().map(|s| (*s).to_string()).collect(),
            identities: Vec::new(),
            attestations: Vec::new(),
            relays: Vec::new(),
            agenda: agenda.to_string(),
            features: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let genesis = sealed.into_genesis(roster[0], now);
        let ws = molt_storage::create_workspace(root, &seed, &genesis).expect("create");
        (ws, now)
    }

    /// The live-mirror's own two steps (session push, then surfaces
    /// gather + apply), in its own order. The apply runs DIRECTLY rather
    /// than through `invoke_from_event_loop`: the headless backend never
    /// drains that queue, and the hop onto the UI thread is Slint's
    /// business, not this layer's.
    async fn mirror(
        w: &WalletHandle,
        ui: &AppWindow,
        last: &Arc<Mutex<Option<SessionSettings>>>,
        chat_ui: &Arc<Mutex<ChatUiState>>,
    ) {
        let weak = ui.as_weak();
        push_session(w, &weak, last, SessionScope::Full, chat_ui).await;
        if let Some((_, b)) = gather_surfaces(w, chat_ui).await {
            apply_surfaces(ui, &b);
        }
    }

    /// **THE reported sequence: a cold start, then OPEN a workspace that is
    /// already on disk.**
    ///
    /// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
    /// ich muss auf organization klicken und wieder zurück" — the switch is
    /// what the second push stands for, and the assertion is BEFORE it.
    #[test]
    fn a_cold_open_of_a_stored_workspace_fills_the_chat_pane() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        // --- a workspace ON DISK, the way a previous run left one behind
        let (mut ws, now) = workspace_on_disk(&root, 1, &["walter"], "test the chat");
        // …with a message in it
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 1,
            seq: 2,
            ts: now,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([7u8; 16]),
                "walter",
                "hello group",
                now,
            )),
        })
        .expect("append");
        ws.sync().expect("sync");
        drop(ws);

        // --- second run: a COLD app, the way the user starts it
        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            // the app comes up on the Choice screen and mirrors once
            mirror(&w, &ui, &last, &chat_ui).await;
            assert_eq!(
                chat_rows(&ui),
                0,
                "nothing is open yet - if this is not empty the test proves nothing"
            );

            // …and then the user opens the workspace
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            let open_id = stored;
            w.execute(Command::OpenWorkspace { id: open_id })
                .await
                .expect("the stored workspace opens");
            // the engine's own answer first: if IT is empty, the fault is
            // not in this layer and the assertion below would blame the
            // wrong one
            let engine_rows = match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                    channel: Some(molt_core::ChannelRef::Group),
                    view: None,
                })
                .await
            {
                Ok(Reply::State(snap)) => snap.applied.len(),
                _ => 0,
            };
            assert_eq!(engine_rows, 1, "the engine holds the stored message");

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            chat_rows(&ui) > 0,
            "opening a stored workspace must fill the chat pane - having to \
             visit another surface and come back IS the bug"
        );
    }

    /// **The reported bug: opening a workspace must fill the chat pane.**
    ///
    /// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
    /// ich muss auf organization klicken und wieder zurück" — so the test
    /// asserts the pane after the OPEN, before any surface switch.
    #[test]
    fn opening_a_workspace_fills_the_chat_pane() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter(); // the engine spawns tasks at construction
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

        rt.block_on(async {
            // found a session-only workspace and say something in it
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            ui.get_surfaces().row_count() > 0,
            "the bundle must have landed at all (else this test proves nothing)"
        );
        assert!(
            chat_rows(&ui) > 0,
            "the chat pane must hold the message the engine has - it took a \
             surface switch to appear, which is the reported bug"
        );
    }

    /// **The reported bug: a member wrote into a fresh topic and the two
    /// RECEIVING clients stopped reacting — "klick auf linke navbar Chat
    /// zeigt keine Funktion".**
    ///
    /// Receiver perspective, headless: the workspace holds a group message
    /// and a FOREIGN member's message in a topic channel (arrived over the
    /// wire, so it is unread here). The mirror must survive that state, the
    /// nav must list the topic row, and the Chat click must keep working.
    #[test]
    fn a_foreign_topic_message_keeps_the_chat_usable() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        let (mut ws, now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the chat");
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 1,
            seq: 2,
            ts: now,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([7u8; 16]),
                "walter",
                "hello group",
                now,
            )),
        })
        .expect("append group message");
        // the foreign topic message, the way the wire landed it
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 2,
            seq: 3,
            ts: now,
            by: "ingrid".to_string(),
            body: molt_core::WorkspaceEvent::Chat(
                molt_core::ChatMessage::text(
                    molt_core::MessageId([9u8; 16]),
                    "ingrid",
                    "topic talk",
                    now,
                )
                .with_channel(ChannelRef::Topic {
                    name: "asdf".to_string(),
                }),
            ),
        })
        .expect("append topic message");
        ws.sync().expect("sync");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            mirror(&w, &ui, &last, &chat_ui).await;

            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            // the mirror push that follows the delivery — the receivers
            // froze HERE if this layer chokes on the topic state
            mirror(&w, &ui, &last, &chat_ui).await;
            assert!(
                chat_rows(&ui) > 0,
                "the group log must still show after a topic message arrived"
            );
            assert!(
                ui.get_chat_channels().iter().any(|c| c.key == "topic:asdf"),
                "the nav must list the foreign topic's row"
            );

            // …and the user's Chat click still navigates
            w.execute(Command::SelectSurface {
                surface: Surface::Chat,
            })
            .await
            .expect("the chat click reaches the engine");
            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            chat_rows(&ui) > 0,
            "after clicking Chat the pane must keep its rows - a dead pane \
             IS the reported bug"
        );
    }

    /// **The set_relays vote card shows the CHANGES** (relay story,
    /// 2026-08-09): a pending pool edit reaches the window as a relay-op
    /// card carrying the diff rows — kept, added, removed — instead of the
    /// generic Ist/Soll text pair.
    #[test]
    fn a_pool_edit_proposal_carries_the_diff_rows() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        // one seat cannot drive an edit to applied at m=2 (the proposer
        // already counts as approver), so the effective pool stays empty
        // here and every proposed relay renders as ADDED — the
        // kept/removed semantics are pinned by `relay_pool_diff`'s unit
        // test against a non-empty Ist-Stand
        let (ws, _now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the pool");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            // the pool edit stays pending at m=2 — the vote card under test
            w.execute(Command::Propose {
                surface: Surface::Organization,
                payload: serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://kept.example wss://new.example",
                }),
            })
            .await
            .expect("the pool edit proposes");

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        let org = ui
            .get_surfaces()
            .iter()
            .find(|s| s.key == "organization")
            .expect("org surface present");
        assert_eq!(org.pending.row_count(), 1, "the pool edit is pending");
        let card = org.pending.row_data(0).expect("card row");
        assert!(card.relay_op, "the card knows it is a pool edit");
        let rows: Vec<(i32, String)> = card
            .relay_changes
            .iter()
            .map(|c| (c.sign, c.url.to_string()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (RELAY_ROW_ADDED, "wss://kept.example".to_string()),
                (RELAY_ROW_ADDED, "wss://new.example".to_string()),
            ],
            "the card carries the pool diff (empty Ist-Stand: all added)"
        );
    }

    /// **The reported bug (2026-08-09): after an approval elsewhere applied
    /// the vote, clicking Chat showed "ein kaputtes Panel mit leerem
    /// 'Proposal:', das die Hälfte der Seite einnimmt".**
    ///
    /// A decided vote's discussion stays a selectable read-only view, but
    /// the decision header's lookup chained only pending + declined — an
    /// APPLIED proposal is in neither list, so the card above the chat
    /// rendered from `ProposalRow::default()`: the empty wreck. The header
    /// must carry the decided card.
    #[test]
    fn a_decided_votes_discussion_keeps_its_decision_card() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();
        let (ws, _now) = workspace_on_disk(&root, 1, &["walter"], "test the header");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            mirror(&w, &ui, &last, &chat_ui).await;
            // the vote APPLIES instantly at m=1 — the state right after
            // the approval sound on the reporting client
            w.execute(Command::Propose {
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_name", "value": "NewName" }),
            })
            .await
            .expect("the vote proposes and applies");
            // …and the user opens the decision's discussion
            chat_ui
                .lock()
                .expect("ui state")
                .select(ChannelRef::Patch {
                    id: molt_core::ProposalId(1),
                });
            mirror(&w, &ui, &last, &chat_ui).await;
        });

        let card = ui.get_selected_decision();
        assert!(
            card.id == 1 && !card.text.is_empty(),
            "a decided vote's discussion must head with ITS card, never an \
             empty one (id={}, text={:?})",
            card.id,
            card.text
        );
    }

    // ---- wiki export (docs_archive/memory/wiki_export_plan.md, keystone 5) -------

    /// The 💾 button writes the APPROVED tree, so the gate is the folded
    /// base - never the local stack, which the export deliberately leaves
    /// behind. One place decides it (`WikiState.has-base`), because the
    /// toolbar button and the dialog must never disagree.
    #[test]
    fn the_wiki_export_button_follows_the_approved_base_tree() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let g = ui.global::<WikiState>();

        g.set_base_docs(ModelRc::new(VecModel::from(Vec::<WikiBase>::new())));
        assert!(!g.invoke_has_base(), "an empty base has nothing to export");

        // a local draft alone must NOT arm the button: drafts stay local
        g.set_cs_rows(ModelRc::new(VecModel::from(vec![WikiChangeRow {
            kind: 0,
            label: "notes.md".into(),
        }])));
        assert!(!g.invoke_has_base(), "a local draft is not an approved tree");

        g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
            path: "charter.md".into(),
            content: "hello".into(),
        }])));
        assert!(g.invoke_has_base(), "one approved doc arms the export");
    }

    /// The dialog's drafts line appears only when there IS a local stack -
    /// telling a user with nothing pending that nothing pending stays local
    /// is noise.
    #[test]
    fn the_export_dialog_counts_only_a_non_empty_local_stack() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let g = ui.global::<WikiState>();

        g.set_cs_rows(ModelRc::new(VecModel::from(Vec::<WikiChangeRow>::new())));
        assert_eq!(g.invoke_draft_count(), 0, "no stack, no line");

        g.set_cs_rows(ModelRc::new(VecModel::from(vec![
            WikiChangeRow {
                kind: 0,
                label: "a.md".into(),
            },
            WikiChangeRow {
                kind: 5,
                label: "b.md".into(),
            },
        ])));
        assert_eq!(g.invoke_draft_count(), 2, "the line names the real count");
    }

    /// The outcome toast is built from the engine's own export state, in
    /// both languages, and stays silent while the export is idle or still
    /// running (a toast per session push would repeat forever).
    #[test]
    fn the_wiki_export_toast_carries_the_real_outcome() {
        let idle = molt_core::ExportState::default();
        assert!(
            super::wiki_export_toast(0, &idle).is_none(),
            "nothing happened yet"
        );

        let running = molt_core::ExportState {
            running: true,
            dest: "/tmp/x".to_string(),
            ..molt_core::ExportState::default()
        };
        assert!(
            super::wiki_export_toast(0, &running).is_none(),
            "no verdict while it runs"
        );

        let ok = molt_core::ExportState {
            result: "ok".to_string(),
            files: 12,
            ..molt_core::ExportState::default()
        };
        let (msg, failed) = super::wiki_export_toast(0, &ok).expect("a verdict");
        assert!(!failed);
        assert_eq!(msg, "wiki exported: 12 files");
        let (de, _) = super::wiki_export_toast(1, &ok).expect("a verdict");
        assert_eq!(de, "Wiki exportiert: 12 Dateien");

        // the singular is not "1 files"
        let one = molt_core::ExportState {
            result: "ok".to_string(),
            files: 1,
            ..molt_core::ExportState::default()
        };
        assert_eq!(
            super::wiki_export_toast(0, &one).expect("a verdict").0,
            "wiki exported: 1 file"
        );
        assert_eq!(
            super::wiki_export_toast(1, &one).expect("a verdict").0,
            "Wiki exportiert: 1 Datei"
        );

        // a failure is surfaced verbatim, in the error tone
        let bad = molt_core::ExportState {
            result: "error: dest is not a directory".to_string(),
            ..molt_core::ExportState::default()
        };
        let (msg, failed) = super::wiki_export_toast(0, &bad).expect("a verdict");
        assert!(failed, "a failure toasts in the error tone");
        assert!(
            msg.contains("dest is not a directory"),
            "the real reason survives: {msg}"
        );
    }

    /// The same outcome must toast ONCE: `apply_session` runs on every
    /// engine change, and a settled export state stays settled.
    #[test]
    fn a_settled_wiki_export_toasts_once_not_on_every_push() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            wiki_export: molt_core::ExportState {
                result: "ok".to_string(),
                dest: "/tmp/out".to_string(),
                files: 3,
                bytes: 90,
                ..molt_core::ExportState::default()
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!(ui.get_toast_text().as_str(), "wiki exported: 3 files");

        // a second, unchanged push must not speak again
        ui.invoke_show_toast("something else".into());
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!(
            ui.get_toast_text().as_str(),
            "something else",
            "an unchanged export state re-toasted"
        );
    }

    /// **The dialog's Confirm reaches the engine with what the user picked.**
    /// Both halves are pinned: the destination (the tree lands exactly
    /// there) and the proof flag (this workspace has no chain, so a
    /// `proof: true` export is REFUSED - if the flag were dropped the very
    /// same call would write a tree).
    #[test]
    fn the_export_dialog_issues_the_command_with_the_picked_path_and_the_proof_flag() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();

        // a single-operator group: propose + one approval applies the patch
        let w = molt_engine::spawn(
            GroupConfig {
                member: "me".to_string(),
                members: vec!["me".to_string()],
                threshold: 1,
                self_cosign: false,
            },
            SessionView::default(),
        );
        rt.block_on(async {
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: serde_json::json!({
                        "op": "wiki_patch",
                        "summary": "a.md",
                        "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hello\n",
                    }),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
        });

        let ui = AppWindow::new().expect("headless window");
        wire_wiki_export(&ui, rt.handle(), &w);

        // --- the proof flag: no chain here, so the engine must refuse
        let refused = tmp.path().join("refused");
        ui.invoke_wiki_export(refused.display().to_string().into(), true);

        // --- the destination: the same call without the bundle writes
        let dest = tmp.path().join("out");
        ui.invoke_wiki_export(dest.display().to_string().into(), false);
        rt.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let Ok(Reply::Session(s)) = w.execute(Command::ReadSession).await else {
                    panic!("read session");
                };
                if !s.wiki_export.running && !s.wiki_export.result.is_empty() {
                    assert_eq!(s.wiki_export.result, "ok", "the export failed: {s:?}");
                    assert_eq!(s.wiki_export.dest, dest.display().to_string());
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the export never settled: {:?}",
                    s.wiki_export
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        assert_eq!(
            std::fs::read_to_string(dest.join("wiki/a.md")).expect("the exported doc"),
            "hello\n"
        );
        assert!(
            !dest.join("proof").exists(),
            "proof: false must write no bundle"
        );
        // the refused call ran first and left nothing behind
        assert!(
            !refused.exists(),
            "a proof export without a chain must be refused, not written"
        );
    }

    /// i18n: every wiki-export string carries a real English AND a real
    /// German arm (an empty or identical pair is a missing translation),
    /// and none of them smuggles in an em dash.
    #[test]
    fn every_wiki_export_string_reads_in_both_languages() {
        let en = Lexicon::en();
        let de = Lexicon::de();
        let pairs = [
            ("mem_tb_export", en.mem_tb_export, de.mem_tb_export),
            ("mem_ex_title", en.mem_ex_title, de.mem_ex_title),
            ("mem_ex_body", en.mem_ex_body, de.mem_ex_body),
            ("mem_ex_confirm", en.mem_ex_confirm, de.mem_ex_confirm),
            ("mem_ex_proof", en.mem_ex_proof, de.mem_ex_proof),
            ("mem_ex_reveals", en.mem_ex_reveals, de.mem_ex_reveals),
            ("mem_ex_drafts", en.mem_ex_drafts, de.mem_ex_drafts),
            ("mem_ex_done", en.mem_ex_done, de.mem_ex_done),
            ("mem_ex_file", en.mem_ex_file, de.mem_ex_file),
            ("mem_ex_files", en.mem_ex_files, de.mem_ex_files),
            ("mem_ex_failed", en.mem_ex_failed, de.mem_ex_failed),
        ];
        for (key, e, d) in pairs {
            assert!(!e.is_empty() && !d.is_empty(), "{key}: an empty arm");
            assert_ne!(e, d, "{key}: untranslated");
            assert!(!e.contains('—') && !d.contains('—'), "{key}: em dash");
        }
        // the disclosure names what the bundle actually reveals
        for l in [en, de] {
            let line = l.mem_ex_reveals.to_lowercase();
            for token in ["relay", "chart"] {
                assert!(
                    line.contains(token),
                    "the disclosure drops {token}: {}",
                    l.mem_ex_reveals
                );
            }
        }
    }

    /// The engine's export refusals reach the user in German too - the
    /// `localize_error` match carries no wildcard, so a new phrase is a
    /// compile-time reminder, but a phrase without an arm would silently
    /// stay English.
    #[test]
    fn the_wiki_export_refusals_render_in_german() {
        for phrase in [
            "a target directory is required",
            "an export is already running",
            "the wiki is empty",
            "proof needs chain governance",
            "proof needs the genesis block",
        ] {
            let e = molt_core::MoltError::WikiExport(phrase);
            let de = super::localize_error(1, &e);
            assert!(de.starts_with("Wiki-Export: "), "{de}");
            assert!(!de.contains(phrase), "phrase without a German arm: {de}");
        }
    }
}
