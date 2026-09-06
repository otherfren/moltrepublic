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
    w.execute(Command::Approve { proposal: id, note: None })
        .await
        .expect("approve");
}

/// E4: the graph reads answer `index_building: true` with empty lists
/// instead of refusing, so a test waits that out too.
fn still_building(reply: &Reply) -> bool {
    matches!(
        reply,
        Reply::WikiHealth { index_building: true, .. }
            | Reply::WikiNeighbors { index_building: true, .. }
            | Reply::WikiSearch { index_building: true, .. }
    )
}

/// The health read, waiting out the off-actor index build.
async fn health(w: &WalletHandle, limit: u32) -> Reply {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match w.execute(Command::WikiHealth { limit }).await {
            Ok(reply) if !still_building(&reply) => return reply,
            Ok(_) | Err(MoltError::IndexBuilding { .. }) => {
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

    // from revision 2 the caller HELD notes/old.md, so the move is the news
    let list = change_list(&changes(&w, 2, 0, 0).await);
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

    // …and to a caller that never held it, the document is simply NEW at
    // the path it ended up on
    let list = change_list(&changes(&w, 0, 0, 0).await);
    let moved = list
        .iter()
        .find(|c| c.path == "notes/b.md")
        .expect("still one entry");
    assert_eq!((moved.kind.as_str(), moved.from.as_deref()), ("added", None));
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
    // entry - `added`, because to a caller at revision 0 it is simply new
    let list = change_list(&changes(&w, 0, 0, 0).await);
    assert_eq!(
        list.iter()
            .map(|c| (c.path.as_str(), c.kind.as_str(), c.rev))
            .collect::<Vec<_>>(),
        vec![("notes/b.md", "added", 2), ("notes/a.md", "added", 3)],
        "rev-ordered, one entry per path"
    );
    // …while a caller that already HAD it is told it changed
    let list = change_list(&changes(&w, 1, 0, 0).await);
    assert_eq!(
        list.iter()
            .map(|c| (c.path.as_str(), c.kind.as_str(), c.rev))
            .collect::<Vec<_>>(),
        vec![("notes/b.md", "added", 2), ("notes/a.md", "modified", 3)]
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

/// **E3 (R6/R25)**: `key_drift` sees two SPELLINGS of one key. It cannot
/// see a WRONG key - seven `person` pages carrying `year` while the rest
/// carry `notable_year` read as clean. `props_by_type` is the histogram
/// that shows it in one call.
#[tokio::test]
async fn wiki_health_counts_the_keys_each_type_carries() {
    let w = spawn_solo();
    for n in 0..3 {
        apply_patch(
            &w,
            &add(
                &format!("menschen/a{n}.md"),
                "---\ntype: person\nnotable_year: 1970\n---\n# A\n",
            ),
        )
        .await;
    }
    apply_patch(
        &w,
        &add("menschen/odd.md", "---\ntype: person\nyear: 1970\n---\n# Odd\n"),
    )
    .await;
    apply_patch(
        &w,
        &add("orgs/x.md", "---\ntype: org\nfounded: 1999\n---\n# X\n"),
    )
    .await;

    let Reply::WikiHealth { props_by_type, props_by_type_total, .. } = health(&w, 0).await else {
        panic!("not a health reply");
    };
    assert_eq!(props_by_type_total, 2, "person and org");
    let person = props_by_type
        .iter()
        .find(|t| t.kind == "person")
        .expect("the person group");
    assert_eq!(person.pages, 4);
    let keys: Vec<(&str, u64)> = person.keys.iter().map(|k| (k.key.as_str(), k.pages)).collect();
    assert!(keys.contains(&("notable_year", 3)), "{keys:?}");
    assert!(keys.contains(&("year", 1)), "the odd one out is visible: {keys:?}");
    assert!(keys.contains(&("type", 4)), "{keys:?}");
}

/// **E3 (R25)**: a relation asserted in the wrong DIRECTION reads
/// correctly in prose and is invisible to every other check. The wiki's
/// own distribution is the only norm there is, so the group is a
/// HEURISTIC: `authored_by` runs work -> person eight times and
/// person -> work twice, and the two are what a maintainer looks at.
#[tokio::test]
async fn wiki_health_flags_a_relation_asserted_backwards() {
    let w = spawn_solo();
    apply_patch(&w, &add("menschen/anna.md", "---\ntype: person\n---\n# Anna\n")).await;
    for n in 0..8 {
        apply_patch(
            &w,
            &add(
                &format!("werke/w{n}.md"),
                "---\ntype: work\n---\nWritten by [[authored_by::menschen/anna.md]].\n",
            ),
        )
        .await;
    }
    // …and two person pages that assert it the other way round
    for n in 0..2 {
        apply_patch(
            &w,
            &add(
                &format!("menschen/b{n}.md"),
                "---\ntype: person\n---\nWrote [[authored_by::werke/w0.md]].\n",
            ),
        )
        .await;
    }

    let Reply::WikiHealth { direction_outliers, direction_outliers_total, .. } = health(&w, 0).await
    else {
        panic!("not a health reply");
    };
    assert_eq!(direction_outliers_total, 1, "{direction_outliers:?}");
    let odd = &direction_outliers[0];
    assert_eq!(odd.predicate, "authored_by");
    assert_eq!(odd.subject_type, "person");
    assert_eq!(odd.object_type, "work");
    assert_eq!(odd.count, 2);
    assert_eq!(odd.usual, "work -> person");
    assert_eq!(odd.usual_count, 8);
    assert_eq!(
        odd.from,
        vec!["menschen/b0.md".to_string(), "menschen/b1.md".to_string()]
    );
}
