// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings: the Error log tab.

use super::*;
use crate::actions::settings::{apply_log, log_request, log_runs};
use molt_core::{LogEntry, LogFilter};

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

/// The settings screen, with every `log-load` it fires recorded.
fn settings_window() -> (AppWindow, Shown, Rc<RefCell<Vec<bool>>>) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    let loads = Rc::new(RefCell::new(Vec::new()));
    let seen = loads.clone();
    ui.on_log_load(move |all| seen.borrow_mut().push(all));
    let shown = show_headless(&ui);
    (ui, shown, loads)
}

fn entry(level: &str, msg: &str) -> LogEntry {
    LogEntry {
        level: level.to_string(),
        text: format!("2026-11-12T10:00:00.000000Z {level:>5} moltd: {msg}"),
    }
}

fn view(ui: &AppWindow) -> Handle {
    Handle::find_by_element_id(ui, "AppWindow::log-view").next().expect("the log view")
}

#[test]
fn the_error_log_is_a_tab_that_loads_warnings_when_opened() {
    let (ui, _shown, loads) = settings_window();
    let mut bar: Vec<Handle> = Handle::find_by_element_type_name(&ui, "SettingsTab").collect();
    bar.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
    assert!(loads.borrow().is_empty(), "nothing is read before the tab opens");
    assert_eq!(bar.len(), 10);
    click(&ui, bar.last().expect("the last tab without a workspace"));
    assert_eq!(ui.get_set_tab(), 10);
    assert!(button(&ui, "Error log").absolute_position().x > button(&ui, "Agent").absolute_position().x);
    assert_eq!(*loads.borrow(), [false]);
    assert_eq!(labelled(&ui, "No log file").len(), 1, "nothing loaded yet");

    apply_strings(&ui, 1);
    button(&ui, "Fehlerlog");
    assert_eq!(labelled(&ui, "Warnungen und Fehler").len(), 1);
}

/// With a workspace the chain history stays the last tab.
#[test]
fn the_tab_sits_before_the_chain_history() {
    let (ui, _shown, _) = settings_window();
    ui.set_active_workspace("ws".into());
    let x = |label: &str| button(&ui, label).absolute_position().x;
    assert_eq!(Handle::find_by_element_type_name(&ui, "SettingsTab").count(), 11);
    assert!(x("Agent") < x("Error log") && x("Error log") < x("Workspace chain history"));
}

#[test]
fn the_switch_and_refresh_ask_for_what_is_selected() {
    let (ui, _shown, loads) = settings_window();
    ui.set_set_tab(10);
    click(&ui, &labelled(&ui, "Everything")[0]);
    assert!(ui.get_log_all());
    click(&ui, &labelled(&ui, "Refresh")[0]);
    click(&ui, &labelled(&ui, "Warnings and errors")[0]);
    assert!(!ui.get_log_all());
    assert_eq!(*loads.borrow(), [false, true, true, false]);

    let filter = |all| match log_request(all) {
        Command::ReadLog { lines, level } => (lines, level),
        other => panic!("{other:?}"),
    };
    assert_eq!(filter(false), (500, LogFilter::Warn));
    assert_eq!(filter(true), (500, LogFilter::All));
}

#[test]
fn entries_of_one_tint_share_a_run() {
    let entries =
        [entry("INFO", "a"), entry("DEBUG", "b"), entry("WARN", "c"), entry("ERROR", "d"), entry("ERROR", "e")];
    let runs = log_runs(&entries);
    assert_eq!(runs.iter().map(|r| r.level).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(runs[0].text.as_str(), format!("{}\n{}", entries[0].text, entries[1].text));
    assert!(log_runs(&[]).is_empty());
}

/// A line without a break point still wraps, and a log taller than the
/// view opens on its newest line.
#[test]
fn long_lines_wrap_inside_the_view_which_opens_at_the_end() {
    let (ui, _shown, _) = settings_window();
    ui.set_set_tab(10);
    let mut entries: Vec<LogEntry> = (0..120).map(|i| entry("INFO", &format!("line {i}"))).collect();
    entries.push(entry("WARN", &"x".repeat(1500)));
    entries.push(entry("ERROR", "the newest"));
    apply_log(&ui, "/n/logs/config/moltd.2026-11-12.log", &entries);

    let view = view(&ui);
    let (left, top) = (view.absolute_position().x, view.absolute_position().y);
    let (right, bottom) = (left + view.size().width, top + view.size().height);
    let runs: Vec<Handle> = ui
        .get_log_runs()
        .iter()
        .map(|r| labelled(&ui, r.text.as_str()).pop().expect("a rendered run"))
        .collect();
    assert_eq!(runs.len(), 3);
    for run in &runs {
        let x = run.absolute_position().x;
        assert!(x >= left && x + run.size().width <= right, "{x} + {} in {left}..{right}", run.size().width);
    }
    assert!(runs[1].size().height > 3.0 * runs[2].size().height, "the long line wraps");
    assert!(runs[0].size().height > view.size().height, "taller than the view");
    let newest = runs[2].absolute_position().y + runs[2].size().height;
    assert!(newest <= bottom && newest > bottom - 12.0, "{newest} at {bottom}");
    assert_eq!(labelled(&ui, "/n/logs/config/moltd.2026-11-12.log").len(), 1, "the path");
    assert!(labelled(&ui, "No entries").is_empty());
}

#[test]
fn nothing_to_show_says_so() {
    let (ui, _shown, _) = settings_window();
    ui.set_set_tab(10);
    apply_log(&ui, "/n/logs/config/moltd.2026-11-12.log", &[]);
    assert_eq!(labelled(&ui, "No entries").len(), 1);
    assert!(labelled(&ui, "No log file").is_empty());
    apply_log(&ui, "", &[]);
    assert_eq!(labelled(&ui, "No log file").len(), 1);
}

#[test]
fn copy_hands_the_shown_lines_to_the_clipboard_route() {
    let (ui, _shown, _) = settings_window();
    ui.set_set_tab(10);
    let copied = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = copied.clone();
    ui.on_copy_text(move |t| seen.borrow_mut().push(t.to_string()));
    click(&ui, &labelled(&ui, "Copy")[0]);
    assert!(copied.borrow().is_empty(), "nothing shown, nothing copied");

    let entries = [entry("WARN", "a"), entry("ERROR", "b")];
    apply_log(&ui, "/n/moltd.2026-11-12.log", &entries);
    click(&ui, &labelled(&ui, "Copy")[0]);
    assert_eq!(*copied.borrow(), [format!("{}\n{}", entries[0].text, entries[1].text)]);
    click(&ui, &labelled(&ui, "/n/moltd.2026-11-12.log")[0]);
    assert_eq!(copied.borrow().len(), 2);
    assert_eq!(copied.borrow()[1], "/n/moltd.2026-11-12.log");
}
