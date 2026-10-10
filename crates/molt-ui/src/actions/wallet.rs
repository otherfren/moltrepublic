// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse's callbacks (wallet plan §11): set up, consent, try again,
//! the node, the loss acknowledgement - the same seat tools an MCP agent
//! drives.

use molt_core::relay::RelayKind;
use molt_core::Command;
use slint::ComponentHandle;

use crate::app::Ctx;
use crate::{AppWindow, Purse, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    let p = ui.global::<Purse>();
    {
        let cx = ctx.clone();
        p.on_set_up(move || cx.issue(Command::WalletInit));
    }
    {
        let cx = ctx.clone();
        p.on_consent(move |accept| cx.issue(Command::WalletConsent { accept }));
    }
    {
        let cx = ctx.clone();
        p.on_retry(move || cx.issue(Command::WalletRetry));
    }
    {
        let cx = ctx.clone();
        p.on_acknowledge_loss(move || cx.issue(Command::WalletAcknowledgeLoss));
    }
    {
        let weak = ui.as_weak();
        p.on_copy(move |text| {
            let Some(ui) = weak.upgrade() else { return };
            ui.invoke_copy_text(text);
            ui.invoke_show_toast(ui.global::<Strings>().get_toast_copied());
        });
    }
    {
        let cx = ctx.clone();
        p.on_use_node(move |url| {
            let Some(ui) = cx.weak.upgrade() else { return };
            let p = ui.global::<Purse>();
            match crate::wallet::node_choice(ui.get_lang_index(), &url) {
                Err(line) => p.set_node_error(line.into()),
                Ok((url, RelayKind::Onion)) => use_node(&cx, url, false),
                Ok((url, kind)) => {
                    p.set_node_error("".into());
                    p.set_confirm_url(url.into());
                    p.set_confirm_kind(if kind == RelayKind::Local { 2 } else { 1 });
                }
            }
        });
    }
    {
        let cx = ctx.clone();
        ui.global::<Purse>().on_node_confirmed(move |url| use_node(&cx, url.to_string(), true));
    }
}

/// Store the node, confirmed; a non-onion one also switches non-onion
/// dialing on, as a confirmed relay does.
fn use_node(cx: &Ctx, url: String, outside_tor: bool) {
    let w = cx.wallet.clone();
    let weak = cx.weak.clone();
    cx.rt.spawn(async move {
        let patch = serde_json::json!({ "wallet_daemon_url": url, "wallet_daemon_confirmed": true });
        let mut res = w.execute(Command::PatchSettings { patch }).await;
        if res.is_ok() && outside_tor {
            res = w.execute(Command::RelayClearnetSession { unlock: true }).await;
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let p = ui.global::<Purse>();
            match res {
                Ok(_) => {
                    p.set_node_draft("".into());
                    p.set_node_error("".into());
                }
                Err(e) => p.set_node_error(crate::i18n::localize_error(ui.get_lang_index(), &e).into()),
            }
        });
    });
}
