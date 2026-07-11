// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
// Slint's generated code (via `include_modules!`) is injected into this crate and
// uses `as` casts, `unwrap`s, float layout math and a `todo!()` embed-stub that
// our (money-crate-oriented) workspace lints flag. We also cast small ints to
// Slint's `i32`. These allows are scoped to this UI crate only, so the rest of
// the workspace keeps the strict posture.
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
    ChannelInfo, ChannelRef, ChatMessage, Command, Event, MessageId, ProposalId, ProposalView,
    Reply, Screen, SessionScope, SessionSettings, SessionView, Surface, SurfaceSnapshot,
};
use molt_engine::WalletHandle;
use slint::{Model, ModelRc, VecModel};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

slint::include_modules!();

/// Open the GUI and run the Slint event loop on the calling (main) thread.
///
/// `config_path` is shown in the settings panel as the location a real save
/// *would* target. Returns when the window closes, or an error if the GUI cannot
/// start (e.g. no display) — in which case the caller falls back to headless.
pub fn run_app(
    wallet: WalletHandle,
    rt: Handle,
    config_path: PathBuf,
) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_config_path(config_path.display().to_string().into());

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
                // instant banner feedback — for a fresh (still empty) topic
                // this is the only visible signal until its first message
                // exists; the next push refreshes it with the lazy title
                ui.set_selected_channel_label(
                    channel_display_label(&ch, &HashMap::new()).as_str().into(),
                );
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
            // only the file's METADATA is read and shared — no bytes move
            rt.spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new().pick_file().await else {
                    return; // cancelled
                };
                let name = file.file_name();
                let (size, modified) = std::fs::metadata(file.path())
                    .map(|md| {
                        let modified = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0); // 0 = the engine stamps "now"
                        (md.len(), modified)
                    })
                    .unwrap_or((0, 0));
                let kind = file_kind_label(&name);
                let cmd = Command::ShareFile {
                    name,
                    size,
                    kind,
                    modified,
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
            issue(&rt, &w, &weak, Command::DownloadFile { id });
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

/// Map a session workspace into the Slint-side row struct.
fn workspace_item(w: &molt_core::WorkspaceInfo) -> WorkspaceItem {
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
        status: sync_status_label(w.state, w.last_sync_min, w.sync_queue).into(),
        synced: w.synced,
        state: i32::from(w.state),
        last_sync_min: w.last_sync_min as i32,
        s3: w.s3,
        seed: w.seed.as_str().into(),
        net: w.net.as_str().into(),
        members: ModelRc::new(VecModel::from(members)),
    }
}

/// A human "x ago" label from minutes.
fn ago_label(minutes: u32) -> String {
    match minutes {
        0 => "just now".to_string(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 1440 => format!("{} h ago", m / 60),
        m => format!("{} d ago", m / 1440),
    }
}

/// Render the human sync-status line from the machine fields — prose is
/// presentation, so it lives here and not in the shared data.
fn sync_status_label(state: u8, last_sync_min: u32, sync_queue: u32) -> String {
    match state {
        1 => format!("Syncing… {sync_queue} items left"),
        2 => format!("Offline · last sync {}", ago_label(last_sync_min)),
        _ => format!("Synced · {}", ago_label(last_sync_min)),
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
fn backup_when_label(minutes: u32) -> String {
    if minutes == molt_core::WorkspaceInfo::NEVER {
        "never".to_string()
    } else {
        ago_label(minutes)
    }
}

/// The settings backup table: every local workspace mapped to its bucket
/// backup (if auto-backup is on), then the bucket-only orphans.
fn backup_rows(sv: &SessionView) -> Vec<BackupRow> {
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
            last: backup_when_label(w.last_backup_min).into(),
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
        last: backup_when_label(o.last_backup_min).into(),
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
/// minted link, and the rejoiner's started/failed/done lifecycle.
#[derive(Debug, PartialEq, Eq)]
enum RecoverNotice {
    /// Not a recovery notice (every other notice, e.g. "saved").
    None,
    /// Coordinator: the engine minted a single-use `molt://recover/…` link.
    Link(String),
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
    if let Some(link) = notice.strip_prefix("recovery-link:") {
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
        s3_backup: ui.get_cfg_s3_backup(),
        s3_endpoint: ui.get_cfg_s3_endpoint().to_string(),
        s3_access_key: ui.get_cfg_s3_access().to_string(),
        s3_secret_key: ui.get_cfg_s3_secret().to_string(),
        s3_bucket: ui.get_cfg_s3_bucket().to_string(),
        s3_interval_min: ui.get_cfg_s3_interval() as u16,
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
    ui.set_screen(from_screen(sv.screen));
    ui.set_selected_surface(sv.surface.as_str().into());
    ui.set_selected_view(sv.view.as_str().into());

    // the Open screen's list mirrors the session's workspaces, re-applying
    // whatever column sort the user picked
    let mut items: Vec<WorkspaceItem> = sv.workspaces.iter().map(workspace_item).collect();
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
                sync_status_label(w.state, w.last_sync_min, w.sync_queue),
            )
        })
        .unwrap_or((0, String::new()));
    ui.set_active_state(a_state);
    ui.set_active_status(a_status.into());
    // the ratified founding charter for the Constitution surface (empty until a
    // deliberated workspace is open)
    ui.set_active_agenda(active.map(|w| w.agenda.as_str()).unwrap_or_default().into());
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
    let lang = i32::from(sv.language == "de");
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
            RecoverNotice::Link(link) => {
                // coordinator: present the freshly minted single-use link
                ui.set_recovery_link(link.into());
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
                // the engine flips to Main itself — just clear the modal so a
                // later return to the choice screen starts clean
                ui.set_rv_running(false);
                ui.set_rv_error("".into());
                ui.set_rv_open(false);
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
    ui.set_cfg_s3_backup(s.s3_backup);
    ui.set_cfg_s3_endpoint(s.s3_endpoint.clone().into());
    ui.set_cfg_s3_access(s.s3_access_key.clone().into());
    ui.set_cfg_s3_secret(s.s3_secret_key.clone().into());
    ui.set_cfg_s3_bucket(s.s3_bucket.clone().into());
    ui.set_cfg_s3_interval(i32::from(s.s3_interval_min));
    ui.set_cfg_mcp_port(s.mcp_port as i32);
    ui.set_cfg_mcp_allow(s.mcp_allow.clone().into());
    ui.set_cfg_mcp_token(s.mcp_token.clone().into());
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
}
struct SurfaceData {
    key: String,
    name: String,
    gated: bool,
    log: Vec<LogLineData>,
    pending: Vec<ProposalRowData>,
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
    let (member, threshold_badge) = match wallet.execute(Command::Status).await {
        Ok(Reply::Status(s)) => (s.member, format!("{}-of-{}", s.threshold, s.members.len())),
        _ => return,
    };
    // the chat-bus UI state is per-workspace: bind it to the active id so
    // a workspace switch drops the previous selection/unread/first-seen
    let active_ws = match wallet.execute(Command::ReadSession).await {
        Ok(Reply::Session(s)) => s.active_workspace.clone(),
        _ => String::new(),
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
        })
        .await
    {
        Ok(Reply::State(snap)) => Some(snap),
        _ => None,
    };
    let mut snaps: Vec<(Surface, SurfaceSnapshot)> = Vec::new();
    for sf in Surface::ALL {
        let channel = (sf == Surface::Chat).then(|| selected.clone());
        if let Ok(Reply::State(snap)) = wallet
            .execute(Command::ReadState { surface: sf, channel })
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
    let (unread, first_seen, known) = {
        let mut st = chat_ui.lock().expect("chat ui state poisoned");
        if !st.is_current(my_gen) {
            // a newer selection/push owns the state — observing now would
            // mis-mark the fresh channel read, and the bundle is stale
            return;
        }
        for p in &all_pending {
            st.first_seen.entry(p.id.0).or_insert(now);
        }
        update_known_proposals(&mut st.proposals, &all_pending, &applied_by_surface);
        (
            st.ledger.observe(&counts, &selected_key),
            st.first_seen.clone(),
            st.proposals.clone(),
        )
    };
    // titles come from the cache, so a patch channel keeps its name (and
    // its ✓/⊘ state line) after the proposal left the Proposed-only read
    let titles = known_titles(&known);
    let channels = derive_channels(&infos, &titles, &unread);
    let selected_label = channel_display_label(&selected, &titles);
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
            surface_data(*sf, snap, &member, (*sf == Surface::Chat).then_some(&ctx))
        })
        .collect();
    let bundle = SurfacesBundle {
        member,
        threshold_badge,
        surfaces,
        channels,
        selected_key,
        selected_label,
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
    ui.set_threshold_badge(b.threshold_badge.clone().into());
    let tabs: Vec<SurfaceTab> = b
        .surfaces
        .iter()
        .map(|s| {
            let log: Vec<LogLine> = s
                .log
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
                    }
                })
                .collect();
            let pending: Vec<ProposalRow> = s
                .pending
                .iter()
                .map(|p| ProposalRow {
                    id: p.id,
                    text: p.text.clone().into(),
                    approvals: p.approvals,
                    threshold: p.threshold,
                })
                .collect();
            // the surface's sub-views come straight from the shared
            // molt-core vocabulary (same list select_view validates against)
            let views: Vec<ViewItem> = Surface::parse(&s.key)
                .map(|sf| {
                    sf.views()
                        .iter()
                        .map(|(key, label)| ViewItem {
                            key: (*key).into(),
                            name: (*label).into(),
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
                log: ModelRc::new(VecModel::from(log)),
                pending: ModelRc::new(VecModel::from(pending)),
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
    ui.set_selected_channel_label(b.selected_label.as_str().into());
}

/// Render a chat timestamp as `2026-06-02 13:37 (~20 minutes ago)` in the
/// local timezone. The relative part refreshes with every surfaces push.
fn when_label(ts: u64) -> String {
    when_label_at(ts, chrono::Utc::now().timestamp())
}

/// [`when_label`] against an explicit "now" (testable).
fn when_label_at(ts: u64, now: i64) -> String {
    let Ok(secs) = i64::try_from(ts) else {
        return String::new();
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    let local = utc.with_timezone(&chrono::Local);
    let ago = (now - secs).max(0);
    let rel = if ago < 60 {
        "just now".to_string()
    } else if ago < 3600 {
        let m = ago / 60;
        format!("~{m} minute{} ago", if m == 1 { "" } else { "s" })
    } else if ago < 86_400 {
        let h = ago / 3600;
        format!("~{h} hour{} ago", if h == 1 { "" } else { "s" })
    } else {
        let d = ago / 86_400;
        format!("~{d} day{} ago", if d == 1 { "" } else { "s" })
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
        "statistics" => "📊",
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
    sf: Surface,
    snap: &SurfaceSnapshot,
    me: &str,
    chat_ctx: Option<&ChatViewCtx>,
) -> SurfaceData {
    let mut log: Vec<LogLineData> = if sf == Surface::Chat {
        let msgs = chat_messages(snap);
        // (ts, line) pairs: the patch-channel system lines merge in by time
        let pairs: Vec<(u64, LogLineData)> = msgs.iter().map(|m| (m.ts, chat_line(m, me))).collect();
        let system = match chat_ctx.map(|c| &c.selected) {
            Some(ChannelRef::Patch { id }) => {
                let ctx = chat_ctx.expect("checked above");
                patch_system_lines(id.0, &ctx.proposals, &ctx.known, &ctx.first_seen)
            }
            _ => Vec::new(),
        };
        merge_by_time(pairs, system)
    } else {
        snap.applied
            .iter()
            .map(|v| LogLineData {
                id: String::new(),
                lead: String::new(),
                text: summarize(v),
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
            })
            .collect()
    };
    let no_quotes = HashMap::new();
    annotate_chat_log(&mut log, chat_ctx.map_or(&no_quotes, |c| &c.quotes));
    let pending: Vec<ProposalRowData> = snap
        .pending
        .iter()
        .map(|p| ProposalRowData {
            id: p.id.0 as i32,
            text: summarize(&p.payload),
            approvals: p.approvals as i32,
            threshold: p.threshold as i32,
        })
        .collect();
    SurfaceData {
        key: sf.as_str().to_string(),
        name: surface_name(sf).to_string(),
        gated: snap.gated,
        log,
        pending,
    }
}

/// One typed chat message, projected for display. Quote resolution (row +
/// teaser) happens later in [`annotate_chat_log`]: the row index can only
/// be known once system lines are merged in, and the teaser may resolve
/// against a message outside the displayed (filtered) log.
fn chat_line(m: &ChatMessage, me: &str) -> LogLineData {
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
            when_label(m.ts)
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

/// The display type of a shared file, from its extension (proper MIME
/// sniffing can come with the transport; the label is presentation).
fn file_kind_label(name: &str) -> String {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase());
    match ext.as_deref() {
        Some("pdf") => "PDF",
        Some("jpg" | "jpeg" | "png" | "webp" | "gif" | "svg") => "Image",
        Some("md" | "txt") => "Text",
        Some("ods" | "xlsx" | "csv") => "Spreadsheet",
        Some("odt" | "docx") => "Document",
        Some("zip" | "tar" | "gz" | "7z") => "Archive",
        Some("mp3" | "ogg" | "flac" | "opus") => "Audio",
        Some("mp4" | "mkv" | "webm") => "Video",
        _ => "File",
    }
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

/// The sidebar's channel rows from the engine enumeration: `group` always
/// first (synthesized when absent), then patch channels by ascending
/// proposal id — titles resolved lazily from proposal state with a `#id`
/// fallback, never an error on an unknown id (concept Q4) — then topics in
/// byte order (channel equality is exact-string in core, so no case folding
/// here either).
fn derive_channels(
    infos: &[ChannelInfo],
    titles: &HashMap<u64, String>,
    unread: &HashMap<String, usize>,
) -> Vec<ChannelRowData> {
    let unread_of =
        |key: &str| i32::try_from(unread.get(key).copied().unwrap_or(0)).unwrap_or(i32::MAX);
    let mut patches: Vec<u64> = Vec::new();
    let mut topics: Vec<String> = Vec::new();
    for i in infos {
        match &i.channel {
            ChannelRef::Group => {}
            ChannelRef::Patch { id } => patches.push(id.0),
            ChannelRef::Topic { name } => topics.push(name.clone()),
        }
    }
    patches.sort_unstable();
    patches.dedup();
    topics.sort();
    topics.dedup();
    let mut rows = vec![ChannelRowData {
        key: "group".to_string(),
        label: String::new(), // the UI substitutes the localized group label
        icon: "💬".to_string(),
        unread: unread_of("group"),
    }];
    for id in patches {
        let key = format!("patch:{id}");
        rows.push(ChannelRowData {
            unread: unread_of(&key),
            key,
            label: titles.get(&id).cloned().unwrap_or_else(|| format!("#{id}")),
            icon: "🗳️".to_string(),
        });
    }
    for name in topics {
        let key = format!("topic:{name}");
        rows.push(ChannelRowData {
            unread: unread_of(&key),
            key,
            label: name,
            icon: "#️⃣".to_string(),
        });
    }
    rows
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

/// What the UI remembers about a proposal beyond the read contract's
/// Proposed-only `pending` window. The engine never re-exposes a terminal
/// proposal (a sealed block's `applied` value is the bare payload, without
/// the proposal id), so title and governance state would vanish from the
/// patch channel the moment a block seals — this cache keeps them.
#[derive(Clone)]
struct KnownProposal {
    /// `summarize(&payload)` at the last sighting — the sidebar/banner title.
    title: String,
    /// The full payload; the fate probe matches it against the applied log.
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
    applied: &HashMap<Surface, Vec<serde_json::Value>>,
) {
    for p in pending {
        known.insert(
            p.id.0,
            KnownProposal {
                title: summarize(&p.payload),
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
}

/// The lazy patch-channel titles (sidebar rows + compose banner), from the
/// proposal cache — so a title survives the proposal leaving `pending`.
fn known_titles(known: &HashMap<u64, KnownProposal>) -> HashMap<u64, String> {
    known.iter().map(|(id, k)| (*id, k.title.clone())).collect()
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
    patch: u64,
    pending: &[ProposalView],
    known: &HashMap<u64, KnownProposal>,
    first_seen: &HashMap<u64, u64>,
) -> Vec<(u64, LogLineData)> {
    let text = match pending.iter().find(|p| p.id.0 == patch) {
        Some(p) => format!(
            "⚖ #{patch} · {} — {}/{}",
            summarize(&p.payload),
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
                format!("⚖ #{patch} · {} — {progress}", k.title)
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

/// A short human label for a surface transition payload.
fn summarize(v: &serde_json::Value) -> String {
    if let Some(obj) = v.as_object() {
        let op = obj.get("op").and_then(serde_json::Value::as_str);
        for key in ["title", "label", "memo", "note", "text", "name", "summary"] {
            if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
                return match op {
                    Some(o) => format!("{o} · {s}"),
                    None => s.to_string(),
                };
            }
        }
        if let Some(o) = op {
            return o.to_string();
        }
    }
    v.to_string()
}

fn surface_name(sf: Surface) -> &'static str {
    match sf {
        Surface::Organization => "Organization",
        Surface::Chat => "Chat",
        Surface::Memory => "Memory",
        Surface::Quests => "Quests",
        Surface::Vault => "Vault",
        Surface::Wallet => "Wallet",
    }
}

/// The default transition op the GUI uses when proposing on a surface.
fn default_op(sf: Surface) -> &'static str {
    match sf {
        Surface::Memory => "add_note",
        Surface::Quests => "add_quest",
        Surface::Vault => "seal_secret",
        Surface::Wallet => "transfer",
        // read-only / ungated surfaces have nothing to propose
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

/// Map an anonymity-network name to its ComboBox index. The dropdown offers
/// tor, nym (greyed — not implemented yet), and none; a lingering "nym" from
/// an old config still displays on its own row rather than masquerading as
/// tor.
fn net_index(s: &str) -> i32 {
    match s {
        "nym" => 1,
        "none" => 2,
        _ => 0,
    }
}

/// Map a ComboBox index back to an anonymity-network name. Index 1 (nym) is
/// non-selectable in the UI, so it is only ever produced by round-tripping an
/// existing nym config.
fn net_name(i: i32) -> String {
    match i {
        1 => "nym",
        2 => "none",
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
    choice_create_title: "Create", "Gründen";
    choice_create_sub: "A new workspace", "Workspace erstellen";
    choice_open_title: "Open", "Öffnen";
    choice_open_sub: "Open a local workspace", "Lokalen Workspace öffnen";
    choice_join_title: "Join", "Beitreten";
    choice_join_sub: "Via an invite link", "Einem Workspace per Einladungslink beitreten";
    choice_restore_title: "Restore", "Wiederherstellen";
    choice_restore_sub: "Seed phrase + backup", "Seed-Phrase + Backup";
    nav_back: "Back", "Zurück";
    field_network: "Anonymity network", "Anonymitäts-Netzwerk";
    not_implemented_yet: "not yet", "noch nicht";
    field_s3_tor: "Route over Tor (onion endpoint)", "Über Tor (Onion-Endpoint)";
    field_s3_onion: "Onion endpoint", "Onion-Endpoint";
    field_tor_mode: "Tor mode", "Tor-Modus";
    field_tor_port: "Tor SOCKS port", "Tor-SOCKS-Port";
    field_smp_server: "SMP messaging server", "SMP-Nachrichtenserver";
    smp_public: "Public default", "Öffentlicher Standard";
    smp_custom: "Custom server", "Eigener Server";
    field_smp_url: "Server URL", "Server-URL";
    smp_test: "Test connection", "Verbindung testen";
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
    cw_net_ok_nym: "Anonymized via the Nym mixnet.", "Anonymisiert via Nym-Mixnet.";
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
    org_surfaces: "Surfaces", "Bereiche";
    ow_title: "Open local workspace", "Lokalen Workspace öffnen";
    ow_empty: "No local workspaces found.", "Keine lokalen Workspaces gefunden.";
    ow_change_folder: "Change folder", "Ordner wechseln";
    ow_col_name: "Name", "Name";
    ow_col_sync: "Last sync", "Letzter Sync";
    ow_open: "Open", "Öffnen";
    ow_delete: "Delete", "Löschen";
    ow_select_hint: "Select a republic to see its status.", "Wähle eine Republik, um ihren Status zu sehen.";
    ow_s3_on: "S3 active", "S3 aktiv";
    ow_s3_off: "No S3", "Kein S3";
    ow_grp_sync: "Sync", "Sync";
    ow_grp_backup: "Backup", "Backup";
    ow_grp_net: "Network", "Netzwerk";
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
    choice_recover_title: "Recover", "Wieder beitreten";
    choice_recover_sub: "Rejoin with a recovery link", "Nach Geräteverlust, mit Recovery-Link";
    om_recover_link: "Create recovery link", "Recovery-Link erstellen";
    rlk_title: "Recovery link", "Recovery-Link";
    rlk_body: "Hand this link to the returning member so they can rejoin this republic from a new device.", "Gib diesen Link dem zurückkehrenden Mitglied, damit es dieser Republik von einem neuen Gerät wieder beitreten kann.";
    rlk_caution: "Share it off-band, over a private channel. It is single-use and dies with this session — after an app restart, mint a fresh one.", "Teile ihn off-band über einen privaten Kanal. Er ist einmalig nutzbar und stirbt mit dieser Sitzung — nach einem Neustart der App einen neuen erstellen.";
    rv_title: "Recover your seat", "Deinen Sitz wiederherstellen";
    rv_body: "Paste the recovery link a surviving member minted for you, plus your recovery phrase.", "Füge den Recovery-Link ein, den ein verbliebenes Mitglied für dich erstellt hat, dazu deine Wiederherstellungs-Phrase.";
    rv_go: "Recover", "Wiederherstellen";
    rv_running_note: "Waiting for the surviving members to approve your re-admission. This human step can take a while — it times out after ~15 minutes.", "Warte auf die Zustimmung der verbliebenen Mitglieder zur Wiederaufnahme. Dieser menschliche Schritt kann dauern — Timeout nach ~15 Minuten.";
    rv_failed_hint: "Recovery links are single-use — ask any surviving member for a fresh one and try again.", "Recovery-Links sind einmalig — bitte ein verbliebenes Mitglied um einen neuen und versuch es erneut.";
    rw_title: "Restore", "Wiederherstellen";
    rw_seed: "Recovery phrase", "Wiederherstellungs-Phrase";
    rw_paste: "Paste", "Einfügen";
    rw_seed_hint: "Needed for every restore path — all keys derive from this phrase.", "Für jeden Weg erforderlich — alle Schlüssel werden aus dieser Phrase abgeleitet.";
    rw_continue: "Continue", "Weiter";
    rw_via_peer: "Social peer-restore", "Social Peer-Restore";
    rw_peer_hint: "Re-syncs the whole workspace from another member.", "Synchronisiert den gesamten Workspace von einem anderen Mitglied.";
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
    toast_s3_ok: "S3 connection OK (mock)", "S3-Verbindung OK (Mock)";
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
    unsaved_title: "Unsaved changes", "Ungespeicherte Änderungen";
    unsaved_body: "You changed settings without saving them. Save them to config.toml, or discard the edits?", "Du hast Einstellungen geändert, aber nicht gespeichert. In die config.toml speichern oder die Änderungen verwerfen?";
    unsaved_save: "Save & continue", "Speichern & weiter";
    unsaved_discard: "Discard & continue", "Verwerfen & weiter";
    unsaved_cancel: "Cancel", "Abbruch";
    mv_send: "Send", "Senden";
    mv_propose: "Propose", "Vorschlagen";
    mv_approve: "Approve", "Zustimmen";
    mv_decline: "Decline", "Ablehnen";
    mv_pending: "Pending", "Ausstehend";
    mv_applied: "Applied", "Angewandt";
    mv_chat_ph: "Write a message…", "Nachricht schreiben…";
    mv_propose_ph: "Describe a proposal…", "Vorschlag beschreiben…";
    mv_empty_chat: "No messages yet.", "Noch keine Nachrichten.";
    mv_later: "Nothing here yet — this view comes with a later build.", "Hier ist noch nichts — diese Ansicht kommt mit einem späteren Build.";
    mv_empty_pending: "Nothing awaiting approval.", "Nichts wartet auf Zustimmung.";
    mv_empty_applied: "Nothing applied yet.", "Noch nichts angewandt.";
    mv_deleted_by: "deleted by", "gelöscht durch";
    ch_channels: "Channels", "Kanäle";
    ch_group: "Group", "Gruppe";
    ch_new_topic: "New topic", "Neues Thema";
    ch_topic_ph: "Topic name…", "Themenname…";
    ch_topic_open: "Open topic", "Thema öffnen";
    mv_file_gone: "File no longer available — its owner deleted it.", "Datei nicht mehr verfügbar — der Besitzer hat sie gelöscht.";
    toast_download: "Downloading (mock):", "Lade herunter (Mock):";
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

    /// An engine-authored System-kind message maps onto the same per-line
    /// `system` flag the governance rows use — one quiet rendering path,
    /// never a second style; a User message stays a normal card.
    #[test]
    fn a_system_kind_message_maps_onto_the_quiet_line_flag() {
        let user = ChatMessage::text(MessageId([1; 16]), "petra", "gm", 100);
        assert!(!chat_line(&user, "me").system);
        let notice = ChatMessage::text(MessageId([2; 16]), "petra", "🔑 back", 101)
            .with_kind(molt_core::ChatKind::System);
        assert!(chat_line(&notice, "me").system);
    }

    /// The recovery flow rides the transient session notice (the engine's
    /// contract: `recovery-link:` / `recover-started:` / `recover-failed:` /
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
    fn derive_channels_lists_group_first_then_patches_then_topics_with_unread() {
        let infos = vec![
            ChannelInfo {
                channel: ChannelRef::Topic { name: "zeta".into() },
                count: 4,
                last_ts: 40,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(7) },
                count: 1,
                last_ts: 30,
            },
            ChannelInfo {
                channel: ChannelRef::Topic { name: "Alpha".into() },
                count: 2,
                last_ts: 20,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(3) },
                count: 5,
                last_ts: 10,
            },
            ChannelInfo {
                channel: ChannelRef::Group,
                count: 9,
                last_ts: 50,
            },
        ];
        let titles = HashMap::from([(3u64, "raise budget".to_string())]);
        let unread = HashMap::from([("topic:zeta".to_string(), 4usize), ("group".to_string(), 1)]);
        let rows = derive_channels(&infos, &titles, &unread);
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["group", "patch:3", "patch:7", "topic:Alpha", "topic:zeta"],
            "group first, then patches by id, then topics by name"
        );
        assert_eq!(rows[1].label, "raise budget", "patch title from proposal state");
        assert_eq!(rows[2].label, "#7", "unknown proposal → #id fallback, never an error");
        assert_eq!(rows[3].label, "Alpha");
        assert_eq!(
            rows.iter().map(|r| r.unread).collect::<Vec<_>>(),
            [1, 0, 0, 0, 4]
        );
        // group is synthesized even when the engine list lacks it
        let rows = derive_channels(&[], &HashMap::new(), &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "group");
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
        };
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(4, &[pv], &HashMap::new(), &first_seen);
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
        let sys_unknown = patch_system_lines(9, &[], &HashMap::new(), &first_seen);
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
        };
        let mut known = HashMap::new();
        // while pending: cached with title + progress
        update_known_proposals(&mut known, std::slice::from_ref(&pv), &HashMap::new());
        assert_eq!(known[&4].title, "add_note · budget");
        assert_eq!(known[&4].fate, KnownFate::Pending);

        // the proposal leaves the Proposed-only window and its payload
        // shows up in the surface's applied log → Applied
        let applied = HashMap::from([(Surface::Memory, vec![pv.payload.clone()])]);
        update_known_proposals(&mut known, &[], &applied);
        assert_eq!(known[&4].fate, KnownFate::Applied);

        // the system line keeps the title and renders the sealed state
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(4, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("budget") && text.contains('✓'), "{text}");
        assert!(text.contains("3/3"), "sealed at the threshold: {text}");

        // the sidebar label survives via the cache-derived titles
        let titles = known_titles(&known);
        let infos = vec![ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(4) },
            count: 1,
            last_ts: 10,
        }];
        let rows = derive_channels(&infos, &titles, &HashMap::new());
        assert_eq!(rows[1].label, "add_note · budget", "title survives Applied");

        // vanished WITHOUT an applied trace: the read contract cannot tell
        // Rejected from expired — neutral closed marker, title kept, no
        // fabricated verdict
        let pv9 = ProposalView {
            id: ProposalId(9),
            payload: serde_json::json!({ "title": "drop the fee" }),
            ..pv.clone()
        };
        update_known_proposals(&mut known, std::slice::from_ref(&pv9), &applied);
        update_known_proposals(&mut known, &[], &applied);
        assert_eq!(known[&9].fate, KnownFate::Closed);
        let sys = patch_system_lines(9, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("drop the fee") && text.contains('⊘'), "{text}");
        assert!(!text.contains('✓') && !text.contains('✗'), "{text}");

        // an id never seen anywhere still tolerates (concept Q4)
        let sys = patch_system_lines(77, &[], &known, &first_seen);
        assert_eq!(sys[0].1.text, "⚖ #77");

        // a Closed verdict corrects itself when the applied value shows up
        // in a later read (an out-of-order pass must not stick a wrong fate)
        let applied9 = HashMap::from([(
            Surface::Memory,
            vec![serde_json::json!({ "title": "drop the fee" })],
        )]);
        update_known_proposals(&mut known, &[], &applied9);
        assert_eq!(known[&9].fate, KnownFate::Applied);
        // … while an already-Applied fate is sticky even if the surface
        // read is missing this pass
        update_known_proposals(&mut known, &[], &HashMap::new());
        assert_eq!(known[&4].fate, KnownFate::Applied);
        assert_eq!(known[&9].fate, KnownFate::Applied);
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
    fn when_label_relative_part() {
        let ts = 1_750_000_000_u64;
        let at = |offset: i64| when_label_at(ts, 1_750_000_000 + offset);
        assert!(at(5).ends_with("(just now)"), "{}", at(5));
        assert!(at(60).ends_with("(~1 minute ago)"), "{}", at(60));
        assert!(at(20 * 60).ends_with("(~20 minutes ago)"), "{}", at(1200));
        assert!(at(2 * 3600).ends_with("(~2 hours ago)"), "{}", at(7200));
        assert!(at(3 * 86_400).ends_with("(~3 days ago)"), "{}", at(259_200));
    }

    #[test]
    fn sync_status_label_matches_the_demo_prose() {
        assert_eq!(sync_status_label(0, 0, 0), "Synced · just now");
        assert_eq!(sync_status_label(0, 2, 0), "Synced · 2 min ago");
        assert_eq!(sync_status_label(0, 60, 0), "Synced · 1 h ago");
        assert_eq!(sync_status_label(1, 0, 80), "Syncing… 80 items left");
        assert_eq!(sync_status_label(2, 4320, 0), "Offline · last sync 3 d ago");
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
            seed: "".into(),
            net: "".into(),
            members: ModelRc::new(VecModel::from(Vec::new())),
        }
    }

    #[test]
    fn size_and_backup_labels() {
        assert_eq!(size_label(920), "920 KiB");
        assert_eq!(size_label(1840), "1.8 MiB");
        assert_eq!(backup_when_label(molt_core::WorkspaceInfo::NEVER), "never");
        assert_eq!(backup_when_label(0), "just now");
        assert_eq!(backup_when_label(30), "30 min ago");
        assert_eq!(backup_when_label(129_600), "90 d ago");
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
}
