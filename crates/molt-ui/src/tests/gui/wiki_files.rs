// SPDX-License-Identifier: GPL-3.0-or-later
//! Wiki file references (`docs_archive/ui/wiki_files_and_images.md` §3.4/§3.5),
//! headless: every resolution state renders its word, a file link jumps
//! to Shared Files, a ready picture opens large, and an arriving picture
//! patches the row it belongs to - never the whole page.

use super::*;

/// A 12-hex reference prefix and the full sha256 it resolves to.
const HEX: &str = "3f9a2c1b7e04";
const FULL: &str = "3f9a2c1b7e04d5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0";

/// A share the engine would answer with. Every state below is one field
/// of this, so the fixture stays the ONE place the shape lives.
fn upload(name: &str) -> molt_core::UploadView {
    molt_core::UploadView {
        id: MessageId([7u8; 16]),
        member: "petra".to_string(),
        ts: 0,
        name: name.to_string(),
        kind: "Image".to_string(),
        size: 2048,
        available: true,
        expires_ts: 0,
        online: true,
        checksum: FULL.to_string(),
        download: None,
        availability: "relay-held".to_string(),
        local: String::new(),
        persistent: true,
        mirrors: 1,
        mirror_held: 0,
        mirror_of: 0,
    }
}

fn resolved(u: Option<molt_core::UploadView>, local: molt_core::LocalCopy) -> RefAnswer {
    RefAnswer {
        upload: u,
        ambiguous: false,
        temporary: false,
        local,
    }
}

/// A wiki pane with ONE open document, rendered headless, plus what the
/// bridge asked the engine about - recorded from before the first sync,
/// because a reference is CLAIMED the moment it is asked about.
#[cfg(feature = "live-preview")]
fn page_asking(body: &str) -> (AppWindow, Rc<RefCell<Vec<String>>>) {
    i_slint_backend_testing::init_no_event_loop();
    reset_file_refs();
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
    let asked: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let seen = asked.clone();
        ui.global::<WikiState>().on_file_refs_wanted(move |hexes| {
            seen.borrow_mut()
                .extend(hexes.iter().map(|h| h.to_string()));
        });
    }
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "note.md".into(),
        content: body.into(),
        loaded: true,
    }])));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "note.md")
        .expect("the note row")
        .id;
    g.invoke_nav_open(id);
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.show().expect("show headless");
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    (ui, asked)
}

#[cfg(feature = "live-preview")]
fn page(body: &str) -> AppWindow {
    page_asking(body).0
}

/// The image block of the open page.
fn image_block(ui: &AppWindow) -> WikiBlock {
    let blocks = ui.global::<WikiState>().get_blocks();
    (0..blocks.row_count())
        .filter_map(|i| blocks.row_data(i))
        .find(|b| b.kind == 5)
        .expect("the page renders an image block")
}

/// The one span of the open page that is a file run.
fn file_span(ui: &AppWindow) -> WikiSpan {
    let blocks = ui.global::<WikiState>().get_blocks();
    (0..blocks.row_count())
        .filter_map(|i| blocks.row_data(i))
        .flat_map(|b| {
            let s = b.spans;
            (0..s.row_count()).filter_map(move |i| s.row_data(i))
        })
        .find(|sp| sp.file_state != file_state::NONE)
        .expect("the page renders a file run")
}

/// A 2x2 opaque PNG - the smallest honest picture a decode can produce.
fn png_2x2() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode the fixture");
    out.into_inner()
}

/// **Every resolution state renders its own word.** The card is the only
/// thing between a member and a reference they cannot open, so each
/// verdict has to be visible and distinct.
#[cfg(feature = "live-preview")]
#[test]
fn every_reference_state_renders_its_word() {
    let cases: Vec<(RefAnswer, i32, &str)> = vec![
        (
            RefAnswer {
                upload: None,
                ambiguous: false,
                temporary: false,
                local: molt_core::LocalCopy::None,
            },
            file_state::UNKNOWN,
            "unknown file",
        ),
        (
            RefAnswer {
                upload: None,
                ambiguous: true,
                temporary: false,
                local: molt_core::LocalCopy::None,
            },
            file_state::AMBIGUOUS,
            "ambiguous reference",
        ),
        (
            RefAnswer {
                upload: Some(upload("netz.png")),
                ambiguous: false,
                temporary: true,
                local: molt_core::LocalCopy::None,
            },
            file_state::TEMPORARY,
            "not persistent yet",
        ),
        (
            resolved(
                Some(molt_core::UploadView {
                    online: false,
                    availability: "gone".to_string(),
                    available: false,
                    ..upload("netz.png")
                }),
                molt_core::LocalCopy::None,
            ),
            file_state::REMOTE,
            "gone",
        ),
        (
            resolved(
                Some(upload("netz.png")),
                molt_core::LocalCopy::Partial { held: 2, of: 5 },
            ),
            file_state::FETCHING,
            "mirroring 40 %",
        ),
        (
            resolved(
                Some(molt_core::UploadView {
                    download: Some(molt_core::DownloadView {
                        phase: "transferring".to_string(),
                        percent: 30,
                        path: String::new(),
                        error: String::new(),
                    }),
                    ..upload("netz.png")
                }),
                molt_core::LocalCopy::None,
            ),
            file_state::FETCHING,
            "loading 30 %",
        ),
        (
            resolved(
                Some(upload("netz.png")),
                molt_core::LocalCopy::Own {
                    path: "/tmp/netz.png".to_string(),
                },
            ),
            file_state::DECODING,
            "reading",
        ),
    ];
    let ui = page(&format!("Intro\n\n![Netz](upload:{HEX})\n"));
    for (answer, want_state, want_word) in cases {
        file_resolved(&ui, HEX, Some(answer));
        let b = image_block(&ui);
        assert_eq!(b.file_state, want_state, "state for {want_word}");
        assert_eq!(b.file_avail.as_str(), want_word);
    }
    // the engine refusing at all is the `failed` card - and it offers the
    // one verb that can still help
    file_resolved(&ui, HEX, None);
    let b = image_block(&ui);
    assert_eq!(b.file_state, file_state::FAILED);
    assert_eq!(b.file_avail.as_str(), "unreadable");
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Reload")
            .next()
            .is_some(),
        "a failed card offers the reload verb"
    );
}

/// **A malformed reference never reaches the engine.** The pane can tell
/// a bad hex from a missing file by itself, and asking would only teach
/// the member that the fault is somewhere else.
#[cfg(feature = "live-preview")]
#[test]
fn a_malformed_reference_reads_as_unknown_without_asking() {
    let (ui, asked) = page_asking("Intro\n\n![Netz](upload:abc)\n");
    patch_file_rows(&ui);
    assert_eq!(image_block(&ui).file_state, file_state::UNKNOWN);
    assert!(asked.borrow().is_empty(), "a malformed hex is never asked about");
}

/// **A file link's click sets the needle and switches the view.** The
/// jump is the whole point of a file reference the pane cannot render:
/// the member has to land on the row, not on the table.
#[cfg(feature = "live-preview")]
#[test]
fn a_file_links_click_filters_shared_files_to_that_one_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = page(&format!("Read [Bericht Q3](upload:{HEX}) today.\n"));
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let ctx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: chat_ui.clone(),
    };
    wire_wiki_index(&ui, &ctx);
    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("bericht.pdf")),
            molt_core::LocalCopy::None,
        )),
    );
    let sp = file_span(&ui);
    assert_eq!(sp.file_name.as_str(), "bericht.pdf");
    assert_eq!(sp.file_size.as_str(), "2 KiB");

    let run =
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Bericht Q3")
            .next()
            .expect("the file run renders");
    click(&ui, &run);
    assert_eq!(
        chat_ui.lock().expect("chat ui").uploads_filter,
        FULL,
        "the needle is the FULL checksum, not the reference prefix"
    );
    // the view switch rides `Ctx::issue`, which SPAWNS - the read has to
    // wait for it, or the assertion races the command it just fired
    rt.block_on(async {
        for _ in 0..200 {
            if let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await {
                if sv.surface == Surface::Files && sv.view == "persistent" {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the click never selected Files/persistent");
    });
}

/// **A ready picture opens large, and Escape closes it again.** A picture
/// in a text column is a thumbnail; the reason to embed one is being able
/// to look at it.
#[cfg(feature = "live-preview")]
#[test]
fn a_ready_pictures_click_opens_the_large_view_and_escape_closes_it() {
    let ui = page(&format!("Intro\n\n![Netz](upload:{HEX})\n"));
    let want = file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::Own {
                path: "/tmp/netz.png".to_string(),
            },
        )),
    );
    assert_eq!(want.as_deref(), Some(FULL), "a local picture wants its bytes");
    image_decoded(&ui, HEX, crate::images::decode_wiki_image(&png_2x2()));
    assert_eq!(image_block(&ui).file_state, file_state::READY);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));

    let pic = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "BrainBlockView::pica")
        .next()
        .expect("the picture renders");
    click(&ui, &pic);
    let g = ui.global::<WikiState>();
    assert!(g.get_file_view_open(), "the click opens the large view");
    assert_eq!(g.get_file_view_caption().as_str(), "netz.png · 2 KiB · by petra");
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    press(&ui, slint::platform::Key::Escape);
    assert!(!g.get_file_view_open(), "Escape closes it");
}

/// **An arriving picture patches its row, and nothing else.** A resolve
/// that rebuilt the block model would re-create every element of the
/// page - the perf wave's whole point (`wiki_pane_performance.md` F4).
#[cfg(feature = "live-preview")]
#[test]
fn an_arriving_picture_patches_the_row_in_place() {
    let ui = page(&format!("# Head\n\nIntro\n\n![Netz](upload:{HEX})\n\nAfter.\n"));
    let g = ui.global::<WikiState>();
    let before = g.get_blocks();
    let texts: Vec<slint::SharedString> = (0..before.row_count())
        .filter_map(|i| before.row_data(i))
        .map(|b| b.text)
        .collect();
    assert!(texts.len() >= 4, "the page renders around the picture");

    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::Mirrored,
        )),
    );
    image_decoded(&ui, HEX, crate::images::decode_wiki_image(&png_2x2()));

    let after = g.get_blocks();
    assert!(
        std::ptr::eq(
            before
                .as_any()
                .downcast_ref::<VecModel<WikiBlock>>()
                .expect("blocks are a VecModel") as *const _,
            after
                .as_any()
                .downcast_ref::<VecModel<WikiBlock>>()
                .expect("still a VecModel") as *const _,
        ),
        "a resolve and a decode patch rows - they never swap the model"
    );
    let now: Vec<slint::SharedString> = (0..after.row_count())
        .filter_map(|i| after.row_data(i))
        .map(|b| b.text)
        .collect();
    assert_eq!(now, texts, "the prose around the picture is untouched");
    let b = image_block(&ui);
    assert_eq!(b.file_state, file_state::READY);
    assert_eq!(b.image.size().width, 2, "the decoded picture is on the row");
}

/// **A resolve asks the engine once per distinct hex.** The face syncs on
/// every callback, so an unclaimed question would storm the engine the
/// way `content_wanted` once did (F5).
#[cfg(feature = "live-preview")]
#[test]
fn a_reference_is_asked_about_once_however_often_the_face_syncs() {
    let (ui, asked) = page_asking(&format!(
        "![Netz](upload:{HEX})\n\nSee [Bericht](upload:{HEX}) and [Plan](upload:aaaaaaaaaaaa).\n"
    ));
    for _ in 0..5 {
        patch_file_rows(&ui);
    }
    let mut seen = asked.borrow().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec!["3f9a2c1b7e04".to_string(), "aaaaaaaaaaaa".to_string()],
        "two distinct files, one question each - the third occurrence rides the first"
    );
}

/// **A ghost block never fetches.** A picture the changeset removed is
/// history; asking the network for its bytes would be work for a block
/// that is already struck through.
#[cfg(feature = "live-preview")]
#[test]
fn a_ghost_picture_renders_flat_and_offers_no_verb() {
    let ui = page(&format!("Intro\n\n![Netz](upload:{HEX})\n\nAfter.\n"));
    let g = ui.global::<WikiState>();
    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::None,
        )),
    );
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Download")
            .next()
            .is_some(),
        "a live card offers the fetch verb"
    );
    // drop the picture from the working copy: the base still has it, so
    // the preview keeps it as a ghost
    g.invoke_edit_toggle();
    g.invoke_edited("Intro\n\nAfter.\n".into());
    g.invoke_edit_toggle();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    let ghost = image_block(&ui);
    assert_eq!(ghost.status, 3, "the removed picture previews as a ghost");
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Download")
            .next()
            .is_none(),
        "a ghost card offers no verb"
    );
}


/// **A completing transfer re-resolves the reference that names it.**
/// Without this the picture the member just downloaded stays a card until
/// the page is reopened - and the mirror case never resolves at all,
/// because nothing else ever asks again.
#[cfg(feature = "live-preview")]
#[test]
fn a_transfer_and_a_completed_mirror_both_re_resolve_their_reference() {
    let (ui, asked) = page_asking(&format!("Intro\n\n![Netz](upload:{HEX})\n"));
    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::None,
        )),
    );
    assert_eq!(asked.borrow().len(), 1, "asked once so far");

    transfer_touched(&ui, MessageId([7u8; 16]));
    assert_eq!(asked.borrow().len(), 2, "the transfer's share is asked again");
    assert_eq!(
        image_block(&ui).file_name.as_str(),
        "",
        "the answer is forgotten while the new one is in flight"
    );

    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::None,
        )),
    );
    // the mirror finished for this very file: the bytes arrive by
    // themselves, so the reference has to be asked about again
    let rows = vec![UploadRowData {
        checksum_full: FULL.to_string(),
        mirror_held: 5,
        mirror_of: 5,
        ..upload_row()
    }];
    uploads_pushed(&ui, &rows);
    assert_eq!(asked.borrow().len(), 3, "the completed mirror asks again");
}

/// An uploads row with nothing set but the fields the wiki reads.
fn upload_row() -> UploadRowData {
    UploadRowData {
        id: String::new(),
        user: String::new(),
        date: String::new(),
        name: String::new(),
        kind: String::new(),
        size: String::new(),
        available: false,
        online: false,
        checksum: String::new(),
        expires: String::new(),
        status: String::new(),
        status_kind: 0,
        availability: String::new(),
        ts: 0,
        bytes: 0,
        expires_ts: 0,
        checksum_full: String::new(),
        persistent: true,
        vote: String::new(),
        mirrors: 0,
        mirror_held: 0,
        mirror_of: 0,
    }
}


/// **A page whose pictures exceed the budget still settles.** Evicting
/// one the open page references has it re-asked, re-read, re-decoded and
/// evict the next - a livelock of resolve traffic and repaints that
/// never ends. The open page's pictures are therefore never evicted.
#[cfg(feature = "live-preview")]
#[test]
fn a_page_bigger_than_the_picture_budget_settles_instead_of_looping() {
    let hexes = ["aaaaaaaaaaaa", "bbbbbbbbbbbb", "cccccccccccc"];
    let body: String = hexes
        .iter()
        .map(|h| format!("![p](upload:{h})\n\n"))
        .collect();
    let (ui, asked) = page_asking(&body);
    // one 2x2 RGBA decode is 16 bytes: a 24-byte budget holds one
    set_image_budget(24);
    for (i, hex) in hexes.iter().enumerate() {
        let full = format!("{:064x}", i + 1);
        let want = file_resolved(
            &ui,
            hex,
            Some(resolved(
                Some(molt_core::UploadView {
                    checksum: full.clone(),
                    ..upload("netz.png")
                }),
                molt_core::LocalCopy::Mirrored,
            )),
        );
        assert_eq!(want.as_deref(), Some(full.as_str()));
        image_decoded(&ui, hex, crate::images::decode_wiki_image(&png_2x2()));
    }
    assert_eq!(
        asked.borrow().len(),
        hexes.len(),
        "each reference was asked about exactly once"
    );
    let blocks = ui.global::<WikiState>().get_blocks();
    let ready = (0..blocks.row_count())
        .filter_map(|i| blocks.row_data(i))
        .filter(|b| b.kind == 5 && b.file_state == file_state::READY)
        .count();
    assert_eq!(ready, hexes.len(), "every picture of the page stays ready");
    // …and it stays settled: another sync asks for nothing
    for _ in 0..3 {
        patch_file_rows(&ui);
    }
    assert_eq!(asked.borrow().len(), hexes.len(), "nothing is asked again");
    set_image_budget(usize::MAX);
}

/// **A workspace switch drops every answer.** The share id in a cached
/// answer belongs to ONE republic - the download verb would otherwise
/// address a foreign message, and the jump would filter on a checksum
/// this republic does not have.
#[cfg(feature = "live-preview")]
#[test]
fn a_workspace_switch_re_resolves_even_at_the_same_base_revision() {
    let (ui, asked) = page_asking(&format!("Intro\n\n![Netz](upload:{HEX})\n"));
    let g = ui.global::<WikiState>();
    file_resolved(
        &ui,
        HEX,
        Some(resolved(
            Some(upload("netz.png")),
            molt_core::LocalCopy::None,
        )),
    );
    assert_eq!(asked.borrow().len(), 1);
    assert_eq!(image_block(&ui).file_name.as_str(), "netz.png");

    // the same base revision, another republic
    g.set_ws_id("other".into());
    g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
        path: "note.md".into(),
        content: format!("Intro\n\n![Netz](upload:{HEX})\n").into(),
        loaded: true,
    }])));
    g.invoke_base_arrived();
    let rows = g.get_nav_rows();
    let id = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.label.as_str() == "note.md")
        .expect("the note row")
        .id;
    g.invoke_nav_open(id);
    assert_eq!(asked.borrow().len(), 2, "the other republic is asked itself");
    assert_eq!(
        image_block(&ui).file_name.as_str(),
        "",
        "the foreign answer is gone - its share id belongs to the old seat"
    );
}


/// **A PDF written as `![…]` is a card, not a permanent failure.** Only a
/// raster picture is ever read: decoding a PDF cannot work, so it would
/// land on the `failed` card with a Reload verb that can never succeed.
#[cfg(feature = "live-preview")]
#[test]
fn a_non_picture_written_as_an_image_reads_as_a_card_with_the_jump() {
    let ui = page(&format!("Intro\n\n![Netz](upload:{HEX})\n"));
    for (name, kind) in [("bericht.pdf", "PDF"), ("plan.svg", "Image")] {
        let want = file_resolved(
            &ui,
            HEX,
            Some(resolved(
                Some(molt_core::UploadView {
                    kind: kind.to_string(),
                    ..upload(name)
                }),
                molt_core::LocalCopy::Mirrored,
            )),
        );
        assert_eq!(want, None, "{name}: its bytes are never read");
        let b = image_block(&ui);
        assert_eq!(b.file_state, file_state::READY, "{name}");
        assert_eq!(b.file_name.as_str(), name);
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                &ui,
                "Show in Shared Files"
            )
            .next()
            .is_some(),
            "{name}: the card offers the jump"
        );
    }
}
