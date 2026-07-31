// SPDX-License-Identifier: GPL-3.0-or-later

//! N2 (`docs/transport/nostr_n2_plan.md` §2): the pool runtime over
//! [`RelayWs`] — publish with ≥1-OK semantics today; subscriptions, cursors,
//! dedup, the EOSE gate and connection supervision land with the later plan
//! steps. The relay list ALWAYS arrives from `molt_core::relay::dialable`
//! (ADR-0004) — this module never reads the pool or decides dial policy.

use std::time::Duration;

use nostr::{ClientMessage, Event, RelayMessage};

use crate::dial::Dialer;
use crate::relay_ws::RelayWs;
use crate::NetError;

/// Per-relay deadline for one publish attempt (dial + upgrade + OK).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// How one publish went, per relay — ≥1 accepted relay makes the publish a
/// success, but the failures are always REPORTED, never hidden (concept
/// §11 N2: no silent partial).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublishReport {
    /// Relays that accepted the event (or already had it — see
    /// [`counts_as_published`]).
    pub accepted: Vec<String>,
    /// Relays that did not, with the honest reason.
    pub failed: Vec<(String, String)>,
}

/// NIP-01 outcome → did this event land on this relay? An `OK:false` whose
/// message starts with `duplicate:` means the relay ALREADY HOLDS the event —
/// for the delivery guarantee that IS "published", and treating it as an
/// error would make every rewind-resend look like a failure (MDK port #3,
/// `mdk_evaluation.md` §2.2).
fn counts_as_published(status: bool, message: &str) -> bool {
    status || message.starts_with("duplicate:")
}

/// The relay-pool runtime: everything the engine-facing transport will drive
/// (publish now; subscribe/cursor/health in later N2 steps).
#[derive(Debug, Clone)]
pub struct RelayRuntime {
    dialer: Dialer,
    /// Normalized relay URLs, in priority order — the output of
    /// `molt_core::relay::dialable(...)`, never a raw pool.
    urls: Vec<String>,
}

impl RelayRuntime {
    /// A runtime over the CURRENTLY dialable relays. An empty list is legal
    /// (a fresh install) — every operation then fails typed, and connects to
    /// nothing, silently (ADR-0004).
    pub fn new(dialer: Dialer, urls: Vec<String>) -> Self {
        Self { dialer, urls }
    }

    /// Publish one signed event to EVERY relay concurrently. Success iff at
    /// least one relay accepted (or already held) it; the per-relay outcomes
    /// ride in the report either way.
    pub async fn publish(&self, event: &Event) -> Result<PublishReport, NetError> {
        if self.urls.is_empty() {
            return Err(NetError::Unreachable(
                "no dialable relay — the pool is empty or gated".into(),
            ));
        }
        let attempts = self.urls.iter().map(|url| {
            let dialer = self.dialer.clone();
            let event = event.clone();
            async move {
                let outcome =
                    tokio::time::timeout(PUBLISH_TIMEOUT, publish_one(&dialer, url, &event))
                        .await
                        .unwrap_or_else(|_| {
                            Err(NetError::Unreachable("publish timed out".into()))
                        });
                (url.clone(), outcome)
            }
        });
        let mut report = PublishReport::default();
        for (url, outcome) in futures_util::future::join_all(attempts).await {
            match outcome {
                Ok(()) => report.accepted.push(url),
                Err(e) => report.failed.push((url, e.to_string())),
            }
        }
        if report.accepted.is_empty() {
            return Err(NetError::Unreachable(format!(
                "no relay accepted the event: {:?}",
                report.failed
            )));
        }
        Ok(report)
    }
}

/// One relay, one publish: connect, EVENT, await the OK for THIS event id.
async fn publish_one(dialer: &Dialer, url: &str, event: &Event) -> Result<(), NetError> {
    let mut ws = RelayWs::connect(dialer, url).await?;
    ws.send(ClientMessage::event(event.clone())).await?;
    let verdict = loop {
        match ws.recv(PUBLISH_TIMEOUT).await? {
            RelayMessage::Ok { event_id, status, message } if event_id == event.id => {
                break if counts_as_published(status, &message) {
                    Ok(())
                } else {
                    Err(NetError::Unreachable(format!("relay refused: {message}")))
                };
            }
            // frames for other events / notices are not our OK
            _ => {}
        }
    };
    ws.close().await;
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `duplicate:` rule pinned as a pure function, independent of any
    /// relay implementation's actual duplicate behavior.
    #[test]
    fn duplicate_ok_false_counts_as_published() {
        assert!(counts_as_published(true, ""));
        assert!(counts_as_published(false, "duplicate: already have this event"));
        assert!(!counts_as_published(false, "blocked: spam"));
        assert!(!counts_as_published(false, "invalid: bad sig"));
        // the prefix is exact — a message merely CONTAINING the word is a refusal
        assert!(!counts_as_published(false, "error: duplicate elsewhere"));
    }
}
