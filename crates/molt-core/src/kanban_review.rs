//! What a voter and a reader see of the board (`docs/kanban/kanban_workflows.md`
//! §4.5, §5.4, §6): the advisory lines on a pending changeset and the
//! board as one read. Display only, never consensus input; `today` is the
//! reader's UTC date.

use std::collections::BTreeSet;

use chrono::NaiveDate;
use serde_json::{Map, Value};

use crate::kanban_calendar::fmt_date;
use crate::kanban_dates::{derive, impact, Derived, Flag, ImpactChange, Shown};
use crate::kanban_fold::{kanban_fold_one, kanban_precheck, short_id, BoardState, TaskState};

impl Shown {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Shown::Stuck => "stuck",
            Shown::Blocked => "blocked",
            Shown::Scheduled => "scheduled",
            Shown::Later => "later",
            Shown::Open => "open",
            Shown::Wip => "wip",
            Shown::Success => "success",
            Shown::Fail => "fail",
            Shown::Cancelled => "cancelled",
        }
    }
}

impl Flag {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Flag::Overdue => "overdue",
            Flag::DateConflict => "date_conflict",
            Flag::Stuck => "stuck",
            Flag::StartedOnReopened => "started_on_reopened",
        }
    }
}

fn day(d: Option<NaiveDate>) -> String {
    d.map_or_else(|| "none".to_string(), fmt_date)
}

/// The advisory lines on a pending kanban changeset (§4.5): would it void
/// now, which of its tasks moved since `base_rev`, its impact on the
/// derived dates (§5.4) and missing content (§2.1.1). `before` is
/// `derive(board, today)`; `proposal_of_rev` names the proposal that
/// produced a fold revision, where known. Empty for any other payload.
#[must_use]
pub fn advisories(
    board: &BoardState,
    before: &Derived,
    payload: &Value,
    seats: &BTreeSet<String>,
    today: NaiveDate,
    proposal_of_rev: &dyn Fn(u64) -> Option<u64>,
) -> Vec<String> {
    if payload.get("op").and_then(Value::as_str) != Some("kanban_ops") {
        return Vec::new();
    }
    let ops: &[Value] = payload
        .get("ops")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let base_rev = payload.get("base_rev").and_then(Value::as_u64).unwrap_or(0);
    let mut out = Vec::new();

    let after = match kanban_precheck(board, payload, seats) {
        Err(fault) => {
            let by = fault
                .task
                .and_then(|id| board.tasks.get(&id))
                .filter(|t| t.touched_rev > base_rev)
                .and_then(|t| proposal_of_rev(t.touched_rev))
                .map(|p| format!(" (proposal {p})"))
                .unwrap_or_default();
            out.push(format!("would void now: {}{by}", fault.reason));
            None
        }
        Ok(()) => {
            let mut after = board.clone();
            kanban_fold_one(&mut after, payload, seats);
            Some(after)
        }
    };

    let mut changed: Vec<(&str, Vec<&str>)> = Vec::new();
    for op in ops {
        let Some(id) = op.get("id").and_then(Value::as_str) else {
            continue;
        };
        let names: Vec<&str> = match op.get("act").and_then(Value::as_str) {
            Some("set") => op
                .get("fields")
                .and_then(Value::as_object)
                .map(|f| f.keys().map(String::as_str).collect())
                .unwrap_or_default(),
            Some("state") => vec!["state"],
            _ => continue,
        };
        if !board.tasks.get(id).is_some_and(|t| t.touched_rev > base_rev) {
            continue;
        }
        match changed.iter_mut().find(|(i, _)| *i == id) {
            Some((_, list)) => list.extend(names),
            None => changed.push((id, names)),
        }
    }
    for (id, mut names) in changed {
        names.sort_unstable();
        names.dedup();
        let by = board
            .tasks
            .get(id)
            .and_then(|t| proposal_of_rev(t.touched_rev))
            .map(|p| format!(" (proposal {p})"))
            .unwrap_or_default();
        out.push(format!(
            "changed since proposed: {} {}{by}",
            short_id(id),
            names.join(", ")
        ));
    }

    if let Some(after) = &after {
        let line = impact_line(board, after, before, &derive(after, today));
        if !line.is_empty() {
            out.push(line);
        }
    }

    for op in ops {
        let Some(id) = op.get("id").and_then(Value::as_str) else {
            continue;
        };
        let sid = short_id(id);
        match op.get("act").and_then(Value::as_str) {
            Some("add") if op.get("when").is_none() => {
                if op.get("description").is_none() {
                    out.push(format!("{sid}: no description"));
                }
                if op.get("acceptance").is_none() {
                    out.push(format!("{sid}: no acceptance criteria"));
                }
            }
            Some("state")
                if op.get("to").and_then(Value::as_str) == Some(TaskState::Success.as_str()) =>
            {
                let evidence: Vec<&str> = op
                    .get("evidence")
                    .and_then(Value::as_array)
                    .map(|e| e.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let criteria = after
                    .as_ref()
                    .unwrap_or(board)
                    .tasks
                    .get(id)
                    .map_or(0, |t| t.acceptance.len());
                let met = evidence
                    .iter()
                    .take(criteria)
                    .filter(|e| !e.trim().is_empty())
                    .count();
                if (criteria > 0 || !evidence.is_empty())
                    && (evidence.len() != criteria || met != criteria)
                {
                    out.push(format!("{sid}: evidence for {met} of {criteria} criteria"));
                }
            }
            _ => {}
        }
    }
    out
}

/// §5.4 on one line: tasks with the same change share a segment,
/// segments joined by " · ", the task list left out when it repeats.
fn impact_line(
    board: &BoardState,
    after: &BoardState,
    before: &Derived,
    derived_after: &Derived,
) -> String {
    // (tasks, is a needed_by move, the change)
    let mut segments: Vec<(Vec<String>, bool, String)> = Vec::new();
    for change in impact(before, derived_after) {
        let (task, moved, part) = match change {
            ImpactChange::NeededBy { task, from, to } => {
                (task, true, format!("{} → {}", day(from), day(to)))
            }
            ImpactChange::Flag {
                task,
                flag,
                on: false,
            } => (task, false, format!("{} resolved", flag.as_str())),
            ImpactChange::Flag {
                task,
                flag,
                on: true,
            } => {
                let due = (flag == Flag::DateConflict)
                    .then(|| after.tasks.get(&task).and_then(|t| t.due))
                    .flatten()
                    .map(|d| format!(" (due {})", fmt_date(d)))
                    .unwrap_or_default();
                (task, false, format!("{}{due}", flag.as_str()))
            }
        };
        match segments
            .iter_mut()
            .find(|(_, m, p)| *m == moved && *p == part)
        {
            Some((tasks, _, _)) => tasks.push(task),
            None => segments.push((vec![task], moved, part)),
        }
    }
    let name = |id: &String| {
        let title = after
            .tasks
            .get(id)
            .or_else(|| board.tasks.get(id))
            .map(|t| format!(" {}", t.title))
            .unwrap_or_default();
        format!("{}{title}", short_id(id))
    };
    let mut prev: Option<&Vec<String>> = None;
    let mut said_needed_by = false;
    let mut parts = Vec::new();
    for (tasks, moved, part) in &segments {
        let lead = if *moved && !said_needed_by {
            "needed by "
        } else {
            ""
        };
        said_needed_by |= *moved;
        if prev == Some(tasks) {
            parts.push(format!("{lead}{part}"));
        } else {
            let names: Vec<String> = tasks.iter().map(name).collect();
            parts.push(format!("{}: {lead}{part}", names.join(", ")));
        }
        prev = Some(tasks);
    }
    parts.join(" · ")
}

/// The board as one read (§6): every task with its derived status, the
/// priority order and `next` per seat, for `today`.
#[must_use]
pub fn board_view(board: &BoardState, derived: &Derived, today: NaiveDate) -> Value {
    let strs = |v: &[String]| Value::Array(v.iter().cloned().map(Value::String).collect());
    let mut tasks = Map::new();
    for (id, t) in &board.tasks {
        let mut v = t.to_json();
        if let (Some(m), Some(d)) = (v.as_object_mut(), derived.tasks.get(id)) {
            let flags: Vec<String> = [
                (d.overdue, Flag::Overdue),
                (d.date_conflict, Flag::DateConflict),
                (d.stuck, Flag::Stuck),
                (d.started_on_reopened, Flag::StartedOnReopened),
            ]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, f)| f.as_str().to_string())
            .collect();
            m.insert("shown".into(), Value::from(d.shown.as_str()));
            if let Some(nb) = d.needed_by {
                m.insert("needed_by".into(), Value::from(fmt_date(nb)));
            }
            m.insert("flags".into(), strs(&flags));
            m.insert("prerequisite_for".into(), strs(&d.prerequisite_for));
        }
        tasks.insert(id.clone(), v);
    }
    let mut next = Map::new();
    for (seat, ids) in &derived.next {
        next.insert(seat.clone(), strs(ids));
    }
    let mut m = Map::new();
    m.insert("next".into(), Value::Object(next));
    m.insert("priority".into(), strs(&derived.priority));
    m.insert("rev".into(), Value::from(board.rev));
    m.insert("tasks".into(), Value::Object(tasks));
    m.insert("today".into(), Value::from(fmt_date(today)));
    Value::Object(m)
}

#[cfg(test)]
mod tests;
