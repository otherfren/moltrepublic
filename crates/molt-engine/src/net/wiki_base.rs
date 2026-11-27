// SPDX-License-Identifier: GPL-3.0-or-later

//! **The folded wiki base on the file plane** (K6,
//! `docs_archive/memory/knowledge_base_scale.md` §4.9.7).
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
            self.files.wiki_base_fetching = None;
            // the wait is over; a later one is a new episode
            self.files.base_progress.remove("wiki");
            return;
        }
        let want = self.wiki_base_committed().map(|(hash, _)| hash);
        // a SECOND cut moved the commitment: the running fetch is after
        // bytes no holder keeps any more (each keeps the current tree
        // only), so it is abandoned rather than left to wait forever -
        // the new commitment starts a new episode
        if self.files.wiki_base_fetching != want {
            if let Some(h) = self.files.wiki_base_fetch.take() {
                h.abort();
            }
            self.files.wiki_base_fetching = None;
            self.files.wiki_base_next_try = 0;
            self.files.base_progress.remove("wiki");
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
        self.files.wiki_base_fetching = Some(series.hash.clone());
        let (Some(cmd_tx), Some(channel)) = (self.cmd_tx.upgrade(), self.nostr_file_channel())
        else {
            return;
        };
        tracing::info!(
            hash = %series.hash,
            size = series.size,
            "shared memory base: fetching"
        );
        // a day back, not the epoch: a holder may have published these
        // pieces for another fetcher minutes ago, and the catch-up window
        // set is bounded either way
        let from = crate::now_secs().saturating_sub(24 * 60 * 60);
        let progress = self.base_progress_start("wiki");
        self.files.base_progress.insert("wiki", progress.clone());
        self.files.wiki_base_fetch = Some(crate::transfer::spawn_wiki_base_fetch(
            channel,
            series.id,
            series.key,
            from,
            molt_net::file_plane::SeriesExpect {
                count: series.count,
                size: series.size,
                // no root to check against: the chain commits to the
                // CONTENT, not to the transport's framing (§4.9.1), and
                // the assembled bytes are checked against that commitment
                root: None,
                content: Some(series.hash.clone()),
            },
            self.net_scope,
            cmd_tx,
            progress,
        ));
    }

    /// The fetch finished: adopt the bytes if they answer the commitment,
    /// keep the tree on disk, and let the wiki answer again.
    pub(crate) fn cmd_net_wiki_base_fetched(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<Reply, MoltError> {
        self.files.base_fetch_logged = None;
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
        let state = format!("wiki:{reason}");
        if self.files.base_fetch_logged.as_deref() != Some(state.as_str()) {
            let hash = self.wiki_base_committed().map(|(h, _)| h).unwrap_or_default();
            tracing::info!(reason, %hash, retry_secs = RETRY_EVERY_SECS, "wiki_base=fetch_incomplete");
            self.files.base_fetch_logged = Some(state);
        }
        Ok(Reply::Ack)
    }

    /// A peer asks for pieces of the folded base: every holder answers.
    /// There is no election - the base has no registry of holders, and a
    /// node that answers what it holds is strictly better than an elected
    /// one that turns out to be base-pending itself. Returns whether this
    /// was a base request at all.
    pub(crate) fn serve_wiki_base_pieces(&mut self, from: &str, id: MessageId, ranges: &[(u32, u32)]) -> bool {
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
        let ranges = crate::net::files::bounded_want_ranges(ranges, layout.top);
        if ranges.is_empty() {
            return true;
        }
        self.note_base_wanted("wiki", &id, from);
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
        let job = molt_core::PublishJob {
            series: series.id.to_string(),
            key: series.key.to_vec(),
            path: String::new(),
            count: series.count,
            size: series.size,
            // the source is this holder's OWN base, so there is nothing to
            // compare a root against - the sender checks the size instead,
            // and the fetcher checks the assembled bytes against the chain
            root: String::new(),
            ranges,
            next: 0,
            started_at: crate::now_secs(),
            stored: false,
            wiki_base: true,
            vault: None,
            expires_at: None,
        };
        self.spawn_enqueue_publish(job);
    }
}

impl crate::State {
    /// A fresh progress view for an episode that began at `since` - a
    /// still-running episode carries its start across fetch retries.
    pub(crate) fn base_progress_start(&self, kind: &str) -> crate::transfer::BaseProgress {
        let since = self
            .files
            .base_progress
            .get(kind)
            .and_then(|p| p.lock().ok().map(|v| v.since).filter(|s| *s > 0))
            .unwrap_or_else(crate::now_secs);
        crate::transfer::base_progress(since)
    }

    /// A seat asked for a base this node holds: one WARN per seat.
    pub(crate) fn note_base_wanted(&mut self, kind: &str, id: &MessageId, from: &str) {
        let new = self.files.base_wanted_by.entry(id.to_string()).or_default().insert(from.to_string());
        if new {
            tracing::warn!(kind, from, "base=wanted");
        }
    }

    /// The bases this node serves, as its trickle reports them.
    pub(crate) fn base_serving_view(&self) -> Vec<molt_core::BaseServeView> {
        let Some(status) = self.group_net.as_ref().map(|g| g.trickle.status()) else {
            return Vec::new();
        };
        let series = [
            ("wiki", self.wiki_base_series().map(|s| s.id.to_string())),
            ("vault", self.vault_base_series().map(|s| s.id.to_string())),
        ];
        series
            .into_iter()
            .filter_map(|(kind, id)| {
                let id = id?;
                let v = status.get(&id)?;
                Some(molt_core::BaseServeView {
                    kind: kind.to_string(),
                    to: self.files.base_wanted_by.get(&id).map(|s| s.iter().cloned().collect()).unwrap_or_default(),
                    published: v.published,
                    of: v.of,
                    gate: v.gate.clone(),
                    at: v.at,
                })
            })
            .collect()
    }

    /// The bases this node waits for, with what the running fetch holds.
    pub(crate) fn base_fetch_view(&self) -> Vec<molt_core::BaseFetchView> {
        let pending = [("wiki", self.wiki_base_pending()), ("vault", self.vault_base_pending())];
        pending
            .into_iter()
            .filter(|(_, p)| *p)
            .map(|(kind, _)| {
                let p = self.files.base_progress.get(kind).and_then(|p| p.lock().ok().map(|v| v.clone()));
                molt_core::BaseFetchView { kind: kind.to_string(), ..p.unwrap_or_default() }
            })
            .collect()
    }
}
