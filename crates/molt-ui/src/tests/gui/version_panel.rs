// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings footer names the app version, in its own panel left of
//! the config path on the same row.

use super::*;

#[cfg(feature = "live-preview")]
type Handle = i_slint_backend_testing::ElementHandle;

#[cfg(feature = "live-preview")]
fn text_labelled(ui: &AppWindow, wanted: &str) -> Option<Handle> {
    Handle::find_by_element_type_name(ui, "Text")
        .find(|t| t.accessible_label().is_some_and(|l| l == wanted))
}

#[cfg(feature = "live-preview")]
#[test]
fn the_version_panel_sits_left_of_the_config_path_on_one_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    ui.set_config_dir("/home/x/".into());
    ui.set_config_file("config.toml".into());
    ui.set_app_version("0.0.8".into());
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.show().expect("show headless");

    let version = text_labelled(&ui, "v0.0.8").expect("the version renders");
    let path = text_labelled(&ui, "config.toml").expect("the config file name renders");
    let (v, p) = (version.absolute_position(), path.absolute_position());
    assert!((v.y - p.y).abs() < 8.0, "same row: version y {} vs path y {}", v.y, p.y);
    assert!(v.x + version.size().width <= p.x, "the version sits left of the path");
}
