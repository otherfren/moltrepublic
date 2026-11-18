// SPDX-License-Identifier: GPL-3.0-or-later
//! Founding / join / recovery callbacks: the wizard previews (folder,
//! invite, link classification), the founder's relay pick, the phrase
//! gates, and every human decision of the rituals (start, propose, ratify,
//! confirm the backup, cancel, finish) - tools on both surfaces, co-equal
//! with MCP.

use std::sync::Mutex;

use molt_core::{Command, LinkView, MoltError, Reply};
use slint::ComponentHandle;

use crate::i18n::error_toast;
use crate::app::Ctx;
use crate::{AppWindow, LinkPreview, LinkRelay, RelayPick, SeedRow, SeedWord, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
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

    {
        let cx = ctx.clone();
        ui.on_rw_link_changed(move |link| link_changed(&cx, &link));
        let cx = ctx.clone();
        ui.on_rw_relay_toggle(move |idx| {
            let Ok(mut st) = cx.chat_ui.lock() else { return };
            let Some(url) = usize::try_from(idx).ok().and_then(|i| st.link_rows().get(i).map(|r| r.url.clone()))
            else {
                return;
            };
            st.toggle_link_relay(&url);
            if let Some(ui) = cx.weak.upgrade() {
                push_link_rows(&ui, &st);
            }
        });
        let cx = ctx.clone();
        ui.on_rw_relay_adopt(move |url| adopt_link_rows(&cx, |r| r.url == url.as_str()));
        let cx = ctx.clone();
        ui.on_rw_relay_adopt_all(move || adopt_link_rows(&cx, |_| true));
    }

    {
        let cx = ctx.clone();
        ui.on_cw_toggle_relay(move |idx| {
            let Ok(mut st) = cx.chat_ui.lock() else { return };
            let rows = st.create_pick_rows();
            let Some((url, _)) = rows.get(usize::try_from(idx).unwrap_or(0)) else {
                return;
            };
            st.toggle_create_relay(url.clone());
            let rows: Vec<RelayPick> = st
                .create_pick_rows()
                .into_iter()
                .map(|(url, picked)| RelayPick { url: url.into(), picked })
                .collect();
            if let Some(ui) = cx.weak.upgrade() {
                ui.set_cw_relay_picked(crate::mirror::picked_count(&rows));
                ui.set_cw_relay_picks(slint::ModelRc::new(slint::VecModel::from(rows)));
            }
        });
    }

    {
        // the phrase-backup gates (create step 4, join finish): the re-typed
        // phrase must MATCH, but whitespace runs and letter case never block
        // an honest re-type
        ui.on_seed_matches(|typed, expected| {
            let norm = |s: &str| {
                s.split_whitespace()
                    .map(str::to_lowercase)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            !typed.trim().is_empty() && norm(&typed) == norm(&expected)
        });
    }

    {
        // the rejoin phrase area's chips
        ui.on_seed_rows(|text| {
            let rows: Vec<SeedRow> = seed_word_rows(&text)
                .into_iter()
                .map(|row| SeedRow {
                    words: slint::ModelRc::new(slint::VecModel::from(
                        row.into_iter()
                            .map(|(n, word, tone)| SeedWord { n, word: word.into(), tone })
                            .collect::<Vec<_>>(),
                    )),
                })
                .collect();
            slint::ModelRc::new(slint::VecModel::from(rows))
        });
    }

    {
        let cx = ctx.clone();
        ui.on_create_start(move |name, member, threshold, members| {
            // the founder's pick: every dialable relay the wizard did not
            // deselect. Empty means "no explicit choice" to the engine, which
            // is exactly right when nothing was deselected.
            let picked = cx.chat_ui
                .lock()
                .ok()
                .map(|st| st.create_pick())
                .unwrap_or_default();
            cx.issue(
                Command::CreateStart {
                    name: name.to_string(),
                    member: member.to_string(),
                    threshold: u8::try_from(threshold).unwrap_or(0),
                    members: u8::try_from(members).unwrap_or(0),
                    relays: picked,
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_create_cancel(move || {
            cx.issue(Command::CreateCancel);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_create_finish(move || {
            cx.issue(Command::CreateFinish);
        });
    }

    {
        let cx = ctx.clone();
        // the ❻½ phrase-backup confirmation — founder or joiner, the
        // engine routes by the running ritual (a mismatch surfaces as an
        // honest error toast)
        ui.on_confirm_seed_backup(move |phrase| {
            cx.issue(
                Command::ConfirmSeedBackup {
                    phrase: phrase.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_create_propose(move |name, agenda| {
            let features = cx.weak.upgrade().map(|ui| charter_features(&ui)).unwrap_or_default();
            cx.issue(
                Command::CreatePropose {
                    name: name.to_string(),
                    agenda: agenda.to_string(),
                    features,
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_join_start(move |invite, member| {
            // not the plain issue(): a REFUSED start (bad link, no relay,
            // already running) must re-arm the optimistic jw-starting latch,
            // or the join button stays dead with nothing running. An accepted
            // start needs no reset here — the engine session flips jw-step
            // and the form is gone.
            let w = cx.wallet.clone();
            let weak = cx.weak.clone();
            let Some(relays) = cx.chat_ui.lock().ok().and_then(|st| st.link_pick(invite.trim())) else {
                if let Some(ui) = cx.weak.upgrade() {
                    ui.set_jw_starting(false);
                    ui.invoke_show_toast_error(ui.global::<Strings>().get_rw_rl_none());
                }
                return;
            };
            let cmd = Command::JoinStart {
                invite: invite.to_string(),
                member: member.to_string(),
                relays,
            };
            cx.spawn(async move {
                if let Err(e) = w.execute(cmd).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_jw_starting(false);
                            ui.invoke_show_toast_error(error_toast(&ui, &e));
                        }
                    });
                }
            });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_join_cancel(move || {
            // leaving the run re-arms the start latch: the form comes back
            // with a clickable button
            if let Some(ui) = cx.weak.upgrade() {
                ui.set_jw_starting(false);
            }
            cx.issue(Command::JoinCancel);
        });
    }

    // recovery (total-loss rejoin): the coordinator mints a link for an
    // anchored seat; the returning member rejoins from link + phrase. Both
    // are human decisions — tools on both surfaces, co-equal with MCP.
    {
        let cx = ctx.clone();
        ui.on_recover_invite(move |member| {
            cx.issue(
                Command::RecoverInviteStart {
                    member: member.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_recover_start(move |link, phrase| {
            let Some(relays) = cx.chat_ui.lock().ok().and_then(|st| st.link_pick(link.trim())) else {
                if let Some(ui) = cx.weak.upgrade() {
                    ui.invoke_show_toast_error(ui.global::<Strings>().get_rw_rl_none());
                }
                return;
            };
            cx.issue(
                Command::RecoverStart {
                    link: link.to_string(),
                    phrase: phrase.to_string(),
                    relays,
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_join_confirm_charter(move || {
            cx.issue(Command::JoinConfirmCharter);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_join_decline_charter(move || {
            cx.issue(Command::JoinDeclineCharter);
        });
    }

    {
        let cx = ctx.clone();
        ui.on_join_finish(move || {
            cx.issue(Command::JoinFinish);
        });
    }

    {
        let cx = ctx.clone();
        // closing the recovery-link dialog acknowledges the notice that
        // opened it — otherwise the one-shot link re-opens it on the next
        // fresh window (co-equal MCP tool: clear_notice)
        ui.on_clear_notice(move || {
            cx.issue(Command::ClearNotice);
        });
    }
}


/// The link field changed: the old verdict and its relays go NOW, before
/// the engine is asked - Continue arms on the verdict, and an answer
/// takes a round trip.
fn link_changed(cx: &Ctx, field: &str) {
    let link = field.trim().to_string();
    if let Some(ui) = cx.weak.upgrade() {
        // blank text is never read: it is judged here, as no link
        let blank = if link.is_empty() { field } else { "" };
        ui.set_rv_verdict(LinkPreview { link: blank.into(), ..LinkPreview::default() });
        let Ok(mut st) = cx.chat_ui.lock() else { return };
        st.set_link(&link, Vec::new());
        push_link_rows(&ui, &st);
    }
    if link.is_empty() {
        return;
    }
    let (engine, weak, chat_ui) = (cx.wallet.clone(), cx.weak.clone(), cx.chat_ui.clone());
    let sent = link.clone();
    cx.spawn(async move {
        let answer = read_link(&engine, link).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match answer {
                Ok(view) => land_link(&ui, &chat_ui, view),
                Err(e) => link_failed(&ui, &chat_ui, &sent, &e),
            }
        });
    });
}

/// The read for `link` failed: one toast per outage, and none for a link
/// the field no longer holds. The panel stays unjudged.
pub(crate) fn link_failed(ui: &AppWindow, chat_ui: &Mutex<crate::surfaces::ChatUiState>, link: &str, e: &MoltError) {
    if ui.get_rv_link().trim() != link {
        return;
    }
    let Ok(mut st) = chat_ui.lock() else { return };
    if !std::mem::replace(&mut st.link_read_failed, true) {
        ui.invoke_show_toast_error(error_toast(ui, e));
    }
}

/// The engine's verdict on `link` (`Command::ClassifyLink`): its own two
/// invite parsers, so the panel and the run cannot disagree.
pub(crate) async fn read_link(engine: &crate::engine::Engine, link: String) -> Result<LinkView, MoltError> {
    match engine.execute(Command::ClassifyLink { link }).await? {
        Reply::Link(view) => Ok(view),
        _ => Ok(LinkView::default()),
    }
}

/// Show a verdict in the link panel - only one for the link the field
/// still holds: a late answer must never arm join or recover.
pub(crate) fn land_link(ui: &AppWindow, chat_ui: &Mutex<crate::surfaces::ChatUiState>, view: LinkView) {
    let shown = ui.get_rv_link();
    if shown.trim() != view.link {
        return;
    }
    let Ok(mut st) = chat_ui.lock() else { return };
    st.link_read_failed = false;
    st.set_link(&view.link, view.relays);
    push_link_rows(ui, &st);
    ui.set_rv_verdict(LinkPreview {
        link: shown,
        kind: match view.kind {
            1 => 1,
            2 => 2,
            _ => 0,
        },
        republic: view.republic.into(),
        who: view.who.into(),
    });
}

/// Whether `word` is a BIP-39 English list word, exactly as the engine's
/// phrase parser reads it (no case folding). The list is the `bip39`
/// crate's, the one storage parses a phrase with.
fn phrase_word_known(word: &str) -> bool {
    bip39::Language::English.find_word(word).is_some()
}

/// Whether some BIP-39 English word starts with `prefix`.
fn phrase_word_prefix(prefix: &str) -> bool {
    !bip39::Language::English.words_by_prefix(prefix).is_empty()
}

/// The rejoin phrase area's chips: each typed word numbered and toned by the
/// BIP-39 list (1 = a list word, 2 = not one, 0 = the last word while it is
/// still a prefix of one), four to a row.
pub(crate) fn seed_word_rows(text: &str) -> Vec<Vec<(i32, String, i32)>> {
    let typing = !text.ends_with(char::is_whitespace);
    let words: Vec<&str> = text.split_whitespace().collect();
    let last = words.len().saturating_sub(1);
    let toned: Vec<(i32, String, i32)> = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let tone = if phrase_word_known(w) {
                1
            } else if typing && i == last && phrase_word_prefix(w) {
                0
            } else {
                2
            };
            (i32::try_from(i + 1).unwrap_or(i32::MAX), (*w).to_string(), tone)
        })
        .collect();
    toned.chunks(4).map(<[_]>::to_vec).collect()
}

/// The wizard's checkbox selection for `CreatePropose`; the engine
/// canonicalizes. The purse only where W1 lets the box be ticked.
pub(crate) fn charter_features(ui: &AppWindow) -> Vec<String> {
    let wallet = ui.get_cw_feat_wallet() && ui.get_cw_wallet_allowed();
    [
        (ui.get_cw_feat_memory(), "memory"),
        (ui.get_cw_feat_quests(), "quests"),
        (ui.get_cw_feat_vault() && ui.get_cw_vault_allowed(), "vault"),
        (wallet, "wallet"),
    ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, key)| key.to_string())
        .collect()
}

fn adopt_link_rows(cx: &Ctx, keep: impl Fn(&crate::surfaces::LinkRelayRow) -> bool) {
    let Ok(rows) = cx.chat_ui.lock().map(|st| st.link_rows()) else { return };
    let rows: Vec<_> = rows.into_iter().filter(|r| keep(r)).collect();
    crate::actions::relays::adopt_relays(cx, crate::surfaces::AdoptPlan::of(&rows));
}

/// Push the (re)join relay panel's rows and pick count.
pub(crate) fn push_link_rows(ui: &AppWindow, st: &crate::surfaces::ChatUiState) {
    let rows: Vec<LinkRelay> = st
        .link_rows()
        .into_iter()
        .map(|r| LinkRelay { url: r.url.into(), picked: r.picked, block: r.block, kind: r.kind })
        .collect();
    ui.set_rw_relay_picked(i32::try_from(rows.iter().filter(|r| r.picked).count()).unwrap_or(0));
    ui.set_rw_relay_blocked(i32::try_from(rows.iter().filter(|r| r.block != 0).count()).unwrap_or(0));
    crate::models::sync_rows(&ui.get_rw_relays(), rows, |m| ui.set_rw_relays(m));
}
