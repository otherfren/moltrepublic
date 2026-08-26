// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared session state: navigation, settings, view selection and
//! the GUI's published rendering claim.

use super::support::*;
use super::*;

/// `gui_over_mcp.md` steps 1+4, the engine half: the window's publish
/// is readable back verbatim, an action without a window is REFUSED
/// (nothing could perform it — a silent ack would read as "clicked"),
/// and with a window it is announced on the event stream the live
/// mirror consumes.
#[test]
fn the_ui_snapshot_roundtrips_and_actions_are_announced() {
    let mut st = plain_state();
    let mut ev = st.subscribe_events();
    // no window yet: read answers None, an action refuses honestly
    assert!(matches!(
        st.handle(Command::ReadUiState),
        Ok(Reply::UiState { snapshot: None })
    ));
    assert!(st
        .cmd_ui_action(molt_core::UiAction {
            verb: "select_view".to_string(),
            args: serde_json::json!({ "surface": "chat", "view": "today" }),
        })
        .is_err());
    // the window publishes; the claim reads back verbatim
    let snap = molt_core::UiSnapshot {
        screen: "main".to_string(),
        surface: "chat".to_string(),
        view: "today".to_string(),
        chat_rows: 3,
        chat_in_view: true,
        generation: 7,
        ..molt_core::UiSnapshot::default()
    };
    st.handle(Command::UiPublish { snapshot: snap.clone() })
        .expect("publish acks");
    match st.handle(Command::ReadUiState) {
        Ok(Reply::UiState { snapshot: Some(got) }) => assert_eq!(got, snap),
        other => panic!("unexpected: {other:?}"),
    }
    // …and the action is announced for the mirror
    st.cmd_ui_action(molt_core::UiAction {
        verb: "chat_send".to_string(),
        args: serde_json::json!({ "body": "hi" }),
    })
    .expect("a live window performs it");
    let mut seen = false;
    while let Ok(e) = ev.try_recv() {
        if let Event::UiActionRequested { action } = e {
            assert_eq!(action.verb, "chat_send");
            seen = true;
        }
    }
    assert!(seen, "the mirror's event carries the verb");
}

#[test]
fn select_view_is_validated_shared_state() {
    rt().block_on(async {
        // Memory: enabled by the legacy feature baseline, so navigation
        // reaches the view validation (a disabled surface is refused a
        // step earlier — pinned in the D7 gate test)
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::SelectView {
            surface: Surface::Memory,
            // "archive" left the memory vocabulary with the design
            // mock (shared_memory_real.md WP-E) — denied is real
            view: "denied".to_string(),
        })
        .await
        .expect("select");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.surface, Surface::Memory);
                assert_eq!(s.view, "denied");
            }
            other => panic!("unexpected: {other:?}"),
        }
        // a view that belongs to another surface is rejected
        assert!(matches!(
            w.execute(Command::SelectView {
                surface: Surface::Chat,
                view: "balance".to_string(),
            })
            .await,
            Err(MoltError::UnknownView(..))
        ));
        // a plain surface select falls back to that surface's default view
        w.execute(Command::SelectSurface {
            surface: Surface::Memory,
        })
        .await
        .expect("select2");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => assert_eq!(s.view, "brain"),
            other => panic!("unexpected: {other:?}"),
        }
    });
}

#[test]
fn session_navigate_and_save_are_co_equal_state() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let mut ev = w.subscribe();

        // Initial session is the choice screen.
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => assert_eq!(s.screen, Screen::Choice),
            other => panic!("unexpected: {other:?}"),
        }

        // Navigating emits SessionChanged and moves the shared screen.
        w.execute(Command::Navigate {
            screen: Screen::Settings,
        })
        .await
        .expect("navigate");
        assert!(matches!(
            ev.recv().await,
            Ok(Event::SessionChanged {
                scope: SessionScope::Full
            })
        ));

        // A mock save records the values and raises the "saved" notice.
        let settings = SessionSettings {
            anonymity: "tor".to_string(),
            ..SessionSettings::default()
        };
        w.execute(Command::SetNodePosture {
            posture: molt_core::NodePosture::of(&settings),
        })
        .await
        .expect("posture");
        w.execute(Command::SaveSettings {
            settings: settings.clone(),
        })
        .await
        .expect("save");

        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Settings);
                assert_eq!(s.settings.anonymity, "tor");
                assert_eq!(s.notice, "saved");
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}
