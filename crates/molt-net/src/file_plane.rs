// SPDX-License-Identifier: GPL-3.0-or-later

//! The file data plane over relays (F2 of
//! `docs_archive/transport/file_transfer_nostr.md`): a shared file travels as a
//! series of kind-447 chunk events, each sealed exactly like a 445 group
//! frame (exporter-secret AEAD, h-tag of the series' one publish stamp).
//! The share's chat message stays metadata-only; the bytes never touch the
//! workspace log. Publishing is LAZY (the first download request triggers
//! it — engine wiring, F3); while the relays hold the events every further
//! download needs no live sharer.
//!
//! The series id is derived from the share's stable chat message id, so a
//! re-publish after relay retention pruning dedups against a partial fetch
//! and two files never collide. Every chunk of a series is one uniform
//! size ([`FILE_CHUNK_PLAIN_LEN`], zero-padded) — the relay sees blocks,
//! not file-size fingerprints (beyond the count).

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::chunk::{chunk_message_sized, MsgId, PushOutcome, Reassembler, MSG_ID_LEN};
use crate::envelope::open_outer;
use crate::ritual_net::{GroupChannel, GroupRecv};
use crate::NetError;

/// Default per-file byte cap (user decision 2026-08-09: 4 MiB; the engine
/// exposes it as a config key so operators can raise it deliberately).
pub const FILE_CAP_DEFAULT_BYTES: u64 = 4 * 1024 * 1024;

/// One chunk's plaintext size: sealed (+16 AEAD tag +12 nonce) and
/// base64'd it stays under the ~64 KiB event budget the strictest common
/// relays enforce (44_000 → ≈58.7 KiB event content).
pub const FILE_CHUNK_PLAIN_LEN: usize = 44_000;

/// How long a fetch waits for the next chunk before giving up — relays
/// replay history fast; a gap this long means the series is not there.
const FETCH_QUIET: Duration = Duration::from_secs(10);

/// The fetch's OVERALL deadline: a hostile relay trickling matching
/// events must not reset the quiet window forever (review 2026-08-10) —
/// past this budget the fetch ends honestly, whatever arrived.
const FETCH_TOTAL: Duration = Duration::from_secs(300);

/// The chunk-series message id for a share: derived from the share's
/// STABLE chat message id (32-char hex), domain-tagged — deterministic, so
/// a re-publish dedups and two shares never collide.
pub fn series_id(share_id: &str) -> MsgId {
    let mut h = Sha256::new();
    h.update(b"molt-file-v1");
    h.update(share_id.as_bytes());
    let digest = h.finalize();
    let mut id = [0u8; MSG_ID_LEN];
    id.copy_from_slice(&digest[..MSG_ID_LEN]);
    MsgId(id)
}

/// [`publish_series`], gated on the hour's SHARED publish budget (§5.4):
/// one series consumes one round of the same persisted allowance the
/// resend rounds draw from — a spent budget HOLDS the upload with a named
/// refusal instead of loading the pool. The consumption is written back
/// through the same [`StateStore`] the group runtime persists through, so
/// a crash loop cannot buy itself fresh rounds.
pub async fn publish_series_metered<S: crate::supervisor::StateStore>(
    chan: &GroupChannel,
    exporter: &[u8; 32],
    share_id: &str,
    bytes: &[u8],
    cap: u64,
    store: &S,
    now: u64,
) -> Result<(u64, u16), NetError> {
    let mut state = store.load().await;
    let mut cur = state.group.unwrap_or_default();
    if !crate::group_runtime::consume_resend_round(&mut cur, now) {
        return Err(NetError::Framing(
            "publish budget spent — upload held until the hour rolls".to_string(),
        ));
    }
    state.group = Some(cur);
    store.save(state).await;
    publish_series(chan, exporter, share_id, bytes, cap).await
}

/// Publish `bytes` as `share_id`'s chunk series: every chunk under ONE
/// stamp (`created_at` = now), so the whole series sits under one window's
/// h tag and a fetcher only needs that stamp. Returns `(stamp, chunks)`.
///
/// The cap is enforced HERE as well as engine-side (defense in depth): a
/// series that would exceed it never reaches the relays.
pub async fn publish_series(
    chan: &GroupChannel,
    exporter: &[u8; 32],
    share_id: &str,
    bytes: &[u8],
    cap: u64,
) -> Result<(u64, u16), NetError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(NetError::Framing(format!(
            "file of {} bytes exceeds the {cap}-byte cap",
            bytes.len()
        )));
    }
    let chunks = chunk_message_sized(series_id(share_id), bytes, FILE_CHUNK_PLAIN_LEN)?;
    let count = u16::try_from(chunks.len())
        .map_err(|_| NetError::Framing("series chunk count overflow".to_string()))?;
    let stamp = crate::ritual_net::now_secs();
    for chunk in &chunks {
        chan.publish_file_chunk_at(exporter, chunk, stamp).await?;
    }
    Ok((stamp, count))
}

/// Fetch `share_id`'s series published at `published_at`: subscribe the
/// stamp's window tags (kind 447), open every frame against `exporters`
/// (the ring — a re-key between publish and fetch must not orphan the
/// series), reassemble, and verify the bytes against the LOG-ANCHORED
/// share checksum — a series of different bytes is refused, never
/// returned. `Idle` past the quiet window means the relays no longer hold
/// the series (retention) or it was never published: the honest miss, the
/// caller falls back to a `FileRequested` round (F3).
pub async fn fetch_series(
    chan: &GroupChannel,
    exporters: &[[u8; 32]],
    share_id: &str,
    sha256_hex: &str,
    published_at: u64,
    cap: u64,
    quiet: Option<Duration>,
) -> Result<Vec<u8>, NetError> {
    let quiet = quiet.unwrap_or(FETCH_QUIET);
    let want = series_id(share_id);
    let mut sub = chan.subscribe_files_at(published_at).await?;
    let mut reassembler = Reassembler::new_sized(FILE_CHUNK_PLAIN_LEN);
    let cap_usize = usize::try_from(cap).unwrap_or(usize::MAX);
    // the per-chunk payload budget bounds what a claimed `count` may total —
    // rejected on the FIRST chunk, so a forged count=65535 header cannot
    // make this fetcher buffer gigabytes before the cap fires
    let chunk_budget = FILE_CHUNK_PLAIN_LEN - crate::chunk::CHUNK_HEADER_LEN;
    let deadline = tokio::time::Instant::now() + FETCH_TOTAL;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(NetError::Framing(
                "the fetch budget is spent — the series did not complete".to_string(),
            ));
        }
        match sub.recv(quiet).await {
            GroupRecv::Frame { content, .. } => {
                let Ok(plain) = open_outer(exporters, &content) else {
                    // a foreign group's chunk under a colliding tag, or an
                    // epoch outside the ring — not ours to read
                    continue;
                };
                // the header names its series — foreign series just pass by
                if plain.len() < MSG_ID_LEN || plain[..MSG_ID_LEN] != want.0 {
                    continue;
                }
                // header peek: a series whose claimed chunk count could not
                // fit the cap is refused before anything buffers
                if plain.len() >= MSG_ID_LEN + 4 {
                    let count =
                        u16::from_le_bytes([plain[MSG_ID_LEN + 2], plain[MSG_ID_LEN + 3]]);
                    if usize::from(count).saturating_mul(chunk_budget)
                        > cap_usize.saturating_add(chunk_budget)
                    {
                        return Err(NetError::Framing(format!(
                            "series claims {count} chunks — beyond the {cap}-byte cap"
                        )));
                    }
                }
                match reassembler.push(&plain) {
                    Ok(PushOutcome::Complete(_, bytes)) => {
                        if bytes.len() > cap_usize {
                            return Err(NetError::Framing(format!(
                                "series of {} bytes exceeds the {cap}-byte cap",
                                bytes.len()
                            )));
                        }
                        let got = hex::encode(Sha256::digest(&bytes));
                        if got != sha256_hex.to_lowercase() {
                            return Err(NetError::Crypto(
                                "series bytes do not match the share checksum".to_string(),
                            ));
                        }
                        return Ok(bytes);
                    }
                    Ok(PushOutcome::Buffered(_) | PushOutcome::Duplicate(_)) => {}
                    Err(e) => {
                        // a malformed chunk claiming our series id — skip
                        // it; honest chunks can still complete the series
                        tracing::debug!(error = %e, "file plane: dropping a malformed chunk");
                    }
                }
            }
            GroupRecv::Idle => {
                // Idle now guarantees a live relay connection answered the
                // whole quiet window (a dead pool is Deaf, below) — this is
                // the honest MISS: retention pruned it, or never published
                return Err(NetError::Framing(
                    "no chunk of this series arrived — the relays do not hold it".to_string(),
                ));
            }
            GroupRecv::Deaf(why) => {
                return Err(NetError::Framing(format!("file subscription is deaf: {why}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id is deterministic (a re-publish dedups) and share-distinct.
    #[test]
    fn series_ids_are_deterministic_and_distinct() {
        let a = series_id("aa11");
        assert_eq!(a, series_id("aa11"));
        assert_ne!(a, series_id("aa12"));
    }

    /// One sealed+base64'd chunk stays under the strict relay event budget.
    #[test]
    fn a_sealed_chunk_fits_the_relay_budget() {
        // seal_outer: 12-byte nonce + AEAD tag 16, then base64 (4/3)
        let sealed = (FILE_CHUNK_PLAIN_LEN + 12 + 16).div_ceil(3) * 4;
        assert!(sealed < 64 * 1024, "sealed chunk is {sealed} bytes");
    }

    /// The 4-MiB default cap needs far fewer chunks than the u16 ceiling.
    #[test]
    fn the_default_cap_fits_the_chunk_count() {
        let budget = FILE_CHUNK_PLAIN_LEN - crate::chunk::CHUNK_HEADER_LEN;
        let chunks = usize::try_from(FILE_CAP_DEFAULT_BYTES)
            .expect("cap fits usize")
            .div_ceil(budget);
        assert!(chunks < 128, "default cap is {chunks} chunks");
    }
}
