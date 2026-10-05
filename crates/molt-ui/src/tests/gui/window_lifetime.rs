// SPDX-License-Identifier: GPL-3.0-or-later
//! A shown headless window must not outlive its test.
//!
//! The GUI suite in one process grew by every shown window until it was
//! OOM-killed. A shown Slint window keeps its component alive
//! until `hide`, so `show_headless`'s guard hides it on drop. This pins
//! that the window's item tree is really gone afterwards - the root
//! element handle is a weak reference into it.

use super::*;
use i_slint_backend_testing::ElementRoot;

#[test]
fn a_shown_window_is_freed_once_its_guard_and_handle_drop() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let shown = show_headless(&ui);
    let root = ui.root_element();
    assert!(root.is_valid(), "the window is alive while the test holds it");
    drop(shown);
    drop(ui);
    assert!(!root.is_valid(), "the window is freed, not kept by its own visibility");
}

/// The guard only helps if every test goes through it: a bare `.show()`
/// in a GUI test brings the per-window growth straight back.
#[test]
fn no_gui_test_shows_a_window_past_the_guard() {
    // split, so this file's own source does not match
    let needle = concat!(".sh", "ow()");
    let guard_call = concat!("ui", ".sh", "ow().expect(\"show headless\");");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/gui");
    let mut bare = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read the gui test dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read a gui test file");
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for (i, line) in src.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !code.contains(needle) {
                continue;
            }
            // the guard's own call
            if name == "mod.rs" && code == guard_call {
                continue;
            }
            bare.push(format!("{name}:{}", i + 1));
        }
    }
    assert!(bare.is_empty(), "show via show_headless: {bare:?}");
}

#[test]
fn a_window_dropped_before_its_guard_is_freed_by_the_guard() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let shown = show_headless(&ui);
    let root = ui.root_element();
    drop(ui);
    assert!(root.is_valid(), "the guard alone keeps the shown window");
    drop(shown);
    assert!(!root.is_valid(), "and dropping the guard frees it");
}
