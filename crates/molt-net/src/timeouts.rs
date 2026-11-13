//! The relay deadlines (`[transport.nostr] connect_timeout_secs` /
//! `publish_timeout_secs`). Set once at start from the config; every relay
//! runtime, publish pool and dial built afterwards reads them. Tests pin
//! their own per instance (`with_timeouts`) instead of touching the process
//! value.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Default for opening a relay connection: dial (TCP or a Tor circuit, an
/// onion rendezvous included), TLS and the WebSocket upgrade. An onion
/// relay measured 12 s on a live node (2026-11-13), so the old 10 s publish
/// bound abandoned every dial before it finished.
pub const DEFAULT_CONNECT_SECS: u64 = 60;

/// Default for one EVENT → OK round-trip on a live socket.
pub const DEFAULT_PUBLISH_SECS: u64 = 30;

/// Accepted range for both keys; a value outside it falls back to the default.
pub const RANGE_SECS: std::ops::RangeInclusive<u64> = 5..=600;

static CONNECT_SECS: AtomicU64 = AtomicU64::new(DEFAULT_CONNECT_SECS);
static PUBLISH_SECS: AtomicU64 = AtomicU64::new(DEFAULT_PUBLISH_SECS);

/// The two relay deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayTimeouts {
    /// Dial + TLS + WebSocket upgrade.
    pub connect: Duration,
    /// EVENT → OK on a live socket.
    pub publish: Duration,
}

impl RelayTimeouts {
    /// The process values (the config's, or the defaults).
    #[must_use]
    pub fn configured() -> Self {
        Self {
            connect: Duration::from_secs(CONNECT_SECS.load(Ordering::Relaxed)),
            publish: Duration::from_secs(PUBLISH_SECS.load(Ordering::Relaxed)),
        }
    }
}

/// `secs` when inside [`RANGE_SECS`], else `default`.
#[must_use]
pub fn sane_secs(secs: u64, default: u64) -> u64 {
    if RANGE_SECS.contains(&secs) {
        secs
    } else {
        default
    }
}

/// Install the config's values for this process (out-of-range values fall
/// back to the defaults). Called once at start, before any relay is dialed.
pub fn set_relay_timeouts(connect_secs: u64, publish_secs: u64) {
    CONNECT_SECS.store(sane_secs(connect_secs, DEFAULT_CONNECT_SECS), Ordering::Relaxed);
    PUBLISH_SECS.store(sane_secs(publish_secs, DEFAULT_PUBLISH_SECS), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_falls_back_to_the_default() {
        assert_eq!(sane_secs(90, DEFAULT_CONNECT_SECS), 90);
        assert_eq!(sane_secs(0, DEFAULT_CONNECT_SECS), DEFAULT_CONNECT_SECS);
        assert_eq!(sane_secs(4, DEFAULT_PUBLISH_SECS), DEFAULT_PUBLISH_SECS);
        assert_eq!(sane_secs(601, DEFAULT_PUBLISH_SECS), DEFAULT_PUBLISH_SECS);
        assert_eq!(sane_secs(600, DEFAULT_PUBLISH_SECS), 600);
    }
}
