// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban views beside the board (`docs_archive/kanban/kanban_workflows.md`
//! §8): the deadline timeline (`plan`), the calendar and the dependency
//! tree. Pure projections over the Quests snapshot and the basket.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta};
use molt_core::kanban_calendar::{fmt_date, fmt_datetime, parse_date, When};
use molt_core::kanban_fold::short_id;
use molt_core::kanban_view::{quests_view, type_key, type_slot, type_spellings, ViewFilter, ViewQuery};
use serde_json::{json, Map, Value};

use crate::i18n::Lexicon;
use crate::kanban::{
    fake_id, listed, s, shown_label, strs, tasks, today_of, when_line, Basket, KanbanFeed,
};
use crate::{KbCalBlock, KbCalDay, KbDepRow, KbPlanArrow, KbPlanBar, KbPlanRow, KbTick, KbType};

/// Red on every view: stuck, overdue or a date conflict (§8).
pub(crate) fn is_bad(t: &Value) -> bool {
    s(t, "shown") == "stuck" || strs(t, "flags").iter().any(|f| f == "overdue" || f == "date_conflict")
}

/// Whether any chip narrows the view (another seat's view alone does not).
fn filtering(filters: &BTreeSet<String>) -> bool {
    filters.iter().any(|f| ViewFilter::parse(f).is_ok())
}

/// The legend of the types among `ids` (§2.4), a picked type kept.
fn legend(all: &Map<String, Value>, ids: &[String], filters: &BTreeSet<String>) -> Vec<KbType> {
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let present: BTreeSet<String> =
        ids.iter().filter_map(|id| all.get(id)).map(|t| type_key(s(t, "type"))).collect();
    spell
        .into_iter()
        .filter(|(key, _)| present.contains(key) || filters.contains(&format!("type:{key}")))
        .map(|(key, label)| KbType {
            on: filters.contains(&format!("type:{key}")),
            slot: type_slot(&key).map_or(-1, i32::from),
            key: key.into(),
            label: label.into(),
        })
        .collect()
}

fn type_of(spell: &BTreeMap<String, String>, t: &Value) -> (String, i32) {
    let kind = s(t, "type");
    let label = spell.get(&type_key(kind)).cloned().unwrap_or_else(|| kind.to_string());
    (label, type_slot(kind).map_or(-1, i32::from))
}

fn ids_of(rows: &[Value]) -> Vec<String> {
    rows.iter().map(|t| s(t, "id").to_string()).collect()
}

/// Task id -> the tasks it is a prerequisite for, over every task.
fn dependents(all: &Map<String, Value>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, t) in all {
        for b in strs(t, "blocked_by") {
            out.entry(b).or_default().push(id.clone());
        }
    }
    out
}

/// Its prerequisites to any depth: (succeeded, total).
fn progress(all: &Map<String, Value>, id: &str) -> (usize, usize) {
    let mut seen = BTreeSet::from([id.to_string()]);
    let mut stack = vec![id.to_string()];
    let (mut done, mut total) = (0, 0);
    while let Some(cur) = stack.pop() {
        for b in all.get(&cur).map(|t| strs(t, "blocked_by")).unwrap_or_default() {
            if seen.insert(b.clone()) {
                total += 1;
                if all.get(&b).is_some_and(|t| s(t, "state") == "success") {
                    done += 1;
                }
                stack.push(b);
            }
        }
    }
    (done, total)
}

// -- dependencies -------------------------------------------------------------

/// The dependency tree and its legend.
#[derive(Default)]
pub(crate) struct Deps {
    pub(crate) rows: Vec<KbDepRow>,
    pub(crate) legend: Vec<KbType>,
}

/// §8 `dependencies`: the tasks that are a prerequisite for nothing on top,
/// each expandable to any depth; a shared prerequisite under every parent
/// ("also for …"). A filter keeps its matches and, muted, what they are a
/// prerequisite for. Collapsed by default, expanded while filtering;
/// `toggled` holds the paths flipped from that default.
pub(crate) fn dependencies(l: &Lexicon, feed: &KanbanFeed, filters: &BTreeSet<String>, toggled: &BTreeSet<String>) -> Deps {
    let all = tasks(&feed.snap);
    let narrowed = filtering(filters);
    let matched = ids_of(&listed(&feed.snap, filters, true));
    let deps = dependents(&all);
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let mut order = matched.clone();
    let mut visible: BTreeSet<String> = matched.iter().cloned().collect();
    if narrowed {
        let mut stack = matched.clone();
        while let Some(id) = stack.pop() {
            for d in deps.get(&id).into_iter().flatten() {
                if visible.insert(d.clone()) {
                    order.push(d.clone());
                    stack.push(d.clone());
                }
            }
        }
    }
    let hits: BTreeSet<&String> = matched.iter().collect();
    let children = |id: &str| -> Vec<String> {
        let kids = all.get(id).map(|t| strs(t, "blocked_by")).unwrap_or_default();
        kids.into_iter().filter(|b| all.contains_key(b) && (!narrowed || visible.contains(b))).collect()
    };
    let tops: Vec<String> = order
        .iter()
        .filter(|id| !deps.get(*id).into_iter().flatten().any(|d| visible.contains(d)))
        .cloned()
        .collect();

    let mut rows = Vec::new();
    let mut progress_of: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    // filtering opens a task under its first path only, so a shared lattice stays linear
    let mut opened: BTreeSet<String> = BTreeSet::new();
    // (path, id, depth, parent)
    let mut stack: Vec<(String, String, usize, Option<String>)> =
        tops.iter().rev().map(|id| (id.clone(), id.clone(), 0, None)).collect();
    while let Some((path, id, depth, parent)) = stack.pop() {
        let Some(t) = all.get(&id) else { continue };
        let kids = children(&id);
        let open = (narrowed && !opened.contains(&id)) != toggled.contains(&path);
        if open {
            opened.insert(id.clone());
        }
        let (done, total) = *progress_of.entry(id.clone()).or_insert_with(|| progress(&all, &id));
        let (type_label, type_slot) = type_of(&spell, t);
        let also: Vec<String> = deps
            .get(&id)
            .into_iter()
            .flatten()
            .filter(|d| parent.as_deref() != Some(d.as_str()))
            .filter_map(|d| all.get(d).map(|dt| s(dt, "title").to_string()))
            .collect();
        rows.push(KbDepRow {
            key: path.as_str().into(),
            id: id.as_str().into(),
            depth: i32::try_from(depth).unwrap_or(i32::MAX),
            title: s(t, "title").into(),
            short: short_id(&id).into(),
            type_label: type_label.into(),
            type_slot,
            badge: shown_label(l, s(t, "shown")).into(),
            bad: is_bad(t),
            progress: if total > 0 { format!("{done}/{total}") } else { String::new() }.into(),
            needed: match s(t, "needed_by") {
                "" => String::new(),
                nb => format!("{} {}", l.kb_needed, short_date(l, nb)),
            }
            .into(),
            also: if parent.is_some() && !also.is_empty() {
                format!("{} {}", l.kb_also_for, also.join(", "))
            } else {
                String::new()
            }
            .into(),
            muted: narrowed && !hits.contains(&id),
            kids: !kids.is_empty(),
            open: open && !kids.is_empty(),
        });
        if open {
            for k in kids.iter().rev() {
                // a DAG has no cycle; the path guard keeps a corrupt one finite
                if !path.split('>').any(|p| p == k) {
                    stack.push((format!("{path}>{k}"), k.clone(), depth + 1, Some(id.clone())));
                }
            }
        }
    }
    let shown: Vec<String> = rows.iter().map(|r| r.id.to_string()).collect();
    Deps { legend: legend(&all, &shown, filters), rows }
}

// -- dates ----------------------------------------------------------------------

fn month_names(l: &Lexicon) -> Vec<&'static str> {
    l.kb_months.split(' ').collect()
}

/// "12 Oct" / "12. Okt" from `YYYY-MM-DD` (the input when it is no date).
pub(crate) fn short_date(l: &Lexicon, date: &str) -> String {
    parse_date(date).map_or_else(|| date.to_string(), |d| day_label(l, d))
}

fn day_label(l: &Lexicon, d: NaiveDate) -> String {
    let m = month_names(l).get(d.month0() as usize).copied().unwrap_or_default();
    l.kb_day_fmt.replace("{d}", &d.day().to_string()).replace("{m}", m)
}

fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_time(NaiveTime::MIN)
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.checked_add_days(Days::new(1)).unwrap_or(d)
}

// -- plan -------------------------------------------------------------------------

/// The deadline timeline.
#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) rows: Vec<KbPlanRow>,
    pub(crate) bars: Vec<KbPlanBar>,
    pub(crate) arrows: Vec<KbPlanArrow>,
    pub(crate) ticks: Vec<KbTick>,
    /// Today on the axis (0..1).
    pub(crate) today: f32,
    /// The first row of the "no date" lane, if any.
    pub(crate) nodate: Option<usize>,
    pub(crate) legend: Vec<KbType>,
}

/// A span on the time axis: (begins, ends, is a `needed_by` marker).
type Span = (NaiveDateTime, NaiveDateTime, bool);

/// Where a task sits in time: its `when` blocks (a series' occurrences in
/// the axis window), else its `needed_by` marker, else nothing.
fn spans(t: &Value, window: Option<(NaiveDate, NaiveDate)>) -> Vec<Span> {
    if let Some(w) = t.get("when").and_then(|w| When::parse(w).ok()) {
        if t.get("repeat").is_none() {
            return vec![(w.begins(), w.ends(), false)];
        }
        let Some((from, to)) = window else { return Vec::new() };
        return occurrences(t, from, to).into_iter().map(|w| (w.begins(), w.ends(), false)).collect();
    }
    match parse_date(s(t, "needed_by")) {
        Some(nb) => {
            let mid = midnight(nb) + TimeDelta::hours(12);
            vec![(mid, mid, true)]
        }
        None => Vec::new(),
    }
}

/// A series' occurrences in `[from, to]`, through the same read the agents
/// get (`quests_view` calendar).
fn occurrences(t: &Value, from: NaiveDate, to: NaiveDate) -> Vec<When> {
    page_occurrences(json!({ "tasks": { "x": t } }), from, to).into_iter().map(|(_, _, w)| w).collect()
}

/// How far the plan's axis reaches around today, in days.
const YEAR: u64 = 365;
const AXIS_BACK: u64 = YEAR;
const AXIS_AHEAD: u64 = 2 * YEAR;

/// Where a task widens the axis: a series at its next occurrence (else
/// its first), anything else at its spans.
fn axis_spans(t: &Value, today: NaiveDate) -> Vec<Span> {
    match t.get("when").and_then(|w| When::parse(w).ok()) {
        Some(w) if t.get("repeat").is_some() => {
            let ahead = today.checked_add_days(Days::new(YEAR)).unwrap_or(today);
            let next = occurrences(t, today, ahead).into_iter().next().unwrap_or(w);
            vec![(next.begins(), next.ends(), false)]
        }
        _ => spans(t, None),
    }
}

/// A bare Quests snapshot around a board read.
fn board_snap(board: Value) -> Option<molt_core::SurfaceSnapshot> {
    let mut snap: molt_core::SurfaceSnapshot =
        serde_json::from_value(json!({ "surface": "quests", "gated": true, "applied": [], "pending": [] })).ok()?;
    snap.board = Some(board);
    Some(snap)
}

/// §8 `plan`: timed tasks as blocks at their `when`, floating ones as
/// markers at their `needed_by`; a task is a collapsible row over its
/// prerequisites and sits once (under its first parent), every link an
/// arrow; trees without any date form the "no date" lane at the bottom.
pub(crate) fn plan(l: &Lexicon, feed: &KanbanFeed, filters: &BTreeSet<String>, collapsed: &BTreeSet<String>) -> Plan {
    let all = tasks(&feed.snap);
    let kept = ids_of(&listed(&feed.snap, filters, true));
    let keep: BTreeSet<&String> = kept.iter().collect();
    let deps = dependents(&all);
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let today = today_of(feed);
    let children = |id: &str| -> Vec<String> {
        let kids = all.get(id).map(|t| strs(t, "blocked_by")).unwrap_or_default();
        let mut kids: Vec<String> = kids.into_iter().filter(|b| keep.contains(b)).collect();
        // priority order, as the board lists them
        kids.sort_by_key(|k| kept.iter().position(|x| x == k));
        kids
    };
    let tops: Vec<&String> =
        kept.iter().filter(|id| !deps.get(*id).into_iter().flatten().any(|d| keep.contains(d))).collect();

    // place each task once, depth first under its first visible parent
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut trees: Vec<Vec<(String, usize)>> = Vec::new();
    for top in tops {
        if placed.contains(top) {
            continue;
        }
        let mut tree = Vec::new();
        let mut stack = vec![(top.clone(), 0usize)];
        while let Some((id, depth)) = stack.pop() {
            if !placed.insert(id.clone()) {
                continue;
            }
            tree.push((id.clone(), depth));
            if !collapsed.contains(&id) {
                for k in children(&id).into_iter().rev() {
                    if !placed.contains(&k) {
                        stack.push((k, depth + 1));
                    }
                }
            }
        }
        trees.push(tree);
    }

    // the axis: every date and today, at least two weeks, bounded around today
    let mut lo = today;
    let mut hi = today;
    for id in &kept {
        let Some(t) = all.get(id) else { continue };
        for (b, e, _) in axis_spans(t, today) {
            lo = lo.min(b.date());
            hi = hi.max(if e.time() == NaiveTime::MIN && e > b { e.date().pred_opt().unwrap_or(e.date()) } else { e.date() });
        }
    }
    lo = lo.max(today.checked_sub_days(Days::new(AXIS_BACK)).unwrap_or(lo));
    hi = hi.min(today.checked_add_days(Days::new(AXIS_AHEAD)).unwrap_or(hi));
    let from = lo.pred_opt().unwrap_or(lo);
    let mut to = next_day(hi);
    if (to - from).num_days() < 13 {
        to = from.checked_add_days(Days::new(13)).unwrap_or(to);
    }
    let start = midnight(from);
    let span_secs = (midnight(next_day(to)) - start).num_seconds().max(1) as f64;
    // a date beyond the bounded axis sits at its edge
    let x = |t: NaiveDateTime| -> f32 { ((t - start).num_seconds() as f64 / span_secs).clamp(0.0, 1.0) as f32 };

    let dated = |tree: &Vec<(String, usize)>| {
        tree.iter().any(|(id, _)| all.get(id).is_some_and(|t| !axis_spans(t, today).is_empty()))
    };
    let (with, without): (Vec<_>, Vec<_>) = trees.into_iter().partition(dated);
    let mut out = Plan { today: x(midnight(today) + TimeDelta::hours(12)), ..Plan::default() };
    let mut row_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut ends: BTreeMap<String, (f32, f32)> = BTreeMap::new();
    for (lane, tree) in [(0, with), (1, without)] {
        for (id, depth) in tree.into_iter().flatten() {
            let Some(t) = all.get(&id) else { continue };
            if lane == 1 && out.nodate.is_none() {
                out.nodate = Some(out.rows.len());
            }
            let row = out.rows.len();
            let (type_label, type_slot) = type_of(&spell, t);
            let bad = is_bad(t);
            let date = if t.get("when").is_some() {
                when_line(l, t)
            } else {
                match s(t, "needed_by") {
                    "" => String::new(),
                    nb => format!("{} {}", l.kb_needed, short_date(l, nb)),
                }
            };
            let kids = !children(&id).is_empty();
            out.rows.push(KbPlanRow {
                id: id.as_str().into(),
                depth: i32::try_from(depth).unwrap_or(i32::MAX),
                title: s(t, "title").into(),
                short: short_id(&id).into(),
                type_label: type_label.into(),
                type_slot,
                size: s(t, "size").into(),
                badge: shown_label(l, s(t, "shown")).into(),
                bad,
                kids,
                open: kids && !collapsed.contains(&id),
                date: date.into(),
            });
            let mut first: Option<(f32, f32)> = None;
            for (b, e, marker) in spans(t, Some((from, to))) {
                let (x0, x1) = (x(b), x(e));
                first.get_or_insert((x0, x1));
                out.bars.push(KbPlanBar {
                    row: i32::try_from(row).unwrap_or(i32::MAX),
                    x0,
                    x1,
                    marker,
                    slot: type_slot,
                    bad,
                });
            }
            row_of.insert(id.clone(), row);
            if let Some(f) = first {
                ends.insert(id.clone(), f);
            }
        }
    }
    for (id, row) in &row_of {
        let Some(t) = all.get(id) else { continue };
        let Some(&(x0, _)) = ends.get(id) else { continue };
        for b in strs(t, "blocked_by") {
            if let (Some(br), Some(&(_, bx1))) = (row_of.get(&b), ends.get(&b)) {
                out.arrows.push(KbPlanArrow {
                    r1: i32::try_from(*br).unwrap_or(i32::MAX),
                    x1: bx1,
                    r2: i32::try_from(*row).unwrap_or(i32::MAX),
                    x2: x0,
                    bad: bx1 > x0 || strs(t, "flags").iter().any(|f| f == "date_conflict"),
                });
            }
        }
    }
    out.ticks = ticks(l, from, to, &x);
    let shown: Vec<String> = out.rows.iter().map(|r| r.id.to_string()).collect();
    out.legend = legend(&all, &shown, filters);
    out
}

/// Axis labels: daily up to two weeks, Mondays up to four months, the
/// first of each month up to three years, else each new year.
fn ticks(l: &Lexicon, from: NaiveDate, to: NaiveDate, x: &dyn Fn(NaiveDateTime) -> f32) -> Vec<KbTick> {
    let days = (to - from).num_days();
    let mut out = Vec::new();
    let mut d = from;
    while d <= to {
        let on = if days <= 14 {
            true
        } else if days <= 120 {
            d.weekday().num_days_from_monday() == 0
        } else if days <= 1100 {
            d.day() == 1
        } else {
            d.ordinal() == 1
        };
        if on {
            let label = if days > 1100 { d.year().to_string() } else { day_label(l, d) };
            out.push(KbTick { x: x(midnight(d)), label: label.into() });
        }
        d = next_day(d);
        if d == from {
            break;
        }
    }
    out
}

// -- calendar -----------------------------------------------------------------------

/// Where a dragged block came from: one day's segment of an occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CalSrc {
    pub(crate) id: String,
    /// The occurrence date of a series, `None` for a once-timed task.
    pub(crate) occ: Option<NaiveDate>,
    /// The occurrence's window.
    pub(crate) window: When,
    /// The day the segment sits on, and where on it it begins.
    pub(crate) day: NaiveDate,
    pub(crate) begins: NaiveDateTime,
}

/// The calendar page.
#[derive(Default)]
pub(crate) struct Cal {
    pub(crate) title: String,
    pub(crate) days: Vec<KbCalDay>,
    pub(crate) blocks: Vec<KbCalBlock>,
    pub(crate) src: Vec<CalSrc>,
    pub(crate) legend: Vec<KbType>,
}

/// How many blocks a month cell shows before "+n".
pub(crate) const MONTH_ROWS: usize = 3;

/// The days a page shows: a month as whole weeks, or the week of `anchor`.
pub(crate) fn page_days(anchor: NaiveDate, week: bool) -> Vec<NaiveDate> {
    let monday = |d: NaiveDate| d - TimeDelta::days(i64::from(d.weekday().num_days_from_monday()));
    let (first, last) = if week {
        let m = monday(anchor);
        (m, m + TimeDelta::days(6))
    } else {
        let one = anchor.with_day(1).unwrap_or(anchor);
        let end = one.checked_add_months(Months::new(1)).and_then(|d| d.pred_opt()).unwrap_or(one);
        let first = monday(one);
        let last = monday(end) + TimeDelta::days(6);
        (first, last)
    };
    first.iter_days().take_while(|d| *d <= last).collect()
}

/// The board with the basket's `set`s and timed `add`s applied: where the
/// shadows go.
fn staged_board(feed: &KanbanFeed, basket: &Basket) -> Option<Value> {
    let mut board = feed.snap.board.clone()?;
    let tasks = board.get_mut("tasks")?.as_object_mut()?;
    let mut k = 0u32;
    for act in &basket.acts {
        match s(act, "act") {
            "set" => {
                let (Some(t), Some(fields)) =
                    (tasks.get_mut(s(act, "id")).and_then(Value::as_object_mut), act.get("fields").and_then(Value::as_object))
                else {
                    continue;
                };
                for (f, v) in fields {
                    if v.is_null() {
                        t.remove(f);
                    } else {
                        t.insert(f.clone(), v.clone());
                    }
                }
            }
            "add" => {
                k += 1;
                let mut t = act.as_object().cloned().unwrap_or_default();
                t.insert("state".into(), Value::from("todo"));
                t.insert("shown".into(), Value::from("scheduled"));
                tasks.insert(fake_id(k), Value::Object(t));
            }
            _ => {}
        }
    }
    Some(board)
}

/// The occurrences on a page: (task, occurrence, window).
fn page_occurrences(board: Value, from: NaiveDate, to: NaiveDate) -> Vec<(String, Option<NaiveDate>, When)> {
    let Some(snap) = board_snap(board) else { return Vec::new() };
    let q = ViewQuery { from: Some(from), to: Some(to), ..ViewQuery::default() };
    quests_view(&snap, &q)
        .ok()
        .and_then(|v| v.get("calendar").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|o| {
            let w = When::parse(&json!({ "start": s(o, "start"), "end": s(o, "end") })).ok()?;
            Some((s(o, "task").to_string(), parse_date(s(o, "occurrence")), w))
        })
        .collect()
}

/// §8 `calendar`: only timed tasks, a series expanded; the basket's moved
/// or added appointments as dashed shadows. Week blocks carry their place
/// in the day and a lane among overlapping ones.
pub(crate) fn calendar(l: &Lexicon, feed: &KanbanFeed, basket: &Basket, anchor: NaiveDate, week: bool) -> Cal {
    let days = page_days(anchor, week);
    let (Some(&first), Some(&last)) = (days.first(), days.last()) else { return Cal::default() };
    let today = today_of(feed);
    let all = tasks(&feed.snap);
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let staged = staged_board(feed, basket);
    let staged_tasks = staged.as_ref().and_then(|b| b.get("tasks")).and_then(Value::as_object).cloned().unwrap_or_default();
    let real = feed.snap.board.clone().map(|b| page_occurrences(b, first, last)).unwrap_or_default();
    let real_set: BTreeSet<(String, Option<NaiveDate>, When)> = real.iter().cloned().collect();
    let shadows: Vec<_> = staged
        .map(|b| page_occurrences(b, first, last))
        .unwrap_or_default()
        .into_iter()
        .filter(|o| !real_set.contains(o))
        .collect();

    let mut out = Cal::default();
    let index = |d: NaiveDate| usize::try_from((d - first).num_days()).ok();
    let mut per_day: Vec<Vec<KbCalBlock>> = vec![Vec::new(); days.len()];
    for (shadow, (id, occ, w)) in real.into_iter().map(|o| (false, o)).chain(shadows.into_iter().map(|o| (true, o))) {
        let source = if shadow { &staged_tasks } else { &all };
        let Some(t) = source.get(&id) else { continue };
        let (type_label, slot) = type_of(&spell, t);
        let all_day = matches!(w, When::AllDay { .. });
        let (b, e) = (w.begins(), w.ends());
        let mut d = b.date().max(first);
        while d <= last && midnight(d) < e {
            let seg_b = b.max(midnight(d));
            let seg_e = e.min(midnight(next_day(d)));
            let frac = |t: NaiveDateTime| ((t - midnight(d)).num_seconds() as f32) / 86_400.0;
            let src = if shadow {
                -1
            } else {
                out.src.push(CalSrc { id: id.clone(), occ, window: w, day: d, begins: seg_b });
                i32::try_from(out.src.len() - 1).unwrap_or(-1)
            };
            if let Some(i) = index(d) {
                per_day[i].push(KbCalBlock {
                    src,
                    id: id.as_str().into(),
                    title: s(t, "title").into(),
                    type_label: type_label.as_str().into(),
                    time: if all_day || seg_b != b { String::new() } else { b.format("%H:%M").to_string() }.into(),
                    slot,
                    bad: !shadow && is_bad(t),
                    shadow,
                    day: i32::try_from(i).unwrap_or(0),
                    order: 0,
                    all_day,
                    start: if all_day { 0.0 } else { frac(seg_b) },
                    end: if all_day { 1.0 } else { frac(seg_e) },
                    lane: 0,
                    lanes: 1,
                });
            }
            d = next_day(d);
        }
    }
    for (i, blocks) in per_day.iter_mut().enumerate() {
        // all-day first, then by start; shadows after the real block
        blocks.sort_by(|a, b| {
            (!a.all_day, a.start, a.shadow)
                .partial_cmp(&(!b.all_day, b.start, b.shadow))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut lanes_end: Vec<f32> = Vec::new();
        let mut group: Vec<usize> = Vec::new();
        for (n, blk) in blocks.iter_mut().enumerate() {
            blk.order = i32::try_from(n).unwrap_or(0);
        }
        // greedy lanes for the timed ones; a run of overlaps shares a width
        let mut max_end = 0.0f32;
        for n in 0..blocks.len() {
            if blocks[n].all_day {
                continue;
            }
            if blocks[n].start >= max_end && !group.is_empty() {
                let lanes = i32::try_from(lanes_end.len()).unwrap_or(1);
                group.drain(..).for_each(|g| blocks[g].lanes = lanes);
                lanes_end.clear();
            }
            let lane = lanes_end.iter().position(|e| *e <= blocks[n].start).unwrap_or(lanes_end.len());
            if lane == lanes_end.len() {
                lanes_end.push(blocks[n].end);
            } else {
                lanes_end[lane] = blocks[n].end;
            }
            blocks[n].lane = i32::try_from(lane).unwrap_or(0);
            max_end = max_end.max(blocks[n].end);
            group.push(n);
        }
        let lanes = i32::try_from(lanes_end.len().max(1)).unwrap_or(1);
        group.drain(..).for_each(|g| blocks[g].lanes = lanes);
        let d = days[i];
        let more = if week { blocks.iter().filter(|b| b.all_day).count() } else { blocks.len() }.saturating_sub(MONTH_ROWS);
        out.days.push(KbCalDay {
            date: fmt_date(d).into(),
            label: d.day().to_string().into(),
            in_month: week || d.month() == anchor.month(),
            today: d == today,
            more: i32::try_from(more).unwrap_or(0),
        });
    }
    for blocks in per_day {
        // week: the all-day band shows MONTH_ROWS, the hours everything timed
        let shown = |b: &KbCalBlock| usize::try_from(b.order).is_ok_and(|o| o < MONTH_ROWS);
        let kept = blocks.into_iter().filter(|b| (week && !b.all_day) || shown(b));
        out.blocks.extend(kept);
    }
    let ids: Vec<String> = out.blocks.iter().filter(|b| !b.shadow).map(|b| b.id.to_string()).collect();
    out.legend = legend(&all, &ids, &BTreeSet::new());
    let months = month_names_long(l);
    out.title = if week {
        format!("{} - {} {}", day_label(l, first), day_label(l, last), last.year())
    } else {
        format!("{} {}", months.get(anchor.month0() as usize).copied().unwrap_or_default(), anchor.year())
    };
    out
}

fn month_names_long(l: &Lexicon) -> Vec<&'static str> {
    l.kb_months_long.split(' ').collect()
}

/// The `set` a block dropped on `target` stages (§8: a drag moves, it never
/// resizes): a once-timed task's `when` shifts, a series occurrence lands
/// in `moved`. `minute` places it in the day (week view); without one only
/// the day changes. `None` when nothing moves.
pub(crate) fn drop_fields(feed: &KanbanFeed, basket: &Basket, src: &CalSrc, target: NaiveDate, minute: Option<u32>) -> Option<(String, Map<String, Value>)> {
    let delta = match (minute, src.window) {
        (Some(m), When::Timed { .. }) => {
            let at = midnight(target) + TimeDelta::minutes(i64::from(m));
            at - src.begins
        }
        _ => TimeDelta::days((target - src.day).num_days()),
    };
    if delta.is_zero() {
        return None;
    }
    let moved = match src.window {
        When::AllDay { start, end } => When::AllDay {
            start: start.checked_add_signed(TimeDelta::days(delta.num_days()))?,
            end: end.checked_add_signed(TimeDelta::days(delta.num_days()))?,
        },
        When::Timed { start, end } => When::Timed { start: start + delta, end: end + delta },
    };
    let mut fields = Map::new();
    match src.occ {
        None => {
            fields.insert("when".into(), moved.to_json());
        }
        Some(occ) => {
            // the staged `moved` map if one waits, else the board's
            let staged = basket
                .acts
                .iter()
                .rev()
                .find(|a| s(a, "act") == "set" && s(a, "id") == src.id)
                .and_then(|a| a.get("fields").and_then(|f| f.get("moved")).cloned());
            let board = tasks(&feed.snap).get(&src.id).and_then(|t| t.get("moved").cloned());
            let mut map = staged.or(board).and_then(|m| m.as_object().cloned()).unwrap_or_default();
            map.insert(fmt_date(occ), moved.to_json());
            fields.insert("moved".into(), Value::Object(map));
        }
    }
    Some((src.id.clone(), fields))
}

/// The form's window for free slots dragged from `a` to `b` (either way):
/// whole days, or the half-hour slots including both ends; one slot is an
/// hour.
pub(crate) fn slot_range(a: NaiveDate, am: Option<u32>, b: NaiveDate, bm: Option<u32>) -> (String, String) {
    match (am, bm) {
        (Some(am), Some(bm)) => {
            let at = |d: NaiveDate, m: u32| midnight(d) + TimeDelta::minutes(i64::from(m.min(23 * 60 + 30)));
            let (x, y) = (at(a, am), at(b, bm));
            let (lo, hi) = (x.min(y), x.max(y));
            let end = if lo == hi { lo + TimeDelta::hours(1) } else { hi + TimeDelta::minutes(30) };
            (fmt_datetime(lo), fmt_datetime(end))
        }
        _ => (fmt_date(a.min(b)), fmt_date(a.max(b))),
    }
}

/// §2.3: the clock line of Organization › Status.
pub(crate) fn utc_clock(now: NaiveDateTime) -> String {
    format!("{} UTC", now.format("%Y-%m-%d %H:%M"))
}
