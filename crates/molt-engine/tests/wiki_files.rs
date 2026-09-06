// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **What an agent learns about a page's file references**
//! (`docs_archive/ui/wiki_files_and_images.md` §3.7): `wiki_get` answers what
//! each `upload:<hex>` names today, `wiki_edit` warns before a page
//! references nothing, and `wiki_health` finds the references that rotted.

use std::time::Duration;

use molt_core::{
    ChannelRef, Command, GroupConfig, MoltError, ProposalId, Reply, SessionView, Surface, WikiEdit,
};
use molt_engine::WalletHandle;

/// A single-member group: threshold 1, so one `Approve` applies.
fn spawn_solo() -> WalletHandle {
    molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    )
}

/// The graph is built OFF the actor, so a read may refuse until it is there.
async fn settle(w: &WalletHandle, cmd: Command) -> Result<Reply, MoltError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match w.execute(cmd.clone()).await {
            Err(MoltError::IndexBuilding { .. }) => {}
            other => return other,
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wiki index never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn uploads(w: &WalletHandle) -> Vec<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("uploads") {
        Reply::Uploads { uploads } => uploads,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Share `name` out of `dir` and wait for its row; answers `(id, checksum)`.
async fn share(
    w: &WalletHandle,
    dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> (molt_core::MessageId, String) {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the share source");
    w.execute(Command::ShareFile {
        path: path.display().to_string(),
        channel: ChannelRef::default(),
    })
    .await
    .expect("share");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(u) = uploads(w).await.into_iter().find(|u| u.name == name) {
            assert_eq!(u.checksum.len(), 64, "the share carries its sha256");
            return (u.id, u.checksum);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the share message for {name} never posted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The persist vote, at threshold 1.
async fn persist(w: &WalletHandle, id: molt_core::MessageId) {
    let Reply::Proposed { id: pid, .. } = w
        .execute(Command::Propose {
            surface: Surface::Files,
            payload: serde_json::json!({ "op": "persist", "id": id.to_string() }),
        })
        .await
        .expect("propose persist")
    else {
        panic!("propose answers a proposal");
    };
    w.execute(Command::Approve {
        proposal: pid,
        note: None,
    })
    .await
    .expect("approve");
}

fn content(path: &str, body: &str) -> WikiEdit {
    WikiEdit::Content {
        path: path.to_string(),
        content: body.to_string(),
    }
}

/// Propose the edits and approve them (m = 1).
async fn edit(w: &WalletHandle, edits: Vec<WikiEdit>, allow_warnings: bool) -> Result<(), MoltError> {
    let reply = settle(
        w,
        Command::WikiEdit {
            edits,
            dry_run: false,
            allow_warnings,
            supersedes: None,
        },
    )
    .await?;
    let Reply::Proposed { id, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    approve(w, id).await;
    Ok(())
}

async fn approve(w: &WalletHandle, id: ProposalId) {
    w.execute(Command::Approve {
        proposal: id,
        note: None,
    })
    .await
    .expect("approve");
}

/// The `files` list of one page, in the order the read serves it.
async fn page_files(w: &WalletHandle, path: &str) -> Vec<molt_core::WikiFileRef> {
    match settle(
        w,
        Command::WikiGet {
            path: path.to_string(),
        },
    )
    .await
    .expect("the document is there")
    {
        Reply::WikiDocument { files, .. } => files,
        other => panic!("unexpected: {other:?}"),
    }
}

/// `(hex, name, state)` per entry - what the assertions read.
fn triples(files: &[molt_core::WikiFileRef]) -> Vec<(String, String, String)> {
    files
        .iter()
        .map(|f| (f.hex.clone(), f.name.clone(), f.state.clone()))
        .collect()
}

async fn health(w: &WalletHandle, limit: u32) -> molt_core::WikiFileHealth {
    match settle(w, Command::WikiHealth { limit }).await.expect("health") {
        Reply::WikiHealth { files, .. } => files,
        other => panic!("unexpected: {other:?}"),
    }
}

const UNKNOWN: &str = "000000000000deadbeef";

/// **Keystone 1**: a page's references answer with what they name today -
/// the persisted file this seat shared reads `local` with its name, an
/// unknown prefix reads `unknown` with none, and each hex is listed once,
/// in document order.
#[tokio::test]
async fn wiki_get_answers_every_reference_with_its_state() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = spawn_solo();
    let (id, full) = share(&w, tmp.path(), "netz.png", b"not really a png").await;
    persist(&w, id).await;
    let prefix = &full[..12];

    edit(
        &w,
        vec![content(
            "plan.md",
            &format!(
                "# Plan\n\n![Netz](upload:{prefix})\n\nSiehe [Bericht](upload:{UNKNOWN}) und\nnochmal [dasselbe](upload:{full}).\n"
            ),
        )],
        true,
    )
    .await
    .expect("the page proposes (the unknown reference is a warning)");

    let files = page_files(&w, "plan.md").await;
    assert_eq!(
        triples(&files),
        vec![
            (prefix.to_string(), "netz.png".to_string(), "local".to_string()),
            (UNKNOWN.to_string(), String::new(), "unknown".to_string()),
            (full.clone(), "netz.png".to_string(), "local".to_string()),
        ],
        "document order, one entry per distinct hex"
    );
}

/// **Keystone 2**: a share the persist vote has not pinned yet reads
/// `temporary` - the page may reference a file that is about to be
/// persisted, so the state is a fact and not a refusal.
#[tokio::test]
async fn an_unpersisted_share_reads_temporary() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = spawn_solo();
    let (id, full) = share(&w, tmp.path(), "entwurf.pdf", b"minutes").await;
    let prefix = full[..12].to_string();
    edit(
        &w,
        vec![content("plan.md", &format!("[Entwurf](upload:{prefix})\n"))],
        true,
    )
    .await
    .expect("a temporary reference writes");
    assert_eq!(
        triples(&page_files(&w, "plan.md").await),
        vec![(prefix.clone(), "entwurf.pdf".to_string(), "temporary".to_string())]
    );

    persist(&w, id).await;
    assert_eq!(
        triples(&page_files(&w, "plan.md").await),
        vec![(prefix, "entwurf.pdf".to_string(), "local".to_string())],
        "the vote turns the same reference into a real one"
    );
}

/// **Keystone 3**: a reference inside a code fence is an example, not a
/// dependency - it reaches neither `wiki_get` nor the warning.
#[tokio::test]
async fn a_reference_in_a_code_fence_is_no_reference() {
    let w = spawn_solo();
    edit(
        &w,
        vec![content(
            "howto.md",
            &format!("# How to\n\n```\n![Netz](upload:{UNKNOWN})\n```\n\nA `[x](upload:{UNKNOWN})` span too.\n"),
        )],
        false,
    )
    .await
    .expect("no warning, so no allow_warnings");
    assert!(page_files(&w, "howto.md").await.is_empty());
}

/// **Keystone 4**: `wiki_edit` refuses a page that would reference nothing
/// and proposes it once the caller says so - a temporary match never warns.
#[tokio::test]
async fn wiki_edit_warns_about_an_unresolved_reference() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = spawn_solo();
    let (id, full) = share(&w, tmp.path(), "netz.png", b"not really a png").await;
    let prefix = full[..12].to_string();

    let refused = match settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("plan.md", &format!("![Netz](upload:{UNKNOWN})\n"))],
            dry_run: false,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    {
        Err(e) => e.to_string(),
        Ok(other) => panic!("expected a refusal, got {other:?}"),
    };
    assert!(
        refused.contains(&format!("file reference unresolved: upload:{UNKNOWN}")),
        "{refused}"
    );

    // the temporary share is NOT a warning: the persist vote may follow
    edit(
        &w,
        vec![content("draft.md", &format!("[Entwurf](upload:{prefix})\n"))],
        false,
    )
    .await
    .expect("a temporary match writes without allow_warnings");

    // …and the unresolved one writes once the caller accepts it
    edit(
        &w,
        vec![content("plan.md", &format!("![Netz](upload:{UNKNOWN})\n"))],
        true,
    )
    .await
    .expect("allow_warnings proposes anyway");
    assert_eq!(
        triples(&page_files(&w, "plan.md").await),
        vec![(UNKNOWN.to_string(), String::new(), "unknown".to_string())]
    );
    persist(&w, id).await;
}

/// **Keystone 5**: the hygiene pass finds the references that rot and
/// names the pages that carry them - a file nobody shared, and one the
/// persist vote never pinned.
#[tokio::test]
async fn wiki_health_reports_the_rotting_references_with_their_pages() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = spawn_solo();
    let (id, full) = share(&w, tmp.path(), "entwurf.pdf", b"minutes").await;
    let prefix = full[..12].to_string();

    edit(
        &w,
        vec![
            content("a.md", &format!("![Netz](upload:{UNKNOWN})\n")),
            content("b.md", &format!("[Auch](upload:{UNKNOWN}) [Entwurf](upload:{prefix})\n")),
        ],
        true,
    )
    .await
    .expect("both pages");

    let files = health(&w, 0).await;
    assert_eq!(files.dangling_total, 1);
    assert_eq!(files.dangling[0].hex, UNKNOWN);
    assert_eq!(files.dangling[0].paths, vec!["a.md".to_string(), "b.md".to_string()]);
    assert_eq!(files.dangling[0].paths_total, 2);
    assert_eq!(files.temporary_total, 1);
    assert_eq!(files.temporary[0].hex, prefix);
    assert_eq!(files.temporary[0].paths, vec!["b.md".to_string()]);
    assert!(files.ambiguous.is_empty() && files.ambiguous_total == 0);

    // the persist vote clears the temporary finding, the dangling one stays
    persist(&w, id).await;
    let files = health(&w, 0).await;
    assert!(files.temporary.is_empty(), "{:?}", files.temporary);
    assert_eq!(files.dangling_total, 1);

    // each list caps like the other health lists, with its own total
    let files = health(&w, 1).await;
    assert_eq!(files.dangling[0].paths.len(), 1);
    assert_eq!(files.dangling[0].paths_total, 2);
}
