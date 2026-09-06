// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiki file references (`docs/ui/wiki_files_and_images.md` §3.2/§3.3):
//! what an `upload:<hex>` names, whether its bytes are on this device,
//! and the verified read of those bytes.

use std::io::Read as _;

use sha2::Digest as _;

use molt_core::{wiki_refs, LocalCopy, MessageId, MoltError, Reply, UploadView};

use crate::State;

/// What a reference names ([`Reply::UploadResolved`] before it is a
/// reply): shared by the command, the wiki reads and the health pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UploadResolution {
    pub(crate) upload: Option<UploadView>,
    pub(crate) ambiguous: bool,
    pub(crate) temporary: bool,
    pub(crate) local: LocalCopy,
}

impl State {
    /// [`molt_core::Command::ResolveUpload`].
    pub(crate) fn cmd_resolve_upload(&mut self, checksum: &str) -> Result<Reply, MoltError> {
        let r = self.resolve_upload(checksum)?;
        Ok(Reply::UploadResolved {
            upload: r.upload,
            ambiguous: r.ambiguous,
            temporary: r.temporary,
            local: r.local,
        })
    }

    /// One reference against the current tables; refuses a malformed
    /// prefix. A pass over MANY references reads the tables once and
    /// calls [`State::resolve_upload_in`] per reference instead.
    pub(crate) fn resolve_upload(&mut self, checksum: &str) -> Result<UploadResolution, MoltError> {
        let prefix = checksum.trim().to_ascii_lowercase();
        if !wiki_refs::valid_hex(&prefix) {
            return Err(MoltError::BadPayload(format!(
                "checksum: {}..={} hex digits",
                wiki_refs::PREFIX_MIN,
                wiki_refs::HEX_FULL
            )));
        }
        let rows = self.uploads_view();
        let r = self.resolve_upload_in(&rows, &prefix);
        if let Some(u) = &r.upload {
            self.forget_missing_local_copy(&u.checksum);
        }
        Ok(r)
    }

    /// The prefix (lowercase, form already checked) against the
    /// PERSISTENT shares first, like a git short hash; only when none
    /// carries it do the temporary ones answer, flagged `temporary`. Two
    /// rows with the SAME full checksum are one file, not an ambiguity.
    pub(crate) fn resolve_upload_in(&self, rows: &[UploadView], prefix: &str) -> UploadResolution {
        let hits = |persistent: bool| -> Vec<&UploadView> {
            rows.iter()
                .filter(|u| u.persistent == persistent && u.checksum.starts_with(prefix))
                .collect()
        };
        let (candidates, temporary) = match hits(true) {
            v if v.is_empty() => (hits(false), true),
            v => (v, false),
        };
        let mut distinct: Vec<&str> = candidates.iter().map(|u| u.checksum.as_str()).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let ambiguous = distinct.len() > 1;
        let upload = (distinct.len() == 1)
            .then(|| {
                // the row that HAS the bytes, else the first (oldest)
                candidates
                    .iter()
                    .find(|u| self.local_copy_of(u) != LocalCopy::None)
                    .or(candidates.first())
                    .map(|u| (*u).clone())
            })
            .flatten();
        let local = upload.as_ref().map(|u| self.local_copy_of(u)).unwrap_or_default();
        UploadResolution {
            upload,
            ambiguous,
            temporary: temporary && !candidates.is_empty(),
            local,
        }
    }

    /// Whether and how this share's bytes are on this device: the own
    /// source file, a verified download (the live one or the registry),
    /// or the mirror's pieces - in that order.
    pub(crate) fn local_copy_of(&self, u: &UploadView) -> LocalCopy {
        if let Some(p) = self.files.share_paths.get(&u.id) {
            if p.is_file() {
                return LocalCopy::Own { path: p.display().to_string() };
            }
        }
        let live = self
            .files
            .downloads
            .get(&u.id)
            .filter(|d| d.phase == "done" && !d.path.is_empty())
            .map(|d| d.path.clone());
        let registered = self
            .active
            .as_ref()
            .and_then(|a| a.prefs.local_copies.get(&u.checksum).cloned())
            .filter(|_| !u.checksum.is_empty());
        if let Some(path) = live.into_iter().chain(registered).find(|p| std::path::Path::new(p).is_file()) {
            return LocalCopy::Downloaded { path };
        }
        match self.files.mirror.jobs.get(&u.id.to_string()) {
            Some(job) if job.complete => LocalCopy::Mirrored,
            Some(_) => {
                let (held, of) = self.mirror_held_of(&u.id, false, u.mirror_of);
                LocalCopy::Partial { held, of }
            }
            None => LocalCopy::None,
        }
    }

    /// A VERIFIED download's landing, remembered by CONTENT so a restart
    /// does not forget it (§3.2, Q5). Fed from the `NetFileDone` choke
    /// point only - the transfer plane hashes the landed bytes against the
    /// share before it reports done, and [`State::cmd_read_upload_bytes`]
    /// re-hashes them anyway.
    pub(crate) fn register_local_copy(&mut self, id: MessageId, path: &str) {
        if path.is_empty() {
            return;
        }
        let Ok((ident, _)) = self.share_identity(&id) else {
            return;
        };
        if ident.checksum.len() != wiki_refs::HEX_FULL {
            return;
        }
        let Some(a) = &mut self.active else { return };
        if a.prefs.local_copies.get(&ident.checksum).is_some_and(|p| p == path) {
            return;
        }
        a.prefs.local_copies.insert(ident.checksum, path.to_string());
        a.handle.set_prefs(a.prefs.clone());
    }

    /// The registry's read-time clearing (§3.2): an entry whose file is
    /// gone is dropped, so a moved or deleted copy stops being claimed.
    fn forget_missing_local_copy(&mut self, checksum: &str) {
        let stale = self
            .active
            .as_ref()
            .and_then(|a| a.prefs.local_copies.get(checksum))
            .is_some_and(|p| !std::path::Path::new(p).is_file());
        if !stale {
            return;
        }
        if let Some(a) = &mut self.active {
            a.prefs.local_copies.remove(checksum);
            a.handle.set_prefs(a.prefs.clone());
        }
    }

    /// [`molt_core::Command::ReadUploadBytes`] (§3.3). The source is
    /// picked on the actor; the read and the re-hash run in a blocking
    /// task that answers the operator itself through the deferred reply
    /// slot ([`State::deferred_reply`]).
    pub(crate) fn cmd_read_upload_bytes(
        &mut self,
        checksum: String,
        cap: u64,
    ) -> Result<Reply, MoltError> {
        // a closed workspace has no tables, so it answers "unknown" below
        let r = self.resolve_upload(&checksum)?;
        if r.ambiguous {
            return Err(MoltError::Engine("upload bytes: ambiguous reference".to_string()));
        }
        let Some(upload) = r.upload else {
            return Err(MoltError::Engine("upload bytes: unknown reference".to_string()));
        };
        if upload.size > cap {
            return Err(MoltError::BadPayload(format!(
                "upload bytes: {} bytes over the {cap} cap",
                upload.size
            )));
        }
        let source = self.byte_source(&upload, &r.local)?;
        // taken LAST: every refusal above is answered by the actor loop
        let Some(reply) = self.deferred_reply.take() else {
            return Err(MoltError::Engine("upload bytes: no reply channel".to_string()));
        };
        let want = upload.checksum;
        tokio::spawn(async move {
            let out = match tokio::task::spawn_blocking(move || read_verified(&source, &want, cap)).await
            {
                Ok(res) => res,
                Err(e) => Err(MoltError::Engine(format!("upload bytes: read task failed: {e}"))),
            };
            let _ = reply.send(out);
        });
        Ok(Reply::Ack)
    }

    /// The best local source of `upload`'s bytes, in the precedence of
    /// [`State::local_copy_of`]; a partial mirror and an absent copy are
    /// refused here, before any task spawns.
    pub(crate) fn byte_source(
        &self,
        upload: &UploadView,
        local: &LocalCopy,
    ) -> Result<ByteSource, MoltError> {
        match local {
            LocalCopy::Own { path } | LocalCopy::Downloaded { path } => {
                Ok(ByteSource::File(std::path::PathBuf::from(path)))
            }
            LocalCopy::Mirrored => {
                let job = self
                    .files
                    .mirror
                    .jobs
                    .get(&upload.id.to_string())
                    .ok_or_else(|| MoltError::Engine("upload bytes: the mirror is gone".to_string()))?;
                let key = <[u8; 32]>::try_from(job.key.as_slice()).map_err(|_| {
                    MoltError::Engine("upload bytes: the share carries no key".to_string())
                })?;
                let dir = self
                    .mirror_dir()
                    .ok_or_else(|| MoltError::Engine("upload bytes: no mirror folder".to_string()))?
                    .join(upload.id.to_string());
                Ok(ByteSource::Pieces {
                    dir,
                    key,
                    count: job.count,
                    size: job.size,
                })
            }
            LocalCopy::Partial { .. } | LocalCopy::None => {
                Err(MoltError::Engine("upload bytes: not on this device".to_string()))
            }
        }
    }
}

/// Where the bytes come from - resolved on the actor, read off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ByteSource {
    /// A plaintext file: the own share, or a verified download.
    File(std::path::PathBuf),
    /// The mirror folder's sealed data pieces, `0..count`.
    Pieces {
        /// `<mirror_dir>/<series>`.
        dir: std::path::PathBuf,
        /// The share's content key.
        key: [u8; 32],
        /// Data pieces.
        count: u32,
        /// The file's byte length.
        size: u64,
    },
}

/// Read the bytes and answer them ONLY when their sha256 is `want` (§4:
/// the transfer plane's verification is repeated on read, so a swapped
/// file in the exchange folder is a refusal, never a picture).
pub(crate) fn read_verified(source: &ByteSource, want: &str, cap: u64) -> Result<Reply, MoltError> {
    let bytes = match source {
        ByteSource::File(path) => {
            let len = std::fs::metadata(path)
                .map_err(|e| MoltError::Engine(format!("upload bytes: {e}")))?
                .len();
            if len > cap {
                return Err(MoltError::BadPayload(format!(
                    "upload bytes: {len} bytes over the {cap} cap"
                )));
            }
            // read through the cap as well: the file may grow between the
            // stat and the read
            let file = std::fs::File::open(path)
                .map_err(|e| MoltError::Engine(format!("upload bytes: {e}")))?;
            let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
            std::io::Read::take(file, cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| MoltError::Engine(format!("upload bytes: {e}")))?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
                return Err(MoltError::BadPayload(format!("upload bytes: over the {cap} cap")));
            }
            bytes
        }
        ByteSource::Pieces { dir, key, count, size } => {
            if *size > cap {
                return Err(MoltError::BadPayload(format!(
                    "upload bytes: {size} bytes over the {cap} cap"
                )));
            }
            open_pieces(dir, key, *count, *size)?
        }
    };
    if hex::encode(sha2::Sha256::digest(&bytes)) != want {
        return Err(MoltError::Engine("upload bytes: checksum mismatch".to_string()));
    }
    Ok(Reply::UploadBytes { bytes })
}

/// Concatenate the mirror's sealed data pieces in index order. A piece
/// carries its own unpadded length, so `size` is the closing guard rather
/// than a padding rule.
fn open_pieces(
    dir: &std::path::Path,
    key: &[u8; 32],
    count: u32,
    size: u64,
) -> Result<Vec<u8>, MoltError> {
    let want = usize::try_from(size).unwrap_or(usize::MAX);
    let mut out: Vec<u8> = Vec::with_capacity(want.min(1 << 20));
    for index in 0..count {
        let sealed = std::fs::read_to_string(dir.join(index.to_string()))
            .map_err(|e| MoltError::Engine(format!("upload bytes: piece {index}: {e}")))?;
        let (got, _, payload) = molt_net::file_plane::open_piece(key, sealed.trim())
            .map_err(|e| MoltError::Engine(format!("upload bytes: piece {index}: {e}")))?;
        if got != index {
            return Err(MoltError::Engine(format!(
                "upload bytes: piece {index} claims {got}"
            )));
        }
        out.extend_from_slice(&payload);
        // one piece of slack: a padded last slice is trimmed below, a
        // series claiming more than its manifest size is not read on
        if out.len() > want.saturating_add(molt_net::file_plane::PIECE_PAYLOAD_LEN) {
            return Err(MoltError::Engine("upload bytes: the pieces exceed the file".to_string()));
        }
    }
    if out.len() < want {
        return Err(MoltError::Engine("upload bytes: the mirror is incomplete".to_string()));
    }
    out.truncate(want);
    Ok(out)
}
