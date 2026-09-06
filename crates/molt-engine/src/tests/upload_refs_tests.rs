// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiki file references (`wiki_files_and_images.md` §3.2): what an
//! `upload:<hex>` resolves to, and where the bytes are.

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

async fn resolve(w: &WalletHandle, hex: &str) -> Result<(Option<molt_core::UploadView>, bool, bool, molt_core::LocalCopy), MoltError> {
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
        assert!(matches!(local, molt_core::LocalCopy::Own { .. }), "the sharer has the bytes: {local:?}");

        persist(&w, id).await;
        let (up, amb, temp, local) = resolve(&w, &prefix.to_uppercase()).await.expect("resolves");
        assert_eq!(up.as_ref().map(|u| u.id), Some(id));
        assert!(!temp && !amb);
        assert_eq!(
            local,
            molt_core::LocalCopy::Own { path: tmp.path().join("netz.png").display().to_string() }
        );
        let (up, ..) = resolve(&w, &full).await.expect("the full hash resolves too");
        assert_eq!(up.map(|u| u.id), Some(id));

        let (up, amb, temp, local) = resolve(&w, "000000000000").await.expect("unknown is an answer");
        assert!(up.is_none() && !amb && !temp);
        assert_eq!(local, molt_core::LocalCopy::None);

        assert!(matches!(resolve(&w, "abc").await, Err(MoltError::BadPayload(_))), "too short");
        assert!(matches!(resolve(&w, "zzzzzzzzzzzz").await, Err(MoltError::BadPayload(_))), "not hex");
    });
}
