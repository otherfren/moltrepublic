// SPDX-License-Identifier: GPL-3.0-or-later
//! Vault callbacks (`docs/vault/vault_threshold_disclosure.md` §7, §8):
//! pure builders from the dialogs' state to the four vault commands, the
//! read reply rendered into this session's Unsealed rows, and the glue
//! that wires both to the engine.

use molt_core::vault::{SecretText, VAULT_PAYLOAD_MAX};
use molt_core::{Command, Reply};
use slint::{ComponentHandle, Model};

use crate::app::Ctx;
use crate::i18n::error_toast;
use crate::models::sync_rows;
use crate::{AppWindow, Strings, VaultDepositRow, VaultUnsealedRow};

/// The seal dialog's draft as a `VaultSeal`; `None` while a field is
/// empty or the text is over the cap (the dialog's confirm gate).
pub(crate) fn seal_command(ui: &AppWindow) -> Option<Command> {
    let name = ui.get_vt_seal_name().trim().to_string();
    let kind = ui.get_vt_seal_kind().trim().to_string();
    let text = ui.get_vt_seal_text().to_string();
    if name.is_empty() || kind.is_empty() || text.is_empty() || text.len() > VAULT_PAYLOAD_MAX {
        return None;
    }
    Some(Command::VaultSeal {
        name,
        kind,
        text: SecretText(text),
    })
}

/// The grant dialog's pick as a `VaultGrant`; `None` without a seat.
pub(crate) fn grant_command(ui: &AppWindow) -> Option<Command> {
    let secret_id = ui.get_vt_grant_secret().to_string();
    let i = usize::try_from(ui.get_vt_grant_reader()).ok()?;
    let reader = ui.get_vault_seats().row_data(i)?.to_string();
    if secret_id.is_empty() || reader.is_empty() {
        return None;
    }
    Some(Command::VaultGrant { secret_id, reader })
}

/// A read of one granted version.
pub(crate) fn read_command(secret_id: &str) -> Command {
    Command::VaultRead {
        secret_id: secret_id.to_string(),
    }
}

/// A re-seal of one own version (D15).
pub(crate) fn reseal_command(secret_id: &str) -> Command {
    Command::VaultReseal {
        secret_id: secret_id.to_string(),
    }
}

/// Render a `VaultRead` reply into the Unsealed rows (upsert by
/// `secret_id`): the text, or the wait for answers. Anything else is not
/// a read reply and changes nothing (`false`).
pub(crate) fn apply_vault_read_reply(ui: &AppWindow, reply: &Reply) -> bool {
    let row = match reply {
        Reply::VaultText {
            secret_id,
            name,
            kind,
            text,
        } => VaultUnsealedRow {
            secret_id: secret_id.as_str().into(),
            name: name.as_str().into(),
            kind: kind.as_str().into(),
            text: text.0.as_str().into(),
            status: Default::default(),
        },
        Reply::VaultPending {
            secret_id,
            have,
            need,
        } => {
            let deposits = ui.get_vault_deposits();
            let card = deposits.iter().find(|d| d.secret_id.as_str() == secret_id);
            let waiting = ui.global::<Strings>().get_vt_waiting();
            VaultUnsealedRow {
                secret_id: secret_id.as_str().into(),
                name: card.as_ref().map(|d| d.name.clone()).unwrap_or_default(),
                kind: card.map(|d| d.kind).unwrap_or_default(),
                text: Default::default(),
                status: format!("{waiting} {have}/{need}").into(),
            }
        }
        _ => return false,
    };
    let current = ui.get_vault_unsealed();
    let mut rows: Vec<VaultUnsealedRow> = current.iter().collect();
    match rows.iter_mut().find(|r| r.secret_id == row.secret_id) {
        Some(r) => *r = row,
        None => rows.push(row),
    }
    sync_rows(&current, rows, |m| ui.set_vault_unsealed(m));
    true
}

/// A read of `secret_id` is still waiting for answers in this session.
pub(crate) fn vault_read_waiting(ui: &AppWindow, secret_id: &str) -> bool {
    ui.get_vault_unsealed()
        .iter()
        .any(|r| r.secret_id.as_str() == secret_id && !r.status.is_empty())
}

/// Forget every text read in this session (a workspace closed or
/// switched): plaintext lives in UI memory only.
pub(crate) fn clear_vault_reads(ui: &AppWindow) {
    sync_rows(&ui.get_vault_unsealed(), Vec::new(), |m| {
        ui.set_vault_unsealed(m)
    });
}

/// The dialogs' pure helpers: UTF-8 byte count (the cap is bytes, Slint
/// counts chars) and the replace check over the own deposits.
pub(crate) fn wire_local(ui: &AppWindow) {
    ui.set_vault_text_max(i32::try_from(VAULT_PAYLOAD_MAX).unwrap_or(i32::MAX));
    ui.on_vt_text_bytes(|text| i32::try_from(text.len()).unwrap_or(i32::MAX));
    ui.on_vt_mine_named(|name, deposits| {
        let name = name.trim();
        !name.is_empty()
            && deposits
                .iter()
                .any(|d: VaultDepositRow| d.mine && d.name.as_str() == name)
    });
}

/// Run one read on the engine and render its reply on the UI thread.
pub(crate) async fn run_read(
    wallet: &molt_engine::WalletHandle,
    weak: &slint::Weak<AppWindow>,
    secret_id: String,
) {
    let outcome = wallet.execute(read_command(&secret_id)).await;
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        match outcome {
            Ok(reply) => {
                apply_vault_read_reply(&ui, &reply);
            }
            Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
        }
    });
}

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    wire_local(ui);
    {
        let cx = ctx.clone();
        ui.on_vault_seal_confirm(move || {
            let Some(ui) = cx.weak.upgrade() else { return };
            if let Some(cmd) = seal_command(&ui) {
                cx.issue(cmd);
            }
        });
    }
    {
        let cx = ctx.clone();
        ui.on_vault_grant_confirm(move || {
            let Some(ui) = cx.weak.upgrade() else { return };
            if let Some(cmd) = grant_command(&ui) {
                cx.issue(cmd);
            }
        });
    }
    {
        let cx = ctx.clone();
        ui.on_vault_reseal(move |id| cx.issue(reseal_command(&id)));
    }
    {
        let cx = ctx.clone();
        ui.on_vault_read(move |id| {
            if let Some(ui) = cx.weak.upgrade() {
                ui.invoke_select_view("vault".into(), "unsealed".into());
            }
            let cx = cx.clone();
            let id = id.to_string();
            cx.rt.clone().spawn(async move {
                run_read(&cx.wallet, &cx.weak, id).await;
            });
        });
    }
}
