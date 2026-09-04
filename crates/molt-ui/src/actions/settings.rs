// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings-screen callbacks: language / fonts / theme, the draft save
//! (plain, rotate-token, leave-guarded), the S3 and Tor probes, the backup
//! tab, the WM close guard and the sound preview.

use molt_core::Command;
use slint::{ComponentHandle, Global, Model, ModelRc, VecModel};

use crate::alerts::play_alert;
use crate::i18n::error_toast;
use crate::labels::{theme_name, to_screen};
use crate::mirror::sort_bk_rows;
use crate::net_tor::tor_probe_args;
use crate::settings::{
    apply_settings_fields, browse_start_dir, read_settings_draft, save_draft, settings_draft_differs,
    stored_settings,
};
use crate::app::Ctx;
use crate::{AppScreen, AppWindow, BackupRow, Strings, Theme};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    {
        let cx = ctx.clone();
        ui.on_set_language(move |idx| {
            let lang = if idx == 1 { "de" } else { "en" }.to_string();
            cx.issue(Command::SetLanguage { lang });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_set_fonts(move |app, nav, editor| {
            cx.issue(
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
        let cx = ctx.clone();
        ui.global::<Theme>().on_picked(move |i| {
            cx.issue(
                Command::SetTheme {
                    theme: theme_name(i),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_save_settings(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui, &stored_settings(&cx.last_settings));
            let wake = ui.get_cfg_poke_wake().to_string();
            cx.issue_draft(wake, settings);
        });
    }

    {
        // Rotate the MCP token: mint a fresh one, drop it into the draft, and
        // persist the settings in one go (Slint cannot generate randomness).
        let cx = ctx.clone();
        ui.on_rotate_token(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            // a failed mint leaves the OLD token in place: overwriting it
            // with "" would silently disable MCP authentication on save
            let Ok(token) = molt_config::random_token() else {
                ui.invoke_show_toast_error(Strings::get(&ui).get_set_token_failed());
                return;
            };
            ui.set_cfg_mcp_token(token.into());
            let settings = read_settings_draft(&ui, &stored_settings(&cx.last_settings));
            let wake = ui.get_cfg_poke_wake().to_string();
            cx.issue_draft(wake, settings);
        });
    }

    {
        // Issue or rotate the READ-ONLY key (`knowledge_base_scale.md`
        // §4.7). Same mint, same failure posture as the seat key.
        let cx = ctx.clone();
        ui.on_rotate_read_token(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let Ok(token) = molt_config::random_token() else {
                ui.invoke_show_toast_error(Strings::get(&ui).get_set_token_failed());
                return;
            };
            ui.set_cfg_mcp_read_token(token.into());
            let settings = read_settings_draft(&ui, &stored_settings(&cx.last_settings));
            let wake = ui.get_cfg_poke_wake().to_string();
            cx.issue_draft(wake, settings);
        });
    }

    {
        // Revoke the read-only key: empty IS the value (read access off),
        // and the config writer drops the key rather than storing "".
        let cx = ctx.clone();
        ui.on_revoke_read_token(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            ui.set_cfg_mcp_read_token(String::new().into());
            let settings = read_settings_draft(&ui, &stored_settings(&cx.last_settings));
            let wake = ui.get_cfg_poke_wake().to_string();
            cx.issue_draft(wake, settings);
        });
    }

    {
        // Probe the S3 backup target in the draft (not the saved settings),
        // so the user can validate endpoint + credentials before saving.
        // The engine runs a real SigV4-signed HEAD over the configured
        // dialer; the verdict streams back into `cfg-s3-test`.
        let cx = ctx.clone();
        ui.on_test_s3(move |target| {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            // one account for every bucket — only the bucket differs, and
            // all of it comes from the DRAFT so an unsaved edit is probed
            let (target, bucket) = if target == "media" {
                (molt_core::S3Target::Media, ui.get_cfg_media_s3_bucket())
            } else {
                (molt_core::S3Target::Workspaces, ui.get_cfg_s3_bucket())
            };
            cx.issue(
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
        let cx = ctx.clone();
        ui.on_test_tor(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let (network, mode, port) = tor_probe_args(
                ui.get_cfg_network_index(),
                ui.get_cfg_tor_mode_index(),
                ui.get_cfg_tor_port(),
            );
            cx.issue(
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
        let cx = ctx.clone();
        ui.on_list_backups(move || {
            cx.issue(Command::NetListBackups);
        });
    }

    {
        // S7: fetch a bucket-only workspace onto this device — sealed; the
        // outcome arrives on the session notice (backup-fetched/-failed)
        let cx = ctx.clone();
        ui.on_backup_fetch(move |id| {
            if let Some(ui) = cx.weak.upgrade() {
                ui.set_bk_fetched("".into());
                ui.set_bk_fetch_error("".into());
            }
            cx.issue(Command::BackupFetch { id: id.to_string() });
        });
    }

    {
        // Leaving settings is guarded: a clean draft navigates straight back;
        // a dirty one raises the unsaved-changes modal (save / discard / stay).
        let cx = ctx.clone();
        ui.on_close_settings(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let dirty = cx.last_settings
                .lock()
                .ok()
                .and_then(|l| l.clone())
                .is_some_and(|s| settings_draft_differs(&s, &ui));
            if dirty {
                ui.set_confirm_leave_open(true);
            } else {
                cx.issue(
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
        let cx = ctx.clone();
        ui.on_save_and_leave(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let settings = read_settings_draft(&ui, &stored_settings(&cx.last_settings));
            let wake = ui.get_cfg_poke_wake().to_string();
            let screen = to_screen(ui.get_settings_return());
            let w = cx.wallet.clone();
            let weak = ui.as_weak();
            cx.rt.spawn(async move {
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
        let cx = ctx.clone();
        ui.on_discard_and_leave(move || {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            if let Some(s) = cx.last_settings.lock().ok().and_then(|l| l.clone()) {
                apply_settings_fields(&ui, &s);
            }
            cx.issue(
                Command::Navigate {
                    screen: to_screen(ui.get_settings_return()),
                },
            );
        });
    }

    // Intercept the OS/WM window close: with a workspace open OR any engine
    // run (restore / founding / join) in flight, keep the window and raise
    // the quit-confirm modal instead of closing outright (the in-app × is
    // disabled during a run; the WM button must not be a silent bypass).
    // Likewise, quitting from the settings screen with unsaved draft edits
    // raises the save/discard/stay modal instead of silently dropping them.
    {
        let cx = ctx.clone();
        ui.window().on_close_requested(move || {
            if let Some(ui) = cx.weak.upgrade() {
                if ui.get_screen() == AppScreen::Main || ui.get_run_active() {
                    ui.set_confirm_quit_open(true);
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
                let dirty = ui.get_screen() == AppScreen::Settings
                    && cx.last_settings
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

    // sound preview in the settings panel — plays the picked alert once
    {
        ui.on_test_sound(move |kind| {
            play_alert(kind.as_str());
        });
    }

    // browse for the workspace folder via the native dialog (async XDG
    // portal, like the logo picker) — the picked path lands in the modal's
    // draft field, which stays hand-editable as a fallback
    {
        let cx = ctx.clone();
        ui.on_ws_dir_pick(move || {
            let weak = cx.weak.clone();
            // only the property read happens on the UI thread; the stat in
            // browse_start_dir moves to a blocking task (a draft pointing at
            // a hung mount must not freeze the event loop)
            let draft = weak
                .upgrade()
                .map(|ui| ui.get_ws_dir_draft().to_string())
                .unwrap_or_default();
            cx.rt.spawn(async move {
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
}
