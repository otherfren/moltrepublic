// SPDX-License-Identifier: GPL-3.0-or-later

//! The file-transfer task cores: what actually moves a shared file's bytes
//! between two members' disks — off the actor, off the event log, over a
//! dedicated queue pair in the recovery side-channel pattern
//! (`recovery.rs` is the model: mint → advertise inside MLS ciphertext →
//! stream framed messages → feed results back as internal `Command`s).
//!
//! Sharer side: [`run_file_serve`] answers a decrypted
//! [`molt_net::transfer::FetchRequest`] by streaming `Manifest` + `Piece`
//! frames to the requester's reply queue, throttled by a window of acks on
//! its own ack queue. Requester side: [`run_file_fetch`] mints that reply
//! queue, has the actor record the MLS-encrypted request, reassembles the
//! pieces straight into a `.part` file, verifies size + sha256 against the
//! log-anchored share checksum, and renames on success. Both cores are
//! generic over [`Transport`], so the loopback tests drive the exact code
//! SMP runs in production.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{ChannelRef, Command, MessageId};
use molt_net::invite::ReplyHandover;
use molt_net::transfer::{
    decode_ack, decode_frame, encode_ack, encode_frame, pieces_for, FetchRequest, TransferAck,
    TransferFrame, PIECE_LEN64, PIECE_WINDOW,
};
use molt_net::{msg_id, supervisor, QueueId, SndQueueAddr, Transport, WrapKey};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::Envelope;

/// How long a fetch waits for the sharer's `Manifest` before giving up —
/// the sharer may simply be offline.
pub(crate) const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a fetch waits for each subsequent frame once the transfer runs.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the sharer waits for the next ack before abandoning a serve.
const ACK_TIMEOUT: Duration = Duration::from_secs(120);
/// A fetch request expires this long after minting (the mesh outbox is
/// store-and-forward — a request must not trigger a serve hours later,
/// when the requester's recv loop is long gone).
pub(crate) const REQUEST_TTL_SECS: u64 = 600;

/// The fetch's timeout knobs, injectable so tests fail in milliseconds.
#[derive(Clone, Copy)]
pub(crate) struct FetchTimeouts {
    pub manifest: Duration,
    pub idle: Duration,
}

impl Default for FetchTimeouts {
    fn default() -> Self {
        FetchTimeouts {
            manifest: MANIFEST_TIMEOUT,
            idle: IDLE_TIMEOUT,
        }
    }
}

/// What the requester knows about the share it fetches (from its own
/// replayed chat log — the trust anchor).
#[derive(Clone)]
pub(crate) struct FetchTarget {
    /// The share message id, lowercase hex.
    pub id_hex: String,
    /// The shared file's name (the default destination file name).
    pub name: String,
    /// The log-anchored size.
    pub size: u64,
    /// The log-anchored sha256 ("" on legacy shares — then the manifest's
    /// recomputed hash is the only reference).
    pub checksum: String,
}

/// Where a fetched file lands: an explicit destination (file or existing
/// directory) or the session's default download directory.
#[derive(Clone)]
pub(crate) struct DestSpec {
    /// The explicit `dest` argument, if any.
    pub explicit: Option<String>,
    /// The session's download directory (`~` unexpanded).
    pub default_dir: String,
}

impl DestSpec {
    /// Resolve to `(directory, file name)` — an explicit existing directory
    /// keeps the share name, an explicit path is taken literally, no
    /// explicit destination lands in the default download directory.
    fn resolve(&self, share_name: &str) -> (PathBuf, String) {
        match &self.explicit {
            Some(dest) => {
                let p = molt_storage::expand_tilde(dest);
                if p.is_dir() {
                    (p, sanitize_file_name(share_name))
                } else {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| sanitize_file_name(share_name));
                    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
                    (dir, name)
                }
            }
            None => (
                molt_storage::expand_tilde(&self.default_dir),
                sanitize_file_name(share_name),
            ),
        }
    }
}

/// A share's file name is peer input — keep only the last path component
/// so a hostile name ("../../.bashrc") cannot escape the destination.
fn sanitize_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if base.is_empty() || base == "." || base == ".." {
        "download".to_string()
    } else {
        base
    }
}

/// First free variant of `dir/name`: `name.ext`, `name (1).ext`, …
fn resolve_collision(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (name.to_string(), None),
    };
    for n in 1u32.. {
        let next = match &ext {
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !next.exists() {
            return next;
        }
    }
    unreachable!("u32 collision counter exhausted")
}

/// Parse a [`ReplyHandover`] into a send address + wrap key.
fn parse_handover(h: &ReplyHandover) -> Result<(SndQueueAddr, WrapKey), String> {
    let qid = hex::decode(&h.queue_id).map_err(|e| format!("bad handover queue id: {e}"))?;
    let wrap_bytes: [u8; 32] = hex::decode(&h.wrap)
        .map_err(|e| format!("bad handover wrap key: {e}"))?
        .try_into()
        .map_err(|_| "handover wrap key is not 32 bytes".to_string())?;
    Ok((
        SndQueueAddr {
            server: h.server.clone(),
            id: QueueId::from_bytes(qid),
        },
        WrapKey::from_bytes(wrap_bytes),
    ))
}

/// Stream a file's sha256 (64 KiB buffer) plus its byte count.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += u64::try_from(n).unwrap_or(0); // usize fits u64 here
        h.update(&buf[..n]);
    }
    Ok((hex::encode(h.finalize()), total))
}

// ---------------------------------------------------------------------------
// Sharer side
// ---------------------------------------------------------------------------

/// Serve one fetch request: stream the file at `path` to the requester's
/// reply queue. `expected_size`/`expected_modified` are the share's
/// recorded metadata — a file that changed since the share is refused
/// (honest error), matching-content verification is the requester's
/// checksum job. Returns `Err` only for tracing; the requester learns of
/// failures via `Refused` or its own timeouts.
pub(crate) async fn run_file_serve<T: Transport>(
    transport: T,
    path: PathBuf,
    expected_size: u64,
    expected_modified: u64,
    share_id_hex: String,
    reply: ReplyHandover,
) -> Result<(), String> {
    let (reply_snd, reply_wrap) = parse_handover(&reply)?;
    let refuse = |reason: String| {
        let transport = transport.clone();
        let reply_snd = reply_snd.clone();
        let reply_wrap = reply_wrap.clone();
        let share_id_hex = share_id_hex.clone();
        async move {
            let frame = TransferFrame::Refused {
                id: share_id_hex.clone(),
                reason: reason.clone(),
            };
            if let Ok(bytes) = encode_frame(&frame) {
                let _ = supervisor::send_framed(
                    &transport,
                    &reply_snd,
                    &reply_wrap,
                    msg_id(&share_id_hex, "fetch", 0),
                    &bytes,
                )
                .await;
            }
            Err::<(), String>(reason)
        }
    };

    // honesty checks against the recorded share: the file must still be
    // what was shared (same size + mtime; content tampering at equal
    // size/mtime is caught by the requester's checksum verification)
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return refuse(format!("the shared file is gone: {e}")).await,
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if meta.len() != expected_size || (expected_modified != 0 && modified != expected_modified) {
        return refuse(format!(
            "the file changed since it was shared (size {} → {})",
            expected_size,
            meta.len()
        ))
        .await;
    }
    let (sha256, size) = match tokio::task::block_in_place(|| hash_file(&path)) {
        Ok(x) => x,
        Err(e) => return refuse(format!("reading the shared file failed: {e}")).await,
    };

    // the ack queue: subscribe BEFORE advertising it in the manifest
    let ack_q = transport.create_queue().await.map_err(|e| e.to_string())?;
    let ack_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
    let mut ack_rx = transport.subscribe(&ack_q.rcv).await.map_err(|e| e.to_string())?;

    let pieces = pieces_for(size);
    let manifest = TransferFrame::Manifest {
        id: share_id_hex.clone(),
        size,
        pieces,
        sha256,
        ack: ReplyHandover {
            server: ack_q.snd.server.clone(),
            queue_id: hex::encode(&ack_q.snd.id.0),
            wrap: hex::encode(ack_wrap.to_bytes()),
        },
    };
    let bytes = encode_frame(&manifest).map_err(|e| e.to_string())?;
    supervisor::send_framed(
        &transport,
        &reply_snd,
        &reply_wrap,
        msg_id(&share_id_hex, "fetch", 0),
        &bytes,
    )
    .await
    .map_err(|e| e.to_string())?;

    // stream the pieces, at most PIECE_WINDOW unacked in flight
    let result = async {
        use std::io::Read as _;
        let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut reasm = molt_net::Reassembler::new();
        let mut sent: u32 = 0;
        let mut acked: u32 = 0;
        while acked < pieces {
            while sent < pieces && sent - acked < PIECE_WINDOW {
                let want =
                    usize::try_from((size - u64::from(sent) * PIECE_LEN64).min(PIECE_LEN64))
                        .map_err(|_| "piece length overflow".to_string())?;
                let mut buf = vec![0u8; want];
                f.read_exact(&mut buf)
                    .map_err(|e| format!("reading piece {sent}: {e}"))?;
                let frame = TransferFrame::Piece {
                    index: sent,
                    bytes: buf,
                };
                let bytes = encode_frame(&frame).map_err(|e| e.to_string())?;
                supervisor::send_framed(
                    &transport,
                    &reply_snd,
                    &reply_wrap,
                    msg_id(&share_id_hex, "fetch", u64::from(sent) + 1),
                    &bytes,
                )
                .await
                .map_err(|e| e.to_string())?;
                sent += 1;
            }
            // await the next ack (Received advances the window)
            let deadline = tokio::time::Instant::now() + ACK_TIMEOUT;
            'ack: loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let Ok(received) = tokio::time::timeout(remaining, ack_rx.recv()).await else {
                    return Err("timed out waiting for the requester's ack".to_string());
                };
                let Some(delivery) = received else {
                    return Err("the ack queue closed".to_string());
                };
                let Ok(plain) = molt_net::wrap::unwrap_block(&ack_wrap, &delivery.block) else {
                    delivery.ack.ack();
                    continue;
                };
                let outcome = reasm.push(&plain);
                delivery.ack.ack();
                let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
                    continue;
                };
                match decode_ack(&bytes) {
                    Ok(TransferAck::Received { .. }) => {
                        acked += 1;
                        break 'ack;
                    }
                    Ok(TransferAck::Abort { reason }) => {
                        return Err(format!("the requester aborted: {reason}"));
                    }
                    Err(_) => continue,
                }
            }
        }
        Ok(())
    }
    .await;

    let _ = transport.delete_queue(&ack_q.rcv).await;
    result
}

// ---------------------------------------------------------------------------
// Requester side
// ---------------------------------------------------------------------------

/// Fetch one shared file: mint the reply queue, let the actor record the
/// MLS-encrypted request (via `announce` — it returns whether the event
/// was recorded), then reassemble the sharer's frames into
/// `.molt-download-<id>.part`, verify size + sha256, and rename to the
/// collision-free destination. The `.part` is removed on ANY failure.
pub(crate) async fn run_file_fetch<T, F, Fut, P>(
    transport: T,
    group: Arc<Mutex<molt_net::MlsMember>>,
    target: FetchTarget,
    dest: DestSpec,
    timeouts: FetchTimeouts,
    announce: F,
    mut progress: P,
) -> Result<PathBuf, String>
where
    T: Transport,
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
    P: FnMut(u64, u64),
{
    // the reply queue we receive the transfer on — subscribe before
    // advertising it, so the manifest cannot race our subscription
    let reply_q = transport.create_queue().await.map_err(|e| e.to_string())?;
    let reply_wrap = WrapKey::fresh().map_err(|e| e.to_string())?;
    let mut rx = transport.subscribe(&reply_q.rcv).await.map_err(|e| e.to_string())?;

    let request = FetchRequest {
        id: target.id_hex.clone(),
        reply: ReplyHandover {
            server: reply_q.snd.server.clone(),
            queue_id: hex::encode(&reply_q.snd.id.0),
            wrap: hex::encode(reply_wrap.to_bytes()),
        },
        expires: crate::now_secs() + REQUEST_TTL_SECS,
    };
    let request_json = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let ct = {
        let mut g = group.lock().map_err(|_| "mls group lock poisoned".to_string())?;
        hex::encode(g.encrypt(&request_json).map_err(|e| e.to_string())?)
    };
    let result = fetch_frames(
        &transport,
        &reply_wrap,
        &target,
        &dest,
        timeouts,
        announce,
        ct,
        &mut rx,
        &mut progress,
    )
    .await;
    let _ = transport.delete_queue(&reply_q.rcv).await;
    result
}

/// The receive half of [`run_file_fetch`], factored out so queue cleanup
/// has one exit path.
#[allow(clippy::too_many_arguments)]
async fn fetch_frames<T, F, Fut, P>(
    transport: &T,
    reply_wrap: &WrapKey,
    target: &FetchTarget,
    dest: &DestSpec,
    timeouts: FetchTimeouts,
    announce: F,
    ct: String,
    rx: &mut mpsc::Receiver<molt_net::Delivery>,
    progress: &mut P,
) -> Result<PathBuf, String>
where
    T: Transport,
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
    P: FnMut(u64, u64),
{
    if !announce(ct).await {
        return Err("the fetch request could not be recorded".to_string());
    }

    let (dest_dir, file_name) = dest.resolve(&target.name);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("creating {}: {e}", dest_dir.display()))?;
    let part_path = dest_dir.join(format!(".molt-download-{}.part", target.id_hex));
    let cleanup = |part: &Path| {
        let _ = std::fs::remove_file(part);
    };

    let result = async {
        let mut part = std::fs::File::create(&part_path)
            .map_err(|e| format!("creating {}: {e}", part_path.display()))?;
        let mut reasm = molt_net::Reassembler::new();
        let mut hasher = Sha256::new();
        let mut manifest: Option<(u64, u32, String, SndQueueAddr, WrapKey)> = None;
        let mut pending: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut next_write: u32 = 0;
        let mut written: u64 = 0;
        let mut ack_seq: u64 = 0;
        let mut deadline = tokio::time::Instant::now() + timeouts.manifest;

        loop {
            // an empty file completes right after its manifest
            if let Some((size, pieces, _, _, _)) = &manifest {
                if next_write >= *pieces && written == *size {
                    break;
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Ok(received) = tokio::time::timeout(remaining, rx.recv()).await else {
                return Err(if manifest.is_none() {
                    "the sharer did not answer — it may be offline; retry when it is back"
                        .to_string()
                } else {
                    "the transfer stalled — the sharer may have gone offline".to_string()
                });
            };
            let Some(delivery) = received else {
                return Err("the reply queue closed mid-transfer".to_string());
            };
            let Ok(plain) = molt_net::wrap::unwrap_block(reply_wrap, &delivery.block) else {
                delivery.ack.ack();
                continue;
            };
            let outcome = reasm.push(&plain);
            delivery.ack.ack();
            let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
                continue;
            };
            deadline = tokio::time::Instant::now() + timeouts.idle;
            match decode_frame(&bytes) {
                Ok(TransferFrame::Manifest {
                    id,
                    size,
                    pieces,
                    sha256,
                    ack,
                }) => {
                    if id != target.id_hex || manifest.is_some() {
                        continue;
                    }
                    if size != target.size {
                        return Err(format!(
                            "the file changed since it was shared (size {} → {size})",
                            target.size
                        ));
                    }
                    if !target.checksum.is_empty() && sha256 != target.checksum {
                        return Err(
                            "the file changed since it was shared (checksum mismatch)".to_string()
                        );
                    }
                    if pieces != pieces_for(size) {
                        return Err("the manifest's piece count is inconsistent".to_string());
                    }
                    let (ack_snd, ack_wrap) = parse_handover(&ack)?;
                    manifest = Some((size, pieces, sha256, ack_snd, ack_wrap));
                }
                Ok(TransferFrame::Piece { index, bytes }) => {
                    let Some((size, pieces, _, ack_snd, ack_wrap)) = &manifest else {
                        continue; // pieces before the manifest — drop
                    };
                    if index >= *pieces || index < next_write || pending.contains_key(&index) {
                        continue; // out of range or duplicate
                    }
                    pending.insert(index, bytes);
                    // write in order, hash as we go
                    while let Some(chunk) = pending.remove(&next_write) {
                        hasher.update(&chunk);
                        part.write_all(&chunk)
                            .map_err(|e| format!("writing {}: {e}", part_path.display()))?;
                        written += u64::try_from(chunk.len()).unwrap_or(0);
                        next_write += 1;
                    }
                    if written > *size {
                        return Err("the sharer sent more bytes than announced".to_string());
                    }
                    progress(written, *size);
                    // ack the piece (flow control)
                    ack_seq += 1;
                    let ack_frame = encode_ack(&TransferAck::Received { index })
                        .map_err(|e| e.to_string())?;
                    supervisor::send_framed(
                        transport,
                        ack_snd,
                        ack_wrap,
                        msg_id(&target.id_hex, "ack", ack_seq),
                        &ack_frame,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
                Ok(TransferFrame::Refused { id, reason }) => {
                    if id == target.id_hex {
                        return Err(reason);
                    }
                }
                Err(_) => continue,
            }
        }

        // verify: exact size and the log-anchored checksum (or, on a
        // legacy share without one, the manifest's recomputed hash)
        let (size, _, manifest_sha, _, _) =
            manifest.as_ref().ok_or("no manifest arrived".to_string())?;
        if written != *size {
            return Err("the transfer ended short".to_string());
        }
        let got = hex::encode(hasher.finalize());
        let want = if target.checksum.is_empty() {
            manifest_sha
        } else {
            &target.checksum
        };
        if &got != want {
            return Err(
                "checksum mismatch — the bytes served are not the file that was shared"
                    .to_string(),
            );
        }
        part.sync_all()
            .map_err(|e| format!("syncing {}: {e}", part_path.display()))?;
        drop(part);
        let final_path = resolve_collision(&dest_dir, &file_name);
        std::fs::rename(&part_path, &final_path)
            .map_err(|e| format!("moving into place: {e}"))?;
        Ok(final_path)
    }
    .await;

    if result.is_err() {
        cleanup(&part_path);
    }
    result
}

// ---------------------------------------------------------------------------
// Engine spawners (off-actor tasks feeding results back as commands)
// ---------------------------------------------------------------------------

/// Send one internal command back to the actor (fire-and-forget).
async fn feed(cmd_tx: &mpsc::Sender<Envelope>, cmd: Command) {
    let (reply, _rx) = tokio::sync::oneshot::channel();
    let _ = cmd_tx.send(Envelope { cmd, reply }).await;
}

/// Hash a file off the actor and report the share metadata back
/// (`NetFileShared` / `NetFileShareFailed`).
pub(crate) fn spawn_share_hash(
    path: PathBuf,
    channel: ChannelRef,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    tokio::spawn(async move {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let stat = tokio::task::spawn_blocking({
            let path = path.clone();
            move || -> std::io::Result<(String, u64, u64)> {
                let meta = std::fs::metadata(&path)?;
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let (checksum, size) = hash_file(&path)?;
                Ok((checksum, size, modified))
            }
        })
        .await;
        let cmd = match stat {
            Ok(Ok((checksum, size, modified))) => Command::NetFileShared {
                kind: molt_core::file_kind_label(&name),
                name,
                size,
                modified,
                checksum,
                path: path.display().to_string(),
                channel,
                generation: Some(scope),
            },
            Ok(Err(e)) => Command::NetFileShareFailed {
                name,
                reason: e.to_string(),
                generation: Some(scope),
            },
            Err(e) => Command::NetFileShareFailed {
                name,
                reason: format!("hashing task failed: {e}"),
                generation: Some(scope),
            },
        };
        feed(&cmd_tx, cmd).await;
    });
}

/// Kick a peer-to-peer fetch off the actor: the announce closure records
/// the `FileRequested` event on the actor (so the outbox ships it), then
/// the transfer streams to disk; progress/done/failed come back as
/// `NetFile*` commands guarded by `scope`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_file_fetch(
    transport: crate::founding::RitualTransport,
    group: Arc<Mutex<molt_net::MlsMember>>,
    id: MessageId,
    target: FetchTarget,
    dest: DestSpec,
    timeouts: FetchTimeouts,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    tokio::spawn(async move {
        let announce_tx = cmd_tx.clone();
        let announce = |ct: String| async move {
            let (reply, rx) = tokio::sync::oneshot::channel();
            if announce_tx
                .send(Envelope {
                    cmd: Command::NetFileRequestReady {
                        id,
                        ct,
                        generation: Some(scope),
                    },
                    reply,
                })
                .await
                .is_err()
            {
                return false;
            }
            matches!(rx.await, Ok(Ok(_)))
        };
        // progress throttle: at most one report per percent point / 250 ms
        let progress_tx = cmd_tx.clone();
        let mut last_pct: u64 = 0;
        let mut last_at = tokio::time::Instant::now();
        let progress = move |transferred: u64, total: u64| {
            let pct = (transferred * 100).checked_div(total).unwrap_or(100);
            let now = tokio::time::Instant::now();
            if pct > last_pct && now.duration_since(last_at) >= Duration::from_millis(250) {
                last_pct = pct;
                last_at = now;
                let tx = progress_tx.clone();
                let cmd = Command::NetFileProgress {
                    id,
                    transferred,
                    total,
                    generation: Some(scope),
                };
                tokio::spawn(async move { feed(&tx, cmd).await });
            }
        };
        let result = run_file_fetch(transport, group, target, dest, timeouts, announce, progress)
            .await;
        let cmd = match result {
            Ok(path) => Command::NetFileDone {
                id,
                path: path.display().to_string(),
                generation: Some(scope),
            },
            Err(reason) => Command::NetFileFailed {
                id,
                reason,
                generation: Some(scope),
            },
        };
        feed(&cmd_tx, cmd).await;
    });
}

/// Serve one fetch request off the actor, bounded by the serve semaphore
/// (a busy node queues further requests instead of saturating its uplink).
pub(crate) fn spawn_file_serve(
    transport: crate::founding::RitualTransport,
    path: PathBuf,
    expected_size: u64,
    expected_modified: u64,
    share_id_hex: String,
    reply: ReplyHandover,
    slots: Arc<tokio::sync::Semaphore>,
) {
    tokio::spawn(async move {
        let Ok(_permit) = slots.acquire_owned().await else {
            return; // the engine is shutting down
        };
        if let Err(e) = run_file_serve(
            transport,
            path,
            expected_size,
            expected_modified,
            share_id_hex.clone(),
            reply,
        )
        .await
        {
            tracing::warn!(share = %share_id_hex, error = %e, "serving a file download failed");
        }
    });
}

/// Send one honest `Refused` frame to a requester's reply queue (a serve
/// that cannot start: unknown share, unavailable, path lost).
pub(crate) fn spawn_send_refusal(
    transport: crate::founding::RitualTransport,
    reply: ReplyHandover,
    frame: TransferFrame,
) {
    tokio::spawn(async move {
        let Ok((snd, wrap)) = parse_handover(&reply) else {
            return;
        };
        let Ok(bytes) = encode_frame(&frame) else {
            return;
        };
        let share = match &frame {
            TransferFrame::Refused { id, .. } => id.clone(),
            _ => String::new(),
        };
        if let Err(e) =
            supervisor::send_framed(&transport, &snd, &wrap, msg_id(&share, "fetch", 0), &bytes)
                .await
        {
            tracing::debug!(error = %e, "sending a refusal failed (requester gone?)");
        }
    });
}

/// Copy the node's OWN share to the destination (the sharer downloading
/// its own file needs no network — but the same dest/collision/.part rules
/// and the same completion commands apply, so the GUI flow is identical).
pub(crate) fn spawn_local_copy(
    source: PathBuf,
    id: MessageId,
    target: FetchTarget,
    dest: DestSpec,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
            let (dest_dir, file_name) = dest.resolve(&target.name);
            std::fs::create_dir_all(&dest_dir)
                .map_err(|e| format!("creating {}: {e}", dest_dir.display()))?;
            let (checksum, _) =
                hash_file(&source).map_err(|e| format!("reading the shared file failed: {e}"))?;
            if !target.checksum.is_empty() && checksum != target.checksum {
                return Err("the file changed since it was shared (checksum mismatch)".to_string());
            }
            let part = dest_dir.join(format!(".molt-download-{}.part", target.id_hex));
            std::fs::copy(&source, &part).map_err(|e| format!("copying: {e}"))?;
            let final_path = resolve_collision(&dest_dir, &file_name);
            std::fs::rename(&part, &final_path).map_err(|e| {
                let _ = std::fs::remove_file(&part);
                format!("moving into place: {e}")
            })?;
            Ok(final_path)
        })
        .await
        .unwrap_or_else(|e| Err(format!("copy task failed: {e}")));
        let cmd = match result {
            Ok(path) => Command::NetFileDone {
                id,
                path: path.display().to_string(),
                generation: Some(scope),
            },
            Err(reason) => Command::NetFileFailed {
                id,
                reason,
                generation: Some(scope),
            },
        };
        feed(&cmd_tx, cmd).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_net::{LoopbackHub, LoopbackTransport};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// Deterministic pseudo-random content of `n` bytes.
    fn content(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| u8::try_from((i * 31 + 7) % 251).expect("byte range"))
            .collect()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// A real two-member MLS group (the control plane's authentication).
    fn mls_pair() -> (Arc<Mutex<molt_net::MlsMember>>, Arc<Mutex<molt_net::MlsMember>>) {
        let sk_a = molt_storage::SigningKey::from_bytes(&[7u8; 32]);
        let sk_b = molt_storage::SigningKey::from_bytes(&[9u8; 32]);
        let mut a = molt_net::MlsMember::new(&sk_a, "sharer").expect("mls a");
        let mut b = molt_net::MlsMember::new(&sk_b, "requester").expect("mls b");
        a.create_group().expect("group");
        let kp = b.key_package().expect("kp");
        let welcome = a.add_members(&[kp]).expect("add").expect("welcome");
        b.join_from_welcome(&welcome).expect("join");
        (Arc::new(Mutex::new(a)), Arc::new(Mutex::new(b)))
    }

    /// Wire a fetch against a live serve over the loopback hub: the announce
    /// closure plays the mesh — it decrypts the requester's ct with the
    /// SHARER's group half and spawns the serve, exactly what the engine's
    /// `FileRequested` arm does. Returns the fetch result.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_against_serve(
        transport: LoopbackTransport,
        sharer_group: Arc<Mutex<molt_net::MlsMember>>,
        requester_group: Arc<Mutex<molt_net::MlsMember>>,
        source: PathBuf,
        expected_size: u64,
        target: FetchTarget,
        dest_dir: PathBuf,
        timeouts: FetchTimeouts,
        serve: bool,
    ) -> Result<PathBuf, String> {
        let announce_transport = transport.clone();
        let share_id = target.id_hex.clone();
        run_file_fetch(
            transport,
            requester_group,
            target,
            DestSpec {
                explicit: Some(dest_dir.display().to_string()),
                default_dir: "~/Downloads".to_string(),
            },
            timeouts,
            move |ct: String| async move {
                if !serve {
                    return true; // recorded, but the sharer never answers
                }
                let raw = hex::decode(&ct).expect("ct hex");
                let (from, plain) = match sharer_group
                    .lock()
                    .expect("lock")
                    .decrypt(&raw)
                    .expect("group decrypt")
                {
                    molt_net::MlsIncoming::Application { from, plaintext } => (from, plaintext),
                    other => panic!("unexpected mls message: {other:?}"),
                };
                assert_eq!(from, "requester", "MLS authenticates the requester");
                let req: FetchRequest = serde_json::from_slice(&plain).expect("request json");
                assert_eq!(req.id, share_id);
                tokio::spawn(run_file_serve(
                    announce_transport,
                    source,
                    expected_size,
                    0, // 0 = skip the mtime pin (exercised separately)
                    req.id,
                    req.reply,
                ));
                true
            },
            |_, _| {},
        )
        .await
    }

    /// The keystone: bytes leave the sharer's disk and land byte-identical
    /// on the requester's, verified against the log-anchored checksum.
    #[test]
    fn serve_and_fetch_are_byte_identical() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let bytes = content(8 * 1024);
            let source = tmp.path().join("doc.pdf");
            std::fs::write(&source, &bytes).expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();
            let got = fetch_against_serve(
                hub.transport(),
                a,
                b,
                source,
                u64::try_from(bytes.len()).expect("len fits u64"),
                FetchTarget {
                    id_hex: "ab".repeat(16),
                    name: "doc.pdf".to_string(),
                    size: u64::try_from(bytes.len()).expect("len fits u64"),
                    checksum: sha_hex(&bytes),
                },
                dest.clone(),
                FetchTimeouts::default(),
                true,
            )
            .await
            .expect("transfer succeeds");
            assert_eq!(got, dest.join("doc.pdf"));
            assert_eq!(std::fs::read(&got).expect("read"), bytes);
            assert!(
                !dest.join(format!(".molt-download-{}.part", "ab".repeat(16))).exists(),
                "the .part staging file is gone"
            );
        });
    }

    /// A multi-piece file (1 MiB + ragged tail = 5 pieces) exercises the
    /// windowed streaming + ack path and reports progress.
    #[test]
    fn a_one_mib_file_streams_in_pieces() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let bytes = content(1024 * 1024 + 17);
            assert_eq!(pieces_for(u64::try_from(bytes.len()).expect("len fits u64")), 5, "test premise");
            let source = tmp.path().join("big.bin");
            std::fs::write(&source, &bytes).expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();

            // inline fetch_against_serve, but with a counting progress hook
            let progress_hits = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let hits = progress_hits.clone();
            let announce_transport = hub.transport();
            let source_for_serve = source.clone();
            let size = u64::try_from(bytes.len()).expect("len fits u64");
            let got = run_file_fetch(
                hub.transport(),
                b,
                FetchTarget {
                    id_hex: "cd".repeat(16),
                    name: "big.bin".to_string(),
                    size,
                    checksum: sha_hex(&bytes),
                },
                DestSpec {
                    explicit: Some(dest.display().to_string()),
                    default_dir: "~/Downloads".to_string(),
                },
                FetchTimeouts::default(),
                move |ct: String| async move {
                    let raw = hex::decode(&ct).expect("ct hex");
                    let plain = match a.lock().expect("lock").decrypt(&raw).expect("decrypt") {
                        molt_net::MlsIncoming::Application { plaintext, .. } => plaintext,
                        other => panic!("unexpected: {other:?}"),
                    };
                    let req: FetchRequest = serde_json::from_slice(&plain).expect("json");
                    tokio::spawn(run_file_serve(
                        announce_transport,
                        source_for_serve,
                        size,
                        0,
                        req.id,
                        req.reply,
                    ));
                    true
                },
                move |transferred, total| {
                    assert!(transferred <= total);
                    hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                },
            )
            .await
            .expect("transfer succeeds");
            assert_eq!(std::fs::read(&got).expect("read"), bytes);
            assert_eq!(
                progress_hits.load(std::sync::atomic::Ordering::Relaxed),
                5,
                "one progress report per piece"
            );
        });
    }

    /// An empty file transfers (manifest only, zero pieces).
    #[test]
    fn an_empty_file_transfers() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let source = tmp.path().join("empty.txt");
            std::fs::write(&source, b"").expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();
            let got = fetch_against_serve(
                hub.transport(),
                a,
                b,
                source,
                0,
                FetchTarget {
                    id_hex: "ee".repeat(16),
                    name: "empty.txt".to_string(),
                    size: 0,
                    checksum: sha_hex(b""),
                },
                dest.clone(),
                FetchTimeouts::default(),
                true,
            )
            .await
            .expect("empty transfer succeeds");
            assert_eq!(std::fs::read(&got).expect("read"), b"");
        });
    }

    /// A file that changed since the share (size mismatch) is REFUSED by
    /// the sharer — an honest error, not silence, and no bytes land.
    #[test]
    fn a_changed_file_is_refused() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let bytes = content(4096);
            let source = tmp.path().join("doc.pdf");
            // the file GREW since the share (the share recorded 4096)
            let mut grown = bytes.clone();
            grown.push(0xff);
            std::fs::write(&source, &grown).expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();
            let err = fetch_against_serve(
                hub.transport(),
                a,
                b,
                source,
                u64::try_from(bytes.len()).expect("len fits u64"), // what the share recorded
                FetchTarget {
                    id_hex: "aa".repeat(16),
                    name: "doc.pdf".to_string(),
                    size: u64::try_from(bytes.len()).expect("len fits u64"),
                    checksum: sha_hex(&bytes),
                },
                dest.clone(),
                FetchTimeouts::default(),
                true,
            )
            .await
            .expect_err("a changed file must be refused");
            assert!(err.contains("changed"), "honest reason: {err}");
            assert!(
                std::fs::read_dir(&dest).expect("dir").next().is_none(),
                "nothing lands, no .part remains"
            );
        });
    }

    /// Same-size content tampering: the sharer serves bytes whose hash does
    /// not match the log-anchored share checksum → the fetch fails at the
    /// manifest, nothing lands.
    #[test]
    fn a_tampered_file_fails_the_checksum() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let bytes = content(4096);
            let mut tampered = bytes.clone();
            tampered[100] ^= 0x01; // same size, different content
            let source = tmp.path().join("doc.pdf");
            std::fs::write(&source, &tampered).expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();
            let err = fetch_against_serve(
                hub.transport(),
                a,
                b,
                source,
                u64::try_from(bytes.len()).expect("len fits u64"),
                FetchTarget {
                    id_hex: "bb".repeat(16),
                    name: "doc.pdf".to_string(),
                    size: u64::try_from(bytes.len()).expect("len fits u64"),
                    // the log anchors the ORIGINAL bytes' hash
                    checksum: sha_hex(&bytes),
                },
                dest.clone(),
                FetchTimeouts::default(),
                true,
            )
            .await
            .expect_err("tampered bytes must not land");
            assert!(err.contains("checksum") || err.contains("changed"), "reason: {err}");
            assert!(
                std::fs::read_dir(&dest).expect("dir").next().is_none(),
                "nothing lands, no .part remains"
            );
        });
    }

    /// A sharer that never answers times the fetch out with an honest
    /// offline hint, and the .part staging file is cleaned up.
    #[test]
    fn a_dead_sharer_times_the_fetch_out() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let source = tmp.path().join("doc.pdf");
            std::fs::write(&source, b"unserved").expect("source");
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (a, b) = mls_pair();
            let hub = LoopbackHub::calm();
            let err = fetch_against_serve(
                hub.transport(),
                a,
                b,
                source,
                8,
                FetchTarget {
                    id_hex: "dd".repeat(16),
                    name: "doc.pdf".to_string(),
                    size: 8,
                    checksum: String::new(),
                },
                dest.clone(),
                FetchTimeouts {
                    manifest: Duration::from_millis(300),
                    idle: Duration::from_millis(300),
                },
                false, // nobody serves
            )
            .await
            .expect_err("an unanswered fetch times out");
            assert!(err.contains("offline") || err.contains("did not answer"), "reason: {err}");
            assert!(
                std::fs::read_dir(&dest).expect("dir").next().is_none(),
                "the .part staging file is cleaned up"
            );
        });
    }
}

