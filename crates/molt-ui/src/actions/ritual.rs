// SPDX-License-Identifier: GPL-3.0-or-later
//! Founding / join / recovery callbacks: the wizard previews (folder,
//! invite, link classification), the founder's relay pick, the phrase
//! gates, and every human decision of the rituals (start, propose, ratify,
//! confirm the backup, cancel, finish) - tools on both surfaces, co-equal
//! with MCP.

use molt_core::Command;
use slint::ComponentHandle;

use crate::i18n::error_toast;
use crate::app::Ctx;
use crate::{AppWindow, LinkPreview, LinkRelay, RelayPick, SeedRow, SeedWord, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    // The join preview: the same molt-core invite parser the engine's join
    // run uses, so the preview and the run can never disagree.
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
        // the Restore wizard's one link field: which of the two flows is
        // this link for? Pure, so the panel re-reads it
        // on every keystroke without any state to keep in sync. The link's
        // relays are the relay panel's business (`rw-link-changed`).
        ui.on_classify_link(move |s| {
            match link_kind(&s) {
                LinkKind::Invite { republic, inviter } => LinkPreview {
                    kind: 1,
                    republic: republic.into(),
                    who: inviter.into(),
                },
                LinkKind::Recovery { republic, member } => LinkPreview {
                    kind: 2,
                    republic: republic.into(),
                    who: member.into(),
                },
                LinkKind::Unrecognized => LinkPreview::default(),
            }
        });
    }

    {
        let cx = ctx.clone();
        ui.on_rw_link_changed(move |link| {
            let relays = link_relays(&link);
            let Ok(mut st) = cx.chat_ui.lock() else { return };
            st.set_link(link.trim(), relays);
            if let Some(ui) = cx.weak.upgrade() {
                push_link_rows(&ui, &st);
            }
        });
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
            cx.rt.spawn(async move {
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


/// What a `molt://…` link in the Restore wizard's one link field turns out
/// to be — the whole of the Join/Restore merge (`docs_archive/ui/welcome_rework.md`).
///
/// The two shapes are unambiguous by prefix and both already have a parser in
/// `molt-engine`; the panel just asks which one it is holding, because that
/// decides which FIELD is required and which existing command a click issues.
/// No new engine surface: `join_start` and `recover_start` stay exactly the
/// co-equal tools they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// A founding invite (`molt://invite/…`) — needs a NAME, mints its own
    /// recovery phrase, runs `Command::JoinStart`.
    Invite {
        /// The republic's display name, for the panel's confirmation line.
        republic: String,
        /// Who minted the link.
        inviter: String,
    },
    /// A recovery link (`molt://recover/…`) — names its own seat, needs the
    /// PHRASE, runs `Command::RecoverStart`.
    Recovery {
        /// The republic's display name.
        republic: String,
        /// The seat coming back; the name field shows this read-only.
        member: String,
    },
    /// Empty, malformed, or actionable-looking but missing its transport
    /// handover (a preview-only link nothing can be done with).
    Unrecognized,
}

/// Classify a pasted link. Empty input is [`LinkKind::Unrecognized`] like any
/// other unusable value — the panel simply stays unarmed rather than
/// complaining at someone who has not typed anything yet.
#[must_use]
pub fn link_kind(link: &str) -> LinkKind {
    let trimmed = link.trim();
    if trimmed.is_empty() {
        return LinkKind::Unrecognized;
    }
    // both parsers reject a link whose handover is missing or damaged, which
    // is what keeps a preview-only link from arming a flow that cannot run
    if let Ok(inv) = molt_engine::FoundingInvite::parse(trimmed) {
        return LinkKind::Invite {
            republic: inv.info.republic,
            inviter: inv.info.inviter,
        };
    }
    if let Some(rec) = molt_engine::RecoveryInvite::parse(trimmed) {
        return LinkKind::Recovery {
            republic: rec.republic,
            member: rec.member,
        };
    }
    LinkKind::Unrecognized
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
            let tone = if molt_storage::phrase_word_known(w) {
                1
            } else if typing && i == last && molt_storage::phrase_word_prefix(w) {
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

/// The relays a join or recovery link names; empty for anything else.
fn link_relays(link: &str) -> Vec<String> {
    let link = link.trim();
    if let Ok(inv) = molt_engine::FoundingInvite::parse(link) {
        return inv.handover.relays;
    }
    molt_engine::RecoveryInvite::parse(link)
        .and_then(|inv| inv.handover.map(|h| h.relays))
        .unwrap_or_default()
}

/// Push the (re)join relay panel's rows and pick count.
fn adopt_link_rows(cx: &Ctx, keep: impl Fn(&crate::surfaces::LinkRelayRow) -> bool) {
    let Ok(rows) = cx.chat_ui.lock().map(|st| st.link_rows()) else { return };
    let rows: Vec<_> = rows.into_iter().filter(|r| keep(r)).collect();
    crate::actions::relays::adopt_relays(cx, crate::surfaces::AdoptPlan::of(&rows));
}

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
