// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared Files: a nav row only while something is shared (or the surface
//! is the one on screen), the uploads view under it, none under
//! Organization. Both tests run on the demo context `node_with_chat`
//! boots (member "me", no workspace on disk) - sharing needs no more.

use super::*;

fn has_view(ui: &AppWindow, surface: &str, view: &str) -> bool {
    surface_tab(ui, surface).is_some_and(|s| s.views.iter().any(|v| v.key == view))
}

/// Render the engine's selection the way the app does before a surfaces
/// apply: the session apply rides the event loop the headless backend
/// never drains.
async fn apply_selection(w: &WalletHandle, ui: &AppWindow, chat_ui: &Arc<Mutex<ChatUiState>>) {
    if let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await {
        apply_session(ui, &sv, false, chat_ui);
    }
}

/// Nothing shared: no Shared Files row; the first share raises it with
/// its one view, and Organization no longer lists the uploads.
#[test]
fn the_shared_files_row_appears_with_the_first_share() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    let shared = tmp.path().join("protokoll.txt");
    std::fs::write(&shared, b"minutes").expect("write the file to share");

    rt.block_on(async {
        mirror(&w, &ui, &last, &chat_ui).await;
        assert!(ui.get_surfaces().row_count() > 0, "the bundle must have landed");
        assert!(surface_tab(&ui, "files").is_none(), "nothing shared: no Shared Files row");
        assert!(
            !has_view(&ui, "organization", "uploads"),
            "the uploads view left Organization"
        );

        w.execute(Command::ShareFile {
            path: shared.display().to_string(),
            channel: ChannelRef::Group,
        })
        .await
        .expect("the share is accepted");
        // hashing runs off the actor: wait for the share to list
        let mut listed = false;
        for _ in 0..200 {
            if let Ok(Reply::Uploads { uploads }) = w.execute(Command::ReadUploads).await {
                if !uploads.is_empty() {
                    listed = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(listed, "the share never listed");
        mirror(&w, &ui, &last, &chat_ui).await;
    });

    let tab = surface_tab(&ui, "files").expect("one share: the Shared Files row");
    assert_eq!(tab.name.as_str(), "Shared Files");
    let views: Vec<String> = tab.views.iter().map(|v| v.key.to_string()).collect();
    assert_eq!(views, ["uploads", "persistent", "pending", "accepted", "declined"]);
    assert_eq!(
        tab.views.row_data(0).map(|v| v.name.to_string()),
        Some("Temporary Uploads".to_string())
    );
}

/// The row stays while Shared Files is the selected surface with nothing
/// shared (an agent's select_view, or the last share aged out under the
/// reader) - the pane must stay reachable - and leaves with the selection.
#[test]
fn the_shared_files_row_stays_while_selected_and_leaves_with_the_selection() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    rt.block_on(async {
        w.execute(Command::SelectView {
            surface: Surface::Files,
            view: "uploads".to_string(),
        })
        .await
        .expect("the engine accepts the view like any hidden-while-empty one");
        apply_selection(&w, &ui, &chat_ui).await;
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_selected_surface().as_str(), "files");
        assert!(
            surface_tab(&ui, "files").is_some(),
            "selected: the row stays, empty table and all"
        );

        w.execute(Command::SelectSurface {
            surface: Surface::Organization,
        })
        .await
        .expect("navigating away");
        apply_selection(&w, &ui, &chat_ui).await;
        mirror(&w, &ui, &last, &chat_ui).await;
    });
    assert!(
        surface_tab(&ui, "files").is_none(),
        "nothing shared and not selected: no row"
    );
}

/// A 1-of-3 republic-in-a-box whose own vote decides (no self-cosign, so
/// a proposal is visibly open first).
fn one_of_three() -> WalletHandle {
    molt_engine::spawn(
        GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        },
        SessionView::default(),
    )
}

/// Poll until the share lists, returning its id.
async fn listed_share(w: &WalletHandle) -> String {
    for _ in 0..200 {
        if let Ok(Reply::Uploads { uploads }) = w.execute(Command::ReadUploads).await {
            if let Some(u) = uploads.first() {
                return u.id.to_string();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the share never listed");
}

async fn propose_files(w: &WalletHandle, payload: serde_json::Value) -> ProposalId {
    match w
        .execute(Command::Propose {
            surface: Surface::Files,
            payload,
        })
        .await
        .expect("the files proposal is accepted")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The two tables follow the vote: an open persist marks its row, the
/// applied vote moves it to Persistent Uploads, an unpersist moves it back.
#[test]
fn a_persist_vote_moves_the_row_and_an_unpersist_vote_moves_it_back() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = one_of_three();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    let shared = tmp.path().join("protokoll.pdf");
    std::fs::write(&shared, b"minutes").expect("write the file to share");

    rt.block_on(async {
        w.execute(Command::ShareFile {
            path: shared.display().to_string(),
            channel: ChannelRef::Group,
        })
        .await
        .expect("share");
        let id = listed_share(&w).await;
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_uploads().row_count(), 1);
        assert_eq!(ui.get_org_persistent().row_count(), 0);

        let pid = propose_files(&w, serde_json::json!({"op": "persist", "id": id})).await;
        mirror(&w, &ui, &last, &chat_ui).await;
        let row = ui.get_org_uploads().row_data(0).expect("still temporary");
        assert_eq!(row.vote.as_str(), "0/1", "the open vote marks the row");
        let files = surface_tab(&ui, "files").expect("the files tab");
        assert_eq!(files.pending_count, 1, "…and the nav counts it");

        w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_uploads().row_count(), 0, "left Temporary");
        let row = ui.get_org_persistent().row_data(0).expect("now persistent");
        assert!(row.persistent);
        assert_eq!(row.vote.as_str(), "");
        assert_eq!(row.name.as_str(), "protokoll.pdf");
        assert_eq!(row.checksum_full.len(), 64, "the full sha256 rides the row");

        let pid = propose_files(
            &w,
            serde_json::json!({"op": "unpersist", "id": id, "at": crate::labels::unix_now()}),
        )
        .await;
        w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_persistent().row_count(), 0);
        assert_eq!(ui.get_org_uploads().row_count(), 1, "back in Temporary");
        assert!(!ui.get_org_uploads().row_data(0).expect("row").persistent);
    });
}

/// Poll until `n` shares list, returning their ids in the engine's order.
async fn listed_shares(w: &WalletHandle, n: usize) -> Vec<String> {
    for _ in 0..400 {
        if let Ok(Reply::Uploads { uploads }) = w.execute(Command::ReadUploads).await {
            if uploads.len() >= n {
                return uploads.iter().map(|u| u.id.to_string()).collect();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("only some of the {n} shares listed");
}

/// **Both Shared Files tables page at 15 rows, and page apart.** 16
/// temporary and 16 persistent shares fill one page each with a second
/// behind it; a step moves only the table it addressed.
#[test]
fn both_upload_tables_page_at_fifteen_rows_apart() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = one_of_three();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    rt.block_on(async {
        for i in 0..32 {
            let f = tmp.path().join(format!("share-{i:02}.pdf"));
            std::fs::write(&f, format!("minutes {i}").as_bytes()).expect("write the share");
            w.execute(Command::ShareFile {
                path: f.display().to_string(),
                channel: ChannelRef::Group,
            })
            .await
            .expect("share");
        }
        let ids = listed_shares(&w, 32).await;
        // half of them get pinned by a vote (1-of-3, this seat decides)
        for id in ids.iter().take(16) {
            let pid = propose_files(&w, serde_json::json!({"op": "persist", "id": id})).await;
            w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
        }
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_uploads().row_count(), 15, "one page of Temporary");
        assert_eq!(ui.get_org_persistent().row_count(), 15, "…and one of Persistent");
        assert_eq!((ui.get_ou_page(), ui.get_ou_pages()), (1, 2));
        assert_eq!((ui.get_op_page(), ui.get_op_pages()), (1, 2));

        chat_ui.lock().expect("state").page_list_by("files", "uploads", 1);
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_uploads().row_count(), 1, "the 16th share is page 2");
        assert_eq!((ui.get_ou_page(), ui.get_ou_pages()), (2, 2));
        assert_eq!(ui.get_org_persistent().row_count(), 15, "the other table never moved");
        assert_eq!(ui.get_op_page(), 1);
    });
}

/// A Shared Files window with rows pushed straight into the models.
#[cfg(feature = "live-preview")]
fn files_window(rows: Vec<UploadRow>) -> AppWindow {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("files".into());
    ui.set_selected_view("uploads".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "files".into(),
        ..SurfaceTab::default()
    }])));
    ui.set_org_uploads(ModelRc::new(VecModel::from(rows)));
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

/// The pager rows currently on screen.
#[cfg(feature = "live-preview")]
fn pager_rows(ui: &AppWindow) -> Vec<i_slint_backend_testing::ElementHandle> {
    i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "PagerRow").collect()
}

/// The pager's › button: the rightmost of the two inside `row` (the row is
/// centred in the pane, so its own right edge is empty space).
#[cfg(feature = "live-preview")]
fn pager_next(
    ui: &AppWindow,
    row: &i_slint_backend_testing::ElementHandle,
) -> i_slint_backend_testing::ElementHandle {
    let left = row.absolute_position().x;
    let right = left + row.size().width;
    let top = row.absolute_position().y;
    let bottom = top + row.size().height;
    let mut inside: Vec<i_slint_backend_testing::ElementHandle> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "AppButton")
            .filter(|b| {
                let p = b.absolute_position();
                p.x >= left
                    && p.x + b.size().width <= right
                    && p.y >= top
                    && p.y + b.size().height <= bottom
            })
            .collect();
    inside.sort_by(|a, b| {
        a.absolute_position()
            .x
            .partial_cmp(&b.absolute_position().x)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    inside.pop().expect("the pager's two step buttons render")
}

/// **Each uploads table carries its OWN pager.** One copied into the
/// neighbouring view would step the wrong table, and a single page shows
/// none at all.
#[cfg(feature = "live-preview")]
#[test]
fn each_uploads_table_pages_its_own_list() {
    i_slint_backend_testing::init_no_event_loop();
    let row = |name: &str, persistent: bool| UploadRow {
        id: "cc".repeat(16).into(),
        name: name.into(),
        persistent,
        available: true,
        ..UploadRow::default()
    };
    let ui = files_window(vec![row("protokoll.pdf", false)]);
    ui.set_org_persistent(ModelRc::new(VecModel::from(vec![row("charta.pdf", true)])));
    let stepped: Rc<RefCell<Vec<(String, String, i32)>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = stepped.clone();
    ui.on_page_list(move |surface, list, delta| {
        sink.borrow_mut().push((surface.to_string(), list.to_string(), delta));
    });

    ui.set_ou_pages(2);
    let rows = pager_rows(&ui);
    assert_eq!(rows.len(), 1, "Temporary over two pages: its pager, and only its");
    click(&ui, &pager_next(&ui, &rows[0]));
    assert_eq!(
        stepped.borrow().as_slice(),
        [("files".to_string(), "uploads".to_string(), 1)]
    );
    ui.set_ou_pages(1);
    assert!(pager_rows(&ui).is_empty(), "a single page shows no pager");

    ui.set_selected_view("persistent".into());
    ui.set_op_pages(2);
    stepped.borrow_mut().clear();
    let rows = pager_rows(&ui);
    assert_eq!(rows.len(), 1, "Persistent over two pages: its own pager");
    click(&ui, &pager_next(&ui, &rows[0]));
    assert_eq!(
        stepped.borrow().as_slice(),
        [("files".to_string(), "persistent".to_string(), 1)]
    );
    ui.set_op_pages(1);
    assert!(pager_rows(&ui).is_empty(), "a single page shows no pager");
}

/// The checksum cell is an info button; a real click opens the modal on
/// the FULL hash (the cell used to elide it).
#[cfg(feature = "live-preview")]
#[test]
fn the_info_button_opens_the_checksum_modal_with_the_full_hash() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window(vec![UploadRow {
        id: "cc".repeat(16).into(),
        name: "protokoll.pdf".into(),
        user: "walter".into(),
        kind: "PDF".into(),
        checksum: "ab12…".into(),
        checksum_full: "ab".repeat(32).into(),
        available: true,
        online: true,
        ..UploadRow::default()
    }]);
    let button = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "UploadsTable::ou-info")
        .next()
        .expect("the info button");
    click(&ui, &button);
    assert!(ui.get_checksum_modal_open(), "the click opens the modal");
    assert_eq!(ui.get_checksum_modal_hash().as_str(), "ab".repeat(32));
    assert_eq!(ui.get_checksum_modal_name().as_str(), "protokoll.pdf");
}

/// The Type column: header and cells share one box at every font size
/// (both scale, both centre) — the members-table rule.
#[cfg(feature = "live-preview")]
#[test]
fn the_type_column_header_lines_up_with_its_cells() {
    i_slint_backend_testing::init_no_event_loop();
    let row = |kind: &str| UploadRow {
        id: "cc".repeat(16).into(),
        name: "a-rather-long-file-name-to-push-the-columns.pdf".into(),
        user: "bartholomaeus-von-habsburg".into(),
        kind: kind.into(),
        available: true,
        ..UploadRow::default()
    };
    let ui = files_window(vec![row("PDF"), row(""), row("SPREADSHEET")]);
    let boxes = |id: &str| -> Vec<(f32, f32)> {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .filter(|e| e.size().width > 0.0)
            .map(|e| (e.absolute_position().x, e.size().width))
            .collect()
    };
    for font in [14.0_f32, 20.0, 28.0] {
        ui.global::<Theme>().set_fs_app(font);
        let header = boxes("UploadsTable::ou-h-type");
        let cells = boxes("UploadsTable::ou-r-type");
        assert_eq!(header.len(), 1, "font {font}: one Type header");
        assert_eq!(cells.len(), 3, "font {font}: three Type cells");
        for (i, (x, w)) in cells.iter().enumerate() {
            assert!(
                (x - header[0].0).abs() < 1.0 && (w - header[0].1).abs() < 1.0,
                "font {font}: Type cell {i} is at {x}/{w}, its header at {}/{}",
                header[0].0,
                header[0].1
            );
        }
    }
}

/// The mirror row above the Persistent table: switch, quota field, both
/// buttons and the two texts sit on ONE centre line at every font size.
/// A fixed-height child of a HorizontalLayout hangs at the top edge
/// unless it centres itself (reported 2026-09-04: "some centred, some
/// stuck to the top").
#[cfg(feature = "live-preview")]
#[test]
fn the_mirror_row_controls_share_one_centre_line() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window(vec![UploadRow {
        id: "cc".repeat(16).into(),
        name: "plan.pdf".into(),
        user: "walter".into(),
        available: true,
        persistent: true,
        ..UploadRow::default()
    }]);
    ui.set_selected_view("persistent".into());
    ui.set_org_mirror_used("0 B of 1.07 GB".into());
    ui.set_org_mirror_dir("/home/walter/mirror".into());
    let centre = |id: &str| -> f32 {
        let e = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .find(|e| e.size().height > 0.0)
            .unwrap_or_else(|| panic!("{id} is rendered"));
        e.absolute_position().y + e.size().height / 2.0
    };
    for font in [14.0_f32, 20.0, 28.0] {
        ui.global::<Theme>().set_fs_app(font);
        let row = centre("AppWindow::ou-mirror-row");
        for id in [
            "AppWindow::ou-mirror-check",
            "AppWindow::ou-mirror-field",
            "AppWindow::ou-mirror-apply",
            "AppWindow::ou-mirror-used",
            "AppWindow::ou-mirror-dir",
            "AppWindow::ou-mirror-browse",
        ] {
            let c = centre(id);
            assert!(
                (c - row).abs() < 1.0,
                "font {font}: {id} centres at {c}, the row at {row}"
            );
        }
    }
}

/// Mirroring §3.6 on the Persistent table: the own share counts as one
/// whole holder, and the switch, the quota field and the usage line
/// mirror the engine's `read_mirror` (the switch's own `set_mirror` is the
/// same command the test issues).
#[test]
fn the_persistent_table_carries_the_mirror_switch_quota_and_holder_count() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = one_of_three();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    let shared = tmp.path().join("plan.pdf");
    std::fs::write(&shared, b"a plan").expect("write the file to share");

    rt.block_on(async {
        w.execute(Command::ShareFile {
            path: shared.display().to_string(),
            channel: ChannelRef::Group,
        })
        .await
        .expect("share");
        let id = listed_share(&w).await;
        let pid = propose_files(&w, serde_json::json!({"op": "persist", "id": id})).await;
        w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
        mirror(&w, &ui, &last, &chat_ui).await;
        let row = ui.get_org_persistent().row_data(0).expect("the persistent row");
        assert_eq!((row.mirrors, row.mirror_held, row.mirror_of), (1, 1, 1), "the sharer holds it whole");
        assert!(ui.get_org_mirror_on(), "consent is on by default");
        assert_eq!(ui.get_org_mirror_quota().as_str(), "1.07", "1 GiB as a GB field");

        let quota_bytes = crate::labels::gb_bytes("2.5").expect("the field parses");
        w.execute(Command::SetMirror { on: false, quota_bytes })
            .await
            .expect("set mirror");
        mirror(&w, &ui, &last, &chat_ui).await;
        assert!(!ui.get_org_mirror_on(), "the switch follows the engine");
        assert_eq!(ui.get_org_mirror_quota().as_str(), "2.5");
        assert!(ui.get_org_mirror_used().as_str().contains(" of "), "{}", ui.get_org_mirror_used());
    });
}

/// **The uploads filter's text sits on the box's centre line, at every
/// font size.** It used to be pinned at `y: 8px` while the box grew with
/// `Theme.ui-scale` and the body font — so the larger the setting, the
/// higher the text rode inside its own field.
#[cfg(feature = "live-preview")]
#[test]
fn the_uploads_filter_text_is_vertically_centred() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window(vec![UploadRow {
        id: "dd".repeat(16).into(),
        name: "protokoll.pdf".into(),
        ..UploadRow::default()
    }]);
    let centre = |id: &str| -> Option<f32> {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .find(|e| e.size().height > 0.0)
            .map(|e| e.absolute_position().y + e.size().height / 2.0)
    };
    // ui-scale is derived from the app font, so the font IS the sweep
    for font in [14.0_f32, 18.0, 22.0, 28.0] {
        ui.global::<Theme>().set_fs_app(font);
        let box_mid = centre("AppWindow::ou-filter-box").expect("the filter box renders");
        // empty needle: the placeholder is what the user sees
        let text_mid = centre("AppWindow::ou-filter-ph").expect("the placeholder renders");
        assert!(
            (box_mid - text_mid).abs() <= 1.5,
            "font {font}: placeholder centre {text_mid} vs box centre {box_mid}"
        );
        // …and the input the moment there is one. Its TEXT position is not
        // measurable headless, so what is pinned is the precondition that
        // makes `vertical-alignment: center` mean anything: the run SPANS
        // the box. A short box pinned by `y` centres itself and still
        // paints the typed text at the top edge - that was the second bug.
        ui.set_ou_filter("anna".into());
        let input = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::ou-filter-in",
        )
        .find(|e| e.size().height > 0.0)
        .expect("the input renders");
        let boxx = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::ou-filter-box",
        )
        .find(|e| e.size().height > 0.0)
        .expect("the filter box renders");
        assert!(
            (input.size().height - boxx.size().height).abs() <= 0.5,
            "font {font}: the input is {} tall inside a {} box",
            input.size().height,
            boxx.size().height
        );
        ui.set_ou_filter("".into());
    }
}

/// The delete vote (`delete_upload.md` D2): an open delete marks the row
/// on its own button, the applied vote takes the row off both tables.
#[test]
fn a_delete_vote_marks_the_row_and_the_applied_vote_removes_it() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = one_of_three();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    let shared = tmp.path().join("protokoll.pdf");
    std::fs::write(&shared, b"minutes").expect("write the file to share");

    rt.block_on(async {
        w.execute(Command::ShareFile {
            path: shared.display().to_string(),
            channel: ChannelRef::Group,
        })
        .await
        .expect("share");
        let id = listed_share(&w).await;
        let pid = propose_files(&w, serde_json::json!({"op": "delete", "id": id})).await;
        mirror(&w, &ui, &last, &chat_ui).await;
        let row = ui.get_org_uploads().row_data(0).expect("still temporary");
        assert_eq!(row.delete_vote.as_str(), "0/1", "the open delete marks its own button");
        assert_eq!(row.vote.as_str(), "", "…and not the persist button");

        w.execute(Command::Approve { proposal: pid, note: None }).await.expect("approve");
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_uploads().row_count(), 0, "gone from Temporary");
        assert_eq!(ui.get_org_persistent().row_count(), 0, "and from Persistent");
    });
}

/// The delete button sits right of the persist button, fires
/// `delete-upload` with the share id, and an open vote disables both.
#[cfg(feature = "live-preview")]
#[test]
fn the_delete_button_sits_right_of_persist_and_fires_its_callback() {
    i_slint_backend_testing::init_no_event_loop();
    let hex = "cc".repeat(16);
    let row = |delete_vote: &str| UploadRow {
        id: hex.as_str().into(),
        name: "protokoll.pdf".into(),
        user: "walter".into(),
        available: true,
        online: true,
        delete_vote: delete_vote.into(),
        ..UploadRow::default()
    };
    let ui = files_window(vec![row("")]);
    // the temporary table is wider than the default window with this
    // column; a clipped element is not in the tested tree at all
    ui.window().set_size(slint::PhysicalSize::new(1700, 800));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let persisted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = deleted.clone();
    ui.on_delete_upload(move |id| {
        if let Ok(mut v) = sink.lock() {
            v.push(id.to_string());
        }
    });
    let sink = persisted.clone();
    ui.on_persist_upload(move |id| {
        if let Ok(mut v) = sink.lock() {
            v.push(id.to_string());
        }
    });
    let at = |id: &str| {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .next()
            .unwrap_or_else(|| panic!("no element {id}"))
    };
    let persist = at("UploadsTable::ou-vote");
    let delete = at("UploadsTable::ou-delete");
    assert!(
        delete.absolute_position().x > persist.absolute_position().x,
        "the delete button sits right of the persist button"
    );
    click(&ui, &delete);
    assert_eq!(
        deleted.lock().expect("deleted").as_slice(),
        std::slice::from_ref(&hex),
        "the click carries the share id"
    );

    ui.set_org_uploads(ModelRc::new(VecModel::from(vec![row("1/3")])));
    click(&ui, &at("UploadsTable::ou-delete"));
    click(&ui, &at("UploadsTable::ou-vote"));
    assert_eq!(deleted.lock().expect("deleted").len(), 1, "an open vote disables delete");
    assert!(persisted.lock().expect("persisted").is_empty(), "…and persist");
}

/// **The download button hands the row's NAME over with its id** - the
/// save dialog is pre-filled from it (`actions::chat`), and it opened
/// empty because the callback carried the id alone. Both tables are the
/// same component, wired twice.
#[cfg(feature = "live-preview")]
#[test]
fn the_download_button_carries_the_rows_name_in_both_tables() {
    i_slint_backend_testing::init_no_event_loop();
    let hex = "cc".repeat(16);
    let row = UploadRow {
        id: hex.as_str().into(),
        name: "protokoll.pdf".into(),
        user: "walter".into(),
        available: true,
        online: true,
        ..UploadRow::default()
    };
    let ui = files_window(vec![row.clone()]);
    // the download column is off a 1200px window (a clipped element is
    // not in the tested tree at all)
    ui.window().set_size(slint::PhysicalSize::new(1700, 800));
    let asked: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = asked.clone();
    ui.on_download_file(move |id, name| {
        if let Ok(mut v) = sink.lock() {
            v.push((id.to_string(), name.to_string()));
        }
    });
    let button = || {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "UploadsTable::ou-dl")
            .next()
            .expect("the download button")
    };
    click(&ui, &button());

    ui.set_org_persistent(ModelRc::new(VecModel::from(vec![UploadRow {
        persistent: true,
        ..row
    }])));
    ui.set_selected_view("persistent".into());
    click(&ui, &button());

    let want = (hex, "protokoll.pdf".to_string());
    assert_eq!(
        *asked.lock().expect("asked"),
        vec![want.clone(), want],
        "both tables pass the row's name beside its id"
    );
}
