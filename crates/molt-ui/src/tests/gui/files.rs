// SPDX-License-Identifier: GPL-3.0-or-later
//! The Shared Files surface (a design mock), rendered headless.

use super::*;

/// Shared Files on `view`.
#[cfg(feature = "live-preview")]
fn files_window(view: &str) -> AppWindow {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("files".into());
    ui.set_selected_view(view.into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "files".into(),
        gated: true,
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

#[cfg(feature = "live-preview")]
fn has(ui: &AppWindow, id: &str) -> bool {
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, id)
        .next()
        .is_some()
}

/// **Browse and Upload always show the mock, badged** - the seats
/// replicate shared files among themselves, so no S3 bucket gates them
/// (user decision 2026-08-28).
#[cfg(feature = "live-preview")]
#[test]
fn browse_and_upload_always_render_the_badged_mock() {
    i_slint_backend_testing::init_no_event_loop();
    let browse = files_window("browse");
    assert!(has(&browse, "FilesPane::sf-table"));
    let upload = files_window("upload");
    assert!(has(&upload, "FilesPane::sf-queue"));
    for ui in [&browse, &upload] {
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "MockBadge")
                .next()
                .is_some(),
            "a mock is badged"
        );
    }
}

/// **Browse: every row carries its replication meter**, and the meter
/// spans the seat count - a file the uploader alone holds sits at 1.
#[cfg(feature = "live-preview")]
#[test]
fn every_browse_row_shows_its_replication() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window("browse");
    let rows = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "FilesPane::sf-row")
        .count();
    let meters =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ReplMeter").count();
    assert!(rows >= 4, "the mock lists a few files, got {rows}");
    assert_eq!(meters, rows, "one meter per row");
    // the count is a float with two fixed decimals over the seat count:
    // the uploader alone reads 1.00, a chunk-averaged copy 2.75
    for label in ["1.00 / 4", "2.75 / 4", "4.00 / 4"] {
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some(),
            "a meter reads `{label}`"
        );
    }
}

/// **Config renders, and its Settings button is the door** to Settings ›
/// S3 config (tab 3), with the way back remembered.
#[cfg(feature = "live-preview")]
#[test]
fn config_opens_the_s3_settings_tab() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window("config");
    assert!(has(&ui, "FilesPane::sf-config"));
    let went: Rc<RefCell<Option<AppScreen>>> = Rc::new(RefCell::new(None));
    ui.on_navigate({
        let went = went.clone();
        move |screen| *went.borrow_mut() = Some(screen)
    });
    let btn = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "FilesPane::sf-settings")
        .next()
        .expect("the settings button");
    let at = slint::LogicalPosition::new(
        btn.absolute_position().x + btn.size().width / 2.0,
        btn.absolute_position().y + btn.size().height / 2.0,
    );
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    assert_eq!(*went.borrow(), Some(AppScreen::Settings));
    assert_eq!(ui.get_set_tab(), 3, "lands on S3 config");
}
