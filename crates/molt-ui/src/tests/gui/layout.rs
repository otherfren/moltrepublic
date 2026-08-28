// SPDX-License-Identifier: GPL-3.0-or-later
//! Geometry and scaling invariants, measured headless.

use super::*;

/// **Organization → Accepted: the Value column must never overrun its
/// cell.** A description change carries a whole sentence as its value;
/// an unwrapped `Text` reports that whole line as its PREFERRED width,
/// which pushes the row past the table instead of eliding inside it.
#[cfg(feature = "live-preview")]
#[test]
fn a_long_accepted_value_elides_inside_its_cell() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("accepted".into());
    // a seat description is typed into a multi-line box, so its value
    // can carry NEWLINES — a table cell that renders them is three
    // lines tall inside a 40px row and paints over its neighbours
    let long = "Baut an der Autistenzentrale\nund schreibt die Protokolle,\n\
         erreichbar meistens nachts, Zeitzone egal, und noch ein Satz \
         damit die Zelle ganz sicher zu schmal wird";
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        applied_count: 1,
        accepted: ModelRc::new(VecModel::from(vec![ProposalRow {
            id: 1,
            text: "Member description".into(),
            proposed: long.into(),
            ..ProposalRow::default()
        }])),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    ui.show().expect("show headless");

    let table = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
        &ui,
        "DecisionTable",
    )
    .next()
    .expect("the decided-votes table must render");
    let cell = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "DecisionTable::dt-value",
    )
    .next()
    .expect("the value cell must render");
    let right = cell.absolute_position().x + cell.size().width;
    let edge = table.absolute_position().x + table.size().width;
    eprintln!(
        "value cell {:?} w={} right={right} table right={edge}",
        cell.absolute_position(),
        cell.size().width
    );
    assert!(
        right <= edge,
        "the value ran {}px past the table",
        right - edge
    );
}

/// **The chat presence strip: the pill follows the name, and the
/// last-seen label never leaves it.** A seat name is free text, and an
/// unelided `Text` reports the WHOLE name as its preferred width - so a
/// long name pushed itself and the last-seen label straight out of the
/// fixed 150px pill (reported 2026-08-22). Two things are pinned: the
/// column follows the longest name, and inside a pill the NAME is what
/// gives way - the last-seen label stays visible.
#[cfg(feature = "live-preview")]
#[test]
fn a_long_member_name_grows_its_pill_and_keeps_the_last_seen_label() {
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

    let seat = |name: &str, last: &str| MemberSync {
        name: name.into(),
        last: last.into(),
        state: 0,
    };
    let measure = |ui: &AppWindow| -> Vec<(f32, f32, f32)> {
        let pills: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            ui,
            "MemberPill",
        )
        .collect();
        let names: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-name")
                .collect();
        let lasts: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-last")
                .collect();
        assert_eq!(pills.len(), names.len(), "every pill renders its name");
        assert_eq!(pills.len(), lasts.len(), "every pill renders its last-seen");
        pills
            .iter()
            .zip(names.iter())
            .zip(lasts.iter())
            .map(|((p, n), l)| {
                let edge = p.absolute_position().x + p.size().width;
                let name_right = n.absolute_position().x + n.size().width;
                let last_right = l.absolute_position().x + l.size().width;
                eprintln!(
                    "pill w={} right={edge} | name w={} right={name_right} | last w={} right={last_right}",
                    p.size().width,
                    n.size().width,
                    l.size().width
                );
                assert!(
                    name_right <= edge,
                    "the name ran {}px past its pill",
                    name_right - edge
                );
                assert!(
                    l.size().width > 1.0 && last_right <= edge,
                    "the last-seen label is {}px wide and ends {}px past the pill",
                    l.size().width,
                    last_right - edge
                );
                // and it is PARKED at the right edge, so the labels line
                // up down the grid instead of drifting with the name
                assert!(
                    last_right >= edge - 12.0,
                    "the last-seen label floats {}px short of the pill edge",
                    edge - last_right
                );
                (p.size().width, n.size().width, l.size().width)
            })
            .collect()
    };

    ui.set_active_members(ModelRc::new(VecModel::from(vec![
        seat("ada", "2 min ago"),
        seat("bob", "just now"),
    ])));
    ui.show().expect("show headless");
    let short = measure(&ui);

    // a name of ordinary length still fits, but the pill has to GROW for
    // it instead of cutting it off at the 150px column
    ui.set_active_members(ModelRc::new(VecModel::from(vec![
        seat("bartholomaeus-von-habsburg", "2 min ago"),
        seat("bob", "just now"),
    ])));
    let grown = measure(&ui);
    assert!(
        grown[0].0 > short[0].0 + 10.0,
        "the pill did not follow the name: {}px vs {}px",
        grown[0].0,
        short[0].0
    );

    // past every sane cap the NAME elides - the last-seen label stays
    // (measure() asserts it for every pill)
    ui.set_active_members(ModelRc::new(VecModel::from(vec![
        seat(&"x".repeat(300), "2 min ago"),
        seat("bob", "just now"),
    ])));
    let huge = measure(&ui);
    assert!(
        huge[0].1 < 2000.0,
        "the name was not elided: {}px",
        huge[0].1
    );

    // a cut name is not a lost name: hovering the pill spells it out in
    // the window-topmost hint overlay
    let pill = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
        &ui,
        "MemberPill",
    )
    .next()
    .expect("the strip renders its pills");
    let at = slint::LogicalPosition::new(
        pill.absolute_position().x + pill.size().width / 2.0,
        pill.absolute_position().y + pill.size().height / 2.0,
    );
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    assert_eq!(
        ui.global::<HintTip>().get_label().to_string(),
        "x".repeat(300),
        "the elided name must read in full on hover"
    );
}

/// **Every button must survive a bigger font.** The app font is a
/// setting (9-28px); a button whose height or width is a hardcoded
/// pixel count keeps the box of the 14px default while its label grows
/// inside it - which is how the operator met a cut-off "Entschlüsseln"
/// on the Open screen. Two invariants, measured on the real layout:
/// a button's label stays INSIDE the button, and no two buttons
/// overlap (a button taller than its fixed row lands on its neighbour).
#[cfg(feature = "live-preview")]
#[test]
fn every_button_keeps_its_label_at_the_largest_font() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    apply_strings(&ui, 1); // German: the longest labels in the app
    // the biggest size the stepper offers
    ui.global::<Theme>().set_fs_app(28.0);
    ui.set_screen(AppScreen::Open);
    ui.set_ws_list(ModelRc::new(VecModel::from(vec![
        WorkspaceItem {
            id: "a".into(),
            name: "Erste Republik".into(),
            detail: "2-of-3".into(),
            status: "Synchronisiert".into(),
            synced: true,
            backup: "vor 30 Min.".into(),
            ..WorkspaceItem::default()
        },
        WorkspaceItem {
            id: "b".into(),
            name: "Zweite Republik".into(),
            detail: "3-of-5".into(),
            status: "Offline".into(),
            encrypted: true,
            backup: "nie".into(),
            ..WorkspaceItem::default()
        },
    ])));
    ui.show().expect("show headless");
    let mut checked = assert_buttons_scale(&ui, "open");
    assert!(checked > 3, "the Open screen must render its buttons");

    // the choice screen and the three wizards, step by step
    ui.set_screen(AppScreen::Choice);
    checked += assert_buttons_scale(&ui, "choice");
    for (screen, steps, set) in [
        (AppScreen::Create, 4, 0),
        (AppScreen::Join, 3, 1),
        (AppScreen::Restore, 4, 2),
    ] {
        ui.set_screen(screen);
        for step in 0..steps {
            match set {
                0 => ui.set_cw_step(step),
                1 => ui.set_jw_step(step),
                _ => ui.set_rw_step(step),
            }
            checked += assert_buttons_scale(&ui, &format!("{screen:?} step {step}"));
        }
    }

    // the main screen, one pass per surface - WITH rows in them: the
    // buttons that sit inside chat rows, proposal cards and the members
    // table are exactly the ones a fixed row height would squash
    let log = ModelRc::new(VecModel::from(vec![
        LogLine {
            id: "aa".repeat(16).into(),
            lead: "bartholomaeus".into(),
            text: "Erste Nachricht in der Republik".into(),
            when: "2026-08-22 13:37 (gerade eben)".into(),
            first: true,
            quote: -1,
            patch_id: -1,
            ..LogLine::default()
        },
        LogLine {
            id: "bb".repeat(16).into(),
            lead: "petra".into(),
            text: "Zweite Nachricht".into(),
            when: "2026-08-22 13:38 (gerade eben)".into(),
            first: true,
            own: true,
            quote: -1,
            patch_id: -1,
            ..LogLine::default()
        },
    ]));
    let votes = ModelRc::new(VecModel::from(vec![
        ProposalRow {
            id: 1,
            text: "Mitgliedsbeschreibung ändern".into(),
            proposed: "ein neuer Satz".into(),
            ..ProposalRow::default()
        },
        ProposalRow {
            id: 2,
            text: "Relais aufnehmen".into(),
            proposed: "wss://relay.example".into(),
            ..ProposalRow::default()
        },
    ]));
    let surfaces: Vec<SurfaceTab> = ["chat", "organization", "memory", "vault", "kanban"]
        .iter()
        .map(|k| SurfaceTab {
            key: (*k).into(),
            log: log.clone(),
            pending: votes.clone(),
            accepted: votes.clone(),
            pending_count: 2,
            applied_count: 2,
            ..SurfaceTab::default()
        })
        .collect();
    ui.set_surfaces(ModelRc::new(VecModel::from(surfaces.clone())));
    ui.set_org_members(ModelRc::new(VecModel::from(vec![
        MemberRow {
            name: "bartholomaeus".into(),
            last: "vor 2 Min.".into(),
            ..MemberRow::default()
        },
        MemberRow {
            name: "petra".into(),
            last: "22.07.2026".into(),
            ..MemberRow::default()
        },
    ])));
    // the tables whose rows carry buttons - uploads, backups, the relay
    // pickers - are exactly the fixed-height rows a bigger font bursts
    ui.set_org_uploads(ModelRc::new(VecModel::from(vec![UploadRow {
        id: "cc".repeat(16).into(),
        name: "protokoll.pdf".into(),
        user: "bartholomaeus".into(),
        date: "2026-08-22".into(),
        kind: "PDF".into(),
        size: "1.2 MiB".into(),
        available: true,
        online: true,
        expires: "in 13 Tagen".into(),
        ..UploadRow::default()
    }])));
    ui.set_bk_rows(ModelRc::new(VecModel::from(vec![BackupRow {
        id: "a".into(),
        local: "Erste Republik".into(),
        remote: "erste.molt.enc".into(),
        has_local: true,
        size: "1.8 MiB".into(),
        ..BackupRow::default()
    }])));
    ui.set_cw_relay_picks(ModelRc::new(VecModel::from(vec![RelayPick {
        url: "wss://relay.example".into(),
        picked: true,
    }])));
    ui.set_screen(AppScreen::Main);
    for s in &surfaces {
        ui.set_selected_surface(s.key.clone());
        for view in ["", "members", "pending", "accepted", "today"] {
            ui.set_selected_view(view.into());
            checked += assert_buttons_scale(&ui, &format!("main/{}/{view}", s.key));
        }
    }

    // and the settings screen
    ui.set_screen(AppScreen::Settings);
    checked += assert_buttons_scale(&ui, "settings");
    // a sweep that silently stopped finding buttons proves nothing
    assert!(checked > 40, "only {checked} buttons were measured");
}

/// The measured invariants, so every screen can be checked the same
/// way: a label inside its button, and no two buttons overlapping.
#[cfg(feature = "live-preview")]
fn assert_buttons_scale(ui: &AppWindow, screen: &str) -> usize {
    let buttons: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "AppButton")
            .filter(|b| b.size().width > 0.0 && b.size().height > 0.0)
            .collect();
    // the other controls sit in the SAME rows and scale by the same
    // token: a field that outgrows its row lands on the button next to
    // it, so they all go into the overlap check
    let controls: Vec<_> = ["AppField", "AppDropdown", "AppStepper", "AppCheck"]
        .iter()
        .flat_map(|t| {
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, t)
                .filter(|c| c.size().width > 0.0 && c.size().height > 0.0)
                .collect::<Vec<_>>()
        })
        .chain(buttons.iter().cloned())
        .collect();
    let labels: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppButton::abtn-label")
            .filter(|l| l.size().width > 0.0)
            .collect();
    let rect = |e: &i_slint_backend_testing::ElementHandle| {
        let p = e.absolute_position();
        let s = e.size();
        (p.x, p.y, p.x + s.width, p.y + s.height)
    };
    for l in &labels {
        let (lx0, ly0, lx1, ly1) = rect(l);
        // the button this label belongs to: the label sits on its
        // line, so take the button whose vertical span holds the
        // label's middle and whose left edge is the nearest one left
        // of the label (an overflowing label still STARTS inside)
        let mid = (ly0 + ly1) / 2.0;
        let owner = buttons
            .iter()
            .filter(|b| {
                let (bx0, by0, _, by1) = rect(b);
                bx0 <= lx0 + 0.5 && by0 <= mid && mid <= by1
            })
            .max_by(|a, b| {
                rect(a)
                    .0
                    .partial_cmp(&rect(b).0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(owner) = owner else { continue };
        let (bx0, by0, bx1, by1) = rect(owner);
        assert!(
            lx1 <= bx1 + 0.5 && ly1 <= by1 + 0.5 && lx0 >= bx0 - 0.5 && ly0 >= by0 - 0.5,
            "{screen}: the label \"{}\" ({lx0},{ly0})-({lx1},{ly1}) breaks out of its \
             button ({bx0},{by0})-({bx1},{by1})",
            l.accessible_label().unwrap_or_default()
        );
    }
    for (i, a) in controls.iter().enumerate() {
        for b in controls.iter().skip(i + 1) {
            let (ax0, ay0, ax1, ay1) = rect(a);
            let (bx0, by0, bx1, by1) = rect(b);
            let overlap = ax0 < bx1 - 0.5
                && bx0 < ax1 - 0.5
                && ay0 < by1 - 0.5
                && by0 < ay1 - 0.5;
            assert!(
                !overlap,
                "{screen}: two controls overlap - {} ({ax0},{ay0})-({ax1},{ay1}) and \
                 {} ({bx0},{by0})-({bx1},{by1})",
                a.type_name().unwrap_or_default(),
                b.type_name().unwrap_or_default()
            );
        }
    }
    buttons.len()
}

/// **A hint must not outlive the pointer.** The nav rows write the
/// window-topmost `HintTip` overlay on hover and clear it on leave -
/// but the clear was guarded by comparing the tip's ANCHOR to the
/// row's current position, so a row that moved while hovered (the nav
/// expands its sub-views on a click, a list scrolls) could never
/// recognize its own tip again and the bubble stayed on screen for
/// good. Pinned here: leaving the row clears it, and so does leaving
/// the window - even after the row moved underneath the pointer.
#[cfg(feature = "live-preview")]
#[test]
fn a_nav_hint_disappears_when_the_pointer_leaves_the_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1000, 700));
    apply_strings(&ui, 1);
    // big font + long names: the nav label elides, which is what makes
    // the expanded row write a hint at all
    ui.global::<Theme>().set_fs_app(24.0);
    let views = ModelRc::new(VecModel::from(vec![
        ViewItem {
            key: "status".into(),
            name: "Status".into(),
            ..ViewItem::default()
        },
        ViewItem {
            key: "members".into(),
            name: "Mitglieder".into(),
            ..ViewItem::default()
        },
    ]));
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![
        SurfaceTab {
            key: "organization".into(),
            name: "Organisation der Republik".into(),
            views: views.clone(),
            ..SurfaceTab::default()
        },
        SurfaceTab {
            key: "chat".into(),
            name: "Unterhaltung und Beschlüsse".into(),
            views: views.clone(),
            ..SurfaceTab::default()
        },
    ])));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.show().expect("show headless");

    let rows: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
            .filter(|r| r.size().width > 0.0)
            .collect();
    assert!(rows.len() >= 2, "the nav must render its rows");
    // a hover change reaches the `changed` handlers on the next frame -
    // headless, that frame is `mock_elapsed_time` (it runs the change
    // trackers), which the real app gets for free from its render loop
    let frame = || {
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    };
    let hover = |ui: &AppWindow, e: &i_slint_backend_testing::ElementHandle| {
        let p = e.absolute_position();
        let s = e.size();
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(
                    p.x + s.width / 2.0,
                    p.y + s.height / 2.0,
                ),
            });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    };
    let leave = |ui: &AppWindow| {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(900.0, 400.0),
            });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    };
    let tip = |ui: &AppWindow| ui.global::<HintTip>().get_label().to_string();

    // 1. plain enter/leave
    hover(&ui, &rows[1]);
    assert!(!tip(&ui).is_empty(), "hovering a cut nav label shows its hint");
    leave(&ui);
    assert_eq!(tip(&ui), "", "the hint must go when the pointer leaves");

    // 2. the row MOVES while hovered - a bigger font resizes every nav
    //    row, so the hovered one is somewhere else by the time the
    //    pointer leaves. This is the case the old anchor-guard could
    //    not clear (it compared the tip's anchor to the row's CURRENT
    //    position), and it is deliberately not one of the navigations
    //    that drop the hint outright.
    let before = rows[1].absolute_position().y;
    hover(&ui, &rows[1]);
    assert!(!tip(&ui).is_empty(), "hint is up again");
    ui.global::<Theme>().set_fs_app(20.0);
    frame();
    let moved: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
            .filter(|r| r.size().width > 0.0)
            .collect();
    assert_ne!(
        moved[1].absolute_position().y,
        before,
        "the fixture must actually move the row"
    );
    leave(&ui);
    assert_eq!(tip(&ui), "", "a hint whose row moved must still clear");

    // 3. the pointer leaves the WINDOW (the nav sits at the left edge,
    //    so this is the ordinary way out of it)
    hover(&ui, &moved[1]);
    assert!(!tip(&ui).is_empty(), "hint is up again");
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerExited);
    frame();
    assert_eq!(tip(&ui), "", "leaving the window must clear the hint");
}

/// **Organization -> Status: the gated-settings card.** Its rows are
/// label + value + pencil inside a 300px card; an unelided label
/// reports its whole line as the row's preferred width and shoves the
/// pencil through the card's border (reported 2026-08-23). The label
/// is what gives way, and the pencils line up on one right edge.
#[cfg(feature = "live-preview")]
#[test]
fn the_org_settings_pencils_stay_inside_the_card_and_line_up() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    apply_strings(&ui, 1); // German: the long "Chat löschen nach" line
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("status".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        name: "Organisation".into(),
        ..SurfaceTab::default()
    }])));
    ui.set_org_chat_retention(30);
    ui.set_org_relays(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("wss://relay.example"),
    ])));
    ui.show().expect("show headless");

    for font in [14.0_f32, 24.0] {
        ui.global::<Theme>().set_fs_app(font);
        let card = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            &ui,
            "OrgSettingsCard",
        )
        .find(|c| c.size().width > 0.0)
        .expect("the gated-settings card must render");
        let edge = card.absolute_position().x + card.size().width;
        let pencils: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                .filter(|b| {
                    let p = b.absolute_position();
                    b.size().width > 0.0
                        && p.x >= card.absolute_position().x - 1.0
                        && p.y >= card.absolute_position().y - 1.0
                        && p.y <= card.absolute_position().y + card.size().height + 1.0
                })
                .collect();
        assert_eq!(pencils.len(), 2, "font {font}: relays + retention pencil");
        let mut rights = Vec::new();
        for b in &pencils {
            let right = b.absolute_position().x + b.size().width;
            assert!(
                right <= edge,
                "font {font}: the pencil ran {}px through the card border",
                right - edge
            );
            rights.push(right);
        }
        assert!(
            (rights[0] - rights[1]).abs() < 1.0,
            "font {font}: the pencils are not aligned: {rights:?}"
        );

        // and EVERY pencil in the pane's right-hand column shares that
        // edge - they read as one column, so a panel with its own
        // padding staggers visibly against its neighbours
        let column: Vec<f32> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                .filter(|b| {
                    let (w, h) = (b.size().width, b.size().height);
                    let right = b.absolute_position().x + w;
                    // square = one of the ✎ pencils, not a labelled button
                    w > 0.0 && (w - h).abs() < 1.0 && (edge - right).abs() < 60.0
                })
                .map(|b| b.absolute_position().x + b.size().width)
                .collect();
        assert!(column.len() >= 4, "font {font}: found {} pencils", column.len());
        let (lo, hi) = column.iter().fold((f32::MAX, f32::MIN), |(lo, hi), r| {
            (lo.min(*r), hi.max(*r))
        });
        assert!(
            hi - lo < 1.0,
            "font {font}: the pencil column staggers by {}px: {column:?}",
            hi - lo
        );
    }
}

/// The decided-votes table is an INDEX: one line per decision. A seat
/// description is typed into a multi-line box (`Ich bin der Peter\n!`
/// is what the operator's node actually holds), and rendering that
/// newline made the 40px row two lines tall. The CARD keeps the real
/// shape — sign-what-you-see reads the value as it will be applied.
#[test]
fn a_decided_rows_value_reads_as_one_line_while_the_card_keeps_the_shape() {
    let data = ProposalRowData {
        current: "Ich bin der Peter\n!".to_string(),
        proposed: "erste\n\nzweite   dritte".to_string(),
        ..ProposalRowData::default()
    };
    let row = to_decided_row(&data);
    assert_eq!(row.current.as_str(), "Ich bin der Peter !");
    assert_eq!(row.proposed.as_str(), "erste zweite dritte");
    let card = to_proposal_row(&data);
    assert!(
        card.proposed.as_str().contains('\n'),
        "the vote card must show the value as it will be applied"
    );
}

/// **The settings tab bar wraps, the titles do not.** A tab title must
/// never break inside its own tab; when the row cannot hold them all,
/// the BAR takes a second row instead. Measured on the real geometry at
/// two window widths.
#[cfg(feature = "live-preview")]
#[test]
fn the_settings_tabs_stay_one_line_and_the_bar_wraps_when_it_must() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    ui.set_active_workspace("w".into());
    apply_strings(&ui, 1); // German — the widest titles
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    ui.show().expect("show headless");

    let rows_at = |ui: &AppWindow| -> Vec<f32> {
        let mut ys: Vec<f32> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "SettingsTab")
                .map(|t| t.absolute_position().y)
                .collect();
        ys.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        ys.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        ys
    };
    let tabs: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
            .collect();
    assert_eq!(tabs.len(), 9, "nine tabs with a workspace open");
    for t in &tabs {
        assert!(
            t.size().height <= 30.0,
            "a tab grew to {}px - its title wrapped inside the tab",
            t.size().height
        );
    }
    assert_eq!(rows_at(&ui).len(), 1, "1600px holds every tab in one row");

    // …and narrow enough, the BAR breaks instead of the titles
    ui.window().set_size(slint::PhysicalSize::new(700, 900));
    let narrow: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
            .collect();
    assert_eq!(narrow.len(), 9, "no tab may disappear when the bar wraps");
    for t in &narrow {
        assert!(
            t.size().height <= 30.0,
            "a tab grew to {}px in the narrow bar",
            t.size().height
        );
    }
    assert_eq!(rows_at(&ui).len(), 2, "700px needs a second row");
}

/// **Settings: the S3 credentials have their own tab.** "Backup" used
/// to carry two errands at once - WHEN/WHICH workspace is backed up,
/// and WHERE the bucket is. The endpoint, keys and bucket moved to
/// "S3 config"; the schedule stayed. Driven by real clicks on the real
/// bar (the tabs are found by type and ordered left to right - the
/// bar's transparent measuring texts carry the same titles, so looking
/// tabs up by their label would hit those instead).
#[cfg(feature = "live-preview")]
#[test]
fn the_s3_endpoint_moved_out_of_the_backup_tab_onto_its_own() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    ui.show().expect("show headless");

    let shown = |ui: &AppWindow, label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label)
            .next()
            .is_some()
    };
    let click_tab = |ui: &AppWindow, index: usize| {
        let mut tabs: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            ui,
            "SettingsTab",
        )
        .collect();
        tabs.sort_by(|a, b| {
            a.absolute_position()
                .x
                .partial_cmp(&b.absolute_position().x)
                .expect("no NaN")
        });
        let tab = tabs.get(index).expect("the tab must render");
        let pos = tab.absolute_position();
        let size = tab.size();
        let at = slint::LogicalPosition::new(
            pos.x + size.width / 2.0,
            pos.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
    };

    click_tab(&ui, 2); // Backup
    assert!(shown(&ui, "Automatic S3 backup"), "the schedule stays on Backup");
    assert!(!shown(&ui, "S3 endpoint"), "the endpoint left the Backup tab");
    assert!(!shown(&ui, "Access key"), "so did the keys");

    click_tab(&ui, 3); // S3 config
    assert!(shown(&ui, "S3 endpoint"), "the endpoint lives on the S3 tab");
    assert!(shown(&ui, "Access key"), "and so do the keys");
    assert!(shown(&ui, "Bucket"), "and the bucket");
    assert!(!shown(&ui, "Automatic S3 backup"), "the schedule did not follow");
}

/// The Vault mock's secrets list, headless: the deposits render, and a
/// click on "Seal a secret" opens the dialog it was given (the button
/// used to be dead, with a "not yet" tooltip).
#[cfg(feature = "live-preview")]
#[test]
fn the_vault_seal_button_opens_its_dialog() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("vault".into());
    ui.set_selected_view("secrets".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "vault".into(),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    ui.show().expect("show headless");

    let cards: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SecretCard")
            .collect();
    eprintln!("secret cards: {}", cards.len());
    assert!(cards.len() >= 6, "the sample deposits must render");
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
            .next()
            .is_none(),
        "no dialog before the click"
    );

    let button = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "VaultPane::vt-seal-btn",
    )
    .next()
    .expect("the seal button must render");
    let pos = button.absolute_position();
    let size = button.size();
    let at = slint::LogicalPosition::new(
        pos.x + size.width / 2.0,
        pos.y + size.height / 2.0,
    );
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: at,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
            .next()
            .is_some(),
        "the seal dialog must open"
    );
}

/// **Organization → Members: every row cell starts where its header
/// starts** - at every font (the header's spacers stayed 56/22/70/180px
/// while the rows scaled: "2 min ago" under "public key") and with long,
/// short and empty content (a content-sized stretch column drifts the
/// columns behind it).
#[cfg(feature = "live-preview")]
#[test]
fn the_members_table_header_lines_up_with_its_rows() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = members_window(false);
    ui.set_org_chain_governed(true);
    let seat = |name: &str, last: &str, uploads: i32| MemberRow {
        name: name.into(),
        id: "9f".repeat(8).into(),
        pk: "3c".repeat(32).into(),
        last: last.into(),
        uploads,
        desc: "Gründungsmitglied, Kassenwart und Protokoll.".into(),
        ..MemberRow::default()
    };
    ui.set_org_members(ModelRc::new(VecModel::from(vec![
        seat("walter", "2 min ago", 3),
        seat("bartholomaeus-von-habsburg", "22.07.2026", 0),
        seat("bo", "", 0),
    ])));

    let x_of = |id: &str| -> Vec<f32> {
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, id)
            .filter(|e| e.size().width > 0.0)
            .map(|e| e.absolute_position().x)
            .collect()
    };
    for font in [14.0_f32, 20.0, 28.0] {
        ui.global::<Theme>().set_fs_app(font);
        for col in ["name", "desc", "id", "pk", "last", "uploads", "recover"] {
            let header = x_of(&format!("AppWindow::om-h-{col}"));
            let rows = x_of(&format!("AppWindow::om-r-{col}"));
            assert_eq!(header.len(), 1, "font {font}: one {col} header");
            assert_eq!(rows.len(), 3, "font {font}: three {col} cells");
            for (i, x) in rows.iter().enumerate() {
                assert!(
                    (x - header[0]).abs() < 1.0,
                    "font {font}: column {col} row {i} starts at {x}, its header at {}",
                    header[0]
                );
            }
        }
    }
}
