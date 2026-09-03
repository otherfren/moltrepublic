// SPDX-License-Identifier: GPL-3.0-or-later
//! **The GUI's own logic, run headless.**
//!
//! Everything here drives the REAL `AppWindow` against a REAL engine
//! through the same live-mirror functions the running app uses — with
//! `i-slint-backend-testing` there is no display and no window, so these
//! belong in the ordinary suite.
//!
//! They exist because three chat bugs in a row were diagnosed by reading
//! code instead of by evidence: the engine was provably right each time
//! (checked against a live `moltd` over MCP), and the fault was in this
//! layer, where nothing could observe it.

mod chat;
mod layout;
mod poke;
mod recovery_backup;
mod snapshot;
mod wiki;

use super::*;

/// Organization → Members with two seats, rendered headless. `on` is
/// the applied poke switch the menus gate on.
#[cfg(feature = "live-preview")]
fn members_window(on: bool) -> AppWindow {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("members".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        ..SurfaceTab::default()
    }])));
    ui.set_node_member("walter".into());
    ui.set_org_members(ModelRc::new(VecModel::from(vec![
        MemberRow { name: "walter".into(), ..MemberRow::default() },
        MemberRow { name: "petra".into(), ..MemberRow::default() },
    ])));
    ui.global::<Poke>().set_me("walter".into());
    ui.global::<Poke>().set_on(on);
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

/// One real right press inside `area`, `fx` across its width (1.0 = the
/// right edge) and vertically centred.
#[cfg(feature = "live-preview")]
fn right_click(ui: &AppWindow, area: &i_slint_backend_testing::ElementHandle, fx: f32) {
    let pos = area.absolute_position();
    let size = area.size();
    let at = slint::LogicalPosition::new(
        pos.x + (size.width * fx).min(size.width - 2.0),
        pos.y + size.height / 2.0,
    );
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: at,
        button: slint::platform::PointerEventButton::Right,
    });
}

/// The open poke menu, found by the title its single item carries.
#[cfg(feature = "live-preview")]
fn poke_menu_open(
    ui: &AppWindow,
    label: &str,
) -> Option<i_slint_backend_testing::ElementHandle> {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label).next()
}

/// One orphan-row session for the backup-table tests (field bug
/// 2026-08-24): a bucket-only workspace plus one foreign key.
fn sv_backup_orphan() -> (SessionView, String) {
    let id = "ab".repeat(32);
    let sv = SessionView {
        backup_orphans: vec![
            molt_core::BackupOrphan {
                id: id.clone(),
                name: String::new(),
                size_kib: 480,
                last_backup_min: 60,
            },
            molt_core::BackupOrphan {
                id: String::new(),
                name: "molt/leftover.bin".to_string(),
                size_kib: 75,
                last_backup_min: 43_200,
            },
        ],
        // no demo locals: exactly the two bucket rows render
        workspaces: Vec::new(),
        ..SessionView::default()
    };
    (sv, id)
}

/// A node with storage, a founded workspace, and chat in it — the state
/// a user's second launch starts from.
fn node_with_chat(root: &std::path::Path) -> (WalletHandle, String) {
    // exactly the session `moltd` hands the engine at startup: the
    // workspaces are what is ON DISK. `SessionView::default()` carries
    // the demo fixtures, which would list six republics that do not
    // exist and hide the one that does.
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: molt_core::SessionSettings {
            workspace_dir: root.display().to_string(),
            ..molt_core::SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::spawn_with_storage(GroupConfig::demo(), session);
    (w, String::new())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// How many chat rows the window is showing right now.
fn chat_rows(ui: &AppWindow) -> usize {
    ui.get_surfaces()
        .iter()
        .find(|s| s.key == "chat")
        .map_or(0, |s| s.log.row_count())
}

/// A sealed workspace ON DISK, demo-grade (empty identities and
/// attestations), plus the unix `now` its appended events should stamp
/// — NOW, not a fixed stamp: chat older than the retention window is
/// correctly invisible, and a fixture from last year would "reproduce"
/// a bug that is the product working as specified.
fn workspace_on_disk(
    root: &std::path::Path,
    rule_m: u8,
    roster: &[&str],
    agenda: &str,
) -> (molt_storage::OpenedWorkspace, u64) {
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let seed = molt_storage::seed_entropy(&phrase).expect("entropy");
    let sealed = molt_core::SealedRoster {
        name: "DevTest".to_string(),
        republic_id: "d0".repeat(32),
        rule_m,
        rule_n: u8::try_from(roster.len()).expect("roster fits u8"),
        roster: roster.iter().map(|s| (*s).to_string()).collect(),
        identities: Vec::new(),
        attestations: Vec::new(),
        relays: Vec::new(),
        agenda: agenda.to_string(),
        features: None,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let genesis = sealed.into_genesis(roster[0], now);
    let ws = molt_storage::create_workspace(root, &seed, &genesis).expect("create");
    (ws, now)
}

/// The live-mirror's own two steps (session push, then surfaces
/// gather + apply), in its own order. The apply runs DIRECTLY rather
/// than through `invoke_from_event_loop`: the headless backend never
/// drains that queue, and the hop onto the UI thread is Slint's
/// business, not this layer's.
async fn mirror(
    w: &WalletHandle,
    ui: &AppWindow,
    last: &Arc<Mutex<Option<SessionSettings>>>,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) {
    let weak = ui.as_weak();
    push_session(w, &weak, last, SessionScope::Full, chat_ui).await;
    if let Some((_, b)) = gather_surfaces(w, chat_ui).await {
        apply_surfaces(ui, &b);
    }
}
