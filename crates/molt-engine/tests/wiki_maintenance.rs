// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The maintenance reads** (`docs_archive/memory/knowledge_base_scale.md`
//! §4.11): the two reads an agent that MAINTAINS the wiki needs -
//! `wiki_changes` (what moved since a revision) and `wiki_health` (what
//! the republic references and does not have, what nothing points at, and
//! the header keys that differ only in case or separator).
//!
//! Driven through the real command surface on a real engine, so what these
//! pin is what an MCP agent gets.

use std::time::Duration;

use molt_core::{Command, GroupConfig, MoltError, Reply, SessionView, Surface, WikiChange};
use molt_engine::WalletHandle;

/// A single-member group: threshold 1, no self-cosign - one `Approve`
/// applies, and no peer ever writes behind the test's back.
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

/// A `new file` patch for `path` holding `body`.
fn add(path: &str, body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut p = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for l in lines {
        p.push_str(&format!("+{l}\n"));
    }
    p
}

/// A `deleted file` patch that consumes exactly `body`.
fn delete(path: &str, body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut p = format!(
        "diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n@@ -1,{} +0,0 @@\n",
        lines.len()
    );
    for l in lines {
        p.push_str(&format!("-{l}\n"));
    }
    p
}

/// A pure rename, no content change.
fn rename(from: &str, to: &str) -> String {
    format!("diff --git a/{from} b/{to}\nsimilarity index 100%\nrename from {from}\nrename to {to}\n")
}

/// Replace the whole one-line body of `path`.
fn edit_one_line(path: &str, old: &str, new: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,1 @@\n-{old}\n+{new}\n")
}

/// Propose one wiki patch and approve it - the single-operator path.
async fn apply_patch(w: &WalletHandle, patch: &str) {
    let id = match w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "x" }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    w.execute(Command::Approve { proposal: id })
        .await
        .expect("approve");
}

/// The health read, waiting out the off-actor index build (the honest
/// refusal every graph read carries).
async fn health(w: &WalletHandle, limit: u32) -> Reply {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match w.execute(Command::WikiHealth { limit }).await {
            Ok(reply) => return reply,
            Err(MoltError::IndexBuilding { .. }) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the wiki index never finished building"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("wiki_health: {e}"),
        }
    }
}

async fn changes(w: &WalletHandle, since_rev: u64, limit: u32, cursor: u32) -> Reply {
    w.execute(Command::WikiChanges {
        since_rev,
        limit,
        cursor,
    })
    .await
    .expect("wiki_changes")
}

/// The dangling names and the orphan paths, in the order the read serves
/// them.
fn hygiene(reply: &Reply) -> (Vec<String>, Vec<Vec<String>>, Vec<String>) {
    match reply {
        Reply::WikiHealth {
            dangling, orphans, ..
        } => (
            dangling.iter().map(|d| d.name.clone()).collect(),
            dangling.iter().map(|d| d.from.clone()).collect(),
            orphans.clone(),
        ),
        other => panic!("unexpected: {other:?}"),
    }
}

fn change_list(reply: &Reply) -> Vec<WikiChange> {
    match reply {
        Reply::WikiChanges { changes, .. } => changes.clone(),
        other => panic!("unexpected: {other:?}"),
    }
}

const A_LINKS_B: &str = "---\nsee: \"[[b]]\"\n---";

/// **Keystone 1**: deleting a document turns every edge that pointed at it
/// dangling, and the report names both the missing target and who still
/// references it - the "what should I write next" signal.
#[tokio::test]
async fn a_deleted_target_turns_its_in_edges_dangling_and_the_report_says_so() {
    let w = spawn_solo();
    apply_patch(&w, &add("notes/a.md", A_LINKS_B)).await;
    apply_patch(&w, &add("notes/b.md", "# B")).await;

    let (dangling, _, orphans) = hygiene(&health(&w, 0).await);
    assert!(dangling.is_empty(), "the target exists: {dangling:?}");
    assert_eq!(orphans, vec!["notes/a.md".to_string()], "notes/b.md is pointed at");

    apply_patch(&w, &delete("notes/b.md", "# B")).await;

    let reply = health(&w, 0).await;
    let (dangling, from, orphans) = hygiene(&reply);
    assert_eq!(dangling, vec!["b".to_string()]);
    assert_eq!(from, vec![vec!["notes/a.md".to_string()]]);
    assert_eq!(orphans, vec!["notes/a.md".to_string()]);
    match reply {
        Reply::WikiHealth {
            dangling_total,
            orphans_total,
            ..
        } => assert_eq!((dangling_total, orphans_total), (1, 1)),
        other => panic!("unexpected: {other:?}"),
    }
}

/// **Keystone 2**: a rename gives a dangling name its document, so the
/// renamed path leaves the orphan list - and `wiki_changes` says where it
/// came from.
#[tokio::test]
async fn a_rename_moves_a_path_out_of_the_orphan_list() {
    let w = spawn_solo();
    apply_patch(&w, &add("notes/a.md", A_LINKS_B)).await;
    apply_patch(&w, &add("notes/old.md", "# Old")).await;

    let (dangling, _, orphans) = hygiene(&health(&w, 0).await);
    assert_eq!(dangling, vec!["b".to_string()]);
    assert_eq!(orphans, vec!["notes/a.md".to_string(), "notes/old.md".to_string()]);

    apply_patch(&w, &rename("notes/old.md", "notes/b.md")).await;

    let (dangling, _, orphans) = hygiene(&health(&w, 0).await);
    assert!(dangling.is_empty(), "the rename resolved it: {dangling:?}");
    assert_eq!(
        orphans,
        vec!["notes/a.md".to_string()],
        "notes/b.md is pointed at now, notes/old.md is gone"
    );

    let list = change_list(&changes(&w, 0, 0, 0).await);
    let moved = list
        .iter()
        .find(|c| c.path == "notes/b.md")
        .expect("the rename is reported");
    assert_eq!(moved.kind, "renamed");
    assert_eq!(moved.from.as_deref(), Some("notes/old.md"));
    assert!(
        !list.iter().any(|c| c.path == "notes/old.md"),
        "one entry per path: the old path rides the rename's `from`"
    );
}

/// **Keystone 3**: `since_rev` answers exactly the paths the FOLD touched
/// above that revision - coalesced to one entry per path, with the latest
/// kind and the revision it last moved at.
#[tokio::test]
async fn since_rev_answers_exactly_the_paths_the_fold_touched() {
    let w = spawn_solo();
    let patches = [
        add("notes/a.md", "one"),
        add("notes/b.md", "two"),
        edit_one_line("notes/a.md", "one", "ONE"),
    ];
    for p in &patches {
        apply_patch(&w, p).await;
    }

    // the fold's own answer, computed from the patch bytes alone
    let touched = |since: usize| -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for p in patches.iter().skip(since) {
            set.extend(molt_core::wiki_fold::touched_paths(
                &molt_core::wiki_fold::parse_patch(p),
            ));
        }
        set.into_iter().collect()
    };

    for since in 0..=3u64 {
        let reply = changes(&w, since, 0, 0).await;
        let mut paths: Vec<String> = change_list(&reply).into_iter().map(|c| c.path).collect();
        paths.sort();
        assert_eq!(
            paths,
            touched(usize::try_from(since).expect("a small revision")),
            "since_rev {since}"
        );
        match reply {
            Reply::WikiChanges {
                wiki_rev,
                total,
                truncated,
                base,
                ..
            } => {
                assert_eq!(wiki_rev, 3);
                assert_eq!(usize::try_from(total).expect("total"), paths.len());
                assert!(!truncated, "nothing is folded away here");
                assert!(base.is_none(), "this republic has never cut");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // coalesced: notes/a.md was added at 1 and edited at 3, and reads as ONE
    // entry carrying the latest kind
    let list = change_list(&changes(&w, 0, 0, 0).await);
    assert_eq!(
        list.iter()
            .map(|c| (c.path.as_str(), c.kind.as_str(), c.rev))
            .collect::<Vec<_>>(),
        vec![("notes/b.md", "added", 2), ("notes/a.md", "modified", 3)],
        "rev-ordered, one entry per path"
    );

    // …and a revision at or above the current one is an empty PAGE
    assert!(change_list(&changes(&w, 3, 0, 0).await).is_empty());
}

/// Both reads page, and both say how much they cut.
#[tokio::test]
async fn the_maintenance_reads_page_and_report_their_totals() {
    let w = spawn_solo();
    apply_patch(&w, &add("notes/a.md", A_LINKS_B)).await;
    let c = "---\nsee: \"[[ghost]]\"\nStatus: draft\nstatus: draft\n---";
    apply_patch(&w, &add("notes/c.md", c)).await;

    let first = changes(&w, 0, 1, 0).await;
    let (list, cursor, total) = match &first {
        Reply::WikiChanges {
            changes,
            next_cursor,
            total,
            ..
        } => (changes.clone(), *next_cursor, *total),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(list.len(), 1);
    assert_eq!(total, 2);
    assert_eq!(cursor, Some(1));
    let rest = change_list(&changes(&w, 0, 1, cursor.expect("a cursor")).await);
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0].path, list[0].path);

    // the health lists are capped one by one, each with its own total
    match health(&w, 1).await {
        Reply::WikiHealth {
            dangling,
            dangling_total,
            orphans,
            orphans_total,
            key_drift,
            key_drift_total,
            ..
        } => {
            assert_eq!((dangling.len(), dangling_total), (1, 2));
            assert_eq!((orphans.len(), orphans_total), (1, 2));
            assert_eq!(
                (key_drift.len(), key_drift_total),
                (1, 1),
                "`Status` and `status` are one group"
            );
            assert_eq!(
                key_drift[0],
                vec!["Status".to_string(), "status".to_string()]
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}
