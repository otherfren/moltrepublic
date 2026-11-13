// SPDX-License-Identifier: GPL-3.0-or-later

//! When is the embedded Tor client itself unusable? The pure half of the
//! automatic rebuild (`tor_embedded`), free of arti so the default build
//! tests it. A rebuild keeps the guards; only the manual reset drops them.

use std::collections::HashSet;
use std::time::Duration;

/// Unhealthiness must last this long before the first rebuild.
pub const UNHEALTHY_AFTER: Duration = Duration::from_secs(600);
/// The gap between rebuilds never grows beyond this.
pub const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// A failing streak shorter than this proves nothing.
pub const MIN_FAILURES: u32 = 3;

/// What one dial outcome says about the CLIENT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A stream opened, or the far end answered with a refusal: the client works.
    Alive,
    /// A Tor-level failure that one dead destination can also cause (a timeout).
    Ambiguous,
    /// A failure only the client can cause (no directory, no path, state error).
    Client,
    /// The caller dropped the dial before arti answered: its deadline, not
    /// arti's verdict.
    CutShort,
}

/// Why the client counts as unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// arti reports itself not ready or blocked.
    NotReady,
    /// Every dial failed at Tor level.
    AllDialsFailing,
}

impl Reason {
    /// The log token.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotReady => "not_ready",
            Reason::AllDialsFailing => "all_dials_failing",
        }
    }
}

/// The decision for one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing sustained.
    Healthy,
    /// Unusable for long enough, but the last rebuild is too recent.
    Wait(Reason),
    /// Drop the client and build a new one from the same directories.
    Rebuild(Reason),
}

/// Everything [`decide`] looks at. Times are offsets on one monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HealthInput {
    /// Now.
    pub now: Duration,
    /// The first failure since the last proof of life.
    pub failing_since: Option<Duration>,
    /// Failures since then.
    pub failures: u32,
    /// Distinct hosts among them.
    pub hosts: u32,
    /// Whether one of them was [`Evidence::Client`].
    pub definite: bool,
    /// Since when arti reports itself not ready or blocked.
    pub not_ready_since: Option<Duration>,
    /// The last rebuild.
    pub last_rebuild: Option<Duration>,
    /// Rebuilds since the last proof of life.
    pub rebuilds: u32,
}

/// The minimum gap after the `rebuilds`-th rebuild: 20, 40, then 60 minutes.
pub fn backoff(rebuilds: u32) -> Duration {
    let factor = 1u32.checked_shl(rebuilds).unwrap_or(u32::MAX);
    UNHEALTHY_AFTER.saturating_mul(factor).min(BACKOFF_CAP)
}

/// Whether the client is to be rebuilt now.
pub fn decide(i: &HealthInput) -> Verdict {
    let sustained = |since: Option<Duration>| since.is_some_and(|s| i.now.saturating_sub(s) >= UNHEALTHY_AFTER);
    let reason = if sustained(i.not_ready_since) {
        Reason::NotReady
    } else if sustained(i.failing_since) && i.failures >= MIN_FAILURES && (i.definite || i.hosts >= 2) {
        Reason::AllDialsFailing
    } else {
        return Verdict::Healthy;
    };
    match i.last_rebuild {
        Some(at) if i.now.saturating_sub(at) < backoff(i.rebuilds) => Verdict::Wait(reason),
        _ => Verdict::Rebuild(reason),
    }
}

/// The running record [`decide`] is fed from.
#[derive(Debug, Default)]
pub struct Health {
    failing_since: Option<Duration>,
    failures: u32,
    hosts: HashSet<String>,
    definite: bool,
    not_ready_since: Option<Duration>,
    last_rebuild: Option<Duration>,
    rebuilds: u32,
    flagged: bool,
}

impl Health {
    /// Record one dial outcome; `true` when it opens a failing streak.
    pub fn record(&mut self, now: Duration, host: &str, evidence: Evidence) -> bool {
        if evidence == Evidence::Alive {
            *self = Health::default();
            return false;
        }
        if evidence == Evidence::CutShort {
            return false;
        }
        let opened = self.failing_since.is_none();
        self.failing_since.get_or_insert(now);
        self.failures = self.failures.saturating_add(1);
        self.hosts.insert(host.to_string());
        self.definite |= evidence == Evidence::Client;
        opened
    }

    /// Record arti's own readiness; `true` when it just turned bad.
    pub fn record_ready(&mut self, now: Duration, ready: bool) -> bool {
        if ready {
            self.not_ready_since = None;
            return false;
        }
        let turned = self.not_ready_since.is_none();
        self.not_ready_since.get_or_insert(now);
        turned
    }

    /// A bootstrap succeeded: failures from before it say nothing about the
    /// client it produced. The backoff keeps counting.
    pub fn booted(&mut self) {
        *self = Health {
            last_rebuild: self.last_rebuild,
            rebuilds: self.rebuilds,
            ..Health::default()
        };
    }

    /// `true` the first time a bad spell is reported, so it is logged once.
    pub fn flag(&mut self) -> bool {
        !std::mem::replace(&mut self.flagged, true)
    }

    /// A rebuild started: the new client earns its own evidence, the
    /// backoff keeps counting. Returns the rebuild count.
    pub fn rebuilt(&mut self, now: Duration) -> u32 {
        let rebuilds = self.rebuilds.saturating_add(1);
        *self = Health {
            last_rebuild: Some(now),
            rebuilds,
            ..Health::default()
        };
        rebuilds
    }

    /// The record as [`decide`] reads it.
    pub fn input(&self, now: Duration) -> HealthInput {
        HealthInput {
            now,
            failing_since: self.failing_since,
            failures: self.failures,
            hosts: u32::try_from(self.hosts.len()).unwrap_or(u32::MAX),
            definite: self.definite,
            not_ready_since: self.not_ready_since,
            last_rebuild: self.last_rebuild,
            rebuilds: self.rebuilds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn min(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    #[test]
    fn a_healthy_client_is_left_alone() {
        let mut h = Health::default();
        h.record(min(0), "a", Evidence::Alive);
        h.record_ready(min(0), true);
        assert_eq!(decide(&h.input(min(600))), Verdict::Healthy);
    }

    #[test]
    fn one_dead_host_among_working_ones_never_rebuilds() {
        let mut h = Health::default();
        for m in 0..120 {
            h.record(min(m), "dead", Evidence::Client);
            h.record(min(m), "alive", Evidence::Alive);
            assert_eq!(decide(&h.input(min(m))), Verdict::Healthy, "minute {m}");
            h.record(min(m), "dead", Evidence::Client);
        }
    }

    #[test]
    fn a_single_host_timing_out_is_not_evidence_against_the_client() {
        let mut h = Health::default();
        for m in 0..120 {
            h.record(min(m), "only", Evidence::Ambiguous);
        }
        assert_eq!(decide(&h.input(min(120))), Verdict::Healthy);
    }

    #[test]
    fn a_refusal_from_the_far_end_counts_as_proof_of_life() {
        let mut h = Health::default();
        for m in 0..20 {
            h.record(min(m), "a", Evidence::Client);
        }
        h.record(min(20), "b", Evidence::Alive);
        assert_eq!(decide(&h.input(min(21))), Verdict::Healthy);
    }

    #[test]
    fn all_failing_below_the_threshold_waits() {
        let mut h = Health::default();
        for m in 0..9 {
            h.record(min(m), "a", Evidence::Client);
            h.record(min(m), "b", Evidence::Ambiguous);
        }
        assert_eq!(decide(&h.input(min(9))), Verdict::Healthy);
    }

    #[test]
    fn all_failing_past_the_threshold_rebuilds() {
        let mut h = Health::default();
        for m in 0..=10 {
            h.record(min(m), "a", Evidence::Ambiguous);
            h.record(min(m), "b", Evidence::Ambiguous);
        }
        assert_eq!(decide(&h.input(min(10))), Verdict::Rebuild(Reason::AllDialsFailing));
    }

    #[test]
    fn two_failures_ten_minutes_apart_are_not_a_streak() {
        let mut h = Health::default();
        h.record(min(0), "a", Evidence::Client);
        h.record(min(30), "b", Evidence::Client);
        assert_eq!(decide(&h.input(min(30))), Verdict::Healthy);
    }

    #[test]
    fn arti_not_ready_past_the_threshold_rebuilds() {
        let mut h = Health::default();
        assert!(h.record_ready(min(0), false), "turning bad is reported once");
        assert!(!h.record_ready(min(5), false));
        assert_eq!(decide(&h.input(min(9))), Verdict::Healthy);
        assert_eq!(decide(&h.input(min(10))), Verdict::Rebuild(Reason::NotReady));
        h.record_ready(min(11), true);
        assert_eq!(decide(&h.input(min(30))), Verdict::Healthy);
    }

    /// Two pool relays timing out at the relay runtime's connect deadline while the
    /// others sit on long-lived sockets: no new dial, so no proof of life.
    #[test]
    fn dials_cut_short_by_their_caller_never_rebuild_a_ready_client() {
        let mut h = Health::default();
        for m in 0..120 {
            assert!(!h.record(min(m), "a", Evidence::CutShort), "no streak opens");
            h.record(min(m), "b", Evidence::CutShort);
            h.record_ready(min(m), true);
            assert_eq!(decide(&h.input(min(m))), Verdict::Healthy, "minute {m}");
        }
        assert_eq!(h.input(min(120)).failures, 0);
        // they neither prove life: a streak arti itself reported stands
        for m in 120..=130 {
            h.record(min(m), "c", Evidence::Client);
            h.record(min(m), "a", Evidence::CutShort);
        }
        assert_eq!(decide(&h.input(min(130))), Verdict::Rebuild(Reason::AllDialsFailing));
    }

    #[test]
    fn a_dead_client_whose_dials_are_all_cut_short_is_caught_by_its_own_status() {
        let mut h = Health::default();
        for m in 0..=10 {
            h.record(min(m), "a", Evidence::CutShort);
            h.record_ready(min(m), false);
        }
        assert_eq!(decide(&h.input(min(10))), Verdict::Rebuild(Reason::NotReady));
    }

    #[test]
    fn a_successful_bootstrap_clears_the_failures_before_it() {
        let mut h = Health::default();
        h.rebuilt(min(0));
        for m in 1..=15 {
            h.record(min(m), "", Evidence::Client);
        }
        h.record_ready(min(1), false);
        h.booted();
        h.record(min(21), "a", Evidence::Client);
        assert_eq!(decide(&h.input(min(21))), Verdict::Healthy, "the new client starts clean");
        let i = h.input(min(21));
        assert_eq!((i.failures, i.last_rebuild, i.rebuilds), (1, Some(min(0)), 1), "the backoff stays");
    }

    #[test]
    fn rebuilds_back_off_up_to_the_cap() {
        assert_eq!([backoff(1), backoff(2), backoff(3), backoff(40)], [min(20), min(40), min(60), min(60)]);
        let mut h = Health::default();
        let mut at = Vec::new();
        for m in 0..=300 {
            h.record(min(m), "a", Evidence::Client);
            if let Verdict::Rebuild(_) = decide(&h.input(min(m))) {
                h.rebuilt(min(m));
                at.push(m);
            }
        }
        assert_eq!(at, vec![10, 30, 70, 130, 190, 250]);
    }

    #[test]
    fn between_rebuilds_the_verdict_is_wait() {
        let mut h = Health::default();
        h.rebuilt(min(0));
        h.rebuilt(min(1));
        for m in 2..=20 {
            h.record(min(m), "a", Evidence::Client);
        }
        assert_eq!(decide(&h.input(min(20))), Verdict::Wait(Reason::AllDialsFailing));
        assert!(h.flag(), "a bad spell is reported");
        assert!(!h.flag(), "once");
        h.record(min(21), "a", Evidence::Alive);
        assert!(h.flag(), "and again after a recovery");
    }

    #[test]
    fn recovery_resets_the_backoff() {
        let mut h = Health::default();
        assert_eq!(h.rebuilt(min(0)), 1);
        assert_eq!(h.rebuilt(min(30)), 2);
        h.record(min(31), "a", Evidence::Alive);
        for m in 32..=42 {
            h.record(min(m), "a", Evidence::Client);
        }
        assert_eq!(decide(&h.input(min(42))), Verdict::Rebuild(Reason::AllDialsFailing));
        assert_eq!(h.rebuilt(min(42)), 1);
    }
}
