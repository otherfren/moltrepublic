// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings: the Agent tab (wake command, its test, its outcome), the
//! screen's one close control, and the chat's wake lines.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

fn labelled(ui: &AppWindow, wanted: &str) -> Vec<Handle> {
    Handle::find_by_element_type_name(ui, "Text")
        .filter(|t| t.size().height > 0.0 && t.accessible_label().is_some_and(|l| l == wanted))
        .collect()
}

fn settings_window() -> (AppWindow, Shown) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    let shown = show_headless(&ui);
    (ui, shown)
}

/// The wake group left General for a tab of its own.
#[test]
fn the_agent_hooks_have_their_own_tab() {
    let (ui, _shown) = settings_window();
    let tabs = Handle::find_by_element_type_name(&ui, "SettingsTab").count();
    assert_eq!(tabs, 9, "nine tabs without a workspace");
    assert_eq!(ui.get_set_tab(), 0);
    assert!(labelled(&ui, "Wake command").is_empty(), "General lost the wake group");
    assert!(!labelled(&ui, "Enable poking").is_empty(), "poking stays on General");

    let mut bar: Vec<Handle> = Handle::find_by_element_type_name(&ui, "SettingsTab").collect();
    bar.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
    click(&ui, bar.last().expect("the Agent tab is the last one without a workspace"));
    assert_eq!(ui.get_set_tab(), 9);
    for label in [
        "Wake command",
        "Test wake",
        "Show agent skill...",
        "Timeout, seconds",
        "Show poke and test wakes in the chat",
        "Pending vote",
    ] {
        assert!(!labelled(&ui, label).is_empty(), "the Agent tab lacks `{label}`");
    }
    assert!(labelled(&ui, "Enable poking").is_empty());
}

/// One button: it hands the engine the text in the field, saved or not,
/// and the outcome reads as one line each.
#[test]
fn test_wake_takes_the_field_and_shows_the_outcome() {
    let (ui, _shown) = settings_window();
    ui.set_set_tab(9);
    let got = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = got.clone();
    ui.on_wake_test(move |draft| seen.borrow_mut().push(draft.to_string()));
    assert_eq!(labelled(&ui, "Test wake").len(), 1, "one test button");
    click(&ui, &labelled(&ui, "Test wake")[0]);
    assert!(got.borrow().is_empty(), "an empty field tests nothing");

    ui.set_cfg_poke_wake("/srv/draft.sh --once".into());
    click(&ui, &labelled(&ui, "Test wake")[0]);
    assert_eq!(*got.borrow(), ["/srv/draft.sh --once"]);

    assert!(labelled(&ui, "Last wake: exit 127").is_empty());
    ui.set_wake_test_result("check failed".into());
    ui.set_wake_test_check("not found on PATH".into());
    assert_eq!(labelled(&ui, "Last test: check failed - not found on PATH").len(), 1);
    ui.set_wake_test_result("exit 3".into());
    ui.set_wake_test_check("".into());
    ui.set_wake_test_stderr("boom\nagain".into());
    assert_eq!(labelled(&ui, "Last test: exit 3").len(), 1);
    assert_eq!(labelled(&ui, "boom\nagain").len(), 1, "the stderr tail");
    ui.set_wake_last("exit 127".into());
    assert_eq!(labelled(&ui, "Last wake: exit 127").len(), 1);
}

/// A wake line offers to switch its kind off; no other system line does.
#[test]
fn a_wake_line_in_the_chat_offers_to_hide_its_kind() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.set_selected_view("today".into());
    ui.set_selected_channel("group".into());
    apply_strings(&ui, 0);
    let row = |text: &str, wake: bool| LogLine {
        text: text.into(),
        system: true,
        wake,
        quote: -1,
        patch_id: -1,
        ..LogLine::default()
    };
    let log = |rows: Vec<LogLine>| {
        ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "chat".into(),
            applied_count: i32::try_from(rows.len()).expect("small"),
            log: ModelRc::new(VecModel::from(rows)),
            ..SurfaceTab::default()
        }]))
    };
    ui.set_surfaces(log(vec![row("a rejoined", false)]));
    let _shown = show_headless(&ui);
    assert!(labelled(&ui, "Do not show again").is_empty(), "a plain system line");

    ui.set_surfaces(log(vec![row("a rejoined", false), row("wake: test", true)]));
    let hidden = Rc::new(std::cell::Cell::new(0));
    let seen = hidden.clone();
    ui.on_wake_lines_hide(move || seen.set(seen.get() + 1));
    let links = labelled(&ui, "Do not show again");
    assert_eq!(links.len(), 1, "beside the wake line only");
    click(&ui, &links[0]);
    assert_eq!(hidden.get(), 1);
}
