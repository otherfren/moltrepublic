// SPDX-License-Identifier: GPL-3.0-or-later

//! Waking this node's own seat (`docs/kanban/kanban_workflows.md` §6.1):
//! the triggers (`poked`, `vote_pending`, `kanban`, `task_start`, `test`)
//! mark a reason pending; ONE wake runs at a time and fires once the
//! running one ended and `wake_min_interval_secs` passed, with every
//! pending reason. No wake crosses the wire. The `read_actions` list says
//! what is due; the reasons are hints.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use molt_core::kanban_wake::{
    key_begins, start_scan, task_actions, WakeAction, MISSED_HORIZON_SECS, TEST_REASON, WAKE_ENV,
};
use molt_core::{MoltError, Reply, SessionScope};

use crate::State;

/// Seconds between two task-timer scans while nothing is due nearer.
const TIMER_IDLE_SECS: u64 = 3600;

/// Fired keys are kept this long past their start, then forgotten.
const FIRED_KEEP_SECS: i64 = 2 * MISSED_HORIZON_SECS;

/// The wake bookkeeping. Runtime-only except `fired` (`kanban_wakes.json`).
#[derive(Default)]
pub(crate) struct WakeState {
    /// The wake command is running; cleared by its reaper thread.
    running: Arc<AtomicBool>,
    /// How the last wake ended (`exit <code>` / `error: …`), for the reaper
    /// to hand back.
    ended: Arc<Mutex<Option<String>>>,
    /// The running wake carries the test reason.
    test_running: bool,
    /// Reasons waiting for the next wake.
    pending: BTreeSet<String>,
    /// The newest member among the pending triggers (`MOLT_WAKE_BY`).
    pending_by: String,
    /// When the last wake fired, then when it ended (`presence_now`).
    last_at: Option<u64>,
    /// When each proposal was proposed (its envelope ts; display only).
    pub(crate) proposed_at: BTreeMap<u64, u64>,
    /// Pokes not yet answered by a chat read: poker → first arrival.
    pub(crate) pokes: BTreeMap<String, u64>,
    /// Fired `task_start` keys (`task@start`).
    fired: BTreeSet<String>,
    /// Board revision, lead and seat the next timer check was computed for.
    timer: Option<(u64, u16, String, u64)>,
    /// Every reason string fired, in order (tests only).
    #[cfg(test)]
    pub(crate) log: Vec<String>,
}

impl WakeState {
    /// Forget the workspace-scoped part (a switch or close).
    pub(crate) fn reset_workspace(&mut self) {
        self.pending.clear();
        self.pending_by.clear();
        self.last_at = None;
        self.proposed_at.clear();
        self.pokes.clear();
        self.fired.clear();
        self.timer = None;
    }

    /// The fired keys read at open.
    pub(crate) fn adopt_fired(&mut self, keys: Vec<String>) {
        self.fired = keys.into_iter().collect();
        self.timer = None;
    }

    #[cfg(test)]
    pub(crate) fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::SeqCst);
    }
}

impl State {
    /// A trigger: mark `reason` pending unless `wake_on` switches it off,
    /// then fire if nothing holds the wake back.
    pub(crate) fn wake_trigger(&mut self, reason: &str, by: &str) {
        let s = &self.session.settings;
        if s.poke_wake_command.trim().is_empty() {
            return;
        }
        if reason != TEST_REASON && !s.wake_on.iter().any(|r| r == reason) {
            tracing::debug!(reason, "wake_on=off");
            return;
        }
        self.wake.pending.insert(reason.to_string());
        if !by.is_empty() {
            self.wake.pending_by = by.to_string();
        }
        self.wake_pump();
    }

    /// Fire the pending reasons when no wake runs and the interval passed
    /// (a test wake does not wait for the interval).
    fn wake_pump(&mut self) {
        if self.wake.pending.is_empty() || self.wake.running.load(Ordering::SeqCst) {
            return;
        }
        let now = self.presence_now();
        let rest = self.session.settings.wake_min_interval_secs;
        let resting = self
            .wake
            .last_at
            .is_some_and(|last| now.saturating_sub(last) < rest);
        if resting && !self.wake.pending.contains(TEST_REASON) {
            return;
        }
        let reasons: Vec<String> = std::mem::take(&mut self.wake.pending).into_iter().collect();
        let by = std::mem::take(&mut self.wake.pending_by);
        self.wake.last_at = Some(now);
        self.spawn_wake(&reasons, &by);
    }

    /// The 1 s beat: hand back a finished wake, run the task timer, fire
    /// what waits.
    pub(crate) fn wake_tick(&mut self) {
        let ended = self
            .wake
            .ended
            .lock()
            .map(|mut e| e.take())
            .unwrap_or_default();
        if let Some(outcome) = ended {
            // the rest runs from the end of a wake (§6.1)
            self.wake.last_at = Some(self.presence_now());
            if std::mem::take(&mut self.wake.test_running) {
                self.session.wake_test = outcome;
                self.emit_session(SessionScope::Full);
            }
        }
        self.task_timer();
        self.wake_pump();
    }

    /// The `task_start` trigger: every due start of this seat's timed
    /// tasks fires once; a blocked one is consumed silently (§6.1).
    fn task_timer(&mut self) {
        if self.session.active_workspace.is_empty() {
            return;
        }
        let now = self.presence_now();
        let lead = self.session.settings.task_wake_lead_min;
        let me = self.member();
        self.refresh_kanban_cache();
        let rev = self.kanban_cache.as_ref().map_or(0, |c| c.board.rev);
        if let Some((r, l, m, at)) = &self.wake.timer {
            if *r == rev && *l == lead && *m == me && now < *at {
                return;
            }
        }
        let Some(now_t) = utc(now) else {
            return;
        };
        let today = now_t.date();
        let (scan, next) = {
            let cache = self.kanban_board();
            let derived = molt_core::kanban_dates::derive(&cache.board, today);
            let scan = start_scan(&cache.board, &derived, &me, now_t, lead);
            (scan.due, scan.next_fire)
        };
        let next_at = next
            .map(|t| u64::try_from(t.and_utc().timestamp()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX)
            .min(now.saturating_add(TIMER_IDLE_SECS))
            // the day boundary changes what is blocked and due
            .min(day_end(now));
        self.wake.timer = Some((rev, lead, me, next_at));
        let mut changed = self.prune_fired(now_t);
        let mut fire = false;
        for due in scan {
            if self.wake.fired.insert(due.key()) {
                changed = true;
                if due.blocked {
                    tracing::debug!(task = %due.task, "task_start=blocked");
                } else {
                    fire = true;
                }
            }
        }
        if changed {
            self.save_fired();
        }
        if fire {
            self.wake_trigger("task_start", "");
        }
    }

    fn prune_fired(&mut self, now: chrono::NaiveDateTime) -> bool {
        let before = self.wake.fired.len();
        self.wake.fired.retain(|k| {
            key_begins(k).is_some_and(|b| (now - b).num_seconds() < FIRED_KEEP_SECS)
        });
        self.wake.fired.len() != before
    }

    fn save_fired(&self) {
        if let Some(active) = self.active.as_ref() {
            active
                .handle
                .save_kanban_wakes(self.wake.fired.iter().cloned().collect());
        }
    }

    /// `read_actions` (§6.1): what this seat can act on now, derived on
    /// demand - votes, due starts, its wip and startable tasks, unread pokes.
    pub(crate) fn read_actions(&mut self) -> Vec<WakeAction> {
        let me = self.member();
        let mut out: Vec<WakeAction> = self
            .proposals
            .iter()
            .filter(|(id, p)| self.waits_on(**id, p, &me))
            .map(|(id, p)| WakeAction::Vote {
                proposal: *id,
                surface: p.surface,
                since: self.wake.proposed_at.get(id).copied().unwrap_or(0),
            })
            .collect();
        if let Some(now) = utc(self.presence_now()) {
            self.refresh_kanban_cache();
            let cache = self.kanban_board();
            let derived = molt_core::kanban_dates::derive(&cache.board, now.date());
            out.extend(task_actions(
                &cache.board,
                &derived,
                &me,
                now,
                self.session.settings.task_wake_lead_min,
            ));
        }
        out.extend(self.wake.pokes.iter().map(|(by, since)| WakeAction::Poke {
            by: by.clone(),
            since: *since,
        }));
        out
    }

    /// [`molt_core::Command::TestWake`] (§6.2).
    pub(crate) fn cmd_test_wake(&mut self) -> Result<Reply, MoltError> {
        if self.session.settings.poke_wake_command.trim().is_empty() {
            return Err(MoltError::Settings("no wake command".to_string()));
        }
        let waiting = self.wake.running.load(Ordering::SeqCst);
        self.session.wake_test = if waiting { "waiting" } else { "started" }.to_string();
        self.wake_trigger(TEST_REASON, "");
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Spawn the configured wake command, fire-and-forget (a detached thread
    /// reaps it). The command string comes ONLY from local posture; wire
    /// content never reaches the command line - context rides `MOLT_WAKE_*`
    /// env vars, and the woken agent reads the actual state over MCP.
    fn spawn_wake(&mut self, reasons: &[String], by: &str) {
        let cmd = self.session.settings.poke_wake_command.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        let reason = reasons.join(",");
        let workspace = self.session.active_workspace.clone();
        let list = self.read_actions();
        let pending = list.iter().filter(|a| matches!(a, WakeAction::Vote { .. })).count();
        let actions = list.len();
        tracing::info!(reason = %reason, %by, pending, actions, "wake=spawned");
        #[cfg(test)]
        self.wake.log.push(reason.clone());
        self.wake.test_running = reasons.iter().any(|r| r == TEST_REASON);
        let running = self.wake.running.clone();
        let ended = self.wake.ended.clone();
        running.store(true, Ordering::SeqCst);
        let by = by.to_string();
        // `Builder::spawn`, never `thread::spawn`: the latter PANICS when the
        // OS refuses a thread, and this runs on the single-owner actor
        let spawned = std::thread::Builder::new()
            .name("molt-wake".to_string())
            .spawn(move || {
                let outcome = match std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .envs(WAKE_ENV.iter().zip([
                        reason,
                        by,
                        workspace,
                        pending.to_string(),
                        actions.to_string(),
                    ]))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(mut child) => match child.wait() {
                        Ok(status) => status
                            .code()
                            .map_or_else(|| "exit signal".to_string(), |c| format!("exit {c}")),
                        Err(e) => format!("error: {e}"),
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "wake=spawn_failed");
                        format!("error: {e}")
                    }
                };
                if let Ok(mut slot) = ended.lock() {
                    *slot = Some(outcome);
                }
                running.store(false, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            self.wake.running.store(false, Ordering::SeqCst);
            if let Ok(mut slot) = self.wake.ended.lock() {
                *slot = Some(format!("error: {e}"));
            }
            tracing::warn!(error = %e, "wake=no_thread");
        }
    }
}

fn utc(secs: u64) -> Option<chrono::NaiveDateTime> {
    chrono::DateTime::from_timestamp(i64::try_from(secs).ok()?, 0).map(|t| t.naive_utc())
}

/// The first second of the next UTC day.
fn day_end(secs: u64) -> u64 {
    (secs / 86_400 + 1).saturating_mul(86_400)
}

/// Wait until the running wake ended (tests only).
#[cfg(test)]
pub(crate) fn await_idle(s: &State) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while s.wake.running.load(Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the wake never ended");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests;
