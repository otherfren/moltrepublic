// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **What an agent can ASK the wiki**, over a real engine
//! (`docs_archive/memory/knowledge_base_scale.md` §4.5/§4.6): a page is found
//! under the name its header declares, a scalar property is a query path,
//! and a traversal says WHY two documents are related instead of only
//! that they are.

use std::time::Duration;

use molt_core::{Command, GroupConfig, MoltError, Reply, SessionView, Surface};
use molt_engine::WalletHandle;

/// A single-member group: threshold 1, so one `Approve` applies a patch.
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

/// A git patch that creates one document.
fn add(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for l in lines {
        patch.push('+');
        patch.push_str(l);
        patch.push('\n');
    }
    patch
}

/// Propose one wiki patch and approve it (the single-operator path).
async fn write_doc(w: &WalletHandle, path: &str, content: &str) {
    let id = match w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({
                "op": "wiki_patch",
                "value": add(path, content),
                "summary": path,
            }),
        })
        .await
        .expect("propose the patch")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    w.execute(Command::Approve { proposal: id })
        .await
        .expect("approve");
}

/// Both indexes are built OFF the actor, so the first read refuses with
/// `IndexBuilding` - which is a different claim from "nothing matched".
async fn read(w: &WalletHandle, cmd: Command) -> Reply {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match w.execute(cmd.clone()).await {
            Err(MoltError::IndexBuilding { .. }) => {}
            other => return other.expect("the wiki read answers"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wiki index never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn hits(reply: Reply) -> Vec<String> {
    match reply {
        Reply::WikiSearch { hits, .. } => hits.into_iter().map(|h| h.path).collect(),
        other => panic!("unexpected: {other:?}"),
    }
}

fn search(query: &str, props: &[(&str, &str)]) -> Command {
    Command::WikiSearch {
        query: query.to_string(),
        tags: Vec::new(),
        kind: None,
        folder: None,
        props: props
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        limit: 0,
        cursor: 0,
    }
}

/// Step 1 (recall): the header is part of the searchable document, and its
/// pairs are a filter - the two ways an agent that starts from a NAME or
/// from a property finds the page at all.
#[tokio::test]
async fn a_page_is_found_by_what_its_header_declares() {
    let w = spawn_solo();
    write_doc(
        &w,
        "people/mueller.md",
        "---\ntype: person\nstatus: draft\naliases: [P. Müller, Müller]\nworks_at: \"[[acme.md]]\"\n---\n# P.\nSchreibt Berichte.\n",
    )
    .await;
    write_doc(
        &w,
        "people/bob.md",
        "---\ntype: person\nstatus: final\n---\n# Bob\nSchreibt nichts.\n",
    )
    .await;
    write_doc(&w, "acme.md", "# Acme\nEine Firma.\n").await;

    // the name a page DECLARES finds it, and so does a header value
    assert_eq!(
        hits(read(&w, search("Müller", &[])).await),
        vec!["people/mueller.md"],
        "an alias is not searchable"
    );
    let mut found = hits(read(&w, search("Acme", &[])).await);
    found.sort();
    assert_eq!(
        found,
        vec!["acme.md", "people/mueller.md"],
        "the header's link target is not searchable"
    );

    // …and a property is a query path, not a reason to read the tree
    assert_eq!(
        hits(read(&w, search("", &[("status", "draft")])).await),
        vec!["people/mueller.md"]
    );
    assert_eq!(
        hits(read(&w, search("", &[("type", "person")])).await).len(),
        2,
        "the reserved keys stay queryable under their own names"
    );
    // an unknown key narrows to nothing rather than to everything
    assert!(hits(read(&w, search("", &[("nope", "draft")])).await).is_empty());
    assert!(hits(read(&w, search("", &[("status", "draft"), ("type", "note")])).await).is_empty());
}

fn neighbors(path: &str, predicate: Option<&str>, direction: Option<&str>, transitive: bool) -> Command {
    Command::WikiNeighbors {
        path: path.to_string(),
        depth: 2,
        limit: 0,
        predicate: predicate.map(str::to_string),
        direction: direction.map(str::to_string),
        transitive,
    }
}

fn near(reply: Reply) -> (Vec<molt_core::WikiNeighbor>, bool) {
    match reply {
        Reply::WikiNeighbors { docs, capped, .. } => (docs, capped),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Step 2 (traversal that says WHY): a chain of four under one predicate.
/// The bounded walk stops at the depth, the closure runs to the end, and
/// every hit names the edge that reached it.
#[tokio::test]
async fn a_traversal_says_under_which_relation_it_reached_a_document() {
    let w = spawn_solo();
    for (path, content) in [
        ("a.md", "---\npart_of: \"[[b.md]]\"\nknows: \"[[far.md]]\"\n---\n# A\n"),
        ("b.md", "---\npart_of: \"[[c.md]]\"\n---\n# B\n"),
        ("c.md", "---\npart_of: \"[[d.md]]\"\n---\n# C\n"),
        ("d.md", "# D\n"),
        ("far.md", "# Far\n"),
    ] {
        write_doc(&w, path, content).await;
    }

    // the depth bound holds, and the other relation is not this walk
    let (docs, capped) = near(read(&w, neighbors("a.md", Some("part_of"), Some("out"), false)).await);
    assert_eq!(
        docs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
        vec!["b.md", "c.md"]
    );
    assert!(!capped);
    assert_eq!(docs[1].predicate.as_deref(), Some("part_of"));
    assert_eq!(docs[1].direction, "out");
    assert_eq!(docs[1].via, vec!["b.md".to_string()], "the route it came through");

    // …and the closure walks that ONE predicate to the end
    let (docs, _) = near(read(&w, neighbors("a.md", Some("part_of"), Some("out"), true)).await);
    assert_eq!(
        docs.iter().map(|d| (d.path.as_str(), d.distance)).collect::<Vec<_>>(),
        vec![("b.md", 1), ("c.md", 2), ("d.md", 3)]
    );
    // the other way round is the other question: what belongs to d
    let (docs, _) = near(read(&w, neighbors("d.md", Some("part_of"), Some("in"), true)).await);
    assert_eq!(
        docs.iter().map(|d| (d.path.as_str(), d.direction.as_str())).collect::<Vec<_>>(),
        vec![("c.md", "in"), ("b.md", "in"), ("a.md", "in")]
    );

    // a closure over "some relation" is refused, not guessed
    let err = w
        .execute(neighbors("a.md", None, None, true))
        .await
        .expect_err("transitive without a predicate is refused");
    assert!(
        matches!(&err, MoltError::BadPayload(m) if m.contains("predicate")),
        "the refusal names the missing half: {err:?}"
    );
    assert!(w
        .execute(neighbors("a.md", None, Some("sideways"), false))
        .await
        .is_err());
}
