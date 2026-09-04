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
        Reply::Proposed { id } => id,
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

        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
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
        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(ui.get_org_persistent().row_count(), 0);
        assert_eq!(ui.get_org_uploads().row_count(), 1, "back in Temporary");
        assert!(!ui.get_org_uploads().row_data(0).expect("row").persistent);
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
        w.execute(Command::Approve { proposal: pid }).await.expect("approve");
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
