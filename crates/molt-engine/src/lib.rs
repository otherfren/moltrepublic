// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-engine`: the wallet core, as a single owning actor.
//!
//! One `tokio` task owns all authoritative state. Operators never touch that
//! state directly; they hold a cloneable [`WalletHandle`] and send typed
//! [`Command`]s, receiving a [`Reply`] back and observing [`Event`]s on a
//! broadcast channel. This is the structure the Plenum/MoltRepublic design
//! prescribes (architecture_idea A3 / backend_plan R8): serializing every
//! mutation through one owner removes a class of races and gives a single,
//! authoritative event order that both frontends mirror.
//!
//! The approval logic here is a faithful but *simulated* stand-in for the real
//! threshold machine: there is no FROST, no MLS. Each `Approve` counts as one
//! member's co-signature; when the count reaches the group threshold the
//! proposal is applied. Swapping in the real signing backend is a future
//! surface-crate concern and does not change this contract.
//!
//! The implementation is split by concern: [`chat`] (the ungated surface,
//! typed messages, reactions, deletion), [`net`] (the `molt-net` glue: the
//! log-backed outbox feed, the inbound `Net*` handlers, and the loopback
//! demo mesh whose peers replaced the old reply simulator), [`proposals`]
//! (the gated propose/approve/apply machine and snapshots), [`session`]
//! (navigation, settings, workspaces) and [`lifecycles`] (the three
//! engine-run mocks: restore / create / join over one `RunCore`).

mod chat;
mod configstore;
mod events;
mod founding;
mod lifecycles;
mod net;
mod proposals;
mod session;

use std::collections::HashMap;
use std::path::PathBuf;

pub use configstore::ConfigStoreHandle;
#[doc(hidden)]
pub use founding::{
    join_founding_over_smp, ritual_join_over_smp, run_ritual_member, FoundingInvite, InviteMaterial,
    Ratifier, RitualTransport,
};
pub use net::{CmdSink, FileStateStore, StorageLog};

use molt_core::{
    ChatMessage, Command, Event, GroupConfig, MemberId, MoltError, ProposalRecord, Reply,
    SessionScope, SessionView, Surface, WorkspaceId,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Capacity of the inbound command queue.
const CMD_QUEUE: usize = 128;
/// Capacity of the outbound event broadcast.
const EVENT_QUEUE: usize = 512;

/// A command paired with the channel its reply must go back on.
pub(crate) struct Envelope {
    pub(crate) cmd: Command,
    pub(crate) reply: oneshot::Sender<Result<Reply, MoltError>>,
}

/// A cheap, cloneable handle to the running engine. This is the single object
/// every frontend talks to; cloning it and driving it from two places (GUI and
/// MCP) is exactly how the two operators stay co-equal.
#[derive(Clone)]
pub struct WalletHandle {
    cmd_tx: mpsc::Sender<Envelope>,
    ev_tx: broadcast::Sender<Event>,
}

impl WalletHandle {
    /// Execute one command and await its reply.
    pub async fn execute(&self, cmd: Command) -> Result<Reply, MoltError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Envelope { cmd, reply })
            .await
            .map_err(|_| MoltError::Engine("engine stopped".into()))?;
        rx.await
            .map_err(|_| MoltError::Engine("no reply from engine".into()))?
    }

    /// Subscribe to the authoritative event stream (the GUI live-mirror and any
    /// MCP event consumer read from here).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.ev_tx.subscribe()
    }
}

/// Start the engine on the current `tokio` runtime and return a handle to it.
///
/// `session` is the initial shared app/session state (screen, language, settings)
/// — usually built from the loaded `config.toml`. Must be called from within a
/// runtime context (the actor is `tokio::spawn`ed).
///
/// This variant runs without config persistence **and without workspace
/// storage** (unit tests, ephemeral nodes): workspaces live in the session
/// only, exactly like the original mock. `moltd` uses [`spawn_with_config`]
/// so settings round-trip with the node's `config.toml` and workspaces
/// persist under `workspace_dir`.
pub fn spawn(config: GroupConfig, session: SessionView) -> WalletHandle {
    spawn_inner(config, session, None, false)
}

/// Like [`spawn`], but with workspace storage under the session's
/// `workspace_dir`. What `moltd` gets via [`spawn_with_config`]; exposed for
/// integration tests that want real persistence without a config file.
pub fn spawn_with_storage(config: GroupConfig, session: SessionView) -> WalletHandle {
    spawn_inner(config, session, None, true)
}

/// Storage-backed engine whose founding ritual runs in **manual** mode:
/// it does not spawn simulated members but hands each seat's
/// [`founding::InviteMaterial`] out on the returned channel, so a *second*
/// engine instance can run the member side ([`founding::run_ritual_member`])
/// itself. This is the seam the two-instance dev test uses; over a real
/// transport (T3) the same material is what the invite link carries.
#[doc(hidden)]
pub fn __spawn_manual_founding(
    config: GroupConfig,
    session: SessionView,
) -> (WalletHandle, std::sync::mpsc::Receiver<Vec<founding::InviteMaterial>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, None, true, None, Some(tx), false, false);
    (handle, rx)
}

/// Storage-backed engine whose founding runs in the offline **sim** seam:
/// the founder's node simulates the other members over the loopback hub
/// (fast, deterministic, no network) — for founder-side sealing tests. The
/// product never uses this; the in-app founding is always real over SMP.
#[doc(hidden)]
pub fn __spawn_sim_founding(config: GroupConfig, session: SessionView, persist: bool) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(config, session, cmd_tx, cmd_rx, None, persist, None, None, false, true)
}

/// Like [`__spawn_manual_founding`], but the founding runs over the **real
/// SMP server** configured in `session.settings` (custom url or the public
/// default) instead of the loopback hub. The founder's queues live on the
/// server; a genuinely separate instance joins over its own SMP transport.
/// This is the seam the two-instance-over-SMP dev test uses.
#[doc(hidden)]
pub fn __spawn_manual_founding_over_smp(
    config: GroupConfig,
    session: SessionView,
) -> (WalletHandle, std::sync::mpsc::Receiver<Vec<founding::InviteMaterial>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, None, true, None, Some(tx), true, false);
    (handle, rx)
}

/// Start the engine bound to `config_path`: a [`configstore`] task persists
/// every settings change to that file (format-preserving, atomic) and watches
/// it for external edits, which are validated and mirrored into the shared
/// session. Fails fast when another node already runs on the same config.
///
/// The returned [`ConfigStoreHandle`] is for the binary's shutdown path
/// (flush pending writes, release the lock); the engine holds its own clone.
pub fn spawn_with_config(
    config: GroupConfig,
    session: SessionView,
    config_path: PathBuf,
) -> std::io::Result<(WalletHandle, ConfigStoreHandle)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let store = configstore::spawn(config_path, cmd_tx.clone())?;
    let handle = spawn_actor(
        config,
        session,
        cmd_tx,
        cmd_rx,
        Some(store.clone()),
        true,
        None,
        None,
        false,
        false,
    );
    Ok((handle, store))
}

fn spawn_inner(
    config: GroupConfig,
    session: SessionView,
    store: Option<ConfigStoreHandle>,
    persist: bool,
) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(config, session, cmd_tx, cmd_rx, store, persist, None, None, false, false)
}

#[allow(clippy::too_many_arguments)]
fn spawn_actor(
    config: GroupConfig,
    session: SessionView,
    cmd_tx: mpsc::Sender<Envelope>,
    mut cmd_rx: mpsc::Receiver<Envelope>,
    store: Option<ConfigStoreHandle>,
    persist: bool,
    net: Option<net::NetRuntime>,
    ritual_material_sink: Option<std::sync::mpsc::Sender<Vec<founding::InviteMaterial>>>,
    ritual_over_smp: bool,
    ritual_sim: bool,
) -> WalletHandle {
    let (ev_tx, _keep) = broadcast::channel::<Event>(EVENT_QUEUE);

    let mut state = State::new(config, session, ev_tx.clone(), cmd_tx.clone(), store, persist, net);
    state.ritual_material_sink = ritual_material_sink;
    state.ritual_over_smp = ritual_over_smp;
    state.ritual_sim = ritual_sim;
    tokio::spawn(async move {
        while let Some(env) = cmd_rx.recv().await {
            let res = state.handle(env.cmd);
            // The operator may have gone away before the reply; that is fine.
            let _ = env.reply.send(res);
        }
        tracing::debug!("engine actor stopped");
    });

    WalletHandle { cmd_tx, ev_tx }
}

// The one shared clock (event timestamps must not drift from the storage
// layer's backup/trash age math).
pub(crate) use molt_storage::now_secs;

/// The replicated identity of the open workspace, established exclusively by
/// the `Founded` genesis event (and grown by `MemberJoined`) — rule, roster
/// and the acting member never exist outside the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReplicaState {
    pub(crate) name: String,
    pub(crate) member: MemberId,
    pub(crate) roster: Vec<MemberId>,
    pub(crate) rule_m: u8,
    /// The anchored name → identity-key table from the genesis (empty on
    /// pre-ritual workspaces).
    pub(crate) identities: Vec<molt_core::MemberIdentity>,
    /// The ratified founding charter (free-text agenda) from the genesis
    /// (empty on pre-deliberation workspaces).
    pub(crate) agenda: String,
}

/// The storage side of the open workspace: its id, directory, the engine's
/// authoritative copy of the local prefs (the writer applies updates in
/// order, so re-reading the file would race queued writes), and the
/// writer-thread handle every recorded envelope is enqueued on.
pub(crate) struct ActiveStorage {
    pub(crate) id: WorkspaceId,
    pub(crate) dir: PathBuf,
    pub(crate) prefs: molt_core::WorkspacePrefs,
    pub(crate) handle: molt_storage::StorageHandle,
}

/// All authoritative state, owned exclusively by the actor task.
pub(crate) struct State {
    pub(crate) config: GroupConfig,
    ev_tx: broadcast::Sender<Event>,
    /// Handle back into the actor's own queue (run tickers, net sink).
    /// Weak on purpose: the actor must stop once every *operator* handle
    /// is gone — its own self-reference must not keep it alive (demo peer
    /// engines rely on exactly this to terminate on mesh teardown).
    pub(crate) cmd_tx: mpsc::WeakSender<Envelope>,
    /// The chat log — typed, THE schema lives in [`molt_core::ChatMessage`].
    pub(crate) chat: Vec<ChatMessage>,
    /// Applied transition log per gated surface.
    pub(crate) applied: HashMap<Surface, Vec<Value>>,
    /// Every known proposal — stored as the schema type
    /// ([`molt_core::ProposalRecord`]), so snapshots need no conversion.
    pub(crate) proposals: HashMap<u64, ProposalRecord>,
    pub(crate) next_id: u64,
    /// The next event seq (strictly monotonic per workspace; reset on close).
    pub(crate) next_seq: u64,
    /// Identity of the open workspace, from its genesis event (None = no
    /// workspace open; the demo `GroupConfig` fills in).
    pub(crate) replica: Option<ReplicaState>,
    /// The open workspace's storage writer (None = nothing open, or a
    /// session-only workspace on a storage-less engine).
    pub(crate) active: Option<ActiveStorage>,
    /// The transport runtime (the demo loopback mesh today; a persisted
    /// workspace's supervisor once the T2 join flow wires real peers).
    pub(crate) net: Option<net::NetRuntime>,
    /// The founding-ritual runtime (present only while a founding is in
    /// flight — the workspace does not exist yet).
    pub(crate) net_ritual: Option<founding::RitualRuntime>,
    /// Seal signatures collected so far this ritual (founder first at
    /// finalize).
    pub(crate) ritual_attestations: Vec<molt_core::RosterAttestation>,
    /// When set, the founding ritual does NOT spawn simulated members;
    /// instead it hands the per-seat [`founding::InviteMaterial`] out on
    /// this channel so a *second* engine instance runs the member side.
    /// Only the two-instance dev test installs this.
    pub(crate) ritual_material_sink:
        Option<std::sync::mpsc::Sender<Vec<founding::InviteMaterial>>>,
    /// Forces the SMP transport for a founding in manual mode (the
    /// manual-over-SMP dev seam). The in-app founding uses SMP regardless;
    /// only the loopback dev seams leave this off.
    pub(crate) ritual_over_smp: bool,
    /// Offline **test seam only** ([`__spawn_sim_founding`]): found over the
    /// loopback hub with simulated members. The product never sets it — the
    /// in-app founding is always real over SMP; this keeps the founder-side
    /// sealing a fast, deterministic, offline test.
    pub(crate) ritual_sim: bool,
    /// Monotonic mesh/ritual-incarnation counter: `Net*` commands carry
    /// the generation of the runtime that sent them, and commands from a
    /// torn-down runtime are dropped (a delivery queued behind a workspace
    /// switch must not land in the new context's log).
    pub(crate) net_generation: u64,
    /// A separate incarnation counter for the **join** flow (an off-actor SMP
    /// join, possibly long-running). Kept apart from `net_generation` so a
    /// concurrent founding/mesh change can neither be mistaken for a stale
    /// join nor silently drop a live one.
    pub(crate) join_generation: u64,
    /// The channel the off-actor join task waits on for the joiner's charter
    /// ratification (`JoinConfirmCharter` sends `true`; cancel drops it). Set
    /// while a join is paused at the ratification step, else `None`.
    pub(crate) join_confirm: Option<mpsc::Sender<bool>>,
    /// Whether workspaces persist to disk at all ([`spawn`] = false).
    pub(crate) persist: bool,
    /// The shared app/session state (screen, language, settings, …).
    pub(crate) session: SessionView,
    /// The config file owner (None = no persistence: tests, ephemeral nodes).
    pub(crate) store: Option<ConfigStoreHandle>,
    /// The settings the node booted with — restart-required keys are flagged
    /// by comparing the current session against this snapshot.
    pub(crate) boot_settings: molt_core::SessionSettings,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: GroupConfig,
        session: SessionView,
        ev_tx: broadcast::Sender<Event>,
        cmd_tx: mpsc::Sender<Envelope>,
        store: Option<ConfigStoreHandle>,
        persist: bool,
        net: Option<net::NetRuntime>,
    ) -> Self {
        let mut applied = HashMap::new();
        for s in Surface::ALL {
            applied.insert(s, Vec::new());
        }
        let boot_settings = session.settings.clone();
        State {
            config,
            ev_tx,
            cmd_tx: cmd_tx.downgrade(),
            chat: Vec::new(),
            applied,
            proposals: HashMap::new(),
            next_id: 1,
            next_seq: 1,
            replica: None,
            active: None,
            net,
            net_ritual: None,
            ritual_attestations: Vec::new(),
            ritual_material_sink: None,
            ritual_over_smp: false,
            ritual_sim: false,
            net_generation: 0,
            join_generation: 0,
            join_confirm: None,
            persist,
            session,
            store,
            boot_settings,
        }
    }

    /// The acting member: the open workspace's identity, else the boot group.
    pub(crate) fn member(&self) -> MemberId {
        self.replica
            .as_ref()
            .map(|r| r.member.clone())
            .unwrap_or_else(|| self.config.member.clone())
    }

    /// The member roster: the open workspace's, else the boot group's.
    pub(crate) fn roster(&self) -> Vec<MemberId> {
        self.replica
            .as_ref()
            .map(|r| r.roster.clone())
            .unwrap_or_else(|| self.config.members.clone())
    }

    pub(crate) fn emit(&self, ev: Event) {
        // Ignore "no subscribers" — events are best-effort fan-out.
        let _ = self.ev_tx.send(ev);
    }

    /// Announce a session change with the given reach (run tickers use their
    /// narrow scope so mirrors can skip repainting everything at 90 ms).
    pub(crate) fn emit_session(&self, scope: SessionScope) {
        self.emit(Event::SessionChanged { scope });
    }

    pub(crate) fn threshold(&self) -> usize {
        self.replica
            .as_ref()
            .map(|r| usize::from(r.rule_m))
            .unwrap_or(self.config.threshold)
            .max(1)
    }

    /// Dispatch one command to its module.
    fn handle(&mut self, cmd: Command) -> Result<Reply, MoltError> {
        match cmd {
            // chat.rs
            Command::Chat { body, quote } => self.cmd_chat(body, quote),
            Command::ReactChat { index, emoji } => self.cmd_react_chat(index, emoji),
            Command::DeleteChat { index } => self.cmd_delete_chat(index),
            Command::ShareFile {
                name,
                size,
                kind,
                modified,
            } => self.cmd_share_file(name, size, kind, modified),
            Command::DownloadFile { index } => self.cmd_download_file(index),
            Command::RemoveFile { index } => self.cmd_remove_file(index),

            // proposals.rs
            Command::Propose { surface, payload } => self.cmd_propose(surface, payload),
            Command::Approve { proposal } => self.cmd_approve(proposal),
            Command::Decline { proposal } => self.cmd_decline(proposal),
            Command::ReadState { surface } => Ok(Reply::State(self.snapshot(surface))),
            Command::ListProposals => {
                let mut views: Vec<_> = self
                    .proposals
                    .iter()
                    .map(|(id, p)| self.view(*id, p))
                    .collect();
                views.sort_by_key(|v| v.id.0);
                Ok(Reply::Proposals { proposals: views })
            }
            Command::Status => Ok(Reply::Status(self.status())),

            // net.rs (engine-internal, sent by the node's own supervisor)
            Command::NetDelivered {
                from,
                envelope,
                generation,
            } => self.cmd_net_delivered(from, envelope, generation),
            Command::NetPeerSeen { member, generation } => {
                self.cmd_net_peer_seen(member, generation)
            }
            Command::NetSendFailed {
                member,
                reason,
                generation,
            } => self.cmd_net_send_failed(member, reason, generation),

            // session.rs
            Command::ReadSession => Ok(Reply::Session(Box::new(self.session.clone()))),
            Command::Navigate { screen } => self.cmd_navigate(screen),
            Command::SelectSurface { surface } => self.cmd_select_surface(surface),
            Command::SelectView { surface, view } => self.cmd_select_view(surface, view),
            Command::SetLanguage { lang } => self.cmd_set_language(lang),
            Command::SetTheme { theme } => self.cmd_set_theme(theme),
            Command::SaveSettings { settings } => self.cmd_save_settings(settings),
            Command::ReloadSettings {
                settings,
                language,
                theme,
            } => self.cmd_reload_settings(settings, language, theme),
            Command::ConfigNotice { notice } => self.cmd_config_notice(notice),
            Command::OpenWorkspace { id } => self.cmd_open_workspace(id),
            Command::CloseWorkspace => self.cmd_close_workspace(),
            Command::DeleteWorkspace { id } => self.cmd_delete_workspace(id),
            Command::SetWorkspaceBackup { id, enabled } => {
                self.cmd_set_workspace_backup(id, enabled)
            }

            // lifecycles.rs
            Command::RestoreStart { way, target } => self.cmd_restore_start(way, target),
            Command::RestoreTick => self.cmd_restore_tick(),
            Command::RestoreCancel => self.cmd_restore_cancel(),
            Command::RestoreFinish => self.cmd_restore_finish(),
            Command::CreateStart {
                name,
                member,
                threshold,
                members,
                net,
            } => self.cmd_create_start(name, member, threshold, members, net),
            Command::CreatePropose { name, agenda } => self.cmd_create_propose(name, agenda),
            Command::CreateCancel => self.cmd_create_cancel(),
            Command::CreateFinish => self.cmd_create_finish(),
            Command::NetJoinRequested {
                seat,
                member,
                identity_pk,
                proof,
                reply,
                key_package,
                generation,
            } => self.cmd_net_join_requested(
                seat,
                member,
                identity_pk,
                proof,
                reply,
                key_package,
                generation,
            ),
            Command::NetSealSigned {
                seat,
                sig,
                generation,
            } => self.cmd_net_seal_signed(seat, sig, generation),
            Command::NetTestServer { url } => self.cmd_net_test_server(url),
            Command::NetTestResult { result } => self.cmd_net_test_result(result),
            Command::NetRitualLinkReady {
                seat,
                link,
                generation,
            } => self.cmd_net_ritual_link_ready(seat, link, generation),
            Command::JoinStart { invite, member } => self.cmd_join_start(invite, member),
            Command::JoinConfirmCharter => self.cmd_join_confirm_charter(),
            Command::JoinDeclineCharter => self.cmd_join_decline_charter(),
            Command::NetJoinDeclined { seat, generation } => {
                self.cmd_net_join_declined(seat, generation)
            }
            Command::NetJoinCharterProposed {
                name,
                agenda,
                generation,
            } => self.cmd_net_join_charter_proposed(name, agenda, generation),
            Command::JoinCancel => self.cmd_join_cancel(),
            Command::NetRitualFailed { error, generation } => {
                self.cmd_net_ritual_failed(error, generation)
            }
            Command::NetJoinSealed {
                sealed,
                mls,
                generation,
            } => self.cmd_net_join_sealed(sealed, mls, generation),
            Command::NetJoinFailed { error, generation } => {
                self.cmd_net_join_failed(error, generation)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{demo_workspace_id, Screen, SessionSettings};
    use serde_json::json;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A bare actor state for unit tests of the event applier (no runtime,
    /// no storage, no config store).
    pub(crate) fn plain_state() -> State {
        let (ev_tx, _keep) = broadcast::channel::<Event>(8);
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
        State::new(
            GroupConfig::demo(),
            SessionView::default(),
            ev_tx,
            cmd_tx,
            None,
            false,
            None,
        )
    }

    async fn read_session(w: &WalletHandle) -> Box<SessionView> {
        match w.execute(Command::ReadSession).await.expect("read session") {
            Reply::Session(s) => s,
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Drive a founding to completion: the ritual runs its simulated
    /// members asynchronously (activate → key → seal), so we poll the
    /// session until the workspace is sealed (`create.run.outcome == 1`).
    async fn await_founding(w: &WalletHandle) {
        for _ in 0..600 {
            let s = read_session(w).await;
            if s.create.run.outcome == 1 {
                return;
            }
            if s.create.run.outcome == 2 {
                panic!("founding failed: {:?}", s.create.run.log);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("founding did not seal in time");
    }

    async fn read_surface(w: &WalletHandle, surface: Surface) -> molt_core::SurfaceSnapshot {
        match w
            .execute(Command::ReadState { surface })
            .await
            .expect("read state")
        {
            Reply::State(s) => s,
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The "it survives a restart" keystone: found a republic on a storage
    /// engine, write chat + a threshold-applied proposal, close, reopen —
    /// the replayed state equals the live state exactly.
    #[test]
    fn workspace_state_survives_close_and_reopen() {
        let tmp = tempfile::tempdir().expect("tmp");
        rt().block_on(async {
            let session = SessionView {
                workspaces: Vec::new(),
                settings: SessionSettings {
                    workspace_dir: tmp.path().join("workspaces").display().to_string(),
                    ..SessionSettings::default()
                },
                ..SessionView::default()
            };
            // offline sim seam, storage-backed (this test reopens from disk)
            let w = __spawn_sim_founding(GroupConfig::demo(), session, true);

            // found a 2-of-3 republic
            w.execute(Command::CreateStart {
                name: "Keystone".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                net: "tor".to_string(),
            })
            .await
            .expect("create start");
            await_founding(&w).await;
            w.execute(Command::CreateFinish).await.expect("finish");
            let s = read_session(&w).await;
            let id = s.active_workspace.clone();
            assert_eq!(id.len(), 64, "a real derived workspace id");
            let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
            assert_eq!(ws.name, "Keystone");
            // the recovery phrase is shown once in the wizard and never
            // kept in the shared session of a persisted workspace
            assert!(ws.seed.is_empty());

            // write history: chat, reaction, delete, proposal to threshold
            w.execute(Command::Chat {
                body: "first".to_string(),
                quote: None,
            })
            .await
            .expect("chat 1");
            w.execute(Command::Chat {
                body: "second".to_string(),
                quote: Some(0),
            })
            .await
            .expect("chat 2");
            w.execute(Command::ReactChat {
                index: 0,
                emoji: "👍".to_string(),
            })
            .await
            .expect("react");
            let pid = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: json!({"op":"add_note","title":"persisted"}),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            w.execute(Command::Approve { proposal: pid })
                .await
                .expect("approve");
            w.execute(Command::DeleteChat { index: 1 })
                .await
                .expect("delete");
            // two file shares: one stays available, one is removed — both
            // states must survive the reopen
            w.execute(Command::ShareFile {
                name: "charter.pdf".into(),
                size: 48_000,
                kind: "PDF".into(),
                modified: 1_751_000_000,
            })
            .await
            .expect("share");
            w.execute(Command::ShareFile {
                name: "draft.md".into(),
                size: 900,
                kind: "Text".into(),
                modified: 1_751_000_000,
            })
            .await
            .expect("share 2");
            w.execute(Command::RemoveFile { index: 3 })
                .await
                .expect("remove");

            let chat_before = read_surface(&w, Surface::Chat).await;
            let memory_before = read_surface(&w, Surface::Memory).await;

            // close (flush + closing snapshot + LOCK release), then reopen
            w.execute(Command::CloseWorkspace).await.expect("close");
            assert_eq!(read_session(&w).await.active_workspace, "");
            w.execute(Command::OpenWorkspace { id: id.clone() })
                .await
                .expect("reopen");

            let s = read_session(&w).await;
            assert_eq!(s.active_workspace, id);
            assert_eq!(s.screen, Screen::Main);
            let chat_after = read_surface(&w, Surface::Chat).await;
            let memory_after = read_surface(&w, Surface::Memory).await;
            assert_eq!(chat_after.applied, chat_before.applied);
            assert_eq!(memory_after.applied, memory_before.applied);
            assert_eq!(memory_after.pending.len(), memory_before.pending.len());

            // the file shares replay with their availability intact
            w.execute(Command::DownloadFile { index: 2 })
                .await
                .expect("kept file downloads after reopen");
            assert!(matches!(
                w.execute(Command::DownloadFile { index: 3 }).await,
                Err(MoltError::FileUnavailable(3))
            ));

            // the roster and rule replayed from the genesis event
            match w.execute(Command::Status).await.expect("status") {
                Reply::Status(st) => {
                    assert_eq!(st.member, "petra");
                    assert_eq!(st.threshold, 2);
                    assert_eq!(st.members.len(), 3);
                }
                other => panic!("unexpected: {other:?}"),
            }

            // a second engine cannot open it while we hold the LOCK
            w.execute(Command::CloseWorkspace).await.expect("close 2");
            w.execute(Command::OpenWorkspace { id: id.clone() })
                .await
                .expect("open 3");
            let session2 = SessionView {
                workspaces: read_session(&w).await.workspaces.clone(),
                settings: SessionSettings {
                    workspace_dir: tmp.path().join("workspaces").display().to_string(),
                    ..SessionSettings::default()
                },
                ..SessionView::default()
            };
            let w2 = spawn_with_storage(GroupConfig::demo(), session2);
            assert!(matches!(
                w2.execute(Command::OpenWorkspace { id: id.clone() }).await,
                Err(MoltError::WorkspaceBusy(_))
            ));

            // deleting moves the directory to .trash and closes it
            w.execute(Command::DeleteWorkspace { id: id.clone() })
                .await
                .expect("delete ws");
            let root = tmp.path().join("workspaces");
            assert!(molt_storage::find_workspace_dir(&root, &id).is_none());
            assert!(root.join(".trash").read_dir().expect("trash").count() > 0);
        });
    }

    #[test]
    fn chat_is_ungated_and_propose_rejects_chat() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            assert!(matches!(
                w.execute(Command::Chat {
                    body: "hi".into(),
                    quote: None,
                })
                .await,
                Ok(Reply::Ack)
            ));
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Chat,
                    payload: json!({"op":"x"}),
                })
                .await;
            assert!(matches!(err, Err(MoltError::ChatNotGated)));
        });
    }

    #[test]
    fn workspace_backup_toggles_and_stamps() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            // "Savings-DAO" ships without auto-backup
            w.execute(Command::SetWorkspaceBackup {
                id: demo_workspace_id("Savings-DAO"),
                enabled: true,
            })
            .await
            .expect("enable");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    let ws = s
                        .workspaces
                        .iter()
                        .find(|ws| ws.name == "Savings-DAO")
                        .expect("workspace");
                    assert!(ws.s3);
                    assert_eq!(ws.last_backup_min, 0, "enabling stamps a first backup");
                }
                other => panic!("unexpected: {other:?}"),
            }
            let err = w
                .execute(Command::SetWorkspaceBackup {
                    id: demo_workspace_id("No Such"),
                    enabled: true,
                })
                .await;
            assert!(matches!(err, Err(MoltError::UnknownWorkspace(_))));
        });
    }

    #[test]
    fn chat_reactions_toggle_and_switch() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::Chat {
                body: "gm".into(),
                quote: None,
            })
            .await
            .expect("chat");

            let read = |w: WalletHandle| async move {
                match w
                    .execute(Command::ReadState {
                        surface: Surface::Chat,
                    })
                    .await
                    .expect("read")
                {
                    Reply::State(s) => s.applied[0].clone(),
                    other => panic!("unexpected: {other:?}"),
                }
            };

            // react 👍 — my name lands under that emoji
            w.execute(Command::ReactChat {
                index: 0,
                emoji: "👍".into(),
            })
            .await
            .expect("react");
            let msg = read(w.clone()).await;
            assert_eq!(msg["reactions"]["👍"], json!(["me"]));

            // switching to 🔥 removes 👍 (one reaction per member)
            w.execute(Command::ReactChat {
                index: 0,
                emoji: "🔥".into(),
            })
            .await
            .expect("switch");
            let msg = read(w.clone()).await;
            assert!(msg["reactions"].get("👍").is_none());
            assert_eq!(msg["reactions"]["🔥"], json!(["me"]));

            // reacting with the same emoji again un-reacts; the empty map
            // disappears from the wire entirely
            w.execute(Command::ReactChat {
                index: 0,
                emoji: "🔥".into(),
            })
            .await
            .expect("unreact");
            let msg = read(w.clone()).await;
            assert!(msg.get("reactions").is_none());

            // out-of-range message
            assert!(matches!(
                w.execute(Command::ReactChat {
                    index: 7,
                    emoji: "👍".into(),
                })
                .await,
                Err(MoltError::UnknownMessage(7))
            ));
        });
    }

    #[test]
    fn file_share_lifecycle_download_until_removed() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::ShareFile {
                name: "charter.pdf".into(),
                size: 48_000,
                kind: "PDF".into(),
                modified: 1_751_000_000,
            })
            .await
            .expect("share");

            // the chat log carries exactly the metadata
            match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                })
                .await
                .expect("read")
            {
                Reply::State(s) => {
                    let f = &s.applied[0]["file"];
                    assert_eq!(f["name"], json!("charter.pdf"));
                    assert_eq!(f["size"], json!(48_000));
                    assert_eq!(f["kind"], json!("PDF"));
                    assert_eq!(f["modified"], json!(1_751_000_000));
                    assert_eq!(f["available"], json!(true));
                }
                other => panic!("unexpected: {other:?}"),
            }

            // downloadable while the sharer keeps the file …
            w.execute(Command::DownloadFile { index: 0 })
                .await
                .expect("download works while available");

            // … the sharer removes it locally → permanently unavailable
            w.execute(Command::RemoveFile { index: 0 })
                .await
                .expect("remove own share");
            assert!(matches!(
                w.execute(Command::DownloadFile { index: 0 }).await,
                Err(MoltError::FileUnavailable(0))
            ));
            assert!(matches!(
                w.execute(Command::RemoveFile { index: 0 }).await,
                Err(MoltError::FileUnavailable(0))
            ));

            // plain messages have nothing to download
            w.execute(Command::Chat {
                body: "hi".into(),
                quote: None,
            })
            .await
            .expect("chat");
            assert!(matches!(
                w.execute(Command::DownloadFile { index: 1 }).await,
                Err(MoltError::NoFile(1))
            ));
            // deleting a share message drops the share entirely
            w.execute(Command::ShareFile {
                name: "notes.md".into(),
                size: 10,
                kind: "".into(),
                modified: 0,
            })
            .await
            .expect("share 2");
            w.execute(Command::DeleteChat { index: 2 })
                .await
                .expect("delete");
            assert!(matches!(
                w.execute(Command::DownloadFile { index: 2 }).await,
                Err(MoltError::NoFile(2))
            ));
            assert!(matches!(
                w.execute(Command::DownloadFile { index: 9 }).await,
                Err(MoltError::UnknownMessage(9))
            ));
        });
    }

    #[test]
    fn chat_delete_leaves_a_tombstone() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::Chat {
                body: "secret".into(),
                quote: None,
            })
            .await
            .expect("chat");
            w.execute(Command::ReactChat {
                index: 0,
                emoji: "🔥".into(),
            })
            .await
            .expect("react");
            w.execute(Command::DeleteChat { index: 0 })
                .await
                .expect("delete");
            match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                })
                .await
                .expect("read")
            {
                Reply::State(s) => {
                    let msg = &s.applied[0];
                    assert_eq!(msg["body"], json!(""));
                    assert_eq!(msg["deleted_by"], json!("me"));
                    assert!(msg.get("reactions").is_none());
                }
                other => panic!("unexpected: {other:?}"),
            }
            assert!(matches!(
                w.execute(Command::DeleteChat { index: 9 }).await,
                Err(MoltError::UnknownMessage(9))
            ));
        });
    }

    #[test]
    fn propose_then_threshold_applies() {
        rt().block_on(async {
            // 2-of-3 demo, self_cosign => propose=1 approval, one more applies.
            let w = spawn(GroupConfig::demo(), SessionView::default());
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: json!({"op":"add_note","title":"t"}),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
            match w
                .execute(Command::ReadState {
                    surface: Surface::Memory,
                })
                .await
                .expect("read")
            {
                Reply::State(s) => {
                    assert_eq!(s.applied.len(), 1, "note should be applied at threshold");
                    assert!(s.pending.is_empty());
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn workspaces_and_restore_lifecycle_are_shared() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());

            // open by id moves to main and records the active workspace
            w.execute(Command::OpenWorkspace {
                id: demo_workspace_id("Family Office"),
            })
            .await
            .expect("open");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Main);
                    assert_eq!(s.active_workspace, demo_workspace_id("Family Office"));
                }
                other => panic!("unexpected: {other:?}"),
            }

            // deleting an unknown workspace is an error
            assert!(matches!(
                w.execute(Command::DeleteWorkspace {
                    id: demo_workspace_id("Nope"),
                })
                .await,
                Err(MoltError::UnknownWorkspace(_))
            ));

            // a plausible restore ticks to success; finishing lands in the
            // restored workspace (no completion-screen stopover)
            w.execute(Command::RestoreStart {
                way: "peer".to_string(),
                target: "smp://node/inbox".to_string(),
            })
            .await
            .expect("start");
            for _ in 0..60 {
                if w.execute(Command::RestoreTick).await.is_err() {
                    break;
                }
            }
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.restore.run.progress_pct, 100);
                    assert_eq!(s.restore.run.outcome, 1);
                    assert!(!s.restore.run.log.is_empty());
                }
                other => panic!("unexpected: {other:?}"),
            }
            w.execute(Command::RestoreFinish).await.expect("finish");
            match w.execute(Command::ReadSession).await.expect("read3") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Main);
                    assert_eq!(s.active_workspace, demo_workspace_id("Restored Republic"));
                    assert!(s
                        .workspaces
                        .iter()
                        .any(|ws| ws.name == "Restored Republic"));
                    assert_eq!(s.restore.run.step, 0);
                }
                other => panic!("unexpected: {other:?}"),
            }

            // an implausible target fails at ~45 %
            w.execute(Command::RestoreStart {
                way: "peer".to_string(),
                target: "asd".to_string(),
            })
            .await
            .expect("start2");
            for _ in 0..60 {
                if w.execute(Command::RestoreTick).await.is_err() {
                    break;
                }
            }
            match w.execute(Command::ReadSession).await.expect("read4") {
                Reply::Session(s) => {
                    assert_eq!(s.restore.run.outcome, 2);
                    assert!(s.restore.run.progress_pct < 100);
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn create_lifecycle_founds_a_republic() {
        rt().block_on(async {
            // the offline sim seam (session-only): simulated members seal the
            // ritual so the founder-side lifecycle can be tested without a
            // network — the product founds over SMP instead
            let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);

            // invalid configurations are rejected up front
            assert!(matches!(
                w.execute(Command::CreateStart {
                    name: "X".to_string(),
                    member: "me".to_string(),
                    threshold: 4,
                    members: 3,
                    net: "tor".to_string(),
                })
                .await,
                Err(MoltError::Create(_))
            ));
            for bad_n in [1_u8, 14] {
                assert!(matches!(
                    w.execute(Command::CreateStart {
                        name: "X".to_string(),
                        member: "me".to_string(),
                        threshold: 1,
                        members: bad_n,
                        net: "tor".to_string(),
                    })
                    .await,
                    Err(MoltError::Create(_))
                ));
            }

            // a valid founding runs the ritual: two seats, each activated
            // and sealed by a simulated member, then the workspace is born
            w.execute(Command::CreateStart {
                name: "Chess Club".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                net: "tor".to_string(),
            })
            .await
            .expect("start");
            // "Enter republic" is refused until every seat is sealed
            assert!(matches!(
                w.execute(Command::CreateFinish).await,
                Err(MoltError::Create(_))
            ));
            await_founding(&w).await;
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_eq!(s.create.run.outcome, 1);
                    assert_eq!(s.create.seed.split(' ').count(), 24);
                    assert_eq!(s.create.seats.len(), 2);
                    for seat in &s.create.seats {
                        assert_eq!(seat.state, 2, "every seat sealed");
                        assert!(!seat.member.is_empty(), "the member named itself");
                        let info =
                            molt_core::InviteInfo::parse(&seat.link).expect("invite parses");
                        assert_eq!(info.republic, "Chess Club");
                        assert_eq!(info.inviter, "petra");
                    }
                    // the log carries the real ritual events, not a fake anim
                    assert!(s.create.run.log.iter().any(|l| l.contains("activated invite")));
                    assert!(s.create.run.log.iter().any(|l| l.contains("signed the roster")));
                    assert!(s.create.run.log.iter().any(|l| l.contains("workspace created")));
                }
                other => panic!("unexpected: {other:?}"),
            }
            w.execute(Command::CreateFinish).await.expect("finish");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Main);
                    assert_eq!(s.active_workspace, demo_workspace_id("Chess Club"));
                    let ws = s
                        .workspaces
                        .iter()
                        .find(|w| w.name == "Chess Club")
                        .expect("workspace added");
                    assert_eq!(ws.detail, "2-of-3");
                    assert_eq!(ws.members.len(), 3);
                    assert_eq!(s.create, molt_core::CreateState::default());
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn join_requires_a_joinable_link() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());

            // empty, plain text, and a bare preview link (no transport
            // handover) are all rejected — a real join needs a link that
            // carries the SMP address
            for bad in [
                "  ",
                "not-an-invite",
                "molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa",
            ] {
                assert!(
                    matches!(
                        w.execute(Command::JoinStart {
                            invite: bad.to_string(),
                            member: "petra".to_string(),
                        })
                        .await,
                        Err(MoltError::Join(_))
                    ),
                    "should reject `{bad}`"
                );
            }
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => assert_eq!(s.join, molt_core::JoinState::default()),
                other => panic!("unexpected: {other:?}"),
            }

            // a real founding link (with the transport handover) starts the
            // join: the joiner's own recovery phrase is shown and the run is in
            // progress (the background ritual over a bogus host will fail, but
            // we cancel before that lands)
            let link = crate::FoundingInvite {
                info: molt_core::InviteInfo {
                    republic: "Chess Club".to_string(),
                    threshold: 2,
                    members: 2,
                    inviter: "walter".to_string(),
                    ticket: "ab".repeat(32),
                },
                server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid"
                    .to_string(),
                queue_id: "cd".repeat(12),
                wrap: "ef".repeat(32),
                seat: 0,
            }
            .render();
            w.execute(Command::JoinStart {
                invite: link,
                member: "petra".to_string(),
            })
            .await
            .expect("a joinable link starts the join");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Join);
                    assert_eq!(s.join.republic, "Chess Club");
                    assert_eq!((s.join.rule_m, s.join.rule_n), (2, 2));
                    assert!(!s.join.seed.is_empty(), "the joiner's recovery phrase is shown");
                    assert_eq!(s.join.run.outcome, 0, "still joining");
                }
                other => panic!("unexpected: {other:?}"),
            }
            // cancel stops the run and invalidates the background task's result
            w.execute(Command::JoinCancel).await.expect("cancel");
        });
    }

    /// A joinable link with a bogus host (the background ritual task will fail,
    /// but our directly-injected commands are processed first, in-process).
    fn joinable_link() -> String {
        crate::FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "R".to_string(),
                threshold: 2,
                members: 2,
                inviter: "walter".to_string(),
                ticket: "ab".repeat(32),
            },
            server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid"
                .to_string(),
            queue_id: "cd".repeat(12),
            wrap: "ef".repeat(32),
            seat: 0,
        }
        .render()
    }

    fn valid_sealed_roster() -> molt_core::SealedRoster {
        use molt_core::{MemberIdentity, RosterAttestation};
        let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
        let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
        let identities = vec![
            MemberIdentity { member: "founder".to_string(), identity_pk: pk_a },
            MemberIdentity { member: "petra".to_string(), identity_pk: pk_b },
        ];
        let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
        let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "");
        let attestations = vec![
            RosterAttestation { member: "founder".to_string(), sig: molt_storage::identity_sign(&sk_a, &table) },
            RosterAttestation { member: "petra".to_string(), sig: molt_storage::identity_sign(&sk_b, &table) },
        ];
        molt_core::SealedRoster {
            name: "R".to_string(),
            republic_id,
            rule_m: 2,
            rule_n: 2,
            roster: vec!["founder".to_string(), "petra".to_string()],
            identities,
            attestations,
            agenda: String::new(),
        }
    }

    #[test]
    fn join_failure_surfaces_into_the_run_and_drops_stale_reports() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::JoinStart { invite: joinable_link(), member: "petra".to_string() })
                .await
                .expect("start");
            // a stale-generation failure is ignored
            w.execute(Command::NetJoinFailed { error: "old".to_string(), generation: Some(999) })
                .await
                .expect("stale");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => assert_eq!(s.join.run.outcome, 0, "stale failure ignored"),
                other => panic!("unexpected: {other:?}"),
            }
            // the current-generation failure surfaces into the run
            w.execute(Command::NetJoinFailed { error: "boom".to_string(), generation: Some(1) })
                .await
                .expect("fail");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.join.run.outcome, 2);
                    assert!(s.join.run.log.iter().any(|l| l.contains("boom")));
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn join_seals_into_the_republic_from_a_valid_roster() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::JoinStart { invite: joinable_link(), member: "petra".to_string() })
                .await
                .expect("start");
            let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
            w.execute(Command::NetJoinSealed { sealed, mls: String::new(), generation: Some(1) })
                .await
                .expect("sealed");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Main, "entered the republic");
                    assert_eq!(s.join, molt_core::JoinState::default(), "join reset");
                    assert!(s.workspaces.iter().any(|ws| ws.name == "R"), "workspace added");
                }
                other => panic!("unexpected: {other:?}"),
            }

            // a garbage roster fails the join rather than materialising anything
            let w2 = spawn(GroupConfig::demo(), SessionView::default());
            w2.execute(Command::JoinStart { invite: joinable_link(), member: "x".to_string() })
                .await
                .expect("start2");
            w2.execute(Command::NetJoinSealed { sealed: "{".to_string(), mls: String::new(), generation: Some(1) })
                .await
                .expect("bad");
            match w2.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => assert_eq!(s.join.run.outcome, 2, "garbage roster fails"),
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn leaving_the_create_screen_abandons_an_in_flight_founding() {
        rt().block_on(async {
            // manual seam: the ritual opens but no member joins, so it stays
            // open (it cannot seal and hijack the session behind our back)
            let (w, _material_rx) =
                __spawn_manual_founding(GroupConfig::demo(), SessionView::default());
            w.execute(Command::CreateStart {
                name: "Duet".to_string(),
                member: "founder".to_string(),
                threshold: 2,
                members: 2,
                net: "tor".to_string(),
            })
            .await
            .expect("start");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_ne!(s.create, molt_core::CreateState::default(), "founding is open")
                }
                other => panic!("unexpected: {other:?}"),
            }
            // navigating away abandons it (the session is in-memory)
            w.execute(Command::Navigate { screen: Screen::Choice }).await.expect("nav");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Choice);
                    assert_eq!(s.create, molt_core::CreateState::default(), "founding abandoned");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    #[test]
    fn select_view_is_validated_shared_state() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::SelectView {
                surface: Surface::Quests,
                view: "my-quests".to_string(),
            })
            .await
            .expect("select");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_eq!(s.surface, Surface::Quests);
                    assert_eq!(s.view, "my-quests");
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
                surface: Surface::Wallet,
            })
            .await
            .expect("select2");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => assert_eq!(s.view, "balance"),
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
                anonymity: "nym".to_string(),
                ..SessionSettings::default()
            };
            w.execute(Command::SaveSettings {
                settings: settings.clone(),
            })
            .await
            .expect("save");

            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Settings);
                    assert_eq!(s.settings.anonymity, "nym");
                    assert_eq!(s.notice, "saved");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }
}
