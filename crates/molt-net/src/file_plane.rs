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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;

use crate::chunk::{chunk_message_sized, MsgId, PushOutcome, Reassembler, MSG_ID_LEN};
use crate::envelope::{open_outer, seal_outer};
use crate::ritual_net::{GroupChannel, GroupRecv};
use crate::NetError;

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
    cap: Option<u64>,
    store: &S,
    now: u64,
) -> Result<(u64, u16), NetError> {
    take_publish_round(store, now).await?;
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
    cap: Option<u64>,
) -> Result<(u64, u16), NetError> {
    if let Some(cap) = cap.filter(|cap| u64::try_from(bytes.len()).unwrap_or(u64::MAX) > *cap) {
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
                "the fetch budget is spent - the series did not complete".to_string(),
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
                            "series claims {count} chunks - beyond the {cap}-byte cap"
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
                    "no chunk of this series arrived - the relays do not hold it".to_string(),
                ));
            }
            GroupRecv::Deaf(why) => {
                return Err(NetError::Framing(format!("file subscription is deaf: {why}")));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Series v2 (`docs/files/mirroring.md` §3.1): sealed under the FILE's key
// ---------------------------------------------------------------------------
//
// A v2 piece is a 447 event exactly like a v1 chunk, but its outer layer is
// keyed by `outer_key(K)` (K = the share's content key, members-only via the
// chat) instead of the epoch's exporter secret — so it stays readable across
// every re-key and by every later joiner, for as long as the share is
// pinned. The plaintext is `index u32le | count u32le | len u32le | payload`,
// the payload the index-th 44 000-byte slice of the file, zero-padded (the
// relay sees uniform blocks). Above the data: the slice-hash list, chunked
// at indices `count..count+k`, and ONE top-level record at `count+k` that
// names the chunks by hash; the share's `root` = sha256(top record) is what
// every holder verifies against - each level by the one above it, so a
// forged chunk of either level is dropped like a forged slice.

/// The piece header: `index u32le | count u32le | len u32le`.
pub const PIECE_HEADER_LEN: usize = 12;

/// One piece's payload slice (the same block size as a v1 chunk).
pub const PIECE_PAYLOAD_LEN: usize = FILE_CHUNK_PLAIN_LEN;

/// The top-level record's fixed head: `count u32le | size u64le`.
pub const MANIFEST_HEADER_LEN: usize = 12;

/// Slice hashes one manifest chunk carries.
pub const HASHES_PER_CHUNK: usize = PIECE_PAYLOAD_LEN / 32;

/// The largest series: its top record (one chunk hash per manifest chunk)
/// must fit one piece - `1_374 chunks × 1_375 slices` ≈ 83 GB (the unit
/// test pins the product to the block geometry).
pub const MAX_SERIES_PIECES: u32 = 1_374 * 1_375;

/// The catch-up horizon one fetch subscribes at once (§3.1): 60 day windows.
pub const MAX_CATCHUP_WINDOWS: usize = 60;

/// Manifest chunks kept per slot while the top record is still missing
/// (they cannot be verified before it): an insider's forgery costs one
/// event per slot, the truth arrives and wins by hash.
const MANIFEST_CANDIDATES_PER_SLOT: usize = 8;

/// The per-file OUTER key: `HKDF-SHA256(ikm = K, info = "molt-piece-outer-v2")`.
/// One derivation step away from K so the chat's key never seals bytes
/// directly.
pub fn outer_key(key: &[u8; 32]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<Sha256>::new(None, key);
    let mut out = [0u8; 32];
    hk.expand(b"molt-piece-outer-v2", &mut out)
        .expect("32 bytes is within the HKDF-SHA256 expand limit");
    out
}

/// Frame one piece: header + payload, zero-padded to the uniform block.
pub fn frame_piece(index: u32, count: u32, payload: &[u8]) -> Result<Vec<u8>, NetError> {
    if payload.len() > PIECE_PAYLOAD_LEN {
        return Err(NetError::Framing(format!(
            "piece payload of {} bytes exceeds {PIECE_PAYLOAD_LEN}",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len()).map_err(|_| NetError::Framing("piece len".into()))?;
    let mut out = Vec::with_capacity(PIECE_HEADER_LEN + PIECE_PAYLOAD_LEN);
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(PIECE_HEADER_LEN + PIECE_PAYLOAD_LEN, 0);
    Ok(out)
}

/// Parse a framed piece: `(index, count, payload)`. Untrusted input —
/// every field is bounds-checked.
pub fn parse_piece(plain: &[u8]) -> Result<(u32, u32, &[u8]), NetError> {
    if plain.len() != PIECE_HEADER_LEN + PIECE_PAYLOAD_LEN {
        return Err(NetError::Framing(format!(
            "piece of {} bytes is not one uniform block",
            plain.len()
        )));
    }
    let word = |at: usize| {
        let mut b = [0u8; 4];
        b.copy_from_slice(&plain[at..at + 4]);
        u32::from_le_bytes(b)
    };
    let (index, count, len) = (word(0), word(4), word(8));
    let len = usize::try_from(len).map_err(|_| NetError::Framing("piece len".into()))?;
    if len > PIECE_PAYLOAD_LEN {
        return Err(NetError::Framing(format!("piece claims {len} payload bytes")));
    }
    Ok((index, count, &plain[PIECE_HEADER_LEN..PIECE_HEADER_LEN + len]))
}

/// Seal one piece for a holder's store or a publish: base64 of
/// `nonce ‖ AEAD(outer_key(K), frame)` — the 447 content.
pub fn seal_piece(key: &[u8; 32], index: u32, count: u32, payload: &[u8]) -> Result<String, NetError> {
    let framed = frame_piece(index, count, payload)?;
    seal_outer(&outer_key(key), &framed).map_err(|e| NetError::Crypto(e.to_string()))
}

/// Open one 447 content under the file's key: `(index, count, payload)`.
/// A foreign key (another file, another group) fails the AEAD - the key IS
/// the series filter.
pub fn open_piece(key: &[u8; 32], content: &str) -> Result<(u32, u32, Vec<u8>), NetError> {
    let plain = open_outer(&[outer_key(key)], content).map_err(|e| NetError::Crypto(e.to_string()))?;
    let (index, count, payload) = parse_piece(&plain)?;
    Ok((index, count, payload.to_vec()))
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The manifest: the sha256 of every UNPADDED plaintext slice, in order,
/// plus the geometry. On the wire it is two levels: the hash list chunked
/// into pieces of [`HASHES_PER_CHUNK`], and the top record naming those
/// chunks by hash; `root` = sha256(top record) is what the share (and the
/// persist block) carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Data pieces (`ceil(size / PIECE_PAYLOAD_LEN)`).
    pub count: u32,
    /// The file's byte length.
    pub size: u64,
    /// sha256 of every unpadded slice, in order.
    pub hashes: Vec<[u8; 32]>,
}

/// The top-level record as parsed off the wire: `count u32le | size u64le |
/// sha256(chunk_0) ‖ …`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopRecord {
    /// Data pieces.
    pub count: u32,
    /// The file's byte length.
    pub size: u64,
    /// sha256 of every manifest chunk, in slot order.
    pub chunk_hashes: Vec<[u8; 32]>,
}

impl TopRecord {
    /// The inverse of [`Manifest::top_bytes`]; the geometry must add up
    /// and fit one piece.
    pub fn parse(bytes: &[u8]) -> Result<TopRecord, NetError> {
        if bytes.len() < MANIFEST_HEADER_LEN || bytes.len() > PIECE_PAYLOAD_LEN {
            return Err(NetError::Framing("top record size".into()));
        }
        let mut c = [0u8; 4];
        c.copy_from_slice(&bytes[..4]);
        let count = u32::from_le_bytes(c);
        let mut z = [0u8; 8];
        z.copy_from_slice(&bytes[4..12]);
        let size = u64::from_le_bytes(z);
        let chunks = usize::try_from(Manifest::chunk_count_for(count)).unwrap_or(usize::MAX);
        let want = MANIFEST_HEADER_LEN.saturating_add(chunks.saturating_mul(32));
        if bytes.len() != want || count != Manifest::piece_count_for(size) || count > MAX_SERIES_PIECES {
            return Err(NetError::Framing("top record geometry does not add up".into()));
        }
        let chunk_hashes = bytes[MANIFEST_HEADER_LEN..]
            .chunks_exact(32)
            .map(|h| {
                let mut a = [0u8; 32];
                a.copy_from_slice(h);
                a
            })
            .collect();
        Ok(TopRecord { count, size, chunk_hashes })
    }
}

impl Manifest {
    /// How many data pieces `size` bytes take.
    pub fn piece_count_for(size: u64) -> u32 {
        let block = u64::try_from(PIECE_PAYLOAD_LEN).unwrap_or(u64::MAX);
        u32::try_from(size.div_ceil(block)).unwrap_or(u32::MAX)
    }

    /// How many manifest chunks `count` slices take (0 for an empty file).
    pub fn chunk_count_for(count: u32) -> u32 {
        let per = u32::try_from(HASHES_PER_CHUNK).unwrap_or(u32::MAX);
        count.div_ceil(per)
    }

    /// The series' piece indices: data `0..count`, chunks `count..count+k`,
    /// the top record at `count+k`. `None` beyond [`MAX_SERIES_PIECES`].
    pub fn layout_for(count: u32) -> Option<SeriesLayout> {
        if count > MAX_SERIES_PIECES {
            return None;
        }
        let chunks = Manifest::chunk_count_for(count);
        let top = count.checked_add(chunks)?;
        Some(SeriesLayout { count, chunks, top })
    }

    /// The slot-th chunk of the hash list.
    pub fn chunk(&self, slot: u32) -> Vec<u8> {
        let from = usize::try_from(slot).unwrap_or(usize::MAX).saturating_mul(HASHES_PER_CHUNK);
        let to = from.saturating_add(HASHES_PER_CHUNK).min(self.hashes.len());
        self.hashes
            .get(from.min(self.hashes.len())..to)
            .unwrap_or(&[])
            .iter()
            .flat_map(|h| h.iter().copied())
            .collect()
    }

    /// `count u32le | size u64le | sha256(chunk_0) ‖ …` - the top record.
    pub fn top_bytes(&self) -> Vec<u8> {
        let chunks = Manifest::chunk_count_for(self.count);
        let mut out = Vec::with_capacity(MANIFEST_HEADER_LEN + 32 * usize::try_from(chunks).unwrap_or(0));
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        for slot in 0..chunks {
            out.extend_from_slice(&sha(&self.chunk(slot)));
        }
        out
    }

    /// Lowercase hex sha256 of [`Manifest::top_bytes`].
    pub fn root(&self) -> String {
        hex::encode(sha(&self.top_bytes()))
    }

    /// Rebuild the manifest from a verified top record and its chunks (the
    /// caller verified each chunk against the record).
    pub fn from_parts(top: &TopRecord, chunks: &[Vec<u8>]) -> Result<Manifest, NetError> {
        let bytes: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        let want = usize::try_from(top.count).unwrap_or(usize::MAX).saturating_mul(32);
        if bytes.len() != want {
            return Err(NetError::Framing("manifest chunks do not add up to the count".into()));
        }
        let hashes = bytes
            .chunks_exact(32)
            .map(|h| {
                let mut a = [0u8; 32];
                a.copy_from_slice(h);
                a
            })
            .collect();
        Ok(Manifest { count: top.count, size: top.size, hashes })
    }
}

/// Where a series' pieces sit: `0..count` data, `count..top` manifest
/// chunks, `top` the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeriesLayout {
    /// Data pieces.
    pub count: u32,
    /// Manifest chunks (indices `count..top`).
    pub chunks: u32,
    /// The top record's index.
    pub top: u32,
}

impl SeriesLayout {
    /// Every piece of the series, the top record included.
    pub fn pieces(self) -> u64 {
        u64::from(self.top) + 1
    }

    /// The stored-event bound one fetch hands its subscription: twice the
    /// series (a re-publish per piece) plus headroom for other files'
    /// pieces under the same windows - the relay's default bound would
    /// cut any series past ~5 000 pieces.
    pub fn history_bound(self) -> usize {
        usize::try_from(self.pieces().saturating_mul(2).saturating_add(100)).unwrap_or(usize::MAX)
    }
}

/// Build the manifest by reading `reader` slice by slice (a 1 GB file
/// costs one block of memory); also the whole file's sha256 hex, the
/// share checksum, in the same pass.
pub fn manifest_of_reader<R: Read>(mut reader: R) -> std::io::Result<(Manifest, String)> {
    let mut whole = Sha256::new();
    let mut hashes = Vec::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; PIECE_PAYLOAD_LEN];
    loop {
        let n = read_full(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        hashes.push(sha(&buf[..n]));
        size = size.saturating_add(u64::try_from(n).unwrap_or(0));
        if hashes.len() > usize::try_from(MAX_SERIES_PIECES).unwrap_or(usize::MAX) {
            return Err(std::io::Error::other("the file exceeds the largest series (~83 GB)"));
        }
        if n < PIECE_PAYLOAD_LEN {
            break;
        }
    }
    let count = u32::try_from(hashes.len())
        .map_err(|_| std::io::Error::other("more pieces than a u32 counts"))?;
    Ok((Manifest { count, size, hashes }, hex::encode(whole.finalize())))
}

/// Read until `buf` is full or the reader ends; the number of bytes read.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// What a fetch expects of a series, straight from the share message the
/// members hold (`FileMeta`): the ratified geometry and root.
#[derive(Clone, Debug)]
pub struct SeriesExpect {
    /// The share's data piece count.
    pub count: u32,
    /// The share's byte length.
    pub size: u64,
    /// The share's manifest root, hex.
    pub root: String,
}

/// Where fetched slices land: memory for tests and small files, a `.part`
/// file on the engine side (write-at, never pre-sized). A slice lands as
/// it ARRIVES - AEAD-authenticated under the members' key - and is
/// verified against its manifest chunk once that is known; a slice that
/// then fails is re-marked missing and the honest one overwrites it, so
/// the sink must tolerate a second write at the same index.
pub trait PieceSink {
    /// Land one slice at `index * PIECE_PAYLOAD_LEN`.
    fn put(&mut self, index: u32, payload: &[u8]) -> std::io::Result<()>;
}

impl PieceSink for Vec<u8> {
    fn put(&mut self, index: u32, payload: &[u8]) -> std::io::Result<()> {
        let at = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(PIECE_PAYLOAD_LEN))
            .ok_or_else(|| std::io::Error::other("piece offset overflow"))?;
        let end = at.saturating_add(payload.len());
        if self.len() < end {
            self.resize(end, 0);
        }
        self[at..end].copy_from_slice(payload);
        Ok(())
    }
}

/// Take one round of the hour's SHARED publish budget (§5.4 - the same
/// persisted allowance the resend rounds draw from): `Err` = spent, the
/// upload is held until the hour rolls. Taken BEFORE any work the
/// publish would do, so a spent budget costs nothing.
pub async fn take_publish_round<S: crate::supervisor::StateStore>(
    store: &S,
    now: u64,
) -> Result<(), NetError> {
    let mut granted = false;
    store
        .update(|state| {
            let mut cur = state.group.unwrap_or_default();
            granted = crate::group_runtime::consume_resend_round(&mut cur, now);
            if granted {
                state.group = Some(cur);
            }
            granted
        })
        .await;
    if granted {
        Ok(())
    } else {
        Err(NetError::Framing(
            "publish budget spent - upload held until the hour rolls".to_string(),
        ))
    }
}

/// How often a rate-limited publish is retried before the series gives
/// up (M1 publishes a series whole; M2's trickle paces it instead).
const RATE_LIMIT_RETRIES: u32 = 20;

/// Publish one sealed piece, waiting out a relay's rate limit: a "slow
/// down" refusal is the relay asking for pacing, not a dead pool - a
/// burst of pieces earns it from any relay worth having.
async fn publish_piece_paced(
    chan: &GroupChannel,
    outer: &[u8; 32],
    framed: &[u8],
    stamp: u64,
) -> Result<u64, NetError> {
    let mut attempt = 0u32;
    loop {
        match chan.publish_file_chunk_at(outer, framed, stamp).await {
            Ok((at, _)) => return Ok(at),
            Err(e) if attempt < RATE_LIMIT_RETRIES && e.to_string().contains("rate-limited") => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(u64::from(attempt.min(8)) * 250)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Publish a v2 series from `source`: the top record, then the manifest
/// chunks (so a replay serves them early), then every data slice in
/// order, each verified against `manifest` as it is read - a slice that
/// changed since the share aborts the publish (the round is spent; the
/// requester's watchdog fails it honestly). `stamp_for(index)` names each
/// piece's `created_at` (its window tag) - `None` = now. Returns `(first
/// stamp, data piece count)`; the first stamp is the series start.
pub async fn publish_series_v2(
    chan: &GroupChannel,
    key: &[u8; 32],
    source: &std::path::Path,
    manifest: &Manifest,
    stamp_for: Option<&(dyn Fn(u32) -> u64 + Sync)>,
) -> Result<(u64, u32), NetError> {
    let outer = outer_key(key);
    let count = manifest.count;
    let layout = Manifest::layout_for(count)
        .ok_or_else(|| NetError::Framing("the file exceeds the largest series".into()))?;
    let stamp = |index: u32| stamp_for.map_or_else(crate::ritual_net::now_secs, |f| f(index));
    let mut first: Option<u64> = None;
    let mut note = |at: u64| first = Some(first.map_or(at, |f| f.min(at)));
    let framed = frame_piece(layout.top, count, &manifest.top_bytes())?;
    note(publish_piece_paced(chan, &outer, &framed, stamp(layout.top)).await?);
    for slot in 0..layout.chunks {
        let index = count.saturating_add(slot);
        let framed = frame_piece(index, count, &manifest.chunk(slot))?;
        note(publish_piece_paced(chan, &outer, &framed, stamp(index)).await?);
    }
    // slice by slice: the file never sits in memory whole (a 1 GB share
    // is 24 000 pieces, one block at a time)
    let mut file = std::fs::File::open(source)
        .map_err(|e| NetError::Framing(format!("opening the shared file: {e}")))?;
    let mut buf = vec![0u8; PIECE_PAYLOAD_LEN];
    for index in 0..count {
        let n = read_full(&mut file, &mut buf)
            .map_err(|e| NetError::Framing(format!("reading the shared file: {e}")))?;
        let slice = &buf[..n];
        let want = manifest
            .hashes
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .ok_or_else(|| NetError::Framing("manifest shorter than its count".into()))?;
        if sha(slice) != *want {
            return Err(NetError::Framing(format!(
                "slice {index} no longer matches the share - the file changed"
            )));
        }
        let framed = frame_piece(index, count, slice)?;
        note(publish_piece_paced(chan, &outer, &framed, stamp(index)).await?);
    }
    Ok((first.unwrap_or_else(crate::ritual_net::now_secs), count))
}

/// [`publish_series_v2`] on the hour's shared publish budget - one round
/// per series, like v1 (M2 turns the whole publish into a trickle).
pub async fn publish_series_v2_metered<S: crate::supervisor::StateStore>(
    chan: &GroupChannel,
    key: &[u8; 32],
    source: &std::path::Path,
    manifest: &Manifest,
    store: &S,
    now: u64,
) -> Result<(u64, u32), NetError> {
    take_publish_round(store, now).await?;
    publish_series_v2(chan, key, source, manifest, None).await
}

/// Fetch a v2 series that started at `start_stamp`: subscribe the windows
/// from there to now (at most [`MAX_CATCHUP_WINDOWS`]) with a stored-event
/// bound sized to the series, open every piece under the file's key, land
/// data slices as they arrive, verify the top record by root, each chunk
/// by the record, each slice by its chunk (relays replay newest first, so
/// the manifest tends to come LAST - nothing waits for it, nothing is
/// dropped for lack of it). A slice still missing after the relays went
/// quiet is the honest miss - the caller re-requests (M2).
pub async fn fetch_series_v2(
    chan: &GroupChannel,
    key: &[u8; 32],
    start_stamp: u64,
    expect: &SeriesExpect,
    sink: &mut (dyn PieceSink + Send),
    quiet: Option<Duration>,
) -> Result<Manifest, NetError> {
    if expect.count != Manifest::piece_count_for(expect.size) {
        return Err(NetError::Framing(
            "the share's piece count does not match its size".to_string(),
        ));
    }
    let layout = Manifest::layout_for(expect.count)
        .ok_or_else(|| NetError::Framing("the share exceeds the largest series".into()))?;
    let root = expect.root.to_lowercase();
    let quiet = quiet.unwrap_or(FETCH_QUIET);
    let outer = outer_key(key);
    let count = layout.count;
    let chunk_slots = usize::try_from(layout.chunks).unwrap_or(usize::MAX);
    let mut sub = chan
        .subscribe_files_from(start_stamp, MAX_CATCHUP_WINDOWS, Some(layout.history_bound()))
        .await?;
    let mut top: Option<TopRecord> = None;
    let mut chunks: Vec<Option<Vec<u8>>> = vec![None; chunk_slots];
    let mut candidates: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
    // data landed but not yet verifiable (its chunk is missing): the hash
    // of what sits in the sink at that index
    let mut landed: HashMap<u32, [u8; 32]> = HashMap::new();
    let mut verified: HashSet<u32> = HashSet::new();
    // a series of many pieces takes longer to replay than a small one
    let deadline = tokio::time::Instant::now()
        + FETCH_TOTAL
        + Duration::from_secs(u64::from(count) / 10);
    let per = u32::try_from(HASHES_PER_CHUNK).unwrap_or(u32::MAX);
    let hash_in = |chunk: &[u8], index: u32| -> Option<[u8; 32]> {
        let at = usize::try_from(index % per).ok()?.checked_mul(32)?;
        chunk.get(at..at + 32).map(|h| {
            let mut a = [0u8; 32];
            a.copy_from_slice(h);
            a
        })
    };
    // a chunk arrived and verified: settle every landed slice it covers
    let settle = |slot: u32, chunk: &[u8], landed: &mut HashMap<u32, [u8; 32]>, verified: &mut HashSet<u32>| {
        let from = slot.saturating_mul(per);
        let to = from.saturating_add(per).min(count);
        for index in from..to {
            if let Some(have) = landed.remove(&index) {
                if hash_in(chunk, index) == Some(have) {
                    verified.insert(index);
                } else {
                    tracing::debug!(index, "file plane: a landed slice fails its manifest hash - waiting for another");
                }
            }
        }
    };
    loop {
        let done = top.is_some()
            && chunks.iter().all(Option::is_some)
            && verified.len() == usize::try_from(count).unwrap_or(usize::MAX);
        if done {
            let top = top.take().ok_or_else(|| NetError::Framing("no top record".into()))?;
            let parts: Vec<Vec<u8>> = chunks.into_iter().flatten().collect();
            return Manifest::from_parts(&top, &parts);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(NetError::Framing(
                "the fetch budget is spent - the series did not complete".to_string(),
            ));
        }
        match sub.recv(quiet).await {
            GroupRecv::Frame { content, .. } => {
                // another file's piece, or a foreign group's: not ours to read
                let Ok(plain) = open_outer(&[outer], &content) else {
                    continue;
                };
                let Ok((index, claimed, payload)) = parse_piece(&plain) else {
                    continue;
                };
                if claimed != count || index > layout.top {
                    continue; // a re-share under the same key cannot happen; refuse the geometry anyway
                }
                if index == layout.top {
                    if top.is_some() || hex::encode(sha(payload)) != root {
                        continue; // a record the root disowns is a forgery
                    }
                    let Ok(record) = TopRecord::parse(payload) else {
                        continue;
                    };
                    if record.count != count || record.size != expect.size {
                        continue;
                    }
                    // the chunks that waited for the record: the one whose
                    // hash it names wins, the rest were forgeries
                    for (slot, cands) in std::mem::take(&mut candidates) {
                        let want = record.chunk_hashes.get(usize::try_from(slot).unwrap_or(usize::MAX));
                        if let Some(good) = cands.into_iter().find(|c| Some(&sha(c)) == want) {
                            settle(slot, &good, &mut landed, &mut verified);
                            if let Some(entry) = chunks.get_mut(usize::try_from(slot).unwrap_or(usize::MAX)) {
                                *entry = Some(good);
                            }
                        }
                    }
                    top = Some(record);
                    continue;
                }
                if index >= count {
                    let slot = index - count;
                    let at = usize::try_from(slot).unwrap_or(usize::MAX);
                    if chunks.get(at).is_some_and(Option::is_some) {
                        continue;
                    }
                    match &top {
                        Some(record) => {
                            if record.chunk_hashes.get(at) == Some(&sha(payload)) {
                                settle(slot, payload, &mut landed, &mut verified);
                                if let Some(entry) = chunks.get_mut(at) {
                                    *entry = Some(payload.to_vec());
                                }
                            }
                        }
                        None => {
                            let cands = candidates.entry(slot).or_default();
                            if cands.len() < MANIFEST_CANDIDATES_PER_SLOT && !cands.iter().any(|c| c == payload) {
                                cands.push(payload.to_vec());
                            }
                        }
                    }
                    continue;
                }
                if verified.contains(&index) {
                    continue;
                }
                let have = sha(payload);
                match chunks.get(usize::try_from(index / per).unwrap_or(usize::MAX)).and_then(Option::as_ref) {
                    Some(chunk) => {
                        if hash_in(chunk, index) == Some(have) {
                            sink.put(index, payload)
                                .map_err(|e| NetError::Framing(format!("landing piece {index}: {e}")))?;
                            verified.insert(index);
                        } else {
                            tracing::debug!(index, "file plane: dropping a slice that fails its manifest hash");
                        }
                    }
                    None => {
                        if landed.get(&index) == Some(&have) {
                            continue;
                        }
                        sink.put(index, payload)
                            .map_err(|e| NetError::Framing(format!("landing piece {index}: {e}")))?;
                        landed.insert(index, have);
                    }
                }
            }
            GroupRecv::Idle => {
                return Err(match &top {
                    None => NetError::Framing(
                        "no manifest of this series arrived - the relays do not hold it".to_string(),
                    ),
                    Some(_) => NetError::Framing(format!(
                        "{} of {count} pieces missing after the relays went quiet",
                        usize::try_from(count).unwrap_or(usize::MAX).saturating_sub(verified.len())
                    )),
                });
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

    /// A v2 piece (header + block) sealed and base64'd stays under the
    /// relay budget too.
    #[test]
    fn a_sealed_v2_piece_fits_the_relay_budget() {
        let sealed = (PIECE_HEADER_LEN + PIECE_PAYLOAD_LEN + 12 + 16).div_ceil(3) * 4;
        assert!(sealed < 64 * 1024, "sealed piece is {sealed} bytes");
    }

    /// Frame → seal → open → parse round-trips; the payload length is real,
    /// the block uniform; a foreign key does not open it.
    #[test]
    fn a_piece_round_trips_under_its_key_and_not_under_another() {
        let key = [3u8; 32];
        let content = seal_piece(&key, 7, 9, b"short slice").expect("seal");
        let (index, count, payload) = open_piece(&key, &content).expect("open");
        assert_eq!((index, count), (7, 9));
        assert_eq!(payload, b"short slice");
        assert!(open_piece(&[4u8; 32], &content).is_err(), "another key is another series");
        assert!(frame_piece(0, 1, &vec![0u8; PIECE_PAYLOAD_LEN + 1]).is_err());
    }

    /// The manifest of a reader: three slices (two full, one short), the
    /// geometry pinned, the whole-file sha256 alongside, the root stable,
    /// and the two wire levels round-trip through the top record.
    #[test]
    fn the_manifest_reads_slice_by_slice() {
        let bytes: Vec<u8> = (0..(2 * PIECE_PAYLOAD_LEN + 100)).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let (m, sha) = manifest_of_reader(bytes.as_slice()).expect("manifest");
        assert_eq!(m.count, 3);
        assert_eq!(m.size, u64::try_from(bytes.len()).expect("fits"));
        assert_eq!(m.hashes.len(), 3);
        assert_eq!(m.hashes[2], <[u8; 32]>::from(Sha256::digest(&bytes[2 * PIECE_PAYLOAD_LEN..])));
        assert_eq!(sha, hex::encode(Sha256::digest(&bytes)));
        assert_eq!(m.root().len(), 64);
        assert_eq!(Manifest::piece_count_for(0), 0);
        assert_eq!(Manifest::piece_count_for(1), 1);
        let layout = Manifest::layout_for(m.count).expect("layout");
        assert_eq!((layout.chunks, layout.top, layout.pieces()), (1, 4, 5));
        let top = TopRecord::parse(&m.top_bytes()).expect("top parses");
        assert_eq!((top.count, top.size), (m.count, m.size));
        assert_eq!(top.chunk_hashes, vec![sha256_of(&m.chunk(0))]);
        assert_eq!(Manifest::from_parts(&top, &[m.chunk(0)]).expect("rebuild"), m);
        // a record whose count disagrees with its size is refused
        let mut bad = m.top_bytes();
        bad[0] = 9;
        assert!(TopRecord::parse(&bad).is_err());
        assert!(Manifest::from_parts(&top, &[m.chunk(0)[..32].to_vec()]).is_err());
    }

    fn sha256_of(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    /// A manifest past one chunk (1 375 slices) splits into chunks the top
    /// record names by hash; the largest series is the one whose record
    /// still fits a piece; the fetch bound follows the whole series.
    #[test]
    fn a_large_manifest_chunks_and_the_series_is_bounded() {
        let count = 3_000u32;
        let hashes: Vec<[u8; 32]> = (0..count).map(|i| [u8::try_from(i % 251).unwrap_or(0); 32]).collect();
        let m = Manifest {
            count,
            size: u64::from(count) * u64::try_from(PIECE_PAYLOAD_LEN).expect("fits"),
            hashes,
        };
        let layout = Manifest::layout_for(count).expect("layout");
        assert_eq!(layout.chunks, 3);
        assert_eq!(layout.top, 3_003);
        assert_eq!(layout.history_bound(), 2 * 3_004 + 100);
        let top = TopRecord::parse(&m.top_bytes()).expect("top");
        assert_eq!(top.chunk_hashes.len(), 3);
        let chunks: Vec<Vec<u8>> = (0..3).map(|s| m.chunk(s)).collect();
        assert_eq!(chunks[2].len(), (3_000 - 2 * HASHES_PER_CHUNK) * 32);
        assert_eq!(Manifest::from_parts(&top, &chunks).expect("rebuild"), m);
        assert_eq!(
            usize::try_from(MAX_SERIES_PIECES).expect("fits"),
            (PIECE_PAYLOAD_LEN - MANIFEST_HEADER_LEN) / 32 * HASHES_PER_CHUNK,
            "the top record of the largest series fills exactly one piece"
        );
        assert_eq!(Manifest::layout_for(MAX_SERIES_PIECES).map(|l| l.chunks), Some(1_374));
        assert!(Manifest::layout_for(MAX_SERIES_PIECES + 1).is_none());
        assert_eq!(Manifest::layout_for(0).expect("empty file").pieces(), 1);
    }

    /// The outer key is one HKDF step from K - pinned to its value so a
    /// silent change of the derivation cannot orphan every published series.
    #[test]
    fn the_outer_key_derivation_is_pinned() {
        assert_eq!(
            hex::encode(outer_key(&[0u8; 32])),
            "6c911e70bba42849427e78e7a3eca17573df92b2c5f19b378c12cda2fcddf679"
        );
        assert_ne!(outer_key(&[0u8; 32]), outer_key(&[1u8; 32]));
    }
}
