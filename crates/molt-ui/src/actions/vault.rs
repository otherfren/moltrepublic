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
    if !label_ok(&name) || !label_ok(&kind) || text.is_empty() || text.len() > VAULT_PAYLOAD_MAX {
        return None;
    }
    Some(Command::VaultSeal {
        name,
        kind,
        text: SecretText(text),
    })
}

/// A name or kind the builder takes; also the dialog's `vt-label-ok`.
fn label_ok(s: &str) -> bool {
    !s.trim().is_empty()
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
            // the two read tasks post in any order: never downgrade a text
            let delivered = ui
                .get_vault_unsealed()
                .iter()
                .any(|r| r.secret_id.as_str() == secret_id && !r.text.is_empty());
            if delivered {
                return true;
            }
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

/// `VaultReadable` re-reads only a read this session is waiting on.
pub(crate) fn should_reread(ui: &AppWindow, secret_id: &str) -> bool {
    vault_read_waiting(ui, secret_id)
}

/// A seal left for the engine: no second confirm until it answers.
pub(crate) fn seal_issued(ui: &AppWindow) {
    ui.set_vt_seal_busy(true);
}

/// The engine answered the seal: an accept wipes the draft and closes
/// the dialog, a refusal leaves both for a retry. A reply after a
/// workspace reset finds nothing in flight and touches nothing.
pub(crate) fn seal_settled(ui: &AppWindow, ok: bool) {
    if !ui.get_vt_seal_busy() {
        return;
    }
    ui.set_vt_seal_busy(false);
    if ok {
        ui.set_vt_seal_text("".into());
        ui.set_vt_seal_open(false);
    }
}

/// Reset the vault's session state (a workspace closed or switched):
/// read texts live in UI memory only, and an open dialog must not
/// confirm into another republic.
pub(crate) fn reset_vault_session(ui: &AppWindow) {
    sync_rows(&ui.get_vault_unsealed(), Vec::new(), |m| {
        ui.set_vault_unsealed(m)
    });
    ui.set_vt_seal_open(false);
    ui.set_vt_seal_busy(false);
    ui.set_vt_seal_text("".into());
    ui.set_vt_seal_name("".into());
    ui.set_vt_grant_open(false);
    ui.set_vt_grant_secret("".into());
}

/// The dialogs' pure helpers: UTF-8 byte count (the cap is bytes, Slint
/// counts chars) and the replace check over the own deposits.
pub(crate) fn wire_local(ui: &AppWindow) {
    ui.set_vault_text_max(i32::try_from(VAULT_PAYLOAD_MAX).unwrap_or(i32::MAX));
    ui.on_vt_text_bytes(|text| i32::try_from(text.len()).unwrap_or(i32::MAX));
    ui.on_vt_label_ok(|s| label_ok(&s));
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
            let Some(cmd) = seal_command(&ui) else { return };
            seal_issued(&ui);
            let cx = cx.clone();
            cx.rt.clone().spawn(async move {
                let outcome = cx.wallet.execute(cmd).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = cx.weak.upgrade() else { return };
                    if let Err(e) = &outcome {
                        ui.invoke_show_toast_error(error_toast(&ui, e));
                    }
                    seal_settled(&ui, outcome.is_ok());
                });
            });
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
