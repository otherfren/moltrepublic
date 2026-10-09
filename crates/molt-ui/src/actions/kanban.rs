// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban pane's wiring (`docs_archive/kanban/kanban_workflows.md` §8): the
//! UI-local state (filters, the basket, the form, the drill-in, the
//! notification batch) lives here on the UI thread; every push re-renders
//! the `Kanban` global from it and the engine's board.

use std::cell::RefCell;

use chrono::Datelike;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use molt_core::{Command, Event, Reply, Surface};
use serde_json::Value;
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app::Ctx;
use crate::i18n::{error_toast, Lexicon};
use crate::kanban::{
    auto_summary, blocked_candidates, board, declined, detail, drop_target, filter_rows,
    form_act, mine, needs_note, notice_reason, resolve_task, review, toggle_filter, Basket, Form,
    KanbanFeed, Notices,
};
use crate::kanban_views::{calendar, dependencies, drop_fields, plan, slot_range, utc_clock, CalSrc};
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
    /// A Propose is on its way to the engine.
    pub(crate) in_flight: bool,
    /// This workspace's stored basket was taken (once per workspace).
    pub(crate) draft_adopted: bool,
    /// `plan` rows folded over their prerequisites.
    pub(crate) plan_closed: BTreeSet<String>,
    /// `dependencies` paths flipped from their default.
    pub(crate) deps_toggled: BTreeSet<String>,
    /// The calendar page: its anchor day (`None` = today) and week mode.
    pub(crate) cal_anchor: Option<chrono::NaiveDate>,
    pub(crate) cal_week: bool,
    /// The rendered page's draggable blocks.
    pub(crate) cal_src: Vec<CalSrc>,
}

impl KanbanUi {
    /// Start a Propose: the payload against the fold the review checked,
    /// and the acts it sends. `None` while one is in flight or nothing is
    /// staged.
    pub(crate) fn begin_propose(&mut self, summary: String, auto: &str) -> Option<(Value, Vec<Value>)> {
        if self.in_flight || self.basket.acts.is_empty() {
            return None;
        }
        let rev = self.feed.as_ref()?.folded.rev;
        self.in_flight = true;
        self.basket.summary = summary;
        Some((self.basket.payload(rev, auto), self.basket.acts.clone()))
    }

    /// The Propose came back: on success (`Some`) the sent acts leave.
    pub(crate) fn settle_propose(&mut self, sent: Option<&[Value]>) {
        self.in_flight = false;
        if let Some(sent) = sent {
            self.basket.settle(sent);
        }
    }
}

/// Basket saves in the order they were made: each one waits for the
/// previous and an older one never lands after a newer one.
#[derive(Default)]
pub(crate) struct DraftSaver {
    issued: AtomicU64,
    written: tokio::sync::Mutex<u64>,
}

impl DraftSaver {
    /// The next save's generation.
    pub(crate) fn next(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Run `write` unless a newer generation already landed.
    pub(crate) async fn save<F, Fut>(&self, generation: u64, write: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut written = self.written.lock().await;
        if generation > *written {
            write().await;
            *written = generation;
        }
    }
}

static SAVER: LazyLock<DraftSaver> = LazyLock::new(DraftSaver::default);

thread_local! {
    static KB: RefCell<KanbanUi> = RefCell::new(KanbanUi::default());
    // the APPLIED `wake_on`, never the settings draft
    static WAKE_ON: RefCell<Vec<String>> = RefCell::new(
        molt_core::kanban_wake::WAKE_REASONS.iter().map(|r| (*r).to_string()).collect(),
    );
}

/// The applied `wake_on` (the mirror, on every settings push).
pub(crate) fn set_wake_on(on: Vec<String>) {
    WAKE_ON.with_borrow_mut(|w| *w = on);
}

/// Open the drill-in of `id` (a unique prefix will do); "" closes it.
fn pick(ui: &AppWindow, id: &str) {
    let full = with(|st| {
        st.evidence.clear();
        st.feed.as_ref().and_then(|f| resolve_task(f, id))
    });
    let k = ui.global::<Kanban>();
    k.set_note("".into());
    sync_strings(&k.get_evidence(), &[], |m| k.set_evidence(m));
    k.set_sel(full.unwrap_or_else(|| id.to_string()).into());
    render(ui);
}

/// A wiki `quest:` link: the Kanban pane with that task open; a dead link
/// stays put.
pub(crate) fn open_quest(ui: &AppWindow, hex: &str) {
    // the engine's link grammar, or the link would work one way only
    if !molt_core::wiki_refs::valid_quest_hex(hex) {
        return;
    }
    let Some(id) = with(|st| st.feed.as_ref().and_then(|f| resolve_task(f, hex))) else {
        return;
    };
    ui.invoke_select_surface("quests".into());
    pick(ui, &id);
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
/// state; the first push with a board brings the stored basket), then
/// render.
pub(crate) fn apply(ui: &AppWindow, b: &SurfacesBundle) {
    let k = ui.global::<Kanban>();
    with(|st| {
        if st.workspace != b.workspace {
            // toasted starts outlive the switch: task ids never collide
            let seen = std::mem::take(&mut st.notices.seen_starts);
            *st = KanbanUi { workspace: b.workspace.clone(), ..KanbanUi::default() };
            st.notices.seen_starts = seen;
            k.set_sel("".into());
            k.set_form_open(false);
            k.set_basket_summary("".into());
        }
        st.lang = b.lang;
        st.feed = b.kanban.clone();
        let draft = st.feed.as_ref().and_then(|f| f.draft.as_deref());
        if let (false, Some(draft)) = (st.draft_adopted, draft) {
            st.draft_adopted = true;
            if st.basket.acts.is_empty() {
                st.basket = Basket::from_draft(draft);
                k.set_basket_summary(st.basket.summary.as_str().into());
            }
        }
    });
    note_starts(ui);
    render(ui);
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
        k.set_todo_count(b.counts[0]);
        k.set_wip_count(b.counts[1]);
        k.set_done_count(b.counts[2]);
        sync_rows(&k.get_legend(), b.legend, |m| k.set_legend(m));
        let [who, status, state, seat]: [Vec<KbPick>; 4] =
            filter_rows(&l, feed, &st.filters).try_into().unwrap_or_default();
        sync_rows(&k.get_filters(), who, |m| k.set_filters(m));
        sync_rows(&k.get_filters_status(), status, |m| k.set_filters_status(m));
        sync_rows(&k.get_filters_state(), state, |m| k.set_filters_state(m));
        sync_rows(&k.get_filters_seat(), seat, |m| k.set_filters_seat(m));
        st.cal_src = render_views(&k, feed, st, &l);

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
                sync_rows(&k.get_cited_by(), d.cited_by, |m| k.set_cited_by(m));
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

        let (actions, rest) = mine(&l, st.lang, feed);
        k.set_action_count(i32::try_from(actions.len()).unwrap_or(i32::MAX));
        sync_rows(&k.get_actions(), actions, |m| k.set_actions(m));
        sync_rows(&k.get_mine(), rest, |m| k.set_mine(m));
        sync_rows(&k.get_declined(), declined(feed), |m| k.set_declined(m));

        if k.get_form_open() {
            render_form_pickers(&k, st);
        }
    });
}

/// The views beside the board (§8): plan, dependencies, calendar.
fn render_views(k: &Kanban<'_>, feed: &KanbanFeed, st: &KanbanUi, l: &Lexicon) -> Vec<CalSrc> {
    let p = plan(l, feed, &st.filters, &st.plan_closed);
    sync_rows(&k.get_plan_rows(), p.rows, |m| k.set_plan_rows(m));
    sync_rows(&k.get_plan_bars(), p.bars, |m| k.set_plan_bars(m));
    sync_rows(&k.get_plan_arrows(), p.arrows, |m| k.set_plan_arrows(m));
    sync_rows(&k.get_plan_ticks(), p.ticks, |m| k.set_plan_ticks(m));
    sync_rows(&k.get_plan_legend(), p.legend, |m| k.set_plan_legend(m));
    k.set_plan_today(p.today);
    k.set_plan_nodate(p.nodate.and_then(|n| i32::try_from(n).ok()).unwrap_or(-1));
    let d = dependencies(l, feed, &st.filters, &st.deps_toggled);
    sync_rows(&k.get_deps(), d.rows, |m| k.set_deps(m));
    sync_rows(&k.get_deps_legend(), d.legend, |m| k.set_deps_legend(m));
    let anchor = st.cal_anchor.unwrap_or_else(|| crate::kanban::today_of(feed));
    let c = calendar(l, feed, &st.basket, anchor, st.cal_week);
    k.set_cal_title(c.title.into());
    k.set_cal_week(st.cal_week);
    let weekdays: Vec<String> = l.kb_weekdays.split(' ').map(str::to_string).collect();
    sync_strings(&k.get_cal_weekdays(), &weekdays, |m| k.set_cal_weekdays(m));
    sync_rows(&k.get_cal_days(), c.days, |m| k.set_cal_days(m));
    sync_rows(&k.get_cal_blocks(), c.blocks, |m| k.set_cal_blocks(m));
    sync_rows(&k.get_cal_legend(), c.legend, |m| k.set_cal_legend(m));
    c.src
}

/// No board: nothing of the previous workspace may stay on screen.
fn clear(k: &Kanban<'_>) {
    sync_rows(&k.get_plan_rows(), Vec::new(), |m| k.set_plan_rows(m));
    sync_rows(&k.get_plan_bars(), Vec::new(), |m| k.set_plan_bars(m));
    sync_rows(&k.get_plan_arrows(), Vec::new(), |m| k.set_plan_arrows(m));
    sync_rows(&k.get_plan_ticks(), Vec::new(), |m| k.set_plan_ticks(m));
    sync_rows(&k.get_plan_legend(), Vec::new(), |m| k.set_plan_legend(m));
    k.set_plan_today(-1.0);
    k.set_plan_nodate(-1);
    sync_rows(&k.get_deps(), Vec::new(), |m| k.set_deps(m));
    sync_rows(&k.get_deps_legend(), Vec::new(), |m| k.set_deps_legend(m));
    sync_rows(&k.get_cal_blocks(), Vec::new(), |m| k.set_cal_blocks(m));
    sync_rows(&k.get_cal_days(), Vec::new(), |m| k.set_cal_days(m));
    sync_rows(&k.get_cal_legend(), Vec::new(), |m| k.set_cal_legend(m));
    sync_strings(&k.get_cal_weekdays(), &[], |m| k.set_cal_weekdays(m));
    k.set_cal_title("".into());
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
    k.set_todo_count(0);
    k.set_wip_count(0);
    k.set_done_count(0);
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

/// An engine event on the UI thread: the notification it raises here.
pub(crate) fn notice_event(ui: &AppWindow, ev: &Event) {
    if let Some((reason, by)) = notice_reason(ev, ui.get_node_member().as_str()) {
        notice(ui, reason, &by);
    }
}

/// A trigger from the event stream (UI thread): `poked` carries the poker.
fn notice(ui: &AppWindow, reason: &'static str, by: &str) {
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
    let generation = SAVER.next();
    let w = ctx.wallet.clone();
    ctx.rt.spawn(async move {
        SAVER
            .save(generation, || async move {
                let _ = w.execute(Command::KanbanDraftSave { draft }).await;
            })
            .await;
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

/// A calendar block dropped on page day `day` (`minute` < 0: no time):
/// stage the move (§8). `false` when nothing moved.
pub(crate) fn cal_drop(ui: &AppWindow, src: usize, day: usize, minute: i32) -> bool {
    let staged = with(|st| {
        let feed = st.feed.as_ref()?;
        let from = st.cal_src.get(src)?.clone();
        let anchor = st.cal_anchor.unwrap_or_else(|| crate::kanban::today_of(feed));
        let target = *crate::kanban_views::page_days(anchor, st.cal_week).get(day)?;
        let (id, fields) = drop_fields(feed, &st.basket, &from, target, u32::try_from(minute).ok())?;
        st.basket.stage_set(&id, fields);
        Some(())
    });
    if staged.is_some() {
        render(ui);
    }
    staged.is_some()
}

/// Free calendar slots from `day`/`minute` to `end_day`/`end_minute`
/// (minute < 0: whole days): the New task form, timed once there.
pub(crate) fn cal_new(ui: &AppWindow, day: usize, minute: i32, end_day: usize, end_minute: i32) {
    let target = with(|st| {
        let feed = st.feed.as_ref()?;
        let anchor = st.cal_anchor.unwrap_or_else(|| crate::kanban::today_of(feed));
        let days = crate::kanban_views::page_days(anchor, st.cal_week);
        Some((*days.get(day)?, *days.get(end_day)?))
    });
    let Some((a, b)) = target else { return };
    form_new(ui);
    let (start, end) = slot_range(a, u32::try_from(minute).ok(), b, u32::try_from(end_minute).ok());
    let k = ui.global::<Kanban>();
    k.set_f_mode(1);
    k.set_f_start(start.into());
    k.set_f_end(end.into());
}

/// Page the calendar: `step` pages forward (negative: back), 0 = today.
pub(crate) fn cal_step(ui: &AppWindow, step: i32) {
    with(|st| {
        let Some(feed) = st.feed.as_ref() else { return };
        let today = crate::kanban::today_of(feed);
        let anchor = st.cal_anchor.unwrap_or(today);
        st.cal_anchor = Some(match (step, st.cal_week) {
            (0, _) => today,
            (n, true) => anchor + chrono::TimeDelta::weeks(i64::from(n)),
            (n, false) => {
                let months = chrono::Months::new(n.unsigned_abs());
                let first = anchor.with_day(1).unwrap_or(anchor);
                if n > 0 { first.checked_add_months(months) } else { first.checked_sub_months(months) }.unwrap_or(anchor)
            }
        });
    });
    render(ui);
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
    let (form, l) = with(|st| (read_form(&k, st), lex(st.lang)));
    match form_act(&l, &form) {
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
    let Some((payload, sent)) = with(|st| st.begin_propose(summary, &auto)) else {
        return;
    };
    let w = ctx.wallet.clone();
    let weak = ctx.weak.clone();
    let cx = ctx.clone();
    ctx.rt.spawn(async move {
        let outcome = w.execute(Command::Propose { surface: Surface::Quests, payload }).await;
        let _ = slint::invoke_from_event_loop(move || {
            let ok = outcome.is_ok();
            with(|st| st.settle_propose(ok.then_some(sent.as_slice())));
            let Some(ui) = weak.upgrade() else { return };
            match outcome {
                Ok(_) => {
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
                with(|st| toggle_filter(&mut st.filters, key.as_str()));
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
            on_ui(&|ui| pick(ui, id.as_str()));
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_open_page(move |path| {
            on_ui(&|ui| {
                ui.invoke_select_surface("memory".into());
                ui.global::<crate::WikiState>().invoke_open_link(path.clone());
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
        let cx = ctx.clone();
        k.on_basket_summary_edited(move |text| {
            with(|st| st.basket.summary = text.to_string());
            save_basket(&cx);
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
                with(|st| {
                    toggle(&mut st.form.assignees, seat.as_str());
                    render_form_pickers(&ui.global::<Kanban>(), st);
                });
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_blocked_toggle(move |id| {
            on_ui(&|ui| {
                with(|st| {
                    toggle(&mut st.form.blocked, id.as_str());
                    render_form_pickers(&ui.global::<Kanban>(), st);
                });
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_f_blocked_filter_edited(move |_| {
            on_ui(&|ui| with(|st| render_form_pickers(&ui.global::<Kanban>(), st)));
        });
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
    wire_views(&k, &on_ui, ctx);
    wire_settings(ui, ctx);
    start_actions_timer(ctx);
    start_clock(ui);
}

/// The callbacks of plan, dependencies and calendar.
fn wire_views(k: &Kanban<'_>, on_ui: &(impl Fn(&dyn Fn(&AppWindow)) + Clone + 'static), ctx: &Ctx) {
    {
        let on_ui = on_ui.clone();
        k.on_plan_toggle(move |id| {
            on_ui(&|ui| {
                with(|st| flip(&mut st.plan_closed, id.as_str()));
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_deps_toggle(move |key| {
            on_ui(&|ui| {
                with(|st| flip(&mut st.deps_toggled, key.as_str()));
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_cal_step(move |n| on_ui(&|ui| cal_step(ui, n)));
    }
    {
        let on_ui = on_ui.clone();
        k.on_cal_today(move || on_ui(&|ui| cal_step(ui, 0)));
    }
    {
        let on_ui = on_ui.clone();
        k.on_cal_set_week(move |week| {
            on_ui(&|ui| {
                with(|st| st.cal_week = week);
                render(ui);
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        let cx = ctx.clone();
        k.on_cal_drop(move |src, day, minute| {
            on_ui(&|ui| {
                if let (Ok(src), Ok(day)) = (usize::try_from(src), usize::try_from(day)) {
                    if cal_drop(ui, src, day, minute) {
                        save_basket(&cx);
                    }
                }
            });
        });
    }
    {
        let on_ui = on_ui.clone();
        k.on_cal_new(move |day, minute, end_day, end_minute| {
            on_ui(&|ui| {
                if let (Ok(day), Ok(end_day)) = (usize::try_from(day), usize::try_from(end_day)) {
                    cal_new(ui, day, minute, end_day, end_minute);
                }
            });
        });
    }
}

fn flip(set: &mut BTreeSet<String>, key: &str) {
    if !set.remove(key) {
        set.insert(key.to_string());
    }
}

thread_local! {
    static CLOCK: slint::Timer = slint::Timer::default();
}

/// Organization › Status shows the UTC time (§2.3).
pub(crate) fn start_clock(ui: &AppWindow) {
    let tick = |ui: &AppWindow| ui.set_org_utc_now(utc_clock(chrono::Utc::now().naive_utc()).into());
    tick(ui);
    let weak = ui.as_weak();
    CLOCK.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(10), move || {
            if let Some(ui) = weak.upgrade() {
                tick(&ui);
            }
        });
    });
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
            let outcome = write_skill(&dir);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                match outcome {
                    Ok(path) => {
                        let saved = ui.global::<Strings>().get_skill_saved();
                        ui.invoke_show_toast(format!("{saved} {}", path.display()).into());
                    }
                    Err(e) => ui.invoke_show_toast_error(format!("SKILL.md: {e}").into()),
                }
            });
        });
    });
}

/// `dir/SKILL.md`, created new: an existing file or link there is refused.
pub(crate) fn write_skill(dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    let path = dir.join("SKILL.md");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?
        .write_all(molt_mcp::WAKE_SKILL.as_bytes())?;
    Ok(path)
}
