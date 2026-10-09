// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban pane's wiring (`docs/kanban/kanban_workflows.md` §8): the
//! UI-local state (filters, the basket, the form, the drill-in, the
//! notification batch) lives here on the UI thread; every push re-renders
//! the `Kanban` global from it and the engine's board.

use std::cell::RefCell;
use std::collections::BTreeSet;

use molt_core::{Command, Reply, Surface};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app::Ctx;
use crate::i18n::{error_toast, Lexicon};
use crate::kanban::{
    auto_summary, blocked_candidates, board, declined, detail, drop_target, filter_chips,
    form_act, mine, needs_note, review, Basket, Form, KanbanFeed, Notices,
};
use crate::models::{sync_rows, sync_strings};
use crate::surfaces::SurfacesBundle;
use crate::{AppWindow, Kanban, KbLine, KbPick, Strings};

/// Everything the pane keeps between pushes.
#[derive(Default)]
pub(crate) struct KanbanUi {
    pub(crate) feed: Option<KanbanFeed>,
    pub(crate) workspace: String,
    pub(crate) lang: i32,
    pub(crate) filters: BTreeSet<String>,
    pub(crate) basket: Basket,
    pub(crate) form: Form,
    pub(crate) evidence: Vec<String>,
    pub(crate) notices: Notices,
    /// The basket acts a Propose is sending (`None` = nothing in flight).
    pub(crate) in_flight: Option<usize>,
}

thread_local! {
    static KB: RefCell<KanbanUi> = RefCell::new(KanbanUi::default());
    // the engine handles, for the draft load a workspace switch needs
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
    // the APPLIED `wake_on`, never the settings draft
    static WAKE_ON: RefCell<Vec<String>> = RefCell::new(
        molt_core::kanban_wake::WAKE_REASONS.iter().map(|r| (*r).to_string()).collect(),
    );
}

/// The applied `wake_on` (the mirror, on every settings push).
pub(crate) fn set_wake_on(on: Vec<String>) {
    WAKE_ON.with_borrow_mut(|w| *w = on);
}

/// Run `f` on the pane's state.
pub(crate) fn with<R>(f: impl FnOnce(&mut KanbanUi) -> R) -> R {
    KB.with_borrow_mut(f)
}

fn lex(lang: i32) -> Lexicon {
    if lang == 1 {
        Lexicon::de()
    } else {
        Lexicon::en()
    }
}

/// The mirror's entry: adopt the push (a workspace switch resets the local
/// state and takes the stored basket), then render.
pub(crate) fn apply(ui: &AppWindow, b: &SurfacesBundle) {
    with(|st| {
        if st.workspace != b.workspace {
            *st = KanbanUi { workspace: b.workspace.clone(), ..KanbanUi::default() };
            ui.global::<Kanban>().set_sel("".into());
            ui.global::<Kanban>().set_form_open(false);
            ui.global::<Kanban>().set_basket_summary("".into());
            if !b.workspace.is_empty() {
                load_draft(b.workspace.clone());
            }
        }
        st.lang = b.lang;
        st.feed = b.kanban.clone();
    });
    note_starts(ui);
    render(ui);
}

/// Adopt the stored basket of `workspace` (still open, nothing staged yet).
fn load_draft(workspace: String) {
    let Some(cx) = CTX.with_borrow(Clone::clone) else { return };
    let w = cx.wallet.clone();
    let weak = cx.weak.clone();
    cx.rt.spawn(async move {
        let Ok(Reply::KanbanDraft { draft }) = w.execute(Command::KanbanDraftLoad).await else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                adopt_draft(&ui, &workspace, &draft);
            }
        });
    });
}

/// Take a stored basket for `workspace` unless something is staged already.
pub(crate) fn adopt_draft(ui: &AppWindow, workspace: &str, draft: &str) {
    let summary = with(|st| {
        (st.workspace == workspace && st.basket.acts.is_empty()).then(|| {
            st.basket = Basket::from_draft(draft);
            st.basket.summary.clone()
        })
    });
    if let Some(summary) = summary {
        ui.global::<Kanban>().set_basket_summary(summary.into());
        render(ui);
    }
}

/// Re-render the `Kanban` global from the state.
pub(crate) fn render(ui: &AppWindow) {
    let k = ui.global::<Kanban>();
    with(|st| {
        let l = lex(st.lang);
        let Some(feed) = st.feed.as_ref() else {
            clear(&k);
            return;
        };
        let b = board(&l, feed, &st.filters, &st.basket);
        let [todo, wip, done] = b.cols;
        sync_rows(&k.get_todo(), todo, |m| k.set_todo(m));
        sync_rows(&k.get_wip(), wip, |m| k.set_wip(m));
        sync_rows(&k.get_done(), done, |m| k.set_done(m));
        sync_rows(&k.get_legend(), b.legend, |m| k.set_legend(m));
        sync_rows(&k.get_filters(), filter_chips(&l, &st.filters), |m| k.set_filters(m));

        let sel = k.get_sel().to_string();
        match (!sel.is_empty()).then(|| detail(&l, feed, &st.basket, &sel)).flatten() {
            Some(d) => {
                if st.evidence.len() != d.checks.len() {
                    st.evidence = vec![String::new(); d.checks.len()];
                    sync_strings(&k.get_evidence(), &st.evidence, |m| k.set_evidence(m));
                }
                k.set_detail(d.head);
                sync_rows(&k.get_blocked_by(), d.blocked_by, |m| k.set_blocked_by(m));
                sync_rows(&k.get_prereq_for(), d.prereq_for, |m| k.set_prereq_for(m));
                sync_rows(&k.get_prereqs(), d.prereqs, |m| k.set_prereqs(m));
                sync_rows(&k.get_checks(), d.checks, |m| k.set_checks(m));
                sync_strings(&k.get_scope(), &d.scope, |m| k.set_scope(m));
                sync_rows(&k.get_moves(), d.moves, |m| k.set_moves(m));
            }
            None if !sel.is_empty() => k.set_sel("".into()),
            None => {}
        }

        let r = review(&l, feed, &st.basket);
        let auto = auto_summary(&r.acts);
        k.set_basket_count(i32::try_from(st.basket.acts.len()).unwrap_or(i32::MAX));
        k.set_basket_auto(auto.into());
        k.set_basket_tip(r.advisories.join("\n").into());
        let rows: Vec<KbLine> = r
            .rows
            .iter()
            .enumerate()
            .map(|(i, text)| KbLine {
                key: i.to_string().into(),
                kind: "act".into(),
                glyph: "🧺".into(),
                text: text.as_str().into(),
                sub: "".into(),
                bad: false,
            })
            .collect();
        sync_rows(&k.get_basket_rows(), rows, |m| k.set_basket_rows(m));

        let (actions, rest) = mine(&l, feed);
        k.set_action_count(i32::try_from(actions.len()).unwrap_or(i32::MAX));
        sync_rows(&k.get_actions(), actions, |m| k.set_actions(m));
        sync_rows(&k.get_mine(), rest, |m| k.set_mine(m));
        sync_rows(&k.get_declined(), declined(feed), |m| k.set_declined(m));

        if k.get_form_open() {
            render_form_pickers(&k, st);
        }
    });
}

/// No board: nothing of the previous workspace may stay on screen.
fn clear(k: &Kanban<'_>) {
    sync_rows(&k.get_todo(), Vec::new(), |m| k.set_todo(m));
    sync_rows(&k.get_wip(), Vec::new(), |m| k.set_wip(m));
    sync_rows(&k.get_done(), Vec::new(), |m| k.set_done(m));
    sync_rows(&k.get_legend(), Vec::new(), |m| k.set_legend(m));
    sync_rows(&k.get_basket_rows(), Vec::new(), |m| k.set_basket_rows(m));
    sync_rows(&k.get_actions(), Vec::new(), |m| k.set_actions(m));
    sync_rows(&k.get_mine(), Vec::new(), |m| k.set_mine(m));
    sync_rows(&k.get_declined(), Vec::new(), |m| k.set_declined(m));
    k.set_basket_count(0);
    k.set_action_count(0);
    k.set_basket_tip("".into());
    k.set_sel("".into());
    k.set_form_open(false);
}

fn render_form_pickers(k: &Kanban<'_>, st: &KanbanUi) {
    let Some(feed) = st.feed.as_ref() else { return };
    let seats: Vec<KbPick> = feed
        .seats
        .iter()
        .map(|s| KbPick { key: s.as_str().into(), label: s.as_str().into(), on: st.form.assignees.contains(s) })
        .collect();
    sync_rows(&k.get_f_assignees(), seats, |m| k.set_f_assignees(m));
    let picks = blocked_candidates(feed, &k.get_f_blocked_filter(), &st.form.blocked);
    sync_rows(&k.get_f_blocked(), picks, |m| k.set_f_blocked(m));
}

/// New `task_start` actions: one notification each (§8).
fn note_starts(ui: &AppWindow) {
    let wake_on = wake_on();
    let armed = with(|st| {
        let actions = st.feed.as_ref().map(|f| f.actions.clone()).unwrap_or_default();
        st.notices.note_starts(&actions, &wake_on)
    });
    if armed {
        arm_notice_timer(ui);
    }
}

/// The applied `wake_on` (toasts follow the wake's switches, §8).
fn wake_on() -> Vec<String> {
    WAKE_ON.with_borrow(Clone::clone)
}

/// A trigger from the event stream (UI thread): `poked` carries the poker.
pub(crate) fn notice(ui: &AppWindow, reason: &'static str, by: &str) {
    let wake_on = wake_on();
    let armed = with(|st| {
        if reason == "poked" {
            st.notices.note_poke(by, &wake_on)
        } else {
            st.notices.note(reason, &wake_on)
        }
    });
    if armed {
        arm_notice_timer(ui);
    }
}

thread_local! {
    static NOTICE_TIMER: slint::Timer = slint::Timer::default();
}

/// A burst coalesces into one toast after a short quiet.
fn arm_notice_timer(ui: &AppWindow) {
    let weak = ui.as_weak();
    NOTICE_TIMER.with(|t| {
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(1500), move || {
            if let Some(ui) = weak.upgrade() {
                flush_notices(&ui);
            }
        });
    });
}

/// Show the batched notification now.
pub(crate) fn flush_notices(ui: &AppWindow) {
    let poked = ui.global::<Strings>().get_toast_poked().to_string();
    if let Some(msg) = with(|st| {
        let l = lex(st.lang);
        st.notices.take(&l, &poked)
    }) {
        ui.invoke_show_toast(msg.into());
    }
}

fn save_basket(ctx: &Ctx) {
    let draft = with(|st| st.basket.to_draft());
    let w = ctx.wallet.clone();
    ctx.rt.spawn(async move {
        let _ = w.execute(Command::KanbanDraftSave { draft }).await;
    });
}

fn toast_error(ui: &AppWindow, msg: &str) {
    ui.invoke_show_toast_error(msg.into());
}

/// Stage the move of `id` to `to` (§8: a drop or a move button stages).
pub(crate) fn stage(ui: &AppWindow, id: &str, to: &str) -> bool {
    let k = ui.global::<Kanban>();
    let note = k.get_note().to_string();
    let l = with(|st| lex(st.lang));
    if needs_note(to) && note.trim().is_empty() {
        toast_error(ui, l.kb_note_needed);
        return false;
    }
    with(|st| {
        let evidence = std::mem::take(&mut st.evidence);
        st.basket.stage_state(id, to, &note, &evidence);
    });
    k.set_note("".into());
    sync_strings(&k.get_evidence(), &[], |m| k.set_evidence(m));
    render(ui);
    true
}

/// A card dropped on column `col`: stage the move, or say why not.
pub(crate) fn drop_card(ui: &AppWindow, id: &str, col: usize) -> bool {
    let (target, l) = with(|st| (st.feed.as_ref().map(|f| drop_target(f, id, col)), lex(st.lang)));
    match target.flatten() {
        Some(to) => {
            with(|st| st.basket.stage_state(id, to, "", &[]));
            render(ui);
            true
        }
        None => {
            let same = with(|st| {
                st.feed.as_ref().is_some_and(|f| {
                    f.snap
                        .board
                        .as_ref()
                        .and_then(|b| b.get("tasks"))
                        .and_then(|t| t.get(id))
                        .and_then(|t| t.get("state"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| crate::kanban::column_of(s) == col)
                })
            });
            if !same {
                toast_error(ui, l.kb_move_illegal);
            }
            false
        }
    }
}

/// The form's fields into a [`Form`].
fn read_form(k: &Kanban<'_>, st: &KanbanUi) -> Form {
    Form {
        title: k.get_f_title().to_string(),
        kind: k.get_f_type().to_string(),
        size: k.get_f_size().to_string(),
        assignees: st.form.assignees.clone(),
        blocked: st.form.blocked.clone(),
        due: k.get_f_due().to_string(),
        after: k.get_f_after().to_string(),
        mode: k.get_f_mode(),
        start: k.get_f_start().to_string(),
        end: k.get_f_end().to_string(),
        freq: k.get_f_freq().to_string(),
        interval: k.get_f_interval().to_string(),
        until: k.get_f_until().to_string(),
        count: k.get_f_count().to_string(),
        desc: k.get_f_desc().to_string(),
        acceptance: st.form.acceptance.clone(),
        scope: st.form.scope.clone(),
    }
}

/// Open an empty form, the own seat assigned.
pub(crate) fn form_new(ui: &AppWindow) {
    let k = ui.global::<Kanban>();
    for set in [
        Kanban::set_f_title,
        Kanban::set_f_type,
        Kanban::set_f_size,
        Kanban::set_f_due,
        Kanban::set_f_after,
        Kanban::set_f_start,
        Kanban::set_f_end,
        Kanban::set_f_interval,
        Kanban::set_f_until,
        Kanban::set_f_count,
        Kanban::set_f_desc,
        Kanban::set_f_error,
        Kanban::set_f_blocked_filter,
    ] {
        set(&k, "".into());
    }
    k.set_f_mode(0);
    k.set_f_freq("weekly".into());
    with(|st| {
        let me = st.feed.as_ref().map(|f| f.me.clone()).unwrap_or_default();
        st.form = Form { assignees: vec![me], acceptance: vec![String::new()], ..Form::default() };
    });
    sync_lists(&k);
    k.set_form_open(true);
    render(ui);
}

fn sync_lists(k: &Kanban<'_>) {
    let (acc, scope) = with(|st| (st.form.acceptance.clone(), st.form.scope.clone()));
    k.set_f_acceptance(ModelRc::new(VecModel::from(acc.into_iter().map(Into::into).collect::<Vec<slint::SharedString>>())));
    k.set_f_scope(ModelRc::new(VecModel::from(scope.into_iter().map(Into::into).collect::<Vec<slint::SharedString>>())));
}

/// The form into the basket; `false` with the reason shown on the form.
pub(crate) fn form_submit(ui: &AppWindow) -> bool {
    let k = ui.global::<Kanban>();
    let form = with(|st| read_form(&k, st));
    match form_act(&form) {
        Ok(act) => {
            with(|st| st.basket.add(act));
            k.set_f_error("".into());
            k.set_form_open(false);
            render(ui);
            true
        }
        Err(e) => {
            k.set_f_error(e.into());
            false
        }
    }
}

/// Propose the basket as one changeset; it empties once the engine took it.
fn propose(ui: &AppWindow, ctx: &Ctx) {
    let summary = ui.global::<Kanban>().get_basket_summary().to_string();
    let auto = ui.global::<Kanban>().get_basket_auto().to_string();
    let Some((payload, sent)) = with(|st| {
        if st.in_flight.is_some() || st.basket.acts.is_empty() {
            return None;
        }
        st.in_flight = Some(st.basket.acts.len());
        st.basket.summary = summary;
        let rev = st
            .feed
            .as_ref()
            .and_then(|f| f.snap.board.as_ref())
            .and_then(|b| b.get("rev"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some((st.basket.payload(rev, &auto), st.basket.acts.clone()))
    }) else {
        return;
    };
    let w = ctx.wallet.clone();
    let weak = ctx.weak.clone();
    let cx = ctx.clone();
    ctx.rt.spawn(async move {
        let outcome = w.execute(Command::Propose { surface: Surface::Quests, payload }).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            with(|st| st.in_flight = None);
            match outcome {
                Ok(_) => {
                    // only what was sent leaves: a move staged meanwhile stays
                    with(|st| {
                        if st.basket.acts.starts_with(&sent) {
                            st.basket.acts.drain(..sent.len());
                        }
                        st.basket.summary.clear();
                    });
                    ui.global::<Kanban>().set_basket_summary("".into());
                    save_basket(&cx);
                    render(&ui);
                    ui.invoke_show_toast(ui.global::<Strings>().get_toast_proposed());
                }
                Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
            }
        });
    });
}

/// Wire the `Kanban` global's callbacks.
pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    CTX.with_borrow_mut(|c| *c = Some(ctx.clone()));
    let k = ui.global::<Kanban>();
    let weak = ui.as_weak();
    let on_ui = move |f: &dyn Fn(&AppWindow)| {
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    };
    {
        let on_ui = on_ui.clone();
        k.on_filter_toggle(move |key| {
            on_ui(&|ui| {
                with(|st| {
                    if !st.filters.remove(key.as_str()) {
                        st.filters.insert(key.to_string());
                    }
                });
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_type_toggle(move |key| {
            on_ui(&|ui| {
                let f = format!("type:{key}");
                with(|st| {
                    if !st.filters.remove(&f) {
                        st.filters.insert(f.clone());
                    }
                });
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_pick(move |id| {
            on_ui(&|ui| {
                with(|st| st.evidence.clear());
                let k = ui.global::<Kanban>();
                k.set_note("".into());
                sync_strings(&k.get_evidence(), &[], |m| k.set_evidence(m));
                k.set_sel(id.clone());
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_move(move |id, col| {
            on_ui(&|ui| {
                if drop_card(ui, id.as_str(), usize::try_from(col).unwrap_or(0)) {
                    save_basket(&cx);
                }
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_stage(move |to| {
            on_ui(&|ui| {
                let id = ui.global::<Kanban>().get_sel().to_string();
                if !id.is_empty() && stage(ui, &id, to.as_str()) {
                    save_basket(&cx);
                }
            });
        });
    }
    k.on_evidence_set(move |i, text| {
        with(|st| {
            if let Some(e) = usize::try_from(i).ok().and_then(|i| st.evidence.get_mut(i)) {
                *e = text.to_string();
            }
        });
    });
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_basket_propose(move || on_ui(&|ui| propose(ui, &cx)));
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_basket_discard(move || {
            on_ui(&|ui| {
                with(|st| st.basket = Basket::default());
                ui.global::<Kanban>().set_basket_summary("".into());
                save_basket(&cx);
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_basket_remove(move |i| {
            on_ui(&|ui| {
                with(|st| {
                    if let Ok(i) = usize::try_from(i) {
                        if i < st.basket.acts.len() {
                            st.basket.acts.remove(i);
                        }
                    }
                });
                save_basket(&cx);
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_rescue(move |id| {
            on_ui(&|ui| {
                let n = with(|st| {
                    let payload = st
                        .feed
                        .as_ref()
                        .and_then(|f| f.snap.declined.iter().find(|p| u64::try_from(id).is_ok_and(|id| p.id.0 == id)))
                        .map(|p| p.payload.clone());
                    payload.map(|p| st.basket.rescue(&p))
                });
                if let Some(n) = n {
                    let l = with(|st| lex(st.lang));
                    let summary = with(|st| st.basket.summary.clone());
                    ui.global::<Kanban>().set_basket_summary(summary.into());
                    ui.invoke_show_toast(format!("{n} {}", l.kb_rescued).into());
                    save_basket(&cx);
                    render(ui);
                }
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_form_new(move || on_ui(&|ui| form_new(ui)));
    }
    {
        let on_ui = on_ui.clone();
        k.on_form_cancel(move || on_ui(&|ui| ui.global::<Kanban>().set_form_open(false)));
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_form_submit(move || {
            on_ui(&|ui| {
                if form_submit(ui) {
                    save_basket(&cx);
                }
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_assignee_toggle(move |seat| {
            on_ui(&|ui| {
                with(|st| toggle(&mut st.form.assignees, seat.as_str()));
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_blocked_toggle(move |id| {
            on_ui(&|ui| {
                with(|st| toggle(&mut st.form.blocked, id.as_str()));
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_blocked_filter_edited(move |_| on_ui(&|ui| render(ui)));
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_list_add(move |which| {
            on_ui(&|ui| {
                with(|st| list_of(st, which).push(String::new()));
                sync_lists(&ui.global::<Kanban>());
            });
        });
    }
    k.on_f_list_set(move |which, i, text| {
        with(|st| {
            if let Some(item) = usize::try_from(i).ok().and_then(|i| list_of(st, which).get_mut(i)) {
                *item = text.to_string();
            }
        });
    });
    {
        let on_ui = on_ui.clone();
        k.on_f_list_remove(move |which, i| {
            on_ui(&|ui| {
                with(|st| {
                    let list = list_of(st, which);
                    if let Ok(i) = usize::try_from(i) {
                        if i < list.len() {
                            list.remove(i);
                        }
                    }
                });
                sync_lists(&ui.global::<Kanban>());
            });
        });
    }
    wire_settings(ui, ctx);
    start_actions_timer(ctx);
}

thread_local! {
    static ACTIONS_TIMER: slint::Timer = slint::Timer::default();
}

/// An appointment starts on the clock, not on an event: re-read the list
/// once a minute so its notification is on time.
fn start_actions_timer(ctx: &Ctx) {
    let cx = ctx.clone();
    ACTIONS_TIMER.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(60), move || {
            if with(|st| st.feed.is_none()) {
                return;
            }
            let w = cx.wallet.clone();
            let weak = cx.weak.clone();
            cx.rt.spawn(async move {
                let Ok(Reply::Actions { actions }) = w.execute(Command::ReadActions).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    with(|st| {
                        if let Some(f) = st.feed.as_mut() {
                            f.actions = actions;
                        }
                    });
                    note_starts(&ui);
                    render(&ui);
                });
            });
        });
    });
}

fn toggle(list: &mut Vec<String>, key: &str) {
    if let Some(i) = list.iter().position(|k| k == key) {
        list.remove(i);
    } else {
        list.push(key.to_string());
    }
}

fn list_of(st: &mut KanbanUi, which: i32) -> &mut Vec<String> {
    if which == 0 {
        &mut st.form.acceptance
    } else {
        &mut st.form.scope
    }
}

/// The Wake group's two buttons and the skill text (§6.2).
fn wire_settings(ui: &AppWindow, ctx: &Ctx) {
    ui.set_skill_text(molt_mcp::WAKE_SKILL.into());
    {
        let cx = ctx.clone();
        ui.on_wake_test(move || cx.issue(Command::TestWake));
    }
    let weak = ui.as_weak();
    let rt = ctx.rt.clone();
    ui.on_skill_save(move || {
        let weak = weak.clone();
        rt.spawn_blocking(move || {
            let Some(dir) = rfd::FileDialog::new().pick_folder() else {
                return;
            };
            let outcome = std::fs::write(dir.join("SKILL.md"), molt_mcp::WAKE_SKILL);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                match outcome {
                    Ok(()) => {
                        let saved = ui.global::<Strings>().get_skill_saved();
                        ui.invoke_show_toast(format!("{saved} {}", dir.join("SKILL.md").display()).into());
                    }
                    Err(e) => ui.invoke_show_toast_error(format!("SKILL.md: {e}").into()),
                }
            });
        });
    });
}
