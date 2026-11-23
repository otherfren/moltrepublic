// SPDX-License-Identifier: GPL-3.0-or-later
//! Organization -> Status, the seat table: each seat's build, reported
//! head and the faults an operator must see without reading logs; and a
//! busy relay in the pool.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// `(x, y, width, height)`.
type Rect = (f32, f32, f32, f32);

fn rect(e: &Handle) -> Rect {
    (e.absolute_position().x, e.absolute_position().y, e.size().width, e.size().height)
}

fn by_id(ui: &AppWindow, id: &str) -> Option<Rect> {
    Handle::find_by_element_id(ui, id).map(|e| rect(&e)).find(|r| r.2 > 0.0)
}

fn shown_text(ui: &AppWindow, label: &str) -> Option<Rect> {
    Handle::find_by_accessible_label(ui, label).map(|e| rect(&e)).find(|r| r.2 > 0.0)
}

/// Organization -> Status at the minimum window width (tall enough that
/// the table is on screen at both scales).
fn status_window() -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(990, 1400));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("status".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        name: "Organization".into(),
        ..SurfaceTab::default()
    }])));
    ui.set_active_workspace("Republic".into());
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    rail_settled();
    (ui, shown)
}

fn seat(member: &str, build: &str) -> SeatRow {
    SeatRow {
        member: member.into(),
        seen: "just now".into(),
        height: "5".into(),
        signed: "5".into(),
        files: "0 / 2".into(),
        build: build.into(),
        ..SeatRow::default()
    }
}

fn hover(ui: &AppWindow, r: Rect) {
    let at = slint::LogicalPosition::new(r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
}

#[test]
fn the_build_column_shows_the_version_or_a_question_mark_with_its_reason() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![seat("ana", "0.1.2"), seat("bo", "?")])));
    assert!(shown_text(&ui, "0.1.2").is_some(), "the known build");
    let unknown = shown_text(&ui, "?").expect("the unknown build");
    hover(&ui, unknown);
    assert_eq!(ui.global::<HintTip>().get_label(), "Older than 0.1.2 or not heard yet");
}

#[test]
fn differing_builds_raise_one_line_above_the_table() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![seat("ana", "0.1.2"), seat("bo", "0.1.3")])));
    ui.set_org_builds_mixed(true);
    let line = Handle::find_by_accessible_label(&ui, "Seats run different versions")
        .filter(|e| e.size().width > 0.0)
        .map(|e| rect(&e))
        .collect::<Vec<_>>();
    assert_eq!(line.len(), 1, "one line");
    let row = by_id(&ui, "AppWindow::os-seat").expect("a row");
    assert!(line[0].1 < row.1, "above the table");
    ui.set_org_builds_mixed(false);
    assert!(shown_text(&ui, "Seats run different versions").is_none(), "gone when equal");
}

#[test]
fn a_seat_on_another_branch_or_dropped_says_so_under_its_row() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    let fork = SeatRow { hash: "cccccc".into(), fork: true, note: "other branch at 5".into(), ..seat("bo", "0.1.2") };
    let dup = SeatRow {
        note: "its messages are dropped here as already seen".into(),
        note_tip: "This node missed its restore; restart this node on 0.1.2 or newer.".into(),
        ..seat("ed", "?")
    };
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![seat("ana", "0.1.2"), fork, dup])));
    let hash = shown_text(&ui, "cccccc").expect("the reported head's hash");
    let note = shown_text(&ui, "other branch at 5").expect("the branch note");
    assert!(note.1 > hash.1, "under the row");
    let dropped = shown_text(&ui, "its messages are dropped here as already seen").expect("the dup note");
    hover(&ui, dropped);
    assert!(ui.global::<HintTip>().get_label().contains("restart this node"));
    let rows = Handle::find_by_element_id(&ui, "AppWindow::os-seat").filter(|e| e.size().width > 0.0).count();
    assert_eq!(rows, 3, "one row per seat");
}

/// Every header cell sits over its column, and the widened table fits the
/// minimum window at both scales.
#[test]
fn the_widened_seat_table_lines_up_and_fits_at_both_scales() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![
        SeatRow { hash: "a1b2c3".into(), height: "1234".into(), ..seat("a-rather-long-member-name", "0.1.2") },
        seat("bo", "?"),
    ])));
    for (font, scale) in [(14.0_f32, 1.0_f32), (21.0, 1.5)] {
        ui.global::<Theme>().set_fs_app(font);
        rail_settled();
        for col in ["member", "seen", "height", "signed", "build", "files"] {
            let h = by_id(&ui, &format!("AppWindow::os-h-{col}")).unwrap_or_else(|| panic!("{col} header"));
            let c = by_id(&ui, &format!("AppWindow::os-c-{col}")).unwrap_or_else(|| panic!("{col} cell"));
            assert!((h.0 - c.0).abs() < 0.5 && (h.2 - c.2).abs() < 0.5, "scale {scale}: {col} {h:?} over {c:?}");
        }
        // the pane's content column: the banner spans it
        let pane = Handle::find_by_element_type_name(&ui, "OrgBanner")
            .map(|e| rect(&e))
            .find(|r| r.2 > 0.0)
            .expect("the banner");
        let group = by_id(&ui, "AppWindow::os-seats").expect("the seats group");
        assert!(group.0 + group.2 <= pane.0 + pane.2 + 0.5, "scale {scale}: group {group:?} past the pane {pane:?}");
    }
}

#[test]
fn a_busy_relay_says_so_in_the_pool() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.set_screen(AppScreen::Settings);
    ui.set_set_tab(5);
    apply_strings(&ui, 0);
    let relay = |url: &str| RelayItem { url: url.into(), kind: 0, confirmed: true, pos: 1, ..RelayItem::default() };
    ui.set_relay_rows(ModelRc::new(VecModel::from(vec![relay("wss://a.onion"), relay("wss://b.onion")])));
    let _shown = show_headless(&ui);
    let before = ui.get_relay_rows();
    apply_relay_busy(
        &ui,
        &[molt_core::RelayRefusals { url: "wss://b.onion".into(), busy: 3, ..molt_core::RelayRefusals::default() }],
    );
    assert!(before == ui.get_relay_rows(), "patched in place");
    assert!(shown_text(&ui, "busy 3x in the last hour").is_some());
    assert!(shown_text(&ui, "connects automatically").is_some(), "the quiet relay looks as before");
    apply_relay_busy(&ui, &[]);
    assert!(shown_text(&ui, "busy 3x in the last hour").is_none(), "a quiet hour clears it");
}
