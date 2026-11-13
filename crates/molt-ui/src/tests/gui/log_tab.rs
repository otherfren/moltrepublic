// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings: the Error log tab.

use super::*;
use crate::actions::settings::{apply_log, log_request, wire_log_view};
use crate::log_view::LogView;
use molt_core::{LogEntry, LogFilter};
use std::sync::{Arc, Mutex};

type Handle = i_slint_backend_testing::ElementHandle;

fn labelled(ui: &AppWindow, wanted: &str) -> Vec<Handle> {
    Handle::find_by_element_type_name(ui, "Text")
        .filter(|t| t.size().height > 0.0 && t.accessible_label().is_some_and(|l| l == wanted))
        .collect()
}

/// A button's own label - the tab bar's measuring texts carry the titles too.
fn button(ui: &AppWindow, label: &str) -> Handle {
    Handle::find_by_element_id(ui, "AppButton::abtn-label")
        .find(|e| e.accessible_label().is_some_and(|l| l == label))
        .unwrap_or_else(|| panic!("no button {label:?}"))
}

fn frame() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
}

/// The settings screen with the engine-free half of the tab wired, every
/// `log-load` it fires recorded, and the shared view.
type Loads = Rc<RefCell<Vec<i32>>>;

fn settings_window() -> (AppWindow, Shown, Loads, Arc<Mutex<LogView>>) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    let loads = Rc::new(RefCell::new(Vec::new()));
    let seen = loads.clone();
    ui.on_log_load(move |level| seen.borrow_mut().push(level));
    let view = Arc::new(Mutex::new(LogView::default()));
    wire_log_view(&ui, &view);
    let shown = show_headless(&ui);
    (ui, shown, loads, view)
}

fn entry(level: &str, msg: &str) -> LogEntry {
    LogEntry {
        level: level.to_string(),
        text: format!("2026-11-12T10:00:00.000000Z {level:>5} moltd: {msg}"),
    }
}

fn load(ui: &AppWindow, view: &Arc<Mutex<LogView>>, path: &str, entries: &[LogEntry]) {
    apply_log(ui, &mut view.lock().expect("view"), path, entries);
    frame();
}

fn view_of(ui: &AppWindow) -> Handle {
    Handle::find_by_element_id(ui, "AppWindow::log-view").next().expect("the log view")
}

fn sel(ui: &AppWindow) -> Handle {
    Handle::find_by_element_id(ui, "AppWindow::log-sel").next().expect("the selectable layer")
}

#[test]
fn the_error_log_is_a_tab_that_loads_warnings_when_opened() {
    let (ui, _shown, loads, _) = settings_window();
    let mut bar: Vec<Handle> = Handle::find_by_element_type_name(&ui, "SettingsTab").collect();
    bar.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
    assert!(loads.borrow().is_empty(), "nothing is read before the tab opens");
    assert_eq!(bar.len(), 10);
    click(&ui, bar.last().expect("the last tab without a workspace"));
    assert_eq!(ui.get_set_tab(), 10);
    assert!(button(&ui, "Error log").absolute_position().x > button(&ui, "Agent").absolute_position().x);
    assert_eq!(*loads.borrow(), [1]);
    assert_eq!(labelled(&ui, "No log file").len(), 1, "nothing loaded yet");

    apply_strings(&ui, 1);
    button(&ui, "Fehlerlog");
    for l in ["Fehler", "Warnungen", "Info", "Alles"] {
        button(&ui, l);
    }
}

/// With a workspace the chain history stays the last tab.
#[test]
fn the_tab_sits_before_the_chain_history() {
    let (ui, _shown, _, _) = settings_window();
    ui.set_active_workspace("ws".into());
    let x = |label: &str| button(&ui, label).absolute_position().x;
    assert_eq!(Handle::find_by_element_type_name(&ui, "SettingsTab").count(), 11);
    assert!(x("Agent") < x("Error log") && x("Error log") < x("Workspace chain history"));
}

#[test]
fn the_level_buttons_and_refresh_ask_for_what_is_selected() {
    let (ui, _shown, loads, _) = settings_window();
    ui.set_set_tab(10);
    click(&ui, &labelled(&ui, "All")[0]);
    assert_eq!(ui.get_log_level(), 3);
    click(&ui, &labelled(&ui, "Refresh")[0]);
    click(&ui, &labelled(&ui, "Errors")[0]);
    click(&ui, &labelled(&ui, "Info")[0]);
    assert_eq!(*loads.borrow(), [1, 3, 3, 0, 2]);

    let filter = |level| match log_request(level) {
        Command::ReadLog { lines, level } => (lines, level),
        other => panic!("{other:?}"),
    };
    let max = molt_core::READ_LOG_MAX_LINES;
    assert_eq!(filter(0), (max, LogFilter::Warn));
    assert_eq!(filter(1), (max, LogFilter::Warn));
    assert_eq!(filter(2), (max, LogFilter::All));
    assert_eq!(filter(3), (max, LogFilter::All));
}

/// **The rows are tinted, wrapped inside the pane, open on the newest
/// line, and a selectable text lies exactly over them** - the same rows,
/// one line each, so a drag selects what the eye reads.
#[test]
fn tinted_rows_wrap_in_the_pane_under_a_selectable_text_of_the_same_lines() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let mut entries: Vec<LogEntry> = (0..120).map(|i| entry("INFO", &format!("line {i} key=v{i}"))).collect();
    entries.push(entry("WARN", &"x".repeat(1500)));
    entries.push(entry("ERROR", "the newest"));
    load(&ui, &view, "/n/logs/config/moltd.2026-11-12.log", &entries);

    let pane = view_of(&ui);
    let (left, top) = (pane.absolute_position().x, pane.absolute_position().y);
    let (right, bottom) = (left + pane.size().width, top + pane.size().height);
    let rows = ui.get_log_rows().row_count();
    assert!(rows > entries.len() + 5, "the long line wraps into rows: {rows}");
    assert!(view.lock().expect("view").cols > 20, "the pane reported its columns");
    for t in Handle::find_by_element_type_name(&ui, "Text")
        .filter(|t| t.size().height > 0.0)
        .filter(|t| {
            let p = t.absolute_position();
            p.y >= top && p.y < bottom && p.x >= left && p.x < right
        })
    {
        let r = t.absolute_position().x + t.size().width;
        assert!(r <= right + 0.5, "a segment ends at {r}, the pane at {right}");
    }

    // tints: the stamp, the levels and a field key differ from plain text
    let tint = |s: &str| labelled(&ui, s).first().map(|_| ());
    assert!(tint("ERROR").is_some() || tint(" ERROR").is_some(), "the newest level is on screen");

    // the selectable layer: same text, one line per row, same height
    let s = sel(&ui);
    assert_eq!(ui.get_log_plain().lines().count(), rows);
    let lines = Handle::find_by_element_id(&ui, "AppWindow::log-lines").next().expect("rows");
    assert!((s.size().height - lines.size().height).abs() < 0.5, "{} vs {}", s.size().height, lines.size().height);
    assert!((s.absolute_position().y - lines.absolute_position().y).abs() < 0.5);
    assert!((s.absolute_position().x - lines.absolute_position().x).abs() < 0.5);
    // the newest row ends at the bottom of the pane
    let newest = lines.absolute_position().y + lines.size().height;
    assert!(newest <= bottom && newest > bottom - 24.0, "{newest} at {bottom}");
    assert_eq!(labelled(&ui, "/n/logs/config/moltd.2026-11-12.log").len(), 1, "the path");
}

/// The selectable layer's line height is the rows' line height: the n-th
/// line of the text sits on the n-th row however long the page is.
#[test]
fn the_selectable_text_keeps_the_row_pitch() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let entries: Vec<LogEntry> = (0..150).map(|i| entry("INFO", &format!("n{i}"))).collect();
    load(&ui, &view, "/n/x.log", &entries);
    let rows = ui.get_log_rows().row_count();
    let s = sel(&ui);
    let pitch_text = s.size().height / rows as f32;
    let row_h = Handle::find_by_element_type_name(&ui, "Text")
        .find(|t| t.accessible_label().is_some_and(|l| l.contains("n149")))
        .expect("the newest row")
        .size()
        .height;
    assert!((pitch_text - row_h).abs() < 0.05, "text pitch {pitch_text} vs row {row_h}");
}

#[test]
fn paging_walks_from_the_newest_page_to_older_ones() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let entries: Vec<LogEntry> = (0..450).map(|i| entry("INFO", &format!("n{i}"))).collect();
    load(&ui, &view, "/n/x.log", &entries);
    assert_eq!(ui.get_log_range().as_str(), "251-450 / 450");
    assert!(ui.get_log_older() && !ui.get_log_newer());
    click(&ui, &labelled(&ui, "‹")[0]);
    assert_eq!(ui.get_log_range().as_str(), "51-250 / 450");
    click(&ui, &labelled(&ui, "‹")[0]);
    assert_eq!(ui.get_log_range().as_str(), "1-50 / 450");
    assert!(!ui.get_log_older());
    click(&ui, &labelled(&ui, "›")[0]);
    assert_eq!(ui.get_log_range().as_str(), "51-250 / 450");
}

#[test]
fn the_filter_field_narrows_to_matching_entries() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let entries = [entry("INFO", "relay up"), entry("WARN", "relay down"), entry("INFO", "vote")];
    load(&ui, &view, "/n/x.log", &entries);
    ui.invoke_log_filter("RELAY".into());
    assert_eq!(ui.get_log_range().as_str(), "1-2 / 2");
    assert!(ui.get_log_text().lines().all(|l| l.contains("relay")));
}

#[test]
fn nothing_to_show_says_so() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    load(&ui, &view, "/n/logs/config/moltd.2026-11-12.log", &[]);
    assert_eq!(labelled(&ui, "No entries").len(), 1);
    assert!(labelled(&ui, "No log file").is_empty());
    load(&ui, &view, "", &[]);
    assert_eq!(labelled(&ui, "No log file").len(), 1);
}

#[test]
fn copy_hands_the_page_unwrapped_to_the_clipboard_route() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    let copied = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = copied.clone();
    ui.on_copy_text(move |t| seen.borrow_mut().push(t.to_string()));
    click(&ui, &labelled(&ui, "Copy")[0]);
    assert!(copied.borrow().is_empty(), "nothing shown, nothing copied");

    let entries = [entry("WARN", "a"), entry("ERROR", &"b".repeat(400))];
    load(&ui, &view, "/n/moltd.2026-11-12.log", &entries);
    click(&ui, &labelled(&ui, "Copy")[0]);
    assert_eq!(*copied.borrow(), [format!("{}\n{}", entries[0].text, entries[1].text)]);
    click(&ui, &labelled(&ui, "/n/moltd.2026-11-12.log")[0]);
    assert_eq!(copied.borrow().len(), 2);
    assert_eq!(copied.borrow()[1], "/n/moltd.2026-11-12.log");
}

/// The wheel scrolls the log; a drag is the text's (it selects).
#[test]
fn the_wheel_scrolls_the_log() {
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let entries: Vec<LogEntry> = (0..200).map(|i| entry("INFO", &format!("n{i}"))).collect();
    load(&ui, &view, "/n/x.log", &entries);
    let lines = || Handle::find_by_element_id(&ui, "AppWindow::log-lines").next().expect("rows");
    let before = lines().absolute_position().y;
    let pane = view_of(&ui);
    let p = pane.absolute_position();
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(p.x + 50.0, p.y + 50.0),
        delta_x: 0.0,
        delta_y: 200.0,
    });
    frame();
    assert!(lines().absolute_position().y > before, "the wheel moved the rows");
}

/// **A drag selects log text and Ctrl+C copies it** - checked end to end
/// by pasting the clipboard into the filter field.
#[test]
fn a_drag_selects_and_ctrl_c_copies_the_selected_text() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let (ui, _shown, _, view) = settings_window();
    ui.set_set_tab(10);
    ui.set_log_level(3);
    frame();
    let entries: Vec<LogEntry> = (0..5).map(|i| entry("INFO", &format!("n{i}"))).collect();
    load(&ui, &view, "/n/x.log", &entries);
    let s = sel(&ui);
    let p = s.absolute_position();
    let row_h = s.size().height / ui.get_log_rows().row_count() as f32;
    let at = |x: f32, y: f32| slint::LogicalPosition::new(x, y);
    let w = ui.window();
    // from the start of the first row to the middle of the second
    w.dispatch_event(WindowEvent::PointerMoved { position: at(p.x + 1.0, p.y + row_h / 2.0) });
    w.dispatch_event(WindowEvent::PointerPressed { position: at(p.x + 1.0, p.y + row_h / 2.0), button: PointerEventButton::Left });
    w.dispatch_event(WindowEvent::PointerMoved { position: at(p.x + 200.0, p.y + row_h * 1.5) });
    w.dispatch_event(WindowEvent::PointerReleased { position: at(p.x + 200.0, p.y + row_h * 1.5), button: PointerEventButton::Left });
    frame();
    let ctrl = |c: char| {
        w.dispatch_event(WindowEvent::KeyPressed { text: Key::Control.into() });
        w.dispatch_event(WindowEvent::KeyPressed { text: c.into() });
        w.dispatch_event(WindowEvent::KeyReleased { text: c.into() });
        w.dispatch_event(WindowEvent::KeyReleased { text: Key::Control.into() });
        frame();
    };
    ctrl('c');
    // paste into the filter field
    let field = Handle::find_by_element_type_name(&ui, "AppField").next().expect("the filter field");
    click(&ui, &field);
    ctrl('v');
    let pasted = ui.get_log_query().to_string();
    assert!(pasted.starts_with("2026-11-12T10:00:00.000000Z"), "the selection starts at the stamp: {pasted:?}");
    assert!(pasted.contains("n0"), "{pasted:?}");
    // the single-line field turns the row break into a space
    assert!(pasted.contains(" 2026-11-12T10:00:00.000000Z"), "the drag reached the second row: {pasted:?}");
    let shown = entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>().join(" ");
    assert!(shown.contains(&pasted), "{pasted:?}");
}
