// SPDX-License-Identifier: GPL-3.0-or-later

//! At most one log line per key per minute, carrying how many events it
//! stands for and from how many addresses. The first event speaks at once;
//! what the window held is flushed when it closes (the accept loop asks
//! [`LogThrottle::next_due`]), so a burst followed by silence is counted.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
/// Distinct addresses tracked per window; past it `sources` reads the bound.
const MAX_SOURCES: usize = 1_024;

/// One key's due line: events since the previous line (this one included),
/// the distinct source addresses among them, and the seconds since that
/// line (0 = the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    pub(crate) count: u64,
    pub(crate) sources: usize,
    pub(crate) window_s: u64,
}

#[derive(Default)]
struct Slot {
    last_line: Option<Instant>,
    pending: u64,
    sources: HashSet<IpAddr>,
}

impl Slot {
    fn take(&mut self, now: Instant) -> Due {
        let since = self.last_line.map_or(0, |at| now.saturating_duration_since(at).as_secs());
        let due = Due { count: self.pending, sources: self.sources.len(), window_s: since };
        self.pending = 0;
        self.sources.clear();
        self.last_line = Some(now);
        due
    }
}

/// Shared by every connection of one listener.
pub(crate) struct LogThrottle {
    slots: Mutex<HashMap<&'static str, Slot>>,
    window: Duration,
}

impl Default for LogThrottle {
    fn default() -> Self {
        Self::with_window(WINDOW)
    }
}

impl LogThrottle {
    pub(crate) fn with_window(window: Duration) -> Self {
        Self { slots: Mutex::new(HashMap::new()), window }
    }

    /// Count one `key` event from `source`; `Some` = log a line for it now.
    pub(crate) fn hit(&self, key: &'static str, source: Option<IpAddr>) -> Option<Due> {
        self.hit_at(key, source, Instant::now())
    }

    fn hit_at(&self, key: &'static str, source: Option<IpAddr>, now: Instant) -> Option<Due> {
        let mut slots = self.slots.lock().ok()?;
        let slot = slots.entry(key).or_default();
        slot.pending = slot.pending.saturating_add(1);
        if let Some(ip) = source {
            if slot.sources.len() < MAX_SOURCES {
                slot.sources.insert(ip);
            }
        }
        if slot.last_line.is_some_and(|at| now.saturating_duration_since(at) < self.window) {
            return None;
        }
        Some(slot.take(now))
    }

    /// When the earliest held count is due ( `None` = nothing held).
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let slots = self.slots.lock().ok()?;
        slots
            .values()
            .filter(|s| s.pending > 0)
            .filter_map(|s| s.last_line.map(|at| at + self.window))
            .min()
    }

    /// The held counts whose window closed.
    pub(crate) fn flush_due(&self) -> Vec<(&'static str, Due)> {
        self.flush_due_at(Instant::now())
    }

    fn flush_due_at(&self, now: Instant) -> Vec<(&'static str, Due)> {
        let Ok(mut slots) = self.slots.lock() else { return Vec::new() };
        let window = self.window;
        slots
            .iter_mut()
            .filter(|(_, s)| s.pending > 0 && s.last_line.is_some_and(|at| now.saturating_duration_since(at) >= window))
            .map(|(k, s)| (*k, s.take(now)))
            .collect()
    }
}

/// The line for `key` (`http`/`line` = connects, `refused_*` = refusals);
/// `peer` names the event that spoke at once, a flushed count has none.
pub(crate) fn emit(key: &'static str, d: Due, peer: Option<std::net::SocketAddr>) {
    let what = match key {
        "http" | "line" => {
            tracing::info!(via = key, count = d.count, window_s = d.window_s, "mcp_clients");
            return;
        }
        "refused_bearer" => "MCP HTTP refused: missing or invalid bearer token",
        "refused_allowlist" => "MCP connection refused: peer IP not on the allowlist",
        "refused_full" => "MCP connection refused: too many open",
        _ => "MCP refused",
    };
    match peer {
        Some(peer) => tracing::warn!(%peer, count = d.count, sources = d.sources, window_s = d.window_s, "{what}"),
        None => tracing::warn!(count = d.count, sources = d.sources, window_s = d.window_s, "{what}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_per_minute_with_the_count_and_none_when_idle() {
        let t = LogThrottle::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(t.hit_at("http", None, at(0)), Some(Due { count: 1, sources: 0, window_s: 0 }), "the first speaks at once");
        for s in 1..60 {
            assert_eq!(t.hit_at("http", None, at(s)), None, "folded at {s}s");
        }
        assert_eq!(t.hit_at("line", None, at(30)), Some(Due { count: 1, sources: 0, window_s: 0 }), "keys are apart");
        assert_eq!(t.hit_at("http", None, at(60)), Some(Due { count: 60, sources: 0, window_s: 60 }));
        // idle: nothing is held, nothing is due
        assert_eq!(t.flush_due_at(at(600)), Vec::new());
        assert_eq!(t.hit_at("http", None, at(600)), Some(Due { count: 1, sources: 0, window_s: 540 }));
    }

    /// A burst followed by silence is flushed when its window closes, with
    /// the number of distinct addresses: one brute-forcing address reads
    /// apart from many clients.
    #[test]
    fn a_burst_then_silence_is_flushed_with_its_sources() {
        let t = LogThrottle::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let one: IpAddr = "10.0.0.1".parse().expect("ip");
        let two: IpAddr = "10.0.0.2".parse().expect("ip");
        assert_eq!(t.hit_at("bearer", Some(one), at(0)), Some(Due { count: 1, sources: 1, window_s: 0 }));
        for s in 1..=4 {
            assert_eq!(t.hit_at("bearer", Some(one), at(s)), None);
        }
        t.hit_at("bearer", Some(two), at(5));
        assert_eq!(t.next_due(), Some(at(60)));
        assert_eq!(t.flush_due_at(at(59)), Vec::new(), "the window is still open");
        assert_eq!(t.flush_due_at(at(60)), vec![("bearer", Due { count: 5, sources: 2, window_s: 60 })]);
        assert_eq!(t.next_due(), None, "nothing held after the flush");
        assert_eq!(t.flush_due_at(at(200)), Vec::new());
    }
}
