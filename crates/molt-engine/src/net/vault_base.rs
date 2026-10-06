// SPDX-License-Identifier: GPL-3.0-or-later

//! **The folded vault base on the file plane** (vault spec §9.3, plan
//! stage S5): the wiki base's twin. The series id and key derive from the
//! commitment ([`molt_net::file_plane::vault_base_series`]), every holder
//! answers, and the assembled bytes are adopted only after the hash check
//! AND a full re-verification under the base's own vault context.

use molt_core::MessageId;
use molt_net::supervisor::StateStore as _;

/// How long the beat waits before the next attempt.
const RETRY_EVERY_SECS: u64 = 15;

/// The base fetch of the open workspace (at most one).
#[derive(Default)]
pub(crate) struct VaultBasePlane {
    /// The running fetch.
    pub(crate) fetch: Option<tokio::task::AbortHandle>,
    /// Unix seconds before which no new fetch starts.
    pub(crate) next_try: u64,
    /// The commitment the running fetch is after.
    pub(crate) fetching: Option<String>,
}

/// The folded vault base as the plane addresses it.
pub(crate) struct VaultBaseSeries {
    pub(crate) hash: String,
    pub(crate) id: MessageId,
    pub(crate) key: [u8; 32],
    pub(crate) size: u64,
    pub(crate) count: u32,
}

impl crate::State {
    /// `None` where nothing commits to a base or the plane is unreachable.
    pub(crate) fn vault_base_series(&self) -> Option<VaultBaseSeries> {
        let (hash, size) = self.vault_base_committed()?;
        let nostr = self.nostr.as_ref()?;
        Some(VaultBaseSeries {
            key: molt_net::file_plane::vault_base_key(&nostr.rotation_seed, &hash),
            id: MessageId(molt_net::file_plane::vault_base_series(&hash)),
            count: molt_net::file_plane::Manifest::piece_count_for(size),
            hash,
            size,
        })
    }

    /// Base-pending: keep exactly one fetch running; never gives up.
    pub(crate) fn vault_base_tick(&mut self) {
        if !self.vault_base_pending() {
            if let Some(h) = self.files.vault_base.fetch.take() {
                h.abort();
            }
            self.files.vault_base.fetching = None;
            return;
        }
        let want = self.vault_base_committed().map(|(hash, _)| hash);
        if self.files.vault_base.fetching != want {
            if let Some(h) = self.files.vault_base.fetch.take() {
                h.abort();
            }
            self.files.vault_base.fetching = None;
            self.files.vault_base.next_try = 0;
        }
        if self.files.vault_base.fetch.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let now = crate::now_secs();
        if now < self.files.vault_base.next_try {
            return;
        }
        self.files.vault_base.next_try = now.saturating_add(RETRY_EVERY_SECS);
        let Some(series) = self.vault_base_series() else {
            return;
        };
        self.files.vault_base.fetching = Some(series.hash.clone());
        let (Some(cmd_tx), Some(channel)) = (self.cmd_tx.upgrade(), self.nostr_file_channel()) else {
            return;
        };
        tracing::info!(hash = %series.hash, size = series.size, "vault_base=fetching");
        let from = crate::now_secs().saturating_sub(24 * 60 * 60);
        self.files.vault_base.fetch = Some(crate::transfer::spawn_vault_base_fetch(
            channel,
            series.id,
            series.key,
            from,
            molt_net::file_plane::SeriesExpect { count: series.count, size: series.size, root: None },
            self.net_scope,
            cmd_tx,
        ));
    }

    /// A peer wants pieces of the vault base: every holder answers.
    /// Returns whether `id` is the base series at all.
    pub(crate) fn serve_vault_base_pieces(&mut self, id: MessageId, ranges: &[(u32, u32)]) -> bool {
        let Some(series) = self.vault_base_series() else {
            return false;
        };
        if series.id != id {
            return false;
        }
        if self.vault_base_pending() {
            return true;
        }
        let Some(layout) = molt_net::file_plane::Manifest::layout_for(series.count) else {
            return true;
        };
        let ranges: Vec<(u32, u32)> = ranges
            .iter()
            .copied()
            .filter(|(lo, hi)| lo <= hi && *hi <= layout.top)
            .take(molt_net::piece_want::PIECE_WANT_MAX_RANGES)
            .collect();
        if !ranges.is_empty() {
            tracing::debug!(%id, ranges = ranges.len(), "vault_base=wanted");
            self.enqueue_vault_base_publish(&series, ranges);
        }
        true
    }

    fn enqueue_vault_base_publish(&mut self, series: &VaultBaseSeries, ranges: Vec<(u32, u32)>) {
        let Some(store) = self.file_store() else {
            return;
        };
        let waker = self.group_net.as_ref().map(|g| g.trickle.waker());
        let job = molt_core::PublishJob {
            series: series.id.to_string(),
            key: series.key.to_vec(),
            // no file name: the trickle reads the sealed vault_base.bin
            path: String::new(),
            count: series.count,
            size: series.size,
            root: String::new(),
            ranges,
            next: 0,
            started_at: crate::now_secs(),
            stored: false,
            wiki_base: false,
            vault: Some(series.hash.clone()),
        };
        tokio::spawn(async move {
            store.update(|s| molt_net::trickle::enqueue_publish(s, job)).await;
            if let Some(w) = waker {
                w.notify_one();
            }
        });
    }
}
