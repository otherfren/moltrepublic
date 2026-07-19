// SPDX-License-Identifier: GPL-3.0-or-later

//! Import of a `molt-export-v1` blob — the storage half of restore
//! (mock_todo story 13, `backup_restore_design.md` §4).
//!
//! The pipeline is **stage → verify → commit**, layering-clean:
//! `molt-storage` cannot call the engine's `verify_chain`, so
//! [`import_stage`] only decrypts, validates the *format* and the *key
//! hierarchy*, and writes everything into an invisible dot-staging dir
//! (`root/.import-<id>/`). It hands back an [`ImportStaging`] exposing the
//! parsed chain — the ENGINE then hard-verifies (`verify_chain` /
//! `verify_suffix_chain`, republic-id consistency) and only on success
//! calls [`ImportStaging::commit`], which re-seals the key material under
//! the LOCAL device key, writes a **fresh minimal** `transport.state`
//! (derived identity only — live MLS/queue state is never imported, §3.3)
//! and atomically renames the staging dir into place. Nothing partial ever
//! becomes visible; dropping the staging (or a crash) leaves only a
//! dot-dir the next [`import_stage`] for the same workspace sweeps.
//!
//! Blocking (Argon2 for passphrase blobs + file I/O) — call off-actor only.

use std::fs;
use std::path::{Path, PathBuf};

use molt_core::{EventEnvelope, WorkspaceEvent, WorkspaceManifest};
use zeroize::Zeroizing;

use crate::export::{self, ExportSecret};
use crate::StorageError;

/// The staged, decrypted, format-validated content of one export blob —
/// between [`import_stage`] and the engine's verify + [`Self::commit`].
/// Dropping it (or [`Self::abort`]) removes the staging dir.
pub struct ImportStaging {
    /// The staging directory (`root/.import-<id>`, dot-invisible).
    dir: PathBuf,
    /// Whether [`Self::commit`] renamed the dir into place (drop then
    /// leaves it alone).
    committed: bool,
    /// The blob's `manifest.toml` — the unauthenticated cover sheet; the
    /// genesis/chain are authoritative (design §4.1).
    pub manifest: WorkspaceManifest,
    /// The decrypted `Founded` genesis envelope (frame 1 of segment 1) —
    /// decrypting it with the payload's workspace key is the proof that
    /// key and content belong together.
    pub genesis: EventEnvelope,
    /// The checkpoint blob of a pruned chain (`None` = full chain).
    pub checkpoint: Option<molt_core::CheckpointState>,
    /// The persistent commit-block chain, still UNVERIFIED — the engine
    /// must hard-verify before anything materializes.
    pub chain: Vec<molt_core::ChainBlock>,
    /// Authenticated creation time of the export (rollback honesty §3.7).
    pub created: u64,
    /// At-rest state of the source at export time (`"device" | "phrase"`).
    pub at_rest: String,
    /// The workspace key from the authenticated payload meta.
    workspace_key: Zeroizing<[u8; 32]>,
    /// The recovery-seed entropy, when the blob carries it (§3.6).
    seed: Option<Zeroizing<Vec<u8>>>,
}

/// The commit hook the caller uses to trash an existing same-id dir on an
/// explicit replace (design P2) — kept as a parameter-free bool because the
/// trash path is fixed (`trash_workspace`).
///
/// Stage one blob into `root/.import-<id>/`. `secret` is the ONE
/// user-supplied secret string; its meaning follows the blob's own header
/// (never guessed): a `passphrase`-mode blob (manual export) takes it as
/// the export passphrase, a `workspace`-mode blob (auto-backup) takes it
/// as the BIP-39 **recovery phrase** and derives
/// `workspace_key = HKDF(entropy, "molt-ws-key", <header id>)`.
///
/// Everything §4.1 step 1 demands happens here, hard-reject: format/version
/// gates and KDF caps (in `read_export`), an entry-path **allowlist**
/// (`manifest.toml`, `prefs.toml`, `chain.state`, `log/*.mlog`,
/// `snapshots/*.msnap`, `logo.<ext>` — anything else, `keys/`/
/// `transport.state` included, rejects the whole blob), manifest ↔ header
/// id consistency, the key-hierarchy pin, a genesis frame that decrypts
/// under the payload's workspace key, and a `chain.state` that decrypts and
/// parses. Blocking — call off-actor only.
pub fn import_stage(
    root: &Path,
    blob: &[u8],
    secret: &str,
) -> Result<ImportStaging, StorageError> {
    // the header decides how the secret is applied (§3.4 key modes)
    let (header, _bytes) = export::read_header(&mut &blob[..])?;
    let export_secret = match header.key_mode.as_str() {
        "workspace" => {
            let entropy = crate::seed_entropy(secret).map_err(|_| {
                StorageError::BadSeed(
                    "this backup unlocks with the RECOVERY PHRASE (24 words) — \
                     the typed secret does not parse as one"
                        .to_string(),
                )
            })?;
            ExportSecret::WorkspaceKey(crate::derive_workspace_key(
                &entropy,
                &header.workspace_id,
            ))
        }
        // "passphrase" — and any unknown mode is rejected inside read_export
        _ => ExportSecret::passphrase(secret),
    };
    let archive = export::read_export(&mut &blob[..], &export_secret)?;

    // entry-path allowlist (subsumes traversal, which read_export already
    // rejects): keys/ and transport.state can never ride a blob into a dir.
    // Duplicate paths are a hard reject in the same pass: every validation
    // below uses the FIRST match of a path while the write loop lets the
    // LAST write win, so a forged blob pairing a benign twin (which passes
    // verification) with a malicious one (which is what materializes) at the
    // same path must never be accepted.
    let mut seen = std::collections::BTreeSet::new();
    for entry in &archive.entries {
        if !allowed_entry(&entry.path) {
            return Err(StorageError::Corrupt(format!(
                "blob carries a file outside the import allowlist: `{}`",
                entry.path
            )));
        }
        if !seen.insert(entry.path.as_str()) {
            return Err(StorageError::Corrupt(format!(
                "blob carries a duplicate entry path: `{}`",
                entry.path
            )));
        }
    }

    // the cover sheet: parse + gate the manifest before writing anything
    let manifest_bytes = archive
        .entries
        .iter()
        .find(|e| e.path == "manifest.toml")
        .ok_or_else(|| StorageError::Corrupt("blob carries no manifest.toml".to_string()))?;
    let manifest: WorkspaceManifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes.data)
            .map_err(|_| StorageError::BadFile("manifest.toml is not UTF-8".to_string()))?,
    )
    .map_err(|e| StorageError::BadFile(format!("manifest.toml: {e}")))?;
    if manifest.format != molt_core::MANIFEST_FORMAT {
        return Err(StorageError::BadFile(format!(
            "the blob's manifest is not a workspace manifest (format `{}`)",
            manifest.format
        )));
    }
    if manifest.version > molt_core::STORAGE_VERSION_SEALED {
        return Err(StorageError::NewerVersion(manifest.version));
    }
    if manifest.workspace.id != archive.header.workspace_id {
        return Err(StorageError::Corrupt(
            "blob is internally inconsistent (manifest id != header id)".to_string(),
        ));
    }
    let id_hex = manifest.workspace.id.clone();
    let id = crate::id_bytes(&id_hex)?;

    // at-rest marker consistency (S6): the vocabulary is closed, a sealed
    // source never carries a seed, and the inner manifest must agree
    let at_rest = archive.meta.at_rest.clone();
    match at_rest.as_str() {
        molt_core::SEALED_DEVICE => {
            if crate::is_sealed(&manifest) {
                return Err(StorageError::Corrupt(
                    "blob is internally inconsistent (device-at-rest blob with a \
                     phrase-sealed manifest)"
                        .to_string(),
                ));
            }
        }
        molt_core::SEALED_PHRASE => {
            if archive.meta.seed.is_some() {
                return Err(StorageError::Corrupt(
                    "blob is internally inconsistent (a phrase-sealed export \
                     must not carry the seed)"
                        .to_string(),
                ));
            }
            if !crate::is_sealed(&manifest) {
                return Err(StorageError::Corrupt(
                    "blob is internally inconsistent (phrase-at-rest blob with a \
                     device-sealed manifest)"
                        .to_string(),
                ));
            }
        }
        other => {
            return Err(StorageError::Corrupt(format!(
                "unknown at-rest state `{other}` in the export meta"
            )));
        }
    }

    // key material from the authenticated meta (hierarchy already pinned
    // against the seed inside read_export)
    let ws_key: Zeroizing<[u8; 32]> = Zeroizing::new(
        hex::decode(&archive.meta.workspace_key)
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
            .ok_or_else(|| {
                StorageError::Corrupt("export meta carries no valid workspace key".to_string())
            })?,
    );
    let seed: Option<Zeroizing<Vec<u8>>> = match &archive.meta.seed {
        Some(hexstr) => Some(Zeroizing::new(hex::decode(hexstr).map_err(|e| {
            StorageError::Corrupt(format!("export meta seed: {e}"))
        })?)),
        None => None,
    };
    // the workspace id itself is part of the hierarchy: id = HKDF(seed,
    // "molt-ws-id", member) — pin it when the seed travels (the genesis
    // below names the member)
    // (checked after the genesis decrypt, which yields the member)

    // decrypt the genesis frame — the proof that key and content match
    let seg1 = archive
        .entries
        .iter()
        .find(|e| e.path == "log/000001.mlog")
        .ok_or_else(|| {
            StorageError::Corrupt("blob carries no log/000001.mlog (no genesis)".to_string())
        })?;
    let (frames, _torn) = crate::split_frames(&seg1.data);
    let first = frames.first().ok_or_else(|| {
        StorageError::Corrupt("the blob's first log segment holds no valid frame".to_string())
    })?;
    let genesis_plain = crate::decrypt_frame(&ws_key, &id, 1, 1, first.nonce, first.ciphertext)
        .map_err(|_| {
            StorageError::Crypto(
                "blob is internally inconsistent (the genesis does not decrypt \
                 under the blob's workspace key)"
                    .to_string(),
            )
        })?;
    let genesis: EventEnvelope = serde_json::from_slice(&genesis_plain)
        .map_err(|e| StorageError::Corrupt(format!("genesis envelope: {e}")))?;
    let WorkspaceEvent::Founded { member, .. } = &genesis.body else {
        return Err(StorageError::Corrupt(
            "the blob's first event is not a Founded genesis".to_string(),
        ));
    };
    if genesis.seq != 1 {
        return Err(StorageError::Corrupt("genesis must be seq 1".to_string()));
    }
    if let Some(s) = &seed {
        if crate::derive_workspace_id(s, member) != id_hex {
            return Err(StorageError::Crypto(
                "blob is internally inconsistent (key hierarchy: the seed does \
                 not derive this workspace id)"
                    .to_string(),
            ));
        }
    }

    // decrypt + parse chain.state (hard errors — unlike an open, where a
    // local file may be damaged, an import must never guess)
    let (checkpoint, chain) = match archive.entries.iter().find(|e| e.path == "chain.state") {
        None => (None, Vec::new()),
        Some(entry) => {
            let (frames, torn) = crate::split_frames(&entry.data);
            if frames.len() != 1 || torn.is_some() {
                return Err(StorageError::Corrupt(
                    "the blob's chain.state framing is damaged".to_string(),
                ));
            }
            let chain_key = crate::hkdf32(&*ws_key, "molt-chain-state", &id);
            let plain = crate::decrypt_frame(
                &chain_key,
                &id,
                crate::CHAIN_SEGMENT,
                0,
                frames[0].nonce,
                frames[0].ciphertext,
            )
            .map_err(|_| {
                StorageError::Crypto(
                    "the blob's chain.state does not authenticate under the \
                     blob's workspace key"
                        .to_string(),
                )
            })?;
            match serde_json::from_slice::<crate::ChainStateFile>(&plain) {
                Ok(crate::ChainStateFile::Full(chain)) => (None, chain),
                Ok(crate::ChainStateFile::Pruned {
                    checkpoint_blob,
                    blocks,
                }) => (Some(checkpoint_blob), blocks),
                Err(e) => {
                    return Err(StorageError::Corrupt(format!(
                        "the blob's chain.state does not parse: {e}"
                    )));
                }
            }
        }
    };

    // write the staging dir (dot-invisible to the Open scan); a stale
    // staging for the same id is a leftover crash artifact — sweep it
    let staging = root.join(format!(".import-{id_hex}"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    let write = || -> Result<(), StorageError> {
        for sub in ["keys", "log", "snapshots", "tmp"] {
            fs::create_dir_all(staging.join(sub))?;
        }
        for entry in &archive.entries {
            let target = staging.join(&entry.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &entry.data)?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    Ok(ImportStaging {
        dir: staging,
        committed: false,
        manifest,
        genesis,
        checkpoint,
        chain,
        created: archive.meta.created,
        at_rest,
        workspace_key: ws_key,
        seed,
    })
}

/// The §4.1 entry allowlist: exactly the files the export include-table
/// (§3.2) can produce. Rejecting everything else subsumes traversal AND
/// pins the §3.3 exclusions (`keys/`, `transport.state`) a forged blob
/// might try to smuggle in.
fn allowed_entry(path: &str) -> bool {
    match path {
        "manifest.toml" | "prefs.toml" | "chain.state" => true,
        p => {
            if let Some(file) = p.strip_prefix("log/") {
                return numeric_stem(file, ".mlog");
            }
            if let Some(file) = p.strip_prefix("snapshots/") {
                return numeric_stem(file, ".msnap");
            }
            if let Some(ext) = p.strip_prefix("logo.") {
                return !ext.is_empty()
                    && ext.len() <= 8
                    && ext.bytes().all(|b| b.is_ascii_alphanumeric());
            }
            false
        }
    }
}

/// `NNN…<ext>` with a purely numeric, non-empty stem and no path parts.
fn numeric_stem(file: &str, ext: &str) -> bool {
    file.strip_suffix(ext)
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
        && !file.contains('/')
}

impl ImportStaging {
    /// The staging directory (dot-invisible until commit).
    pub fn staging_dir(&self) -> &Path {
        &self.dir
    }

    /// The recovery-seed entropy the blob carries, if any (§3.6). The
    /// ENGINE uses it to derive + roster-verify the seat identity before
    /// commit — storage cannot know which ritual derivation was anchored.
    pub fn seed_entropy(&self) -> Option<&[u8]> {
        self.seed.as_deref().map(Vec::as_slice)
    }

    /// Commit the verified staging into `root` (§4.1 step 3) — call ONLY
    /// after the engine hard-verified the chain. Re-seals the workspace key
    /// (and seed, when present) under the **local** device key for a
    /// device-at-rest blob; a phrase-sealed blob round-trips sealed (no key
    /// material lands on disk). When `identity_sk` is given (the engine's
    /// roster-anchored derivation), a **fresh minimal** `transport.state`
    /// carries it — derived, never cloned: no MLS ratchet, no queue
    /// credentials, no cursors (§3.3), so the workspace opens *detached*.
    ///
    /// Collision (design P2): an existing same-id workspace refuses with
    /// [`StorageError::Exists`]; `replace = true` moves the existing dir to
    /// the recoverable `.trash` first. Consumes the staging: on ANY error
    /// the staging dir is removed and nothing became visible.
    pub fn commit(
        mut self,
        root: &Path,
        replace: bool,
        identity_sk: Option<&crate::SigningKey>,
    ) -> Result<PathBuf, StorageError> {
        let result = self.commit_inner(root, replace, identity_sk);
        if result.is_ok() {
            self.committed = true; // drop leaves the renamed dir alone
        }
        result
    }

    fn commit_inner(
        &mut self,
        root: &Path,
        replace: bool,
        identity_sk: Option<&crate::SigningKey>,
    ) -> Result<PathBuf, StorageError> {
        let id_hex = self.manifest.workspace.id.clone();
        let id = crate::id_bytes(&id_hex)?;

        // collision policy (P2): refuse by default — the existing dir may
        // be AHEAD of the backup; an explicit replace trashes it (recoverable
        // 30 days), never an in-place merge. We only DECIDE here; the
        // destructive trash is deferred until the replacement is fully staged
        // (below), so a failure mid-commit can never leave the id with ZERO
        // visible dirs (old already trashed, new not yet materialized).
        let existing = crate::find_workspace_dir(root, &id_hex);
        let final_dir = root.join(crate::workspace_dirname(
            &self.manifest.workspace.name,
            &id_hex,
        ));
        if let Some(dir) = &existing {
            if !replace {
                return Err(StorageError::Exists(dir.clone()));
            }
        } else if final_dir.exists() {
            // same directory name without a matching manifest id — foreign
            // content we must not clobber
            return Err(StorageError::Exists(final_dir));
        }

        // --- stage every write into the (still invisible) staging dir BEFORE
        //     anything destructive touches the pre-existing workspace ---

        // prefs travel (§3.2), but `last_backup` is THIS-node bookkeeping —
        // stamped only when the RUNNING node confirms an upload. The source
        // node's stamp must not survive the import: the restored node would
        // claim an upload it never made, and the ticker would skip the
        // fresh first backup of the imported content.
        let mut prefs = crate::read_prefs(&self.dir);
        if prefs.last_backup.take().is_some() {
            crate::write_prefs(&self.dir, &prefs)?;
        }

        // key material per the at-rest state (a phrase-sealed blob commits
        // WITHOUT keys — S6 semantics survive the round trip)
        if self.at_rest == molt_core::SEALED_DEVICE {
            let device_key = crate::load_or_create_device_key(&crate::device_key_path(root))?;
            let sealed = crate::seal_workspace_key(&device_key, &id, &self.workspace_key)?;
            crate::write_atomic(&self.dir, "keys/workspace.key", &sealed, true)?;
            if let Some(seed) = &self.seed {
                let sealed_seed = crate::seal_seed_entropy(&device_key, &id, seed)?;
                crate::write_atomic(&self.dir, "keys/seed.sealed", &sealed_seed, true)?;
            }
            // fresh minimal transport.state: version + derived identity
            // only — NEVER ratchets or queue credentials (§3.3)
            if let Some(sk) = identity_sk {
                let state = molt_core::TransportState {
                    identity_sk: Some(sk.to_bytes().to_vec()),
                    ..Default::default()
                };
                crate::write_transport_state_at(&self.dir, &self.workspace_key, &id, &state)?;
            }
        }

        // --- the destructive swap, ordered so the id is never left with zero
        //     visible dirs: trash the pre-existing dir only now that the
        //     replacement is fully staged, and roll that trash BACK if the
        //     final rename still fails (ENOSPC, a foreign dir occupying the
        //     target name, …) so the old workspace stays visible ---
        let rescued = match &existing {
            Some(dir) => Some((dir.clone(), crate::trash_workspace(root, dir)?)),
            None => None,
        };
        if let Err(e) = fs::rename(&self.dir, &final_dir) {
            if let Some((original, trashed)) = rescued {
                // best-effort: return the id's only workspace to visibility
                let _ = fs::rename(&trashed, &original);
            }
            return Err(e.into());
        }
        // make the rename durable (same rule as create_workspace)
        if let Ok(d) = fs::File::open(root) {
            let _ = d.sync_all();
        }
        Ok(final_dir)
    }

    /// Discard the staging (explicit spelling of drop).
    pub fn abort(self) {
        drop(self);
    }
}

// manual Debug: the staged KEY MATERIAL must never reach a log line
impl std::fmt::Debug for ImportStaging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportStaging")
            .field("dir", &self.dir)
            .field("id", &self.manifest.workspace.id)
            .field("chain_blocks", &self.chain.len())
            .field("at_rest", &self.at_rest)
            .field("seed", &self.seed.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for ImportStaging {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{export_dir, ExportKey};

    const PASS: &str = "correct horse battery";

    /// The `Founded` genesis every fixture workspace is built from (member
    /// `mithra`, so the same seed always derives the same workspace id).
    fn founded_genesis() -> EventEnvelope {
        EventEnvelope {
            seq: 1,
            ts: 42,
            by: "mithra".to_string(),
            body: WorkspaceEvent::Founded {
                name: "Chess Club".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "mithra".to_string(),
                roster: vec!["mithra".to_string(), "anahita".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
            },
        }
    }

    /// A populated workspace under `root` (chain.state included) — the
    /// export fixture. Returns `(root, ws_dir, seed, id)`.
    fn make_ws(tmp: &Path) -> (PathBuf, PathBuf, Vec<u8>, String) {
        let root = tmp.join("src-root");
        let seed =
            crate::seed_entropy(&crate::generate_seed_phrase().expect("gen")).expect("entropy");
        let ws = crate::create_workspace(&root, &seed, &founded_genesis()).expect("create");
        ws.write_chain(None, &[]).expect("chain.state");
        let id = ws.manifest.workspace.id.clone();
        let dir = ws.dir().to_path_buf();
        drop(ws);
        std::fs::write(dir.join("logo.png"), b"logo bytes").expect("logo");
        std::fs::write(dir.join("transport.state"), b"NEVER IMPORTED").expect("ts");
        (root, dir, seed, id)
    }

    fn blob_of(root: &Path, dir: &Path, key: &ExportKey) -> Vec<u8> {
        let mut blob = Vec::new();
        export_dir(root, dir, key, &mut blob).expect("export");
        blob
    }

    /// No dot-entries (staging) may become visible to the Open scan.
    fn scan_names(root: &Path) -> Vec<String> {
        crate::scan_workspaces(root)
            .into_iter()
            .map(|e| e.manifest.workspace.id.clone())
            .collect()
    }

    /// Stage + commit round-trips a passphrase blob into a FRESH root: the
    /// dir opens, its genesis matches, the key material is re-sealed to the
    /// LOCAL device key, and no transport.state with live state exists —
    /// only the fresh minimal one when an identity is passed.
    #[test]
    fn stage_and_commit_round_trip_into_a_fresh_root() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (src_root, src_dir, seed, id) = make_ws(tmp.path());
        let blob = blob_of(&src_root, &src_dir, &ExportKey::passphrase(PASS));

        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");
        let staging = import_stage(&dest_root, &blob, PASS).expect("stage");
        assert_eq!(staging.manifest.workspace.id, id);
        assert_eq!(staging.genesis.seq, 1);
        assert!(staging.created > 0, "authenticated meta.created");
        assert_eq!(staging.at_rest, molt_core::SEALED_DEVICE);
        assert_eq!(staging.seed_entropy(), Some(seed.as_slice()));
        assert!(scan_names(&dest_root).is_empty(), "staging is invisible");

        // the engine would verify the chain here; commit with an identity
        let (sk, _pk) = crate::derive_identity_key(&seed, &id);
        let dir = staging.commit(&dest_root, false, Some(&sk)).expect("commit");
        assert_eq!(scan_names(&dest_root), vec![id.clone()], "now visible");

        // the imported dir OPENS with the local device key (re-sealed) and
        // replays the same genesis
        let (opened, loaded) = crate::open_workspace(&dir).expect("open imported");
        assert_eq!(opened.manifest.workspace.id, id);
        assert_eq!(loaded.tail.first().map(|e| e.seq), Some(1));
        // fresh minimal transport.state: identity only, no mesh, no MLS
        let ts = opened.read_transport_state();
        assert_eq!(
            ts.identity_sk.as_deref(),
            Some(sk.to_bytes().as_slice()),
            "the derived identity travels"
        );
        assert!(ts.mls.is_none(), "no MLS ratchet is ever imported");
        assert!(ts.mesh.is_empty(), "no mesh links are ever imported");
        assert!(ts.smp_queues.is_none(), "no queue credentials are ever imported");
        // the seed is re-sealed for the details panel
        let phrase = crate::read_sealed_seed(&dest_root, &dir, &id).expect("seed stored");
        assert_eq!(crate::seed_entropy(&phrase).expect("entropy"), seed);
    }

    /// A workspace-mode blob (the auto-backup) stages with the recovery
    /// PHRASE as the secret; garbage in the phrase field is an honest
    /// BadSeed, not an AEAD failure.
    #[test]
    fn workspace_mode_blob_stages_with_the_recovery_phrase() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (src_root, src_dir, seed, id) = make_ws(tmp.path());
        let blob = blob_of(&src_root, &src_dir, &ExportKey::Workspace);
        let phrase = bip39::Mnemonic::from_entropy(&seed).expect("bip39").to_string();

        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");
        let staging = import_stage(&dest_root, &blob, &phrase).expect("stage with phrase");
        assert_eq!(staging.manifest.workspace.id, id);
        staging.abort();
        assert!(
            std::fs::read_dir(&dest_root).expect("dir").next().is_none(),
            "abort leaves no residue"
        );

        let err = import_stage(&dest_root, &blob, "not a phrase")
            .expect_err("garbage secret");
        assert!(
            err.to_string().contains("RECOVERY PHRASE"),
            "honest secret-kind message: {err}"
        );
        // a WRONG (but valid) phrase fails at the AEAD, indistinguishable
        // from tampering (§4.2)
        let wrong = crate::generate_seed_phrase().expect("gen");
        let err = import_stage(&dest_root, &blob, &wrong).expect_err("wrong phrase");
        assert!(err.to_string().contains("wrong passphrase or damaged blob"), "{err}");
        assert!(
            std::fs::read_dir(&dest_root).expect("dir").next().is_none(),
            "no staging residue on failure"
        );
    }

    /// §4.3 collision: a same-id import refuses by default; an explicit
    /// replace trashes the existing dir (recoverable) and commits.
    #[test]
    fn collision_refuses_and_replace_trashes_first() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, _seed, id) = make_ws(tmp.path());
        let blob = blob_of(&root, &dir, &ExportKey::passphrase(PASS));

        // import into the SAME root that still holds the workspace
        let staging = import_stage(&root, &blob, PASS).expect("stage");
        let err = staging.commit(&root, false, None).expect_err("collision");
        assert!(matches!(err, StorageError::Exists(_)), "refuse: {err}");

        // the refusal aborted the staging; a fresh stage + replace commits
        let staging = import_stage(&root, &blob, PASS).expect("stage 2");
        let new_dir = staging.commit(&root, true, None).expect("replace commits");
        assert!(new_dir.exists());
        assert_eq!(scan_names(&root), vec![id], "one visible workspace");
        assert!(
            root.join(".trash").read_dir().expect("trash").count() > 0,
            "the old dir is recoverable in .trash"
        );
    }

    /// The allowlist rejects a forged blob smuggling `keys/…` or
    /// `transport.state` entries — §3.3 stays pinned on the read side.
    #[test]
    fn forged_entries_outside_the_allowlist_are_rejected() {
        assert!(allowed_entry("manifest.toml"));
        assert!(allowed_entry("prefs.toml"));
        assert!(allowed_entry("chain.state"));
        assert!(allowed_entry("log/000001.mlog"));
        assert!(allowed_entry("snapshots/000000000009.msnap"));
        assert!(allowed_entry("logo.png"));
        for evil in [
            "transport.state",
            "keys/workspace.key",
            "keys/seed.sealed",
            "LOCK",
            "log/evil.mlog",
            "log/1/2.mlog",
            "snapshots/x.msnap",
            "logo.", // empty extension
            "logo.reallylongext",
            "notes.txt",
            "tmp/x",
        ] {
            assert!(!allowed_entry(evil), "must reject `{evil}`");
        }
    }

    /// Tampering anywhere in the blob rejects the stage (spot check — the
    /// exhaustive byte-flip loop lives in export.rs); and a truncated blob
    /// rejects too. No staging residue either way.
    #[test]
    fn tampered_or_truncated_blobs_reject_with_no_residue() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (root, dir, _seed, _id) = make_ws(tmp.path());
        let blob = blob_of(&root, &dir, &ExportKey::passphrase(PASS));
        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");

        let mut tampered = blob.clone();
        let at = tampered.len() / 2;
        tampered[at] ^= 0x01;
        assert!(import_stage(&dest_root, &tampered, PASS).is_err(), "flip rejects");
        assert!(
            import_stage(&dest_root, &blob[..blob.len() - 7], PASS).is_err(),
            "truncation rejects"
        );
        assert!(
            std::fs::read_dir(&dest_root).expect("dir").next().is_none(),
            "no staging residue"
        );
    }

    /// §4.1 replace must NEVER leave the id with zero visible workspaces. If
    /// the commit fails *after* the pre-existing dir was trashed (here: the
    /// final rename fails because a foreign dir occupies the target name),
    /// the trash is rolled back and the old workspace stays openable.
    #[test]
    fn replace_rolls_back_the_trash_when_the_final_rename_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (src_root, src_dir, seed, id) = make_ws(tmp.path());
        let blob = blob_of(&src_root, &src_dir, &ExportKey::passphrase(PASS));

        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");

        // dest_root already holds the SAME workspace (same seed → same id),
        // but its dir was renamed on disk to a NON-canonical name, so the
        // import's final_dir (derived from the manifest name) differs from it
        let existing = crate::create_workspace(&dest_root, &seed, &founded_genesis())
            .expect("existing ws");
        existing.write_chain(None, &[]).expect("chain.state");
        let existing_dir = existing.dir().to_path_buf();
        drop(existing);
        let renamed = dest_root.join("renamed-existing");
        std::fs::rename(&existing_dir, &renamed).expect("rename existing aside");
        assert_eq!(
            crate::find_workspace_dir(&dest_root, &id),
            Some(renamed.clone()),
            "the existing workspace is found by id under its new name"
        );

        // occupy the canonical final_dir with foreign, NON-EMPTY content (no
        // manifest → not the existing-by-id dir) so the final rename fails
        let final_dir = dest_root.join(crate::workspace_dirname("Chess Club", &id));
        std::fs::create_dir_all(&final_dir).expect("final dir");
        std::fs::write(final_dir.join("junk"), b"foreign").expect("junk");

        let staging = import_stage(&dest_root, &blob, PASS).expect("stage");
        let err = staging
            .commit(&dest_root, true, None)
            .expect_err("the final rename into an occupied dir must fail");
        assert!(matches!(err, StorageError::Io(_)), "rename failure: {err}");

        // the id is NOT lost: the old workspace was rolled back out of trash
        // and is still visible and openable
        assert_eq!(
            crate::find_workspace_dir(&dest_root, &id),
            Some(renamed.clone()),
            "the old workspace is rolled back and visible"
        );
        let (opened, _loaded) = crate::open_workspace(&renamed).expect("old ws still opens");
        assert_eq!(opened.manifest.workspace.id, id);
    }

    /// A forged blob carrying two entries at the SAME path is a hard reject:
    /// validation uses the FIRST match while the write loop lets the LAST
    /// write win, so a benign twin could otherwise smuggle a malicious file
    /// onto disk. Nothing stages.
    #[test]
    fn duplicate_entry_paths_are_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dest_root = tmp.path().join("dest-root");
        std::fs::create_dir_all(&dest_root).expect("dest root");

        let (blob, phrase) = forge_workspace_blob(&[
            ("manifest.toml", b"benign"),
            ("manifest.toml", b"malicious"),
        ]);
        let err = import_stage(&dest_root, &blob, &phrase).expect_err("duplicate path");
        assert!(
            err.to_string().contains("duplicate entry path"),
            "honest duplicate-path reject: {err}"
        );
        assert!(
            std::fs::read_dir(&dest_root).expect("dir").next().is_none(),
            "no staging residue"
        );

        // control: a single entry clears the duplicate gate (it then fails
        // later, at the manifest parse — proving the reject above is the
        // duplicate check, not a blanket refusal of every forged blob)
        let (ok_blob, ok_phrase) = forge_workspace_blob(&[("manifest.toml", b"benign")]);
        let err = import_stage(&dest_root, &ok_blob, &ok_phrase)
            .expect_err("garbage manifest still fails");
        assert!(
            !err.to_string().contains("duplicate entry path"),
            "a single entry must clear the duplicate gate: {err}"
        );
    }

    /// Forge a decryptable `molt-export-v1` blob (workspace key mode) whose
    /// payload carries exactly `entries` — shapes the honest exporter never
    /// emits (here: duplicate entry paths). `import_stage` re-derives the
    /// workspace key from the returned phrase + the header id, so the blob
    /// decrypts; the entries need only be format-valid, not semantically real
    /// (the duplicate reject fires before any manifest/genesis parse). One
    /// final chunk.
    fn forge_workspace_blob(entries: &[(&str, &[u8])]) -> (Vec<u8>, String) {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::{XChaCha20Poly1305, XNonce};

        let phrase = crate::generate_seed_phrase().expect("gen");
        let entropy = crate::seed_entropy(&phrase).expect("entropy");
        let id_hex = "ab".repeat(32);
        let id = crate::id_bytes(&id_hex).expect("id");
        let ws_key = crate::derive_workspace_key(&entropy, &id_hex);

        let header = crate::export::ExportHeader {
            format: "molt-export-v1".to_string(),
            version: 1,
            workspace_id: id_hex.clone(),
            key_mode: "workspace".to_string(),
            kdf: None,
            cipher: "xchacha20poly1305".to_string(),
            chunk_bytes: 4096,
        };
        let header_bytes = serde_json::to_vec(&header).expect("header json");

        // meta: `files` must match the entry count, `seed=null` sidesteps the
        // hierarchy pin, `workspace_key` must be valid 32-byte hex
        let meta = serde_json::json!({
            "created": 7,
            "exporter": "test",
            "at_rest": "device",
            "workspace_key": hex::encode(ws_key),
            "seed": serde_json::Value::Null,
            "files": entries.len(),
        });
        let meta_bytes = serde_json::to_vec(&meta).expect("meta json");

        let mut payload = u32::try_from(meta_bytes.len())
            .expect("meta len")
            .to_le_bytes()
            .to_vec();
        payload.extend_from_slice(&meta_bytes);
        for (path, data) in entries {
            payload.extend_from_slice(&u16::try_from(path.len()).expect("path len").to_le_bytes());
            payload.extend_from_slice(path.as_bytes());
            payload.extend_from_slice(&u64::try_from(data.len()).expect("data len").to_le_bytes());
            payload.extend_from_slice(data);
        }

        // workspace-mode key schedule (mirrors export.rs's frozen HKDF tags)
        let k_root = crate::hkdf32(&ws_key, "molt-export-backup-v1", &id);
        let k_stream = crate::hkdf32(&k_root, "molt-export-stream-v1", &header_bytes);
        let cipher = XChaCha20Poly1305::new((&k_stream).into());

        // a single final chunk: aad = magic ‖ id ‖ index(0) ‖ final(1)
        let mut aad = [0u8; 56];
        aad[..15].copy_from_slice(b"molt-export-v1\0");
        aad[15..47].copy_from_slice(&id);
        aad[55] = 1;
        let nonce = [0u8; 24];
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: &payload, aad: &aad })
            .expect("encrypt");

        let mut blob = b"molt-export-v1\0".to_vec();
        blob.extend_from_slice(
            &u32::try_from(header_bytes.len()).expect("header len").to_le_bytes(),
        );
        blob.extend_from_slice(&header_bytes);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&u32::try_from(ct.len()).expect("ct len").to_le_bytes());
        blob.extend_from_slice(&ct);
        (blob, phrase)
    }
}
