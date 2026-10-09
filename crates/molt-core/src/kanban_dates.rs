//! Deadlines and order, derived on read (`docs_archive/kanban/kanban_workflows.md`
//! §5): `needed_by`, priority, the flags and `next`. Never consensus state -
//! what the board means on `today`, which the caller passes (UTC).
//! Reads dates and links only; `size` never enters it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;

use crate::kanban_fold::{BoardState, Task, TaskId, TaskState};

/// The one status a task shows (§2.2): the governed state, with `todo`
/// split into its derived sub-states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shown {
    /// `todo`, waiting on a failed prerequisite (at any depth).
    Stuck,
    /// `todo`, waiting on an unmet prerequisite.
    Blocked,
    /// `todo`, not blocked, with a `when`.
    Scheduled,
    /// `todo`, not blocked, its `after` not yet reached.
    Later,
    /// `todo`, not blocked, floating.
    Open,
    /// In progress.
    Wip,
    /// Done.
    Success,
    /// Failed.
    Fail,
    /// Cancelled.
    Cancelled,
}

/// What `derive` says about one task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDerived {
    /// The status shown.
    pub shown: Shown,
    /// The earliest date the open work downstream needs it by.
    pub needed_by: Option<NaiveDate>,
    /// `needed_by` lies before today.
    pub overdue: bool,
    /// Its own `due`/`after` lies after `needed_by`, or it is timed and
    /// ends after a timed dependent starts.
    pub date_conflict: bool,
    /// Waiting on a failed prerequisite.
    pub stuck: bool,
    /// `wip` while a prerequisite is unmet again.
    pub started_on_reopened: bool,
    /// The tasks it is a prerequisite for.
    pub prerequisite_for: Vec<TaskId>,
}

/// The derived view of a board on one day.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Derived {
    /// Per task.
    pub tasks: BTreeMap<TaskId, TaskDerived>,
    /// Open work (`todo` + `wip`, series aside) in priority order.
    pub priority: Vec<TaskId>,
    /// Per assignee: its `wip` tasks, then its startable `todo` tasks.
    pub next: BTreeMap<String, Vec<TaskId>>,
}

fn is_open(t: &Task) -> bool {
    matches!(t.state, TaskState::Todo | TaskState::Wip)
}

/// Every task with dependents before its prerequisites (Kahn over the
/// `blocked_by` DAG; a task on a cycle the fold would have refused is
/// left out).
fn dependents_first(board: &BoardState, dependents: &BTreeMap<&str, Vec<&str>>) -> Vec<TaskId> {
    let mut waiting: BTreeMap<&str, usize> = board
        .tasks
        .keys()
        .map(|id| (id.as_str(), dependents.get(id.as_str()).map_or(0, Vec::len)))
        .collect();
    let mut ready: BTreeSet<&str> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order = Vec::new();
    while let Some(id) = ready.pop_first() {
        order.push(id.to_string());
        if let Some(t) = board.tasks.get(id) {
            for b in &t.blocked_by {
                if let Some(n) = waiting.get_mut(b.as_str()) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        ready.insert(b.as_str());
                    }
                }
            }
        }
    }
    order
}

/// Derive deadlines, flags, priority and `next` for `today` (§5.2).
#[must_use]
pub fn derive(board: &BoardState, today: NaiveDate) -> Derived {
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, t) in &board.tasks {
        for b in &t.blocked_by {
            if board.tasks.contains_key(b) {
                dependents.entry(b.as_str()).or_default().push(id.as_str());
            }
        }
    }
    let order = dependents_first(board, &dependents);

    let mut needed_by: BTreeMap<&str, NaiveDate> = BTreeMap::new();
    for id in &order {
        let Some((key, t)) = board.tasks.get_key_value(id) else {
            continue;
        };
        if !is_open(t) {
            continue;
        }
        let downstream = dependents
            .get(key.as_str())
            .into_iter()
            .flatten()
            .filter(|d| board.tasks.get(**d).is_some_and(is_open))
            .filter_map(|d| needed_by.get(d).copied());
        if let Some(nb) = t.due.into_iter().chain(downstream).min() {
            needed_by.insert(key.as_str(), nb);
        }
    }

    let mut stuck: BTreeSet<&str> = BTreeSet::new();
    for id in order.iter().rev() {
        let Some((key, t)) = board.tasks.get_key_value(id) else {
            continue;
        };
        if t.state != TaskState::Todo {
            continue;
        }
        let waits_on_fail = t.blocked_by.iter().any(|b| {
            board
                .tasks
                .get(b)
                .is_some_and(|p| p.state == TaskState::Fail)
                || stuck.contains(b.as_str())
        });
        if waits_on_fail {
            stuck.insert(key.as_str());
        }
    }

    let unmet = |t: &Task| {
        t.blocked_by
            .iter()
            .any(|b| board.tasks.get(b).is_some_and(|p| !p.state.is_met()))
    };

    let mut out = Derived::default();
    for (id, t) in &board.tasks {
        let nb = needed_by.get(id.as_str()).copied();
        let is_stuck = stuck.contains(id.as_str());
        let blocked = unmet(t);
        let shown = match t.state {
            TaskState::Todo if is_stuck => Shown::Stuck,
            TaskState::Todo if blocked => Shown::Blocked,
            TaskState::Todo if t.when.is_some() => Shown::Scheduled,
            TaskState::Todo if t.after.is_some_and(|a| a > today) => Shown::Later,
            TaskState::Todo => Shown::Open,
            TaskState::Wip => Shown::Wip,
            TaskState::Success => Shown::Success,
            TaskState::Fail => Shown::Fail,
            TaskState::Cancelled => Shown::Cancelled,
        };
        let deps: Vec<TaskId> = dependents
            .get(id.as_str())
            .into_iter()
            .flatten()
            .map(|d| (*d).to_string())
            .collect();
        let timed_conflict = t.when.is_some_and(|w| {
            deps.iter()
                .filter_map(|d| board.tasks.get(d))
                .filter(|d| is_open(d))
                .any(|d| d.when.is_some_and(|dw| w.ends() > dw.begins()))
        });
        let open = is_open(t);
        out.tasks.insert(
            id.clone(),
            TaskDerived {
                shown,
                needed_by: nb,
                overdue: open && nb.is_some_and(|n| n < today),
                date_conflict: open
                    && (nb.is_some_and(|n| {
                        t.due.is_some_and(|d| d > n) || t.after.is_some_and(|a| a > n)
                    }) || timed_conflict),
                stuck: is_stuck,
                started_on_reopened: t.state == TaskState::Wip && blocked,
                prerequisite_for: deps,
            },
        );
    }

    let mut prio: Vec<&TaskId> = board
        .tasks
        .iter()
        .filter(|(_, t)| is_open(t) && !t.is_series())
        .map(|(id, _)| id)
        .collect();
    prio.sort_by_cached_key(|id| {
        let wip = board
            .tasks
            .get(*id)
            .is_some_and(|t| t.state == TaskState::Wip);
        let nb = needed_by.get(id.as_str()).copied();
        (!wip, nb.is_none(), nb, (*id).clone())
    });
    out.priority = prio.into_iter().cloned().collect();

    for id in &out.priority {
        let Some(t) = board.tasks.get(id) else {
            continue;
        };
        for a in &t.assignees {
            out.next.entry(a.clone()).or_default();
        }
    }
    for pass_wip in [true, false] {
        for id in &out.priority {
            let Some(t) = board.tasks.get(id) else {
                continue;
            };
            let take = if pass_wip {
                t.state == TaskState::Wip
            } else {
                t.state == TaskState::Todo && !unmet(t) && !t.after.is_some_and(|a| a > today)
            };
            if take {
                for a in &t.assignees {
                    out.next.entry(a.clone()).or_default().push(id.clone());
                }
            }
        }
    }
    out
}

/// One line of a changeset's impact (§5.4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImpactChange {
    /// `needed_by` moved.
    NeededBy {
        /// The task.
        task: TaskId,
        /// Before.
        from: Option<NaiveDate>,
        /// After.
        to: Option<NaiveDate>,
    },
    /// A flag came or went.
    Flag {
        /// The task.
        task: TaskId,
        /// Which flag.
        flag: Flag,
        /// `true` = raised, `false` = resolved.
        on: bool,
    },
}

/// The flags of §5.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Flag {
    /// Late.
    Overdue,
    /// Promised later than needed.
    DateConflict,
    /// Waiting on a failure.
    Stuck,
    /// Started on a prerequisite that was reopened.
    StartedOnReopened,
}

/// What changed between two derivations (before and after a pending
/// changeset, same `today`), by task id.
#[must_use]
pub fn impact(before: &Derived, after: &Derived) -> Vec<ImpactChange> {
    let ids: BTreeSet<&TaskId> = before.tasks.keys().chain(after.tasks.keys()).collect();
    let mut out = Vec::new();
    for id in ids {
        let b = before.tasks.get(id);
        let a = after.tasks.get(id);
        let nb = |t: Option<&TaskDerived>| t.and_then(|t| t.needed_by);
        let (b_nb, a_nb) = (nb(b), nb(a));
        if b_nb != a_nb {
            out.push(ImpactChange::NeededBy {
                task: id.clone(),
                from: b_nb,
                to: a_nb,
            });
        }
        let flags = |t: Option<&TaskDerived>| {
            t.map_or([false; 4], |t| {
                [t.overdue, t.date_conflict, t.stuck, t.started_on_reopened]
            })
        };
        let (bf, af) = (flags(b), flags(a));
        for (k, flag) in [
            Flag::Overdue,
            Flag::DateConflict,
            Flag::Stuck,
            Flag::StartedOnReopened,
        ]
        .into_iter()
        .enumerate()
        {
            if bf[k] != af[k] {
                out.push(ImpactChange::Flag {
                    task: id.clone(),
                    flag,
                    on: af[k],
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
