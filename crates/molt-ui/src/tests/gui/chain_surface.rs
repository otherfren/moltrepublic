// SPDX-License-Identifier: GPL-3.0-or-later
//! v0.1.3's chain surface: the block format row and its switch, the
//! branch diagnostics (the choice is dark), and the closing state.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

fn shown(ui: &AppWindow, label: &str) -> Option<Handle> {
    Handle::find_by_accessible_label(ui, label).find(|e| e.size().width > 0.0)
}

/// The lowest visible element carrying `label` (a dialog's button sits
/// below its title).
fn lowest(ui: &AppWindow, label: &str) -> Handle {
    Handle::find_by_accessible_label(ui, label)
        .filter(|e| e.size().width > 0.0)
        .max_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y))
        .unwrap_or_else(|| panic!("{label} is shown"))
}

/// Organization -> Status, wide enough for the side settings card.
fn status_window() -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 1400));
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

fn set_row(ui: &AppWindow, (state, text, tip): (i32, String, String)) {
    ui.set_org_bv2_state(state);
    ui.set_org_bv2_text(text.into());
    ui.set_org_bv2_tip(tip.into());
}

#[test]
fn the_block_format_row_reads_new_waiting_or_ready() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    assert!(shown(&ui, "Block format").is_none(), "no row outside a chain republic");

    set_row(&ui, crate::surfaces::block_format_row(0, true, true, 12, ""));
    assert!(shown(&ui, "new (since block 12)").is_some());
    assert!(shown(&ui, "Switch block format").is_none());

    set_row(&ui, crate::surfaces::block_format_row(0, true, false, 0, "bo runs an older build"));
    let waiting = shown(&ui, "old - waiting for bo").expect("the waiting row");
    let at = slint::LogicalPosition::new(
        waiting.absolute_position().x + waiting.size().width / 2.0,
        waiting.absolute_position().y + waiting.size().height / 2.0,
    );
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    assert_eq!(ui.global::<HintTip>().get_label(), "bo runs an older build");
    assert!(shown(&ui, "Switch block format").is_none(), "no switch while a seat cannot follow");

    set_row(&ui, crate::surfaces::block_format_row(0, true, false, 0, ""));
    assert!(shown(&ui, "old - ready to switch").is_some());
    assert!(shown(&ui, "Switch block format").is_some(), "one action once every seat is ready");
}

/// The switch explains itself in a table and proposes the feature vote,
/// carrying the enabled surfaces along.
#[test]
fn the_switch_explains_its_price_and_proposes_the_feature_vote() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_feat_memory(true);
    set_row(&ui, crate::surfaces::block_format_row(0, true, false, 0, ""));
    let proposed = Rc::new(RefCell::new(Vec::new()));
    let p = proposed.clone();
    ui.on_org_propose(move |op, value| p.borrow_mut().push((op.to_string(), value.to_string())));
    click(&ui, &shown(&ui, "Switch block format").expect("the switch"));
    for label in ["What changes", "What you notice", "The price", "Who decides"] {
        assert!(shown(&ui, label).is_some(), "the table names {label}");
    }
    assert!(shown(&ui, Lexicon::en().os_bv2_price).is_some());
    click(&ui, &lowest(&ui, Lexicon::en().oc_propose));
    assert_eq!(*proposed.borrow(), [("set_features".to_string(), " memory block-v2".to_string())]);
    assert!(!ui.get_org_bv2_modal_open());
}

/// New foundings always carry the format: a fixed row, never a switch.
#[test]
fn the_founding_lists_the_block_format_as_fixed() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_member("b".into());
    ui.set_cw_step(1);
    ui.set_cw_total(2);
    ui.set_cw_can_propose(true);
    apply_strings(&ui, 0);
    let _shown = show_headless(&ui);
    let row = shown(&ui, "One block per decision").expect("the fixed row");
    click(&ui, &row);
    assert!(
        !crate::actions::ritual::charter_features(&ui).iter().any(|k| k == "block-v2"),
        "not a selection: the engine adds it to every founding"
    );
}

fn scripted_fork() -> molt_core::branch::BranchStatusView {
    use molt_core::branch::{BranchBlockRow, BranchStatusView, BranchView, ForkView};
    let block = |height: u64, risk: &str| BranchBlockRow {
        height,
        hash: format!("{height:064x}"),
        kind: "applied".to_string(),
        surface: "vault".to_string(),
        op: "grant".to_string(),
        risk: risk.to_string(),
        ..BranchBlockRow::default()
    };
    BranchStatusView {
        forks: vec![ForkView {
            base_height: 12,
            branches: vec![
                BranchView {
                    head_height: 14,
                    head_hash: "a1b2c3d4".to_string(),
                    own: true,
                    blocks: vec![block(13, ""), block(14, "irreversible")],
                    counts: [("applied".to_string(), 2)].into_iter().collect(),
                    irreversible: 1,
                    ..BranchView::default()
                },
                BranchView {
                    head_height: 13,
                    head_hash: "d4e5f6a7".to_string(),
                    blocks: vec![block(13, "")],
                    counts: [("applied".to_string(), 1)].into_iter().collect(),
                    ..BranchView::default()
                },
            ],
            ..ForkView::default()
        }],
        ..BranchStatusView::default()
    }
}

fn set_forks(ui: &AppWindow, status: &molt_core::branch::BranchStatusView) {
    let lines: Vec<slint::SharedString> =
        crate::branches::fork_lines(0, status).into_iter().map(Into::into).collect();
    ui.set_org_fork_lines(ModelRc::new(VecModel::from(lines)));
    let rows: Vec<crate::BranchRow> = crate::branches::branch_rows(0, status)
        .into_iter()
        .map(|r| crate::BranchRow { fork: r.fork, heading: r.heading, text: r.text.into(), flag: r.flag.into() })
        .collect();
    ui.set_org_branch_rows(ModelRc::new(VecModel::from(rows)));
}

#[test]
fn the_branches_block_shows_only_with_a_fork_and_details_flag_the_irreversible() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    ui.set_org_seats(ModelRc::new(VecModel::from(vec![SeatRow { member: "ana".into(), ..SeatRow::default() }])));
    assert!(shown(&ui, "Branches").is_none(), "hidden without a fork");
    set_forks(&ui, &scripted_fork());
    let line = "At block 12: ours 14 · a1b2c3, other 13 · d4e5f6";
    assert!(shown(&ui, line).is_some());
    assert!(shown(&ui, Lexicon::en().ob_later).is_some(), "repair by vote is later");
    click(&ui, &shown(&ui, "Details").expect("details"));
    assert_eq!(ui.get_org_branch_open(), 0);
    assert!(shown(&ui, "Our branch: 14 · a1b2c3 · 2 decisions · 1 irreversible").is_some());
    assert!(shown(&ui, "14 · decision vault grant").is_some());
    assert_eq!(Handle::find_by_accessible_label(&ui, "irreversible").filter(|e| e.size().width > 0.0).count(), 1);
    assert!(shown(&ui, "choose").is_none() && shown(&ui, "Keep this branch").is_none(), "no choice while dark");
}

#[test]
fn evidence_export_and_import_hand_their_commands() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window();
    set_forks(&ui, &scripted_fork());
    let sent = Rc::new(RefCell::new(Vec::new()));
    let s = sent.clone();
    ui.on_branch_evidence(move |mode, path, pass| s.borrow_mut().push((mode.to_string(), path.to_string(), pass.to_string())));
    for (button, mode) in [("Export evidence…", "export"), ("Import evidence…", "import")] {
        click(&ui, &shown(&ui, button).expect("the action"));
        assert_eq!(ui.get_ob_ev_mode(), mode);
        ui.set_ob_ev_path("/tmp/ev.molt.enc".into());
        let pass = Handle::find_by_element_type_name(&ui, "AppField")
            .filter(|e| e.size().width > 0.0)
            .max_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y))
            .expect("the passphrase field");
        click(&ui, &pass);
        for c in "long enough pass".chars() {
            type_char(&ui, &c.to_string());
        }
        let title = if mode == "export" { "Export evidence" } else { "Import evidence" };
        click(&ui, &lowest(&ui, title));
        assert_eq!(ui.get_ob_ev_mode(), "", "{mode}: the dialog closes");
    }
    let sent = sent.borrow();
    assert_eq!(sent.len(), 2);
    assert_eq!((sent[0].0.as_str(), sent[0].1.as_str(), sent[0].2.as_str()), ("export", "/tmp/ev.molt.enc", "long enough pass"));
    assert_eq!(sent[1].0, "import");
    assert!(matches!(
        crate::actions::org::branch_evidence_command("export", " /tmp/a ", "p"),
        Some(Command::BranchExport { ref dest, .. }) if dest == "/tmp/a"
    ));
    assert!(matches!(
        crate::actions::org::branch_evidence_command("import", "/tmp/a", "p"),
        Some(Command::BranchImport { ref path, .. }) if path == "/tmp/a"
    ));
    assert!(crate::actions::org::branch_evidence_command("choose", "/tmp/a", "p").is_none());
}

/// A clean close may wait for the relays: the window says so until the
/// command returns.
#[test]
fn closing_shows_until_the_close_returns() {
    let o = super::discussion::Opened::new(|root| drop(workspace_on_disk(root, 2, &["walter", "mara"], "test")));
    o.ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    apply_strings(&o.ui, 0);
    let cx = Ctx {
        rt: o.rt.handle().clone(),
        wallet: o.w.clone(),
        weak: o.ui.as_weak(),
        last_settings: o.last.clone(),
        chat_ui: o.chat_ui.clone(),
    };
    crate::actions::workspace::wire(&o.ui, &cx);
    let _shown = show_headless(&o.ui);
    o.ui.invoke_close_workspace();
    assert!(o.ui.get_closing());
    assert!(shown(&o.ui, "Closing - sending the last messages").is_some());
    crate::actions::workspace::close_done(&o.ui, Ok(Reply::Ack));
    assert!(!o.ui.get_closing());
    assert!(shown(&o.ui, "Closing - sending the last messages").is_none());
}
