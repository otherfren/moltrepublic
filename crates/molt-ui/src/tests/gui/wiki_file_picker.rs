// SPDX-License-Identifier: GPL-3.0-or-later
//! **The wiki's `+ Datei` picker** (`wiki_files_and_images.md` §3.6): the
//! toolbar button, the modal over the republic's PERSISTENT files, and the
//! one click that writes `![name](upload:<12 hex>)` / `[name](upload:<12
//! hex>)` at the caret.

use super::*;

/// A Shared-Memory window with one open document, the picker's stage.
#[cfg(feature = "live-preview")]
fn picker_window(content: &str) -> AppWindow {
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
    std::mem::forget(wire_wiki(&ui));
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

/// One share, as `read_uploads` projects it.
fn upload(name: &str, kind: &str, member: &str, sum: &str, persistent: bool) -> molt_core::UploadView {
    molt_core::UploadView {
        id: MessageId::NIL,
        member: member.to_string(),
        ts: 0,
        name: name.to_string(),
        kind: kind.to_string(),
        size: 12_288,
        available: true,
        expires_ts: 0,
        online: true,
        checksum: sum.to_string(),
        download: None,
        availability: "relay-held".to_string(),
        local: String::new(),
        persistent,
        mirrors: 0,
        mirror_held: 0,
        mirror_of: 0,
    }
}

/// The two rows every modal test starts from: an image and a PDF.
#[cfg(feature = "live-preview")]
fn rows() -> ModelRc<WikiFileRow> {
    let ups = vec![
        upload("Netzplan.png", "Image", "petra", &"1".repeat(64), true),
        upload("Bericht.pdf", "PDF", "walter", &"2".repeat(64), true),
    ];
    ModelRc::new(VecModel::from(file_picks(&ups, 0)))
}

/// What the picker is showing right now.
#[cfg(feature = "live-preview")]
fn shown(ui: &AppWindow) -> Vec<String> {
    ui.global::<WikiState>()
        .get_file_rows()
        .iter()
        .map(|r| r.name.to_string())
        .collect()
}

/// **A temporary share is never a target** (Q2): the page would outlive
/// it. Neither is a share the grammar cannot name - a reference names a
/// file by its CONTENT, so a missing or too-short checksum is no target
/// either.
#[test]
fn only_a_persistent_share_the_grammar_can_name_reaches_the_picker() {
    let ups = vec![
        upload("Netzplan.png", "Image", "petra", &"1".repeat(64), true),
        upload("scratch.txt", "Text", "petra", &"3".repeat(64), false),
        upload("legacy.pdf", "PDF", "walter", "", true),
        upload("stub.pdf", "PDF", "walter", "abc", true),
    ];
    let picks = file_picks(&ups, 0);
    assert_eq!(picks.len(), 1, "{picks:?}");
    assert_eq!(picks[0].name.as_str(), "Netzplan.png");
    assert_eq!(picks[0].checksum.as_str(), &"1".repeat(64));
    assert_eq!(picks[0].glyph.as_str(), "🖼️");
    assert_eq!(picks[0].detail.as_str(), "12 KiB · by petra");
    assert_eq!(
        file_picks(&ups, 1)[0].detail.as_str(),
        "12 KiB · von petra",
        "the sharer line speaks the active language"
    );
    assert_eq!(file_kind_glyph("Something else"), "📎");
}

/// The toolbar's own button raises the picker, and it opens on the search
/// field - the way in is typing, not clicking.
#[cfg(feature = "live-preview")]
#[test]
fn the_file_button_opens_the_picker_on_its_search_field() {
    let ui = picker_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    // the engine read the open fires - once, not once per sync
    let asked = Rc::new(RefCell::new(0_u32));
    {
        let a = asked.clone();
        g.on_file_wanted(move || *a.borrow_mut() += 1);
    }
    let button =
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "+ File")
            .next()
            .expect("the toolbar offers the file picker");
    click(&ui, &button);
    assert!(g.get_file_modal_open(), "the toolbar raises the modal");
    assert_eq!(*asked.borrow(), 1, "opening asks the engine for the files");
    g.invoke_set_file_filter("x".into());
    assert_eq!(*asked.borrow(), 1, "…and a keystroke does not ask again");
    assert!(!g.get_file_loaded(), "no answer yet: the empty line stays away");
    g.invoke_set_file_filter("".into());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    type_char(&ui, "b");
    assert_eq!(g.get_file_filter().as_str(), "b", "the search field had the focus");
}

/// Escape cancels the picker from inside its field, and takes the needle
/// with it - the next open starts clean.
#[cfg(feature = "live-preview")]
#[test]
fn escape_cancels_the_picker_and_drops_its_needle() {
    let ui = picker_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(rows());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    type_char(&ui, "b");
    assert_eq!(g.get_file_filter().as_str(), "b");
    press(&ui, slint::platform::Key::Escape);
    assert!(!g.get_file_modal_open(), "Escape closes it from the field");

    g.invoke_file_open();
    g.set_file_modal_open(true);
    assert_eq!(g.get_file_filter().as_str(), "", "the needle went with it");
    assert!(shown(&ui).is_empty(), "…and so did the stale list");
}

/// Enter in the search field takes the TOP row - the dialog's own button
/// only closes, so confirming from the field must not throw the search
/// away. An empty list has nothing to take, and Enter does nothing.
#[cfg(feature = "live-preview")]
#[test]
fn enter_in_the_search_field_takes_the_top_row() {
    let ui = picker_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(ModelRc::new(VecModel::from(Vec::<WikiFileRow>::new())));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    press(&ui, slint::platform::Key::Return);
    assert!(g.get_file_modal_open(), "nothing to take, nothing happens");

    g.invoke_file_list(rows());
    type_char(&ui, "b");
    type_char(&ui, "e");
    assert_eq!(shown(&ui), ["Bericht.pdf"]);
    press(&ui, slint::platform::Key::Return);
    assert!(!g.get_file_modal_open(), "Enter picked and closed");
    assert!(
        g.get_raw().as_str().contains("[Bericht.pdf](upload:222222222222)"),
        "{}",
        g.get_raw()
    );
}

/// Typing narrows the list over the name AND the sharer, in Rust - the
/// pane renders what it is handed.
#[cfg(feature = "live-preview")]
#[test]
fn typing_filters_the_picker_over_name_and_sharer() {
    let ui = picker_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(rows());
    assert_eq!(shown(&ui), ["Netzplan.png", "Bericht.pdf"]);
    assert!(g.get_file_loaded());

    g.invoke_set_file_filter("beri".into());
    assert_eq!(shown(&ui), ["Bericht.pdf"]);
    g.invoke_set_file_filter("PETRA".into());
    assert_eq!(shown(&ui), ["Netzplan.png"], "the sharer filters too");
    g.invoke_set_file_filter("nothing here".into());
    assert!(shown(&ui).is_empty());
    // …and a filter with no hit says "no hit", never "persist a file first"
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(
            &ui,
            "No file is kept permanently yet"
        )
        .count(),
        0,
        "a filtered-away list is not an empty republic"
    );
}

/// **One click writes the reference at the caret and closes the modal.**
/// An Image gets the inline form, anything else the link form.
#[cfg(feature = "live-preview")]
#[test]
fn a_click_on_a_row_writes_the_reference_at_the_caret() {
    let ui = picker_window("# A\n\nSiehe hier.\n");
    let g = ui.global::<WikiState>();
    // the raw editor is open and the caret sits after "Siehe"
    g.invoke_edit_toggle();
    g.invoke_set_cursor(i32::try_from("# A\n\nSiehe".len()).expect("offset"));
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(rows());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));

    let row = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Netzplan.png")
        .next()
        .expect("the image row renders");
    click(&ui, &row);
    assert!(!g.get_file_modal_open(), "the pick closed the picker");
    assert_eq!(g.get_file_error().as_str(), "");
    assert_eq!(
        g.get_raw().as_str(),
        "# A\n\nSiehe ![Netzplan.png](upload:111111111111) hier.\n",
        "an Image renders inline"
    );

    // …and a PDF is a link, at the end when no caret rides along
    g.invoke_edit_toggle();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(rows());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    let row = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Bericht.pdf")
        .next()
        .expect("the pdf row renders");
    click(&ui, &row);
    assert!(!g.get_file_modal_open());
    assert!(
        g.get_raw()
            .as_str()
            .ends_with("\n\n[Bericht.pdf](upload:222222222222)\n"),
        "{}",
        g.get_raw()
    );
}

/// The written page carries the reference the GRAMMAR reads back - the
/// contract every other part of the feature resolves against.
///
/// NOTE: the preview's own SPAN for an `upload:` destination is the
/// parser change that rides with the renderer (§3.4); this worktree
/// predates it, so the assertion is over the page's text and
/// `wiki_refs::file_refs`, which is what the span will be built from.
#[cfg(feature = "live-preview")]
#[test]
fn the_written_reference_is_the_one_the_grammar_reads_back() {
    let ui = picker_window("# A\n\nprose.\n");
    let g = ui.global::<WikiState>();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    g.invoke_file_list(rows());
    g.invoke_file_pick(slint::SharedString::from("1".repeat(64)));

    let refs = molt_core::wiki_refs::file_refs(g.get_raw().as_str());
    assert_eq!(refs.len(), 1, "{}", g.get_raw());
    assert_eq!(refs[0].hex, "111111111111");
    assert_eq!(refs[0].alt, "Netzplan.png");
    assert!(refs[0].image);
    assert!(refs[0].valid(), "a 12-hex prefix is a resolvable reference");
    // the page also says it out loud, so the raw editor shows what landed
    assert!(g.get_raw().as_str().contains("![Netzplan.png](upload:111111111111)"));
}

/// **A republic that keeps nothing permanently says so in one line** and
/// hands the member over to Shared Files, where the persist vote lives.
#[cfg(feature = "live-preview")]
#[test]
fn the_empty_picker_offers_the_jump_to_shared_files() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());

    let ui = picker_window("# A\n\nprose.\n");
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    wire_wiki_file_picker(&ui, &cx);
    let g = ui.global::<WikiState>();
    g.invoke_file_open();
    g.set_file_modal_open(true);
    // the read landed, and this republic keeps nothing permanently
    g.invoke_file_list(ModelRc::new(VecModel::from(Vec::<WikiFileRow>::new())));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    assert_eq!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(
            &ui,
            "No file is kept permanently yet"
        )
        .count(),
        1,
        "the one line, once"
    );

    let jump = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Shared Files")
        .next()
        .expect("the empty state offers the jump");
    click(&ui, &jump);
    assert!(!g.get_file_modal_open(), "the jump closes the picker");
    let landed = rt.block_on(async {
        for _ in 0..200 {
            if let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await {
                if sv.surface == Surface::Files && sv.view == "persistent" {
                    return true;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    });
    assert!(landed, "the jump selects Shared Files › Persistent Uploads");
}

/// The picker is a modal like the other two: its buttons stay inside the
/// window at every app font size.
#[cfg(feature = "live-preview")]
#[test]
fn the_file_picker_fits_the_window_at_every_font_size() {
    let ui = picker_window("# A\n\nprose.\n");
    ui.window().set_size(slint::PhysicalSize::new(1100, 760));
    let g = ui.global::<WikiState>();
    for font in [14.0_f32, 20.0, 26.0] {
        ui.global::<Theme>().set_fs_app(font);
        g.invoke_file_open();
        g.set_file_modal_open(true);
        g.invoke_file_list(rows());
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        let modals =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
                .count();
        assert_eq!(modals, 1, "exactly one modal is up at font {font}");
        let low = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppButton::abtn-label",
        )
        .filter(|e| e.accessible_label().is_some_and(|l| l == "Close"))
        .map(|e| e.absolute_position().y + e.size().height)
        .fold(f32::MIN, f32::max);
        assert!(low > f32::MIN, "the dialog's own button must be found");
        assert!(
            low <= 760.0,
            "the file picker at font {font}: its button ends at {low}, past the window"
        );
        g.set_file_modal_open(false);
        g.invoke_file_close();
    }
}
