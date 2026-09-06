// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiki file references (`docs/ui/wiki_files_and_images.md` §3.2/§3.3):
//! what an `upload:<hex>` names, whether its bytes are on this device,
//! and the verified read of those bytes.

use molt_core::{wiki_refs, LocalCopy, MoltError, Reply, UploadView};

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
        Ok(self.resolve_upload_in(&rows, &prefix))
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

    /// [`molt_core::Command::ReadUploadBytes`]: not built yet (stage 2 of
    /// the plan) - refuses honestly instead of answering unverified bytes.
    pub(crate) fn cmd_read_upload_bytes(&mut self, checksum: String, cap: u64) -> Result<Reply, MoltError> {
        let _ = (checksum, cap);
        Err(MoltError::Engine("upload bytes: not available".to_string()))
    }
}
