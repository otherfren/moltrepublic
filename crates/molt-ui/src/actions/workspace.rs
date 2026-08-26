// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace lifecycle callbacks: navigation between screens and surfaces,
//! open / seal / unseal / close / delete / export, the backup toggle, the
//! Open-list sort and the restore wizard.

use molt_core::{Command, Surface};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::labels::to_screen;
use crate::mirror::sort_ws_items;
use crate::{AppWindow, Ctx, WorkspaceItem};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    // --- actions: each becomes a Command on the shared engine ---
    {
        let cx = ctx.clone();
        ui.on_navigate(move |screen| {
            cx.issue(
                Command::Navigate {
                    screen: to_screen(screen),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_open_workspace(move |id| {
            cx.issue(
                Command::OpenWorkspace { id: id.to_string() },
            );
        });
    }

    // real at-rest sealing (S6) — same commands as the MCP
    // encrypt_/decrypt_workspace tools; the engine verifies the phrase
    {
        let cx = ctx.clone();
        ui.on_encrypt_workspace(move |id, phrase| {
            cx.issue(
                Command::EncryptWorkspace {
                    id: id.to_string(),
                    phrase: phrase.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_decrypt_workspace(move |id, phrase| {
            cx.issue(
                Command::DecryptWorkspace {
                    id: id.to_string(),
                    phrase: phrase.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_close_workspace(move || {
            cx.issue(Command::CloseWorkspace);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_delete_workspace(move |id| {
            cx.issue(
                Command::DeleteWorkspace { id: id.to_string() },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_set_ws_backup(move |id, enabled| {
            cx.issue(
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
        let cx = ctx.clone();
        ui.on_export_workspace(move |id, dest, passphrase| {
            cx.issue(
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

    {
        let cx = ctx.clone();
        ui.on_restore_start(move |way, target, secret| {
            cx.issue(
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
        let cx = ctx.clone();
        ui.on_restore_cancel(move || {
            cx.issue(Command::RestoreCancel);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_restore_finish(move || {
            cx.issue(Command::RestoreFinish);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_select_surface(move |key| {
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            cx.issue(Command::SelectSurface { surface });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_select_view(move |key, view| {
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            cx.issue(
                Command::SelectView {
                    surface,
                    view: view.to_string(),
                },
            );
        });
    }
}
