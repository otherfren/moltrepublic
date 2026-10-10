// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse's callbacks (wallet plan §11): set up, consent, try again,
//! the node, the loss acknowledgement - the same seat tools an MCP agent
//! drives.

use slint::ComponentHandle;

use crate::app::Ctx;
use crate::wallet::{node_commands, node_step, NodeStep, PurseAct};
use crate::{AppWindow, Purse, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    let p = ui.global::<Purse>();
    {
        let cx = ctx.clone();
        p.on_set_up(move || cx.issue(PurseAct::SetUp.command()));
    }
    {
        let cx = ctx.clone();
        p.on_consent(move |accept| cx.issue(PurseAct::Consent(accept).command()));
    }
    {
        let cx = ctx.clone();
        p.on_retry(move || cx.issue(PurseAct::Retry.command()));
    }
    {
        let cx = ctx.clone();
        p.on_acknowledge_loss(move || cx.issue(PurseAct::AcknowledgeLoss.command()));
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
            match node_step(ui.get_lang_index(), &url) {
                NodeStep::Bad(line) => p.set_node_error(line.into()),
                NodeStep::Take(url) => use_node(&cx, &url, false),
                NodeStep::Confirm(url, kind) => {
                    p.set_node_error("".into());
                    p.set_confirm_url(url.into());
                    p.set_confirm_kind(kind);
                }
            }
        });
    }
    {
        let cx = ctx.clone();
        ui.global::<Purse>().on_node_confirmed(move |url| use_node(&cx, &url, true));
    }
}

fn use_node(cx: &Ctx, url: &str, outside_tor: bool) {
    let w = cx.wallet.clone();
    let weak = cx.weak.clone();
    let cmds = node_commands(url, outside_tor);
    cx.rt.spawn(async move {
        let mut res = Ok(molt_core::Reply::Ack);
        for cmd in cmds {
            res = w.execute(cmd).await;
            if res.is_err() {
                break;
            }
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
