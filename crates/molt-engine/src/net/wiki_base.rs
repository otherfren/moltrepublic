// SPDX-License-Identifier: GPL-3.0-or-later

//! **The folded wiki base on the file plane** (K6,
//! `docs/memory/knowledge_base_scale.md` §4.9.7).
//!
//! A folded cut names the ratified wiki by content hash and drops the
//! patches that produced it. The bytes travel here: the same piece plane
//! shared files use, but a series of its own - content-addressed, so every
//! holder publishes into it and a base-pending node takes pieces from
//! whoever is online.
//!
//! Two things make that work without a holder registry: the series id and
//! the key are both DERIVED from the commitment
//! ([`molt_net::file_plane::wiki_base_key`]), and the assembled bytes are
//! checked against the chain's own commitment, which is threshold-signed.
//! A wrong piece costs a retry, never a wrong wiki.

use molt_net::supervisor::StateStore as _;

use molt_core::{MessageId, MoltError, Reply};

/// How long the beat waits before starting the next attempt. A fetch with
/// no holder online ends in seconds; the base is not urgent enough to ask
/// the whole group every second, and not optional enough to give up.
const RETRY_EVERY_SECS: u64 = 15;

/// The folded base as the file plane addresses it.
pub(crate) struct WikiBaseSeries {
    /// The chain's commitment (hex sha256 over the canonical bytes).
    pub(crate) hash: String,
    /// The series id, derived from the commitment.
    pub(crate) id: MessageId,
    /// The key every holder derives for this series.
    pub(crate) key: [u8; 32],
    /// The committed byte length.
    pub(crate) size: u64,
    /// Data pieces.
    pub(crate) count: u32,
}

impl crate::State {
    /// The folded base's identity on the plane - `None` where nothing
    /// commits to one, or where this node cannot address the plane at all.
    pub(crate) fn wiki_base_series(&self) -> Option<WikiBaseSeries> {
        let (hash, size) = self.wiki_base_committed()?;
        let nostr = self.nostr.as_ref()?;
        let raw = molt_net::file_plane::wiki_base_series(&hash)?;
        Some(WikiBaseSeries {
            key: molt_net::file_plane::wiki_base_key(&nostr.rotation_seed, &hash),
            id: MessageId(raw),
            count: molt_net::file_plane::Manifest::piece_count_for(size),
            hash,
            size,
        })
    }

    /// Base-pending: keep exactly ONE fetch of the folded base running.
    /// Runs on the file plane's own beat; a completed or failed fetch
    /// simply starts again next beat, which is what "never times out, never
    /// gives up" means here (§4.9.9).
    pub(crate) fn wiki_base_tick(&mut self) {
        if self.chain.wiki_base.is_some() {
            return;
        }
        if self
            .files
            .wiki_base_fetch
            .as_ref()
            .is_some_and(|h| !h.is_finished())
        {
            return;
        }
        let now = crate::now_secs();
        if now < self.files.wiki_base_next_try {
            return;
        }
        self.files.wiki_base_next_try = now.saturating_add(RETRY_EVERY_SECS);
        let Some(series) = self.wiki_base_series() else {
            return;
        };
        let (Some(cmd_tx), Some(channel)) = (self.cmd_tx.upgrade(), self.nostr_file_channel())
        else {
            return;
        };
        tracing::info!(
            hash = %series.hash,
            size = series.size,
            "shared memory base: fetching"
        );
        self.files.wiki_base_fetch = Some(crate::transfer::spawn_wiki_base_fetch(
            channel,
            series.id,
            series.key,
            molt_net::file_plane::SeriesExpect {
                count: series.count,
                size: series.size,
                // no root to check against: the chain commits to the
                // CONTENT, not to the transport's framing (§4.9.1), and
                // the assembled bytes are checked against that commitment
                root: None,
            },
            self.net_scope,
            cmd_tx,
        ));
    }

    /// The fetch finished: adopt the bytes if they answer the commitment,
    /// keep the tree on disk, and let the wiki answer again.
    pub(crate) fn cmd_net_wiki_base_fetched(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<Reply, MoltError> {
        self.adopt_wiki_base(Some(bytes));
        let Some(tree) = self.chain.wiki_base.clone() else {
            tracing::warn!("shared memory base: the fetched bytes are not the committed base");
            return Ok(Reply::Ack);
        };
        if !self.persist_wiki_base(Some(&tree)) {
            // in memory it still works; the next open re-fetches
            tracing::warn!("shared memory base: fetched, but it did not reach the disk");
        }
        self.bump_applied_epoch();
        self.spawn_wiki_index_build();
        self.emit_session(molt_core::SessionScope::Full);
        tracing::info!(docs = tree.len(), "shared memory base: complete");
        Ok(Reply::Ack)
    }

    /// The fetch ended without the tree. Nothing to do but say so: the
    /// beat starts the next one, and the surface keeps showing the
    /// pending state rather than an empty wiki.
    pub(crate) fn cmd_net_wiki_base_failed(&mut self, reason: &str) -> Result<Reply, MoltError> {
        tracing::debug!(reason, "shared memory base: fetch did not complete");
        Ok(Reply::Ack)
    }

    /// A peer asks for pieces of the folded base: every holder answers.
    /// There is no election - the base has no registry of holders, and a
    /// node that answers what it holds is strictly better than an elected
    /// one that turns out to be base-pending itself. Returns whether this
    /// was a base request at all.
    pub(crate) fn serve_wiki_base_pieces(&mut self, id: MessageId, ranges: &[(u32, u32)]) -> bool {
        let Some(series) = self.wiki_base_series() else {
            return false;
        };
        if series.id != id {
            return false;
        }
        if self.chain.wiki_base.is_none() {
            return true; // ours to answer, but we are waiting for it too
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
        if ranges.is_empty() {
            return true;
        }
        tracing::debug!(%id, ranges = ranges.len(), "shared memory base: pieces wanted");
        self.enqueue_wiki_base_publish(&series, ranges);
        true
    }

    /// Queue base pieces for the trickle sender. The sender reads them one
    /// by one out of the sealed store, so the wiki is never written out in
    /// plaintext to be published.
    pub(crate) fn enqueue_wiki_base_publish(
        &mut self,
        series: &WikiBaseSeries,
        ranges: Vec<(u32, u32)>,
    ) {
        let Some(store) = self.file_store() else {
            return;
        };
        let waker = self.group_net.as_ref().map(|g| g.trickle.waker());
        let job = molt_core::PublishJob {
            series: series.id.to_string(),
            key: series.key.to_vec(),
            path: String::new(),
            count: series.count,
            size: series.size,
            root: self.wiki_base_root(series),
            ranges,
            next: 0,
            started_at: crate::now_secs(),
            stored: false,
            wiki_base: true,
        };
        tokio::spawn(async move {
            let mut queued = false;
            store
                .update(|s| {
                    queued = molt_net::trickle::enqueue_publish(s, job);
                    queued
                })
                .await;
            if let Some(w) = waker {
                w.notify_one();
            }
        });
    }

    /// The manifest root of this holder's own base - transport framing,
    /// computed here rather than carried in the chain (§4.9.1).
    fn wiki_base_root(&self, series: &WikiBaseSeries) -> String {
        let Some(tree) = self.chain.wiki_base.as_ref() else {
            return String::new();
        };
        let bytes = molt_core::wiki_fold::wiki_base_canonical_bytes(tree);
        let mut hashes = Vec::new();
        for slice in bytes.chunks(molt_net::file_plane::PIECE_PAYLOAD_LEN) {
            hashes.push(<[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(slice)));
        }
        molt_net::file_plane::Manifest {
            count: series.count,
            size: series.size,
            hashes,
        }
        .root()
    }
}
