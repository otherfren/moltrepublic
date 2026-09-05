// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **What an agent can WRITE to the wiki**, over a real engine
//! (`docs/memory/wiki_semantic_gaps.md` §7 step 5): structured edits
//! against the current base instead of a hand-written positional diff,
//! and a refusal AT THE CALL when one of them cannot land.

use std::time::Duration;

use molt_core::{Command, GroupConfig, MoltError, Reply, SessionView, Surface, WikiEdit};
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

/// The graph is built OFF the actor, so a read (and an `add_relation`,
/// which resolves against it) may refuse until it is there.
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

/// Edit and approve: the proposal is a real threshold vote, m = 1 here.
async fn edit(w: &WalletHandle, edits: Vec<WikiEdit>) -> Result<(), MoltError> {
    let reply = settle(w, Command::WikiEdit { edits }).await?;
    let Reply::Proposed { id, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    w.execute(Command::Approve { proposal: id }).await?;
    Ok(())
}

/// The refusal text of a rejected edit set.
async fn refusal(w: &WalletHandle, edits: Vec<WikiEdit>) -> String {
    match settle(w, Command::WikiEdit { edits }).await {
        Err(e) => e.to_string(),
        Ok(other) => panic!("expected a refusal, got {other:?}"),
    }
}

async fn doc(w: &WalletHandle, path: &str) -> String {
    match settle(w, Command::WikiGet { path: path.to_string() })
        .await
        .expect("the document is there")
    {
        Reply::WikiDocument { content, .. } => content,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn open_proposals(w: &WalletHandle) -> usize {
    match w.execute(Command::ListProposals).await.expect("list") {
        Reply::Proposals { proposals } => proposals.len(),
        other => panic!("unexpected: {other:?}"),
    }
}

fn content(path: &str, body: &str) -> WikiEdit {
    WikiEdit::Content {
        path: path.to_string(),
        content: body.to_string(),
    }
}

/// One call, every op: the engine folds them onto a working copy in
/// order, diffs it against the base and proposes THAT patch - so an
/// agent contributes without ever writing a hunk header.
#[tokio::test]
async fn every_edit_kind_lands_in_the_base() {
    let w = spawn_solo();
    edit(
        &w,
        vec![
            content("acme.md", "# Acme\n"),
            content("people/anna.md", "# Anna\n\nWorks somewhere.\n"),
            content("old.md", "# Old\n"),
            content("gone.md", "# Gone\n"),
        ],
    )
    .await
    .expect("the first documents");

    edit(
        &w,
        vec![
            WikiEdit::Replace {
                path: "people/anna.md".to_string(),
                old: "Works somewhere.".to_string(),
                new: "Works at the office.".to_string(),
            },
            WikiEdit::SetProps {
                path: "people/anna.md".to_string(),
                props: serde_json::json!({ "type": "person", "tags": ["berlin"] })
                    .as_object()
                    .expect("an object")
                    .clone(),
            },
            WikiEdit::AddRelation {
                path: "people/anna.md".to_string(),
                predicate: "works_at".to_string(),
                target: "acme.md".to_string(),
                display: Some("Acme".to_string()),
            },
            WikiEdit::Rename {
                from: "old.md".to_string(),
                to: "notes/new.md".to_string(),
            },
            WikiEdit::Delete {
                path: "gone.md".to_string(),
            },
        ],
    )
    .await
    .expect("the second changeset");

    assert_eq!(
        doc(&w, "people/anna.md").await,
        "---\ntags:\n  - \"berlin\"\ntype: \"person\"\n---\n# Anna\n\nWorks at the office.\n\n[[works_at::acme.md|Acme]]\n"
    );
    assert_eq!(doc(&w, "notes/new.md").await, "# Old\n");
    let missing = settle(&w, Command::WikiGet { path: "gone.md".to_string() }).await;
    assert!(missing.is_err(), "the deletion landed: {missing:?}");

    // …and the relation is a REAL edge, the same one a header key writes
    let Reply::WikiLinks { edges, .. } = settle(
        &w,
        Command::WikiLinks {
            path: "people/anna.md".to_string(),
            direction: Some("out".to_string()),
            predicate: Some("works_at".to_string()),
            limit: 0,
            cursor: 0,
        },
    )
    .await
    .expect("links")
    else {
        panic!("wrong reply")
    };
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].path, "acme.md");
}

/// The feedback loop the positional diff lacked: an edit that cannot land
/// is refused AT THE CALL, with the reason - and nothing at all is
/// proposed, not even the edits that would have worked.
#[tokio::test]
async fn an_edit_that_cannot_land_is_refused_and_proposes_nothing() {
    let w = spawn_solo();
    edit(&w, vec![content("a.md", "one\ntwo\none\n")])
        .await
        .expect("a document");
    let before = open_proposals(&w).await;

    let ambiguous = refusal(
        &w,
        vec![
            content("fresh.md", "# Fresh\n"),
            WikiEdit::Replace {
                path: "a.md".to_string(),
                old: "one".to_string(),
                new: "eins".to_string(),
            },
        ],
    )
    .await;
    assert!(
        ambiguous.contains("occurs 2 times in a.md"),
        "say how often: {ambiguous}"
    );
    assert_eq!(open_proposals(&w).await, before, "nothing was proposed");
    assert_eq!(doc(&w, "a.md").await, "one\ntwo\none\n", "and nothing wrote");

    for (edits, want) in [
        (
            vec![WikiEdit::Replace {
                path: "a.md".to_string(),
                old: "drei".to_string(),
                new: "x".to_string(),
            }],
            "old not found in a.md",
        ),
        (
            vec![WikiEdit::Delete {
                path: "nope.md".to_string(),
            }],
            "no such document: nope.md",
        ),
        (
            vec![WikiEdit::Rename {
                from: "a.md".to_string(),
                to: "a.md".to_string(),
            }],
            "already exists: a.md",
        ),
        (
            vec![WikiEdit::Rename {
                from: "a.md".to_string(),
                to: "/x.md".to_string(),
            }],
            "invalid path: /x.md",
        ),
        (
            vec![WikiEdit::SetProps {
                path: "a.md".to_string(),
                props: serde_json::json!({ "ok": true })
                    .as_object()
                    .expect("an object")
                    .clone(),
            }],
            "ok: value outside the header subset",
        ),
        (
            vec![WikiEdit::AddRelation {
                path: "a.md".to_string(),
                predicate: "works at".to_string(),
                target: "a.md".to_string(),
                display: None,
            }],
            "not a relation key: works at",
        ),
        (
            vec![WikiEdit::AddRelation {
                path: "a.md".to_string(),
                predicate: "knows".to_string(),
                target: "Nobody".to_string(),
                display: None,
            }],
            "no document named Nobody",
        ),
        (
            vec![content("a.md", "one\ntwo\none\n")],
            "nothing to change",
        ),
        (Vec::new(), "no edits"),
    ] {
        let got = refusal(&w, edits).await;
        assert!(got.contains(want), "wanted `{want}`, got `{got}`");
    }
    assert_eq!(open_proposals(&w).await, before, "no edit ever proposed");
}

/// A header the parser would not read back is refused rather than
/// written: the fold does not read headers, so a broken one would only
/// surface as a document that lost its properties.
#[tokio::test]
async fn a_header_the_parser_cannot_read_refuses_the_edit() {
    let w = spawn_solo();
    edit(&w, vec![content("a.md", "---\n- not a mapping\n---\n# A\n")])
        .await
        .expect("a document with a broken header");
    let got = refusal(
        &w,
        vec![WikiEdit::SetProps {
            path: "a.md".to_string(),
            props: serde_json::json!({ "type": "note" })
                .as_object()
                .expect("an object")
                .clone(),
        }],
    )
    .await;
    assert!(got.contains("header unreadable"), "{got}");
}

/// `wiki_resolve` says the ambiguity out loud - the check an agent needs
/// before it writes a link, since resolution is case-exact and silent.
#[tokio::test]
async fn wiki_resolve_names_both_candidates_for_an_ambiguous_basename() {
    let w = spawn_solo();
    edit(
        &w,
        vec![
            content("people/anna.md", "---\naliases: [Chefin]\n---\n# Anna\n"),
            content("orga/anna.md", "# Anna, Orga\n"),
            content("acme.md", "# Acme\n"),
        ],
    )
    .await
    .expect("three documents");

    let resolve = |name: &str| Command::WikiResolve {
        name: name.to_string(),
    };
    let Reply::WikiResolve {
        exact, candidates, ..
    } = settle(&w, resolve("anna")).await.expect("resolve")
    else {
        panic!("wrong reply")
    };
    assert_eq!(exact, None, "two documents, no binding");
    let named: Vec<(String, String)> = candidates
        .iter()
        .map(|c| (c.path.clone(), c.via.clone()))
        .collect();
    assert_eq!(
        named,
        [
            ("orga/anna.md".to_string(), "basename".to_string()),
            ("people/anna.md".to_string(), "basename".to_string()),
        ]
    );

    // the unambiguous ones: a path, an alias, and the "did you mean"
    for (name, want, via) in [
        ("acme.md", "acme.md", "path"),
        ("acme", "acme.md", "basename"),
        ("Chefin", "people/anna.md", "alias"),
    ] {
        let Reply::WikiResolve {
            exact, candidates, ..
        } = settle(&w, resolve(name)).await.expect("resolve")
        else {
            panic!("wrong reply")
        };
        assert_eq!(exact.as_deref(), Some(want), "{name}");
        assert_eq!(candidates.first().map(|c| c.via.as_str()), Some(via), "{name}");
    }
    let Reply::WikiResolve {
        exact, candidates, ..
    } = settle(&w, resolve("ACME")).await.expect("resolve")
    else {
        panic!("wrong reply")
    };
    assert_eq!(exact, None, "resolution is case-exact");
    assert_eq!(
        candidates
            .iter()
            .map(|c| (c.path.as_str(), c.via.as_str()))
            .collect::<Vec<_>>(),
        [("acme.md", "case")],
        "…but the real spelling is offered"
    );
}

/// The other half of the feedback loop (§1.1): a RAW `wiki_patch` that
/// does not apply to the current base is refused at propose instead of
/// becoming a vote that dies as VOID.
#[tokio::test]
async fn a_raw_patch_that_does_not_apply_is_refused_at_propose() {
    let w = spawn_solo();
    edit(&w, vec![content("a.md", "hello\nworld\n")])
        .await
        .expect("a document");
    let stale = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n hello\n-welt\n+monde\n";
    let err = w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "wiki_patch", "summary": "x", "value": stale }),
        })
        .await
        .expect_err("a stale patch is refused");
    assert!(
        err.to_string().contains("patch does not apply"),
        "with the reason: {err}"
    );
    // …and a good one still proposes
    let good = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n hello\n-world\n+welt\n";
    w.execute(Command::Propose {
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "wiki_patch", "summary": "x", "value": good }),
    })
    .await
    .expect("a patch that applies still proposes");
}
