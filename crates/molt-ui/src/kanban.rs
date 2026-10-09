// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban surface (`docs/kanban/kanban_workflows.md` §8): the engine's
//! board as card rows, the drill-in, the local basket of staged acts,
//! "Mine" and the coalesced notifications. Pure projections over the
//! Quests snapshot; the window wiring is `actions/kanban.rs`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use molt_core::kanban_calendar::parse_date;
use molt_core::kanban_dates::derive;
use molt_core::kanban_fold::{
    kanban_canonicalize, kanban_fold, kanban_precheck, short_id, validate_kanban_payload,
};
use molt_core::kanban_review::{advisories, render_acts, WOULD_VOID_PREFIX};
use molt_core::kanban_view::{quests_view, type_slot, type_spellings, ViewFilter, ViewQuery};
use molt_core::kanban_wake::WakeAction;
use molt_core::{ProposalState, SurfaceSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::i18n::Lexicon;
use crate::{KbCard, KbCheck, KbDetail, KbLine, KbPick, KbType};

/// What one mirror pass carries for the board.
#[derive(Clone, Debug)]
pub(crate) struct KanbanFeed {
    /// The Quests snapshot (its `board` is the engine's derived read).
    pub(crate) snap: SurfaceSnapshot,
    /// `read_actions` for this seat.
    pub(crate) actions: Vec<WakeAction>,
    /// The roster.
    pub(crate) seats: Vec<String>,
    /// The local seat.
    pub(crate) me: String,
}

/// The board filters the chips offer (§8), by wire name.
pub(crate) const FILTERS: [&str; 6] =
    ["mine", "to_act_on", "starting_now", "created_by_me", "needs_my_vote", "closed"];

fn filter_label(l: &Lexicon, key: &str) -> &'static str {
    match key {
        "mine" => l.kb_f_mine,
        "to_act_on" => l.kb_f_act,
        "starting_now" => l.kb_f_starting,
        "created_by_me" => l.kb_f_created,
        "needs_my_vote" => l.kb_f_vote,
        _ => l.kb_f_closed,
    }
}

/// The filter chips with their state.
pub(crate) fn filter_chips(l: &Lexicon, on: &BTreeSet<String>) -> Vec<KbPick> {
    FILTERS
        .iter()
        .map(|k| KbPick { key: (*k).into(), label: filter_label(l, k).into(), on: on.contains(*k) })
        .collect()
}

/// A governed state or derived status, localized.
pub(crate) fn shown_label(l: &Lexicon, s: &str) -> &'static str {
    match s {
        "stuck" => l.kb_sub_stuck,
        "blocked" => l.kb_sub_blocked,
        "scheduled" => l.kb_sub_scheduled,
        "later" => l.kb_sub_later,
        "open" => l.kb_sub_open,
        "todo" => l.kb_st_todo,
        "wip" => l.kb_st_wip,
        "success" => l.kb_st_success,
        "fail" => l.kb_st_fail,
        "cancelled" => l.kb_st_cancelled,
        _ => "",
    }
}

fn flag_label(l: &Lexicon, f: &str) -> &'static str {
    match f {
        "overdue" => l.kb_flag_overdue,
        "date_conflict" => l.kb_flag_conflict,
        "started_on_reopened" => l.kb_flag_reopened,
        _ => "",
    }
}

fn s<'a>(t: &'a Value, key: &str) -> &'a str {
    t.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn strs(t: &Value, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// The board read's tasks (`{id: task}`), empty without one.
fn tasks(snap: &SurfaceSnapshot) -> Map<String, Value> {
    snap.board
        .as_ref()
        .and_then(|b| b.get("tasks"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The column a state lands in: 0 to do, 1 in progress, 2 done.
pub(crate) fn column_of(state: &str) -> usize {
    match state {
        "todo" => 0,
        "wip" => 1,
        _ => 2,
    }
}

/// The order of the derived sub-states in To do (§8).
fn todo_rank(shown: &str) -> u8 {
    match shown {
        "blocked" => 0,
        "stuck" => 1,
        "scheduled" => 2,
        "later" => 3,
        _ => 4,
    }
}

fn freq_label(l: &Lexicon, f: &str) -> &'static str {
    match f {
        "daily" => l.kb_daily,
        "monthly" => l.kb_monthly,
        _ => l.kb_weekly,
    }
}

/// `when` / `repeat` in one line ("" for floating work).
fn when_line(l: &Lexicon, t: &Value) -> String {
    let Some(w) = t.get("when") else {
        return String::new();
    };
    let span = format!("{} - {}", s(w, "start"), s(w, "end"));
    match t.get("repeat") {
        Some(r) => format!("{span} · {}", freq_label(l, s(r, "freq"))),
        None => span,
    }
}

fn dates_line(l: &Lexicon, t: &Value) -> String {
    let mut parts = Vec::new();
    let when = when_line(l, t);
    if !when.is_empty() {
        parts.push(when);
    }
    if !s(t, "after").is_empty() {
        parts.push(format!("{} {}", l.kb_after, s(t, "after")));
    }
    if !s(t, "due").is_empty() {
        parts.push(format!("{} {}", l.kb_due, s(t, "due")));
    }
    let nb = s(t, "needed_by");
    if !nb.is_empty() && nb != s(t, "due") {
        parts.push(format!("{} {nb}", l.kb_needed));
    }
    parts.join(" · ")
}

fn flags_line(l: &Lexicon, t: &Value) -> String {
    strs(t, "flags")
        .iter()
        .map(|f| flag_label(l, f))
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Task id -> (badge, proposal) for every task a pending changeset touches
/// ("vote: → success (2/3)", §8); the first changeset wins.
pub(crate) fn vote_badges(l: &Lexicon, snap: &SurfaceSnapshot) -> BTreeMap<String, (String, i32)> {
    let mut out = BTreeMap::new();
    for p in snap.pending.iter().filter(|p| p.state == ProposalState::Proposed) {
        let ops = p.payload.get("ops").and_then(Value::as_array).into_iter().flatten();
        for op in ops {
            let id = s(op, "id");
            let what = match s(op, "act") {
                "state" => format!("→ {}", shown_label(l, s(op, "to"))),
                "set" => l.kb_vote_edit.to_string(),
                _ => continue,
            };
            out.entry(id.to_string()).or_insert_with(|| {
                (
                    format!("{}: {what} ({}/{})", l.kb_vote, p.approvals, p.threshold),
                    i32::try_from(p.id.0).unwrap_or(-1),
                )
            });
        }
    }
    out
}

fn card(
    l: &Lexicon,
    id: &str,
    t: &Value,
    spell: &BTreeMap<String, String>,
    votes: &BTreeMap<String, (String, i32)>,
    staged: Option<&str>,
) -> KbCard {
    let kind = s(t, "type");
    let folded = kind.trim().to_lowercase();
    let flags = strs(t, "flags");
    let shown = s(t, "shown");
    let (vote, vote_id) = votes.get(id).cloned().unwrap_or((String::new(), -1));
    KbCard {
        id: id.into(),
        short: short_id(id).into(),
        title: s(t, "title").into(),
        type_label: spell.get(&folded).map_or(kind, String::as_str).into(),
        type_slot: type_slot(kind).map_or(-1, i32::from),
        size: s(t, "size").into(),
        who: strs(t, "assignees").join(", ").into(),
        shown: shown.into(),
        badge: shown_label(l, shown).into(),
        bad: shown == "stuck" || flags.iter().any(|f| f == "overdue" || f == "date_conflict"),
        dates: dates_line(l, t).into(),
        flags: flags_line(l, t).into(),
        note: s(t, "note").into(),
        vote: vote.into(),
        vote_id,
        staged: staged
            .map(|to| format!("{} → {}", l.kb_in_basket, shown_label(l, to)))
            .unwrap_or_default()
            .into(),
        shadow: false,
    }
}

/// The three columns and the type legend.
#[derive(Default)]
pub(crate) struct Board {
    pub(crate) cols: [Vec<KbCard>; 3],
    pub(crate) legend: Vec<KbType>,
}

/// The board as the filters and the basket see it (§8): To do sorted
/// blocked / stuck / scheduled / later / open, each staged move as a
/// dashed shadow in its target column.
pub(crate) fn board(
    l: &Lexicon,
    feed: &KanbanFeed,
    filters: &BTreeSet<String>,
    basket: &Basket,
) -> Board {
    let all = tasks(&feed.snap);
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let filter: Vec<ViewFilter> = filters.iter().filter_map(|f| ViewFilter::parse(f).ok()).collect();
    let q = ViewQuery { filter, ..ViewQuery::default() };
    let listed = quests_view(&feed.snap, &q)
        .ok()
        .and_then(|v| v.get("tasks").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let votes = vote_badges(l, &feed.snap);
    let staged = basket.staged_moves();
    let mut out = Board::default();
    let mut todo: Vec<(u8, KbCard)> = Vec::new();
    let mut present: BTreeMap<String, (String, i32)> = BTreeMap::new();
    for t in &listed {
        let id = s(t, "id");
        let c = card(l, id, t, &spell, &votes, staged.get(id).map(String::as_str));
        if c.type_slot >= 0 {
            present.insert(s(t, "type").trim().to_lowercase(), (c.type_label.to_string(), c.type_slot));
        }
        match column_of(s(t, "state")) {
            0 => todo.push((todo_rank(s(t, "shown")), c)),
            col => out.cols[col].push(c),
        }
        if let Some(to) = staged.get(id) {
            let col = column_of(to);
            if col != column_of(s(t, "state")) {
                let mut shadow = card(l, id, t, &spell, &votes, Some(to));
                shadow.shadow = true;
                out.cols[col].insert(0, shadow);
            }
        }
    }
    // stable: the priority order holds within a rank
    todo.sort_by_key(|(rank, _)| *rank);
    let shadows: Vec<KbCard> = out.cols[0].drain(..).collect();
    out.cols[0] = shadows.into_iter().chain(todo.into_iter().map(|(_, c)| c)).collect();
    out.legend = present
        .into_iter()
        .map(|(key, (label, slot))| KbType {
            on: filters.contains(&format!("type:{key}")),
            key: key.into(),
            label: label.into(),
            slot,
        })
        .collect();
    out
}

/// Whether `to` is a legal transition for a task in `state` (§2.2).
pub(crate) fn legal_move(state: &str, timed_once: bool, series: bool, to: &str) -> bool {
    if series {
        return matches!((state, to), ("todo", "cancelled") | ("cancelled", "todo"));
    }
    match (state, to) {
        ("todo", "wip" | "cancelled") => true,
        ("todo", "success" | "fail") => timed_once,
        ("wip", "todo" | "success" | "fail" | "cancelled") => true,
        ("fail", "wip" | "cancelled" | "todo") => true,
        ("success" | "cancelled", "todo") => true,
        _ => false,
    }
}

/// A transition that needs a `note` (§2.2).
pub(crate) fn needs_note(to: &str) -> bool {
    matches!(to, "fail" | "cancelled")
}

/// The target of a card dropped on `col`, if the move is legal: done means
/// `success` (fail and cancel need a note - the drill-in).
pub(crate) fn drop_target(feed: &KanbanFeed, id: &str, col: usize) -> Option<&'static str> {
    let all = tasks(&feed.snap);
    let t = all.get(id)?;
    let to = match col {
        0 => "todo",
        1 => "wip",
        _ => "success",
    };
    let (state, timed_once, series) = shape(t);
    (column_of(state) != col && legal_move(state, timed_once, series, to)).then_some(to)
}

fn shape(t: &Value) -> (&str, bool, bool) {
    let series = t.get("repeat").is_some();
    (s(t, "state"), t.get("when").is_some() && !series, series)
}

fn move_label(l: &Lexicon, state: &str, to: &str) -> &'static str {
    match (state, to) {
        ("todo", "wip") => l.kb_mv_start,
        ("fail", "wip") => l.kb_mv_retry,
        ("wip", "todo") => l.kb_mv_pause,
        (_, "todo") => l.kb_mv_reopen,
        (_, "success") => l.kb_mv_succeed,
        (_, "fail") => l.kb_mv_fail,
        _ => l.kb_mv_cancel,
    }
}

/// Everything the drill-in shows of one task.
#[derive(Default)]
pub(crate) struct Detail {
    pub(crate) head: KbDetail,
    pub(crate) blocked_by: Vec<KbLine>,
    pub(crate) prereq_for: Vec<KbLine>,
    pub(crate) prereqs: Vec<KbLine>,
    pub(crate) checks: Vec<KbCheck>,
    pub(crate) scope: Vec<String>,
    pub(crate) moves: Vec<KbPick>,
}

fn task_line(l: &Lexicon, all: &Map<String, Value>, id: &str) -> KbLine {
    let t = all.get(id).cloned().unwrap_or(Value::Null);
    KbLine {
        key: id.into(),
        kind: "task".into(),
        glyph: String::new().into(),
        text: format!("{} {}", short_id(id), s(&t, "title")).into(),
        sub: shown_label(l, s(&t, "shown")).into(),
        bad: s(&t, "shown") == "stuck" || s(&t, "state") == "fail",
    }
}

/// The drill-in of `id` (§8): both link directions, its prerequisites to
/// any depth with succeeded/total, the acceptance checklist with evidence,
/// the legal moves.
pub(crate) fn detail(l: &Lexicon, feed: &KanbanFeed, basket: &Basket, id: &str) -> Option<Detail> {
    let all = tasks(&feed.snap);
    let t = all.get(id)?;
    let q = ViewQuery { task: Some(id.to_string()), ..ViewQuery::default() };
    let full = quests_view(&feed.snap, &q).ok()?.get("task").cloned()?;
    let spell = type_spellings(all.values().map(|t| s(t, "type")));
    let votes = vote_badges(l, &feed.snap);
    let staged = basket.staged_moves();
    let c = card(l, id, t, &spell, &votes, staged.get(id).map(String::as_str));
    let evidence = strs(&full, "evidence");
    let succeeded = full.get("progress").and_then(|p| p.get("succeeded")).and_then(Value::as_u64);
    let total = full.get("progress").and_then(|p| p.get("total")).and_then(Value::as_u64);
    let progress = match (succeeded, total) {
        (Some(d), Some(n)) if n > 0 => format!("{d}/{n} {}", l.kb_progress),
        _ => String::new(),
    };
    let (state, timed_once, series) = shape(t);
    let moves = ["wip", "todo", "success", "fail", "cancelled"]
        .into_iter()
        .filter(|to| legal_move(state, timed_once, series, to))
        .map(|to| KbPick { key: to.into(), label: move_label(l, state, to).into(), on: staged.get(id).is_some_and(|s| s == to) })
        .collect();
    Some(Detail {
        head: KbDetail {
            id: id.into(),
            short: c.short,
            title: c.title,
            type_label: c.type_label,
            type_slot: c.type_slot,
            size: c.size,
            who: c.who,
            creator: s(t, "creator").into(),
            badge: c.badge,
            bad: c.bad,
            dates: c.dates,
            flags: c.flags,
            when: when_line(l, t).into(),
            description: s(&full, "description").into(),
            note: s(t, "note").into(),
            progress: progress.into(),
            vote: c.vote,
            vote_id: c.vote_id,
            staged: c.staged,
        },
        blocked_by: strs(t, "blocked_by").iter().map(|b| task_line(l, &all, b)).collect(),
        prereq_for: strs(t, "prerequisite_for").iter().map(|b| task_line(l, &all, b)).collect(),
        prereqs: full
            .get("prerequisites")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| {
                let mut line = task_line(l, &all, s(p, "id"));
                let depth = p.get("depth").and_then(Value::as_u64).unwrap_or(1);
                line.text = format!("{}{}", "  ".repeat(usize::try_from(depth.saturating_sub(1)).unwrap_or(0)), line.text).into();
                line
            })
            .collect(),
        checks: strs(&full, "acceptance")
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let ev = evidence.get(i).cloned().unwrap_or_default();
                KbCheck { done: !ev.is_empty(), text: text.into(), evidence: ev.into() }
            })
            .collect(),
        scope: strs(&full, "out_of_scope"),
        moves,
    })
}

/// The local basket: acts staged, not yet proposed (§8). Persisted as
/// `kanban_draft.json` through the engine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Basket {
    #[serde(default)]
    pub(crate) acts: Vec<Value>,
    #[serde(default)]
    pub(crate) summary: String,
    /// The next `ref` an added task gets.
    #[serde(default)]
    pub(crate) next_ref: u32,
}

impl Basket {
    /// A stored draft; anything unreadable is an empty basket.
    pub(crate) fn from_draft(draft: &str) -> Basket {
        serde_json::from_str(draft).unwrap_or_default()
    }

    /// The stored form ("" = nothing to keep).
    pub(crate) fn to_draft(&self) -> String {
        if self.acts.is_empty() && self.summary.trim().is_empty() {
            return String::new();
        }
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Stage a move: it replaces an earlier move of the same task.
    pub(crate) fn stage_state(&mut self, id: &str, to: &str, note: &str, evidence: &[String]) {
        self.acts.retain(|a| !(s(a, "act") == "state" && s(a, "id") == id));
        let mut act = json!({ "act": "state", "id": id, "to": to });
        if !note.trim().is_empty() {
            act["note"] = Value::from(note.trim());
        }
        if to == "success" && evidence.iter().any(|e| !e.trim().is_empty()) {
            act["evidence"] = evidence.iter().map(|e| Value::from(e.trim())).collect();
        }
        self.acts.push(act);
    }

    /// Stage a new task, with a `ref` later acts may cite.
    pub(crate) fn add(&mut self, mut act: Value) {
        self.next_ref += 1;
        act["ref"] = Value::from(format!("n{}", self.next_ref));
        self.acts.push(act);
    }

    /// Task id -> the staged target state (the last move wins).
    pub(crate) fn staged_moves(&self) -> BTreeMap<String, String> {
        self.acts
            .iter()
            .filter(|a| s(a, "act") == "state")
            .map(|a| (s(a, "id").to_string(), s(a, "to").to_string()))
            .collect()
    }

    /// Append a declined changeset's acts (the `rescue_patch` idiom);
    /// returns how many came back.
    pub(crate) fn rescue(&mut self, payload: &Value) -> usize {
        let ops: Vec<Value> = payload.get("ops").and_then(Value::as_array).cloned().unwrap_or_default();
        let n = ops.len();
        self.acts.extend(ops);
        if self.summary.trim().is_empty() {
            self.summary = s(payload, "summary").to_string();
        }
        n
    }

    /// The changeset to propose.
    pub(crate) fn payload(&self, base_rev: u64, auto: &str) -> Value {
        let summary = if self.summary.trim().is_empty() { auto } else { self.summary.trim() };
        json!({ "op": "kanban_ops", "summary": summary, "base_rev": base_rev, "ops": self.acts })
    }
}

/// What a voter would see of the basket (§5.4), before proposing: the
/// acts rendered against the board, then would-void, impact and warnings.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BasketReview {
    pub(crate) acts: Vec<String>,
    pub(crate) advisories: Vec<String>,
}

/// Fake ids stand in for the ones the engine mints; they render as "new".
fn fake_id(k: u32) -> String {
    format!("{k:08x}{}", "f".repeat(24))
}

/// Review the basket on the folded board (the engine's own fold over the
/// same applied log, so precheck and impact match the propose door).
pub(crate) fn review(l: &Lexicon, feed: &KanbanFeed, basket: &Basket) -> BasketReview {
    if basket.acts.is_empty() {
        return BasketReview::default();
    }
    let seats: BTreeSet<String> = feed.seats.iter().cloned().collect();
    let board = kanban_fold(&feed.snap.applied, &seats);
    let payload = basket.payload(board.rev, "-");
    let mut k = 0u32;
    let canon = match kanban_canonicalize(&payload, &feed.me, &mut || {
        k += 1;
        Ok(fake_id(k))
    }) {
        Ok(c) => c,
        Err(e) => return BasketReview { acts: Vec::new(), advisories: vec![e] },
    };
    let rename = |line: String| -> String {
        (1..=k).fold(line, |acc, i| acc.replace(&short_id(&fake_id(i)), &format!("{} {i}", l.kb_new_ref)))
    };
    let acts: Vec<String> = render_acts(Some(&board), &canon.payload).into_iter().map(rename).collect();
    let today = today_of(feed);
    let mut lines = Vec::new();
    if let Err(f) = kanban_precheck(&board, &canon.payload, &seats) {
        lines.push(format!("{WOULD_VOID_PREFIX}{}", f.reason.0));
    }
    let before = derive(&board, today);
    lines.extend(advisories(&board, &before, &canon.payload, &seats, today, &|_| None));
    BasketReview { acts, advisories: lines.into_iter().map(rename).collect() }
}

/// The engine's UTC day, else this machine's.
fn today_of(feed: &KanbanFeed) -> NaiveDate {
    feed.snap
        .board
        .as_ref()
        .and_then(|b| b.get("today"))
        .and_then(Value::as_str)
        .and_then(parse_date)
        .unwrap_or_else(|| chrono::Utc::now().date_naive())
}

/// The headline a basket proposes under when nobody wrote one.
pub(crate) fn auto_summary(acts: &[String]) -> String {
    match acts {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (+{})", rest.len()),
    }
}

/// The New task form's content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Form {
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) size: String,
    pub(crate) assignees: Vec<String>,
    pub(crate) blocked: Vec<String>,
    pub(crate) due: String,
    pub(crate) after: String,
    /// 0 floating · 1 once · 2 recurring.
    pub(crate) mode: i32,
    pub(crate) start: String,
    pub(crate) end: String,
    pub(crate) freq: String,
    pub(crate) interval: String,
    pub(crate) until: String,
    pub(crate) count: String,
    pub(crate) desc: String,
    pub(crate) acceptance: Vec<String>,
    pub(crate) scope: Vec<String>,
}

fn non_empty(items: &[String]) -> Vec<String> {
    items.iter().map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect()
}

/// The `add` act the form describes, checked by the propose door's own
/// shape check; `Err` is its first reason.
pub(crate) fn form_act(f: &Form) -> Result<Value, String> {
    let mut act = Map::new();
    act.insert("act".into(), Value::from("add"));
    act.insert("title".into(), Value::from(f.title.trim()));
    act.insert("assignees".into(), f.assignees.iter().cloned().map(Value::from).collect());
    let opt = |act: &mut Map<String, Value>, k: &str, v: &str| {
        if !v.trim().is_empty() {
            act.insert(k.into(), Value::from(v.trim()));
        }
    };
    opt(&mut act, "type", &f.kind);
    opt(&mut act, "size", &f.size);
    opt(&mut act, "due", &f.due);
    opt(&mut act, "after", &f.after);
    opt(&mut act, "description", &f.desc);
    if !f.blocked.is_empty() {
        act.insert("blocked_by".into(), f.blocked.iter().cloned().map(Value::from).collect());
    }
    for (k, items) in [("acceptance", &f.acceptance), ("out_of_scope", &f.scope)] {
        let items = non_empty(items);
        if !items.is_empty() {
            act.insert(k.into(), items.into_iter().map(Value::from).collect());
        }
    }
    if f.mode > 0 {
        act.insert("when".into(), json!({ "start": f.start.trim(), "end": f.end.trim() }));
    }
    if f.mode == 2 {
        let mut r = Map::new();
        r.insert("freq".into(), Value::from(f.freq.as_str()));
        let num = |v: &str, what: &str| -> Result<Option<u64>, String> {
            if v.trim().is_empty() {
                return Ok(None);
            }
            v.trim().parse::<u64>().map(Some).map_err(|_| format!("{what}: not a number"))
        };
        if let Some(n) = num(&f.interval, "interval")? {
            r.insert("interval".into(), Value::from(n));
        }
        if let Some(n) = num(&f.count, "count")? {
            r.insert("count".into(), Value::from(n));
        }
        if !f.until.trim().is_empty() {
            r.insert("until".into(), Value::from(f.until.trim()));
        }
        act.insert("repeat".into(), Value::Object(r));
    }
    let act = Value::Object(act);
    validate_kanban_payload(&json!({ "op": "kanban_ops", "summary": "-", "base_rev": 0, "ops": [act.clone()] }))?;
    Ok(act)
}

/// The tasks the blocked-by picker offers: not a series, not closed for
/// good, matching the filter.
pub(crate) fn blocked_candidates(feed: &KanbanFeed, filter: &str, on: &[String]) -> Vec<KbPick> {
    let needle = filter.trim().to_lowercase();
    let mut out: Vec<KbPick> = tasks(&feed.snap)
        .iter()
        .filter(|(_, t)| t.get("repeat").is_none() && s(t, "state") != "cancelled")
        .filter(|(id, t)| {
            on.contains(*id)
                || needle.is_empty()
                || s(t, "title").to_lowercase().contains(&needle)
                || id.starts_with(needle.trim_start_matches('#'))
        })
        .map(|(id, t)| KbPick {
            key: id.as_str().into(),
            label: format!("{} {}", short_id(id), s(t, "title")).into(),
            on: on.contains(id),
        })
        .collect();
    out.sort_by_key(|p| !p.on);
    out
}

/// "Mine" (§8): the `read_actions` list first, then every other task the
/// seat is assigned to or created. (`next` is the task part of the list.)
pub(crate) fn mine(l: &Lexicon, feed: &KanbanFeed) -> (Vec<KbLine>, Vec<KbLine>) {
    let all = tasks(&feed.snap);
    let title = |id: &str| all.get(id).map(|t| s(t, "title").to_string()).unwrap_or_default();
    let mut listed = BTreeSet::new();
    let actions: Vec<KbLine> = feed
        .actions
        .iter()
        .map(|a| match a {
            WakeAction::Vote { proposal, surface, .. } => {
                let summary = feed
                    .snap
                    .pending
                    .iter()
                    .find(|p| p.id.0 == *proposal)
                    .map(|p| s(&p.payload, "summary").to_string())
                    .unwrap_or_default();
                KbLine {
                    key: proposal.to_string().into(),
                    kind: "vote".into(),
                    glyph: "🗳️".into(),
                    text: format!("#{proposal} {summary}").trim_end().into(),
                    sub: format!("{} · {}", l.kb_a_vote, surface.as_str()).into(),
                    bad: false,
                }
            }
            WakeAction::TaskStart { task, occurrence, late } => {
                listed.insert(task.clone());
                let mut sub = l.kb_a_start.to_string();
                if let Some(o) = occurrence {
                    sub = format!("{sub} {o}");
                }
                if *late {
                    sub = format!("{sub} · {}", l.kb_a_late);
                }
                KbLine { key: task.as_str().into(), kind: "task".into(), glyph: "⏰".into(), text: title(task).into(), sub: sub.into(), bad: *late }
            }
            WakeAction::TaskWip { task } => {
                listed.insert(task.clone());
                KbLine { key: task.as_str().into(), kind: "task".into(), glyph: "🔨".into(), text: title(task).into(), sub: l.kb_a_wip.into(), bad: false }
            }
            WakeAction::TaskStartable { task } => {
                listed.insert(task.clone());
                KbLine { key: task.as_str().into(), kind: "task".into(), glyph: "▶️".into(), text: title(task).into(), sub: l.kb_a_startable.into(), bad: false }
            }
            WakeAction::Poke { by, .. } => KbLine {
                key: by.as_str().into(),
                kind: "poke".into(),
                glyph: "👉".into(),
                text: by.as_str().into(),
                sub: l.kb_a_poke.into(),
                bad: false,
            },
        })
        .collect();
    let listed_view = quests_view(&feed.snap, &ViewQuery::default())
        .ok()
        .and_then(|v| v.get("tasks").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let rest = listed_view
        .iter()
        .filter(|t| {
            !listed.contains(s(t, "id"))
                && (strs(t, "assignees").contains(&feed.me) || s(t, "creator") == feed.me)
        })
        .map(|t| task_line(l, &all, s(t, "id")))
        .collect();
    (actions, rest)
}

/// The declined kanban changesets a member may rescue into the basket.
pub(crate) fn declined(feed: &KanbanFeed) -> Vec<KbLine> {
    feed.snap
        .declined
        .iter()
        .filter(|p| s(&p.payload, "op") == "kanban_ops")
        .map(|p| KbLine {
            key: p.id.0.to_string().into(),
            kind: "vote".into(),
            glyph: "🚫".into(),
            text: s(&p.payload, "summary").into(),
            sub: format!("#{} · {}", p.id.0, p.by).into(),
            bad: false,
        })
        .collect()
}

/// The coalesced notifications (§8): every trigger the wake knows, under
/// the same `wake_on` switches, one toast per burst.
#[derive(Default)]
pub(crate) struct Notices {
    reasons: BTreeSet<&'static str>,
    pokers: Vec<String>,
    seen_starts: BTreeSet<String>,
}

impl Notices {
    /// Mark one trigger; `true` when it armed an empty batch (start the
    /// coalescing timer).
    pub(crate) fn note(&mut self, reason: &'static str, wake_on: &[String]) -> bool {
        if !wake_on.iter().any(|r| r == reason) {
            return false;
        }
        let armed = self.reasons.is_empty();
        self.reasons.insert(reason);
        armed
    }

    /// A poke to this seat, with who poked.
    pub(crate) fn note_poke(&mut self, by: &str, wake_on: &[String]) -> bool {
        let armed = self.note("poked", wake_on);
        if self.reasons.contains("poked") && !self.pokers.iter().any(|p| p == by) {
            self.pokers.push(by.to_string());
        }
        armed
    }

    /// Appointment starts not toasted yet: one `task_start` trigger.
    pub(crate) fn note_starts(&mut self, actions: &[WakeAction], wake_on: &[String]) -> bool {
        let mut fresh = false;
        for a in actions {
            if let WakeAction::TaskStart { task, occurrence, .. } = a {
                let key = format!("{task}@{}", occurrence.as_deref().unwrap_or_default());
                fresh |= self.seen_starts.insert(key);
            }
        }
        fresh && self.note("task_start", wake_on)
    }


    /// The batch as one toast, emptied.
    pub(crate) fn take(&mut self, l: &Lexicon, poked: &str) -> Option<String> {
        let reasons = std::mem::take(&mut self.reasons);
        let pokers = std::mem::take(&mut self.pokers);
        let mut parts = Vec::new();
        if reasons.contains("poked") && !pokers.is_empty() {
            parts.push(format!("{} {poked}", pokers.join(", ")));
        }
        for (r, text) in [("vote_pending", l.kb_n_vote), ("kanban", l.kb_n_kanban), ("task_start", l.kb_n_start)] {
            if reasons.contains(r) {
                parts.push(text.to_string());
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}
