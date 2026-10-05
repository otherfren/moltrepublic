// SPDX-License-Identifier: GPL-3.0-or-later

//! **Vault payload files on the file plane** (plan stage S3a, spec §9.2).
//!
//! Every seat of a vault republic holds every payload a deposit names -
//! committed, pending, or replaced but still above the anchor (plan
//! 1.3.14). Holding is mandatory: outside the mirror consent, the mirror
//! quota and the file cap. Each payload is its own content-addressed series
//! ([`molt_net::file_plane::vault_payload_series`]), so a missing seat takes
//! pieces from whoever holds them, and the assembled bytes are checked
//! against the hash the deposit commits to. This tick fetches; it never
//! deletes - a payload retires only at a cut (S5).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use molt_core::vault::{VaultOp, VAULT_PAYLOAD_MAX};
use molt_core::{MessageId, MoltError, ProposalState, Reply, SessionScope, Surface};
use molt_net::supervisor::StateStore as _;
use serde_json::Value;

/// How long the beat waits before the next attempt at one payload.
const RETRY_EVERY_SECS: u64 = 15;

/// A payload file a deposit names: the text cap plus the AEAD.
fn payload_file_max() -> u64 {
    u64::try_from(VAULT_PAYLOAD_MAX + molt_vault::PAYLOAD_OVERHEAD).unwrap_or(u64::MAX)
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A payload this seat must hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedPayload {
    /// The deposit version (the sink's file name).
    pub(crate) secret_id: String,
    /// SHA-256 of the ciphertext, hex.
    pub(crate) hash: String,
    /// The ciphertext length.
    pub(crate) size: u64,
}

impl NamedPayload {
    fn series(&self) -> MessageId {
        MessageId(molt_net::file_plane::vault_payload_series(&self.hash))
    }

    fn count(&self) -> u32 {
        molt_net::file_plane::Manifest::piece_count_for(self.size)
    }
}

/// One payload fetch: the running task and when the next may start.
pub(crate) struct VaultFetch {
    pub(crate) handle: Option<tokio::task::AbortHandle>,
    pub(crate) next_try: u64,
}

/// The payload plane of the open workspace.
#[derive(Default)]
pub(crate) struct VaultPlane {
    /// The `secret_id`s held on disk, valid for [`Self::held_scope`].
    pub(crate) held: BTreeSet<String>,
    /// The workspace incarnation (`net_scope`) `held` was listed for.
    pub(crate) held_scope: Option<u64>,
    /// Fetches by payload hash.
    pub(crate) fetches: BTreeMap<String, VaultFetch>,
    /// Payloads named through the test seam.
    pub(crate) seam_named: Vec<NamedPayload>,
}

/// The handle's vault test seams; never a `Command`.
#[derive(Default)]
pub(crate) struct VaultSeams {
    pub(crate) hold_fetch: AtomicBool,
    staged: Mutex<Vec<(NamedPayload, Option<Vec<u8>>)>>,
}

impl VaultSeams {
    pub(crate) fn name(&self, secret_id: &str, hash: &str, size: u64, bytes: Option<Vec<u8>>) {
        let named = NamedPayload { secret_id: secret_id.to_string(), hash: hash.to_string(), size };
        if let Ok(mut staged) = self.staged.lock() {
            staged.push((named, bytes));
        }
    }

    fn take(&self) -> Vec<(NamedPayload, Option<Vec<u8>>)> {
        self.staged.lock().map(|mut s| std::mem::take(&mut *s)).unwrap_or_default()
    }
}

/// The payload a deposit op names, if it is one and well-formed.
fn deposit_named(payload: &Value, republic_id: &str) -> Option<NamedPayload> {
    let Ok(VaultOp::Deposit(dep)) = serde_json::from_value::<VaultOp>(payload.clone()) else {
        return None;
    };
    if !is_hex64(&dep.payload.hash) || dep.payload.size == 0 || dep.payload.size > payload_file_max() {
        return None;
    }
    Some(NamedPayload {
        secret_id: molt_core::vault::secret_id(republic_id, &dep),
        hash: dep.payload.hash,
        size: dep.payload.size,
    })
}

impl crate::State {
    /// Every payload this seat must hold: named by a committed deposit
    /// block (current or replaced, all above the anchor) or a pending
    /// deposit proposal. Empty outside a vault republic.
    pub(crate) fn vault_named_payloads(&self) -> Vec<NamedPayload> {
        if !self.is_vault_republic() {
            return Vec::new();
        }
        let rid = self.republic_id();
        let pending = self
            .proposals
            .values()
            .filter(|p| p.surface == Surface::Vault && p.state == ProposalState::Proposed && !p.withdrawn)
            .map(|p| &p.payload);
        let mut out: BTreeMap<String, NamedPayload> = BTreeMap::new();
        for named in self
            .applied_payloads(Surface::Vault)
            .chain(pending)
            .filter_map(|v| deposit_named(v, &rid))
            .chain(self.files.vault.seam_named.iter().cloned())
        {
            out.entry(named.secret_id.clone()).or_insert(named);
        }
        out.into_values().collect()
    }

    /// Whether this seat holds the payload of `secret_id`.
    pub(crate) fn vault_payload_held(&self, secret_id: &str) -> bool {
        self.files.vault.held_scope == Some(self.net_scope) && self.files.vault.held.contains(secret_id)
    }

    /// List the held payloads once per workspace incarnation; a new one
    /// abandons the old one's fetches.
    pub(crate) fn vault_sync_held(&mut self) {
        if self.files.vault.held_scope == Some(self.net_scope) {
            return;
        }
        for (_, mut f) in std::mem::take(&mut self.files.vault.fetches) {
            if let Some(h) = f.handle.take() {
                h.abort();
            }
        }
        self.files.vault.held = self
            .active
            .as_ref()
            .map(|a| molt_storage::list_vault_payloads(&a.dir).into_iter().collect())
            .unwrap_or_default();
        self.files.vault.held_scope = Some(self.net_scope);
    }

    /// The 1 s beat: start one fetch per missing payload, at most every
    /// [`RETRY_EVERY_SECS`]. Never deletes a held payload.
    pub(crate) fn vault_payload_tick(&mut self) {
        self.vault_sync_held();
        for (named, bytes) in self.vault_seams.take() {
            if let Some(bytes) = bytes {
                if self.vault_store_payload(&named, &bytes) {
                    self.enqueue_vault_publish(&named.secret_id, &named.hash, named.size);
                }
            }
            self.files.vault.seam_named.push(named);
        }
        let missing: Vec<NamedPayload> = self
            .vault_named_payloads()
            .into_iter()
            .filter(|n| !self.vault_payload_held(&n.secret_id))
            .collect();
        // a fetch nothing names any more is abandoned
        self.files.vault.fetches.retain(|hash, f| {
            let keep = missing.iter().any(|n| &n.hash == hash);
            if !keep {
                if let Some(h) = f.handle.take() {
                    h.abort();
                }
            }
            keep
        });
        if self.vault_seams.hold_fetch.load(Ordering::SeqCst) {
            return;
        }
        let now = crate::now_secs();
        for named in missing {
            let due = self.files.vault.fetches.get(&named.hash).map_or(true, |f| {
                !f.handle.as_ref().is_some_and(|h| !h.is_finished()) && now >= f.next_try
            });
            if !due {
                continue;
            }
            let handle = self.spawn_vault_payload_fetch(&named);
            self.files.vault.fetches.insert(
                named.hash.clone(),
                VaultFetch { handle, next_try: now.saturating_add(RETRY_EVERY_SECS) },
            );
        }
    }

    fn spawn_vault_payload_fetch(&self, named: &NamedPayload) -> Option<tokio::task::AbortHandle> {
        let nostr = self.nostr.as_ref()?;
        let key = molt_net::file_plane::vault_payload_key(&nostr.rotation_seed, &named.hash);
        let (Some(cmd_tx), Some(channel)) = (self.cmd_tx.upgrade(), self.nostr_file_channel()) else {
            return None;
        };
        tracing::info!(secret_id = %named.secret_id, size = named.size, "vault payload: fetching");
        let from = crate::now_secs().saturating_sub(24 * 60 * 60);
        Some(crate::transfer::spawn_vault_payload_fetch(
            channel,
            named.series(),
            key,
            from,
            molt_net::file_plane::SeriesExpect { count: named.count(), size: named.size, root: None },
            named.hash.clone(),
            self.net_scope,
            cmd_tx,
        ))
    }

    /// Check `bytes` against what `named` commits to and persist them.
    /// Returns whether the payload is now held.
    fn vault_store_payload(&mut self, named: &NamedPayload, bytes: &[u8]) -> bool {
        use sha2::Digest as _;
        if u64::try_from(bytes.len()).ok() != Some(named.size)
            || hex::encode(sha2::Sha256::digest(bytes)) != named.hash
        {
            tracing::warn!(secret_id = %named.secret_id, "vault payload: hash mismatch");
            return false;
        }
        let durable = match &self.active {
            Some(a) => a.handle.persist_vault_payload_blocking(&named.secret_id, Some(bytes.to_vec())),
            None => true,
        };
        if !durable {
            tracing::warn!(secret_id = %named.secret_id, "vault payload: not persisted");
            return false;
        }
        self.files.vault.held.insert(named.secret_id.clone());
        true
    }

    pub(crate) fn cmd_net_vault_payload_fetched(
        &mut self,
        hash: String,
        bytes: Vec<u8>,
    ) -> Result<Reply, MoltError> {
        self.vault_sync_held();
        let wanted: Vec<NamedPayload> = self
            .vault_named_payloads()
            .into_iter()
            .filter(|n| n.hash == hash && !self.vault_payload_held(&n.secret_id))
            .collect();
        let mut stored = false;
        for named in &wanted {
            stored |= self.vault_store_payload(named, &bytes);
        }
        if stored {
            self.files.vault.fetches.remove(&hash);
            tracing::info!(hash = %hash, "vault payload: held");
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_vault_payload_failed(&mut self, hash: String) -> Result<Reply, MoltError> {
        tracing::debug!(hash = %hash, "vault payload: fetch did not complete");
        Ok(Reply::Ack)
    }

    /// A peer wants pieces of a payload series: every holder answers, past
    /// the share, mirror and cap gates (holding is mandatory). Returns
    /// whether `id` is a vault payload series at all.
    pub(crate) fn serve_vault_pieces(&mut self, id: MessageId, ranges: &[(u32, u32)]) -> bool {
        let Some(named) = self.vault_named_payloads().into_iter().find(|n| n.series() == id) else {
            return false;
        };
        self.vault_sync_held();
        if !self.vault_payload_held(&named.secret_id) {
            return true;
        }
        let Some(layout) = molt_net::file_plane::Manifest::layout_for(named.count()) else {
            return true;
        };
        let ranges: Vec<(u32, u32)> = ranges
            .iter()
            .copied()
            .filter(|(lo, hi)| lo <= hi && *hi <= layout.top)
            .take(molt_net::piece_want::PIECE_WANT_MAX_RANGES)
            .collect();
        if !ranges.is_empty() {
            self.enqueue_vault_pieces(&named, ranges);
        }
        true
    }

    /// Publish a held payload whole - the depositor right after sealing.
    pub(crate) fn enqueue_vault_publish(&mut self, secret_id: &str, hash: &str, size: u64) {
        let named = NamedPayload { secret_id: secret_id.to_string(), hash: hash.to_string(), size };
        let Some(layout) = molt_net::file_plane::Manifest::layout_for(named.count()) else {
            return;
        };
        self.enqueue_vault_pieces(&named, molt_net::trickle::whole_series_ranges(layout));
    }

    fn enqueue_vault_pieces(&mut self, named: &NamedPayload, ranges: Vec<(u32, u32)>) {
        let (Some(store), Some(nostr)) = (self.file_store(), self.nostr.as_ref()) else {
            return;
        };
        let waker = self.group_net.as_ref().map(|g| g.trickle.waker());
        let job = molt_core::PublishJob {
            series: named.series().to_string(),
            key: molt_net::file_plane::vault_payload_key(&nostr.rotation_seed, &named.hash).to_vec(),
            // the sink's file name; the trickle reads the sealed copy
            path: named.secret_id.clone(),
            count: named.count(),
            size: named.size,
            root: String::new(),
            ranges,
            next: 0,
            started_at: crate::now_secs(),
            stored: false,
            wiki_base: false,
            vault: Some(named.hash.clone()),
        };
        tokio::spawn(async move {
            store.update(|s| molt_net::trickle::enqueue_publish(s, job)).await;
            if let Some(w) = waker {
                w.notify_one();
            }
        });
    }
}

#[cfg(test)]
#[path = "vault_payload_tests.rs"]
mod tests;
