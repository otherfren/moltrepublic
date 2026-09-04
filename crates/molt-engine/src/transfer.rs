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
    /// Series-v2 material (`docs_archive/files/mirroring.md` §3.1): the content
    /// key (base64), the piece count and the manifest root; "" / 0 on a
    /// legacy share, which fetches the exporter-sealed chunk series.
    pub key_b64: String,
    pub pieces: u32,
    pub root: String,
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

/// A resolved download destination: the directory, the file name, and
/// whether the caller named an EXACT target file (a full path). An exact
/// target overwrites — the GUI's save dialog already confirmed the replace,
/// and an MCP caller passing a full path means that path. A directory or
/// the default location dodges collisions instead ("name (1).ext").
#[derive(Clone)]
struct ResolvedDest {
    dir: PathBuf,
    name: String,
    exact: bool,
}

impl DestSpec {
    /// Resolve where the fetched file lands (see [`ResolvedDest`]). An
    /// explicit existing directory keeps the share name (collision-dodged);
    /// an explicit path names an exact file (overwrite); no explicit
    /// destination lands, collision-dodged, in the default directory.
    fn resolve(&self, share_name: &str) -> ResolvedDest {
        match &self.explicit {
            Some(dest) => {
                let p = molt_storage::expand_tilde(dest);
                if p.is_dir() {
                    ResolvedDest {
                        dir: p,
                        name: sanitize_file_name(share_name),
                        exact: false,
                    }
                } else {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| sanitize_file_name(share_name));
                    // a relative path with no parent resolves against the
                    // default directory, never the daemon's cwd
                    let dir = match p.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                        _ => molt_storage::expand_tilde(&self.default_dir),
                    };
                    ResolvedDest {
                        dir,
                        name,
                        exact: true,
                    }
                }
            }
            None => ResolvedDest {
                dir: molt_storage::expand_tilde(&self.default_dir),
                name: sanitize_file_name(share_name),
                exact: false,
            },
        }
    }
}

/// The final path a resolved destination writes to: the exact file when the
/// caller named one (overwrite), otherwise the first free collision variant.
/// Where a fetch lands: the resolved destination with its directory
/// created, and the `.part` path the bytes go into before the rename.
#[derive(Clone)]
struct Landing {
    resolved: ResolvedDest,
    part: PathBuf,
}

/// Resolve `dest` for `name`, create its directory and name the `.part`
/// file - the first half of every landing (queue plane, relay plane,
/// local copy).
fn prepare_landing(dest: &DestSpec, name: &str, id_hex: &str) -> Result<Landing, String> {
    let resolved = dest.resolve(name);
    std::fs::create_dir_all(&resolved.dir)
        .map_err(|e| format!("creating {}: {e}", resolved.dir.display()))?;
    sweep_stale_spills(&resolved.dir);
    let part = resolved.dir.join(format!(".molt-download-{id_hex}.part"));
    Ok(Landing { resolved, part })
}

/// Where a v2 fetch spills manifest-chunk candidates: beside its `.part`,
/// literally `<part>.mspill`.
fn spill_path_for(part: &Path) -> PathBuf {
    PathBuf::from(format!("{}.mspill", part.display()))
}

/// A spill a killed fetch left behind is garbage after a day (a live
/// fetch touches its spill as candidates arrive); nothing else in the
/// download dir is ours to touch.
const STALE_SPILL_AGE: Duration = Duration::from_secs(86_400);

fn sweep_stale_spills(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_spill = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".mspill"));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_SPILL_AGE);
        if is_spill && old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Move a fully written `.part` into place - the last half of every
/// landing. An explicit target overwrites (the caller named that exact
/// file; the GUI's save dialog already confirmed the replace); a
/// directory or the default location dodges collisions instead. A failed
/// rename removes the `.part`.
fn finish_landing(landing: &Landing) -> Result<PathBuf, String> {
    let final_path = final_path(&landing.resolved);
    std::fs::rename(&landing.part, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&landing.part);
        format!("moving into place: {e}")
    })?;
    Ok(final_path)
}

/// The last half of a landing that hashes what ACTUALLY landed: cut to
/// `truncate_to` when the series' verified size is known, durable, the
/// bytes must match `expected` (lowercase hex; "" = a legacy share with
/// no reference), the `.part` is removed on a mismatch, then the rename.
/// Blocking - call it off the runtime.
fn verify_and_finish(
    landing: &Landing,
    expected: &str,
    truncate_to: Option<u64>,
) -> Result<PathBuf, String> {
    let _ = std::fs::remove_file(spill_path_for(&landing.part));
    if let Some(size) = truncate_to {
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(&landing.part)
            .and_then(|f| f.set_len(size));
        if let Err(e) = cut {
            let _ = std::fs::remove_file(&landing.part);
            return Err(format!("cutting the landed file to its size: {e}"));
        }
    }
    if let Ok(f) = std::fs::File::open(&landing.part) {
        let _ = f.sync_all();
    }
    let verdict = hash_file(&landing.part)
        .map_err(|e| format!("reading the landed file failed: {e}"))
        .and_then(|(landed, _)| {
            if !expected.is_empty() && landed != expected.to_lowercase() {
                Err("the landed bytes do not match the share checksum".to_string())
            } else {
                Ok(())
            }
        });
    if let Err(e) = verdict {
        let _ = std::fs::remove_file(&landing.part);
        return Err(e);
    }
    finish_landing(landing)
}

fn final_path(resolved: &ResolvedDest) -> PathBuf {
    if resolved.exact {
        resolved.dir.join(&resolved.name)
    } else {
        resolve_collision(&resolved.dir, &resolved.name)
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
/// reply queue. `expected_size` is the share's recorded size — a file whose
/// size changed since the share is refused (honest error). Content change
/// at the SAME size is caught by the requester's sha256 verification against
/// the log-anchored checksum, so mtime is deliberately NOT a gate: it is
/// fragile (a backup/restore or `touch` resets it without changing a byte,
/// and an unreadable mtime would never round-trip). Returns `Err` only for
/// tracing; the requester learns of failures via `Refused` or its timeouts.
pub(crate) async fn run_file_serve<T: Transport>(
    transport: T,
    path: PathBuf,
    expected_size: u64,
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

    // honesty check against the recorded share: same size (a cheap stat;
    // content change is the requester's checksum job)
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return refuse(format!("the shared file is gone: {e}")).await,
    };
    if meta.len() != expected_size {
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
    // from here the ack queue exists — every exit must delete it, so the
    // manifest send + streaming run inside one block whose result we return
    // AFTER the cleanup (an early `?` before this would leak the queue)
    let serve = async {
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
        Ok::<(), String>(())
    }
    .await;

    // always retire the ack queue, whatever the serve outcome was
    let _ = transport.delete_queue(&ack_q.rcv).await;
    serve
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
    // from here the reply queue exists — every exit deletes it, so the
    // request-build/encrypt and the receive loop run inside one block whose
    // result we return AFTER cleanup (an early `?` would leak the queue)
    let result = async {
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
        fetch_frames(
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
        .await
    }
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

    let landing = prepare_landing(dest, &target.name, &target.id_hex)?;
    let part_path: &Path = &landing.part;
    let cleanup = |part: &Path| {
        let _ = std::fs::remove_file(part);
    };
    // the sharer's ack queue, captured once the manifest arrives — so a
    // mid-transfer failure can tell the sharer to STOP (else it blocks in
    // its ack-wait for the full timeout, holding a serve slot)
    let mut ack_target: Option<(SndQueueAddr, WrapKey)> = None;

    let result = async {
        let mut part = std::fs::File::create(part_path)
            .map_err(|e| format!("creating {}: {e}", part_path.display()))?;
        let mut reasm = molt_net::Reassembler::new();
        let mut hasher = Sha256::new();
        let mut manifest: Option<(u64, u32, String, SndQueueAddr, WrapKey)> = None;
        let mut pending: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut next_write: u32 = 0;
        let mut written: u64 = 0;
        let mut ack_seq: u64 = 0;
        let mut deadline = tokio::time::Instant::now() + timeouts.manifest;

        // the piece indices accepted THIS loop pass, to flow-ack after the
        // shared drain below (empty while the manifest — and with it the
        // ack queue — is still unknown)
        let mut newly: Vec<u32> = Vec::new();
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
                    "the sharer did not answer - retry when it is back online"
                        .to_string()
                } else {
                    "the transfer stalled - the sharer may have gone offline".to_string()
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
                    ack_target = Some((ack_snd.clone(), ack_wrap.clone()));
                    manifest = Some((size, pieces, sha256, ack_snd, ack_wrap));
                    // pieces that RACED AHEAD of this manifest sit parked in
                    // `pending` — drain + flow-ack them now (below)
                    newly = pending.keys().copied().collect();
                }
                Ok(TransferFrame::Piece { index, bytes }) => {
                    // the transport chunk is already acked (the only copy —
                    // it will never redeliver), so a piece arriving BEFORE
                    // its manifest must be PARKED, never dropped: delivery
                    // order is not guaranteed, and a dropped piece stalls
                    // the transfer until both sides time out. The bound is
                    // the LOG-ANCHORED size (the manifest must match it, or
                    // the transfer is refused anyway).
                    let pieces = match &manifest {
                        Some((_, pieces, _, _, _)) => *pieces,
                        None => pieces_for(target.size),
                    };
                    if index >= pieces || index < next_write || pending.contains_key(&index) {
                        continue; // out of range or duplicate
                    }
                    pending.insert(index, bytes);
                    if manifest.is_some() {
                        newly.push(index);
                    }
                }
                Ok(TransferFrame::Refused { id, reason }) => {
                    if id == target.id_hex {
                        return Err(reason);
                    }
                }
                Err(_) => continue,
            }
            // the shared drain: once the manifest (and with it the ack
            // queue) is known, write accepted pieces in order, hash as we
            // go, and flow-ack every NEWLY accepted index — for a piece
            // that arrived after the manifest exactly as before, for the
            // parked early ones the moment the manifest lands
            if newly.is_empty() {
                continue;
            }
            let Some((size, _, _, ack_snd, ack_wrap)) = &manifest else {
                continue;
            };
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
            for index in newly.drain(..) {
                ack_seq += 1;
                let ack_frame =
                    encode_ack(&TransferAck::Received { index }).map_err(|e| e.to_string())?;
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
                "checksum mismatch - the served bytes are not the shared file"
                    .to_string(),
            );
        }
        part.sync_all()
            .map_err(|e| format!("syncing {}: {e}", part_path.display()))?;
        drop(part);
        finish_landing(&landing)
    }
    .await;

    if result.is_err() {
        cleanup(part_path);
        // tell the sharer to stop streaming instead of blocking on acks that
        // will never come — best-effort, the sharer also has its own timeout
        if let Some((ack_snd, ack_wrap)) = &ack_target {
            if let Ok(bytes) = encode_ack(&TransferAck::Abort {
                reason: result.as_ref().err().cloned().unwrap_or_default(),
            }) {
                let _ = supervisor::send_framed(
                    transport,
                    ack_snd,
                    ack_wrap,
                    msg_id(&target.id_hex, "ack", u64::MAX),
                    &bytes,
                )
                .await;
            }
        }
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

/// What the share-hash pass learns about a file.
struct SharedStat {
    checksum: String,
    size: u64,
    modified: u64,
    key_b64: String,
    pieces: u32,
    root: String,
}

fn encode_key(key: &[u8; 32]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(key)
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
            move || -> std::io::Result<SharedStat> {
                let meta = std::fs::metadata(&path)?;
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // one streaming pass: the whole-file sha256 AND the piece
                // manifest of series v2 (`docs_archive/files/mirroring.md` §3.1),
                // plus the file's content key - drawn here, carried by the
                // share message, members-only through MLS
                let (manifest, checksum) =
                    molt_net::file_plane::manifest_of_reader(std::fs::File::open(&path)?)?;
                let mut key = [0u8; 32];
                getrandom::getrandom(&mut key).map_err(std::io::Error::other)?;
                Ok(SharedStat {
                    checksum,
                    size: manifest.size,
                    modified,
                    key_b64: encode_key(&key),
                    pieces: manifest.count,
                    root: manifest.root(),
                })
            }
        })
        .await;
        let cmd = match stat {
            Ok(Ok(st)) => Command::NetFileShared {
                kind: molt_core::file_kind_label(&name),
                name,
                size: st.size,
                modified: st.modified,
                checksum: st.checksum,
                path: path.display().to_string(),
                channel,
                generation: Some(scope),
                key_b64: st.key_b64,
                pieces: st.pieces,
                root: st.root,
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
    transport: molt_net::LoopbackTransport,
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
                // try_send, not a spawned task: progress is a throttled, lossy
                // signal — dropping one on a momentarily full queue is fine,
                // and try_send keeps reports ORDERED (spawned sends could
                // deliver a lower percent after a higher one)
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = progress_tx.try_send(Envelope {
                    cmd: Command::NetFileProgress {
                        id,
                        transferred,
                        total,
                        generation: Some(scope),
                    },
                    reply,
                });
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
    transport: molt_net::LoopbackTransport,
    path: PathBuf,
    expected_size: u64,
    share_id_hex: String,
    reply: ReplyHandover,
    slots: Arc<tokio::sync::Semaphore>,
) {
    tokio::spawn(async move {
        let Ok(_permit) = slots.acquire_owned().await else {
            return; // the engine is shutting down
        };
        if let Err(e) = run_file_serve(transport, path, expected_size, share_id_hex.clone(), reply)
            .await
        {
            tracing::warn!(share = %share_id_hex, error = %e, "serving a file download failed");
        }
    });
}

/// Send one honest `Refused` frame to a requester's reply queue (a serve
/// that cannot start: unknown share, unavailable, path lost).
pub(crate) fn spawn_send_refusal(
    transport: molt_net::LoopbackTransport,
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
/// How long a parked relay download waits for the sharer's `FileServed`
/// before its watchdog fails it (the operator can then retry).
const WANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// The parked download's watchdog: fires `NetFileWantedTimeout` after the
/// window — the actor ignores it when the park has drained (fetch running).
pub(crate) fn spawn_want_timeout(id: MessageId, scope: u64, cmd_tx: mpsc::Sender<Envelope>) {
    tokio::spawn(async move {
        tokio::time::sleep(WANT_TIMEOUT).await;
        feed(
            &cmd_tx,
            Command::NetFileWantedTimeout { id, generation: Some(scope) },
        )
        .await;
    });
}

/// Report a download verdict without any task work — the spawn-shaped
/// failure path for "the plane cannot even start" (no relay, no ring).
pub(crate) fn spawn_file_verdict(
    id: MessageId,
    result: Result<String, String>,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    tokio::spawn(async move {
        let cmd = match result {
            Ok(path) => Command::NetFileDone { id, path, generation: Some(scope) },
            Err(reason) => Command::NetFileFailed { id, reason, generation: Some(scope) },
        };
        feed(&cmd_tx, cmd).await;
    });
}

/// RELAY file plane: fetch a share's chunk series (`file_transfer_nostr.md`)
/// and land it in the download dir — same completion path as every other
/// download. The series verifies against the log-anchored checksum inside
/// `fetch_series`; the landed file is NOT re-hashed (the bytes went
/// straight from the verified buffer to disk).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_nostr_fetch(
    channel: molt_net::ritual_net::GroupChannel,
    ring: Vec<[u8; 32]>,
    id: MessageId,
    at: u64,
    target: FetchTarget,
    dest: DestSpec,
    cap: u64,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let result: Result<String, String> = async {
            let bytes = molt_net::file_plane::fetch_series(
                &channel,
                &ring,
                &target.id_hex,
                &target.checksum,
                at,
                cap,
                None,
            )
            .await
            .map_err(|e| format!("fetching the chunk series: {e}"))?;
            let id_hex = target.id_hex.clone();
            let name = target.name.clone();
            tokio::task::spawn_blocking(move || -> Result<String, String> {
                let landing = prepare_landing(&dest, &name, &id_hex)?;
                if let Err(e) = std::fs::write(&landing.part, &bytes) {
                    let _ = std::fs::remove_file(&landing.part);
                    return Err(format!("writing: {e}"));
                }
                // durable before the rename, like the queue-plane landing
                if let Ok(f) = std::fs::File::open(&landing.part) {
                    let _ = f.sync_all();
                }
                Ok(finish_landing(&landing)?.display().to_string())
            })
            .await
            .unwrap_or_else(|e| Err(format!("write task failed: {e}")))
        }
        .await;
        let cmd = match result {
            Ok(path) => Command::NetFileDone { id, path, generation: Some(scope) },
            Err(reason) => Command::NetFileFailed { id, reason, generation: Some(scope) },
        };
        feed(&cmd_tx, cmd).await;
    })
    .abort_handle()
}

/// RELAY file plane, sharer side: read the shared file and publish its
/// chunk series (lazy — a `FileWanted` triggered this). Metered on the
/// hour's shared publish budget (§5.4) — a spent budget holds the upload
/// with a warn naming it. Reports the stamp back as the internal
/// `NetFileSeriesPublished` (0 = failed); the ACTOR records the
/// group-visible `FileServed` announcement.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_series_publish(
    channel: molt_net::ritual_net::GroupChannel,
    exporter: [u8; 32],
    id: MessageId,
    path: PathBuf,
    cap: Option<u64>,
    store: crate::net::FileStateStore,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) {
    tokio::spawn(async move {
        let read = tokio::task::spawn_blocking(move || std::fs::read(&path)).await;
        let at = match read {
            Ok(Ok(bytes)) => {
                match molt_net::file_plane::publish_series_metered(
                    &channel,
                    &exporter,
                    &id.to_string(),
                    &bytes,
                    cap,
                    &store,
                    crate::now_secs(),
                )
                .await
                {
                    Ok((stamp, chunks)) => {
                        tracing::debug!(%id, chunks, "file series published");
                        stamp
                    }
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "file series publish failed");
                        0
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(%id, error = %e, "reading the shared file failed");
                0
            }
            Err(e) => {
                tracing::warn!(%id, error = %e, "the read task failed");
                0
            }
        };
        feed(
            &cmd_tx,
            Command::NetFileSeriesPublished { id, at, generation: Some(scope) },
        )
        .await;
    });
}

/// How long a resumable fetch waits after the relays went quiet before it
/// asks the holders for the missing pieces (`docs_archive/files/mirroring.md`
/// §3.2), in ms - a static so the integration tests can shorten it
/// (`crate::__set_piece_want_after`).
static PIECE_WANT_AFTER_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(600_000);

/// How often an incomplete fetch repeats its ask.
const PIECE_WANT_REPEAT: Duration = Duration::from_secs(30 * 60);

pub(crate) fn set_piece_want_after(d: Duration) {
    PIECE_WANT_AFTER_MS.store(
        u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::SeqCst,
    );
}

fn piece_want_after() -> Duration {
    Duration::from_millis(PIECE_WANT_AFTER_MS.load(std::sync::atomic::Ordering::SeqCst))
}

/// Verified pieces between two bitmap persists, and the longest a persist
/// may lag: a close or crash re-lands at most this much (the relay
/// replays it). Under the trickle's pace every piece persists.
const HELD_PERSIST_EVERY: u32 = 32;
const HELD_PERSIST_AFTER: Duration = Duration::from_secs(1);

/// Where a piece fetch's job persists - synchronous and fire-and-forget,
/// so the sink can save from inside the fetch (`transport.state` via the
/// storage writer, whose queue is FIFO with the clean-close merge).
pub(crate) trait FetchJobStore: Clone + Send + Sync + 'static {
    /// Upsert the job by series.
    fn save_job(&self, job: molt_core::FetchJob);
    /// The fetch of `series` ended.
    fn remove_job(&self, series: &str);
    /// Upsert a mirrored series' job (`docs_archive/files/mirroring.md` §3.3).
    fn save_mirror_job(&self, series: &str, job: molt_core::MirrorJob);
    /// The mirror of `series` is gone.
    fn remove_job_mirror(&self, series: &str);
}

/// The mirror folder's series directory: one sealed piece per index, the
/// manifest pieces included once the series is complete. The sink seals
/// each verified slice under the file's key (a fresh nonce; the folder
/// holds ciphertext only), keeps the job's bitmap and byte count, persists
/// them debounced and reports progress.
struct MirrorSink<S: FetchJobStore> {
    dir: PathBuf,
    key: [u8; 32],
    series: String,
    job: molt_core::MirrorJob,
    store: S,
    cmd_tx: mpsc::Sender<Envelope>,
    id: MessageId,
    scope: u64,
    since_persist: u32,
    last_persist: tokio::time::Instant,
}

impl<S: FetchJobStore> MirrorSink<S> {
    fn persist(&mut self) {
        self.since_persist = 0;
        self.last_persist = tokio::time::Instant::now();
        self.store.save_mirror_job(&self.series, self.job.clone());
    }

    /// Seal and store one piece (data, chunk or top record).
    fn store_piece(&self, index: u32, payload: &[u8]) -> std::io::Result<()> {
        let sealed = molt_net::file_plane::seal_piece(&self.key, index, self.job.count, payload)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tmp = self.dir.join(format!("{index}.tmp"));
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, self.dir.join(index.to_string()))
    }
}

impl<S: FetchJobStore> Drop for MirrorSink<S> {
    fn drop(&mut self) {
        if self.since_persist > 0 {
            self.store.save_mirror_job(&self.series, self.job.clone());
        }
    }
}

impl<S: FetchJobStore> molt_net::file_plane::PieceSink for MirrorSink<S> {
    fn put(&mut self, index: u32, payload: &[u8]) -> std::io::Result<()> {
        self.store_piece(index, payload)
    }

    fn spill_path(&self) -> Option<PathBuf> {
        Some(self.dir.join("manifest.mspill"))
    }

    fn held(&self) -> Vec<u32> {
        self.job.held_indices()
    }

    fn verified(&mut self, index: u32) {
        if self.job.holds(index) {
            return;
        }
        self.job.mark(index);
        let block = u64::try_from(molt_net::file_plane::PIECE_PAYLOAD_LEN).unwrap_or(0);
        let head = u64::from(index).saturating_mul(block);
        self.job.bytes = self
            .job
            .bytes
            .saturating_add(self.job.size.saturating_sub(head).min(block));
        self.since_persist += 1;
        if self.since_persist >= HELD_PERSIST_EVERY || self.last_persist.elapsed() >= HELD_PERSIST_AFTER {
            self.persist();
        }
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.try_send(Envelope {
            cmd: Command::NetMirrorProgress {
                id: self.id,
                held: self.job.held_count(),
                bytes: self.job.bytes,
                generation: Some(self.scope),
            },
            reply,
        });
    }
}

/// The mirror worker's fetch (`docs_archive/files/mirroring.md` §3.3): the
/// resumable job of §3.2 with the mirror folder as its sink. On
/// completion the manifest pieces are sealed into the folder too (a
/// re-seed publishes them like the data), the job is marked complete and
/// saved; a failure removes the folder and the job.
pub(crate) fn spawn_mirror_fetch<S: FetchJobStore>(
    channel: molt_net::ritual_net::GroupChannel,
    id: MessageId,
    dir: PathBuf,
    mut job: molt_core::MirrorJob,
    store: S,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let series = id.to_string();
        let result: Result<(), String> = async {
            let key = <[u8; 32]>::try_from(job.key.as_slice())
                .map_err(|_| "the share carries no usable key".to_string())?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("creating the mirror folder: {e}"))?;
            // a folder that lost its pieces starts over
            if !job.held.is_empty()
                && !job.held_indices().iter().all(|i| dir.join(i.to_string()).is_file())
            {
                job.held.clear();
                job.bytes = 0;
            }
            store.save_mirror_job(&series, job.clone());
            let expect = molt_net::file_plane::SeriesExpect {
                count: job.count,
                size: job.size,
                root: job.root.clone(),
            };
            let (count, started_at) = (job.count, job.started_at);
            let mut sink = MirrorSink {
                dir: dir.clone(),
                key,
                series: series.clone(),
                job,
                store: store.clone(),
                cmd_tx: cmd_tx.clone(),
                id,
                scope,
                since_persist: 0,
                last_persist: tokio::time::Instant::now(),
            };
            let started = tokio::time::Instant::now();
            let mut last_ask: Option<tokio::time::Instant> = None;
            let ask_tx = cmd_tx.clone();
            let mut on_quiet = move |missing: Vec<(u32, u32)>| -> bool {
                if missing.is_empty() {
                    return true;
                }
                let due = match last_ask {
                    None => started.elapsed() >= piece_want_after(),
                    Some(at) => at.elapsed() >= PIECE_WANT_REPEAT,
                };
                if due {
                    last_ask = Some(tokio::time::Instant::now());
                    let (reply, _rx) = tokio::sync::oneshot::channel();
                    let _ = ask_tx.try_send(Envelope {
                        cmd: Command::NetPieceWantSend { id, ranges: missing, generation: Some(scope) },
                        reply,
                    });
                }
                true
            };
            let opts = molt_net::file_plane::FetchOpts {
                quiet: molt_net::file_plane::FETCH_QUIET,
                ceiling: None,
                on_quiet: Some(&mut on_quiet),
            };
            let manifest =
                molt_net::file_plane::fetch_series_v2_with(&channel, &key, started_at, &expect, &mut sink, opts)
                    .await
                    .map_err(|e| format!("fetching the piece series: {e}"))?;
            // the manifest pieces, so a re-seed serves the whole series
            let layout = molt_net::file_plane::Manifest::layout_for(count)
                .ok_or_else(|| "the series exceeds the largest layout".to_string())?;
            for slot in 0..layout.chunks {
                sink.store_piece(count + slot, &manifest.chunk(slot))
                    .map_err(|e| format!("storing a manifest chunk: {e}"))?;
            }
            sink.store_piece(layout.top, &manifest.top_bytes())
                .map_err(|e| format!("storing the top record: {e}"))?;
            sink.job.complete = true;
            sink.persist();
            Ok(())
        }
        .await;
        let cmd = match result {
            Ok(()) => Command::NetMirrorDone { id, ok: true, reason: String::new(), generation: Some(scope) },
            Err(reason) => {
                let _ = std::fs::remove_dir_all(&dir);
                store.remove_job_mirror(&series);
                Command::NetMirrorDone { id, ok: false, reason, generation: Some(scope) }
            }
        };
        feed(&cmd_tx, cmd).await;
    })
    .abort_handle()
}

/// The `.part` file every slice lands in at its offset as it arrives - a
/// 1 GB fetch never sits in memory, and the file is never pre-sized: a
/// hostile size claim allocates nothing until pieces actually land. Keeps
/// the job's verified bitmap and persists it debounced (§3.2: a restart
/// resumes at the bitmap), and reports progress.
struct FileSink<S: FetchJobStore> {
    file: std::fs::File,
    /// Where manifest-chunk candidates spill while the top record is
    /// missing (`<part>.mspill`, [`spill_path_for`]; the fetch removes
    /// it on every exit, `prepare_landing` sweeps what a kill left).
    spill: PathBuf,
    job: molt_core::FetchJob,
    store: S,
    cmd_tx: mpsc::Sender<Envelope>,
    id: MessageId,
    scope: u64,
    held_count: u64,
    since_persist: u32,
    last_persist: tokio::time::Instant,
    last_pct: u64,
}

impl<S: FetchJobStore> FileSink<S> {
    /// Fresh: a new `.part`. Resumed: the existing `.part` and the job's
    /// bitmap - unless the `.part` is gone, then from scratch.
    fn open(
        part: &Path,
        mut job: molt_core::FetchJob,
        store: S,
        cmd_tx: mpsc::Sender<Envelope>,
        id: MessageId,
        scope: u64,
    ) -> std::io::Result<FileSink<S>> {
        let spill = spill_path_for(part);
        // a retry after a kill: the stale spill would otherwise keep its
        // size until the first push truncates it - if one ever comes
        let _ = std::fs::remove_file(&spill);
        let resume = !job.held.is_empty() && part.is_file();
        let file = if resume {
            std::fs::OpenOptions::new().write(true).open(part)?
        } else {
            job.held.clear();
            std::fs::File::create(part)?
        };
        let held_count = u64::try_from(job.held_indices().len()).unwrap_or(0);
        Ok(FileSink {
            file,
            spill,
            job,
            store,
            cmd_tx,
            id,
            scope,
            held_count,
            since_persist: 0,
            last_persist: tokio::time::Instant::now(),
            last_pct: 0,
        })
    }

    fn persist(&mut self) {
        self.since_persist = 0;
        self.last_persist = tokio::time::Instant::now();
        self.store.save_job(self.job.clone());
    }
}

impl<S: FetchJobStore> Drop for FileSink<S> {
    fn drop(&mut self) {
        if self.since_persist > 0 {
            self.store.save_job(self.job.clone());
        }
    }
}

impl<S: FetchJobStore> molt_net::file_plane::PieceSink for FileSink<S> {
    fn put(&mut self, index: u32, payload: &[u8]) -> std::io::Result<()> {
        use std::io::{Seek as _, Write as _};
        let block = u64::try_from(molt_net::file_plane::PIECE_PAYLOAD_LEN)
            .map_err(|_| std::io::Error::other("block size"))?;
        self.file.seek(std::io::SeekFrom::Start(u64::from(index).saturating_mul(block)))?;
        self.file.write_all(payload)
    }

    fn spill_path(&self) -> Option<PathBuf> {
        Some(self.spill.clone())
    }

    fn held(&self) -> Vec<u32> {
        self.job.held_indices()
    }

    fn verified(&mut self, index: u32) {
        if self.job.holds(index) {
            return;
        }
        self.job.mark(index);
        self.held_count += 1;
        self.since_persist += 1;
        if self.since_persist >= HELD_PERSIST_EVERY || self.last_persist.elapsed() >= HELD_PERSIST_AFTER {
            self.persist();
        }
        let pct = (self.held_count * 100).checked_div(u64::from(self.job.count)).unwrap_or(100);
        if pct > self.last_pct {
            self.last_pct = pct;
            let block = u64::try_from(molt_net::file_plane::PIECE_PAYLOAD_LEN).unwrap_or(0);
            let (reply, _rx) = tokio::sync::oneshot::channel();
            // lossy and ordered, like the queue plane's progress
            let _ = self.cmd_tx.try_send(Envelope {
                cmd: Command::NetFileProgress {
                    id: self.id,
                    transferred: self.held_count.saturating_mul(block).min(self.job.size),
                    total: self.job.size,
                    generation: Some(self.scope),
                },
                reply,
            });
        }
    }
}

/// RELAY file plane, requester side, series v2: the resumable job of
/// §3.2. The pieces land in the `.part` file as they arrive and verify
/// against the manifest; the verified bitmap persists with the job, so a
/// restart resumes it; the subscription stays open (day roll included)
/// with no total deadline; after the relays go quiet the job asks the
/// holders for what it misses (`PieceWanted`, once the wait passed, then
/// every half hour); the landed file must hash to the log-anchored
/// checksum, then the rename. The job ends on completion or on a failure
/// - both remove it.
pub(crate) fn spawn_nostr_fetch_v2<S: FetchJobStore>(
    channel: molt_net::ritual_net::GroupChannel,
    id: MessageId,
    job: molt_core::FetchJob,
    store: S,
    scope: u64,
    cmd_tx: mpsc::Sender<Envelope>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let series = job.series.clone();
        let result: Result<String, String> = async {
            let key = <[u8; 32]>::try_from(job.key.as_slice())
                .map_err(|_| "the share carries no usable key".to_string())?;
            let dest = DestSpec {
                explicit: job.dest.clone(),
                default_dir: job.default_dir.clone(),
            };
            let landing = prepare_landing(&dest, &job.name, &job.series)?;
            store.save_job(job.clone());
            let outcome: Result<String, String> = async {
                let mut sink = FileSink::open(&landing.part, job.clone(), store.clone(), cmd_tx.clone(), id, scope)
                    .map_err(|e| format!("creating the landing file: {e}"))?;
                let expect = molt_net::file_plane::SeriesExpect {
                    count: job.count,
                    size: job.size,
                    root: job.root.clone(),
                };
                let started = tokio::time::Instant::now();
                let mut last_ask: Option<tokio::time::Instant> = None;
                let ask_tx = cmd_tx.clone();
                let mut on_quiet = move |missing: Vec<(u32, u32)>| -> bool {
                    if missing.is_empty() {
                        return true;
                    }
                    let due = match last_ask {
                        None => started.elapsed() >= piece_want_after(),
                        Some(at) => at.elapsed() >= PIECE_WANT_REPEAT,
                    };
                    if due {
                        last_ask = Some(tokio::time::Instant::now());
                        let (reply, _rx) = tokio::sync::oneshot::channel();
                        let _ = ask_tx.try_send(Envelope {
                            cmd: Command::NetPieceWantSend { id, ranges: missing, generation: Some(scope) },
                            reply,
                        });
                    }
                    true
                };
                let opts = molt_net::file_plane::FetchOpts {
                    quiet: molt_net::file_plane::FETCH_QUIET,
                    ceiling: None,
                    on_quiet: Some(&mut on_quiet),
                };
                let manifest = molt_net::file_plane::fetch_series_v2_with(
                    &channel,
                    &key,
                    job.started_at,
                    &expect,
                    &mut sink,
                    opts,
                )
                .await
                .map_err(|e| format!("fetching the piece series: {e}"))?;
                drop(sink);
                let checksum = job.checksum.clone();
                let landing_for_task = landing.clone();
                // the size is the top record's, verified by root: whatever
                // an impostor slice may have grown past it is cut off
                let size = manifest.size;
                tokio::task::spawn_blocking(move || {
                    verify_and_finish(&landing_for_task, &checksum, Some(size))
                })
                .await
                .map_err(|e| format!("landing task failed: {e}"))?
                .map(|p| p.display().to_string())
            }
            .await;
            if outcome.is_err() {
                let _ = std::fs::remove_file(&landing.part);
                let _ = std::fs::remove_file(spill_path_for(&landing.part));
            }
            outcome
        }
        .await;
        store.remove_job(&series);
        let cmd = match result {
            Ok(path) => Command::NetFileDone { id, path, generation: Some(scope) },
            Err(reason) => Command::NetFileFailed { id, reason, generation: Some(scope) },
        };
        feed(&cmd_tx, cmd).await;
    })
    .abort_handle()
}

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
            let landing = prepare_landing(&dest, &target.name, &target.id_hex)?;
            let part: &Path = &landing.part;
            // copy first, then verify the bytes that ACTUALLY landed (the
            // source may be rewritten between the hash and the copy) — the
            // .part is removed on any failure, like the network path
            if let Err(e) = std::fs::copy(&source, part) {
                let _ = std::fs::remove_file(part);
                return Err(format!("copying: {e}"));
            }
            verify_and_finish(&landing, &target.checksum, None)
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

    /// A spill a killed fetch left beside its `.part` is gone the moment a
    /// retry opens the sink, and a day-old orphan is swept when any
    /// landing is prepared in that directory.
    #[test]
    fn stale_manifest_spills_are_removed_on_retry_and_swept_by_age() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dest = DestSpec {
            explicit: None,
            default_dir: tmp.path().display().to_string(),
        };
        let landing = prepare_landing(&dest, "big.bin", "aa").expect("landing");
        let spill = spill_path_for(&landing.part);
        assert!(spill.display().to_string().ends_with(".part.mspill"), "{}", spill.display());
        std::fs::write(&spill, b"stale candidates").expect("write spill");
        let _sink = test_sink(&landing.part);
        assert!(!spill.exists(), "a retry drops the stale spill");

        let orphan = tmp.path().join(".molt-download-bb.part.mspill");
        std::fs::write(&orphan, b"orphan").expect("write orphan");
        let old = std::time::SystemTime::now() - Duration::from_secs(2 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .and_then(|f| f.set_modified(old))
            .expect("age the orphan");
        let fresh = tmp.path().join(".molt-download-cc.part.mspill");
        std::fs::write(&fresh, b"live").expect("write fresh");
        let _ = prepare_landing(&dest, "other.bin", "dd").expect("landing");
        assert!(!orphan.exists(), "a day-old spill is swept");
        assert!(fresh.exists(), "a fresh spill belongs to a live fetch");
    }

    /// The landing file is never pre-sized: a size claim allocates nothing
    /// until a piece lands, and a piece lands at its own offset.
    #[test]
    fn the_landing_file_is_never_pre_sized() {
        use molt_net::file_plane::{PieceSink as _, PIECE_PAYLOAD_LEN};
        let tmp = tempfile::tempdir().expect("tmp");
        let part = tmp.path().join("claim.part");
        let mut sink = test_sink(&part);
        assert_eq!(std::fs::metadata(&part).expect("meta").len(), 0, "nothing landed, nothing sized");
        sink.put(2, b"abc").expect("put");
        let want = u64::try_from(2 * PIECE_PAYLOAD_LEN + 3).expect("fits");
        assert_eq!(std::fs::metadata(&part).expect("meta").len(), want);
    }

    /// A store that keeps nothing - the unit tests look at the `.part`.
    #[derive(Clone)]
    struct NoStore;

    impl FetchJobStore for NoStore {
        fn save_job(&self, _job: molt_core::FetchJob) {}
        fn remove_job(&self, _series: &str) {}
        fn save_mirror_job(&self, _series: &str, _job: molt_core::MirrorJob) {}
        fn remove_job_mirror(&self, _series: &str) {}
    }

    /// A sink over a fresh `.part` with an empty job.
    fn test_sink(part: &Path) -> FileSink<NoStore> {
        let (cmd_tx, _rx) = mpsc::channel(4);
        let job = molt_core::FetchJob {
            series: "aa".into(),
            key: vec![0; 32],
            count: 3,
            size: 3,
            root: String::new(),
            checksum: String::new(),
            name: "x".into(),
            dest: None,
            default_dir: String::new(),
            started_at: 0,
            held: Vec::new(),
        };
        FileSink::open(part, job, NoStore, cmd_tx, MessageId([1; 16]), 0).expect("sink")
    }

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
                    req.id,
                    req.reply,
                ));
                true
            },
            |_, _| {},
        )
        .await
    }

    /// A piece that RACES AHEAD of its manifest must be parked, not lost:
    /// the requester acks the transport chunk before decoding (the only
    /// copy — no redelivery), so a dropped early piece stalls the whole
    /// transfer until both sides time out. Delivery order is not
    /// guaranteed (independent delivery tasks — found as a load-dependent
    /// whole-suite flake at --test-threads=32), so the requester must
    /// tolerate the swap. The hook plays a sharer that sends piece 0
    /// FIRST and the manifest after a beat.
    #[test]
    fn a_piece_racing_ahead_of_its_manifest_is_parked_not_lost() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let bytes = content(8 * 1024);
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            let (sharer, requester) = mls_pair();
            let hub = LoopbackHub::calm();
            let transport = hub.transport();
            let share_id = "ab".repeat(16);
            let sha = sha_hex(&bytes);
            let size = u64::try_from(bytes.len()).expect("len fits u64");
            let announce_transport = transport.clone();
            let announce_bytes = bytes.clone();
            let announce_share = share_id.clone();
            let announce_sha = sha.clone();
            let got = run_file_fetch(
                transport,
                requester,
                FetchTarget {
                    id_hex: share_id.clone(),
                    name: "doc.pdf".to_string(),
                    size,
                    checksum: sha,
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
                },
                DestSpec {
                    explicit: Some(dest.display().to_string()),
                    default_dir: "~/Downloads".to_string(),
                },
                FetchTimeouts {
                    manifest: Duration::from_secs(10),
                    idle: Duration::from_secs(3),
                },
                move |ct: String| async move {
                    let raw = hex::decode(&ct).expect("ct hex");
                    let (_, plain) = match sharer
                        .lock()
                        .expect("lock")
                        .decrypt(&raw)
                        .expect("group decrypt")
                    {
                        molt_net::MlsIncoming::Application { from, plaintext } => {
                            (from, plaintext)
                        }
                        other => panic!("unexpected mls message: {other:?}"),
                    };
                    let req: FetchRequest =
                        serde_json::from_slice(&plain).expect("request json");
                    let (reply_snd, reply_wrap) =
                        parse_handover(&req.reply).expect("handover");
                    let t = announce_transport;
                    tokio::spawn(async move {
                        // the racy order: piece 0 first…
                        let piece = encode_frame(&TransferFrame::Piece {
                            index: 0,
                            bytes: announce_bytes.clone(),
                        })
                        .expect("piece frame");
                        supervisor::send_framed(
                            &t,
                            &reply_snd,
                            &reply_wrap,
                            msg_id(&announce_share, "fetch", 1),
                            &piece,
                        )
                        .await
                        .expect("send piece");
                        // …give it time to land (and be processed) before
                        // the manifest follows
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let ack_q = t.create_queue().await.expect("ack queue");
                        let ack_wrap = WrapKey::fresh().expect("wrap");
                        let manifest = encode_frame(&TransferFrame::Manifest {
                            id: announce_share.clone(),
                            size,
                            pieces: pieces_for(size),
                            sha256: announce_sha,
                            ack: ReplyHandover {
                                server: ack_q.snd.server.clone(),
                                queue_id: hex::encode(&ack_q.snd.id.0),
                                wrap: hex::encode(ack_wrap.to_bytes()),
                            },
                        })
                        .expect("manifest frame");
                        supervisor::send_framed(
                            &t,
                            &reply_snd,
                            &reply_wrap,
                            msg_id(&announce_share, "fetch", 0),
                            &manifest,
                        )
                        .await
                        .expect("send manifest");
                    });
                    true
                },
                |_, _| {},
            )
            .await
            .expect("the early piece is parked and the transfer completes");
            assert_eq!(std::fs::read(&got).expect("read"), bytes);
        });
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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
                    key_b64: String::new(),
                    pieces: 0,
                    root: String::new(),
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

