// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **What an agent can WRITE to the wiki**, over a real engine
//! (`docs_archive/memory/wiki_semantic_gaps.md` §7 step 5): structured edits
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
    let reply = settle(w, Command::WikiEdit { edits, dry_run: false, allow_warnings: false, supersedes: None }).await?;
    let Reply::Proposed { id, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    w.execute(Command::Approve { proposal: id, note: None }).await?;
    Ok(())
}

/// The refusal text of a rejected edit set.
async fn refusal(w: &WalletHandle, edits: Vec<WikiEdit>) -> String {
    match settle(w, Command::WikiEdit { edits, dry_run: false, allow_warnings: false, supersedes: None }).await {
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

/// G11: `replace` matches a substring, so the `## Quellen` anchor sits
/// inside every `### Quellen (Sitz B)` block too. A repeated one-line
/// `old` names the lines it hit - without them the caller cannot see
/// which occurrence is the surprise.
#[tokio::test]
async fn a_repeated_one_line_old_names_the_lines_it_hit() {
    let w = spawn_solo();
    edit(
        &w,
        vec![content(
            "q.md",
            "# Q\n\n## Quellen\n\n- a\n\n### Quellen (Sitz B)\n\n- b\n",
        )],
    )
    .await
    .expect("a document");

    let got = refusal(
        &w,
        vec![WikiEdit::Replace {
            path: "q.md".to_string(),
            old: "## Quellen".to_string(),
            new: "## Q2".to_string(),
        }],
    )
    .await;
    assert!(
        got.contains("occurs 2 times in q.md (lines 3, 7)"),
        "name where it hit: {got}"
    );

    // an `old` that already spans lines cannot be pinned to one of them
    let multi = refusal(
        &w,
        vec![WikiEdit::Replace {
            path: "q.md".to_string(),
            old: "\n\n- ".to_string(),
            new: "\nX".to_string(),
        }],
    )
    .await;
    assert!(
        multi.contains("occurs 2 times in q.md -") && !multi.contains("lines"),
        "no line list for a multi-line old: {multi}"
    );
}

/// A one-key `set_props` must touch ONE line. The header is a member's
/// text: re-emitting it whole would hand the voters a diff over the whole
/// block for a one-key change, and they would have to read all of it to
/// see that nothing else moved.
#[tokio::test]
async fn set_props_changes_one_line_of_a_written_header() {
    let w = spawn_solo();
    let before = "---\ntype: person\n# who they are\naliases: [P. Mueller, Mueller]\ntags:\n  - berlin\n  - gruender\nborn: 1975\n---\n# P.\n\nSchreibt.\n";
    edit(&w, vec![content("p.md", before)])
        .await
        .expect("a document with a written header");
    edit(
        &w,
        vec![WikiEdit::SetProps {
            path: "p.md".to_string(),
            props: serde_json::json!({ "born": 1976 })
                .as_object()
                .expect("an object")
                .clone(),
        }],
    )
    .await
    .expect("one scalar");

    let after = doc(&w, "p.md").await;
    let changed: Vec<(&str, &str)> = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        changed,
        [("born: 1975", "born: 1976")],
        "one line, and it is the key's:\n{after}"
    );
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "no line added or dropped:\n{after}"
    );

    // …and a NEW key is appended, still leaving the rest byte-identical
    edit(
        &w,
        vec![WikiEdit::SetProps {
            path: "p.md".to_string(),
            props: serde_json::json!({ "status": "draft", "type": null })
                .as_object()
                .expect("an object")
                .clone(),
        }],
    )
    .await
    .expect("one added, one removed");
    let last = doc(&w, "p.md").await;
    assert!(last.contains("# who they are"), "the comment survives: {last}");
    assert!(!last.contains("type: person"), "the removal landed: {last}");
    assert!(
        last.contains("born: 1976\nstatus: \"draft\"\n"),
        "a new key is appended after the existing ones: {last}"
    );
}

/// A header the parser would not read back is refused rather than
/// written: the fold does not read headers, so a broken one would only
/// surface as a document that lost its properties.
#[tokio::test]
async fn a_header_the_parser_cannot_read_refuses_the_edit() {
    let w = spawn_solo();
    // B5 refuses such a header by default; the fixture asks for it
    let reply = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("a.md", "---\n- not a mapping\n---\n# A\n")],
            dry_run: false,
            allow_warnings: true,
            supersedes: None,
        },
    )
    .await
    .expect("a document with a broken header");
    let Reply::Proposed { id, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("applies");
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

/// B4 (`docs/reviews/mcp_agent_friction_fixes.md`): a dry run answers the
/// patch, its summary and the warnings, and proposes nothing.
#[tokio::test]
async fn a_dry_run_shows_the_patch_and_proposes_nothing() {
    let w = spawn_solo();
    let edits = vec![content("a.md", "---\ntitle: A\n---\ntext\n")];
    let reply = settle(
        &w,
        Command::WikiEdit { edits, dry_run: true, allow_warnings: false, supersedes: None },
    )
    .await
    .expect("a dry run is fine");
    let Reply::WikiPreview { patch, summary, warnings } = reply else {
        panic!("unexpected: {reply:?}");
    };
    assert!(patch.contains("+++ b/a.md") && patch.contains("+title: A"), "the patch: {patch}");
    assert_eq!(summary, "+1");
    assert!(warnings.is_empty());
    assert_eq!(open_proposals(&w).await, 0, "nothing was proposed");
}

/// B5: a header the parser reads differently than written (an unquoted
/// `[[link]]` value is a nested list) REFUSES the call, naming the key;
/// `allow_warnings` proposes it anyway, with the warning on the reply.
#[tokio::test]
async fn a_header_warning_refuses_unless_allowed() {
    let w = spawn_solo();
    let body = "---\ntitle: A\nsuccessor_of: [[b.md]]\n---\ntext\n";
    let text = refusal(&w, vec![content("a.md", body)]).await;
    assert!(text.contains("successor_of") && text.contains("allow_warnings"), "names the key and the way out: {text}");
    assert_eq!(open_proposals(&w).await, 0);
    let reply = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("a.md", body)],
            dry_run: false,
            allow_warnings: true,
            supersedes: None,
        },
    )
    .await
    .expect("allowed");
    let Reply::Proposed { warnings, channel, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    assert!(warnings.iter().any(|w| w.contains("successor_of")), "the warning rides the reply: {warnings:?}");
    assert!(matches!(channel, molt_core::ChannelRef::Patch { .. }), "the reply names the discussion channel");
    assert_eq!(open_proposals(&w).await, 1);
}

/// B4: `supersedes` withdraws the own open proposal it corrects in the same
/// step; a foreign or decided one refuses the whole call.
#[tokio::test]
async fn a_superseding_edit_withdraws_the_old_proposal() {
    let w = spawn_solo();
    let first = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("a.md", "---\ntitle: A\nprice_eur: 1500\n---\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    .expect("proposes");
    let Reply::Proposed { id: old, .. } = first else {
        panic!("unexpected: {first:?}");
    };
    let second = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("a.md", "---\ntitle: A\nprice_eur: 1499\n---\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: Some(old),
        },
    )
    .await
    .expect("supersedes");
    let Reply::Proposed { id: new, .. } = second else {
        panic!("unexpected: {second:?}");
    };
    assert_ne!(old, new);
    let Reply::Proposals { proposals } = w.execute(Command::ListProposals).await.expect("list") else {
        panic!("list");
    };
    let state = |id| proposals.iter().find(|p| p.id == id).map(|p| (p.state, p.withdrawn));
    assert_eq!(state(old), Some((molt_core::ProposalState::Rejected, true)), "the old one is withdrawn");
    assert_eq!(state(new), Some((molt_core::ProposalState::Proposed, false)));
    // superseding a decided proposal refuses before anything moves
    let text = match settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("b.md", "text\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: Some(old),
        },
    )
    .await
    {
        Err(e) => e.to_string(),
        Ok(other) => panic!("expected a refusal, got {other:?}"),
    };
    assert!(text.contains(&format!("proposal {} is already withdrawn", old.0)) || text.contains("is already rejected"), "{text}");
    assert_eq!(open_proposals(&w).await, 2, "nothing new was proposed");
}

/// B6: a vote answers with the record after it; approving an already
/// applied proposal is the same reply, not an error; an unknown id reads
/// as a number, not a Rust type.
#[tokio::test]
async fn a_vote_answers_with_the_record_and_late_approvals_are_fine() {
    let w = spawn_solo();
    let reply = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("a.md", "text\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    .expect("proposes");
    let Reply::Proposed { id, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    let vote = w.execute(Command::Approve { proposal: id, note: None }).await.expect("approves");
    let Reply::Vote { id: vid, state, approvals, threshold, channel, held_for_secs, head } = vote else {
        panic!("unexpected: {vote:?}");
    };
    assert_eq!((vid, state, approvals, threshold), (id, molt_core::ProposalState::Applied, 1, 1));
    assert_eq!(channel, molt_core::ChannelRef::Patch { id });
    // this solo group is not chain-governed: nothing paces a seal, and
    // there is no chain head to name
    assert_eq!((held_for_secs, head), (None, 0));
    let again = w.execute(Command::Approve { proposal: id, note: None }).await.expect("a late approval is not an error");
    assert!(matches!(again, Reply::Vote { state: molt_core::ProposalState::Applied, .. }));
    let err = w
        .execute(Command::Approve { proposal: molt_core::ProposalId(99_999), note: None })
        .await
        .expect_err("unknown");
    assert_eq!(err.to_string(), "unknown proposal 99999");
}

/// B1 (`mcp_agent_friction_fixes_round_2.md`): `create` is the op that
/// cannot become a rewrite. A free path lands; an occupied one refuses,
/// in the base and in the working copy this call built.
#[tokio::test]
async fn a_create_refuses_an_occupied_path() {
    let w = spawn_solo();
    edit(
        &w,
        vec![WikiEdit::Create {
            path: "a.md".to_string(),
            content: "# A\n".to_string(),
        }],
    )
    .await
    .expect("a free path creates");
    assert_eq!(doc(&w, "a.md").await, "# A\n");

    let text = refusal(
        &w,
        vec![WikiEdit::Create {
            path: "a.md".to_string(),
            content: "# Other\n".to_string(),
        }],
    )
    .await;
    assert!(text.contains("already exists: a.md"), "with the reason: {text}");
    assert_eq!(open_proposals(&w).await, 1, "only the create landed");

    // …and the working copy counts too: a create after this call's own
    // delete of the same path is still a rewrite in disguise
    let text = refusal(
        &w,
        vec![
            WikiEdit::Delete {
                path: "a.md".to_string(),
            },
            WikiEdit::Create {
                path: "a.md".to_string(),
                content: "# Other\n".to_string(),
            },
        ],
    )
    .await;
    assert!(text.contains("already exists: a.md"), "with the reason: {text}");
    // a second create in one batch collides with the first
    let text = refusal(
        &w,
        vec![
            WikiEdit::Create {
                path: "b.md".to_string(),
                content: "# B\n".to_string(),
            },
            WikiEdit::Create {
                path: "b.md".to_string(),
                content: "# B again\n".to_string(),
            },
        ],
    )
    .await;
    assert!(text.contains("already exists: b.md"), "with the reason: {text}");
}

/// B2 (G10): the engine holds every open proposal's paths, so a second
/// writer hears about the collision at the call instead of as a phantom
/// rejection once the other card seals.
#[tokio::test]
async fn a_path_in_an_open_proposal_warns() {
    let w = spawn_solo();
    let first = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("standards/xep.md", "# XEP\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    .expect("proposes");
    let Reply::Proposed { id: open, .. } = first else {
        panic!("unexpected: {first:?}");
    };

    let want = format!("standards/xep.md is in open proposal {} by me", open.0);
    let text = refusal(&w, vec![content("standards/xep.md", "# XEP, differently\n")]).await;
    assert!(text.contains(&want), "names the card and its proposer: {text}");

    // a dry run answers the same warning and proposes nothing
    let preview = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("standards/xep.md", "# XEP, differently\n")],
            dry_run: true,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    .expect("a dry run is fine");
    let Reply::WikiPreview { warnings, .. } = preview else {
        panic!("unexpected: {preview:?}");
    };
    assert!(warnings.iter().any(|s| s.contains(&want)), "{warnings:?}");
    assert_eq!(open_proposals(&w).await, 1, "nothing new was proposed");

    // …and `allow_warnings` writes it anyway
    let reply = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("standards/xep.md", "# XEP, differently\n")],
            dry_run: false,
            allow_warnings: true,
            supersedes: None,
        },
    )
    .await
    .expect("allowed");
    let Reply::Proposed { warnings, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    assert!(warnings.iter().any(|s| s.contains(&want)), "the warning rides the reply: {warnings:?}");
    assert_eq!(open_proposals(&w).await, 2);
}

/// B2's rename half (G19): no-new-dangling-edge is a property of a state,
/// not of one proposal - an open card still writing the old path is what
/// the renamer cannot see.
#[tokio::test]
async fn a_rename_warns_about_a_link_an_open_proposal_still_writes() {
    let w = spawn_solo();
    edit(&w, vec![content("people/anna.md", "# Anna\n"), content("acme.md", "# Acme\n")])
        .await
        .expect("the base");
    let reply = settle(
        &w,
        Command::WikiEdit {
            edits: vec![content("acme.md", "# Acme\n\nRun by [[people/anna]].\n")],
            dry_run: false,
            allow_warnings: false,
            supersedes: None,
        },
    )
    .await
    .expect("proposes");
    let Reply::Proposed { id: open, .. } = reply else {
        panic!("unexpected: {reply:?}");
    };
    let text = refusal(
        &w,
        vec![WikiEdit::Rename {
            from: "people/anna.md".to_string(),
            to: "menschen/anna.md".to_string(),
        }],
    )
    .await;
    assert!(
        text.contains(&format!("leaves a link in open proposal {}", open.0)),
        "names the card: {text}"
    );
}

/// B3 (G7): an alias that already names another page takes the name from
/// both - the check the agents wrote by hand, now at the call.
#[tokio::test]
async fn a_colliding_alias_warns() {
    let w = spawn_solo();
    edit(
        &w,
        vec![content(
            "organisationen/signal-foundation.md",
            "---\naliases:\n  - \"Signal\"\n---\n# Signal Foundation\n",
        )],
    )
    .await
    .expect("the first page");
    // the graph is built off the actor; a read waits for it
    settle(
        &w,
        Command::WikiResolve {
            name: "Signal".to_string(),
        },
    )
    .await
    .expect("resolves");

    let text = refusal(
        &w,
        vec![content(
            "software/signal.md",
            "---\naliases:\n  - \"Signal\"\n---\n# Signal\n",
        )],
    )
    .await;
    assert!(
        text.contains("alias \"Signal\" already names organisationen/signal-foundation.md"),
        "names the name and the page: {text}"
    );
    assert_eq!(open_proposals(&w).await, 1, "only the first page landed");
}
