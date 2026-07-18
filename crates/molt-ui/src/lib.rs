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
//! workspace lifecycles are still a **simulation** — no workspace is created
//! on disk yet.
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

use molt_core::{
    ChannelInfo, ChannelRef, ChatMessage, Command, Event, MessageId, NetHealth, ProposalId,
    ProposalView, Reply, Screen, SessionScope, SessionSettings, SessionView, Surface,
    SurfaceSnapshot,
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

    ui.on_parse_invite(|s| match molt_core::InviteInfo::parse(&s) {
        Some(i) => InvitePreview {
            valid: true,
            republic: i.republic.as_str().into(),
            rule: format!("{}-of-{}", i.threshold, i.members).into(),
            inviter: i.inviter.as_str().into(),
        },
        None => InvitePreview::default(),
    });

    // NOTE: the old duplicate-name check is gone by design — display names
    // may repeat, the workspace id disambiguates (the same DAO opened twice
    // locally is a supported setup).

    // The previously applied session settings: the mirror uses it to refresh
    // the settings draft only on real changes, the leave-guard to detect a
    // dirty draft.
    let last_settings: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    // Chat-bus UI state (selected channel, unread ledger, proposal
    // first-seen times) — UI-local by design, see [`ChatUiState`].
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));

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
        ui.on_save_settings(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui);
            issue(&rt, &w, &ui.as_weak(), Command::SaveSettings { settings });
        });
    }
    {
        // Rotate the MCP token: mint a fresh one, drop it into the draft, and
        // persist the settings in one go (Slint cannot generate randomness).
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_rotate_token(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_cfg_mcp_token(molt_config::random_token().into());
            let settings = read_settings_draft(&ui);
            issue(&rt, &w, &ui.as_weak(), Command::SaveSettings { settings });
        });
    }
    {
        // Test the SMP server currently in the draft (not the saved one), so
        // the user can validate a custom URL before saving. Public mode tests
        // the bundled default. The result streams back into `cfg-smp-test`.
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_test_smp_server(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let url = if ui.get_cfg_smp_custom() {
                ui.get_cfg_smp_url().to_string()
            } else {
                molt_config::default_public_smp()
            };
            issue(&rt, &w, &ui.as_weak(), Command::NetTestServer { url });
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
                .is_some_and(|s| s != read_settings_draft(&ui));
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
        ui.on_save_and_leave(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui);
            let screen = to_screen(ui.get_settings_return());
            let w = w.clone();
            let weak = ui.as_weak();
            rt.spawn(async move {
                match w.execute(Command::SaveSettings { settings }).await {
                    Ok(_) => {
                        let _ = w.execute(Command::Navigate { screen }).await;
                    }
                    Err(e) => {
                        let msg = format!("⚠ {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_show_toast(msg.into());
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
    {
        let weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_screen() == AppScreen::Main || ui.get_run_active() {
                    ui.set_confirm_quit_open(true);
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
    // the (mock) at-rest encryption toggle — same commands as the MCP
    // encrypt_/decrypt_workspace tools
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_encrypt_workspace(move |id| {
            issue(
                &rt,
                &w,
                &weak,
                Command::EncryptWorkspace { id: id.to_string() },
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
        ui.on_restore_start(move |way, target| {
            issue(
                &rt,
                &w,
                &weak,
                Command::RestoreStart {
                    way: way.to_string(),
                    target: target.to_string(),
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
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_create_start(move |name, member, threshold, members, net| {
            issue(
                &rt,
                &w,
                &weak,
                Command::CreateStart {
                    name: name.to_string(),
                    member: member.to_string(),
                    threshold: u8::try_from(threshold).unwrap_or(0),
                    members: u8::try_from(members).unwrap_or(0),
                    net: net.to_string(),
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
        ui.on_create_propose(move |name, agenda| {
            issue(
                &rt,
                &w,
                &weak,
                Command::CreatePropose {
                    name: name.to_string(),
                    agenda: agenda.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_start(move |invite, member| {
            issue(
                &rt,
                &w,
                &weak,
                Command::JoinStart {
                    invite: invite.to_string(),
                    member: member.to_string(),
                },
            );
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_join_cancel(move || {
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
                        ui.invoke_show_toast(format!("⚠ {e}").into());
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
                let closed = chat_ui
                    .lock()
                    .map(|st| selected_channel_closed(&ch, &[], &st.proposals))
                    .unwrap_or(false);
                ui.set_selected_channel_closed(closed);
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
                    let msg = format!("⚠ {e}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast(msg.into());
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
                    let msg = format!("⚠ {e}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast(msg.into());
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
            issue(&rt, &w, &weak, Command::RemoveFile { id });
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
                        other => {
                            let msg = format!("\u{26a0} {path}: {other:?}");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.invoke_show_toast(msg.into());
                                }
                            });
                            return;
                        }
                    };
                    if let Err(e) = w
                        .execute(Command::Propose {
                            surface: Surface::Organization,
                            payload,
                        })
                        .await
                    {
                        let msg = format!("\u{26a0} {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_show_toast(msg.into());
                            }
                        });
                    }
                });
                return;
            }
            let payload = serde_json::json!({
                "op": op.as_str(),
                "value": value.as_str(),
            });
            issue(
                &rt,
                &w,
                &weak,
                Command::Propose {
                    surface: Surface::Organization,
                    payload,
                },
            );
        });
    }
    // sound preview in the settings panel — plays the picked alert once
    {
        ui.on_test_sound(move |kind| {
            play_alert(kind.as_str());
        });
    }
    // WP4b: the checkpoint verb — same Command the MCP tool drives
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_propose_checkpoint(move || {
            issue(&rt, &w, &weak, Command::ProposeCheckpoint);
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
                    .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp"]);
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
    // the proposed image behind a pending set_image: the bytes RODE the
    // proposal payload (sign-what-you-see), so the preview decodes them
    // locally on every member's device — no transfer, no proposer needed
    {
        let weak = ui.as_weak();
        ui.on_download_proposal_image(move |img_b64, title| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let decoded = proposal_image_from_b64(img_b64.as_str());
            match decoded {
                Some(img) => {
                    ui.set_img_preview_title(title);
                    ui.set_img_preview_src(img);
                    ui.set_img_preview_open(true);
                }
                None => {
                    let s = ui.global::<Strings>();
                    ui.invoke_show_toast(s.get_pc_img_missing());
                }
            }
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

    // --- live-mirror: re-read and re-render on every engine change ---
    {
        let weak = ui.as_weak();
        let w = wallet.clone();
        let last_settings = last_settings.clone();
        let chat_ui = chat_ui.clone();
        rt.spawn(async move {
            let mut rx = w.subscribe();
            push_session(&w, &weak, &last_settings, SessionScope::Full).await;
            push_surfaces(&w, &weak, &chat_ui).await;
            loop {
                match rx.recv().await {
                    Ok(Event::SessionChanged { scope }) => {
                        push_session(&w, &weak, &last_settings, scope).await;
                        // A Full session change can mean a workspace was
                        // opened or closed — the surface state (replayed
                        // chat history!) changed with it, without any
                        // chat/proposal event firing. Run-scoped ticks
                        // (90 ms) deliberately skip this.
                        if scope == SessionScope::Full {
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
                        let sound = last_settings
                            .lock()
                            .ok()
                            .and_then(|s| s.as_ref().map(|s| s.sound_message.clone()))
                            .unwrap_or_default();
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            if ui.get_node_member() != from.as_str() {
                                play_alert(&sound);
                            }
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(Event::Proposed { .. }) => {
                        // the engine emits Proposed for our own proposals
                        // too — a beep on one's own act is acceptable
                        // feedback (and the event carries no author)
                        let sound = last_settings
                            .lock()
                            .ok()
                            .and_then(|s| s.as_ref().map(|s| s.sound_vote.clone()))
                            .unwrap_or_default();
                        play_alert(&sound);
                        push_surfaces(&w, &weak, &chat_ui).await;
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
                    Ok(Event::CheckpointStale { .. }) => {
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let msg = ui.global::<Strings>().get_toast_checkpoint_stale();
                            ui.invoke_show_toast(msg);
                        });
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
                                    ui.invoke_show_toast(
                                        format!("{} {reason}", st.get_toast_dl_failed()).into(),
                                    );
                                }
                                _ => {}
                            }
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(_) => push_surfaces(&w, &weak, &chat_ui).await,
                    Err(RecvError::Lagged(_)) => {
                        push_session(&w, &weak, &last_settings, SessionScope::Full).await;
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    ui.run()
}

/// Update a model's rows IN PLACE instead of replacing the ModelRc: the
/// repeater keeps its element instances, and with them focus, scroll and
/// selection state (replacing the model recreates everything — that is how
/// the chat compose box once lost its focus mid-typing).
fn sync_rows<T: Clone + 'static>(
    current: &ModelRc<T>,
    items: Vec<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    let Some(m) = current.as_any().downcast_ref::<VecModel<T>>() else {
        set(ModelRc::new(VecModel::from(items)));
        return;
    };
    while m.row_count() > items.len() {
        m.remove(m.row_count() - 1);
    }
    for (i, item) in items.into_iter().enumerate() {
        if i < m.row_count() {
            m.set_row_data(i, item);
        } else {
            m.push(item);
        }
    }
}

/// Rebuild a `[string]` mirror in place.
fn sync_strings(
    current: &ModelRc<slint::SharedString>,
    items: &[String],
    set: impl FnOnce(ModelRc<slint::SharedString>),
) {
    sync_rows(
        current,
        items.iter().map(|l| l.as_str().into()).collect(),
        set,
    );
}

/// Decode a pending `set_image` proposal's payload (base64 of the raw image
/// file) into a renderable [`slint::Image`]. The bytes rode the proposal
/// gossip (sign-what-you-see), so this runs locally on every member's
/// device — no transfer, no proposer needed. `None` on any decode failure.
fn proposal_image_from_b64(img_b64: &str) -> Option<slint::Image> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(img_b64)
        .ok()?;
    image_from_bytes(&bytes)
}

/// Decode raw image-file bytes into a [`slint::Image`], keyed on the
/// CONTENT — a payload carries no file name, so an extension-keyed loader
/// (`Image::load_from_path`, `image::open`) can never work here. Raster
/// formats are sniffed and decoded in memory (exactly the picker's set:
/// png/jpeg/webp/gif/bmp — pure-Rust decoders); an unsniffable payload
/// gets one try as SVG source. Untrusted peer input: decode dimensions are
/// capped so a tiny compressed bomb cannot balloon in memory.
fn image_from_bytes(bytes: &[u8]) -> Option<slint::Image> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if reader.format().is_some() {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        reader.limits(limits);
        let rgba = reader.decode().ok()?.into_rgba8();
        let (w, h) = rgba.dimensions();
        let buf =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
        return Some(slint::Image::from_rgba8(buf));
    }
    // not a known raster signature — the one picker format without a magic
    // number is SVG (plain text): let the vector loader have a try
    slint::Image::load_from_svg_data(bytes).ok()
}

/// Map a session workspace into the Slint-side row struct.
fn workspace_item(lang: i32, w: &molt_core::WorkspaceInfo) -> WorkspaceItem {
    let members: Vec<MemberSync> = w
        .members
        .iter()
        .map(|m| MemberSync {
            name: m.name.as_str().into(),
            last: m.last.as_str().into(),
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

/// A human "x ago" label from minutes (0 = English, 1 = German — these
/// labels are composed per row in Rust, so like [`seat_state_label`] they
/// take the language instead of going through the Slint `Strings` global).
fn ago_label(lang: i32, minutes: u32) -> String {
    if lang == 1 {
        match minutes {
            0 => "gerade eben".to_string(),
            m if m < 60 => format!("vor {m} Min."),
            m if m < 1440 => format!("vor {} Std.", m / 60),
            m if m < 2880 => "vor 1 Tag".to_string(),
            m => format!("vor {} Tagen", m / 1440),
        }
    } else {
        match minutes {
            0 => "just now".to_string(),
            m if m < 60 => format!("{m} min ago"),
            m if m < 1440 => format!("{} h ago", m / 60),
            m => format!("{} d ago", m / 1440),
        }
    }
}

/// Render the human sync-status line from the machine fields — prose is
/// presentation, so it lives here and not in the shared data.
fn sync_status_label(lang: i32, state: u8, last_sync_min: u32, sync_queue: u32) -> String {
    match (lang, state) {
        (1, 1) => format!("Synchronisiere… {sync_queue} ausstehend"),
        (1, 2) => format!("Offline · letzter Sync {}", ago_label(lang, last_sync_min)),
        (1, _) => format!("Synchronisiert · {}", ago_label(lang, last_sync_min)),
        (_, 1) => format!("Syncing… {sync_queue} items left"),
        (_, 2) => format!("Offline · last sync {}", ago_label(lang, last_sync_min)),
        (_, _) => format!("Synced · {}", ago_label(lang, last_sync_min)),
    }
}

/// Human size for a KiB count, e.g. `"920 KiB"` / `"1.8 MiB"`.
fn size_label(size_kib: u32) -> String {
    if size_kib >= 1024 {
        format!("{:.1} MiB", f64::from(size_kib) / 1024.0)
    } else {
        format!("{size_kib} KiB")
    }
}

/// Human "last backup" cell ([`molt_core::WorkspaceInfo::NEVER`] = never).
fn backup_when_label(lang: i32, minutes: u32) -> String {
    if minutes == molt_core::WorkspaceInfo::NEVER {
        if lang == 1 { "nie" } else { "never" }.to_string()
    } else {
        ago_label(lang, minutes)
    }
}

/// The settings backup table: every local workspace mapped to its bucket
/// backup (if auto-backup is on), then the bucket-only orphans.
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
            remote: if w.s3 { w.name.as_str() } else { "" }.into(),
            has_local: true,
            auto: w.s3,
            size: size_label(w.size_kib).into(),
            last: backup_when_label(lang, w.last_backup_min).into(),
            size_kib: i32::try_from(w.size_kib).unwrap_or(i32::MAX),
            last_min: last_key(w.last_backup_min),
        })
        .collect();
    rows.extend(sv.backup_orphans.iter().map(|o| BackupRow {
        id: "".into(),
        local: "".into(),
        remote: o.name.as_str().into(),
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

/// Rows per page of the proposal-outcome lists (Organization → Declined
/// and the gated surfaces' applied log). Below this the pager row hides.
const LIST_PAGE_SIZE: usize = 20;

/// The pure paging window: `(start, end, page, page_count)` over a list of
/// `len` rows, `size` per page. The requested 0-based `page` clamps into
/// range (a shrunk list re-bases onto its last page instead of showing an
/// empty one), and an empty list is one empty page — `page_count` is never
/// zero, so "page x of y" stays well-formed.
fn page_slice(len: usize, page: usize, size: usize) -> (usize, usize, usize, usize) {
    let page_count = len.div_ceil(size).max(1);
    let page = page.min(page_count - 1);
    let start = page * size;
    let end = (start + size).min(len);
    (start, end, page, page_count)
}

/// The bundle's effective page for one paged list (missing key = page 0).
fn page_of(pages: &HashMap<String, usize>, surface: &str, list: &str) -> usize {
    pages.get(&format!("{surface}:{list}")).copied().unwrap_or(0)
}

/// Toggle-or-switch a table's sort state: clicking the active column again
/// flips the direction, a new column starts ascending.
fn toggle_sort(active: &mut String, ascending: &mut bool, column: &str) {
    if active == column {
        *ascending = !*ascending;
    } else {
        *active = column.to_string();
        *ascending = true;
    }
}

/// Sort the Organization → Uploads rows by a header column key ("user" /
/// "date" / "file" / "type" / "size" / "checksum" / "download" /
/// "expires"); an empty or unknown key keeps the engine order. Text
/// columns compare case-insensitively; date/size/expiry sort by the
/// underlying numeric keys carried on the row — never the rendered label.
fn sort_uploads(rows: &mut [UploadRowData], column: &str, ascending: bool) {
    match column {
        "user" => rows.sort_by_key(|r| r.user.to_lowercase()),
        "date" => rows.sort_by_key(|r| r.ts),
        "file" => rows.sort_by_key(|r| r.name.to_lowercase()),
        "type" => rows.sort_by_key(|r| r.kind.to_lowercase()),
        "size" => rows.sort_by_key(|r| r.bytes),
        "checksum" => rows.sort_by_key(|r| r.checksum_full.to_lowercase()),
        "download" => rows.sort_by_key(|r| r.status.to_lowercase()),
        "expires" => rows.sort_by_key(|r| r.expires_ts),
        _ => return,
    }
    if !ascending {
        rows.reverse();
    }
}

/// Keep the uploads rows whose user, filename or checksum contains
/// `needle` case-insensitively; an empty needle keeps every row. The
/// checksum matches on the full sha256 hex, so a pasted full checksum
/// finds its row even though the cell shows a shortened prefix.
fn filter_uploads(rows: Vec<UploadRowData>, needle: &str) -> Vec<UploadRowData> {
    if needle.is_empty() {
        return rows;
    }
    let needle = needle.to_lowercase();
    rows.into_iter()
        .filter(|r| {
            r.user.to_lowercase().contains(&needle)
                || r.name.to_lowercase().contains(&needle)
                || r.checksum_full.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Sort the Organization → Members rows by a header column key ("name" /
/// "id" / "pk" / "last" / "uploads"); an empty or unknown key keeps the
/// roster order. Unanchored (empty) id/pk cells sort last ascending; no
/// real last-seen timestamp exists yet (mock presence), so "last" orders
/// by the presence state first, then the label.
fn sort_members(rows: &mut [MemberRowData], column: &str, ascending: bool) {
    match column {
        "name" => rows.sort_by_key(|r| r.name.to_lowercase()),
        "id" => rows.sort_by_key(|r| (r.id.is_empty(), r.id.to_lowercase())),
        "pk" => rows.sort_by_key(|r| (r.pk.is_empty(), r.pk.to_lowercase())),
        "last" => rows.sort_by_key(|r| (r.state, r.last.to_lowercase())),
        "uploads" => rows.sort_by_key(|r| r.uploads),
        _ => return,
    }
    if !ascending {
        rows.reverse();
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
    /// Coordinator: a link mint started for this member — the dialog opens in
    /// its calm pending state until the outcome notice replaces it.
    LinkPending(String),
    /// Coordinator: the engine minted a single-use `molt://recover/…` link.
    Link(String),
    /// Coordinator: the mint failed for an operational reason of THIS node
    /// (`mesh-not-running`, or a transport failure) — the returning member's
    /// presence is never involved. Rendered as the calm failed state of the
    /// same link dialog, not as an error toast.
    LinkFailed(String),
    /// Rejoiner: the engine accepted link + phrase; the rejoin runs off the
    /// actor (it can span the survivors' human approval).
    Started(String),
    /// Rejoiner: the rejoin failed with this reason (retry = a fresh start,
    /// usually with a freshly minted link).
    Failed(String),
    /// Rejoiner: the seat is recovered — the engine flips to Main itself.
    Done(String),
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
    } else if let Some(error) = notice.strip_prefix("recover-failed:") {
        RecoverNotice::Failed(error.to_string())
    } else if let Some(member) = notice.strip_prefix("recovered:") {
        RecoverNotice::Done(member.to_string())
    } else {
        RecoverNotice::None
    }
}

/// Gather the config-tab draft properties into a [`SessionSettings`].
fn read_settings_draft(ui: &AppWindow) -> SessionSettings {
    SessionSettings {
        headless: ui.get_cfg_headless(),
        workspace_dir: ui.get_cfg_workspace_dir().to_string(),
        download_dir: ui.get_cfg_download_dir().to_string(),
        sound_message: sound_name(ui.get_cfg_sound_message_index()),
        sound_vote: sound_name(ui.get_cfg_sound_vote_index()),
        s3_backup: ui.get_cfg_s3_backup(),
        s3_endpoint: ui.get_cfg_s3_endpoint().to_string(),
        s3_access_key: ui.get_cfg_s3_access().to_string(),
        s3_secret_key: ui.get_cfg_s3_secret().to_string(),
        s3_bucket: ui.get_cfg_s3_bucket().to_string(),
        s3_interval_min: ui.get_cfg_s3_interval() as u16,
        s3_keep_copies: ui.get_cfg_s3_copies() as u16,
        mcp_port: ui.get_cfg_mcp_port() as u16,
        mcp_allow: ui.get_cfg_mcp_allow().to_string(),
        mcp_token: ui.get_cfg_mcp_token().to_string(),
        anonymity: net_name(ui.get_cfg_network_index()),
        tor_mode: mode_name(ui.get_cfg_tor_mode_index()),
        tor_port: ui.get_cfg_tor_port() as u16,
        smp_server: if ui.get_cfg_smp_custom() { "custom" } else { "public" }.to_string(),
        smp_url: ui.get_cfg_smp_url().to_string(),
    }
}

/// Fire a command on the shared handle; the resulting event drives the
/// live-mirror, so callers do not await a reply — but an engine error is
/// surfaced as a toast instead of vanishing silently.
fn issue(rt: &Handle, wallet: &WalletHandle, weak: &slint::Weak<AppWindow>, cmd: Command) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
        if let Err(e) = w.execute(cmd).await {
            let msg = format!("⚠ {e}");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_show_toast(msg.into());
                }
            });
        }
    });
}

/// Read the shared session and push it into the Slint properties on the UI
/// thread. `last_settings` remembers the previously applied settings so the
/// draft form is only refreshed when they really changed.
async fn push_session(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    last_settings: &Arc<Mutex<Option<SessionSettings>>>,
    scope: SessionScope,
) {
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
    let (changed, prev) = {
        let mut last = last_settings.lock().expect("settings cache poisoned");
        let prev = last.clone();
        let changed = prev.as_ref() != Some(&sv.settings);
        if changed {
            *last = Some(sv.settings.clone());
        }
        (changed, prev)
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            // Draft protection: a settings change arriving while the user has
            // unsaved edits open (an external config reload, an MCP save) must
            // not wipe what they typed. The notice/leave-guard tells them; the
            // draft stays until they Save or discard.
            let editing = ui.get_screen() == AppScreen::Settings
                && prev.is_some_and(|p| p != read_settings_draft(&ui));
            apply_session(&ui, &sv, changed && !editing);
        }
    });
}

/// Render one session snapshot into the window: screen, language (and strings),
/// the transient notice, the last-wizard outcome, and the settings draft. The
/// draft fields are only overwritten when the session's settings actually
/// changed — otherwise an unrelated change (language, theme, navigation)
/// would wipe what the user is typing in the settings form.
fn apply_session(ui: &AppWindow, sv: &SessionView, settings_changed: bool) {
    let lang = i32::from(sv.language == "de");
    ui.set_screen(from_screen(sv.screen));
    ui.set_selected_surface(sv.surface.as_str().into());
    ui.set_selected_view(sv.view.as_str().into());

    // the Open screen's list mirrors the session's workspaces, re-applying
    // whatever column sort the user picked
    let mut items: Vec<WorkspaceItem> = sv
        .workspaces
        .iter()
        .map(|w| workspace_item(lang, w))
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
                    last: m.last.as_str().into(),
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

    apply_runs(ui, sv);
    ui.global::<Theme>().set_theme_index(theme_index(&sv.theme));
    ui.set_lang_index(lang);
    ui.set_notice(sv.notice.clone().into());
    // a failed write carries its detail in the notice; split it off so the
    // settings footer can render it in the error tone without string ops
    ui.set_notice_failed(
        if sv.notice.starts_with("save-failed") {
            sv.notice.clone()
        } else {
            String::new()
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
            }
            RecoverNotice::Failed(error) => {
                ui.set_rv_running(false);
                ui.set_rv_error(error.into());
            }
            RecoverNotice::Done(_) => {
                // the engine flips to Main itself — just clear the peer-way
                // state so a later return to the Restore screen starts clean
                ui.set_rv_running(false);
                ui.set_rv_error("".into());
            }
            RecoverNotice::None => {}
        }
    }
    // persistent restart warning: which changed keys only apply on restart
    ui.set_restart_keys(sv.restart_required.join(", ").into());
    // the SMP connection-test status is transient and lives outside the
    // settings draft, so push it on every update — even while the user has
    // an unsaved URL open and `settings_changed` is suppressed
    ui.set_cfg_smp_test(sv.smp_test.clone().into());

    // transport health for the header "chat" pill: tone (green/amber/red) plus
    // the engine's reason string as the hover tooltip (P6). Pushed on every
    // update so a dial outcome repaints the pill regardless of settings edits.
    let (net_tone, net_reason) = net_health_pill(&sv.net_health);
    ui.set_net_health_tone(net_tone);
    ui.set_net_health_reason(net_reason.into());

    if !settings_changed {
        apply_strings(ui, lang);
        return;
    }
    apply_settings_fields(ui, &sv.settings);

    apply_strings(ui, lang);
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
    ui.set_cfg_mcp_port(s.mcp_port as i32);
    ui.set_cfg_mcp_allow(s.mcp_allow.clone().into());
    ui.set_cfg_mcp_token(s.mcp_token.clone().into());
    ui.set_cfg_sound_message_index(sound_index(&s.sound_message));
    ui.set_cfg_sound_vote_index(sound_index(&s.sound_vote));
    ui.set_cfg_network_index(net_index(&s.anonymity));
    ui.set_cfg_tor_mode_index(mode_index(&s.tor_mode));
    ui.set_cfg_tor_port(s.tor_port as i32);
    ui.set_cfg_smp_custom(s.smp_server == "custom");
    ui.set_cfg_smp_url(s.smp_url.clone().into());
}

/// Mirror the three engine-run lifecycles (the engine ticks them at 90 ms;
/// a `SessionChanged` with a run scope re-renders ONLY this, so the rest of
/// the window keeps its focus/scroll state untouched).
/// "Founder" label per language.
fn strings_founder(lang: i32) -> &'static str {
    if lang == 1 {
        "Gründer · versiegelt"
    } else {
        "Founder · sealed"
    }
}

/// A ritual seat's status line once the member activated (state 1/2).
fn seat_state_label(lang: i32, state: u8) -> String {
    match (lang, state) {
        (1, 3) => "hat die Satzung abgelehnt",
        (1, 2) => "versiegelt",
        (1, _) => "Schlüssel erhalten · signiert…",
        (_, 3) => "declined the charter",
        (_, 2) => "sealed",
        (_, _) => "key received · signing…",
    }
    .to_string()
}

fn apply_runs(ui: &AppWindow, sv: &SessionView) {
    let lang = i32::from(sv.language == "de");
    // restore
    ui.set_rw_step(i32::from(sv.restore.run.step));
    ui.set_rw_way(sv.restore.way.clone().into());
    ui.set_rw_target(sv.restore.target.clone().into());
    ui.set_rw_progress(f32::from(sv.restore.run.progress_pct) / 100.0);
    ui.set_rw_outcome(i32::from(sv.restore.run.outcome));
    sync_strings(&ui.get_rw_log(), &sv.restore.run.log, |m| ui.set_rw_log(m));

    // founding ritual; the run header is composed here so an MCP-started
    // founding shows real values even with an empty local form
    ui.set_cw_step(i32::from(sv.create.run.step));
    ui.set_cw_outcome(i32::from(sv.create.run.outcome));
    ui.set_cw_seed(sv.create.seed.clone().into());
    ui.set_cw_run_name(sv.create.name.clone().into());
    ui.set_cw_run_detail(
        format!(
            "{}-of-{} · {}",
            sv.create.threshold, sv.create.members, sv.create.net
        )
        .into(),
    );
    sync_strings(&ui.get_cw_log(), &sv.create.run.log, |m| ui.set_cw_log(m));
    // the ritual member list: founder (always sealed) plus one row per seat
    let mut seats: Vec<RitualSeat> = vec![RitualSeat {
        member: sv.create.member.as_str().into(),
        detail: strings_founder(lang).into(),
        state: 2,
    }];
    for (i, s) in sv.create.seats.iter().enumerate() {
        let (member, detail) = if s.member.is_empty() {
            // only offer the link once it is genuinely joinable (carries the
            // transport handover); until the queue is provisioned it is a
            // non-joinable preview, so show nothing to copy yet
            let detail = if molt_engine::FoundingInvite::parse(&s.link).is_some() {
                s.link.clone()
            } else {
                String::new()
            };
            (format!("Invite {}", i + 1), detail)
        } else {
            (s.member.clone(), seat_state_label(lang, s.state))
        };
        seats.push(RitualSeat {
            member: member.into(),
            detail: detail.into(),
            state: i32::from(s.state),
        });
    }
    let sealed = seats.iter().filter(|s| s.state == 2).count();
    ui.set_cw_sealed(i32::try_from(sealed).unwrap_or(0));
    ui.set_cw_total(i32::try_from(seats.len()).unwrap_or(0));
    ui.set_cw_simulated(sv.create.simulated);
    // the deliberation step: once every seat has joined, the founder proposes
    // the final name + charter for the members to ratify (the agenda itself is
    // a local editable draft in the wizard, like the name)
    ui.set_cw_can_propose(sv.create.can_propose);
    sync_rows(&ui.get_cw_seats(), seats, |m| ui.set_cw_seats(m));

    // join
    ui.set_jw_step(i32::from(sv.join.run.step));
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
    ui.set_jw_proposed_name(sv.join.proposed_name.clone().into());
    ui.set_jw_proposed_agenda(sv.join.proposed_agenda.clone().into());
    sync_strings(&ui.get_jw_log(), &sv.join.run.log, |m| ui.set_jw_log(m));
}

/// Plain, `Send` snapshot of all surfaces, built off the UI thread.
struct SurfacesBundle {
    /// Language the labels were rendered for (0 = en, 1 = de) — the nav's
    /// sub-view names are localized when the bundle lands.
    lang: i32,
    /// The chat surface is showing the Archive sub-view — its log pages
    /// at 20 rows (an archive can hold a whole retention half-window).
    chat_archive: bool,
    /// Every committed chain block, newest first (the Chain-History panel).
    chain_rows: Vec<molt_core::ChainBlockView>,
    member: String,
    threshold_badge: String,
    surfaces: Vec<SurfaceData>,
    /// The chat sidebar's channel rows (chat bus).
    channels: Vec<ChannelRowData>,
    /// Canonical key of the selected channel (echoed into the UI so the
    /// sidebar highlight always matches what the engine filtered by).
    selected_key: String,
    /// Compose-banner label of the selected channel ("" = group).
    selected_label: String,
    /// The selected channel is a decided vote's read-only discussion
    /// (collapses the compose row, shows the banner's 🔒 note).
    selected_closed: bool,
    /// Organization → Members table rows (engine `ReadMembers`), already
    /// ordered by the active sort.
    members: Vec<MemberRowData>,
    /// Organization → Uploads table rows (engine `ReadUploads`), already
    /// thinned by the filter and ordered by the active sort.
    uploads: Vec<UploadRowData>,
    /// Members sort echo: active column ("" = roster order) + direction —
    /// the headers render the ▲/▼ from these.
    members_sort: String,
    members_asc: bool,
    /// Uploads sort echo (like `members_sort`).
    uploads_sort: String,
    uploads_asc: bool,
    /// Uploads filter echo — lands in the filter box only when it differs
    /// (a workspace-switch reset or the members-table uploads-jump; live
    /// typing is guarded by the generation).
    uploads_filter: String,
    /// Effective (push-clamped) 0-based page per paged proposal-outcome
    /// list, keyed `"{surface}:{list}"` — `apply_surfaces` slices the
    /// declined/applied models with it and echoes "page x of y" into the
    /// surface tab (see [`ChatUiState::list_pages`]).
    list_pages: HashMap<String, usize>,
    /// The status info strip (founding date + mock activity trio).
    org_stats: OrgStats,
    /// Group-channel unread count (badges the Gruppe nav row).
    group_unread: i32,
}

/// The Organization → Status info strip, from the engine's Status reply.
struct OrgStats {
    /// Rendered founding date, always `YYYY-MM-DD` (a workspace without a
    /// recorded date shows the epoch, `1970-01-01`).
    founded: String,
    /// The republic's current image (engine `StatusView.image`): the
    /// materialized logo file inside the workspace directory (the bytes
    /// rode the applied proposal, so every device holds them).
    image: String,
    /// The effective "delete chat after" window (engine
    /// `StatusView.chat_retention_days`).
    retention_days: i32,
    /// Whether the open workspace is a chain-governed republic (engine
    /// `StatusView.chain_governed`) — the per-member "recovery link" action
    /// exists exactly there, so the Members table offers it only then.
    chain_governed: bool,
}

/// One rendered row of the Organization → Members table.
struct MemberRowData {
    name: String,
    /// Identity-key fingerprint ("" on unanchored/demo workspaces).
    id: String,
    /// Full anchored identity key, lowercase hex ("" unanchored).
    pk: String,
    last: String,
    /// 0 = synced, 1 = syncing, 2 = offline (mock presence).
    state: i32,
    uploads: i32,
}

/// One rendered row of the Organization → Uploads table (labels are
/// pre-rendered here; the .slint side only displays).
struct UploadRowData {
    /// The carrying chat message id (hex) — what download-file takes.
    id: String,
    user: String,
    date: String,
    name: String,
    kind: String,
    size: String,
    available: bool,
    /// Sharer reachable (a user-to-user transfer needs them online).
    online: bool,
    /// Shortened real sha256 for the cell (the full hex rides MCP).
    checksum: String,
    expires: String,
    /// Live download status label ("" = idle): "42 %" while moving,
    /// a check mark when done, a warning sign when failed.
    status: String,
    /// 0 idle · 1 running · 2 done · 3 failed (drives color + button).
    status_kind: i32,
    /// Share time (unix seconds) — the sort key behind the rendered `date`.
    ts: u64,
    /// Size in bytes — the sort key behind the rendered `size` label.
    bytes: u64,
    /// Link expiry (unix seconds) — the sort key behind `expires`.
    expires_ts: u64,
    /// The FULL sha256 hex ("" on legacy shares) — the filter/sort key
    /// behind the shortened `checksum` cell, so a pasted full checksum
    /// still finds its row.
    checksum_full: String,
}

/// One chat-channel sidebar row (plain, `Send` twin of the Slint
/// `ChannelItem`). The group row's `label` stays empty — the UI substitutes
/// the localized `Strings.ch-group`.
struct ChannelRowData {
    key: String,
    label: String,
    icon: String,
    unread: i32,
}

/// One quotable message, as the teaser renderer needs it — built over the
/// FULL chat log, because a quote may point across channels (the sanctioned
/// cross-post) and must still tease when its target is filtered out of view.
struct QuoteSrc {
    lead: String,
    text: String,
    deleted: bool,
}

/// Per-channel unread bookkeeping (P9 — in-memory only for this iteration;
/// persisting into `WorkspacePrefs` is the B5 stretch package).
#[derive(Default)]
struct UnreadLedger {
    /// Channel key → message count up to which the channel counts as read.
    last_seen: HashMap<String, usize>,
    /// The very first observation seeds `last_seen` (opening a workspace
    /// must not present its whole history as one unread wall).
    seeded: bool,
}

/// UI-local chat-bus state shared between the Slint callbacks and the
/// mirror task. The SELECTED channel deliberately lives here (UI-local,
/// like `nav-collapsed`) and NOT in the shared `SessionView`: the filter
/// itself runs engine-side (`ReadState { channel }`), so GUI and MCP stay
/// co-equal — each operator passes its own filter, and which channel this
/// window looks at is presentation, not shared state.
#[derive(Default)]
struct ChatUiState {
    /// The workspace id this state belongs to — a switch resets everything
    /// (see [`ChatUiState::enter_workspace`]).
    workspace: String,
    /// The channel the chat pane shows; compose files new messages here.
    selected: ChannelRef,
    /// Per-channel unread counts (reset on selection).
    ledger: UnreadLedger,
    /// Proposal id → unix time this UI first saw it. Proposals carry no
    /// timestamp, so the patch-channel system lines interleave at this
    /// first-seen approximation (documented in `patch_system_lines`).
    first_seen: HashMap<u64, u64>,
    /// Everything this UI ever learned about a proposal from `pending` —
    /// the read contract's `pending` is Proposed-only, so a sealed/closed
    /// proposal vanishes from every read and only this cache keeps its
    /// patch channel titled and stated (see [`update_known_proposals`]).
    proposals: HashMap<u64, KnownProposal>,
    /// Push/selection generation. Concurrent `push_surfaces` runs race
    /// last-write-wins on the Slint event loop; every selection change and
    /// every push start bumps this, and a push whose captured generation
    /// is no longer current must neither apply its bundle nor touch the
    /// unread ledger (see [`ChatUiState::begin_push`]).
    generation: u64,
    /// Organization → Members sort: active column ("" = roster order).
    /// Like the channel selection this is UI-LOCAL presentation state —
    /// the engine's `ReadMembers`/`ReadUploads` stay the full projections
    /// (MCP sees them unchanged); this window merely re-orders/thins the
    /// mirrored rows before pushing them into the Slint models. A
    /// workspace switch resets it with the rest of this state.
    members_sort: String,
    /// Members sort direction (meaningful only while `members_sort` != "").
    members_asc: bool,
    /// Organization → Uploads sort: active column ("" = engine order).
    uploads_sort: String,
    /// Uploads sort direction (meaningful only while `uploads_sort` != "").
    uploads_asc: bool,
    /// Uploads filter needle: case-insensitive substring across user,
    /// filename and (full) checksum; "" = all rows.
    uploads_filter: String,
    /// Current 0-based page of the paged proposal-outcome lists, keyed
    /// `"{surface}:{list}"` (list = "declined" | "applied"); a missing key
    /// is page 0. UI-LOCAL presentation like the sorts — the engine's
    /// reads stay the full projections (MCP sees them unchanged). The
    /// stored page re-bases against the list's current length on every
    /// push ([`ChatUiState::clamp_list_page`]); a workspace switch resets
    /// it with the rest of this state.
    list_pages: HashMap<String, usize>,
}

impl ChatUiState {
    /// Bind the state to the active workspace. On a SWITCH (different id,
    /// including to/from "no workspace") everything resets: a stale
    /// Patch/Topic selection from the previous workspace must not filter
    /// the new one's log, the ledger must re-seed on the new history and
    /// the first-seen stamps + proposal cache belong to the old
    /// proposals. Same id → no-op.
    fn enter_workspace(&mut self, active: &str) {
        if self.workspace != active {
            *self = ChatUiState {
                workspace: active.to_string(),
                // the push generation survives the reset: an in-flight
                // push from the previous workspace must never match a
                // freshly zeroed counter
                generation: self.generation,
                ..ChatUiState::default()
            };
        }
    }

    /// Start one `push_surfaces` pass: bind to the active workspace, then
    /// stamp this push's generation. The bump makes every earlier
    /// in-flight push stale, so concurrent pushes resolve newest-wins
    /// instead of last-write-wins.
    fn begin_push(&mut self, active: &str) -> u64 {
        self.enter_workspace(active);
        self.generation += 1;
        self.generation
    }

    /// Select a channel. The bump invalidates every in-flight push: a
    /// bundle read for the previous selection must not land on — or mark
    /// read — the fresh one.
    fn select(&mut self, channel: ChannelRef) {
        self.selected = channel;
        self.generation += 1;
    }

    /// Whether the push stamped `gen` is still the newest observer; a
    /// stale push skips its ledger bookkeeping and its apply closure.
    fn is_current(&self, gen: u64) -> bool {
        self.generation == gen
    }

    /// Click on a Members header column: toggle-or-switch the sort. The
    /// generation bump stales every in-flight push (its bundle carries the
    /// previous order).
    fn sort_members_by(&mut self, column: &str) {
        toggle_sort(&mut self.members_sort, &mut self.members_asc, column);
        self.generation += 1;
    }

    /// Click on an Uploads header column: toggle-or-switch the sort.
    fn sort_uploads_by(&mut self, column: &str) {
        toggle_sort(&mut self.uploads_sort, &mut self.uploads_asc, column);
        self.generation += 1;
    }

    /// Set the uploads filter needle (typed, or pre-filled by the Members
    /// table's uploads-jump).
    fn set_uploads_filter(&mut self, needle: String) {
        self.uploads_filter = needle;
        self.generation += 1;
    }

    /// Step a paged proposal-outcome list by `delta` pages (the pager's
    /// prev/next). Below the first page clamps at zero; the upper bound is
    /// enforced at push time ([`ChatUiState::clamp_list_page`] — only the
    /// push knows the list's current length). The generation bump stales
    /// every in-flight push (its bundle carries the previous page).
    fn page_list_by(&mut self, surface: &str, list: &str, delta: i32) {
        let page = self.list_pages.entry(format!("{surface}:{list}")).or_insert(0);
        *page = page.saturating_add_signed(delta as isize);
        self.generation += 1;
    }

    /// Re-base a stored page against the list's CURRENT length and return
    /// the effective 0-based page. The clamp writes back, so the next
    /// prev/next steps from the page the user actually sees — not from a
    /// stale out-of-range value a shrunk list left behind.
    fn clamp_list_page(&mut self, surface: &str, list: &str, len: usize) -> usize {
        let key = format!("{surface}:{list}");
        let stored = self.list_pages.get(&key).copied().unwrap_or(0);
        let (_, _, page, _) = page_slice(len, stored, LIST_PAGE_SIZE);
        if page != stored {
            self.list_pages.insert(key, page);
        }
        page
    }
}
struct SurfaceData {
    key: String,
    name: String,
    gated: bool,
    log: Vec<LogLineData>,
    pending: Vec<ProposalRowData>,
    /// Pending proposals this node already approved (the rest of `pending`
    /// still wait on this node's vote — the approvals table shows the split).
    pending_voted: usize,
    /// Declined proposals against this surface (total, for the status strip).
    denied: usize,
    /// The declined proposals still inside the display-retention window —
    /// the Declined view empties on the same rhythm as the group chat.
    declined: Vec<ProposalRowData>,
}
struct LogLineData {
    /// Stable message id, 32-char hex ("" on legacy entries without one —
    /// such rows must never offer id-requiring actions, see the `id != ""`
    /// guards in the .slint files: the UI must not fake success).
    id: String,
    lead: String,
    text: String,
    when: String,
    quote: i32,
    /// Quoted message by stable id ("" = none / legacy numeric quote).
    quote_id: String,
    /// A UI-synthesized governance line (patch channels, P8): quiet
    /// styling, no author, no actions.
    system: bool,
    quote_label: String,
    deleted_by: String,
    first: bool,
    own: bool,
    alt: bool,
    mine_emoji: String,
    reactions: Vec<ReactionData>,
    has_file: bool,
    file_name: String,
    file_meta: String,
    file_available: bool,
    /// The proposal this applied-log row came from (the snapshot's parallel
    /// id track) — the 💬 jump into its discussion channel. `None` on chat
    /// rows, system lines and rows of unknown origin (legacy dumps): those
    /// must offer no jump (feedback honesty, like the `id != ""` guards).
    proposal_id: Option<u64>,
}
struct ReactionData {
    emoji: String,
    count: i32,
    mine: bool,
}
struct ProposalRowData {
    id: i32,
    text: String,
    approvals: i32,
    threshold: i32,
    /// Ist-Stand / Soll-Stand display pair ("" = hidden line).
    current: String,
    proposed: String,
    /// set_image / remove_image: the card renders the current picture and
    /// links the proposed image (its bytes ride the payload).
    image_op: bool,
    /// A pending set_image's embedded bytes (base64; "" otherwise) — the
    /// preview decodes them locally on every member's device.
    img_b64: String,
    /// set_charter: long Ist/Soll texts render capped + scrollable.
    charter_op: bool,
    /// Per-member stance in roster order (0 open · 1 approved · 2 declined).
    votes: Vec<(String, i32)>,
    /// Who declined it ("" = not declined) + the human "when" label.
    declined_by: String,
    declined_when: String,
}

/// Read status + every surface snapshot and push them into the Slint models.
///
/// The chat surface is read TWICE: once unfiltered (channel enumeration and
/// quote teasers are whole-log concerns — a quote may point across
/// channels), and once through the engine's channel filter for the
/// displayed log. Filtering client-side would break co-equality with MCP,
/// so the filter deliberately rides `ReadState { channel }`.
async fn push_surfaces(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let (member, threshold_badge, org_stats) = match wallet.execute(Command::Status).await {
        Ok(Reply::Status(s)) => (
            s.member,
            format!("{}-of-{}", s.threshold, s.members.len()),
            OrgStats {
                // the literal epoch (not file_date_label(0): a negative-UTC
                // timezone would render ts 0 as 1969-12-31)
                founded: if s.founded_ts == 0 {
                    "1970-01-01".to_string()
                } else {
                    file_date_label(s.founded_ts)
                },
                image: s.image,
                retention_days: i32::try_from(s.chat_retention_days).unwrap_or(7),
                chain_governed: s.chain_governed,
            },
        ),
        _ => return,
    };
    // the chat-bus UI state is per-workspace: bind it to the active id so
    // a workspace switch drops the previous selection/unread/first-seen
    // (the language rides along — a SetLanguage emits a Full session
    // change, which re-runs this push, so the nav labels stay live)
    // the chat surface's sub-view ("today"/"archive") rides the shared
    // session; the time filter itself is engine-side (`ReadState { view }`,
    // co-equality) — when another surface is selected the chat read stays
    // on the default General view
    let (active_ws, lang, chat_view) = match wallet.execute(Command::ReadSession).await {
        Ok(Reply::Session(s)) => (
            s.active_workspace.clone(),
            i32::from(s.language == "de"),
            if s.surface == Surface::Chat {
                s.view.clone()
            } else {
                Surface::Chat.default_view().to_string()
            },
        ),
        _ => (String::new(), 0, Surface::Chat.default_view().to_string()),
    };
    // stamp this push BEFORE the surface reads: any selection change or
    // newer push from here on makes this pass stale, and a stale pass must
    // neither touch the ledger nor land its bundle (concurrent pushes
    // otherwise race last-write-wins and can revert a fresh selection)
    let Some((my_gen, selected)) = chat_ui
        .lock()
        .ok()
        .map(|mut s| (s.begin_push(&active_ws), s.selected.clone()))
    else {
        return;
    };
    let full_chat = match wallet
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            // whole-log concerns (channel enumeration, quote teasers):
            // deliberately no view filter — a quote may point across the
            // today/archive boundary like it may across channels
            view: None,
        })
        .await
    {
        Ok(Reply::State(snap)) => Some(snap),
        _ => None,
    };
    // the Organization tables ride the same push: the engine's ReadMembers /
    // ReadUploads (the projections the MCP tools of the same name read)
    let members: Vec<MemberRowData> = match wallet.execute(Command::ReadMembers).await {
        Ok(Reply::Members { members: rows }) => rows
            .into_iter()
            .map(|m| MemberRowData {
                name: m.member,
                id: m.id,
                pk: m.identity_pk,
                last: m.last_seen,
                state: i32::from(m.presence),
                uploads: i32::try_from(m.uploads).unwrap_or(i32::MAX),
            })
            .collect(),
        _ => Vec::new(),
    };
    let upload_now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let uploads: Vec<UploadRowData> = match wallet.execute(Command::ReadUploads).await {
        Ok(Reply::Uploads { uploads: rows }) => rows
            .into_iter()
            .map(|u| UploadRowData {
                id: u.id.to_string(),
                user: u.member,
                date: file_date_label(u.ts),
                name: u.name,
                kind: u.kind,
                size: file_size_label(u.size),
                available: u.available,
                online: u.online,
                checksum: u
                    .checksum
                    .get(..10)
                    .map(|s| format!("{s}…"))
                    .unwrap_or_default(),
                expires: expires_label(upload_now, u.expires_ts, u.available),
                ts: u.ts,
                bytes: u.size,
                expires_ts: u.expires_ts,
                checksum_full: u.checksum,
                status: match u.download.as_ref().map(|d| d.phase.as_str()) {
                    Some("requested") => "0 %".to_string(),
                    Some("transferring") => u
                        .download
                        .as_ref()
                        .map(|d| format!("{} %", d.percent))
                        .unwrap_or_default(),
                    Some("done") => "\u{2713}".to_string(),
                    Some("failed") => "\u{26a0}".to_string(),
                    _ => String::new(),
                },
                status_kind: match u.download.as_ref().map(|d| d.phase.as_str()) {
                    Some("requested" | "transferring") => 1,
                    Some("done") => 2,
                    Some("failed") => 3,
                    _ => 0,
                },
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut snaps: Vec<(Surface, SurfaceSnapshot)> = Vec::new();
    for sf in Surface::ALL {
        let channel = (sf == Surface::Chat).then(|| selected.clone());
        // the displayed chat log follows the selected sub-view: General
        // shows the younger half of the retention window, Archive the
        // older half — filtered engine-side, same as the channel
        let view = (sf == Surface::Chat).then(|| chat_view.clone());
        if let Ok(Reply::State(snap)) = wallet
            .execute(Command::ReadState { surface: sf, channel, view })
            .await
        {
            snaps.push((sf, snap));
        }
    }
    // proposal state across ALL surfaces feeds the patch channels: lazy
    // titles for the sidebar and the system lines (P8)
    let all_pending: Vec<ProposalView> = snaps
        .iter()
        .flat_map(|(_, s)| s.pending.iter().cloned())
        .collect();
    // …and the declined lists feed the cache too: a veto this UI never saw
    // pending (fresh open, other member's decline) must still title its
    // discussion channel and flag it closed
    let all_declined: Vec<ProposalView> = snaps
        .iter()
        .flat_map(|(_, s)| s.declined.iter().cloned())
        .collect();
    // the gated surfaces' applied logs — the proposal cache resolves a
    // vanished proposal's fate against them (the applied values ARE the
    // raw proposal payloads, for the chain and the legacy path alike)
    let applied_by_surface: HashMap<Surface, Vec<serde_json::Value>> = snaps
        .iter()
        .filter(|(sf, _)| *sf != Surface::Chat)
        .map(|(sf, s)| (*sf, s.applied.clone()))
        .collect();
    let full_msgs = full_chat.as_ref().map(chat_messages).unwrap_or_default();
    // the engine enumerates the channels (P7): every distinct ref in the
    // log, `Group` always present — authoritative for the chat surface
    // (empty only when no chat read succeeded, i.e. nothing is open)
    let infos = full_chat
        .as_ref()
        .map(|s| s.channels.clone())
        .unwrap_or_default();
    let quotes = quote_sources(&full_msgs);
    let counts: Vec<(String, usize)> = infos
        .iter()
        .map(|i| (channel_key(&i.channel), i.count))
        .collect();
    let selected_key = channel_key(&selected);
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    // the Chain-History settings panel: every committed block, newest
    // first (co-equal read — the MCP read_chain tool serves the same)
    let chain_rows: Vec<molt_core::ChainBlockView> = match wallet.execute(Command::ReadChain).await
    {
        Ok(Reply::Chain { blocks }) => blocks,
        _ => Vec::new(),
    };
    let chain_len = chain_rows.len();
    let (unread, first_seen, known, org_view, list_pages) = {
        let mut st = chat_ui.lock().expect("chat ui state poisoned");
        if !st.is_current(my_gen) {
            // a newer selection/push owns the state — observing now would
            // mis-mark the fresh channel read, and the bundle is stale
            return;
        }
        for p in &all_pending {
            st.first_seen.entry(p.id.0).or_insert(now);
        }
        update_known_proposals(&mut st.proposals, &all_pending, &all_declined, &applied_by_surface);
        // the paged proposal-outcome lists: re-base every stored page
        // against its list's CURRENT length (a shrunk list must never
        // leave the view on a page that no longer exists), then capture
        // the effective pages for the bundle. Chat's log is the chat
        // pane — never paged here.
        let mut list_pages: HashMap<String, usize> = HashMap::new();
        for (sf, s) in &snaps {
            if *sf == Surface::Chat {
                // the Archive sub-view pages like the outcome lists — the
                // General pane stays the full scrollback
                if chat_view == "archive" {
                    list_pages.insert(
                        "chat:archive".to_string(),
                        st.clamp_list_page("chat", "archive", s.applied.len()),
                    );
                }
                continue;
            }
            let key = sf.as_str();
            list_pages.insert(
                format!("{key}:declined"),
                st.clamp_list_page(key, "declined", s.declined.len()),
            );
            list_pages.insert(
                format!("{key}:applied"),
                st.clamp_list_page(key, "applied", s.applied.len()),
            );
        }
        list_pages.insert(
            "chain:history".to_string(),
            st.clamp_list_page("chain", "history", chain_len),
        );
        (
            st.ledger.observe(&counts, &selected_key),
            st.first_seen.clone(),
            st.proposals.clone(),
            (
                st.members_sort.clone(),
                st.members_asc,
                st.uploads_sort.clone(),
                st.uploads_asc,
                st.uploads_filter.clone(),
            ),
            list_pages,
        )
    };
    // the Organization tables' presentation pass (UI-local, like the
    // channel selection): thin the uploads by the filter needle, then
    // order both tables by their active sort column — the engine's
    // ReadMembers/ReadUploads projections stay the full, untouched truth
    let (members_sort, members_asc, uploads_sort, uploads_asc, uploads_filter) = org_view;
    let mut members = members;
    sort_members(&mut members, &members_sort, members_asc);
    let mut uploads = filter_uploads(uploads, &uploads_filter);
    sort_uploads(&mut uploads, &uploads_sort, uploads_asc);
    // titles come from the cache, so a patch channel keeps its name (and
    // its ✓/⊘ state line) after the proposal left the Proposed-only read
    let titles = known_titles(lang, &known);
    let channels = derive_channels(lang, &infos, &known, &unread);
    // the group channel has no sidebar row anymore — its unread count
    // badges the Gruppe nav row instead
    let group_unread =
        i32::try_from(unread.get("group").copied().unwrap_or(0)).unwrap_or(i32::MAX);
    let selected_label = channel_display_label(&selected, &titles);
    let selected_closed = selected_channel_closed(&selected, &infos, &known);
    let ctx = ChatViewCtx {
        selected,
        proposals: all_pending,
        known,
        first_seen,
        quotes,
    };
    let surfaces: Vec<SurfaceData> = snaps
        .iter()
        .map(|(sf, snap)| {
            surface_data(lang, *sf, snap, &member, (*sf == Surface::Chat).then_some(&ctx))
        })
        .collect();
    let bundle = SurfacesBundle {
        lang,
        chain_rows,
        chat_archive: chat_view == "archive",
        member,
        threshold_badge,
        surfaces,
        channels,
        selected_key,
        selected_label,
        selected_closed,
        members,
        uploads,
        members_sort,
        members_asc,
        uploads_sort,
        uploads_asc,
        uploads_filter,
        list_pages,
        org_stats,
        group_unread,
    };
    let weak = weak.clone();
    let chat_ui = chat_ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        // the generation may have moved between the bundle build and this
        // closure running on the UI thread — a stale bundle must not land
        // (it would revert the visible pane until the next engine event)
        if !chat_ui.lock().map(|st| st.is_current(my_gen)).unwrap_or(false) {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            apply_surfaces(&ui, &bundle);
        }
    });
}

/// Build the Slint surface models from a bundle (on the UI thread). The rows
/// of the surfaces model are updated IN PLACE when possible: replacing the
/// whole model would recreate every main-view element on each engine event —
/// and with it drop the keyboard focus out of the chat compose box mid-typing.
fn apply_surfaces(ui: &AppWindow, b: &SurfacesBundle) {
    ui.set_node_member(b.member.clone().into());
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
            } else if s.key == "chat" && b.chat_archive {
                // the Archive sub-view pages at 20 (a half-window of
                // retention can be long); quotes pointing outside the
                // current page degrade to teaser-only — acceptable in a
                // read-only archive
                let page = page_of(&b.list_pages, "chat", "archive");
                page_slice(s.log.len(), page, LIST_PAGE_SIZE)
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
                    LogLine {
                        id: l.id.clone().into(),
                        lead: l.lead.clone().into(),
                        text: l.text.clone().into(),
                        when: l.when.clone().into(),
                        quote: l.quote,
                        system: l.system,
                        quote_label: l.quote_label.clone().into(),
                        deleted_by: l.deleted_by.clone().into(),
                        first: l.first,
                        own: l.own,
                        alt: l.alt,
                        mine_emoji: l.mine_emoji.clone().into(),
                        reactions: ModelRc::new(VecModel::from(reactions)),
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
            let to_row = |p: &ProposalRowData| ProposalRow {
                id: p.id,
                text: p.text.clone().into(),
                approvals: p.approvals,
                threshold: p.threshold,
                current: p.current.clone().into(),
                proposed: p.proposed.clone().into(),
                image_op: p.image_op,
                img_b64: p.img_b64.as_str().into(),
                charter_op: p.charter_op,
                votes: ModelRc::new(VecModel::from(
                    p.votes
                        .iter()
                        .map(|(member, vote)| MemberVoteMark {
                            member: member.as_str().into(),
                            vote: *vote,
                        })
                        .collect::<Vec<_>>(),
                )),
                declined_by: p.declined_by.clone().into(),
                declined_when: p.declined_when.clone().into(),
            };
            // pending stays complete — an open vote must never hide behind
            // a page; the declined (outcome) list pages like the applied log
            let pending: Vec<ProposalRow> = s.pending.iter().map(to_row).collect();
            let d_page = page_of(&b.list_pages, &s.key, "declined");
            let (d_start, d_end, d_page, d_pages) =
                page_slice(s.declined.len(), d_page, LIST_PAGE_SIZE);
            let declined: Vec<ProposalRow> =
                s.declined[d_start..d_end].iter().map(to_row).collect();
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
                log: ModelRc::new(VecModel::from(log)),
                pending: ModelRc::new(VecModel::from(pending)),
                declined: ModelRc::new(VecModel::from(declined)),
                views: ModelRc::new(VecModel::from(views)),
            }
        })
        .collect();
    sync_rows(&ui.get_surfaces(), tabs, |m| ui.set_surfaces(m));

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
    sync_rows(&ui.get_chat_channels(), channels, |m| ui.set_chat_channels(m));
    ui.set_selected_channel(b.selected_key.as_str().into());
    ui.set_selected_channel_votable(b.selected_key.starts_with("patch:"));
    ui.set_selected_channel_label(b.selected_label.as_str().into());
    ui.set_selected_channel_closed(b.selected_closed);

    // the Organization tables (Members / Uploads)
    let members: Vec<MemberRow> = b
        .members
        .iter()
        .map(|m| MemberRow {
            name: m.name.as_str().into(),
            id: m.id.as_str().into(),
            pk: m.pk.as_str().into(),
            last: m.last.as_str().into(),
            state: m.state,
            uploads: m.uploads,
        })
        .collect();
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

    // the republic's image: (re)load the picture only when the file
    // reference changes. The bytes rode the applied set_image proposal, so
    // the engine materializes the logo file on EVERY device; decode by
    // CONTENT (image_from_bytes) — the reference's extension comes from a
    // peer-supplied display value and must not decide the format. On a
    // session-only workspace the reference is no local file — the read
    // fails quietly and the placeholder mark stays.
    if ui.get_org_img_path().as_str() != b.org_stats.image {
        ui.set_org_img_path(b.org_stats.image.as_str().into());
        let loaded = (!b.org_stats.image.is_empty())
            .then(|| std::fs::read(&b.org_stats.image).ok())
            .flatten()
            .and_then(|bytes| image_from_bytes(&bytes));
        ui.set_org_img_set(loaded.is_some());
        ui.set_org_img(loaded.unwrap_or_default());
    }
}

/// Render a chat timestamp as `2026-06-02 13:37 (~20 minutes ago)` in the
/// local timezone. The relative part refreshes with every surfaces push.
fn when_label(lang: i32, ts: u64) -> String {
    when_label_at(lang, ts, chrono::Utc::now().timestamp())
}

/// [`when_label`] against an explicit "now" (testable). The relative part
/// renders in the ACTIVE language (a cached English "(~2 days ago)" was
/// leaking into the German UI — user report 2026-07-18).
fn when_label_at(lang: i32, ts: u64, now: i64) -> String {
    let Ok(secs) = i64::try_from(ts) else {
        return String::new();
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    let local = utc.with_timezone(&chrono::Local);
    let ago = (now - secs).max(0);
    let de = lang == 1;
    let rel = if ago < 60 {
        if de { "gerade eben".to_string() } else { "just now".to_string() }
    } else if ago < 3600 {
        let m = ago / 60;
        if de {
            format!("vor ~{m} Minute{}", if m == 1 { "" } else { "n" })
        } else {
            format!("~{m} minute{} ago", if m == 1 { "" } else { "s" })
        }
    } else if ago < 86_400 {
        let h = ago / 3600;
        if de {
            format!("vor ~{h} Stunde{}", if h == 1 { "" } else { "n" })
        } else {
            format!("~{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
    } else {
        let d = ago / 86_400;
        if de {
            format!("vor ~{d} Tag{}", if d == 1 { "" } else { "en" })
        } else {
            format!("~{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    };
    format!("{} ({rel})", local.format("%Y-%m-%d %H:%M"))
}

/// The colorful (Twemoji) nav icon for a sub-view key. Keys repeating across
/// surfaces (archive, proposals, status, …) deliberately share one glyph.
fn view_icon(key: &str) -> &'static str {
    match key {
        "charter" => "📜",
        "status" => "📡",
        "members" => "👥",
        "uploads" => "📎",
        "pending" => "🗳️",
        "declined" => "🚫",
        "today" => "💬",
        "archive" => "🗄️",
        "brain" => "🧠",
        "proposals" => "🗳️",
        "accepted" => "✅",
        "denied" => "❌",
        "board" => "📋",
        "create" => "✨",
        "my-quests" => "🎯",
        "secrets" => "🔐",
        "disclose" => "📤",
        "exposed" => "🔓",
        "balance" => "💰",
        "history" => "📜",
        "send" => "📤",
        "receive" => "📥",
        "settings" => "⚙️",
        _ => "▪️",
    }
}

/// Everything the chat surface's projection needs beyond its own (possibly
/// channel-filtered) snapshot: the selected channel, the collected proposal
/// state feeding the patch-channel system lines (P8), the UI-side
/// first-seen times standing in for the timestamps proposals do not carry,
/// and the FULL log's quote sources (see [`QuoteSrc`]).
struct ChatViewCtx {
    selected: ChannelRef,
    proposals: Vec<ProposalView>,
    /// The per-workspace proposal cache (title + fate survive a proposal
    /// leaving the Proposed-only `pending` window).
    known: HashMap<u64, KnownProposal>,
    first_seen: HashMap<u64, u64>,
    quotes: HashMap<String, QuoteSrc>,
}

/// The typed chat messages of a snapshot (chat surface only).
fn chat_messages(snap: &SurfaceSnapshot) -> Vec<ChatMessage> {
    snap.applied
        .iter()
        .filter_map(|v| serde_json::from_value::<ChatMessage>(v.clone()).ok())
        .collect()
}

/// Project one surface snapshot into plain display data. `me` is the local
/// member handle — it marks own messages and the own reaction pill.
/// `chat_ctx` is `Some` for the chat surface only.
fn surface_data(
    lang: i32,
    sf: Surface,
    snap: &SurfaceSnapshot,
    me: &str,
    chat_ctx: Option<&ChatViewCtx>,
) -> SurfaceData {
    let mut log: Vec<LogLineData> = if sf == Surface::Chat {
        let msgs = chat_messages(snap);
        // the retention window ("delete chat after N days") is ENGINE
        // semantics now — the read already arrives filtered, identically
        // for the GUI and an MCP agent (co-equality)
        let pairs: Vec<(u64, LogLineData)> = msgs
            .iter()
            .map(|m| (m.ts, chat_line(lang, m, me)))
            .collect();
        let system = match chat_ctx.map(|c| &c.selected) {
            Some(ChannelRef::Patch { id }) => {
                let ctx = chat_ctx.expect("checked above");
                patch_system_lines(lang, id.0, &ctx.proposals, &ctx.known, &ctx.first_seen)
            }
            _ => Vec::new(),
        };
        merge_by_time(pairs, system)
    } else {
        snap.applied
            .iter()
            .enumerate()
            .map(|(i, v)| LogLineData {
                id: String::new(),
                lead: String::new(),
                text: display_title(lang, v),
                when: String::new(),
                quote: -1,
                quote_id: String::new(),
                system: false,
                quote_label: String::new(),
                deleted_by: String::new(),
                first: true,
                own: false,
                alt: false,
                mine_emoji: String::new(),
                reactions: Vec::new(),
                has_file: false,
                file_name: String::new(),
                file_meta: String::new(),
                file_available: false,
                // the id track is positionally parallel to `applied`; a
                // pre-id peer's snapshot has an empty/short track → None
                proposal_id: snap.applied_ids.get(i).copied().flatten(),
            })
            .collect()
    };
    let no_quotes = HashMap::new();
    annotate_chat_log(&mut log, chat_ctx.map_or(&no_quotes, |c| &c.quotes));
    let pending: Vec<ProposalRowData> =
        snap.pending.iter().map(|p| proposal_row(lang, p)).collect();
    // the Declined view empties on the chat-retention rhythm — engine
    // semantics too (the read arrives pre-filtered on declined_at)
    let declined: Vec<ProposalRowData> =
        snap.declined.iter().map(|p| proposal_row(lang, p)).collect();
    SurfaceData {
        key: sf.as_str().to_string(),
        name: surface_name(lang, sf).to_string(),
        gated: snap.gated,
        log,
        pending,
        pending_voted: snap.pending.iter().filter(|p| p.approved_by_me).count(),
        denied: snap.denied,
        declined,
    }
}

/// Project one chain block into its Chain-History row — titles render in
/// the ACTIVE language from the payload's op placeholder, exactly like the
/// applied logs (language-neutral wire, localized display).
fn chain_row(lang: i32, r: &molt_core::ChainBlockView) -> ChainRow {
    let de = lang == 1;
    let (kind, title) = match r.kind.as_str() {
        "genesis" => (
            strings_pick(de, "Founding", "Gründung"),
            r.payload.as_str().unwrap_or_default().to_string(),
        ),
        "membership" => (
            strings_pick(de, "Membership", "Mitgliedschaft"),
            r.payload.as_str().unwrap_or_default().to_string(),
        ),
        "checkpoint" => (
            strings_pick(de, "Checkpoint (compacted)", "Checkpoint (kompaktiert)"),
            format!(
                "{} {}",
                strings_pick(de, "state up to block", "Zustand bis Block"),
                r.payload.as_u64().unwrap_or(0)
            ),
        ),
        // applied: the payload IS the proposal payload — op-placeholder title
        _ => (String::new(), display_title(lang, &r.payload)),
    };
    ChainRow {
        height: if r.height == 0 && r.kind == "applied" {
            strings_pick(de, "— (before the cut)", "— (vor dem Schnitt)")
        } else {
            format!("#{}", r.height)
        }
        .into(),
        kind: kind.into(),
        surface: if r.surface.is_empty() {
            String::new()
        } else {
            Surface::parse(&r.surface)
                .map(|sf| surface_name(lang, sf).to_string())
                .unwrap_or_else(|| r.surface.clone())
        }
        .into(),
        title: title.into(),
        signers: r.signers.join(", ").into(),
    }
}

/// Tiny bilingual pick for labels that live in Rust-side projections.
fn strings_pick(de: bool, en: &str, de_s: &str) -> String {
    if de { de_s.to_string() } else { en.to_string() }
}

/// Project one proposal view into the card row the GUI renders — shared by
/// the pending and the declined list.
fn proposal_row(lang: i32, p: &molt_core::ProposalView) -> ProposalRowData {
    let op = p
        .payload
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    ProposalRowData {
        id: p.id.0 as i32,
        text: display_title(lang, &p.payload),
        approvals: p.approvals as i32,
        threshold: p.threshold as i32,
        current: p.current.clone(),
        proposed: p.proposed.clone(),
        image_op: matches!(op, "set_image" | "remove_image"),
        img_b64: p
            .payload
            .get("bytes_b64")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        charter_op: op == "set_charter",
        votes: p
            .votes
            .iter()
            .map(|v| {
                let stance = match v.vote {
                    molt_core::VoteState::Open => 0,
                    molt_core::VoteState::Approved => 1,
                    molt_core::VoteState::Declined => 2,
                };
                (v.member.clone(), stance)
            })
            .collect(),
        declined_by: p.declined_by.clone(),
        declined_when: if p.declined_at > 0 {
            when_label(lang, p.declined_at)
        } else {
            String::new()
        },
    }
}

/// One typed chat message, projected for display. Quote resolution (row +
/// teaser) happens later in [`annotate_chat_log`]: the row index can only
/// be known once system lines are merged in, and the teaser may resolve
/// against a message outside the displayed (filtered) log.
fn chat_line(lang: i32, m: &ChatMessage, me: &str) -> LogLineData {
    let mut mine_emoji = String::new();
    // the BTreeMap iterates sorted by emoji, so the pill order is
    // deterministic across re-renders
    let reactions: Vec<ReactionData> = m
        .reactions
        .iter()
        .map(|(emoji, who)| {
            let mine = who.iter().any(|w| w == me);
            if mine {
                mine_emoji = emoji.clone();
            }
            ReactionData {
                emoji: emoji.clone(),
                count: i32::try_from(who.len()).unwrap_or(i32::MAX),
                mine,
            }
        })
        .collect();
    // a shared file renders as a card: name plus "size · type · date"
    let (has_file, file_name, file_meta, file_available) = match &m.file {
        Some(f) => (
            true,
            f.name.clone(),
            format!(
                "{} · {} · {}",
                file_size_label(f.size),
                f.kind,
                file_date_label(f.modified)
            ),
            f.available,
        ),
        None => (false, String::new(), String::new(), false),
    };
    LogLineData {
        id: if m.id.is_nil() {
            String::new() // a legacy entry: not addressable until B1
        } else {
            m.id.to_string()
        },
        lead: m.from.clone(),
        text: m.body.clone(),
        when: if m.ts > 0 {
            when_label(lang, m.ts)
        } else {
            String::new()
        },
        // legacy numeric quote only (pre-chat-bus rows; B1 resolves these
        // to quote_id at ingest, after which this path goes dormant) — the
        // id path leaves it to annotate_chat_log
        quote: if m.quote_id.is_none() {
            m.quote
                .and_then(|q| i32::try_from(q).ok())
                .unwrap_or(-1)
        } else {
            -1
        },
        quote_id: m.quote_id.map(|q| q.to_string()).unwrap_or_default(),
        // an engine-authored notice (ChatKind::System, e.g. the recovery
        // rejoin announcement) rides the same quiet-line rendering as the
        // UI-synthesized governance rows — one flag, no second style
        system: !m.kind.is_user(),
        quote_label: String::new(), // teaser, filled in by annotate_chat_log
        deleted_by: m.deleted_by.clone().unwrap_or_default(),
        first: true, // author-block start, filled in by annotate_chat_log
        own: m.from == me,
        alt: false, // author-block zebra, filled in by annotate_chat_log
        mine_emoji,
        reactions,
        has_file,
        proposal_id: None,
        file_name,
        file_meta,
        file_available,
    }
}

/// Human size for a byte count, e.g. `"680 B"` / `"48 KiB"` / `"1.2 MiB"`.
fn file_size_label(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        size_label(u32::try_from(bytes / 1024).unwrap_or(u32::MAX))
    }
}

/// Split the charter into up to `max` visually balanced columns at word
/// boundaries (~320 chars per column) — a DISPLAY split only, the text
/// itself is untouched: short charters stay single-column, long ones use
/// the status panel's width. Empty input yields no columns.
fn charter_columns(text: &str, max: usize) -> Vec<String> {
    const PER_COL: usize = 320;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let cols = trimmed.len().div_ceil(PER_COL).clamp(1, max.max(1));
    let target = trimmed.len().div_ceil(cols);
    let mut out = Vec::new();
    let mut rest = trimmed;
    for _ in 0..cols - 1 {
        if rest.trim().is_empty() {
            break;
        }
        // cut at the first whitespace at/after the balance target
        // (char_indices keeps every cut on a character boundary); a
        // whitespace-free tail keeps the remainder in one column
        let cut = rest
            .char_indices()
            .find(|(i, c)| *i >= target && c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(cut);
        out.push(head.trim().to_string());
        rest = tail;
    }
    if !rest.trim().is_empty() {
        out.push(rest.trim().to_string());
    }
    out
}

/// The uploads table's "expires in" cell: uploads are ephemeral like chat,
/// so the share ages out of the read contract at `expires_ts` (share time +
/// the org's chat retention window). 0 = unknown age, no deadline; an
/// unavailable share has nothing left to expire (both "—").
fn expires_label(now: u64, expires_ts: u64, available: bool) -> String {
    if !available || expires_ts == 0 {
        return "—".to_string();
    }
    if expires_ts <= now {
        return "expired".to_string();
    }
    let left = expires_ts - now;
    if left < 3600 {
        format!("in {} min", (left / 60).max(1))
    } else if left < 86_400 {
        format!("in {} h", left / 3600)
    } else {
        let d = left / 86_400;
        format!("in {d} day{}", if d == 1 { "" } else { "s" })
    }
}

/// The file's own date as a local calendar day, e.g. `"2026-07-01"`.
fn file_date_label(ts: u64) -> String {
    let Ok(secs) = i64::try_from(ts) else {
        return String::new();
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    utc.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// The whole-log pass over a chat: author-block zebra (the stripe flips
/// whenever a DIFFERENT author takes over), the once-per-block header flag,
/// and the quote teasers. Id-based quotes resolve their teaser through
/// `quotes` (built over the FULL log — a cross-channel quote teases even
/// when its target is filtered out of view) and their jump row by scanning
/// the displayed log; legacy numeric quotes resolve by row as before.
fn annotate_chat_log(log: &mut [LogLineData], quotes: &HashMap<String, QuoteSrc>) {
    let mut alt = false;
    let mut prev_lead: Option<String> = None;
    for line in log.iter_mut() {
        if line.system {
            // a governance line is transparent to the author-block rhythm:
            // the surrounding block keeps its stripe and shows no header
            line.first = false;
            line.alt = alt;
            continue;
        }
        if prev_lead.as_deref().is_some_and(|p| p != line.lead) {
            alt = !alt;
        }
        line.alt = alt;
        // the author header (name + time) shows once per author block
        line.first = prev_lead.as_deref() != Some(line.lead.as_str());
        prev_lead = Some(line.lead.clone());
    }
    // id → displayed row: the jump target of an in-view quote
    let row_of: HashMap<String, usize> = log
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.id.is_empty())
        .map(|(i, l)| (l.id.clone(), i))
        .collect();
    for i in 0..log.len() {
        if !log[i].quote_id.is_empty() {
            let qid = log[i].quote_id.clone();
            log[i].quote = row_of
                .get(&qid)
                .and_then(|r| i32::try_from(*r).ok())
                .unwrap_or(-1);
            match quotes.get(&qid) {
                Some(src) if !src.deleted => {
                    log[i].quote_label = format!("{}: {}", src.lead, src.text);
                }
                Some(src) => log[i].quote_label = format!("{}: …", src.lead),
                // not in the full-log map either: dangling — drop the quote
                None => log[i].quote = -1,
            }
        } else if log[i].quote >= 0 {
            // legacy numeric quote (pre-chat-bus rows; B1 resolves these to
            // quote_id at ingest, after which this path goes dormant)
            let q = log[i].quote;
            match usize::try_from(q).ok().and_then(|q| log.get(q)) {
                Some(src) if src.deleted_by.is_empty() => {
                    log[i].quote_label = format!("{}: {}", src.lead, src.text);
                }
                Some(src) => log[i].quote_label = format!("{}: …", src.lead),
                None => log[i].quote = -1,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chat channels (chat bus, package B4): pure projection helpers. All of
// these are engine-data driven — the UI never invents channel state.
// ---------------------------------------------------------------------------

/// The stable string form of a channel across the Rust↔Slint boundary:
/// `"group"`, `"patch:<id>"`, `"topic:<name>"`. Sidebar rows carry it; the
/// `select-channel` callback hands it back.
fn channel_key(c: &ChannelRef) -> String {
    match c {
        ChannelRef::Group => "group".to_string(),
        ChannelRef::Patch { id } => format!("patch:{}", id.0),
        ChannelRef::Topic { name } => format!("topic:{name}"),
    }
}

/// Parse a sidebar channel key back into a [`ChannelRef`]. `None` on junk —
/// a stale or malformed key must never panic the UI.
fn parse_channel_key(key: &str) -> Option<ChannelRef> {
    if key == "group" {
        return Some(ChannelRef::Group);
    }
    if let Some(id) = key.strip_prefix("patch:") {
        return id.parse().ok().map(|id| ChannelRef::Patch { id: ProposalId(id) });
    }
    key.strip_prefix("topic:").map(|name| ChannelRef::Topic {
        name: name.to_string(),
    })
}

/// The sidebar's DISCUSSION rows from the engine enumeration: a discussion
/// is vote-bound, so only patch channels whose proposal is still OPEN
/// (something can be voted on — [`KnownFate::Pending`]) appear, by
/// ascending proposal id with the proposal-state title. No group row (the
/// Gruppe view above covers it), no sealed/closed votes, no unknown
/// proposals, no free topics — the engine's channel enumeration itself
/// stays complete (MCP reads it unfiltered).
fn derive_channels(
    lang: i32,
    infos: &[ChannelInfo],
    known: &HashMap<u64, KnownProposal>,
    unread: &HashMap<String, usize>,
) -> Vec<ChannelRowData> {
    let unread_of =
        |key: &str| i32::try_from(unread.get(key).copied().unwrap_or(0)).unwrap_or(i32::MAX);
    let mut patches: Vec<u64> = Vec::new();
    for i in infos {
        if let ChannelRef::Patch { id } = &i.channel {
            patches.push(id.0);
        }
    }
    patches.sort_unstable();
    patches.dedup();
    patches
        .into_iter()
        .filter_map(|id| {
            let k = known.get(&id)?;
            if k.fate != KnownFate::Pending {
                return None;
            }
            let key = format!("patch:{id}");
            Some(ChannelRowData {
                unread: unread_of(&key),
                key,
                label: display_title(lang, &k.payload),
                icon: "🗳️".to_string(),
            })
        })
        .collect()
}

/// The command the patch-channel banner's "back to the vote" button
/// issues: a discussion is vote-bound (the channel key IS the proposal
/// id), so the jump reuses the sidebar's own navigation verbs — an
/// Organization ballot lives in the pending view (declined once closed;
/// an applied one simply left the list), a gated surface hosts its cards
/// on its main view (plain surface selection). A cache miss falls back to
/// Organization → pending rather than a dead button. Non-patch channels
/// have no vote.
fn vote_jump_command(ch: &ChannelRef, known: &HashMap<u64, KnownProposal>) -> Option<Command> {
    let ChannelRef::Patch { id } = ch else {
        return None;
    };
    let (surface, fate) = known
        .get(&id.0)
        .map(|k| (k.surface, k.fate))
        .unwrap_or((Surface::Organization, KnownFate::Pending));
    Some(if matches!(surface, Surface::Organization) {
        let view = match fate {
            KnownFate::Closed => "declined",
            // WP1: an applied Organization vote's row lives in the
            // accepted (applied-log) view — the jump lands on it
            KnownFate::Applied => "accepted",
            KnownFate::Pending => "pending",
        };
        Command::SelectView { surface, view: view.to_string() }
    } else {
        Command::SelectSurface { surface }
    })
}

/// The compose-banner label of the selected channel ("" = group, which
/// needs no banner). For a fresh topic this is the ONLY visible feedback
/// until its first message exists (a channel exists because a message
/// exists), so it must not depend on the sidebar list.
fn channel_display_label(c: &ChannelRef, titles: &HashMap<u64, String>) -> String {
    match c {
        ChannelRef::Group => String::new(),
        ChannelRef::Patch { id } => titles
            .get(&id.0)
            .cloned()
            .unwrap_or_else(|| format!("#{}", id.0)),
        ChannelRef::Topic { name } => name.clone(),
    }
}

/// Whether the selected channel is a DECIDED vote's discussion — read-only
/// for new messages/shares (the engine refuses them with
/// `DiscussionClosed`; this flag collapses the compose row and shows the
/// banner note BEFORE anyone types into a refusal). The engine's channel
/// annotation ([`ChannelInfo::state`]) is authoritative when present; a
/// channel not (yet) in the enumeration — or an unannotated ref — falls
/// back to the UI's proposal cache ([`KnownProposal::fate`]). Group/Topic,
/// open votes and unknown referents are writable (`false`).
fn selected_channel_closed(
    selected: &ChannelRef,
    infos: &[ChannelInfo],
    known: &HashMap<u64, KnownProposal>,
) -> bool {
    let ChannelRef::Patch { id } = selected else {
        return false;
    };
    if let Some(state) = infos
        .iter()
        .find(|i| &i.channel == selected)
        .and_then(|i| i.state)
    {
        return state != molt_core::ProposalState::Proposed;
    }
    known.get(&id.0).is_some_and(|k| k.fate != KnownFate::Pending)
}

/// What the UI remembers about a proposal beyond the read contract's
/// Proposed-only `pending` window. The engine never re-exposes a terminal
/// proposal (a sealed block's `applied` value is the bare payload, without
/// the proposal id), so title and governance state would vanish from the
/// patch channel the moment a block seals — this cache keeps them.
#[derive(Clone)]
struct KnownProposal {
    /// The full payload; the fate probe matches it against the applied log.
    /// Titles are NOT cached: they render from the payload's machine
    /// placeholder in the active language at display time
    /// ([`display_title`]) — a cached rendered string would freeze the
    /// language of the moment it was seen.
    payload: serde_json::Value,
    /// The gated surface the proposal targets (whose applied log to probe).
    surface: Surface,
    /// Approvals at the last sighting in `pending`.
    approvals: usize,
    /// The threshold at the last sighting.
    threshold: usize,
    /// The lifecycle as this UI resolved it (see [`KnownFate`]).
    fate: KnownFate,
}

/// The UI-side proposal lifecycle, resolved from the data the read
/// contract exposes (the contract itself is frozen — no engine change).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KnownFate {
    /// Still in the engine's Proposed-only `pending` read.
    Pending,
    /// Vanished from `pending` and its payload appeared in the surface's
    /// applied log — the block sealed.
    Applied,
    /// Vanished without an applied trace. The read contract cannot
    /// distinguish Rejected from expired/otherwise closed, so the UI
    /// renders a neutral closed marker — never a fabricated verdict.
    Closed,
}

/// Fold one read pass into the proposal cache: every pending proposal is
/// (re-)cached, and every cached proposal that vanished from the
/// Proposed-only window resolves its fate by probing the applied log of
/// its surface. Applied values are the raw proposal payloads (both the
/// chain projection and the legacy simulation push `payload` verbatim, and
/// neither embeds the proposal id), so payload equality is the only match
/// the read contract allows — two byte-identical proposals are therefore
/// indistinguishable here, which at worst upgrades a closed twin to ✓.
/// `Applied` is sticky; `Closed` re-probes, so an out-of-order read that
/// briefly missed the applied value corrects itself on the next pass. A
/// surface missing from `applied` (failed read) resolves nothing.
fn update_known_proposals(
    known: &mut HashMap<u64, KnownProposal>,
    pending: &[ProposalView],
    declined: &[ProposalView],
    applied: &HashMap<Surface, Vec<serde_json::Value>>,
) {
    for p in pending {
        known.insert(
            p.id.0,
            KnownProposal {
                payload: p.payload.clone(),
                surface: p.surface,
                approvals: p.approvals,
                threshold: p.threshold,
                fate: KnownFate::Pending,
            },
        );
    }
    for (id, k) in known.iter_mut() {
        if pending.iter().any(|p| p.id.0 == *id) || k.fate == KnownFate::Applied {
            continue;
        }
        let Some(vals) = applied.get(&k.surface) else {
            continue;
        };
        k.fate = if vals.contains(&k.payload) {
            KnownFate::Applied
        } else {
            KnownFate::Closed
        };
    }
    // the snapshots' declined lists are AUTHORITATIVE Rejected knowledge
    // (the engine names the veto) — fold them last: a veto this UI never
    // saw pending still gets a titled Closed entry, an out-of-order
    // payload-probe verdict is overridden, but an Applied fate is never
    // downgraded (the probe proved the seal; the byte-identical-twin
    // ambiguity must not un-seal it here).
    for p in declined {
        let entry = known.entry(p.id.0).or_insert_with(|| KnownProposal {
            payload: p.payload.clone(),
            surface: p.surface,
            approvals: p.approvals,
            threshold: p.threshold,
            fate: KnownFate::Closed,
        });
        if entry.fate != KnownFate::Applied {
            entry.payload = p.payload.clone();
            entry.fate = KnownFate::Closed;
        }
    }
}

/// The lazy patch-channel titles (sidebar rows + compose banner), from the
/// proposal cache — so a title survives the proposal leaving `pending`.
fn known_titles(lang: i32, known: &HashMap<u64, KnownProposal>) -> HashMap<u64, String> {
    known
        .iter()
        .map(|(id, k)| (*id, display_title(lang, &k.payload)))
        .collect()
}

/// Quote-teaser sources over the FULL chat log, keyed by hex message id.
fn quote_sources(msgs: &[ChatMessage]) -> HashMap<String, QuoteSrc> {
    msgs.iter()
        .filter(|m| !m.id.is_nil())
        .map(|m| {
            (
                m.id.to_string(),
                QuoteSrc {
                    lead: m.from.clone(),
                    text: m.body.clone(),
                    deleted: m.deleted_by.is_some(),
                },
            )
        })
        .collect()
}

/// A UI-synthesized governance line (P8): no author, no id — so the
/// id-requiring row actions stay hidden by the same guard that protects
/// legacy rows — rendered quiet via the `system` flag. The text is
/// deliberately symbols + numbers + user content ("⚖ #4 · title — 2/3"),
/// so it reads the same in every language and needs no lexicon entry.
fn system_line_data(text: String) -> LogLineData {
    LogLineData {
        id: String::new(),
        lead: String::new(),
        text,
        when: String::new(),
        quote: -1,
        quote_id: String::new(),
        system: true,
        quote_label: String::new(),
        deleted_by: String::new(),
        first: false,
        own: false,
        alt: false,
        mine_emoji: String::new(),
        reactions: Vec::new(),
        has_file: false,
        file_name: String::new(),
        file_meta: String::new(),
        file_available: false,
        proposal_id: None,
    }
}

/// The system lines of one `Patch(id)` channel, synthesized from proposal
/// state (P8 — a UI-side merge, no engine/wire change). Proposals carry no
/// timestamp, so lines are stamped with the UI's FIRST-SEEN time — an
/// approximation that keeps a line stable within a session and near the
/// governance moment it reports (0 = never seen: sorts to the top). A
/// proposal no longer in the Proposed-only `pending` window renders from
/// the [`KnownProposal`] cache: sealed shows `m/m ✓` (the engine seals a
/// block at exactly the threshold), vanished-without-apply shows the
/// neutral `⊘` (Rejected vs expired is not distinguishable from the read
/// contract). An id known nowhere yields a bare `⚖ #id` line and never an
/// error (concept Q4).
fn patch_system_lines(
    lang: i32,
    patch: u64,
    pending: &[ProposalView],
    known: &HashMap<u64, KnownProposal>,
    first_seen: &HashMap<u64, u64>,
) -> Vec<(u64, LogLineData)> {
    let text = match pending.iter().find(|p| p.id.0 == patch) {
        Some(p) => format!(
            "⚖ #{patch} · {} — {}/{}",
            display_title(lang, &p.payload),
            p.approvals,
            p.threshold
        ),
        None => match known.get(&patch) {
            Some(k) => {
                let progress = match k.fate {
                    KnownFate::Applied => format!("{}/{} ✓", k.threshold, k.threshold),
                    KnownFate::Closed => "⊘".to_string(),
                    KnownFate::Pending => format!("{}/{}", k.approvals, k.threshold),
                };
                format!("⚖ #{patch} · {} — {progress}", display_title(lang, &k.payload))
            }
            None => format!("⚖ #{patch}"),
        },
    };
    let ts = first_seen.get(&patch).copied().unwrap_or(0);
    vec![(ts, system_line_data(text))]
}

/// Merge the system lines into the chat lines by timestamp. The chat log's
/// own order is authoritative (it is the engine's log order) and is never
/// disturbed; a system line ties BEFORE the chat line of the same second.
fn merge_by_time(
    chat: Vec<(u64, LogLineData)>,
    mut system: Vec<(u64, LogLineData)>,
) -> Vec<LogLineData> {
    system.sort_by_key(|(ts, _)| *ts); // stable: equal stamps keep their order
    let mut out = Vec::with_capacity(chat.len() + system.len());
    let mut sys = system.into_iter().peekable();
    for (ts, line) in chat {
        while sys.peek().is_some_and(|(sts, _)| *sts <= ts) {
            out.push(sys.next().expect("peeked").1);
        }
        out.push(line);
    }
    out.extend(sys.map(|(_, l)| l));
    out
}

impl UnreadLedger {
    /// Fold one fresh per-channel count set into the ledger and return the
    /// unread count per channel key. The selected channel is always marked
    /// read ("reset on channel selection", and messages arriving while a
    /// channel is on screen are being read); the first observation seeds
    /// the ledger so an opened workspace starts caught-up.
    fn observe(&mut self, counts: &[(String, usize)], selected: &str) -> HashMap<String, usize> {
        if !self.seeded {
            self.last_seen = counts.iter().cloned().collect();
            self.seeded = true;
        }
        let selected_count = counts
            .iter()
            .find(|(k, _)| k == selected)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        self.last_seen.insert(selected.to_string(), selected_count);
        counts
            .iter()
            .map(|(k, c)| {
                (
                    k.clone(),
                    c.saturating_sub(self.last_seen.get(k).copied().unwrap_or(0)),
                )
            })
            .collect()
    }
}

/// A short human label for a surface transition payload: the human title
/// alone — the op code stays wire-side (nobody proposes "set_image", they
/// propose "Logo der Republik ändern"). The op is only the fallback when a
/// payload (e.g. a minimal MCP proposal) carries no display key at all.
fn summarize(v: &serde_json::Value) -> String {
    if let Some(obj) = v.as_object() {
        for key in ["title", "label", "memo", "note", "text", "name", "summary"] {
            if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
                return s.to_string();
            }
        }
        if let Some(o) = obj.get("op").and_then(serde_json::Value::as_str) {
            return o.to_string();
        }
    }
    v.to_string()
}

/// The localized label of an Organization governance op (`None` = not a
/// governance op). These live HERE, not in the payload: the payload carries
/// only the machine `op` placeholder, so every UI renders the title in its
/// own active language (a pre-rendered string would freeze the proposer's
/// language and mix languages across the group).
fn org_op_label(lang: i32, op: &str) -> Option<&'static str> {
    Some(match (lang, op) {
        (1, "set_name") => "Name ändern",
        (_, "set_name") => "Rename",
        (1, "set_charter") => "Satzung ändern",
        (_, "set_charter") => "Change the charter",
        (1, "set_image") => "Logo der Republik ändern",
        (_, "set_image") => "Change the republic's image",
        (1, "remove_image") => "Logo der Republik entfernen",
        (_, "remove_image") => "Remove the republic's image",
        (1, "set_chat_retention") => "Löschfrist für Chat-Logs ändern",
        (_, "set_chat_retention") => "Change when chat logs are deleted",
        _ => return None,
    })
}

/// The display title of a proposal payload, in the ACTIVE language: an org
/// governance op renders from its machine placeholder via [`org_op_label`]
/// (even when a legacy payload carries a baked title in some language);
/// everything else falls back to the payload's own user content
/// ([`summarize`]).
fn display_title(lang: i32, v: &serde_json::Value) -> String {
    v.get("op")
        .and_then(serde_json::Value::as_str)
        .and_then(|op| org_op_label(lang, op))
        .map(str::to_string)
        .unwrap_or_else(|| summarize(v))
}

/// Localized surface label for the sidebar (0 = English, 1 = German) —
/// presentation, like [`seat_state_label`]; the machine key stays
/// [`Surface::as_str`].
fn surface_name(lang: i32, sf: Surface) -> &'static str {
    if lang == 1 {
        match sf {
            Surface::Organization => "Organisation",
            Surface::Chat => "Chat",
            Surface::Memory => "Shared Memory",
            Surface::Quests => "Quests",
            Surface::Vault => "Tresor",
            Surface::Wallet => "Wallet",
        }
    } else {
        match sf {
            Surface::Organization => "Organization",
            Surface::Chat => "Chat",
            Surface::Memory => "Shared Memory",
            Surface::Quests => "Quests",
            Surface::Vault => "Vault",
            Surface::Wallet => "Wallet",
        }
    }
}

/// Localized sub-view label for a nav row. The English display label comes
/// from the shared `molt-core` vocabulary ([`Surface::views`]); German maps
/// by the machine key here — keys repeating across surfaces (archive,
/// proposals, status, …) deliberately share one word.
fn view_label(lang: i32, key: &str, en: &str) -> String {
    if lang != 1 {
        return en.to_string();
    }
    match key {
        "members" => "Mitglieder",
        "uploads" => "Uploads",
        "pending" => "Ausstehend",
        "declined" => "Abgelehnt",
        "today" => "Allgemein",
        "archive" => "Archiv",
        "proposals" => "Vorschläge",
        "accepted" => "Angenommen",
        "denied" => "Abgelehnt",
        "create" => "Erstellen",
        "my-quests" => "Meine Quests",
        "secrets" => "Geheimnisse",
        "disclose" => "Offenlegen",
        "exposed" => "Offengelegt",
        "balance" => "Kontostand",
        "history" => "Verlauf",
        "send" => "Senden",
        "receive" => "Empfangen",
        "settings" => "Einstellungen",
        // Status, Brain, Board — shared or product terms
        _ => en,
    }
    .to_string()
}

/// The default transition op the GUI uses when proposing on a surface.
fn default_op(sf: Surface) -> &'static str {
    match sf {
        Surface::Memory => "add_note",
        Surface::Quests => "add_quest",
        Surface::Vault => "seal_secret",
        Surface::Wallet => "transfer",
        // organization changes come from the dedicated edit modals
        // (org-propose carries the specific op); chat is ungated
        Surface::Chat | Surface::Organization => "note",
    }
}

fn to_screen(s: AppScreen) -> Screen {
    match s {
        AppScreen::Choice => Screen::Choice,
        AppScreen::Create => Screen::Create,
        AppScreen::Open => Screen::Open,
        AppScreen::Join => Screen::Join,
        AppScreen::Restore => Screen::Restore,
        AppScreen::Settings => Screen::Settings,
        AppScreen::Main => Screen::Main,
    }
}

fn from_screen(s: Screen) -> AppScreen {
    match s {
        Screen::Choice => AppScreen::Choice,
        Screen::Create => AppScreen::Create,
        Screen::Open => AppScreen::Open,
        Screen::Join => AppScreen::Join,
        Screen::Restore => AppScreen::Restore,
        Screen::Settings => AppScreen::Settings,
        Screen::Main => AppScreen::Main,
    }
}

/// Map a theme name to the Theme global's index.
fn theme_index(s: &str) -> i32 {
    match s {
        "classic" => 0,
        "brutalism" => 2,
        _ => 1,
    }
}

/// Map a theme index back to its name.
fn theme_name(i: i32) -> String {
    match i {
        0 => "classic",
        2 => "brutalism",
        _ => "dark",
    }
    .to_string()
}

/// Map an alert-sound name to its ComboBox index (none/bell/chime/pop).
fn sound_index(s: &str) -> i32 {
    match s {
        "bell" => 1,
        "chime" => 2,
        "pop" => 3,
        _ => 0,
    }
}

/// Map a ComboBox index back to an alert-sound name.
fn sound_name(i: i32) -> String {
    match i {
        1 => "bell",
        2 => "chime",
        3 => "pop",
        _ => "none",
    }
    .to_string()
}

/// The last time an alert actually played — a debounce so a reconnect
/// catch-up of hundreds of queued messages cannot spawn a player storm.
static LAST_ALERT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Play a short alert sound, fire-and-forget. The sample is synthesized in
/// pure Rust (a tiny WAV, cached in the temp dir) and handed to the system
/// player — pw-play/paplay/aplay, runtime-detected, silently a no-op when
/// none exists. Deliberately NO compiled audio stack: cpal/rodio would pull
/// ALSA's C bindings, and the pure-Rust posture stands (CLAUDE.md).
///
/// Total-review hardening: (1) at most one alert per 400 ms (a message
/// burst plays once, not hundreds of times); (2) ALL work — the first-play
/// WAV synthesis and the player spawn — runs on a detached thread, never
/// the caller's UI/runtime thread; (3) the spawned player is REAPED (its
/// `wait()` runs on that thread) so no zombies accumulate.
fn play_alert(kind: &str) {
    if kind == "none" || kind.is_empty() {
        return;
    }
    {
        let now = std::time::Instant::now();
        let mut last = match LAST_ALERT.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(400)) {
            return;
        }
        *last = Some(now);
    }
    let kind = kind.to_string();
    std::thread::spawn(move || {
        // a per-process, per-kind path (the pid keeps two instances from
        // racing the same file, and avoids trusting a world-writable name
        // another local user could pre-plant)
        let path = std::env::temp_dir()
            .join(format!("molt-alert-{}-{kind}.wav", std::process::id()));
        if !path.exists() && write_alert_wav(&path, &kind).is_err() {
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
                    let _ = child.wait(); // reap — no zombie
                    return;
                }
                Err(_) => continue,
            }
        }
    });
}

/// Synthesize one alert as a 44.1 kHz mono 16-bit WAV: a few decaying
/// sine partials per kind — bell (bright fifth), chime (rising triad),
/// pop (short thump).
fn write_alert_wav(path: &std::path::Path, kind: &str) -> std::io::Result<()> {
    let (freqs, dur): (&[f32], f32) = match kind {
        "bell" => (&[880.0, 1318.5], 0.35),
        "chime" => (&[523.25, 659.25, 783.99], 0.5),
        _ => (&[220.0, 440.0], 0.12), // pop
    };
    let rate = 44_100u32;
    let n = (dur * rate as f32) as usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let env = (-6.0 * t / dur).exp();
        let mut v = 0.0f32;
        for (k, f) in freqs.iter().enumerate() {
            // chime arpeggiates: each partial enters a beat later
            let start = if kind == "chime" { k as f32 * 0.12 } else { 0.0 };
            if t >= start {
                v += ((t - start) * f * std::f32::consts::TAU).sin() * env;
            }
        }
        let v = (v / freqs.len() as f32 * 0.4 * f32::from(i16::MAX)) as i16;
        samples.push(v);
    }
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(0);
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, wav)
}

/// Map an anonymity-network name to its ComboBox index. The dropdown
/// offers tor and none (nym was removed from the UI 2026-07-18 — never
/// implemented); a lingering "nym" in an old config displays as none
/// (fail-closed would silently DIAL, so the honest reading is "no
/// anonymity network configured").
fn net_index(s: &str) -> i32 {
    match s {
        "none" | "nym" => 1,
        _ => 0,
    }
}

/// Map a ComboBox index back to an anonymity-network name.
fn net_name(i: i32) -> String {
    match i {
        1 => "none",
        _ => "tor",
    }
    .to_string()
}

/// Map a Tor-mode name to its ComboBox index.
fn mode_index(s: &str) -> i32 {
    match s {
        "embedded" => 1,
        "whonix" => 2,
        _ => 0,
    }
}

/// Map a ComboBox index back to a Tor-mode name.
fn mode_name(i: i32) -> String {
    match i {
        1 => "embedded",
        2 => "whonix",
        _ => "local",
    }
    .to_string()
}

/// The tor-mode dropdown's per-row `enabled` flags (parallel to the model
/// `["local", "embedded", "whonix"]`). `local` and `whonix` route to a system
/// SOCKS proxy and are always available; `embedded` needs the in-process arti
/// dialer, which only exists when the binary was built with the `embedded-tor`
/// feature — so it is greyed (like nym) unless `embedded_available` is true
/// (the compile-time truth crossing the app→ui seam, P3).
fn tor_mode_enabled(embedded_available: bool) -> [bool; 3] {
    [true, embedded_available, true]
}

/// Map the transport-health state onto the header "chat" pill's tone index and
/// hover tooltip (P6). Tone index: `0` = good/green, `1` = warn/amber,
/// `2` = bad/red. The nominal `Ok` state carries no tooltip; the impaired and
/// down states carry the engine's reason string.
fn net_health_pill(health: &NetHealth) -> (i32, String) {
    match health {
        NetHealth::Ok => (0, String::new()),
        NetHealth::Degraded { reason } => (1, reason.clone()),
        NetHealth::Down { reason } => (2, reason.clone()),
    }
}

/// Declare the whole localized string table in ONE place: the macro
/// generates the `Lexicon` struct, its English and German tables, and
/// `apply_strings` (which pushes every entry into the Slint `Strings`
/// global). Adding a string = one line here + its declaration in
/// `theme.slint` — the four-places-per-string era is over.
macro_rules! lexicon {
    ($( $field:ident: $en:expr, $de:expr; )+) => {
        /// The full localized string table for one language.
        struct Lexicon {
            $( $field: &'static str, )+
        }

        impl Lexicon {
            fn en() -> Self {
                Lexicon { $( $field: $en, )+ }
            }

            fn de() -> Self {
                Lexicon { $( $field: $de, )+ }
            }
        }

        /// Push the localized string table for `lang` (0 = English,
        /// 1 = German) into the Slint `Strings` global.
        fn apply_strings(ui: &AppWindow, lang: i32) {
            let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
            let s = ui.global::<Strings>();
            paste::paste! {
                $( s.[<set_ $field>](l.$field.into()); )+
            }
        }
    };
}

lexicon! {
    choice_title: "Welcome", "Willkommen";
    choice_subtitle: "Choose how to begin.", "Wähle, wie du beginnen möchtest.";
    choice_mock_note: "Workspaces are stored encrypted in the workspace folder (see Settings).", "Workspaces werden verschlüsselt im Workspace-Ordner gespeichert (siehe Einstellungen).";
    choice_group_republic: "New republic", "Neue Republik";
    // card titles split as hotkey letter + rest: the letter renders
    // underlined and typing it activates the card
    choice_create_key: "C", "G";
    choice_create_rest: "reate", "ründen";
    choice_create_sub: "A new workspace", "Workspace erstellen";
    choice_open_key: "O", "Ö";
    choice_open_rest: "pen", "ffnen";
    choice_open_sub: "Open a local workspace", "Lokalen Workspace öffnen";
    choice_join_key: "J", "B";
    choice_join_rest: "oin", "eitreten";
    choice_join_sub: "Via an invite link", "Einem Workspace per Einladungslink beitreten";
    choice_restore_key: "R", "W";
    choice_restore_rest: "estore", "iederherstellen";
    choice_restore_sub: "With your phrase — from a backup or after device loss", "Mit deiner Phrase — aus Backup oder nach Geräteverlust";
    nav_back: "Back", "Zurück";
    field_network: "Anonymity network", "Anonymitäts-Netzwerk";
    not_implemented_yet: "not yet", "noch nicht";
    field_tor_mode: "Tor mode", "Tor-Modus";
    field_tor_port: "Tor SOCKS port", "Tor-SOCKS-Port";
    field_smp_server: "SMP messaging server", "SMP-Nachrichtenserver";
    smp_public: "Public default", "Öffentlicher Standard";
    smp_custom: "Custom server", "Eigener Server";
    field_smp_url: "Server URL", "Server-URL";
    smp_test: "Test connection", "Verbindung testen";
    smp_test_tip: "Dials over the configured transport — Tor when it is enabled, the server's onion host if it advertises one.", "Verbindet über den konfigurierten Transport — via Tor, wenn aktiviert, und über den Onion-Host des Servers, falls vorhanden.";
    smp_untested: "not tested yet", "noch nicht getestet";
    smp_testing: "testing…", "teste…";
    smp_ok: "reachable ✓", "erreichbar ✓";
    smp_hint: "The founding ritual and group messages route over this SMP server. The public default needs no server of your own; a custom URL looks like smp://<fingerprint>@host.", "Das Gründungsritual und Gruppennachrichten laufen über diesen SMP-Server. Der öffentliche Standard braucht keinen eigenen Server; eine eigene URL sieht aus wie smp://<fingerprint>@host.";
    field_threshold: "Threshold (m)", "Schwelle (m)";
    field_members: "Members (n)", "Mitglieder (n)";
    field_language: "Language", "Sprache";
    field_theme: "Theme", "Design";
    field_workspace_dir: "Workspace directory", "Workspace-Verzeichnis";
    field_mcp_port: "MCP port", "MCP-Port";
    field_mcp_allow: "Allowed client IPs", "Erlaubte Client-IPs";
    field_mcp_token: "API token", "API-Token";
    set_rotate: "Rotate", "Rotieren";
    set_token_note: "Clients send this as the token in their initialize request. Rotating saves a fresh token to config.toml; the running MCP endpoint picks it up on the next restart.", "Clients senden dies als token im initialize-Request. Rotieren speichert ein frisches Token in die config.toml; der laufende MCP-Endpunkt übernimmt es beim nächsten Neustart.";
    set_token_show: "Reveal", "Anzeigen";
    set_token_hide: "Hide", "Verbergen";
    field_headless: "Headless (MCP only, no GUI)", "Headless (nur MCP, keine GUI)";
    cw_title: "Found a new Republic", "Neue Republik gründen";
    cw_grp_republic: "Workspace", "Workspace";
    ph_ws_name: "My new republic", "Meine neue Republik";
    ph_member: "my name", "mein Name";
    ph_seed: "word1 word2 word3 …", "wort1 wort2 wort3 …";
    cw_republic_hint: "Its name, and the handle the other members will see you by.", "Ihr Name und das Handle, unter dem dich die anderen Mitglieder sehen.";
    cw_grp_rule: "Approval Rules", "Zustimmungsregeln";
    cw_rule_hint: "Gated changes apply only once enough members approve.", "Geschützte Änderungen gelten erst, wenn genug Mitglieder zustimmen.";
    cw_rule_warn: "not recommended", "nicht empfohlen";
    cw_rule_a: "Every gated change needs", "Jede geschützte Änderung braucht";
    cw_rule_b: "of", "von";
    cw_rule_c: "approvals.", "Stimmen.";
    cw_grp_transport: "Anonymization Layer", "Anonymisierungsschicht";
    cw_transport_hint: "How this node reaches the other members.", "Wie dieser Node die anderen Mitglieder erreicht.";
    cw_net_ok_tor: "Anonymized via Tor circuits.", "Anonymisiert via Tor-Circuits.";
    cw_net_warn: "Not anonymized — peers see your IP.", "Nicht anonymisiert — Peers sehen deine IP.";
    cw_found: "Begin ritual", "Ritual beginnen";
    cw_invites: "Invites", "Einladungen";
    cw_invites_hint: "One link per future member — share each once, over a private channel.", "Ein Link pro künftigem Mitglied — jeden nur einmal teilen, über einen privaten Kanal.";
    cw_members_title: "Members", "Mitglieder";
    cw_sealed_word: "sealed", "versiegelt";
    cw_sim_badge: "SIMULATION", "SIMULATION";
    cw_ritual_hint: "Share each link once, over a private channel. The republic is created once every member has activated their link and signed the roster.", "Teile jeden Link einmal, über einen privaten Kanal. Die Republik entsteht, sobald jedes Mitglied seinen Link aktiviert und die Mitgliederliste signiert hat.";
    cw_ritual_hint_sim: "No real network yet: this node simulates the other members — it auto-activates and signs for them. Nothing is shared with anyone. Real members over SMP arrive with T3.", "Noch kein echtes Netzwerk: dieser Knoten simuliert die anderen Mitglieder — er aktiviert und signiert selbst für sie. Es wird nichts mit jemandem geteilt. Echte Mitglieder über SMP kommen mit T3.";
    cw_log_title: "Ritual log", "Ritual-Protokoll";
    cw_charter_title: "Agree on the charter", "Auf die Satzung einigen";
    cw_charter_name_ph: "Final republic name", "Endgültiger Name der Republik";
    cw_charter_agenda_ph: "Agenda / charter — what this republic is for", "Agenda / Satzung — wofür diese Republik steht";
    cw_charter_hint: "Every member has joined. Propose the final name and a charter; each member ratifies it with their signature before the workspace opens.", "Alle Mitglieder sind beigetreten. Schlage den endgültigen Namen und eine Satzung vor; jedes Mitglied ratifiziert sie mit seiner Signatur, bevor der Workspace aufgeht.";
    cw_propose: "Propose & seal", "Vorschlagen & versiegeln";
    jw_ratify_title: "Ratify the charter", "Satzung ratifizieren";
    jw_ratify_hint: "The founder proposed this name and charter. Confirm to add your signature and join; the workspace opens once every member has ratified.", "Der Gründer hat diesen Namen und diese Satzung vorgeschlagen. Bestätige, um deine Signatur beizusteuern und beizutreten; der Workspace geht auf, sobald jedes Mitglied ratifiziert hat.";
    jw_ratify_confirm: "Confirm & join", "Bestätigen & beitreten";
    jw_ratify_decline: "Decline", "Ablehnen";
    jw_ratify_agenda_empty: "(no agenda set)", "(keine Agenda festgelegt)";
    const_immutable: "Immutable · ratified by everyone at founding", "Unveränderlich · von allen bei der Gründung ratifiziert";
    const_charter_title: "Charter", "Satzung";
    const_no_agenda: "(founded without a written charter)", "(ohne schriftliche Satzung gegründet)";
    const_signatories: "Founding members · ratified by all", "Gründungsmitglieder · von allen ratifiziert";
    enter_republic: "Enter republic", "Republik betreten";
    org_reachable: "reachable", "erreichbar";
    org_approvals: "Approvals", "Approvals";
    oa_col_surface: "Surface", "Bereich";
    oa_pending: "Pending", "Offen";
    oa_denied: "Denied", "Abgelehnt";
    oa_list_pending: "List pending", "Offene zeigen";
    org_edit: "Edit", "Bearbeiten";
    ol_title: "Republic image", "Bild der Republik";
    ol_body: "Pick a new image via the file dialog, or remove the current one. Either way the change is a gated proposal the members approve by threshold. The image itself (up to 256 KiB) travels inside the proposal, so every member sees exactly what they approve — once applied, it shows on every device.", "Wähle über den Datei-Dialog ein neues Bild oder entferne das aktuelle. Beides ist eine geschützte Änderung, der die Mitglieder per Schwelle zustimmen. Das Bild selbst (bis 256 KiB) reist im Vorschlag mit — jedes Mitglied sieht genau, worüber es abstimmt; nach dem Anwenden erscheint es auf jedem Gerät.";
    ol_remove: "Remove image", "Bild entfernen";
    ol_current: "Current image", "Aktuelles Bild";
    ol_none: "No image set.", "Kein Bild gesetzt.";
    ol_pick: "Choose…", "Auswählen…";
    oc_title: "Edit charter", "Satzung bearbeiten";
    oc_body: "The charter was ratified by everyone at the founding — an edit is a gated change: the draft becomes a proposal the members approve by threshold. Once applied, every view shows the new charter; the founding charter stays immutable in block 0.", "Die Satzung wurde bei der Gründung von allen ratifiziert — eine Bearbeitung ist eine geschützte Änderung: der Entwurf wird ein Vorschlag, dem die Mitglieder per Schwelle zustimmen. Nach dem Anwenden zeigt jede Ansicht die neue Satzung; die Gründungssatzung bleibt unveränderlich in Block 0.";
    oc_propose: "Propose change", "Änderung vorschlagen";
    toast_proposed: "Proposed — awaiting approvals", "Vorgeschlagen — wartet auf Zustimmungen";
    om_col_id: "ID", "ID";
    om_col_pk: "Public key", "Public Key";
    om_col_last: "Last seen", "Zuletzt gesehen";
    om_col_uploads: "Uploads", "Uploads";
    om_me: "(that's me)", "(das bin ich)";
    om_col_recovery: "Recovery link", "Recovery-Link";
    ou_col_user: "User", "Nutzer";
    ou_col_date: "Date", "Datum";
    ou_col_file: "Filename", "Dateiname";
    ou_col_type: "Type", "Typ";
    ou_col_size: "Size", "Größe";
    ou_col_checksum: "Checksum", "Checksum";
    ou_col_download: "Download", "Download";
    ou_col_expires: "Expires in", "Läuft ab in";
    ou_download: "Download", "Download";
    ou_offline: "user offline", "Nutzer offline";
    ou_empty: "No files shared yet.", "Noch keine Dateien geteilt.";
    ou_filter_ph: "Filter: user, filename or checksum", "Filter: Nutzer, Dateiname oder Checksum";
    // the paged lists' "Page x of y" label, split around the two numbers
    pg_page: "Page", "Seite";
    pg_of: "of", "von";
    ou_no_match: "No uploads match the filter.", "Keine Uploads passen zum Filter.";
    orn_title: "Rename republic", "Republik umbenennen";
    orn_body: "The name was ratified at the founding — renaming is a gated change: the draft becomes a proposal the members approve by threshold. Once applied, the republic shows its new name everywhere; its identity (the republic id) never changes.", "Der Name wurde bei der Gründung ratifiziert — eine Umbenennung ist eine geschützte Änderung: der Entwurf wird ein Vorschlag, dem die Mitglieder per Schwelle zustimmen. Nach dem Anwenden trägt die Republik überall den neuen Namen; ihre Identität (die Republik-ID) ändert sich nie.";
    pc_current: "Current", "Ist-Stand";
    pc_proposed: "Proposed", "Soll-Stand";
    pc_discuss: "Discussion", "Diskussion";
    ch_readonly: "read-only — the vote is decided", "nur lesen — die Abstimmung ist entschieden";
    pc_proposal: "Proposal:", "Vorschlag:";
    pc_img_hint: "Click to view the proposed image", "Klicken zum Anzeigen des vorgeschlagenen Bilds";
    pc_img_missing: "The proposed image could not be decoded.", "Das vorgeschlagene Bild konnte nicht dekodiert werden.";
    os_founded: "Founded", "Gegründet";
    os_consensus: "Consensus", "Konsens";
    cv_shrink: "Shrink", "Verkleinern";
    ocs_title: "Settings", "Einstellungen";
    ocs_chat_retention: "Delete chat after", "Chat löschen nach";
    ocs_days: "days", "Tage";
    ocr_title: "Change chat deletion period", "Chat-Löschfrist ändern";
    ocr_body: "Chat is ephemeral: messages older than this are deleted on every member. Changing the period is a gated change — the draft becomes a proposal the members approve by threshold. (Applying it is not wired yet.)", "Chat ist flüchtig: ältere Nachrichten werden bei allen Mitgliedern gelöscht. Die Frist zu ändern ist eine geschützte Änderung — der Entwurf wird ein Vorschlag, dem die Mitglieder per Schwelle zustimmen. (Das Anwenden ist noch nicht verdrahtet.)";
    ou_note: "Only metadata is shared — the bytes move user-to-user via the share link, as long as the sharer keeps the file. (Transfer and expiry are mocks.)", "Geteilt werden nur Metadaten — die Bytes wandern user-to-user über den Share-Link, solange der Teilende die Datei behält. (Übertragung und Ablauf sind Mocks.)";
    ow_title: "Open local workspace", "Lokalen Workspace öffnen";
    ow_empty: "No local workspaces found.", "Keine lokalen Workspaces gefunden.";
    ow_change_folder: "Change folder", "Ordner wechseln";
    ow_col_name: "Name", "Name";
    ow_col_sync: "Last sync", "Letzter Sync";
    ow_col_backup: "Backup", "Backup";
    ow_col_status: "Status", "Status";
    ow_enc: "encrypted", "verschlüsselt";
    ow_unenc: "unencrypted", "entschlüsselt";
    ow_encrypt: "Encrypt", "Verschlüsseln";
    ow_decrypt: "Decrypt", "Entschlüsseln";
    dw_title: "Decrypt workspace", "Workspace entschlüsseln";
    dw_body: "Enter the recovery phrase to decrypt this workspace on disk; it can then be opened again. (Mock — the phrase is not verified yet.)", "Gib die Wiederherstellungs-Phrase ein, um diesen Workspace auf der Platte zu entschlüsseln; danach lässt er sich wieder öffnen. (Mock — die Phrase wird noch nicht geprüft.)";
    ow_open: "Open", "Öffnen";
    ow_delete: "Delete", "Löschen";
    ow_select_hint: "Select a republic to see its status.", "Wähle eine Republik, um ihren Status zu sehen.";
    ow_s3_on: "S3 active", "S3 aktiv";
    ow_s3_off: "No S3", "Kein S3";
    ow_grp_backup: "Backup", "Backup";
    ow_grp_seed: "Seed", "Seed";
    ow_seed_missing: "No seed is stored on this device — only your written-down phrase can restore this workspace.", "Auf diesem Gerät ist kein Seed gespeichert — nur deine notierte Phrase kann diesen Workspace wiederherstellen.";
    ow_members: "Members", "Mitglieder";
    ow_backup_cfg: "Settings", "Settings";
    ow_export: "Manual backup", "Manuelles Backup";
    ow_export_note: "Exported (mock):", "Exportiert (Mock):";
    ow_seed_show: "Reveal seed", "Seed zeigen";
    ow_seed_hide: "Hide seed", "Seed verbergen";
    ow_seed_note: "Every secret key of this workspace is derived deterministically from this seed. Never share it.", "Alle geheimen Schlüssel dieses Workspace werden deterministisch aus diesem Seed abgeleitet. Niemals weitergeben.";
    ow_copy: "Copy", "Kopieren";
    ow_hold_tip: "Hold to reveal", "Halten zum Anzeigen";
    toast_copied: "Copied to clipboard", "In die Zwischenablage kopiert";
    del_ws_title: "Delete workspace?", "Workspace löschen?";
    del_ws_body: "This removes the republic from this device. Type its name to confirm. (Mock — nothing on disk is touched.)", "Dies entfernt die Republik von diesem Gerät. Tippe zur Bestätigung ihren Namen aus. (Mock — auf der Platte wird nichts angefasst.)";
    del_ws_confirm: "Delete permanently", "Endgültig löschen";
    bk_title: "Manual backup", "Manuelles Backup";
    bk_body: "The whole workspace is written to this location as one encrypted blob. (Mock — nothing is written.)", "Der gesamte Workspace wird als ein verschlüsselter Blob an diesen Ort geschrieben. (Mock — es wird nichts geschrieben.)";
    bk_path: "Target file", "Zieldatei";
    bk_confirm: "Save backup", "Backup speichern";
    field_s3_backup: "Automatic S3 backup", "Automatisches S3-Backup";
    field_s3_endpoint: "S3 endpoint", "S3-Endpunkt";
    jw_title: "Join by invite", "Per Einladung beitreten";
    jw_grp_invite: "Invite link", "Einladungslink";
    jw_invite_hint: "Paste the one-time molt:// invite another member created for you.", "Füge die einmalige molt://-Einladung ein, die ein Mitglied für dich erstellt hat.";
    jw_ok: "Invite looks OK.", "Einladung sieht OK aus.";
    jw_grp_preview: "You are joining …", "Du trittst bei …";
    jw_preview_hint: "Details are exchanged during the handshake.", "Details werden beim Handshake ausgetauscht.";
    jw_invited_by: "invited by", "eingeladen von";
    jw_join: "Join republic", "Republik beitreten";
    jw_ph1: "Contacting the inviter…", "Kontaktiere den Einlader…";
    jw_ph2: "Receiving MLS welcome…", "Empfange MLS-Welcome…";
    jw_ph3: "Syncing surfaces…", "Synchronisiere Surfaces…";
    jw_failed: "Failed — invite rejected", "Fehlgeschlagen — Einladung abgelehnt";
    om_recover_link: "Recovery link", "Recovery-Link";
    rlk_title: "Recovery link", "Recovery-Link";
    rlk_body: "Hand this link to the returning member so they can rejoin this republic from a new device.", "Gib diesen Link dem zurückkehrenden Mitglied, damit es dieser Republik von einem neuen Gerät wieder beitreten kann.";
    rlk_caution: "Share it off-band, over a private channel. It is single-use and dies with this session — after an app restart, mint a fresh one.", "Teile ihn off-band über einen privaten Kanal. Er ist einmalig nutzbar und stirbt mit dieser Sitzung — nach einem Neustart der App einen neuen erstellen.";
    rlk_pending: "Creating the link…", "Link wird erstellt…";
    rlk_pending_hint: "The returning member does not need to be online — a recovery link is made for someone who is unreachable.", "Das zurückkehrende Mitglied muss dafür nicht online sein — ein Recovery-Link ist ja gerade für ein unerreichbares Mitglied gedacht.";
    rlk_failed_mesh: "The link could not be created: this device is not connected to the republic's mesh right now. Close and reopen the republic to reconnect, then try again. The returning member does not need to be online for this.", "Der Link konnte nicht erstellt werden: Dieses Gerät ist gerade nicht mit dem Mesh der Republik verbunden. Schließe die Republik und öffne sie erneut, dann versuche es noch einmal. Das zurückkehrende Mitglied muss dafür nicht online sein.";
    rlk_failed_prefix: "The link could not be created: ", "Der Link konnte nicht erstellt werden: ";
    rv_running_note: "Waiting for the surviving members to approve your re-admission. This human step can take a while — it times out after ~15 minutes.", "Warte auf die Zustimmung der verbliebenen Mitglieder zur Wiederaufnahme. Dieser menschliche Schritt kann dauern — Timeout nach ~15 Minuten.";
    rv_failed_hint: "Recovery links are single-use — ask any surviving member for a fresh one and try again.", "Recovery-Links sind einmalig — bitte ein verbliebenes Mitglied um einen neuen und versuch es erneut.";
    rw_title: "Restore", "Wiederherstellen";
    rw_seed: "Recovery phrase", "Wiederherstellungs-Phrase";
    rw_paste: "Paste", "Einfügen";
    rw_seed_hint: "Needed for every restore path — all keys derive from this phrase.", "Für jeden Weg erforderlich — alle Schlüssel werden aus dieser Phrase abgeleitet.";
    rw_continue: "Continue", "Weiter";
    rw_via_peer: "Social peer-restore", "Social Peer-Restore";
    rw_peer_hint: "Rejoins via another member — paste the recovery link a member minted for you.", "Tritt über ein anderes Mitglied wieder bei — füge den Recovery-Link ein, den ein Mitglied für dich erstellt hat.";
    rw_via_s3: "Online-restore via S3", "Online-Restore via S3";
    rw_s3_hint: "Pulls the encrypted backup from the S3 bucket in the storage settings.", "Holt das verschlüsselte Backup aus dem S3-Bucket der Speicher-Einstellungen.";
    rw_s3_none: "No S3 endpoint configured.", "Kein S3-Endpunkt konfiguriert.";
    rw_s3_ok: "reachable", "erreichbar";
    rw_via_file: "Manual restore", "Manuelles Restore";
    rw_file_hint: "Restores from an encrypted .molt.enc file backup.", "Stellt aus einem verschlüsselten .molt.enc-Datei-Backup wieder her.";
    rw_choose: "Choose file…", "Datei wählen…";
    rw_no_file: "No backup file chosen.", "Keine Backup-Datei gewählt.";
    rw_file_title: "Choose encrypted backup", "Verschlüsseltes Backup wählen";
    rw_file_body: "Path of the encrypted workspace blob (.molt.enc). (Mock — nothing is read.)", "Pfad des verschlüsselten Workspace-Blobs (.molt.enc). (Mock — es wird nichts gelesen.)";
    rw_file_pick: "Select", "Auswählen";
    rw_log_title: "Live details", "Live-Details";
    rw_finish: "Finish", "Fertigstellen";
    rw_failed: "Failed — timeout while connecting", "Fehlgeschlagen — Timeout beim Verbinden";
    rw_ph1: "Connecting…", "Verbinde…";
    rw_ph2: "Fetching encrypted data…", "Lade verschlüsselte Daten…";
    rw_ph3: "Decrypting & verifying…", "Entschlüssele & prüfe…";
    set_title: "Settings", "Einstellungen";
    set_tab_general: "General", "Allgemein";
    set_tab_workspace: "Workspace", "Workspace";
    set_tab_backup: "Backup", "Backup";
    set_tab_network: "Network", "Netzwerk";
    set_tab_mcp: "MCP", "MCP";
    set_tab_node: "Node", "Node";
    set_tab_chain: "Chain-History", "Chain-History";
    chain_col_height: "Block", "Block";
    chain_col_what: "Change", "Änderung";
    chain_col_signers: "Signed by", "Signiert von";
    chain_kind_genesis: "Founding", "Gründung";
    chain_kind_membership: "Membership", "Mitgliedschaft";
    chain_kind_checkpoint: "Checkpoint (compacted)", "Checkpoint (kompaktiert)";
    chain_pre_cut: "before the cut", "vor dem Schnitt";
    chain_empty: "No chain — this workspace is not chain-governed.", "Keine Chain — dieser Workspace ist nicht chain-regiert.";
    set_ws_choose: "Choose folder…", "Ordner auswählen…";
    set_ws_dir_title: "Choose workspace folder", "Workspace-Ordner auswählen";
    set_ws_dir_body: "Path of the folder that holds your workspaces. (Mock — no real file dialog.)", "Pfad des Ordners, der deine Workspaces enthält. (Mock — kein echter Dateidialog.)";
    set_ws_found_one: "workspace found in this folder", "Workspace in diesem Ordner gefunden";
    set_ws_found_many: "workspaces found in this folder", "Workspaces in diesem Ordner gefunden";
    field_s3_access: "Access key", "Access-Key";
    field_s3_secret: "Secret key", "Secret-Key";
    field_s3_bucket: "Bucket", "Bucket";
    set_s3_test: "Test connection", "Verbindung testen";
    set_s3_active: "active", "aktiv";
    set_s3_every: "every", "alle";
    set_s3_unit_min: "min", "Minuten";
    set_s3_keep: "save up to", "behalte bis zu";
    set_s3_unit_copies: "copies", "Kopien";
    toast_s3_ok: "S3 backup isn't wired up yet — nothing was tested.", "S3-Backup ist noch nicht angebunden — es wurde nichts getestet.";
    bk_col_local: "Local workspace", "Lokaler Workspace";
    bk_col_remote: "Backup in bucket", "Backup im Bucket";
    bk_col_auto: "Auto", "Auto";
    bk_col_size: "Size", "Größe";
    bk_col_last: "Last backup", "Letztes Backup";
    set_save: "Save", "Speichern";
    set_save_note: "Saved to config.toml.", "In config.toml gespeichert.";
    set_close: "Close", "Schließen";
    set_path_label: "Writes to", "Schreibt nach";
    set_reloaded_note: "config.toml changed on disk — settings reloaded.", "config.toml wurde auf der Platte geändert — Einstellungen neu geladen.";
    set_conflict_note: "config.toml on disk is invalid — the running settings stay. Fix the file or run --repair-config.", "config.toml auf der Platte ist ungültig — die laufenden Einstellungen bleiben. Datei korrigieren oder --repair-config ausführen.";
    set_restart_note: "Takes effect after a restart:", "Wirkt erst nach einem Neustart:";
    set_panel_appearance: "Language & appearance", "Sprache & Design";
    set_panel_sounds: "Sound alerts", "Sound-Alerts";
    field_sound_message: "New message", "Neue Nachricht";
    field_sound_vote: "New vote", "Neues Vote";
    sound_off: "off", "aus";
    set_tor_embedded_missing: "\"embedded\" is greyed out: this build was compiled without the embedded-tor feature (in-process Tor). Rebuild with --features embedded-tor, or use a local Tor daemon.", "\"embedded\" ist ausgegraut: dieses Binary wurde ohne das embedded-tor-Feature (In-Prozess-Tor) gebaut. Mit --features embedded-tor neu bauen oder einen lokalen Tor-Daemon nutzen.";
    unsaved_title: "Unsaved changes", "Ungespeicherte Änderungen";
    unsaved_body: "You changed settings without saving them. Save them to config.toml, or discard the edits?", "Du hast Einstellungen geändert, aber nicht gespeichert. In die config.toml speichern oder die Änderungen verwerfen?";
    unsaved_save: "Save & continue", "Speichern & weiter";
    unsaved_discard: "Discard & continue", "Verwerfen & weiter";
    unsaved_cancel: "Cancel", "Abbruch";
    mv_send: "Send", "Senden";
    mv_propose: "Propose", "Vorschlagen";
    mv_approve: "Approve", "Zustimmen";
    mv_decline: "Decline", "Ablehnen";
    mv_pending: "Pending decisions", "Offene Entscheidungen";
    mv_declined: "Declined proposals", "Abgelehnte Vorschläge";
    mv_empty_declined: "No declined proposals right now — this view empties on the chat retention rhythm.", "Gerade keine abgelehnten Vorschläge — diese Ansicht leert sich im Chat-Aufbewahrungsrhythmus.";
    pc_declined_by: "Declined by", "Abgelehnt von";
    mv_applied: "Applied", "Angewandt";
    mv_accepted: "Accepted changes", "Angenommene Änderungen";
    org_checkpoint: "Checkpoint", "Checkpoint";
    org_checkpoint_tip: "Propose a chain checkpoint: m-of-n confirm the summarized state, history below the cut is dropped on every device.", "Chain-Checkpoint vorschlagen: m-of-n bestätigen den zusammengefassten Zustand, die Historie unter dem Schnitt wird auf jedem Gerät gelöscht.";
    toast_checkpoint_sealed: "Checkpoint sealed", "Checkpoint besiegelt";
    toast_checkpoint_stale: "Checkpoint went stale — another block sealed first; propose again.", "Checkpoint veraltet — ein anderer Block kam zuerst; bitte neu vorschlagen.";
    mv_chat_ph: "Write a message…", "Nachricht schreiben…";
    mv_propose_ph: "Describe a proposal…", "Vorschlag beschreiben…";
    mv_empty_chat: "No messages yet.", "Noch keine Nachrichten.";
    mv_later: "Nothing here yet — this view comes with a later build.", "Hier ist noch nichts — diese Ansicht kommt mit einem späteren Build.";
    mv_empty_pending: "Nothing awaiting approval.", "Nichts wartet auf Zustimmung.";
    mv_empty_applied: "Nothing applied yet.", "Noch nichts angewandt.";
    mv_deleted_by: "deleted by", "gelöscht durch";
    ch_discussions: "Discussions", "Diskussionen";
    ch_group: "General", "Allgemein";
    ch_new_topic: "New topic", "Neues Thema";
    ch_topic_ph: "Topic name…", "Themenname…";
    ch_topic_open: "Open topic", "Thema öffnen";
    ch_to_vote: "To the vote", "Zur Abstimmung";
    mv_file_gone: "File no longer available — its owner deleted it.", "Datei nicht mehr verfügbar — der Besitzer hat sie gelöscht.";
    toast_dl_done: "Saved:", "Gespeichert:";
    toast_dl_failed: "Download failed:", "Download fehlgeschlagen:";
    toast_file_removed: "Local file deleted — the share is no longer available.", "Lokale Datei gelöscht — die Freigabe ist nicht mehr verfügbar.";
    dm_title: "Delete message?", "Nachricht löschen?";
    dm_body: "The text disappears for everyone and only a deletion notice remains. (Mock — nothing on disk.)", "Der Text verschwindet für alle, nur ein Lösch-Hinweis bleibt. (Mock — nichts auf der Platte.)";
    dm_confirm: "Delete", "Löschen";
    mv_close_ws: "Close workspace", "Workspace schließen";
    close_ws_title: "Close workspace?", "Workspace schließen?";
    close_ws_body: "You'll return to the start screen. This is a mock, so nothing is written to disk.", "Du kehrst zum Startbildschirm zurück. Dies ist ein Mock, es wird nichts auf die Platte geschrieben.";
    close_ws_confirm: "Close workspace", "Workspace schließen";
    close_ws_cancel: "Cancel", "Abbrechen";
    tip_theme: "Theme", "Theme";
    tip_language: "Language", "Sprache";
    tip_settings: "Settings", "Einstellungen";
    quit_title: "Quit MoltRepublic?", "MoltRepublic beenden?";
    quit_body: "A workspace is open. Quitting shuts the node down; the GUI and its MCP endpoint stop.", "Ein Workspace ist offen. Beenden fährt den Node herunter; GUI und MCP-Endpoint stoppen.";
    quit_confirm: "Quit", "Beenden";
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::ProposalState;

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
            deleted_by: String::new(),
            first: true,
            own: false,
            alt: false,
            mine_emoji: String::new(),
            reactions: Vec::new(),
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

    /// An engine-authored System-kind message maps onto the same per-line
    /// `system` flag the governance rows use — one quiet rendering path,
    /// never a second style; a User message stays a normal card.
    #[test]
    fn a_system_kind_message_maps_onto_the_quiet_line_flag() {
        let user = ChatMessage::text(MessageId([1; 16]), "petra", "gm", 100);
        assert!(!chat_line(0, &user, "me").system);
        let notice = ChatMessage::text(MessageId([2; 16]), "petra", "🔑 back", 101)
            .with_kind(molt_core::ChatKind::System);
        assert!(chat_line(0, &notice, "me").system);
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
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(7) },
                count: 1,
                last_ts: 30,
                state: None,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(5) },
                count: 2,
                last_ts: 20,
                state: Some(ProposalState::Applied),
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(3) },
                count: 5,
                last_ts: 10,
                state: Some(ProposalState::Proposed),
            },
            ChannelInfo {
                channel: ChannelRef::Group,
                count: 9,
                last_ts: 50,
                state: None,
            },
        ];
        let known = HashMap::from([
            (3u64, known_of("raise budget", KnownFate::Pending)),
            (5u64, known_of("sealed one", KnownFate::Applied)),
        ]);
        let unread = HashMap::from([("patch:3".to_string(), 2usize), ("group".to_string(), 1)]);
        let rows = derive_channels(0, &infos, &known, &unread);
        // a discussion is vote-bound: only OPEN votes (something can still
        // be voted on) appear — no group row (the Gruppe view covers it),
        // no sealed/closed votes, no unknown proposals, no free topics
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["patch:3"],
            "only the open vote's discussion survives"
        );
        assert_eq!(rows[0].label, "raise budget", "patch title from proposal state");
        assert_eq!(rows[0].unread, 2);
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
        assert_eq!(display_title(0, &legacy), "Change the republic's image");
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
            channels: Vec::new(),
        };
        let data = surface_data(0, Surface::Memory, &snap, "petra", None);
        assert_eq!(data.log.len(), 2);
        assert_eq!(data.log[0].proposal_id, Some(7));
        assert_eq!(data.log[1].proposal_id, None);
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
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
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
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
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
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: if state == ProposalState::Rejected { 100 } else { 0 },
            declined_by: if state == ProposalState::Rejected {
                "ashi".to_string()
            } else {
                String::new()
            },
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
        }];
        assert!(
            derive_channels(0, &infos, &known, &HashMap::new()).is_empty(),
            "a declined vote's discussion is not a sidebar row"
        );
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

    /// Review finding: concurrent pushes raced last-write-wins — a stale
    /// bundle could land after a fresh selection and revert the visible
    /// pane (mis-marking unread on the way). Every selection change and
    /// every newer push start must invalidate the in-flight pushes.
    #[test]
    fn push_generation_guard_invalidates_stale_pushes() {
        let mut st = ChatUiState::default();
        let g1 = st.begin_push("ws-1");
        assert!(st.is_current(g1), "the newest push is current");
        // a newer push start supersedes the older one
        let g2 = st.begin_push("ws-1");
        assert!(!st.is_current(g1));
        assert!(st.is_current(g2));
        // a selection change invalidates every in-flight push …
        st.select(ChannelRef::Topic {
            name: "budget".into(),
        });
        assert!(!st.is_current(g2));
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".into()
            }
        );
        // … and the counter survives the workspace-switch reset, so an old
        // push can never match a freshly reset state
        let g3 = st.begin_push("ws-2");
        assert!(g3 > g2, "monotonic across enter_workspace resets");
        assert!(st.is_current(g3));
        assert!(!st.is_current(g2));
    }

    #[test]
    fn unread_counts_reset_on_channel_selection() {
        let mut ledger = UnreadLedger::default();
        let counts = vec![("group".to_string(), 3usize), ("topic:t".to_string(), 2)];
        // the first sight of a workspace counts as read — no unread wall
        let unread = ledger.observe(&counts, "group");
        assert!(unread.values().all(|u| *u == 0), "{unread:?}");
        // new traffic shows up everywhere but the channel on screen
        let counts = vec![("group".to_string(), 5), ("topic:t".to_string(), 4)];
        let unread = ledger.observe(&counts, "group");
        assert_eq!(unread["group"], 0);
        assert_eq!(unread["topic:t"], 2);
        // selecting the topic resets its count …
        let unread = ledger.observe(&counts, "topic:t");
        assert_eq!(unread["topic:t"], 0);
        assert_eq!(unread["group"], 0, "group stays read up to its last viewing");
        // … and a channel arriving after the seed starts fully unread
        let counts = vec![
            ("group".to_string(), 5),
            ("topic:t".to_string(), 4),
            ("topic:new".to_string(), 3),
        ];
        let unread = ledger.observe(&counts, "topic:t");
        assert_eq!(unread["topic:new"], 3);
    }

    /// A workspace switch must not leak the previous workspace's channel
    /// state into the next one: a stale Patch/Topic selection would filter
    /// the new workspace's log until manually cleared, the ledger would
    /// misread its counts and the first-seen stamps would misplace system
    /// lines. Same workspace → everything is kept.
    #[test]
    fn chat_ui_state_resets_on_workspace_switch() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        st.selected = ChannelRef::Topic {
            name: "budget".to_string(),
        };
        st.first_seen.insert(4, 100);
        let counts = vec![("group".to_string(), 3usize)];
        let _ = st.ledger.observe(&counts, "topic:budget");

        // the same workspace: selection, ledger and stamps survive
        st.enter_workspace("ws-1");
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".to_string()
            }
        );
        assert_eq!(st.first_seen.get(&4), Some(&100));

        // a switch: back to Group, fresh ledger (the next observation
        // seeds — no unread wall from the new workspace's history),
        // stamps gone, and the new identity sticks
        st.enter_workspace("ws-2");
        assert_eq!(st.selected, ChannelRef::Group);
        assert!(st.first_seen.is_empty());
        let unread = st.ledger.observe(&[("group".to_string(), 9usize)], "topic:x");
        assert_eq!(unread["group"], 0, "the fresh ledger re-seeds");
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
            "columns are a display split — every word survives"
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
        assert_eq!(expires_label(100, 100 + 13 * 86_400, true), "in 13 days");
        assert_eq!(expires_label(100, 100 + 86_400, true), "in 1 day");
        assert_eq!(expires_label(100, 100 + 7_200, true), "in 2 h");
        assert_eq!(expires_label(100, 100 + 120, true), "in 2 min");
        assert_eq!(expires_label(500, 100, true), "expired");
        assert_eq!(
            expires_label(100, 0, true),
            "—",
            "0 = unknown share age, no deadline (the engine keeps it forever)"
        );
        assert_eq!(
            expires_label(100, 100 + 86_400, false),
            "—",
            "an unavailable share has nothing left to expire"
        );
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

    #[test]
    fn sort_bk_rows_by_size_and_names_with_empties_last() {
        let sv = SessionView::default();
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
        let sv = SessionView::default();
        let rows = backup_rows(&sv);
        assert_eq!(rows.len(), sv.workspaces.len() + sv.backup_orphans.len());
        // locals first: name on the left, bucket side only when auto is on
        for (row, w) in rows.iter().zip(&sv.workspaces) {
            assert!(row.has_local);
            assert_eq!(row.local.as_str(), w.name);
            assert_eq!(row.auto, w.s3);
            assert_eq!(!row.remote.is_empty(), w.s3);
        }
        // orphans last: bucket side only, no toggle
        for (row, o) in rows[sv.workspaces.len()..].iter().zip(&sv.backup_orphans) {
            assert!(!row.has_local);
            assert_eq!(row.local.as_str(), "");
            assert_eq!(row.remote.as_str(), o.name);
            assert!(!row.auto);
        }
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
    fn member(name: &str, id: &str, last: &str, state: i32, uploads: i32) -> MemberRowData {
        MemberRowData {
            name: name.to_string(),
            id: id.to_string(),
            pk: id.to_string(),
            last: last.to_string(),
            state,
            uploads,
        }
    }

    #[test]
    fn sort_members_by_name_uploads_and_presence() {
        let mut rows = vec![
            member("bob", "0b", "just now", 0, 3),
            member("Alice", "aa", "5 min ago", 1, 10),
            member("carol", "", "offline", 2, 2),
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
        // no real timestamp exists (mock presence) — recency approximates
        // as presence state first: synced < syncing < offline
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
}
