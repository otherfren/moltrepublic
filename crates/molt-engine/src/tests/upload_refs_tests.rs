// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiki file references (`wiki_files_and_images.md` §3.2/§3.3): what an
//! `upload:<hex>` resolves to, where the bytes are, and the verified read.

use super::support::*;
use crate::upload_refs::{read_verified, ByteSource};
use crate::*;
use molt_core::{LocalCopy, MirrorJob, UploadView};
use serde_json::json;

fn one_of_three() -> GroupConfig {
    GroupConfig {
        threshold: 1,
        self_cosign: false,
        ..GroupConfig::demo()
    }
}

async fn resolve(w: &WalletHandle, hex: &str) -> Result<(Option<UploadView>, bool, bool, LocalCopy), MoltError> {
    match w.execute(Command::ResolveUpload { checksum: hex.to_string() }).await? {
        Reply::UploadResolved { upload, ambiguous, temporary, local } => Ok((upload, ambiguous, temporary, local)),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn persist(w: &WalletHandle, id: MessageId) {
    let pid = match w
        .execute(Command::Propose {
            surface: Surface::Files,
            payload: json!({"op": "persist", "id": id.to_string()}),
        })
        .await
        .expect("propose persist")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
}

/// A synthetic table row - the only way to put two files under ONE
/// 12-hex prefix (real sha256es never collide there).
fn row(tag: u8, checksum: &str, persistent: bool) -> UploadView {
    UploadView {
        id: MessageId([tag; 16]),
        member: "petra".to_string(),
        ts: 1_000 + u64::from(tag),
        name: format!("datei-{tag}.bin"),
        kind: "Image".to_string(),
        size: 40,
        available: true,
        expires_ts: 0,
        online: true,
        checksum: checksum.to_string(),
        download: None,
        availability: "relay-held".to_string(),
        persistent,
        mirrors: 0,
        mirror_held: 0,
        mirror_of: 4,
    }
}

fn job(count: u32, held: &[u32], complete: bool) -> MirrorJob {
    let mut j = MirrorJob {
        count,
        size: u64::from(count) * 40,
        root: String::new(),
        key: vec![7u8; 32],
        started_at: 1_000,
        held: Vec::new(),
        complete,
        bytes: 0,
    };
    for i in held {
        j.mark(*i);
    }
    j
}

/// A prefix names the persisted share; the own source file is its local
/// copy; before the vote the same share answers `temporary`; an unknown
/// prefix answers nothing; a malformed one is refused.
#[test]
fn a_reference_resolves_against_the_persistent_shares_first() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(one_of_three(), SessionView::default());
        let id = share_temp_file(&w, tmp.path(), "netz.png", b"not really a png").await;
        let full = match w.execute(Command::ReadUploads).await.expect("uploads") {
            Reply::Uploads { uploads } => uploads[0].checksum.clone(),
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(full.len(), 64);
        let prefix = &full[..12];

        let (up, amb, temp, local) = resolve(&w, prefix).await.expect("resolves");
        assert_eq!(up.as_ref().map(|u| u.id), Some(id), "a temporary match is still shown");
        assert!(temp, "…but flagged: the page waits for the persist vote");
        assert!(!amb);
        assert!(matches!(local, LocalCopy::Own { .. }), "the sharer has the bytes: {local:?}");

        persist(&w, id).await;
        let (up, amb, temp, local) = resolve(&w, &prefix.to_uppercase()).await.expect("resolves");
        assert_eq!(up.as_ref().map(|u| u.id), Some(id));
        assert!(!temp && !amb);
        assert_eq!(
            local,
            LocalCopy::Own { path: tmp.path().join("netz.png").display().to_string() }
        );
        let (up, ..) = resolve(&w, &full).await.expect("the full hash resolves too");
        assert_eq!(up.map(|u| u.id), Some(id));

        let (up, amb, temp, local) = resolve(&w, "000000000000").await.expect("unknown is an answer");
        assert!(up.is_none() && !amb && !temp);
        assert_eq!(local, LocalCopy::None);

        assert!(matches!(resolve(&w, "abc").await, Err(MoltError::BadPayload(_))), "too short");
        assert!(matches!(resolve(&w, "zzzzzzzzzzzz").await, Err(MoltError::BadPayload(_))), "not hex");
    });
}

/// Two persistent files under one prefix are SHOWN as ambiguous, never
/// guessed; one digit more separates them again.
#[test]
fn an_ambiguous_prefix_answers_nothing() {
    let st = plain_state();
    let a = row(1, &format!("abcdef012345{}", "0".repeat(52)), true);
    let b = row(2, &format!("abcdef012345{}", "1".repeat(52)), true);
    let rows = [a.clone(), b.clone()];

    let r = st.resolve_upload_in(&rows, "abcdef012345");
    assert!(r.ambiguous, "two distinct files carry the prefix");
    assert!(r.upload.is_none() && !r.temporary);
    assert_eq!(r.local, LocalCopy::None);

    let r = st.resolve_upload_in(&rows, "abcdef0123450");
    assert!(!r.ambiguous);
    assert_eq!(r.upload.map(|u| u.id), Some(a.id));

    // the SAME file listed twice is one file, not an ambiguity
    let twice = [a.clone(), row(3, &a.checksum, true)];
    let r = st.resolve_upload_in(&twice, "abcdef012345");
    assert!(!r.ambiguous);
    assert_eq!(r.upload.map(|u| u.id), Some(a.id));
}

/// A mirror job answers its progress until it is complete, and only a
/// complete one is a byte source.
#[test]
fn a_mirror_job_answers_partial_until_it_is_complete() {
    let mut st = plain_state();
    let u = row(9, &format!("abcdef012345{}", "0".repeat(52)), true);

    st.files.mirror.jobs.insert(u.id.to_string(), job(4, &[0, 1], false));
    assert_eq!(st.local_copy_of(&u), LocalCopy::Partial { held: 2, of: 4 });
    assert!(
        st.byte_source(&u, &LocalCopy::Partial { held: 2, of: 4 }).is_err(),
        "a partial mirror is not a source"
    );

    st.files.mirror.jobs.insert(u.id.to_string(), job(4, &[0, 1, 2, 3], true));
    assert_eq!(st.local_copy_of(&u), LocalCopy::Mirrored);
    // no workspace open: the folder is unknown, so the source refuses
    assert!(st.byte_source(&u, &LocalCopy::Mirrored).is_err());

    // an own path and a registry path both read as a plain file
    let own = LocalCopy::Own { path: "/tmp/a.png".to_string() };
    assert_eq!(
        st.byte_source(&u, &own).expect("own"),
        ByteSource::File(std::path::PathBuf::from("/tmp/a.png"))
    );
    let dl = LocalCopy::Downloaded { path: "/tmp/b.png".to_string() };
    assert_eq!(
        st.byte_source(&u, &dl).expect("downloaded"),
        ByteSource::File(std::path::PathBuf::from("/tmp/b.png"))
    );
    assert!(st.byte_source(&u, &LocalCopy::None).is_err());
}

/// §3.3/§4: the mirror's sealed pieces assemble back to the exact
/// plaintext, and a tampered piece answers a mismatch - never bytes.
#[test]
fn the_mirror_pieces_assemble_and_a_tampered_one_is_refused() {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path().join("series");
    std::fs::create_dir_all(&dir).expect("series dir");
    let key = [7u8; 32];
    let len = molt_net::file_plane::PIECE_PAYLOAD_LEN + 5;
    let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    for (index, slice) in bytes.chunks(molt_net::file_plane::PIECE_PAYLOAD_LEN).enumerate() {
        let index = u32::try_from(index).expect("index");
        let sealed = molt_net::file_plane::seal_piece(&key, index, 2, slice).expect("seal");
        std::fs::write(dir.join(index.to_string()), sealed).expect("store piece");
    }
    let want = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let src = ByteSource::Pieces {
        dir: dir.clone(),
        key,
        count: 2,
        size: u64::try_from(bytes.len()).expect("len"),
    };

    match read_verified(&src, &want, 32 << 20).expect("the pieces assemble") {
        Reply::UploadBytes { bytes: got } => assert_eq!(got, bytes),
        other => panic!("unexpected: {other:?}"),
    }
    // a foreign key opens nothing
    let wrong = ByteSource::Pieces { dir: dir.clone(), key: [8u8; 32], count: 2, size: u64::try_from(bytes.len()).expect("len") };
    assert!(read_verified(&wrong, &want, 32 << 20).is_err());

    // a piece resealed with other content: the re-hash refuses
    let tampered = molt_net::file_plane::seal_piece(&key, 1, 2, b"other").expect("seal");
    std::fs::write(dir.join("1"), tampered).expect("overwrite piece");
    let err = read_verified(&src, &want, 32 << 20).expect_err("the re-hash refuses");
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
}

/// A plain file source is capped and re-hashed too.
#[test]
fn a_file_source_is_capped_and_re_hashed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("bild.png");
    std::fs::write(&path, b"not really a png").expect("write");
    let want = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"not really a png"));
    let src = ByteSource::File(path.clone());

    match read_verified(&src, &want, 1 << 20).expect("reads") {
        Reply::UploadBytes { bytes } => assert_eq!(bytes, b"not really a png"),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(read_verified(&src, &want, 4).is_err(), "the cap refuses before the read");
    std::fs::write(&path, b"swapped").expect("swap");
    let err = read_verified(&src, &want, 1 << 20).expect_err("the swap is caught");
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
    assert!(read_verified(&ByteSource::File(tmp.path().join("weg.png")), &want, 1 << 20).is_err());
}

/// Q5: the registry remembers a VERIFIED download by CONTENT, so a reopen
/// still finds the bytes; a copy that is gone drops its entry at read time.
#[test]
fn a_verified_download_survives_a_reopen_and_a_deleted_copy_is_forgotten() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let w = __spawn_sim_founding(GroupConfig::demo(), storage_session(&tmp), true);
        w.execute(Command::CreateStart {
            name: "Bildarchiv".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let ws = read_session(&w).await.active_workspace.clone();
        let root = tmp.path().join("workspaces");
        let dir = molt_storage::find_workspace_dir(&root, &ws).expect("workspace dir");

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).expect("src dir");
        let id = share_temp_file(&w, &src, "netz.png", b"not really a png").await;
        let full = match w.execute(Command::ReadUploads).await.expect("uploads") {
            Reply::Uploads { uploads } => uploads[0].checksum.clone(),
            other => panic!("unexpected: {other:?}"),
        };
        let dl = tmp.path().join("dl");
        std::fs::create_dir_all(&dl).expect("dl dir");
        w.execute(Command::DownloadFile { id, dest: Some(dl.display().to_string()) })
            .await
            .expect("download");
        await_file(&dl.join("netz.png"), b"not really a png").await;
        let copy = dl.join("netz.png").display().to_string();

        // the own source answers first while it is there
        let (_, _, _, local) = resolve(&w, &full).await.expect("resolves");
        assert!(matches!(local, LocalCopy::Own { .. }), "{local:?}");

        // …gone, the verified download answers
        std::fs::remove_file(src.join("netz.png")).expect("remove the source");
        let (_, _, _, local) = resolve(&w, &full).await.expect("resolves");
        assert_eq!(local, LocalCopy::Downloaded { path: copy.clone() });

        // …and it survives the reopen (the LIVE download map does not)
        w.execute(Command::CloseWorkspace).await.expect("close");
        {
            let stored = reopen(&dir);
            assert_eq!(stored.prefs.local_copies.get(&full), Some(&copy), "the registry persisted");
        }
        w.execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("reopen");
        let (_, _, _, local) = resolve(&w, &full).await.expect("resolves after the reopen");
        assert_eq!(local, LocalCopy::Downloaded { path: copy });

        // the copy is deleted: the answer falls back and the entry goes
        std::fs::remove_file(dl.join("netz.png")).expect("remove the copy");
        let (_, _, _, local) = resolve(&w, &full).await.expect("resolves");
        assert_eq!(local, LocalCopy::None);
        w.execute(Command::CloseWorkspace).await.expect("close 2");
        let stored = reopen(&dir);
        assert!(stored.prefs.local_copies.is_empty(), "the stale entry left the registry");
    });
}

/// `ReadUploadBytes` refuses what it cannot answer honestly - and never
/// with bytes.
#[test]
fn read_upload_bytes_refuses_before_it_reads() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(one_of_three(), SessionView::default());
        let id = share_temp_file(&w, tmp.path(), "netz.png", b"not really a png").await;
        persist(&w, id).await;
        let full = match w.execute(Command::ReadUploads).await.expect("uploads") {
            Reply::Uploads { uploads } => uploads[0].checksum.clone(),
            other => panic!("unexpected: {other:?}"),
        };

        let bytes = match w
            .execute(Command::ReadUploadBytes { checksum: full[..12].to_string(), cap: 1 << 20 })
            .await
            .expect("a prefix reads the resolved file")
        {
            Reply::UploadBytes { bytes } => bytes,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(bytes, b"not really a png");

        let err = w
            .execute(Command::ReadUploadBytes { checksum: full.clone(), cap: 4 })
            .await
            .expect_err("the cap refuses first");
        assert!(err.to_string().contains("cap"), "{err}");

        let err = w
            .execute(Command::ReadUploadBytes { checksum: "0".repeat(64), cap: 1 << 20 })
            .await
            .expect_err("an unknown reference is not bytes");
        assert!(err.to_string().contains("unknown"), "{err}");

        assert!(
            w.execute(Command::ReadUploadBytes { checksum: "abc".to_string(), cap: 1 << 20 })
                .await
                .is_err(),
            "a malformed prefix is refused"
        );

        // the file is swapped behind the share: a placeholder, never a picture
        std::fs::write(tmp.path().join("netz.png"), b"a different file").expect("swap");
        let err = w
            .execute(Command::ReadUploadBytes { checksum: full, cap: 1 << 20 })
            .await
            .expect_err("the re-hash refuses");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    });
}
