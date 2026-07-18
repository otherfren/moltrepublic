// SPDX-License-Identifier: GPL-3.0-or-later

//! S6 at-rest sealing (`documents/backup_restore_design.md` §5): the
//! recovery phrase is the credential; **sealed = no key material on disk**.
//!
//! The workspace key is HKDF-derivable from the phrase + the plaintext
//! manifest id, so sealing does not store a phrase-encrypted key copy —
//! it *removes* `keys/workspace.key` and `keys/seed.sealed` and marks the
//! manifest (`[crypto] sealed = "phrase"`, version raised to
//! `STORAGE_VERSION_SEALED` so older binaries refuse politely instead of
//! tripping over the keyless dir). Unsealing derives the key from the
//! typed phrase and **verifies it against the encrypted genesis frame**
//! (the Poly1305 tag is the real phrase check — a wrong phrase is a hard
//! error that changes nothing on disk), then re-seals the key material
//! under the local device key and restores the version floor.
//!
//! Verification-oracle note (design §5.1): deriving-and-trying against the
//! genesis frame exposes exactly the brute-force interface the AEAD already
//! gives an attacker holding the directory — nothing new. The phrase carries
//! 256-bit entropy, which is also why no memory-hard KDF is needed here
//! (`[crypto].kdf = "argon2id"` stays reserved for a possible future
//! weak-password mode, deliberately not built).
//!
//! Both operations take the workspace flock for their duration, so an OPEN
//! workspace (this process or another) can never be sealed or unsealed from
//! under its writer. Both verify the phrase BEFORE touching any file:
//! sealing must prove the caller holds the credential before deleting their
//! only other way in (encrypt-requires-phrase-proof).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use molt_core::{
    WorkspaceManifest, SEALED_DEVICE, SEALED_PHRASE, STORAGE_VERSION, STORAGE_VERSION_PRUNED,
    STORAGE_VERSION_SEALED,
};
use zeroize::Zeroizing;

use crate::StorageError;

/// Whether a manifest marks its directory as phrase-sealed at rest. Derived
/// from the directory (not from any session flag), so the state survives
/// restarts — `scan_workspaces` reports it through this one predicate.
pub fn is_sealed(manifest: &WorkspaceManifest) -> bool {
    manifest.crypto.sealed == SEALED_PHRASE
}

/// Seal a closed workspace at rest under its recovery phrase.
///
/// Verifies the phrase first (BIP-39 checksum, then an authenticated
/// decrypt of the genesis frame with the derived workspace key) — only
/// then removes the device-sealed key material (best-effort overwrite,
/// then unlink), marks the manifest `sealed = "phrase"` and raises its
/// version to [`STORAGE_VERSION_SEALED`]. Idempotent: re-sealing an
/// already-sealed directory (e.g. after a crash between the deletion and
/// the manifest write) converges to the sealed state.
///
/// Blocking (file I/O under the workspace flock) — call it off-actor or
/// from a synchronous handler that accepts the few file operations; the
/// crypto itself is one HKDF + one AEAD open (no Argon2, see module doc).
pub fn seal_at_rest(ws_dir: &Path, phrase: &str) -> Result<(), StorageError> {
    // cheap pre-checks before the LOCK is even created: is this a
    // workspace at all, and does the phrase pass its BIP-39 checksum
    // (typos fail here, before any file is touched)
    crate::read_manifest(ws_dir)?;
    let seed = Zeroizing::new(crate::seed_entropy(phrase)?);
    // hold the flock: an OPEN workspace must never lose its keys mid-run
    let _lock = crate::acquire_lock(ws_dir)?;
    // (re)read the manifest UNDER the lock — a copy taken before it could
    // be stale (a concurrently-closing engine's rename or version bump)
    // and writing it back below would silently revert those updates
    let mut manifest = crate::read_manifest(ws_dir)?;
    if manifest.version > STORAGE_VERSION_SEALED {
        return Err(StorageError::NewerVersion(manifest.version));
    }
    let id = crate::id_bytes(&manifest.workspace.id)?;
    let key = Zeroizing::new(crate::derive_workspace_key(&seed, &manifest.workspace.id));
    // proof-of-credential: the caller must hold the phrase BEFORE we delete
    // their only other way in
    verify_phrase_key(ws_dir, &id, &key)?;
    // point of no return: remove the device-sealed key material. Deletion
    // first, manifest second — a crash in between leaves a dir whose marker
    // ("device") and key files disagree, which `open_workspace` reports as
    // honest corruption and `unseal_at_rest` repairs (it rewrites both).
    secure_remove(&ws_dir.join(&manifest.crypto.key_file))?;
    secure_remove(&ws_dir.join("keys").join("seed.sealed"))?;
    // the materialized logo is republic CONTENT in plaintext (an applied
    // org image, mirrored out of the encrypted log for display) — it must
    // not stay readable in a sealed dir. Deleting it is safe: the log
    // replays it and `sync_logo_file` rebuilds the file at the next open
    // after a decrypt. The manifest (name, m/n) deliberately stays — the
    // Open screen lists sealed workspaces by their identity card.
    remove_logo_files(ws_dir)?;
    manifest.crypto.sealed = SEALED_PHRASE.to_string();
    manifest.version = manifest.version.max(STORAGE_VERSION_SEALED);
    crate::write_manifest(ws_dir, &manifest)
}

/// Unseal a phrase-sealed workspace: derive the workspace key from the
/// typed phrase, verify it against the encrypted genesis (wrong phrase =
/// hard [`StorageError::Crypto`], nothing changed on disk), re-seal
/// `keys/workspace.key` + `keys/seed.sealed` under the local device key,
/// set `sealed = "device"` and restore the manifest version floor
/// (pruned chain present → [`STORAGE_VERSION_PRUNED`], else
/// [`STORAGE_VERSION`]) so older binaries can open it again.
///
/// Also the repair path for a crash window that left the marker and the
/// key files disagreeing — it rewrites both from the verified phrase.
pub fn unseal_at_rest(root: &Path, ws_dir: &Path, phrase: &str) -> Result<(), StorageError> {
    // cheap pre-checks before the LOCK is created (see seal_at_rest)
    crate::read_manifest(ws_dir)?;
    let seed = Zeroizing::new(crate::seed_entropy(phrase)?);
    let _lock = crate::acquire_lock(ws_dir)?;
    // (re)read under the lock — the pre-lock copy could be stale and its
    // write-back below would revert concurrent manifest updates
    let mut manifest = crate::read_manifest(ws_dir)?;
    if manifest.version > STORAGE_VERSION_SEALED {
        return Err(StorageError::NewerVersion(manifest.version));
    }
    let id = crate::id_bytes(&manifest.workspace.id)?;
    let key = Zeroizing::new(crate::derive_workspace_key(&seed, &manifest.workspace.id));
    // the Poly1305 tag of the genesis frame IS the real phrase verification
    verify_phrase_key(ws_dir, &id, &key)?;
    // re-seal the key material under the local device key (keys first,
    // manifest second: a crash in between leaves the dir marked sealed
    // with keys present — a second decrypt converges it)
    let device_key = crate::load_or_create_device_key(&crate::device_key_path(root))?;
    let sealed_key = crate::seal_workspace_key(&device_key, &id, &key)?;
    crate::write_atomic(ws_dir, "keys/workspace.key", &sealed_key, true)?;
    let sealed_seed = crate::seal_seed_entropy(&device_key, &id, &seed)?;
    crate::write_atomic(ws_dir, "keys/seed.sealed", &sealed_seed, true)?;
    manifest.version = chain_version_floor(ws_dir, &key, &id);
    manifest.crypto.sealed = SEALED_DEVICE.to_string();
    crate::write_manifest(ws_dir, &manifest)
}

/// Authenticated decrypt of the genesis frame (segment 1, seq 1) with the
/// phrase-derived key — succeeds iff the phrase is THIS workspace's.
fn verify_phrase_key(ws_dir: &Path, id: &[u8; 32], key: &[u8; 32]) -> Result<(), StorageError> {
    let data = fs::read(ws_dir.join("log").join(crate::segment_name(1)))?;
    let (frames, _torn) = crate::split_frames(&data);
    let first = frames
        .first()
        .ok_or_else(|| StorageError::Corrupt("workspace has no genesis frame".to_string()))?;
    crate::decrypt_frame(key, id, 1, 1, first.nonce, first.ciphertext)
        .map(Zeroizing::new) // wipe the decrypted genesis on drop
        .map_err(|_| {
            StorageError::Crypto("the recovery phrase does not match this workspace".to_string())
        })?;
    Ok(())
}

/// The manifest version floor a decrypted workspace returns to, decided by
/// `chain.state`:
///
/// * absent → [`STORAGE_VERSION`] (pre-chain / freshly founded);
/// * readable FULL chain → [`STORAGE_VERSION`] (old binaries read it fine);
/// * readable PRUNED chain → [`STORAGE_VERSION_PRUNED`] (the WP4b gate);
/// * **present but unreadable** (torn, tampered, newer layout) →
///   [`STORAGE_VERSION_PRUNED`], the CONSERVATIVE answer: the file might be
///   a pruned chain, and dropping the floor would let a pre-pruning binary
///   run a pruned republic chainless — the state fork the version gate
///   exists to hard-stop. Over-describing only costs an old binary a
///   polite refusal.
fn chain_version_floor(ws_dir: &Path, key: &[u8; 32], id: &[u8; 32]) -> u32 {
    let data = match fs::read(ws_dir.join("chain.state")) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return STORAGE_VERSION,
        Err(_) => return STORAGE_VERSION_PRUNED,
    };
    let (frames, torn) = crate::split_frames(&data);
    if frames.len() != 1 || torn.is_some() {
        return STORAGE_VERSION_PRUNED;
    }
    let chain_key = Zeroizing::new(crate::hkdf32(key, "molt-chain-state", id));
    let Ok(plaintext) = crate::decrypt_frame(
        &chain_key,
        id,
        crate::CHAIN_SEGMENT,
        0,
        frames[0].nonce,
        frames[0].ciphertext,
    )
    .map(Zeroizing::new) else {
        return STORAGE_VERSION_PRUNED;
    };
    match serde_json::from_slice::<crate::ChainStateFile>(&plaintext) {
        Ok(crate::ChainStateFile::Pruned { .. }) | Err(_) => STORAGE_VERSION_PRUNED,
        Ok(crate::ChainStateFile::Full(_)) => STORAGE_VERSION,
    }
}

/// Remove every materialized `logo.*` file (plaintext republic content
/// mirrored out of the encrypted log; rebuilt by `sync_logo_file` at the
/// next open after a decrypt).
fn remove_logo_files(ws_dir: &Path) -> Result<(), StorageError> {
    let Ok(rd) = fs::read_dir(ws_dir) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("logo.") {
            secure_remove(&entry.path())?;
        }
    }
    Ok(())
}

/// Best-effort overwrite-then-unlink (design §5.1). Honest limits: on
/// modern filesystems/SSDs (journaling, wear leveling, snapshots) the
/// overwrite is not guaranteed to reach the old blocks — the threat model
/// is the synced/backed-up workspace directory, same as the rest of
/// storage. A missing file is fine (idempotent re-seal after a crash
/// window); a file that cannot be REMOVED is a hard error (key material
/// would stay behind while the manifest claims sealed).
fn secure_remove(path: &Path) -> Result<(), StorageError> {
    match fs::metadata(path) {
        Ok(md) => {
            if let Ok(mut f) = OpenOptions::new().write(true).open(path) {
                let len = usize::try_from(md.len()).unwrap_or(0).min(1 << 20);
                let _ = f.write_all(&vec![0u8; len]);
                let _ = f.sync_all();
            }
            fs::remove_file(path)?;
            // make the unlink itself durable BEFORE the manifest is marked
            // sealed: without the parent-dir fsync a power loss could keep
            // the durably-sealed marker while resurrecting the key file —
            // key material riding along in every "sealed" backup, and no
            // path that ever notices (same rule as write_atomic's rename)
            if let Some(parent) = path.parent() {
                if let Ok(d) = fs::File::open(parent) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Tests (design §8.2 — the red anchors of story 10)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        create_workspace, open_workspace, peek_genesis, read_manifest, read_sealed_seed,
        scan_workspaces, seed_entropy, StorageError,
    };
    use molt_core::{EventEnvelope, WorkspaceEvent};
    use std::path::PathBuf;

    fn founded() -> EventEnvelope {
        EventEnvelope {
            seq: 1,
            ts: 42,
            by: "petra".to_string(),
            body: WorkspaceEvent::Founded {
                name: "Sealed Club".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "petra".to_string(),
                roster: vec!["petra".to_string(), "juno".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: "keep secrets".to_string(),
            },
        }
    }

    /// Create a workspace under `root` and return `(dir, phrase, id)`.
    fn make_ws(root: &Path) -> (PathBuf, String, String) {
        let phrase = crate::generate_seed_phrase().expect("gen");
        let seed = seed_entropy(&phrase).expect("entropy");
        let ws = create_workspace(root, &seed, &founded()).expect("create");
        let dir = ws.dir().to_path_buf();
        let id = ws.manifest.workspace.id.clone();
        drop(ws); // release the LOCK
        (dir, phrase, id)
    }

    /// The at-rest-relevant bytes of a workspace dir (manifest, key files,
    /// log), for "nothing changed" assertions. The LOCK is excluded on
    /// purpose: taking the flock rewrites its pid note.
    fn state_fingerprint(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for rel in ["manifest.toml", "keys/workspace.key", "keys/seed.sealed", "log/000001.mlog"] {
            out.push((rel.to_string(), fs::read(dir.join(rel)).unwrap_or_default()));
        }
        out
    }

    #[test]
    fn seal_removes_key_material_and_scan_reports_encrypted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, id) = make_ws(&root);
        assert!(dir.join("keys/workspace.key").exists());
        assert!(dir.join("keys/seed.sealed").exists());

        seal_at_rest(&dir, &phrase).expect("seal");

        // design §8.2 test 4: NO key material while sealed
        assert!(!dir.join("keys/workspace.key").exists(), "workspace.key removed");
        assert!(!dir.join("keys/seed.sealed").exists(), "seed.sealed removed");
        assert_eq!(read_sealed_seed(&root, &dir, &id), None, "no phrase to show");
        assert!(peek_genesis(&root, &dir, &id).is_none(), "no roster to peek");

        // the marker + version gate are on disk
        let m = read_manifest(&dir).expect("manifest");
        assert!(is_sealed(&m));
        assert_eq!(m.version, STORAGE_VERSION_SEALED);

        // design §8.2 test 1: the state is derived from the directory — a
        // FRESH scan (≈ restart) reports it, no session memory involved
        let entries = scan_workspaces(&root);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].info().encrypted, "scan derives encrypted from the dir");

        // …and open refuses with the TYPED sealed error (mapped to
        // MoltError::WorkspaceEncrypted, so every frontend routes it to
        // the decrypt flow even when its session flag is stale)
        match open_workspace(&dir) {
            Err(StorageError::Sealed(sealed_id)) => assert_eq!(sealed_id, id),
            other => panic!("expected Sealed, got {:?}", other.map(|_| ())),
        }
    }

    /// Story 9 × story 10: exporting a phrase-sealed dir refuses on the
    /// marker with the TYPED sealed error (design §5 — the blob would need
    /// the phrase-derived key, which is not on disk).
    #[test]
    fn a_sealed_dir_refuses_export_honestly() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, id) = make_ws(&root);
        seal_at_rest(&dir, &phrase).expect("seal");
        let mut out = Vec::new();
        match crate::export::export_dir(
            &root,
            &dir,
            &crate::export::ExportKey::Passphrase(zeroize::Zeroizing::new(
                "long enough passphrase".to_string(),
            )),
            &mut out,
        ) {
            Err(StorageError::Sealed(sealed_id)) => assert_eq!(sealed_id, id),
            other => panic!("expected Sealed, got {other:?}"),
        }
        assert!(out.is_empty(), "not a single blob byte written");
    }

    /// The materialized plaintext logo is republic content — it must not
    /// stay readable in a sealed dir (it is rebuilt from the log at the
    /// next open after a decrypt).
    #[test]
    fn sealing_removes_the_plaintext_logo() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        fs::write(dir.join("logo.png"), b"the republic's face").expect("logo");
        seal_at_rest(&dir, &phrase).expect("seal");
        assert!(!dir.join("logo.png").exists(), "content removed while sealed");
    }

    /// A damaged (present-but-unreadable) chain.state must keep the PRUNED
    /// floor on unseal: the file might be a pruned chain, and dropping to
    /// the base version would let a pre-pruning binary run the republic
    /// chainless — over-describing only costs an old binary a refusal.
    #[test]
    fn a_damaged_chain_state_keeps_the_conservative_pruned_floor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        seal_at_rest(&dir, &phrase).expect("seal");
        fs::write(dir.join("chain.state"), b"torn garbage").expect("damage");
        unseal_at_rest(&root, &dir, &phrase).expect("unseal");
        assert_eq!(
            read_manifest(&dir).expect("m").version,
            STORAGE_VERSION_PRUNED,
            "unreadable chain.state must not drop the version floor"
        );
    }

    #[test]
    fn seal_with_wrong_phrase_is_refused_and_deletes_nothing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, _phrase, _id) = make_ws(&root);
        let before = state_fingerprint(&dir);

        // design §8.2 test 6: encrypt requires proof — a VALID BIP-39
        // phrase that is not this workspace's is refused
        let foreign = crate::generate_seed_phrase().expect("gen");
        match seal_at_rest(&dir, &foreign) {
            Err(StorageError::Crypto(_)) => {}
            other => panic!("expected Crypto, got {other:?}"),
        }
        assert_eq!(state_fingerprint(&dir), before, "nothing deleted, nothing changed");
        assert!(!is_sealed(&read_manifest(&dir).expect("manifest")));

        // a typo'd phrase fails the BIP-39 checksum before any crypto
        match seal_at_rest(&dir, "amber basalt cedar") {
            Err(StorageError::BadSeed(_)) => {}
            other => panic!("expected BadSeed, got {other:?}"),
        }
        assert_eq!(state_fingerprint(&dir), before);
    }

    #[test]
    fn unseal_with_wrong_phrase_is_a_hard_error_and_stays_sealed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        seal_at_rest(&dir, &phrase).expect("seal");
        let before = state_fingerprint(&dir);

        // design §8.2 test 2: wrong (valid BIP-39) phrase → Crypto error,
        // dir unchanged
        let foreign = crate::generate_seed_phrase().expect("gen");
        match unseal_at_rest(&root, &dir, &foreign) {
            Err(StorageError::Crypto(_)) => {}
            other => panic!("expected Crypto, got {other:?}"),
        }
        assert_eq!(state_fingerprint(&dir), before, "still sealed, nothing changed");
        assert!(is_sealed(&read_manifest(&dir).expect("manifest")));

        // typo'd phrase → BIP-39 checksum error before any file is touched
        match unseal_at_rest(&root, &dir, "amber basalt cedar") {
            Err(StorageError::BadSeed(_)) => {}
            other => panic!("expected BadSeed, got {other:?}"),
        }
        assert_eq!(state_fingerprint(&dir), before);
    }

    #[test]
    fn unseal_with_the_right_phrase_restores_keys_floor_and_open() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, id) = make_ws(&root);
        seal_at_rest(&dir, &phrase).expect("seal");

        // design §8.2 test 3
        unseal_at_rest(&root, &dir, &phrase).expect("unseal");
        let m = read_manifest(&dir).expect("manifest");
        assert!(!is_sealed(&m));
        assert_eq!(m.version, STORAGE_VERSION, "version floor restored");
        // the key material is back and device-unsealable
        assert_eq!(read_sealed_seed(&root, &dir, &id).as_deref(), Some(phrase.as_str()));
        let genesis = peek_genesis(&root, &dir, &id).expect("peek");
        assert!(matches!(genesis.body, WorkspaceEvent::Founded { ref name, .. } if name == "Sealed Club"));
        // …and a full open replays the history
        let (ws, loaded) = open_workspace(&dir).expect("open");
        assert_eq!(ws.next_seq, 2);
        assert_eq!(loaded.tail.len(), 1);
        // a fresh scan agrees
        assert!(!scan_workspaces(&root)[0].info().encrypted);
    }

    #[test]
    fn seal_and_unseal_preserve_the_pruned_version_floor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        // prune the chain through the real writer wiring (raises to v2)
        let (ws, _loaded) = open_workspace(&dir).expect("open");
        let blob = molt_core::CheckpointState {
            founding_name: "t".to_string(),
            rule_m: 1,
            rule_n: 1,
            founding_identities: Vec::new(),
            agenda: String::new(),
            republic_id: String::new(),
            roster: Vec::new(),
            applied: Vec::new(),
            consumed_ids: Vec::new(),
            upto: 0,
        };
        let handle = crate::start_writer(ws);
        handle.persist_chain_blocking(Some(blob), Vec::new());
        handle.close(None);
        assert_eq!(read_manifest(&dir).expect("m").version, STORAGE_VERSION_PRUNED);

        seal_at_rest(&dir, &phrase).expect("seal");
        assert_eq!(read_manifest(&dir).expect("m").version, STORAGE_VERSION_SEALED);
        unseal_at_rest(&root, &dir, &phrase).expect("unseal");
        // the floor comes back as PRUNED, not base — an old binary must
        // still refuse to run chainless on the pruned view
        assert_eq!(read_manifest(&dir).expect("m").version, STORAGE_VERSION_PRUNED);
        assert!(open_workspace(&dir).is_ok(), "this build reopens it fine");
    }

    #[test]
    fn a_locked_open_workspace_refuses_sealing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        let (_ws, _loaded) = open_workspace(&dir).expect("open holds the LOCK");
        match seal_at_rest(&dir, &phrase) {
            Err(StorageError::Busy(_)) => {}
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    /// Design §8.2 test 7 (golden-fixture style): the sealed manifest sits
    /// at [`STORAGE_VERSION_SEALED`]; an older reader — whose gate is
    /// `version > its own max` — refuses politely. This build's own gate
    /// demonstrates the mechanism one version up.
    #[test]
    fn the_version_gate_stops_older_readers_politely() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        seal_at_rest(&dir, &phrase).expect("seal");
        // sealing must be above every version an older binary accepts
        const {
            assert!(STORAGE_VERSION_SEALED > STORAGE_VERSION_PRUNED);
        }
        // simulate a manifest one capability ahead of THIS build
        let mut m = read_manifest(&dir).expect("m");
        m.version = STORAGE_VERSION_SEALED + 1;
        let text = toml::to_string_pretty(&m).expect("render");
        fs::write(dir.join("manifest.toml"), text).expect("write");
        match open_workspace(&dir) {
            Err(StorageError::NewerVersion(v)) => assert_eq!(v, STORAGE_VERSION_SEALED + 1),
            other => panic!("expected NewerVersion, got {:?}", other.map(|_| ())),
        }
        match seal_at_rest(&dir, &phrase) {
            Err(StorageError::NewerVersion(_)) => {}
            other => panic!("sealing a too-new dir must refuse, got {other:?}"),
        }
        match unseal_at_rest(&root, &dir, &phrase) {
            Err(StorageError::NewerVersion(_)) => {}
            other => panic!("unsealing a too-new dir must refuse, got {other:?}"),
        }
    }

    /// Zero-migration: a pre-S6 manifest (no `sealed` field at all) parses
    /// as device-sealed, scans as unencrypted and opens unchanged.
    #[test]
    fn old_unsealed_dirs_keep_working_untouched() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, _phrase, _id) = make_ws(&root);
        // strip the `sealed` line: the manifest a pre-S6 binary wrote
        let text = fs::read_to_string(dir.join("manifest.toml")).expect("read");
        assert!(text.contains("sealed"), "new manifests carry the marker");
        let stripped: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("sealed"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join("manifest.toml"), stripped).expect("write");

        let m = read_manifest(&dir).expect("manifest");
        assert_eq!(m.crypto.sealed, SEALED_DEVICE, "serde default = device");
        assert!(!is_sealed(&m));
        assert_eq!(m.version, STORAGE_VERSION, "version untouched — no gate change");
        assert!(!scan_workspaces(&root)[0].info().encrypted);
        assert!(open_workspace(&dir).is_ok(), "opens exactly as before");
    }

    /// The crash window between key deletion and the manifest write leaves
    /// marker and key files disagreeing: open reports honest corruption
    /// (never a guess), unseal repairs.
    #[test]
    fn a_half_sealed_dir_is_honest_corruption_and_unseal_repairs_it() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().join("workspaces");
        let (dir, phrase, _id) = make_ws(&root);
        // simulate the crash: keys gone, manifest still says "device"
        fs::remove_file(dir.join("keys/workspace.key")).expect("rm key");
        fs::remove_file(dir.join("keys/seed.sealed")).expect("rm seed");
        match open_workspace(&dir) {
            Err(StorageError::BadFile(msg)) => {
                assert!(msg.contains("key material"), "honest reason, got: {msg}")
            }
            other => panic!("expected BadFile, got {:?}", other.map(|_| ())),
        }
        // the phrase repairs the dir either way
        unseal_at_rest(&root, &dir, &phrase).expect("repair");
        assert!(open_workspace(&dir).is_ok());
    }
}
