// SPDX-License-Identifier: GPL-3.0-or-later
//! The live mirror: every engine change re-reads the shared session and
//! the surfaces and pushes them into the window's properties and models
//! (rows patched in place). Nothing here holds state of its own - the
//! GUI renders what the engine holds, co-equal with an MCP operator.

use std::sync::{Arc, Mutex};

use molt_core::relay::{RelayBlock, RelayKind, RelayStatus};
use molt_core::{Command, Event, Reply, SessionScope, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::broadcast::error::RecvError;

use crate::alerts::alert_unless_own;
use crate::app::Ctx;
use crate::i18n::{
    apply_strings, localize_headline, localize_log_line, localize_net_reason,
    localize_recover_failed, localize_recover_note, localize_s3_verdict,
};
use crate::images::{image_from_bytes, logo_needs_reload, AVATARS, LOGO_KEY};
use crate::labels::{
    backup_when_label, charter_columns, file_size_label, genesis_undelivered_copy, from_screen,
    never_seen_label, orphan_remote_label, seat_state_label, seen_label, short_hex_id, size_label,
    strings_founder, sync_status_label, theme_index, unix_now, view_icon, view_label,
};
use crate::models::{sync_model, sync_rows, sync_strings};
use crate::net_tor::{net_health_pill, tor_test_detail, tor_test_tone, tor_verdict_copy_for};
use crate::settings::{apply_settings_fields, settings_draft_differs};
use crate::surfaces::{
    chain_row, gather_surfaces, page_of, page_slice, to_decided_row, to_proposal_row, ChatUiState,
    SurfacesBundle,
    LIST_PAGE_SIZE,
};
use crate::wiki_bridge::{patch_view_sync, wiki_export_toast};
use crate::{
    AppScreen, AppWindow, BackupRow, ChainRow, ChannelItem, LogLine, MemberRow, MemberSync,
    PatchView, Poke, ProposalRow, ReactionItem, ReceiptItem, RecoverSeatRow, RelayItem, RelayPick,
    RitualSeat, Strings, SurfaceTab, Theme, UploadRow, ViewItem, WikiBase, WikiState,
    WorkspaceItem,
};

/// Map a session workspace into the Slint-side row struct. Member chips
/// render the relative age from the real stamp; a never-seen member (all
/// of them, on a closed workspace) shows a bare chip.
fn workspace_item(lang: i32, now: u64, w: &molt_core::WorkspaceInfo) -> WorkspaceItem {
    let members: Vec<MemberSync> = w
        .members
        .iter()
        .map(|m| MemberSync {
            name: m.name.as_str().into(),
            last: seen_label(lang, now, m.last_seen, "").into(),
            state: i32::from(m.state),
        })
        .collect();
    WorkspaceItem {
        id: w.id.as_str().into(),
        name: w.name.as_str().into(),
        detail: w.detail.as_str().into(),
        status: sync_status_label(lang, w.state, w.last_sync_min, w.sync_queue).into(),
        synced: w.synced,
        state: i32::from(w.state),
        last_sync_min: w.last_sync_min as i32,
        s3: w.s3,
        backup: backup_when_label(lang, w.last_backup_min).into(),
        encrypted: w.encrypted,
        seed: w.seed.as_str().into(),
        net: w.net.as_str().into(),
        members: ModelRc::new(VecModel::from(members)),
    }
}

/// The settings backup table: every local workspace mapped to its bucket
/// backup (if auto-backup is on), then the bucket-only orphans from the
/// last real listing (none until one ran).
pub(crate) fn backup_rows(sv: &SessionView) -> Vec<BackupRow> {
    let lang = i32::from(sv.language == "de");
    // machine sort key for the last-backup column ("never" sorts last)
    fn last_key(min: u32) -> i32 {
        if min == molt_core::WorkspaceInfo::NEVER {
            i32::MAX
        } else {
            i32::try_from(min).unwrap_or(i32::MAX)
        }
    }
    let mut rows: Vec<BackupRow> = sv
        .workspaces
        .iter()
        .map(|w| BackupRow {
            id: w.id.as_str().into(),
            local: w.name.as_str().into(),
            // the bucket-side cell is honest: the last backup attempt's
            // real failure, else what the last real listing saw in the
            // bucket (copies × the id pseudonym — the bucket stores no
            // names), else nothing. The auto-toggle alone claims nothing.
            remote: if !w.backup_error.is_empty() {
                w.backup_error.clone()
            } else if w.backup_copies > 0 {
                format!("{}\u{00d7} \u{00b7} {}", w.backup_copies, short_hex_id(&w.id))
            } else {
                String::new()
            }
            .into(),
            has_local: true,
            auto: w.s3,
            size: size_label(w.size_kib).into(),
            last: backup_when_label(lang, w.last_backup_min).into(),
            size_kib: i32::try_from(w.size_kib).unwrap_or(i32::MAX),
            last_min: last_key(w.last_backup_min),
        })
        .collect();
    rows.extend(sv.backup_orphans.iter().map(|o| BackupRow {
        // the FULL workspace-id pseudonym rides along (the label below is
        // shortened): restore-from-S3 starts from exactly this id
        id: o.id.as_str().into(),
        local: "".into(),
        remote: orphan_remote_label(o).into(),
        has_local: false,
        auto: false,
        size: size_label(o.size_kib).into(),
        last: backup_when_label(lang, o.last_backup_min).into(),
        size_kib: i32::try_from(o.size_kib).unwrap_or(i32::MAX),
        last_min: last_key(o.last_backup_min),
    }));
    rows
}

/// Sort the backup table by a header column ("local"/"remote"/"auto"/
/// "size"/"last"); an empty key keeps the session order. Empty name cells
/// and never-backed-up rows sort last (ascending).
pub(crate) fn sort_bk_rows(items: &mut [BackupRow], key: &str, desc: bool) {
    match key {
        "local" => items.sort_by_key(|r| (r.local.is_empty(), r.local.to_lowercase())),
        "remote" => items.sort_by_key(|r| (r.remote.is_empty(), r.remote.to_lowercase())),
        "auto" => items.sort_by_key(|r| !r.auto),
        "size" => items.sort_by_key(|r| r.size_kib),
        "last" => items.sort_by_key(|r| r.last_min),
        _ => return,
    }
    if desc {
        items.reverse();
    }
}

/// Sort the Open screen's workspace rows by a header column ("name" or
/// "sync"); an empty key keeps the session order. Ascending "sync" puts the
/// most recently synced republic first.
pub(crate) fn sort_ws_items(items: &mut [WorkspaceItem], key: &str, desc: bool) {
    match key {
        "name" => items.sort_by_key(|w| w.name.to_lowercase()),
        "sync" => items.sort_by_key(|w| w.last_sync_min),
        _ => return,
    }
    if desc {
        items.reverse();
    }
}

/// The recovery-flow reading of the transient session notice — the engine's
/// contract for the recovery ritual (`recovery_ritual.md`): a coordinator's
/// mint lifecycle (pending → link | failed), and the rejoiner's
/// started/failed/done lifecycle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoverNotice {
    /// Not a recovery notice (every other notice, e.g. "saved").
    None,
    /// Rejoiner: the recovery task's live status line (a bounded wait must
    /// not look dead — `NetRecoverNote`).
    Note(String),
    /// Coordinator: a link mint started for this member — the dialog opens in
    /// its calm pending state until the outcome notice replaces it.
    LinkPending(String),
    /// Coordinator: the engine minted a single-use `molt://recover/…` link.
    Link(String),
    /// Coordinator: the mint failed for an operational reason of THIS node —
    /// `mesh-not-running` on the legacy queue shape, or (on Nostr) a relay
    /// reason naming the missing piece. The returning member's presence is
    /// never involved. Rendered as the calm failed state of the same link
    /// dialog, not as an error toast.
    LinkFailed(String),
    /// Rejoiner: the engine accepted link + phrase; the rejoin runs off the
    /// actor (it can span the survivors' human approval).
    Started(String),
    /// Rejoiner: the rejoin failed with this reason (retry = a fresh start,
    /// usually with a freshly minted link).
    Failed(String),
    /// Rejoiner: the seat is recovered — the engine flips to Main itself.
    Done(String),
    /// Coordinator: a returning member's request arrived and was REFUSED
    /// (`member:reason` — e.g. the R5 relay gate naming the relay to add).
    /// Without this the coordinator stares at a silent screen while the
    /// rejoiner waits out its timeout.
    Refused(String),
}

/// Split a session notice into its recovery reading (verbatim payload —
/// an error may itself contain colons).
pub(crate) fn parse_recover_notice(notice: &str) -> RecoverNotice {
    if let Some(member) = notice.strip_prefix("recovery-link-pending:") {
        RecoverNotice::LinkPending(member.to_string())
    } else if let Some(reason) = notice.strip_prefix("recovery-link-failed:") {
        RecoverNotice::LinkFailed(reason.to_string())
    } else if let Some(link) = notice.strip_prefix("recovery-link:") {
        RecoverNotice::Link(link.to_string())
    } else if let Some(member) = notice.strip_prefix("recover-started:") {
        RecoverNotice::Started(member.to_string())
    } else if let Some(line) = notice.strip_prefix("recover-note:") {
        RecoverNotice::Note(line.to_string())
    } else if let Some(error) = notice.strip_prefix("recover-failed:") {
        RecoverNotice::Failed(error.to_string())
    } else if let Some(member) = notice.strip_prefix("recovered:") {
        RecoverNotice::Done(member.to_string())
    } else if let Some(rest) = notice.strip_prefix("recover-refused:") {
        RecoverNotice::Refused(rest.to_string())
    } else {
        RecoverNotice::None
    }
}

/// Copy one run log into its Slint model, localized line-wise (E5). The
/// TONE model keeps reading the engine lines — the glyph survives
/// localization (pinned) — so both stay in step.
fn sync_log_localized(
    lang: i32,
    current: &ModelRc<slint::SharedString>,
    items: &[String],
    set: impl FnOnce(ModelRc<slint::SharedString>),
) {
    sync_rows(
        current,
        items.iter().map(|l| localize_log_line(lang, l).into()).collect(),
        set,
    );
}

/// Perform one requested GUI interaction (`gui_over_mcp.md`): domain
/// verbs mapped onto the SAME Slint callbacks a human's click invokes,
/// so the action surface cannot drift from what a person can do. An
/// unknown verb is a logged no-op — the next snapshot's generation still
/// answers the caller.
fn perform_ui_action(ui: &AppWindow, action: &molt_core::UiAction) {
    let s = |k: &str| {
        action
            .args
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match action.verb.as_str() {
        "select_view" => ui.invoke_select_view(s("surface").into(), s("view").into()),
        "select_channel" => ui.invoke_select_channel(s("channel").into()),
        "open_workspace" => ui.invoke_open_workspace(s("id").into()),
        "close_workspace" => ui.invoke_close_workspace(),
        "chat_send" => ui.invoke_send_chat(s("body").into(), String::new().into()),
        other => {
            tracing::warn!(verb = %other, "ui_action: unknown verb — performed as a no-op");
        }
    }
}

/// The GUI's monotone publish counter (`gui_over_mcp.md`): bumped per
/// snapshot so an agent can await "my action landed" by polling
/// `read_ui_state` for a larger generation.
static UI_PUBLISH_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the window ACTUALLY holds right now (`gui_over_mcp.md`): read
/// from the UI models/properties on the UI thread — deliberately not from
/// the engine bundle, because "does the pane hold what the engine holds"
/// is exactly the question this exists to answer.
pub(crate) fn build_ui_snapshot(ui: &AppWindow) -> molt_core::UiSnapshot {
    use slint::Model as _;
    let screen = match ui.get_screen() {
        AppScreen::Choice => "choice",
        AppScreen::Create => "create",
        AppScreen::Open => "open",
        AppScreen::Join => "join",
        AppScreen::Restore => "restore",
        AppScreen::Settings => "settings",
        AppScreen::Main => "main",
    };
    let surfaces = ui.get_surfaces();
    let chat = surfaces.iter().find(|s| s.key == "chat");
    let (chat_rows, chat_last) = chat
        .map(|s| {
            let n = s.log.row_count();
            let last = (n.saturating_sub(3)..n)
                .filter_map(|i| s.log.row_data(i))
                .map(|l| l.text.to_string())
                .collect();
            (u32::try_from(n).unwrap_or(u32::MAX), last)
        })
        .unwrap_or((0, Vec::new()));
    let wizard = match screen {
        "create" => format!("create:{}", ui.get_cw_step()),
        "join" => format!("join:{}", ui.get_jw_step()),
        _ => String::new(),
    };
    molt_core::UiSnapshot {
        screen: screen.to_string(),
        surface: ui.get_selected_surface().to_string(),
        view: ui.get_selected_view().to_string(),
        channel: ui.get_selected_channel().to_string(),
        chat_rows,
        chat_last,
        compose_visible: screen == "main" && ui.get_selected_surface() == "chat",
        chat_in_view: ui.get_chat_log_in_view(),
        nav: surfaces.iter().map(|s| s.key.to_string()).collect(),
        pending_count: surfaces
            .iter()
            .map(|s| u32::try_from(s.pending_count.max(0)).unwrap_or(0))
            .sum(),
        wizard,
        toast: ui.get_toast_text().to_string(),
        generation: UI_PUBLISH_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
    }
}

/// Build the snapshot on the UI thread and hand it to the engine — the
/// publish half of `gui_over_mcp.md`, run at the end of every mirror pass
/// (what it publishes is what it just rendered).
async fn publish_ui_state(wallet: &WalletHandle, weak: &slint::Weak<AppWindow>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = tx.send(build_ui_snapshot(&ui));
        }
    });
    if let Ok(snapshot) = rx.await {
        let _ = wallet.execute(Command::UiPublish { snapshot }).await;
    }
}

/// Read the shared session and push it into the Slint properties on the UI
/// thread. `last_settings` remembers the previously applied settings so the
/// draft form is only refreshed when they really changed.
pub(crate) async fn push_session(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    last_settings: &Arc<Mutex<Option<SessionSettings>>>,
    scope: SessionScope,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let chat_ui_for_apply = chat_ui.clone();
    let sv = match wallet.execute(Command::ReadSession).await {
        Ok(Reply::Session(sv)) => *sv,
        _ => return,
    };
    // a run tick only advanced one lifecycle — repaint just that mirror
    if scope != SessionScope::Full {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_runs(&ui, &sv);
            }
        });
        return;
    }
    // THE workspace switch, and the only place it happens. It used to ride
    // `begin_push`, keyed on whatever session copy that push had read — so a
    // push that read the session BEFORE an open could re-enter the state as
    // "no workspace" AFTER the open, bump the epoch past the good push, and
    // land its own empty bundle. That is what showed an empty chat on the
    // first open, until switching surfaces forced a fresh one.
    //
    // Here it is ordered by the event stream instead: the session mirror
    // runs on every SessionChanged, before the surfaces do.
    {
        let mut st = match chat_ui.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = st.workspace.clone();
        st.enter_workspace(&sv.active_workspace);
        if before != st.workspace {
            tracing::debug!(from = %before, to = %st.workspace, gen = st.generation, "ui: workspace switch");
        }
    }
    let (changed, prev) = {
        // POISON-TOLERANT, like every other lock site here. A panic in some
        // other callback must not stop the live mirror for the rest of the
        // session: this cache only decides whether the settings FORM is
        // refreshed, and reading a possibly-stale copy is a redraw, while
        // panicking here ends the task that redraws anything at all.
        let mut last = match last_settings.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prev = last.clone();
        let changed = prev.as_ref() != Some(&sv.settings);
        if changed {
            *last = Some(sv.settings.clone());
        }
        (changed, prev)
    };
    let weak2 = weak.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            // Draft protection: a settings change arriving while the user has
            // unsaved edits open (an external config reload, an MCP save) must
            // not wipe what they typed. The notice/leave-guard tells them; the
            // draft stays until they Save or discard.
            let editing = ui.get_screen() == AppScreen::Settings
                && prev.is_some_and(|p| settings_draft_differs(&p, &ui));
            apply_session(&ui, &sv, changed && !editing, &chat_ui_for_apply);
        }
    });
    // gui_over_mcp.md: publish what this pass rendered (queued behind the
    // apply above on the event loop, so the claim is post-render)
    publish_ui_state(wallet, &weak2).await;
}

/// Render one session snapshot into the window: screen, language (and strings),
/// the transient notice, the last-wizard outcome, and the settings draft. The
/// draft fields are only overwritten when the session's settings actually
/// changed — otherwise an unrelated change (language, theme, navigation)
/// would wipe what the user is typing in the settings form.
pub(crate) fn apply_session(
    ui: &AppWindow,
    sv: &SessionView,
    settings_changed: bool,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let lang = i32::from(sv.language == "de");
    ui.set_screen(from_screen(sv.screen));
    ui.set_selected_surface(sv.surface.as_str().into());
    ui.set_selected_view(sv.view.as_str().into());
    // What the poke menus gate on: the APPLIED value, never the settings
    // draft. A ticked-but-unsaved checkbox would otherwise offer a menu whose
    // command the engine refuses (and an unticked one would hide a menu that
    // still works).
    ui.global::<Poke>().set_on(sv.settings.poke_enabled);

    // the Open screen's list mirrors the session's workspaces, re-applying
    // whatever column sort the user picked
    let now = unix_now();
    let mut items: Vec<WorkspaceItem> = sv
        .workspaces
        .iter()
        .map(|w| workspace_item(lang, now, w))
        .collect();
    sort_ws_items(
        &mut items,
        ui.get_ws_sort_key().as_str(),
        ui.get_ws_sort_desc(),
    );
    sync_rows(&ui.get_ws_list(), items, |m| ui.set_ws_list(m));

    // the settings backup tab mirrors the same workspaces as a local↔bucket
    // mapping table
    let mut bk = backup_rows(sv);
    sort_bk_rows(
        &mut bk,
        ui.get_bk_sort_key().as_str(),
        ui.get_bk_sort_desc(),
    );
    sync_rows(&ui.get_bk_rows(), bk, |m| ui.set_bk_rows(m));

    // the main header shows the active workspace (+ its sync status); the
    // chat's members strip mirrors the active workspace's roster/presence.
    // `active_workspace` is an id; the header wants the display name.
    let active = sv.workspaces.iter().find(|w| w.id == sv.active_workspace);
    ui.set_active_workspace(active.map(|w| w.name.as_str()).unwrap_or_default().into());
    // drafts are PER WORKSPACE: entering another republic resets the wiki
    // model and loads that workspace's stored draft (WP-D)
    {
        let g = ui.global::<WikiState>();
        let ws: slint::SharedString = sv.active_workspace.as_str().into();
        if g.get_ws_id() != ws {
            g.set_ws_id(ws);
            g.invoke_workspace_changed();
        }
    }
    let (a_state, a_status) = active
        .map(|w| {
            (
                i32::from(w.state),
                sync_status_label(lang, w.state, w.last_sync_min, w.sync_queue),
            )
        })
        .unwrap_or((0, String::new()));
    ui.set_active_state(a_state);
    ui.set_active_status(a_status.into());
    // the ratified founding charter for the Constitution surface (empty until a
    // deliberated workspace is open) — plus its balanced display columns
    let agenda = active.map(|w| w.agenda.as_str()).unwrap_or_default();
    ui.set_active_agenda(agenda.into());
    let cols: Vec<slint::SharedString> = charter_columns(agenda, 3)
        .into_iter()
        .map(slint::SharedString::from)
        .collect();
    ui.set_charter_cols(ModelRc::new(VecModel::from(cols)));
    let roster: Vec<MemberSync> = active
        .map(|w| {
            w.members
                .iter()
                .map(|m| MemberSync {
                    name: m.name.as_str().into(),
                    last: seen_label(lang, now, m.last_seen, never_seen_label(lang)).into(),
                    state: i32::from(m.state),
                })
                .collect()
        })
        .unwrap_or_default();
    sync_rows(&ui.get_active_members(), roster, |m| {
        ui.set_active_members(m)
    });
    // Organization → Status/Members: reachable count from the roster (a member
    // is reachable unless offline, state 2)
    let total = active.map(|w| w.members.len()).unwrap_or(0);
    let online = active
        .map(|w| w.members.iter().filter(|m| m.state != 2).count())
        .unwrap_or(0);
    ui.set_org_online(i32::try_from(online).unwrap_or(0));
    ui.set_org_total(i32::try_from(total).unwrap_or(0));

    // the create wizard's folder preview roots at the configured
    // workspace dir (shown raw, as configured — `~` and all)
    ui.set_cw_dir(sv.settings.workspace_dir.clone().into());

    // the manual-export status (Open-detail note): honest mirror of the
    // engine's export state — running / real success / real failure
    {
        let s = ui.global::<Strings>();
        let ex = &sv.export;
        let (note, failed) = if ex.running {
            (format!("{} {}", s.get_ow_export_running(), ex.dest), false)
        } else if ex.result == "ok" {
            let mut note =
                format!("{} {} ({})", s.get_ow_export_note(), ex.dest, file_size_label(ex.bytes));
            if !ex.skipped.is_empty() {
                note.push_str(&format!(
                    " - {} {}",
                    s.get_ow_export_skipped(),
                    ex.skipped.join(", ")
                ));
            }
            (note, false)
        } else if let Some(err) = ex.result.strip_prefix("error: ") {
            (format!("{} {}", s.get_ow_export_failed(), err), true)
        } else {
            (String::new(), false)
        };
        ui.set_export_note(note.into());
        ui.set_export_failed(failed);
        ui.set_export_ws(ex.workspace.as_str().into());
    }

    // the wiki export's honest outcome, EDGE-triggered: the session
    // re-pushes unchanged, so a toast per push would repeat the same result
    // forever. A started export clears the mark, which is what lets the very
    // same export toast a second time.
    {
        let ex = &sv.wiki_export;
        let mark = if ex.running {
            String::new()
        } else {
            format!("{}|{}|{}|{}", ex.dest, ex.result, ex.files, ex.bytes)
        };
        if ui.get_wiki_export_seen() != mark.as_str() {
            ui.set_wiki_export_seen(mark.as_str().into());
            if let Some((msg, failed)) = wiki_export_toast(lang, ex) {
                if failed {
                    ui.invoke_show_toast_error(msg.into());
                } else {
                    ui.invoke_show_toast(msg.into());
                }
            }
        }
    }

    apply_runs(ui, sv);
    ui.global::<Theme>().set_theme_index(theme_index(&sv.theme));
    ui.global::<Theme>()
        .set_fs_app(f32::from(sv.settings.font_app));
    ui.global::<Theme>()
        .set_fs_nav(f32::from(sv.settings.font_nav));
    ui.global::<Theme>()
        .set_fs_editor(f32::from(sv.settings.font_editor));
    ui.set_lang_index(lang);
    ui.set_notice(sv.notice.clone().into());
    // what §10.7 file gating and the §6.5 coarse-presence hint key on
    ui.set_session_transport(sv.transport.as_str().into());
    // a failed write carries its detail in the notice; split it off so the
    // settings footer can render it in the error tone without string ops
    ui.set_notice_failed(
        match sv.notice.strip_prefix("save-failed: ") {
            Some(d) if lang == 1 => format!("Speichern fehlgeschlagen: {d}"),
            _ if sv.notice.starts_with("save-failed") => sv.notice.clone(),
            _ => String::new(),
        }
        .into(),
    );
    // The recovery flow rides the same transient notice, but EDGE-triggered:
    // the notice lingers in the session until something replaces it, and the
    // link dialog / rejoin state must react once per NEW notice — a re-push of
    // an unchanged session must not re-open a dismissed link dialog. The
    // last-handled notice is mirrored into the window (like the settings
    // draft cache) and compared before acting.
    if ui.get_recover_notice_seen() != sv.notice.as_str() {
        ui.set_recover_notice_seen(sv.notice.clone().into());
        // S7: the fetch outcome rides the notice — the backup tab shows it
        if let Some(fetched) = sv.notice.strip_prefix("backup-fetched:") {
            ui.set_bk_fetched(fetched.into());
            ui.set_bk_fetch_error("".into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-fetch-failed:") {
            ui.set_bk_fetch_error(err.into());
        }
        match parse_recover_notice(&sv.notice) {
            RecoverNotice::LinkPending(member) => {
                // coordinator: a mint attempt started — open the dialog in
                // its calm pending state (the outcome notice fills it in)
                ui.set_recover_link_member(member.into());
                ui.set_recovery_link("".into());
                ui.set_recovery_link_error("".into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::Link(link) => {
                // coordinator: present the freshly minted single-use link
                ui.set_recovery_link(link.into());
                ui.set_recovery_link_error("".into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::LinkFailed(reason) => {
                // coordinator: the mint failed for an operational reason of
                // THIS node — same dialog, calm failed state (never a toast;
                // the returning member's presence is irrelevant to a mint)
                ui.set_recovery_link("".into());
                ui.set_recovery_link_error(reason.into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::Started(member) => {
                ui.set_rv_member(member.into());
                ui.set_rv_running(true);
                ui.set_rv_error("".into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Failed(error) => {
                ui.set_rv_running(false);
                ui.set_rv_error(localize_recover_failed(lang, &error).into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Note(line) => {
                ui.set_rv_note(localize_recover_note(lang, &line).into());
            }
            RecoverNotice::Done(_) => {
                // the engine flips to Main itself — just clear the peer-way
                // state so a later return to the Restore screen starts clean
                ui.set_rv_running(false);
                ui.set_rv_error("".into());
                ui.set_rv_note("".into());
            }
            RecoverNotice::Refused(what) => {
                // coordinator: the request was refused — same link dialog,
                // loud error slot (the reason names what to fix, e.g. the
                // relay the pool must gain)
                ui.set_recovery_link_error(what.into());
                ui.set_recover_link_open(true);
            }
            RecoverNotice::None => {}
        }
        // the same edge-triggered channel carries the backup/restore
        // honesty notices (story 12/13): toast them once per NEW notice
        let s = ui.global::<Strings>();
        if sv.notice == "detached" {
            // §4.4: knowledge restored, membership not — say exactly that
            ui.invoke_show_toast_error(s.get_toast_detached());
        } else if sv.notice == "reattaching" {
            // detached_reattach.md: the seat announces itself — a calm
            // in-progress note, not an error
            ui.invoke_show_toast(s.get_toast_reattaching());
        } else if let Some(err) = sv.notice.strip_prefix("backup-failed:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_failed()).into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-prune-failed:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_prune()).into());
        } else if let Some(err) = sv.notice.strip_prefix("backup-quota:") {
            ui.invoke_show_toast_error(format!("{} {err}", s.get_toast_backup_quota()).into());
        } else if let Some(rest) = sv.notice.strip_prefix("relay-refused:") {
            // B4: the probe's one-line verdict — the entry stays unconfirmed
            ui.invoke_show_toast_error(format!("{} {rest}", s.get_toast_relay_refused()).into());
        } else if let Some(rest) = sv.notice.strip_prefix("relay-unverified:") {
            // …and the honest middle class: confirmed on the operator's
            // consent, but the relay could not be judged right now
            ui.invoke_show_toast(format!("{} {rest}", s.get_toast_relay_unverified()).into());
        } else if let Some(err) = sv.notice.strip_prefix("genesis-undelivered:") {
            // The republic EXISTS here but its members were never told: the
            // genesis 445 reached no relay. Copy lives Rust-side (the
            // tor_verdict_copy_for precedent) so this needs no Strings entry
            // and no ~6 GiB molt-ui-window rebuild. Without a surface the
            // engine's notice would itself be an inert seam — the exact sin
            // this fix is about.
            ui.invoke_show_toast_error(format!("{} {err}", genesis_undelivered_copy(lang)).into());
        }
    }
    // persistent restart warning: which changed keys only apply on restart
    ui.set_restart_keys(sv.restart_required.join(", ").into());
    // the S3 test status is transient and lives outside the settings draft,
    // so push it on every update — even while the user has an unsaved field
    // open and `settings_changed` is suppressed
    ui.set_cfg_s3_test(localize_s3_verdict(lang, &sv.s3_test).into());
    ui.set_cfg_media_s3_test(localize_s3_verdict(lang, &sv.s3_media_test).into());
    ui.set_cfg_bk_list(localize_s3_verdict(lang, &sv.s3_list).into());
    // …and the same for the Tor probe's verdict. The rung's key drives the
    // "testing" affordance, the sentence and its tone come from Rust so the
    // "only a proven circuit is green" rule stays a tested statement, and the
    // technical specifics ride along untranslated.
    ui.set_cfg_tor_test(sv.tor_test.state.as_str().into());
    // a confirmed-but-switched-off pool is a DIFFERENT problem from an empty
    // one, and telling the user to "confirm a relay" they already confirmed
    // is advice that cannot help (review finding). The classification is
    // molt-core's single `pool_gap` — this used to hand-roll the same
    // predicate, which is how three copies of it came to exist.
    // the create wizard's relay picker: what this node can dial, and the
    // founder's choice among it. Refreshed from the session so a relay
    // confirmed while the wizard is open shows up.
    {
        let dialable = molt_core::relay::dialable(&sv.settings.relays, sv.clearnet_session);
        if let Ok(mut st) = chat_ui.lock() {
            st.set_create_relays(dialable);
            let rows: Vec<RelayPick> = st
                .create_pick_rows()
                .into_iter()
                .map(|(url, picked)| RelayPick { url: url.into(), picked })
                .collect();
            ui.set_cw_relay_picks(slint::ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    let session_locked = molt_core::relay::pool_gap(&sv.settings.relays, sv.clearnet_session)
        == Some(molt_core::relay::PoolGap::NonOnionOff);
    ui.set_cfg_tor_test_text(
        tor_verdict_copy_for(lang, sv.tor_test.state, session_locked).into(),
    );
    ui.set_cfg_tor_test_tone(tor_test_tone(sv.tor_test.state));
    ui.set_cfg_tor_test_detail(tor_test_detail(lang, &sv.tor_test).into());

    // transport health for the header "chat" pill: tone (green/amber/red) plus
    // the engine's reason string as the hover tooltip (P6). Pushed on every
    // update so a dial outcome repaints the pill regardless of settings edits.
    let (net_tone, net_reason) = net_health_pill(&sv.net_health);
    ui.set_net_health_tone(net_tone);
    ui.set_net_health_reason(localize_net_reason(lang, &net_reason).into());

    // the create screen's read-only "Network" line: the EFFECTIVE global
    // anonymity network. NOT a draft field (the user never types it), so it
    // is pushed on every update — inside the settings_changed guard a GUI
    // save would leave it stale (the draft-protection `editing` flag
    // suppresses the mirror exactly then)
    ui.set_cw_net(molt_core::effective_net_label(&sv.settings.anonymity).into());

    // the relay pool is edited LIVE through the Relay* commands, so it is not
    // a draft field: push it on every update, even while an unsaved form edit
    // suppresses the draft mirror
    apply_relays(ui, sv);

    if !settings_changed {
        apply_strings(ui, lang);
        return;
    }
    apply_settings_fields(ui, &sv.settings);

    apply_strings(ui, lang);
}

/// The relay pool as the Network panel renders it: the entries in priority
/// order, each carrying the ENGINE's derived verdict. The GUI never
/// re-evaluates the dial policy — it turns `blocked` into words, nothing more
/// (`docs_archive/transport/relay_pool.md` §3).
pub(crate) fn relay_rows(relays: &[RelayStatus]) -> Vec<RelayItem> {
    let last = relays.len().saturating_sub(1);
    relays
        .iter()
        .enumerate()
        .map(|(i, r)| RelayItem {
            url: r.url.as_str().into(),
            kind: match r.kind {
                RelayKind::Onion => 0,
                RelayKind::Clearnet => 1,
                RelayKind::Local => 2,
            },
            confirmed: r.confirmed,
            blocked: match r.blocked {
                None => 0,
                Some(RelayBlock::Unconfirmed) => 1,
                Some(RelayBlock::ClearnetSessionLocked) => 2,
            },
            pos: i32::try_from(i.saturating_add(1)).unwrap_or(i32::MAX),
            first: i == 0,
            last: i == last,
        })
        .collect()
}

/// Mirror the relay pool into Settings → Nostr relays: the rows, how many relays
/// are dialable right now (0 = this node is connected to nothing), whether a
/// confirmed clearnet relay exists at all (only then is there anything for
/// the session activation to unlock), and the session unlock itself.
fn apply_relays(ui: &AppWindow, sv: &SessionView) {
    sync_rows(&ui.get_relay_rows(), relay_rows(&sv.relays), |m| {
        ui.set_relay_rows(m);
    });
    let dialable = sv.relays.iter().filter(|r| r.blocked.is_none()).count();
    ui.set_relay_dialable(i32::try_from(dialable).unwrap_or(i32::MAX));
    // the session toggle concerns everything reached outside Tor: clearnet
    // AND local relays (§10.14) share the one per-session activation
    ui.set_relay_clearnet_confirmed(
        sv.relays
            .iter()
            .any(|r| r.confirmed && r.kind != RelayKind::Onion),
    );
    ui.set_cfg_clearnet_session(sv.clearnet_session);
    // the clearnet warning tailors its sentence to the SAVED anonymity
    // setting — never to the unsaved dropdown draft, which would promise a
    // Tor circuit the node does not have yet
    ui.set_net_tor_active(sv.settings.anonymity == "tor");
}

/// Mirror the three engine-run lifecycles (the engine ticks them at 90 ms;
/// a `SessionChanged` with a run scope re-renders ONLY this, so the rest of
/// the window keeps its focus/scroll state untouched).
fn apply_runs(ui: &AppWindow, sv: &SessionView) {
    let lang = i32::from(sv.language == "de");
    // restore
    ui.set_rw_step(i32::from(sv.restore.run.step));
    ui.set_rw_way(sv.restore.way.clone().into());
    ui.set_rw_target(sv.restore.target.clone().into());
    ui.set_rw_progress(f32::from(sv.restore.run.progress_pct) / 100.0);
    ui.set_rw_outcome(i32::from(sv.restore.run.outcome));
    sync_log_localized(ui.get_lang_index(), &ui.get_rw_log(), &sv.restore.run.log, |m| {
        ui.set_rw_log(m)
    });
    sync_log_tones(&ui.get_rw_log_tone(), &sv.restore.run.log, |m| ui.set_rw_log_tone(m));
    ui.set_rw_headline(localize_headline(ui.get_lang_index(), &sv.restore.run.headline).into());

    // founding ritual; the run header is composed here so an MCP-started
    // founding shows real values even with an empty local form
    ui.set_cw_step(i32::from(sv.create.run.step));
    ui.set_cw_outcome(i32::from(sv.create.run.outcome));
    ui.set_cw_seed(sv.create.seed.clone().into());
    ui.set_cw_backup_confirmed(sv.create.backup_confirmed);
    ui.set_cw_run_name(sv.create.name.clone().into());
    ui.set_cw_run_detail(
        format!(
            "{}-of-{} · {}",
            sv.create.threshold, sv.create.members, sv.create.net
        )
        .into(),
    );
    sync_log_localized(ui.get_lang_index(), &ui.get_cw_log(), &sv.create.run.log, |m| {
        ui.set_cw_log(m)
    });
    sync_log_tones(&ui.get_cw_log_tone(), &sv.create.run.log, |m| ui.set_cw_log_tone(m));
    ui.set_cw_headline(localize_headline(ui.get_lang_index(), &sv.create.run.headline).into());
    // a declined seat switches the failure banner to "the founding is over"
    ui.set_cw_declined(sv.create.seats.iter().any(|s| s.state == 3));
    // the ritual member list: founder plus one row per seat. The founder
    // row is sealed by construction (2) and fully green (4) once their own
    // phrase backup is confirmed — same scale as the member seats.
    let mut seats: Vec<RitualSeat> = vec![RitualSeat {
        member: sv.create.member.as_str().into(),
        detail: strings_founder(lang).into(),
        state: if sv.create.backup_confirmed { 4 } else { 2 },
    }];
    for (i, s) in sv.create.seats.iter().enumerate() {
        let (member, detail) = if s.member.is_empty() {
            // only offer the link once it is genuinely joinable (carries the
            // transport handover); until the queue is provisioned it is a
            // non-joinable preview, so show nothing to copy yet
            let detail = if molt_engine::FoundingInvite::parse(&s.link).is_ok() {
                s.link.clone()
            } else {
                String::new()
            };
            (
                if lang == 1 {
                    format!("Einladung {}", i + 1)
                } else {
                    format!("Invite {}", i + 1)
                },
                detail,
            )
        } else {
            (s.member.clone(), seat_state_label(lang, s.state))
        };
        seats.push(RitualSeat {
            member: member.into(),
            detail: detail.into(),
            state: i32::from(s.state),
        });
    }
    // sealed = the roster signature is in (state 2), key secured or not
    // (state 4) — counting only 2 read as a regression at the very end
    let sealed = seats.iter().filter(|s| s.state == 2 || s.state == 4).count();
    ui.set_cw_sealed(i32::try_from(sealed).unwrap_or(0));
    ui.set_cw_total(i32::try_from(seats.len()).unwrap_or(0));
    // every MEMBER has ratified the charter (signature delivered, 2/4) —
    // gates the founder's own phrase-backup prompt: it comes only after
    // all others accepted the charter and features
    ui.set_cw_all_ratified(
        !sv.create.seats.is_empty()
            && sv
                .create
                .seats
                .iter()
                .all(|s| s.state == 2 || s.state == 4),
    );
    ui.set_cw_simulated(sv.create.simulated);
    // the deliberation step: once every seat has joined, the founder proposes
    // the final name + charter for the members to ratify (the agenda itself is
    // a local editable draft in the wizard, like the name)
    ui.set_cw_can_propose(sv.create.can_propose);
    sync_rows(&ui.get_cw_seats(), seats, |m| ui.set_cw_seats(m));

    // the rejoiner's re-admission checklist (recovery_auto_approval.md §5):
    // roster seats with their counted voices, toward `need` approvals
    let rv_seats: Vec<RecoverSeatRow> = sv
        .recover
        .seats
        .iter()
        .map(|s| RecoverSeatRow {
            member: s.member.as_str().into(),
            approved: s.approved,
        })
        .collect();
    ui.set_rv_have(
        i32::try_from(sv.recover.seats.iter().filter(|s| s.approved).count()).unwrap_or(0),
    );
    ui.set_rv_need(i32::try_from(sv.recover.need).unwrap_or(0));
    sync_rows(&ui.get_rv_seats(), rv_seats, |m| ui.set_rv_seats(m));

    // join
    ui.set_jw_step(i32::from(sv.join.run.step));
    // the engine returned the join session to the idle form (join_finish
    // after success, or an invalidation by create/recover/open) — re-arm
    // the optimistic start latch, or the NEXT join in this app run has a
    // dead button (review 2026-08-12). While a start is in flight the
    // engine's own re-entry guard still refuses a double, so the worst a
    // racing pre-start push can do is re-show the toast this latch avoids.
    if sv.join.run.step == 0 {
        ui.set_jw_starting(false);
    }
    ui.set_jw_progress(f32::from(sv.join.run.progress_pct) / 100.0);
    ui.set_jw_outcome(i32::from(sv.join.run.outcome));
    ui.set_jw_republic(sv.join.republic.clone().into());
    ui.set_jw_rule(format!("{}-of-{}", sv.join.rule_m, sv.join.rule_n).into());
    ui.set_jw_inviter(sv.join.inviter.clone().into());
    // the joiner's own recovery phrase, generated + shown once (like the
    // founder's on the create screen)
    ui.set_jw_seed(sv.join.seed.clone().into());
    // the ratification step: the founder's proposed charter, which the joiner
    // must confirm before its signature is released and the workspace opens
    ui.set_jw_awaiting_ratify(sv.join.awaiting_ratify);
    ui.set_jw_awaiting_backup(sv.join.awaiting_backup);
    ui.set_jw_sealed(!sv.join.sealed_id.is_empty());
    ui.set_jw_proposed_name(sv.join.proposed_name.clone().into());
    ui.set_jw_proposed_agenda(sv.join.proposed_agenda.clone().into());
    // the proposed feature selection, exactly as it will be signed; a
    // pre-v5 founder (None) renders the legacy baseline
    let jw_feat = |key: &str| match &sv.join.proposed_features {
        Some(f) => f.iter().any(|k| k == key),
        None => molt_core::Surface::LEGACY_FEATURES.contains(&key),
    };
    ui.set_jw_feat_memory(jw_feat("memory"));
    ui.set_jw_feat_quests(jw_feat("quests"));
    ui.set_jw_feat_vault(jw_feat("vault"));
    ui.set_jw_feat_wallet(jw_feat("wallet"));
    sync_log_localized(ui.get_lang_index(), &ui.get_jw_log(), &sv.join.run.log, |m| {
        ui.set_jw_log(m)
    });
    sync_log_tones(&ui.get_jw_log_tone(), &sv.join.run.log, |m| ui.set_jw_log_tone(m));
    ui.set_jw_headline(localize_headline(ui.get_lang_index(), &sv.join.run.headline).into());
}

/// Per-line tone of a run log (0 neutral, 1 good, 2 bad) from the ✓/✗
/// prefix convention every engine log line follows — lets the Slint side
/// highlight terminal lines without string surgery it cannot do.
// Mirrored in place via sync_rows: the log modal's per-line colour bindings
// consume these models, and a fresh ModelRc per engine tick would reset the
// repeater on every tick while the modal is open.
fn sync_log_tones(current: &ModelRc<i32>, log: &[String], set: impl FnOnce(ModelRc<i32>)) {
    sync_rows(
        current,
        log.iter()
            .map(|l| match l.chars().next() {
                Some('✓') => 1,
                Some('✗') => 2,
                _ => 0,
            })
            .collect::<Vec<i32>>(),
        set,
    );
}

/// Read every surface and push it into the window.
///
/// The gathering ([`gather_surfaces`]) is separate so it can be TESTED: it
/// is all of the decisions — which workspace, which channel, whether this
/// pass is still current — and it needs no window and no event loop. What
/// stays here is the hop onto the UI thread, which is Slint's own.
pub(crate) async fn push_surfaces(
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let Some((my_gen, bundle)) = gather_surfaces(wallet, chat_ui).await else {
        return;
    };
    let weak2 = weak.clone();
    let chat_ui = chat_ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let weak = weak2;
        // the generation may have moved between the bundle build and this
        // closure running on the UI thread — a stale bundle must not land
        // (it would revert the visible pane until the next engine event)
        if !chat_ui.lock().map(|st| st.is_current(my_gen)).unwrap_or(false) {
            tracing::debug!(gen = my_gen, "ui: bundle DROPPED as stale");
            return;
        }
        if let Some(ui) = weak.upgrade() {
            apply_surfaces(&ui, &bundle);
            tracing::debug!(
                gen = my_gen,
                chat_rows = ui
                    .get_surfaces()
                    .iter()
                    .find(|s| s.key == "chat")
                    .map_or(0, |s| s.log.row_count()),
                "ui: bundle applied"
            );
        }
    });
    // the snapshot must claim what this pass just rendered
    // (gui_over_mcp.md): the publish closure queues AFTER the apply
    // closure above, so it reads the fresh models — without this, a
    // surfaces-only change (a chat message arriving) left the published
    // snapshot stale until the next SESSION push
    publish_ui_state(wallet, weak).await;
}

/// Build the Slint surface models from a bundle (on the UI thread). The rows
/// of the surfaces model are updated IN PLACE when possible: replacing the
/// whole model would recreate every main-view element on each engine event —
/// and with it drop the keyboard focus out of the chat compose box mid-typing.
pub(crate) fn apply_surfaces(ui: &AppWindow, b: &SurfacesBundle) {
    ui.set_node_member(b.member.clone().into());
    // the poke menus need the own seat too: every site gates "never myself"
    // through Poke.can()
    ui.global::<Poke>().set_me(b.member.clone().into());
    // the Chain-History panel: paged at 20, newest first (rows arrive
    // newest-first from read_chain)
    {
        let page = page_of(&b.list_pages, "chain", "history");
        let (start, end, page, pages) = page_slice(b.chain_rows.len(), page, LIST_PAGE_SIZE);
        let rows: Vec<ChainRow> = b.chain_rows[start..end]
            .iter()
            .map(|r| chain_row(b.lang, r))
            .collect();
        ui.set_chain_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_chain_page(i32::try_from(page + 1).unwrap_or(1));
        ui.set_chain_pages(i32::try_from(pages).unwrap_or(1));
    }
    ui.set_threshold_badge(b.threshold_badge.clone().into());
    let tabs: Vec<SurfaceTab> = b
        .surfaces
        .iter()
        .map(|s| {
            // the paged lists (page size LIST_PAGE_SIZE, pager row in the
            // .slint side): a gated surface's log IS its applied/accepted
            // history, so it pages; chat's log is the chat pane — full.
            // Counts stay full-list so the status strip and the nav badges
            // never shrink to a page.
            let a_page = page_of(&b.list_pages, &s.key, "applied");
            let (a_start, a_end, a_page, a_pages) = if s.gated {
                page_slice(s.log.len(), a_page, LIST_PAGE_SIZE)
            } else {
                (0, s.log.len(), 0, 1)
            };
            let log: Vec<LogLine> = s.log[a_start..a_end]
                .iter()
                .map(|l| {
                    let reactions: Vec<ReactionItem> = l
                        .reactions
                        .iter()
                        .map(|r| ReactionItem {
                            emoji: r.emoji.as_str().into(),
                            count: r.count,
                            mine: r.mine,
                        })
                        .collect();
                    let receipts: Vec<ReceiptItem> = l
                        .receipts
                        .iter()
                        .map(|r| ReceiptItem {
                            name: r.name.as_str().into(),
                            read: r.read,
                        })
                        .collect();
                    LogLine {
                        id: l.id.clone().into(),
                        lead: l.lead.clone().into(),
                        text: l.text.clone().into(),
                        when: l.when.clone().into(),
                        quote: l.quote,
                        system: l.system,
                        quote_label: l.quote_label.clone().into(),
                        quote_indent: l.quote_indent,
                        deleted_by: l.deleted_by.clone().into(),
                        first: l.first,
                        own: l.own,
                        alt: l.alt,
                        mine_emoji: l.mine_emoji.clone().into(),
                        reactions: ModelRc::new(VecModel::from(reactions)),
                        receipts: ModelRc::new(VecModel::from(receipts)),
                        has_file: l.has_file,
                        file_name: l.file_name.clone().into(),
                        file_meta: l.file_meta.clone().into(),
                        file_available: l.file_available,
                        // -1 = no known proposal origin: the row offers no
                        // 💬 jump (feedback honesty, like the id guards)
                        patch_id: l
                            .proposal_id
                            .and_then(|i| i32::try_from(i).ok())
                            .unwrap_or(-1),
                    }
                })
                .collect();
            let to_row = to_proposal_row;
            // pending pages too (ask 2026-08-15: proposal cards are tall,
            // a long list needs pages) — the nav badges and
            // `pending-my-vote` alerts stay full-list, so an open vote on
            // a later page still alarms
            let p_page = page_of(&b.list_pages, &s.key, "pending");
            let (p_start, p_end, p_page, p_pages) = if s.gated {
                page_slice(s.pending.len(), p_page, LIST_PAGE_SIZE)
            } else {
                (0, s.pending.len(), 0, 1)
            };
            let pending: Vec<ProposalRow> = s.pending[p_start..p_end].iter().map(to_row).collect();
            let d_page = page_of(&b.list_pages, &s.key, "declined");
            let (d_start, d_end, d_page, d_pages) =
                page_slice(s.declined.len(), d_page, LIST_PAGE_SIZE);
            let declined: Vec<ProposalRow> =
                s.declined[d_start..d_end].iter().map(to_decided_row).collect();
            // the Accepted table pages in lockstep with the applied log
            // (same source list, same length — `accepted` is its
            // newest-first projection)
            let (ac_start, ac_end, _, _) = if s.gated {
                page_slice(s.accepted.len(), a_page, LIST_PAGE_SIZE)
            } else {
                (0, 0, 0, 1)
            };
            let accepted: Vec<ProposalRow> =
                s.accepted[ac_start..ac_end].iter().map(to_decided_row).collect();
            // the surface's sub-views come straight from the shared
            // molt-core vocabulary (same list select_view validates against)
            let views: Vec<ViewItem> = Surface::parse(&s.key)
                .map(|sf| {
                    sf.views()
                        .iter()
                        .map(|(key, label)| ViewItem {
                            key: (*key).into(),
                            name: view_label(b.lang, key, label).into(),
                            icon: view_icon(key).into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            SurfaceTab {
                key: s.key.clone().into(),
                name: s.name.clone().into(),
                gated: s.gated,
                applied_count: s.log.len() as i32,
                pending_count: s.pending.len() as i32,
                pending_voted_count: s.pending_voted as i32,
                pending_my_vote_count: (s.pending.len() - s.pending_voted) as i32,
                denied_count: s.denied as i32,
                declined_count: s.declined.len() as i32,
                // the pager echo, 1-based for the "page x of y" label
                applied_page: (a_page + 1) as i32,
                applied_pages: a_pages as i32,
                declined_page: (d_page + 1) as i32,
                declined_pages: d_pages as i32,
                pending_page: (p_page + 1) as i32,
                pending_pages: p_pages as i32,
                log: ModelRc::new(VecModel::from(log)),
                pending: ModelRc::new(VecModel::from(pending)),
                declined: ModelRc::new(VecModel::from(declined)),
                accepted: ModelRc::new(VecModel::from(accepted)),
                views: ModelRc::new(VecModel::from(views)),
            }
        })
        .collect();
    sync_rows(&ui.get_surfaces(), tabs, |m| ui.set_surfaces(m));

    // the Shared-Memory base: hand the folded tree to the wiki model over
    // the WikiState bridge (this apply runs from a Send-bound closure that
    // cannot hold the UI-thread Rc model; the base-arrived handler can)
    if let Some(mem) = b.surfaces.iter().find(|s| s.key == "memory") {
        let g = ui.global::<WikiState>();
        let docs: Vec<WikiBase> = mem
            .wiki_tree
            .iter()
            .map(|(p, c)| WikiBase {
                path: p.as_str().into(),
                content: c.as_str().into(),
            })
            .collect();
        sync_model(&g.get_base_docs(), docs, PartialEq::eq, |m| g.set_base_docs(m));
        g.set_base_rev(i32::try_from(mem.wiki_rev).unwrap_or(i32::MAX));
        g.invoke_base_arrived();
    }

    // the chat sidebar's channel rows + the canonical selection echo (so
    // the highlight always names the channel the engine filtered by)
    let channels: Vec<ChannelItem> = b
        .channels
        .iter()
        .map(|c| ChannelItem {
            key: c.key.as_str().into(),
            label: c.label.as_str().into(),
            icon: c.icon.as_str().into(),
            unread: c.unread,
        })
        .collect();
    // does the CURRENT filter have a row of its own? The Gruppe nav row
    // stands in for every filter that does not (the group itself, a decided
    // vote's read-only discussion), and it must NOT stand in for one that
    // does - a lit Gruppe row while the pane shows a topic tells the user
    // they are somewhere they are not.
    let selected_listed = b.channels.iter().any(|c| c.key == b.selected_key);
    sync_rows(&ui.get_chat_channels(), channels, |m| ui.set_chat_channels(m));
    ui.set_selected_channel_listed(selected_listed);
    ui.set_selected_channel(b.selected_key.as_str().into());
    ui.set_selected_channel_votable(b.selected_key.starts_with("patch:"));
    ui.set_selected_channel_label(b.selected_label.as_str().into());
    ui.set_selected_channel_closed(b.selected_closed);
    ui.set_selected_channel_org(b.selected_org);
    // the wiki-patch diff viewer: a SELECTED patch decision owns the
    // global; otherwise the first pending wiki patch on Shared Memory
    // does (its card in the proposals view carries the inline viewer).
    // Either way the parse refreshes only when the OWNING id changes, so
    // the user's file selection survives the mirror ticks.
    let decision_changed = ui.get_selected_decision().id != b.selected_decision.id;
    ui.set_selected_decision(to_proposal_row(&b.selected_decision));
    if b.selected_decision.patch_op {
        if decision_changed {
            patch_view_sync(
                ui,
                &b.selected_decision.proposed,
                0,
                b.selected_decision.id,
            );
        }
    } else {
        let mem_patch = b
            .surfaces
            .iter()
            .find(|s| s.key == "memory")
            .and_then(|s| s.pending.iter().find(|p| p.patch_op));
        let held = ui.global::<PatchView>().get_for_id();
        match mem_patch {
            Some(p) if held != p.id => patch_view_sync(ui, &p.proposed, 0, p.id),
            None if held != 0 => patch_view_sync(ui, "", 0, 0),
            _ => {}
        }
    }

    // the Organization tables (Members / Uploads). The avatars go through
    // the path-keyed cache: `sync_rows` below rewrites EVERY row on EVERY
    // push, so decoding here would re-decode the whole roster per tick
    let members: Vec<MemberRow> = AVATARS.with_borrow_mut(|cache| {
        cache.retain_live(&b.members.iter().map(|m| m.image_key.as_str()).collect());
        b.members
            .iter()
            .map(|m| {
                // the picture rode the applied proposal, so the engine
                // materialized the file on every device; decode by CONTENT
                // (the name's extension comes from a peer-supplied value)
                let avatar = (!m.image.is_empty())
                    .then(|| {
                        cache.get(&m.image_key, |_| {
                            std::fs::read(&m.image)
                                .ok()
                                .and_then(|b| image_from_bytes(&b))
                        })
                    })
                    .flatten();
                MemberRow {
                    name: m.name.as_str().into(),
                    id: m.id.as_str().into(),
                    pk: m.pk.as_str().into(),
                    last: m.last.as_str().into(),
                    state: m.state,
                    uploads: m.uploads,
                    split: m.split.as_str().into(),
                    avatar_set: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                    avatar_path: m.image.as_str().into(),
                    desc: m.desc.as_str().into(),
                }
            })
            .collect()
    });
    sync_rows(&ui.get_org_members(), members, |m| ui.set_org_members(m));
    let uploads: Vec<UploadRow> = b
        .uploads
        .iter()
        .map(|u| UploadRow {
            id: u.id.as_str().into(),
            user: u.user.as_str().into(),
            date: u.date.as_str().into(),
            name: u.name.as_str().into(),
            kind: u.kind.as_str().into(),
            size: u.size.as_str().into(),
            available: u.available,
            online: u.online,
            checksum: u.checksum.as_str().into(),
            expires: u.expires.as_str().into(),
            status: u.status.as_str().into(),
            status_kind: u.status_kind,
            availability: u.availability.as_str().into(),
        })
        .collect();
    sync_rows(&ui.get_org_uploads(), uploads, |m| ui.set_org_uploads(m));

    // the tables' sort/filter echo: the headers render the ▲/▼ arrow from
    // these; the filter box only refreshes when Rust owns the change (a
    // workspace-switch reset or the members-table uploads-jump) — a live
    // keystroke bumps the push generation, so a stale echo never lands
    ui.set_om_sort_column(b.members_sort.as_str().into());
    ui.set_om_sort_ascending(b.members_asc);
    ui.set_ou_sort_column(b.uploads_sort.as_str().into());
    ui.set_ou_sort_ascending(b.uploads_asc);
    if ui.get_ou_filter().as_str() != b.uploads_filter {
        ui.set_ou_filter(b.uploads_filter.as_str().into());
    }

    ui.set_group_unread(b.group_unread);

    // the status info strip (founding date + mock activity trio)
    ui.set_org_founded(b.org_stats.founded.as_str().into());
    ui.set_org_chat_retention(b.org_stats.retention_days);
    // the Members table offers "recovery link" only where recovery exists
    ui.set_org_chain_governed(b.org_stats.chain_governed);
    let feat_on = |key: &str| b.org_stats.features.iter().any(|f| f == key);
    ui.set_org_feat_memory(feat_on("memory"));
    ui.set_org_feat_quests(feat_on("quests"));
    ui.set_org_feat_vault(feat_on("vault"));
    ui.set_org_feat_wallet(feat_on("wallet"));
    sync_strings(&ui.get_org_relays(), &b.org_stats.relays, |m| ui.set_org_relays(m));
    // the R6 pencil's draft prefill: the same pool, space-joined
    ui.set_org_relays_joined(b.org_stats.relays.join(" ").into());

    // the republic's image: (re)load the picture only when the file
    // reference changes. The bytes rode the applied set_image proposal, so
    // the engine materializes the logo file on EVERY device; decode by
    // CONTENT (image_from_bytes) — the reference's extension comes from a
    // peer-supplied display value and must not decide the format. On a
    // session-only workspace the reference is no local file — the read
    // fails quietly and the placeholder mark stays.
    if let Some(key) = logo_needs_reload(
        &LOGO_KEY.with_borrow(std::clone::Clone::clone),
        &b.org_stats.image,
    ) {
        LOGO_KEY.with_borrow_mut(|k| *k = key);
        ui.set_org_img_path(b.org_stats.image.as_str().into());
        let loaded = (!b.org_stats.image.is_empty())
            .then(|| std::fs::read(&b.org_stats.image).ok())
            .flatten()
            .and_then(|bytes| image_from_bytes(&bytes));
        ui.set_org_img_set(loaded.is_some());
        ui.set_org_img(loaded.unwrap_or_default());
    }
    // the picture budget this republic still carries: the engine stays the
    // authority, this is what the member-picture fit aims at
    ui.set_mp_img_budget(i32::try_from(b.org_stats.image_budget).unwrap_or(i32::MAX));
}

/// The live-mirror task: the first full session + surfaces push, then a
/// re-read on every engine event - session changes repaint the session
/// (a run-scoped tick only its run), surface events re-read the surfaces,
/// and the alert/toast side effects ride the same stream. Never holds
/// state: a lagged receiver simply re-reads everything.
pub(crate) fn spawn_mirror(ctx: &Ctx) {
    let Ctx {
        rt,
        wallet: w,
        weak,
        last_settings,
        chat_ui,
    } = ctx.clone();
    rt.spawn(async move {
        let mut rx = w.subscribe();
        push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
        push_surfaces(&w, &weak, &chat_ui).await;
        loop {
            match rx.recv().await {
                Ok(Event::SessionChanged { scope }) => {
                    push_session(&w, &weak, &last_settings, scope, &chat_ui).await;
                    // A Full session change can mean a workspace was
                    // opened or closed — the surface state (replayed
                    // chat history!) changed with it, without any
                    // chat/proposal event firing. Run-scoped ticks
                    // (90 ms) deliberately skip this.
                    if scope == SessionScope::Full {
                        // a Full change can be a workspace open/close:
                        // proposal ids are per-workspace counters, so a
                        // stale inline-viewer id would light up an id-
                        // colliding card in the NEXT workspace with the
                        // previous one's decoded image — drop it
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                ui.set_img_inline_id(-1);
                            }
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                }
                // Any surface event (chat / propose / approve / …) re-reads
                // the surfaces, so the GUI mirrors what an MCP agent did.
                // An Event::Chat carries id+channel and could tick unread
                // counters directly, but the re-read stays the single
                // source of truth — event payloads never drive state.
                // A finished download additionally toasts its outcome
                // (the table repaints via the same re-read).
                // alert sounds: an INCOMING chat message (never our own
                // echo) and a new vote play the configured alert — read
                // from the last APPLIED settings, so an unsaved draft
                // never changes behavior
                Ok(Event::Chat { from, .. }) => {
                    alert_unless_own(&last_settings, |s| s.sound_message.clone(), &weak, from);
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                // only a vote somebody ELSE initiated rings — the
                // proposer already knows what they just did
                Ok(Event::Proposed { by, .. }) => {
                    alert_unless_own(&last_settings, |s| s.sound_vote.clone(), &weak, by);
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                // a poke addressed to THIS seat toasts who poked and
                // rings its own sound (the engine already gated opt-in +
                // cooldown); the sender side confirms quietly. No
                // push_surfaces — a poke changes no surface state.
                Ok(Event::Poked { by, to }) => {
                    alert_unless_own(&last_settings, |s| s.sound_poke.clone(), &weak, by.clone());
                    let weak2 = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak2.upgrade() else { return };
                        let st = ui.global::<Strings>();
                        let me = ui.get_node_member();
                        if to.as_str() == me.as_str() {
                            ui.invoke_show_toast(format!("{by} {}", st.get_toast_poked()).into());
                        } else if by.as_str() == me.as_str() {
                            ui.invoke_show_toast(
                                format!("{} {to}", st.get_toast_poke_sent()).into(),
                            );
                        }
                    });
                }
                // WP4b: checkpoint lifecycle closure for the operator —
                // sealed toasts the height, stale tells them to re-cut
                Ok(Event::CheckpointSealed { height, .. }) => {
                    let weak2 = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak2.upgrade() else { return };
                        let msg = ui.global::<Strings>().get_toast_checkpoint_sealed();
                        ui.invoke_show_toast(format!("{msg} #{height}").into());
                    });
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                // CheckpointStale is NOT toasted: the automation re-cuts
                // by itself on the very next commit — a "propose again"
                // instruction would be noise (the event stays on the
                // stream for MCP observers)
                Ok(Event::CheckpointStale { .. }) => {
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                Ok(Event::FileTransfer { phase, .. }) => {
                    let weak2 = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak2.upgrade() else { return };
                        let st = ui.global::<Strings>();
                        match &phase {
                            molt_core::TransferPhase::Done { path } => {
                                ui.invoke_show_toast(
                                    format!("{} {path}", st.get_toast_dl_done()).into(),
                                );
                            }
                            molt_core::TransferPhase::Failed { reason } => {
                                ui.invoke_show_toast_error(
                                    format!("{} {reason}", st.get_toast_dl_failed()).into(),
                                );
                            }
                            _ => {}
                        }
                    });
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                Ok(Event::UiActionRequested { action }) => {
                    // gui_over_mcp.md, the drive half: perform the verb
                    // through the SAME callbacks a human's click takes,
                    // then publish — both queue on the event loop, so
                    // the publish claim is post-perform. A pure view
                    // change may not ring the engine, hence the
                    // explicit publish.
                    let weak2 = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak2.upgrade() {
                            perform_ui_action(&ui, &action);
                        }
                    });
                    publish_ui_state(&w, &weak).await;
                }
                Ok(_) => push_surfaces(&w, &weak, &chat_ui).await,
                Err(RecvError::Lagged(_)) => {
                    push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
                    push_surfaces(&w, &weak, &chat_ui).await;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}
