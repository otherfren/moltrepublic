// SPDX-License-Identifier: GPL-3.0-or-later
//! The wiki bridge and the wiki export dialog, headless.

use super::*;

/// The wiki bridge drives the REAL generated `WikiState` face headless:
/// open → edit → close → delete through the same callbacks the pane
/// fires, asserting the models follow. This is the layer the unit tests
/// in `wiki.rs` cannot see (types, models, borrow discipline).
#[test]
fn wiki_bridge_opens_edits_closes_and_deletes_headless() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    // production starts EMPTY; the engine base arrives over the real
    // bridge (base-docs + base-arrived), exactly like the surfaces
    // mirror delivers it
    assert_eq!(g.get_tabs().row_count(), 0);
    assert!(!g.get_doc_open());
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "charter.md".into(),
            content: "# Charter\n\nWhat we agreed to.".into(),
            loaded: true,
        },
        WikiBase {
            path: "glossary.md".into(),
            content: "# Glossary\n\nThe words we keep using.".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    assert_eq!(g.get_nav_rows().row_count(), 2, "the folded base lands");
    assert_eq!(g.get_cs_rows().row_count(), 0, "a clean tree has no panel");
    // open the charter via the open route so a tab exists
    let rows = g.get_nav_rows();
    let charter = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "charter.md")
        .expect("charter row");
    g.invoke_nav_open(charter.id);
    assert_eq!(g.get_tabs().row_count(), 1);
    assert!(g.get_doc_open());
    assert_eq!(g.get_doc_path().as_str(), "charter.md");
    // open glossary.md via the open route
    let rows = g.get_nav_rows();
    let glossary = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "glossary.md")
        .expect("glossary row");
    // a mark must PATCH the row model, never replace it: a swap
    // re-creates the row elements mid-double-click, which is exactly
    // how "double-click does not open" happened live
    g.invoke_nav_mark(glossary.id);
    let rows_after = g.get_nav_rows();
    assert!(
        std::ptr::eq(
            rows.as_any()
                .downcast_ref::<VecModel<WikiNavRow>>()
                .expect("nav rows are a VecModel") as *const _,
            rows_after
                .as_any()
                .downcast_ref::<VecModel<WikiNavRow>>()
                .expect("still a VecModel") as *const _,
        ),
        "the nav model must survive a mark (rows patch in place)"
    );
    g.invoke_nav_open(glossary.id);
    assert_eq!(g.get_tabs().row_count(), 2);
    assert_eq!(g.get_doc_path().as_str(), "glossary.md");
    // a base refresh with the SAME content is a no-op for the models
    g.invoke_base_arrived();
    assert_eq!(g.get_tabs().row_count(), 2);
    assert_eq!(g.get_doc_path().as_str(), "glossary.md");
    // an edit turns up on the changeset stack, the tab status and the
    // preview diff
    g.invoke_edit_toggle();
    let edited = format!("{}\n\nA new closing thought.", g.get_raw());
    g.invoke_edited(edited.into());
    assert_eq!(g.get_cs_rows().row_count(), 1);
    let row = g.get_cs_rows().row_data(0).expect("stack row");
    assert_eq!(row.kind, 5, "an edit row");
    assert_eq!(row.label.as_str(), "glossary.md");
    assert!(g.get_cs_lines() > 0, "touched lines are counted");
    let tabs = g.get_tabs();
    let gtab = (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i))
        .find(|t| t.label.as_str() == "glossary.md")
        .expect("glossary tab");
    assert_eq!(gtab.status, 2, "the tab paints modified");
    g.invoke_edit_toggle();
    let blocks = g.get_blocks();
    assert!(
        (0..blocks.row_count())
            .filter_map(|i| blocks.row_data(i))
            .any(|b| b.status == 1 && b.text.as_str().contains("closing thought")),
        "the appended paragraph previews as Added"
    );
    // Ctrl+W closes glossary; focus falls back to the charter tab
    g.invoke_close_active();
    assert_eq!(g.get_tabs().row_count(), 1);
    assert_eq!(g.get_doc_path().as_str(), "charter.md");
    // Del on the marked (still glossary) row: a pending deletion — the
    // row stays, struck, and the chip carries both changes
    g.invoke_delete_marked();
    let rows = g.get_nav_rows();
    let struck = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "glossary.md")
        .expect("the deleted row stays listed");
    assert_eq!(struck.status, 3);
    // the stack narrates both actions, the NET counts only the delete
    assert_eq!(g.get_cs_rows().row_count(), 2);
    assert_eq!(g.get_cs_deleted(), 1);
    assert_eq!(g.get_cs_lines(), 0, "a deleted file's edits are not lines");
    // undo takes back the deletion (the edit stays pending) …
    g.invoke_cs_undo();
    assert_eq!(g.get_cs_rows().row_count(), 1);
    assert_eq!(g.get_cs_deleted(), 0);
    assert!(g.get_cs_lines() > 0);
    // … a per-file revert clears the file without touching others …
    g.invoke_nav_revert(struck.id);
    assert_eq!(g.get_cs_rows().row_count(), 0, "the panel is gone");
    // … and after fresh changes, revert-all clears everything at once
    g.invoke_new_file();
    g.invoke_new_folder();
    assert_eq!(g.get_cs_rows().row_count(), 2);
    assert_eq!(g.get_cs_added(), 1);
    g.invoke_cs_revert();
    assert_eq!(g.get_cs_rows().row_count(), 0);
    assert_eq!(g.get_cs_added(), 0);
}

// ---- wiki export (docs_archive/memory/wiki_export_plan.md, keystone 5) -------

/// The 💾 button writes the APPROVED tree, so the gate is the folded
/// base - never the local stack, which the export deliberately leaves
/// behind. One place decides it (`WikiState.has-base`), because the
/// toolbar button and the dialog must never disagree.
#[test]
fn the_wiki_export_button_follows_the_approved_base_tree() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let g = ui.global::<WikiState>();

    g.set_base_docs(ModelRc::new(VecModel::from(Vec::<WikiBase>::new())));
    assert!(!g.invoke_has_base(), "an empty base has nothing to export");

    // a local draft alone must NOT arm the button: drafts stay local
    g.set_cs_rows(ModelRc::new(VecModel::from(vec![WikiChangeRow {
        kind: 0,
        label: "notes.md".into(),
    }])));
    assert!(!g.invoke_has_base(), "a local draft is not an approved tree");

    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "charter.md".into(),
        content: "hello".into(),
        loaded: true,
    }])));
    assert!(g.invoke_has_base(), "one approved doc arms the export");
}

/// The dialog's drafts line appears only when there IS a local stack -
/// telling a user with nothing pending that nothing pending stays local
/// is noise.
#[test]
fn the_export_dialog_counts_only_a_non_empty_local_stack() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let g = ui.global::<WikiState>();

    g.set_cs_rows(ModelRc::new(VecModel::from(Vec::<WikiChangeRow>::new())));
    assert_eq!(g.invoke_draft_count(), 0, "no stack, no line");

    g.set_cs_rows(ModelRc::new(VecModel::from(vec![
        WikiChangeRow {
            kind: 0,
            label: "a.md".into(),
        },
        WikiChangeRow {
            kind: 5,
            label: "b.md".into(),
        },
    ])));
    assert_eq!(g.invoke_draft_count(), 2, "the line names the real count");
}

/// The outcome toast is built from the engine's own export state, in
/// both languages, and stays silent while the export is idle or still
/// running (a toast per session push would repeat forever).
#[test]
fn the_wiki_export_toast_carries_the_real_outcome() {
    let idle = molt_core::ExportState::default();
    assert!(
        super::wiki_export_toast(0, &idle).is_none(),
        "nothing happened yet"
    );

    let running = molt_core::ExportState {
        running: true,
        dest: "/tmp/x".to_string(),
        ..molt_core::ExportState::default()
    };
    assert!(
        super::wiki_export_toast(0, &running).is_none(),
        "no verdict while it runs"
    );

    let ok = molt_core::ExportState {
        result: "ok".to_string(),
        files: 12,
        ..molt_core::ExportState::default()
    };
    let (msg, failed) = super::wiki_export_toast(0, &ok).expect("a verdict");
    assert!(!failed);
    assert_eq!(msg, "wiki exported: 12 files");
    let (de, _) = super::wiki_export_toast(1, &ok).expect("a verdict");
    assert_eq!(de, "Wiki exportiert: 12 Dateien");

    // the singular is not "1 files"
    let one = molt_core::ExportState {
        result: "ok".to_string(),
        files: 1,
        ..molt_core::ExportState::default()
    };
    assert_eq!(
        super::wiki_export_toast(0, &one).expect("a verdict").0,
        "wiki exported: 1 file"
    );
    assert_eq!(
        super::wiki_export_toast(1, &one).expect("a verdict").0,
        "Wiki exportiert: 1 Datei"
    );

    // a failure is surfaced verbatim, in the error tone
    let bad = molt_core::ExportState {
        result: "error: dest is not a directory".to_string(),
        ..molt_core::ExportState::default()
    };
    let (msg, failed) = super::wiki_export_toast(0, &bad).expect("a verdict");
    assert!(failed, "a failure toasts in the error tone");
    assert!(
        msg.contains("dest is not a directory"),
        "the real reason survives: {msg}"
    );
}

/// The same outcome must toast ONCE: `apply_session` runs on every
/// engine change, and a settled export state stays settled.
#[test]
fn a_settled_wiki_export_toasts_once_not_on_every_push() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let sv = SessionView {
        wiki_export: molt_core::ExportState {
            result: "ok".to_string(),
            dest: "/tmp/out".to_string(),
            files: 3,
            bytes: 90,
            ..molt_core::ExportState::default()
        },
        ..SessionView::default()
    };
    apply_session(&ui, &sv, true, &chat_ui);
    assert_eq!(ui.get_toast_text().as_str(), "wiki exported: 3 files");

    // a second, unchanged push must not speak again
    ui.invoke_show_toast("something else".into());
    apply_session(&ui, &sv, true, &chat_ui);
    assert_eq!(
        ui.get_toast_text().as_str(),
        "something else",
        "an unchanged export state re-toasted"
    );
}

/// **The dialog's Confirm reaches the engine with what the user picked.**
/// Both halves are pinned: the destination (the tree lands exactly
/// there) and the proof flag (this workspace has no chain, so a
/// `proof: true` export is REFUSED - if the flag were dropped the very
/// same call would write a tree).
#[test]
fn the_export_dialog_issues_the_command_with_the_picked_path_and_the_proof_flag() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();

    // a single-operator group: propose + one approval applies the patch
    let w = molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    );
    rt.block_on(async {
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: serde_json::json!({
                    "op": "wiki_patch",
                    "summary": "a.md",
                    "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hello\n",
                }),
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
    });

    let ui = AppWindow::new().expect("headless window");
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    wire_wiki_export(&ui, &cx);

    // --- the proof flag: no chain here, so the engine must refuse
    let refused = tmp.path().join("refused");
    ui.invoke_wiki_export(refused.display().to_string().into(), true);

    // --- the destination: the same call without the bundle writes
    let dest = tmp.path().join("out");
    ui.invoke_wiki_export(dest.display().to_string().into(), false);
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let Ok(Reply::Session(s)) = w.execute(Command::ReadSession).await else {
                panic!("read session");
            };
            if !s.wiki_export.running && !s.wiki_export.result.is_empty() {
                assert_eq!(s.wiki_export.result, "ok", "the export failed: {s:?}");
                assert_eq!(s.wiki_export.dest, dest.display().to_string());
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the export never settled: {:?}",
                s.wiki_export
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    assert_eq!(
        std::fs::read_to_string(dest.join("wiki/a.md")).expect("the exported doc"),
        "hello\n"
    );
    assert!(
        !dest.join("proof").exists(),
        "proof: false must write no bundle"
    );
    // the refused call ran first and left nothing behind
    assert!(
        !refused.exists(),
        "a proof export without a chain must be refused, not written"
    );
}

/// i18n: every wiki-export string carries a real English AND a real
/// German arm (an empty or identical pair is a missing translation),
/// and none of them smuggles in an em dash.
#[test]
fn every_wiki_export_string_reads_in_both_languages() {
    let en = Lexicon::en();
    let de = Lexicon::de();
    let pairs = [
        ("mem_tb_export", en.mem_tb_export, de.mem_tb_export),
        ("mem_ex_title", en.mem_ex_title, de.mem_ex_title),
        ("mem_ex_body", en.mem_ex_body, de.mem_ex_body),
        ("mem_ex_confirm", en.mem_ex_confirm, de.mem_ex_confirm),
        ("mem_ex_proof", en.mem_ex_proof, de.mem_ex_proof),
        ("mem_ex_reveals", en.mem_ex_reveals, de.mem_ex_reveals),
        ("mem_ex_drafts", en.mem_ex_drafts, de.mem_ex_drafts),
        ("mem_ex_done", en.mem_ex_done, de.mem_ex_done),
        ("mem_ex_file", en.mem_ex_file, de.mem_ex_file),
        ("mem_ex_files", en.mem_ex_files, de.mem_ex_files),
        ("mem_ex_failed", en.mem_ex_failed, de.mem_ex_failed),
    ];
    for (key, e, d) in pairs {
        assert!(!e.is_empty() && !d.is_empty(), "{key}: an empty arm");
        assert_ne!(e, d, "{key}: untranslated");
        assert!(!e.contains('—') && !d.contains('—'), "{key}: em dash");
    }
    // the disclosure names what the bundle actually reveals
    for l in [en, de] {
        let line = l.mem_ex_reveals.to_lowercase();
        for token in ["relay", "chart"] {
            assert!(
                line.contains(token),
                "the disclosure drops {token}: {}",
                l.mem_ex_reveals
            );
        }
    }
}

/// The engine's export refusals reach the user in German too - the
/// `localize_error` match carries no wildcard, so a new phrase is a
/// compile-time reminder, but a phrase without an arm would silently
/// stay English.
#[test]
fn the_wiki_export_refusals_render_in_german() {
    for phrase in [
        "a target directory is required",
        "an export is already running",
        "the wiki is empty",
        "proof needs chain governance",
        "proof needs the genesis block",
    ] {
        let e = molt_core::MoltError::WikiExport(phrase);
        let de = super::localize_error(1, &e);
        assert!(de.starts_with("Wiki-Export: "), "{de}");
        assert!(!de.contains(phrase), "phrase without a German arm: {de}");
    }
}

/// **The viewer's infobox, headless** (`knowledge_base_scale.md` §4.10):
/// the front matter reaches Slint as a property table, the raw header
/// never reaches the prose, and a link-valued property carries the target
/// the row is clickable with.
#[test]
fn the_front_matter_reaches_slint_as_a_property_table() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "anna.md".into(),
            content: "---\ntype: person\nworks_at: \"[[Acme]]\"\n---\n# Anna\n\nShe builds things."
                .into(),
            loaded: true,
        },
        WikiBase {
            path: "Acme.md".into(),
            content: "# Acme".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let anna = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "anna.md")
        .expect("anna row");
    g.invoke_nav_open(anna.id);

    let props = g.get_props();
    let got: Vec<(String, String, String)> = (0..props.row_count())
        .filter_map(|i| props.row_data(i))
        .map(|p| {
            (
                p.key.to_string(),
                p.value.to_string(),
                p.link.to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("type".to_string(), "person".to_string(), String::new()),
            ("works_at".to_string(), "Acme".to_string(), "Acme".to_string()),
        ],
        "the header renders as a table, the link keeps its target"
    );
    let blocks = g.get_blocks();
    assert!(
        !(0..blocks.row_count())
            .filter_map(|i| blocks.row_data(i))
            .any(|b| b.text.as_str().contains("type:")),
        "the raw header must not appear as prose"
    );
    // …and the editor still sees the document as written
    assert!(g.get_raw().as_str().starts_with("---\n"));

    // closing the document clears the table
    g.invoke_close_active();
    assert_eq!(g.get_props().row_count(), 0);
}

/// **The in-edge request rides the DOCUMENT change**, not every model
/// mutation: an engine read per keystroke would be a new cost on every
/// edit, and the reply would race the typing it belongs to.
#[test]
fn the_backlink_request_rides_the_document_change() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let seen = asked.clone();
    g.on_backlinks_wanted(move |p| seen.borrow_mut().push(p.to_string()));

    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "a.md".into(),
            content: "# A".into(),
            loaded: true,
        },
        WikiBase {
            path: "b.md".into(),
            content: "# B".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id_of = |name: &str| {
        (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == name)
            .expect("row")
            .id
    };
    g.invoke_nav_open(id_of("a.md"));
    assert_eq!(asked.borrow().as_slice(), ["a.md"], "opening asks once");

    // an edit is not a new document
    g.invoke_edit_toggle();
    g.invoke_edited(format!("{}\n\nmore", g.get_raw()).into());
    assert_eq!(asked.borrow().len(), 1, "an edit must not ask again");

    // …switching to another one is
    g.invoke_nav_open(id_of("b.md"));
    assert_eq!(asked.borrow().as_slice(), ["a.md", "b.md"]);
}

/// **A `[[Title]]` link opens its page even when the pane cannot bind
/// it.** The pane resolves a click by path and file name only; a header
/// title or an alias binds in the ENGINE's index, and for a page whose
/// bytes were never fetched the pane does not even hold the title. So a
/// miss asks the engine, and its answer opens the page (reported
/// 2026-09-08: title links in an agent-written wiki went nowhere).
#[test]
fn a_title_link_the_pane_cannot_bind_asks_the_engine_and_opens_the_answer() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let seen = asked.clone();
    g.on_link_resolve_wanted(move |name| seen.borrow_mut().push(name.to_string()));

    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "start.md".into(),
            content: "# Start\n\nSee [[Kafka]].\n".into(),
            loaded: true,
        },
        // the target's bytes are not here: only the engine knows its title
        WikiBase {
            path: "systeme/kafka.md".into(),
            content: "".into(),
            loaded: false,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "start.md")
        .expect("nav row")
        .id;
    g.invoke_nav_open(id);

    g.invoke_open_link("Kafka".into());
    assert_eq!(g.get_doc_path().as_str(), "start.md", "a miss stays put");
    assert_eq!(asked.borrow().as_slice(), ["Kafka"], "…and asks the engine");

    g.invoke_link_resolved("systeme/kafka.md".into());
    assert_eq!(
        g.get_doc_path().as_str(),
        "systeme/kafka.md",
        "the engine's answer opens the page"
    );

    // a target the pane binds itself never asks
    g.invoke_open_link("start.md".into());
    assert_eq!(g.get_doc_path().as_str(), "start.md");
    assert_eq!(asked.borrow().len(), 1, "a local hit is no question");
}

/// **The Linked section is navigation, not decoration.** Its entries
/// rendered as passive pills while the Backlinks right below them were
/// live links, so the one list that walks the graph FORWARD was dead
/// (reported 2026-09-06).
#[cfg(feature = "live-preview")]
#[test]
fn a_linked_entry_opens_its_target() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    // the display text differs from the target, so the entry's label is
    // the only thing in the pane reading "notes/b.md"
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "a.md".into(),
            content: "# A\n\nsee [Bee](notes/b.md)\n".into(),
            loaded: true,
        },
        WikiBase {
            path: "notes/b.md".into(),
            content: "# B\n".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "a.md")
        .expect("nav row")
        .id;
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_nav_open(id);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    let links: Vec<String> = (0..g.get_links().row_count())
        .filter_map(|i| g.get_links().row_data(i))
        .map(|l| l.to_string())
        .collect();
    assert_eq!(links, ["notes/b.md"], "the outgoing target is listed");

    let entry =
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "notes/b.md")
            .next()
            .expect("the Linked section renders the target");
    click(&ui, &entry);
    assert_eq!(
        g.get_doc_path().as_str(),
        "notes/b.md",
        "a Linked entry opens its document"
    );
}

/// **The changeset panel is ONE row, however long the stack gets.** It
/// used to list the actions themselves, so a long editing session grew
/// the panel until it owned the pane; the stack reads in the Changes
/// modal now (reported 2026-09-05).
#[cfg(feature = "live-preview")]
#[test]
fn the_changeset_panel_stays_one_row_however_long_the_stack_gets() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    let geom = |id: &str| {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .find(|e| e.size().height > 0.0)
            .map(|e| (e.absolute_position().y, e.size().height))
    };
    for actions in [3, 30] {
        for _ in 0..actions {
            g.invoke_new_file();
        }
        assert!(g.get_cs_rows().row_count() >= actions, "the stack carries them");
        for font in [14.0_f32, 20.0, 26.0] {
            ui.global::<Theme>().set_fs_app(font);
            let scale = (font / 14.0).max(1.0);
            let (_, panel_h) = geom("MemoryPane::cs-panel").expect("the changeset panel renders");
            // padding (2 x 9) + the header row, and nothing else
            let want = 18.0 + 28.0 * scale;
            assert!(
                (panel_h - want).abs() < 1.0,
                "{actions} actions, font {font}: the panel is {panel_h} tall, not the {want} of its one row"
            );
        }
    }
}

/// **What the panel no longer lists, the Changes modal shows — all of
/// it, scrollable.** Losing the rows from the panel must not lose them
/// from the app.
#[cfg(feature = "live-preview")]
#[test]
fn the_changes_modal_holds_the_whole_stack() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    for _ in 0..12 {
        g.invoke_new_file();
    }
    let stack = g.get_cs_rows().row_count();
    assert!(stack >= 12, "the stack carries them");

    let rows = |ui: &AppWindow| {
        i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppWindow::csm-row").count()
    };
    assert_eq!(rows(&ui), 0, "the modal is closed - if this is not 0 the test proves nothing");

    // the panel's Changes button, found by its own label
    let button =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppButton::abtn-label")
            .find(|e| e.accessible_label().is_some_and(|l| l == "Changes"))
            .expect("the Changes button renders");
    click(&ui, &button);
    assert_eq!(rows(&ui), stack, "the modal lists every action the stack holds");
}

/// The ontology is content, not code: a document without a header offers
/// TAGS - the one reserved key the index turns into facets - and wears
/// them as coloured pills once they are written. The offer disappears the
/// moment a header exists.
#[cfg(feature = "live-preview")]
#[test]
fn a_document_without_a_header_offers_tags_and_then_wears_them() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "bare.md".into(),
            content: "# Bare\n\nNo header here.".into(),
            loaded: true,
        },
        WikiBase {
            path: "anna.md".into(),
            content: "---\ntype: person\n---\n# Anna\n".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let open = |path: &str| {
        let rows = g.get_nav_rows();
        let row = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == path)
            .expect("nav row");
        g.invoke_nav_open(row.id);
    };
    let pills = |ui: &AppWindow| {
        i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "TagFlow::tag-pill")
            .count()
    };

    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");

    open("bare.md");
    assert!(g.get_can_add_tags(), "a header-less document offers tags");
    let chips: Vec<String> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "PropChip")
            .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
            .collect();
    assert_eq!(chips, vec!["+ Tag".to_string()], "one offer, and it says what it does");
    assert_eq!(pills(&ui), 0, "nothing tagged yet");

    // the chip raises the modal; the modal writes the header
    let chip = i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "PropChip")
        .next()
        .expect("the offer renders");
    click(&ui, &chip);
    assert!(g.get_tag_modal_open(), "the chip opens the modal");
    assert_eq!(g.get_tag_rows().row_count(), 1, "one empty row to type into");
    g.invoke_tag_set(0, "Berlin".into());
    g.invoke_tag_add("gruender".into());
    g.invoke_tag_commit();
    g.set_tag_modal_open(false);

    assert!(!g.get_can_add_tags(), "the document has a header now");
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "PropChip").count(),
        0,
        "no offer once a header exists"
    );
    assert_eq!(pills(&ui), 2, "one pill per tag");
    let tags = g.get_tag_pills();
    let hues: Vec<i32> = (0..tags.row_count())
        .filter_map(|i| tags.row_data(i))
        .map(|p| p.hue)
        .collect();
    assert_eq!(hues.len(), 2);
    assert_ne!(hues[0], hues[1], "two tags, two colours");

    // a document that HAS a header makes no offer at all
    open("anna.md");
    assert!(!g.get_can_add_tags());
    assert_eq!(pills(&ui), 0, "type: person is a row, not a tag");
}

/// The semantic link is reachable from all three places it was asked for,
/// and its DEFAULT write is an inline claim in the open document's prose
/// (`wiki_semantic_gaps.md` §6).
#[cfg(feature = "live-preview")]
#[test]
fn a_semantic_link_is_written_from_the_toolbar_the_editor_and_the_navigator() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase { path: "a.md".into(), content: "# A\n".into(), loaded: true },
        WikiBase { path: "anna.md".into(), content: "# Anna\n".into(), loaded: true },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let row_id = |path: &str| {
        let rows = g.get_nav_rows();
        (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == path)
            .expect("nav row")
            .id
    };
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_nav_open(row_id("a.md"));

    // 1) the toolbar button
    let button = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
        &ui,
        "Semantic link",
    )
    .next()
    .expect("the toolbar offers it");
    click(&ui, &button);
    assert!(g.get_link_modal_open(), "the toolbar raises the modal");
    assert!(!g.get_link_ready(), "no target, no relation, no link");

    g.invoke_set_link_target("anna.md".into());
    assert_eq!(g.get_link_name().as_str(), "Anna", "the name follows the target");
    g.invoke_link_toggle("is_a".into());
    assert!(g.get_link_ready());
    assert!(!g.get_link_header(), "the prose is the default form");
    g.invoke_link_commit();
    assert_eq!(g.get_link_error().as_str(), "", "the write went through");
    g.set_link_modal_open(false);

    assert_eq!(
        g.get_raw().as_str(),
        "# A\n\n[[is_a::anna.md|Anna]]\n",
        "the claim is a sentence, not a header key"
    );
    assert_eq!(g.get_props().row_count(), 0, "…and the header stays empty");
    // the preview shows the target and carries the predicate as its claim
    let spans: Vec<(String, String, String)> = (0..g.get_blocks().row_count())
        .filter_map(|i| g.get_blocks().row_data(i))
        .flat_map(|b| {
            (0..b.spans.row_count())
                .filter_map(move |i| b.spans.row_data(i))
                .map(|sp| (sp.text.to_string(), sp.link.to_string(), sp.rel.to_string()))
        })
        .collect();
    assert!(
        spans.contains(&("Anna".to_string(), "anna.md".to_string(), "is_a".to_string())),
        "{spans:?}"
    );

    // 2) the navigator's own menu opens the file and raises the same modal
    g.invoke_link_open("".into());
    g.set_link_modal_open(false);
    g.invoke_nav_open(row_id("anna.md"));
    g.invoke_link_open("".into());
    g.set_link_modal_open(true);
    assert!(g.get_can_write_body(), "the subject is the OPEN document");
    assert!(
        !g.get_link_targets().iter().any(|t| t == "anna.md"),
        "a document cannot be its own target"
    );
    g.set_link_modal_open(false);
}

/// The caret rides every keystroke, arrow key and click, so it must NOT
/// drag a whole-face sync along - the face does not read it, the next
/// commit does. Pinned by a sentinel a sync would overwrite.
#[cfg(feature = "live-preview")]
#[test]
fn a_caret_move_does_not_resync_the_whole_face() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "a.md")
        .expect("nav row")
        .id;
    g.invoke_nav_open(id);

    // a sync rewrites doc-meta from the model, so a surviving sentinel
    // means no sync ran
    g.set_doc_meta("sentinel".into());
    g.invoke_set_cursor(2);
    assert_eq!(
        g.get_doc_meta().as_str(),
        "sentinel",
        "a caret move must not push the face"
    );
    assert_eq!(g.get_raw().as_str(), "# A\n", "…and it writes nothing");

    // …while a real action does, or the sentinel would prove nothing
    g.invoke_nav_mark(id);
    assert_ne!(g.get_doc_meta().as_str(), "sentinel");
}

/// **A keystroke renders nothing** (`docs_archive/ui/wiki_pane_performance.md`
/// F4): the block view is off screen while the editor is up
/// (surfaces.slint gates it on `!editing`), so the markdown parse of
/// working AND base text plus the Myers diff must not run per keystroke -
/// it was 27 ms of model work per character on a held base. Leaving the
/// editor still shows what was typed.
#[cfg(feature = "live-preview")]
#[test]
fn a_keystroke_does_not_rebuild_the_block_view() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n\nfirst\n\nsecond\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_nav_open(nav_id(&ui, "a.md"));
    let blocks = g.get_blocks();
    let rows_before: Vec<slint::SharedString> = (0..blocks.row_count())
        .filter_map(|i| blocks.row_data(i))
        .map(|b| b.text)
        .collect();
    assert!(rows_before.len() > 1, "the document renders");

    g.invoke_edit_toggle();
    crate::wiki::counters::reset();
    let mut text = g.get_raw().to_string();
    for _ in 0..20 {
        text.push('x');
        g.invoke_edited(text.as_str().into());
    }
    assert_eq!(
        crate::wiki::counters::preview(),
        0,
        "the editor's echo parses and diffs nothing"
    );
    let after = g.get_blocks();
    assert!(
        std::ptr::eq(
            blocks
                .as_any()
                .downcast_ref::<VecModel<WikiBlock>>()
                .expect("blocks are a VecModel") as *const _,
            after
                .as_any()
                .downcast_ref::<VecModel<WikiBlock>>()
                .expect("still a VecModel") as *const _,
        ),
        "…and never touches the model"
    );
    let rows_after: Vec<slint::SharedString> = (0..after.row_count())
        .filter_map(|i| after.row_data(i))
        .map(|b| b.text)
        .collect();
    assert_eq!(rows_after, rows_before, "the rows stand still");

    // leaving the editor is a key change: the blocks are fresh the moment
    // they are visible again
    g.invoke_edit_toggle();
    assert!(crate::wiki::counters::preview() > 0);
    let shown = g.get_blocks();
    assert!(
        (0..shown.row_count())
            .filter_map(|i| shown.row_data(i))
            .any(|b| b.text.as_str().contains("xxxxxxxxxxxxxxxxxxxx")),
        "what was typed is on screen"
    );
}

/// **The draft is serialized once per change window, not per keystroke**
/// (F4a). It is the WHOLE model as JSON - 21 ms on a held base - compared
/// against a string that is thrown away 2 s out of 2 s. The 2 s contract
/// itself stands: a hard kill loses at most that window (WP-D).
#[cfg(feature = "live-preview")]
#[test]
fn the_draft_is_serialized_once_per_change_window() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let saved = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let s = saved.clone();
        ui.on_wiki_draft_save(move |d| s.borrow_mut().push(d.to_string()));
    }
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_nav_open(nav_id(&ui, "a.md"));
    g.invoke_edit_toggle();

    crate::wiki::counters::reset();
    let mut text = g.get_raw().to_string();
    for _ in 0..20 {
        text.push('x');
        g.invoke_edited(text.as_str().into());
    }
    assert_eq!(
        crate::wiki::counters::draft(),
        0,
        "inside the window nothing is serialized"
    );
    assert!(saved.borrow().is_empty(), "…and nothing reaches the engine");

    // the window is over: the next keystroke saves, the 19 after it do not
    std::thread::sleep(std::time::Duration::from_millis(2_050));
    for _ in 0..20 {
        text.push('y');
        g.invoke_edited(text.as_str().into());
    }
    assert_eq!(crate::wiki::counters::draft(), 1, "exactly one serialization");
    assert_eq!(saved.borrow().len(), 1, "…and exactly one engine hop");
    assert!(
        saved.borrow()[0].contains("xxxxxxxxxxxxxxxxxxxxy"),
        "the saved draft carries the work"
    );
}

/// **…and the last change of a burst is not lost.** A change made INSIDE
/// the window has no sync of its own to save it: a member who types once
/// and walks away used to leave the edit sitting there until a callback
/// that never comes, which is not "a hard kill loses at most that window"
/// - it is a window with no end. One timer per window closes it.
#[cfg(feature = "live-preview")]
#[test]
fn a_change_inside_the_window_is_flushed_when_it_closes() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let saved = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let s = saved.clone();
        ui.on_wiki_draft_save(move |d| s.borrow_mut().push(d.to_string()));
    }
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_nav_open(nav_id(&ui, "a.md"));
    g.invoke_edit_toggle();

    // ONE keystroke, then the member walks away
    let mut text = g.get_raw().to_string();
    text.push('z');
    g.invoke_edited(text.as_str().into());
    assert!(saved.borrow().is_empty(), "the window is still open");

    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(2_100));
    assert_eq!(saved.borrow().len(), 1, "the window closing saves it");
    assert!(
        saved.borrow()[0].contains("# A\\nz"),
        "the flushed draft carries the keystroke: {}",
        saved.borrow()[0]
    );

    // …and a burst arms ONE timer, not one per keystroke
    crate::wiki::counters::reset();
    for _ in 0..20 {
        text.push('q');
        g.invoke_edited(text.as_str().into());
    }
    assert_eq!(
        crate::wiki::counters::flush(),
        1,
        "twenty keystrokes, one armed flush"
    );
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(2_100));
    assert_eq!(saved.borrow().len(), 2, "one hop per window, not per keystroke");
    assert_eq!(
        crate::wiki::counters::draft(),
        1,
        "…and one serialization with it"
    );
}

/// **A document's bytes are asked for once per attempt** (F5). Every sync
/// used to re-ask while the fetch was still running - 45 `wiki_get` round
/// trips in a 60-interaction flow, each reply re-syncing the face. A
/// failure makes the path askable again; an arrival ends it.
#[test]
fn the_bytes_in_flight_are_not_asked_for_again_on_every_sync() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let wanted = lazy_base(&ui, &["a.md"]);
    let id = nav_id(&ui, "a.md");

    g.invoke_nav_open(id);
    assert_eq!(wanted.borrow().as_slice(), ["a.md".to_string()]);
    for _ in 0..10 {
        g.invoke_nav_mark(id);
    }
    assert_eq!(
        wanted.borrow().len(),
        1,
        "a sync per click is not a wiki_get per click"
    );

    // the fetch gave up: the next member action retries it, and once
    g.invoke_content_failed("a.md".into());
    g.invoke_nav_mark(id);
    g.invoke_nav_mark(id);
    assert_eq!(wanted.borrow().len(), 2, "a failure makes it askable again");

    // …and the bytes arriving end it
    g.invoke_content_arrived("a.md".into(), "# A\n".into());
    for _ in 0..10 {
        g.invoke_nav_mark(id);
    }
    assert_eq!(wanted.borrow().len(), 2, "an arrival ends the asking");
}

/// The header stays reachable, as a DELIBERATE second option: the
/// qualified relation `{to, since, role}` has no inline shape, and a
/// refusal keeps the modal up with its reason rather than writing.
#[cfg(feature = "live-preview")]
#[test]
fn the_link_modal_switches_to_a_qualified_header_write() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase { path: "a.md".into(), content: "# A\n".into(), loaded: true },
        WikiBase { path: "anna.md".into(), content: "# Anna\n".into(), loaded: true },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let a = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "a.md")
        .expect("nav row")
        .id;
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_nav_open(a);

    g.invoke_link_open("".into());
    g.set_link_modal_open(true);
    g.invoke_set_link_header(true);
    assert!(g.get_link_header());
    g.invoke_set_link_target("anna.md".into());
    g.invoke_link_toggle("works_at".into());

    // a qualifier line the subset cannot read writes NOTHING and says so
    g.invoke_set_link_qualifiers("since 2019".into());
    g.invoke_link_commit();
    assert_eq!(
        g.get_link_error().as_str(),
        ui.global::<Strings>().get_mem_link_err_qsyntax().as_str()
    );
    assert_eq!(g.get_raw().as_str(), "# A\n", "the document is untouched");

    g.invoke_set_link_qualifiers("since: 2019, role: CTO".into());
    assert_eq!(g.get_link_error().as_str(), "", "correcting clears the reason");
    g.invoke_link_commit();
    assert_eq!(g.get_link_error().as_str(), "");
    assert_eq!(
        g.get_raw().as_str(),
        "---\nworks_at: {role: \"CTO\", since: 2019, to: \"[[anna.md|Anna]]\"}\n---\n# A\n"
    );
    let props = g.get_props();
    assert!(
        (0..props.row_count())
            .filter_map(|i| props.row_data(i))
            .any(|p| p.key.as_str() == "works_at"),
        "the infobox shows the qualified relation"
    );
}

/// **K6 §4.9.6 in the window**: while the folded base is being fetched the
/// pane must not read as an empty knowledge base. "Nothing here yet" and
/// "not here YET" are different claims, and the second one is the true one.
#[cfg(feature = "live-preview")]
#[test]
fn a_pending_base_replaces_the_empty_state() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");

    let empty = ui.global::<Strings>().get_mem_empty().to_string();
    let seen = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label).count() > 0
    };
    assert!(seen(&empty), "an empty wiki says so");

    g.set_base_pending("Shared memory arriving (0 / 42 KB)".into());
    assert!(
        seen("Shared memory arriving (0 / 42 KB)"),
        "the pending line takes the empty state's place"
    );
    assert!(!seen(&empty), "…and the empty claim is gone");
}

/// Reveal used to mark a row the member could not see: nothing scrolled
/// the navigator. Now the marked row lands inside the pane, and the button
/// stays enabled while a document is active (the row can be marked AND
/// out of view).
#[cfg(feature = "live-preview")]
#[test]
fn reveal_scrolls_the_navigator_to_the_marked_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    // one folder of 60 files: far more than a 700 px navigator shows
    let docs: Vec<WikiBase> = (0..60)
        .map(|i| WikiBase {
            path: format!("people/person-{i:02}.md").into(),
            content: format!("# Person {i}\n\nprose.\n").into(),
            loaded: true,
        })
        .collect();
    g.set_base_docs(ModelRc::new(VecModel::from(docs)));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    assert_eq!(g.get_nav_rows().row_count(), 1, "a base folder starts closed");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_open_link("people/person-55.md".into());
    assert!(g.get_doc_open());
    assert!(g.get_can_reveal(), "a document is active: reveal is on");
    g.invoke_reveal();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    assert_eq!(g.get_nav_rows().row_count(), 61, "reveal unfolded people/");
    assert_eq!(g.get_nav_scroll_to(), -1, "the request was consumed");
    // the open tab carries the same label; the navigator's row is the
    // leftmost match
    let row = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "person-55.md")
        .min_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x))
        .expect("the revealed row exists");
    let nav = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ScrollBody::fl")
        .min_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x))
        .expect("the navigator's flickable");
    let (top, bottom) = (nav.absolute_position().y, nav.absolute_position().y + nav.size().height);
    let y = row.absolute_position().y;
    assert!(
        y > top && y < bottom,
        "the marked row sits inside the navigator ({top}..{bottom}), y={y}"
    );
    // marked AND active: still revealable (it may be scrolled away again)
    assert!(g.get_can_reveal());
}

/// The wiki pane shown headless over a base of `paths` - the fixture the
/// navigator tests below drive with real mouse events.
#[cfg(feature = "live-preview")]
fn nav_window(paths: &[&str]) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let docs: Vec<WikiBase> = paths
        .iter()
        .map(|p| WikiBase {
            path: (*p).into(),
            content: "prose\n".into(),
            loaded: true,
        })
        .collect();
    g.set_base_docs(ModelRc::new(VecModel::from(docs)));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    settle();
    ui
}

/// Every element tree of a navigator row (by its named parts).
#[cfg(feature = "live-preview")]
fn nav_row_trees(ui: &AppWindow, id: &str) -> Vec<i_slint_backend_testing::ElementHandle> {
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, id).collect()
}

/// The navigator renders a VIEWPORT's worth of rows, never one per
/// document. (The windowing behind it - `live` on each row - cannot be
/// counted here: the testing search API skips anything the flickable
/// clips, so an eager tree and a windowed one look identical from
/// outside. What windowing buys is measured by the paint probe.)
#[cfg(feature = "live-preview")]
#[test]
fn the_navigator_renders_a_viewports_worth_of_rows() {
    let paths: Vec<String> = (0..200).map(|i| format!("note-{i:03}.md")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let ui = nav_window(&refs);
    let g = ui.global::<WikiState>();
    assert_eq!(g.get_nav_rows().row_count(), 200, "every file is a row");
    let nav = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ScrollBody::fl")
        .min_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x))
        .expect("the navigator's flickable");
    let fits = (nav.size().height / 28.0).ceil() as usize;
    let live = nav_row_trees(&ui, "MemoryPane::filerow").len();
    assert!(live > 0, "the visible rows exist");
    assert!(
        live <= fits + 8,
        "{live} row trees for a {fits}-row viewport"
    );
}

/// **A right-click marks the row and opens the pane's ONE menu.** The
/// per-row `ContextMenuArea` (a seven-item `Menu` per row) is gone; the
/// entries are built from the marked row, so the click has to mark first.
#[cfg(feature = "live-preview")]
#[test]
fn a_right_click_on_a_file_row_marks_it_and_opens_the_pane_menu() {
    let ui = nav_window(&["alpha.md", "beta.md", "gamma.md"]);
    let g = ui.global::<WikiState>();
    let verb = ui.global::<Strings>().get_mem_menu_copy_link().to_string();
    assert!(!verb.is_empty(), "the fixture must carry the menu title");
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, &verb)
            .next()
            .is_none(),
        "no menu may be findable before the click"
    );
    let rows = nav_row_trees(&ui, "MemoryPane::filerow");
    assert_eq!(rows.len(), 3, "three file rows render");
    right_click(&ui, &rows[1], 0.5);
    settle();
    assert!(g.get_has_marked(), "the right-click marked the row");
    assert!(!g.get_marked_is_folder());
    assert_eq!(g.get_marked_id(), nav_id(&ui, "beta.md"));
    // (WHERE it opens cannot be checked here: the testing backend puts
    // every popup in the same corner, whatever position it is handed)
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, &verb)
            .next()
            .is_some(),
        "right-click opened no menu carrying {verb:?}"
    );
}

/// The same for a FOLDER row: it marks the folder and offers the folder
/// verbs - never the file ones.
#[cfg(feature = "live-preview")]
#[test]
fn a_right_click_on_a_folder_row_marks_it_and_opens_the_folder_menu() {
    let ui = nav_window(&["team/roles.md", "note.md"]);
    let g = ui.global::<WikiState>();
    let s = ui.global::<Strings>();
    let folder_verb = s.get_mem_tb_new_file().to_string();
    let file_verb = s.get_mem_menu_copy_link().to_string();
    assert!(!folder_verb.is_empty() && !file_verb.is_empty());
    let rows = nav_row_trees(&ui, "MemoryPane::frow");
    assert_eq!(rows.len(), 1, "one folder row renders");
    right_click(&ui, &rows[0], 0.5);
    settle();
    assert!(g.get_marked_is_folder(), "the folder row is the marked one");
    assert_eq!(g.get_marked_path().as_str(), "team");
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, &folder_verb)
            .next()
            .is_some(),
        "the folder menu is missing {folder_verb:?}"
    );
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, &file_verb)
            .next()
            .is_none(),
        "a folder row must not offer {file_verb:?}"
    );
}

/// **A mark must not move a glyph** (`wiki_pane_performance.md` F2): the
/// weight used to switch 400 → 600, which changes the label's text
/// metrics, invalidates the layout above it and repaints the whole
/// window instead of two rows. The strike-through of a deleted row is
/// measured off `nlabel.preferred-width`, so it reads that metric back.
#[cfg(feature = "live-preview")]
#[test]
fn a_mark_leaves_the_row_label_metrics_alone() {
    let ui = nav_window(&["deleted-note.md", "keeper.md"]);
    let g = ui.global::<WikiState>();
    let id = nav_id(&ui, "deleted-note.md");
    g.invoke_nav_delete(id);
    settle();
    let strike = || {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "MemoryPane::strike")
            .next()
            .expect("the deleted row is struck")
            .size()
            .width
    };
    let label = || {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "MemoryPane::nlabel")
            .next()
            .expect("the row label")
            .size()
    };
    let (before, lbefore) = (strike(), label());
    assert!(before > 0.0, "the strike is measured off the text");
    g.invoke_nav_mark(id);
    settle();
    assert!(g.get_has_marked(), "the row is marked");
    assert_eq!(strike(), before, "the mark changed the label's metrics");
    assert_eq!(label(), lbefore, "the mark resized the label");
}

/// **A mark repaints its rows, not the window** (`wiki_pane_performance.md`
/// F2). The weight switch changed the label's metrics and the accent bar
/// was created and destroyed per mark; both invalidated the pane, so a
/// mark dirtied 1600x1000. This renders the pane offscreen through the
/// software renderer - the only thing that can read a dirty region back -
/// and asserts the second mark touches a few rows.
#[cfg(feature = "live-preview")]
#[test]
fn a_mark_repaints_its_rows_not_the_window() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::Rgb8Pixel;
    struct P(std::rc::Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for P {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    const W: u32 = 1200;
    const H: u32 = 800;
    let win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    // Slint's platform is thread-local and every #[test] owns its
    // thread, so this window never meets the testing backend
    slint::platform::set_platform(Box::new(P(win.clone())))
        .expect("software platform: this test needs a thread of its own");
    win.set_size(slint::PhysicalSize::new(W, H));
    let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
    let ui = AppWindow::new().expect("window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let docs: Vec<WikiBase> = (0..60)
        .map(|i| WikiBase {
            path: format!("note-{i:02}.md").into(),
            content: "prose\n".into(),
            loaded: true,
        })
        .collect();
    g.set_base_docs(ModelRc::new(VecModel::from(docs)));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    ui.show().expect("show");
    let mut frame = || {
        let mut region = None;
        win.draw_if_needed(|r| {
            region = Some(r.render(&mut buf, W as usize));
        });
        region.map(|r| r.bounding_box_size())
    };
    frame().expect("the cold frame paints");
    let rows = g.get_nav_rows();
    let ids: Vec<i32> = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .filter(|r| !r.is_folder)
        .map(|r| r.id)
        .collect();
    g.invoke_nav_mark(ids[1]);
    // the first mark also enables the toolbar's delete button, so its
    // region spans both - the second one is the mark alone
    frame().expect("the mark paints");
    g.invoke_nav_mark(ids[4]);
    let moved = frame().expect("the moved mark paints");
    assert!(
        moved.height < 200 && moved.width < W / 2,
        "a mark dirtied {}x{} - the pane, not two rows",
        moved.width,
        moved.height
    );
}

/// A row that starts renaming has to be REACHABLE: its inline input only
/// exists while the row is inside the navigator's window, and a new file
/// lands at the foot of a long tree.
#[cfg(feature = "live-preview")]
#[test]
fn a_new_file_brings_its_rename_input_into_view() {
    let paths: Vec<String> = (0..200).map(|i| format!("note-{i:03}.md")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let ui = nav_window(&refs);
    let g = ui.global::<WikiState>();
    g.invoke_new_file();
    settle();
    assert_eq!(g.get_nav_rows().row_count(), 201, "the new row is listed");
    let nav = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ScrollBody::fl")
        .min_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x))
        .expect("the navigator's flickable");
    let rin = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "MemoryPane::rin")
        .next()
        .expect("the rename input is in view");
    let (top, bottom) = (nav.absolute_position().y, nav.absolute_position().y + nav.size().height);
    let y = rin.absolute_position().y;
    assert!(
        y >= top && y < bottom,
        "the rename input sits outside the navigator ({top}..{bottom}), y={y}"
    );
}

/// The double-click route stays open with one pane-level menu (the
/// per-row menu used to host the TouchArea that carries it).
#[cfg(feature = "live-preview")]
#[test]
fn a_double_click_on_a_navigator_row_still_opens_the_document() {
    let ui = nav_window(&["alpha.md", "beta.md"]);
    let g = ui.global::<WikiState>();
    let rows = nav_row_trees(&ui, "MemoryPane::filerow");
    click(&ui, &rows[0]);
    click(&ui, &rows[0]);
    settle();
    assert!(g.get_doc_open(), "the double-click opened nothing");
    assert_eq!(g.get_doc_path().as_str(), "alpha.md");
}

/// Polish: both authoring modals have to FIT - a dialog whose buttons sit
/// below the window edge cannot be confirmed at all, and the app font is
/// a setting (9..28px).
#[cfg(feature = "live-preview")]
#[test]
fn the_authoring_modals_fit_the_window_at_every_font_size() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase { path: "a.md".into(), content: "# A\n".into(), loaded: true },
        WikiBase { path: "anna.md".into(), content: "# Anna\n".into(), loaded: true },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "a.md")
        .expect("nav row")
        .id;
    ui.window().set_size(slint::PhysicalSize::new(1100, 760));
    ui.show().expect("show headless");
    g.invoke_nav_open(id);

    // the DIALOG's own extent, not the window's: a modal is a scrim over
    // the whole window, so its confirm button is what has to stay inside
    let fits = |ui: &AppWindow, what: &str, font: f32| {
        let modals = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            ui,
            "ConfirmModal",
        )
        .count();
        assert_eq!(modals, 1, "{what}: exactly one modal is up");
        // the confirm button is the LAST AppButton of the dialog, and the
        // dialog is the only thing on screen that carries these labels
        let low = i_slint_backend_testing::ElementHandle::find_by_element_id(
            ui,
            "AppButton::abtn-label",
        )
        .filter(|e| {
            e.accessible_label()
                .is_some_and(|l| l == "Save" || l == "Link" || l == "Cancel")
        })
        .map(|e| e.absolute_position().y + e.size().height)
        .fold(f32::MIN, f32::max);
        assert!(low > f32::MIN, "{what}: the dialog's own buttons must be found");
        assert!(
            low <= 760.0,
            "{what} at font {font}: its buttons end at {low}, past the window"
        );
    };

    for font in [14.0_f32, 20.0, 26.0] {
        ui.global::<Theme>().set_fs_app(font);
        g.invoke_tag_open();
        g.set_tag_modal_open(true);
        // as many rows as the vocabulary chips plus the + button can make
        for _ in 0..20 {
            g.invoke_tag_add("".into());
        }
        fits(&ui, "the tag modal", font);
        g.set_tag_modal_open(false);

        g.invoke_link_open("".into());
        g.set_link_modal_open(true);
        fits(&ui, "the link modal", font);
        g.set_link_modal_open(false);
        g.invoke_link_close();
    }
}

/// Removing a tag row must not leave its neighbour showing the text it no
/// longer holds. A one-way binding onto an input the member has already
/// typed into is dead, so the surviving rows have to be rebuilt.
#[cfg(feature = "live-preview")]
#[test]
fn dropping_a_tag_row_redraws_the_rows_that_stay() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = rows.row_data(0).expect("nav row").id;
    ui.window().set_size(slint::PhysicalSize::new(1100, 800));
    ui.show().expect("show headless");
    g.invoke_nav_open(id);

    let shown = |ui: &AppWindow| -> Vec<String> {
        i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppField::input")
            .filter_map(|e| e.accessible_value().map(|v| v.to_string()))
            .filter(|v| !v.is_empty())
            .collect()
    };

    g.invoke_tag_open();
    g.set_tag_modal_open(true);
    g.invoke_tag_set(0, "berlin".into());
    g.invoke_tag_add("gruender".into());
    assert_eq!(shown(&ui), vec!["berlin".to_string(), "gruender".to_string()]);

    // REAL typing into the first row: that is what kills the one-way
    // binding, and without it this test would pass either way
    let field = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppField::input")
        .find(|e| e.accessible_value().is_some_and(|v| v == "berlin"))
        .expect("the first row renders");
    click(&ui, &field);
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: 'x'.into() });
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: 'x'.into() });
    assert!(
        shown(&ui).iter().any(|v| v.contains('x')),
        "the keystroke reached the field: {:?}",
        shown(&ui)
    );

    g.invoke_tag_remove(0);
    assert_eq!(
        shown(&ui),
        vec!["gruender".to_string()],
        "the dropped row's text must go with it"
    );
    g.set_tag_modal_open(false);
}

/// Tab is the wiki document pane's mode switch: raw editor ⇄ preview, in
/// both directions, on a REAL key event. Slint runs its own Tab focus
/// walk only on keys nobody took, so the window scope can claim it — and
/// the toolbar tooltip has to name the shortcut.
#[cfg(feature = "live-preview")]
#[test]
fn tab_switches_the_wiki_document_between_preview_and_the_raw_editor() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: "# A\n\nprose.\n".into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let id = g.get_nav_rows().row_data(0).expect("nav row").id;
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_nav_open(id);
    assert!(g.get_doc_open(), "the document is open");
    assert!(!g.get_editing(), "…and starts in preview");

    let tab = |ui: &AppWindow| {
        let text: slint::SharedString = slint::platform::Key::Tab.into();
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    };

    tab(&ui);
    assert!(g.get_editing(), "Tab in preview opens the raw editor");
    tab(&ui);
    assert!(!g.get_editing(), "Tab in the raw editor goes back to preview");
    // and again, so neither direction was a one-shot of the focus walk
    tab(&ui);
    assert!(g.get_editing(), "the switch keeps working");
    tab(&ui);
    assert!(!g.get_editing());

    // Escape leaves the editor the same way, and must leave the keyboard
    // routed: the editor's own FocusScope goes with it
    tab(&ui);
    assert!(g.get_editing());
    let esc: slint::SharedString = slint::platform::Key::Escape.into();
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: esc.clone() });
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: esc });
    assert!(!g.get_editing(), "Escape still leaves the editor");
    tab(&ui);
    assert!(g.get_editing(), "…and the next Tab is still heard");
    tab(&ui);

    // a modal owns Tab while it is up - that is where it walks fields
    g.invoke_tag_open();
    g.set_tag_modal_open(true);
    tab(&ui);
    assert!(!g.get_editing(), "the tag modal keeps Tab");
    g.set_tag_modal_open(false);

    // and no other surface sees the shortcut at all
    ui.set_selected_surface("chat".into());
    tab(&ui);
    assert!(!g.get_editing(), "Tab is the wiki pane's alone");
    ui.set_selected_surface("memory".into());

    // the toolbar says so, in both modes and both languages
    let s = ui.global::<Strings>();
    for lang in [0, 1] {
        apply_strings(&ui, lang);
        for t in [s.get_mem_tb_edit().to_string(), s.get_mem_tb_preview().to_string()] {
            assert!(t.contains("Tab"), "lang {lang}: {t:?} names the shortcut");
            assert!(!t.contains('—'), "lang {lang}: {t:?} carries no em dash");
        }
    }
}

/// **The tooltip IS the safety mechanism** (`wiki_semantic_gaps.md` §6):
/// an inline annotation attaches to the PAGE, never to the sentence's
/// grammatical subject, so the preview renders the claim it actually
/// writes - this document, the predicate, the target - where a reader
/// sees it before ratifying. The predicate never shows in the link text.
#[cfg(feature = "live-preview")]
#[test]
fn an_inline_predicate_shows_its_claim_over_the_link() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "anna.md".into(),
            content: "# Anna\n\n[[works_at::Acme]]\n".into(),
            loaded: true,
        },
        WikiBase {
            path: "carl.md".into(),
            content: "# Carl\n\n[[Acme]]\n".into(),
            loaded: true,
        },
        WikiBase {
            path: "Acme.md".into(),
            content: "# Acme".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let open = |path: &str| {
        let rows = g.get_nav_rows();
        let row = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == path)
            .expect("nav row");
        g.invoke_nav_open(row.id);
    };
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    open("anna.md");

    // the predicate is the TOOLTIP, never the text: the sentence reads as
    // the author wrote it
    let spans: Vec<(String, String, String)> = (0..g.get_blocks().row_count())
        .filter_map(|i| g.get_blocks().row_data(i))
        .flat_map(|b| {
            (0..b.spans.row_count())
                .filter_map(move |i| b.spans.row_data(i))
                .map(|sp| (sp.text.to_string(), sp.link.to_string(), sp.rel.to_string()))
        })
        .collect();
    assert!(
        spans.contains(&("Acme".to_string(), "Acme".to_string(), "works_at".to_string())),
        "the run shows the target and carries the predicate: {spans:?}"
    );

    // a `changed` handler runs from the change trackers, which headless
    // means mock_elapsed_time - the real app gets that frame from its
    // render loop
    let hover = |ui: &AppWindow| {
        let link =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "LinkRun")
                .find(|e| e.size().width > 0.0)
                .expect("the preview renders the link");
        let at = slint::LogicalPosition::new(
            link.absolute_position().x + link.size().width / 2.0,
            link.absolute_position().y + link.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        ui.global::<HintTip>().get_label().to_string()
    };
    let leave = |ui: &AppWindow| {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(1350.0, 870.0),
            });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        ui.global::<HintTip>().get_label().to_string()
    };

    assert_eq!(hover(&ui), "Anna works_at Acme", "the tip renders the claim");
    assert_eq!(leave(&ui), "", "…and goes with the pointer");

    // …a link that asserts nothing says nothing
    open("carl.md");
    assert_eq!(hover(&ui), "", "a plain link has no claim to render");
}

/// A tag pill's label must sit in the MIDDLE of its pill: the same air on
/// both sides at every app font size. The chip's total width adds the
/// inset twice, so anything the row does asymmetrically shows up as an
/// off-centre label.
#[cfg(feature = "live-preview")]
#[test]
fn a_tag_pill_pads_its_label_symmetrically() {
    let ui = tagged_doc_window("---\ntags: [alpha]\n---\n# A\n\nprose.\n");
    for font in [14.0_f32, 20.0, 26.0] {
        ui.global::<Theme>().set_fs_app(font);
        let pill = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "TagFlow::tag-pill")
            .next()
            .expect("the pill renders");
        // …and its OWN row and label: the flow measures with hidden
        // chips of the same type, which come first in tree order
        let row = pill
            .query_descendants()
            .match_id("TagChip::tag-row")
            .find_first()
            .expect("the label row renders");
        let label = pill
            .query_descendants()
            .match_id("TagChip::tag-label")
            .find_first()
            .expect("the label renders");
        let left = row.absolute_position().x - pill.absolute_position().x;
        let right = (pill.absolute_position().x + pill.size().width)
            - (row.absolute_position().x + row.size().width);
        assert!(
            (left - right).abs() < 0.6,
            "font {font}: the pill pads {left} left and {right} right"
        );
        assert!(left > 0.5, "font {font}: the pill has no padding at all");
        // the label's own glyphs stay inside that inset - the defect was
        // the label element running past the pill's right edge
        assert!(
            label.absolute_position().x + label.size().width
                <= pill.absolute_position().x + pill.size().width - left + 0.6,
            "font {font}: the label reaches the pill's right edge"
        );
    }
}

/// One wiki document open in the Memory pane's preview, headless.
#[cfg(feature = "live-preview")]
fn tagged_doc_window(content: &str) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let wiki = wire_wiki(&ui);
    std::mem::forget(wiki);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "a.md".into(),
        content: content.into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let id = g.get_nav_rows().row_data(0).expect("nav row").id;
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    g.invoke_nav_open(id);
    ui
}

/// **The preview opens on what the document IS.** The tag pills are the
/// first thing in the document area, at the pane's own padding and
/// nothing more; no raw front-matter line reaches the prose; and one grey
/// rule sets the header band apart from the body.
#[cfg(feature = "live-preview")]
#[test]
fn the_preview_opens_with_the_header_band_at_the_top_edge() {
    let ui = tagged_doc_window(
        "---\ntags: [alpha]\ntype: person\n---\n# A\n\nprose.\n",
    );
    let at = |id: &str| {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .next()
            .unwrap_or_else(|| panic!("{id} renders"))
    };
    let body = at("MemoryPane::doc-body");
    let pill = at("TagFlow::tag-pill");
    assert!(
        (pill.absolute_position().y - body.absolute_position().y - 16.0).abs() < 0.6,
        "the pills sit at the body's own padding: {} vs {}",
        pill.absolute_position().y,
        body.absolute_position().y
    );
    // no front-matter line reaches the prose
    let g = ui.global::<WikiState>();
    let blocks: Vec<String> = (0..g.get_blocks().row_count())
        .filter_map(|i| g.get_blocks().row_data(i))
        .map(|b| b.text.to_string())
        .collect();
    assert!(
        !blocks.iter().any(|t| t.contains("---") || t.contains("type:")),
        "the header must not render as prose: {blocks:?}"
    );

    // the rule spans the body's inner width, one pane gap from the band
    // above and from the prose below
    let rule = at("MemoryPane::head-rule");
    assert!((rule.size().height - 1.0).abs() < 0.6, "a hairline, not a bar");
    assert!(
        (rule.size().width - (body.size().width - 32.0)).abs() < 1.0,
        "the rule runs the full body width: {} in {}",
        rule.size().width,
        body.size().width
    );
    let chip = i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "RelChip")
        .next()
        .expect("a relation chip renders");
    let gap_above = rule.absolute_position().y - (chip.absolute_position().y + chip.size().height);
    assert!((gap_above - 8.0).abs() < 0.6, "one pane gap above the rule, got {gap_above}");
}

/// A document with no header has no band and therefore no rule - a bare
/// line over nothing reads as a broken layout.
#[cfg(feature = "live-preview")]
#[test]
fn a_headerless_document_shows_no_rule() {
    let ui = tagged_doc_window("# A\n\nprose.\n");
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "MemoryPane::head-rule")
            .count(),
        0,
        "no header, no rule"
    );
}

/// **Relations flow, they do not stack.** Every `key: value` pair is a
/// chip of its own, side by side while the width allows and wrapping to
/// the next row when it does not.
#[cfg(feature = "live-preview")]
#[test]
fn header_relations_flow_side_by_side_and_wrap() {
    let ui = tagged_doc_window(
        "---\ntype: person\nborn: 1975\nworks_at: \"[[b.md|Acme]]\"\n---\n# A\n\nprose.\n",
    );
    let chips = |ui: &AppWindow| -> Vec<(f32, f32, f32)> {
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "RelChip")
            .map(|e| (e.absolute_position().x, e.absolute_position().y, e.size().width))
            .collect()
    };
    let wide = chips(&ui);
    assert_eq!(wide.len(), 3, "one chip per pair");
    assert!(
        wide.iter().all(|c| (c.1 - wide[0].1).abs() < 0.6),
        "at 1400px they share one row: {wide:?}"
    );
    assert!(
        wide.windows(2).all(|w| w[0].0 + w[0].2 <= w[1].0 + 0.6),
        "…side by side, in order: {wide:?}"
    );

    // narrow the window and they wrap instead of running off the pane
    ui.window().set_size(slint::PhysicalSize::new(560, 900));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    let narrow = chips(&ui);
    assert_eq!(narrow.len(), 3, "the chips stay");
    let rows: std::collections::BTreeSet<i32> =
        narrow.iter().map(|c| c.1 as i32).collect();
    assert!(rows.len() > 1, "a narrow pane wraps them: {narrow:?}");
}

/// A long value elides under the chip cap instead of pushing the flow off
/// the pane.
#[cfg(feature = "live-preview")]
#[test]
fn a_long_relation_value_elides_under_the_chip_cap() {
    let long = "x".repeat(400);
    let ui = tagged_doc_window(&format!(
        "---\nnote: \"{long}\"\ntype: person\n---\n# A\n\nprose.\n"
    ));
    let widest = i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "RelChip")
        .map(|e| e.size().width)
        .fold(0.0_f32, f32::max);
    assert!(widest > 0.0, "the chips render");
    assert!(widest <= 321.0, "a chip stays under its cap, got {widest}");
}

/// **A header link sits where its key sits.** `LinkRun` carries a fixed
/// height, and a row hands a fixed-height child the top edge while it
/// stretches the neighbours - so the one chip whose value is a link hung
/// above the centre line the rest of the band shares (reported
/// 2026-09-05, `subcategory_of: "[[…]]"`).
#[cfg(feature = "live-preview")]
#[test]
fn a_header_link_sits_on_the_chip_centre_line() {
    let ui = tagged_doc_window(
        "---\ntype: person\nsubcategory_of: \"[[b.md|Robots]]\"\n---\n# A\n\nprose.\n",
    );
    let mid = |e: &i_slint_backend_testing::ElementHandle| {
        e.absolute_position().y + e.size().height / 2.0
    };
    for font in [14.0_f32, 20.0, 28.0] {
        ui.global::<Theme>().set_fs_app(font);
        let chip =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "RelChip")
                .find(|c| {
                    c.query_descendants().match_id("RelChip::rc-link").find_first().is_some()
                })
                .expect("the linked chip renders");
        let key = chip
            .query_descendants()
            .match_id("RelChip::rc-key")
            .find_first()
            .expect("the key renders");
        let link = chip
            .query_descendants()
            .match_id("RelChip::rc-link")
            .find_first()
            .expect("the link renders");
        assert!(link.size().height > 0.0, "font {font}: the link has no height");
        assert!(
            (mid(&key) - mid(&chip)).abs() < 0.6,
            "font {font}: the key itself is off-centre ({} in {})",
            mid(&key),
            mid(&chip)
        );
        assert!(
            (mid(&link) - mid(&key)).abs() < 0.6,
            "font {font}: the link centres at {} where the key centres at {}",
            mid(&link),
            mid(&key)
        );
        // …and a plain value, which never had the fault, stays put
        let plain_chip =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "RelChip")
                .find(|c| {
                    c.query_descendants().match_id("RelChip::rc-val").find_first().is_some()
                })
                .expect("the plain chip renders");
        let plain = plain_chip
            .query_descendants()
            .match_id("RelChip::rc-val")
            .find_first()
            .expect("the plain value renders");
        assert!(
            (mid(&plain) - mid(&plain_chip)).abs() < 0.6,
            "font {font}: a plain value is off its own chip's centre"
        );
    }
}

/// **A hint never outlives the page it belongs to.** A preview link that
/// is unmounted while hovered never gets a hover-out, so it can never
/// clear its own bubble: the claim stayed on screen while the reader was
/// already on another document (reported 2026-09-05). Every path that
/// replaces the rendered page - navigator, a click on the link itself,
/// the raw editor - has to drop it.
#[cfg(feature = "live-preview")]
#[test]
fn a_page_change_takes_the_link_hint_with_it() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let wiki = wire_wiki(&ui);
    std::mem::forget(wiki);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![
        WikiBase {
            path: "anna.md".into(),
            content: "# Anna\n\n[[works_at::Acme]]\n".into(),
            loaded: true,
        },
        WikiBase {
            path: "Acme.md".into(),
            content: "# Acme\n\nprose.\n".into(),
            loaded: true,
        },
    ])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");

    let tip = |ui: &AppWindow| ui.global::<HintTip>().get_label().to_string();
    let park = |ui: &AppWindow| {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(1350.0, 870.0),
            });
        settle();
    };
    // hover the claim-bearing run and return where the pointer now sits
    let hover = |ui: &AppWindow| -> slint::LogicalPosition {
        let link =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "LinkRun")
                .find(|e| e.size().width > 0.0)
                .expect("the preview renders the link");
        let at = slint::LogicalPosition::new(
            link.absolute_position().x + link.size().width / 2.0,
            link.absolute_position().y + link.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        settle();
        at
    };

    // the navigator: the rows under the pointer vanish, the hint may not
    g.invoke_nav_open(nav_id(&ui, "anna.md"));
    park(&ui);
    hover(&ui);
    assert_eq!(tip(&ui), "Anna works_at Acme", "the tip renders the claim");
    g.invoke_nav_open(nav_id(&ui, "Acme.md"));
    settle();
    assert_eq!(tip(&ui), "", "a page change takes the hint with it");

    // the link's own click, without the pointer ever moving off it
    g.invoke_nav_open(nav_id(&ui, "anna.md"));
    park(&ui);
    let at = hover(&ui);
    assert!(!tip(&ui).is_empty(), "the tip is up before the click");
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
    assert_eq!(tip(&ui), "", "following the link takes the hint with it");

    // the raw editor replaces the preview rows outright
    g.invoke_nav_open(nav_id(&ui, "anna.md"));
    park(&ui);
    hover(&ui);
    assert!(!tip(&ui).is_empty(), "the tip is up before the editor opens");
    g.invoke_edit_toggle();
    settle();
    assert_eq!(tip(&ui), "", "the raw editor takes the hint with it");
}

/// **A header the parser rejects is not a header.** `split_front_matter`
/// says so, and the index, the graph and every write path read those
/// lines as body text - so the preview shows them too. A pane that hid
/// them would be the only component pretending a header is there, and the
/// member would never learn why their tags do not show.
#[cfg(feature = "live-preview")]
#[test]
fn an_unreadable_header_stays_visible_as_prose() {
    let ui = tagged_doc_window("---\ntags: [alpha]\n# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    let blocks: Vec<String> = (0..g.get_blocks().row_count())
        .filter_map(|i| g.get_blocks().row_data(i))
        .map(|b| b.text.to_string())
        .collect();
    assert!(
        blocks.iter().any(|t| t.contains("tags: [alpha]")),
        "an unclosed block is prose, and the member has to see it: {blocks:?}"
    );
    assert_eq!(g.get_tag_pills().row_count(), 0, "…and it is no header");
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "MemoryPane::head-rule")
            .count(),
        0,
        "no band, no rule"
    );
}

/// **A dialog opens ready to type.** The tag modal's first row has the
/// focus, Enter saves, and a modal whose confirm is disabled saves
/// nothing.
#[cfg(feature = "live-preview")]
#[test]
fn the_tag_modal_opens_on_its_first_row_and_enter_saves() {
    let ui = tagged_doc_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_tag_open();
    g.set_tag_modal_open(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));

    // an empty row keeps the confirm disabled - Enter must do nothing
    press(&ui, slint::platform::Key::Return);
    assert!(g.get_tag_modal_open(), "a disabled confirm does not fire");

    type_char(&ui, "x");
    let row = g.get_tag_rows().row_data(0).expect("one row").to_string();
    assert_eq!(row, "x", "the first row had the focus");
    // a second row takes the focus with it: the rebuild destroys the input
    // that had it, and a dialog nothing can type into is a dead dialog
    g.invoke_tag_add("".into());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    type_char(&ui, "y");
    assert_eq!(
        g.get_tag_rows().row_data(1).expect("second row").to_string(),
        "y",
        "the new row has the focus"
    );

    press(&ui, slint::platform::Key::Return);
    assert!(!g.get_tag_modal_open(), "Enter saves and closes");
    assert!(
        g.get_raw().as_str().starts_with("---\ntags: [x, y]\n---\n"),
        "…and it wrote both rows"
    );
}

/// The link modal opens on the TARGET FILTER - the field a member types
/// in first - and Tab walks on to the next input.
#[cfg(feature = "live-preview")]
#[test]
fn the_link_modal_opens_on_the_target_filter_and_tab_walks() {
    let ui = tagged_doc_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_link_open("".into());
    g.set_link_modal_open(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));

    type_char(&ui, "b");
    assert_eq!(g.get_link_filter().as_str(), "b", "the filter had the focus");
    // nothing is picked yet, so Enter must not write
    press(&ui, slint::platform::Key::Return);
    assert!(g.get_link_modal_open(), "a disabled confirm does not fire");

    press(&ui, slint::platform::Key::Tab);
    type_char(&ui, "z");
    assert_eq!(g.get_link_custom().as_str(), "z", "Tab walked to the next input");
    assert_eq!(g.get_link_filter().as_str(), "b", "…and left the filter alone");

    // …and back the other way, to the field above the filter
    back_tab(&ui);
    back_tab(&ui);
    type_char(&ui, "n");
    assert_eq!(g.get_link_name().as_str(), "n", "Shift+Tab walks back");
}

/// The captions a placeholder already carries are gone: fewer rows, and
/// the dialog fits at a large font.
#[cfg(feature = "live-preview")]
#[test]
fn the_link_modal_drops_the_captions_its_placeholders_carry() {
    let ui = tagged_doc_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_link_open("".into());
    g.set_link_modal_open(true);
    g.set_link_header(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    for gone in ["Name", "Target", "Details (optional)"] {
        assert_eq!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, gone).count(),
            0,
            "{gone} is a caption its placeholder already carries"
        );
    }
}

/// A base whose documents carry METADATA ONLY, exactly as `wiki_list`
/// delivers it (`knowledge_base_scale.md` §4.10), plus a recorder for
/// every path the bridge asks bytes for.
fn lazy_base(ui: &AppWindow, paths: &[&str]) -> Rc<RefCell<Vec<String>>> {
    let g = ui.global::<WikiState>();
    let wanted: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let w = wanted.clone();
        g.on_content_wanted(move |p| w.borrow_mut().push(p.to_string()));
    }
    g.set_base_docs(ModelRc::new(VecModel::from(
        paths
            .iter()
            .map(|p| WikiBase {
                path: (*p).into(),
                content: "".into(),
                loaded: false,
            })
            .collect::<Vec<_>>(),
    )));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    wanted
}

/// The navigator verbs are deferred (slint#6426 class); headless, the
/// frame that runs them is `mock_elapsed_time`.
fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
}

/// The navigator row for `label`.
fn nav_id(ui: &AppWindow, label: &str) -> i32 {
    let rows = ui.global::<WikiState>().get_nav_rows();
    (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == label)
        .unwrap_or_else(|| panic!("no nav row {label}"))
        .id
}

/// **A change made in the navigator asks for its ratified bytes even
/// though no tab is open.** The request used to ride the OPEN document
/// only, so a delete or move made from the tree left its bytes unfetched
/// forever - and the vote refused forever with it.
#[test]
fn a_navigator_change_with_no_tab_open_still_asks_for_the_ratified_bytes() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let wanted = lazy_base(&ui, &["a.md"]);
    assert!(
        wanted.borrow().is_empty(),
        "an untouched base wants nothing"
    );
    assert!(!g.get_doc_open(), "no tab is open");

    g.invoke_nav_delete(nav_id(&ui, "a.md"));
    settle();
    assert_eq!(
        wanted.borrow().as_slice(),
        ["a.md".to_string()],
        "the deletion hunk needs the ratified text"
    );
}

/// **The bytes are asked for under the BASE path.** A rename moves the
/// working path while the ratified counterpart stays where it was; asking
/// under the new one got an unknown-path error the engine could never
/// answer, and `load_base` could not have landed the reply either.
#[test]
fn a_rename_with_no_tab_open_asks_under_the_base_path_and_then_patches() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let _wiki = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let wanted = lazy_base(&ui, &["a.md"]);

    g.invoke_nav_rename_start(nav_id(&ui, "a.md"));
    g.invoke_nav_rename_commit(nav_id(&ui, "a.md"), "b.md".into());
    settle();
    assert_eq!(
        wanted.borrow().last().map(String::as_str),
        Some("a.md"),
        "the OLD path is the one the republic ratified"
    );
    assert_eq!(
        g.get_cs_patch().as_str(),
        "",
        "no patch over bytes this node does not hold"
    );

    g.invoke_content_arrived("a.md".into(), "alpha\n".into());
    let patch = g.get_cs_patch().to_string();
    assert!(patch.contains("rename from a.md"), "{patch}");
    assert!(patch.contains("rename to b.md"), "{patch}");
}

/// **A vote over bytes still in flight is QUEUED, never refused.** The
/// click marks the vote pending and re-requests; the arrival fires the
/// proposal without a second click, and exactly once.
#[test]
fn a_vote_over_unfetched_bytes_is_queued_and_fires_on_arrival() {
    i_slint_backend_testing::init_no_event_loop();
    let rt = rt();
    let _guard = rt.enter();
    let w = molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    );
    // the republic has to HOLD a.md, or the local deletion patch would be
    // a change against nothing and the engine would refuse it
    rt.block_on(async {
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: serde_json::json!({
                    "op": "wiki_patch",
                    "summary": "a.md",
                    "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+alpha\n",
                }),
            })
            .await
            .expect("seed propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Approve { proposal: id, note: None })
            .await
            .expect("seed approve");
    });
    let pending = |w: &WalletHandle| {
        rt.block_on(async {
            match w
                .execute(Command::ReadState {
                    surface: Surface::Memory,
                    channel: None,
                    view: None,
                })
                .await
            {
                Ok(Reply::State(s)) => s.pending.len(),
                other => panic!("read memory: {other:?}"),
            }
        })
    };

    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let (model, last) = wire_wiki(&ui);
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    wire_wiki_vote(&ui, &cx, &model, &last);
    let g = ui.global::<WikiState>();
    let wanted = lazy_base(&ui, &["a.md"]);
    g.invoke_nav_delete(nav_id(&ui, "a.md"));
    settle();
    wanted.borrow_mut().clear();

    // the click while the bytes are in flight
    g.invoke_cs_vote();
    assert!(g.get_cs_vote_queued(), "the vote is queued, not refused");
    assert_eq!(
        ui.get_toast_text().as_str(),
        Lexicon::en().mem_toast_vote_queued,
        "the toast promises the vote, it does not send the member away"
    );
    assert_eq!(
        wanted.borrow().last().map(String::as_str),
        Some("a.md"),
        "the click re-requests instead of trusting a flight in progress"
    );
    assert_eq!(pending(&w), 0, "nothing may go out over unheld bytes");

    // …and the arrival fires it, without a second click
    g.invoke_content_arrived("a.md".into(), "alpha\n".into());
    assert!(!g.get_cs_vote_queued(), "the queue emptied");
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Reply::State(s)) = w
                .execute(Command::ReadState {
                    surface: Surface::Memory,
                    channel: None,
                    view: None,
                })
                .await
            {
                if !s.pending.is_empty() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the queued vote never reached the engine");
    });
    assert_eq!(pending(&w), 1);

    // a second arrival must not propose the same patch again
    g.invoke_content_arrived("a.md".into(), "alpha\n".into());
    rt.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(200)).await });
    assert_eq!(pending(&w), 1, "a queued vote fires exactly once");
}

/// The author/version/age line still renders where a document HAS one -
/// at the foot of the preview, under the prose it belongs to.
#[cfg(feature = "live-preview")]
#[test]
fn a_drafted_document_carries_its_meta_line_at_the_foot() {
    let ui = tagged_doc_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_new_file();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    let meta = g.get_doc_meta().to_string();
    assert!(!meta.is_empty(), "a draft has an author, a version and an age");
    let line = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, &meta)
        .next()
        .expect("the meta line renders");
    let lowest_block = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
        &ui,
        "BrainBlockView",
    )
    .map(|e| e.absolute_position().y)
    .fold(f32::MIN, f32::max);
    assert!(lowest_block > f32::MIN, "the prose renders");
    assert!(
        line.absolute_position().y > lowest_block,
        "the meta line sits under the prose, not over the tags"
    );
}

/// Every tag pill the flow is showing - the row branch and the wrapped
/// one carry different ids, and only one of them exists at a time.
#[cfg(feature = "live-preview")]
fn tag_pills(ui: &AppWindow) -> Vec<i_slint_backend_testing::ElementHandle> {
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "TagFlow::tag-pill")
        .chain(i_slint_backend_testing::ElementHandle::find_by_element_id(
            ui,
            "TagFlow::tag-cell",
        ))
        .collect()
}

/// **The tag band wraps.** Many tags on a narrow pane must fall into a
/// second row - a plain start-aligned row ran them off the pane's edge,
/// where the last ones cannot be read at all.
#[cfg(feature = "live-preview")]
#[test]
fn many_tag_pills_wrap_inside_the_pane() {
    let tags: Vec<String> = (0..12).map(|i| format!("longish-tag-{i:02}")).collect();
    let ui = tagged_doc_window(&format!(
        "---\ntags: [{}]\n---\n# A\n\nprose.\n",
        tags.join(", ")
    ));
    ui.window().set_size(slint::PhysicalSize::new(900, 900));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));

    let flow = i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "TagFlow")
        .next()
        .expect("the flow renders");
    let pills = tag_pills(&ui);
    assert_eq!(pills.len(), 12, "one pill per tag");

    let mut rows: Vec<i32> =
        pills.iter().map(|p| p.absolute_position().y.round() as i32).collect();
    rows.sort_unstable();
    rows.dedup();
    assert!(rows.len() > 1, "12 tags do not fit on one row: {rows:?}");

    let right = flow.absolute_position().x + flow.size().width;
    for p in &pills {
        let edge = p.absolute_position().x + p.size().width;
        assert!(edge <= right + 0.6, "a pill ends at {edge}, past the pane's {right}");
    }
}

/// …and while they fit, they keep their own widths: a wrapping grid for
/// three short tags would pad them all to the widest one.
#[cfg(feature = "live-preview")]
#[test]
fn a_few_tag_pills_stay_on_one_row_at_natural_widths() {
    let ui = tagged_doc_window("---\ntags: [a, bbbbbbbbbbbb, cc]\n---\n# A\n\nprose.\n");
    let pills = tag_pills(&ui);
    assert_eq!(pills.len(), 3, "one pill per tag");
    let mut rows: Vec<i32> =
        pills.iter().map(|p| p.absolute_position().y.round() as i32).collect();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(rows.len(), 1, "three tags share one row");
    let widths: Vec<f32> = pills.iter().map(|p| p.size().width).collect();
    assert!(
        widths[0] < widths[1],
        "the pills keep their natural widths: {widths:?}"
    );
}

/// Timing probe, not an assertion: what ONE face sync costs on a real
/// corpus. `MOLT_WIKI_BENCH_CORPUS=<json [{path,content}]>` prints the
/// per-step timings; `MOLT_WIKI_BENCH_LAZY=1` leaves the bytes unfetched
/// (the production shape, K7).
#[test]
#[ignore]
fn wiki_face_sync_cost_on_a_real_corpus() {
    let Ok(path) = std::env::var("MOLT_WIKI_BENCH_CORPUS") else {
        return;
    };
    let raw = std::fs::read_to_string(&path).expect("corpus file");
    let corpus: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("corpus json");
    let lazy = std::env::var("MOLT_WIKI_BENCH_LAZY").is_ok();
    fn time<T>(label: &str, f: impl Fn() -> T) {
        let t = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(f());
        }
        println!("  {label}: {:?}", t.elapsed() / 10);
    }
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let (model, _last) = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let wanted = std::rc::Rc::new(std::cell::Cell::new(0usize));
    {
        let w = wanted.clone();
        g.on_content_wanted(move |_| w.set(w.get() + 1));
    }
    let docs: Vec<WikiBase> = corpus
        .iter()
        .map(|d| WikiBase {
            path: d["path"].as_str().unwrap_or("").into(),
            content: if lazy {
                "".into()
            } else {
                d["content"].as_str().unwrap_or("").into()
            },
            loaded: !lazy,
        })
        .collect();
    let t = std::time::Instant::now();
    g.set_base_docs(ModelRc::new(VecModel::from(docs)));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_fold_all(true); // base folders start closed; the probe wants every row
    println!(
        "base_arrived + fold_all(true): {:?} rows={} lazy={lazy}",
        t.elapsed(),
        g.get_nav_rows().row_count()
    );
    {
        let w = model.borrow();
        time("nav_rows", || w.nav_rows());
        time("to_draft", || w.to_draft());
        time("build_patch", || w.build_patch());
        time("link_targets", || w.link_targets());
    }
    let rows = g.get_nav_rows();
    let files: Vec<i32> = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .filter(|r| !r.is_folder)
        .map(|r| r.id)
        .collect();
    let t = std::time::Instant::now();
    for id in files.iter().take(20) {
        g.invoke_nav_mark(*id);
    }
    println!("nav_mark (no doc open) avg: {:?}", t.elapsed() / 20);
    let big = corpus
        .iter()
        .max_by_key(|d| d["content"].as_str().map_or(0, str::len))
        .expect("a document");
    let big_path = big["path"].as_str().unwrap_or("");
    let big_text = big["content"].as_str().unwrap_or("").to_string();
    let t = std::time::Instant::now();
    g.invoke_open_link(big_path.into());
    println!(
        "open_link(biggest, {} bytes): {:?} blocks={}",
        big_text.len(),
        t.elapsed(),
        g.get_blocks().row_count()
    );
    {
        let w = model.borrow();
        let id = w.active_id().expect("active doc");
        time("preview", || w.preview(id));
        time("infobox", || w.infobox(id));
        time("links", || w.links(id));
        time("to_draft (tab open)", || w.to_draft());
    }
    let t = std::time::Instant::now();
    for id in files.iter().skip(20).take(20) {
        g.invoke_nav_mark(*id);
    }
    println!("nav_mark (doc open) avg: {:?}", t.elapsed() / 20);
    let t = std::time::Instant::now();
    g.invoke_fold_all(false);
    println!(
        "fold_all(false): {:?} rows={}",
        t.elapsed(),
        g.get_nav_rows().row_count()
    );
    let t = std::time::Instant::now();
    g.invoke_reveal();
    println!(
        "reveal: {:?} rows={} can_reveal={}",
        t.elapsed(),
        g.get_nav_rows().row_count(),
        g.get_can_reveal()
    );
    let t = std::time::Instant::now();
    g.invoke_fold_all(true);
    println!(
        "fold_all(true): {:?} rows={}",
        t.elapsed(),
        g.get_nav_rows().row_count()
    );
    g.invoke_edit_toggle();
    let mut text = big_text.clone();
    let t = std::time::Instant::now();
    for _ in 0..20 {
        text.push('x');
        g.invoke_edited(text.as_str().into());
    }
    println!("edited (keystroke) avg: {:?}", t.elapsed() / 20);
    {
        let w = model.borrow();
        let id = w.active_id().expect("active doc");
        time("preview (dirty)", || w.preview(id));
        time("build_patch (dirty)", || w.build_patch());
        time("to_draft (dirty)", || w.to_draft());
    }
    println!("content_wanted fired {} times", wanted.get());
}

/// Paint probe, not an assertion: the wiki pane rendered OFFSCREEN through
/// Slint's software renderer, so the cost of a frame - layout plus
/// rasterization - is measured, not only the model side. Runs alone (it
/// installs its own platform): `MOLT_WIKI_BENCH_CORPUS=<json>`,
/// `MOLT_PAINT_MODE=full|partial` (a fresh buffer every frame, or the
/// dirty-region repaint the software renderer offers).
#[test]
#[ignore]
fn wiki_pane_paint_cost_offscreen() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::Rgb8Pixel;
    let Ok(path) = std::env::var("MOLT_WIKI_BENCH_CORPUS") else {
        return;
    };
    let raw = std::fs::read_to_string(&path).expect("corpus file");
    let corpus: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("corpus json");
    let lazy = std::env::var("MOLT_WIKI_BENCH_LAZY").is_ok();
    let partial = std::env::var("MOLT_PAINT_MODE").map_or(true, |m| m != "full");
    struct P(std::rc::Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for P {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    const W: u32 = 1600;
    const H: u32 = 1000;
    let win = MinimalSoftwareWindow::new(if partial {
        RepaintBufferType::ReusedBuffer
    } else {
        RepaintBufferType::NewBuffer
    });
    slint::platform::set_platform(Box::new(P(win.clone()))).expect("platform");
    win.set_size(slint::PhysicalSize::new(W, H));
    let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
    let ui = AppWindow::new().expect("window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    let (_model, _last) = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    let docs: Vec<WikiBase> = corpus
        .iter()
        .map(|d| WikiBase {
            path: d["path"].as_str().unwrap_or("").into(),
            content: if lazy {
                "".into()
            } else {
                d["content"].as_str().unwrap_or("").into()
            },
            loaded: !lazy,
        })
        .collect();
    g.set_base_docs(ModelRc::new(VecModel::from(docs)));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_fold_all(true); // base folders start closed; the probe wants every row
    ui.show().expect("show");
    let mut frame = |label: &str| {
        let t = std::time::Instant::now();
        let mut region = None;
        let drew = win.draw_if_needed(|r| {
            region = Some(r.render(&mut buf, W as usize));
        });
        let dims = region.map(|r| r.bounding_box_size());
        println!(
            "  frame {label}: {:?} drew={drew} dirty={}x{}",
            t.elapsed(),
            dims.map_or(0, |d| d.width),
            dims.map_or(0, |d| d.height)
        );
    };
    println!("mode={} lazy={lazy} rows={}", if partial { "partial" } else { "full" }, g.get_nav_rows().row_count());
    frame("first (cold)");
    frame("idle (nothing changed)");
    let rows = g.get_nav_rows();
    let files: Vec<i32> = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .filter(|r| !r.is_folder)
        .map(|r| r.id)
        .collect();
    g.invoke_nav_mark(files[3]);
    frame("after nav_mark");
    g.invoke_nav_mark(files[40]);
    frame("after nav_mark #2");
    // hover: the pointer crossing three navigator rows
    for y in [200.0f32, 226.0, 252.0] {
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(120.0, y),
        });
        frame("after pointer move over nav");
    }
    let big = corpus
        .iter()
        .max_by_key(|d| d["content"].as_str().map_or(0, str::len))
        .expect("a document");
    g.invoke_open_link(big["path"].as_str().unwrap_or("").into());
    frame("after open_link(biggest)");
    frame("idle");
    g.invoke_nav_mark(files[7]);
    frame("after nav_mark (doc open)");
    g.invoke_fold_all(false);
    frame("after fold_all(false)");
    g.invoke_reveal();
    frame("after reveal");
    g.invoke_fold_all(true);
    frame("after fold_all(true)");
    // hover over the document body
    for y in [300.0f32, 340.0, 380.0] {
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(900.0, y),
        });
        frame("after pointer move over doc");
    }
    // what every engine event does: the surfaces model rewritten
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "memory".into(),
        ..SurfaceTab::default()
    }])));
    frame("after surfaces rewrite (engine event)");
    g.invoke_edit_toggle();
    frame("after edit_toggle");
    let mut text = big["content"].as_str().unwrap_or("").to_string();
    for _ in 0..3 {
        text.push('x');
        g.invoke_edited(text.as_str().into());
        frame("after keystroke");
    }
}

