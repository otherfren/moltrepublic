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
//! typed messages, reactions, deletion, the wire appliers and the P6
//! parking buffer), [`net`] (the `molt-net` glue as a module directory:
//! the log-backed outbox feed and the net builders in `mod.rs`, the
//! delivery guarantee, wire ingest, the relay file plane, coordinator-side
//! recovery, presence + net health, and the loopback demo mesh whose peers
//! replaced the old reply simulator), [`proposals`] (the gated
//! propose/approve/apply machine and snapshots), [`session`] (navigation,
//! settings, workspaces) and [`lifecycles`] (the three engine-run mocks:
//! restore / create / join over one `RunCore`). `State` groups its
//! workspace-scoped bookkeeping into sub-structs ([`DeliveryState`],
//! [`PresenceState`], [`FilePlane`], [`ChainProjection`],
//! [`RecoveryState`]); the unit tests live under `tests/`.

mod backup;
mod chain;
mod compaction;
mod chat;
mod configstore;
mod events;
mod files_state;
mod founding;
mod lifecycles;
mod loopback_mesh;
mod net;
mod nostr_ritual;
mod proposals;
mod recovery;
mod ritual_member;
mod relay_msg;
pub use relay_msg::known_headlines;
pub use relay_msg::{known_log_shapes, LogShape};
mod session;
mod transfer;
mod wiki_export;
mod wiki_index;

/// The wiki's body links — markdown destinations ending in `.md` plus the
/// readable `[[Name]]` / `[[pred::Name]]` form, code masked out. ONE
/// parser, [`link_parts`] included: the GUI's link navigation and its
/// tooltip read the same edges the index does
/// (`docs_archive/memory/knowledge_base_scale.md` §4.5).
pub use wiki_index::graph::{body_link_targets, body_links, link_parts, BodyLink, LinkParts};

/// The document header's boundaries, its properties and what counts as a
/// link inside one (§4.4). ONE parser, like [`body_links`]: the infobox
/// the GUI renders and the relations the index reads come from the same
/// rules, so a document cannot look different in the two.
pub use wiki_index::front_matter::{
    first_heading, key_ok as header_key_ok, link_target, properties, split as split_front_matter,
};

use std::collections::HashMap;
use std::path::PathBuf;

pub use configstore::ConfigStoreHandle;
#[doc(hidden)]
pub use chain::{verify_chain, ChainHead};
/// The public wiki-export verifier — the reference implementation an external
/// reviewer runs (`cargo run -p molt-engine --example verify_wiki_export`).
pub use chain::{verify_wiki_export, WikiExportReport};
/// Reading an export directory back: the I/O half of [`verify_wiki_export`].
pub use wiki_export::read_wiki_export;
#[doc(hidden)]
pub use recovery::RecoveryInvite;
#[doc(hidden)]
pub use recovery::{run_rejoin, RejoinOutcome};
#[doc(hidden)]
pub use founding::{
    make_seat_proof, member_identity, run_ritual_member, verify_seat_proof, FoundingInvite,
    InviteMaterial, Ratifier,
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

/// Shorten the resumable fetch's wait before it asks for missing pieces
/// (`docs_archive/files/mirroring.md` §3.2; 10 min in production). A test seam for
/// the integration tests, like `__spawn_with_reopen_transport`.
#[doc(hidden)]
pub fn __set_piece_want_after(d: std::time::Duration) {
    transfer::set_piece_want_after(d);
}

/// The delivery-guarantee beat (`Command::NetDeliveryTick`): due-ACK flush +
/// debounced persists. 1 s keeps the real ack latency at debounce+1s ≈ 4 s,
/// safely inside the sender's 30 s resend timer.
const DELIVERY_TICK_MS: u64 = 1_000;

/// The honest refusal for a recovery link carrying no v2 transport handover.
///
/// Recovery runs over relays since N4b step 6e; a legacy link names an SMP
/// server this build no longer speaks to, so there is nothing to dial.
/// Surfaced through recovery's EXISTING failure path (the recovery notice) —
/// never a fake success.
pub(crate) const LEGACY_RECOVERY_LINK: &str =
    "this recovery link is the old queue shape - ask the coordinator for a new one";

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
    let seams = SpawnSeams { persist: true, ritual_material_sink: Some(tx), ..SpawnSeams::default() };
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, seams);
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
    let seams = SpawnSeams {
        persist: true,
        ritual_material_sink: Some(tx),
        ritual_bootstrap: true,
        ..SpawnSeams::default()
    };
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, seams);
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
    let seams = SpawnSeams {
        persist: true,
        ritual_material_sink: Some(tx),
        ritual_bootstrap: true,
        recovery_material_sink: Some(rtx),
        ..SpawnSeams::default()
    };
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, seams);
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
    let seams = SpawnSeams { demo_mesh: true, ..SpawnSeams::default() };
    spawn_actor(config, session, cmd_tx, cmd_rx, seams)
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
    transport: molt_net::LoopbackTransport,
) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let seams = SpawnSeams { persist: true, reopen_seam: Some(transport), ..SpawnSeams::default() };
    spawn_actor(config, session, cmd_tx, cmd_rx, seams)
}

/// Storage-backed engine whose founding runs in the offline **sim** seam:
/// the founder's node simulates the other members over the loopback hub
/// (fast, deterministic, no network) — for founder-side sealing tests. The
/// product never uses this: a production founding fails honestly until N4's
/// Nostr transport lands.
#[doc(hidden)]
pub fn __spawn_sim_founding(config: GroupConfig, session: SessionView, persist: bool) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let seams = SpawnSeams { persist, ritual_sim: true, ..SpawnSeams::default() };
    spawn_actor(config, session, cmd_tx, cmd_rx, seams)
}

/// Storage-backed engine with the post-founding **mesh bootstrap** ON — the
/// production joiner configuration (`spawn_with_config` sets the same flag),
/// as a seam for multi-instance tests whose joiners must assemble a real
/// direct mesh after `JoinStart`.
#[doc(hidden)]
pub fn __spawn_with_storage_bootstrap(config: GroupConfig, session: SessionView) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let seams = SpawnSeams { persist: true, ritual_bootstrap: true, ..SpawnSeams::default() };
    spawn_actor(config, session, cmd_tx, cmd_rx, seams)
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
    let seams = SpawnSeams {
        store: Some(store.clone()),
        persist: true,
        // the real product runs the post-founding mesh bootstrap: the founder
        // (here) and the joiner (cmd_join_start) exchange announcements, then
        // each stands its runtime supervisor up over the direct mesh — live
        // peer-to-peer MLS chat the moment the republic is founded
        ritual_bootstrap: true,
        // the production engine: the demo-mesh test seam stays OFF — no
        // context ever spawns simulated peers here
        ..SpawnSeams::default()
    };
    let handle = spawn_actor(config, session, cmd_tx, cmd_rx, seams);
    Ok((handle, store))
}

fn spawn_inner(
    config: GroupConfig,
    session: SessionView,
    store: Option<ConfigStoreHandle>,
    persist: bool,
) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    let seams = SpawnSeams { store, persist, ..SpawnSeams::default() };
    spawn_actor(config, session, cmd_tx, cmd_rx, seams)
}

/// What a spawner wires into the actor beyond its config and session:
/// persistence and the config store (the production pair), an injected
/// transport runtime (the demo peers), and the test-only ritual / recovery
/// / demo-mesh / reopen seams. `Default` is everything off - a session-only
/// engine with no fake peers; [`spawn_with_config`] is the production
/// shape (store + persist + bootstrap, nothing else).
#[derive(Default)]
pub(crate) struct SpawnSeams {
    pub(crate) store: Option<ConfigStoreHandle>,
    pub(crate) persist: bool,
    pub(crate) net: Option<net::NetRuntime>,
    pub(crate) ritual_material_sink: Option<std::sync::mpsc::Sender<Vec<founding::InviteMaterial>>>,
    pub(crate) ritual_sim: bool,
    pub(crate) ritual_bootstrap: bool,
    pub(crate) recovery_material_sink: Option<std::sync::mpsc::Sender<recovery::RecoveryMaterial>>,
    pub(crate) demo_mesh: bool,
    pub(crate) reopen_seam: Option<molt_net::LoopbackTransport>,
}

fn spawn_actor(
    config: GroupConfig,
    session: SessionView,
    cmd_tx: mpsc::Sender<Envelope>,
    mut cmd_rx: mpsc::Receiver<Envelope>,
    seams: SpawnSeams,
) -> WalletHandle {
    let (ev_tx, _keep) = broadcast::channel::<Event>(EVENT_QUEUE);

    let mut state = State::new(
        config,
        session,
        ev_tx.clone(),
        cmd_tx.clone(),
        seams.store,
        seams.persist,
        seams.net,
    );
    state.ritual_material_sink = seams.ritual_material_sink;
    state.ritual_sim = seams.ritual_sim;
    state.ritual_bootstrap = seams.ritual_bootstrap;
    state.recovery.material_sink = seams.recovery_material_sink;
    state.demo_mesh = seams.demo_mesh;
    state.reopen_seam = seams.reopen_seam;
    // the presence ticker lives as long as the actor: it re-ages the member
    // pills from their real last-seen stamps (net/presence.rs::cmd_net_presence_tick)
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
    /// The ratified founding feature set (roster-v5) from the genesis.
    /// `None` = founded pre-v5 (the legacy baseline applies).
    pub(crate) features: Option<Vec<String>>,
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
/// The running group runtime of an open Nostr workspace: the handle to shut
/// it down and the wakeup its outbox waits on.
pub(crate) struct GroupNet {
    pub(crate) handle: molt_net::group_runtime::GroupHandle,
    /// The SAME group the runtime advances — the engine snapshots this ratchet
    /// on a clean close, and a snapshot→restore round trip here would reuse
    /// sender generations (which are replay-rejected and silently lost).
    pub(crate) mls: std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>,
    pub(crate) wakeup: tokio::sync::watch::Sender<u64>,
    /// What the runtime reports about the channel — folded into
    /// `session.net_health` on the presence beat (`apply_group_health`).
    pub(crate) health: tokio::sync::watch::Receiver<molt_net::group_runtime::GroupHealth>,
    /// The file trickle sender riding this runtime's channel
    /// (`docs_archive/files/mirroring.md` §3.2); stops with the handle.
    pub(crate) trickle: molt_net::trickle::TrickleHandle,
}

/// This seat's live Nostr transport material for the open workspace.
///
/// It exists because the sealed `transport.state` was being read at open and
/// then thrown away except for `identity_sk`: a reopened survivor held its
/// governance signing key but neither its own transport secret nor the group's
/// relay list, so it could not build a [`molt_net::ritual_net::RitualNet`] at
/// all — which is why minting a recovery link reported "mesh-not-running" on a
/// republic that has no mesh and needs none (N4b §8.8 step 5a).
pub(crate) struct NostrTransport {
    /// This seat's WORKING transport secret — the 32-byte secp256k1 scalar
    /// whose public half is the anchor other members address gift wraps to.
    pub(crate) sk: zeroize::Zeroizing<Vec<u8>>,
    /// The relays the GROUP agreed on at founding/join. What this node may
    /// actually dial is its own confirmed pool intersected with these — the
    /// two are deliberately different (`relay_pool.md` §3).
    pub(crate) relays: Vec<String>,
    /// The group's stable h-tag seed, from the Welcome. Without it this node
    /// cannot compute a single `h` tag, so it can neither publish a 445 nor
    /// subscribe to one — it was persisted at founding/join and then, like
    /// the secret above, left on disk.
    ///
    /// Adopted ahead of its consumer: the group runtime (N5.2) is the first
    /// thing that reads it, and adopting it in the same place as the secret
    /// keeps the "a Nostr workspace has all three or none" rule in ONE
    /// match arm rather than two.
    pub(crate) rotation_seed: [u8; 32],
}

/// The delivery-guarantee bookkeeping of the open workspace
/// (`docs_archive/transport/delivery_guarantee.md`): the per-sender accept
/// windows and their persist debounces, the due ACKs, the G7 in-order park,
/// the send/link trouble pins that drive presence and health, and the last
/// broadcast claim sheet. Active-workspace scope: `reset_workspace_state`
/// clears every field.
pub(crate) struct DeliveryState {
    /// The last broadcast claim sheet published, so an unchanged one is not
    /// republished — full state means silence is the correct steady state.
    pub(crate) last_group_ack: Option<molt_net::group_ack::GroupAck>,
    /// Per SENDER: which of that sender's log seqs this engine has accepted
    /// (delivery guarantee §4.2 — the envelope-level dedup twin of the mesh
    /// ACK payload). Loaded from `transport.state` at open, mutated on every
    /// authenticated wire delivery, persisted debounced + at close. Active-
    /// workspace scope — [`State::reset_workspace_state`] clears it.
    pub(crate) accepted: std::collections::BTreeMap<MemberId, molt_core::AcceptedWindow>,
    /// Whether [`DeliveryState::accepted`] changed since it was last persisted (the
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
    /// `ORDERED_PARK_GIVEUP_SECS` (net/delivery.rs). Runtime-only, workspace
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
    pub(crate) unreachable: std::collections::HashSet<MemberId>,
    /// Inbound legs currently down (member → reason), reported by the
    /// resubscribe watchdog — drives `NetHealth::Degraded` (Stage B).
    pub(crate) link_down: std::collections::BTreeMap<MemberId, String>,
    /// Outbound legs whose sends keep failing (member → reason) — set by
    /// `NetSendFailed`, cleared by `NetSendOk` (Stage B).
    pub(crate) send_stuck: std::collections::BTreeMap<MemberId, String>,
    /// `presence_now` of the last wire-crossing frame the engine emitted to a
    /// real mesh (stamped in [`State::record`]). Read by the debounced live
    /// MLS-ratchet persist (`persist_mls_if_due` — "did anything go out since
    /// the last snapshot?"). Runtime-only; reset with the workspace.
    pub(crate) last_mesh_out: u64,
}

/// Presence bookkeeping of the open workspace: the test-only clock seam every
/// presence stamp and aging pass reads through (`State::presence_now`), and
/// the poke / auto-wake cooldowns. Active-workspace scope for the cooldowns;
/// the clock seam outlives a workspace switch.
pub(crate) struct PresenceState {
    /// Per-sender cooldown for accepted pokes (`member → presence_now of the
    /// last reacted poke`): a poke inside the window is dropped quietly, so
    /// a flooding member cannot ring this node's sound or spawn its wake
    /// command in a loop. Active-workspace scope.
    pub(crate) poke_at: std::collections::HashMap<MemberId, u64>,
    /// Global holdoff stamp for the pending-vote auto-wake (`presence_now`
    /// of the last fired wake): new proposals inside the window do not
    /// re-spawn the wake command — one nudge, then the agent reads state.
    pub(crate) wake_at: Option<u64>,
    /// Presence clock **test seam** (same posture as [`State::demo_mesh`]):
    /// `None` in every production context — presence stamping/aging then
    /// runs on the shared [`now_secs`] clock; tests pin it to age pills
    /// deterministically.
    pub(crate) clock_override: Option<u64>,
}

/// The file plane of the open workspace: this seat's own shares and their
/// live download status, the sharer-side serve throttle, and the RELAY plane's
/// series stamps, pending fetches, in-flight publishes and fetch tasks
/// (`file_transfer_nostr.md`). Active-workspace scope: `reset_workspace_state`
/// clears the relay-plane maps and aborts the fetches.
pub(crate) struct FilePlane {
    /// MY shares: message id → local source path (runtime mirror of
    /// `prefs.shared_files`; NEVER wire, NEVER log — the paths would leak
    /// this node's filesystem layout).
    pub(crate) share_paths: HashMap<MessageId, std::path::PathBuf>,
    /// Requester-side live download status per share (runtime-only; feeds
    /// [`molt_core::UploadView::download`]).
    pub(crate) downloads: HashMap<MessageId, molt_core::DownloadView>,
    /// Sharer-side serve throttle: at most 2 concurrent uploads; further
    /// requests queue on the semaphore instead of saturating the uplink.
    pub(crate) serve_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// RELAY file plane (`file_transfer_nostr.md`): the known publish stamp
    /// per share — what a fetch names the series' h-tag window with.
    /// Runtime-only; re-learned from `FileServed` announcements (a fetch
    /// without a stamp asks via `FileWanted`).
    pub(crate) series: HashMap<molt_core::MessageId, u64>,
    /// Downloads waiting for a `FileServed` announcement: the fetch spawns
    /// the moment the stamp arrives. Runtime-only.
    pub(crate) pending:
        HashMap<molt_core::MessageId, (crate::transfer::FetchTarget, crate::transfer::DestSpec)>,
    /// Shares whose lazy series publish is in flight (sharer-side dedup —
    /// a burst of `FileWanted`s must not publish the series N times).
    pub(crate) serving: std::collections::HashSet<molt_core::MessageId>,
    /// Abort handles of running relay-plane fetch tasks (FP3): each holds
    /// a PRIVATE subscription no net teardown reaches, so the workspace
    /// boundary ends them explicitly. Inbound-only readers — abort is safe
    /// (the landing write runs inside `spawn_blocking`, which an abort
    /// never interrupts mid-file).
    pub(crate) fetches: Vec<tokio::task::AbortHandle>,
    /// Sharer-side: when this node last ANNOUNCED each share's stamp — a
    /// `FileWanted` right after an announce means the requester cannot use
    /// that series (pruned/foreign epoch) and a fresh publish is due.
    pub(crate) announced: HashMap<molt_core::MessageId, u64>,
    /// The mirror gossip (`docs_archive/files/mirroring.md` §3.4): this seat's
    /// declaration and what the members told it - the runtime copy of
    /// `transport.state`'s, loaded at open.
    pub(crate) mirror: molt_core::MirrorState,
    /// When this seat last sent its declaration / status, what the status
    /// said, when it last answered an ask, and whether it asked this run.
    pub(crate) mirror_decl_sent: u64,
    pub(crate) mirror_status_sent: u64,
    pub(crate) mirror_status_last: Vec<molt_core::MirrorHold>,
    pub(crate) mirror_who_answered: u64,
    pub(crate) mirror_who_asked: bool,
    /// The mirror worker (§3.3): the running fetch per series, the series
    /// waiting for the sharer's stamp, the planning beat, the one notice.
    pub(crate) mirror_fetches: HashMap<molt_core::MessageId, tokio::task::AbortHandle>,
    pub(crate) mirror_pending: HashMap<molt_core::MessageId, u64>,
    pub(crate) mirror_planned_at: u64,
    /// K6: the running fetch of the folded wiki base (at most one).
    pub(crate) wiki_base_fetch: Option<tokio::task::AbortHandle>,
    /// Unix seconds before which no new base fetch starts. A fetch that
    /// finds nobody online ends in seconds, and without this the beat
    /// would restart it every second for as long as the republic is
    /// quiet.
    pub(crate) wiki_base_next_try: u64,
    /// Which commitment the running fetch is after. A second cut moves the
    /// commitment, and holders keep only the CURRENT tree - so a fetch
    /// left running for the old one would wait forever for bytes nobody
    /// has any more.
    pub(crate) wiki_base_fetching: Option<String>,
    pub(crate) mirror_quota_noted: bool,
    /// Verified pieces of each running mirror fetch, as last reported.
    pub(crate) mirror_progress: HashMap<molt_core::MessageId, u32>,
    /// A member's status pages still arriving ([`MirrorPages`]).
    pub(crate) mirror_pages: HashMap<MemberId, MirrorPages>,
    /// Series whose mirror fetch failed: no retry before this stamp.
    pub(crate) mirror_failed: HashMap<molt_core::MessageId, u64>,
}

/// A status generation being collected: `(generation, page count, when
/// its first page arrived, the pages so far)`.
pub(crate) type MirrorPages = (u64, u16, u64, std::collections::BTreeMap<u16, Vec<molt_core::MirrorHold>>);

/// The republic's persistent commit-block chain on this holder
/// (`docs_archive/chain/persistent_chain.md`) and everything derived from
/// or feeding it: the verified blocks + head + cached walk, the checkpoint
/// blobs, the applied / signature / anchor / relay-ledger projections, the
/// ephemeral vote collections (signatures, declines, withdrawals, the exact
/// change per open proposal) and the catch-up buffer. Rebuilt from the chain
/// or the gossip, never trusted from elsewhere; `reset_workspace_state`
/// clears it with the workspace.
pub(crate) struct ChainProjection {
    /// The republic's persistent commit-block chain — the converged, verified
    /// governance record (`docs_archive/chain/persistent_chain.md`). Block 0 is the
    /// founding; empty when no chain-aware workspace is open.
    pub(crate) blocks: Vec<molt_core::ChainBlock>,
    /// The verified head of [`ChainProjection::blocks`] (`None` = empty chain).
    pub(crate) head: Option<chain::ChainHead>,
    /// The verification walk over [`ChainProjection::blocks`], kept so appending a block
    /// costs that block's signatures instead of a re-walk from the anchor.
    /// Runtime-only and always re-derivable; `None` simply means the next
    /// append pays one full verification and re-fills it. Never trusted
    /// blindly — [`chain::ChainWalk::describes`] must still match the chain,
    /// and [`State::set_checkpoint_blob`] clears it.
    pub(crate) walk: Option<chain::ChainWalk>,
    /// WP4b: a SERVED blob awaiting its anchor block (runtime-only, never
    /// persisted — re-served on the next catch-up if lost).
    pub(crate) pending_served_blob: Option<molt_core::CheckpointState>,
    /// WP4b: the checkpoint blob a PRUNED holder anchors on — `Some` once
    /// history below a sealed checkpoint was dropped locally; [`ChainProjection::blocks`]
    /// then starts with the checkpoint block instead of the genesis.
    pub(crate) checkpoint_blob: Option<molt_core::CheckpointState>,
    /// K6: the ratified wiki tree behind the commitment
    /// [`ChainProjection::checkpoint_blob`] carries
    /// (`knowledge_base_scale.md` §4.9). A folded cut names the tree by
    /// content hash and drops its patches; the bytes travel on the file
    /// plane and live here once they verify against that hash. `None` =
    /// no folded cut behind this holder, or the tree is still being
    /// fetched (base-pending) - the two are told apart by whether the
    /// blob's memory group carries a commitment.
    pub(crate) wiki_base: Option<std::collections::BTreeMap<String, String>>,
    /// The gated surfaces' applied logs **derived from the chain** — a separate
    /// projection from the legacy log-driven [`State::applied`] so the two never
    /// collide: a single-operator workspace keeps its counted governance in
    /// `applied` (chain genesis-only → this stays empty), while real
    /// threshold-committed governance lands here. Reads combine both. Re-folded
    /// wholesale on every chain change, so a re-base is free. Same
    /// `(proposal id, payload)` shape as [`State::applied`]; the id is always
    /// present here (every `Applied` block names its proposal).
    pub(crate) applied: HashMap<Surface, Vec<(Option<u64>, Value)>>,
    /// The sealing signatures per Applied proposal id — a chain PROJECTION
    /// like [`ChainProjection::applied`], maintained by the same two writers
    /// (full re-fold + append). `ProposalView` building reads voters from
    /// here in O(1); the per-card reverse chain scan it replaces made every
    /// snapshot O(cards × chain) once ALL applied history materializes
    /// cards (review 2026-08-16).
    pub(crate) applied_sigs: HashMap<u64, Vec<molt_core::RosterAttestation>>,
    /// WORKING transport anchors — `member -> nostr_pk` for every seat a
    /// `Restored` block re-anchored. A chain PROJECTION like
    /// [`ChainProjection::applied`]: rebuilt from the chain, never persisted
    /// separately, so it cannot drift from what the blocks say.
    ///
    /// The roster's anchor is the immutable FOUNDING record and stays that
    /// way; this is where a recovered seat's current key lives. Read it
    /// through [`State::working_nostr_pk`] — a send site that reaches for
    /// `identities[i].nostr_pk` addresses a key the member no longer holds,
    /// and does so silently.
    pub(crate) anchors: HashMap<MemberId, String>,
    /// The relay LEDGER (R3b): each seat's DECLARED reachable pool, folded
    /// from `Membership` blocks exactly like [`ChainProjection::anchors`] (and
    /// seeded from the checkpoint summary after a cut). Read through
    /// [`State::member_relays`], which falls back to the ratified group pool
    /// for seats that never declared. The split-detection input (R4).
    pub(crate) member_relays: HashMap<MemberId, Vec<String>>,
    /// R4: split pairs already warned about (runtime-only) — the log line
    /// fires once per pair, the members-surface marker stays live.
    pub(crate) split_noted: std::collections::HashSet<(MemberId, MemberId)>,
    /// Ephemeral per-proposal signature collection for chain governance
    /// (keyed by proposal id; never persisted, rebuilt from gossip). Once a
    /// proposal gathers m distinct signatures the committer seals a block.
    pub(crate) pending_sigs: HashMap<u64, chain::PendingApproval>,
    /// The proposals THIS node signed (its own decisions), written only by
    /// the own signing path. The re-base re-expresses a standing decision
    /// from here — never from `pending_sigs`, whose entries any peer fills
    /// under any roster name (review 2026-08-25: a forged own entry made
    /// the node sign for real).
    pub(crate) own_approvals: std::collections::BTreeSet<u64>,
    /// When this node last served a catch-up to each requester: a
    /// `ChainRequest` is an amplifier (the whole chain + blob + open cards
    /// per frame, into the durable log), so one requester is served at
    /// most once per `CHAIN_SERVE_DEBOUNCE_SECS` (net/ingest.rs) (review C3).
    pub(crate) served_at: HashMap<MemberId, u64>,
    /// Declines waiting for their proposal (keyed by proposal id, one entry
    /// per member with the decline's ts): a decline travels on a different
    /// sender's G7 chain than its proposal, and an own-log decline replays
    /// before a re-served foreign proposal returns — parked votes register
    /// the moment the proposal is known ([`State::register_decline`]).
    /// Ephemeral and bounded, like the signature collection above.
    pub(crate) pending_declines: HashMap<u64, Vec<(MemberId, u64, String)>>,
    /// Parked withdraws whose proposal is not known yet (one slot per id —
    /// a withdraw has exactly one legitimate author), the decline park's
    /// sibling. Drained by `receive_proposed`.
    pub(crate) pending_withdrawals: HashMap<u64, (MemberId, u64)>,
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
}

/// Recovery on both sides of the open workspace: what this seat coordinates
/// (pending re-admissions, the spend-once tickets, the announce window and
/// extension cooldown, the standing seat inbox and the per-link inboxes, the
/// self-heal reattach cap) and what it runs as the rejoiner (the task, its
/// generation, the parsed link + phrase, the transport slot, the test-only
/// material sink). The phrase in `ctx` is `Zeroizing`; this struct
/// deliberately derives nothing, so it can never be printed.
pub(crate) struct RecoveryState {
    /// Live recovery-inbox tasks (N4b step 5), one per minted link.
    ///
    /// They MUST be aborted when the workspace closes. The loopback twin gets
    /// away with forgetting its handle because that task dies with the ritual
    /// transport; a relay subscription does not — the pool outlives the
    /// workspace, so a forgotten task would sit on a relay socket forever and
    /// every mint would add another.
    pub(crate) inboxes: Vec<tokio::task::JoinHandle<()>>,
    /// The STANDING seat inbox (`detached_reattach.md` §2.1): an open Nostr
    /// workspace's own-anchor 1059 subscription, so a restored seat can
    /// announce itself without a minted link. Torn down with the net.
    pub(crate) seat_inbox: Option<tokio::task::JoinHandle<()>>,
    /// Self-heal bookkeeping (`detached_reattach.md` §2.4): how often this
    /// session already re-attached itself out of a stuck epoch, and when it
    /// last tried. Session-lifetime cap — two devices restoring the same
    /// seat would otherwise re-key each other in an endless ping-pong.
    pub(crate) reattach_attempts: u32,
    pub(crate) last_reattach: Option<u64>,
    /// Self-service reattach cooldown (`detached_reattach.md` §2.2): a
    /// `(member, new_anchor) → unix stamp` map that swallows relay replays
    /// of an accepted request (the accept window does not cover 1059 wraps).
    pub(crate) unsolicited_cooldown: std::collections::HashMap<(String, String), u64>,
    /// Recoveries this node is coordinating, keyed by the returning **member**
    /// (so the trigger fires whether this node commits the Restored block or
    /// receives it): the fresh KeyPackage + reply queue, kept until the Restored
    /// block commits and the coordinator re-keys the group + sends the Welcome.
    pub(crate) pending: HashMap<String, chain::PendingRecovery>,
    /// Recovery tickets this node has minted and is still listening for — the
    /// spend-once guard. A ticket is inserted when a recovery link is minted and
    /// removed the moment a valid request spends it, so a replayed request on a
    /// live recovery queue finds a dead ticket and is dropped.
    pub(crate) tickets: HashMap<String, MemberId>,
    /// Members whose recovery re-key just completed and whose **mesh announce**
    /// the coordinator therefore expects on the recovery queue (dynamic mesh
    /// membership) — armed in `coordinator_rekey`, disarmed when the announce
    /// is handled. The recovery queue can never re-point any OTHER member's
    /// links.
    pub(crate) mesh_window: std::collections::HashSet<MemberId>,
    /// Per-member cooldown for mesh extensions (`member → now_secs of the
    /// last accepted announce`): folding a link in costs every peer a full
    /// supervisor teardown+rebuild+fsync, so a member re-announcing inside
    /// the window is ignored — one rotation per member per minute is ample,
    /// and it caps the churn a misbehaving member can inflict.
    pub(crate) mesh_extension_at: std::collections::HashMap<MemberId, u64>,
    /// The recovery twin of [`Self::ritual_material_sink`]: when set, the
    /// recovery link-mint hands the minted queue's transport handover out on
    /// this channel so a *second* engine can run the returning-member side.
    /// Only the two-instance recovery dev test installs it; a real mint reports
    /// the link to the operator instead.
    pub(crate) material_sink:
        Option<std::sync::mpsc::Sender<recovery::RecoveryMaterial>>,
    /// The running Nostr rejoiner task (N4b step 6e), the [`State::join_task`]
    /// twin — aborted on a restarted recovery, for the same two reasons.
    pub(crate) task: Option<tokio::task::JoinHandle<()>>,
    /// A separate incarnation counter for the **recovery** flow (an off-actor
    /// rejoin) — the twin of [`State::join_generation`].
    pub(crate) generation: u64,
    /// While a recovery is in flight: the parsed recovery link + the phrase
    /// the rejoin task runs with. `cmd_net_recover_sealed` re-derives the seat
    /// identity from the phrase (the ritual salts it with a workspace-id
    /// string, so it must NOT be re-derived from the member handle) and checks
    /// the served chain against the link. `None` outside a recovery.
    pub(crate) ctx: Option<(recovery::RecoveryInvite, zeroize::Zeroizing<String>)>,
    /// The **rejoiner's** transport slot — the twin of
    /// [`State::join_transport`]: the off-actor rejoin task parks a clone of
    /// its transport here (its `Arc` owns the re-established mesh queues'
    /// receive credentials), so `cmd_net_recover_sealed` can stand the runtime
    /// supervisor up over the recovered mesh. Replaced per `RecoverStart`.
    pub(crate) transport:
        std::sync::Arc<std::sync::Mutex<Option<molt_net::LoopbackTransport>>>,
}

/// The folded Memory base, kept across reads
/// (`docs_archive/memory/knowledge_base_scale.md` §4.1). A pure DERIVATION of the
/// applied logs: dropping it costs a refold, never a different tree.
pub(crate) struct WikiCache {
    /// The folded tree.
    pub(crate) tree: std::collections::BTreeMap<String, String>,
    /// How many patches APPLIED into it (`wiki_fold_with_rev`'s revision).
    pub(crate) rev: u64,
    /// Entries already folded from the legacy log / from the chain log —
    /// the concat order is legacy-then-chain, so an append extends the
    /// cache only while the OTHER half stands still.
    pub(crate) legacy: usize,
    /// See [`WikiCache::legacy`].
    pub(crate) chain: usize,
    /// The epoch the fold was taken under.
    pub(crate) epoch: u64,
    /// What each applied revision TOUCHED, in revision order (§4.11).
    /// It rides the cache so it is invalidated exactly when the tree is -
    /// one staleness story, not two - and `wiki_changes` then answers in
    /// O(entries since its `since_rev`) instead of a refold per call. It
    /// holds paths, never content, and a folded cut clears it.
    pub(crate) history: Vec<WikiRevChanges>,
    /// The folded base underneath (K6), if the republic has cut. A cut
    /// RE-BASES the revision counter, and the extension loop refolds
    /// whenever one appears, so this only ever changes with the whole
    /// cache.
    pub(crate) base: Option<String>,
}

/// What ONE applied revision touched (§4.11), as the fold derives it.
pub(crate) struct WikiRevChanges {
    /// The revision this patch produced.
    pub(crate) rev: u64,
    /// One entry per file the patch named.
    pub(crate) items: Vec<WikiTouch>,
}

/// One file of one applied patch: the path as the patch LEFT it, what it
/// did, and - on a rename - where it came from. Coalescing across
/// revisions happens at read time, not here.
pub(crate) struct WikiTouch {
    /// The path the change left behind (the old path for a deletion).
    pub(crate) path: String,
    /// `"added"`, `"modified"`, `"deleted"` or `"renamed"`.
    pub(crate) kind: &'static str,
    /// The path a rename moved away from.
    pub(crate) from: Option<String>,
}

/// Both derived indexes over one folded base, as an off-actor build
/// hands them back (§4.5/§4.6). They are built together because they
/// parse the same documents: two tasks would parse the tree twice.
pub(crate) struct WikiIndexes {
    /// The link graph.
    pub(crate) graph: wiki_index::graph::WikiGraph,
    /// The full-text index.
    pub(crate) search: wiki_index::search::WikiSearch,
}

/// A pending `wiki_patch` proposal, parsed once (§4.2).
pub(crate) struct PendingPatch {
    /// The parsed files.
    pub(crate) files: Vec<molt_core::wiki_fold::PatchFile>,
    /// Every path they name, old side and new side.
    pub(crate) paths: std::collections::BTreeSet<String>,
}

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
    /// B2 — the seat's own per-channel read cursors (channel storage key →
    /// message id hex), the WORKING copy of `prefs.read_cursors`: loaded at
    /// open (`adopt_read_cursors`), written through on every `MarkRead`.
    pub(crate) read_cursors: std::collections::BTreeMap<String, String>,
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
    pub(crate) parked: chat::ParkedRefs,
    pub(crate) files: FilePlane,
    /// The GUI's last published rendering claim (`gui_over_mcp.md`):
    /// written only by `Command::UiPublish` (the window's live mirror),
    /// read back over `read_ui_state`. `None` = no window published yet.
    pub(crate) ui_state: Option<molt_core::UiSnapshot>,
    /// Applied transition log per gated surface: `(proposal id, payload)`
    /// pairs — one source for the payload and its origin, so the snapshot's
    /// parallel id track can never drift. `None` = origin unknown (restored
    /// from a pre-id dump).
    pub(crate) applied: HashMap<Surface, Vec<(Option<u64>, Value)>>,
    /// The folded Memory base, cached across reads (§4.1). Never persisted,
    /// never consensus input — a keystone pins it equal to a fresh fold.
    pub(crate) wiki_cache: Option<WikiCache>,
    /// Bumped whenever the Memory projection changes in a way an APPEND
    /// cannot describe: a wholesale rebuild, a blob swap, a restore, a
    /// close. The cache extends itself only while this stands still.
    pub(crate) applied_epoch: u64,
    /// Pending `wiki_patch` proposals, parsed once (§4.2). A CACHE: a miss
    /// costs a parse, never a wrong verdict — the walk's candidate list
    /// always comes from `proposals`.
    pub(crate) wiki_pending: HashMap<u64, PendingPatch>,
    /// The link graph over the folded base (§4.5). Built on first use and
    /// updated per applied patch; like every index here it is a pure
    /// derivation, so dropping it costs a rebuild and nothing else.
    pub(crate) wiki_graph: Option<wiki_index::graph::WikiGraph>,
    /// The epoch [`State::wiki_graph`] was built under.
    pub(crate) wiki_graph_epoch: u64,
    /// Paths whose documents changed since the graph last resolved.
    pub(crate) wiki_graph_dirty: std::collections::BTreeSet<String>,
    /// The full-text index over the folded base (§4.6). RAM only: an index
    /// of the wiki IS the wiki, so it never lies beside the sealed
    /// workspace.
    pub(crate) wiki_search: Option<wiki_index::search::WikiSearch>,
    /// The epoch [`State::wiki_search`] was built under.
    pub(crate) wiki_search_epoch: u64,
    /// Paths whose documents changed since the index last committed.
    pub(crate) wiki_search_dirty: std::collections::BTreeSet<String>,
    /// The epoch an OFF-ACTOR index build is running under, if any - the
    /// in-flight guard, so N reads spawn one build and not N
    /// (`docs_archive/memory/knowledge_base_scale.md` §4.5/§4.6).
    pub(crate) wiki_index_building: Option<u64>,
    /// Where an off-actor build parks its result. Neither index type is
    /// serializable, so the artefacts ride a shared slot and the internal
    /// command carries only the epoch they were built under - the
    /// `restore_staging` idiom.
    pub(crate) wiki_index_staging: std::sync::Arc<std::sync::Mutex<Option<WikiIndexes>>>,
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
    /// The open workspace's transport discriminator, from its sealed
    /// `transport.state`. Read FIRST wherever the two shapes diverge — a
    /// Nostr republic carries no queue creds or mesh links by design, not by
    /// damage (N4 §7.5).
    pub(crate) transport_kind: Option<molt_core::TransportKind>,
    /// The open workspace's Nostr transport material, adopted alongside
    /// [`State::identity_sk`] at founding/join and at every reopen.
    ///
    /// `None` on a legacy/loopback workspace, and on a Nostr one whose secret
    /// is missing or malformed — the two are distinguished by
    /// [`State::transport_kind`], because "this is not a Nostr republic" and
    /// "this IS one but its transport secret did not load" are different
    /// faults and must not share a refusal.
    pub(crate) nostr: Option<NostrTransport>,
    /// The kind-445 group runtime of an open Nostr workspace (N5.2), with the
    /// wakeup its outbox reads. `None` on a legacy/queue workspace, and on a
    /// Nostr one whose MLS group or relay set did not come up.
    pub(crate) group_net: Option<GroupNet>,
    pub(crate) delivery: DeliveryState,
    pub(crate) recovery: RecoveryState,
    pub(crate) chain: ChainProjection,
    pub(crate) presence: PresenceState,
    /// Are clearnet relays activated for THIS session? Runtime-only **on
    /// purpose** — it is never persisted, so every start re-arms the gate and
    /// no clearnet packet leaves before the user acts again
    /// (`docs_archive/transport/relay_pool.md` §3). Onion relays are unaffected.
    pub(crate) clearnet_session: bool,
    /// Relay confirmations whose probe verdict has not landed yet
    /// (`cmd_relay_confirm` → async probe → `cmd_net_relay_probed`).
    /// Founding and joining REFUSE while this is non-empty: minting invites
    /// (or gating a link) from a pool the operator just changed silently
    /// dropped the relay they had consented to seconds earlier (observed
    /// 2026-08-16 — the "invites went stale" note fired right after the
    /// ritual opened). Every probe path is timeout-bounded, so an entry
    /// always clears.
    pub(crate) pending_relay_confirms: std::collections::HashSet<String>,
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
    /// The last REAL bucket listing's objects (None until one succeeded, and
    /// cleared with it). Kept so the orphan classification can be re-run
    /// against the CURRENT workspace list when it changes (a deleted
    /// workspace's copies must surface as restorable orphans without a fresh
    /// network round — field bug 2026-08-24).
    pub(crate) backup_listing: Option<Vec<molt_core::BackupObject>>,
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
    pub(crate) reopen_seam: Option<molt_net::LoopbackTransport>,
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
    pub(crate) runtime_transport: Option<molt_net::LoopbackTransport>,
    /// The **joiner's** equivalent: the off-actor join task hands its ritual
    /// transport (which owns the bootstrap queues' receive credentials) back
    /// through this slot just before it reports `NetJoinSealed`, so the runtime
    /// supervisor reuses the same instance. A fresh per-join `Arc` (replaced in
    /// `cmd_join_start`) isolates a stale task's late fill from a new join.
    pub(crate) join_transport: std::sync::Arc<std::sync::Mutex<Option<molt_net::LoopbackTransport>>>,
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
    /// The channel the off-actor join task waits on for the joiner's charter
    /// ratification (`JoinConfirmCharter` sends `true`; cancel drops it). Set
    /// while a join is paused at the ratification step, else `None`.
    pub(crate) join_confirm: Option<mpsc::Sender<bool>>,
    /// The joiner's phrase-backup gate (`seed_backup_confirmation.md` ❻½):
    /// the member task blocks on the paired receiver after ratifying;
    /// `cmd_confirm_seed_backup` releases it. Dropped on invalidation, so a
    /// torn-down join ends the wait.
    pub(crate) join_backup: Option<mpsc::Sender<bool>>,
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
            read_cursors: std::collections::BTreeMap::new(),
            chat_pruned: false,
            chat_pruned_counts: std::collections::BTreeMap::new(),
            compacted_at: 0,
            parked: chat::ParkedRefs::new(),
            files: FilePlane {
                wiki_base_fetch: None,
                wiki_base_next_try: 0,
                wiki_base_fetching: None,
                share_paths: HashMap::new(),
                downloads: HashMap::new(),
                serve_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
                series: HashMap::new(),
                pending: HashMap::new(),
                serving: std::collections::HashSet::new(),
                fetches: Vec::new(),
                announced: HashMap::new(),
                mirror: molt_core::MirrorState::default(),
                mirror_decl_sent: 0,
                mirror_status_sent: 0,
                mirror_status_last: Vec::new(),
                mirror_who_answered: 0,
                mirror_who_asked: false,
                mirror_fetches: HashMap::new(),
                mirror_pending: HashMap::new(),
                mirror_planned_at: 0,
                mirror_quota_noted: false,
                mirror_progress: HashMap::new(),
                mirror_pages: HashMap::new(),
                mirror_failed: HashMap::new(),
            },
            ui_state: None,
            applied,
            wiki_cache: None,
            applied_epoch: 0,
            wiki_pending: HashMap::new(),
            wiki_graph: None,
            wiki_graph_epoch: 0,
            wiki_graph_dirty: std::collections::BTreeSet::new(),
            wiki_search: None,
            wiki_search_epoch: 0,
            wiki_search_dirty: std::collections::BTreeSet::new(),
            wiki_index_building: None,
            wiki_index_staging: std::sync::Arc::new(std::sync::Mutex::new(None)),
            proposals: HashMap::new(),
            next_id: 1,
            next_seq: 1,
            replica: None,
            identity_sk: None,
            transport_kind: None,
            nostr: None,
            group_net: None,
            delivery: DeliveryState {
                last_group_ack: None,
                accepted: std::collections::BTreeMap::new(),
                accepted_dirty: false,
                accepted_saved_at: 0,
                mls_persisted_at: 0,
                ack_due: std::collections::HashMap::new(),
                last_own_ackable: 0,
                ordered_park: std::collections::HashMap::new(),
                unreachable: std::collections::HashSet::new(),
                link_down: std::collections::BTreeMap::new(),
                send_stuck: std::collections::BTreeMap::new(),
                last_mesh_out: 0,
            },
            recovery: RecoveryState {
                inboxes: Vec::new(),
                seat_inbox: None,
                reattach_attempts: 0,
                last_reattach: None,
                unsolicited_cooldown: std::collections::HashMap::new(),
                pending: HashMap::new(),
                tickets: HashMap::new(),
                mesh_window: std::collections::HashSet::new(),
                mesh_extension_at: std::collections::HashMap::new(),
                material_sink: None,
                task: None,
                generation: 0,
                ctx: None,
                transport: std::sync::Arc::new(std::sync::Mutex::new(None)),
            },
            chain: ChainProjection {
                blocks: Vec::new(),
                head: None,
                walk: None,
                pending_served_blob: None,
                checkpoint_blob: None,
                wiki_base: None,
                applied: HashMap::new(),
                applied_sigs: HashMap::new(),
                anchors: HashMap::new(),
                member_relays: HashMap::new(),
                split_noted: std::collections::HashSet::new(),
                pending_sigs: HashMap::new(),
                own_approvals: std::collections::BTreeSet::new(),
                served_at: HashMap::new(),
                pending_declines: HashMap::new(),
                pending_withdrawals: HashMap::new(),
                proposal_changes: HashMap::new(),
                pending_blocks: std::collections::BTreeMap::new(),
                catchup_from: None,
            },
            presence: PresenceState {
                poke_at: std::collections::HashMap::new(),
                wake_at: None,
                clock_override: None,
            },
            // the STORED decision is what a fresh process starts from
            // (ADR-0004 amendment): an operator who acknowledged clearnet
            // exposure is not asked again on every restart
            clearnet_session: session.settings.clearnet_relays_enabled,
            pending_relay_confirms: std::collections::HashSet::new(),
            s3_list_gen: 0,
            tor_test_gen: 0,
            backup_inflight: std::collections::HashSet::new(),
            backup_last_done: std::collections::HashMap::new(),
            backup_listing: None,
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
            join_confirm: None,
            join_backup: None,
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
    /// [`now_secs`] clock, unless a test pinned [`PresenceState::clock_override`].
    /// Every presence stamp, aging pass and activity-trio read runs on
    /// THIS accessor so tests can age pills deterministically.
    pub(crate) fn presence_now(&self) -> u64 {
        self.presence.clock_override.unwrap_or_else(now_secs)
    }

    /// The 0/1/2 presence pill for one member of the open workspace - the
    /// single derivation every surface shares ([`net::pill_state`]; the
    /// presence tick writes the very same answer onto the pills).
    pub(crate) fn presence_of(&self, member: &str, last_seen: u64, now: u64) -> u8 {
        net::pill_state(
            &self.member(),
            &self.delivery.unreachable,
            self.nostr.is_some(),
            member,
            last_seen,
            now,
        )
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
            Command::ShareFileFromExchange { name, channel } => {
                self.cmd_share_file_from_exchange(name, channel)
            }
            Command::DownloadFile { id, dest } => self.cmd_download_file(id, dest),
            Command::RemoveFile { id } => self.cmd_remove_file(id),
            Command::MarkChannelRead { channel, up_to } => {
                self.cmd_mark_channel_read(channel, up_to)
            }
            Command::ClearNotice => {
                self.session.notice = String::new();
                self.emit_session(SessionScope::Full);
                Ok(Reply::Ack)
            }
            Command::Poke { member } => self.cmd_poke(member),
            Command::NetPoked {
                from,
                to,
                generation,
            } => {
                // the MESH generation, like every other supervisor-sourced
                // command (the sink carries `net_generation`, not the
                // workspace scope) — a torn-down mesh's late nudge is dropped
                if !self.net_generation_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.receive_poke(&from, &to);
                Ok(Reply::Ack)
            }
            // the FETCH's own scope, like every other off-actor file task
            Command::NetWikiBaseFetched { bytes, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_wiki_base_fetched(bytes)
            }
            Command::NetWikiBaseFailed { reason, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_wiki_base_failed(&reason)
            }
            Command::NetPieceWanted { from, id, ranges, generation } => {
                if !self.net_generation_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_piece_wanted(&from, id, ranges)
            }
            Command::NetPieceWantSend { id, ranges, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_piece_want_send(id, ranges)
            }
            Command::ReadMirror => Ok(Reply::Mirror(Box::new(self.mirror_view()))),
            Command::SetMirror { on, quota_bytes } => self.cmd_set_mirror(on, quota_bytes),
            Command::SetMirrorDir { path } => self.cmd_set_mirror_dir(path),
            Command::NetMirrorDecl { from, on, quota, rev, generation } => {
                if !self.net_generation_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_mirror_decl(&from, on, quota, rev)
            }
            Command::NetMirrorStatus { from, holds, gen, page, pages, generation } => {
                if !self.net_generation_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_mirror_status(&from, holds, gen, page, pages)
            }
            Command::NetMirrorWho { from, generation } => {
                if !self.net_generation_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_mirror_who(&from)
            }
            Command::NetMirrorProgress { id, held, bytes, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_mirror_progress(id, held, bytes)
            }
            Command::NetMirrorDone { id, ok, reason, bytes, generation } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_mirror_done(id, ok, reason, bytes)
            }
            Command::SetWakeCommand { command } => self.cmd_set_wake_command(command),
            Command::SetNodePosture { posture } => self.cmd_set_node_posture(posture),
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
                key_b64,
                pieces,
                root,
            } => {
                if !self.net_scope_current(generation) {
                    return Ok(Reply::Ack);
                }
                self.cmd_net_file_shared(
                    name,
                    size,
                    kind,
                    modified,
                    checksum,
                    path,
                    channel,
                    (key_b64, pieces, root),
                )
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
                // relay plane: a failed fetch invalidates the cached stamp
                // (the series may be pruned or sealed under a foreign
                // epoch) — the NEXT attempt asks fresh via FileWanted and
                // the sharer's repeated-want rule re-publishes (review
                // 2026-08-10: the stale stamp made every retry replay the
                // same dead window)
                self.files.series.remove(&id);
                self.files.pending.remove(&id);
                self.set_download_phase(id, molt_core::TransferPhase::Failed { reason });
                Ok(Reply::Ack)
            }
            Command::NetFileSeriesPublished { id, at, generation } => {
                self.cmd_net_file_series_published(id, at, generation)
            }
            Command::NetFileWantedTimeout { id, generation } => {
                self.cmd_net_file_wanted_timeout(id, generation)
            }

            // proposals.rs
            Command::Propose { surface, payload } => self.cmd_propose(surface, payload),
            Command::Approve { proposal } => self.cmd_approve(proposal),
            Command::Decline { proposal } => self.cmd_decline(proposal),
            Command::Withdraw { proposal } => self.cmd_withdraw(proposal),
            Command::ReadState { surface, channel, view } => {
                // the view key is shared vocabulary (`Surface::views`, the
                // same list `select_view` validates against) PLUS chat's
                // read-only slices (`CHAT_READ_SLICES` — a read axis, not a
                // nav view); an unknown key must error, never silently read
                // the wrong window
                if let Some(v) = &view {
                    let nav = surface.views().iter().any(|(k, _)| k == v);
                    let slice = surface == Surface::Chat
                        && molt_core::CHAT_READ_SLICES.contains(&v.as_str());
                    if !nav && !slice {
                        return Err(MoltError::UnknownView(surface, v.clone()));
                    }
                }
                // the fold is the Memory read's whole cost: warm the cache
                // here, where the borrow is mutable (§4.1)
                if surface == Surface::Memory {
                    self.refresh_wiki_cache();
                }
                let snap = self.snapshot(surface, channel, view.as_deref());
                // retrieval IS the reading: the chat messages just handed
                // out get the same honest receipts the GUI sends when it
                // renders them (State::receipt_returned_chat)
                self.receipt_returned_chat(&snap);
                Ok(Reply::State(snap))
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
            Command::UiPublish { snapshot } => {
                self.ui_state = Some(snapshot);
                Ok(Reply::Ack)
            }
            Command::ReadUiState => Ok(Reply::UiState { snapshot: self.ui_state.clone() }),
            Command::UiAction { action } => self.cmd_ui_action(action),
            Command::ReadChain => self.cmd_read_chain(),
            Command::WikiList {
                prefix,
                cursor,
                limit,
            } => self.cmd_wiki_list(prefix, cursor, limit),
            Command::WikiGet { path } => self.cmd_wiki_get(path),
            Command::NetWikiIndexReady { epoch } => self.cmd_net_wiki_index_ready(epoch),
            Command::WikiChanges {
                since_rev,
                limit,
                cursor,
            } => self.cmd_wiki_changes(since_rev, limit, cursor),
            Command::WikiHealth { limit } => self.cmd_wiki_health(limit),
            Command::WikiProps => self.cmd_wiki_props(),
            Command::WikiLinks {
                path,
                direction,
                predicate,
                limit,
                cursor,
            } => self.cmd_wiki_links(path, direction, predicate, limit, cursor),
            Command::WikiNeighbors {
                path,
                depth,
                limit,
                predicate,
                direction,
                transitive,
            } => self.cmd_wiki_neighbors(path, depth, limit, predicate, direction, transitive),
            Command::WikiSearch {
                query,
                tags,
                kind,
                folder,
                props,
                limit,
                cursor,
            } => self.cmd_wiki_search(query, tags, kind, folder, props, limit, cursor),

            // net/ (engine-internal, sent by the node's own supervisor)
            Command::NetDelivered {
                from,
                envelope,
                generation,
            } => self.cmd_net_delivered(from, envelope, generation),
            Command::NetPeerSeen { member, generation } => {
                self.cmd_net_peer_seen(member, generation)
            }
            Command::NetPeerRekeyed { member, generation } => {
                self.cmd_net_peer_rekeyed(member, generation)
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
            Command::RelayProbe { url } => self.cmd_relay_probe(url),
            Command::NetRelayProbed { url, error, unreachable, confirm } => {
                self.cmd_net_relay_probed(url, error, unreachable, confirm)
            }
            Command::RelayClearnetSession { unlock } => self.cmd_relay_clearnet_session(unlock),
            Command::Navigate { screen } => self.cmd_navigate(screen),
            Command::SelectSurface { surface } => self.cmd_select_surface(surface),
            Command::SelectView { surface, view } => self.cmd_select_view(surface, view),
            Command::SetLanguage { lang } => self.cmd_set_language(lang),
            Command::SetTheme { theme } => self.cmd_set_theme(theme),
            Command::SetFonts { app, nav, editor } => self.cmd_set_fonts(app, nav, editor),
            Command::SetReadReceipts { enabled } => self.cmd_set_read_receipts(enabled),
            Command::SaveSettings { settings } => self.cmd_save_settings(settings),
            Command::PatchSettings { patch } => self.cmd_patch_settings(patch),
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
                self.cmd_export_workspace(id, dest, passphrase, false)
            }
            Command::NetExportDone { id, dest, bytes, skipped } => {
                self.cmd_net_export_done(id, dest, bytes, skipped)
            }
            Command::NetExportFailed { id, error } => self.cmd_net_export_failed(id, error),
            Command::WikiExport { dest, proof } => self.cmd_wiki_export(dest, proof),
            Command::WikiExportArchive { name, proof } => self.cmd_wiki_export_archive(name, proof),
            Command::ExportWorkspaceArchive { id, name, passphrase } => {
                self.cmd_export_workspace_archive(id, name, passphrase)
            }
            Command::NetWikiExportDone { dest, files, bytes } => {
                self.cmd_net_wiki_export_done(dest, files, bytes)
            }
            Command::NetWikiExportFailed { error } => self.cmd_net_wiki_export_failed(error),

            // backup.rs (story 12: the auto-backup ticker + manual trigger)
            Command::BackupNow { id } => self.cmd_backup_now(id),
            Command::BackupFetch { id } => self.cmd_backup_fetch(id),
            Command::NetBackupFetched { id, error } => self.cmd_net_backup_fetched(id, error),
            Command::BackupTick => self.cmd_backup_tick(),
            Command::NetBackupDone {
                id,
                ts,
                object,
                bytes,
                prune_error,
                quota_error,
            } => self.cmd_net_backup_done(id, ts, object, bytes, prune_error, quota_error),
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
                relays,
            } => self.cmd_create_start(name, member, threshold, members, relays),
            Command::CreatePropose {
                name,
                agenda,
                features,
            } => self.cmd_create_propose(name, agenda, features),
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
                relays,
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
                relays,
                generation,
            ),
            Command::NetSealSigned {
                seat,
                sig,
                from,
                generation,
            } => self.cmd_net_seal_signed(seat, sig, from, generation),
            Command::ConfirmSeedBackup { phrase } => self.cmd_confirm_seed_backup(&phrase),
            Command::WikiDraftSave { draft } => self.cmd_wiki_draft_save(&draft),
            Command::WikiDraftLoad => self.cmd_wiki_draft_load(),
            Command::NetBackupConfirmed {
                seat,
                sig,
                from,
                generation,
            } => self.cmd_net_backup_confirmed(seat, sig, from, generation),
            Command::RecoverInviteStart { member } => self.cmd_recover_invite_start(member),
            Command::RecoverStart { link, phrase } => self.cmd_recover_start(link, phrase),
            Command::NetRecoverSealed {
                member,
                chain,
                mls,
                mesh,
                nostr_sk,
                rotation_seed,
                generation,
            } => self.cmd_net_recover_sealed(
                member,
                chain,
                mls,
                mesh,
                nostr_sk,
                rotation_seed,
                generation,
            ),
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
                new_nostr_pk,
                relays,
                consent,
                key_package,
                ticket,
                seat_proof,
                reply,
                sender_npub,
                generation,
            } => self.cmd_net_recover_requested(
                member,
                identity_pk,
                key_package,
                ticket,
                seat_proof,
                new_nostr_pk,
                relays,
                consent,
                reply,
                sender_npub,
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
                target,
                endpoint,
                access_key,
                secret_key,
                bucket,
            } => self.cmd_net_test_s3(target, endpoint, access_key, secret_key, bucket),
            Command::NetTestS3Result { target, result } => {
                self.cmd_net_test_s3_result(target, result)
            }
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
            Command::JoinFinish => self.cmd_join_finish(),
            Command::NetJoinDeclined { seat, from, generation } => {
                self.cmd_net_join_declined(seat, from, generation)
            }
            Command::NetJoinAccepted { generation } => self.cmd_net_join_accepted(generation),
            Command::NetJoinCharterProposed {
                name,
                agenda,
                features,
                generation,
            } => self.cmd_net_join_charter_proposed(name, agenda, features, generation),
            Command::JoinCancel => self.cmd_join_cancel(),
            Command::NetRitualNote { note, generation } => {
                self.cmd_net_ritual_note(note, generation)
            }
            Command::NetJoinNote { note, generation } => self.cmd_net_join_note(note, generation),
            Command::NetRecoverNote { note, generation } => {
                self.cmd_net_recover_note(note, generation)
            }
            Command::NetRecoverProgress { member, need, roster, approved, generation } => {
                self.cmd_net_recover_progress(member, need, roster, approved, generation)
            }
            Command::NetRitualPublished {
                what,
                accepted,
                failed,
                generation,
                workspace,
            } => self.cmd_net_ritual_published(&what, &accepted, &failed, generation, &workspace),
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
mod tests;
