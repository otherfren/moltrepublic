// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings draft: reading the config-tab form into a
//! [`SessionSettings`], pushing stored settings back into the fields, the
//! dirty check behind the leave guard, the MiB quota rendering and the
//! ordered three-door save.

use std::sync::{Arc, Mutex};

use molt_core::{Command, SessionSettings};
use molt_engine::WalletHandle;
use tokio::runtime::Handle;

use crate::alerts::{sound_index, sound_name};
use crate::i18n::error_toast;
use crate::net_tor::{mode_index, mode_name, net_index, net_name};
use crate::AppWindow;

/// The directory the workspace-folder browse dialog should start in, given
/// the modal's hand-editable draft: the draft — its leading `~` expanded the
/// same way the engine resolves the setting (`molt_storage::expand_tilde`,
/// so the config default "~/…" starts at the real folder) — when it names an
/// existing directory, otherwise `None`: an empty draft or a typo must not
/// derail the dialog (rfd then opens at its platform default). Runs a stat —
/// call it off the UI thread (the draft may point at a slow mount).
pub(crate) fn browse_start_dir(draft: &str) -> Option<std::path::PathBuf> {
    let path = molt_storage::expand_tilde(draft);
    path.is_dir().then_some(path)
}

/// Does the settings form hold unsaved edits against `stored`?
///
/// The relay pool is deliberately excluded: it is NOT part of the settings
/// draft (it is edited live through the `Relay*` commands, and
/// [`read_settings_draft`] therefore always yields an empty pool). Comparing
/// it would make every node that has a relay look permanently "dirty" — the
/// leave-guard would fire on every exit and an external settings change would
/// be suppressed as "the user is editing".
pub(crate) fn settings_draft_differs(stored: &SessionSettings, ui: &AppWindow) -> bool {
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
pub(crate) fn stored_settings(last: &Arc<Mutex<Option<SessionSettings>>>) -> SessionSettings {
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
pub(crate) fn mib_label(bytes: u64) -> String {
    bytes.div_ceil(MIB).to_string()
}

/// The field's text back to bytes — keeping the STORED byte value whenever
/// it still renders as the same MiB. A `s3_max_bytes = 500000000` written by
/// hand shows as 477 MiB and stays 500000000 unless the operator actually
/// changes the number; without this, saving an unrelated setting would
/// silently round every quota onto the MiB grid. An emptied field is 0 (no
/// limit); text that is not a number at all keeps the stored value rather
/// than inventing one.
pub(crate) fn mib_text_to_bytes(text: &str, stored: u64) -> u64 {
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
pub(crate) fn read_settings_draft(ui: &AppWindow, stored: &SessionSettings) -> SessionSettings {
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
        shared_files: ui.get_cfg_shared_files(),
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

/// The settings draft's three doors, in ORDER and in ONE task: the wake
/// command (a local shell hook — its own door, so no other surface can
/// plant one), the host posture with both secrets (`SetNodePosture` —
/// the GUI's door; MCP operates the seat, not the machine), then the
/// wholesale save, which re-merges the stored posture and so must land
/// LAST. "Save & continue" and "Rotate token" used to skip the wake door
/// and lose an edited command (review 2026-08-25 F3).
pub(crate) async fn save_draft(
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
pub(crate) fn issue_draft(
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

/// The Shared Files store is usable: switched on, with the shared account
/// and ITS bucket set (the backup bucket is not the store).
pub(crate) fn files_ready(s: &SessionSettings) -> bool {
    s.shared_files
        && !s.s3_endpoint.is_empty()
        && !s.s3_access_key.is_empty()
        && !s.media_s3_bucket.is_empty()
}

/// Push one settings value into the draft form fields (the mirror on real
/// changes, and the leave-guard's "discard" reset).
pub(crate) fn apply_settings_fields(ui: &AppWindow, s: &SessionSettings) {
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
    ui.set_cfg_shared_files(s.shared_files);
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
