// SPDX-License-Identifier: GPL-3.0-or-later

//! The byte doors (`docs_archive/security/mcp-security.md`, "The byte
//! doors") through the command surface, on a workspace that is on disk:
//! an upload into the store, its share, the reads by id and by key.

use super::support::*;
use crate::*;
use base64::Engine as _;
use serde_json::json;

/// A founded 2-of-3 republic on disk: the handle, its directory, its id.
async fn stored(tmp: &tempfile::TempDir) -> (WalletHandle, std::path::PathBuf, String) {
    let w = __spawn_sim_founding(GroupConfig::demo(), storage_session(tmp), true);
    w.execute(Command::CreateStart {
        name: "Store".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("finish");
    let id = read_session(&w).await.active_workspace.clone();
    let root = tmp.path().join("workspaces");
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("the workspace is on disk");
    (w, dir, id)
}

async fn begin(w: &WalletHandle, name: &str, size: usize) -> Result<String, MoltError> {
    let size = u64::try_from(size).expect("small");
    match w.execute(Command::UploadBegin { name: name.to_string(), size }).await? {
        Reply::UploadProgress { upload, received: 0, done: false, .. } => Ok(upload),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn chunk(w: &WalletHandle, upload: &str, offset: usize, bytes: &[u8], last: bool) -> Result<Reply, MoltError> {
    w.execute(Command::UploadChunk {
        upload: upload.to_string(),
        offset: u64::try_from(offset).expect("small"),
        bytes: bytes.to_vec(),
        last,
    })
    .await
}

/// A whole upload, in pieces of `step` bytes.
async fn upload(w: &WalletHandle, name: &str, bytes: &[u8], step: usize) -> String {
    let id = begin(w, name, bytes.len()).await.expect("begin");
    let mut offset = 0;
    loop {
        let end = (offset + step).min(bytes.len());
        let last = end == bytes.len();
        match chunk(w, &id, offset, &bytes[offset..end], last).await.expect("chunk") {
            Reply::UploadProgress { received, done, .. } => {
                assert_eq!(usize::try_from(received).ok(), Some(end));
                assert_eq!(done, last);
            }
            other => panic!("unexpected: {other:?}"),
        }
        if last {
            return id;
        }
        offset = end;
    }
}

async fn bytes_of(w: &WalletHandle, cmd: Command) -> Result<Vec<u8>, MoltError> {
    match w.execute(cmd).await? {
        Reply::UploadBytes { bytes } => Ok(bytes),
        other => panic!("unexpected: {other:?}"),
    }
}

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

async fn await_gone(path: &std::path::Path) {
    for _ in 0..500 {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} is still there", path.display());
}

/// The share message for `name`, once the off-actor hash posted it.
async fn await_share(w: &WalletHandle, name: &str) -> MessageId {
    for _ in 0..500 {
        let snap = read_surface(w, Surface::Chat).await;
        if let Some(row) = snap.applied.iter().find(|m| m["file"]["name"] == json!(name)) {
            return msg_id(row);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the share for {name} never posted");
}

/// The keystone: bytes go in as chunks, land whole in the workspace's
/// store, become a share under the file's own name, read back by the
/// share's id - and leave the disk with the share.
#[test]
fn an_upload_becomes_a_share_reads_back_by_id_and_leaves_with_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        let content: Vec<u8> = (0..=u8::MAX).cycle().take(1000).collect();
        let up = upload(&w, "notes.txt", &content, 300).await;
        let file = dir.join("uploads").join(&up).join("notes.txt");
        assert_eq!(std::fs::read(&file).expect("the file landed"), content);

        let promised = match w
            .execute(Command::ShareUpload { upload: up.clone(), channel: molt_core::ChannelRef::default() })
            .await
            .expect("share")
        {
            Reply::Sent { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(await_share(&w, "notes.txt").await, promised);
        // the upload is the share's now: no second share, no abort
        let again = w
            .execute(Command::ShareUpload { upload: up.clone(), channel: molt_core::ChannelRef::default() })
            .await
            .expect_err("shared once");
        assert!(again.to_string().contains("unknown id"), "{again}");
        assert!(w.execute(Command::UploadAbort { upload: up.clone() }).await.is_err());
        assert!(file.exists(), "a refused abort removes nothing");

        // phase two of a download: the bytes by the share's id
        let got = bytes_of(&w, Command::ReadFileBytes { id: promised, cap: 1 << 20 }).await.expect("read");
        assert_eq!(got, content);
        let over = bytes_of(&w, Command::ReadFileBytes { id: promised, cap: 999 }).await.expect_err("the cap");
        assert!(over.to_string().contains("cap"), "{over}");
        let none = bytes_of(&w, Command::ReadFileBytes { id: MessageId([7; 16]), cap: 1 << 20 }).await;
        assert!(none.is_err(), "an id that names no share");

        // the share survives a reopen, and so does its file
        let ws = read_session(&w).await.active_workspace.clone();
        w.execute(Command::CloseWorkspace).await.expect("close");
        w.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");
        assert_eq!(entries(&dir.join("uploads")), std::slice::from_ref(&up));
        let got = bytes_of(&w, Command::ReadFileBytes { id: promised, cap: 1 << 20 }).await.expect("read");
        assert_eq!(got, content);

        w.execute(Command::RemoveFile { id: promised }).await.expect("remove");
        assert!(!file.exists(), "the store's copy goes with the share");
        assert!(!dir.join("uploads").exists(), "an empty store is no directory");
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// A name is one path component: anything that could leave the store is
/// refused before a byte or a directory exists.
#[test]
fn a_path_as_a_name_is_refused_before_anything_is_written() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        for bad in ["../escape", "a/b", "/etc/passwd", "..", "", "a\\b"] {
            let err = begin(&w, bad, 1).await.expect_err(bad);
            assert!(matches!(err, MoltError::BadPayload(_)), "{bad:?}: {err}");
        }
        let over = begin(&w, "big", usize::try_from(molt_core::wire::MAX_TRANSFER).expect("fits") + 1).await;
        assert!(over.expect_err("oversize").to_string().contains("limit"));
        assert!(entries(&dir.join("uploads")).is_empty());
        assert!(!tmp.path().join("workspaces").join("escape").exists());
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// Out of order, oversize, unknown: each refused, and a refusal does not
/// cost the upload - the right chunk still lands.
#[test]
fn a_wrong_chunk_is_refused_and_the_right_one_still_lands() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        let up = begin(&w, "a.bin", 6).await.expect("begin");
        let refused = |r: Result<Reply, MoltError>| r.expect_err("refused").to_string();
        assert!(refused(chunk(&w, "00ff", 0, b"abc", false).await).contains("unknown id"));
        assert!(refused(chunk(&w, &up, 3, b"def", true).await).contains("expected 0"));
        assert!(refused(chunk(&w, &up, 0, b"abcdefg", true).await).contains("more bytes"));
        let fat = vec![0u8; molt_core::wire::CHUNK + 1];
        assert!(refused(chunk(&w, &up, 0, &fat, false).await).contains("at most"));
        chunk(&w, &up, 0, b"abc", false).await.expect("first");
        assert!(refused(chunk(&w, &up, 0, b"abc", false).await).contains("expected 3"), "a repeat");
        // not whole: no share yet
        let early = w
            .execute(Command::ShareUpload { upload: up.clone(), channel: molt_core::ChannelRef::default() })
            .await
            .expect_err("not whole");
        assert!(early.to_string().contains("not whole"), "{early}");
        chunk(&w, &up, 3, b"def", true).await.expect("last");
        assert_eq!(std::fs::read(dir.join("uploads").join(&up).join("a.bin")).expect("landed"), b"abcdef");
        w.execute(Command::UploadAbort { upload: up.clone() }).await.expect("abort");
        assert!(entries(&dir.join("uploads")).is_empty());
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// An upload belongs to the workspace it was begun in: closing it ends
/// the upload, and whatever a crash left in the store goes at the next
/// open.
#[test]
fn an_upload_ends_with_its_workspace_and_leftovers_go_at_open() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, ws) = stored(&tmp).await;
        let store = dir.join("uploads");
        let up = begin(&w, "a.bin", 6).await.expect("begin");
        chunk(&w, &up, 0, b"abc", false).await.expect("first");
        assert_eq!(entries(&store), std::slice::from_ref(&up));
        w.execute(Command::CloseWorkspace).await.expect("close");
        let late = chunk(&w, &up, 3, b"def", true).await.expect_err("the workspace is closed");
        assert!(late.to_string().contains("unknown id"), "{late}");
        await_gone(&store.join(&up)).await;
        // what a hard kill would have left behind
        std::fs::create_dir_all(store.join("deadbeef")).expect("orphan");
        std::fs::write(store.join("deadbeef").join("part"), b"abc").expect("orphan part");
        w.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");
        assert!(entries(&store).is_empty(), "the open sweeps the store");
        let late = chunk(&w, &up, 3, b"def", true).await.expect_err("still unknown");
        assert!(late.to_string().contains("unknown id"), "{late}");
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// A workspace that is not on disk has no store.
#[test]
fn a_session_only_workspace_takes_no_upload() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let err = begin(&w, "a.bin", 1).await.expect_err("no store");
        assert!(err.to_string().contains("no stored workspace"), "{err}");
    });
}

/// A stored share that expired leaves the disk on the sweep; one inside
/// the window, and a share from anywhere else on the host, stay.
#[test]
fn an_expired_stored_share_is_removed_by_the_sweep() {
    let tmp = tempfile::tempdir().expect("tmp");
    let store = tmp.path().join("uploads");
    let elsewhere = tmp.path().join("elsewhere");
    let mut st = plain_state();
    let mut share = |n: u8, ts: u64, dir: &std::path::Path| {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join("a.bin");
        std::fs::write(&path, b"minutes").expect("file");
        let id = MessageId([n; 16]);
        let mut msg = molt_core::ChatMessage::text(id, "me", "a share", ts);
        msg.file = Some(molt_core::FileMeta {
            name: "a.bin".to_string(),
            size: 7,
            kind: "BIN".to_string(),
            modified: 100,
            available: true,
            checksum: "ab".repeat(32),
            key_b64: String::new(),
            pieces: 0,
            root: String::new(),
        });
        let env = st.make_env("me".to_string(), molt_core::WorkspaceEvent::Chat(msg));
        st.apply(&env);
        st.files.share_paths.insert(id, path.clone());
        (id, path)
    };
    let old_dir = store.join("a1".repeat(16));
    let (old, old_path) = share(1, 100, &old_dir);
    let (fresh, fresh_path) = share(2, now_secs(), &store.join("a2".repeat(16)));
    let (host, host_path) = share(3, 100, &elsewhere);
    st.forget_expired_stored_shares(&store);
    assert!(!old_path.exists() && !old_dir.exists(), "the expired upload is gone");
    assert!(!st.files.share_paths.contains_key(&old));
    assert!(fresh_path.exists() && st.files.share_paths.contains_key(&fresh));
    assert!(host_path.exists(), "a file outside the store is never this sweep's");
    assert!(st.files.share_paths.contains_key(&host));
}

/// The sweep removes what nothing names - and a file this node's own
/// record of its shares still names is not that, running path or not.
#[test]
fn the_sweep_keeps_every_share_this_node_recorded() {
    let b = crate::chain::test_support::Builder::new(&["petra", "walter", "dora"], 2);
    let (mut st, _tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);
    let store = dir.join("uploads");
    let recorded = "a1".repeat(16);
    for d in [recorded.as_str(), "orphan"] {
        std::fs::create_dir_all(store.join(d)).expect("dir");
        std::fs::write(store.join(d).join("a.bin"), b"abc").expect("file");
    }
    let id = MessageId([9; 16]);
    let active = st.active.as_mut().expect("a stored workspace");
    active.prefs.shared_files.insert(id.to_string(), store.join(&recorded).join("a.bin").display().to_string());
    assert!(!st.files.share_paths.contains_key(&id), "not adopted into the running map");
    st.sweep_upload_store();
    assert!(store.join(&recorded).join("a.bin").exists());
    assert!(!store.join("orphan").exists());
}

/// The store is no second path door: a share by path never becomes the
/// store's, and what the store deletes is only a directory it minted.
#[test]
fn a_share_by_path_is_never_the_stores_to_delete() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        let store = dir.join("uploads");
        let content = b"minutes".to_vec();
        let up = upload(&w, "a.bin", &content, 7).await;
        let stored_file = store.join(&up).join("a.bin");
        std::fs::write(dir.join("notes.txt"), b"mine").expect("a file beside the store");
        let share = |path: String| {
            let w = w.clone();
            async move { w.execute(Command::ShareFile { path, channel: molt_core::ChannelRef::default() }).await }
        };
        // the store's own file is shared by its upload, never by its path:
        // a second share would take the first one's bytes when it leaves
        for inside in [
            stored_file.display().to_string(),
            format!("{}/../notes.txt", store.display()),
            format!("{}/{up}/../../notes.txt", store.display()),
        ] {
            let err = share(inside.clone()).await.expect_err(&inside);
            assert!(err.to_string().contains("upload store"), "{inside}: {err}");
        }
        // ...and a path that only resolves into it is an ordinary host file
        let name = dir.file_name().expect("name").to_string_lossy().into_owned();
        let alias = format!("{}/../{name}/uploads/{up}/a.bin", dir.display());
        let by_path = match share(alias).await.expect("shared by path") {
            Reply::Sent { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(await_share(&w, "a.bin").await, by_path);
        w.execute(Command::RemoveFile { id: by_path }).await.expect("remove");
        assert_eq!(std::fs::read(&stored_file).expect("the upload is untouched"), content);
        assert!(dir.join("notes.txt").exists() && dir.join("manifest.toml").exists());
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// A remembered path that climbs out of the store with `..` deletes
/// nothing when its share goes.
#[test]
fn a_forgotten_share_path_with_dot_dot_deletes_nothing() {
    let b = crate::chain::test_support::Builder::new(&["petra", "walter", "dora"], 2);
    let (mut st, _tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);
    let store = dir.join("uploads");
    let live = store.join("cd".repeat(16));
    std::fs::create_dir_all(&live).expect("store");
    std::fs::write(live.join("a.bin"), b"abc").expect("file");
    std::fs::write(dir.join("notes.txt"), b"mine").expect("file");
    let before = entries(&dir);
    for (n, path) in [
        (1, store.join("..").join("notes.txt")),
        (2, live.join("..").join("..").join("notes.txt")),
        (3, live.join("..").join("a.bin")),
    ] {
        let id = MessageId([n; 16]);
        st.files.share_paths.insert(id, path);
        st.forget_share_path(&id);
    }
    assert_eq!(entries(&dir), before, "the workspace is whole");
    assert!(live.join("a.bin").exists() && dir.join("notes.txt").exists());
}

/// A share whose hash fails gives its upload back at once: the bytes, the
/// quota and the open slot.
#[test]
fn a_failed_share_drops_its_upload() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        let up = upload(&w, "a.bin", b"abc", 3).await;
        // the stored file is unreadable by the time the hash runs
        std::fs::remove_file(dir.join("uploads").join(&up).join("a.bin")).expect("gone behind the engine");
        w.execute(Command::ShareUpload { upload: up.clone(), channel: molt_core::ChannelRef::default() })
            .await
            .expect("accepted");
        for _ in 0..500 {
            if read_session(&w).await.notice.starts_with("share-failed:a.bin") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(read_session(&w).await.notice.starts_with("share-failed:a.bin"));
        let gone = w.execute(Command::UploadAbort { upload: up.clone() }).await.expect_err("released");
        assert!(gone.to_string().contains("unknown id"), "{gone}");
        await_gone(&dir.join("uploads").join(&up)).await;
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// A real 2x2 PNG.
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";

/// A picture is read after the writer wrote it: the file is materialized
/// off the actor, and a read right behind the applied change must not
/// answer the file as it was before.
#[test]
fn a_picture_read_waits_for_the_writer() {
    let b = crate::chain::test_support::Builder::new(&["petra", "walter", "dora"], 2);
    let (mut st, _tmp, _dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_B64).expect("b64");
    rt().block_on(async {
        for round in 0..20 {
            // the same path every round: a first picture, then replaced
            let bytes: Vec<u8> = png.iter().copied().chain(std::iter::repeat_n(0, round)).collect();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            st.applied.insert(
                Surface::Organization,
                vec![(None, json!({"op": "set_image", "value": "crest.png", "bytes_b64": b64}))],
            );
            st.sync_logo_file();
            let logo = st.status().image;
            let (tx, rx) = tokio::sync::oneshot::channel();
            st.deferred_reply = Some(tx);
            st.cmd_read_image_bytes(logo, 1 << 20).expect("accepted");
            let Ok(Reply::UploadBytes { bytes: got }) = rx.await.expect("answered") else { panic!("round {round}: no bytes") };
            assert_eq!(got, bytes, "round {round}");
        }
    });
}

/// The logo comes by the key the status view carries - and ONLY by a key
/// that is a current picture of this workspace: the key is compared,
/// never opened as a path.
#[test]
fn a_picture_reads_by_its_view_key_and_no_other_path_does() {
    let b = crate::chain::test_support::Builder::new(&["petra", "walter", "dora"], 2);
    let (mut st, tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_B64).expect("b64");
    st.applied.insert(
        Surface::Organization,
        vec![
            (None, json!({"op": "set_image", "value": "crest.png", "bytes_b64": PNG_B64})),
            (None, json!({"op": "set_member_image", "member": "dora", "value": "me.png", "bytes_b64": PNG_B64})),
        ],
    );
    st.sync_logo_file();
    st.sync_avatar_files();
    let logo = st.status().image;
    assert_eq!(std::path::Path::new(&logo), dir.join("logo.png"));
    let avatar = st.member_profiles().get("dora").expect("dora's profile").image.clone();
    assert!(avatar.contains("avatar-dora-"), "{avatar}");
    // the writer materializes both off the actor
    for _ in 0..500 {
        if std::path::Path::new(&logo).is_file() && std::path::Path::new(&avatar).is_file() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    rt().block_on(async {
        let read = async |st: &mut State, key: &str, cap: u64| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            st.deferred_reply = Some(tx);
            match st.cmd_read_image_bytes(key.to_string(), cap) {
                Err(e) => Err(e),
                Ok(_) => match rx.await.expect("answered") {
                    Ok(Reply::UploadBytes { bytes }) => Ok(bytes),
                    Ok(other) => panic!("unexpected: {other:?}"),
                    Err(e) => Err(e),
                },
            }
        };
        assert_eq!(read(&mut st, &logo, 1 << 20).await.expect("the logo"), png);
        assert_eq!(read(&mut st, &avatar, 1 << 20).await.expect("the avatar"), png);
        let over = read(&mut st, &logo, 3).await.expect_err("cap");
        assert!(over.to_string().contains("cap"), "{over}");

        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, b"not a picture").expect("file");
        let name = dir.file_name().expect("name").to_string_lossy().into_owned();
        for other in [
            secret.display().to_string(),
            dir.join("manifest.toml").display().to_string(),
            dir.join("keys").join("seed.sealed").display().to_string(),
            format!("{logo}/../manifest.toml"),
            format!("{}/../{name}/logo.png", dir.display()),
            "logo.png".to_string(),
            String::new(),
        ] {
            let err = read(&mut st, &other, 1 << 20).await.expect_err(&other);
            assert!(err.to_string().contains("unknown image"), "{other:?}: {err}");
        }

        // a session-only workspace's reference is whatever was proposed:
        // it matches the view and is still never opened
        let mut loose = plain_state();
        let value = secret.display().to_string();
        loose.applied.insert(
            Surface::Organization,
            vec![(None, json!({"op": "set_image", "value": value, "bytes_b64": PNG_B64}))],
        );
        assert_eq!(loose.status().image, value);
        let err = read(&mut loose, &value, 1 << 20).await.expect_err("never a path");
        assert!(err.to_string().contains("unknown image"), "{err}");
    });
}

/// A picture too large for a frame goes up as an upload and is proposed
/// by its id; the engine reads it, and the store keeps nothing.
#[test]
fn a_picture_proposal_takes_an_upload_as_its_source() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let (w, dir, _) = stored(&tmp).await;
        let png = base64::engine::general_purpose::STANDARD.decode(PNG_B64).expect("b64");
        let up = upload(&w, "crest.png", &png, 40).await;
        let both = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "upload": up, "bytes_b64": PNG_B64}),
            })
            .await
            .expect_err("two sources");
        assert!(both.to_string().contains("alternatives"), "{both}");
        let unknown = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "upload": "00ff"}),
            })
            .await
            .expect_err("unknown upload");
        assert!(unknown.to_string().contains("unknown id"), "{unknown}");
        let missing = w
            .execute(Command::Propose { surface: Surface::Organization, payload: json!({"op": "set_image", "upload": ""}) })
            .await
            .expect_err("no source");
        assert!(missing.to_string().contains("`path`, `upload` or `bytes_b64`"), "{missing}");
        w.execute(Command::Propose { surface: Surface::Organization, payload: json!({"op": "set_image", "upload": up}) })
            .await
            .expect("propose");
        let pending = read_surface(&w, Surface::Organization).await.pending;
        let card = pending.iter().max_by_key(|p| p.id.0).expect("a card");
        assert_eq!(card.payload["value"], "crest.png", "named after the uploaded file");
        assert!(card.payload.get("upload").is_none(), "the id is never signed");
        assert_eq!(crate::proposals::image_bytes(&card.payload).expect("bytes"), png);
        await_gone(&dir.join("uploads").join(&up)).await;
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}

/// An export into the exchange folder reads back as bytes; the read
/// names no path, only "the last export".
#[test]
fn the_last_export_reads_back_as_bytes() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let mut session = storage_session(&tmp);
        session.settings.download_dir = tmp.path().join("exchange").display().to_string();
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Store".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let id = read_session(&w).await.active_workspace.clone();

        let none = bytes_of(&w, Command::ReadExportBytes { cap: 1 << 26 }).await.expect_err("no export yet");
        assert!(none.to_string().contains("no export"), "{none}");
        w.execute(Command::ExportWorkspace {
            id,
            dest: "store.molt.enc".to_string(),
            passphrase: "correct horse battery".to_string(),
        })
        .await
        .expect("export");
        let done = await_export(&w).await;
        assert_eq!(done.result, "ok");
        let on_disk = std::fs::read(tmp.path().join("exchange").join("store.molt.enc")).expect("the blob");
        let got = bytes_of(&w, Command::ReadExportBytes { cap: 1 << 26 }).await.expect("the export");
        assert_eq!(got, on_disk);
        let over = bytes_of(&w, Command::ReadExportBytes { cap: 16 }).await.expect_err("cap");
        assert!(over.to_string().contains("cap"), "{over}");
        w.execute(Command::CloseWorkspace).await.expect("close");
    });
}
