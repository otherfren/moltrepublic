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
//! The approval logic is honest on both of its paths: a **chain-governed**
//! republic (every ritual-founded workspace) runs real signed m-of-n
//! threshold governance over the MLS-encrypted mesh ([`chain`]); every
//! other context (the solo boot group, legacy pre-chain workspaces) runs a
//! **single-operator** path where this node records at most its OWN
//! approval — a proposal applies only when that one real vote meets the
//! threshold (honest 1-of-1), and a repeated `Approve` is refused instead
//! of being counted as an invented peer.
//!
//! The implementation is split by concern: [`chat`] (the ungated surface,
//! typed messages, reactions, deletion), [`net`] (the `molt-net` glue: the
//! log-backed outbox feed, the inbound `Net*` handlers, and the loopback
//! demo mesh whose peers replaced the old reply simulator), [`proposals`]
//! (the gated propose/approve/apply machine and snapshots), [`session`]
//! (navigation, settings, workspaces) and [`lifecycles`] (the three
//! engine-run mocks: restore / create / join over one `RunCore`).

mod backup;
mod chain;
mod compaction;
mod chat;
mod configstore;
mod events;
mod founding;
mod lifecycles;
mod net;
mod nostr_ritual;
mod proposals;
mod recovery;
mod relay_msg;
mod session;
mod transfer;

use std::collections::HashMap;
use std::path::PathBuf;

pub use configstore::ConfigStoreHandle;
#[doc(hidden)]
pub use chain::{verify_chain, ChainHead};
#[doc(hidden)]
pub use recovery::RecoveryInvite;
#[doc(hidden)]
pub use recovery::{run_rejoin, RejoinOutcome};
#[doc(hidden)]
pub use founding::{
    make_seat_proof, run_ritual_member, verify_seat_proof, FoundingInvite, InviteMaterial,
    Ratifier, RitualTransport,
};
pub use net::{CmdSink, FileStateStore, StorageLog};

use molt_core::{
    ChatMessage, Command, Event, GroupConfig, MemberId, MessageId, MoltError, ProposalRecord,
    Reply, SessionScope, SessionView, Surface, WorkspaceId,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Capacity of the inbound command queue.
const CMD_QUEUE: usize = 128;
/// Capacity of the outbound event broadcast.
const EVENT_QUEUE: usize = 512;
/// Period of the presence-aging ticker (30 s): pill states move at
/// minutes-scale thresholds ([`molt_core::MemberInfo::ONLINE_SECS`] /
/// `STALE_SECS`), so a slow beat is plenty and keeps the actor quiet.
const PRESENCE_TICK_MS: u64 = 30_000;

/// The delivery-guarantee beat (`Command::NetDeliveryTick`): due-ACK flush +
/// debounced persists. 1 s keeps the real ack latency at debounce+1s ≈ 4 s,
/// safely inside the sender's 30 s resend timer.
const DELIVERY_TICK_MS: u64 = 1_000;

/// The honest gap error for the paths not yet over Nostr: founding and join
/// run over relays since N4a, but **recovery** (the total-loss rejoin) does
/// not — the recovery-link v2 story is N4b. Surfaced through recovery's
/// EXISTING failure path (the recovery notice) — never a fake success.
pub(crate) const NO_TRANSPORT_YET: &str = "recovery over Nostr is not built yet — the \
     recovery-link v2 flow lands with N4b (founding and join already run over relays)";

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
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, None, true, None, Some(tx), false, false, None, false, None);
    (handle, rx)
}

/// Like [`__spawn_manual_founding`], but the founder also runs the post-founding
/// **mesh bootstrap** ([`State::ritual_bootstrap`]) after sealing: it exchanges
/// mesh announcements with the joined member(s) over the loopback star and
/// persists the assembled direct mesh + post-bootstrap MLS into its workspace.
/// The seam the two-instance bootstrap test uses to exercise the founder side.
#[doc(hidden)]
pub fn __spawn_manual_founding_bootstrap(
    config: GroupConfig,
    session: SessionView,
) -> (WalletHandle, std::sync::mpsc::Receiver<Vec<founding::InviteMaterial>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, None, true, None, Some(tx), false, true, None, false, None);
    (handle, rx)
}

/// Like [`__spawn_manual_founding_bootstrap`], but also installs the recovery
/// material sink: a coordinator that founds + bootstraps its mesh here and then
/// mints a recovery link hands the minted queue's transport handover out on the
/// second receiver, so a *separate* returning-member side can drive the request.
/// The recovery two-instance dev test uses this.
#[doc(hidden)]
#[allow(clippy::type_complexity)]
pub fn __spawn_manual_founding_bootstrap_recoverable(
    config: GroupConfig,
    session: SessionView,
) -> (
    WalletHandle,
    std::sync::mpsc::Receiver<Vec<founding::InviteMaterial>>,
    std::sync::mpsc::Receiver<recovery::RecoveryMaterial>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (rtx, rrx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let handle = spawn_actor(
        config, session, cmd_tx, cmd_rx, None, true, None, Some(tx), false, true, Some(rtx), false, None,
    );
    (handle, rx, rrx)
}

/// Engine with the **demo loopback mesh** enabled ([`State::demo_mesh`]):
/// on a session-only context the roster's other members run as real
/// loopback peers with canned-reply brains, so transport-path tests get a
/// deterministic answering mesh without a network. The product never runs
/// it — a production engine (every public spawner) keeps the seam OFF and
/// spawns no fake peers, ever.
#[doc(hidden)]
pub fn __spawn_demo_mesh(config: GroupConfig, session: SessionView) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(
        config, session, cmd_tx, cmd_rx, None, false, None, None, false, false, None, true, None,
    )
}

/// Storage-backed engine that resumes a persisted mesh over the GIVEN
/// transport — the loopback reopen seam for the hard-kill tests (their hub
/// survives in the test process, like a real server would). The product
/// never uses it: a production reopen has no transport to rebuild in this
/// build and opens honestly detached.
#[doc(hidden)]
pub fn __spawn_with_reopen_transport(
    config: GroupConfig,
    session: SessionView,
    transport: founding::RitualTransport,
) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(
        config,
        session,
        cmd_tx,
        cmd_rx,
        None,
        true,
        None,
        None,
        false,
        false,
        None,
        false,
        Some(transport),
    )
}

/// Storage-backed engine whose founding runs in the offline **sim** seam:
/// the founder's node simulates the other members over the loopback hub
/// (fast, deterministic, no network) — for founder-side sealing tests. The
/// product never uses this: a production founding fails honestly until N4's
/// Nostr transport lands.
#[doc(hidden)]
pub fn __spawn_sim_founding(config: GroupConfig, session: SessionView, persist: bool) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(config, session, cmd_tx, cmd_rx, None, persist, None, None, true, false, None, false, None)
}

/// Storage-backed engine with the post-founding **mesh bootstrap** ON — the
/// production joiner configuration (`spawn_with_config` sets the same flag),
/// as a seam for multi-instance tests whose joiners must assemble a real
/// direct mesh after `JoinStart`.
#[doc(hidden)]
pub fn __spawn_with_storage_bootstrap(config: GroupConfig, session: SessionView) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(config, session, cmd_tx, cmd_rx, None, true, None, None, false, true, None, false, None)
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
        // the real product runs the post-founding mesh bootstrap: the founder
        // (here) and the joiner (cmd_join_start) exchange announcements, then
        // each stands its runtime supervisor up over the direct mesh — live
        // peer-to-peer MLS chat the moment the republic is founded
        true,
        None,
        // the production engine: the demo-mesh test seam stays OFF — no
        // context ever spawns simulated peers here
        false,
        None,
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
    spawn_actor(config, session, cmd_tx, cmd_rx, store, persist, None, None, false, false, None, false, None)
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
    ritual_sim: bool,
    ritual_bootstrap: bool,
    recovery_material_sink: Option<std::sync::mpsc::Sender<recovery::RecoveryMaterial>>,
    demo_mesh: bool,
    reopen_seam: Option<founding::RitualTransport>,
) -> WalletHandle {
    let (ev_tx, _keep) = broadcast::channel::<Event>(EVENT_QUEUE);

    let mut state = State::new(config, session, ev_tx.clone(), cmd_tx.clone(), store, persist, net);
    state.ritual_material_sink = ritual_material_sink;
    state.ritual_sim = ritual_sim;
    state.ritual_bootstrap = ritual_bootstrap;
    state.recovery_material_sink = recovery_material_sink;
    state.demo_mesh = demo_mesh;
    state.reopen_seam = reopen_seam;
    // the presence ticker lives as long as the actor: it re-ages the member
    // pills from their real last-seen stamps (net.rs::cmd_net_presence_tick)
    state.spawn_ticker_every(Command::NetPresenceTick, PRESENCE_TICK_MS);
    // the delivery-guarantee beat: due ACKs + the debounced accept-window /
    // live-ratchet persists — fast, because the 30 s presence tick alone
    // stretched the "3 s" ack debounce into a 33 s latency (E7 review)
    state.spawn_ticker_every(Command::NetDeliveryTick, DELIVERY_TICK_MS);
    // the backup ticker lives as long as the actor: its synchronous decide
    // pass spawns real upload tasks for due workspaces (backup.rs; story 12)
    state.spawn_ticker_every(Command::BackupTick, backup::BACKUP_TICK_MS);
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
    /// The neutral, content-derived republic id from the genesis — kept so the
    /// persistent chain can compute `approval_bytes` at runtime (empty on a
    /// pre-republic genesis).
    pub(crate) republic_id: String,
    /// The genesis envelope's timestamp — the founding date surfaced by
    /// `Status` (0 on a pre-ritual/demo genesis).
    pub(crate) founded_ts: u64,
}

/// An in-flight founder mesh bootstrap: its ritual generation, the founded
/// workspace id the assembled mesh persists into, and the channel carrying
/// members' announcement ciphertext into the off-actor bootstrap task.
type FounderMeshIn = (u64, molt_core::WorkspaceId, mpsc::UnboundedSender<(u32, String)>);

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
    /// Chat-bus id → position in [`State::chat`] — the O(1) lookup every
    /// id-addressed verb resolves through, and the wire's duplicate-id gate.
    /// Runtime state, NOT persisted: rebuilt on ingest/restore. It indexes
    /// the WHOLE log: legacy (nil-id) entries get their deterministic id
    /// synthesized at the ingest choke points (P4), so no message in state
    /// ever carries a nil id.
    pub(crate) chat_pos: HashMap<MessageId, usize>,
    /// This workspace has physically dropped expired chat (WP4a compaction),
    /// so chat POSITIONS are no longer meaningful and a legacy index-addressed
    /// op must be ignored instead of mis-applied ([`State::chat_target`]).
    /// Persisted in the snapshot dump (`EngineStateDump::chat_pruned`) and
    /// sticky — an older, un-pruned snapshot never clears it.
    pub(crate) chat_pruned: bool,
    /// When the last log-compaction round ran (WP4a F8: one per day). 0 =
    /// none yet this session, so the first eligible tick runs one.
    pub(crate) compacted_at: u64,
    /// Per sender, how many of its chat messages compaction dropped — the
    /// carry-forward that keeps synthesized legacy ids stable across a prune
    /// ([`molt_core::EngineStateDump::chat_pruned_counts`]).
    pub(crate) chat_pruned_counts: std::collections::BTreeMap<MemberId, u64>,
    /// The P6 parking buffer: wire reactions/deletes/file-removes whose
    /// target message has not arrived yet (cross-sender ordering is not
    /// guaranteed), drained when the `Chat` lands. Bounded; runtime-only —
    /// never persisted, a restart loses parked refs (ephemerality is fine).
    pub(crate) parked: net::ParkedRefs,
    /// MY shares: message id → local source path (runtime mirror of
    /// `prefs.shared_files`; NEVER wire, NEVER log — the paths would leak
    /// this node's filesystem layout).
    pub(crate) share_paths: HashMap<MessageId, std::path::PathBuf>,
    /// Requester-side live download status per share (runtime-only; feeds
    /// [`molt_core::UploadView::download`]).
    pub(crate) downloads: HashMap<MessageId, molt_core::DownloadView>,
    /// Sharer-side serve throttle: at most 2 concurrent uploads; further
    /// requests queue on the semaphore instead of saturating the uplink.
    pub(crate) file_serve_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// Applied transition log per gated surface: `(proposal id, payload)`
    /// pairs — one source for the payload and its origin, so the snapshot's
    /// parallel id track can never drift. `None` = origin unknown (restored
    /// from a pre-id dump).
    pub(crate) applied: HashMap<Surface, Vec<(Option<u64>, Value)>>,
    /// Every known proposal — stored as the schema type
    /// ([`molt_core::ProposalRecord`]), so snapshots need no conversion.
    pub(crate) proposals: HashMap<u64, ProposalRecord>,
    pub(crate) next_id: u64,
    /// The next event seq (strictly monotonic per workspace; reset on close).
    pub(crate) next_seq: u64,
    /// Identity of the open workspace, from its genesis event (None = no
    /// workspace open; the demo `GroupConfig` fills in).
    pub(crate) replica: Option<ReplicaState>,
    /// This node's identity signing key for the open workspace, loaded from the
    /// sealed `transport.state` (set at founding/join). Signs governance
    /// approvals for the persistent chain; `None` when no chain-aware workspace
    /// is open (or a pre-chain workspace).
    pub(crate) identity_sk: Option<molt_storage::SigningKey>,
    /// The republic's persistent commit-block chain — the converged, verified
    /// governance record (`docs/chain/persistent_chain.md`). Block 0 is the
    /// founding; empty when no chain-aware workspace is open.
    pub(crate) chain: Vec<molt_core::ChainBlock>,
    /// The verified head of [`State::chain`] (`None` = empty chain).
    pub(crate) chain_head: Option<chain::ChainHead>,
    /// WP4b: a SERVED blob awaiting its anchor block (runtime-only, never
    /// persisted — re-served on the next catch-up if lost).
    pub(crate) pending_served_blob: Option<molt_core::CheckpointState>,
    /// WP4b: the checkpoint blob a PRUNED holder anchors on — `Some` once
    /// history below a sealed checkpoint was dropped locally; [`State::chain`]
    /// then starts with the checkpoint block instead of the genesis.
    pub(crate) checkpoint_blob: Option<molt_core::CheckpointState>,
    /// The gated surfaces' applied logs **derived from the chain** — a separate
    /// projection from the legacy log-driven [`State::applied`] so the two never
    /// collide: a single-operator workspace keeps its counted governance in
    /// `applied` (chain genesis-only → this stays empty), while real
    /// threshold-committed governance lands here. Reads combine both. Re-folded
    /// wholesale on every chain change, so a re-base is free. Same
    /// `(proposal id, payload)` shape as [`State::applied`]; the id is always
    /// present here (every `Applied` block names its proposal).
    pub(crate) chain_applied: HashMap<Surface, Vec<(Option<u64>, Value)>>,
    /// Ephemeral per-proposal signature collection for chain governance
    /// (keyed by proposal id; never persisted, rebuilt from gossip). Once a
    /// proposal gathers m distinct signatures the committer seals a block.
    pub(crate) pending_sigs: HashMap<u64, chain::PendingApproval>,
    /// The exact [`molt_core::ChainChange`] each open proposal is voting on
    /// (keyed by proposal id) — so approvers sign, and the committer seals, the
    /// SAME bytes for any change kind (a gated `Applied` or a `Membership`
    /// re-admission). Ephemeral, rebuilt from the proposal gossip.
    pub(crate) proposal_changes: HashMap<u64, molt_core::ChainChange>,
    /// Out-of-order catch-up buffer: blocks received ahead of our head (keyed
    /// by height), applied as the head advances to meet them. Ephemeral.
    pub(crate) pending_blocks: std::collections::BTreeMap<u64, molt_core::ChainBlock>,
    /// The height a catch-up request is currently outstanding for (dedups the
    /// request while a gap persists; cleared when the head reaches it).
    pub(crate) catchup_from: Option<u64>,
    /// Recoveries this node is coordinating, keyed by the returning **member**
    /// (so the trigger fires whether this node commits the Restored block or
    /// receives it): the fresh KeyPackage + reply queue, kept until the Restored
    /// block commits and the coordinator re-keys the group + sends the Welcome.
    pub(crate) pending_recovery: HashMap<String, chain::PendingRecovery>,
    /// Recovery tickets this node has minted and is still listening for — the
    /// spend-once guard. A ticket is inserted when a recovery link is minted and
    /// removed the moment a valid request spends it, so a replayed request on a
    /// live recovery queue finds a dead ticket and is dropped.
    pub(crate) recovery_tickets: std::collections::HashSet<String>,
    /// Members whose recovery re-key just completed and whose **mesh announce**
    /// the coordinator therefore expects on the recovery queue (dynamic mesh
    /// membership) — armed in `coordinator_rekey`, disarmed when the announce
    /// is handled. The recovery queue can never re-point any OTHER member's
    /// links.
    pub(crate) recovery_mesh_window: std::collections::HashSet<MemberId>,
    /// Per-member cooldown for mesh extensions (`member → now_secs of the
    /// last accepted announce`): folding a link in costs every peer a full
    /// supervisor teardown+rebuild+fsync, so a member re-announcing inside
    /// the window is ignored — one rotation per member per minute is ample,
    /// and it caps the churn a misbehaving member can inflict.
    pub(crate) mesh_extension_at: std::collections::HashMap<MemberId, u64>,
    /// Per SENDER: which of that sender's log seqs this engine has accepted
    /// (delivery guarantee §4.2 — the envelope-level dedup twin of the mesh
    /// ACK payload). Loaded from `transport.state` at open, mutated on every
    /// authenticated wire delivery, persisted debounced + at close. Active-
    /// workspace scope — [`State::reset_workspace_state`] clears it.
    pub(crate) accepted: std::collections::BTreeMap<MemberId, molt_core::AcceptedWindow>,
    /// Whether [`Self::accepted`] changed since it was last persisted (the
    /// debounced save on the presence tick checks this).
    pub(crate) accepted_dirty: bool,
    /// Per SENDER: when a delivery ACK to them is due (`member →
    /// presence_now deadline`). Every accepted OR duplicate delivery arms
    /// this (a dup means the previous ack was lost or lags — re-acking
    /// closes that loop); the presence tick flushes what is due. Runtime-
    /// only, workspace scope.
    pub(crate) ack_due: std::collections::HashMap<MemberId, u64>,
    /// The seq of this node's last OWN ackable envelope (`MlsCommit`s
    /// excluded) — the tail of the G7 in-order chain `make_env` stamps as
    /// `prev_seq`. Derived in `apply` (live records and the open replay),
    /// runtime-only, workspace scope.
    pub(crate) last_own_ackable: u64,
    /// G7 in-order hold: per SENDER, wire envelopes whose `prev_seq` is not
    /// yet in the accept window (`seq → (envelope, parked_at)`). Deliberately
    /// NOT accept-marked while parked: the sender keeps them unacked and a
    /// crash simply re-earns them via the resend machinery. Drained in seq
    /// order as predecessors land; a stale entry (pathological chain) is
    /// released loudly by the delivery tick after
    /// [`crate::net::ORDERED_PARK_GIVEUP_SECS`]. Runtime-only, workspace
    /// scope.
    #[allow(clippy::type_complexity)]
    pub(crate) ordered_park:
        std::collections::HashMap<MemberId, std::collections::BTreeMap<u64, (molt_core::EventEnvelope, u64)>>,
    /// `presence_now` of the last persisted accept-window save (debounce).
    pub(crate) accepted_saved_at: u64,
    /// `presence_now` of the last debounced live MLS-ratchet merge into
    /// `transport.state` (§4.6 / E6 — bounds the hard-kill regression).
    pub(crate) mls_persisted_at: u64,
    /// Members whose sends keep failing (outbox backoff): their pill is
    /// pinned unreachable (state 2) regardless of how fresh the last-seen
    /// stamp is, until the next real sighting clears the pin. Runtime-only,
    /// active-workspace scope — [`State::reset_workspace_state`] clears it
    /// at the close/switch boundary so a pin never leaks into the next.
    pub(crate) net_unreachable: std::collections::HashSet<MemberId>,
    /// Inbound legs currently down (member → reason), reported by the
    /// resubscribe watchdog — drives `NetHealth::Degraded` (Stage B).
    pub(crate) net_link_down: std::collections::BTreeMap<MemberId, String>,
    /// Outbound legs whose sends keep failing (member → reason) — set by
    /// `NetSendFailed`, cleared by `NetSendOk` (Stage B).
    pub(crate) net_send_stuck: std::collections::BTreeMap<MemberId, String>,
    /// `presence_now` of the last wire-crossing frame the engine emitted to a
    /// real mesh (stamped in [`State::record`]). Read by the debounced live
    /// MLS-ratchet persist (`persist_mls_if_due` — "did anything go out since
    /// the last snapshot?"). Runtime-only; reset with the workspace.
    pub(crate) last_mesh_out: u64,
    /// Are clearnet relays activated for THIS session? Runtime-only **on
    /// purpose** — it is never persisted, so every start re-arms the gate and
    /// no clearnet packet leaves before the user acts again
    /// (`docs/transport/relay_pool.md` §3). Onion relays are unaffected.
    pub(crate) clearnet_session: bool,
    /// Presence clock **test seam** (same posture as [`State::demo_mesh`]):
    /// `None` in every production context — presence stamping/aging then
    /// runs on the shared [`now_secs`] clock; tests pin it to age pills
    /// deterministically.
    pub(crate) clock_override: Option<u64>,
    /// Generation of the newest backup-bucket listing request
    /// ([`molt_core::Command::NetListBackups`]): bumped per request and on a
    /// backup-target settings change, so a stale off-actor result can never
    /// overwrite a newer table (last-REQUEST wins, not last arrival).
    pub(crate) s3_list_gen: u64,
    /// Generation of the newest Tor connectivity probe
    /// ([`molt_core::Command::NetTestTor`]): bumped per request and on an
    /// anonymity settings change, so a probe still in flight can never land
    /// as a verdict about a configuration it did not test.
    pub(crate) tor_test_gen: u64,
    /// Workspaces with a backup upload task in flight (story 12): the
    /// ticker never spawns a second task for one while its first is out,
    /// and Done/Failed clear the mark. Runtime-only.
    pub(crate) backup_inflight: std::collections::HashSet<WorkspaceId>,
    /// Last CONFIRMED upload timestamp per workspace, in memory (runtime-
    /// only). Stamped on `NetBackupDone` and consulted as a fallback in the
    /// ticker's due-check + label re-aging, so a `prefs.last_backup` that
    /// could not be persisted (read-only dir) does not trigger a full-blob
    /// re-upload every minute forever (review finding).
    pub(crate) backup_last_done: std::collections::HashMap<WorkspaceId, u64>,
    /// Restore incarnation (story 13): bumped per `RestoreStart`/cancel so
    /// a superseded task's late progress/staged/failed reports are dropped.
    pub(crate) restore_generation: u64,
    /// The in-flight restore fetch+stage task (aborted on cancel — the
    /// download is inbound-only, so abort is safe).
    pub(crate) restore_task: Option<tokio::task::JoinHandle<()>>,
    /// The slot the restore task parks its staged blob in
    /// (`lifecycles.rs::restore_task` → `cmd_net_restore_staged`): the
    /// staging handle never rides a Command — a forged internal command
    /// without a really-staged blob can materialize nothing. Replaced per
    /// restore incarnation.
    pub(crate) restore_staging:
        std::sync::Arc<std::sync::Mutex<Option<molt_storage::import::ImportStaging>>>,
    /// The collision policy of the restore in flight (design P2).
    pub(crate) restore_replace: bool,
    /// The workspace a successful restore materialized — what
    /// `RestoreFinish` opens (detached).
    pub(crate) restored_id: Option<WorkspaceId>,
    /// The open workspace's storage writer (None = nothing open, or a
    /// session-only workspace on a storage-less engine).
    pub(crate) active: Option<ActiveStorage>,
    /// The transport runtime: the open workspace's real mesh supervisor
    /// (T2), or — on the [`State::demo_mesh`] test seam only — the demo
    /// loopback mesh. `None` in every other context (production runs no
    /// transport without an open, mesh-backed workspace).
    pub(crate) net: Option<net::NetRuntime>,
    /// The founding-ritual runtime (present only while a founding is in
    /// flight — the workspace does not exist yet).
    pub(crate) net_ritual: Option<founding::RitualRuntime>,
    /// Whether THIS ritual already published its `Seal` 445. `maybe_seal` is
    /// reachable from two call sites, and a second Seal would both
    /// double-report and advance the MLS ratchet past the snapshot
    /// `finalize_founding` takes. Reset with the ritual.
    pub(crate) seal_published: bool,
    /// Seal signatures collected so far this ritual (founder first at
    /// finalize).
    pub(crate) ritual_attestations: Vec<molt_core::RosterAttestation>,
    /// When set, the founding ritual does NOT spawn simulated members;
    /// instead it hands the per-seat [`founding::InviteMaterial`] out on
    /// this channel so a *second* engine instance runs the member side.
    /// Only the two-instance dev test installs this.
    pub(crate) ritual_material_sink:
        Option<std::sync::mpsc::Sender<Vec<founding::InviteMaterial>>>,
    /// The recovery twin of [`Self::ritual_material_sink`]: when set, the
    /// recovery link-mint hands the minted queue's transport handover out on
    /// this channel so a *second* engine can run the returning-member side.
    /// Only the two-instance recovery dev test installs it; a real mint reports
    /// the link to the operator instead.
    pub(crate) recovery_material_sink:
        Option<std::sync::mpsc::Sender<recovery::RecoveryMaterial>>,
    /// Offline **test seam only** ([`__spawn_sim_founding`]): found over the
    /// loopback hub with simulated members. The product never sets it — a
    /// production founding fails honestly until N4's Nostr transport lands;
    /// this keeps the founder-side sealing a fast, deterministic, offline test.
    pub(crate) ritual_sim: bool,
    /// Loopback demo-mesh **test seam only** ([`__spawn_demo_mesh`]): when
    /// set, a session-only context (and a workspace flagged
    /// `prefs.simulated_members`) runs the roster's other members as
    /// loopback peers with canned-reply brains. The product never sets it —
    /// a production engine spawns no fake peers, in no context; the flag in
    /// the prefs stays parsed but inert.
    pub(crate) demo_mesh: bool,
    /// Reopen **test seam only** ([`__spawn_with_reopen_transport`]): resume a
    /// persisted mesh over THIS transport. Lets the loopback tests drive a
    /// literal hard-kill + reopen of a full engine (their hub survives in the
    /// test, like a real server would). The product never sets it — and until
    /// N4's Nostr transport lands, a production reopen has no transport to
    /// rebuild, so a mesh-bearing workspace opens honestly detached.
    pub(crate) reopen_seam: Option<founding::RitualTransport>,
    /// Opt-in: after sealing, the founder runs the post-founding **mesh
    /// bootstrap** over the star (exchanges [`molt_net::mesh::MeshAnnounce`]s
    /// with the members, assembles the direct mesh, persists it). Off by
    /// default so the existing seal-only paths are byte-for-byte unchanged; the
    /// two-instance loopback test turns it on.
    pub(crate) ritual_bootstrap: bool,
    /// While a founder bootstrap is in flight: its ritual `generation`, the
    /// **founded workspace id** it will persist the mesh into, and the channel
    /// feeding members' [`Command::NetMeshAnnounced`] ciphertext into the
    /// off-actor bootstrap task. The id binds the eventual persist to the exact
    /// workspace, so a late bootstrap can never overwrite a workspace the
    /// operator has since switched to. `None` outside a bootstrap.
    pub(crate) founder_mesh_in: Option<FounderMeshIn>,
    /// The founder keeps a clone of the ritual transport across its mesh
    /// bootstrap so the runtime supervisor can reuse it once the mesh is
    /// assembled (on the loopback hub the queues can't be reconstructed).
    /// Consumed when the real net is built (`NetMeshReady`); cleared on
    /// teardown.
    pub(crate) runtime_transport: Option<founding::RitualTransport>,
    /// The **joiner's** equivalent: the off-actor join task hands its ritual
    /// transport (which owns the bootstrap queues' receive credentials) back
    /// through this slot just before it reports `NetJoinSealed`, so the runtime
    /// supervisor reuses the same instance. A fresh per-join `Arc` (replaced in
    /// `cmd_join_start`) isolates a stale task's late fill from a new join.
    pub(crate) join_transport: std::sync::Arc<std::sync::Mutex<Option<founding::RitualTransport>>>,
    /// The running Nostr member-join task (N4a) — aborted on cancel or on a
    /// restarted join (its generation-guarded commands would be dropped
    /// anyway; aborting also releases its relay sockets).
    pub(crate) join_task: Option<tokio::task::JoinHandle<()>>,
    /// Monotonic mesh/ritual-incarnation counter: `Net*` commands carry
    /// the generation of the runtime that sent them, and commands from a
    /// torn-down runtime are dropped (a delivery queued behind a workspace
    /// switch must not land in the new context's log).
    pub(crate) net_generation: u64,
    /// Scope counter for the **open workspace** (bumped by
    /// `reset_workspace_state`, i.e. on every workspace switch/close). The
    /// recovery recv loops and mesh-extension tasks are scoped to the open
    /// workspace, NOT to a mesh incarnation: a mesh-extension REBUILD bumps
    /// `net_generation` mid-recovery and must not orphan an outstanding
    /// recovery link or a concurrent extension — only a workspace switch may.
    pub(crate) net_scope: u64,
    /// A separate incarnation counter for the **join** flow (an off-actor
    /// join, possibly long-running). Kept apart from `net_generation` so a
    /// concurrent founding/mesh change can neither be mistaken for a stale
    /// join nor silently drop a live one.
    pub(crate) join_generation: u64,
    /// A separate incarnation counter for the **recovery** flow (an off-actor
    /// rejoin) — the twin of [`State::join_generation`].
    pub(crate) recover_generation: u64,
    /// While a recovery is in flight: the parsed recovery link + the phrase
    /// the rejoin task runs with. `cmd_net_recover_sealed` re-derives the seat
    /// identity from the phrase (the ritual salts it with a workspace-id
    /// string, so it must NOT be re-derived from the member handle) and checks
    /// the served chain against the link. `None` outside a recovery.
    pub(crate) recover_ctx: Option<(recovery::RecoveryInvite, String)>,
    /// The **rejoiner's** transport slot — the twin of
    /// [`State::join_transport`]: the off-actor rejoin task parks a clone of
    /// its transport here (its `Arc` owns the re-established mesh queues'
    /// receive credentials), so `cmd_net_recover_sealed` can stand the runtime
    /// supervisor up over the recovered mesh. Replaced per `RecoverStart`.
    pub(crate) recover_transport:
        std::sync::Arc<std::sync::Mutex<Option<founding::RitualTransport>>>,
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
            chat_pos: HashMap::new(),
            chat_pruned: false,
            chat_pruned_counts: std::collections::BTreeMap::new(),
            compacted_at: 0,
            parked: net::ParkedRefs::new(),
            share_paths: HashMap::new(),
            downloads: HashMap::new(),
            file_serve_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            applied,
            proposals: HashMap::new(),
            next_id: 1,
            next_seq: 1,
            replica: None,
            identity_sk: None,
            chain: Vec::new(),
            chain_head: None,
            pending_served_blob: None,
            checkpoint_blob: None,
            chain_applied: HashMap::new(),
            pending_sigs: HashMap::new(),
            proposal_changes: HashMap::new(),
            pending_blocks: std::collections::BTreeMap::new(),
            catchup_from: None,
            pending_recovery: HashMap::new(),
            recovery_tickets: std::collections::HashSet::new(),
            recovery_mesh_window: std::collections::HashSet::new(),
            mesh_extension_at: std::collections::HashMap::new(),
            accepted: std::collections::BTreeMap::new(),
            accepted_dirty: false,
            accepted_saved_at: 0,
            mls_persisted_at: 0,
            ack_due: std::collections::HashMap::new(),
            last_own_ackable: 0,
            ordered_park: std::collections::HashMap::new(),
            net_unreachable: std::collections::HashSet::new(),
            net_link_down: std::collections::BTreeMap::new(),
            net_send_stuck: std::collections::BTreeMap::new(),
            last_mesh_out: 0,
            // the STORED decision is what a fresh process starts from
            // (ADR-0004 amendment): an operator who acknowledged clearnet
            // exposure is not asked again on every restart
            clearnet_session: session.settings.clearnet_relays_enabled,
            clock_override: None,
            s3_list_gen: 0,
            tor_test_gen: 0,
            backup_inflight: std::collections::HashSet::new(),
            backup_last_done: std::collections::HashMap::new(),
            restore_generation: 0,
            restore_task: None,
            restore_staging: std::sync::Arc::new(std::sync::Mutex::new(None)),
            restore_replace: false,
            restored_id: None,
            active: None,
            net,
            net_ritual: None,
            seal_published: false,
            ritual_attestations: Vec::new(),
            ritual_material_sink: None,
            recovery_material_sink: None,
            ritual_sim: false,
            demo_mesh: false,
            reopen_seam: None,
            ritual_bootstrap: false,
            founder_mesh_in: None,
            runtime_transport: None,
            join_transport: std::sync::Arc::new(std::sync::Mutex::new(None)),
            join_task: None,
            net_generation: 0,
            net_scope: 0,
            join_generation: 0,
            recover_generation: 0,
            recover_ctx: None,
            recover_transport: std::sync::Arc::new(std::sync::Mutex::new(None)),
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

    /// The presence clock: seconds since the unix epoch — the shared
    /// [`now_secs`] clock, unless a test pinned [`State::clock_override`].
    /// Every presence stamp, aging pass and activity-trio read runs on
    /// THIS accessor so tests can age pills deterministically.
    pub(crate) fn presence_now(&self) -> u64 {
        self.clock_override.unwrap_or_else(now_secs)
    }

    /// The 0/1/2 presence pill for one member, the single derivation every
    /// surface shares: THIS node is always online (it is the one running —
    /// it never hears itself on the wire, so its stamp would otherwise age
    /// out); a send-failure pin forces offline; everyone else ages from
    /// their real last-seen stamp.
    pub(crate) fn presence_of(&self, member: &str, last_seen: u64, now: u64) -> u8 {
        if member == self.member() {
            0
        } else if self.net_unreachable.contains(member) {
            2
        } else {
            molt_core::presence_state(now, last_seen)
        }
    }

    /// The member roster: the open workspace's, else the boot group's.
    pub(crate) fn roster(&self) -> Vec<MemberId> {
        self.replica
            .as_ref()
            .map(|r| r.roster.clone())
            .unwrap_or_else(|| self.config.members.clone())
    }

    /// The open workspace's content-derived republic id (empty when no
    /// chain-aware workspace is open) — the salt `approval_bytes` needs.
    pub(crate) fn republic_id(&self) -> String {
        self.replica
            .as_ref()
            .map(|r| r.republic_id.clone())
            .unwrap_or_default()
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

    /// Subscribe to the actor's event stream — unit tests observe which
    /// commands actually push a session (e.g. the presence push contract).
    #[cfg(test)]
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.ev_tx.subscribe()
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
            Command::Chat {
                body,
                quote,
                channel,
            } => self.cmd_chat(body, quote, channel),
            Command::ReactChat { id, emoji } => self.cmd_react_chat(id, emoji),
            Command::MarkRead { ids } => self.cmd_mark_read(ids),
            Command::DeleteChat { id } => self.cmd_delete_chat(id),
            Command::ShareFile { path, channel } => self.cmd_share_file(path, channel),
            Command::DownloadFile { id, dest } => self.cmd_download_file(id, dest),
            Command::RemoveFile { id } => self.cmd_remove_file(id),
            // file-transfer task feedback (engine-internal, scope-guarded)
            Command::NetFileShared {
                name,
                size,
                kind,
                modified,
                checksum,
                path,
                channel,
                generation,
            } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_file_shared(name, size, kind, modified, checksum, path, channel)
            }
            Command::NetFileShareFailed {
                name,
                reason,
                generation,
            } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_file_share_failed(name, reason)
            }
            Command::NetFileRequestReady { id, ct, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_file_request_ready(id, ct)
            }
            Command::NetFileProgress {
                id,
                transferred,
                total,
                generation,
            } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                let percent = (transferred * 100)
                    .checked_div(total)
                    .map_or(100, |p| u8::try_from(p).unwrap_or(100));
                self.set_download_phase(id, molt_core::TransferPhase::Progress { percent });
                Ok(Reply::Ack)
            }
            Command::NetFileDone { id, path, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.set_download_phase(id, molt_core::TransferPhase::Done { path });
                Ok(Reply::Ack)
            }
            Command::NetFileFailed { id, reason, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.set_download_phase(id, molt_core::TransferPhase::Failed { reason });
                Ok(Reply::Ack)
            }

            // proposals.rs
            Command::Propose { surface, payload } => self.cmd_propose(surface, payload),
            Command::Approve { proposal } => self.cmd_approve(proposal),
            Command::Decline { proposal } => self.cmd_decline(proposal),
            Command::ReadState { surface, channel, view } => {
                // the view key is shared vocabulary (`Surface::views`, the
                // same list `select_view` validates against); an unknown
                // key must error, never silently read the wrong window
                if let Some(v) = &view {
                    if !surface.views().iter().any(|(k, _)| k == v) {
                        return Err(MoltError::UnknownView(surface, v.clone()));
                    }
                }
                Ok(Reply::State(self.snapshot(surface, channel, view.as_deref())))
            }
            Command::ProposeCheckpoint => self.cmd_propose_checkpoint(),
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
            Command::ReadMembers => Ok(Reply::Members { members: self.members_view() }),
            Command::ReadUploads => Ok(Reply::Uploads { uploads: self.uploads_view() }),
            Command::ReadChain => self.cmd_read_chain(),

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
            Command::NetLinkUp { member, generation } => {
                self.cmd_net_link_up(member, generation)
            }
            Command::NetLinkDown {
                member,
                reason,
                generation,
            } => self.cmd_net_link_down(member, reason, generation),
            Command::NetSendOk { member, generation } => {
                self.cmd_net_send_ok(member, generation)
            }
            Command::NetPresenceTick => self.cmd_net_presence_tick(),
            Command::NetDeliveryTick => self.cmd_net_delivery_tick(),

            // session.rs
            Command::ReadSession => {
                // the relay pool's DERIVED state is computed here, at the one
                // place a session view leaves the actor, so it can never go
                // stale against `settings.relays` / the session's clearnet lock
                let mut view = self.session.clone();
                view.relays = molt_core::relay::pool_status(
                    &self.session.settings.relays,
                    self.clearnet_session,
                );
                view.clearnet_session = self.clearnet_session;
                Ok(Reply::Session(Box::new(view)))
            }
            Command::RelayAdd { url } => self.cmd_relay_add(url),
            Command::RelayRemove { url } => self.cmd_relay_remove(url),
            Command::RelayMove { url, up } => self.cmd_relay_move(url, up),
            Command::RelayConfirm { url, accept_clearnet } => {
                self.cmd_relay_confirm(url, accept_clearnet)
            }
            Command::RelayRevoke { url } => self.cmd_relay_revoke(url),
            Command::RelayClearnetSession { unlock } => self.cmd_relay_clearnet_session(unlock),
            Command::Navigate { screen } => self.cmd_navigate(screen),
            Command::SelectSurface { surface } => self.cmd_select_surface(surface),
            Command::SelectView { surface, view } => self.cmd_select_view(surface, view),
            Command::SetLanguage { lang } => self.cmd_set_language(lang),
            Command::SetTheme { theme } => self.cmd_set_theme(theme),
            Command::SetReadReceipts { enabled } => self.cmd_set_read_receipts(enabled),
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
            Command::EncryptWorkspace { id, phrase } => self.cmd_encrypt_workspace(id, phrase),
            Command::DecryptWorkspace { id, phrase } => self.cmd_decrypt_workspace(id, phrase),
            Command::SetWorkspaceBackup { id, enabled } => {
                self.cmd_set_workspace_backup(id, enabled)
            }
            Command::ExportWorkspace { id, dest, passphrase } => {
                self.cmd_export_workspace(id, dest, passphrase)
            }
            Command::NetExportDone { id, dest, bytes, skipped } => {
                self.cmd_net_export_done(id, dest, bytes, skipped)
            }
            Command::NetExportFailed { id, error } => self.cmd_net_export_failed(id, error),

            // backup.rs (story 12: the auto-backup ticker + manual trigger)
            Command::BackupNow { id } => self.cmd_backup_now(id),
            Command::BackupTick => self.cmd_backup_tick(),
            Command::NetBackupDone {
                id,
                ts,
                object,
                bytes,
                prune_error,
            } => self.cmd_net_backup_done(id, ts, object, bytes, prune_error),
            Command::NetBackupFailed { id, error } => self.cmd_net_backup_failed(id, error),

            // lifecycles.rs
            Command::RestoreStart {
                way,
                target,
                secret,
                replace,
            } => self.cmd_restore_start(way, target, secret, replace),
            Command::NetRestoreProgress {
                pct,
                line,
                generation,
            } => self.cmd_net_restore_progress(pct, line, generation),
            Command::NetRestoreStaged { generation } => self.cmd_net_restore_staged(generation),
            Command::NetRestoreFailed { error, generation } => {
                self.cmd_net_restore_failed(error, generation)
            }
            Command::RestoreCancel => self.cmd_restore_cancel(),
            Command::RestoreFinish => self.cmd_restore_finish(),
            Command::CreateStart {
                name,
                member,
                threshold,
                members,
            } => self.cmd_create_start(name, member, threshold, members),
            Command::CreatePropose { name, agenda } => self.cmd_create_propose(name, agenda),
            Command::CreateCancel => self.cmd_create_cancel(),
            Command::CreateFinish => self.cmd_create_finish(),
            Command::NetJoinRequested {
                seat,
                member,
                identity_pk,
                nostr_pk,
                proof,
                reply,
                sender_npub,
                key_package,
                generation,
            } => self.cmd_net_join_requested(
                seat,
                member,
                identity_pk,
                nostr_pk,
                proof,
                reply,
                sender_npub,
                key_package,
                generation,
            ),
            Command::NetSealSigned {
                seat,
                sig,
                from,
                generation,
            } => self.cmd_net_seal_signed(seat, sig, from, generation),
            Command::RecoverInviteStart { member } => self.cmd_recover_invite_start(member),
            Command::RecoverStart { link, phrase } => self.cmd_recover_start(link, phrase),
            Command::NetRecoverSealed {
                member,
                chain,
                mls,
                mesh,
                generation,
            } => self.cmd_net_recover_sealed(member, chain, mls, mesh, generation),
            Command::NetRecoverFailed { error, generation } => {
                self.cmd_net_recover_failed(error, generation)
            }
            Command::NetRecoverAnnounced { ct, generation } => {
                self.cmd_net_recover_announced(ct, generation)
            }
            Command::NetMeshExtended { link, generation } => {
                self.cmd_net_mesh_extended(link, generation)
            }
            Command::NetRecoverRequested {
                member,
                identity_pk,
                key_package,
                ticket,
                seat_proof,
                reply,
                generation,
            } => self.cmd_net_recover_requested(
                member,
                identity_pk,
                key_package,
                ticket,
                seat_proof,
                reply,
                generation,
            ),
            Command::NetRecoverLinkReady {
                member,
                link,
                generation,
            } => self.cmd_net_recover_link_ready(member, link, generation),
            Command::NetRecoverLinkFailed {
                member,
                reason,
                ticket,
                generation,
            } => self.cmd_net_recover_link_failed(member, reason, ticket, generation),
            Command::NetTestS3 {
                endpoint,
                access_key,
                secret_key,
                bucket,
            } => self.cmd_net_test_s3(endpoint, access_key, secret_key, bucket),
            Command::NetTestS3Result { result } => self.cmd_net_test_s3_result(result),
            Command::NetTestTor {
                network,
                mode,
                port,
            } => self.cmd_net_test_tor(network, mode, port),
            Command::NetTestTorResult { result, generation } => {
                self.cmd_net_test_tor_result(result, generation)
            }
            Command::NetListBackups => self.cmd_net_list_backups(),
            Command::NetListBackupsResult {
                result,
                objects,
                generation,
            } => self.cmd_net_list_backups_result(result, objects, generation),
            Command::NetRitualLinkReady {
                seat,
                link,
                generation,
            } => self.cmd_net_ritual_link_ready(seat, link, generation),
            Command::JoinStart { invite, member } => self.cmd_join_start(invite, member),
            Command::JoinConfirmCharter => self.cmd_join_confirm_charter(),
            Command::JoinDeclineCharter => self.cmd_join_decline_charter(),
            Command::NetJoinDeclined { seat, from, generation } => {
                self.cmd_net_join_declined(seat, from, generation)
            }
            Command::NetJoinAccepted { generation } => self.cmd_net_join_accepted(generation),
            Command::NetJoinCharterProposed {
                name,
                agenda,
                generation,
            } => self.cmd_net_join_charter_proposed(name, agenda, generation),
            Command::JoinCancel => self.cmd_join_cancel(),
            Command::NetRitualNote { note, generation } => {
                self.cmd_net_ritual_note(note, generation)
            }
            Command::NetJoinNote { note, generation } => self.cmd_net_join_note(note, generation),
            Command::NetRitualPublished {
                what,
                accepted,
                failed,
                generation,
            } => self.cmd_net_ritual_published(&what, &accepted, &failed, generation),
            Command::NetRitualFailed { error, generation } => {
                self.cmd_net_ritual_failed(error, generation)
            }
            Command::NetJoinSealed {
                sealed,
                mls,
                mesh,
                nostr_sk,
                relays,
                rotation_seed,
                generation,
            } => self.cmd_net_join_sealed(sealed, mls, mesh, nostr_sk, relays, rotation_seed, generation),
            Command::NetJoinFailed { error, generation } => {
                self.cmd_net_join_failed(error, generation)
            }
            Command::NetMeshAnnounced {
                seat,
                ct,
                generation,
            } => self.cmd_net_mesh_announced(seat, ct, generation),
            Command::NetMeshReady {
                mesh,
                mls_snapshot,
                generation,
            } => self.cmd_net_mesh_ready(mesh, mls_snapshot, generation),
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
            .execute(Command::ReadState {
                surface,
                channel: None,
                view: None,
            })
            .await
            .expect("read state")
        {
            Reply::State(s) => s,
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The stable id a chat snapshot row carries, parsed back into the type.
    fn msg_id(v: &serde_json::Value) -> MessageId {
        v["id"]
            .as_str()
            .expect("message id on the wire")
            .parse()
            .expect("valid message id")
    }

    /// Write `content` to `dir/name` and share it — awaiting the share
    /// message (posting is async: it appears once the off-actor hash
    /// completes). Returns the share's stable id.
    async fn share_temp_file(
        w: &WalletHandle,
        dir: &std::path::Path,
        name: &str,
        content: &[u8],
    ) -> MessageId {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write share source");
        w.execute(Command::ShareFile {
            path: path.display().to_string(),
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("share");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snap = read_surface(w, Surface::Chat).await;
            if let Some(row) = snap
                .applied
                .iter()
                .find(|m| m["file"]["name"] == serde_json::json!(name))
            {
                return msg_id(row);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the share message for {name} never posted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Poll until `path` exists with exactly `content`.
    async fn await_file(path: &std::path::Path, content: &[u8]) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::fs::read(path).is_ok_and(|b| b == content) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} never landed with the expected bytes",
                path.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
            // the recovery phrase stays in the entry (decision 2026-07-15:
            // stored device-sealed, shown by the Open screen's details
            // panel while the workspace is at-rest-unencrypted)
            assert_eq!(ws.seed.split(' ').count(), 24, "the real phrase: {}", ws.seed);

            // write history: chat, reaction, delete, proposal to threshold
            // (all chat verbs address by stable id since the chat bus)
            w.execute(Command::Chat {
                body: "first".to_string(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat 1");
            let first_id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);
            w.execute(Command::Chat {
                body: "second".to_string(),
                quote: Some(first_id),
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat 2");
            let second_id = msg_id(&read_surface(&w, Surface::Chat).await.applied[1]);
            w.execute(Command::ReactChat {
                id: first_id,
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
            w.execute(Command::DeleteChat { id: second_id })
                .await
                .expect("delete");
            // two file shares (real temp files): one stays available, one
            // is removed — both states must survive the reopen
            let src_dir = tmp.path().join("sources");
            std::fs::create_dir_all(&src_dir).expect("src dir");
            let kept_share_id =
                share_temp_file(&w, &src_dir, "charter.pdf", b"the sealed charter").await;
            let removed_share_id =
                share_temp_file(&w, &src_dir, "draft.md", b"a draft to remove").await;
            w.execute(Command::RemoveFile {
                id: removed_share_id,
            })
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

            // the file shares replay with their availability intact — and
            // stay addressable by the SAME ids after the reopen. The kept
            // share's source path came back via prefs (this node keeps
            // serving/copying across restarts): downloading the own share
            // is an honest local copy into the destination
            let dest_dir = tmp.path().join("downloads");
            std::fs::create_dir_all(&dest_dir).expect("dest dir");
            w.execute(Command::DownloadFile {
                id: kept_share_id,
                dest: Some(dest_dir.display().to_string()),
            })
            .await
            .expect("kept file downloads after reopen");
            await_file(&dest_dir.join("charter.pdf"), b"the sealed charter").await;
            assert!(matches!(
                w.execute(Command::DownloadFile {
                    id: removed_share_id,
                    dest: None,
                })
                .await,
                Err(MoltError::FileUnavailable(i)) if i == removed_share_id
            ));

            // the roster, rule, and founding date replayed from the genesis
            match w.execute(Command::Status).await.expect("status") {
                Reply::Status(st) => {
                    assert_eq!(st.member, "petra");
                    assert_eq!(st.threshold, 2);
                    assert_eq!(st.members.len(), 3);
                    assert!(
                        st.founded_ts > 0,
                        "the genesis envelope's timestamp is the founding date"
                    );
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

    /// Story 9: the manual export drives a REAL `molt-export-v1` blob onto
    /// disk (decryptable at the storage layer), enforces the passphrase
    /// policy synchronously, and reports an unwritable path as an honest
    /// error — never a fake success.
    #[test]
    fn manual_export_writes_a_real_blob_and_fails_honestly() {
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
            let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
            w.execute(Command::CreateStart {
                name: "Blob Republic".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
            })
            .await
            .expect("create start");
            await_founding(&w).await;
            w.execute(Command::CreateFinish).await.expect("finish");
            let id = read_session(&w).await.active_workspace.clone();
            w.execute(Command::Chat {
                body: "history to back up".to_string(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");

            let pass = "correct horse battery".to_string();
            // passphrase policy: engine-enforced, synchronous, honest
            let err = w
                .execute(Command::ExportWorkspace {
                    id: id.clone(),
                    dest: tmp.path().join("x.molt.enc").display().to_string(),
                    passphrase: "neunchars".to_string(),
                })
                .await
                .expect_err("9 chars must be refused");
            assert!(err.to_string().contains("at least 10"), "{err}");
            // unknown workspace is refused before anything runs
            assert!(w
                .execute(Command::ExportWorkspace {
                    id: "77".repeat(32),
                    dest: tmp.path().join("x.molt.enc").display().to_string(),
                    passphrase: pass.clone(),
                })
                .await
                .is_err());

            // the real export, into a directory that does not exist yet
            let dest = tmp.path().join("backups").join("blob.molt.enc");
            w.execute(Command::ExportWorkspace {
                id: id.clone(),
                dest: dest.display().to_string(),
                passphrase: pass.clone(),
            })
            .await
            .expect("export kickoff");
            let sv = read_session(&w).await;
            assert_eq!(sv.export.workspace, id);
            let outcome = await_export(&w).await;
            assert_eq!(outcome.result, "ok", "export must succeed: {outcome:?}");
            assert!(outcome.bytes > 0);
            let blob = std::fs::read(&dest).expect("blob on disk");
            assert_eq!(outcome.bytes, u64::try_from(blob.len()).expect("len"));
            // the blob decrypts and verifies at the storage layer
            let a = molt_storage::export::read_export(
                &mut blob.as_slice(),
                &molt_storage::export::ExportSecret::passphrase(pass.clone()),
            )
            .expect("blob decrypts");
            assert_eq!(a.header.workspace_id, id);
            assert!(a.entries.iter().any(|e| e.path == "manifest.toml"));
            assert!(a.entries.iter().any(|e| e.path == "log/000001.mlog"));
            assert!(
                a.entries.iter().all(|e| e.path != "transport.state"),
                "live transport state must never be exported"
            );
            // no stray .part file remains
            assert!(std::fs::read_dir(dest.parent().expect("parent"))
                .expect("dir")
                .all(|e| !e
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")));

            // honest failure: the destination's parent is a FILE — the task
            // must report the real error, not a fake success
            let blocker = tmp.path().join("blocker");
            std::fs::write(&blocker, b"in the way").expect("blocker");
            w.execute(Command::ExportWorkspace {
                id: id.clone(),
                dest: blocker.join("nope.molt.enc").display().to_string(),
                passphrase: pass,
            })
            .await
            .expect("kickoff acks; the failure arrives async");
            let outcome = await_export(&w).await;
            assert!(
                outcome.result.starts_with("error: "),
                "unwritable path must fail honestly, got: {outcome:?}"
            );
        });
    }


    /// Poll the session until the in-flight export settles (~Argon2-bounded).
    async fn await_export(w: &WalletHandle) -> molt_core::ExportState {
        for _ in 0..600 {
            let sv = read_session(w).await;
            if !sv.export.running && !sv.export.result.is_empty() {
                return sv.export;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("export did not settle in time");
    }

    /// **The story-10 keystone:** at-rest sealing is real, phrase-verified
    /// and derived from the directory (design §8.2, engine level). Found a
    /// republic on a storage engine, close it, seal it with the real phrase
    /// — the key material is gone from disk, a fresh scan (≈ restart)
    /// reports the sealed state, open refuses honestly, a wrong phrase
    /// changes nothing, and the right phrase brings everything back.
    #[test]
    fn at_rest_sealing_is_real_verified_and_survives_a_restart() {
        let tmp = tempfile::tempdir().expect("tmp");
        rt().block_on(async {
            let root = tmp.path().join("workspaces");
            let session = SessionView {
                workspaces: Vec::new(),
                settings: SessionSettings {
                    workspace_dir: root.display().to_string(),
                    ..SessionSettings::default()
                },
                ..SessionView::default()
            };
            let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
            w.execute(Command::CreateStart {
                name: "Vaulted".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
            })
            .await
            .expect("create start");
            await_founding(&w).await;
            w.execute(Command::CreateFinish).await.expect("finish");
            let s = read_session(&w).await;
            let id = s.active_workspace.clone();
            let phrase = s.workspaces.iter().find(|x| x.id == id).expect("entry").seed.clone();
            assert_eq!(phrase.split(' ').count(), 24, "the real phrase");
            let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");

            // the ACTIVE workspace cannot be sealed from under itself
            assert!(matches!(
                w.execute(Command::EncryptWorkspace {
                    id: id.clone(),
                    phrase: phrase.clone(),
                })
                .await,
                Err(MoltError::WorkspaceBusy(_))
            ));
            w.execute(Command::CloseWorkspace).await.expect("close");

            // encrypt requires phrase PROOF: a foreign (valid) phrase and an
            // empty one are refused, and nothing is deleted
            let foreign = molt_storage::generate_seed_phrase().expect("gen");
            assert!(w
                .execute(Command::EncryptWorkspace {
                    id: id.clone(),
                    phrase: foreign.clone(),
                })
                .await
                .is_err());
            assert!(w
                .execute(Command::EncryptWorkspace {
                    id: id.clone(),
                    phrase: String::new(),
                })
                .await
                .is_err());
            assert!(dir.join("keys/workspace.key").exists(), "nothing deleted");
            assert!(dir.join("keys/seed.sealed").exists());

            // the real phrase seals: key material gone, session honest
            w.execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: phrase.clone(),
            })
            .await
            .expect("encrypt");
            assert!(!dir.join("keys/workspace.key").exists(), "key removed");
            assert!(!dir.join("keys/seed.sealed").exists(), "seed removed");
            {
                let s = read_session(&w).await;
                let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
                assert!(ws.encrypted);
                assert!(ws.seed.is_empty(), "no phrase to show while sealed");
                assert!(ws.members.is_empty(), "no roster to show while sealed");
            }
            assert!(matches!(
                w.execute(Command::OpenWorkspace { id: id.clone() }).await,
                Err(MoltError::WorkspaceEncrypted(_))
            ));

            // restart persistence: a FRESH scan of the directory (what boot
            // does) derives the sealed state — no session memory involved
            let entries = molt_storage::scan_workspaces(&root);
            assert_eq!(entries.len(), 1);
            assert!(entries[0].info().encrypted, "a restart still sees it sealed");
            // …and a second engine booted from that scan refuses the open
            let session2 = SessionView {
                workspaces: entries.iter().map(|e| e.info()).collect(),
                settings: SessionSettings {
                    workspace_dir: root.display().to_string(),
                    ..SessionSettings::default()
                },
                ..SessionView::default()
            };
            let w2 = spawn_with_storage(GroupConfig::demo(), session2);
            assert!(matches!(
                w2.execute(Command::OpenWorkspace { id: id.clone() }).await,
                Err(MoltError::WorkspaceEncrypted(_))
            ));

            // wrong phrase on decrypt: hard error, still sealed on disk
            assert!(w
                .execute(Command::DecryptWorkspace {
                    id: id.clone(),
                    phrase: foreign,
                })
                .await
                .is_err());
            assert!(!dir.join("keys/workspace.key").exists(), "still sealed");
            assert!(
                molt_storage::scan_workspaces(&root)[0].info().encrypted,
                "still sealed after the failed attempt"
            );

            // the right phrase unseals; the entry gets its details back and
            // the workspace opens and replays
            w.execute(Command::DecryptWorkspace {
                id: id.clone(),
                phrase: phrase.clone(),
            })
            .await
            .expect("decrypt");
            {
                let s = read_session(&w).await;
                let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
                assert!(!ws.encrypted);
                assert_eq!(ws.seed, phrase, "the stored phrase is shown again");
                assert!(!ws.members.is_empty(), "the roster is back");
            }
            w.execute(Command::OpenWorkspace { id: id.clone() })
                .await
                .expect("open after decrypt");
            match w.execute(Command::Status).await.expect("status") {
                Reply::Status(st) => {
                    assert_eq!(st.member, "petra");
                    assert_eq!(st.members.len(), 3, "the history replayed");
                }
                other => panic!("unexpected: {other:?}"),
            }
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
                    channel: molt_core::ChannelRef::default(),
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
            let before = match w.execute(Command::ReadSession).await.expect("read0") {
                Reply::Session(s) => s
                    .workspaces
                    .iter()
                    .find(|ws| ws.name == "Savings-DAO")
                    .expect("workspace")
                    .last_backup_min,
                other => panic!("unexpected: {other:?}"),
            };
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
                    // honest stamps (story 12): enabling persists the pref and
                    // NOTHING else — the stamp moves only on a confirmed
                    // upload (NetBackupDone), never on the toggle
                    assert_eq!(
                        ws.last_backup_min, before,
                        "enabling must never invent a backup stamp"
                    );
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
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");
            let id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);

            let read = |w: WalletHandle| async move {
                match w
                    .execute(Command::ReadState {
                        surface: Surface::Chat,
                        channel: None,
                        view: None,
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
                id,
                emoji: "👍".into(),
            })
            .await
            .expect("react");
            let msg = read(w.clone()).await;
            assert_eq!(msg["reactions"]["👍"], json!(["me"]));

            // switching to 🔥 removes 👍 (one reaction per member)
            w.execute(Command::ReactChat {
                id,
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
                id,
                emoji: "🔥".into(),
            })
            .await
            .expect("unreact");
            let msg = read(w.clone()).await;
            assert!(msg.get("reactions").is_none());

            // an unknown message id
            let unknown = MessageId([9u8; 16]);
            assert!(matches!(
                w.execute(Command::ReactChat {
                    id: unknown,
                    emoji: "👍".into(),
                })
                .await,
                Err(MoltError::UnknownMessage(i)) if i == unknown
            ));
        });
    }

    #[test]
    fn file_share_lifecycle_download_until_removed() {
        rt().block_on(async {
            use sha2::Digest as _;
            let tmp = tempfile::tempdir().expect("tmp");
            let w = spawn(GroupConfig::demo(), SessionView::default());
            let content: &[u8] = b"the sealed charter, for real this time";
            let share_id = share_temp_file(&w, tmp.path(), "charter.pdf", content).await;

            // the chat log carries the REAL metadata the engine derived —
            // including the streamed sha256 (the download anchor)
            let snap = read_surface(&w, Surface::Chat).await;
            let f = &snap.applied[0]["file"];
            assert_eq!(f["name"], json!("charter.pdf"));
            assert_eq!(f["size"], json!(content.len()));
            assert_eq!(f["kind"], json!("PDF"));
            assert!(f["modified"].as_u64().is_some_and(|m| m > 0));
            assert_eq!(f["available"], json!(true));
            let want_sha = hex::encode(sha2::Sha256::digest(content));
            assert_eq!(f["checksum"], json!(want_sha), "the real sha256 is log-anchored");

            // downloading the OWN share is an honest local copy — and a
            // name collision resolves as "name (1).ext", never overwrites
            let dest = tmp.path().join("dl");
            std::fs::create_dir_all(&dest).expect("dest");
            w.execute(Command::DownloadFile {
                id: share_id,
                dest: Some(dest.display().to_string()),
            })
            .await
            .expect("download works while available");
            await_file(&dest.join("charter.pdf"), content).await;
            w.execute(Command::DownloadFile {
                id: share_id,
                dest: Some(dest.display().to_string()),
            })
            .await
            .expect("second download");
            await_file(&dest.join("charter (1).pdf"), content).await;

            // … the sharer removes it locally → permanently unavailable
            w.execute(Command::RemoveFile { id: share_id })
                .await
                .expect("remove own share");
            assert!(matches!(
                w.execute(Command::DownloadFile { id: share_id, dest: None }).await,
                Err(MoltError::FileUnavailable(i)) if i == share_id
            ));
            assert!(matches!(
                w.execute(Command::RemoveFile { id: share_id }).await,
                Err(MoltError::FileUnavailable(i)) if i == share_id
            ));

            // plain messages have nothing to download
            w.execute(Command::Chat {
                body: "hi".into(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");
            let plain_id = msg_id(
                read_surface(&w, Surface::Chat)
                    .await
                    .applied
                    .iter()
                    .find(|m| m["body"] == json!("hi"))
                    .expect("plain message"),
            );
            assert!(matches!(
                w.execute(Command::DownloadFile { id: plain_id, dest: None }).await,
                Err(MoltError::NoFile(i)) if i == plain_id
            ));
            // deleting a share message drops the share entirely
            let share2_id = share_temp_file(&w, tmp.path(), "notes.md", b"notes").await;
            w.execute(Command::DeleteChat { id: share2_id })
                .await
                .expect("delete");
            assert!(matches!(
                w.execute(Command::DownloadFile { id: share2_id, dest: None }).await,
                Err(MoltError::NoFile(i)) if i == share2_id
            ));
            let unknown = MessageId([9u8; 16]);
            assert!(matches!(
                w.execute(Command::DownloadFile { id: unknown, dest: None }).await,
                Err(MoltError::UnknownMessage(i)) if i == unknown
            ));
            // sharing an unreadable path fails honestly (no share message,
            // an honest notice instead)
            w.execute(Command::ShareFile {
                path: tmp.path().join("missing.bin").display().to_string(),
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("kickoff succeeds; the failure surfaces async");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let s = read_session(&w).await;
                if s.notice.starts_with("share-failed:missing.bin") {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the share failure never surfaced: {:?}",
                    s.notice
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
    }

    #[test]
    fn chat_delete_leaves_a_tombstone() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::Chat {
                body: "secret".into(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");
            let id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);
            w.execute(Command::ReactChat {
                id,
                emoji: "🔥".into(),
            })
            .await
            .expect("react");
            w.execute(Command::DeleteChat { id })
                .await
                .expect("delete");
            match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                    channel: None,
                    view: None,
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
            let unknown = MessageId([9u8; 16]);
            assert!(matches!(
                w.execute(Command::DeleteChat { id: unknown }).await,
                Err(MoltError::UnknownMessage(i)) if i == unknown
            ));
        });
    }

    /// Chat bus Stage A pin: every chat verb addresses messages by their
    /// stable id — send, react, delete, and a quote all work by id; an
    /// unknown id is `UnknownMessage`.
    #[test]
    fn chat_commands_address_by_id() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::Chat {
                body: "root".into(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");
            // the demo peers may chat back mid-test — address rows by body
            let row = |snap: &molt_core::SurfaceSnapshot, body: &str| {
                snap.applied
                    .iter()
                    .find(|m| m["body"] == json!(body))
                    .cloned()
                    .unwrap_or_else(|| panic!("no chat row with body {body:?}"))
            };
            let snap = read_surface(&w, Surface::Chat).await;
            let root_id = msg_id(&row(&snap, "root"));
            assert!(!root_id.is_nil(), "a new message carries a minted id");

            // react by id
            w.execute(Command::ReactChat {
                id: root_id,
                emoji: "👍".into(),
            })
            .await
            .expect("react by id");
            let snap = read_surface(&w, Surface::Chat).await;
            assert_eq!(row(&snap, "root")["reactions"]["👍"], json!(["me"]));

            // quote by id survives in the log (as quote_id; the legacy
            // numeric quote is never written by new code)
            w.execute(Command::Chat {
                body: "reply".into(),
                quote: Some(root_id),
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("quoted reply");
            let snap = read_surface(&w, Surface::Chat).await;
            let reply = row(&snap, "reply");
            assert_eq!(
                reply["quote_id"],
                json!(root_id.to_string()),
                "the quote rides as a stable id"
            );
            assert!(
                reply.get("quote").is_none(),
                "new code never writes the legacy index quote"
            );
            let reply_id = msg_id(&reply);

            // delete by id
            w.execute(Command::DeleteChat { id: reply_id })
                .await
                .expect("delete by id");
            let snap = read_surface(&w, Surface::Chat).await;
            let tombstone = snap
                .applied
                .iter()
                .find(|m| m["id"] == json!(reply_id.to_string()))
                .expect("the deleted row remains as a tombstone");
            assert_eq!(tombstone["deleted_by"], json!("me"));
            assert_eq!(tombstone["body"], json!(""));

            // an unknown id is rejected with the id in the error
            let unknown = MessageId([7u8; 16]);
            assert!(matches!(
                w.execute(Command::ReactChat {
                    id: unknown,
                    emoji: "👍".into(),
                })
                .await,
                Err(MoltError::UnknownMessage(i)) if i == unknown
            ));
            assert!(matches!(
                w.execute(Command::DeleteChat { id: unknown }).await,
                Err(MoltError::UnknownMessage(i)) if i == unknown
            ));

            // a quote pointing at an unknown id is dropped, not kept dangling
            w.execute(Command::Chat {
                body: "dangling".into(),
                quote: Some(unknown),
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat with dangling quote");
            let snap = read_surface(&w, Surface::Chat).await;
            assert!(row(&snap, "dangling").get("quote_id").is_none());
        });
    }

    /// Chat bus Stage A pin: ids are minted per message — non-nil and
    /// pairwise distinct.
    #[test]
    fn every_new_message_gets_a_unique_nonnil_id() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            const N: usize = 20;
            for i in 0..N {
                w.execute(Command::Chat {
                    body: format!("msg {i}"),
                    quote: None,
                    channel: molt_core::ChannelRef::default(),
                })
                .await
                .expect("chat");
            }
            let snap = read_surface(&w, Surface::Chat).await;
            // the demo peers may have chatted back — pick out OUR messages
            let ids: Vec<MessageId> = snap
                .applied
                .iter()
                .filter(|m| m["from"] == json!("me"))
                .map(msg_id)
                .collect();
            assert_eq!(ids.len(), N);
            assert!(ids.iter().all(|id| !id.is_nil()), "no nil ids");
            let distinct: std::collections::HashSet<_> = ids.iter().collect();
            assert_eq!(distinct.len(), N, "all ids are pairwise distinct");
        });
    }

    #[test]
    fn propose_then_threshold_applies() {
        rt().block_on(async {
            // 1-of-3, no self-cosign: the proposal genuinely waits for a
            // vote, and this node's OWN single approval honestly meets the
            // threshold — no peer is ever counted for.
            let cfg = GroupConfig {
                threshold: 1,
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
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
            assert_eq!(
                read_surface(&w, Surface::Memory).await.pending.len(),
                1,
                "no self-cosign: the proposal waits for this node's vote"
            );
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
            match w
                .execute(Command::ReadState {
                    surface: Surface::Memory,
                    channel: None,
                    view: None,
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

    /// Without chain governance this node records at most its OWN approval.
    /// The pre-chain counting simulation (a repeated `Approve` counted as
    /// the next member's co-signature) is gone from the production path: a
    /// repeat is refused with an honest error, the counter never moves, and
    /// no proposal applies on invented peer approvals.
    #[test]
    fn approve_never_counts_invented_peer_approvals() {
        rt().block_on(async {
            // self_cosign: proposing already recorded my one real approval
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
            for _ in 0..2 {
                let err = w
                    .execute(Command::Approve { proposal: id })
                    .await
                    .expect_err("a second local approval cannot stand in for a peer");
                assert!(
                    matches!(err, MoltError::AlreadyApproved(got) if got == id),
                    "unexpected: {err:?}"
                );
            }
            let snap = read_surface(&w, Surface::Memory).await;
            assert!(snap.applied.is_empty(), "2-of-3 never applies on one member");
            assert_eq!(snap.pending.len(), 1);
            assert_eq!(
                snap.pending[0].approvals, 1,
                "exactly this node's own approval, nothing invented"
            );
            assert!(snap.pending[0].approved_by_me);
        });
    }

    /// The explicit-vote twin: without self-cosign the FIRST `Approve` is
    /// this node's real vote and is recorded; the second is the refused
    /// simulation. The votes row attributes only what is known — me.
    #[test]
    fn second_local_approval_is_refused_without_chain_governance() {
        rt().block_on(async {
            let cfg = GroupConfig {
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
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
                .expect("my own first approval is real");
            let err = w
                .execute(Command::Approve { proposal: id })
                .await
                .expect_err("no second local approval");
            assert!(
                matches!(err, MoltError::AlreadyApproved(got) if got == id),
                "unexpected: {err:?}"
            );
            let snap = read_surface(&w, Surface::Memory).await;
            assert!(snap.applied.is_empty());
            assert_eq!(snap.pending[0].approvals, 1);
            // honest attribution: my vote is mine, the peers stay open
            for v in &snap.pending[0].votes {
                let expect = if v.member == "me" {
                    molt_core::VoteState::Approved
                } else {
                    molt_core::VoteState::Open
                };
                assert_eq!(v.vote, expect, "stance of {}", v.member);
            }
        });
    }

    /// The open-time crash recovery must not resurrect the simulation: a
    /// legacy log whose counter reached a threshold > 1 did so on invented
    /// peer approvals, and minting a fresh `Applied` from that count would
    /// fake a threshold decision no member made. Such proposals stay
    /// pending (decline is the only exit).
    #[test]
    fn recovery_never_applies_from_simulated_counts() {
        let mut st = plain_state(); // 2-of-3 demo config
        let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: by.to_string(),
            body,
        };
        st.apply(&e(
            1,
            "me",
            molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            },
        ));
        // a legacy pre-chain log: two counted approvals (the second was the
        // simulation), crash before the Applied frame
        for seq in [2, 3] {
            st.apply(&e(
                seq,
                "me",
                molt_core::WorkspaceEvent::Approved {
                    id: molt_core::ProposalId(1),
                    by: "me".to_string(),
                    height: 0,
                    sig: String::new(),
                },
            ));
        }
        st.recover_pending_applies();
        let snap = st.snapshot(Surface::Memory, None, None);
        assert!(snap.applied.is_empty(), "no apply on invented peer counts");
        assert_eq!(snap.pending.len(), 1, "the legacy proposal stays pending");
    }

    /// The honest twin: at threshold 1 the one recorded vote is the local
    /// operator's real decision, so a crash between the `Approved` frame
    /// and its `Applied` frame recovers into the applied state at open.
    #[test]
    fn recovery_completes_a_real_single_operator_decision() {
        let mut st = plain_state();
        st.config.threshold = 1;
        let e = |seq: u64, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: "me".to_string(),
            body,
        };
        st.apply(&e(
            1,
            molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            },
        ));
        st.apply(&e(
            2,
            molt_core::WorkspaceEvent::Approved {
                id: molt_core::ProposalId(1),
                by: "me".to_string(),
                height: 0,
                sig: String::new(),
            },
        ));
        st.recover_pending_applies();
        let snap = st.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied.len(), 1, "my one real vote recovers to applied");
        assert!(snap.pending.is_empty());
    }

    /// The solo boot group (1-of-1) is REAL governance, not a simulation:
    /// the only member's own self-cosigned approval meets the threshold,
    /// so a proposal applies through the same honest single-operator path.
    #[test]
    fn solo_boot_group_runs_real_one_of_one_governance() {
        rt().block_on(async {
            let w = spawn(GroupConfig::solo(), SessionView::default());
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: json!({"op":"add_note","title":"solo"}),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            let snap = read_surface(&w, Surface::Memory).await;
            assert_eq!(
                snap.applied.len(),
                1,
                "the sole member's own approval meets threshold 1"
            );
            assert!(snap.pending.is_empty());
            // a late vote on the decided proposal names the terminal state
            let err = w
                .execute(Command::Approve { proposal: id })
                .await
                .expect_err("the vote is decided");
            assert!(
                matches!(err, MoltError::AlreadyTerminal(got, _) if got == id),
                "unexpected: {err:?}"
            );
        });
    }

    /// The Organization read projections behind the Members and Uploads
    /// tables: every roster member with its identity anchor + governance +
    /// upload counters, and every file shared into the chat with its
    /// retention deadline. Read-only commands — MCP tools like every read,
    /// so an agent can auto-test the same tables the GUI renders.
    #[test]
    fn members_and_uploads_projections_serve_the_org_tables() {
        rt().block_on(async {
            let tmp = tempfile::tempdir().expect("tmp");
            let w = spawn(GroupConfig::demo(), SessionView::default());
            share_temp_file(&w, tmp.path(), "charter.pdf", b"real shared bytes").await;
            // a self-cosigned pending proposal: no longer waiting on me,
            // still waiting on both peers
            w.execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose");
            match w.execute(Command::ReadMembers).await.expect("members") {
                Reply::Members { members: rows } => {
                    assert_eq!(rows.len(), 3, "one row per roster member");
                    let me = rows.iter().find(|m| m.member == "me").expect("me");
                    assert_eq!(me.uploads, 1, "the share counts as my upload");
                    assert_eq!(me.open_proposals, 0, "self-cosign → not waiting on me");
                    assert!(
                        me.identity_pk.is_empty() && me.id.is_empty(),
                        "a demo workspace anchors no identities"
                    );
                    let peer = rows.iter().find(|m| m.member == "peer-1").expect("peer");
                    assert_eq!(peer.uploads, 0);
                    assert_eq!(peer.open_proposals, 1, "the proposal waits on the peer");
                }
                other => panic!("unexpected: {other:?}"),
            }
            match w.execute(Command::ReadUploads).await.expect("uploads") {
                Reply::Uploads { uploads: rows } => {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].member, "me");
                    assert_eq!(rows[0].name, "charter.pdf");
                    assert_eq!(rows[0].kind, "PDF");
                    assert!(rows[0].available);
                    assert!(!rows[0].id.is_nil(), "addressable for download_file");
                    assert_eq!(
                        rows[0].expires_ts,
                        rows[0].ts + 7 * 86_400,
                        "the share expires with the chat retention window (default 7 days)"
                    );
                    assert_eq!(
                        rows[0].checksum,
                        {
                            use sha2::Digest as _;
                            hex::encode(sha2::Sha256::digest(b"real shared bytes"))
                        },
                        "the REAL sha256 of the shared bytes, log-anchored"
                    );
                    assert!(
                        rows[0].online,
                        "the sharer is this node itself — always online"
                    );
                    assert!(
                        rows[0].download.is_none(),
                        "no download of this share is running"
                    );
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    /// At-rest sealing on a SESSION-ONLY node (no storage — unit tests,
    /// ephemeral nodes): there are no on-disk bytes to seal and no genesis
    /// to verify a phrase against, so BOTH commands refuse honestly instead
    /// of faking a flag flip (the pre-story-10 mock accepted any phrase
    /// here while the tool texts promised real verification). The real,
    /// phrase-verified path is pinned by
    /// [`at_rest_sealing_is_real_verified_and_survives_a_restart`]; the
    /// session's `encrypted` flag still gates open (it is scan-derived on
    /// storage nodes), pinned there too.
    #[test]
    fn a_storageless_node_refuses_to_fake_at_rest_sealing() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            let id = demo_workspace_id("Family Office");
            // an empty phrase is rejected before anything else
            assert!(
                w.execute(Command::EncryptWorkspace {
                    id: id.clone(),
                    phrase: String::new(),
                })
                .await
                .is_err(),
                "encrypting needs a phrase"
            );
            // …and WITH a phrase the storage-less node still refuses: it
            // cannot verify or seal anything, and must not pretend to
            assert!(matches!(
                w.execute(Command::EncryptWorkspace {
                    id: id.clone(),
                    phrase: "word1 word2 word3".into(),
                })
                .await,
                Err(MoltError::Storage(_))
            ));
            let entry = |s: &SessionView| {
                s.workspaces
                    .iter()
                    .find(|ws| ws.id == id)
                    .map(|ws| ws.encrypted)
                    .expect("entry")
            };
            assert!(!entry(&*read_session(&w).await), "nothing was faked");
            assert!(matches!(
                w.execute(Command::DecryptWorkspace {
                    id: id.clone(),
                    phrase: "word1 word2 word3".into(),
                })
                .await,
                Err(MoltError::Storage(_))
            ));
            // an unknown id reports UnknownWorkspace, not a phrase error
            assert!(matches!(
                w.execute(Command::EncryptWorkspace {
                    id: "no-such".into(),
                    phrase: String::new(),
                })
                .await,
                Err(MoltError::UnknownWorkspace(_))
            ));
        });
    }

    /// The status summary carries the founding date (the genesis envelope's
    /// timestamp — real on replayed workspaces, 0 on the sessionless demo)
    /// and the REAL activity trio: nobody in the demo boot group has ever
    /// been seen on the wire, so only the local member counts anywhere.
    #[test]
    fn status_carries_founding_date_and_activity() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            match w.execute(Command::Status).await.expect("status") {
                Reply::Status(st) => {
                    assert_eq!(st.founded_ts, 0, "the demo group has no genesis event");
                    assert_eq!(
                        st.active_7d, 1,
                        "honest presence: never-seen peers count nowhere — only the local member"
                    );
                    assert!(st.active_1h <= st.active_24h && st.active_24h <= st.active_7d);
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    /// The pending cards' "Ist-Stand / Soll-Stand" pair: an Organization
    /// edit proposal exposes what the state is now (from the genesis
    /// replica) and what the change would make it (the payload's `value`).
    /// Display data, never consensus input — empty when unknown.
    #[test]
    fn org_pending_cards_carry_current_and_proposed_state() {
        let eff = |image: &str| proposals::OrgEffective {
            name: "Guild".into(),
            agenda: "alte Satzung".into(),
            retention_days: 7,
            image: image.to_string(),
        };
        let rec = |surface: Surface, op: &str, value: &str| molt_core::ProposalRecord {
            surface,
            payload: json!({"op": op, "title": "t", "value": value}),
            approvals: 0,
            state: molt_core::ProposalState::Proposed,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
        };
        assert_eq!(
            proposals::change_summary(
                &eff(""),
                &rec(Surface::Organization, "set_charter", "neue Satzung")
            ),
            ("alte Satzung".to_string(), "neue Satzung".to_string())
        );
        assert_eq!(
            proposals::change_summary(
                &eff(""),
                &rec(Surface::Organization, "set_name", "New Guild")
            ),
            ("Guild".to_string(), "New Guild".to_string())
        );
        // the image ops carry the current image reference as their Ist-Stand
        // ("" while none is set → the UI hides the empty line)
        assert_eq!(
            proposals::change_summary(
                &eff(""),
                &rec(Surface::Organization, "set_image", "~/logo.png")
            ),
            (String::new(), "~/logo.png".to_string())
        );
        assert_eq!(
            proposals::change_summary(
                &eff("/tmp/old.png"),
                &rec(Surface::Organization, "set_image", "~/logo.png")
            ),
            ("/tmp/old.png".to_string(), "~/logo.png".to_string())
        );
        assert_eq!(
            proposals::change_summary(
                &eff("/tmp/old.png"),
                &rec(Surface::Organization, "remove_image", "")
            ),
            ("/tmp/old.png".to_string(), String::new())
        );
        // a non-organization proposal exposes no pair beyond its value
        assert_eq!(
            proposals::change_summary(&eff(""), &rec(Surface::Memory, "add_note", "")),
            (String::new(), String::new())
        );
        // the chat-retention setting's Ist-Stand is the effective window
        assert_eq!(
            proposals::change_summary(
                &eff(""),
                &rec(Surface::Organization, "set_chat_retention", "14 days")
            ),
            ("7 days".to_string(), "14 days".to_string())
        );
        // ops are free-form wire strings, so an older log may carry one this
        // build doesn't know (e.g. the retired plugin vocabulary): tolerated,
        // the Ist-Stand simply stays empty — never a rejection
        assert_eq!(
            proposals::change_summary(
                &eff(""),
                &rec(Surface::Organization, "enable_plugin", "calendar")
            ),
            (String::new(), "calendar".to_string())
        );
    }

    /// The republic's effective display identity is a fold of the applied
    /// Organization log over the genesis: an applied `set_name` /
    /// `set_charter` / `set_chat_retention` actually changes what every
    /// reader sees (`StatusView.name/agenda/chat_retention_days`), and the
    /// pending cards carry the EFFECTIVE state as their Ist-Stand. The
    /// genesis itself stays immutable — it is only the fold's floor.
    #[test]
    fn effective_identity_follows_the_applied_org_ops() {
        rt().block_on(async {
            // 1-of-3, no self-cosign: this node's own single approval
            // honestly applies each change (no peer is counted for)
            let cfg = GroupConfig {
                threshold: 1,
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
            let status = |w: &WalletHandle| {
                let w = w.clone();
                async move {
                    match w.execute(Command::Status).await.expect("status") {
                        Reply::Status(st) => st,
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            };
            let propose = |op: &'static str, value: &'static str| {
                let w = w.clone();
                async move {
                    let payload = json!({"op": op, "title": "t", "value": value});
                    match w
                        .execute(Command::Propose {
                            surface: Surface::Organization,
                            payload,
                        })
                        .await
                        .expect("propose")
                    {
                        Reply::Proposed { id } => id,
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            };
            let st = status(&w).await;
            assert_eq!(st.name, "", "a demo workspace has no genesis name");
            assert_eq!(st.agenda, "");
            assert_eq!(st.chat_retention_days, 7, "the default window is 7 days");
            for (op, value) in [
                ("set_name", "Neue Gilde"),
                ("set_charter", "wir bauen echte dinge"),
                ("set_chat_retention", "14 days"),
            ] {
                let id = propose(op, value).await;
                w.execute(Command::Approve { proposal: id }).await.expect("approve");
            }
            let st = status(&w).await;
            assert_eq!(st.name, "Neue Gilde");
            assert_eq!(st.agenda, "wir bauen echte dinge");
            assert_eq!(st.chat_retention_days, 14);
            // a follow-up proposal shows the EFFECTIVE state as Ist-Stand
            let _next = propose("set_name", "Dritte Gilde").await;
            let pending = read_surface(&w, Surface::Organization).await.pending;
            assert_eq!(pending[0].current, "Neue Gilde");
            assert_eq!(pending[0].proposed, "Dritte Gilde");
            // a bare number parses as days too
            let id = propose("set_chat_retention", "21").await;
            w.execute(Command::Approve { proposal: id }).await.expect("approve");
            assert_eq!(status(&w).await.chat_retention_days, 21);
            // nonsense is refused at propose time — an unparseable window
            // must never reach the applied log
            for bad in ["bald", "", "0 days", "9999 days"] {
                let err = w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload: json!({"op": "set_chat_retention", "title": "t", "value": bad}),
                    })
                    .await
                    .expect_err("an unparseable retention window is refused");
                assert!(
                    matches!(err, MoltError::BadPayload(_)),
                    "unexpected error for {bad:?}: {err:?}"
                );
            }
            // an empty name is refused too (the fold must never go blank)
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op": "set_name", "title": "t", "value": "  "}),
                })
                .await
                .expect_err("an empty name is refused");
            assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        });
    }

    /// "Delete chat after N days" is engine semantics, enforced at the read
    /// contract (co-equality: GUI and MCP see the same filtered snapshot):
    /// chat messages older than the effective window and declined proposals
    /// whose veto aged out disappear from `ReadState`; a legacy ts of 0
    /// stays visible (unknown age must not silently vanish), and the
    /// channel enumeration keeps covering the full log.
    #[test]
    fn chat_retention_filters_the_read_contract() {
        let mut st = plain_state();
        let now = now_secs();
        let stale = now - 10 * 86_400;
        let fresh = now - 3_600;
        let msg = |seq: u64, ts: u64, body: &str| molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: if ts == 0 { now } else { ts },
            by: "peer-1".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
                "peer-1",
                body,
                ts,
            )),
        };
        st.apply(&msg(1, stale, "stale"));
        st.apply(&msg(2, fresh, "fresh"));
        st.apply(&msg(3, 0, "legacy"));
        let snap = st.snapshot(Surface::Chat, None, None);
        assert_eq!(
            snap.applied.len(),
            2,
            "the 10-day-old message ages out of the 7-day default window"
        );
        assert_eq!(
            snap.channels[0].count, 2,
            "channel counts agree with the retention-filtered read (the stale \
             message ages out of the count too, ts 0 stays)"
        );
        // widening the window to 30 days via an applied org change brings
        // the stale message back — the setting is REAL state
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 4,
            ts: now,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: Surface::Organization,
                payload: json!({"op": "set_chat_retention", "title": "t", "value": "30 days"}),
            },
        });
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 5,
            ts: now,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Applied { id: molt_core::ProposalId(1) },
        });
        assert_eq!(st.snapshot(Surface::Chat, None, None).applied.len(), 3);
        // declined proposals age out on the same rhythm (their veto stamp)
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 6,
            ts: stale,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(2),
                surface: Surface::Organization,
                payload: json!({"op": "set_name", "title": "t", "value": "abgelehnt"}),
            },
        });
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 7,
            ts: now - 40 * 86_400,
            by: "peer-1".to_string(),
            body: molt_core::WorkspaceEvent::Declined {
                id: molt_core::ProposalId(2),
                by: "peer-1".to_string(),
            },
        });
        let org = st.snapshot(Surface::Organization, None, None);
        assert!(
            org.declined.is_empty(),
            "a veto older than the retention window is hidden: {:?}",
            org.declined
        );
        assert_eq!(org.denied, 0, "the denied count follows the filtered view");
    }

    /// Uploads are ephemeral exactly like chat: a file share is a chat
    /// message, so it ages out of EVERY read surface on the same
    /// `retention_days` rhythm (one knob — no separate link TTL). The
    /// uploads table hides an expired share, its `expires_ts` is the real
    /// retention deadline (`ts` + window; 0 = unknown age, kept forever),
    /// and a download attempt of an expired share fails cleanly with
    /// [`MoltError::FileExpired`] — a widened window brings both back.
    #[test]
    fn uploads_age_out_with_the_chat_retention_window() {
        let mut st = plain_state();
        let now = now_secs();
        let stale_ts = now - 10 * 86_400;
        let fresh_ts = now - 3_600;
        let share = |seq: u64, ts: u64, name: &str| {
            let mut m = molt_core::ChatMessage::text(
                molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
                "peer-1",
                "",
                ts,
            );
            m.file = Some(molt_core::FileMeta {
                name: name.to_string(),
                size: 3,
                kind: "PDF".to_string(),
                modified: 1,
                available: true,
                checksum: String::new(),
            });
            molt_core::EventEnvelope { prev_seq: 0,
                seq,
                ts: if ts == 0 { now } else { ts },
                by: "peer-1".to_string(),
                body: molt_core::WorkspaceEvent::Chat(m),
            }
        };
        let stale_id = molt_core::MessageId([1u8; 16]);
        let legacy_id = molt_core::MessageId([3u8; 16]);
        st.apply(&share(1, stale_ts, "stale.pdf"));
        st.apply(&share(2, fresh_ts, "fresh.pdf"));
        st.apply(&share(3, 0, "legacy.pdf"));

        // the uploads table follows the chat window (default 7 days)
        let rows = st.uploads_view();
        assert_eq!(
            rows.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            vec!["fresh.pdf", "legacy.pdf"],
            "the 10-day-old share ages out of the 7-day default window, ts 0 stays"
        );
        assert_eq!(
            rows[0].expires_ts,
            fresh_ts + 7 * 86_400,
            "the share expires on the retention deadline — the org window, not a mock TTL"
        );
        assert_eq!(
            rows[1].expires_ts, 0,
            "unknown age (ts 0) never ages out — 0 = no deadline"
        );

        // downloading the expired share fails cleanly, the others pass the gate
        let err = st
            .cmd_download_file(stale_id, None)
            .expect_err("an expired share must not be downloadable");
        assert!(
            matches!(err, MoltError::FileExpired(id) if id == stale_id),
            "unexpected: {err:?}"
        );
        let err = st
            .cmd_download_file(legacy_id, None)
            .expect_err("plain_state has no live engine to spawn the fetch");
        assert!(
            !matches!(err, MoltError::FileExpired(_)),
            "ts 0 passes the retention gate: {err:?}"
        );

        // widening the window to 30 days via an applied org change brings
        // the stale share back — same knob as chat, REAL state
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 4,
            ts: now,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(1),
                surface: Surface::Organization,
                payload: json!({"op": "set_chat_retention", "title": "t", "value": "30 days"}),
            },
        });
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 5,
            ts: now,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Applied { id: molt_core::ProposalId(1) },
        });
        let rows = st.uploads_view();
        assert_eq!(rows.len(), 3, "the widened window re-exposes the stale share");
        assert_eq!(
            rows[0].expires_ts,
            stale_ts + 30 * 86_400,
            "the deadline follows the effective window"
        );
        let err = st
            .cmd_download_file(stale_id, None)
            .expect_err("plain_state has no live engine to spawn the fetch");
        assert!(
            !matches!(err, MoltError::FileExpired(_)),
            "inside the widened window the share is downloadable again: {err:?}"
        );
    }

    /// The today/archive boundary is a pure function of the message
    /// timestamp, "now" and the retention window (explicit `now`, like the
    /// `*_label_at` helpers): "today" admits the younger half of the
    /// window, "archive" the older half still inside it, `None` the whole
    /// window — and a legacy ts of 0 (unknown age) files under the general
    /// view, never the archive, and never vanishes.
    #[test]
    fn chat_view_boundary_splits_the_retention_window_at_half() {
        use crate::proposals::chat_view_admits;
        let now = 1_700_000_000;
        let days = 10; // window: 864 000 s, half: 432 000 s
        let at = |pct: u64| now - 864_000 * pct / 100;
        // 10 % of the window old: today, not archive
        assert!(chat_view_admits(Some("today"), at(10), now, days));
        assert!(!chat_view_admits(Some("archive"), at(10), now, days));
        assert!(chat_view_admits(None, at(10), now, days));
        // exactly 50 %: still today (the boundary is inclusive young-side)
        assert!(chat_view_admits(Some("today"), at(50), now, days));
        assert!(!chat_view_admits(Some("archive"), at(50), now, days));
        // 60 %: archive, not today
        assert!(!chat_view_admits(Some("today"), at(60), now, days));
        assert!(chat_view_admits(Some("archive"), at(60), now, days));
        assert!(chat_view_admits(None, at(60), now, days));
        // exactly 100 %: the window's oldest visible instant — archive
        assert!(chat_view_admits(Some("archive"), at(100), now, days));
        assert!(chat_view_admits(None, at(100), now, days));
        // 110 %: aged out everywhere (deleted, exactly as today)
        assert!(!chat_view_admits(Some("today"), at(110), now, days));
        assert!(!chat_view_admits(Some("archive"), at(110), now, days));
        assert!(!chat_view_admits(None, at(110), now, days));
        // ts 0 = unknown age: general view + unfiltered, never archive
        assert!(chat_view_admits(Some("today"), 0, now, days));
        assert!(!chat_view_admits(Some("archive"), 0, now, days));
        assert!(chat_view_admits(None, 0, now, days));
    }

    /// `ReadState { view }` splits the visible chat log on the retention
    /// half-window: General ("today") shows only the young half, Archive
    /// only the old half, no view the whole window — and the channel
    /// enumeration stays unfiltered across all three reads (same posture
    /// as the channel filter).
    #[test]
    fn archive_view_holds_the_older_half_of_the_retention_window() {
        let mut st = plain_state();
        let now = now_secs();
        let window = 7 * 86_400; // the default 7-day retention window
        let young = now - window * 10 / 100;
        let old = now - window * 60 / 100;
        let gone = now - window * 110 / 100;
        let msg = |seq: u64, ts: u64, body: &str| molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: if ts == 0 { now } else { ts },
            by: "peer-1".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
                "peer-1",
                body,
                ts,
            )),
        };
        st.apply(&msg(1, young, "young"));
        st.apply(&msg(2, old, "old"));
        st.apply(&msg(3, gone, "gone"));
        let body_of = |v: &serde_json::Value| v["body"].as_str().expect("body").to_string();
        let today = st.snapshot(Surface::Chat, None, Some("today"));
        assert_eq!(
            today.applied.iter().map(body_of).collect::<Vec<_>>(),
            vec!["young"],
            "General holds only the messages younger than half the window"
        );
        let archive = st.snapshot(Surface::Chat, None, Some("archive"));
        assert_eq!(
            archive.applied.iter().map(body_of).collect::<Vec<_>>(),
            vec!["old"],
            "Archive holds only the older half (still inside the window)"
        );
        let all = st.snapshot(Surface::Chat, None, None);
        assert_eq!(
            all.applied.iter().map(body_of).collect::<Vec<_>>(),
            vec!["young", "old"],
            "no view = the whole retention window, exactly as before"
        );
        // the enumeration is a whole-window concern, like with `channel`
        assert_eq!(today.channels, all.channels);
        assert_eq!(archive.channels, all.channels);
        // a legacy ts of 0 files under the general view, never the archive
        st.apply(&msg(4, 0, "legacy"));
        assert_eq!(
            st.snapshot(Surface::Chat, None, Some("today")).applied.len(),
            2,
            "unknown age joins the general view"
        );
        assert_eq!(
            st.snapshot(Surface::Chat, None, Some("archive")).applied.len(),
            1,
            "unknown age never files as archived"
        );
    }

    /// WP1 (governance follow-ups): the read contract carries a parallel id
    /// track — `SurfaceSnapshot.applied_ids` is positionally parallel to
    /// `applied` and names the proposal each entry came from. `None` =
    /// origin unknown (chat rows, legacy dumps). The payloads themselves
    /// stay byte-identical — the UI fate probe and MCP readers compare them.
    #[test]
    fn applied_entries_carry_their_proposal_id() {
        let mut st = plain_state();
        let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: 100 + seq,
            by: by.to_string(),
            body,
        };
        let payload = json!({"op": "add_note", "title": "minutes"});
        st.apply(&e(
            1,
            "petra",
            molt_core::WorkspaceEvent::Proposed {
                id: molt_core::ProposalId(4),
                surface: Surface::Memory,
                payload: payload.clone(),
            },
        ));
        st.apply(&e(
            2,
            "walter",
            molt_core::WorkspaceEvent::Applied {
                id: molt_core::ProposalId(4),
            },
        ));
        let snap = st.snapshot(Surface::Memory, None, None);
        assert_eq!(snap.applied, vec![payload.clone()], "payload untouched");
        assert_eq!(
            snap.applied_ids,
            vec![Some(4)],
            "the applied entry knows the proposal it came from"
        );
        // chat rows have no proposal origin: same length, all None
        st.apply(&e(
            3,
            "petra",
            // ts 0 = unknown age: always inside the retention read window
            molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([7u8; 16]),
                "petra",
                "gm",
                0,
            )),
        ));
        let chat = st.snapshot(Surface::Chat, None, None);
        assert_eq!(chat.applied.len(), 1);
        assert_eq!(chat.applied_ids, vec![None]);
        // a NEW dump round-trips the id track…
        let dump = st.snapshot_now().state;
        let mut st2 = plain_state();
        st2.restore_dump(dump.clone());
        assert_eq!(
            st2.snapshot(Surface::Memory, None, None).applied_ids,
            vec![Some(4)]
        );
        // …a LEGACY dump (a pre-id writer: the field is absent) restores the
        // payloads unchanged with unknown origin
        let mut v = serde_json::to_value(&dump).expect("dump serializes");
        v.as_object_mut().expect("a JSON object").remove("applied_ids");
        let legacy: molt_core::EngineStateDump =
            serde_json::from_value(v).expect("legacy dump deserializes");
        let mut st3 = plain_state();
        st3.restore_dump(legacy);
        let restored = st3.snapshot(Surface::Memory, None, None);
        assert_eq!(restored.applied, vec![payload], "payloads survive untouched");
        assert_eq!(restored.applied_ids, vec![None], "unknown origin stays honest");
    }

    /// The republic's current image is derived from the applied
    /// Organization log: the last applied `set_image` wins, an applied
    /// `remove_image` clears it — and the pending image cards carry it as
    /// their Ist-Stand. A `set_image` now CARRIES the bytes (base64 in the
    /// payload — sign-what-you-see: members vote on the actual image); on
    /// a session-only workspace (no storage dir to materialize a logo
    /// file into) the reference falls back to the proposed display value.
    #[test]
    fn current_image_follows_the_applied_org_ops() {
        use base64::Engine as _;
        rt().block_on(async {
            // 1-of-3, no self-cosign: this node's own single approval
            // honestly applies each change (no peer is counted for)
            let cfg = GroupConfig {
                threshold: 1,
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
            let status = |w: &WalletHandle| {
                let w = w.clone();
                async move {
                    match w.execute(Command::Status).await.expect("status") {
                        Reply::Status(st) => st,
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            };
            // a real 2x2 PNG — since WP3 the bytes must decode as a picture
            let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==".to_string();
            let propose = |op: &'static str, value: &'static str, with_bytes: bool| {
                let w = w.clone();
                let b64 = b64.clone();
                async move {
                    let mut payload = json!({"op": op, "title": "t", "value": value});
                    if with_bytes {
                        payload["bytes_b64"] = json!(b64);
                    }
                    match w
                        .execute(Command::Propose {
                            surface: Surface::Organization,
                            payload,
                        })
                        .await
                        .expect("propose")
                    {
                        Reply::Proposed { id } => id,
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            };
            assert_eq!(status(&w).await.image, "", "no image before any change");
            // 1-of-3: this node's own approval applies the change
            let id = propose("set_image", "team.png", true).await;
            w.execute(Command::Approve { proposal: id }).await.expect("approve");
            assert_eq!(status(&w).await.image, "team.png");
            // a follow-up image proposal shows the applied state as Ist-Stand
            let next = propose("set_image", "new.png", true).await;
            let pending = read_surface(&w, Surface::Organization).await.pending;
            assert_eq!(pending[0].current, "team.png");
            assert_eq!(pending[0].proposed, "new.png");
            w.execute(Command::Approve { proposal: next }).await.expect("approve");
            assert_eq!(status(&w).await.image, "new.png", "last applied wins");
            // an applied remove_image clears the state again
            let rm = propose("remove_image", "", false).await;
            w.execute(Command::Approve { proposal: rm }).await.expect("approve");
            assert_eq!(status(&w).await.image, "");
            // a set_image without the actual bytes is refused — the mock
            // path-reference era is over (nothing real could be applied)
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op": "set_image", "title": "t", "value": "x.png"}),
                })
                .await
                .expect_err("a set_image without bytes is refused");
            assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
            // oversized bytes are refused with a clear error
            let big = base64::engine::general_purpose::STANDARD
                .encode(vec![0u8; proposals::ORG_IMAGE_MAX_BYTES + 1]);
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op": "set_image", "title": "t", "value": "big.png", "bytes_b64": big}),
                })
                .await
                .expect_err("an oversized image is refused");
            assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        });
    }

    /// A BMP whose HEADER declares the given dimensions; carries no pixel
    /// data (dimension sniffs read only the header, so none is needed).
    pub(crate) fn tiny_bmp_header(w: u32, h: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&54u32.to_le_bytes()); // "file size" (header only)
        b.extend_from_slice(&[0; 4]); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        b.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        b.extend_from_slice(&i32::try_from(w).expect("small dims").to_le_bytes());
        b.extend_from_slice(&i32::try_from(h).expect("small dims").to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        b.extend_from_slice(&[0; 24]); // compression/size/ppm/palette zeros
        b
    }

    /// WP3: a `set_image` proposal must carry DECODABLE bytes — a member
    /// asked to sign-what-they-see must be able to see it. The engine
    /// sniffs format + header dimensions (never a full decode — decode
    /// bombs); real 2×2 fixtures of every picker format pass, garbage and
    /// a dimension bomb are refused with an honest error.
    #[test]
    fn an_undecodable_set_image_proposal_is_refused() {
        use base64::Engine as _;
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            let propose = |b64: String| {
                let w = w.clone();
                async move {
                    w.execute(Command::Propose {
                        surface: Surface::Organization,
                        payload: json!({
                            "op": "set_image", "value": "x.png", "bytes_b64": b64,
                        }),
                    })
                    .await
                }
            };
            // garbage bytes: refused with a clear error
            let garbage =
                base64::engine::general_purpose::STANDARD.encode(b"definitely not an image");
            let err = propose(garbage).await.expect_err("garbage is refused");
            assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
            // a dimension bomb: a valid BMP HEADER declaring 20000x20000 —
            // the sniff reads only the header and refuses before any decode
            let bomb = base64::engine::general_purpose::STANDARD
                .encode(tiny_bmp_header(20_000, 20_000));
            let err = propose(bomb).await.expect_err("a dimension bomb is refused");
            assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
            // real minimal files (2x2, PIL-generated — the molt-ui preview
            // fixtures) pass for every picker format, svg by prefix sniff
            let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
            let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
            let svg = base64::engine::general_purpose::STANDARD.encode(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
            );
            for (fmt, b64) in [("png", png.to_string()), ("webp", webp.to_string()), ("svg", svg)] {
                propose(b64).await.unwrap_or_else(|e| panic!("{fmt} must pass: {e:?}"));
            }
        });
    }

    /// Organization is a gated surface like the others: charter / name /
    /// logo / retention changes go through propose → threshold → applied — and
    /// because the MCP `propose` tool derives its surface list from
    /// `is_gated`, the GUI edit modals and an MCP agent drive the SAME path.
    #[test]
    fn organization_changes_are_gated_proposals() {
        rt().block_on(async {
            // 1-of-3, no self-cosign: propose leaves the vote genuinely
            // open, this node's own approval honestly applies it
            let cfg = GroupConfig {
                threshold: 1,
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op":"set_charter","title":"Charter ändern","value":"neue Satzung"}),
                })
                .await
                .expect("propose on organization")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            // the pending view carries the Soll-Stand (the payload's value);
            // the Ist-Stand stays empty on a demo workspace (no genesis)
            let pending = read_surface(&w, Surface::Organization).await.pending;
            assert_eq!(pending[0].proposed, "neue Satzung");
            assert_eq!(pending[0].current, "");
            // threshold 1: this node's own approval applies the change
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
            let snap = read_surface(&w, Surface::Organization).await;
            assert!(snap.gated, "organization is threshold-gated");
            assert_eq!(snap.applied.len(), 1, "applied at threshold");
            assert!(snap.pending.is_empty());
            // an op this build doesn't know still proposes: ops are free-form
            // wire strings (an MCP agent or an older/newer build may mint
            // one), so the validator only vets the ops it understands
            w.execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op":"enable_plugin","title":"t","value":"calendar"}),
            })
            .await
            .expect("an unknown org op is tolerated, not rejected");
        });
    }

    /// The pending cards render a voting row: per-member stance in roster
    /// order. On the single-operator path the only attributable vote is
    /// this node's own — my approval flips exactly my pill, every peer
    /// honestly stays open.
    #[test]
    fn pending_views_carry_per_member_votes() {
        rt().block_on(async {
            let cfg = GroupConfig {
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let roster = cfg.members.clone();
            let w = spawn(cfg, SessionView::default());
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: json!({"op":"add_note","title":"minutes"}),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            // fresh proposal, no self-cosign: the whole roster is open
            let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
            assert_eq!(
                votes.iter().map(|v| v.member.clone()).collect::<Vec<_>>(),
                roster,
                "one entry per roster member, in roster order"
            );
            assert!(votes.iter().all(|v| v.vote == molt_core::VoteState::Open));
            // my approval flips exactly my entry (the demo member is "me")
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
            let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
            for v in votes {
                let expect = if v.member == "me" {
                    molt_core::VoteState::Approved
                } else {
                    molt_core::VoteState::Open
                };
                assert_eq!(v.vote, expect, "stance of {}", v.member);
            }
        });
    }

    /// The read contract splits a surface's open governance by the reader:
    /// a pending proposal says whether THIS node already approved it
    /// (`approved_by_me`), and declined proposals count into `denied` —
    /// the Organization → Status approvals table renders exactly these.
    #[test]
    fn pending_views_split_by_my_vote_and_count_denied() {
        rt().block_on(async {
            // no self-cosign: a fresh proposal starts with zero approvals,
            // so it genuinely waits on this node's vote
            let cfg = GroupConfig {
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
            let propose = |title: &str| {
                let w = &w;
                let payload = json!({"op":"add_note","title":title});
                async move {
                    match w
                        .execute(Command::Propose {
                            surface: Surface::Memory,
                            payload,
                        })
                        .await
                        .expect("propose")
                    {
                        Reply::Proposed { id } => id,
                        other => panic!("unexpected: {other:?}"),
                    }
                }
            };
            let waiting_on_me = propose("waiting").await;
            let voted = propose("voted").await;
            let declined = propose("declined").await;
            // one approval of two: still pending, but no longer waiting on me
            w.execute(Command::Approve { proposal: voted })
                .await
                .expect("approve");
            w.execute(Command::Decline { proposal: declined })
                .await
                .expect("decline");
            let snap = read_surface(&w, Surface::Memory).await;
            assert_eq!(snap.pending.len(), 2);
            let by_id = |id| {
                snap.pending
                    .iter()
                    .find(|p| p.id == id)
                    .expect("pending view")
            };
            assert!(
                !by_id(waiting_on_me).approved_by_me,
                "an untouched proposal waits on this node's vote"
            );
            assert!(
                by_id(voted).approved_by_me,
                "the own approval must reflect in the pending view"
            );
            assert_eq!(snap.denied, 1, "the declined proposal counts as denied");
        });
    }

    /// A declined proposal leaves `pending` and surfaces in the snapshot's
    /// `declined` list — with who declined and when (the envelope ts the
    /// GUI's retention window filters on), and the decliner's stance marked
    /// in the votes row. The Organization → Declined view renders exactly
    /// this projection.
    #[test]
    fn declined_proposals_surface_with_decliner_and_timestamp() {
        rt().block_on(async {
            let cfg = GroupConfig {
                self_cosign: false,
                ..GroupConfig::demo()
            };
            let w = spawn(cfg, SessionView::default());
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: json!({"op":"add_note","title":"nope"}),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            w.execute(Command::Decline { proposal: id })
                .await
                .expect("decline");
            let snap = read_surface(&w, Surface::Memory).await;
            assert!(snap.pending.is_empty(), "a decline leaves pending");
            assert_eq!(snap.denied, 1, "the count stays for the status strip");
            assert_eq!(snap.declined.len(), 1, "the declined view is exposed");
            let v = &snap.declined[0];
            assert_eq!(v.id, id);
            assert_eq!(v.state, molt_core::ProposalState::Rejected);
            assert_eq!(v.declined_by, "me", "the decliner is named");
            assert!(v.declined_at > 0, "the decline carries its envelope ts");
            let mine = v
                .votes
                .iter()
                .find(|x| x.member == "me")
                .expect("my roster row");
            assert_eq!(
                mine.vote,
                molt_core::VoteState::Declined,
                "the votes row marks the decliner"
            );
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

            // the fake-progress restore is GONE: a storage-less engine has
            // nowhere to restore into and refuses honestly instead of
            // running a progress show (story 13 — the real pipeline is
            // exercised end-to-end in tests/restore_real.rs)
            let err = w
                .execute(Command::RestoreStart {
                    way: "s3".to_string(),
                    target: "ab".repeat(32),
                    secret: "some secret".to_string(),
                    replace: false,
                })
                .await
                .expect_err("no storage → no restore");
            assert!(err.to_string().contains("storage"), "{err}");
            // finishing without a successful restore stays refused
            assert!(w.execute(Command::RestoreFinish).await.is_err());
        });
    }

    /// N4a: a PRODUCTION founding (no test seam) runs over Nostr — and on a
    /// fresh node with an EMPTY relay pool (ADR-0004: nothing pre-configured)
    /// it fails honestly, naming the missing prerequisite, before a single
    /// ticket leaves the engine.
    #[test]
    fn create_start_without_a_confirmed_relay_fails_honestly() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            let err = w
                .execute(Command::CreateStart {
                    name: "Gap".to_string(),
                    member: "petra".to_string(),
                    threshold: 2,
                    members: 3,
                })
                .await
                .expect_err("no confirmed relay → no founding");
            assert!(
                err.to_string().contains("no relay configured"),
                "the honest prerequisite error surfaces: {err}"
            );
        });
    }

    /// …and the SAME refusal must not misdiagnose the pool it is looking at.
    /// A confirmed clearnet relay with non-onion dialing switched off (the
    /// hand-written `confirmed = true` without `clearnet_enabled = true`) was
    /// told to "add and confirm one" — the one thing the operator had already
    /// done, while the switch that was actually off went unmentioned.
    #[test]
    fn create_start_names_the_clearnet_switch_when_that_is_what_blocks_it() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::RelayAdd { url: "wss://relay.example.org".to_string() })
                .await
                .expect("add");
            w.execute(Command::RelayConfirm {
                url: "wss://relay.example.org".to_string(),
                accept_clearnet: true,
            })
            .await
            .expect("confirm");
            // the operator (or their config file) leaves non-onion dialing off
            w.execute(Command::RelayClearnetSession { unlock: false })
                .await
                .expect("dark");
            let err = w
                .execute(Command::CreateStart {
                    name: "Gap".to_string(),
                    member: "petra".to_string(),
                    threshold: 2,
                    members: 3,
                })
                .await
                .expect_err("nothing dialable → no founding");
            let err = err.to_string();
            assert!(
                err.contains("clearnet_enabled") && !err.contains("no relay configured"),
                "the refusal names the switch, not a confirmation that exists: {err}"
            );
        });
    }

    #[test]
    fn create_lifecycle_founds_a_republic() {
        rt().block_on(async {
            // the offline sim seam (session-only): simulated members seal the
            // ritual so the founder-side lifecycle can be tested without a
            // network — a production founding fails honestly until N4
            let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);

            // invalid configurations are rejected up front
            assert!(matches!(
                w.execute(Command::CreateStart {
                    name: "X".to_string(),
                    member: "me".to_string(),
                    threshold: 4,
                    members: 3,
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

    /// The persisted `WorkspaceInfo.net` label mirrors the EFFECTIVE global
    /// anonymity setting — the ritual transport always comes from the global
    /// settings (`resolve_dialer`), so the label must reflect those and never
    /// a client-supplied string (tor_transport_implementation.md §P8).
    #[test]
    fn workspace_net_label_mirrors_the_global_anonymity_setting() {
        rt().block_on(async {
            // default settings (anonymity = "none") → the label says "none"
            let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);
            w.execute(Command::CreateStart {
                name: "Plain".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
            })
            .await
            .expect("start");
            // the run header shows the effective network while the ritual runs
            assert_eq!(read_session(&w).await.create.net, "none");
            await_founding(&w).await;
            w.execute(Command::CreateFinish).await.expect("finish");
            let s = read_session(&w).await;
            let ws = s.workspaces.iter().find(|x| x.name == "Plain").expect("entry");
            assert_eq!(ws.net, "none", "label = the effective global setting");

            // tor configured globally → the label says "tor"
            let session = SessionView {
                settings: SessionSettings {
                    anonymity: "tor".to_string(),
                    ..SessionSettings::default()
                },
                ..SessionView::default()
            };
            let w = __spawn_sim_founding(GroupConfig::demo(), session, false);
            w.execute(Command::CreateStart {
                name: "Onioned".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
            })
            .await
            .expect("start tor");
            assert_eq!(read_session(&w).await.create.net, "tor");
            await_founding(&w).await;
            w.execute(Command::CreateFinish).await.expect("finish tor");
            let s = read_session(&w).await;
            let ws = s.workspaces.iter().find(|x| x.name == "Onioned").expect("entry");
            assert_eq!(ws.net, "tor", "label = the effective global setting");
        });
    }

    #[test]
    fn join_requires_a_joinable_link() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());

            // empty, plain text, and a bare preview link (no transport
            // handover) are all rejected — a real join needs a link that
            // carries the transport handover
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

            // a real founding link (with the transport handover) arms the
            // wizard — and then fails HONESTLY: this build has no network
            // relay-gate refusal: this node shares NO relay with the invite
            // (its pool is empty), so the run says exactly that — naming both
            // sides — instead of dialing somewhere the operator never approved
            let link = crate::FoundingInvite {
                info: molt_core::InviteInfo {
                    republic: "Chess Club".to_string(),
                    threshold: 2,
                    members: 2,
                    inviter: "walter".to_string(),
                    ticket: "ab".repeat(32),
                },
                handover: molt_net::invite::InviteHandoverV2 {
                    seat: 0,
                    ticket: "ab".repeat(32),
                    npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
                    relays: vec!["wss://no-such-relay.invalid".to_string()],
                },
            }
            .render()
            .expect("a well-formed handover renders");
            w.execute(Command::JoinStart {
                invite: link,
                member: "petra".to_string(),
            })
            .await
            .expect("a joinable link arms the wizard");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert_eq!(s.screen, Screen::Join);
                    assert_eq!(s.join.republic, "Chess Club");
                    assert_eq!((s.join.rule_m, s.join.rule_n), (2, 2));
                    assert!(!s.join.seed.is_empty(), "the joiner's recovery phrase is shown");
                    assert_eq!(s.join.run.outcome, 2, "no shared relay → the run fails honestly");
                    assert!(
                        s.join.run.log.iter().any(|l| l.contains("no relay in common")),
                        "the honest relay-gate error is in the run log: {:?}",
                        s.join.run.log
                    );
                }
                other => panic!("unexpected: {other:?}"),
            }
            // cancel still clears the failed run
            w.execute(Command::JoinCancel).await.expect("cancel");
        });
    }

    /// A joinable link with an unreachable relay — parseable, so
    /// `cmd_join_start` arms the wizard before it fails honestly.
    fn joinable_link() -> String {
        crate::FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "R".to_string(),
                threshold: 2,
                members: 2,
                inviter: "walter".to_string(),
                ticket: "ab".repeat(32),
            },
            handover: molt_net::invite::InviteHandoverV2 {
                seat: 0,
                ticket: "ab".repeat(32),
                npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
                relays: vec!["wss://no-such-relay.invalid".to_string()],
            },
        }
        .render()
        .expect("a well-formed handover renders")
    }

    /// Petra's nostr identity for the join fixtures — a REAL derived pair,
    /// so the sealed handler's sk↔anchored-pk cross-check has a genuine
    /// secret to validate (the anchors must be real canonical curve points
    /// anyway: `verify_sealed_roster` rejects anything else).
    fn petra_nostr() -> ([u8; 32], String) {
        molt_net::nostr_identity(b"petra-entropy", "ticket-petra")
    }

    fn valid_sealed_roster() -> molt_core::SealedRoster {
        use molt_core::{MemberIdentity, RosterAttestation};
        let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
        let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
        let identities = vec![
            MemberIdentity {
                member: "founder".to_string(),
                identity_pk: pk_a,
                nostr_pk: molt_net::nostr_identity(b"founder-entropy", "ticket-f").1,
            },
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: pk_b,
                nostr_pk: petra_nostr().1,
            },
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

    /// An honest join failure (here: the ADR-0004 relay gate) rides the join
    /// run's EXISTING failure surface (`cmd_net_join_failed`), and that
    /// surface keeps its gates: a report after the run already failed is
    /// dropped, not double-appended.
    #[test]
    fn join_fails_honestly_and_late_reports_stay_dropped() {
        rt().block_on(async {
            let w = spawn(GroupConfig::demo(), SessionView::default());
            w.execute(Command::JoinStart { invite: joinable_link(), member: "petra".to_string() })
                .await
                .expect("start");
            match w.execute(Command::ReadSession).await.expect("read") {
                Reply::Session(s) => {
                    assert_eq!(s.join.run.outcome, 2, "no shared relay → honest failure");
                    assert!(s.join.run.log.iter().any(|l| l.contains("no relay in common")));
                }
                other => panic!("unexpected: {other:?}"),
            }
            // a late failure report (any generation) is dropped — the run is
            // already settled, its log must not grow a second failure line
            w.execute(Command::NetJoinFailed { error: "boom".to_string(), generation: Some(1) })
                .await
                .expect("late");
            match w.execute(Command::ReadSession).await.expect("read2") {
                Reply::Session(s) => {
                    assert!(
                        !s.join.run.log.iter().any(|l| l.contains("boom")),
                        "a settled run drops late failure reports: {:?}",
                        s.join.run.log
                    );
                }
                other => panic!("unexpected: {other:?}"),
            }
        });
    }

    /// The GENERATION clause of the join gates (`cmd_join_cancel` bumps the
    /// generation to invalidate in-flight tasks): a report from a superseded
    /// generation is dropped even while a run is LIVE, and the sealed handler
    /// shares the clause — a stale-generation seal materializes nothing.
    #[test]
    fn join_reports_from_a_stale_generation_are_dropped_while_live() {
        let mut st = plain_state();
        st.join_generation = 2;
        assert_eq!(st.session.join.run.outcome, 0, "run starts live");
        st.cmd_net_join_failed("boom".to_string(), Some(1)).expect("stale gen");
        assert_eq!(st.session.join.run.outcome, 0, "stale-generation report ignored");
        st.cmd_net_join_failed("boom".to_string(), None).expect("no gen");
        assert_eq!(st.session.join.run.outcome, 0, "generation-less report ignored");
        st.cmd_net_join_failed("boom".to_string(), Some(2)).expect("current gen");
        assert_eq!(st.session.join.run.outcome, 2, "matching generation lands");
        assert!(st.session.join.run.log.iter().any(|l| l.contains("boom")));

        let mut st2 = plain_state();
        st2.join_generation = 2;
        let before = st2.session.workspaces.len();
        let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
        st2.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
            .expect("stale seal");
        assert_eq!(
            st2.session.workspaces.len(),
            before,
            "a stale-generation seal materializes nothing"
        );
    }

    /// `NetJoinSealed` stays on the surface (dormant — N4's Nostr join task
    /// re-emits it), so its materialization is pinned by arming the join
    /// context DIRECTLY: `cmd_join_start` fails honestly without a transport
    /// (its run settles at outcome 2, which gates the sealed handler off).
    #[test]
    fn join_seals_into_the_republic_from_a_valid_roster() {
        let rt = rt();
        let _guard = rt.enter();
        // a verified sealed roster materializes the republic
        let mut st = plain_state();
        st.join_generation = 1;
        st.session.join = molt_core::JoinState {
            member: "petra".to_string(),
            seed: "wombat lattice orbit".to_string(),
            ..molt_core::JoinState::default()
        };
        let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
        st.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
            .expect("sealed");
        assert_eq!(st.session.screen, Screen::Main, "entered the republic");
        assert_eq!(st.session.join, molt_core::JoinState::default(), "join reset");
        let ws = st.session.workspaces.iter().find(|ws| ws.name == "R").expect("workspace added");
        // the net label mirrors the joiner's own global anonymity setting
        // ("none" by default) — never a hardcoded "tor"
        assert_eq!(ws.net, "none", "label = the effective global setting");

        // a garbage roster fails the join rather than materialising anything
        let mut st2 = plain_state();
        st2.join_generation = 1;
        st2.session.join = molt_core::JoinState {
            member: "x".to_string(),
            ..molt_core::JoinState::default()
        };
        let before = st2.session.workspaces.len();
        st2.cmd_net_join_sealed("{".to_string(), String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
            .expect("bad");
        assert_eq!(st2.session.join.run.outcome, 2, "garbage roster fails");
        assert_eq!(st2.session.workspaces.len(), before, "nothing materialized");
    }

    /// N1 PIN — the secret that pairs with the FOREVER-anchored third anchor
    /// is validated before it is persisted: `cmd_net_join_sealed` must
    /// refuse a nostr_sk that is not 32 bytes of hex, or whose x-only public
    /// key is not OUR seat's anchored `nostr_pk` — in both directions the
    /// join FAILS (like the corrupt-MLS arm), because sealing a genesis
    /// whose transport secret the node does not actually hold surfaces only
    /// when N4's transport first uses the key, with the salting ticket long
    /// dead and no re-derivation path. The matching secret persists into
    /// `transport.state.nostr_sk` byte-exactly.
    #[test]
    fn join_sealed_validates_the_persisted_nostr_secret() {
        let rt = rt();
        let _guard = rt.enter();
        let tmp = tempfile::tempdir().expect("tmp");
        let persist_state = || {
            let (ev_tx, _keep) = broadcast::channel::<Event>(8);
            let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
            let mut st = State::new(
                GroupConfig::demo(),
                SessionView {
                    settings: molt_core::SessionSettings {
                        workspace_dir: tmp.path().display().to_string(),
                        ..molt_core::SessionSettings::default()
                    },
                    ..SessionView::default()
                },
                ev_tx,
                cmd_tx,
                None,
                true, // persist — the secret-lifecycle path under test
                None,
            );
            st.join_generation = 1;
            st.session.join = molt_core::JoinState {
                member: "petra".to_string(),
                seed: molt_storage::generate_seed_phrase().expect("seed"),
                ..molt_core::JoinState::default()
            };
            st
        };
        let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
        let (petra_sk, petra_npk) = petra_nostr();

        // absent, truncated, and odd-length secrets all FAIL the join
        for bad in ["", "abcd", "ab", &hex::encode(&petra_sk[..16])] {
            let mut st = persist_state();
            st.cmd_net_join_sealed(sealed.clone(), String::new(), Vec::new(), bad.to_string(), Vec::new(), String::new(), Some(1))
                .expect("handler never errors");
            assert_eq!(
                st.session.join.run.outcome, 2,
                "a malformed nostr secret {bad:?} must fail the join"
            );
            assert!(st.active.is_none(), "nothing materialized for {bad:?}");
        }
        // a well-formed scalar that is NOT the private half of petra's
        // anchored nostr_pk fails too (the wrong-seat/wrong-derivation case)
        let (foreign_sk, _) = molt_net::nostr_identity(b"someone-else", "ticket-x");
        let mut st = persist_state();
        st.cmd_net_join_sealed(
            sealed.clone(),
            String::new(),
            Vec::new(),
            hex::encode(foreign_sk),
            Vec::new(),
            String::new(),
            Some(1),
        )
        .expect("handler never errors");
        assert_eq!(st.session.join.run.outcome, 2, "a mismatched secret must fail the join");
        assert!(st.active.is_none(), "nothing materialized for the mismatch");

        // the matching secret seals the join and persists byte-exactly
        let mut st = persist_state();
        st.cmd_net_join_sealed(
            sealed.clone(),
            String::new(),
            Vec::new(),
            hex::encode(petra_sk),
            Vec::new(),
            String::new(),
            Some(1),
        )
        .expect("handler never errors");
        assert_eq!(st.session.screen, Screen::Main, "the matching secret enters the republic");
        let dir = st.active.as_ref().expect("materialized").dir.clone();
        drop(st); // release the writer + flock before reopening
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ws = loop {
            match molt_storage::open_workspace(&dir) {
                Ok((ws, _)) => break ws,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("reopening the joined workspace: {e}"),
            }
        };
        let ts = ws.read_transport_state();
        assert_eq!(
            ts.nostr_sk.as_deref(),
            Some(&petra_sk[..]),
            "the validated secret is sealed into transport.state"
        );
        assert_eq!(
            molt_net::nostr_pk_for_sk(&petra_sk).expect("pk"),
            petra_npk,
            "…and it IS the private half of the anchored third anchor"
        );
    }

    /// A real, threshold-signed **two-block chain** for the recovery tests: a
    /// genesis anchoring a coordinator and "bob" — whose identity derives from
    /// `phrase` exactly as the ritual derives it — plus one gated `Applied`
    /// block (m=1, signed by the coordinator). Recovering must adopt the FULL
    /// chain: the genesis alone would not project block 1's surface state.
    fn recovered_chain(phrase: &str) -> (Vec<molt_core::ChainBlock>, String) {
        use molt_core::{ChainBlock, ChainChange, MemberIdentity, RosterAttestation, GENESIS_PREV};
        let (coord_sk, coord_pk) = molt_storage::derive_identity_key(&[7u8; 32], "coordinator");
        let (bob_sk, bob_pk) =
            crate::founding::member_identity(phrase).expect("bob's ritual identity");
        let identities = vec![
            MemberIdentity {
                member: "coordinator".to_string(),
                identity_pk: coord_pk,
                nostr_pk: "cc".repeat(32),
            },
            MemberIdentity {
                member: "bob".to_string(),
                identity_pk: bob_pk,
                nostr_pk: "dd".repeat(32),
            },
        ];
        let republic_id = molt_storage::republic_id("Guild", 1, 2, &identities);
        let change = ChainChange::Genesis {
            name: "Guild".to_string(),
            republic_id: republic_id.clone(),
            rule_m: 1,
            rule_n: 2,
            identities,
            agenda: "survive total loss".to_string(),
        };
        let bytes = molt_core::approval_bytes(&republic_id, 0, &change);
        let genesis = ChainBlock {
            height: 0,
            prev: GENESIS_PREV.to_string(),
            sigs: vec![
                RosterAttestation {
                    member: "coordinator".to_string(),
                    sig: molt_storage::identity_sign(&coord_sk, &bytes),
                },
                RosterAttestation {
                    member: "bob".to_string(),
                    sig: molt_storage::identity_sign(&bob_sk, &bytes),
                },
            ],
            change,
        };
        let change1 = ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"survived the loss"}),
        };
        let bytes1 = molt_core::approval_bytes(&republic_id, 1, &change1);
        let block1 = ChainBlock {
            height: 1,
            prev: molt_storage::content_hash(&molt_core::block_link_bytes(&republic_id, &genesis)),
            sigs: vec![RosterAttestation {
                member: "coordinator".to_string(),
                sig: molt_storage::identity_sign(&coord_sk, &bytes1),
            }],
            change: change1,
        };
        (vec![genesis, block1], republic_id)
    }

    /// An actionable recovery link with a bogus host — parseable, so
    /// `cmd_recover_start` arms the context (generation + link + phrase)
    /// before its honest no-transport failure; the injected
    /// `NetRecoverSealed` then materializes against that context (the same
    /// seam the two-instance tests drive).
    fn recover_link(member: &str, republic_id: &str) -> String {
        crate::recovery::RecoveryInvite {
            republic: "Guild".to_string(),
            member: member.to_string(),
            ticket: "ab".repeat(8),
            server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid"
                .to_string(),
            queue_id: "cd".repeat(12),
            wrap: "ef".repeat(32),
            republic_id: republic_id.to_string(),
        }
        .render()
    }

    fn storage_session(tmp: &tempfile::TempDir) -> SessionView {
        SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: tmp.path().join("workspaces").display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        }
    }

    #[test]
    fn recover_start_guards_the_link_and_the_storage() {
        let tmp = tempfile::tempdir().expect("tmp");
        rt().block_on(async {
            // a bare preview link carries no transport handover — not actionable
            let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
            let err = w
                .execute(Command::RecoverStart {
                    link: "molt://recover/Guild/bob/abcdef".to_string(),
                    phrase: "some phrase".to_string(),
                })
                .await
                .expect_err("a preview link cannot start a recovery");
            assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");

            // a storage-less node has nowhere to materialize the recovery
            let w2 = spawn(GroupConfig::demo(), SessionView::default());
            let err = w2
                .execute(Command::RecoverStart {
                    link: recover_link("bob", "f00d"),
                    phrase: "some phrase".to_string(),
                })
                .await
                .expect_err("a storage-less node cannot recover");
            assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");
        });
    }

    /// **The A2 keystone:** a completed rejoin materializes the recovered
    /// workspace from the verified chain — adopting the FULL chain (block 1's
    /// gated `Applied` projects into the surface state), anchoring the seat's
    /// phrase-derived identity, and entering the republic.
    #[test]
    fn recovery_materializes_the_workspace_from_the_full_verified_chain() {
        let tmp = tempfile::tempdir().expect("tmp");
        rt().block_on(async {
            let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
            let phrase = molt_storage::generate_seed_phrase().expect("phrase");
            let (chain, republic_id) = recovered_chain(&phrase);
            w.execute(Command::RecoverStart {
                link: recover_link("bob", &republic_id),
                phrase: phrase.clone(),
            })
            .await
            .expect("recover start");
            // the production path fails honestly (no transport in this build)
            // on the recovery notice channel — the armed context survives it
            let s = read_session(&w).await;
            assert_eq!(
                s.notice,
                format!("recover-failed:{}", crate::NO_TRANSPORT_YET),
                "the honest N-demo gap error rides the recovery notice"
            );

            // a stale-generation result is dropped without a trace
            let chain_json = serde_json::to_string(&chain).expect("chain json");
            w.execute(Command::NetRecoverSealed {
                member: "bob".to_string(),
                chain: chain_json.clone(),
                mls: String::new(),
                mesh: Vec::new(),
                generation: Some(999),
            })
            .await
            .expect("stale sealed");
            let s = read_session(&w).await;
            assert!(s.workspaces.is_empty(), "a stale result must not materialize");

            // the current-generation result materializes the workspace
            w.execute(Command::NetRecoverSealed {
                member: "bob".to_string(),
                chain: chain_json,
                mls: String::new(),
                mesh: Vec::new(),
                generation: Some(1),
            })
            .await
            .expect("sealed");
            let s = read_session(&w).await;
            assert_eq!(s.screen, Screen::Main, "entered the recovered republic");
            let ws = s
                .workspaces
                .iter()
                .find(|x| x.name == "Guild")
                .expect("the recovered workspace is listed");
            assert_eq!(s.active_workspace, ws.id);
            assert_eq!(ws.agenda, "survive total loss");
            // the FULL chain was adopted, not just the genesis: block 1's
            // gated Applied projects into the surface state
            let mem = read_surface(&w, Surface::Memory).await;
            assert_eq!(mem.applied.len(), 1, "block 1 projected");
            assert_eq!(mem.applied[0]["title"], "survived the loss");
        });
    }

    /// Defence in depth on the actor: a chain whose roster does not anchor the
    /// identity derived from THIS recovery's phrase is hard-rejected — a forged
    /// internal command (or a coordinator serving someone else's chain) must
    /// not materialize a workspace the seat cannot sign for.
    #[test]
    fn recovery_hard_rejects_a_chain_that_does_not_anchor_the_phrase() {
        let tmp = tempfile::tempdir().expect("tmp");
        rt().block_on(async {
            let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
            // the chain anchors an identity derived from a DIFFERENT phrase
            let other = molt_storage::generate_seed_phrase().expect("other phrase");
            let (chain, republic_id) = recovered_chain(&other);
            let phrase = molt_storage::generate_seed_phrase().expect("phrase");
            w.execute(Command::RecoverStart {
                link: recover_link("bob", &republic_id),
                phrase,
            })
            .await
            .expect("recover start");
            w.execute(Command::NetRecoverSealed {
                member: "bob".to_string(),
                chain: serde_json::to_string(&chain).expect("chain json"),
                mls: String::new(),
                mesh: Vec::new(),
                generation: Some(1),
            })
            .await
            .expect("sealed");
            let s = read_session(&w).await;
            assert!(s.workspaces.is_empty(), "an unanchored chain must not materialize");
            assert_ne!(s.screen, Screen::Main);
            assert!(
                s.notice.starts_with("recover-failed:"),
                "the failure surfaces to the operator; notice = {:?}",
                s.notice
            );
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
