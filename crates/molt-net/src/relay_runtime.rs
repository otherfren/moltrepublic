// SPDX-License-Identifier: GPL-3.0-or-later

//! N2 (`docs/transport/nostr_n2_plan.md` §2): the pool runtime over
//! [`RelayWs`] — publish with ≥1-OK semantics today; subscriptions, cursors,
//! dedup, the EOSE gate and connection supervision land with the later plan
//! steps. The relay list ALWAYS arrives from `molt_core::relay::dialable`
//! (ADR-0004) — this module never reads the pool or decides dial policy.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use nostr::{ClientMessage, Event, EventId, Filter, RelayMessage, SubscriptionId};
use tokio::sync::{mpsc, Mutex};

use crate::dial::Dialer;
use crate::relay_ws::RelayWs;
use crate::NetError;

/// Per-relay deadline for one publish attempt (dial + upgrade + OK).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// A subscription reader's per-frame idle bound. Liveness/reconnect proper is
/// plan step 7 — until then an hour of silence ends the reader instead of
/// pinning a task forever.
const SUB_IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Dedup ring capacity: enough for the WP4a-horizon backlog of a busy
/// republic, bounded so a hostile relay cannot balloon memory.
const DEDUP_CAP: usize = 4096;

/// Clock-skew margin for the cursor clamp: a delivered event may advance the
/// cursor at most this far past LOCAL now (concept §4.4's ±1h skew).
const CURSOR_SKEW: u64 = 3_600;

/// Re-subscribe overlap: `since = cursor − OVERLAP`, the full NIP-59
/// timestamp-tweak width — without it, offline gift-wraps are permanently
/// skipped (MDK port #2, `mdk_evaluation.md` §2.2).
const CURSOR_OVERLAP: u64 = 172_800;

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
    /// Per-relay resume cursor: the max CLAMPED `created_at` delivered by
    /// that relay. What a reopen persists and reseeds ([`Self::with_cursors`]).
    cursors: Arc<Mutex<std::collections::HashMap<String, u64>>>,
}

impl RelayRuntime {
    /// A runtime over the CURRENTLY dialable relays. An empty list is legal
    /// (a fresh install) — every operation then fails typed, and connects to
    /// nothing, silently (ADR-0004).
    pub fn new(dialer: Dialer, urls: Vec<String>) -> Self {
        Self { dialer, urls, cursors: Arc::new(Mutex::new(std::collections::HashMap::new())) }
    }

    /// Reseed the per-relay cursors (the reopen path — cursors come from the
    /// persisted transport state).
    #[must_use]
    pub fn with_cursors(self, cursors: std::collections::HashMap<String, u64>) -> Self {
        Self { cursors: Arc::new(Mutex::new(cursors)), ..self }
    }

    /// The current per-relay cursors — what a close persists.
    pub async fn cursors(&self) -> std::collections::HashMap<String, u64> {
        self.cursors.lock().await.clone()
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

/// The bounded first-seen event-id ring behind the subscription fan-in:
/// relay-count copies collapse to ONE delivery.
struct DedupRing {
    seen: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl DedupRing {
    fn new() -> Self {
        Self { seen: HashSet::new(), order: VecDeque::new() }
    }

    /// `true` iff this id was not seen before (and is now recorded).
    fn fresh(&mut self, id: EventId) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > DEDUP_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

/// A pooled subscription: one filter, every relay, ONE deduplicated stream
/// out. Dropping it aborts the per-relay readers (the sanctioned inbound
/// abort — outbound draining rules don't apply to pure readers).
pub struct Subscription {
    rx: mpsc::Receiver<Event>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl Subscription {
    /// The next deduplicated event, or `None` if nothing arrived within
    /// `timeout` (also `None` once every reader has ended and the buffer is
    /// drained).
    pub async fn recv(&mut self, timeout: Duration) -> Option<Event> {
        tokio::time::timeout(timeout, self.rx.recv()).await.ok().flatten()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
    }
}

impl RelayRuntime {
    /// Subscribe the SAME filter on every reachable relay and fan the
    /// deliveries into one deduplicated stream. Succeeds if at least one
    /// relay accepted the REQ; unreachable relays are skipped (step 7 adds
    /// reconnect/health).
    pub async fn subscribe(&self, filter: Filter) -> Result<Subscription, NetError> {
        if self.urls.is_empty() {
            return Err(NetError::Unreachable(
                "no dialable relay — the pool is empty or gated".into(),
            ));
        }
        let (tx, rx) = mpsc::channel(256);
        let dedup = Arc::new(Mutex::new(DedupRing::new()));
        let stored = self.cursors.lock().await.clone();
        let mut readers = Vec::new();
        for url in &self.urls {
            let Ok(mut ws) = RelayWs::connect(&self.dialer, url).await else {
                continue;
            };
            // resume from this relay's cursor, widened by the overlap — the
            // clamp on the WRITE side (below) is what keeps this `since`
            // honest against future-dated events
            let mut relay_filter = filter.clone();
            if let Some(cursor) = stored.get(url) {
                relay_filter = relay_filter.since(nostr::Timestamp::from_secs(
                    cursor.saturating_sub(CURSOR_OVERLAP),
                ));
            }
            let sub_id = SubscriptionId::generate();
            if ws
                .send(ClientMessage::req(sub_id, vec![relay_filter]))
                .await
                .is_err()
            {
                continue;
            }
            let tx = tx.clone();
            let dedup = dedup.clone();
            let cursors = self.cursors.clone();
            let url = url.clone();
            readers.push(tokio::spawn(async move {
                loop {
                    match ws.recv(SUB_IDLE_TIMEOUT).await {
                        Ok(RelayMessage::Event { event, .. }) => {
                            // this relay DELIVERED the event, so its cursor
                            // advances (duplicates included) — but never past
                            // local now + skew: a +24h `created_at` must not
                            // blind the next reopen (concept §4.3)
                            let clamp = nostr::Timestamp::now().as_secs() + CURSOR_SKEW;
                            let stamp = event.created_at.as_secs().min(clamp);
                            {
                                let mut c = cursors.lock().await;
                                let entry = c.entry(url.clone()).or_insert(0);
                                *entry = (*entry).max(stamp);
                            }
                            let fresh = dedup.lock().await.fresh(event.id);
                            if fresh && tx.send(event.into_owned()).await.is_err() {
                                break; // consumer gone
                            }
                        }
                        // EOSE/OK/NOTICE etc. — the EOSE gate is plan step 5
                        Ok(_) => {}
                        // closed or idle past the bound — reader ends
                        Err(_) => break,
                    }
                }
            }));
        }
        if readers.is_empty() {
            return Err(NetError::Unreachable(
                "no relay accepted the subscription".into(),
            ));
        }
        Ok(Subscription { rx, readers })
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
