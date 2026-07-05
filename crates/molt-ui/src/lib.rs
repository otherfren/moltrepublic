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

use molt_core::{
    ChatMessage, Command, Event, ProposalId, Reply, Screen, SessionScope, SessionSettings,
    SessionView, Surface, SurfaceSnapshot,
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
        ui.on_send_chat(move |body, quote| {
            let body = body.trim().to_string();
            if body.is_empty() {
                return;
            }
            let quote = u64::try_from(quote).ok();
            issue(&rt, &w, &weak, Command::Chat { body, quote });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_delete_chat(move |index| {
            let Ok(index) = u64::try_from(index) else {
                return;
            };
            issue(&rt, &w, &weak, Command::DeleteChat { index });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_share_pick(move || {
            let w = w.clone();
            let weak = weak.clone();
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
        ui.on_download_file(move |index| {
            let Ok(index) = u64::try_from(index) else {
                return;
            };
            issue(&rt, &w, &weak, Command::DownloadFile { index });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_remove_file(move |index| {
            let Ok(index) = u64::try_from(index) else {
                return;
            };
            issue(&rt, &w, &weak, Command::RemoveFile { index });
        });
    }
    {
        let rt = rt.clone();
        let w = wallet.clone();
        let weak = ui.as_weak();
        ui.on_toggle_reaction(move |index, emoji| {
            let Ok(index) = u64::try_from(index) else {
                return;
            };
            issue(
                &rt,
                &w,
                &weak,
                Command::ReactChat {
                    index,
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
        rt.spawn(async move {
            let mut rx = w.subscribe();
            push_session(&w, &weak, &last_settings, SessionScope::Full).await;
            push_surfaces(&w, &weak).await;
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
                            push_surfaces(&w, &weak).await;
                        }
                    }
                    // Any surface event (chat / propose / approve / …) re-reads
                    // the surfaces, so the GUI mirrors what an MCP agent did.
                    Ok(_) => push_surfaces(&w, &weak).await,
                    Err(RecvError::Lagged(_)) => {
                        push_session(&w, &weak, &last_settings, SessionScope::Full).await;
                        push_surfaces(&w, &weak).await;
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
    // persistent restart warning: which changed keys only apply on restart
    ui.set_restart_keys(sv.restart_required.join(", ").into());

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
}

/// Mirror the three engine-run lifecycles (the engine ticks them at 90 ms;
/// a `SessionChanged` with a run scope re-renders ONLY this, so the rest of
/// the window keeps its focus/scroll state untouched).
fn apply_runs(ui: &AppWindow, sv: &SessionView) {
    // restore
    ui.set_rw_step(i32::from(sv.restore.run.step));
    ui.set_rw_way(sv.restore.way.clone().into());
    ui.set_rw_target(sv.restore.target.clone().into());
    ui.set_rw_progress(f32::from(sv.restore.run.progress_pct) / 100.0);
    ui.set_rw_outcome(i32::from(sv.restore.run.outcome));
    sync_strings(&ui.get_rw_log(), &sv.restore.run.log, |m| ui.set_rw_log(m));

    // founding; the run header is composed here so an MCP-started founding
    // shows real values even with an empty local form
    ui.set_cw_step(i32::from(sv.create.run.step));
    ui.set_cw_progress(f32::from(sv.create.run.progress_pct) / 100.0);
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
    sync_strings(&ui.get_cw_invites(), &sv.create.invites, |m| {
        ui.set_cw_invites(m)
    });

    // join
    ui.set_jw_step(i32::from(sv.join.run.step));
    ui.set_jw_progress(f32::from(sv.join.run.progress_pct) / 100.0);
    ui.set_jw_outcome(i32::from(sv.join.run.outcome));
    ui.set_jw_republic(sv.join.republic.clone().into());
    ui.set_jw_rule(format!("{}-of-{}", sv.join.rule_m, sv.join.rule_n).into());
    ui.set_jw_inviter(sv.join.inviter.clone().into());
    sync_strings(&ui.get_jw_log(), &sv.join.run.log, |m| ui.set_jw_log(m));
}

/// Plain, `Send` snapshot of all surfaces, built off the UI thread.
struct SurfacesBundle {
    member: String,
    threshold_badge: String,
    surfaces: Vec<SurfaceData>,
}
struct SurfaceData {
    key: String,
    name: String,
    gated: bool,
    log: Vec<LogLineData>,
    pending: Vec<ProposalRowData>,
}
struct LogLineData {
    lead: String,
    text: String,
    when: String,
    quote: i32,
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
async fn push_surfaces(wallet: &WalletHandle, weak: &slint::Weak<AppWindow>) {
    let (member, threshold_badge) = match wallet.execute(Command::Status).await {
        Ok(Reply::Status(s)) => (s.member, format!("{}-of-{}", s.threshold, s.members.len())),
        _ => return,
    };
    let mut surfaces = Vec::new();
    for sf in Surface::ALL {
        if let Ok(Reply::State(snap)) = wallet.execute(Command::ReadState { surface: sf }).await {
            surfaces.push(surface_data(sf, &snap, &member));
        }
    }
    let bundle = SurfacesBundle {
        member,
        threshold_badge,
        surfaces,
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
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
                        lead: l.lead.clone().into(),
                        text: l.text.clone().into(),
                        when: l.when.clone().into(),
                        quote: l.quote,
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

/// Project one surface snapshot into plain display data. `me` is the local
/// member handle — it marks own messages and the own reaction pill.
fn surface_data(sf: Surface, snap: &SurfaceSnapshot, me: &str) -> SurfaceData {
    let mut log: Vec<LogLineData> = if sf == Surface::Chat {
        snap.applied
            .iter()
            .filter_map(|v| serde_json::from_value::<ChatMessage>(v.clone()).ok())
            .map(|m| chat_line(&m, me))
            .collect()
    } else {
        snap.applied
            .iter()
            .map(|v| LogLineData {
                lead: String::new(),
                text: summarize(v),
                when: String::new(),
                quote: -1,
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
    annotate_chat_log(&mut log);
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

/// One typed chat message, projected for display.
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
        lead: m.from.clone(),
        text: m.body.clone(),
        when: if m.ts > 0 {
            when_label(m.ts)
        } else {
            String::new()
        },
        quote: m.quote.and_then(|q| i32::try_from(q).ok()).unwrap_or(-1),
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
/// and the quote teasers (which need the quoted line resolved).
fn annotate_chat_log(log: &mut [LogLineData]) {
    let mut alt = false;
    let mut prev_lead: Option<String> = None;
    for line in log.iter_mut() {
        if prev_lead.as_deref().is_some_and(|p| p != line.lead) {
            alt = !alt;
        }
        line.alt = alt;
        // the author header (name + time) shows once per author block
        line.first = prev_lead.as_deref() != Some(line.lead.as_str());
        prev_lead = Some(line.lead.clone());
    }
    for i in 0..log.len() {
        let q = log[i].quote;
        if q >= 0 {
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

/// Map an anonymity-network name to its ComboBox index. The settings
/// dropdown offers only tor and none; a lingering "nym" from an old
/// config displays as tor.
fn net_index(s: &str) -> i32 {
    match s {
        "none" => 1,
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
    field_tor_mode: "Tor mode", "Tor-Modus";
    field_tor_port: "Tor SOCKS port", "Tor-SOCKS-Port";
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
    mock_banner: "Mock: confirming does NOT create a workspace and writes nothing to disk.", "Mock: Bestätigen legt KEINEN Workspace an und schreibt nichts auf die Platte.";
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
    cw_found: "Found republic", "Republik gründen";
    cw_ph1: "Generating group secret…", "Erzeuge Gruppengeheimnis…";
    cw_ph2: "Deriving member shares…", "Leite Mitglieds-Shares ab…";
    cw_ph3: "Sealing workspace & minting invites…", "Versiegle Workspace & präge Einladungen…";
    cw_invites: "Invites", "Einladungen";
    cw_invites_hint: "One link per future member — share each once, over a private channel.", "Ein Link pro künftigem Mitglied — jeden nur einmal teilen, über einen privaten Kanal.";
    enter_republic: "Enter republic", "Republik betreten";
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

    fn line(lead: &str, text: &str) -> LogLineData {
        LogLineData {
            lead: lead.to_string(),
            text: text.to_string(),
            when: String::new(),
            quote: -1,
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

    #[test]
    fn annotate_chat_log_author_blocks_and_teasers() {
        let mut log = vec![
            line("me", "first"),
            line("me", "second"),
            line("ashi", "answer"),
            line("me", "back"),
        ];
        log[2].quote = 0;
        log[3].quote = 99; // out of range
        annotate_chat_log(&mut log);
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
        assert_eq!(log[3].quote, -1, "dangling quotes are dropped");
    }

    #[test]
    fn annotate_chat_log_teases_deleted_quotes_with_ellipsis() {
        let mut log = vec![line("me", ""), line("ashi", "reply")];
        log[0].deleted_by = "me".to_string();
        log[1].quote = 0;
        annotate_chat_log(&mut log);
        assert_eq!(log[1].quote_label, "me: …");
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
