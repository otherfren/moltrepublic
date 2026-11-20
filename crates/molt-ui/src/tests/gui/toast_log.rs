// SPDX-License-Identifier: GPL-3.0-or-later
//! Every error toast also lands in the error log (WARN `gui_error`).

use super::*;
use crate::app::wire_toast_log;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

type Event = (tracing::Level, String, String);

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Event>>>);

#[derive(Default)]
struct Fields {
    message: String,
    toast: String,
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "toast" => self.toast = format!("{value:?}"),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Captured {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut f = Fields::default();
        event.record(&mut f);
        self.0
            .lock()
            .expect("capture lock")
            .push((*event.metadata().level(), f.message, f.toast));
    }
}

#[test]
fn an_error_toast_is_logged_once_and_a_plain_one_not() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    wire_toast_log(&ui);
    let _shown = show_headless(&ui);
    let cap = Captured::default();
    let sub = tracing_subscriber::registry().with(cap.clone());
    tracing::subscriber::with_default(sub, || {
        ui.invoke_show_toast("copied".into());
        ui.invoke_show_toast_error("⚠ not confirmed".into());
    });
    let events = cap.0.lock().expect("capture lock").clone();
    let logged: Vec<&Event> = events.iter().filter(|(_, m, _)| m == "gui_error").collect();
    assert_eq!(logged.len(), 1, "{events:?}");
    assert_eq!(logged[0].0, tracing::Level::WARN);
    assert_eq!(logged[0].2, "not confirmed");
}
