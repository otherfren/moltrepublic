//! What wakes a seat and what it finds when it wakes
//! (`docs/kanban/kanban_workflows.md` §6.1): the `task_start` timer's due
//! starts and the `read_actions` list. Pure - the caller passes the board,
//! its derivation and the UTC clock.

use std::collections::BTreeSet;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::kanban_calendar::{expand, fmt_date, fmt_datetime};
use crate::kanban_dates::{Derived, Shown};
use crate::kanban_fold::{BoardState, TaskId, TaskState};
use crate::{MemberId, Surface};

/// Every reason a trigger may wake a seat, in `[node] wake_on` spelling.
pub const WAKE_REASONS: [&str; 4] = ["poked", "vote_pending", "kanban", "task_start"];

/// The reason a test wake carries; never switched off by `wake_on`.
pub const TEST_REASON: &str = "test";

/// The environment a wake command runs with: why, who poked, the
/// workspace, pending votes and the `read_actions` length.
pub const WAKE_ENV: [&str; 5] = [
    "MOLT_WAKE_REASON",
    "MOLT_WAKE_BY",
    "MOLT_WAKE_WORKSPACE",
    "MOLT_WAKE_PENDING",
    "MOLT_WAKE_ACTIONS",
];

/// A missed start older than this never fires (§6.1).
pub const MISSED_HORIZON_SECS: i64 = 24 * 3600;

/// A start reported past by less than this is the timer's own jitter.
pub const LATE_GRACE_SECS: i64 = 60;

/// How far ahead the timer looks for its next start.
const LOOKAHEAD_DAYS: u64 = 15;

/// One entry of the `read_actions` list: only what the seat can act on now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeAction {
    /// A proposal waits for this seat's vote.
    Vote {
        /// The proposal.
        proposal: u64,
        /// Its surface.
        surface: Surface,
        /// When it was proposed (unix seconds; 0 = unknown).
        since: u64,
    },
    /// A timed task of this seat reached its start (minus the lead).
    TaskStart {
        /// The task.
        task: TaskId,
        /// The occurrence's original date, for a series.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        occurrence: Option<String>,
        /// The start has passed.
        late: bool,
    },
    /// This seat's task in progress.
    TaskWip {
        /// The task.
        task: TaskId,
    },
    /// This seat's `todo` task that is not blocked.
    TaskStartable {
        /// The task.
        task: TaskId,
    },
    /// A member poked this seat and the chat was not read since.
    Poke {
        /// The poker.
        by: MemberId,
        /// When the first unread poke arrived (unix seconds).
        since: u64,
    },
}

/// A start of one of the seat's timed tasks whose fire time has come.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartDue {
    /// The task.
    pub task: TaskId,
    /// The occurrence's original date, for a series.
    pub occurrence: Option<NaiveDate>,
    /// When it begins (UTC; an all-day window at 00:00).
    pub begins: NaiveDateTime,
    /// The task is blocked or stuck: it fires nothing.
    pub blocked: bool,
}

impl StartDue {
    /// The `kanban_wakes.json` key: one fire per task and start.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}@{}", self.task, fmt_datetime(self.begins))
    }

    /// Whether the start lies behind `now` (beyond the timer's jitter).
    #[must_use]
    pub fn late(&self, now: NaiveDateTime) -> bool {
        (now - self.begins).num_seconds() > LATE_GRACE_SECS
    }
}

/// The start in a `kanban_wakes.json` key.
#[must_use]
pub fn key_begins(key: &str) -> Option<NaiveDateTime> {
    key.rsplit_once('@')
        .and_then(|(_, t)| crate::kanban_calendar::parse_datetime(t))
}

/// What the `task_start` timer sees at `now`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartScan {
    /// Starts whose fire time has come and that are less than a day old.
    pub due: Vec<StartDue>,
    /// The next fire time after `now`, within the lookahead.
    pub next_fire: Option<NaiveDateTime>,
}

/// The `todo` timed tasks `seat` is assigned to, expanded around `now`:
/// fire time = start minus `lead_min` (§6.1).
#[must_use]
pub fn start_scan(
    board: &BoardState,
    derived: &Derived,
    seat: &str,
    now: NaiveDateTime,
    lead_min: u16,
) -> StartScan {
    let lead = TimeDelta::minutes(i64::from(lead_min));
    let horizon = TimeDelta::seconds(MISSED_HORIZON_SECS);
    let from = (now - horizon).date();
    let to = (now + lead)
        .date()
        .checked_add_days(chrono::Days::new(LOOKAHEAD_DAYS))
        .unwrap_or(NaiveDate::MAX);
    let mut out = StartScan::default();
    for (id, t) in &board.tasks {
        let Some(when) = t.when.as_ref() else {
            continue;
        };
        if t.state != TaskState::Todo || !t.assignees.iter().any(|a| a == seat) {
            continue;
        }
        let blocked = derived
            .tasks
            .get(id)
            .is_some_and(|d| matches!(d.shown, Shown::Blocked | Shown::Stuck));
        for occ in expand(when, t.repeat.as_ref(), &t.skip, &t.moved, from, to) {
            let begins = occ.window.begins();
            let fire = begins - lead;
            if fire > now {
                out.next_fire = Some(out.next_fire.map_or(fire, |n| n.min(fire)));
            } else if now - begins < horizon {
                out.due.push(StartDue {
                    task: id.clone(),
                    occurrence: t.is_series().then_some(occ.date),
                    begins,
                    blocked,
                });
            }
        }
    }
    out
}

/// The task part of `read_actions` for `seat` at `now`: due starts, then
/// its `wip`, then its startable `todo` tasks (§5.2 `next`). Blocked or
/// stuck work is never listed.
#[must_use]
pub fn task_actions(
    board: &BoardState,
    derived: &Derived,
    seat: &str,
    now: NaiveDateTime,
    lead_min: u16,
) -> Vec<WakeAction> {
    let mut out = Vec::new();
    let mut starting = BTreeSet::new();
    for due in start_scan(board, derived, seat, now, lead_min).due {
        if due.blocked {
            continue;
        }
        let late = due.late(now);
        starting.insert(due.task.clone());
        out.push(WakeAction::TaskStart {
            task: due.task,
            occurrence: due.occurrence.map(fmt_date),
            late,
        });
    }
    let next = derived.next.get(seat).map_or(&[][..], Vec::as_slice);
    let state = |id: &TaskId| board.tasks.get(id).map(|t| t.state);
    out.extend(
        next.iter()
            .filter(|id| state(id) == Some(TaskState::Wip))
            .map(|id| WakeAction::TaskWip { task: id.clone() }),
    );
    out.extend(
        next.iter()
            .filter(|id| state(id) == Some(TaskState::Todo) && !starting.contains(*id))
            .map(|id| WakeAction::TaskStartable { task: id.clone() }),
    );
    out
}

#[cfg(test)]
mod tests;
