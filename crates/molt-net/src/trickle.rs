// SPDX-License-Identifier: GPL-3.0-or-later

//! The trickle sender (`docs_archive/files/mirroring.md` §3.2): ONE queue of
//! publish jobs per runtime, persisted in `TransportState` so a restart
//! resumes at the cursor, drained one kind-447 event per interval - and
//! only while the group outbox is idle, the hour's resend budget keeps two
//! rounds of headroom (chat and governance first) and the day's byte cap
//! is not spent. A job names a series and the piece ranges to publish:
//! the whole series on the first `FileWanted`, exactly the missed pieces
//! on a `PieceWanted`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use molt_core::{PublishJob, TransportState};
use tokio::sync::{watch, Notify};

use crate::file_plane::{
    expected_slice_len, frame_piece, manifest_of_reader, outer_key, publish_content_paced,
    publish_piece_paced, read_full, transient_publish_error, Manifest, SeriesLayout,
    PIECE_PAYLOAD_LEN,
};
use crate::ritual_net::GroupChannel;
use crate::supervisor::StateStore;
use crate::NetError;

/// What one published piece costs on the wire: the sealed block (nonce +
/// AEAD tag) base64'd - what the daily cap counts. Pinned; the unit test
/// derives it from the block geometry.
pub const PIECE_WIRE_BYTES: u64 = 58_720;

/// Resend rounds the hour must still hold before a piece may go out.
const BUDGET_HEADROOM: u32 = 2;

/// Queued range entries per series before a further want is refused (it
/// repeats later); a whole-series job needs three.
const MAX_JOB_RANGES: usize = 256;

/// The sender's pace and budget. `clock` is the BUDGET clock (the day the
/// byte counter belongs to) - a test seam; the wire stamp is always now.
pub struct TrickleConfig {
    /// Time between two pieces.
    pub interval: Duration,
    /// Piece bytes per UTC day.
    pub daily_bytes: u64,
    /// Unix seconds for the daily counter.
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl TrickleConfig {
    /// Production: `interval_secs` (at least 1) and `daily_bytes`.
    #[must_use]
    pub fn new(interval_secs: u64, daily_bytes: u64) -> TrickleConfig {
        TrickleConfig {
            interval: Duration::from_secs(interval_secs.max(1)),
            daily_bytes,
            clock: Arc::new(crate::ritual_net::now_secs),
        }
    }
}

/// A running trickle sender.
pub struct TrickleHandle {
    wake: Arc<Notify>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TrickleHandle {
    /// Something was enqueued (or a gate lifted): look now, not at the
    /// next interval.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// The wake signal, for a task that enqueues off the actor.
    #[must_use]
    pub fn waker(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Stop after the piece in flight - drained, never aborted.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TrickleHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// The publish order of a whole series: the top record, the manifest
/// chunks, the data - as inclusive ranges.
#[must_use]
pub fn whole_series_ranges(layout: SeriesLayout) -> Vec<(u32, u32)> {
    let mut out = vec![(layout.top, layout.top)];
    if layout.chunks > 0 {
        out.push((layout.count, layout.top - 1));
    }
    if layout.count > 0 {
        out.push((0, layout.count - 1));
    }
    out
}

/// Pieces a job's ranges cover.
#[must_use]
pub fn job_total(ranges: &[(u32, u32)]) -> u64 {
    ranges
        .iter()
        .map(|(lo, hi)| u64::from(hi.saturating_sub(*lo)) + 1)
        .sum()
}

/// The piece index at `position` of the concatenated ranges.
#[must_use]
pub fn index_at(ranges: &[(u32, u32)], position: u32) -> Option<u32> {
    let mut left = u64::from(position);
    for (lo, hi) in ranges {
        let len = u64::from(hi.saturating_sub(*lo)) + 1;
        if left < len {
            return u32::try_from(u64::from(*lo) + left).ok();
        }
        left -= len;
    }
    None
}

fn is_whole(job: &PublishJob) -> bool {
    Manifest::layout_for(job.count).is_some_and(|l| whole_series_ranges(l) == job.ranges)
}

/// Queue `job`: one entry per series - a whole-series job replaces or
/// absorbs a range job, range jobs merge (new ranges appended). `false` =
/// nothing new was queued.
pub fn enqueue_publish(state: &mut TransportState, job: PublishJob) -> bool {
    let Some(existing) = state
        .file_jobs
        .publish
        .iter_mut()
        .find(|j| j.series == job.series)
    else {
        state.file_jobs.publish.push(job);
        return true;
    };
    if is_whole(existing) {
        return false;
    }
    if is_whole(&job) {
        *existing = job;
        return true;
    }
    let fresh: Vec<(u32, u32)> = job
        .ranges
        .into_iter()
        .filter(|r| !existing.ranges.contains(r))
        .collect();
    if fresh.is_empty() || existing.ranges.len() + fresh.len() > MAX_JOB_RANGES {
        return false;
    }
    existing.ranges.extend(fresh);
    true
}

/// Start the sender. `busy` is the group outbox's pending flag.
pub fn spawn_trickle<S: StateStore>(
    chan: GroupChannel,
    store: S,
    busy: watch::Receiver<bool>,
    cfg: TrickleConfig,
) -> TrickleHandle {
    let wake = Arc::new(Notify::new());
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(trickle_loop(chan, store, busy, cfg, wake.clone(), stop_rx));
    TrickleHandle { wake, stop, task: Some(task) }
}

/// Why the loop did not publish this tick.
enum Hold {
    /// Nothing queued: wait for a wake only.
    Idle,
    /// A gate is closed: try again next interval.
    Gated,
    /// A refusal: back off.
    Failed,
}

async fn trickle_loop<S: StateStore>(
    chan: GroupChannel,
    store: S,
    busy: watch::Receiver<bool>,
    cfg: TrickleConfig,
    wake: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut manifests: HashMap<String, Manifest> = HashMap::new();
    let mut idle = false;
    let mut backoff = 0u32;
    let mut last: Option<String> = None;
    loop {
        let delay = cfg.interval.saturating_mul(1u32 << backoff.min(6));
        tokio::select! {
            _ = stop.changed() => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(delay), if !idle => {}
        }
        if *stop.borrow() {
            return;
        }
        match tick(&chan, &store, &busy, &cfg, &mut manifests, &mut last).await {
            Ok(()) => {
                idle = false;
                backoff = 0;
            }
            Err(Hold::Idle) => idle = true,
            Err(Hold::Gated) => idle = false,
            Err(Hold::Failed) => {
                idle = false;
                backoff = backoff.saturating_add(1);
            }
        }
    }
}

/// Which unfinished job the tick serves: a range job (someone waits for
/// exactly those pieces) before any whole-series job, and within a group
/// the one after `last` - round-robin, so one large series never starves
/// the rest.
fn pick_job(jobs: &[PublishJob], last: Option<&str>) -> Option<PublishJob> {
    let open: Vec<&PublishJob> = jobs
        .iter()
        .filter(|j| u64::from(j.next) < job_total(&j.ranges))
        .collect();
    let group: Vec<&PublishJob> = if open.iter().any(|j| !is_whole(j)) {
        open.iter().copied().filter(|j| !is_whole(j)).collect()
    } else {
        open
    };
    if group.is_empty() {
        return None;
    }
    let after = last
        .and_then(|l| group.iter().position(|j| j.series == l))
        .map_or(0, |p| (p + 1) % group.len());
    group.get(after).map(|j| (*j).clone())
}

/// One tick: publish the next piece of the job the round-robin names, or
/// say why not.
async fn tick<S: StateStore>(
    chan: &GroupChannel,
    store: &S,
    busy: &watch::Receiver<bool>,
    cfg: &TrickleConfig,
    manifests: &mut HashMap<String, Manifest>,
    last: &mut Option<String>,
) -> Result<(), Hold> {
    let state = store.load().await;
    let Some(job) = pick_job(&state.file_jobs.publish, last.as_deref()) else {
        return Err(Hold::Idle);
    };
    *last = Some(job.series.clone());
    if *busy.borrow() {
        tracing::debug!(series = %job.series, gate = "outbox", "file trickle: waiting");
        return Err(Hold::Gated);
    }
    let now = (cfg.clock)();
    if crate::group_runtime::resend_headroom(&state.group.unwrap_or_default(), now) < BUDGET_HEADROOM {
        tracing::debug!(series = %job.series, gate = "budget", "file trickle: waiting");
        return Err(Hold::Gated);
    }
    let day = now / 86_400;
    let sent = if state.file_jobs.sent_day == day {
        state.file_jobs.sent_bytes
    } else {
        0
    };
    if sent.saturating_add(PIECE_WIRE_BYTES) > cfg.daily_bytes {
        tracing::debug!(series = %job.series, gate = "daily", "file trickle: waiting");
        return Err(Hold::Gated);
    }
    let series = job.series.clone();
    // a mirror's stored piece: verified when it was stored, published as is
    if job.stored {
        let Some(index) = index_at(&job.ranges, job.next) else {
            drop_job(store, series.clone(), "malformed job".into()).await;
            return Err(Hold::Failed);
        };
        let path = std::path::Path::new(&job.path).join(index.to_string());
        let content = match tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                drop_job(store, series.clone(), format!("reading stored piece {index}: {e}")).await;
                return Err(Hold::Failed);
            }
            Err(e) => {
                drop_job(store, series.clone(), format!("piece task: {e}")).await;
                return Err(Hold::Failed);
            }
        };
        return match publish_content_paced(chan, &content, crate::ritual_net::now_secs()).await {
            Ok(_) => {
                advance(store, &job, day).await;
                tracing::debug!(series = %job.series, index, "file trickle: stored piece published");
                Ok(())
            }
            Err(e) => {
                tracing::debug!(series = %job.series, index, error = %e, "file trickle: stored piece held");
                Err(Hold::Failed)
            }
        };
    }
    // K6: this holder's own folded wiki base. Every piece is decrypted
    // from the sealed file for exactly this publish - the knowledge base
    // never exists in plaintext on disk, the way a shared file does.
    if job.wiki_base {
        return publish_wiki_base_piece(chan, store, &job, manifests, day).await;
    }
    let manifest = match manifests.get(&job.series) {
        Some(m) => m.clone(),
        None => {
            let path = std::path::PathBuf::from(&job.path);
            let built = tokio::task::spawn_blocking(move || {
                std::fs::File::open(&path).and_then(manifest_of_reader)
            })
            .await;
            match built {
                Ok(Ok((m, _))) if m.root() == job.root && m.count == job.count => {
                    manifests.insert(job.series.clone(), m.clone());
                    m
                }
                Ok(Ok(_)) => {
                    drop_job(store, series.clone(), "the file changed since it was shared".into()).await;
                    return Err(Hold::Failed);
                }
                Ok(Err(e)) => {
                    drop_job(store, series.clone(), format!("reading the shared file: {e}")).await;
                    return Err(Hold::Failed);
                }
                Err(e) => {
                    drop_job(store, series.clone(), format!("manifest task: {e}")).await;
                    return Err(Hold::Failed);
                }
            }
        }
    };
    let (Some(index), Ok(key)) = (
        index_at(&job.ranges, job.next),
        <[u8; 32]>::try_from(job.key.as_slice()),
    ) else {
        drop_job(store, series.clone(), "malformed job".into()).await;
        return Err(Hold::Failed);
    };
    let framed = {
        let (manifest, job) = (manifest.clone(), job.clone());
        tokio::task::spawn_blocking(move || piece_bytes(&job, &manifest, index)).await
    };
    let framed = match framed {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            manifests.remove(&job.series);
            drop_job(store, series.clone(), e.to_string()).await;
            return Err(Hold::Failed);
        }
        Err(e) => {
            drop_job(store, series.clone(), format!("piece task: {e}")).await;
            return Err(Hold::Failed);
        }
    };
    let stamp = crate::ritual_net::now_secs();
    match publish_piece_paced(chan, &outer_key(&key), &framed, stamp).await {
        Ok(_) => {
            advance(store, &job, day).await;
            tracing::debug!(series = %job.series, index, "file trickle: piece published");
            Ok(())
        }
        Err(e) => {
            if transient_publish_error(&e) {
                tracing::debug!(series = %job.series, index, error = %e, "file trickle: piece held");
            } else {
                tracing::warn!(series = %job.series, index, error = %e, "file trickle: piece refused");
            }
            Err(Hold::Failed)
        }
    }
}

/// One piece of the folded wiki base (K6). The manifest is built once
/// from the holder's own sealed copy and cached like any other series';
/// a base that is not here yet HOLDS the job rather than dropping it,
/// because the very next cut may bring it.
async fn publish_wiki_base_piece<S: StateStore>(
    chan: &GroupChannel,
    store: &S,
    job: &PublishJob,
    manifests: &mut HashMap<String, Manifest>,
    day: u64,
) -> Result<(), Hold> {
    let series = job.series.clone();
    let manifest = match manifests.get(&series) {
        Some(m) => m.clone(),
        None => {
            let mut hashes = Vec::new();
            let mut size = 0u64;
            for i in 0..job.count {
                let Some(piece) = store.wiki_base_piece(i).await else {
                    tracing::debug!(series = %series, gate = "no base", "file trickle: waiting");
                    return Err(Hold::Gated);
                };
                size = size.saturating_add(u64::try_from(piece.len()).unwrap_or(0));
                hashes.push(<[u8; 32]>::from(sha2::Sha256::digest(&piece)));
            }
            let m = Manifest { count: job.count, size, hashes };
            // no root to compare: the source IS this holder's own base, so
            // the root would be a self-comparison. The size is what catches
            // a store that moved under the job, and the FETCHER checks the
            // assembled bytes against the chain's commitment.
            if m.size != job.size {
                drop_job(store, series, "the wiki base moved since the job was queued".into()).await;
                return Err(Hold::Failed);
            }
            manifests.insert(job.series.clone(), m.clone());
            m
        }
    };
    let (Some(index), Ok(key)) = (
        index_at(&job.ranges, job.next),
        <[u8; 32]>::try_from(job.key.as_slice()),
    ) else {
        drop_job(store, series, "malformed job".into()).await;
        return Err(Hold::Failed);
    };
    let framed = match meta_piece(&manifest, index) {
        Ok(Some(meta)) => meta,
        Ok(None) => {
            let Some(slice) = store.wiki_base_piece(index).await else {
                return Err(Hold::Gated);
            };
            match frame_piece(index, manifest.count, &slice) {
                Ok(f) => f,
                Err(e) => {
                    drop_job(store, series, e.to_string()).await;
                    return Err(Hold::Failed);
                }
            }
        }
        Err(e) => {
            manifests.remove(&series);
            drop_job(store, series, e.to_string()).await;
            return Err(Hold::Failed);
        }
    };
    let stamp = crate::ritual_net::now_secs();
    match publish_piece_paced(chan, &outer_key(&key), &framed, stamp).await {
        Ok(_) => {
            advance(store, job, day).await;
            tracing::debug!(series = %series, index, "file trickle: wiki base piece published");
            Ok(())
        }
        Err(e) => {
            tracing::debug!(series = %series, index, error = %e, "file trickle: wiki base piece held");
            Err(Hold::Failed)
        }
    }
}

/// One piece went out: the day's counter and the job's cursor advance, a
/// finished job leaves the queue.
async fn advance<S: StateStore>(store: &S, job: &PublishJob, day: u64) {
    store
        .update(|s| {
            if s.file_jobs.sent_day != day {
                s.file_jobs.sent_day = day;
                s.file_jobs.sent_bytes = 0;
            }
            s.file_jobs.sent_bytes = s.file_jobs.sent_bytes.saturating_add(PIECE_WIRE_BYTES);
            if let Some(j) = s.file_jobs.publish.iter_mut().find(|j| j.series == job.series) {
                j.next = j.next.saturating_add(1);
            }
            s.file_jobs
                .publish
                .retain(|j| u64::from(j.next) < job_total(&j.ranges));
            true
        })
        .await;
}

/// Forget a job that can never complete (the file changed or is gone).
async fn drop_job<S: StateStore>(store: &S, series: String, why: String) {
    tracing::warn!(series = %series, reason = %why, "file trickle: dropping the job");
    store
        .update(|s| {
            let before = s.file_jobs.publish.len();
            s.file_jobs.publish.retain(|j| j.series != series);
            before != s.file_jobs.publish.len()
        })
        .await;
}

/// The framed piece at `index` when it is NOT a data slice: the top
/// record or a manifest chunk. `None` means "this is a data slice", which
/// every source answers its own way.
fn meta_piece(manifest: &Manifest, index: u32) -> Result<Option<Vec<u8>>, NetError> {
    let layout = Manifest::layout_for(manifest.count)
        .ok_or_else(|| NetError::Framing("the series exceeds the largest layout".into()))?;
    if index == layout.top {
        return frame_piece(index, manifest.count, &manifest.top_bytes()).map(Some);
    }
    if index > layout.top {
        return Err(NetError::Framing(format!("piece {index} is outside the series")));
    }
    if index >= manifest.count {
        return frame_piece(index, manifest.count, &manifest.chunk(index - manifest.count)).map(Some);
    }
    Ok(None)
}

/// The framed piece at `index`: a data slice read at its offset and
/// checked against the manifest (a changed file is refused), a manifest
/// chunk, or the top record.
fn piece_bytes(job: &PublishJob, manifest: &Manifest, index: u32) -> Result<Vec<u8>, NetError> {
    use std::io::Seek as _;
    if let Some(meta) = meta_piece(manifest, index)? {
        return Ok(meta);
    }
    let want_len = expected_slice_len(index, manifest.count, manifest.size)
        .ok_or_else(|| NetError::Framing("slice geometry".into()))?;
    let mut file = std::fs::File::open(&job.path)
        .map_err(|e| NetError::Framing(format!("opening the shared file: {e}")))?;
    let offset = u64::from(index)
        .checked_mul(u64::try_from(PIECE_PAYLOAD_LEN).unwrap_or(u64::MAX))
        .ok_or_else(|| NetError::Framing("slice offset".into()))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| NetError::Framing(format!("seeking the shared file: {e}")))?;
    let mut buf = vec![0u8; want_len];
    let n = read_full(&mut file, &mut buf)
        .map_err(|e| NetError::Framing(format!("reading the shared file: {e}")))?;
    let slice = &buf[..n];
    let want = manifest
        .hashes
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .ok_or_else(|| NetError::Framing("manifest shorter than its count".into()))?;
    if n != want_len || <[u8; 32]>::from(sha2::Sha256::digest(slice)) != *want {
        return Err(NetError::Framing(format!(
            "slice {index} no longer matches the share - the file changed"
        )));
    }
    frame_piece(index, manifest.count, slice)
}

use sha2::Digest as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// A range job goes before a whole-series job; whole jobs take turns.
    #[test]
    fn the_pick_serves_ranges_first_and_whole_jobs_in_turn() {
        let job = |series: &str, ranges: Vec<(u32, u32)>| PublishJob {
            series: series.into(),
            key: vec![0; 32],
            path: String::new(),
            count: 3,
            size: 3,
            root: String::new(),
            ranges,
            next: 0,
            started_at: 0,
            stored: false,
            wiki_base: false,
        };
        let whole = whole_series_ranges(Manifest::layout_for(3).expect("layout"));
        let jobs = vec![job("a", whole.clone()), job("b", vec![(1, 1)]), job("c", whole)];
        assert_eq!(pick_job(&jobs, None).map(|j| j.series), Some("b".into()), "the range job first");
        let wholes = vec![jobs[0].clone(), jobs[2].clone()];
        assert_eq!(pick_job(&wholes, None).map(|j| j.series), Some("a".into()));
        assert_eq!(pick_job(&wholes, Some("a")).map(|j| j.series), Some("c".into()));
        assert_eq!(pick_job(&wholes, Some("c")).map(|j| j.series), Some("a".into()), "round-robin");
        assert!(pick_job(&[], None).is_none());
    }

    /// The range walk: positions map onto indices in range order.
    #[test]
    fn positions_walk_the_ranges_in_order() {
        let ranges = vec![(9, 9), (3, 4), (0, 2)];
        assert_eq!(job_total(&ranges), 6);
        let walked: Vec<Option<u32>> = (0..7).map(|p| index_at(&ranges, p)).collect();
        assert_eq!(walked, vec![Some(9), Some(3), Some(4), Some(0), Some(1), Some(2), None]);
        let sealed = (crate::file_plane::PIECE_HEADER_LEN + PIECE_PAYLOAD_LEN + 12 + 16).div_ceil(3) * 4;
        assert_eq!(u64::try_from(sealed).expect("fits"), PIECE_WIRE_BYTES);
    }
}
