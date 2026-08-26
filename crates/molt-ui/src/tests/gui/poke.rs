// SPDX-License-Identifier: GPL-3.0-or-later
//! The poke menus and their gate, headless.

use super::*;

/// **The 0px-collapse trap, pinned.** Seven of the nine poke sites wrap
/// an existing Text in a `ContextMenuArea`, and three of those wrappers
/// sit inside a LAYOUT — where an element contributes its children's
/// size constraints or nothing at all. Nothing at all means an
/// invisible, unclickable name. This measures the real geometry of the
/// chat author's name after a live mirror pass.
///
/// **Runs on the dev-ui chain only** — `ElementHandle` queries need the
/// element names Slint keeps under `SLINT_EMIT_DEBUG_INFO`, which the
/// interpreter path carries anyway while the code generator would put
/// them into the ~400k-line module (a build that already peaks at ~9 GiB).
/// The layout engine under test is the same in both paths. Run it with
/// `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test
/// -p molt-ui --lib --features molt-ui/live-preview`.
#[cfg(feature = "live-preview")]
#[test]
fn the_chat_author_name_keeps_its_width_inside_the_poke_menu_wrapper() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    ui.window()
        .set_size(slint::PhysicalSize::new(1200, 800));

    rt.block_on(async {
        w.execute(Command::CreateStart {
            name: "DevTest".to_string(),
            member: "walter".to_string(),
            threshold: 1,
            members: 1,
            relays: Vec::new(),
        })
        .await
        .ok();
        w.execute(Command::Chat {
            body: "hello group".to_string(),
            quote: None,
            channel: ChannelRef::Group,
        })
        .await
        .ok();
        mirror(&w, &ui, &last, &chat_ui).await;
    });
    assert!(chat_rows(&ui) > 0, "no chat row, nothing to measure");
    // the repeaters only materialize on a shown window in the main screen
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.show().expect("show headless");

    let names: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-name")
            .collect();
    let menus: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-menu")
            .collect();
    assert!(!names.is_empty(), "the author header must render");
    assert_eq!(names.len(), menus.len(), "every name carries its menu area");
    for (n, m) in names.iter().zip(menus.iter()) {
        assert!(
            n.size().width > 1.0,
            "author name collapsed to {}px",
            n.size().width
        );
        // the CLICK area is what breaks silently: a wrapper that
        // contributes no size constraint is invisible to the pointer
        assert!(
            m.size().width >= n.size().width && m.size().height >= n.size().height,
            "menu area {}x{} does not cover the name {}x{}",
            m.size().width,
            m.size().height,
            n.size().width,
            n.size().height
        );
    }
}

/// **A right-click must actually OPEN the menu.** Every poke site is a
/// `ContextMenuArea`; the operator reported that right-clicking does
/// nothing anywhere, while the engine path is provably fine (a poke
/// issued over MCP toasts on both nodes). This dispatches a REAL right
/// press onto the chat author's menu area and looks for the menu item
/// that must appear — checked to be ABSENT before the click, so a
/// find that always matches cannot pass for a menu.
///
/// **Runs on the dev-ui chain only** (element ids), like its geometry
/// sibling above.
#[cfg(feature = "live-preview")]
#[test]
fn a_right_click_on_a_poke_site_opens_the_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    rt.block_on(async {
        w.execute(Command::CreateStart {
            name: "DevTest".to_string(),
            member: "walter".to_string(),
            threshold: 1,
            members: 1,
            relays: Vec::new(),
        })
        .await
        .ok();
        w.execute(Command::Chat {
            body: "hello group".to_string(),
            quote: None,
            channel: ChannelRef::Group,
        })
        .await
        .ok();
        mirror(&w, &ui, &last, &chat_ui).await;
    });
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    // the author is the own seat in this fixture — make it a POKABLE
    // name so the area is enabled (the gate is what `Poke.can` decides)
    ui.global::<Poke>().set_me("petra".into());
    ui.global::<Poke>().set_on(true);

    let label = ui.global::<Strings>().get_mem_poke().to_string();
    assert!(!label.is_empty(), "the fixture must carry the menu title");
    assert!(
        poke_menu_open(&ui, &label).is_none(),
        "no menu may be findable before the click"
    );
    let menu = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "ChatRow::author-menu",
    )
    .next()
    .expect("the author menu area must render");
    right_click(&ui, &menu, 0.5);
    assert!(
        poke_menu_open(&ui, &label).is_some(),
        "right-click opened no menu carrying {label:?}"
    );
}

/// The SAME right-click, on the site the operator actually uses:
/// Organization → Members. Its `ContextMenuArea` wraps the whole row,
/// so the press must reach it wherever the row is not covered by a
/// control of its own.
#[cfg(feature = "live-preview")]
#[test]
fn a_right_click_on_a_member_row_opens_the_poke_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = members_window(true);
    let label = ui.global::<Strings>().get_mem_poke().to_string();
    assert!(!label.is_empty(), "the fixture must carry the menu title");
    assert!(
        poke_menu_open(&ui, &label).is_none(),
        "no menu may be findable before the click"
    );
    let rows: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
            .collect();
    assert_eq!(rows.len(), 2, "one menu area per member row");
    // row 1 is petra — the pokable seat
    let area = &rows[1];
    assert!(
        area.size().width > 1.0 && area.size().height > 1.0,
        "the menu area collapsed to {:?}",
        area.size()
    );
    right_click(&ui, area, 0.98);
    assert!(
        poke_menu_open(&ui, &label).is_some(),
        "right-click on the member row opened no menu"
    );
}

/// **Poking off must not make the feature vanish.** An entry that is
/// simply absent reads as a dead right-click (that is how the operator
/// met it). With the switch off the menu still opens and names the
/// action - greyed, so it says "this exists, it is off" instead of
/// nothing at all.
#[cfg(feature = "live-preview")]
#[test]
fn with_poking_off_the_member_row_still_offers_the_entry_greyed() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = members_window(false);
    let label = ui.global::<Strings>().get_mem_poke().to_string();
    assert!(!label.is_empty(), "the fixture must carry the menu title");
    assert!(
        poke_menu_open(&ui, &label).is_none(),
        "no menu may be findable before the click"
    );
    let rows: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
            .collect();
    right_click(&ui, &rows[1], 0.98);
    assert!(
        poke_menu_open(&ui, &label).is_some(),
        "the entry must still be offered, greyed - `Poke.on` is what the \
         MenuItem binds its `enabled` to, and `can()` (pinned separately) \
         is what refuses the command"
    );
}

/// The own seat is never a poke target, switch or no switch: its row
/// offers no menu at all.
#[cfg(feature = "live-preview")]
#[test]
fn the_own_seats_row_offers_no_poke_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = members_window(true);
    let label = ui.global::<Strings>().get_mem_poke().to_string();
    let rows: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
            .collect();
    // row 0 is walter — this node's own seat
    right_click(&ui, &rows[0], 0.98);
    assert!(
        poke_menu_open(&ui, &label).is_none(),
        "the own seat must not offer the entry"
    );
}

/// The pill CLIPS - a pane too narrow for even the elided name must not
/// paint over its neighbour - and a clipped element must still hand its
/// right-click to the poke menu, which renders in the window's popup
/// layer rather than inside the pill.
#[cfg(feature = "live-preview")]
#[test]
fn the_clipped_presence_pill_still_opens_its_poke_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "chat".into(),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    let poke = ui.global::<Poke>();
    poke.set_on(true);
    poke.set_me("walter".into());
    ui.set_active_members(ModelRc::new(VecModel::from(vec![MemberSync {
        name: "ada".into(),
        last: "2 min ago".into(),
        state: 0,
    }])));
    ui.show().expect("show headless");

    let label = ui.global::<Strings>().get_mem_poke().to_string();
    assert!(!label.is_empty(), "the fixture must carry the menu title");
    assert!(
        poke_menu_open(&ui, &label).is_none(),
        "no menu may be findable before the click"
    );
    let pills: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "MemberPill")
            .collect();
    assert_eq!(pills.len(), 1, "the strip renders the seat");
    right_click(&ui, &pills[0], 0.5);
    assert!(
        poke_menu_open(&ui, &label).is_some(),
        "right-click on the presence pill opened no menu"
    );
}

/// The poke gate lives in ONE place (`Poke.can`, theme.slint) because
/// nine sites render a member name and each offers the menu. This pins
/// what every one of them inherits: off means no menu anywhere, the own
/// seat is never a target, and an empty name (system lines, tombstone-
/// free rows) never is either.
#[test]
fn the_poke_gate_refuses_the_own_seat_the_empty_name_and_the_off_switch() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let poke = ui.global::<Poke>();
    poke.set_me("walter".into());

    poke.set_on(false);
    assert!(!poke.invoke_can("petra".into()), "off: no menu anywhere");

    poke.set_on(true);
    assert!(poke.invoke_can("petra".into()), "on: another seat pokable");
    assert!(!poke.invoke_can("walter".into()), "never the own seat");
    assert!(!poke.invoke_can("".into()), "no name, no target");
}

/// The menus gate on the APPLIED switch, never the settings draft: a
/// ticked-but-unsaved checkbox would offer a menu the engine refuses.
#[test]
fn the_poke_gate_follows_the_applied_setting_not_the_draft() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let sv = SessionView {
        settings: molt_core::SessionSettings {
            poke_enabled: true,
            ..molt_core::SessionSettings::default()
        },
        ..SessionView::default()
    };
    apply_session(&ui, &sv, true, &chat_ui);
    assert!(ui.global::<Poke>().get_on(), "applied switch reaches the menus");

    // the draft alone must not move it
    ui.set_cfg_poke_enabled(false);
    assert!(ui.global::<Poke>().get_on(), "the draft does not gate the menu");
}
