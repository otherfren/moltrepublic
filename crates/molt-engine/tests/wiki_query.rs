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
