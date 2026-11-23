// SPDX-License-Identifier: GPL-3.0-or-later
//! What a relay's `OK false` / `CLOSED` reason means, and a per-relay tally
//! of the refusals this process saw (`relay_pool.md` §5.2).
//!
//! The prefix is NIP-01's machine-readable one, parsed by
//! [`nostr::message::MachineReadablePrefix::parse`] (nostr 0.44.6,
//! `src/message/relay.rs`: `duplicate`, `pow`, `blocked`, `rate-limited`,
//! `invalid`, `error`, `unsupported`, plus NIP-42's `auth-required` and
//! `restricted`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use nostr::message::MachineReadablePrefix;

/// What a refusal means for THIS event on THIS relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalClass {
    /// The relay holds the event (`duplicate:`).
    Accepted,
    /// Ask again later on the same relay (busy, rate-limited, any other `error:`).
    Transient,
    /// The same answer in an hour: not retried there.
    Permanent,
}

/// A classified refusal: its class and a stable shape token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// Retry, give up, or count as landed.
    pub class: RefusalClass,
    /// `busy`, `rate_limited`, `error`, `invalid`, `blocked`, `pow`,
    /// `restricted`, `unsupported`, `auth_required`, `duplicate` or
    /// `unprefixed`.
    pub shape: &'static str,
}

/// `error:` texts that name a storage or load hiccup. Every other `error:`
/// is transient too; these only get the `busy` shape.
const BUSY_SHAPES: &[&str] = &[
    "database is locked",
    "database is busy",
    "busy",
    "locked",
    "try again",
    "temporar",
    "overload",
    "timed out",
    "timeout",
    "too many",
];

/// Classify a relay's refusal message: by NIP-01 prefix first, then (for
/// `error:`) by [`BUSY_SHAPES`]. An unknown `error:` text and a reason with
/// no prefix are transient; the retry budget bounds them.
pub fn classify(message: &str) -> Refusal {
    let t = |shape| Refusal { class: RefusalClass::Transient, shape };
    let p = |shape| Refusal { class: RefusalClass::Permanent, shape };
    match MachineReadablePrefix::parse(message) {
        Some(MachineReadablePrefix::Duplicate) => Refusal { class: RefusalClass::Accepted, shape: "duplicate" },
        Some(MachineReadablePrefix::RateLimited) => t("rate_limited"),
        Some(MachineReadablePrefix::Error) => {
            let text = message.to_ascii_lowercase();
            if BUSY_SHAPES.iter().any(|s| text.contains(s)) {
                t("busy")
            } else {
                t("error")
            }
        }
        Some(MachineReadablePrefix::Invalid) => p("invalid"),
        Some(MachineReadablePrefix::Blocked) => p("blocked"),
        Some(MachineReadablePrefix::Pow) => p("pow"),
        Some(MachineReadablePrefix::Restricted) => p("restricted"),
        Some(MachineReadablePrefix::Unsupported) => p("unsupported"),
        Some(MachineReadablePrefix::AuthRequired) => p("auth_required"),
        None => t("unprefixed"),
    }
}

/// One relay's refusals as this process saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayRefusals {
    /// Transient refusals within the last hour.
    pub busy: u32,
    /// Shape of the latest refusal ([`Refusal::shape`]).
    pub last_shape: &'static str,
    /// When it came (unix seconds).
    pub last_at: u64,
}

const WINDOW_SECS: u64 = 3_600;
/// Bound on the stamps kept per relay: a relay refusing faster than this
/// per hour reads as the bound.
const MAX_STAMPS: usize = 4_096;

#[derive(Default)]
struct Tally {
    transient: VecDeque<u64>,
    last_shape: &'static str,
    last_at: u64,
    /// shape -> when it last reached WARN
    warned: HashMap<&'static str, u64>,
}

fn tallies() -> &'static Mutex<HashMap<String, Tally>> {
    static TALLIES: OnceLock<Mutex<HashMap<String, Tally>>> = OnceLock::new();
    TALLIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Count one refusal from `url`. `true` = this shape has not been logged
/// at WARN for this relay within the hour (the caller logs it there; every
/// repeat stays at DEBUG).
pub fn record(url: &str, refusal: Refusal) -> bool {
    record_at(url, refusal, now_secs())
}

fn record_at(url: &str, refusal: Refusal, now: u64) -> bool {
    if refusal.class == RefusalClass::Accepted {
        return false;
    }
    let Ok(mut all) = tallies().lock() else { return false };
    let tally = all.entry(url.to_string()).or_default();
    if refusal.class == RefusalClass::Transient {
        if tally.transient.len() >= MAX_STAMPS {
            tally.transient.pop_front();
        }
        tally.transient.push_back(now);
    }
    tally.last_shape = refusal.shape;
    tally.last_at = now;
    let due = tally
        .warned
        .get(refusal.shape)
        .map_or(true, |at| now.saturating_sub(*at) >= WINDOW_SECS);
    if due {
        tally.warned.insert(refusal.shape, now);
    }
    due
}

/// Every relay that refused something since the process started.
pub fn refusals() -> HashMap<String, RelayRefusals> {
    refusals_at(now_secs())
}

fn refusals_at(now: u64) -> HashMap<String, RelayRefusals> {
    let Ok(mut all) = tallies().lock() else { return HashMap::new() };
    all.iter_mut()
        .map(|(url, t)| {
            while t.transient.front().is_some_and(|at| now.saturating_sub(*at) >= WINDOW_SECS) {
                t.transient.pop_front();
            }
            let busy = u32::try_from(t.transient.len()).unwrap_or(u32::MAX);
            (url.clone(), RelayRefusals { busy, last_shape: t.last_shape, last_at: t.last_at })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_sort_by_prefix_then_busy_shape() {
        let c = |m: &str| classify(m);
        assert_eq!(c("error: database is locked"), Refusal { class: RefusalClass::Transient, shape: "busy" });
        assert_eq!(c("error: SQLITE_BUSY").shape, "busy");
        assert_eq!(c("rate-limited: slow down").class, RefusalClass::Transient);
        assert_eq!(c("error: disk quota"), Refusal { class: RefusalClass::Transient, shape: "error" });
        assert_eq!(c("whatever"), Refusal { class: RefusalClass::Transient, shape: "unprefixed" });
        assert_eq!(c("duplicate: have it").class, RefusalClass::Accepted);
        for (m, shape) in [
            ("invalid: bad sig", "invalid"),
            ("blocked: no", "blocked"),
            ("pow: 20 bits", "pow"),
            ("restricted: members", "restricted"),
            ("unsupported: kind", "unsupported"),
            ("auth-required: x", "auth_required"),
        ] {
            assert_eq!(c(m), Refusal { class: RefusalClass::Permanent, shape }, "{m}");
        }
        // the prefix decides, not a busy word further on
        assert_eq!(c("invalid: database is locked").class, RefusalClass::Permanent);
    }

    #[test]
    fn the_tally_counts_an_hour_and_warns_once_per_shape() {
        let url = "ws://tally.test";
        let busy = classify("error: database is locked");
        let bad = classify("invalid: x");
        assert!(record_at(url, busy, 1_000), "the first busy warns");
        assert!(!record_at(url, busy, 1_010), "a repeat does not");
        assert!(record_at(url, bad, 1_020), "another shape warns on its own");
        let r = refusals_at(1_020)[url].clone();
        assert_eq!(r, RelayRefusals { busy: 2, last_shape: "invalid", last_at: 1_020 });
        let later = refusals_at(1_005 + WINDOW_SECS)[url].clone();
        assert_eq!(later.busy, 1, "stamps past the hour fall out");
        assert!(record_at(url, busy, 1_000 + WINDOW_SECS), "an hour later it warns again");
        assert!(!record_at(url, classify("duplicate: x"), 5_000), "a duplicate is no refusal");
    }
}
