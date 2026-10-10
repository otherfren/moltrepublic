// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse in plain words (wallet plan §11): the engine's `WalletView`
//! rendered for the pane, the purse stage and the details. U1 holds here:
//! no crypto vocabulary reaches a string a human reads.

use molt_core::relay::RelayKind;
use molt_core::wallet::{RunStage, ShareStatus, WalletPhase, WalletRunView, WalletView};
use molt_core::Command;
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::i18n::Lexicon;
use crate::{AppWindow, Purse, WalletSeatRow, WalletTxRow};

/// Piconero per XMR.
const PICO: u64 = 1_000_000_000_000;

fn lex(lang: i32) -> Lexicon {
    if lang == 1 {
        Lexicon::de()
    } else {
        Lexicon::en()
    }
}

/// An amount in XMR (U3): trailing zeros trimmed, never piconero, the
/// whole part grouped per locale.
pub(crate) fn xmr(lang: i32, pico: u64) -> String {
    let (group, point) = if lang == 1 { ('.', ',') } else { (',', '.') };
    let digits = (pico / PICO).to_string();
    let mut whole = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            whole.push(group);
        }
        whole.push(c);
    }
    let frac = format!("{:012}", pico % PICO);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{whole} XMR")
    } else {
        format!("{whole}{point}{frac} XMR")
    }
}

/// One run as the purse stage shows it (U2).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct StageLine {
    /// The stage in plain words.
    pub(crate) text: String,
    /// The one bar, 0..=1.
    pub(crate) progress: f32,
    /// Who is missing ("" = nobody).
    pub(crate) missing: String,
    /// The run ended without a purse.
    pub(crate) aborted: bool,
    /// The purse committed.
    pub(crate) done: bool,
}

/// `done` of `of` as a share of one stage's quarter of the bar.
fn part(done: u32, of: u32) -> f32 {
    if of == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let f = done.min(of) as f32 / of as f32;
    f
}

/// The purse stage's line for `run`.
pub(crate) fn stage_line(lang: i32, run: &WalletRunView) -> StageLine {
    let l = lex(lang);
    let count = format!("{}/{}", run.done, run.of);
    let quarter = |base: f32| base + 0.24 * part(run.done, run.of);
    let (text, progress) = match run.stage {
        RunStage::Ready => (format!("{} {count}", l.wl_waiting), quarter(0.0)),
        RunStage::Round1 => (l.wl_creating.to_string(), 0.25 + 0.12 * part(run.done, run.of)),
        RunStage::Round2 => (l.wl_creating.to_string(), 0.375 + 0.12 * part(run.done, run.of)),
        RunStage::Attest => (format!("{} {count}", l.wl_confirming), quarter(0.5)),
        RunStage::Sealing => (l.wl_sealing.to_string(), 0.9),
        RunStage::Done => (l.wl_ready.to_string(), 1.0),
        RunStage::Aborted => (abort_reason(&l, run.reason.as_deref(), &run.missing), 0.0),
    };
    let missing = if run.stage == RunStage::Ready && !run.missing.is_empty() {
        format!("{} {}", l.wl_missing, run.missing.join(", "))
    } else {
        String::new()
    };
    StageLine {
        text,
        progress,
        missing,
        aborted: run.stage == RunStage::Aborted,
        done: run.stage == RunStage::Done,
    }
}

/// The one abort line: who, where the engine names them, else why.
fn abort_reason(l: &Lexicon, reason: Option<&str>, missing: &[String]) -> String {
    let names = missing.join(", ");
    match reason.unwrap_or_default() {
        "declined" if !names.is_empty() => format!("{names} {}", l.wl_r_declined),
        "not ready" | "timeout" if missing.len() == 1 => format!("{names} {}", l.wl_r_offline_one),
        "not ready" | "timeout" if !names.is_empty() => format!("{names} {}", l.wl_r_offline_many),
        "restart" => l.wl_r_restart.to_string(),
        "invalid" => l.wl_r_invalid.to_string(),
        "equivocation" | "transcript" => l.wl_r_mismatch.to_string(),
        "storage" => l.wl_r_storage.to_string(),
        _ => l.wl_r_stopped.to_string(),
    }
}

/// A purse vote's title (`None` = not one): the op stays wire-side.
pub(crate) fn op_title(lang: i32, op: Option<&str>) -> Option<String> {
    let l = lex(lang);
    match op? {
        "wallet_init" => Some(l.wl_set_up.to_string()),
        "wallet_created" => Some(l.wl_seal_title.to_string()),
        _ => None,
    }
}

/// Confirmations before a transfer counts (design §7).
const CONFIRMATIONS: u64 = 20;
/// QR quiet zone, in modules.
const QR_QUIET: usize = 4;
/// Pixels per QR module; the Image scales it, pixelated.
const QR_PX: usize = 4;

/// The `monero:` URI of `address` as a module grid with its quiet zone
/// (`qrcode` with no image stack, wallet plan §6).
pub(crate) fn qr_image(address: &str) -> slint::Image {
    let Ok(code) = qrcode::QrCode::new(format!("monero:{address}")) else {
        return slint::Image::default();
    };
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QR_QUIET) * QR_PX;
    let Ok(edge) = u32::try_from(side) else {
        return slint::Image::default();
    };
    let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(edge, edge);
    let px = buf.make_mut_slice();
    for (i, p) in px.iter_mut().enumerate() {
        let (x, y) = ((i % side) / QR_PX, (i / side) / QR_PX);
        let dark = x >= QR_QUIET
            && y >= QR_QUIET
            && x < QR_QUIET + modules
            && y < QR_QUIET + modules
            && colors[(y - QR_QUIET) * modules + (x - QR_QUIET)] == qrcode::Color::Dark;
        let v = if dark { 0x1a } else { 0xff };
        *p = slint::Rgb8Pixel { r: v, g: v, b: v };
    }
    slint::Image::from_rgb8(buf)
}

fn phase_index(phase: WalletPhase) -> i32 {
    match phase {
        WalletPhase::Off => 0,
        WalletPhase::Bounds => 1,
        WalletPhase::NoPurse => 2,
        WalletPhase::Init => 3,
        WalletPhase::Ready => 4,
    }
}

/// Fill the `Purse` global from the engine's view (`None` = no open
/// republic).
pub(crate) fn apply_wallet(ui: &AppWindow, lang: i32, ws: &str, view: Option<&WalletView>) {
    let l = lex(lang);
    let empty = WalletView::default();
    let v = view.unwrap_or(&empty);
    let p = ui.global::<Purse>();
    p.set_phase(phase_index(v.phase));
    p.set_founding(v.founding);
    p.set_can_watch(v.can_watch);
    p.set_balance(xmr(lang, v.balance).into());
    p.set_pending(if v.pending > 0 { format!("{} {}", xmr(lang, v.pending), l.wl_pending) } else { String::new() }.into());
    if p.get_address() != v.address.as_str() {
        p.set_qr(if v.address.is_empty() { slint::Image::default() } else { qr_image(&v.address) });
        p.set_address(v.address.as_str().into());
    }
    let history: Vec<WalletTxRow> = v
        .history
        .iter()
        .map(|t| WalletTxRow {
            incoming: t.incoming,
            amount: format!("{} {}", if t.incoming { "+" } else { "-" }, xmr(lang, t.amount)).into(),
            date: t.at.map(crate::labels::file_date_label).unwrap_or_default().into(),
            conf: format!("{}/{CONFIRMATIONS}", t.confirmations.min(CONFIRMATIONS)).into(),
            confirmed: t.confirmations >= CONFIRMATIONS,
            txid: t.txid.as_str().into(),
        })
        .collect();
    p.set_history(ModelRc::new(VecModel::from(history)));
    let seats: Vec<WalletSeatRow> = v
        .shareholders
        .iter()
        .map(|(name, status)| WalletSeatRow {
            name: name.as_str().into(),
            part: match status {
                ShareStatus::Held => l.wl_part_held,
                ShareStatus::WatchOnly => l.wl_part_view,
                ShareStatus::Unknown => l.wl_part_unknown,
            }
            .into(),
            held: *status == ShareStatus::Held,
        })
        .collect();
    let held = v.shareholders.iter().filter(|(_, s)| *s == ShareStatus::Held).count();
    p.set_parts_line(format!("{held}/{}", v.participants).into());
    p.set_seats(ModelRc::new(VecModel::from(seats)));
    p.set_network(v.network.as_str().into());
    p.set_height_line(format!("{} / {}", v.scan_height, v.daemon_height).into());
    let fault = match v.scan_paused.as_deref() {
        Some("update needed") => l.wl_paused_update,
        Some(_) => l.wl_node_fault,
        None if v.phase == WalletPhase::Ready && !v.connected => l.wl_node_offline,
        None => "",
    };
    p.set_fault_line(fault.into());
    apply_run(ui, lang, ws, v);
}

/// The run's one bar; a new run or a new consent question brings the
/// panel back (W10).
fn apply_run(ui: &AppWindow, lang: i32, ws: &str, v: &WalletView) {
    let p = ui.global::<Purse>();
    // the edges below compare against this republic's last state, never another's
    if p.get_workspace() != ws {
        p.set_workspace(ws.into());
        p.set_run_active(false);
        p.set_run_aborted(false);
        p.set_needs_consent(false);
        p.set_run_finished(false);
        p.set_panel_dismissed(false);
    }
    let (was_active, was_aborted, was_asking) = (p.get_run_active(), p.get_run_aborted(), p.get_needs_consent());
    let run = v.run.as_ref().filter(|_| v.phase != WalletPhase::Ready);
    let can_start = run.is_none() && v.can_start;
    let line = match run {
        Some(r) => stage_line(lang, r),
        None if can_start => StageLine { text: lex(lang).wl_not_running.to_string(), ..StageLine::default() },
        None => StageLine { text: lex(lang).wl_waiting.to_string(), ..StageLine::default() },
    };
    let active = run.is_some_and(|r| r.stage != RunStage::Done);
    let asking = run.is_some_and(|r| r.needs_consent);
    if (active && !line.aborted && (!was_active || was_aborted)) || (asking && !was_asking) {
        p.set_panel_dismissed(false);
    }
    p.set_run_finished(v.phase == WalletPhase::Ready && (was_active || p.get_run_finished()));
    p.set_run_active(active);
    p.set_run_line(line.text.into());
    p.set_run_missing(line.missing.into());
    p.set_run_progress(line.progress);
    p.set_run_aborted(line.aborted);
    p.set_can_start(can_start);
    p.set_needs_consent(asking);
    let hint = run.map(|r| r.daemon_hint.as_str()).unwrap_or_default();
    if !hint.is_empty() && p.get_node().is_empty() && p.get_node_draft().is_empty() {
        p.set_node_draft(hint.into());
    }
}

/// The configured node, mirrored live from the settings.
pub(crate) fn apply_node(ui: &AppWindow, lang: i32, settings: &molt_core::SessionSettings) {
    let p = ui.global::<Purse>();
    let url = settings.wallet_daemon_url.as_str();
    p.set_node(url.into());
    p.set_node_line(if url.is_empty() { lex(lang).wl_no_node } else { url }.into());
}

/// What a typed node leads to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NodeStep {
    /// Not a node address: the line under the field.
    Bad(String),
    /// An onion: stored outright.
    Take(String),
    /// Clearnet (1) or local (2): waits for the relay-style acknowledgement.
    Confirm(String, i32),
}

/// Classify a typed node by the relay host rule.
pub(crate) fn node_step(lang: i32, url: &str) -> NodeStep {
    match molt_core::relay::daemon_kind(url.trim()) {
        Err(_) => NodeStep::Bad(lex(lang).wl_bad_url.to_string()),
        Ok((url, RelayKind::Onion)) => NodeStep::Take(url),
        Ok((url, RelayKind::Local)) => NodeStep::Confirm(url, 2),
        Ok((url, _)) => NodeStep::Confirm(url, 1),
    }
}

/// Storing a node: confirmed, and a non-onion one also switches
/// non-onion dialing on, as a confirmed relay does.
pub(crate) fn node_commands(url: &str, outside_tor: bool) -> Vec<Command> {
    let patch = serde_json::json!({ "wallet_daemon_url": url, "wallet_daemon_confirmed": true });
    let mut cmds = vec![Command::PatchSettings { patch }];
    if outside_tor {
        cmds.push(Command::RelayClearnetSession { unlock: true });
    }
    cmds
}

/// The purse's buttons: the four seat tools an MCP agent drives.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PurseAct {
    SetUp,
    Consent(bool),
    Retry,
    AcknowledgeLoss,
}

impl PurseAct {
    pub(crate) fn command(self) -> Command {
        match self {
            PurseAct::SetUp => Command::WalletInit,
            PurseAct::Consent(accept) => Command::WalletConsent { accept },
            PurseAct::Retry => Command::WalletRetry,
            PurseAct::AcknowledgeLoss => Command::WalletAcknowledgeLoss,
        }
    }
}

/// What consenting to the purse means (design §3.3), one line each.
pub(crate) fn consent_notes(lang: i32) -> String {
    let l = lex(lang);
    [l.wl_note_sees, l.wl_note_lost, l.wl_note_spend].join("\n")
}

/// W5: the set-aside acts on the open republic only, so the dialog asks
/// once per damage of the open one's key part file.
pub(crate) fn apply_loss(ui: &AppWindow, sv: &molt_core::SessionView) {
    let p = ui.global::<Purse>();
    let damaged = molt_storage::StorageError::WalletKeysDamaged.to_string();
    let open = &sv.active_workspace;
    let hit = !open.is_empty() && sv.workspaces.iter().any(|w| &w.id == open && w.backup_error == damaged);
    if !hit {
        p.set_loss_ws("".into());
    } else if p.get_loss_ws() != open.as_str() {
        p.set_loss_ws(open.as_str().into());
        p.set_loss_open(true);
    }
}

/// The closed republics a damaged key part file holds, by name.
pub(crate) fn damaged_elsewhere(sv: &molt_core::SessionView) -> Vec<String> {
    let damaged = molt_storage::StorageError::WalletKeysDamaged.to_string();
    sv.workspaces
        .iter()
        .filter(|w| w.id != sv.active_workspace && w.backup_error == damaged)
        .map(|w| w.name.clone())
        .collect()
}
