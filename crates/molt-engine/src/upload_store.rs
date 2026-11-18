// SPDX-License-Identifier: GPL-3.0-or-later
//! The upload store (`docs_archive/security/mcp-security.md`, "The byte
//! doors"): where a frontend that holds BYTES and no host path - the
//! browser window - lands a file before it is shared or proposed.
//!
//! One directory per upload under `<workspace>/uploads/<id>/`, the file
//! inside under its own name (a share is named after its source file).
//! The store is durable state of the workspace: a shared upload lives as
//! long as its share and leaves with it. What it is NOT is a second path
//! door - no command names a path here; an upload is addressed by a
//! random id the engine minted, and its name is one path component.
//!
//! The actor keeps the bookkeeping (order, size, quota); one blocking
//! writer per upload does the disk work in the order the actor accepted
//! the chunks and answers each chunk itself.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use molt_core::wire::{CHUNK, MAX_TRANSFER};
use molt_core::{ChannelRef, MessageId, MoltError, Reply};
use tokio::sync::{mpsc, oneshot};

use crate::State;

/// Bytes one workspace's store may hold: open uploads at their declared
/// size plus every shared upload.
pub(crate) const QUOTA: u64 = 1024 * 1024 * 1024;
/// Uploads that may be open (begun, not yet shared) at once.
pub(crate) const OPEN_MAX: usize = 8;
/// An upload nobody touched for this long is abandoned.
pub(crate) const IDLE_SECS: u64 = 900;
/// The sweep's beat on the presence tick.
const SWEEP_EVERY_SECS: u64 = 600;
/// Chunks the writer may have queued; a client awaits each chunk's
/// answer, so more than a few is a client that does not.
const QUEUE: usize = 4;
const DIR: &str = "uploads";
const PART: &str = "part";

type ReplySlot = oneshot::Sender<Result<Reply, MoltError>>;

/// One accepted chunk on its way to disk.
struct Job {
    bytes: Vec<u8>,
    last: bool,
    reply: ReplySlot,
}

/// What the writer and the actor agree on about the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The writer still owns it.
    Writing,
    /// The file is whole under its name; the actor owns it.
    Landed,
    /// Nobody wants it: whoever holds it last removes it.
    Dropped,
}

enum Stage {
    /// Chunks are still due.
    Receiving(mpsc::Sender<Job>),
    /// Every byte was accepted.
    Whole,
    /// Handed to a share that has not posted yet.
    Sharing(MessageId),
}

struct Entry {
    dir: PathBuf,
    name: String,
    size: u64,
    received: u64,
    touched: u64,
    stage: Stage,
    fate: Arc<Mutex<Fate>>,
}

impl Entry {
    fn fate(&self) -> Fate {
        self.fate.lock().map(|f| *f).unwrap_or(Fate::Dropped)
    }

    /// Nobody wants this upload any more. Runs on the actor: one unlink
    /// of a one-file directory, like the logo reconcile.
    fn discard(self) {
        let Ok(mut fate) = self.fate.lock() else { return };
        if *fate == Fate::Landed {
            prune(&self.dir);
        }
        // a writer still at work removes it when it sees this
        *fate = Fate::Dropped;
    }
}

/// The open uploads of the open workspace.
#[derive(Default)]
pub(crate) struct Uploads {
    entries: BTreeMap<String, Entry>,
    swept_at: u64,
}

fn refused(why: &str) -> MoltError {
    MoltError::BadPayload(format!("upload: {why}"))
}

fn unknown() -> MoltError {
    refused("unknown id")
}

/// One path component a file can be named: no separator, no `.`/`..`,
/// no control character.
fn name_ok(name: &str) -> bool {
    molt_core::is_bare_name(name) && !name.chars().any(char::is_control) && name == name.trim()
}

impl Uploads {
    /// Bytes the open uploads claim.
    fn claimed(&self) -> u64 {
        self.entries.values().map(|e| e.size).sum()
    }

    /// Open a new upload under `store`. `shared` is what the store's
    /// shared uploads already hold.
    pub(crate) fn begin(
        &mut self,
        store: &Path,
        id: String,
        name: &str,
        size: u64,
        shared: u64,
        now: u64,
    ) -> Result<Reply, MoltError> {
        if !name_ok(name) {
            return Err(refused("the name must be a plain file name"));
        }
        if size > MAX_TRANSFER {
            return Err(refused(&format!("{size} bytes over the {MAX_TRANSFER} limit")));
        }
        if self.entries.len() >= OPEN_MAX {
            return Err(refused(&format!("{OPEN_MAX} uploads already open")));
        }
        if self.claimed().saturating_add(shared).saturating_add(size) > QUOTA {
            return Err(refused(&format!("the store is full ({QUOTA} bytes)")));
        }
        let dir = store.join(&id);
        let fate = Arc::new(Mutex::new(Fate::Writing));
        let (tx, rx) = mpsc::channel(QUEUE);
        let writer = Writer { dir: dir.clone(), name: name.to_string(), id: id.clone(), size, fate: fate.clone() };
        tokio::task::spawn_blocking(move || writer.run(rx));
        self.entries.insert(
            id.clone(),
            Entry { dir, name: name.to_string(), size, received: 0, touched: now, stage: Stage::Receiving(tx), fate },
        );
        Ok(Reply::UploadProgress { upload: id, received: 0, size, done: false })
    }

    /// Accept the next chunk and hand it to the writer, which answers
    /// through `reply`. A refusal leaves `reply` in place for the actor.
    pub(crate) fn chunk(
        &mut self,
        upload: &str,
        offset: u64,
        bytes: Vec<u8>,
        last: bool,
        now: u64,
        reply: &mut Option<ReplySlot>,
    ) -> Result<Reply, MoltError> {
        let entry = self.entries.get_mut(upload).ok_or_else(unknown)?;
        let Stage::Receiving(jobs) = &entry.stage else {
            return Err(refused("already whole"));
        };
        if bytes.len() > CHUNK {
            return Err(refused(&format!("a chunk is {CHUNK} bytes at most")));
        }
        if offset != entry.received {
            return Err(refused(&format!("offset {offset}, expected {}", entry.received)));
        }
        let end = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if end > entry.size {
            return Err(refused("more bytes than declared"));
        }
        if last != (end == entry.size) {
            return Err(refused("the last chunk ends at the declared size"));
        }
        let Some(slot) = reply.take() else {
            return Err(MoltError::Engine("upload: no reply channel".to_string()));
        };
        match jobs.try_send(Job { bytes, last, reply: slot }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(job)) => {
                *reply = Some(job.reply);
                return Err(refused("busy - await each chunk"));
            }
            Err(mpsc::error::TrySendError::Closed(job)) => {
                *reply = Some(job.reply);
                return Err(MoltError::Engine("upload: the writer is gone".to_string()));
            }
        }
        entry.received = end;
        entry.touched = now;
        if last {
            // dropping the sender lets the writer finish and leave
            entry.stage = Stage::Whole;
        }
        Ok(Reply::Ack)
    }

    /// Drop an upload that was not shared.
    pub(crate) fn abort(&mut self, upload: &str) -> Result<Reply, MoltError> {
        match self.entries.get(upload).map(|e| &e.stage) {
            None => Err(unknown()),
            Some(Stage::Sharing(_)) => Err(refused("already shared")),
            Some(_) => {
                if let Some(entry) = self.entries.remove(upload) {
                    entry.discard();
                }
                Ok(Reply::Ack)
            }
        }
    }

    /// The file of a whole upload that is on disk.
    pub(crate) fn whole_path(&self, upload: &str) -> Result<PathBuf, MoltError> {
        let entry = self.entries.get(upload).ok_or_else(unknown)?;
        match entry.stage {
            Stage::Whole if entry.fate() == Fate::Landed => Ok(entry.dir.join(&entry.name)),
            Stage::Sharing(_) => Err(refused("already shared")),
            _ => Err(refused("not whole yet")),
        }
    }

    /// The upload became the source of share `id`, whose message has not
    /// posted yet.
    pub(crate) fn mark_sharing(&mut self, upload: &str, id: MessageId, now: u64) {
        if let Some(entry) = self.entries.get_mut(upload) {
            entry.stage = Stage::Sharing(id);
            // the idle window restarts: the hash reads the file now
            entry.touched = now;
        }
    }

    /// Share `id` never posted (its hash failed): the upload behind it is
    /// dropped - its client gave it up when the share was accepted.
    pub(crate) fn share_failed(&mut self, id: &MessageId) {
        let of: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| matches!(&e.stage, Stage::Sharing(s) if s == id))
            .map(|(upload, _)| upload.clone())
            .collect();
        for upload in of {
            if let Some(entry) = self.entries.remove(&upload) {
                entry.discard();
            }
        }
    }

    /// Share `id` posted: its file is the share's now, not an open upload.
    pub(crate) fn shared(&mut self, id: &MessageId) {
        self.entries.retain(|_, e| !matches!(&e.stage, Stage::Sharing(of) if of == id));
    }

    /// Take a whole upload out of the store's books: the caller reads
    /// the file and removes the returned directory.
    pub(crate) fn take_whole(&mut self, upload: &str) -> Result<(PathBuf, PathBuf), MoltError> {
        let file = self.whole_path(upload)?;
        let entry = self.entries.remove(upload).ok_or_else(unknown)?;
        Ok((file, entry.dir))
    }

    /// Drop what nobody touched for [`IDLE_SECS`]: an upload whose client
    /// went away, or a share whose hash never reported back.
    pub(crate) fn sweep(&mut self, now: u64) {
        let idle: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.touched) > IDLE_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            if let Some(entry) = self.entries.remove(&id) {
                tracing::info!(upload = %id, "upload abandoned");
                entry.discard();
            }
        }
    }

    /// The workspace closes: nothing open survives it.
    pub(crate) fn clear(&mut self) {
        for (_, entry) in std::mem::take(&mut self.entries) {
            entry.discard();
        }
        self.swept_at = 0;
    }

    /// The directories the open uploads own.
    fn dirs(&self) -> impl Iterator<Item = &Path> {
        self.entries.values().map(|e| e.dir.as_path())
    }
}

/// The disk half of one upload.
struct Writer {
    dir: PathBuf,
    name: String,
    id: String,
    size: u64,
    fate: Arc<Mutex<Fate>>,
}

impl Writer {
    /// Write the chunks in the order the actor accepted them; the last
    /// one is synced and renamed to the file's name. A queue that closes
    /// before that is an abandoned upload: the directory goes.
    fn run(self, mut jobs: mpsc::Receiver<Job>) {
        let mut file: Option<std::fs::File> = None;
        let mut failed: Option<String> = None;
        let mut written = 0u64;
        let mut landed = false;
        while let Some(job) = jobs.blocking_recv() {
            let outcome = match &failed {
                Some(why) => Err(std::io::Error::other(why.clone())),
                None => self.write(&mut file, &job),
            };
            let answer = match outcome {
                Ok(done) => {
                    written = written.saturating_add(u64::try_from(job.bytes.len()).unwrap_or(0));
                    landed = done;
                    Ok(Reply::UploadProgress { upload: self.id.clone(), received: written, size: self.size, done })
                }
                Err(e) => {
                    tracing::warn!(upload = %self.id, error = %e, "upload write failed");
                    failed = Some(e.to_string());
                    Err(MoltError::Engine(format!("upload: {e}")))
                }
            };
            let _ = job.reply.send(answer);
        }
        if !landed {
            prune(&self.dir);
        }
    }

    /// One chunk; `true` once the file is whole under its name.
    fn write(&self, file: &mut Option<std::fs::File>, job: &Job) -> std::io::Result<bool> {
        if file.is_none() {
            make_dir(&self.dir)?;
            // create_new: a directory this run did not make is not written into
            *file = Some(std::fs::File::options().write(true).create_new(true).open(self.dir.join(PART))?);
        }
        let Some(f) = file.as_mut() else {
            return Err(std::io::Error::other("no file"));
        };
        f.write_all(&job.bytes)?;
        if !job.last {
            return Ok(false);
        }
        f.sync_all()?;
        let mut fate = self.fate.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        if *fate == Fate::Dropped {
            return Err(std::io::Error::other("dropped"));
        }
        std::fs::rename(self.dir.join(PART), self.dir.join(&self.name))?;
        *fate = Fate::Landed;
        Ok(true)
    }
}

/// `<workspace>/uploads`.
pub(crate) fn store_dir(workspace: &Path) -> PathBuf {
    workspace.join(DIR)
}

/// The upload directory `path` lies in, if it is a file of `store`:
/// exactly `<store>/<id>/<name>`, the id in the spelling the engine mints.
/// Decided on the components, so a `..` is never read away - a path that
/// climbs is not the store's, whatever it resolves to.
pub(crate) fn stored_dir(store: &Path, path: &Path) -> Option<PathBuf> {
    use std::path::Component::Normal;
    let mut inside = path.strip_prefix(store).ok()?.components();
    let (Some(Normal(dir)), Some(Normal(_)), None) = (inside.next(), inside.next(), inside.next()) else {
        return None;
    };
    let dir = dir.to_str()?;
    let minted = dir.parse::<MessageId>().is_ok_and(|id| id.to_string() == dir);
    minted.then(|| store.join(dir))
}

/// Serializes making an upload's directory against removing the empty
/// store, across the writers and the actor.
static STORE_DIRS: Mutex<()> = Mutex::new(());

fn store_dirs() -> std::sync::MutexGuard<'static, ()> {
    STORE_DIRS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An upload's directory, store included.
fn make_dir(dir: &Path) -> std::io::Result<()> {
    let _held = store_dirs();
    std::fs::create_dir_all(dir)
}

/// Remove one upload's directory, and the store itself once it is empty:
/// an export names every directory it leaves out, and an empty store is
/// nothing to name.
pub(crate) fn prune(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    if let Some(store) = dir.parent() {
        remove_empty(store);
    }
}

/// Remove `store` if nothing is in it.
fn remove_empty(store: &Path) {
    let _held = store_dirs();
    // refuses a directory that still holds something
    let _ = std::fs::remove_dir(store);
}

/// Remove every directory of `store` that is not in `keep`: what a crash,
/// a close or an expired share left behind.
fn remove_orphans(store: &Path, keep: &HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(store) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep.contains(&path) {
            continue;
        }
        tracing::info!(dir = %entry.file_name().to_string_lossy(), "upload store: orphan removed");
        let gone = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if let Err(e) = gone {
            tracing::warn!(error = %e, "upload store: orphan not removed");
        }
    }
    remove_empty(store);
}

impl State {
    /// The open workspace's store; uploads need a workspace on disk.
    pub(crate) fn upload_store(&self) -> Result<PathBuf, MoltError> {
        self.active
            .as_ref()
            .map(|a| store_dir(&a.dir))
            .ok_or_else(|| refused("no stored workspace is open"))
    }

    /// This seat's shares whose source file lies in `store`: the running
    /// path map and the persisted one (`prefs.shared_files`). The second
    /// matters: a share whose path was not adopted at open is still this
    /// node's record of a file it keeps.
    fn stored_shares(&self, store: &Path) -> BTreeMap<String, PathBuf> {
        let persisted = self
            .active
            .iter()
            .flat_map(|a| a.prefs.shared_files.iter())
            .map(|(id, path)| (id.clone(), PathBuf::from(path)));
        let running = self.files.share_paths.iter().map(|(id, path)| (id.to_string(), path.clone()));
        persisted.chain(running).filter(|(_, path)| stored_dir(store, path).is_some()).collect()
    }

    /// Bytes the store's shared uploads hold, from the shares themselves.
    fn stored_share_bytes(&self, store: &Path) -> u64 {
        self.stored_shares(store)
            .keys()
            .filter_map(|id| id.parse::<MessageId>().ok())
            .map(|id| self.share_identity(&id).map(|(ident, _)| ident.size).unwrap_or(0))
            .sum()
    }

    /// [`molt_core::Command::UploadBegin`].
    pub(crate) fn cmd_upload_begin(&mut self, name: String, size: u64) -> Result<Reply, MoltError> {
        let store = self.upload_store()?;
        let shared = self.stored_share_bytes(&store);
        let id = crate::chat::mint_message_id()?.to_string();
        let now = self.presence_now();
        self.uploads.begin(&store, id, &name, size, shared, now)
    }

    /// [`molt_core::Command::UploadChunk`]: checked here, written and
    /// answered by the upload's writer.
    pub(crate) fn cmd_upload_chunk(
        &mut self,
        upload: String,
        offset: u64,
        bytes: Vec<u8>,
        last: bool,
    ) -> Result<Reply, MoltError> {
        let now = self.presence_now();
        let mut slot = self.deferred_reply.take();
        let out = self.uploads.chunk(&upload, offset, bytes, last, now, &mut slot);
        if slot.is_some() {
            self.deferred_reply = slot;
        }
        out
    }

    /// [`molt_core::Command::UploadAbort`].
    pub(crate) fn cmd_upload_abort(&mut self, upload: String) -> Result<Reply, MoltError> {
        self.uploads.abort(&upload)
    }

    /// [`molt_core::Command::ShareUpload`]: `share_file` with the store's
    /// file as the source.
    pub(crate) fn cmd_share_upload(&mut self, upload: String, channel: ChannelRef) -> Result<Reply, MoltError> {
        let path = self.uploads.whole_path(&upload)?;
        let reply = self.share_source(path, channel)?;
        if let Reply::Sent { id } = &reply {
            let now = self.presence_now();
            self.uploads.mark_sharing(&upload, *id, now);
        }
        Ok(reply)
    }

    /// A share's source path is forgotten: a stored one leaves the disk
    /// with it. On the actor - one unlink of a one-file directory.
    pub(crate) fn drop_stored_share(&self, path: &Path) {
        let Ok(store) = self.upload_store() else { return };
        if let Some(dir) = stored_dir(&store, path) {
            prune(&dir);
        }
    }

    /// Forget the stored shares that expired (the path map keeps a share
    /// until compaction; its bytes need not wait for that).
    pub(crate) fn forget_expired_stored_shares(&mut self, store: &Path) {
        let states = self.files_state();
        let expired: Vec<MessageId> = self
            .files
            .share_paths
            .iter()
            .filter(|(id, p)| stored_dir(store, p).is_some() && self.share_expired_in(&states, id))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let Some(path) = self.files.share_paths.get(&id).cloned() else { continue };
            self.forget_share_path(&id);
            // also where no workspace is open to name the store by
            if let Some(dir) = stored_dir(store, &path) {
                prune(&dir);
            }
        }
    }

    /// Make the store hold exactly what something still names: the open
    /// uploads and the live shares. Run at open (a crash's leftovers) and
    /// on the sweep beat (abandoned uploads, expired shares).
    pub(crate) fn sweep_upload_store(&mut self) {
        let Ok(store) = self.upload_store() else { return };
        self.uploads.sweep(self.presence_now());
        self.forget_expired_stored_shares(&store);
        let keep: HashSet<PathBuf> = self
            .uploads
            .dirs()
            .map(Path::to_path_buf)
            .chain(self.stored_shares(&store).values().filter_map(|p| stored_dir(&store, p)))
            .collect();
        remove_orphans(&store, &keep);
    }

    /// The sweep beat, riding the presence tick.
    pub(crate) fn upload_store_tick(&mut self) {
        let now = self.presence_now();
        if now.saturating_sub(self.uploads.swept_at) < SWEEP_EVERY_SECS {
            return;
        }
        self.uploads.swept_at = now;
        self.sweep_upload_store();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::rt;

    fn slot() -> (Option<ReplySlot>, oneshot::Receiver<Result<Reply, MoltError>>) {
        let (tx, rx) = oneshot::channel();
        (Some(tx), rx)
    }

    async fn gone(path: &Path) {
        for _ in 0..500 {
            if !path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} is still there", path.display());
    }

    fn begin(u: &mut Uploads, store: &Path, id: &str, size: u64, now: u64) {
        u.begin(store, id.to_string(), "a.bin", size, 0, now).expect("begin");
    }

    #[test]
    fn a_name_that_is_not_one_plain_component_is_refused() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            for bad in ["", ".", "..", "../a", "a/b", "/etc/passwd", "a\\b", "a\0b", "a\nb", " a", "a "] {
                let err = u.begin(tmp.path(), "id".into(), bad, 1, 0, 0).expect_err(bad);
                assert!(err.to_string().contains("plain file name"), "{bad:?}: {err}");
            }
            assert!(u.entries.is_empty());
            assert_eq!(std::fs::read_dir(tmp.path()).expect("dir").count(), 0, "nothing touched the disk");
        });
    }

    #[test]
    fn size_count_and_quota_are_refused_at_begin() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            let err = u.begin(tmp.path(), "x".into(), "a", MAX_TRANSFER + 1, 0, 0).expect_err("oversize");
            assert!(err.to_string().contains("limit"), "{err}");
            // the shared uploads count against the quota
            let err = u.begin(tmp.path(), "x".into(), "a", 11, QUOTA - 10, 0).expect_err("quota");
            assert!(err.to_string().contains("full"), "{err}");
            u.begin(tmp.path(), "fits".into(), "a", 10, QUOTA - 10, 0).expect("exactly the quota");
            // ...and so do the open ones, at their declared size
            let err = u.begin(tmp.path(), "x".into(), "a", 1, QUOTA - 10, 0).expect_err("quota");
            assert!(err.to_string().contains("full"), "{err}");
            u.clear();
            for i in 0..OPEN_MAX {
                begin(&mut u, tmp.path(), &format!("u{i}"), 1, 0);
            }
            let err = u.begin(tmp.path(), "x".into(), "a", 1, 0, 0).expect_err("too many");
            assert!(err.to_string().contains("already open"), "{err}");
        });
    }

    #[test]
    fn chunks_are_taken_in_order_only_and_within_the_declared_size() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            begin(&mut u, tmp.path(), "u", 6, 0);
            let refuse = |u: &mut Uploads, id: &str, offset: u64, bytes: &[u8], last: bool| {
                let (mut s, _rx) = slot();
                let err = u.chunk(id, offset, bytes.to_vec(), last, 0, &mut s).expect_err("refused");
                assert!(s.is_some(), "a refusal leaves the reply to the actor");
                err.to_string()
            };
            assert!(refuse(&mut u, "nope", 0, b"abc", false).contains("unknown id"));
            assert!(refuse(&mut u, "u", 3, b"abc", true).contains("expected 0"), "a gap");
            assert!(refuse(&mut u, "u", 0, b"abcdefg", true).contains("more bytes"));
            assert!(refuse(&mut u, "u", 0, b"abc", true).contains("last chunk"), "last before the end");
            assert!(refuse(&mut u, "u", 0, b"abcdef", false).contains("last chunk"), "the end without last");
            assert!(refuse(&mut u, "u", 0, &vec![0u8; CHUNK + 1], false).contains("at most"));

            let (mut s, rx) = slot();
            u.chunk("u", 0, b"abc".to_vec(), false, 0, &mut s).expect("first");
            assert!(s.is_none(), "the writer answers");
            assert!(matches!(rx.await, Ok(Ok(Reply::UploadProgress { received: 3, size: 6, done: false, .. }))));
            assert!(refuse(&mut u, "u", 0, b"abc", false).contains("expected 3"), "a repeat");
            assert!(u.whole_path("u").expect_err("not whole").to_string().contains("not whole"));

            let (mut s, rx) = slot();
            u.chunk("u", 3, b"def".to_vec(), true, 0, &mut s).expect("last");
            assert!(matches!(rx.await, Ok(Ok(Reply::UploadProgress { received: 6, done: true, .. }))));
            let file = u.whole_path("u").expect("whole");
            assert_eq!(file, tmp.path().join("u").join("a.bin"));
            assert_eq!(std::fs::read(&file).expect("read"), b"abcdef");
            assert!(!tmp.path().join("u").join(PART).exists());
            assert!(refuse(&mut u, "u", 6, b"", true).contains("already whole"));
        });
    }

    #[test]
    fn an_empty_file_is_one_empty_last_chunk() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            begin(&mut u, tmp.path(), "u", 0, 0);
            let (mut s, rx) = slot();
            u.chunk("u", 0, Vec::new(), true, 0, &mut s).expect("last");
            assert!(matches!(rx.await, Ok(Ok(Reply::UploadProgress { received: 0, done: true, .. }))));
            assert_eq!(std::fs::read(u.whole_path("u").expect("whole")).expect("read"), b"");
        });
    }

    #[test]
    fn an_abort_removes_the_bytes_in_every_stage() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            // half received
            begin(&mut u, tmp.path(), "half", 6, 0);
            let (mut s, rx) = slot();
            u.chunk("half", 0, b"abc".to_vec(), false, 0, &mut s).expect("chunk");
            rx.await.expect("answered").expect("written");
            assert!(tmp.path().join("half").join(PART).exists());
            u.abort("half").expect("abort");
            gone(&tmp.path().join("half")).await;
            // whole
            begin(&mut u, tmp.path(), "whole", 1, 0);
            let (mut s, rx) = slot();
            u.chunk("whole", 0, b"x".to_vec(), true, 0, &mut s).expect("chunk");
            rx.await.expect("answered").expect("written");
            u.abort("whole").expect("abort");
            assert!(!tmp.path().join("whole").exists(), "a landed file goes at once");
            // unknown, and one a share already took
            assert!(u.abort("whole").expect_err("gone").to_string().contains("unknown id"));
            begin(&mut u, tmp.path(), "shared", 1, 0);
            let (mut s, rx) = slot();
            u.chunk("shared", 0, b"x".to_vec(), true, 0, &mut s).expect("chunk");
            rx.await.expect("answered").expect("written");
            u.mark_sharing("shared", MessageId([1; 16]), 0);
            assert!(u.abort("shared").expect_err("shared").to_string().contains("already shared"));
            assert!(tmp.path().join("shared").join("a.bin").exists());
            u.shared(&MessageId([1; 16]));
            assert!(u.entries.is_empty(), "the share owns the file now");
            assert!(tmp.path().join("shared").join("a.bin").exists());
        });
    }

    #[test]
    fn an_abandoned_upload_is_swept_after_the_idle_window() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            begin(&mut u, tmp.path(), "stale", 6, 100);
            begin(&mut u, tmp.path(), "fresh", 6, 100);
            for id in ["stale", "fresh"] {
                let (mut s, rx) = slot();
                u.chunk(id, 0, b"abc".to_vec(), false, if id == "fresh" { 900 } else { 100 }, &mut s).expect("chunk");
                rx.await.expect("answered").expect("written");
            }
            u.sweep(100 + IDLE_SECS);
            assert_eq!(u.entries.len(), 2, "inside the window nothing goes");
            u.sweep(100 + IDLE_SECS + 1);
            assert!(!u.entries.contains_key("stale"));
            assert!(u.entries.contains_key("fresh"), "a chunk re-arms the window");
            gone(&tmp.path().join("stale")).await;
            assert!(tmp.path().join("fresh").join(PART).exists());
            // a close leaves nothing open
            u.clear();
            gone(&tmp.path().join("fresh")).await;
        });
    }

    #[test]
    fn a_stored_path_is_told_from_any_other() {
        let store = Path::new("/w/uploads");
        let id = "ab".repeat(16);
        let at = |rest: &str| stored_dir(store, &store.join(rest));
        assert_eq!(at(&format!("{id}/a.bin")), Some(store.join(&id)));
        assert_eq!(at("a.bin"), None);
        assert_eq!(at(&format!("{id}/c/a.bin")), None);
        assert_eq!(stored_dir(store, Path::new("/home/x/a.bin")), None);
        // only a directory the engine minted: an id, in its one spelling
        assert_eq!(at("ab/a.bin"), None);
        assert_eq!(at(&format!("{}/a.bin", id.to_uppercase())), None);
        // `..` is never resolved by reading it away: these name the
        // workspace, the store and a sibling, not an upload
        assert_eq!(at("../a.bin"), None);
        assert_eq!(at(&format!("{id}/../a.bin")), None);
        assert_eq!(at(&format!("{id}/../../a.bin")), None);
        assert_eq!(at(&format!("../uploads/{id}/a.bin")), None);
        assert_eq!(at(&format!("{id}/..")), None);
    }

    #[test]
    fn a_share_in_flight_is_not_an_abandoned_upload() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            begin(&mut u, tmp.path(), "u", 1, 0);
            let (mut s, rx) = slot();
            u.chunk("u", 0, b"x".to_vec(), true, 0, &mut s).expect("chunk");
            rx.await.expect("answered").expect("written");
            // shared at the end of its idle window: the sweep that follows
            // must not take the file from under the hash
            u.mark_sharing("u", MessageId([1; 16]), IDLE_SECS);
            u.sweep(IDLE_SECS + 1);
            assert!(u.entries.contains_key("u"));
            assert!(tmp.path().join("u").join("a.bin").exists());
            // a hash that never reports back is abandoned in its turn
            u.sweep(2 * IDLE_SECS + 1);
            assert!(u.entries.is_empty());
            assert!(!tmp.path().join("u").exists());
        });
    }

    #[test]
    fn a_failed_share_releases_its_upload() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut u = Uploads::default();
            for id in ["a", "b"] {
                begin(&mut u, tmp.path(), id, 1, 0);
                let (mut s, rx) = slot();
                u.chunk(id, 0, b"x".to_vec(), true, 0, &mut s).expect("chunk");
                rx.await.expect("answered").expect("written");
            }
            u.mark_sharing("a", MessageId([1; 16]), 0);
            u.share_failed(&MessageId([2; 16]));
            assert_eq!(u.entries.len(), 2, "another share's failure");
            u.share_failed(&MessageId([1; 16]));
            assert!(!u.entries.contains_key("a") && !tmp.path().join("a").exists());
            assert!(tmp.path().join("b").join("a.bin").exists(), "a whole upload is nobody's failed share");
        });
    }

    /// One upload's directory leaving must not take the store from under
    /// another upload's first chunk.
    #[test]
    fn a_prune_never_removes_the_store_under_a_starting_upload() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = tmp.path().join("uploads");
        let pruner = std::thread::spawn({
            let store = store.clone();
            move || {
                for _ in 0..20_000 {
                    prune(&store.join("leaving"));
                }
            }
        });
        for i in 0..20_000 {
            let dir = store.join(format!("u{i}"));
            make_dir(&dir).unwrap_or_else(|e| panic!("round {i}: {e}"));
            std::fs::remove_dir(&dir).expect("the directory just made");
        }
        pruner.join().expect("pruner");
    }

    #[test]
    fn orphans_go_and_named_directories_stay() {
        let tmp = tempfile::tempdir().expect("tmp");
        for d in ["keep", "orphan"] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("dir");
            std::fs::write(tmp.path().join(d).join("f"), b"x").expect("file");
        }
        std::fs::write(tmp.path().join("stray"), b"x").expect("file");
        remove_orphans(tmp.path(), &HashSet::from([tmp.path().join("keep")]));
        let mut left: Vec<String> = std::fs::read_dir(tmp.path())
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["keep"]);
    }
}
