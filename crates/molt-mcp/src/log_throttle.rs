// SPDX-License-Identifier: GPL-3.0-or-later

//! At most one log line per key per minute, carrying how many events it
//! stands for. Folded lazily on the event path: no timer, nothing when idle.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);

/// One key's due line: events since the previous line (this one included)
/// and the seconds since that line (0 = the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    pub(crate) count: u64,
    pub(crate) window_s: u64,
}

#[derive(Default)]
struct Slot {
    last_line: Option<Instant>,
    pending: u64,
}

/// Shared by every connection of one listener.
#[derive(Default)]
pub(crate) struct LogThrottle {
    slots: Mutex<HashMap<&'static str, Slot>>,
}

impl LogThrottle {
    /// Count one `key` event; `Some` = log a line for it now.
    pub(crate) fn hit(&self, key: &'static str) -> Option<Due> {
        self.hit_at(key, Instant::now())
    }

    fn hit_at(&self, key: &'static str, now: Instant) -> Option<Due> {
        let mut slots = self.slots.lock().ok()?;
        let slot = slots.entry(key).or_default();
        slot.pending = slot.pending.saturating_add(1);
        let since = slot.last_line.map(|at| now.saturating_duration_since(at));
        if since.is_some_and(|s| s < WINDOW) {
            return None;
        }
        let due = Due { count: slot.pending, window_s: since.map_or(0, |s| s.as_secs()) };
        slot.pending = 0;
        slot.last_line = Some(now);
        Some(due)
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
        assert_eq!(t.hit_at("http", at(0)), Some(Due { count: 1, window_s: 0 }), "the first speaks at once");
        for s in 1..60 {
            assert_eq!(t.hit_at("http", at(s)), None, "folded at {s}s");
        }
        assert_eq!(t.hit_at("line", at(30)), Some(Due { count: 1, window_s: 0 }), "keys are apart");
        assert_eq!(t.hit_at("http", at(60)), Some(Due { count: 60, window_s: 60 }));
        // idle: nothing is emitted; the next event carries the gap
        assert_eq!(t.hit_at("http", at(600)), Some(Due { count: 1, window_s: 540 }));
    }
}
