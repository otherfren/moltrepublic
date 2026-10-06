// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault pane, fixture-fed (vault spec §7, §8, D4, D8): a `VaultView`
//! goes through `apply_surfaces` exactly as the mirror delivers it, the
//! dialogs build their commands through the pure builders, and a read
//! reply lands in this session's Unsealed rows only.

use molt_core::vault::{
    SecretText, VaultComplaintStatus, VaultComplaintView, VaultDepositState, VaultDepositView,
    VaultGrantState, VaultGrantView, VaultMyCheck, VaultView,
};

use molt_core::{MoltError, SurfaceSnapshot};

use super::*;
use crate::actions::vault::{
    apply_vault_read_reply, grant_command, read_command, seal_command, wire_local,
};

fn deposit(
    name: &str,
    depositor: &str,
    state: VaultDepositState,
    verified: u8,
) -> VaultDepositView {
    VaultDepositView {
        secret_id: format!("{name}{depositor}").repeat(8),
        depositor: depositor.to_string(),
        name: name.to_string(),
        kind: "text".to_string(),
        size: 2048,
        state,
        proposal: (state == VaultDepositState::Pending).then_some(7),
        replaces: None,
        verified,
        holders: 3,
        readable_by: 2,
        complaints: Vec::new(),
        reseal: false,
        mine: depositor == "a",
        held: true,
        my_check: VaultMyCheck::Ok,
    }
}

fn grant(name: &str, reader: &str, state: VaultGrantState) -> VaultGrantView {
    VaultGrantView {
        grant_id: format!("g{name}{reader}"),
        secret_id: format!("{name}b").repeat(8),
        depositor: "b".to_string(),
        name: name.to_string(),
        reader: reader.to_string(),
        state,
        proposal: (state == VaultGrantState::Pending).then_some(9),
        at: None,
        mine: reader == "a",
        answers: 0,
        need: 2,
        bad_answers: Vec::new(),
    }
}

/// A 2-of-4 vault republic seen from seat `a`: one card per proven
/// state, one committed grant to `a`.
fn fixture() -> VaultView {
    VaultView {
        real: true,
        m: 2,
        n: 4,
        base_pending: None,
        deposits: vec![
            deposit("one", "b", VaultDepositState::Committed, 1),
            deposit("two", "b", VaultDepositState::Sealed, 2),
            deposit("three", "a", VaultDepositState::Hardened, 3),
        ],
        grants: vec![grant("two", "a", VaultGrantState::Committed)],
    }
}

/// A bundle carrying `vault` and a Vault surface tab, the way
/// `gather_surfaces` builds one.
fn bundle(vault: Option<VaultView>) -> SurfacesBundle {
    let pending: Vec<ProposalView> = vault
        .iter()
        .flat_map(|v| {
            let deposits = v.deposits.iter().filter_map(|d| {
                d.proposal.map(|id| {
                    serde_json::json!({
                        "id": id, "surface": "vault", "approvals": 1, "threshold": 2,
                        "state": "proposed",
                        "payload": { "op": "deposit", "depositor": d.depositor, "name": d.name, "kind": d.kind },
                    })
                })
            });
            let grants = v.grants.iter().filter_map(|g| {
                g.proposal.map(|id| {
                    serde_json::json!({
                        "id": id, "surface": "vault", "approvals": 1, "threshold": 2,
                        "state": "proposed",
                        "payload": { "op": "grant", "grant_id": g.grant_id, "secret_id": g.secret_id, "reader": g.reader },
                    })
                })
            });
            deposits.chain(grants).collect::<Vec<_>>()
        })
        .map(|j| serde_json::from_value(j).unwrap_or_else(|e| panic!("a proposal view: {e}")))
        .collect();
    let snap: SurfaceSnapshot = serde_json::from_value(serde_json::json!({
        "surface": "vault", "gated": true, "applied": [], "pending": [],
    }))
    .expect("a snapshot");
    let snap = SurfaceSnapshot {
        pending,
        vault: vault.clone(),
        ..snap
    };
    let tab = surface_data(0, Surface::Vault, &snap, "a", None, &HashMap::new());
    SurfacesBundle {
        member: "a".to_string(),
        surfaces: vec![tab],
        members: ["a", "b", "c", "d"]
            .iter()
            .map(|n| MemberRowData {
                name: (*n).to_string(),
                id: String::new(),
                pk: String::new(),
                last: String::new(),
                last_ts: 0,
                state: 0,
                uploads: 0,
                split: String::new(),
                image: String::new(),
                image_key: String::new(),
                desc: String::new(),
            })
            .collect(),
        org_stats: OrgStats {
            features: vec!["vault".to_string()],
            mirror_quota: 1,
            ..OrgStats::default()
        },
        vault,
        ..SurfacesBundle::default()
    }
}

thread_local! {
    static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn window(view: Option<VaultView>) -> AppWindow {
    // one testing backend per test thread
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    wire_local(&ui);
    apply_surfaces(&ui, &bundle(view));
    ui
}

fn deposit_rows(ui: &AppWindow) -> Vec<VaultDepositRow> {
    ui.get_vault_deposits().iter().collect()
}

fn grant_rows(ui: &AppWindow) -> Vec<VaultGrantRow> {
    ui.get_vault_grants().iter().collect()
}

fn complaint_lines(row: &VaultDepositRow) -> Vec<String> {
    row.complaints.iter().map(|c| c.text.to_string()).collect()
}

#[test]
fn the_pane_shows_committed_sealed_hardened() {
    let ui = window(Some(fixture()));
    assert!(ui.get_vault_real());
    let rows = deposit_rows(&ui);
    let states: Vec<(String, i32, String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.name.to_string(),
                r.state,
                r.state_word.to_string(),
                r.verified.to_string(),
            )
        })
        .collect();
    assert_eq!(
        states,
        vec![
            ("one".into(), 1, "committed".into(), "1/3 verified".into()),
            ("two".into(), 2, "sealed".into(), "2/3 verified".into()),
            ("three".into(), 3, "hardened".into(), "3/3 verified".into()),
        ]
    );
    assert_eq!(
        rows[0].caption.to_string(),
        format!("by b · {}", file_size_label(2048))
    );
    assert!(
        rows.iter().all(|r| r.can_grant),
        "a committed version can be granted"
    );
    assert_eq!(
        rows.iter().map(|r| r.can_read).collect::<Vec<_>>(),
        vec![false, true, false],
        "Read only where a committed grant names this seat"
    );
}

#[test]
fn each_complaint_status_renders_its_line() {
    let mut v = fixture();
    v.deposits[0].complaints = [
        ("c", VaultComplaintStatus::Open),
        ("c", VaultComplaintStatus::BadShare),
        ("d", VaultComplaintStatus::False),
        ("d", VaultComplaintStatus::Lie),
    ]
    .iter()
    .map(|(h, s)| VaultComplaintView {
        holder: (*h).to_string(),
        status: *s,
    })
    .collect();
    let ui = window(Some(v));
    let row = &deposit_rows(&ui)[0];
    assert_eq!(
        complaint_lines(row),
        vec![
            "complaint open".to_string(),
            "bad share from b".to_string(),
            "false complaint by d".to_string(),
            "bad reveal from b".to_string(),
        ]
    );
    let bad: Vec<bool> = row.complaints.iter().map(|c| c.bad).collect();
    assert_eq!(
        bad,
        vec![false, true, false, true],
        "a line naming the depositor is the bad tone"
    );
}

#[test]
fn a_lowered_threshold_renders_readable_by() {
    let mut v = fixture();
    v.deposits[1].readable_by = 1;
    let ui = window(Some(v));
    let rows = deposit_rows(&ui);
    assert_eq!(rows[1].readable.as_str(), "readable by 1 instead of 2");
    assert_eq!(
        rows[0].readable.as_str(),
        "",
        "the full threshold says nothing"
    );
}

#[test]
fn reseal_shows_only_when_the_view_says_so() {
    let mut v = fixture();
    v.deposits[2].reseal = true;
    let ui = window(Some(v));
    let rows = deposit_rows(&ui);
    assert_eq!(
        rows.iter().map(|r| r.reseal).collect::<Vec<_>>(),
        vec![false, false, true]
    );
}

#[test]
fn the_text_cap_is_the_core_cap() {
    let ui = window(Some(fixture()));
    assert_eq!(
        usize::try_from(ui.get_vault_text_max()).expect("positive"),
        molt_core::vault::VAULT_PAYLOAD_MAX
    );
}

#[test]
fn the_seal_modal_refuses_over_100_kib() {
    let ui = window(Some(fixture()));
    ui.set_vt_seal_name("a".into());
    ui.set_vt_seal_kind("text".into());
    // multi-byte chars: the cap counts bytes, not chars
    let at_cap = "ä".repeat(molt_core::vault::VAULT_PAYLOAD_MAX / 2);
    ui.set_vt_seal_text(at_cap.as_str().into());
    assert!(ui.get_vt_seal_ok(), "exactly at the cap seals");
    assert!(seal_command(&ui).is_some());
    ui.set_vt_seal_text(format!("{at_cap}x").into());
    assert!(ui.get_vt_seal_too_large());
    assert!(!ui.get_vt_seal_ok(), "one byte over refuses");
    assert!(seal_command(&ui).is_none(), "the builder refuses as well");
}

#[test]
fn the_seal_modal_warns_on_replace() {
    let ui = window(Some(fixture()));
    ui.set_vt_seal_name("one".into());
    assert!(!ui.get_vt_seal_replace(), "`one` is b's, not mine");
    ui.set_vt_seal_name("three".into());
    assert!(
        ui.get_vt_seal_replace(),
        "`three` is mine: a seal replaces it"
    );
}

#[cfg(feature = "live-preview")]
#[test]
fn the_seal_modal_shows_both_warning_lines() {
    let ui = window(Some(fixture()));
    let _shown = show_headless(&ui);
    ui.set_vt_seal_open(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    ui.set_vt_seal_name("three".into());
    let seen = |t: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, t)
            .next()
            .is_some()
    };
    assert!(seen(
        "Any 2 members can open this at any time - by vote for one of them, or among themselves. There is no way back."
    ));
    assert!(seen(
        "Replaces three. Old copies stay readable until the next cut and in backups."
    ));
}

#[test]
fn seal_confirm_builds_vault_seal() {
    let ui = window(Some(fixture()));
    ui.set_vt_seal_name(" a ".into());
    ui.set_vt_seal_kind("password".into());
    ui.set_vt_seal_text("one".into());
    match seal_command(&ui) {
        Some(Command::VaultSeal { name, kind, text }) => {
            assert_eq!((name.as_str(), kind.as_str()), ("a", "password"));
            assert_eq!(text, SecretText("one".to_string()));
        }
        other => panic!("expected VaultSeal, got {other:?}"),
    }
    ui.set_vt_seal_text("".into());
    assert!(seal_command(&ui).is_none(), "no text, no seal");
}

/// The confirm gate is the builder's: a blank name or kind grays it.
#[test]
fn a_blank_name_or_kind_grays_the_seal_confirm() {
    let ui = window(Some(fixture()));
    ui.set_vt_seal_text("one".into());
    ui.set_vt_seal_name("a".into());
    assert!(ui.get_vt_seal_confirmable());
    for (name, kind) in [("  ", "text"), ("a", " ")] {
        ui.set_vt_seal_name(name.into());
        ui.set_vt_seal_kind(kind.into());
        assert!(seal_command(&ui).is_none());
        assert!(!ui.get_vt_seal_confirmable(), "{name:?}/{kind:?} is confirmable");
    }
}

#[test]
fn grant_confirm_builds_vault_grant() {
    let ui = window(Some(fixture()));
    let seats: Vec<String> = ui.get_vault_seats().iter().map(|s| s.to_string()).collect();
    assert_eq!(
        seats,
        vec!["a", "b", "c", "d"],
        "every seat can be the reader"
    );
    let target = deposit_rows(&ui)[0].secret_id.clone();
    ui.set_vt_grant_secret(target.clone());
    ui.set_vt_grant_reader(2);
    match grant_command(&ui) {
        Some(Command::VaultGrant { secret_id, reader }) => {
            assert_eq!(secret_id.as_str(), target.as_str());
            assert_eq!(reader, "c");
        }
        other => panic!("expected VaultGrant, got {other:?}"),
    }
    assert!(matches!(
        read_command("x"),
        Command::VaultRead { secret_id } if secret_id == "x"
    ));
}

#[test]
fn read_reply_renders_pending_then_text() {
    let ui = window(Some(fixture()));
    let id = deposit_rows(&ui)[1].secret_id.to_string();
    assert!(apply_vault_read_reply(
        &ui,
        &Reply::VaultPending {
            secret_id: id.clone(),
            have: 1,
            need: 2
        }
    ));
    let rows: Vec<VaultUnsealedRow> = ui.get_vault_unsealed().iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].name.as_str(),
        "two",
        "the card names the pending read"
    );
    assert_eq!(rows[0].status.as_str(), "waiting for answers 1/2");
    assert!(crate::actions::vault::vault_read_waiting(&ui, &id));

    assert!(apply_vault_read_reply(
        &ui,
        &Reply::VaultText {
            secret_id: id.clone(),
            name: "two".to_string(),
            kind: "text".to_string(),
            text: SecretText("one".to_string()),
        }
    ));
    let rows: Vec<VaultUnsealedRow> = ui.get_vault_unsealed().iter().collect();
    assert_eq!(rows.len(), 1, "the text replaces the wait in place");
    assert_eq!(
        (rows[0].text.as_str(), rows[0].status.as_str()),
        ("one", "")
    );
    assert!(!crate::actions::vault::vault_read_waiting(&ui, &id));
    // the two read tasks post in any order: a late wait never hides a text
    assert!(apply_vault_read_reply(
        &ui,
        &Reply::VaultPending {
            secret_id: id.clone(),
            have: 1,
            need: 2
        }
    ));
    let rows: Vec<VaultUnsealedRow> = ui.get_vault_unsealed().iter().collect();
    assert_eq!(
        (rows.len(), rows[0].text.as_str(), rows[0].status.as_str()),
        (1, "one", ""),
        "a late pending reply keeps the text"
    );
    assert!(
        !apply_vault_read_reply(&ui, &Reply::Ack),
        "not a read reply"
    );
}

/// `VaultReadable` re-reads only a row this session is waiting on; a
/// grant nobody asked to read stays closed.
#[test]
fn a_readable_event_rereads_only_a_waiting_row() {
    use crate::actions::vault::should_reread;
    let ui = window(Some(fixture()));
    let id = deposit_rows(&ui)[1].secret_id.to_string();
    assert!(!should_reread(&ui, &id), "nobody asked to read it");
    apply_vault_read_reply(
        &ui,
        &Reply::VaultPending {
            secret_id: id.clone(),
            have: 1,
            need: 2,
        },
    );
    assert!(should_reread(&ui, &id), "a waiting row re-reads");
    assert!(!should_reread(&ui, "other"), "only that row");
    apply_vault_read_reply(
        &ui,
        &Reply::VaultText {
            secret_id: id.clone(),
            name: "two".to_string(),
            kind: "text".to_string(),
            text: SecretText("one".to_string()),
        },
    );
    assert!(!should_reread(&ui, &id), "a delivered text is not re-read");
}

/// The draft belongs to the outcome: a refused seal keeps the dialog and
/// its text, an accepted one wipes both.
#[test]
fn a_refused_seal_keeps_the_draft() {
    use crate::actions::vault::{seal_issued, seal_settled};
    let ui = window(Some(fixture()));
    ui.set_vt_seal_open(true);
    ui.set_vt_seal_name("keys".into());
    ui.set_vt_seal_text("s3cret".into());
    assert!(seal_command(&ui).is_some());
    seal_issued(&ui);
    assert!(ui.get_vt_seal_busy(), "busy while in flight");
    assert!(!ui.get_vt_seal_confirmable(), "no second seal meanwhile");
    seal_settled(&ui, false);
    assert!(ui.get_vt_seal_open(), "a refusal keeps the dialog");
    assert_eq!(ui.get_vt_seal_text().as_str(), "s3cret");
    assert!(!ui.get_vt_seal_busy(), "and lets the user retry");
    seal_issued(&ui);
    seal_settled(&ui, true);
    assert!(!ui.get_vt_seal_open(), "a seal closes the dialog");
    assert_eq!(ui.get_vt_seal_text().as_str(), "", "and wipes the text");
    ui.set_vt_seal_open(true);
    ui.set_vt_seal_text("draft".into());
    seal_settled(&ui, true);
    assert_eq!(
        ui.get_vt_seal_text().as_str(),
        "draft",
        "a reply nobody waits on touches nothing"
    );
}

#[test]
fn a_bad_answer_renders_its_seat() {
    let mut v = fixture();
    v.grants[0].answers = 1;
    v.grants[0].bad_answers = vec!["c".to_string()];
    let ui = window(Some(v));
    let g = &grant_rows(&ui)[0];
    assert_eq!(g.bad.as_str(), "bad answer from c");
    assert_eq!(g.waiting.as_str(), "waiting for answers 1/2");
}

#[test]
fn a_displaced_grant_renders_dimmed() {
    let mut v = fixture();
    v.grants = vec![
        grant("two", "c", VaultGrantState::Committed),
        grant("one", "c", VaultGrantState::Void),
        grant("one", "d", VaultGrantState::Displaced),
        grant("two", "d", VaultGrantState::Pending),
    ];
    v.grants[0].at = Some(1);
    let ui = window(Some(v));
    let g = grant_rows(&ui);
    let shape: Vec<(String, bool)> = g
        .iter()
        .map(|r| (r.state_word.to_string(), r.dimmed))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("granted".into(), false),
            ("void".into(), true),
            ("released, then displaced".into(), true),
            ("pending".into(), false),
        ]
    );
    assert!(g[0].line.starts_with("two - c - "), "{}", g[0].line);
    assert_eq!(
        (g[3].proposal, g[3].votes.as_str(), g[3].can_vote),
        (9, "1/2 signed", true)
    );
}

#[test]
fn vault_proposals_carry_their_titles() {
    let mut v = fixture();
    let mut pending = deposit("four", "a", VaultDepositState::Pending, 0);
    pending.proposal = Some(7);
    let mut replace = deposit("three", "a", VaultDepositState::Pending, 0);
    replace.secret_id = "ff".repeat(32);
    replace.proposal = Some(8);
    v.deposits.push(pending);
    v.deposits.push(replace);
    v.grants.push(grant("two", "d", VaultGrantState::Pending));
    let b = bundle(Some(v));
    let titles: Vec<String> = b.surfaces[0]
        .pending
        .iter()
        .map(|p| p.text.clone())
        .collect();
    assert_eq!(
        titles,
        vec![
            "deposit four (text)".to_string(),
            "replace three (text)".to_string(),
            "grant two to d".to_string(),
        ]
    );
}

#[test]
fn a_non_vault_republic_shows_one_line() {
    let ui = window(Some(VaultView {
        real: false,
        ..VaultView::default()
    }));
    assert!(!ui.get_vault_real());
    assert_eq!(ui.get_vault_deposits().row_count(), 0);
    let ui2 = window(None);
    assert!(!ui2.get_vault_real(), "no vault surface reads as no vault");
}

#[cfg(feature = "live-preview")]
fn vault_screen(ui: &AppWindow, view: &str) -> Shown {
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("vault".into());
    ui.set_selected_view(view.into());
    let shown = show_headless(ui);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    shown
}

#[cfg(feature = "live-preview")]
#[test]
fn a_non_vault_republic_renders_one_line_and_no_seal_button() {
    type H = i_slint_backend_testing::ElementHandle;
    let ui = window(Some(VaultView {
        real: false,
        ..VaultView::default()
    }));
    let _shown = vault_screen(&ui, "secrets");
    assert!(H::find_by_accessible_label(&ui, "needs a vault republic")
        .next()
        .is_some());
    assert!(H::find_by_element_id(&ui, "VaultPane::vt-seal-btn")
        .next()
        .is_none());
    assert_eq!(H::find_by_element_type_name(&ui, "DepositCard").count(), 0);
}

#[cfg(feature = "live-preview")]
#[test]
fn a_pending_base_shows_its_progress_instead_of_cards() {
    type H = i_slint_backend_testing::ElementHandle;
    let mut v = fixture();
    v.base_pending = Some(molt_core::vault::VaultBaseProgress {
        have: 10,
        size: 100,
    });
    let ui = window(Some(v));
    assert_eq!(ui.get_vault_base_line().as_str(), "loading vault 10/100");
    let _shown = vault_screen(&ui, "secrets");
    assert!(H::find_by_accessible_label(&ui, "loading vault 10/100")
        .next()
        .is_some());
    assert_eq!(H::find_by_element_type_name(&ui, "DepositCard").count(), 0);
}

#[cfg(feature = "live-preview")]
#[test]
fn the_cards_render_with_their_actions() {
    type H = i_slint_backend_testing::ElementHandle;
    let ui = window(Some(fixture()));
    let _shown = vault_screen(&ui, "secrets");
    assert_eq!(H::find_by_element_type_name(&ui, "DepositCard").count(), 3);
    assert_eq!(H::find_by_accessible_label(&ui, "Read").count(), 1);
    assert_eq!(H::find_by_accessible_label(&ui, "Grant").count(), 3);
}

#[test]
fn unsealed_is_cleared_on_close() {
    let ui = window(Some(fixture()));
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let open = SessionView {
        active_workspace: "w".to_string(),
        ..SessionView::default()
    };
    apply_session(&ui, &open, true, &chat_ui);
    apply_vault_read_reply(
        &ui,
        &Reply::VaultText {
            secret_id: "s".to_string(),
            name: "a".to_string(),
            kind: "text".to_string(),
            text: SecretText("one".to_string()),
        },
    );
    apply_session(&ui, &open, false, &chat_ui);
    assert_eq!(
        ui.get_vault_unsealed().row_count(),
        1,
        "a re-push keeps the read"
    );
    let closed = SessionView {
        active_workspace: String::new(),
        ..SessionView::default()
    };
    ui.set_vt_seal_open(true);
    ui.set_vt_seal_name("keys".into());
    ui.set_vt_seal_text("s3cret".into());
    ui.set_vt_seal_busy(true);
    ui.set_vt_grant_open(true);
    ui.set_vt_grant_secret("s".into());
    apply_session(&ui, &closed, false, &chat_ui);
    assert_eq!(
        ui.get_vault_unsealed().row_count(),
        0,
        "closing forgets every text"
    );
    assert_eq!(
        (
            ui.get_vt_seal_open(),
            ui.get_vt_seal_text().as_str(),
            ui.get_vt_seal_name().as_str(),
            ui.get_vt_seal_busy(),
        ),
        (false, "", "", false),
        "a seal draft never moves to another republic"
    );
    assert_eq!(
        (ui.get_vt_grant_open(), ui.get_vt_grant_secret().as_str()),
        (false, ""),
        "nor does a grant"
    );
}

#[test]
fn no_em_dash_in_vault_strings() {
    let lex = include_str!("../../i18n.rs");
    let lines: Vec<&str> = lex.lines().filter(|l| l.starts_with("    vt_")).collect();
    assert!(
        lines.len() > 40,
        "the vault lexicon scan found {}",
        lines.len()
    );
    for l in lines {
        assert!(!l.contains('\u{2014}'), "{l}");
    }
    let pane = include_str!("../../../../molt-ui-window/ui/surfaces.slint");
    let pane = pane
        .split("// Vault - sealed secrets")
        .nth(1)
        .and_then(|s| s.split("// A signer mark on a mock card").next())
        .expect("the vault section");
    assert!(
        !pane.contains('\u{2014}'),
        "the vault pane carries no em dash"
    );
    // every rendered row of a fixture with every state and line
    let mut v = fixture();
    v.deposits[0].readable_by = 1;
    v.deposits[0].complaints = vec![VaultComplaintView {
        holder: "c".to_string(),
        status: VaultComplaintStatus::Lie,
    }];
    v.grants.push(grant("one", "c", VaultGrantState::Displaced));
    for lang in [0, 1] {
        let ui = window(Some(v.clone()));
        apply_strings(&ui, lang);
        let mut b = bundle(Some(v.clone()));
        b.lang = lang;
        apply_surfaces(&ui, &b);
        for r in deposit_rows(&ui) {
            let all = format!(
                "{} {} {} {} {}",
                r.caption,
                r.state_word,
                r.verified,
                r.readable,
                complaint_lines(&r).join(" ")
            );
            assert!(!all.contains('\u{2014}'), "lang {lang}: {all}");
        }
        for g in grant_rows(&ui) {
            let all = format!(
                "{} {} {} {} {}",
                g.line, g.state_word, g.votes, g.waiting, g.bad
            );
            assert!(!all.contains('\u{2014}'), "lang {lang}: {all}");
        }
    }
}

#[test]
fn model_rows_patch_in_place() {
    let ui = window(Some(fixture()));
    let deposits = ui.get_vault_deposits();
    let grants = ui.get_vault_grants();
    let mut v = fixture();
    v.deposits[0].verified = 2;
    v.grants[0].answers = 1;
    apply_surfaces(&ui, &bundle(Some(v)));
    assert!(
        std::ptr::eq(
            deposits
                .as_any()
                .downcast_ref::<VecModel<VaultDepositRow>>()
                .expect("a VecModel"),
            ui.get_vault_deposits()
                .as_any()
                .downcast_ref::<VecModel<VaultDepositRow>>()
                .expect("a VecModel"),
        ),
        "the deposit model is patched, never swapped"
    );
    assert!(std::ptr::eq(
        grants
            .as_any()
            .downcast_ref::<VecModel<VaultGrantRow>>()
            .expect("a VecModel"),
        ui.get_vault_grants()
            .as_any()
            .downcast_ref::<VecModel<VaultGrantRow>>()
            .expect("a VecModel"),
    ));
    assert_eq!(
        deposit_rows(&ui)[0].verified.as_str(),
        "2/3 verified",
        "the patch landed"
    );
}

// --- against a real engine (U3): one engine, no relay, so a deposit
// stays pending at m = 2; commit, grant and read are the lab's proof
// (`scripts/vault_lab.py`).

/// One engine with its window and the mirror's own state.
struct Node {
    w: WalletHandle,
    ui: AppWindow,
    last: Arc<Mutex<Option<SessionSettings>>>,
    chat_ui: Arc<Mutex<ChatUiState>>,
}

/// Open the one stored workspace under `root` in a fresh engine and
/// mirror it into a fresh window.
fn open_stored(root: &std::path::Path, rt: &tokio::runtime::Runtime) -> Node {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let (w, _) = node_with_chat(root);
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    wire_local(&ui);
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        let id = molt_storage::scan_workspaces(root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        w.execute(Command::OpenWorkspace { id })
            .await
            .expect("the stored workspace opens");
        mirror(&w, &ui, &last, &chat_ui).await;
    });
    Node { w, ui, last, chat_ui }
}

/// Fill the seal dialog the way a human does and confirm it on the engine.
fn seal_from_the_pane(
    w: &WalletHandle,
    ui: &AppWindow,
    rt: &tokio::runtime::Runtime,
    name: &str,
    text: &str,
) -> Result<Reply, MoltError> {
    ui.set_vt_seal_name(name.into());
    ui.set_vt_seal_kind("text".into());
    ui.set_vt_seal_text(text.into());
    let cmd = seal_command(ui).expect("the dialog builds a seal");
    rt.block_on(w.execute(cmd))
}

#[test]
fn the_vault_pane_shows_a_real_engine_deposit_as_pending() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    drop(vault_workspace_on_disk(tmp.path(), 2, &["a", "b", "c", "d"]).0);
    let Node { w, ui, last, chat_ui } = open_stored(tmp.path(), &rt);
    assert!(ui.get_vault_real(), "a roster-v6 genesis is a real vault");
    assert_eq!((ui.get_vault_m(), ui.get_vault_n()), (2, 4));
    assert!(deposit_rows(&ui).is_empty());

    seal_from_the_pane(&w, &ui, &rt, "a", "one").expect("the engine takes the seal");
    rt.block_on(mirror(&w, &ui, &last, &chat_ui));

    let rows = deposit_rows(&ui);
    assert_eq!(rows.len(), 1, "one card: {rows:?}");
    assert_eq!(
        (
            rows[0].name.as_str(),
            rows[0].kind.as_str(),
            rows[0].state,
            rows[0].state_word.as_str()
        ),
        ("a", "text", 0, "pending")
    );
    assert!(rows[0].mine, "the own deposit");
    assert!(!rows[0].can_grant, "a pending version cannot be granted");
    let tab = surface_tab(&ui, "vault").expect("the vault tab");
    assert_eq!(tab.pending.row_count(), 1, "the deposit's proposal card");
}

#[test]
fn a_v5_mock_vault_workspace_shows_one_line() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    drop(chain_workspace_on_disk(tmp.path(), 2, &["a", "b", "c", "d"], false).0);
    let Node { w, ui, .. } = open_stored(tmp.path(), &rt);
    assert!(surface_tab(&ui, "vault").is_some(), "the feature lists the tab");
    assert!(!ui.get_vault_real(), "no seat keys: the one-line pane");
    assert_eq!(ui.get_vault_deposits().row_count(), 0);
    let refused = seal_from_the_pane(&w, &ui, &rt, "a", "one");
    assert!(
        matches!(
            refused,
            Err(MoltError::Vault(molt_core::vault::VaultRefusal::NoVault))
        ),
        "{refused:?}"
    );
}

#[test]
fn ui_snapshot_counts_vault_rows() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    drop(vault_workspace_on_disk(tmp.path(), 2, &["a", "b", "c", "d"]).0);
    let Node { w, ui, last, chat_ui } = open_stored(tmp.path(), &rt);
    assert_eq!(crate::mirror::build_ui_snapshot(&ui).vault_rows, 0);
    seal_from_the_pane(&w, &ui, &rt, "a", "one").expect("seal a");
    seal_from_the_pane(&w, &ui, &rt, "b", "two").expect("seal b");
    rt.block_on(mirror(&w, &ui, &last, &chat_ui));
    assert_eq!(deposit_rows(&ui).len(), 2);
    assert_eq!(crate::mirror::build_ui_snapshot(&ui).vault_rows, 2);
}

#[test]
fn ui_snapshot_counts_grant_rows_too() {
    let ui = window(Some(fixture()));
    assert_eq!(deposit_rows(&ui).len(), 3);
    assert_eq!(grant_rows(&ui).len(), 1);
    assert_eq!(crate::mirror::build_ui_snapshot(&ui).vault_rows, 4);
}

/// Spec §7: approve is offered only once this seat's share checks and the
/// payload is here; otherwise the card grays it and names why.
fn unverified_fixture() -> VaultView {
    let mut v = fixture();
    let mut arriving = deposit("four", "b", VaultDepositState::Pending, 0);
    (arriving.proposal, arriving.held, arriving.my_check) = (Some(7), false, VaultMyCheck::Pending);
    let mut ok = deposit("five", "b", VaultDepositState::Pending, 0);
    ok.proposal = Some(8);
    let mut bad = deposit("six", "c", VaultDepositState::Pending, 0);
    (bad.proposal, bad.my_check) = (Some(10), VaultMyCheck::Bad);
    let mut own = deposit("seven", "a", VaultDepositState::Pending, 0);
    (own.proposal, own.my_check) = (Some(11), VaultMyCheck::None);
    v.deposits.extend([arriving, ok, bad, own]);
    v
}

#[test]
fn an_unverified_deposit_grays_approve_with_its_reason() {
    let b = bundle(Some(unverified_fixture()));
    let blocked: Vec<(i32, String)> = b.surfaces[0]
        .pending
        .iter()
        .map(|p| (p.id, p.blocked.clone()))
        .collect();
    let want = [(7, "payload not held"), (8, ""), (10, "not verified"), (11, "")];
    assert_eq!(blocked, want.map(|(id, r)| (id, r.to_string())));
    assert_eq!(to_proposal_row(&b.surfaces[0].pending[0]).blocked.as_str(), "payload not held");
}

#[cfg(feature = "live-preview")]
#[test]
fn an_unverified_deposit_card_names_its_reason() {
    type H = i_slint_backend_testing::ElementHandle;
    let ui = window(Some(unverified_fixture()));
    let _shown = vault_screen(&ui, "proposals");
    assert_eq!(H::find_by_accessible_label(&ui, "payload not held").count(), 1);
    assert_eq!(H::find_by_accessible_label(&ui, "not verified").count(), 1);
}
