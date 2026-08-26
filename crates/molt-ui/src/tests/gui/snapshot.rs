// SPDX-License-Identifier: GPL-3.0-or-later
//! gui_over_mcp.md: the published snapshot claims what the window holds.

use super::*;

/// `gui_over_mcp.md` step 1's pin: the published snapshot claims what
/// the WINDOW's models hold — screen, selection, the chat surface's
/// row count and last bodies, the nav keys and the pending sum. The
/// snapshot is the read half agents test the window through, so a
/// drift here would make every such test lie.
#[test]
fn the_ui_snapshot_claims_what_the_window_holds() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.set_selected_view("today".into());
    ui.set_selected_channel("group".into());
    let log = ModelRc::new(VecModel::from(vec![
        LogLine { text: "erste".into(), ..LogLine::default() },
        LogLine { text: "zweite".into(), ..LogLine::default() },
        LogLine { text: "dritte".into(), ..LogLine::default() },
        LogLine { text: "vierte".into(), ..LogLine::default() },
    ]));
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![
        SurfaceTab {
            key: "chat".into(),
            log,
            pending_count: 0,
            ..SurfaceTab::default()
        },
        SurfaceTab { key: "organization".into(), pending_count: 2, ..SurfaceTab::default() },
    ])));
    let snap = build_ui_snapshot(&ui);
    assert_eq!(
        (snap.screen.as_str(), snap.surface.as_str(), snap.view.as_str(), snap.channel.as_str()),
        ("main", "chat", "today", "group")
    );
    assert_eq!(snap.chat_rows, 4, "the model's row count, not the engine's");
    assert_eq!(
        snap.chat_last,
        vec!["zweite".to_string(), "dritte".to_string(), "vierte".to_string()],
        "the last three rendered bodies"
    );
    assert_eq!(snap.nav, vec!["chat".to_string(), "organization".to_string()]);
    assert_eq!(snap.pending_count, 2);
    assert!(snap.compose_visible);
    let again = build_ui_snapshot(&ui);
    assert!(again.generation > snap.generation, "every publish bumps");
}
