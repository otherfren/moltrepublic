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
fn a_seat_on_another_branch_or_dropped_says_so_in_its_notes_cell() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seat_notes(true);
    let fork = SeatRow { hash: "cccccc".into(), fork: true, note: "other branch at 5".into(), ..seat("bo", "0.1.2") };
    let dup = SeatRow {
        note: "many repeats from this seat - fetch its history again if messages are missing".into(),
        note_tip: "Likely a restore this node missed".into(),
        ..seat("ed", "?")
    };
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![seat("ana", "0.1.2"), fork, dup])));
    let hash = shown_text(&ui, "cccccc").expect("the reported head's hash");
    let note = shown_text(&ui, "other branch at 5").expect("the branch note");
    assert!(note.0 > hash.0 + hash.2, "right of the row's cells, not under them");
    assert!((note.1 - hash.1).abs() < hash.3.max(note.3), "on the row's own line");
    let dropped = shown_text(&ui, "many repeats from this seat - fetch its history again if messages are missing").expect("the dup note");
    hover(&ui, dropped);
    assert!(ui.global::<HintTip>().get_label().contains("restore this node missed"));
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

const RESYNC: &str = "Fetch history again";

fn visible(ui: &AppWindow, label: &str) -> Vec<Rect> {
    Handle::find_by_accessible_label(ui, label).map(|e| rect(&e)).filter(|r| r.2 > 0.0).collect()
}

/// Every other seat offers the resync in its row's menu; the own seat
/// does not; a seat dropped as already seen has it beside its fault line.
#[test]
fn every_other_seat_offers_to_fetch_its_history_again() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    let dropped = SeatRow {
        dropped: true,
        note: "many repeats from this seat - fetch its history again if messages are missing".into(),
        ..seat("ed", "?")
    };
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![
        SeatRow { me: true, ..seat("ana", "0.1.2") },
        seat("bo", "0.1.2"),
        dropped,
    ])));
    ui.set_org_seat_notes(true);
    let inline = visible(&ui, RESYNC);
    assert_eq!(inline.len(), 1, "one button, on the dropped seat's fault line");
    let note = visible(&ui, "many repeats from this seat - fetch its history again if messages are missing")[0];
    // the button's label sits inside the button's own padding
    assert!(
        inline[0].0 >= note.0 && inline[0].0 < note.0 + note.2 && inline[0].1 > note.1,
        "in the hint's cell, under its text: {inline:?} {note:?}"
    );

    let menus: Vec<_> = Handle::find_by_element_id(&ui, "AppWindow::os-seat-menu").collect();
    assert_eq!(menus.len(), 3, "one menu area per row");
    right_click(&ui, &menus[0], 0.5);
    assert_eq!(visible(&ui, RESYNC).len(), 1, "the own seat offers nothing");
    right_click(&ui, &menus[1], 0.5);
    assert!(Handle::find_by_accessible_label(&ui, RESYNC).count() > 1, "bo's menu offers it");
}

/// Confirming sends `ResyncSeat` for that seat; the engine's "running"
/// answer to a second one is what the toast says.
#[test]
fn a_confirmed_resync_reaches_the_engine_and_a_second_says_running() {
    let o = super::discussion::Opened::new(|root| drop(workspace_on_disk(root, 2, &["walter", "mara"], "test")));
    o.ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    o.ui.set_screen(AppScreen::Main);
    apply_strings(&o.ui, 0);
    let cx = Ctx {
        rt: o.rt.handle().clone(),
        wallet: o.w.clone(),
        weak: o.ui.as_weak(),
        last_settings: o.last.clone(),
        chat_ui: o.chat_ui.clone(),
    };
    crate::actions::org::wire(&o.ui, &cx);
    let _shown = show_headless(&o.ui);
    o.ui.set_os_resync_member("mara".into());
    assert_eq!(
        visible(&o.ui, "Re-fetch mara's retained history? Their live messages wait until it is through.").len(),
        1,
        "the one-line confirm"
    );
    // the dialog's title says it too; the button sits below it
    let confirm = Handle::find_by_accessible_label(&o.ui, RESYNC)
        .filter(|e| e.size().width > 0.0)
        .max_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y))
        .expect("the confirm button");
    click(&o.ui, &confirm);
    assert_eq!(o.ui.get_os_resync_member(), "", "the confirm closes");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let second = loop {
        match o.rt.block_on(o.w.execute(Command::ResyncSeat { member: "mara".to_string(), give_up: false })) {
            Err(e) => break e,
            Ok(_) => {
                assert!(std::time::Instant::now() < deadline, "the first resync never landed");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    crate::actions::org::resync_outcome(&o.ui, Err(second));
    assert!(o.ui.get_toast_text().contains("resync of mara running"), "{}", o.ui.get_toast_text());
}

#[test]
fn a_seat_whose_history_is_fetched_says_so_instead_of_offering_it() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    let fetching = SeatRow {
        dropped: true,
        fetching: true,
        note: "many repeats from this seat - fetch its history again if messages are missing".into(),
        ..seat("ed", "?")
    };
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![SeatRow { me: true, ..seat("ana", "0.1.2") }, fetching])));
    ui.set_org_seat_notes(true);
    assert_eq!(visible(&ui, "fetching history").len(), 1);
    assert!(visible(&ui, RESYNC).is_empty(), "no second request while one runs");
}

const GIVE_UP: &str = "Stop waiting";

/// Stop waiting is offered only while a seat's history is fetched: beside
/// the status text and in that row's menu, which offers it instead of the
/// fetch.
#[test]
fn stop_waiting_is_offered_only_while_fetching() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![
        SeatRow { me: true, ..seat("ana", "0.1.2") },
        seat("bo", "0.1.2"),
        SeatRow { fetching: true, ..seat("ed", "0.1.2") },
    ])));
    ui.set_org_seat_notes(true);
    let beside = visible(&ui, GIVE_UP);
    assert_eq!(beside.len(), 1, "one button, beside the status text");
    let status = visible(&ui, "fetching history")[0];
    assert!((beside[0].1 + beside[0].3 / 2.0 - (status.1 + status.3 / 2.0)).abs() < status.3.max(beside[0].3), "beside the status text");

    let menus: Vec<_> = Handle::find_by_element_id(&ui, "AppWindow::os-seat-menu").collect();
    right_click(&ui, &menus[1], 0.5);
    assert_eq!(visible(&ui, GIVE_UP).len(), 1, "bo's menu does not offer it");
    assert!(Handle::find_by_accessible_label(&ui, RESYNC).count() > 0, "bo's menu fetches");
    press(&ui, slint::platform::Key::Escape);
    assert_eq!(Handle::find_by_accessible_label(&ui, RESYNC).count(), 0, "bo's menu closed");
    right_click(&ui, &menus[2], 0.5);
    assert!(Handle::find_by_accessible_label(&ui, GIVE_UP).count() > 1, "ed's menu offers it");
}

/// The confirm states the price and hands `give_up: true` for that seat to
/// the window's resync callback; once the engine stops listing the seat as
/// fetching, the row is back to normal.
#[test]
fn a_confirmed_stop_waiting_gives_up_and_the_row_returns_to_normal() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    let fetching = SeatRow { fetching: true, ..seat("ed", "0.1.2") };
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![SeatRow { me: true, ..seat("ana", "0.1.2") }, fetching])));
    ui.set_org_seat_notes(true);
    let sent = Rc::new(RefCell::new(Vec::new()));
    let rec = sent.clone();
    ui.on_resync_seat(move |member, give_up| rec.borrow_mut().push((member.to_string(), give_up)));
    let button = Handle::find_by_accessible_label(&ui, GIVE_UP).find(|e| e.size().width > 0.0).expect("the button");
    click(&ui, &button);
    assert_eq!(
        visible(&ui, "Stop waiting for ed's history? Messages not fetched yet stay missing.").len(),
        1,
        "the one-line confirm states the price"
    );
    let confirm = Handle::find_by_accessible_label(&ui, GIVE_UP)
        .filter(|e| e.size().width > 0.0)
        .max_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y))
        .expect("the confirm button");
    click(&ui, &confirm);
    assert_eq!(*sent.borrow(), [("ed".to_string(), true)]);
    assert_eq!(ui.get_os_resync_member(), "", "the confirm closes");

    ui.set_org_seats(ModelRc::new(VecModel::from(vec![SeatRow { me: true, ..seat("ana", "0.1.2") }, seat("ed", "0.1.2")])));
    assert!(visible(&ui, "fetching history").is_empty() && visible(&ui, GIVE_UP).is_empty(), "back to normal");
}

/// No hints: no notes column, the table as before.
#[test]
fn without_hints_there_is_no_notes_column() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![seat("ana", "0.1.2"), seat("bo", "0.1.2")])));
    assert!(by_id(&ui, "AppWindow::os-h-notes").is_none());
    assert!(by_id(&ui, "AppWindow::os-c-notes").is_none());
}

/// Hints have their own column: header and cells line up at both scales,
/// a seat without hints keeps an empty cell, two hints stack.
#[test]
fn hints_sit_in_their_own_column_aligned_at_both_scales() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![
        seat("ana", "0.1.2"),
        SeatRow { fork: true, note: "other branch at 5".into(), ..seat("bo", "0.1.2") },
        SeatRow { note: "other branch at 5\nrestore unfinished - create a new link".into(), ..seat("cy", "?") },
    ])));
    ui.set_org_seat_notes(true);
    for font in [14.0_f32, 21.0] {
        ui.global::<Theme>().set_fs_app(font);
        rail_settled();
        let header = by_id(&ui, "AppWindow::os-h-notes").expect("the notes header");
        let cells: Vec<Rect> = Handle::find_by_element_id(&ui, "AppWindow::os-c-notes")
            .map(|e| rect(&e))
            .filter(|r| r.2 > 0.0)
            .collect();
        assert_eq!(cells.len(), 3, "font {font}: a cell per row");
        for c in &cells {
            assert!((c.0 - header.0).abs() < 0.5 && (c.2 - header.2).abs() < 0.5, "font {font}: {header:?} over {c:?}");
        }
        let member = by_id(&ui, "AppWindow::os-c-member").expect("the member cell");
        assert!(header.0 > member.0 + member.2, "font {font}: right of the name");
        let stacked = shown_text(&ui, "other branch at 5\nrestore unfinished - create a new link").expect("two hints");
        assert!(stacked.3 > 1.5 * ui.global::<Theme>().get_fs_cap(), "font {font}: the two hints stack");
    }
}

/// A long hint wraps inside its column instead of widening the table.
#[test]
fn a_long_hint_wraps_inside_its_column() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    let long = "many repeats from this seat - fetch its history again if messages are missing";
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![SeatRow { note: long.into(), ..seat("bo", "0.1.2") }])));
    ui.set_org_seat_notes(true);
    let cell = by_id(&ui, "AppWindow::os-c-notes").expect("the notes cell");
    let text = shown_text(&ui, long).expect("the hint");
    assert!(text.0 >= cell.0 - 0.5 && text.0 + text.2 <= cell.0 + cell.2 + 0.5, "{text:?} inside {cell:?}");
    let pane = Handle::find_by_element_type_name(&ui, "OrgBanner").map(|e| rect(&e)).find(|r| r.2 > 0.0).expect("the banner");
    let group = by_id(&ui, "AppWindow::os-seats").expect("the seats group");
    assert!(group.0 + group.2 <= pane.0 + pane.2 + 0.5, "the table stays in the pane: {group:?} {pane:?}");
}
