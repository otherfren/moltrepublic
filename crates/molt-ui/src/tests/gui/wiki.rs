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
        w.execute(Command::Approve { proposal: id })
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

/// The ontology is content, not code: nothing in the UI prescribes a
/// header, so a document without one has to OFFER it - with the keys the
/// republic already uses. The offer disappears once a header exists.
#[test]
fn a_document_without_a_header_offers_the_republics_own_keys() {
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
    // the vocabulary the engine derived from the ratified tree
    g.set_prop_keys(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("tags"),
        slint::SharedString::from("type"),
    ])));
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

    open("bare.md");
    assert!(g.get_can_add_property(), "a header-less document offers one");
    // the chips render what the engine offered, plus the generic starter
    let chips: Vec<String> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "PropChip")
            .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
            .collect();
    assert_eq!(
        chips,
        vec!["+ Property".to_string(), "tags".to_string(), "type".to_string()],
        "the offer leads with the generic starter, then the republic's keys"
    );

    // one click writes the syntax and hands over to the editor
    g.invoke_add_property("type".into());
    assert!(g.get_editing(), "the header is typed, not read");
    assert_eq!(g.get_raw().as_str(), "---\ntype: \n---\n# Bare\n\nNo header here.");

    // a document that HAS a header makes no offer
    open("anna.md");
    assert!(!g.get_can_add_property());
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "PropChip")
            .count(),
        0,
        "no offer, no chips"
    );
}

/// **K6 §4.9.6 in the window**: while the folded base is being fetched the
/// pane must not read as an empty knowledge base. "Nothing here yet" and
/// "not here YET" are different claims, and the second one is the true one.
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
