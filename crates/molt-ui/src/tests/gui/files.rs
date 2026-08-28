// SPDX-License-Identifier: GPL-3.0-or-later
//! The Shared Files surface (a design mock), rendered headless.

use super::*;

/// Shared Files with the given store readiness, on `view`.
#[cfg(feature = "live-preview")]
fn files_window(ready: bool, view: &str) -> AppWindow {
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
    ui.set_sf_ready(ready);
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

/// **Browse and Upload say "not configured" until the store is on, and
/// show the mock once it is.** The gate is one Rust-computed flag
/// (`files_ready`), so a half-configured bucket cannot open the pane.
#[cfg(feature = "live-preview")]
#[test]
fn browse_and_upload_gate_on_the_store() {
    i_slint_backend_testing::init_no_event_loop();
    for view in ["browse", "upload"] {
        let off = files_window(false, view);
        assert!(has(&off, "FilesPane::sf-unconfigured"), "{view}: the honest line");
        assert!(!has(&off, "FilesPane::sf-table"), "{view}: no mock table while off");
        assert!(!has(&off, "FilesPane::sf-queue"), "{view}: no mock queue while off");
        let on = files_window(true, view);
        assert!(!has(&on, "FilesPane::sf-unconfigured"), "{view}: configured");
        assert!(
            has(&on, "FilesPane::sf-table") || has(&on, "FilesPane::sf-queue"),
            "{view}: the mock renders"
        );
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&on, "MockBadge")
                .next()
                .is_some(),
            "{view}: a mock is badged"
        );
    }
}

/// **Browse: every row carries its replication meter**, and the meter
/// spans the seat count - a file the uploader alone holds sits at 1.
#[cfg(feature = "live-preview")]
#[test]
fn every_browse_row_shows_its_replication() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window(true, "browse");
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

/// **Config always renders, and its Settings button is the door** to
/// Settings › S3 config (tab 3), with the way back remembered.
#[cfg(feature = "live-preview")]
#[test]
fn config_opens_the_s3_settings_tab() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = files_window(false, "config");
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
