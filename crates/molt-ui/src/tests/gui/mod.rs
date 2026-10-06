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
mod files;
mod founding_fold;
mod layout;
mod mirror;
#[cfg(feature = "live-preview")]
mod modals;
mod poke;
mod recovery_backup;
mod ritual_log;
mod seed_confirm;
mod snapshot;
mod tab_order;
mod vault;
#[cfg(feature = "live-preview")]
mod vault_charter;
mod version_panel;
mod wiki;
#[cfg(feature = "live-preview")]
mod wiki_files;
mod wiki_file_picker;
#[cfg(feature = "live-preview")]
mod window_lifetime;

use super::*;

/// A headless window shown by [`show_headless`]; dropping it hides the
/// window again.
///
/// A shown Slint window holds a strong reference to its own component
/// (`WindowInner::show` keeps it until `hide`), and the component holds
/// the window adapter: a cycle that dropping every `AppWindow` handle
/// does not break. Unhidden, each shown test window stayed resident
/// until the process ended (~200 MB in a debug build, the whole
/// interpreter-compiled UI) - which is what OOM-killed the GUI suite
/// run in one process. Pinned by `window_lifetime.rs`.
#[must_use = "bind it (`let _shown = …`): dropping it hides the window"]
struct Shown(AppWindow);

impl Drop for Shown {
    fn drop(&mut self) {
        // A failure to hide only means the window stays resident - the
        // old behaviour - so it must not turn a passing test into a panic.
        let _ = self.0.hide();
    }
}

/// Show `ui` on the testing backend; the returned guard hides it again
/// when the test is done with it. The ONLY way a GUI test shows a window.
fn show_headless(ui: &AppWindow) -> Shown {
    ui.show().expect("show headless");
    Shown(ui.clone_strong())
}

/// Organization → Members with two seats, rendered headless. `on` is
/// the applied poke switch the menus gate on.
#[cfg(feature = "live-preview")]
fn members_window(on: bool) -> (AppWindow, Shown) {
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
    let shown = show_headless(&ui);
    (ui, shown)
}

/// Type one character into the focused element.
#[cfg(feature = "live-preview")]
fn type_char(ui: &AppWindow, c: &str) {
    let text: slint::SharedString = c.into();
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
}

/// Press one named key. Backtab (Shift+Tab) has no `Key` variant - it is
/// the raw code Slint's focus walk reads.
#[cfg(feature = "live-preview")]
fn press(ui: &AppWindow, key: slint::platform::Key) {
    type_char(ui, &slint::SharedString::from(key));
}

/// Shift+Tab: one step BACK through the focus order.
#[cfg(feature = "live-preview")]
fn back_tab(ui: &AppWindow) {
    type_char(ui, "\u{0019}");
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

/// One real left click at the centre of `area`.
#[cfg(feature = "live-preview")]
fn click(ui: &AppWindow, area: &i_slint_backend_testing::ElementHandle) {
    let pos = area.absolute_position();
    let size = area.size();
    let at = slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
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
#[cfg(feature = "live-preview")]
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

/// The nav's tab for a surface key, if the window lists it.
fn surface_tab(ui: &AppWindow, key: &str) -> Option<SurfaceTab> {
    ui.get_surfaces().iter().find(|s| s.key == key)
}

/// How many chat rows the window is showing right now.
fn chat_rows(ui: &AppWindow) -> usize {
    surface_tab(ui, "chat").map_or(0, |s| s.log.row_count())
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
        founded_ts: 0,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let genesis = sealed.into_genesis(roster[0], now);
    let ws = molt_storage::create_workspace(root, &seed, &genesis).expect("create");
    (ws, now)
}

/// A vault republic ON DISK as seat `roster[0]`: a
/// threshold-signed roster-v6 genesis (every seat keyed, `vault` in the
/// features) and this seat's identity key and vault seed in its
/// transport state - what a finished vault founding leaves behind.
fn vault_workspace_on_disk(
    root: &std::path::Path,
    rule_m: u8,
    roster: &[&str],
) -> (molt_storage::OpenedWorkspace, u64) {
    chain_workspace_on_disk(root, rule_m, roster, true)
}

/// A signed chain genesis with the `vault` feature; `keyed = false` is
/// the v5 mock vault (the feature without seat keys).
fn chain_workspace_on_disk(
    root: &std::path::Path,
    rule_m: u8,
    roster: &[&str],
    keyed: bool,
) -> (molt_storage::OpenedWorkspace, u64) {
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let own = molt_storage::seed_entropy(&phrase).expect("entropy");
    let mut keys = Vec::new();
    let mut identities = Vec::new();
    let mut own_seed = None;
    for (i, member) in roster.iter().enumerate() {
        let entropy = if i == 0 {
            own.clone()
        } else {
            vec![u8::try_from(i).expect("small roster"); 32]
        };
        let (sk, identity_pk) = molt_storage::derive_identity_key(&entropy, member);
        let nostr_pk = molt_net::nostr_identity(&entropy, "t").1;
        let vault_pk = if keyed {
            let seed = molt_vault::derive_vault_seed(&entropy, &nostr_pk, &identity_pk);
            let pk = molt_vault::vault_keypair(&seed).1;
            if i == 0 {
                own_seed = Some(molt_core::vault::SecretBytes(seed.to_vec()));
            }
            pk
        } else {
            String::new()
        };
        identities.push(molt_core::MemberIdentity {
            member: (*member).to_string(),
            identity_pk,
            nostr_pk,
            vault_pk,
        });
        keys.push(sk);
    }
    let name = "DevTest".to_string();
    let rule_n = u8::try_from(roster.len()).expect("roster fits u8");
    let republic_id = molt_storage::republic_id(&name, rule_m, rule_n, &identities);
    let features = Some(vec!["vault".to_string()]);
    let change = molt_core::ChainChange::Genesis {
        name: name.clone(),
        republic_id: republic_id.clone(),
        rule_m,
        rule_n,
        identities: identities.clone(),
        agenda: "keep secrets".to_string(),
        relays: Vec::new(),
        features: features.clone(),
    };
    let bytes = molt_core::approval_bytes(&republic_id, 0, &change);
    let attestations: Vec<molt_core::RosterAttestation> = roster
        .iter()
        .zip(&keys)
        .map(|(member, sk)| molt_core::RosterAttestation {
            member: (*member).to_string(),
            sig: molt_storage::identity_sign(sk, &bytes),
        })
        .collect();
    let block = molt_core::ChainBlock {
        height: 0,
        prev: molt_core::GENESIS_PREV.to_string(),
        change,
        sigs: attestations.clone(),
    };
    let sealed = molt_core::SealedRoster {
        name,
        republic_id,
        rule_m,
        rule_n,
        roster: roster.iter().map(|s| (*s).to_string()).collect(),
        identities,
        attestations,
        relays: Vec::new(),
        agenda: "keep secrets".to_string(),
        features,
        founded_ts: 0,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ws = molt_storage::create_workspace(root, &own, &sealed.into_genesis(roster[0], now))
        .expect("create");
    ws.write_chain(None, &[block]).expect("chain");
    ws.write_transport_state(&molt_core::TransportState {
        identity_sk: Some(keys[0].to_bytes().to_vec()),
        vault_seed: own_seed,
        ..molt_core::TransportState::default()
    })
    .expect("transport state");
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
