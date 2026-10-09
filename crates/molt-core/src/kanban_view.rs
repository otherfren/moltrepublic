//! The kanban read an agent works from (`docs/kanban/kanban_workflows.md`
//! §6 `quests_view`, §8 filters): the Quests snapshot narrowed to a seat,
//! a task, a proposal or a calendar window. Display only; `today` and the
//! board's derived status come from the snapshot.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Map, Value};

use crate::kanban_calendar::{
    expand, fmt_date, parse_date, parse_datetime, Repeat, When,
};
use crate::kanban_fold::{Size, TaskState};
use crate::kanban_review::render_acts;
use crate::{ProposalState, ProposalView, SurfaceSnapshot, VoteState};

/// A closed task stays in the default list for this many changesets.
pub const RECENT_CLOSED_REVS: u64 = 10;

/// The longest calendar window one read expands, in days (inclusive).
pub const MAX_WINDOW_DAYS: i64 = 366;

/// The fields that ride `task`, never the list.
const TEXT_FIELDS: [&str; 4] = ["description", "acceptance", "out_of_scope", "evidence"];

/// One §8 filter, by its wire name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewFilter {
    /// The seat is an assignee.
    Mine,
    /// The seat's `next` list.
    ToActOn,
    /// The seat's timed task whose start passed within a day, still `todo`.
    StartingNow,
    /// The seat created it.
    CreatedByMe,
    /// A pending changeset the seat has not voted on touches it.
    NeedsMyVote,
    /// Derived `blocked`.
    Blocked,
    /// Derived `stuck`.
    Stuck,
    /// Flag `overdue`.
    Overdue,
    /// Flag `date_conflict`.
    DateConflict,
    /// `success`, `fail` and `cancelled`.
    Closed,
    /// One governed state.
    State(TaskState),
    /// No `when`.
    Floating,
    /// A `when`.
    Timed,
    /// One size.
    Size(Size),
    /// One type, compared trimmed and case-folded.
    Type(String),
}

impl ViewFilter {
    /// The vocabulary, as a refusal names it.
    pub const NAMES: &'static str = "mine, to_act_on, starting_now, created_by_me, needs_my_vote, \
blocked, stuck, overdue, date_conflict, closed, todo, wip, success, fail, cancelled, floating, \
timed, size:<XS|S|M|L|XL|XXL>, type:<label>";

    /// Parse one wire name.
    ///
    /// # Errors
    /// An unknown name, size or an empty type.
    pub fn parse(s: &str) -> Result<ViewFilter, String> {
        let unknown = || format!("unknown filter {s} - one of {}", Self::NAMES);
        if let Some(size) = s.strip_prefix("size:") {
            return Size::parse(size).map(ViewFilter::Size).ok_or_else(unknown);
        }
        if let Some(label) = s.strip_prefix("type:") {
            let folded = fold_label(label);
            return if folded.is_empty() {
                Err(unknown())
            } else {
                Ok(ViewFilter::Type(folded))
            };
        }
        if let Some(state) = TaskState::parse(s) {
            return Ok(ViewFilter::State(state));
        }
        Ok(match s {
            "mine" => ViewFilter::Mine,
            "to_act_on" => ViewFilter::ToActOn,
            "starting_now" => ViewFilter::StartingNow,
            "created_by_me" => ViewFilter::CreatedByMe,
            "needs_my_vote" => ViewFilter::NeedsMyVote,
            "blocked" => ViewFilter::Blocked,
            "stuck" => ViewFilter::Stuck,
            "overdue" => ViewFilter::Overdue,
            "date_conflict" => ViewFilter::DateConflict,
            "closed" => ViewFilter::Closed,
            "floating" => ViewFilter::Floating,
            "timed" => ViewFilter::Timed,
            _ => return Err(unknown()),
        })
    }

    fn is_state(&self) -> bool {
        matches!(self, ViewFilter::Closed | ViewFilter::State(_))
    }
}

fn fold_label(s: &str) -> String {
    s.trim().to_lowercase()
}

/// What one `quests_view` asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewQuery {
    /// Whose view: the seat-relative filters and `next` (default: the reader).
    pub seat: Option<String>,
    /// One task, by id or unique prefix (`#` allowed).
    pub task: Option<String>,
    /// One kanban proposal: the review read.
    pub proposal: Option<u64>,
    /// Calendar window start.
    pub from: Option<NaiveDate>,
    /// Calendar window end, inclusive.
    pub to: Option<NaiveDate>,
    /// §8 filters: OR within states, time modes, sizes and types, AND across.
    pub filter: Vec<ViewFilter>,
}

impl ViewQuery {
    /// The calendar window, if one was asked for.
    ///
    /// # Errors
    /// One end without the other, `from > to`, or more than
    /// [`MAX_WINDOW_DAYS`] days.
    pub fn window(&self) -> Result<Option<(NaiveDate, NaiveDate)>, String> {
        match (self.from, self.to) {
            (None, None) => Ok(None),
            (Some(from), Some(to)) => {
                if from > to {
                    Err("from is after to".to_string())
                } else if (to - from).num_days() + 1 > MAX_WINDOW_DAYS {
                    Err(format!("window over {MAX_WINDOW_DAYS} days"))
                } else {
                    Ok(Some((from, to)))
                }
            }
            _ => Err("from and to go together".to_string()),
        }
    }
}

/// A task's calendar placement, parsed back from its board JSON.
struct Timing {
    when: When,
    repeat: Option<Repeat>,
    skip: Vec<NaiveDate>,
    moved: BTreeMap<NaiveDate, When>,
}

fn timing(t: &Value) -> Option<Timing> {
    let when = When::parse(t.get("when")?).ok()?;
    let repeat = t.get("repeat").and_then(|r| Repeat::parse(r).ok());
    let skip = t
        .get("skip")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| d.as_str().and_then(parse_date))
        .collect();
    let moved = t
        .get("moved")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, w)| Some((parse_date(k)?, When::parse(w).ok()?)))
        .collect();
    Some(Timing { when, repeat, skip, moved })
}

fn str_of<'a>(t: &'a Value, key: &str) -> &'a str {
    t.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn has_str(t: &Value, key: &str, want: &str) -> bool {
    t.get(key)
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(want)))
}

fn state_of(t: &Value) -> Option<TaskState> {
    TaskState::parse(str_of(t, "state"))
}

/// What the seat-relative filters read.
struct Ctx<'a> {
    seat: &'a str,
    now: Option<NaiveDateTime>,
    next: BTreeSet<&'a str>,
    voting: BTreeSet<String>,
}

impl Ctx<'_> {
    fn starting_now(&self, t: &Value) -> bool {
        let Some(now) = self.now else {
            return false;
        };
        if !has_str(t, "assignees", self.seat) || str_of(t, "shown") != "scheduled" {
            return false;
        }
        let Some(tm) = timing(t) else {
            return false;
        };
        let since = now - chrono::TimeDelta::days(1);
        let from = since.date();
        expand(&tm.when, tm.repeat.as_ref(), &tm.skip, &tm.moved, from, now.date())
            .iter()
            .any(|o| o.window.begins() > since && o.window.begins() <= now)
    }

    fn keeps(&self, id: &str, t: &Value, f: &ViewFilter) -> bool {
        match f {
            ViewFilter::Mine => has_str(t, "assignees", self.seat),
            ViewFilter::ToActOn => self.next.contains(id),
            ViewFilter::StartingNow => self.starting_now(t),
            ViewFilter::CreatedByMe => str_of(t, "creator") == self.seat,
            ViewFilter::NeedsMyVote => self.voting.contains(id),
            ViewFilter::Blocked => str_of(t, "shown") == "blocked",
            ViewFilter::Stuck => str_of(t, "shown") == "stuck" || has_str(t, "flags", "stuck"),
            ViewFilter::Overdue => has_str(t, "flags", "overdue"),
            ViewFilter::DateConflict => has_str(t, "flags", "date_conflict"),
            ViewFilter::Closed => state_of(t).is_some_and(TaskState::is_terminal),
            ViewFilter::State(s) => state_of(t) == Some(*s),
            ViewFilter::Floating => t.get("when").is_none(),
            ViewFilter::Timed => t.get("when").is_some(),
            ViewFilter::Size(s) => str_of(t, "size") == s.as_str(),
            ViewFilter::Type(label) => fold_label(str_of(t, "type")) == *label,
        }
    }

    /// The filters, OR within a group and AND across.
    fn matches(&self, id: &str, t: &Value, filters: &[ViewFilter]) -> bool {
        let group = |f: &ViewFilter| -> u8 {
            match f {
                ViewFilter::Closed | ViewFilter::State(_) => 1,
                ViewFilter::Floating | ViewFilter::Timed => 2,
                ViewFilter::Size(_) => 3,
                ViewFilter::Type(_) => 4,
                _ => 0,
            }
        };
        let mut any: BTreeMap<u8, bool> = BTreeMap::new();
        for f in filters {
            let hit = self.keeps(id, t, f);
            match group(f) {
                0 if !hit => return false,
                0 => {}
                g => *any.entry(g).or_insert(false) |= hit,
            }
        }
        any.values().all(|hit| *hit)
    }
}

/// Pending changesets awaiting `seat`'s vote: the task ids they touch.
fn awaiting(pending: &[ProposalView], seat: &str, me: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in pending.iter().filter(|p| p.state == ProposalState::Proposed) {
        let open = match p.votes.iter().find(|v| v.member == seat) {
            Some(v) => v.vote == VoteState::Open,
            None => seat == me && !p.approved_by_me && !p.declined_by_me,
        };
        if !open {
            continue;
        }
        let ops = p.payload.get("ops").and_then(Value::as_array).into_iter().flatten();
        out.extend(
            ops.filter(|op| op.get("act").and_then(Value::as_str) != Some("add"))
                .filter_map(|op| op.get("id").and_then(Value::as_str))
                .map(str::to_string),
        );
    }
    out
}

fn header(id: &str, t: &Value) -> Value {
    let mut m = t.as_object().cloned().unwrap_or_default();
    for k in TEXT_FIELDS {
        m.remove(k);
    }
    m.insert("id".into(), Value::from(id));
    Value::Object(m)
}

/// The id a caller named: a full id or a unique prefix, `#` allowed.
fn resolve<'a>(tasks: &'a Map<String, Value>, named: &str) -> Result<&'a str, String> {
    let prefix = named.trim().trim_start_matches('#');
    if prefix.is_empty() {
        return Err("unknown task".to_string());
    }
    if let Some((id, _)) = tasks.get_key_value(prefix) {
        return Ok(id);
    }
    let mut hits = tasks.keys().filter(|id| id.starts_with(prefix));
    match (hits.next(), hits.next()) {
        (Some(id), None) => Ok(id),
        (Some(_), Some(_)) => Err(format!("ambiguous task {named}")),
        _ => Err(format!("unknown task {named}")),
    }
}

/// The task with its text, its prerequisites to any depth (breadth first,
/// each once, with its depth) and how many of them succeeded.
fn task_detail(tasks: &Map<String, Value>, id: &str) -> Value {
    let t = tasks.get(id).cloned().unwrap_or(Value::Null);
    let mut out = t.as_object().cloned().unwrap_or_default();
    out.insert("id".into(), Value::from(id));
    let mut seen = BTreeSet::from([id.to_string()]);
    let mut level: Vec<String> = vec![id.to_string()];
    let mut pre = Vec::new();
    let mut succeeded = 0u64;
    let mut depth = 0u64;
    while !level.is_empty() {
        depth += 1;
        let mut next = Vec::new();
        for parent in &level {
            let blocked_by = tasks
                .get(parent)
                .and_then(|p| p.get("blocked_by"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            for b in blocked_by {
                if !seen.insert(b.to_string()) {
                    continue;
                }
                let bt = tasks.get(b).cloned().unwrap_or(Value::Null);
                if state_of(&bt) == Some(TaskState::Success) {
                    succeeded += 1;
                }
                let mut row = Map::new();
                row.insert("id".into(), Value::from(b));
                row.insert("title".into(), Value::from(str_of(&bt, "title")));
                row.insert("state".into(), Value::from(str_of(&bt, "state")));
                row.insert("shown".into(), Value::from(str_of(&bt, "shown")));
                row.insert("depth".into(), Value::from(depth));
                pre.push(Value::Object(row));
                next.push(b.to_string());
            }
        }
        level = next;
    }
    let total = u64::try_from(pre.len()).unwrap_or(u64::MAX);
    out.insert("prerequisites".into(), Value::Array(pre));
    let mut progress = Map::new();
    progress.insert("succeeded".into(), Value::from(succeeded));
    progress.insert("total".into(), Value::from(total));
    out.insert("progress".into(), Value::Object(progress));
    Value::Object(out)
}

/// Every occurrence of the kept, not cancelled timed tasks in `[from, to]`.
fn calendar(
    tasks: &Map<String, Value>,
    keep: &dyn Fn(&str, &Value) -> bool,
    from: NaiveDate,
    to: NaiveDate,
) -> Value {
    let mut rows: Vec<(NaiveDateTime, String, Value)> = Vec::new();
    for (id, t) in tasks {
        if state_of(t) == Some(TaskState::Cancelled) || !keep(id, t) {
            continue;
        }
        let Some(tm) = timing(t) else {
            continue;
        };
        for o in expand(&tm.when, tm.repeat.as_ref(), &tm.skip, &tm.moved, from, to) {
            let mut row = Map::new();
            row.insert("task".into(), Value::from(id.as_str()));
            row.insert("title".into(), Value::from(str_of(t, "title")));
            if let Some(kind) = t.get("type") {
                row.insert("type".into(), kind.clone());
            }
            if let Some(w) = o.window.to_json().as_object() {
                row.extend(w.clone());
            }
            if tm.repeat.is_some() {
                row.insert("occurrence".into(), Value::from(fmt_date(o.date)));
            }
            if o.moved {
                row.insert("moved".into(), Value::Bool(true));
            }
            row.insert("shown".into(), Value::from(str_of(t, "shown")));
            rows.push((o.window.begins(), id.clone(), Value::Object(row)));
        }
    }
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Value::Array(rows.into_iter().map(|(_, _, r)| r).collect())
}

/// The review read of one kanban proposal: its acts rendered, the
/// advisories split into would-void, changed-since and impact.
fn proposal_view(snap: &SurfaceSnapshot, id: u64) -> Result<Value, String> {
    let p = snap
        .pending
        .iter()
        .chain(&snap.accepted)
        .chain(&snap.declined)
        .find(|p| p.id.0 == id)
        .ok_or_else(|| format!("unknown kanban proposal {id}"))?;
    let strs = |v: Vec<String>| Value::Array(v.into_iter().map(Value::String).collect());
    let mut would_void = Vec::new();
    let mut changed = Vec::new();
    let mut impact = Vec::new();
    for line in &p.advisories {
        if let Some(r) = line.strip_prefix("would void now: ") {
            would_void.push(r.to_string());
        } else if let Some(r) = line.strip_prefix("changed since proposed: ") {
            changed.push(r.to_string());
        } else {
            impact.push(line.clone());
        }
    }
    let acts = if p.rendered.is_empty() {
        render_acts(None, &p.payload)
    } else {
        p.rendered.clone()
    };
    let state = if p.withdrawn {
        "withdrawn"
    } else {
        match p.state {
            ProposalState::Proposed => "proposed",
            ProposalState::Applied => "applied",
            ProposalState::Rejected => "rejected",
        }
    };
    let mut m = Map::new();
    m.insert("id".into(), Value::from(id));
    m.insert("state".into(), Value::from(state));
    m.insert("by".into(), Value::from(p.by.clone()));
    m.insert("approvals".into(), Value::from(p.approvals));
    m.insert("threshold".into(), Value::from(p.threshold));
    for key in ["summary", "base_rev", "ops"] {
        if let Some(v) = p.payload.get(key) {
            m.insert(key.into(), v.clone());
        }
    }
    m.insert("acts".into(), strs(acts));
    m.insert("would_void".into(), strs(would_void));
    m.insert("changed_since".into(), strs(changed));
    m.insert("impact".into(), strs(impact));
    if let Some(v) = &p.void {
        m.insert("void".into(), Value::from(v.clone()));
    }
    Ok(Value::Object(m))
}

/// `quests_view` (§6): with `task` or `proposal` only that part, else the
/// tasks (open work in priority order, then recently closed unless a
/// state filter says otherwise) and `next`; a window adds the calendar.
///
/// # Errors
/// An unknown or ambiguous task, an unknown proposal, a bad window.
pub fn quests_view(snap: &SurfaceSnapshot, q: &ViewQuery) -> Result<Value, String> {
    let empty = Value::Object(Map::new());
    let board = snap.board.as_ref().unwrap_or(&empty);
    let no_tasks = Map::new();
    let tasks = board.get("tasks").and_then(Value::as_object).unwrap_or(&no_tasks);
    let rev = board.get("rev").and_then(Value::as_u64).unwrap_or(0);
    let me = str_of(board, "me");
    let seat = q.seat.as_deref().unwrap_or(me);
    let window = q.window()?;

    let mut out = Map::new();
    out.insert("rev".into(), Value::from(rev));
    for key in ["today", "now"] {
        if let Some(v) = board.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    out.insert("seat".into(), Value::from(seat));

    if let Some(named) = &q.task {
        let id = resolve(tasks, named)?;
        out.insert("task".into(), task_detail(tasks, id));
    }
    if let Some(p) = q.proposal {
        out.insert("proposal".into(), proposal_view(snap, p)?);
    }

    let next_of = |s: &str| -> Value {
        board
            .get("next")
            .and_then(|n| n.get(s))
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()))
    };
    let ctx = Ctx {
        seat,
        now: parse_datetime(str_of(board, "now")),
        next: board
            .get("next")
            .and_then(|n| n.get(seat))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect(),
        voting: awaiting(&snap.pending, seat, me),
    };
    let keep = |id: &str, t: &Value| ctx.matches(id, t, &q.filter);

    if q.task.is_none() && q.proposal.is_none() {
        let stated = q.filter.iter().any(ViewFilter::is_state);
        let listed = |id: &str, t: &Value| -> bool {
            if !keep(id, t) {
                return false;
            }
            stated
                || match state_of(t) {
                    Some(s) if s.is_terminal() => {
                        t.get("touched_rev").and_then(Value::as_u64).unwrap_or(0)
                            + RECENT_CLOSED_REVS
                            > rev
                    }
                    _ => true,
                }
        };
        let priority: Vec<&str> = board
            .get("priority")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut order: Vec<&str> = priority.iter().copied().filter(|id| tasks.contains_key(*id)).collect();
        let ranked: BTreeSet<&str> = order.iter().copied().collect();
        let mut open: Vec<&str> = Vec::new();
        let mut closed: Vec<(u64, &str)> = Vec::new();
        for (id, t) in tasks {
            if ranked.contains(id.as_str()) {
                continue;
            }
            match state_of(t) {
                Some(s) if s.is_terminal() => closed.push((
                    t.get("touched_rev").and_then(Value::as_u64).unwrap_or(0),
                    id.as_str(),
                )),
                _ => open.push(id.as_str()),
            }
        }
        closed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        order.extend(open);
        order.extend(closed.into_iter().map(|(_, id)| id));
        let list: Vec<Value> = order
            .into_iter()
            .filter_map(|id| tasks.get(id).map(|t| (id, t)))
            .filter(|(id, t)| listed(id, t))
            .map(|(id, t)| header(id, t))
            .collect();
        out.insert("tasks".into(), Value::Array(list));
        let next = if q.seat.is_some() {
            next_of(seat)
        } else {
            board.get("next").cloned().unwrap_or_else(|| Value::Object(Map::new()))
        };
        out.insert("next".into(), next);
    }
    if let Some((from, to)) = window {
        out.insert("calendar".into(), calendar(tasks, &keep, from, to));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests;
