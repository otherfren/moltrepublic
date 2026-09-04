// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent Uploads (`docs_archive/files/persistent_uploads.md`): a vote pins a
//! share for good, a second vote lets it expire again.

use super::support::*;
use crate::*;
use serde_json::json;

fn one_of_three() -> GroupConfig {
    GroupConfig {
        threshold: 1,
        self_cosign: false,
        ..GroupConfig::demo()
    }
}

async fn uploads(w: &WalletHandle) -> Vec<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn propose(
    w: &WalletHandle,
    payload: serde_json::Value,
) -> Result<molt_core::ProposalId, MoltError> {
    match w
        .execute(Command::Propose {
            surface: Surface::Files,
            payload,
        })
        .await?
    {
        Reply::Proposed { id } => Ok(id),
        other => panic!("unexpected: {other:?}"),
    }
}

/// The engine fills the file identity into the persist proposal, the
/// applied vote moves the share to the persistent table with no expiry,
/// and an unpersist vote moves it back with a fresh window.
#[test]
fn a_persist_vote_pins_the_share_and_an_unpersist_vote_restarts_its_clock() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(one_of_three(), SessionView::default());
        let id = share_temp_file(&w, tmp.path(), "protokoll.pdf", b"minutes").await;
        let before = uploads(&w).await;
        assert_eq!(before.len(), 1);
        assert!(!before[0].persistent);
        assert!(before[0].expires_ts > 0);

        let pid = propose(&w, json!({"op": "persist", "id": id.to_string()}))
            .await
            .expect("a live share may be persisted");
        let pending = read_surface(&w, Surface::Files).await.pending;
        assert_eq!(pending.len(), 1);
        let payload = &pending[0].payload;
        assert_eq!(payload["name"], json!("protokoll.pdf"), "the engine filled the identity");
        assert_eq!(payload["size"], json!(7));
        assert_eq!(payload["by"], json!("me"));
        assert!(!payload["checksum"].as_str().unwrap_or("").is_empty());
        assert!(
            matches!(
                propose(&w, json!({"op": "persist", "id": id.to_string()})).await,
                Err(MoltError::BadPayload(_))
            ),
            "a second persist while one is pending is refused"
        );

        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
        let after = uploads(&w).await;
        assert_eq!(after.len(), 1, "one share, one row");
        assert!(after[0].persistent, "the vote pinned it");
        assert_eq!(after[0].expires_ts, 0, "no deadline");
        assert_eq!(after[0].name, "protokoll.pdf");
        assert!(
            matches!(
                propose(&w, json!({"op": "persist", "id": id.to_string()})).await,
                Err(MoltError::BadPayload(_))
            ),
            "persisting a persistent share is refused"
        );

        let at = now_secs();
        let pid = propose(&w, json!({"op": "unpersist", "id": id.to_string(), "at": at}))
            .await
            .expect("a persistent share may be unpersisted");
        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
        let back = uploads(&w).await;
        assert_eq!(back.len(), 1);
        assert!(!back[0].persistent);
        assert_eq!(
            back[0].expires_ts,
            at + 7 * 86_400,
            "the clock restarts at the unpersist stamp (default 7-day window)"
        );
        assert!(
            matches!(
                propose(&w, json!({"op": "unpersist", "id": id.to_string(), "at": at})).await,
                Err(MoltError::BadPayload(_))
            ),
            "unpersisting a temporary share is refused"
        );
    });
}

/// Refusals: an unknown id, a missing op, an unpersist stamp off the clock.
#[test]
fn files_proposals_are_validated_against_the_live_table() {
    rt().block_on(async {
        let w = spawn(one_of_three(), SessionView::default());
        let ghost = "cd".repeat(16);
        assert!(matches!(
            propose(&w, json!({"op": "persist", "id": ghost})).await,
            Err(MoltError::BadPayload(_))
        ));
        assert!(matches!(
            propose(&w, json!({"op": "unpersist", "id": ghost, "at": 1})).await,
            Err(MoltError::BadPayload(_))
        ));
        assert!(matches!(
            propose(&w, json!({"op": "note", "title": "x"})).await,
            Err(MoltError::BadPayload(_))
        ));
    });
}

/// A persisted share outlives the chat retention window: its message is
/// aged out of the chat read, the row still lists from the block's
/// identity, and the download gate lets it through.
#[test]
fn a_persisted_share_outlives_the_chat_window() {
    let mut st = plain_state();
    let id = molt_core::MessageId([5u8; 16]);
    let old = now_secs() - 30 * 86_400;
    let mut msg = molt_core::ChatMessage::text(id, "me", "a share", old);
    msg.file = Some(molt_core::FileMeta {
        name: "plan.pdf".to_string(),
        size: 9,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    let env = st.make_env("me".to_string(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);
    assert!(st.uploads_view().is_empty(), "aged out: not in the temporary table");

    st.applied.entry(Surface::Files).or_default().push((None, json!({
        "op": "persist", "id": id.to_string(), "name": "plan.pdf", "kind": "PDF",
        "size": 9, "checksum": "ab".repeat(32), "by": "me", "shared_ts": old
    })));
    let rows = st.uploads_view();
    assert_eq!(rows.len(), 1, "the block keeps it listed");
    assert!(rows[0].persistent);
    assert_eq!(rows[0].expires_ts, 0);
    assert_eq!(rows[0].name, "plan.pdf");
    assert!(!st.share_expired(&id), "no deadline for the download gate");

    // …and an unpersisted share expires at its own stamp, not the message's
    let at = now_secs() - 3 * 86_400;
    st.applied.entry(Surface::Files).or_default().push((None, json!({
        "op": "unpersist", "id": id.to_string(), "at": at
    })));
    let rows = st.uploads_view();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].persistent);
    assert_eq!(rows[0].expires_ts, at + 7 * 86_400);
    assert!(!st.share_expired(&id));
    // a stamp older than the window: gone from the table, refused at the gate
    st.applied.entry(Surface::Files).or_default().push((None, json!({
        "op": "unpersist", "id": id.to_string(), "at": old
    })));
    assert!(st.uploads_view().is_empty());
    assert!(st.share_expired(&id));
}

/// A checkpoint keeps only the latest op per share: a log holding just
/// the unpersist (with its identity) must still fold, and its stamp never
/// restarts the window before the share existed.
#[test]
fn an_unpersist_alone_folds_from_its_own_identity() {
    let mut st = plain_state();
    let id = molt_core::MessageId([6u8; 16]);
    let shared = now_secs() - 2 * 86_400;
    st.applied.entry(Surface::Files).or_default().push((None, json!({
        "op": "unpersist", "id": id.to_string(), "at": shared - 86_400,
        "name": "cut.pdf", "kind": "PDF", "size": 3, "checksum": "cd".repeat(32),
        "by": "me", "shared_ts": shared
    })));
    let rows = st.uploads_view();
    assert_eq!(rows.len(), 1, "the unpersist carries what the fold needs");
    assert!(!rows[0].persistent);
    assert_eq!(rows[0].name, "cut.pdf");
    assert_eq!(rows[0].expires_ts, shared + 7 * 86_400, "floored at the share's stamp");
}

/// The wire door: the shape every seat can check without state.
#[test]
fn files_payloads_are_validated_at_the_wire_door() {
    use crate::files_state::validate_files_payload;
    let id = "ef".repeat(16);
    let good = json!({"op": "persist", "id": id, "by": "me", "name": "a.txt", "size": 1});
    assert!(validate_files_payload(&good).is_ok());
    let unpersist = json!({"op": "unpersist", "id": id, "by": "me", "name": "a.txt", "size": 1, "at": 5});
    assert!(validate_files_payload(&unpersist).is_ok());
    for bad in [
        json!({"op": "note", "id": id, "by": "me", "name": "a", "size": 1}),
        json!({"op": "persist", "id": "nope", "by": "me", "name": "a", "size": 1}),
        json!({"op": "persist", "id": id, "by": "me", "name": "a"}),
        json!({"op": "persist", "id": id, "by": "", "name": "a", "size": 1}),
        json!({"op": "unpersist", "id": id, "by": "me", "name": "a", "size": 1}),
    ] {
        assert!(validate_files_payload(&bad).is_err(), "{bad}");
    }
}

/// The approve door: a proposal that arrived over the wire naming a
/// different file than this seat has under that id gets no signature.
#[test]
fn an_approve_refuses_a_files_vote_that_names_a_different_file() {
    let mut st = plain_state();
    let id = molt_core::MessageId([7u8; 16]);
    let mut msg = molt_core::ChatMessage::text(id, "peer-1", "a share", now_secs());
    msg.file = Some(molt_core::FileMeta {
        name: "real.pdf".to_string(),
        size: 9,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    let env = st.make_env("peer-1".to_string(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);
    let forged = json!({
        "op": "persist", "id": id.to_string(), "by": "peer-1", "name": "real.pdf",
        "kind": "PDF", "size": 9, "checksum": "ff".repeat(32), "shared_ts": env.ts
    });
    assert!(
        matches!(st.check_files_vote(&forged), Err(MoltError::BadPayload(_))),
        "a foreign checksum is refused"
    );
    let honest = json!({
        "op": "persist", "id": id.to_string(), "by": "peer-1", "name": "real.pdf",
        "kind": "PDF", "size": 9, "checksum": "ab".repeat(32), "shared_ts": env.ts
    });
    assert!(st.check_files_vote(&honest).is_ok(), "the matching identity passes");
}

/// A pinned share is the republic's: its sharer can neither remove the
/// file nor tombstone the message until an unpersist vote frees it.
#[test]
fn a_persistent_share_cannot_be_removed_or_deleted_by_its_sharer() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(one_of_three(), SessionView::default());
        let id = share_temp_file(&w, tmp.path(), "pinned.txt", b"keep").await;
        let pid = propose(&w, json!({"op": "persist", "id": id.to_string()}))
            .await
            .expect("persist");
        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
        assert!(matches!(
            w.execute(Command::RemoveFile { id }).await,
            Err(MoltError::BadPayload(_))
        ));
        assert!(matches!(
            w.execute(Command::DeleteChat { id }).await,
            Err(MoltError::BadPayload(_))
        ));
        assert!(uploads(&w).await[0].persistent, "still listed as persistent");
    });
}

/// The persist block copies the series-v2 material with the identity
/// (members ratify the key and root along with the file), and a legacy
/// payload without it still validates.
#[test]
fn the_persist_payload_carries_the_series_material() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(one_of_three(), SessionView::default());
        let id = share_temp_file(&w, tmp.path(), "v2.bin", b"pinned bytes").await;
        propose(&w, json!({"op": "persist", "id": id.to_string()}))
            .await
            .expect("persist");
        let pending = read_surface(&w, Surface::Files).await.pending;
        let p = &pending[0].payload;
        assert_eq!(p["pieces"], json!(1));
        assert_eq!(p["root"].as_str().map(str::len), Some(64));
        assert_eq!(p["key_b64"].as_str().map(str::len), Some(44));
        let legacy = json!({"op": "persist", "id": "ab".repeat(16), "by": "me", "name": "old", "size": 3});
        assert!(crate::files_state::validate_files_payload(&legacy).is_ok());
    });
}

/// `file_cap_bytes`: absent = no cap, 0 = sharing off, n = a cap.
#[test]
fn the_file_cap_reads_absent_as_no_cap() {
    use crate::net::files::FileCap;
    let mut st = plain_state();
    st.session.settings.file_cap_bytes = None;
    assert_eq!(st.effective_file_cap(), FileCap::Unlimited);
    st.session.settings.file_cap_bytes = Some(0);
    assert_eq!(st.effective_file_cap(), FileCap::Off);
    st.session.settings.file_cap_bytes = Some(9);
    assert_eq!(st.effective_file_cap(), FileCap::Limit(9));
}

/// Mixed builds: a persist proposed by an older build carries no series
/// material; a new-build approver still matches it against its v2 share.
/// A claim that DOES carry material must match it exactly.
#[test]
fn an_older_builds_persist_still_matches_a_v2_share() {
    let mut st = plain_state();
    let id = molt_core::MessageId([8u8; 16]);
    let mut msg = molt_core::ChatMessage::text(id, "peer-1", "a share", now_secs());
    let key_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([9u8; 32])
    };
    msg.file = Some(molt_core::FileMeta {
        name: "v2.pdf".to_string(),
        size: 9,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: key_b64.clone(),
        pieces: 1,
        root: "cd".repeat(32),
    });
    let env = st.make_env("peer-1".to_string(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);
    let old_build = json!({
        "op": "persist", "id": id.to_string(), "by": "peer-1", "name": "v2.pdf",
        "kind": "PDF", "size": 9, "checksum": "ab".repeat(32), "shared_ts": env.ts
    });
    assert!(st.check_files_vote(&old_build).is_ok(), "six matching fields are enough");
    let wrong_root = json!({
        "op": "persist", "id": id.to_string(), "by": "peer-1", "name": "v2.pdf",
        "kind": "PDF", "size": 9, "checksum": "ab".repeat(32), "shared_ts": env.ts,
        "key_b64": key_b64, "pieces": 1, "root": "ef".repeat(32)
    });
    assert!(
        matches!(st.check_files_vote(&wrong_root), Err(MoltError::BadPayload(_))),
        "material that disagrees is refused"
    );
}

/// The wire door checks the series material's shape when a payload
/// carries any of it: key length, root hex, the piece count vs the size.
#[test]
fn files_payloads_check_the_series_material_shape() {
    use crate::files_state::validate_files_payload;
    let id = "ef".repeat(16);
    let key = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([1u8; 32])
    };
    let base = |extra: serde_json::Value| {
        let mut v = json!({"op": "persist", "id": id, "by": "me", "name": "a.bin", "size": 44_001});
        for (k, val) in extra.as_object().expect("object") {
            v[k] = val.clone();
        }
        v
    };
    assert!(validate_files_payload(&base(json!({}))).is_ok(), "legacy: no material");
    assert!(validate_files_payload(&base(json!({"key_b64": key, "pieces": 2, "root": "ab".repeat(32)}))).is_ok());
    for bad in [
        json!({"key_b64": "c2hvcnQ=", "pieces": 2, "root": "ab".repeat(32)}),
        json!({"key_b64": key, "pieces": 2, "root": "zz".repeat(32)}),
        json!({"key_b64": key, "pieces": 1, "root": "ab".repeat(32)}),
        json!({"root": "ab".repeat(32)}),
    ] {
        assert!(validate_files_payload(&base(bad.clone())).is_err(), "{bad}");
    }
}

/// A legacy (v1) fetch is bounded by the configured cap, else by the
/// 1 GiB floor - "no cap" is never stricter than a raised cap was.
#[test]
fn the_v1_fetch_bound_follows_the_cap() {
    use crate::net::files::{v1_fetch_bound, FileCap};
    assert_eq!(v1_fetch_bound(FileCap::Limit(50 * 1024 * 1024)), 50 * 1024 * 1024);
    assert_eq!(v1_fetch_bound(FileCap::Unlimited), 1024 * 1024 * 1024);
    assert_eq!(v1_fetch_bound(FileCap::Off), 1024 * 1024 * 1024);
}
