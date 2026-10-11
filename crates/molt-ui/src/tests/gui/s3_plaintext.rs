// SPDX-License-Identifier: GPL-3.0-or-later
//! Plaintext S3 to a clearnet host (decided 2026-10-11): Test and Save ask
//! once, with a one-line warning, and the acknowledgement reaches the
//! engine before the probe. Element lookups: live-preview only.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

thread_local! {
    static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn backend() {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(300));
}

fn shown(ui: &AppWindow, label: &str) -> bool {
    Handle::find_by_accessible_label(ui, label).next().is_some()
}

fn one(ui: &AppWindow, label: &str) -> Handle {
    Handle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("{label:?} on screen"))
}

#[test]
fn a_plaintext_endpoint_asks_once_with_its_warning_line() {
    backend();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1100, 900));
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    crate::actions::settings::wire(&ui, &cx);
    let _shown = show_headless(&ui);
    let session = |w: &WalletHandle| match rt.block_on(w.execute(Command::ReadSession)) {
        Ok(Reply::Session(s)) => s,
        other => panic!("a session: {other:?}"),
    };
    let s = ui.global::<Strings>();
    let warning = s.get_s3_pt_warn().to_string();
    assert!(!warning.is_empty() && !warning.contains('\u{2014}'), "{warning}");

    let plain = "http://203.0.113.9:9000";
    ui.set_cfg_s3_endpoint(plain.into());
    ui.set_cfg_s3_access("AK".into());
    ui.set_cfg_s3_secret("SK".into());
    ui.set_cfg_s3_bucket("b".into());
    ui.invoke_test_s3("".into());
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), plain, "the acknowledgement first");
    assert!(shown(&ui, &warning), "the plaintext line");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(session(&w).s3_test, "", "no probe before it");

    click(&ui, &one(&ui, &s.get_s3_pt_confirm()));
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), plain, "not without the tick");
    click(&ui, &one(&ui, &s.get_s3_pt_ack()));
    settle();
    click(&ui, &one(&ui, &s.get_s3_pt_confirm()));
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), "", "closed");
    let mut probed = false;
    for _ in 0..200 {
        let sv = session(&w);
        if sv.settings.s3_plaintext_confirmed == plain && !sv.s3_test.is_empty() {
            assert!(!sv.s3_test.contains("not confirmed"), "{}", sv.s3_test);
            probed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(probed, "the acknowledgement landed, then the probe ran");

    // the live mirror hands the stored settings back to the GUI
    *cx.last_settings.lock().expect("settings lock") = Some(session(&w).settings);
    ui.invoke_test_s3("".into());
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), "", "asked once");
    ui.set_cfg_s3_endpoint("https://s3.example.org".into());
    ui.invoke_test_s3("".into());
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), "", "https never asks");
    assert!(!shown(&ui, &warning));
}

/// An unchanged, unacknowledged endpoint (stored by an older build) never
/// blocks saving the other settings; using it still asks.
#[test]
fn an_unchanged_plaintext_endpoint_does_not_hold_up_save() {
    backend();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    crate::actions::settings::wire(&ui, &cx);
    let _shown = show_headless(&ui);
    let session = |w: &WalletHandle| match rt.block_on(w.execute(Command::ReadSession)) {
        Ok(Reply::Session(s)) => s,
        other => panic!("a session: {other:?}"),
    };
    let plain = "http://203.0.113.9:9000";
    rt.block_on(w.execute(Command::PatchSettings {
        patch: serde_json::json!({ "s3_endpoint": plain }),
    }))
    .expect("an older build's endpoint");
    let stored = session(&w).settings;
    crate::settings::apply_settings_fields(&ui, &stored);
    *cx.last_settings.lock().expect("settings lock") = Some(stored);

    ui.set_cfg_s3_bucket("other".into());
    ui.invoke_save_settings();
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), "", "nothing new to acknowledge");
    let mut saved = false;
    for _ in 0..200 {
        if session(&w).settings.s3_bucket == "other" {
            saved = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(saved, "the save went through");

    ui.invoke_test_s3("".into());
    settle();
    assert_eq!(ui.get_cfg_s3_pt_url(), plain, "using it still asks");
}
