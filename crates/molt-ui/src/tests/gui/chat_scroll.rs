// SPDX-License-Identifier: GPL-3.0-or-later
//! The chat log follows new messages only while the reader is at the
//! bottom (issue #8): scrolled up, what they read stays put and a chip
//! counts what arrived below.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// `(x, y, width, height)`.
type Rect = (f32, f32, f32, f32);

fn rect(e: &Handle) -> Rect {
    (e.absolute_position().x, e.absolute_position().y, e.size().width, e.size().height)
}

fn line(i: usize) -> LogLine {
    LogLine {
        id: format!("{i:032x}").into(),
        lead: "petra".into(),
        text: format!("message {i}").into(),
        first: true,
        quote: -1,
        patch_id: -1,
        ..LogLine::default()
    }
}

/// The chat pane with `n` messages, at app font `font`; the log's model.
fn chat_window(n: usize, font: f32) -> (AppWindow, Shown, Rc<VecModel<LogLine>>) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.set_selected_view("today".into());
    ui.set_selected_channel("group".into());
    ui.global::<Theme>().set_fs_app(font);
    let log = Rc::new(VecModel::from((0..n).map(line).collect::<Vec<_>>()));
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "chat".into(),
        applied_count: i32::try_from(n).expect("small"),
        log: ModelRc::from(log.clone()),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    settle();
    (ui, shown, log)
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(50));
}

fn log_box(ui: &AppWindow) -> Rect {
    Handle::find_by_element_id(ui, "AppWindow::chfl").map(|e| rect(&e)).find(|r| r.2 > 0.0).expect("the chat log")
}

/// Every message row's rect, in log order.
fn rows(ui: &AppWindow) -> Vec<Rect> {
    Handle::find_by_element_type_name(ui, "ChatRow").map(|e| rect(&e)).filter(|r| r.3 > 0.0).collect()
}

/// The newest row's bottom sits inside the box's lower edge.
fn at_bottom(ui: &AppWindow) -> bool {
    let b = log_box(ui);
    let last = *rows(ui).last().expect("rows");
    (last.1 + last.3) <= b.1 + b.3 + 0.5 && (last.1 + last.3) >= b.1 + b.3 - 40.0
}

fn wheel(ui: &AppWindow, dy: f32) {
    let b = log_box(ui);
    let at = slint::LogicalPosition::new(b.0 + b.2 / 2.0, b.1 + b.3 / 2.0);
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: dy });
    settle();
}

fn chip(ui: &AppWindow, label: &str) -> Option<Handle> {
    Handle::find_by_accessible_label(ui, label).find(|e| e.size().width > 0.0)
}

fn arrive(log: &VecModel<LogLine>) {
    log.push(line(log.row_count()));
    settle();
}

#[test]
fn at_the_bottom_a_new_message_keeps_the_log_at_the_bottom() {
    i_slint_backend_testing::init_no_event_loop();
    for font in [14.0, 21.0] {
        let (ui, _shown, log) = chat_window(40, font);
        assert!(at_bottom(&ui), "font {font}: opens at the bottom");
        arrive(&log);
        assert!(at_bottom(&ui), "font {font}: follows the newest");
        assert!(chip(&ui, "1 new").is_none());
    }
}

#[test]
fn scrolled_up_a_new_message_moves_nothing_and_the_chip_counts() {
    i_slint_backend_testing::init_no_event_loop();
    for font in [14.0, 21.0] {
        let (ui, _shown, log) = chat_window(40, font);
        wheel(&ui, 600.0);
        assert!(!at_bottom(&ui), "font {font}: scrolled up");
        let before = rows(&ui);
        let read = (0..before.len()).find(|i| before[*i].1 >= log_box(&ui).1).expect("a row on screen");
        arrive(&log);
        assert!((rows(&ui)[read].1 - before[read].1).abs() < 0.5, "font {font}: the read row stays put");
        assert!(chip(&ui, "1 new").is_some(), "font {font}: the chip counts one");
        arrive(&log);
        assert!(chip(&ui, "2 new").is_some(), "font {font}: and two");
        assert!((rows(&ui)[read].1 - before[read].1).abs() < 0.5);

        click(&ui, &chip(&ui, "2 new").expect("the chip"));
        settle();
        assert!(at_bottom(&ui), "font {font}: the chip jumps to the newest");
        assert!(chip(&ui, "2 new").is_none(), "font {font}: and goes");
    }
}

#[test]
fn scrolling_down_by_hand_clears_the_chip() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown, log) = chat_window(40, 14.0);
    wheel(&ui, 600.0);
    arrive(&log);
    assert!(chip(&ui, "1 new").is_some());
    wheel(&ui, -100_000.0);
    assert!(at_bottom(&ui));
    assert!(chip(&ui, "1 new").is_none());
    arrive(&log);
    assert!(at_bottom(&ui), "at the bottom again, it follows again");
}

#[test]
fn an_own_send_or_a_channel_switch_returns_to_the_bottom() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown, log) = chat_window(40, 14.0);
    wheel(&ui, 600.0);
    ui.invoke_send_draft();
    arrive(&log);
    assert!(at_bottom(&ui), "the own message is shown");

    wheel(&ui, 600.0);
    assert!(!at_bottom(&ui));
    ui.set_selected_channel("topic:plan".into());
    arrive(&log);
    assert!(at_bottom(&ui), "another channel starts at the bottom");
}
