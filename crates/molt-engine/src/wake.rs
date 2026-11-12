// SPDX-License-Identifier: GPL-3.0-or-later

//! Waking this node's own seat (`docs_archive/kanban/kanban_workflows.md` §6.1):
//! the triggers (`poked`, `vote_pending`, `kanban`, `task_start`, `test`)
//! mark a reason pending; ONE wake runs at a time and fires once the
//! running one ended and `wake_min_interval_secs` passed, with every
//! pending reason. No wake crosses the wire. The `read_actions` list says
//! what is due; the reasons are hints.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use molt_core::kanban_wake::{
    key_begins, start_scan, task_actions, WakeAction, MISSED_HORIZON_SECS, TEST_REASON, WAKE_ENV,
};
use molt_core::{ChatKind, ChatMessage, MoltError, Reply, SessionScope};

use crate::State;

/// Seconds between two task-timer scans while nothing is due nearer.
const TIMER_IDLE_SECS: u64 = 3600;

/// Fired keys are kept this long past their start, then forgotten.
const FIRED_KEEP_SECS: i64 = 2 * MISSED_HORIZON_SECS;

/// Bytes of a test wake's stderr kept while it runs.
const STDERR_KEEP: usize = 4096;

/// Lines of it handed back.
const STDERR_LINES: usize = 8;

/// Wake lines kept for the chat (runtime only).
const LINES_KEEP: usize = 200;

/// How a wake ended, from its reaper thread.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct WakeEnd {
    /// `exit <code>`, `exit signal`, `timeout`, `check failed`, `error: …`.
    outcome: String,
    /// The check a test wake failed; then nothing ran.
    check: String,
    /// The stderr tail of a test wake.
    stderr: String,
}

impl WakeEnd {
    fn of(outcome: impl Into<String>) -> WakeEnd {
        WakeEnd { outcome: outcome.into(), ..WakeEnd::default() }
    }
}

/// The wake bookkeeping. Runtime-only except `fired` (`kanban_wakes.json`).
#[derive(Default)]
pub(crate) struct WakeState {
    /// The wake command is running; cleared by its reaper thread.
    running: Arc<AtomicBool>,
    /// How the last wake ended, for the reaper to hand back.
    ended: Arc<Mutex<Option<WakeEnd>>>,
    /// The running wake carries the test reason.
    test_running: bool,
    /// The running wake's reasons (`MOLT_WAKE_REASON`).
    running_reason: String,
    /// The command a waiting test wake runs; `None` = the stored one.
    test_command: Option<String>,
    /// The poke and test wake lines of the open workspace's group chat.
    /// Never an event: nothing here reaches the log or the wire.
    pub(crate) lines: Vec<ChatMessage>,
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
        // a wake still running belongs to the workspace left: no line here
        self.running_reason.clear();
        self.pending.clear();
        self.pending_by.clear();
        self.last_at = None;
        self.proposed_at.clear();
        self.pokes.clear();
        self.fired.clear();
        self.timer = None;
        self.lines.clear();
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
        if reason != TEST_REASON {
            if s.poke_wake_command.trim().is_empty() {
                return;
            }
            if !s.wake_on.iter().any(|r| r == reason) {
                tracing::debug!(reason, "wake_on=off");
                return;
            }
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
        // a test wake runs alone: its command may be an unsaved draft
        if self.wake.pending.remove(TEST_REASON) {
            let command = self.wake.test_command.take();
            self.wake.last_at = Some(now);
            self.spawn_wake(&[TEST_REASON.to_string()], "", command);
            return;
        }
        if resting {
            return;
        }
        let reasons: Vec<String> = std::mem::take(&mut self.wake.pending).into_iter().collect();
        let by = std::mem::take(&mut self.wake.pending_by);
        self.wake.last_at = Some(now);
        self.spawn_wake(&reasons, &by, None);
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
        if let Some(end) = ended {
            // the rest runs from the end of a wake (§6.1)
            self.wake.last_at = Some(self.presence_now());
            self.wake_ended(end);
        }
        self.task_timer();
        self.wake_pump();
    }

    /// Surface how a wake ended: a test in `session.wake_test*`, a failed
    /// real one in the log and `session.wake_last`, both kinds of line in
    /// the chat.
    fn wake_ended(&mut self, end: WakeEnd) {
        let reason = std::mem::take(&mut self.wake.running_reason);
        let test = std::mem::take(&mut self.wake.test_running);
        let mut changed = self.wake_line(&reason, &result_line(&end));
        // a test's own failure is its answer; its timeout killed a process
        if end.outcome != "exit 0" && (!test || end.outcome == "timeout") {
            let exit = end.outcome.strip_prefix("exit ").unwrap_or(&end.outcome);
            tracing::warn!(%reason, %exit, "wake=failed");
        }
        if test {
            self.session.wake_test = end.outcome;
            self.session.wake_test_check = end.check;
            self.session.wake_test_stderr = end.stderr;
            changed = true;
        } else {
            let failed = if end.outcome == "exit 0" { String::new() } else { end.outcome };
            changed |= self.session.wake_last != failed;
            self.session.wake_last = failed;
        }
        if changed {
            self.emit_session(SessionScope::Full);
        }
    }

    /// Note a poke or test wake in the open workspace's group chat: a
    /// line of THIS node, held outside the event log.
    fn wake_line(&mut self, reason: &str, body: &str) -> bool {
        let noted = reason.split(',').any(|r| r == "poked" || r == TEST_REASON);
        noted && self.note_line(body)
    }

    /// A local line in the open workspace's group chat, never on the wire.
    pub(crate) fn note_line(&mut self, body: &str) -> bool {
        if self.session.active_workspace.is_empty() {
            return false;
        }
        let Ok(id) = crate::chat::mint_message_id() else {
            return false;
        };
        let line = ChatMessage::text(id, self.member(), body, crate::now_secs())
            .with_kind(ChatKind::Wake);
        self.wake.lines.push(line);
        if self.wake.lines.len() > LINES_KEEP {
            self.wake.lines.remove(0);
        }
        true
    }

    /// The wake lines the group chat shows, unless switched off.
    pub(crate) fn wake_lines(&self) -> &[ChatMessage] {
        if self.session.settings.wake_chat_lines {
            &self.wake.lines
        } else {
            &[]
        }
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
        let Some(now_t) = crate::kanban::utc(now) else {
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
        // the proposals map is a HashMap: its order is no order
        out.sort_unstable_by_key(|a| match a {
            WakeAction::Vote { proposal, since, .. } => (*since, *proposal),
            _ => (0, 0),
        });
        if let Some(now) = crate::kanban::utc(self.presence_now()) {
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
    pub(crate) fn cmd_test_wake(&mut self, command: Option<String>) -> Result<Reply, MoltError> {
        let command = command.map(|c| c.trim().to_string());
        let tested = command.as_deref().unwrap_or(&self.session.settings.poke_wake_command);
        if tested.trim().is_empty() {
            return Err(MoltError::Settings("no wake command".to_string()));
        }
        crate::session::validate_wake_command(tested)?;
        self.wake.test_command = command;
        let waiting = self.wake.running.load(Ordering::SeqCst);
        self.session.wake_test = if waiting { "waiting" } else { "started" }.to_string();
        self.session.wake_test_check.clear();
        self.session.wake_test_stderr.clear();
        self.wake_trigger(TEST_REASON, "");
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Spawn the wake command, fire-and-forget (a detached thread reaps
    /// it). The command string comes ONLY from local posture or this
    /// node's own operator; wire content never reaches the command line -
    /// context rides `MOLT_WAKE_*` env vars, and the woken agent reads the
    /// actual state over MCP.
    fn spawn_wake(&mut self, reasons: &[String], by: &str, command: Option<String>) {
        let cmd = command
            .unwrap_or_else(|| self.session.settings.poke_wake_command.clone())
            .trim()
            .to_string();
        if cmd.is_empty() {
            return;
        }
        let reason = reasons.join(",");
        let test = reasons.iter().any(|r| r == TEST_REASON);
        let workspace = self.session.active_workspace.clone();
        let list = self.read_actions();
        let pending = list.iter().filter(|a| matches!(a, WakeAction::Vote { .. })).count();
        let actions = list.len();
        tracing::info!(reason = %reason, %by, pending, actions, "wake=spawned");
        #[cfg(test)]
        self.wake.log.push(reason.clone());
        self.wake.test_running = test;
        self.wake.running_reason.clone_from(&reason);
        let start = if by.is_empty() {
            format!("wake: {reason}")
        } else {
            format!("wake: {reason} by {by}")
        };
        if self.wake_line(&reason, &start) {
            self.emit_session(SessionScope::Full);
        }
        let timeout = Duration::from_secs(self.session.settings.wake_timeout_secs.max(1));
        let running = self.wake.running.clone();
        let ended = self.wake.ended.clone();
        running.store(true, Ordering::SeqCst);
        // one value per name, or this fails to compile
        let vals: [String; WAKE_ENV.len()] = [
            reason,
            by.to_string(),
            workspace,
            pending.to_string(),
            actions.to_string(),
        ];
        // `Builder::spawn`, never `thread::spawn`: the latter PANICS when the
        // OS refuses a thread, and this runs on the single-owner actor
        let spawned = std::thread::Builder::new()
            .name("molt-wake".to_string())
            .spawn(move || {
                let end = run_wake(&cmd, vals, timeout, test);
                if let Ok(mut slot) = ended.lock() {
                    *slot = Some(end);
                }
                running.store(false, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            self.wake.running.store(false, Ordering::SeqCst);
            if let Ok(mut slot) = self.wake.ended.lock() {
                *slot = Some(WakeEnd::of(format!("error: {e}")));
            }
        }
    }
}

/// The chat line a wake's end leaves.
fn result_line(end: &WakeEnd) -> String {
    if end.check.is_empty() {
        format!("wake: {}", end.outcome)
    } else {
        format!("wake: {} - {}", end.outcome, end.check)
    }
}

/// Run one wake to its end or its timeout. A test wake is checked first
/// and hands back its stderr tail; a real one runs with stderr closed.
fn run_wake(cmd: &str, vals: [String; WAKE_ENV.len()], timeout: Duration, test: bool) -> WakeEnd {
    if test {
        if let Err(check) = check::check_command(cmd, &check::CheckEnv::of_process()) {
            return WakeEnd { outcome: "check failed".to_string(), check, stderr: String::new() };
        }
    }
    let stderr = if test { std::process::Stdio::piped() } else { std::process::Stdio::null() };
    // a shell that forks for the line would take the timeout's kill itself
    let line = if check::runs_one_program(cmd) { format!("exec {cmd}") } else { cmd.to_string() };
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .envs(WAKE_ENV.iter().zip(vals))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return WakeEnd::of(format!("error: {e}")),
    };
    let tail = child.stderr.take().map(tail_reader);
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break status
                    .code()
                    .map_or_else(|| "exit signal".to_string(), |c| format!("exit {c}"));
            }
            Ok(None) if Instant::now() >= deadline => {
                // the script only: what it started keeps running
                let _ = child.kill();
                let _ = child.wait();
                break "timeout".to_string();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break format!("error: {e}"),
        }
    };
    let stderr = tail.map(Tail::take).unwrap_or_default();
    WakeEnd { outcome, check: String::new(), stderr }
}

/// The last bytes of a child's stderr, read beside it.
struct Tail {
    kept: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

fn tail_reader(mut pipe: std::process::ChildStderr) -> Tail {
    let tail = Tail { kept: Arc::default(), done: Arc::default() };
    let (kept, done) = (tail.kept.clone(), tail.done.clone());
    let reader = std::thread::Builder::new().name("molt-wake-err".to_string()).spawn(move || {
        use std::io::Read as _;
        let mut buf = [0u8; 1024];
        while let Ok(n @ 1..) = pipe.read(&mut buf) {
            if let Ok(mut k) = kept.lock() {
                k.extend_from_slice(&buf[..n]);
                let over = k.len().saturating_sub(STDERR_KEEP);
                k.drain(..over);
            }
        }
        done.store(true, Ordering::SeqCst);
    });
    if reader.is_err() {
        tail.done.store(true, Ordering::SeqCst);
    }
    tail
}

impl Tail {
    /// The last lines. A grandchild may hold the pipe open past the
    /// script's end, so the reader gets a moment, not a join.
    fn take(self) -> String {
        let until = Instant::now() + Duration::from_millis(300);
        while !self.done.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        let bytes = self.kept.lock().map(|k| k.clone()).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(STDERR_LINES)..].join("\n")
    }
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

mod check;
#[cfg(test)]
mod tests;
