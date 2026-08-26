// SPDX-License-Identifier: GPL-3.0-or-later
//! Founding / join / recovery callbacks: the wizard previews (folder,
//! invite, link classification), the founder's relay pick, the phrase
//! gates, and every human decision of the rituals (start, propose, ratify,
//! confirm the backup, cancel, finish) - tools on both surfaces, co-equal
//! with MCP.

use molt_core::Command;
use slint::{ComponentHandle, Model};

use crate::i18n::error_toast;
use crate::{AppWindow, Ctx, InvitePreview, LinkPreview, RelayPick};

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
        let weak = ui.as_weak();
        // the FULL parser, not the preview one: since the neutral link shape
        // (2026-08-08) the preview data rides inside the handover segment,
        // and only a JOINABLE link should ever preview as valid
        ui.on_parse_invite(move |s| match molt_engine::FoundingInvite::parse(&s).map(|i| i.info) {
            Ok(i) => {
                // how many of the republic's relays this node does not have.
                // The invite carries them, so a refused joiner never has to
                // copy them out of a chat message by hand.
                let missing = weak
                    .upgrade()
                    .map(|ui| invite_relays_missing(&ui, &s))
                    .unwrap_or(0);
                InvitePreview {
                    valid: true,
                    republic: i.republic.as_str().into(),
                    rule: format!("{}-of-{}", i.threshold, i.members).into(),
                    inviter: i.inviter.as_str().into(),
                    missing_relays: missing,
                }
            }
            Err(_) => InvitePreview::default(),
        });
        // the Restore wizard's one link field: which of the two flows is
        // this link for? Pure, like parse_invite, so the panel re-reads it
        // on every keystroke without any state to keep in sync. The relay
        // deviation rides along: relays do not federate, so a link whose
        // pool this node does not share is a hard blocker worth showing
        // BEFORE the run can fail on it.
        let weak_cl = ui.as_weak();
        ui.on_classify_link(move |s| {
            let missing = |relays: &[String]| -> (i32, slint::SharedString) {
                let Some(ui) = weak_cl.upgrade() else { return (0, "".into()) };
                let have: Vec<String> =
                    ui.get_relay_rows().iter().map(|r| r.url.to_string()).collect();
                let miss: Vec<&String> =
                    relays.iter().filter(|u| !have.contains(u)).collect();
                (
                    i32::try_from(miss.len()).unwrap_or(0),
                    miss.first().map(|u| u.as_str()).unwrap_or("").into(),
                )
            };
            match link_kind(&s) {
                LinkKind::Invite { republic, inviter } => {
                    let (n, first) = molt_engine::FoundingInvite::parse(s.trim())
                        .map(|inv| missing(&inv.handover.relays))
                        .unwrap_or((0, "".into()));
                    LinkPreview {
                        kind: 1,
                        republic: republic.into(),
                        who: inviter.into(),
                        missing: n,
                        missing_first: first,
                    }
                }
                LinkKind::Recovery { republic, member } => {
                    let (n, first) = molt_engine::RecoveryInvite::parse(s.trim())
                        .and_then(|inv| inv.handover.map(|h| missing(&h.relays)))
                        .unwrap_or((0, "".into()));
                    LinkPreview {
                        kind: 2,
                        republic: republic.into(),
                        who: member.into(),
                        missing: n,
                        missing_first: first,
                    }
                }
                LinkKind::Unrecognized => LinkPreview::default(),
            }
        });
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
            // the wizard's checkbox selection; the engine canonicalizes
            let features = cx
                .weak
                .upgrade()
                .map(|ui| {
                    // quests/vault/wallet have no wizard checkbox (locked
                    // off, not built) — no property to read until they ship
                    [(ui.get_cw_feat_memory(), "memory")]
                    .into_iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, key)| key.to_string())
                    .collect()
                })
                .unwrap_or_default();
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
            let cmd = Command::JoinStart {
                invite: invite.to_string(),
                member: member.to_string(),
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
            cx.issue(
                Command::RecoverStart {
                    link: link.to_string(),
                    phrase: phrase.to_string(),
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

/// The invite's relays this node does not hold yet.
pub(crate) fn invite_relays_missing(ui: &AppWindow, link: &str) -> i32 {
    let Ok(inv) = molt_engine::FoundingInvite::parse(link) else {
        return 0;
    };
    let have: Vec<String> = ui.get_relay_rows().iter().map(|r| r.url.to_string()).collect();
    i32::try_from(
        inv.handover
            .relays
            .iter()
            .filter(|u| !have.contains(u))
            .count(),
    )
    .unwrap_or(0)
}
