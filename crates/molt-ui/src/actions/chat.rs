// SPDX-License-Identifier: GPL-3.0-or-later
//! Chat callbacks: compose, the channel selection (UI-local, engine-side
//! filter), the vote jump, delete / react, file share / download / remove,
//! and the directed poke.

use std::collections::HashMap;

use molt_core::{ChannelRef, Command, MessageId};
use slint::ComponentHandle;

use crate::channels::{
    channel_display_label, channel_key, parse_channel_key, selected_channel_closed,
    selected_channel_org, vote_jump_command,
};
use crate::i18n::error_toast;
use crate::app::Ctx;
use crate::{AppWindow, Poke, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    {
        let cx = ctx.clone();
        ui.on_send_chat(move |body, quote| {
            let body = body.trim().to_string();
            if body.is_empty() {
                return;
            }
            // "" = no quote; a legacy row without an id can't be quoted
            let quote = quote.parse::<MessageId>().ok();
            // compose files into the channel this window has selected
            let channel = cx.chat_ui
                .lock()
                .ok()
                .map(|s| s.selected.clone())
                .unwrap_or_default();
            cx.issue(Command::Chat { body, quote, channel });
        });
    }

    {
        // Channel selection (chat bus). UI-LOCAL state, not a session
        // command: the filter itself is engine-side (`ReadState{channel}`),
        // so co-equality holds — an MCP agent passes its own filter and
        // neither operator can hijack the other's view. The canonical key
        // is echoed back into `selected-channel` (single writer: Rust).
        let cx = ctx.clone();
        ui.on_select_channel(move |key| {
            let Some(ch) = parse_channel_key(&key) else {
                return;
            };
            // topics normalize on selection exactly as on send (trim, cap);
            // a rejected name is told, not silently swallowed
            let ch = match ch.normalized() {
                Ok(ch) => ch,
                Err(e) => {
                    if let Some(ui) = cx.weak.upgrade() {
                        // a normalization refusal is a plain String — wrap
                        // it as the payload error it is, so it localizes
                        ui.invoke_show_toast_error(error_toast(
                            &ui,
                            &molt_core::MoltError::BadPayload(e),
                        ));
                    }
                    return;
                }
            };
            if let Some(ui) = cx.weak.upgrade() {
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
                let (closed, org) = cx.chat_ui
                    .lock()
                    .map(|st| {
                        (
                            selected_channel_closed(&ch, &[], &st.proposals),
                            selected_channel_org(&ch, &st.proposals),
                        )
                    })
                    .unwrap_or((false, false));
                ui.set_selected_channel_closed(closed);
                // instant, like `closed`: the nav must not collapse the
                // section the click came from while the push is in flight
                ui.set_selected_channel_org(org);
            }
            if let Ok(mut st) = cx.chat_ui.lock() {
                // bumps the push generation: every in-flight push read
                // for the previous selection is stale from this moment
                st.select(ch);
            }
            // re-read through the engine filter (the point of the bus)
            cx.refresh_surfaces();
        });
    }

    {
        // "back to the vote" from a patch channel's banner: the selected
        // channel names the proposal, the proposal cache names its hosting
        // surface — the jump reuses the sidebar's own SelectView /
        // SelectSurface commands (no new engine verb).
        let cx = ctx.clone();
        ui.on_jump_to_vote(move || {
            let Some(cmd) = cx.chat_ui
                .lock()
                .ok()
                .and_then(|st| vote_jump_command(&st.selected, &st.proposals))
            else {
                return;
            };
            cx.issue(cmd);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_delete_chat(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            cx.issue(Command::DeleteChat { id });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_share_pick(move || {
            let w = cx.wallet.clone();
            let weak = cx.weak.clone();
            // a share files into the channel this window has selected —
            // captured at click time (the view the sharer was looking at),
            // same source as compose (concept Q8)
            let channel = cx.chat_ui
                .lock()
                .ok()
                .map(|s| s.selected.clone())
                .unwrap_or_default();
            // the native picker runs async (XDG portal) off the UI thread;
            // the engine derives the metadata + real sha256 from this path
            // and posts the share when hashing completes
            cx.rt.spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new().pick_file().await else {
                    return; // cancelled
                };
                let cmd = Command::ShareFile {
                    path: file.path().display().to_string(),
                    channel,
                };
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_download_file(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            let w = cx.wallet.clone();
            let weak = cx.weak.clone();
            // save-dialog per download (product decision): the user picks
            // the destination, then the engine fetches peer-to-peer;
            // completion/failure surfaces via Event::FileTransfer
            cx.rt.spawn(async move {
                let Some(dest) = rfd::AsyncFileDialog::new().save_file().await else {
                    return; // cancelled
                };
                let cmd = Command::DownloadFile {
                    id,
                    dest: Some(dest.path().display().to_string()),
                };
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_remove_file(move |id| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            let msg = cx
                .weak
                .upgrade()
                .map(|ui| ui.global::<Strings>().get_toast_file_removed().to_string())
                .unwrap_or_default();
            cx.issue_then_toast(Command::RemoveFile { id }, msg);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_toggle_reaction(move |id, emoji| {
            let Ok(id) = id.parse::<MessageId>() else {
                return; // legacy row without an id — nothing to address
            };
            cx.issue(
                Command::ReactChat {
                    id,
                    emoji: emoji.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        // right-click on a member: the directed nudge (co-equal MCP tool: poke).
        // One door for all nine name sites — the Poke global (theme.slint).
        ui.global::<Poke>().on_go(move |member| {
            cx.issue(
                Command::Poke {
                    member: member.to_string(),
                },
            );
        });
    }
}
