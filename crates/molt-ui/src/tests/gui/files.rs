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
    assert_eq!(views, ["uploads"]);
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
