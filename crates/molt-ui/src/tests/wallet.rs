// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse in plain words (wallet plan §11): amounts (U3) and the one
//! progress bar's stages (U2).

use molt_core::wallet::{RunStage, ShareStatus, WalletPhase, WalletRunView, WalletTxView, WalletView};

use super::*;
use crate::wallet::{stage_line, xmr};

const XMR: u64 = 1_000_000_000_000;

/// U3: XMR, trailing zeros trimmed, never piconero, grouped per locale.
#[test]
fn amounts_render_in_xmr() {
    assert_eq!(xmr(0, 0), "0 XMR");
    assert_eq!(xmr(0, XMR), "1 XMR");
    assert_eq!(xmr(0, XMR + XMR / 4), "1.25 XMR");
    assert_eq!(xmr(0, XMR / 2), "0.5 XMR");
    assert_eq!(xmr(0, 1), "0.000000000001 XMR");
    assert_eq!(xmr(0, 1_234_567 * XMR + 500_000_000_000), "1,234,567.5 XMR");
    assert_eq!(xmr(1, 1_234_567 * XMR + 500_000_000_000), "1.234.567,5 XMR");
    assert_eq!(xmr(1, 999 * XMR), "999 XMR");
    assert_eq!(xmr(0, u64::MAX), "18,446,744.073709551615 XMR");
}

fn run(stage: RunStage, done: u32, of: u32) -> WalletRunView {
    WalletRunView {
        stage,
        done,
        of,
        ..WalletRunView::default()
    }
}

/// U2: one bar, four plain stages.
#[test]
fn the_run_reads_in_four_plain_stages() {
    let mut ready = run(RunStage::Ready, 3, 5);
    ready.missing = vec!["bob".to_string(), "carol".to_string()];
    let line = stage_line(0, &ready);
    assert_eq!(line.text, "Waiting for members 3/5");
    assert_eq!(line.missing, "Missing: bob, carol");
    assert_eq!(stage_line(0, &run(RunStage::Round1, 2, 5)).text, "Creating the purse");
    assert_eq!(stage_line(0, &run(RunStage::Round2, 4, 5)).text, "Creating the purse");
    assert_eq!(stage_line(0, &run(RunStage::Attest, 4, 5)).text, "Confirming 4/5");
    assert_eq!(stage_line(0, &run(RunStage::Sealing, 5, 5)).text, "Sealing");
    assert!(stage_line(0, &run(RunStage::Done, 5, 5)).done);
    assert_eq!(stage_line(1, &run(RunStage::Attest, 4, 5)).text, "Bestätigung 4/5");
    let mut last = -1.0_f32;
    for stage in [RunStage::Ready, RunStage::Round1, RunStage::Round2, RunStage::Attest, RunStage::Sealing, RunStage::Done] {
        let p = stage_line(0, &run(stage, 1, 5)).progress;
        assert!(p > last && (0.0..=1.0).contains(&p), "{stage:?}: {p} after {last}");
        last = p;
    }
    assert!((last - 1.0).abs() < f32::EPSILON, "done fills the bar");
}

/// U2: an abort is one reason, naming who where the engine names them.
#[test]
fn an_abort_reads_as_one_reason() {
    let abort = |reason: &str, missing: &[&str]| {
        let mut r = run(RunStage::Aborted, 0, 3);
        r.reason = Some(reason.to_string());
        r.missing = missing.iter().map(ToString::to_string).collect();
        r
    };
    let line = stage_line(0, &abort("declined", &["bob"]));
    assert!(line.aborted);
    assert_eq!(line.text, "bob declined");
    assert_eq!(line.missing, "");
    assert_eq!(stage_line(0, &abort("not ready", &["bob"])).text, "bob is offline");
    assert_eq!(stage_line(0, &abort("timeout", &["bob", "carol"])).text, "bob, carol are offline");
    assert_eq!(stage_line(1, &abort("declined", &["bob"])).text, "bob hat abgelehnt");
    for reason in ["restart", "invalid", "equivocation", "transcript", "storage", "aborted", "something new"] {
        let text = stage_line(0, &abort(reason, &[])).text;
        assert!(!text.is_empty() && text != reason, "{reason}: {text}");
    }
}

fn window() -> AppWindow {
    thread_local! {
        static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui
}

const ADDRESS: &str = "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A";

fn purse() -> WalletView {
    WalletView {
        address: ADDRESS.to_string(),
        network: "stagenet".to_string(),
        balance: 1_250_000_000_000,
        pending: 500_000_000_000,
        scan_height: 90,
        daemon_height: 100,
        connected: true,
        threshold: 2,
        participants: 3,
        phase: WalletPhase::Ready,
        shareholders: vec![
            ("a".to_string(), ShareStatus::Held),
            ("b".to_string(), ShareStatus::WatchOnly),
            ("c".to_string(), ShareStatus::Unknown),
        ],
        history: vec![
            WalletTxView { txid: "aa".to_string(), incoming: true, amount: XMR, height: 98, at: Some(0), confirmations: 3 },
            WalletTxView { txid: "bb".to_string(), incoming: true, amount: XMR / 4, height: 10, at: Some(0), confirmations: 91 },
        ],
        can_watch: true,
        ..WalletView::default()
    }
}

/// The QR is the `monero:` URI of the address, module for module.
#[test]
fn the_qr_encodes_the_monero_uri() {
    let ui = window();
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&purse()));
    let img = ui.global::<Purse>().get_qr().to_rgb8().expect("an RGB pixel buffer");
    let code = qrcode::QrCode::new(format!("monero:{ADDRESS}")).expect("encodable");
    let modules = code.width();
    let side = usize::try_from(img.width()).expect("width");
    assert_eq!(side, usize::try_from(img.height()).expect("height"), "square");
    let quiet = 4;
    assert_eq!(side % (modules + 2 * quiet), 0, "whole pixels per module");
    let k = side / (modules + 2 * quiet);
    let px = img.as_slice();
    let dark_at = |x: usize, y: usize| px[((quiet + y) * k + k / 2) * side + (quiet + x) * k + k / 2].r < 128;
    let colors = code.to_colors();
    for y in 0..modules {
        for x in 0..modules {
            assert_eq!(dark_at(x, y), colors[y * modules + x] == qrcode::Color::Dark, "module {x},{y}");
        }
    }
    assert!(px[k / 2 * side + k / 2].r > 128, "a light quiet zone");
    let bare = qrcode::QrCode::new(ADDRESS).expect("encodable");
    assert_ne!(bare.to_colors(), colors, "the URI, not the bare address");
}

/// The pane's rows: k/20 until confirmed, then the check; details per
/// seat; pending only when > 0.
#[test]
fn the_purse_renders_its_rows() {
    let ui = window();
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&purse()));
    let p = ui.global::<Purse>();
    assert_eq!(p.get_balance(), "1.25 XMR");
    assert_eq!(p.get_pending(), "0.5 XMR pending");
    assert_eq!(p.get_phase(), 4);
    let rows: Vec<WalletTxRow> = p.get_history().iter().collect();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].amount.as_str(), rows[0].conf.as_str(), rows[0].confirmed), ("+ 1 XMR", "3/20", false));
    assert_eq!((rows[1].amount.as_str(), rows[1].confirmed), ("+ 0.25 XMR", true));
    assert_eq!(rows[1].txid, "bb");
    let seats: Vec<(String, String)> = p.get_seats().iter().map(|s| (s.name.to_string(), s.part.to_string())).collect();
    assert_eq!(
        seats,
        vec![
            ("a".to_string(), "key part".to_string()),
            ("b".to_string(), "view only".to_string()),
            ("c".to_string(), "unknown".to_string()),
        ]
    );
    assert_eq!(p.get_parts_line(), "1/3");
    assert_eq!(p.get_network(), "stagenet");
    assert_eq!(p.get_height_line(), "90 / 100");
    assert_eq!(p.get_fault_line(), "");
    let paused = WalletView { scan_paused: Some("update needed".to_string()), pending: 0, ..purse() };
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&paused));
    assert_eq!(p.get_pending(), "", "nothing pending: no line");
    assert_eq!(p.get_fault_line(), "Scanning paused: update needed");
    let fault = WalletView { scan_paused: Some("daemon fault".to_string()), ..purse() };
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&fault));
    assert_eq!(p.get_fault_line(), "Node fault");
    let offline = WalletView { connected: false, ..purse() };
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&offline));
    assert_eq!(p.get_fault_line(), "Node not reachable");
    crate::wallet::apply_wallet(&ui, 0, "w", None);
    assert_eq!(p.get_phase(), 0, "no workspace: no purse");
    assert_eq!(p.get_address(), "");
}

/// U1: no crypto vocabulary in any wallet string, English or German.
#[test]
fn no_crypto_words_in_wallet_strings() {
    const BANNED: [&str; 21] = [
        "dkg", "round", "rounds", "runde", "runden", "transcript", "transkript", "attestation",
        "attestations", "attestierung", "share", "shares", "init", "nonce", "nonces", "viewkey",
        "view-key", "multisig", "piconero", "daemon", "shareholder",
    ];
    let lex = include_str!("../i18n.rs");
    let mut texts: Vec<String> = Vec::new();
    for line in lex.lines() {
        let t = line.trim_start();
        if t.starts_with("wl_") || t.starts_with("feat_wallet") {
            texts.extend(t.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    assert!(texts.len() > 100, "the scan found only {} strings", texts.len());
    for lang in [0, 1] {
        for stage in [RunStage::Ready, RunStage::Round1, RunStage::Round2, RunStage::Attest, RunStage::Sealing, RunStage::Done] {
            let mut r = run(stage, 1, 3);
            r.missing = vec!["b".to_string()];
            let l = stage_line(lang, &r);
            texts.push(l.text);
            texts.push(l.missing);
        }
        for reason in ["declined", "not ready", "timeout", "restart", "invalid", "equivocation", "transcript", "storage", "aborted"] {
            let mut r = run(RunStage::Aborted, 0, 3);
            r.reason = Some(reason.to_string());
            texts.push(stage_line(lang, &r).text);
        }
        for op in ["wallet_init", "wallet_created"] {
            texts.push(display_title(lang, &serde_json::json!({ "op": op })));
        }
    }
    use molt_core::wallet::WalletRefusal as R;
    let refusals = || {
        [
            R::NotYet, R::NoKeysFile, R::KeysIntact, R::NotChain, R::Bounds, R::InitExists, R::InitPending,
            R::NoDaemon, R::Checking, R::Birthday, R::Network, R::UnknownOp, R::UseInit, R::Cancelled,
            R::NoRun, R::RunActive, R::NotMine, R::Rng, R::Daemon("daemon: engine stopped".to_string()),
            R::Held(Box::new(R::NoDaemon)),
        ]
    };
    for lang in [0, 1] {
        for r in refusals() {
            texts.push(localize_error(lang, &molt_core::MoltError::Wallet(r)));
        }
    }
    for text in &texts {
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric() && c != '-').collect();
        for w in BANNED {
            assert!(!words.contains(&w), "{w:?} in {text:?}");
        }
        assert!(!lower.contains("view key") && !lower.contains('—'), "{text:?}");
    }
}

/// A session with two republics, `a` open; `damaged` lists the ids whose
/// backup the damaged key part file holds.
fn two_republics(open: bool, damaged: &[usize], notice: &str) -> SessionView {
    let err = molt_storage::StorageError::WalletKeysDamaged.to_string();
    let workspaces = ["a", "b"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            serde_json::from_value::<molt_core::WorkspaceInfo>(serde_json::json!({
                "id": format!("ws-{name}"), "name": name, "detail": "2-of-3", "synced": true, "state": 0,
                "last_sync_min": 0, "sync_queue": 0, "s3": true, "net": "", "members": [],
                "backup_error": if damaged.contains(&i) { err.clone() } else { String::new() },
            }))
            .expect("a workspace entry")
        })
        .collect();
    let mut sv = SessionView { notice: notice.to_string(), workspaces, ..SessionView::default() };
    sv.active_workspace = if open { sv.workspaces[0].id.clone() } else { String::new() };
    sv
}

/// W5: a backup held by a damaged key part file of the OPEN republic asks
/// once to set it aside; any other backup failure stays a toast.
#[test]
fn a_damaged_key_part_file_opens_the_loss_dialog() {
    let ui = window();
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let damaged = molt_storage::StorageError::WalletKeysDamaged.to_string();
    apply_session(&ui, &two_republics(true, &[], "backup-failed:bucket full"), false, &chat_ui);
    assert!(!ui.global::<Purse>().get_loss_open());
    apply_session(&ui, &two_republics(true, &[0], &format!("backup-failed:{damaged}")), false, &chat_ui);
    assert!(ui.global::<Purse>().get_loss_open());
    ui.global::<Purse>().set_loss_open(false);
    apply_session(&ui, &two_republics(true, &[0], &format!("backup-failed:{damaged} ")), false, &chat_ui);
    apply_session(&ui, &two_republics(true, &[0], &format!("backup-failed:{damaged}")), false, &chat_ui);
    assert!(!ui.global::<Purse>().get_loss_open(), "once per damage");
    apply_session(&ui, &two_republics(true, &[], ""), false, &chat_ui);
    apply_session(&ui, &two_republics(true, &[0], ""), false, &chat_ui);
    assert!(ui.global::<Purse>().get_loss_open(), "a new damage asks again");
}

/// W5: the set-aside acts on the open republic only, so a closed one's
/// damage is a toast naming it, and the dialog waits until it is opened.
#[test]
fn a_closed_republics_damage_names_it_and_waits_for_its_open() {
    let ui = window();
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let damaged = molt_storage::StorageError::WalletKeysDamaged.to_string();
    let mut sv = two_republics(true, &[1], &format!("backup-failed:{damaged}"));
    apply_session(&ui, &sv, false, &chat_ui);
    assert!(!ui.global::<Purse>().get_loss_open(), "b is not open");
    assert!(ui.get_toast_text().contains("b:"), "{}", ui.get_toast_text());
    sv.active_workspace = sv.workspaces[1].id.clone();
    sv.notice = String::new();
    apply_session(&ui, &sv, false, &chat_ui);
    assert!(ui.global::<Purse>().get_loss_open(), "b opened: the dialog");
}

/// W5: a manual export refused over the damaged file asks to set it aside
/// when it is the open republic's; another's keeps the red note only.
#[test]
fn a_refused_export_opens_the_loss_dialog_for_the_open_republic() {
    let damaged = molt_storage::StorageError::WalletKeysDamaged.to_string();
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    for (which, opens) in [(0, true), (1, false)] {
        let ui = window();
        let mut sv = two_republics(true, &[], "");
        sv.export.workspace = sv.workspaces[which].id.clone();
        sv.export.result = format!("error: {damaged}");
        apply_session(&ui, &sv, false, &chat_ui);
        assert_eq!(ui.global::<Purse>().get_loss_open(), opens, "export of {which}");
        assert!(ui.get_export_failed());
    }
}

/// A workspace switch starts the purse's run state afresh: no finished
/// panel carried over, and the new republic's run brings its panel.
#[test]
fn a_workspace_switch_starts_the_run_state_afresh() {
    let ui = window();
    let p = ui.global::<Purse>();
    let running = WalletView {
        phase: WalletPhase::Init,
        run: Some(run(RunStage::Ready, 1, 3)),
        ..WalletView::default()
    };
    crate::wallet::apply_wallet(&ui, 0, "a", Some(&running));
    assert!(p.get_run_active());
    crate::wallet::apply_wallet(&ui, 0, "b", Some(&purse()));
    assert!(!p.get_run_finished(), "b's purse did not just finish");
    assert!(!p.get_run_active());
    crate::wallet::apply_wallet(&ui, 0, "a", Some(&running));
    p.set_panel_dismissed(true);
    crate::wallet::apply_wallet(&ui, 0, "b", Some(&running));
    assert!(!p.get_panel_dismissed(), "b's run brings its panel");
}

/// A purse set-up card shows what approving it consents to (design §3.3).
#[test]
fn the_set_up_card_carries_the_consent_lines() {
    let card = |op: &str, surface: Surface| {
        let mut p = super::channels::view_of(1, "", molt_core::ProposalState::Proposed);
        p.surface = surface;
        p.payload = serde_json::json!({ "op": op });
        proposal_row(0, &p).notes
    };
    let notes = card("wallet_init", Surface::Wallet);
    let l = crate::i18n::Lexicon::en();
    for line in [l.wl_note_sees, l.wl_note_lost, l.wl_note_spend] {
        assert!(notes.contains(line), "{line:?} in {notes:?}");
    }
    assert_eq!(card("wallet_created", Surface::Wallet), "");
    assert_eq!(card("set_name", Surface::Organization), "");
}

/// The node: an onion is taken outright, a clearnet or local one waits
/// for the acknowledgement, a URL with a login is refused under the field.
#[test]
fn a_node_url_is_classified_like_a_relay() {
    use crate::wallet::NodeStep;
    let onion = "http://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion:18081";
    assert_eq!(crate::wallet::node_step(0, onion), NodeStep::Take(onion.to_string()));
    assert_eq!(
        crate::wallet::node_step(0, " https://node.example.org:18089 "),
        NodeStep::Confirm("https://node.example.org:18089".to_string(), 1)
    );
    assert_eq!(crate::wallet::node_step(0, "http://127.0.0.1:18081"), NodeStep::Confirm("http://127.0.0.1:18081".to_string(), 2));
    assert_eq!(crate::wallet::node_step(0, "http://u:p@node.example.org"), NodeStep::Bad("not a node address".to_string()));
    assert_eq!(crate::wallet::node_step(0, "ws://abc.onion"), NodeStep::Bad("not a node address".to_string()));
}

/// The node is stored confirmed; only a non-onion one switches non-onion
/// dialing on, as a confirmed relay does.
#[test]
fn a_taken_node_is_stored_confirmed_and_only_a_clearnet_one_opens_dialing() {
    let patch = |cmds: &[Command]| match cmds.first() {
        Some(Command::PatchSettings { patch }) => patch.clone(),
        other => panic!("a settings patch first: {other:?}"),
    };
    let onion = crate::wallet::node_commands("http://x.onion:18081", false);
    assert_eq!(onion.len(), 1, "{onion:?}");
    assert_eq!(patch(&onion), serde_json::json!({ "wallet_daemon_url": "http://x.onion:18081", "wallet_daemon_confirmed": true }));
    let clear = crate::wallet::node_commands("https://n.example.org", true);
    assert_eq!(patch(&clear)["wallet_daemon_confirmed"], serde_json::json!(true));
    assert!(matches!(clear.get(1), Some(Command::RelayClearnetSession { unlock: true })), "{clear:?}");
    assert_eq!(clear.len(), 2);
}

/// The purse's buttons are the four seat tools, consent carrying its answer.
#[test]
fn the_purse_buttons_are_the_seat_tools() {
    use crate::wallet::PurseAct;
    assert!(matches!(PurseAct::SetUp.command(), Command::WalletInit));
    assert!(matches!(PurseAct::Consent(true).command(), Command::WalletConsent { accept: true }));
    assert!(matches!(PurseAct::Consent(false).command(), Command::WalletConsent { accept: false }));
    assert!(matches!(PurseAct::Retry.command(), Command::WalletRetry));
    assert!(matches!(PurseAct::AcknowledgeLoss.command(), Command::WalletAcknowledgeLoss));
}
