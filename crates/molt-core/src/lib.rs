// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-core`: the shared vocabulary of MoltRepublic.
//!
//! This crate defines the **one command set** that the whole system is built
//! around. Every operator of the software — a human through the GUI, or an
//! agent through the MCP interface — expresses what it wants to do as a
//! [`Command`], and observes what happened as an [`Event`]. The two frontends
//! are therefore co-equal by construction: they share this contract and neither
//! can do anything the other cannot.
//!
//! There is no I/O here. The actor that *executes* commands lives in
//! `molt-engine`; the frontends that *issue* them live in `molt-mcp` and
//! `molt-ui`. See `documents` in `../../moltrepublic-docs` for the design
//! (the generalized approval engine, R0, and the surfaces).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The republic's persistent-change chain: a single-branch, threshold-signed
/// sequence of commit blocks (the founding is block 0). See [`chain`].
pub mod chain;
pub mod relay;
pub mod wiki_fold;
pub use chain::{
    applied_lww_slot, approval_bytes, block_link_bytes, checkpoint_canonical_bytes, ChainBlock,
    ChainChange, CheckpointState, MembershipOp, GENESIS_PREV,
};

/// A member of the republic (the holder of one threshold share). In this
/// scaffold it is just a display handle; the real per-group MLS identity is a
/// future `molt-identity` concern.
pub type MemberId = String;

/// The sole address of a workspace across the command set: 32 bytes, lowercase
/// hex (64 chars), derived from the recovery seed and the member identity
/// (`HKDF(seed, "molt-ws-id", member)` — see `molt-storage`). Display names
/// are presentation only and may repeat; the id never does.
pub type WorkspaceId = String;

/// The per-file cap every pre-mirroring config wrote unconditionally. A
/// config carrying exactly it reads as "no cap" (the file is never
/// rewritten for it), so a deliberate cap near 4 MiB uses another value.
pub const LEGACY_FILE_CAP_BYTES: u64 = 4_194_304;

/// The shared surfaces. [`Surface::Chat`] is ungated (a message changes no
/// shared state); every other surface — Organization included — changes the
/// shared state only through a threshold-approved proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// Who this republic is: status (with the ratified charter) and roster.
    /// Changing it (charter, name, image, chat retention) is gated like any
    /// other state change.
    Organization,
    /// Free conversation. Ungated: a message changes no shared state.
    Chat,
    /// The shared brain: versioned, cross-linked notes. Gated.
    Memory,
    /// The quest board: tasks put forward, taken and completed. Gated.
    Quests,
    /// Sealed secrets, disclosed by threshold vote to one elected reader.
    /// Gated.
    Vault,
    /// Shared funds (Monero multisig in production). Gated.
    Wallet,
    /// The files shared into the chat: the temporary uploads table (chat
    /// messages) and the persistent one (a vote pinned the share — its
    /// identity lives in the applied block, so it outlives the message).
    /// Core (never a feature key), gated: the persist/unpersist ops. Added
    /// after checkpoint-v7 — never in [`Surface::CHECKPOINT_V7_SURFACES`].
    Files,
}

impl Surface {
    /// Every surface, in display (= navigation) order. May grow: a surface
    /// added here reaches a checkpoint with its first applied entry (v8),
    /// never through [`Surface::CHECKPOINT_V7_SURFACES`].
    pub const ALL: [Surface; 7] = [
        Surface::Organization,
        Surface::Chat,
        Surface::Memory,
        Surface::Quests,
        Surface::Vault,
        Surface::Wallet,
        Surface::Files,
    ];

    /// The surfaces whose applied group EVERY checkpoint carries, empty or
    /// not — the set `molt-chain-checkpoint-v7` froze. FROZEN: adding to
    /// it re-hashes every cut checkpoint.
    pub const CHECKPOINT_V7_SURFACES: [Surface; 6] = [
        Surface::Organization,
        Surface::Chat,
        Surface::Memory,
        Surface::Quests,
        Surface::Vault,
        Surface::Wallet,
    ];

    /// Lowercase wire/display name.
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Organization => "organization",
            Surface::Chat => "chat",
            Surface::Memory => "memory",
            Surface::Quests => "quests",
            Surface::Vault => "vault",
            Surface::Wallet => "wallet",
            Surface::Files => "files",
        }
    }

    /// Whether changes to this surface require a threshold of approvals.
    /// Chat is the only ungated surface — a message changes no shared state.
    pub fn is_gated(self) -> bool {
        !matches!(self, Surface::Chat)
    }

    /// Parse a surface from its lowercase name.
    pub fn parse(s: &str) -> Option<Surface> {
        Surface::ALL.into_iter().find(|x| x.as_str() == s)
    }

    /// Whether this surface is an optional **charter feature**
    /// (`docs_archive/ritual/charter_features.md` D1): activated at founding or by a
    /// later threshold vote, never deactivated. Organization, Chat and Files
    /// are core — always on, never in a feature set.
    pub fn is_charter_feature(self) -> bool {
        !matches!(self, Surface::Organization | Surface::Chat | Surface::Files)
    }

    /// The D6 legacy baseline: what a republic founded before roster-v5
    /// (`features: None`) has enabled — exactly what was usable before the
    /// gating existed (user-decided 2026-08-11). ONE definition; the engine
    /// fold and the GUI's pre-v5 rendering both read it.
    pub const LEGACY_FEATURES: [&'static str; 1] = ["memory"];

    /// Validate one proposed feature key: a core surface gets its own
    /// diagnosis (it is not "unknown", it is not optional), anything the
    /// vocabulary does not name is refused.
    fn check_feature_key(key: &str) -> Result<(), String> {
        match Surface::parse(key) {
            Some(sf) if sf.is_charter_feature() => Ok(()),
            Some(_) => Err(format!("{key} is always on")),
            None => Err(format!("unknown feature: {key}")),
        }
    }

    /// The surface's sub-views as `(key, display label)` pairs, in navigation
    /// order. The first entry is the default view. Shared vocabulary: the GUI
    /// nav and the `select_view` command validate against this same list.
    pub fn views(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Surface::Organization => &[
                ("status", "Status"),
                ("members", "Members"),
                // in-voting organization changes (charter / name / image /
                // retention); the GUI shows this entry only while non-empty
                ("pending", "Pending"),
                // the applied organization changes (the surface's applied
                // log) — every accepted vote's row, with the 💬 back-link
                // into its discussion; hidden while empty, like pending
                ("accepted", "Accepted"),
                // declined organization changes, within the display-retention
                // window; the GUI shows this entry only while non-empty too
                ("declined", "Declined"),
            ],
            // "today" is the general view: everything within the chat
            // retention window (the key predates the renames and stays —
            // it is select_view's wire vocabulary)
            // ONE view: the group chat, as far back as the retention window
            // reaches. It used to be split in half by age ("today" = the
            // younger half, "archive" = the older) — an invisible cliff that
            // read as "my chat disappeared" the moment a conversation was
            // older than half a window (default: 3.5 days). The key stays
            // `today`: it is select_view's wire vocabulary.
            Surface::Chat => &[("today", "General")],
            Surface::Memory => &[
                ("brain", "Multisig-Wiki"),
                ("proposals", "Proposals"),
                ("accepted", "Accepted"),
                ("denied", "Denied"),
                // "archive" was a design mock and is REMOVED until a real
                // per-doc history design exists (shared_memory_real.md
                // WP-E) — the Accepted table is the decision history
            ],
            Surface::Quests => &[
                ("board", "Board"),
                // sprint scheduling (kanban_workflows.md §6.3)
                ("plan", "Planning"),
                ("create", "Create"),
                ("proposals", "Proposals"),
                // the wire key predates the Kanban rework and stays — it is
                // select_view's vocabulary; only the label moved to "Mine"
                ("my-quests", "Mine"),
                ("archive", "Archive"),
            ],
            Surface::Vault => &[
                ("secrets", "Secrets"),
                // access requests: a threshold vote elects ONE reader; the
                // committed grant re-seals the key shares to that member
                // alone (docs/vault/vault_threshold_disclosure.md)
                ("requests", "Requests"),
                ("proposals", "Proposals"),
                ("unsealed", "Unsealed"),
            ],
            Surface::Wallet => &[
                ("balance", "Balance"),
                ("history", "History"),
                ("send", "Send"),
                ("receive", "Receive"),
                ("status", "Status"),
                ("settings", "Settings"),
            ],
            // the GUI shows the surface only while a share is listed (or
            // it is on screen); the three outcome views hide while empty,
            // exactly like Organization's
            Surface::Files => &[
                ("uploads", "Temporary Uploads"),
                ("persistent", "Persistent Uploads"),
                ("pending", "Pending"),
                ("accepted", "Accepted"),
                ("declined", "Declined"),
            ],
        }
    }

    /// The view a surface opens on (the first of [`Surface::views`], or `""`
    /// for a surface with no sub-views — e.g. Constitution).
    pub fn default_view(self) -> &'static str {
        self.views().first().map(|v| v.0).unwrap_or("")
    }
}

/// Canonicalize a PROPOSED feature selection (`charter_features.md`): every
/// key must name an optional surface ([`Surface::is_charter_feature`] — a
/// core key gets its own diagnosis), the result is sorted + deduped. One
/// set, one byte form — the ONE rule the wizard command and the
/// `set_features` op share.
pub fn canonical_features(keys: &[String]) -> Result<Vec<String>, String> {
    for key in keys {
        Surface::check_feature_key(key)?;
    }
    let mut out = keys.to_vec();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Verify a RECEIVED feature selection is canonical — known optional keys,
/// strictly sorted, no duplicates — without re-sorting it: the bytes are
/// (or become) threshold-signed, so a second encoding of the same set must
/// be refused, never repaired. Sign-what-you-see closes here too: a member
/// must not ratify a key its build cannot render.
pub fn verify_canonical_features(keys: &[String]) -> Result<(), String> {
    for key in keys {
        Surface::check_feature_key(key)?;
    }
    if !keys.windows(2).all(|w| w[0] < w[1]) {
        return Err("the feature set is not canonical".to_string());
    }
    Ok(())
}

/// Chat READ slices for `ReadState { view }` — a different axis from the
/// NAV views ([`Surface::views`]), and deliberately so.
///
/// `unread` is agent-facing: exactly the messages after this seat's read
/// cursor (B2). It was briefly a nav row too, and that broke the chat for a
/// human — the GUI marks the on-screen channel read on every refresh, so the
/// pane emptied itself the moment it was looked at, and the compose row
/// (gated on the general view) stayed collapsed. A slice that is empty by
/// construction is something an agent ASKS for, never somewhere a person
/// navigates to.
pub const CHAT_READ_SLICES: &[&str] = &["unread"];

/// The top-level screen the GUI is showing. This is **shared session state**:
/// it lives in the engine, not the GUI, so an MCP agent can navigate the node
/// and the GUI live-mirrors the move (and vice versa) — the same co-equal rule
/// the surfaces follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Screen {
    /// First-run choice: create / open / join / restore.
    #[default]
    Choice,
    /// Create-workspace wizard.
    Create,
    /// Open a local workspace.
    Open,
    /// Join via invite.
    Join,
    /// Restore from backup / seed.
    Restore,
    /// Settings (config editor).
    Settings,
    /// Main view (the surfaces live-mirror).
    Main,
}

impl Screen {
    /// Lowercase wire/display name.
    pub fn as_str(self) -> &'static str {
        match self {
            Screen::Choice => "choice",
            Screen::Create => "create",
            Screen::Open => "open",
            Screen::Join => "join",
            Screen::Restore => "restore",
            Screen::Settings => "settings",
            Screen::Main => "main",
        }
    }

    /// Parse a screen from its lowercase name.
    pub fn parse(s: &str) -> Option<Screen> {
        [
            Screen::Choice,
            Screen::Create,
            Screen::Open,
            Screen::Join,
            Screen::Restore,
            Screen::Settings,
            Screen::Main,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }
}

/// The node's editable settings, mirrored from `config.toml` at startup and
/// kept in sync with it in both directions: a save persists to the file
/// (format-preserving, atomic), an external file edit is watched, validated
/// and mirrored back into the session. The values live in the engine session
/// so the GUI and an MCP agent edit the *same* settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettings {
    /// Start without a GUI (MCP-only).
    pub headless: bool,
    /// Per-group workspace root.
    pub workspace_dir: String,
    /// Automatically back workspaces up to an S3-compatible store.
    pub s3_backup: bool,
    /// S3 endpoint / bucket URL the automatic backup targets.
    pub s3_endpoint: String,
    /// S3 access key id.
    #[serde(default)]
    pub s3_access_key: String,
    /// S3 secret key. NEVER SERIALIZED (2026-08-26): a credential no
    /// surface needs to read back — set blind through `patch_settings` or
    /// the GUI (`SetNodePosture`); `read_session` carries "".
    #[serde(skip_serializing, default)]
    pub s3_secret_key: String,
    /// S3 bucket name. Defaults to something inconspicuous — the bucket
    /// listing should not advertise what it holds.
    #[serde(default = "default_s3_bucket")]
    pub s3_bucket: String,
    /// Automatic-backup interval in minutes.
    #[serde(default = "default_s3_interval")]
    pub s3_interval_min: u16,
    /// How many automatic-backup copies to keep per workspace (older
    /// copies are pruned once a newer one lands).
    #[serde(default = "default_s3_keep_copies")]
    pub s3_keep_copies: u16,
    /// Byte quota for the backup bucket, counting only the backups THIS
    /// node wrote (foreign objects are neither counted nor deleted).
    /// `0` = no limit. Over the limit the oldest copies are pruned - never
    /// a workspace's newest one (`docs_archive/storage/s3_buckets.md` §4). Note the
    /// neighbouring `file_cap_bytes` reads 0 as "off"; here 0 is "no limit".
    #[serde(default)]
    pub s3_max_bytes: u64,
    /// The MEDIA bucket at the same endpoint and credentials as the backup
    /// bucket above - one S3 account, several buckets. No cover-name default
    /// like the backup bucket: empty means "not configured", not "some name
    /// nobody chose". Configuration only - nothing writes media to S3 yet
    /// (`docs_archive/storage/s3_buckets.md` §7), and the GUI says so.
    #[serde(default)]
    pub media_s3_bucket: String,
    /// Byte quota for the media bucket, `0` = no limit. Stored and shown;
    /// with no writer there is nothing to enforce it against yet.
    #[serde(default)]
    pub media_s3_max_bytes: u64,
    /// MCP server TCP port.
    pub mcp_port: u16,
    /// MCP client allowlist (`"127.0.0.1" | "0.0.0.0" | comma-separated`).
    pub mcp_allow: String,
    /// MCP API token clients must present. NEVER SERIALIZED (2026-08-26):
    /// the token is the whole TCP credential; a client that holds it needs
    /// no copy, and one that rotates it must not be the only party that
    /// learns the new one. The GUI reads it in-process; its one door is
    /// `SetNodePosture`.
    #[serde(skip_serializing, default)]
    pub mcp_token: String,
    /// Anonymity network: `"tor" | "none"`. (`"nym"` was retired — the
    /// mixnet was never implemented and the dialer refuses it.)
    pub anonymity: String,
    /// Tor mode: `"local" | "embedded" | "whonix"`.
    pub tor_mode: String,
    /// Local Tor SOCKS port.
    pub tor_port: u16,
    /// Where downloaded chat files land when no explicit destination is
    /// given (`~` expands).
    #[serde(default = "default_download_dir")]
    pub download_dir: String,
    /// Per-file byte cap for sharing: `None` = no cap (the mirroring
    /// decision, `docs_archive/files/mirroring.md` §1), `Some(0)` = file sharing
    /// OFF (FP4, 2026-08-16), `Some(n)` = a deliberate cap - never
    /// [`LEGACY_FILE_CAP_BYTES`], which reads as no cap. Serialized as
    /// `null` when absent: the settings patch door keys on the field.
    #[serde(default)]
    pub file_cap_bytes: Option<u64>,
    /// Seconds between two piece publishes of the trickle sender
    /// (`docs_archive/files/mirroring.md` §3.2); node-local, at least 1.
    #[serde(default = "default_mirror_publish_interval_secs")]
    pub mirror_publish_interval_secs: u64,
    /// Piece bytes the trickle sender may publish per UTC day; node-local.
    #[serde(default = "default_mirror_daily_bytes")]
    pub mirror_daily_bytes: u64,
    /// Alert sound for an incoming chat message:
    /// `"none" | "bell" | "chime" | "pop"`.
    #[serde(default = "default_sound")]
    pub sound_message: String,
    /// Alert sound for a new incoming vote (proposal), same vocabulary.
    #[serde(default = "default_sound")]
    pub sound_vote: String,
    /// Alert sound for an incoming poke, same vocabulary.
    #[serde(default = "default_sound")]
    pub sound_poke: String,
    /// Whether this node reacts to pokes at all (toast, sound, and the wake
    /// hook when poked) and offers poking in its own UI. **Off by default —
    /// an explicit opt-in.** It does NOT gate the pending-vote wake, which
    /// is its own opt-in (a non-empty [`Self::poke_wake_command`]): a
    /// headless seat may want to be woken for votes without taking part in
    /// poking at all.
    #[serde(default)]
    pub poke_enabled: bool,
    /// Command this node runs (via `sh -c`) when its seat is poked or new
    /// work awaits its vote — the wake hook for a sleeping agent harness.
    /// Empty = off, one wake runs at a time. Wire content never reaches the
    /// command line; context arrives as `MOLT_WAKE_*` env vars only.
    ///
    /// Settable ONLY through [`Command::SetWakeCommand`] (the GUI) or
    /// `config.toml`: the wholesale settings paths refuse it, because a
    /// surface that could plant a shell command would turn republic access
    /// into code execution on the operator's machine.
    #[serde(default)]
    pub poke_wake_command: String,
    /// Send (and show) per-message chat read receipts. A local per-node
    /// privacy switch, on by default; while off this node broadcasts no
    /// receipts and hides others' from its chat view (symmetric).
    #[serde(default = "default_true")]
    pub read_receipts: bool,
    /// App-chrome font size in px.
    #[serde(default = "default_font_app")]
    pub font_app: u16,
    /// Wiki-navigator font size in px.
    #[serde(default = "default_font_nav")]
    pub font_nav: u16,
    /// Editor/document font size in px.
    #[serde(default = "default_font_editor")]
    pub font_editor: u16,
    /// The Nostr relay pool, in priority order (position 0 is tried first).
    /// **Empty by default — nothing is pre-trusted.** Edited through the
    /// `Relay*` commands (never a free-form text field), because every entry
    /// is URL-validated and a clearnet confirmation is gated;
    /// `docs_archive/transport/relay_pool.md`.
    #[serde(default)]
    pub relays: Vec<crate::relay::RelayEntry>,
    /// Whether this node may dial relays that are NOT onion services
    /// (clearnet, LAN, loopback — everything reached outside Tor).
    ///
    /// **Off by default: a fresh install dials no such relay.** It is turned
    /// on by acknowledging a clearnet/local relay's exposure when confirming
    /// it (`RelayConfirm { accept_clearnet: true }`), and off again by
    /// `RelayClearnetSession { unlock: false }` — and BOTH decisions are
    /// persisted (ADR-0004 amendment, 2026-07-31). It used to be a
    /// session-only flag that reset on every start; that made the informed
    /// consent unrepeatable-in-practice rather than strong, since the
    /// operator had to re-perform it after every restart and every config
    /// edit. The consent moment is unchanged — it is simply remembered now.
    #[serde(default)]
    pub clearnet_relays_enabled: bool,
}

/// Default alert sound: silent until the operator opts in.
/// The trickle sender's default pace: one piece per 15 s.
fn default_mirror_publish_interval_secs() -> u64 {
    15
}

/// The trickle sender's default daily budget: 512 MiB.
fn default_mirror_daily_bytes() -> u64 {
    512 * 1024 * 1024
}

fn default_sound() -> String {
    "none".to_string()
}

/// Default for an opt-out boolean preference (on unless the operator
/// disables it, and present-by-absence in an older `config.toml`).
fn default_font_app() -> u16 {
    14
}

fn default_font_nav() -> u16 {
    13
}

fn default_font_editor() -> u16 {
    14
}

fn default_true() -> bool {
    true
}

/// The display label for an anonymity-network setting — what
/// [`WorkspaceInfo::net`] / [`CreateState::net`] and the create screen's
/// read-only "Network" line show. The SINGLE normalization every surface
/// shares (engine, GUI, startup scan): only `"tor"` reads as `"tor"`;
/// `"none"`, the legacy `"nym"`, and any unknown value read as `"none"` —
/// they never dial (an unknown network fails the dialer resolution closed),
/// so the honest reading is "no anonymity network configured".
pub fn effective_net_label(anonymity: &str) -> &'static str {
    match anonymity {
        "tor" => "tor",
        _ => "none",
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        SessionSettings {
            headless: false,
            workspace_dir: "~/.moltrepublic/workspaces".to_string(),
            s3_backup: false,
            s3_endpoint: String::new(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_bucket: default_s3_bucket(),
            s3_interval_min: default_s3_interval(),
            s3_keep_copies: default_s3_keep_copies(),
            s3_max_bytes: 0,
            media_s3_bucket: String::new(),
            media_s3_max_bytes: 0,
            mcp_port: 4040,
            mcp_allow: "127.0.0.1".to_string(),
            mcp_token: String::new(),
            anonymity: "none".to_string(),
            tor_mode: "local".to_string(),
            tor_port: 9050,
            download_dir: default_download_dir(),
            file_cap_bytes: None,
            mirror_publish_interval_secs: default_mirror_publish_interval_secs(),
            mirror_daily_bytes: default_mirror_daily_bytes(),
            sound_message: default_sound(),
            sound_vote: default_sound(),
            sound_poke: default_sound(),
            poke_enabled: false,
            poke_wake_command: String::new(),
            read_receipts: true,
            font_app: default_font_app(),
            font_nav: default_font_nav(),
            font_editor: default_font_editor(),
            // no relay ships with the app: a fresh install connects nowhere
            relays: Vec::new(),
            clearnet_relays_enabled: false,
        }
    }
}

/// Which of the node's two S3 buckets a command addresses
/// (`docs_archive/storage/s3_buckets.md`). They share one endpoint and one set of
/// credentials; the bucket name and the byte quota are per target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum S3Target {
    /// The workspace-backup bucket. The default, so an older caller that
    /// names no target keeps addressing exactly what it used to.
    #[default]
    Workspaces,
    /// The media bucket - configured, no consumer yet.
    Media,
}

impl S3Target {
    /// The wire/word form (`"workspaces"` | `"media"`).
    pub fn as_str(self) -> &'static str {
        match self {
            S3Target::Workspaces => "workspaces",
            S3Target::Media => "media",
        }
    }
}

/// The inconspicuous default S3 bucket name.
fn default_s3_bucket() -> String {
    "media-archive".to_string()
}

/// The default download destination.
fn default_download_dir() -> String {
    "~/Downloads".to_string()
}

/// Default automatic-backup interval (minutes).
fn default_s3_interval() -> u16 {
    60
}

/// Default number of automatic-backup copies kept per workspace.
fn default_s3_keep_copies() -> u16 {
    5
}

/// One member of a workspace with its REAL last-seen info. Prose ("2 min
/// ago") is presentation and is rendered client-side from the stamp — the
/// `last_sync_min` pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Display handle.
    pub name: String,
    /// Unix seconds this member was last actually observed: authenticated
    /// inbound traffic, or completing a ritual with us - the founding is
    /// one, which is why a freshly founded republic never reads back as
    /// never-seen. Sightings are remembered locally in
    /// [`WorkspacePrefs::last_seen`], so a restart keeps them.
    /// [`MemberInfo::NEVER`] = no local evidence at all. Additive with a
    /// default: older data honestly reads back as never-seen.
    #[serde(default)]
    pub last_seen: u64,
    /// 0 = online, 1 = stale, 2 = offline/unreachable — aged from
    /// `last_seen` by [`presence_state`] (the engine's presence ticker
    /// keeps the pushed pills current; a send-failure pins 2 until the
    /// next sighting).
    pub state: u8,
}

impl MemberInfo {
    /// The `last_seen` sentinel for "never seen by this install" (unix 0
    /// predates every republic, and `#[serde(default)]` makes it what
    /// stamp-less legacy data reads back as).
    pub const NEVER: u64 = 0;
    /// Seen within this many seconds → online (state 0). 5 minutes: the
    /// mesh has no beacons, so presence is only as fresh as real traffic.
    pub const ONLINE_SECS: u64 = 300;
    /// Seen within this many seconds → stale (state 1); older (or never)
    /// → offline (state 2). 30 minutes.
    pub const STALE_SECS: u64 = 1800;
    /// How long a relay-transport sighting keeps a silent member merely
    /// STALE instead of dark (engine §6.5: presence over relays is
    /// traffic-derived and coarse, so short silence is not absence). One
    /// week - past it silence IS absence, and the dot says so; without the
    /// bound the founding stamp alone would paint an absent seat yellow
    /// forever.
    pub const COARSE_SECS: u64 = 7 * 86_400;
}

/// Age a member's REAL last-seen stamp into the 0/1/2 pill state — the one
/// classification every surface uses (thresholds on [`MemberInfo`]). Pure:
/// core owns no clock, callers pass `now`.
pub fn presence_state(now: u64, last_seen: u64) -> u8 {
    if last_seen == MemberInfo::NEVER {
        return 2;
    }
    let age = now.saturating_sub(last_seen);
    if age <= MemberInfo::ONLINE_SECS {
        0
    } else if age <= MemberInfo::STALE_SECS {
        1
    } else {
        2
    }
}

/// Project a member roster into the session's [`MemberInfo`] shape from
/// each member's REAL last-seen stamp ([`MemberInfo::NEVER`] = never
/// seen). The one projection every flow uses; `now` comes from the caller
/// (core owns no clock).
pub fn roster_members(
    roster: &[MemberId],
    now: u64,
    last_seen_of: impl Fn(&str) -> u64,
) -> Vec<MemberInfo> {
    roster
        .iter()
        .map(|m| {
            let last_seen = last_seen_of(m);
            MemberInfo {
                name: m.clone(),
                last_seen,
                state: presence_state(now, last_seen),
            }
        })
        .collect()
}

/// A locally known workspace/republic. It lives in the shared session so the
/// GUI's Open screen and an MCP agent see the *same* list; on a real node it
/// is built by scanning `workspace_dir/*/manifest.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// The workspace id — the sole address across the command set.
    #[serde(default)]
    pub id: WorkspaceId,
    /// Display name.
    pub name: String,
    /// Threshold summary, e.g. `"3-of-5"`.
    pub detail: String,
    /// Whether fully synced.
    pub synced: bool,
    /// 0 = synced, 1 = syncing, 2 = offline.
    pub state: u8,
    /// Minutes since the last completed sync (0 = just now / syncing);
    /// also the sort key of the workspace list. The human status line is
    /// RENDERED from `state` + this + `sync_queue` — prose is presentation.
    pub last_sync_min: u32,
    /// Items still waiting to sync (only meaningful while `state == 1`).
    pub sync_queue: u32,
    /// Automatic S3 backup configured.
    pub s3: bool,
    /// Real on-disk size of the workspace directory in KiB, rounded up
    /// (0 for a session-only entry with no directory). Measured at the
    /// quiescent entry choke points — boot scan, materialize, open,
    /// clean close — not continuously.
    #[serde(default)]
    pub size_kib: u32,
    /// Minutes since the last backup THIS node completed (stamped only on
    /// a confirmed upload — `NetBackupDone`); [`WorkspaceInfo::NEVER`]
    /// = never backed up. Prose is rendered UI-side.
    #[serde(default = "WorkspaceInfo::never")]
    pub last_backup_min: u32,
    /// Backup copies of this workspace the last REAL bucket listing saw
    /// (`Command::NetListBackups`); 0 until a listing ran, and reset to 0
    /// when a listing fails — the table never claims invented bucket
    /// contents. Additive.
    #[serde(default)]
    pub backup_copies: u32,
    /// The last backup attempt's failure, verbatim (empty = no failure
    /// since the last success/toggle). Includes the honest "sealed at
    /// rest" skip status of design P6. Additive.
    #[serde(default)]
    pub backup_error: String,
    /// The real recovery phrase every secret key of this workspace derives
    /// from — a secret. Populated from the device-sealed seed while the
    /// workspace is unsealed; cleared to "" the moment it is sealed at rest
    /// (no key material on disk means none in session memory either).
    ///
    /// NEVER SERIALIZED: an in-process display value for the GUI's
    /// hold-to-peek only. It rode `read_session` to every MCP client until
    /// 2026-08-26 (review K4); a phrase is private and leaves the process on
    /// no surface. Reads back as "" from any wire form.
    #[serde(skip_serializing, default)]
    pub seed: String,
    /// The effective global anonymity network (`"tor" | "none"`) when this
    /// entry was founded/joined — a display label (routing always follows
    /// the LIVE global settings); demo entries may carry legacy values.
    pub net: String,
    /// Sealed at rest: an encrypted workspace has had its device-stored key
    /// material removed, so it is inactive — opening it again requires a
    /// decrypt with the recovery phrase.
    #[serde(default)]
    pub encrypted: bool,
    /// A fetched-but-unopened backup (S7 — [`SEALED_RESTORED`]): sealed like
    /// `encrypted`, but the label and the decrypt path differ (opening runs
    /// the verified restore pipeline). Additive.
    #[serde(default)]
    pub restored: bool,
    /// Members and when each of them last synced.
    pub members: Vec<MemberInfo>,
    /// The ratified founding charter (free-text agenda) from the genesis —
    /// populated for the open workspace so the Constitution surface can show it.
    /// Empty on unopened entries and pre-deliberation workspaces.
    #[serde(default)]
    pub agenda: String,
}

/// The human half of a workspace directory name: lowercase, runs of
/// non-alphanumerics collapsed to single dashes. Shared vocabulary on
/// purpose — `molt-storage` builds the real directory name from it and
/// the GUI previews it live under the create wizard's name field, so the
/// preview and the disk can never disagree.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = true; // suppress a leading dash
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// FNV-1a over a string: the one stable, non-cryptographic name hash the
/// demo machinery shares (workspace ids, brain seeds) — never key material.
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A stable fake [`WorkspaceId`] for demo entries: an FNV-1a hash of the
/// name, expanded to 32 bytes with splitmix-style mixing — distinct names
/// yield distinct ids (no cyclic-name collisions, unlike naive byte
/// repetition). Real ids come from the seed derivation in `molt-storage`;
/// this exists so session-only demo lists are addressable by id too.
pub fn demo_workspace_id(name: &str) -> WorkspaceId {
    let mut h = fnv1a64(name);
    // hash the length in as well so "a" and "a\0"-style paddings differ
    h ^= u64::try_from(name.len()).unwrap_or(u64::MAX);
    let mut id = String::with_capacity(64);
    for i in 0..4u64 {
        // splitmix64 finalizer over (h + block index)
        let mut z = h.wrapping_add(i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        id.push_str(&format!("{z:016x}"));
    }
    id
}

impl WorkspaceInfo {
    /// Sentinel for [`WorkspaceInfo::last_backup_min`]: never backed up.
    pub const NEVER: u32 = u32::MAX;

    fn never() -> u32 {
        Self::NEVER
    }

    /// The one rendering of the threshold rule (`"m-of-n"`) every list
    /// entry uses — Open screen, lifecycle finishes and the disk scan must
    /// not each own a format string.
    pub fn rule_detail(rule_m: u8, rule_n: usize) -> String {
        format!("{rule_m}-of-{rule_n}")
    }

    /// TEST FIXTURE ONLY — the demo republics of the first scaffold, kept
    /// `pub` because the engine, MCP and GUI test suites address them by
    /// name. No production constructor lists them (`SessionView::default()`
    /// is empty since review K6; the Open screen lists what is on disk).
    #[doc(hidden)]
    pub fn demo_set() -> Vec<WorkspaceInfo> {
        // Demo entries are closed workspaces: no presence knowledge exists
        // for them, so every member is honestly never-seen (the state keeps
        // the demo pill color only).
        fn m(name: &str, _last: &str, state: u8) -> MemberInfo {
            MemberInfo {
                name: name.to_string(),
                last_seen: MemberInfo::NEVER,
                state,
            }
        }
        #[allow(clippy::too_many_arguments)]
        fn w(
            name: &str,
            detail: &str,
            synced: bool,
            state: u8,
            last_sync_min: u32,
            sync_queue: u32,
            s3: bool,
            size_kib: u32,
            last_backup_min: u32,
            seed: &str,
            net: &str,
            members: Vec<MemberInfo>,
        ) -> WorkspaceInfo {
            WorkspaceInfo {
                id: demo_workspace_id(name),
                name: name.to_string(),
                detail: detail.to_string(),
                synced,
                state,
                last_sync_min,
                sync_queue,
                s3,
                size_kib,
                last_backup_min,
                backup_copies: 0,
                backup_error: String::new(),
                seed: seed.to_string(),
                net: net.to_string(),
                encrypted: false,
                members,
                agenda: String::new(),
                restored: false,
            }
        }
        vec![
            w(
                "Family Office",
                "4-of-7",
                true,
                0,
                2,
                0,
                true,
                1840,
                30,
                "canyon velvet mango orbit thrive lunar biscuit ember quartz willow drift anchor",
                "tor",
                vec![
                    m("mithra", "2 min ago", 0),
                    m("anahita", "5 min ago", 0),
                    m("ashi", "1 h ago", 0),
                    m("atar", "3 h ago", 0),
                    m("daena", "syncing", 1),
                    m("trust-agent", "just now", 0),
                    m("notary", "2 d ago", 2),
                ],
            ),
            w(
                "Savings-DAO",
                "3-of-5",
                false,
                1,
                0,
                80,
                false,
                920,
                WorkspaceInfo::NEVER,
                "pepper mosaic tundra violin nectar glacier saddle bloom copper raven mercy pilot",
                "tor",
                vec![
                    m("vayu", "just now", 0),
                    m("haoma", "syncing", 1),
                    m("armaiti", "4 h ago", 0),
                    m("rashnu", "1 d ago", 2),
                    m("sam-ki", "2 min ago", 0),
                ],
            ),
            w(
                "Neighborhood Fund",
                "2-of-3",
                true,
                0,
                0,
                0,
                true,
                310,
                240,
                "harbor sketch lentil aurora fossil timber pledge onion vapor cricket dune salute",
                "clearnet",
                vec![
                    m("me", "just now", 0),
                    m("chista", "10 min ago", 0),
                    m("airyaman", "1 h ago", 0),
                ],
            ),
            w(
                "Maker Studio",
                "3-of-4",
                true,
                0,
                60,
                0,
                false,
                2650,
                WorkspaceInfo::NEVER,
                "walnut prism cargo meadow tiger relish opera funnel jasper cloak ripple summit",
                "nym",
                vec![
                    m("zurvan", "1 h ago", 0),
                    m("bahram", "2 h ago", 0),
                    m("shop-bot", "1 h ago", 0),
                    m("sraosha", "6 h ago", 0),
                ],
            ),
            w(
                "Travel Pool",
                "2-of-3",
                false,
                2,
                4320,
                0,
                false,
                150,
                WorkspaceInfo::NEVER,
                "lagoon carbon sonnet farmer beacon myrtle candle octave slate hammock verge iris",
                "clearnet",
                vec![
                    m("haurvatat", "3 d ago", 2),
                    m("ameretat", "3 d ago", 2),
                    m("tishtrya", "5 d ago", 2),
                ],
            ),
            w(
                "Founders Circle",
                "5-of-9",
                false,
                1,
                0,
                12,
                true,
                4210,
                90,
                "quiver stamen ledger poncho basil zephyr magnet trellis cocoa infant scale rustic",
                "tor",
                vec![
                    m("ohrmazd", "just now", 0),
                    m("spenta", "syncing", 1),
                    m("vohuman", "8 min ago", 0),
                    m("zamyad", "22 min ago", 0),
                    m("asman", "1 h ago", 0),
                    m("mah", "2 h ago", 0),
                    m("hvare", "syncing", 1),
                    m("apam", "1 d ago", 2),
                    m("legal-ki", "5 min ago", 0),
                ],
            ),
        ]
    }
}

/// A backup found in the S3 bucket with no matching local workspace,
/// aggregated from a real bucket listing ([`Command::NetListBackups`]).
/// Shows up in the settings backup table with an empty "local" column.
///
/// Backups are named by the workspace-id *pseudonym*, never by display name
/// (`backup_restore_design.md` §6.2), so an orphan carries only its [`id`]
/// — [`name`] stays empty. A bucket key that does not follow the backup
/// naming scheme at all is still listed honestly: as an unknown entry with
/// an empty [`id`] and the raw object key as [`name`].
///
/// [`id`]: BackupOrphan::id
/// [`name`]: BackupOrphan::name
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupOrphan {
    /// The workspace-id pseudonym from the object key (empty for a foreign
    /// key that does not follow the backup naming scheme). Restore-from-S3
    /// starts from this id.
    #[serde(default)]
    pub id: WorkspaceId,
    /// Display label when one exists: empty for a real orphan (the bucket
    /// carries no workspace names), the raw object key for a foreign entry.
    pub name: String,
    /// Total size of the entry's objects in KiB (rounded up).
    pub size_kib: u32,
    /// Minutes since the entry's newest object was written.
    pub last_backup_min: u32,
}

/// One object from a real bucket listing, as reported by the off-actor
/// listing task ([`Command::NetListBackupsResult`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupObject {
    /// The full object key, e.g. `molt/<workspace_id>/<ts>.molt.enc`.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Last-modified time, unix seconds.
    pub modified: u64,
}

/// The bucket prefix every backup object lives under
/// (`backup_restore_design.md` §6.2) — the single authority for the scheme,
/// shared by the listing (engine) and [`parse_backup_key`]; story 12's
/// writer must build its keys from it too.
pub const BACKUP_OBJECT_PREFIX: &str = "molt/";

/// Build the bucket key of one backup object — the writer half of the
/// naming scheme [`parse_backup_key`] reads (`backup_restore_design.md`
/// §6.2): `molt/<workspace_id>/<ts:012>.molt.enc`. The timestamp is
/// zero-padded to 12 digits so lexicographic key order equals age order
/// forever — the retention pruner sorts keys, nothing parses times back.
pub fn backup_key(id: &WorkspaceId, ts: u64) -> String {
    format!("{BACKUP_OBJECT_PREFIX}{id}/{ts:012}.molt.enc")
}

/// Parse a bucket key against the backup naming scheme
/// `molt/<workspace_id>/<unix_ts:012>.molt.enc` (`backup_restore_design.md`
/// §6.2): the id is 64 lowercase hex chars, the stem EXACTLY 12 decimal
/// digits (the writer's fixed zero-padded width). Returns `(workspace_id,
/// ts)`, or `None` for any foreign key.
pub fn parse_backup_key(key: &str) -> Option<(WorkspaceId, u64)> {
    let rest = key.strip_prefix(BACKUP_OBJECT_PREFIX)?;
    let (id, file) = rest.split_once('/')?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    let stem = file.strip_suffix(".molt.enc")?;
    // EXACTLY the writer's 12-wide zero-padded width, digits only. Any other
    // width is a foreign key, never a backup: a short planted stem (e.g. `9`)
    // would otherwise sort lexicographically after a real newer key and invert
    // the retention pruner's oldest/newest pick. Digits-only also rejects the
    // leading '+' that `u64::parse` alone would accept.
    if stem.len() != 12 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ts: u64 = stem.parse().ok()?;
    Some((id.to_string(), ts))
}

/// Classify a real bucket listing into the session's orphan entries
/// (mock_todo story 8): objects whose workspace id is in `local_ids` belong
/// to a locally known workspace (their table row exists already) and are
/// skipped; parseable prefixes without a local workspace aggregate into one
/// [`BackupOrphan`] each (sizes summed, dated by the newest object);
/// foreign keys become per-key unknown entries — never silently dropped.
/// Pure and deterministic (orphans sorted by id, then unknowns by key);
/// `now` is unix seconds.
pub fn backup_orphans_from_listing(
    objects: &[BackupObject],
    local_ids: &[WorkspaceId],
    now: u64,
) -> Vec<BackupOrphan> {
    let minutes_since = |modified: u64| -> u32 {
        u32::try_from(now.saturating_sub(modified) / 60).unwrap_or(u32::MAX)
    };
    let kib = |bytes: u64| -> u32 { u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX) };
    // id → (total bytes, newest modified)
    let mut per_id: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut unknown: Vec<BackupOrphan> = Vec::new();
    for o in objects {
        match parse_backup_key(&o.key) {
            Some((id, _ts)) => {
                if local_ids.contains(&id) {
                    continue;
                }
                let e = per_id.entry(id).or_insert((0, 0));
                e.0 = e.0.saturating_add(o.size);
                e.1 = e.1.max(o.modified);
            }
            None => unknown.push(BackupOrphan {
                id: String::new(),
                name: o.key.clone(),
                size_kib: kib(o.size),
                last_backup_min: minutes_since(o.modified),
            }),
        }
    }
    let mut out: Vec<BackupOrphan> = per_id
        .into_iter()
        .map(|(id, (bytes, newest))| BackupOrphan {
            id,
            name: String::new(),
            size_kib: kib(bytes),
            last_backup_min: minutes_since(newest),
        })
        .collect();
    unknown.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(unknown);
    out
}

/// Metadata of a file shared into the chat. Only metadata travels in the
/// chat message — the bytes stay on the sharer's disk; a download fetches
/// them peer-to-peer over a dedicated encrypted queue (so the sharer must
/// be online). When the sharer deletes the local file the share flips to
/// unavailable for everyone, permanently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// File name (no path — where it lives is the sharer's business).
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Display type, e.g. `"PDF"` (proper MIME types come with transport).
    pub kind: String,
    /// The file's own date, unix seconds.
    pub modified: u64,
    /// Still present on the sharer's disk (`false` = removed; downloads
    /// answer "no longer available").
    #[serde(default = "file_available_default")]
    pub available: bool,
    /// sha256 over the file's bytes, lowercase hex, computed by the sharer
    /// at share time. The download anchor: a fetched file must hash to
    /// exactly this. Additive; "" on legacy shares (honestly unknown) —
    /// skipped when empty so the legacy wire shape stays byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checksum: String,
    /// Series-v2 material (`docs_archive/files/mirroring.md` §3.1): the file's
    /// content key (32 bytes, base64), its data piece count and the
    /// manifest root (hex sha256). Absent on a legacy exporter-sealed
    /// share - skipped when empty so the legacy wire shape stays
    /// byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_b64: String,
    /// Data pieces of the series (0 on a legacy share).
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub pieces: u32,
    /// Hex sha256 of the manifest ("" on a legacy share).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root: String,
}

fn u32_is_zero(v: &u32) -> bool {
    *v == 0
}

fn file_available_default() -> bool {
    true
}

/// The display type of a shared file, from its extension (proper MIME
/// sniffing can come with the transport; the label is presentation).
pub fn file_kind_label(name: &str) -> String {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase());
    match ext.as_deref() {
        Some("pdf") => "PDF",
        Some("jpg" | "jpeg" | "png" | "webp" | "gif" | "svg") => "Image",
        Some("md" | "txt") => "Text",
        Some("ods" | "xlsx" | "csv") => "Spreadsheet",
        Some("odt" | "docx") => "Document",
        Some("zip" | "tar" | "gz" | "7z") => "Archive",
        Some("mp3" | "ogg" | "flac" | "opus") => "Audio",
        Some("mp4" | "mkv" | "webm") => "Video",
        _ => "File",
    }
    .to_string()
}

/// The stable, globally unique identity of one chat message (chat-bus
/// concept Q1): 16 random bytes, minted by the **sender's engine** per
/// message (`molt-core` holds no I/O, so no RNG lives here — the bytes are
/// passed into the constructor). On the wire and in JSON it is a 32-char
/// **lowercase** hex string; [`MessageId::NIL`] (all zero) marks a legacy
/// message that predates ids and is skipped when serializing, so old log
/// entries re-serialize byte-identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct MessageId(pub [u8; 16]);

impl MessageId {
    /// The all-zero id: "this message predates stable ids". Never minted
    /// for a new message.
    pub const NIL: MessageId = MessageId([0u8; 16]);

    /// Whether this is the [`MessageId::NIL`] placeholder.
    pub fn is_nil(&self) -> bool {
        self.0 == [0u8; 16]
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl std::str::FromStr for MessageId {
    type Err = String;

    /// Parse the canonical form only: exactly 32 lowercase hex chars.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(format!(
                "a message id is 32 lowercase hex chars, got {s:?}"
            ));
        }
        let bytes = hex::decode(s).map_err(|e| format!("bad message id: {e}"))?;
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes);
        Ok(MessageId(id))
    }
}

impl Serialize for MessageId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Which chat "channel" a message files under (chat-bus concept Q2). There
/// is exactly **one** broadcast stream per republic; a channel is a *view*
/// over it, never a boundary — every member receives every message, a tag
/// hides nothing from anyone, and nothing engine-side trusts it beyond
/// display routing. Exactly one channel per message; cross-posting happens
/// by quoting into another channel.
///
/// New *system* channel kinds are new enum variants — deliberate, additive
/// design decisions, never free-form strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelRef {
    /// The all-hands group chat — the serde default, so every legacy
    /// message (and every old sender) files here.
    #[default]
    Group,
    /// Discussion attached to one proposal ("patch"); the UI resolves the
    /// title from proposal/chain state, lazily and tolerant of unknown ids.
    Patch {
        /// The referenced proposal.
        id: ProposalId,
    },
    /// A free, human-named topic channel — the escape valve.
    Topic {
        /// The topic's display name. Compared by **exact string equality**
        /// — no case or unicode folding in v1 (`"Budget"` and `"budget"`
        /// are different channels); normalization happens once, on send
        /// ([`ChannelRef::normalized`]).
        name: String,
    },
}

/// The cap a topic name is normalized against (in `char`s, on send).
pub const TOPIC_NAME_MAX_CHARS: usize = 64;

impl ChannelRef {
    /// The channel's stable string key (`"group"` / `"patch:<id>"` /
    /// `"topic:<name>"`) — what the B2 read cursors are stored under, and
    /// the same encoding the GUI sidebar rows carry. Injective: topic
    /// names cannot contain `:`-prefixed collisions with the fixed forms
    /// because the prefix alone decides the variant.
    pub fn storage_key(&self) -> String {
        match self {
            ChannelRef::Group => "group".to_string(),
            ChannelRef::Patch { id } => format!("patch:{}", id.0),
            ChannelRef::Topic { name } => format!("topic:{name}"),
        }
    }

    /// Whether this is the all-hands [`ChannelRef::Group`] channel (the
    /// `skip_serializing_if` guard that keeps a legacy-shaped message
    /// byte-identical on the wire).
    pub fn is_group(&self) -> bool {
        matches!(self, ChannelRef::Group)
    }

    /// Normalize on send: a topic name is trimmed, must be non-empty and at
    /// most [`TOPIC_NAME_MAX_CHARS`] chars. Case is **preserved** and
    /// equality stays exact-string — deliberately no unicode folding in v1.
    /// `Group` and `Patch` pass through unchanged.
    pub fn normalized(self) -> Result<ChannelRef, String> {
        match self {
            ChannelRef::Topic { name } => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return Err("a topic name must not be empty".to_string());
                }
                if name.chars().count() > TOPIC_NAME_MAX_CHARS {
                    return Err(format!(
                        "a topic name is at most {TOPIC_NAME_MAX_CHARS} characters"
                    ));
                }
                Ok(ChannelRef::Topic { name })
            }
            other => Ok(other),
        }
    }
}

/// What kind of chat message this is. An **enum, not a bool** — future
/// kinds stay open (`WorkspaceEvent` rule: additive-only evolution). `User`
/// is the default and stays invisible on the wire (`skip_serializing_if`),
/// so a pre-kind message serializes byte-identically; `System` marks
/// engine-authored notices (first use: the recovery rejoin announcement)
/// that render as quiet system lines, never as member speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatKind {
    /// A human message — the default every legacy entry decodes to.
    #[default]
    User,
    /// An engine-authored notice, rendered as a quiet system line.
    System,
}

impl ChatKind {
    /// Whether this is the default [`ChatKind::User`] (the
    /// `skip_serializing_if` guard that keeps a legacy-shaped message
    /// byte-identical on the wire).
    pub fn is_user(&self) -> bool {
        matches!(self, ChatKind::User)
    }
}

impl ChatMessage {
    /// A plain text message — the one constructor chat posts and test
    /// builders share, so the default-field shape cannot drift. The id is
    /// minted by the caller (the engine's CSPRNG; [`MessageId::NIL`] only
    /// for pre-id fixtures); the channel defaults to `Group` — set it via
    /// [`ChatMessage::with_channel`]; the kind defaults to `User` — set it
    /// via [`ChatMessage::with_kind`].
    pub fn text(
        id: MessageId,
        from: impl Into<MemberId>,
        body: impl Into<String>,
        ts: u64,
    ) -> ChatMessage {
        ChatMessage {
            id,
            from: from.into(),
            body: body.into(),
            ts,
            quote: None,
            quote_id: None,
            channel: ChannelRef::Group,
            kind: ChatKind::User,
            reactions: BTreeMap::new(),
            read_by: BTreeSet::new(),
            deleted_by: None,
            file: None,
        }
    }

    /// The same message filed under `channel` (builder-style).
    pub fn with_channel(mut self, channel: ChannelRef) -> ChatMessage {
        self.channel = channel;
        self
    }

    /// The same message carrying `kind` (builder-style).
    pub fn with_kind(mut self, kind: ChatKind) -> ChatMessage {
        self.kind = kind;
        self
    }
}

/// One chat message — THE schema of the chat log. The engine mutates and
/// the GUI reads this one type; on the wire (`read_state.applied`) it
/// serializes to the same JSON object as before, with **additive fields
/// since the chat bus** (`id`, `channel`, `quote_id`) that all skip their
/// default/legacy state — a pre-chat-bus message re-serializes
/// byte-identically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Stable unique message id (chat bus). [`MessageId::NIL`] on legacy
    /// log entries written before ids existed; never nil on a new message.
    #[serde(default, skip_serializing_if = "MessageId::is_nil")]
    pub id: MessageId,
    /// Sender handle.
    pub from: MemberId,
    /// Message body (empty once deleted).
    pub body: String,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub ts: u64,
    /// Legacy quote (0-based position in the chat log). Readable forever,
    /// **never written by new code** — new quotes use [`ChatMessage::quote_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<u64>,
    /// Quoted message by stable id (chat bus), if replying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_id: Option<MessageId>,
    /// The one channel this message files under (chat bus); `Group` for
    /// every legacy message.
    #[serde(default, skip_serializing_if = "ChannelRef::is_group")]
    pub channel: ChannelRef,
    /// What kind of message this is; `User` for every legacy message and
    /// invisible on the wire while it is (the byte-identity fixtures pin
    /// that a User message serializes exactly as before this field).
    #[serde(default, skip_serializing_if = "ChatKind::is_user")]
    pub kind: ChatKind,
    /// Emoji → the members who picked it (one reaction per member; a
    /// BTreeMap keeps the pill order stable across re-renders).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reactions: BTreeMap<String, Vec<MemberId>>,
    /// Members (never the author) who have confirmed reading this message
    /// (read receipts). Monotonic — insert-only, there is no "un-read";
    /// bounded by the roster; a `BTreeSet` keeps the dot order stable.
    /// Empty on every legacy message and invisible on the wire while it is
    /// (the byte-identity fixtures pin that an empty set serializes exactly
    /// as before this field — the `skip_serializing_if` is load-bearing).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub read_by: BTreeSet<MemberId>,
    /// Who deleted the message (`None` = live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<MemberId>,
    /// A shared file's metadata (`None` = a plain text message). Deleting
    /// the message drops the share with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileMeta>,
}

// ---------------------------------------------------------------------------
// Workspace storage schema (concept-workspace-storage.md). Typed structs in
// molt-core ARE the schema; the I/O lives in `molt-storage` (core holds no
// I/O). Every file starts with a format marker and version.
// ---------------------------------------------------------------------------

/// Format marker of `manifest.toml`.
pub const MANIFEST_FORMAT: &str = "molt-workspace";
/// Format marker of `prefs.toml`.
pub const PREFS_FORMAT: &str = "molt-workspace-prefs";
/// Highest manifest/log schema version this build understands. Opening
/// refuses politely above it; listing stays possible (forward compatibility
/// is a feature of the list screen, not of opening).
pub const STORAGE_VERSION: u32 = 1;

/// The manifest version a workspace is RAISED to on its first chain prune
/// (WP4b stage 5): an older binary — which would read a pruned
/// `chain.state` as "no chain" and happily run the legacy counted path
/// with a partial view — refuses the whole workspace at the manifest gate
/// instead ("newer than supported"). Unpruned workspaces keep
/// [`STORAGE_VERSION`], so old binaries read them unchanged.
pub const STORAGE_VERSION_PRUNED: u32 = 2;

/// The manifest version a workspace is RAISED to when it is sealed at rest
/// under its recovery phrase (S6, `docs_archive/storage/backup_restore_design.md` §5.4):
/// an older binary — which would trip over the keyless directory with a raw
/// I/O error — refuses the whole workspace politely at the manifest gate
/// instead. Unsealing recomputes the floor (pruned chain present →
/// [`STORAGE_VERSION_PRUNED`], else [`STORAGE_VERSION`]), so a decrypted
/// workspace stays openable by older binaries when nothing else requires
/// newness.
pub const STORAGE_VERSION_SEALED: u32 = 3;

/// [`CryptoParams::sealed`] marker of the default at-rest state: key material
/// device-sealed under `~/.moltrepublic/device.key` (`keys/workspace.key` +
/// `keys/seed.sealed` present). Shared vocabulary with the export blob's
/// `at_rest` meta field (S4).
pub const SEALED_DEVICE: &str = "device";
/// [`CryptoParams::sealed`] marker of the phrase-sealed at-rest state (S6):
/// NO key material on disk — the workspace key is re-derived from the
/// recovery phrase + the manifest id on decrypt (derive-and-verify).
pub const SEALED_PHRASE: &str = "phrase";
/// [`CryptoParams::sealed`] marker of a FETCHED backup that has not been
/// opened yet (S7, `backup_restore_design.md` §10): the directory holds the
/// downloaded `.molt.enc` blob VERBATIM and nothing else — no key material,
/// no name (the name is inside the ciphertext). `DecryptWorkspace` with the
/// recovery phrase drives the verified restore pipeline.
pub const SEALED_RESTORED: &str = "restored";

/// `manifest.toml` — the plaintext identity card of a workspace directory.
/// Deliberately *minimal*: what the Open screen needs before the user
/// authorizes decryption, and nothing that leaks content (no roster, no
/// acting member handle). Written once at creation, rewritten only on rename.
///
/// Parsed leniently (`deny_unknown_fields` off, defaults where sensible):
/// manifests written by a newer node must stay listable by an older one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceManifest {
    /// Format marker, [`MANIFEST_FORMAT`].
    #[serde(default)]
    pub format: String,
    /// Schema version; opening checks `version <= STORAGE_VERSION`.
    #[serde(default)]
    pub version: u32,
    /// The identity card.
    pub workspace: ManifestWorkspace,
    /// Crypto parameters for the key-sealing path.
    #[serde(default)]
    pub crypto: CryptoParams,
}

/// The `[workspace]` table of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestWorkspace {
    /// 32-byte hex id, derived from the seed + member identity.
    pub id: WorkspaceId,
    /// Display name (presentation only; may repeat across workspaces).
    pub name: String,
    /// Creation time, unix seconds.
    #[serde(default)]
    pub created: u64,
    /// Approval threshold (m). A plaintext copy for the list screen; the
    /// authoritative value is the `Founded` genesis event.
    #[serde(default)]
    pub rule_m: u8,
    /// Member count (n). Plaintext copy, like `rule_m`.
    #[serde(default)]
    pub rule_n: u8,
}

/// The `[crypto]` table of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CryptoParams {
    /// Key-derivation for the (future, opt-in) passphrase sealing path.
    #[serde(default = "default_kdf")]
    pub kdf: String,
    /// The AEAD used for frames, snapshots and exports.
    #[serde(default = "default_cipher")]
    pub cipher: String,
    /// Path of the sealed workspace key, relative to the workspace dir.
    #[serde(default = "default_key_file")]
    pub key_file: String,
    /// The at-rest state: [`SEALED_DEVICE`] (default — key material sealed to
    /// the device key) or [`SEALED_PHRASE`] (S6 — no key material on disk;
    /// the recovery phrase is the credential). Additive: manifests written
    /// before this field default to `"device"`, which is exactly what they
    /// are — zero migration.
    #[serde(default = "default_sealed")]
    pub sealed: String,
}

impl Default for CryptoParams {
    fn default() -> Self {
        CryptoParams {
            kdf: default_kdf(),
            cipher: default_cipher(),
            key_file: default_key_file(),
            sealed: default_sealed(),
        }
    }
}

fn default_kdf() -> String {
    "argon2id".to_string()
}
fn default_cipher() -> String {
    "xchacha20poly1305".to_string()
}
/// The ONE place a workspace's sealed key may live, relative to its dir.
/// A manifest naming anything else is refused (`molt_storage::read_manifest`):
/// the path is joined and later zero-filled + unlinked by the seal, and a
/// synced or imported manifest is attacker-writable plaintext.
pub const DEFAULT_KEY_FILE: &str = "keys/workspace.key";

fn default_key_file() -> String {
    DEFAULT_KEY_FILE.to_string()
}
fn default_sealed() -> String {
    SEALED_DEVICE.to_string()
}

/// `prefs.toml` — per-workspace settings that are *this node's business*,
/// not shared history: they belong neither in the manifest (identity only)
/// nor in the event log (toggling a local backup must not fork history).
/// Rewritten atomically via `tmp/` on every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePrefs {
    /// Format marker, [`PREFS_FORMAT`].
    #[serde(default)]
    pub format: String,
    /// Schema version.
    #[serde(default)]
    pub version: u32,
    /// Automatic S3 backup on/off.
    #[serde(default)]
    pub s3_backup: bool,
    /// Unix seconds of the last completed backup; `None` = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<u64>,
    /// Legacy marker: this republic was founded by the pre-network
    /// in-process simulation (its other members never existed as real
    /// nodes). **Inert in production** — no engine spawns fake peers, and
    /// governance never counts for peers; only the demo-mesh test seam
    /// reads it to decide which loopback contexts get demo peers. Kept so
    /// old prefs files stay parseable and honestly labeled.
    #[serde(default)]
    pub simulated_members: bool,
    /// MY shares: chat message id (hex) → absolute local source path, so
    /// this node can keep serving downloads across restarts. Strictly this
    /// node's business — prefs.toml never crosses the wire and is not
    /// history (the paths would leak the local filesystem layout).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub shared_files: std::collections::BTreeMap<String, String>,
    /// B2 — the seat's own read state: channel key
    /// ([`ChannelRef::storage_key`]) → the [`MessageId`] (hex) this seat
    /// has read THROUGH. Id-addressed, never a position: positions shift
    /// under WP4a compaction, ids do not. Local convenience like the rest
    /// of the prefs — never history, never on the wire; the shared read
    /// RECEIPTS are a different thing and stay in the log.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub read_cursors: std::collections::BTreeMap<String, String>,
    /// Where this seat keeps the mirrored pieces (`docs_archive/files/mirroring.md`
    /// §3.5); empty = `<workspace root>/../mirror/<republic-id>/`. Private,
    /// any-path: GUI/config-only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mirror_dir: String,
    /// This node's presence MEMORY: member name → unix seconds it was last
    /// observed (a real sighting, or the roster event that put it in the
    /// republic — the founding is one). Local convenience like the rest of
    /// the prefs: never history, never on the wire, and deliberately NOT
    /// chained — who was online when is nobody else's business and would
    /// need no consensus. Without it a restart reads every seat back as
    /// [`MemberInfo::NEVER`], which on a republic this node founded itself
    /// is simply false.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub last_seen: std::collections::BTreeMap<String, u64>,
}

impl Default for WorkspacePrefs {
    fn default() -> Self {
        WorkspacePrefs {
            format: PREFS_FORMAT.to_string(),
            version: STORAGE_VERSION,
            s3_backup: false,
            last_backup: None,
            simulated_members: false,
            shared_files: std::collections::BTreeMap::new(),
            read_cursors: std::collections::BTreeMap::new(),
            last_seen: std::collections::BTreeMap::new(),
            mirror_dir: String::new(),
        }
    }
}

/// Format marker of `transport.state` (the node-local encrypted transport
/// bookkeeping file — concept-transport-simplex-tor.md §6). v2 added the
/// `identity_sk` field (additive; a v1 file loads with it defaulting to
/// `None`); v3 added `nostr_sk` the same way; v4 added the Nostr transport
/// shape (`kind`, `relays`, `rotation_seed`, `relay_cursors` — all additive,
/// `nostr_transport_marmot.md` §4.1/N4).
pub const TRANSPORT_STATE_VERSION: u32 = 4;

/// Which transport family a workspace's `transport.state` describes. Absent
/// (`None`) on every pre-N4 file — those are queue-shaped (the loopback test
/// transport today; SMP historically) and keep being classified by their
/// field shape. The discriminator exists so the resume/offline gates read
/// KIND, not shape (`nostr_n05_engine_inventory.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// A Nostr-relay workspace (N4+): group traffic as kind-445 events over
    /// the workspace relay list; no queues, no per-pair mesh.
    Nostr,
}

/// The outbound half of one delivery cursor: how far this node's log has
/// been fanned out to one peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundCursor {
    /// The last local log seq whose fan-out to this peer was accepted by
    /// the transport (the outbox resumes at `log_seq + 1`).
    pub log_seq: u64,
    /// Wire messages sent on this link so far (each message carries the
    /// next value — the receiver's dedup/order key).
    pub wire_seq: u64,
    /// Delivery guarantee §4.4: every OWN wire-relevant seq at or below this
    /// is confirmed ENGINE-accepted by the peer (advanced from its ACK
    /// frames against our own log) — the rewind target on every leg
    /// re-establishment. Additive; 0 until the first ack.
    #[serde(default)]
    pub acked_floor: u64,
    /// Whether an ACK from this peer was ever received (the mixed-version
    /// gate §4.8: rewind/resend semantics activate only for proven-updated
    /// peers — an old peer keeps exactly the pre-guarantee behavior).
    #[serde(default)]
    pub ack_seen: bool,
    /// Bumped on every rewind: salts the resend msg ids (§4.5) so the
    /// receiver's completed-id ring can never swallow a resend unread.
    #[serde(default)]
    pub resend_epoch: u32,
}

/// The receive-side accept window (delivery guarantee,
/// `docs_archive/transport/delivery_guarantee.md` §4.2): per SENDER, which of that sender's log seqs
/// this node's engine has accepted. It is both the envelope-level dedup (a
/// resent envelope must never re-apply — G2) and the payload of the mesh ACK
/// frame the sender trims its resend range with. `high` is the highest
/// accepted seq; `bits` marks the [`ACCEPT_WINDOW_BITS`] seqs directly below
/// it (bit offset `high - 1 - seq`, little-endian across `u64` words). Seqs
/// below the window read as accepted (aged out — W is large against the
/// resend cadence, so nothing legit lingers there unconfirmed). The bit
/// positions live in the sender's LOG seq space, which contains its own
/// events only sparsely — a zero bit is "not seen", never by itself "lost";
/// only the sender can diff it against what it actually sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedWindow {
    /// Highest accepted seq from this sender (0 = nothing accepted yet).
    #[serde(default)]
    pub high: u64,
    /// Accept marks for the `ACCEPT_WINDOW_BITS` seqs below `high`; missing
    /// or short vectors read as all-zero (serde-additive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bits: Vec<u64>,
}

/// Width of [`AcceptedWindow`]: how many seqs below `high` keep explicit
/// accept marks before aging out as accepted.
pub const ACCEPT_WINDOW_BITS: u64 = 1024;
const ACCEPT_WINDOW_WORDS: usize = 16;
const _: () = assert!(ACCEPT_WINDOW_WORDS * 64 == 1024, "words must cover the window");

/// Split an in-window bit offset into its word index and bit position.
/// Callers guard `offset < ACCEPT_WINDOW_BITS`, so the conversion is total;
/// the fallback only defends against a future guard slip (reads miss, writes
/// no-op — never a panic, never a wrong bit).
fn window_word_bit(offset: u64) -> (usize, u64) {
    (usize::try_from(offset / 64).unwrap_or(usize::MAX), offset % 64)
}

impl AcceptedWindow {
    /// Whether `seq` counts as accepted: the high mark, a marked bit inside
    /// the window, or anything below the window (aged out).
    pub fn is_accepted(&self, seq: u64) -> bool {
        if self.high == 0 || seq > self.high {
            return false;
        }
        if seq == self.high {
            return true;
        }
        let offset = self.high - 1 - seq;
        if offset >= ACCEPT_WINDOW_BITS {
            return true; // below the window: treated accepted (aged out)
        }
        let (w, b) = window_word_bit(offset);
        self.bits.get(w).is_some_and(|word| word & (1u64 << b) != 0)
    }

    /// Record `seq` as accepted. `true` = fresh (apply it), `false` = already
    /// accepted (a duplicate — the caller drops the envelope). `seq 0` is
    /// never valid (log seqs start at 1) and is rejected outright — a
    /// crafted envelope must not reach the `high - 1 - seq` arithmetic
    /// (overflow-checks would turn it into an engine-killing panic).
    pub fn accept(&mut self, seq: u64) -> bool {
        if seq == 0 || self.is_accepted(seq) {
            return false;
        }
        if seq > self.high {
            let shift = seq - self.high;
            self.shift_up(shift);
            // the previous high becomes a below-high mark — unless there was
            // none (high 0) or it just aged past the window
            if self.high > 0 && shift <= ACCEPT_WINDOW_BITS {
                self.set(shift - 1);
            }
            self.high = seq;
        } else {
            self.set(self.high - 1 - seq);
        }
        true
    }

    /// Set the bit at `offset` (< [`ACCEPT_WINDOW_BITS`]).
    fn set(&mut self, offset: u64) {
        if self.bits.len() < ACCEPT_WINDOW_WORDS {
            self.bits.resize(ACCEPT_WINDOW_WORDS, 0);
        }
        let (w, b) = window_word_bit(offset);
        if let Some(word) = self.bits.get_mut(w) {
            *word |= 1u64 << b;
        }
    }

    /// Slide the window up by `by` seqs: every mark's offset grows by `by`;
    /// marks pushed past the window edge age out (they then READ as accepted).
    fn shift_up(&mut self, by: u64) {
        if self.bits.len() < ACCEPT_WINDOW_WORDS {
            self.bits.resize(ACCEPT_WINDOW_WORDS, 0);
        }
        if by >= ACCEPT_WINDOW_BITS {
            self.bits.iter_mut().for_each(|w| *w = 0);
            return;
        }
        let (words, bits) = window_word_bit(by);
        for w in (0..ACCEPT_WINDOW_WORDS).rev() {
            let lower = if w >= words { self.bits[w - words] } else { 0 };
            let carry = if bits > 0 && w > words {
                self.bits[w - words - 1] >> (64 - bits)
            } else {
                0
            };
            self.bits[w] = if bits > 0 { (lower << bits) | carry } else { lower };
        }
    }
}

/// One extra redundant queue on a [`MeshLink`] (Track B Stage 2, N-redundancy):
/// a queue's server + id, sharing the link's single per-direction wrap key
/// (`snd_wrap`/`rcv_wrap`). The primary (index 0) queue stays the scalar
/// `snd_*`/`rcv_*` fields; these are queues 1..N. All strings so `molt-core`
/// keeps no transport dependency.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QueueRef {
    /// The queue's server (`smp://fingerprint@host`; empty for the loopback hub).
    pub server: String,
    /// The queue id, lowercase hex.
    pub queue: String,
}

/// One peer's runtime **full-mesh handover** (concept §3.2/§3.3): the per-pair
/// queues a node uses to reach and hear one peer. All fields are strings so
/// `molt-core` keeps no transport dependency — `molt-net` parses them into a
/// `PeerLink`. `snd_*` is the peer's inbound queue this node SENDS to; `rcv_*`
/// is this node's own inbound queue it RECEIVES on from that peer (each party
/// owns the queue it receives on). Persisted so a reopened workspace rebuilds
/// its mesh without re-bootstrapping (real SMP queues live on their servers;
/// the ephemeral loopback hub rebuilds fresh). Track B Stage 2: `snd_extra`/
/// `rcv_extra` carry the redundant queues 1..N (additive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshLink {
    /// The peer this link reaches.
    pub member: MemberId,
    /// The peer's inbound-queue server (`smp://fingerprint@host`; empty for the
    /// loopback hub).
    pub snd_server: String,
    /// The peer's inbound-queue id (lowercase hex) this node sends to.
    pub snd_queue: String,
    /// The wrap key of the peer's inbound queue (lowercase hex).
    pub snd_wrap: String,
    /// This node's own inbound-queue id (lowercase hex) it receives on from the
    /// peer.
    pub rcv_queue: String,
    /// The wrap key of this node's own inbound queue (lowercase hex).
    pub rcv_wrap: String,
    /// This node's own inbound-queue server (`smp://fingerprint@host`; empty =
    /// the transport's own server / loopback). Additive (Stage 0): an old
    /// `transport.state` without it reads as empty and resumes single-server
    /// exactly as before; a mesh spread across servers persists the real server
    /// here so the resumed leg subscribes on it, not a collapsed single one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rcv_server: String,
    /// EXTRA redundant queues (1..N) on the peer's inbound side this node SENDS
    /// to, sharing `snd_wrap`. Additive (Stage 2): an old `transport.state`
    /// without it reads as empty ⇒ a single-queue leg, exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snd_extra: Vec<QueueRef>,
    /// EXTRA redundant queues (1..N) on this node's OWN inbound side it RECEIVES
    /// on from the peer, sharing `rcv_wrap`. Additive (Stage 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rcv_extra: Vec<QueueRef>,
}

/// `transport.state` — node-local transport bookkeeping (concept §6):
/// delivery cursors today; per-queue wrapping keys, MLS ratchets and the
/// dedup windows join in later milestones. It must **not** live in the
/// shared log: two nodes' cursors legitimately differ, and the log stays
/// replayable shared history. Losing this file loses transport progress
/// (peers absorb the resulting resends via their dedup) — never shared
/// history, BUT since v3 it can also hold `nostr_sk`, a non-re-derivable
/// per-seat secret whose loss is permanent until a recovery ritual
/// re-anchors the seat (the storage read layer logs that loss loudly).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportState {
    /// Schema version ([`TRANSPORT_STATE_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// Per peer: how far our log is fanned out to them.
    #[serde(default)]
    pub outbound: BTreeMap<MemberId, OutboundCursor>,
    /// Per peer: the last inbound wire seq applied (their messages with a
    /// lower or equal seq are duplicates).
    #[serde(default)]
    pub inbound: BTreeMap<MemberId, u64>,
    /// The node's MLS group state (transport concept §6): an opaque
    /// [`molt_net::MlsMember`] snapshot — ratchets, the secret tree, key
    /// packages and the signer. Born in the founding ritual, overwritten (never
    /// appended) on every ratchet advance, so its deletion of old key material
    /// *is* forward secrecy. `None` before the ritual established a group.
    #[serde(default)]
    pub mls: Option<Vec<u8>>,
    /// The runtime full-mesh handovers (one per peer), established after
    /// founding by announcing per-pair queues in-band over MLS. Empty until the
    /// mesh is bootstrapped; a reopened workspace rebuilds its supervisor from
    /// these.
    #[serde(default)]
    pub mesh: Vec<MeshLink>,
    /// The transport's serialized queue **credentials** (opaque
    /// `molt_net::Transport::export_creds` bytes: recipient keys of our mesh
    /// queues + secured sender keys). Written on a clean close so a reopened
    /// node re-adopts the same queues and resumes the mesh; `None` before the
    /// first clean close (or for a credential-less transport). Sensitive — lives
    /// only inside the already-encrypted `transport.state`.
    #[serde(default)]
    pub smp_queues: Option<Vec<u8>>,
    /// This node's own **identity signing seed** (32-byte Ed25519 seed, what
    /// `ed25519_dalek::SigningKey::to_bytes()` returns), derived from the
    /// member's recovery phrase at founding/join and kept here so a reopened
    /// workspace can sign its governance approvals for the persistent chain
    /// without re-entering the phrase. Sensitive — lives only inside the
    /// already-encrypted `transport.state`; the same key also backs the
    /// persisted MLS signer (the Ed25519 pair of the three-anchor
    /// identity). `None` before a chain-aware founding.
    #[serde(default)]
    pub identity_sk: Option<Vec<u8>>,
    /// This node's own **Nostr transport secret** (32-byte secp256k1 scalar,
    /// the third anchor's private half — `nostr_transport_marmot.md` §3),
    /// derived at founding/join from the member's recovery phrase salted with
    /// the seat's single-use ticket, and kept here because the ticket dies
    /// with the ritual — the key is NOT re-derivable later. Sensitive — lives
    /// only inside the already-encrypted `transport.state`, exactly like
    /// `identity_sk`. `None` for legacy workspaces and for a recovered seat
    /// (the old device's ticket is gone; recovery-link v2 owns that story).
    #[serde(default)]
    pub nostr_sk: Option<Vec<u8>>,
    /// Per SENDER: which of that sender's log seqs this node's engine has
    /// accepted ([`AcceptedWindow`] — the delivery guarantee's envelope dedup
    /// and ACK payload). Additive: an old `transport.state` reads as empty ⇒
    /// everything arriving is fresh, exactly as before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accepted: BTreeMap<MemberId, AcceptedWindow>,
    /// The transport family (v4). `None` = a pre-N4 queue-shaped file; the
    /// resume gates fall back to shape-classification exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TransportKind>,
    /// The workspace's group relay list (normalized `ws://`/`wss://` URLs) —
    /// what the group agreed to publish/subscribe on, recorded at
    /// founding/join and (from N6) changed only by a `TransportPolicy` chain
    /// block. Dialing still passes the operator's ADR-0004 pool gates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    /// The group's stable h-tag seed (32 bytes, `envelope::h_tag`), minted at
    /// founding and delivered only inside the authenticated Welcome. Secret —
    /// an observer holding it can compute every past and future window tag of
    /// the group; lives only inside the already-encrypted `transport.state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_seed: Option<Vec<u8>>,
    /// Per relay URL: the subscription cursor (`created_at` floor) the N5
    /// runtime resumes from (`RelayRuntime::cursors()` shape). Strictly an
    /// optimization — correctness rests on the ACK/rewind layer (§4.3).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relay_cursors: BTreeMap<String, u64>,
    /// The BROADCAST send cursor: ONE publish position for the whole republic,
    /// because one kind-445 event reaches every member (§4.1). The per-member
    /// ACK half — `acked_floor`/`ack_seen` — stays in [`Self::outbound`]; only
    /// the SEND position collapses.
    ///
    /// Additive at v4 with no version bump on purpose: `read_transport_state_at`
    /// answers `default()` for any file whose version exceeds the binary's, and
    /// that would throw away `nostr_sk`, which nothing can re-derive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupCursor>,
    /// The file plane's resumable jobs (`docs_archive/files/mirroring.md` §3.2):
    /// the trickle sender's publish queue and the running piece fetches.
    /// Additive like `group`.
    #[serde(default, skip_serializing_if = "FileJobs::is_empty")]
    pub file_jobs: FileJobs,
    /// The mirror gossip (`docs_archive/files/mirroring.md` §3.4): this seat's
    /// declaration and every member's declaration and hold status.
    #[serde(default, skip_serializing_if = "MirrorState::is_default")]
    pub mirror: MirrorState,
}

/// A seat's default mirror budget per republic: 1 GiB.
pub const MIRROR_QUOTA_DEFAULT: u64 = 1024 * 1024 * 1024;

fn mirror_on_default() -> bool {
    true
}

fn one_page() -> u16 {
    1
}

fn mirror_quota_default() -> u64 {
    MIRROR_QUOTA_DEFAULT
}

/// What this seat declared and what the members told it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorState {
    /// This seat mirrors persistent files (consent; on by default).
    #[serde(default = "mirror_on_default")]
    pub on: bool,
    /// This seat's total mirror budget for the republic, bytes.
    #[serde(default = "mirror_quota_default")]
    pub quota: u64,
    /// Revision of this seat's declaration (unix seconds of the change).
    #[serde(default)]
    pub rev: u64,
    /// The other members' declarations, last-wins by revision.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decls: BTreeMap<MemberId, MirrorDecl>,
    /// The other members' hold status, as last reported.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status: BTreeMap<MemberId, Vec<MirrorHold>>,
    /// The series this seat mirrors, by share id hex.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, MirrorJob>,
}

impl Default for MirrorState {
    fn default() -> Self {
        MirrorState {
            on: true,
            quota: MIRROR_QUOTA_DEFAULT,
            rev: 0,
            decls: BTreeMap::new(),
            status: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }
}

impl MirrorState {
    /// Nothing declared, nothing learned.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == MirrorState::default()
    }
}

/// One member's mirror declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorDecl {
    /// Consent.
    pub on: bool,
    /// Budget, bytes.
    pub quota: u64,
    /// Revision (unix seconds of the change).
    pub rev: u64,
}

/// One series a member holds: verified data pieces out of the count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorHold {
    /// The share message's stable id.
    pub id: MessageId,
    /// Verified data pieces.
    pub held: u32,
    /// The series' data piece count.
    pub of: u32,
}

/// The file plane's persisted jobs: a restart RESUMES each at its cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileJobs {
    /// The trickle sender's queue, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<PublishJob>,
    /// The piece fetches in progress.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<FetchJob>,
    /// The unix day (`secs / 86_400`) `sent_bytes` counts.
    #[serde(default)]
    pub sent_day: u64,
    /// Piece bytes the trickle sent on `sent_day` (the daily cap's counter).
    #[serde(default)]
    pub sent_bytes: u64,
}

impl FileJobs {
    /// Nothing queued and nothing counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.publish.is_empty() && self.fetch.is_empty() && self.sent_bytes == 0
    }
}

/// One series (or a range of its pieces) the trickle sender publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishJob {
    /// The share message id, lowercase hex.
    pub series: String,
    /// The share's content key (32 bytes).
    pub key: Vec<u8>,
    /// The local source path (node-local, like `prefs.shared_files`).
    pub path: String,
    /// The share's data piece count.
    pub count: u32,
    /// The share's byte length.
    pub size: u64,
    /// The share's manifest root, hex.
    pub root: String,
    /// The piece indices to publish, as inclusive ranges in publish order.
    pub ranges: Vec<(u32, u32)>,
    /// The position within `ranges` the next publish takes (the cursor).
    #[serde(default)]
    pub next: u32,
    /// The stamp announced as the series start.
    pub started_at: u64,
    /// `path` is a mirror's piece DIRECTORY of sealed pieces (one file per
    /// index) instead of the shared file: published as stored.
    #[serde(default)]
    pub stored: bool,
}

/// Whether bit `index` of a little-endian bitmap is set.
#[must_use]
pub fn bitmap_holds(bits: &[u8], index: u32) -> bool {
    let byte = usize::try_from(index / 8).unwrap_or(usize::MAX);
    bits.get(byte).is_some_and(|b| b & (1 << (index % 8)) != 0)
}

/// Set bit `index` of a little-endian bitmap (grown as needed).
pub fn bitmap_mark(bits: &mut Vec<u8>, index: u32) {
    let byte = usize::try_from(index / 8).unwrap_or(usize::MAX);
    if bits.len() <= byte {
        bits.resize(byte + 1, 0);
    }
    if let Some(b) = bits.get_mut(byte) {
        *b |= 1 << (index % 8);
    }
}

/// One piece fetch in progress on the requester side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchJob {
    /// The share message id, lowercase hex.
    pub series: String,
    /// The share's content key (32 bytes).
    pub key: Vec<u8>,
    /// The share's data piece count.
    pub count: u32,
    /// The share's byte length.
    pub size: u64,
    /// The share's manifest root, hex.
    pub root: String,
    /// The share's sha256, hex ("" on a share without one).
    pub checksum: String,
    /// The share's file name.
    pub name: String,
    /// The explicit destination the download named, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// The session's download directory at the time of the request.
    pub default_dir: String,
    /// The series start stamp the sharer announced.
    pub started_at: u64,
    /// Verified data pieces, one bit per index (little-endian bit order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<u8>,
}

impl FetchJob {
    /// Whether data piece `index` is verified on disk.
    #[must_use]
    pub fn holds(&self, index: u32) -> bool {
        bitmap_holds(&self.held, index)
    }

    /// Mark data piece `index` verified.
    pub fn mark(&mut self, index: u32) {
        bitmap_mark(&mut self.held, index);
    }

    /// Every verified data index below `count`.
    #[must_use]
    pub fn held_indices(&self) -> Vec<u32> {
        (0..self.count).filter(|i| self.holds(*i)).collect()
    }
}

/// One series this seat mirrors (`docs_archive/files/mirroring.md` §3.3): the
/// pieces live sealed in the mirror folder, one file per index; the
/// bitmap says which are verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorJob {
    /// The share's data piece count.
    pub count: u32,
    /// The share's byte length.
    pub size: u64,
    /// The share's manifest root, hex.
    pub root: String,
    /// The share's content key (32 bytes).
    pub key: Vec<u8>,
    /// The series start stamp the sharer announced.
    pub started_at: u64,
    /// Verified data pieces, one bit per index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<u8>,
    /// Every piece verified and the manifest pieces stored.
    #[serde(default)]
    pub complete: bool,
    /// Plaintext bytes held (the quota's counter).
    #[serde(default)]
    pub bytes: u64,
}

impl MirrorJob {
    /// Whether data piece `index` is verified in the store.
    #[must_use]
    pub fn holds(&self, index: u32) -> bool {
        bitmap_holds(&self.held, index)
    }

    /// Mark data piece `index` verified.
    pub fn mark(&mut self, index: u32) {
        bitmap_mark(&mut self.held, index);
    }

    /// Verified data pieces.
    #[must_use]
    pub fn held_count(&self) -> u32 {
        u32::try_from((0..self.count).filter(|i| self.holds(*i)).count()).unwrap_or(u32::MAX)
    }

    /// Every verified data index below `count`.
    #[must_use]
    pub fn held_indices(&self) -> Vec<u32> {
        (0..self.count).filter(|i| self.holds(*i)).collect()
    }
}

/// The broadcast transport's send position and its resend budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCursor {
    /// Last local log seq whose kind-445 publish at least one relay accepted.
    #[serde(default)]
    pub log_seq: u64,
    /// Resend rounds published inside the hour beginning at
    /// `resend_window_start`. PERSISTED, unlike the per-peer backoff: a crash
    /// loop must not re-publish the unacked tail to every relay on every start.
    #[serde(default)]
    pub resend_rounds: u32,
    /// Unix second the current resend hour began.
    #[serde(default)]
    pub resend_window_start: u64,
    /// A claim sheet spoke about this node's events since the last resend
    /// round — a listening, still-lagging peer is PROVEN. Grants the one
    /// budget-free heal round below (live incident 2026-08-09 §2).
    #[serde(default)]
    pub heal_evidence: bool,
    /// Evidence-driven rounds taken past a spent budget this hour (cap 1;
    /// resets with the window).
    #[serde(default)]
    pub heal_rounds: u32,
}

/// One event in a workspace's append-only history: the envelope every log
/// frame carries. `apply(event)` in the engine is the only thing that
/// mutates workspace state — replaying the log from zero reconstructs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Strictly monotonic per workspace; the log's primary key.
    pub seq: u64,
    /// Unix seconds (engine clock at event creation).
    pub ts: u64,
    /// Who caused it (member handle for now; MLS leaf identity later).
    pub by: MemberId,
    /// What happened.
    pub body: WorkspaceEvent,
    /// In-order delivery chain (delivery guarantee G7): the seq of this
    /// author's PREVIOUS own ackable envelope (`MlsCommit`s excluded — they
    /// never reach a peer's engine), stamped at record time. A receiver
    /// holds this envelope until `prev_seq` is in its accept window, so a
    /// resent predecessor can never be applied AFTER its successor. `0` =
    /// no predecessor / a pre-G7 writer — delivered unordered, exactly the
    /// legacy behavior (and serialized-away, so every pre-G7 byte fixture,
    /// log frame and hash stays identical).
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub prev_seq: u64,
}

/// `skip_serializing_if` helper: keeps `prev_seq: 0` (legacy/no-chain) off
/// the wire and out of the log bytes.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

/// One member's anchored identity: the per-workspace Ed25519 public key
/// derived from that member's own recovery phrase (transport concept
/// §3.3 — "all keys derive from this phrase" holds for every member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberIdentity {
    /// The member's display name (their own choice, delivered with the
    /// invite activation).
    pub member: MemberId,
    /// The identity public key, lowercase hex (32 bytes Ed25519).
    pub identity_pk: String,
    /// The member's Nostr transport anchor: a 32-byte x-only BIP-340
    /// secp256k1 public key, lowercase hex in the signed bytes (the third
    /// anchor — `nostr_transport_marmot.md` §3). Ticket-salted per seat, so
    /// one person presents a different key in every republic. Additive
    /// (`#[serde(default)]`) so old persisted logs still decode; an empty
    /// value only ever occurs for that legacy data — the one founding path
    /// always fills it.
    #[serde(default)]
    pub nostr_pk: String,
}

/// One member's founding attestation: a signature with their identity key
/// over [`roster_canonical_bytes`] of the final table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterAttestation {
    /// The signing member.
    pub member: MemberId,
    /// The Ed25519 signature, lowercase hex (64 bytes).
    pub sig: String,
}

/// The complete sealed roster the founder distributes to every member at the
/// end of the ritual, so each writes its **own** `Founded` genesis (own seed,
/// own local workspace) from the same shared constitution. The member fills
/// in its own local `member` handle when materialising.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedRoster {
    /// The republic's display name.
    pub name: String,
    /// The neutral, content-derived republic id (the roster salt).
    pub republic_id: String,
    /// Approval threshold (m).
    pub rule_m: u8,
    /// Member count (n).
    pub rule_n: u8,
    /// Member handles in ritual order (founder first, then invite order).
    pub roster: Vec<MemberId>,
    /// name → identity key, same order.
    pub identities: Vec<MemberIdentity>,
    /// Every member's signature over the canonical table.
    pub attestations: Vec<RosterAttestation>,
    /// The group's relay pool, bound into the signed bytes since v4. A
    /// verifier recomputes over exactly this list, so a founder cannot seal a
    /// pool different from the one everybody ratified.
    #[serde(default)]
    pub relays: Vec<String>,
    /// The deliberated free-text charter every member ratified (concept §3.3).
    #[serde(default)]
    pub agenda: String,
    /// The ratified feature set (roster-v5, `charter_features.md`): the
    /// optional surfaces this republic activates, sorted + deduped. `None`
    /// on every pre-v5 roster — and `None` is what keeps those verifying
    /// (the canonical bytes stay v4). `Some([])` = deliberately none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

impl SealedRoster {
    /// Build the local `Founded` genesis envelope for a member. The single
    /// place a `Founded` body is constructed for a real founding, so a new
    /// genesis field cannot be forgotten at one of the call sites (founder
    /// finalize, GUI join, standalone join). `member` is this node's own local
    /// handle; `ts` the founding timestamp.
    pub fn into_genesis(&self, member: &str, ts: u64) -> EventEnvelope {
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts,
            by: member.to_string(),
            body: WorkspaceEvent::Founded {
                name: self.name.clone(),
                rule_m: self.rule_m,
                rule_n: self.rule_n,
                member: member.to_string(),
                roster: self.roster.clone(),
                identities: self.identities.clone(),
                attestations: self.attestations.clone(),
                republic_id: self.republic_id.clone(),
                agenda: self.agenda.clone(),
                relays: self.relays.clone(),
                features: self.features.clone(),
            },
        }
    }
}

/// Append one `u32`-length-prefixed (little-endian) byte string — THE
/// framing every canonical byte layout in this workspace uses
/// ([`roster_canonical_bytes`], the chain's `approval_bytes` /
/// `block_link_bytes` / `checkpoint_canonical_bytes`, and
/// `molt_storage::republic_id`), so the layouts stay siblings and a field
/// can never be extended by a longer neighbour. Length-prefixed rather than
/// separator-joined for the injectivity reason the storage crate records
/// on `republic_id`: a member-supplied field may carry any byte.
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(
        &u32::try_from(b.len())
            .expect("field exceeds the u32/u64 framing - ambiguous signed bytes are never written")
            .to_le_bytes(),
    );
    out.extend_from_slice(b);
}

/// Append a `u32` (little-endian) entry COUNT — the run header that keeps
/// a counted run injective once it is no longer the last field (see the
/// member-run note in [`roster_canonical_bytes`]). The chain layouts count
/// with `u64`, deliberately their own width; they do not use this.
pub fn put_count(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(
        &u32::try_from(n)
            .expect("field exceeds the u32/u64 framing - ambiguous signed bytes are never written")
            .to_le_bytes(),
    );
}

/// The one canonical serialization of a roster table — what every member
/// signs during the founding ritual's seal round and what every verifier
/// reconstructs. Length-prefixed fields, entries in the given order (the
/// ritual fixes the order: founder first, then invite order). v3 bound the
/// per-member `nostr_pk` (the third anchor).
///
/// **v4 binds the RELAY POOL** (`relay_topology_plan.md` R3, user-ratified
/// 2026-08-02). The pool decides who can reach whom — relays do not federate —
/// so it is as constitutional as the roster itself, and every member ratifies
/// it by signing these bytes.
///
/// Order is preserved, not sorted — but NOT because of dial priority: the
/// relay runtime connects to every relay concurrently, and the priority order
/// in `relay_pool.md` belongs to the node's LOCAL pool, which is a different
/// list. The order is signed because members ratify the list as they were
/// shown it, because the invite↔Welcome equality check compares the lists
/// element-wise, and because the recovery mint subsets them with `take`.
///
/// v4 also COUNTS the member run and length-prefixes `ws_id`, which v3 wrote
/// raw ahead of two single bytes. That boundary was not self-delimiting, so `("ab", 1, …)` and
/// `("a", 0x62, …)` produce overlapping prefixes. It is not exploitable
/// today — `republic_id` is always 64 hex chars — but the sibling layout
/// `molt-republic-id-v2` was length-prefixed for exactly this class after a
/// forged larger founding table collided, and an unframed field in a hash
/// preimage should not survive a version bump that is already rewriting it.
///
/// **v5 binds the ratified FEATURE SET** (`docs_archive/ritual/charter_features.md`
/// D2, user-decided 2026-08-11) — and it is **conditional on presence**,
/// the [`ChainChange::Membership`](crate::chain::ChainChange) precedent
/// lifted to the tag: `features: None` emits bytes **identical to v4** (tag
/// included), so every live v4 republic's genesis keeps verifying; `Some`
/// emits the v5 tag plus the feature run as the final field. `Some([])` is
/// a real, distinct value — a founder that deselected everything.
pub fn roster_canonical_bytes(
    ws_id: &str,
    rule_m: u8,
    rule_n: u8,
    members: &[MemberIdentity],
    agenda: &str,
    relays: &[String],
    features: Option<&[String]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if features.is_some() {
        b"molt-roster-v5\0".as_slice()
    } else {
        b"molt-roster-v4\0".as_slice()
    });
    let id = ws_id.as_bytes();
    put_bytes(&mut out, id);
    out.push(rule_m);
    out.push(rule_n);
    // COUNT the member run. v3 got away without one because the agenda was
    // the last field and had to exhaust the buffer; appending the relay run
    // after it removed that implicit delimiter, and an uncounted run then
    // collides — a one-member table byte-matches a zero-member table whose
    // agenda and relay absorb the same bytes. Same class as the separator-only
    // `republic_id` that let a forged larger founding table collide.
    put_count(&mut out, members.len());
    for m in members {
        let name = m.member.as_bytes();
        put_bytes(&mut out, name);
        let pk = m.identity_pk.as_bytes();
        put_bytes(&mut out, pk);
        // the third anchor rides inside each member's length-prefixed run;
        // a legacy (empty) value length-prefixes as 0 — no special casing
        // that could collide two different rosters onto one byte form
        let npk = m.nostr_pk.as_bytes();
        put_bytes(&mut out, npk);
    }
    // the deliberated charter (DAO name is already folded into the republic id
    // that salts ws_id; the free-text agenda is bound here) — every member's
    // seal signature is its ratification of exactly these bytes
    let ag = agenda.as_bytes();
    put_bytes(&mut out, ag);
    // the group's relay pool: entry-COUNTED then each entry length-prefixed,
    // so the preimage stays injective for any pool content and the field
    // cannot be extended by a longer neighbour
    put_count(&mut out, relays.len());
    for r in relays {
        let b = r.as_bytes();
        put_bytes(&mut out, b);
    }
    // v5: the ratified feature set — entry-COUNTED then each key
    // length-prefixed, like the relay run. Written only when present, so a
    // legacy (None) table stays byte-identical to v4.
    if let Some(f) = features {
        put_count(&mut out, f.len());
        for k in f {
            let b = k.as_bytes();
            put_bytes(&mut out, b);
        }
    }
    out
}

/// The explicit direction of a reaction event (chat bus). The **sender**
/// computes the toggle outcome against its own state and puts the result on
/// the wire; the applier treats it as an idempotent set/unset, so
/// at-least-once redelivery (the transport redelivers un-acked frames after a
/// hard crash; the MLS path has no wire-seq cursor) can never invert a reaction.
///
/// Mixed-version degradation (accepted, chat-bus Q3 posture): an OLD reader
/// drops the unknown `op` field on decode and still *toggles*, so a
/// redelivered duplicate can invert the reaction on that old node only —
/// acceptable while versions are mixed, gone once it upgrades and replays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactOp {
    /// Set the member's reaction to this emoji (a no-op when already set —
    /// one reaction per member, so any other emoji of theirs is cleared).
    Add,
    /// Clear the member's reaction of this emoji (a no-op when absent).
    Remove,
}

/// What can happen in a workspace. **Additive-only evolution**: new kinds
/// append variants; an older reader that meets an unknown variant must not
/// write to that workspace (applying a partial history would fork state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    /// seq 1, exactly once: who this republic is. Rule, roster and the
    /// acting member never exist outside the event stream. Since the
    /// founding ritual precedes the workspace (transport concept §3.3),
    /// the genesis carries the complete identity table and all n
    /// attestations — the member list is sealed from birth. (Both fields
    /// default empty so pre-ritual logs stay readable.)
    Founded {
        /// Display name at founding.
        name: String,
        /// Approval threshold (m).
        rule_m: u8,
        /// Member count (n).
        rule_n: u8,
        /// The acting member on this node.
        member: MemberId,
        /// The full, final member roster (there are no open seats).
        roster: Vec<MemberId>,
        /// name → identity key, founder first, then invite order.
        #[serde(default)]
        identities: Vec<MemberIdentity>,
        /// Every member's signature over the canonical table.
        #[serde(default)]
        attestations: Vec<RosterAttestation>,
        /// The republic's neutral, content-derived id
        /// ([`crate::roster_canonical_bytes`]' salt) the attestations were
        /// signed over — stored so every member verifies against the same
        /// value, independent of its own local workspace id. Empty on
        /// pre-republic-id genesis frames.
        #[serde(default)]
        republic_id: String,
        /// The deliberated charter: a free-text agenda the members agreed on
        /// and every one ratified with their seal signature before the
        /// workspace opened (transport concept §3.3). Immutable from birth,
        /// like the roster. Empty on a genesis founded without deliberation.
        #[serde(default)]
        agenda: String,
        /// The ratified relay pool (v4). Stored here for the same reason as
        /// `republic_id`: a reader holding only the log must be able to
        /// recompute the exact bytes the attestations above were signed over.
        #[serde(default)]
        relays: Vec<String>,
        /// The ratified feature set (roster-v5). Stored for the same
        /// recompute reason as `relays`; `None` on every pre-v5 genesis,
        /// and presence decides the tag the verifier recomputes under.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<Vec<String>>,
    },
    /// A seat filled via invite.
    MemberJoined {
        /// The joining member.
        member: MemberId,
    },
    /// A chat message was posted (the existing typed schema).
    Chat(ChatMessage),
    /// A member's emoji reaction on a chat message changed.
    ChatReacted {
        /// Message position in the chat log (0-based). Legacy addressing —
        /// applied only when `id` is absent (pre-chat-bus log entries);
        /// new events still record the position for older readers.
        index: u64,
        /// The target message's stable id (chat bus; additive — `None` on
        /// legacy log entries). Preferred over `index` when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<MessageId>,
        /// The reaction emoji.
        emoji: String,
        /// Who reacted.
        by: MemberId,
        /// The explicit, idempotent direction (additive — `None` on legacy
        /// log entries, which replay with the original toggle semantics).
        /// New senders always resolve the toggle locally and record
        /// `Some(..)`, so duplicates on the wire are harmless. See
        /// [`ReactOp`] for the accepted mixed-version degradation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<ReactOp>,
    },
    /// A chat message was wiped; only the deletion notice remains.
    ChatDeleted {
        /// Message position in the chat log (0-based). Legacy addressing —
        /// applied only when `id` is absent (pre-chat-bus log entries);
        /// new events still record the position for older readers.
        index: u64,
        /// The target message's stable id (chat bus; additive — `None` on
        /// legacy log entries). Preferred over `index` when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<MessageId>,
        /// Who deleted it.
        by: MemberId,
    },
    /// The sharer deleted a shared file from their disk — the share at
    /// this chat position is unavailable from now on.
    FileRemoved {
        /// The share message's position in the chat log (0-based). Legacy
        /// addressing — applied only when `id` is absent (pre-chat-bus log
        /// entries); new events still record the position for older readers.
        index: u64,
        /// The share message's stable id (chat bus; additive — `None` on
        /// legacy log entries). Preferred over `index` when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<MessageId>,
        /// The sharer.
        by: MemberId,
    },
    /// A member confirmed reading these messages (read receipts). Batched —
    /// one event carries a whole channel-open's worth of ids, encrypted once
    /// and fanned out like any chat event. A post-chat-bus feature, so it
    /// addresses purely by stable id (no legacy `index` fallback: every
    /// message has an id by the time it can be read). `by` is bound to the
    /// authenticated link identity on receive (like [`WorkspaceEvent::ChatReacted`]),
    /// so a member cannot forge another's receipt. No `op` — reads are
    /// monotonic (insert-only), so at-least-once redelivery is a harmless
    /// idempotent set insert.
    ChatRead {
        /// The stable ids the sender has read.
        ids: Vec<MessageId>,
        /// Who read them.
        by: MemberId,
    },
    /// An object was put forward for threshold approval.
    Proposed {
        /// The proposal id (assigned in delivery order).
        id: ProposalId,
        /// The gated target surface.
        surface: Surface,
        /// The surface-specific transition.
        payload: Value,
    },
    /// One member's approval landed on a pending proposal. On a chain-governed
    /// republic it also carries the member's **signature** over the committed
    /// change at a target chain `height` (the real threshold co-signature the
    /// committer bundles into the block); both default empty on the
    /// single-operator (non-chain) path.
    Approved {
        /// The proposal.
        id: ProposalId,
        /// The approving member.
        by: MemberId,
        /// The chain height the signature is bound to (0 on the legacy path).
        #[serde(default)]
        height: u64,
        /// The member's Ed25519 signature (hex) over
        /// [`crate::approval_bytes`] at `height` — empty on the legacy path.
        #[serde(default)]
        sig: String,
    },
    /// A pending proposal was declined.
    Declined {
        /// The proposal.
        id: ProposalId,
        /// The declining member.
        by: MemberId,
        /// The sha256 content anchor of the payload the decliner SAW
        /// (the engine's `decline_payload_hash` formula — a cross-node
        /// contract like `molt-chat-legacy-id`; molt-core stays hash-free)
        /// (D1): a receiver registers the voice only against a record whose
        /// payload hashes identically — two proposers minting one id in a
        /// gossip round-trip must not cross votes. "" (older sender) keeps
        /// id-only semantics; skipped empty so the legacy wire shape stays
        /// byte-identical.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        hash: String,
    },
    /// A proposal reached the threshold; its payload joined the surface log.
    Applied {
        /// The proposal.
        id: ProposalId,
    },
    /// Roster presence checkpoint.
    MemberSeen {
        /// The member that was seen.
        member: MemberId,
        /// When (unix seconds).
        ts: u64,
    },
    /// A threshold-committed block was broadcast to the mesh (chain-governed
    /// republics). It rides the log purely as **transport** — the block is
    /// applied into the recipient's `chain.state`, not replayed from the log
    /// (`apply` treats it as a no-op). The committer authors it so the outbox
    /// fans it out; peers verify-and-append it on receipt.
    Committed(ChainBlock),
    /// A member asks the mesh for every persistent-chain block from `from_height`
    /// onward — the catch-up request a lagging or reconnecting node broadcasts.
    /// Any peer that is further ahead re-serves those blocks (as `Committed`) from
    /// its own `chain.state`, so a single survivor with the full chain suffices.
    /// Transport-only, like `Committed` (`apply` is a no-op).
    ChainRequest {
        /// The first height the requester is missing (its `head + 1`).
        from_height: u64,
    },
    /// A membership change (a re-admission or an added seat) was put forward for
    /// threshold approval — the gossip that lets every member sign the SAME
    /// change. Transport-only, like `Committed` (`apply` is a no-op); the
    /// committed result is a `Membership` chain block, not this announcement.
    MembershipProposed {
        /// The proposal id (per-node, matched to its approvals).
        id: ProposalId,
        /// Add a seat, or re-key (restore) an existing one.
        op: chain::MembershipOp,
        /// The affected member handle.
        member: MemberId,
        /// The member's anchored identity pk the change carries.
        identity_pk: String,
        /// A RESTORED seat's new transport anchor (N4b). It must travel with
        /// the proposal: every member signs `approval_bytes` over the SAME
        /// change, so a receiver that rebuilt the change without this field
        /// would sign different bytes and its approval would never count.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nostr_pk: Option<String>,
        /// The seat's relay declaration (R3b) — travels for the same
        /// sign-the-SAME-change reason as `nostr_pk`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        relays: Vec<String>,
        /// The restored seat's own co-signature over
        /// [`chain::restore_consent_bytes`] — travels for the same
        /// sign-the-SAME-change reason: it rides inside the change every
        /// member signs (recovery approval design, 2026-08-08).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consent: Option<String>,
    },
    /// WP4b: a checkpoint cut was put forward — the gossip that lets every
    /// member recompute and co-sign the SAME state hash. Transport-only,
    /// like [`WorkspaceEvent::MembershipProposed`] (`apply` is a no-op);
    /// the committed result is a `Checkpoint` chain block.
    CheckpointProposed {
        /// The proposal id (per-node, matched to its approvals).
        id: ProposalId,
        /// The cut: the checkpoint attests the state AFTER block `upto`.
        upto: u64,
        /// The proposer's canonical state hash — receivers recompute their
        /// own and refuse to sign on mismatch.
        state_hash: String,
    },
    /// WP4b: a pruned holder answers a catch-up request — the checkpoint
    /// blob its suffix anchors on, served BEFORE the anchor + suffix ride
    /// as `Committed` re-serves. Transport-only (`apply` is a no-op); the
    /// receiver hard-verifies via the suffix rules before adopting.
    CheckpointServed {
        /// The full checkpoint state the anchored `state_hash` commits.
        blob: chain::CheckpointState,
    },
    /// A member wants a shared file's BYTES: its fetch request, carried as
    /// **MLS ciphertext** (hex of the group-encrypted
    /// `molt_net::transfer::FetchRequest` JSON — share id, reply-queue
    /// handover, expiry). Transport-only like [`WorkspaceEvent::MeshAnnounced`]
    /// (`apply` is a no-op): the log stores only ciphertext, so the reply
    /// queue's key never enters shared history, and only the SHARER acts on
    /// it (everyone else decrypts and drops). The bytes themselves flow over
    /// the advertised dedicated queue — never through this log.
    FileRequested {
        /// Hex of the requester's MLS-encrypted fetch request.
        ct: String,
    },
    /// A member wants a share's bytes on the RELAY data plane
    /// (`file_transfer_nostr.md`): publishing is lazy, so this asks the
    /// SHARER to publish the chunk series. Rides the (encrypted) group log
    /// — only members read it; no inner sealing needed, unlike the
    /// queue-plane `FileRequested` whose reply-queue handover is secret.
    FileWanted {
        /// The share message's stable id.
        id: MessageId,
    },
    /// The sharer published (or re-affirmed) a share's chunk series at
    /// this stamp — what a fetcher needs to name the series' h-tag window.
    FileServed {
        /// The share message's stable id.
        id: MessageId,
        /// The series' one publish stamp (unix seconds).
        at: u64,
    },
    /// A **raw MLS re-key commit** to broadcast to the group (recovery: after a
    /// coordinator re-keys a returning member's seat, every OTHER member must
    /// apply the SAME commit to advance the epoch, or the group forks). It rides
    /// the log purely as **transport**, but unlike every other wire event it is
    /// NOT MLS-encrypted on the way out — it IS an MLS handshake message, sent
    /// raw so the recipient's `decrypt` merges it (`apply`/replay is a no-op; the
    /// MLS state lives in the group ratchet, not the log). Distinct frame kind
    /// so the mesh carries the commit in-order with chat, on the one channel the
    /// group already shares.
    MlsCommit {
        /// Hex of the MLS commit's wire bytes.
        commit: String,
        /// The carrier stamp the commit was keyed at on Nostr (`0` = none:
        /// the loopback/mesh path carries no stamp). A RESEND must ride the
        /// same stamp, or the receivers' `CommitKey` differs from the
        /// original's and a tiebreak splits (review M8). Additive.
        #[serde(default)]
        stamp: u64,
    },
    /// A member **(re)announced its per-pair mesh queues** — the relay leg of
    /// dynamic mesh membership (`docs_archive/transport/dynamic_mesh.md`): the coordinator
    /// that received a rejoiner's announce over the recovery channel
    /// re-broadcasts the MLS ciphertext **verbatim**, so every survivor
    /// authenticates the announcer by decryption and extends its own mesh
    /// toward it. Transport-only, like `MlsCommit` (`apply` is a no-op — the
    /// mesh lives in `transport.state`, not the log).
    MeshAnnounced {
        /// Hex of the announcer's MLS-encrypted `MeshAnnounce`.
        ct: String,
        /// Optional relay loop-prevention token (mesh self-heal Stage 3): a
        /// self-initiated re-announce carries a random nonce so a hub that
        /// re-broadcasts it can drop copies it has already relayed (the
        /// runtime `seen`-set). Absent on the recovery-relay and
        /// founding-bootstrap announces, which are single-hop — additive, so
        /// those serialize byte-identically as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nonce: Option<u64>,
    },
    /// The proposer pulled a pending proposal back ("pull back"): terminal
    /// on every node, converging like a decline — but no vote is forged
    /// and the verdict renders as "pulled back". Only the PROPOSER'S link
    /// identity may carry it (`by` must equal the authenticated wire
    /// sender AND the receiver's recorded proposer). Additive: an old
    /// reader ignores it and simply keeps the card open.
    Withdrawn {
        /// The proposal.
        id: ProposalId,
        /// The withdrawing proposer.
        by: MemberId,
    },
}

/// The lenient twin of [`EventEnvelope`]: serde fails the whole envelope on
/// an unknown enum variant, so decoding is two-stage — try the typed
/// envelope, on failure fall back to this raw form. Today the raw decode is
/// a validity probe: the frame stays untouched on disk and only a count of
/// unknown events surfaces (which blocks writing). Re-emitting preserved
/// raw frames on compaction is a later concern — compaction does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEnvelope {
    /// Strictly monotonic per workspace.
    pub seq: u64,
    /// Unix seconds.
    pub ts: u64,
    /// Who caused it.
    pub by: MemberId,
    /// The unparsed event body.
    pub body: Value,
}

/// A serializable proposal record (the engine's in-memory `Proposal`, as
/// the snapshot stores it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRecord {
    /// The gated target surface.
    pub surface: Surface,
    /// The proposed transition.
    pub payload: Value,
    /// Approvals collected so far.
    pub approvals: usize,
    /// Lifecycle state.
    pub state: ProposalState,
    /// When it was declined (the `Declined` envelope's ts, unix seconds;
    /// 0 = not declined). Additive — an older snapshot reads as 0.
    #[serde(default)]
    pub declined_at: u64,
    /// Who declined it ("" = not declined). On a threshold rejection this
    /// is the TIPPING decliner (the one whose voice made approval
    /// impossible); the full set is [`Self::decliners`].
    #[serde(default)]
    pub declined_by: MemberId,
    /// Every member who declined so far, in arrival order (deduplicated —
    /// one voice per member). A decline is not a veto: the proposal turns
    /// Rejected only once `decliners.len() > n − m` (approval can no longer
    /// reach the threshold). Additive — an older snapshot reads as empty.
    #[serde(default)]
    pub decliners: Vec<MemberId>,
    /// Every member whose approval THIS holder collected before the seal
    /// (D6): display data only — the block's m signatures stay the chain
    /// truth, but an over-subscribed voter must not read Open on a vote
    /// they cast. Per holder (each node shows the voices that reached it).
    /// Additive, skipped when empty so pre-field dumps stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voted: Vec<MemberId>,
    /// Who proposed it ("" = unknown: a pre-field record, or a WP2
    /// catch-up re-serve — those arrive re-wrapped under the SERVING
    /// peer's name, so this field is an ownership DISPLAY hint (the
    /// "pull back" button's visibility), never an authorization input:
    /// a future withdraw must gate on the authenticated wire sender at
    /// execution time. Additive, and skipped when empty so pre-field
    /// dumps stay byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: MemberId,
    /// The supersede walk's terminal verdict
    /// (`docs_archive/memory/shared_memory_real.md` §4): a pending wiki patch the
    /// moved base can no longer apply — mechanical and UNATTRIBUTED (the
    /// state is `Rejected` for old readers, but no decline vote is
    /// forged: `declined_by` stays empty). Additive, skipped when false
    /// so pre-field dumps stay byte-identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
    /// The proposer pulled the proposal back ("pull back"): terminal like
    /// a rejection but its OWN verdict — no decline vote is forged, and
    /// the card renders "pulled back", never "declined by". Additive,
    /// skipped when false so pre-field dumps stay byte-identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withdrawn: bool,
}

/// Exactly what the engine actor holds for one workspace — the snapshot
/// payload. Replaying the full log from zero produces the same dump as any
/// snapshot plus its tail (the keystone determinism test pins this).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStateDump {
    /// Display name (from the genesis event).
    pub name: String,
    /// The acting member on this node.
    pub member: MemberId,
    /// Approval threshold (m).
    pub rule_m: u8,
    /// The member roster.
    pub roster: Vec<MemberId>,
    /// The anchored name → identity-key table (empty on pre-ritual
    /// workspaces).
    #[serde(default)]
    pub identities: Vec<MemberIdentity>,
    /// The ratified founding charter (free-text agenda), so a snapshot-restored
    /// workspace keeps it (the genesis is before the snapshot and not replayed).
    #[serde(default)]
    pub agenda: String,
    /// The ratified feature set (roster-v5), carried for the same
    /// snapshot-restore reason as `agenda`. `None` = pre-v5 founding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
    /// The neutral, content-derived republic id (the genesis' value). Kept at
    /// runtime so the persistent-chain path can recompute `approval_bytes`
    /// without re-deriving it; a snapshot-restored open keeps it too (the
    /// genesis is before the snapshot and not replayed). Empty on a pre-republic
    /// genesis.
    #[serde(default)]
    pub republic_id: String,
    /// The genesis envelope's timestamp — the founding date `Status` shows;
    /// carried in the snapshot like the other genesis-derived fields (the
    /// genesis is before the snapshot and not replayed). 0 = unknown.
    #[serde(default)]
    pub founded_ts: u64,
    /// The chat log.
    pub chat: Vec<ChatMessage>,
    /// Applied transition log per gated surface (keyed by surface name).
    pub applied: BTreeMap<String, Vec<Value>>,
    /// The proposal id each `applied` entry came from, positionally matched
    /// per surface (the id track of [`SurfaceSnapshot::applied_ids`]).
    /// Additive with a default: a pre-id dump restores with unknown origin
    /// (`None`), payloads untouched.
    #[serde(default)]
    pub applied_ids: BTreeMap<String, Vec<Option<u64>>>,
    /// Every known proposal by id.
    pub proposals: BTreeMap<u64, ProposalRecord>,
    /// The next proposal id to assign.
    pub next_proposal_id: u64,
    /// This node has physically dropped expired chat content (WP4a
    /// compaction). Additive with a default: an un-pruned dump reads `false`
    /// and behaves exactly as before.
    ///
    /// It is load-bearing, not bookkeeping: chat positions shift when a prefix
    /// is dropped, so a **legacy index-addressed** event (pre-chat-bus, no
    /// `id`) can no longer be resolved — honouring it would silently react on
    /// or delete the WRONG surviving message. Once this is set, the engine
    /// ignores id-less chat ops instead. Sticky: it stays true for the life of
    /// the workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chat_pruned: bool,
    /// How many of each sender's chat messages compaction has physically
    /// dropped. Additive with a default (empty = nothing pruned).
    ///
    /// A legacy (pre-chat-bus) message's id is synthesized from its **sender
    /// ordinal** — how many messages from that sender preceded it. Dropping
    /// old messages would restart that count and give this node a DIFFERENT
    /// id for the same message than its peers, so the count of what was
    /// dropped is carried forward and added to the ordinal. That keeps the
    /// cross-node id contract intact across any number of compactions,
    /// including out-of-order arrivals (it is a total, never a position).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub chat_pruned_counts: BTreeMap<MemberId, u64>,
}

impl EngineStateDump {
    /// Physically drop every chat message that aged past `cutoff` (WP4a
    /// §A.1 R3) — the compactor's content trim, returning how many went.
    /// A `ts` of 0 means "unknown age" and is NEVER dropped, matching the
    /// read filter (`State::aged_out_at`), so unknown content cannot silently
    /// vanish.
    ///
    /// Refuses (drops nothing, returns 0) while any message still carries a
    /// **nil id**: legacy ids are synthesized from a per-sender ordinal
    /// counted over the dump, so trimming before that synthesis would give
    /// this node different ids than its peers. A dump taken from live engine
    /// state always has them materialized (both ingest choke points fill them
    /// in), so this only guards a hand-built or pre-chat-bus dump.
    ///
    /// Dropping entries invalidates the legacy numeric `quote` positions of
    /// the survivors, so they are cleared — the resolved `quote_id` (which
    /// both ingest points materialize) is the real reference and stays. A
    /// quote pointing AT a dropped message keeps its id and simply dangles,
    /// exactly like a quote of a deleted message.
    pub fn prune_chat_before(&mut self, cutoff: u64) -> usize {
        if self.chat.iter().any(|m| m.id.is_nil()) {
            return 0;
        }
        let before = self.chat.len();
        let mut counts = std::mem::take(&mut self.chat_pruned_counts);
        self.chat.retain(|m| {
            let keep = !(m.ts != 0 && m.ts < cutoff);
            if !keep {
                // carry the sender ordinal forward, or every legacy id
                // synthesized after this compaction would differ from the
                // peers' (see the field's doc)
                *counts.entry(m.from.clone()).or_insert(0) += 1;
            }
            keep
        });
        self.chat_pruned_counts = counts;
        let dropped = before - self.chat.len();
        if dropped > 0 {
            self.chat_pruned = true;
            for m in &mut self.chat {
                m.quote = None;
            }
        }
        dropped
    }
}

/// A state snapshot at a log position. Snapshots are an *optimization* —
/// deleting them must always be safe (the log holds the truth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// Snapshot schema version.
    pub version: u32,
    /// The log seq this snapshot captures (replay frames `> at_seq`).
    pub at_seq: u64,
    /// The engine state at `at_seq`.
    pub state: EngineStateDump,
}

/// The display preview of an invite link, parsed for the GUI:
/// `molt://invite/<republic>/<m>of<n>/<inviter>/<ticket>` (spaces in the
/// republic name travel as dashes). A real founding link appends a transport
/// handover segment (see `molt_engine::FoundingInvite`) which this parse
/// ignores — only that richer form is actually joinable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteInfo {
    /// Display name of the republic (dashes decoded back to spaces).
    pub republic: String,
    /// The approval threshold (m).
    pub threshold: u8,
    /// The member count (n).
    pub members: u8,
    /// Handle of the member who minted the invite.
    pub inviter: String,
    /// The one-time ticket blob.
    pub ticket: String,
}

impl InviteInfo {
    /// Parse an invite link; `None` if it is not a well-formed molt:// invite.
    pub fn parse(s: &str) -> Option<InviteInfo> {
        let rest = s.trim().strip_prefix("molt://invite/")?;
        let mut parts = rest.split('/');
        let republic = parts.next()?.replace('-', " ");
        let rule = parts.next()?;
        let inviter = parts.next()?.to_string();
        let ticket = parts.next()?.to_string();
        // a real founding link appends a transport-handover segment after the
        // ticket (see molt_engine::FoundingInvite); the preview ignores it
        let (m, n) = rule.split_once("of")?;
        let threshold: u8 = m.parse().ok()?;
        let members: u8 = n.parse().ok()?;
        if republic.trim().is_empty()
            || inviter.is_empty()
            || ticket.len() < 4
            || threshold == 0
            || members < 2
            || threshold > members
        {
            return None;
        }
        Some(InviteInfo {
            republic,
            threshold,
            members,
            inviter,
            ticket,
        })
    }

    /// Render the invite back into its link form.
    pub fn render(&self) -> String {
        format!(
            "molt://invite/{}/{}of{}/{}/{}",
            self.republic.replace(' ', "-"),
            self.threshold,
            self.members,
            self.inviter,
            self.ticket
        )
    }
}

/// Tiny non-crypto PRNG. This is NOT cryptography and must never touch a
/// key, nonce, ticket or secret (a caller once derived tickets from the
/// deleted LCG sibling — the exact misuse this doc exists to stop; use
/// `getrandom` for anything an adversary must not predict). Hidden from
/// rustdoc so the RNG-free contract crate does not advertise an RNG; the
/// call-site allowlist is pinned by `tests/guards.rs`.
#[doc(hidden)]
pub mod mockrand {
    /// One xorshift64 step, advancing the seed in place.
    pub fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }
}


// NOTE: the old `mock_seed` (12 words off a 48-word LCG list) and
// `mock_ticket` (10 base32 chars off an LCG) are gone on purpose: founded
// workspaces derive their key hierarchy from a real OS-CSPRNG seed
// rendered as a BIP-39 phrase (`molt-storage::keys`), and founding-ritual
// tickets are real high-entropy single-use secrets (`molt-net::invite`).

/// The shared core of every engine-run lifecycle (restore / create / join):
/// step, progress, outcome and the live log. `#[serde(flatten)]`
/// keeps the session JSON identical to the previous inline fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCore {
    /// 0 = the input form, 1 = the run view (running or finished).
    pub step: u8,
    /// Progress in percent (0..=100).
    pub progress_pct: u8,
    /// 0 = running, 1 = succeeded, 2 = failed.
    pub outcome: u8,
    /// The live detail log, newest line last.
    pub log: Vec<String>,
    /// The ONE thing that went wrong, in a few words — rendered large and in
    /// the signal colour, above the log.
    ///
    /// A failure the operator must act on cannot live at the end of a long,
    /// small, technical line: people stop reading walls of text within
    /// seconds and then read nothing at all. The log keeps the detail; this
    /// carries the headline. Empty while the run is fine.
    #[serde(default)]
    pub headline: String,
}

impl RunCore {
    /// Whether the run is currently in flight.
    pub fn running(&self) -> bool {
        self.step == 1 && self.outcome == 0
    }

    /// A freshly started run.
    pub fn started() -> RunCore {
        RunCore {
            step: 1,
            ..RunCore::default()
        }
    }
}

/// One row of the founding ritual's member list (transport concept §3.3):
/// an invite that turns into a sealed member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RitualSeatView {
    /// The one-time `molt://invite/…` link (ephemeral — dies with the
    /// ritual).
    pub link: String,
    /// The member's display name; empty until they activated the link.
    pub member: String,
    /// 0 = waiting for activation, 1 = key received (awaiting seal),
    /// 2 = sealed (signature verified), 3 = declined the charter,
    /// 4 = backup confirmed (the seat's `BackupConfirmed` attestation
    /// verified — `seed_backup_confirmation.md` ❻½).
    pub state: u8,
}

/// The founding-ritual lifecycle. Shared session state like the restore:
/// any operator can start it, both watch the same list and live log. The
/// workspace is created only when every seat is sealed (`run.outcome`
/// flips to 1); until then nothing exists on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateState {
    /// The shared run lifecycle (step / outcome / log; the log carries
    /// the ritual's real events, progress is unused).
    #[serde(flatten)]
    pub run: RunCore,
    /// The republic's name (provisional until the founder proposes the final
    /// charter in the deliberation step).
    pub name: String,
    /// The deliberated free-text charter/agenda the founder proposes for the
    /// members to ratify. Empty until proposed.
    #[serde(default)]
    pub agenda: String,
    /// The proposed feature selection (canonicalized optional-surface
    /// keys), set with the charter proposal. Empty until proposed.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether every seat has joined and the founder may now propose the
    /// charter (the deliberation step is unlocked). Set once, UI-driven.
    #[serde(default)]
    pub can_propose: bool,
    /// The founder's handle.
    pub member: String,
    /// The approval threshold (m).
    pub threshold: u8,
    /// The member count (n).
    pub members: u8,
    /// The EFFECTIVE global anonymity network (`"tor" | "none"`) captured
    /// when the ritual opened — a read-only display value, never a choice.
    pub net: String,
    /// The founder's recovery phrase (shown during the ritual, then gone).
    /// NEVER SERIALIZED — see `WorkspaceInfo::seed`.
    #[serde(skip_serializing, default)]
    pub seed: String,
    /// Whether the FOUNDER's own phrase backup is confirmed (the re-typed
    /// phrase matched — `seed_backup_confirmation.md` ❻½). The ritual
    /// finalizes only once every seat AND this flag confirm.
    #[serde(default)]
    pub backup_confirmed: bool,
    /// The ritual's member list: one row per future member.
    pub seats: Vec<RitualSeatView>,
    /// The members are in-process simulations (no real network yet, T3):
    /// the founder's own node auto-activates and signs. The UI shows a
    /// SIMULATION badge so this is never mistaken for real off-band
    /// sharing. `false` once members join over a real transport.
    #[serde(default)]
    pub simulated: bool,
}

/// The join-via-invite lifecycle (real over SMP). Shared session state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinState {
    /// The shared run lifecycle (step / progress / outcome / log).
    #[serde(flatten)]
    pub run: RunCore,
    /// The raw invite link the run was started with.
    pub invite: String,
    /// The joiner's handle.
    pub member: String,
    /// Republic name: parsed from a well-formed invite, otherwise the
    /// fallback (any non-empty invite is accepted for now).
    pub republic: String,
    /// Parsed approval threshold (m); 0 if unparsed.
    pub rule_m: u8,
    /// Parsed member count (n); 0 if unparsed.
    pub rule_n: u8,
    /// Parsed inviter handle (empty if unparsed).
    pub inviter: String,
    /// The joiner's freshly generated recovery phrase, shown once during the
    /// join (its identity + own workspace derive from it). Empty while idle.
    /// NEVER SERIALIZED — see `WorkspaceInfo::seed`.
    #[serde(skip_serializing, default)]
    pub seed: String,
    /// The founder's proposed final DAO name, surfaced when the ritual reaches
    /// the ratification step (empty until then).
    #[serde(default)]
    pub proposed_name: String,
    /// The founder's proposed free-text charter/agenda, for the joiner to read
    /// before ratifying (empty until the ratification step).
    #[serde(default)]
    pub proposed_agenda: String,
    /// The founder's proposed feature selection, shown with the charter at
    /// the ratification step. `None` until the step — and on a proposal
    /// from a pre-v5 founder.
    #[serde(default)]
    pub proposed_features: Option<Vec<String>>,
    /// Whether the join is paused awaiting the joiner's ratification of the
    /// charter above — the wizard shows the charter + a confirm/decline choice,
    /// and the workspace opens only once confirmed.
    #[serde(default)]
    pub awaiting_ratify: bool,
    /// Whether the join is paused awaiting the joiner's own phrase-backup
    /// confirmation (after ratifying — `seed_backup_confirmation.md` ❻½):
    /// the wizard shows the phrase + re-type; the confirmation releases the
    /// signed attestation to the founder, and the ritual waits for everyone.
    #[serde(default)]
    pub awaiting_backup: bool,
    /// The sealed workspace's id, set when the join completes (run outcome 1).
    /// The joiner does NOT enter automatically (2026-08-08): the wizard's
    /// final step shows the recovery phrase and requires re-typing it —
    /// [`Command::JoinFinish`] then enters. Empty until sealed.
    #[serde(default)]
    pub sealed_id: String,
}

/// One seat's line on the rejoiner's recovery checklist
/// (`recovery_auto_approval.md` §4): has this member's voice backed the
/// re-admission yet?
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverSeat {
    /// The seat's member handle.
    pub member: String,
    /// Whether its approval (or the rejoiner's own consent) is counted.
    pub approved: bool,
}

/// The rejoiner's live view of the re-admission vote, fed by the
/// coordinator's progress frames (display data — the Welcome stays the only
/// thing that finishes a rejoin). Reset when a recovery starts; kept after
/// it ends so the finished checklist stays readable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverState {
    /// The returning seat (empty = no recovery reported yet).
    pub member: String,
    /// Approvals needed (the republic's threshold m).
    pub need: u32,
    /// The full roster, each with its approval state, in roster order.
    pub seats: Vec<RecoverSeat>,
}

/// The restore lifecycle (real: download/read → decrypt+stage →
/// chain-verify → materialize). Shared session state: any operator can
/// start it, both watch the same progress and live log — and every line of
/// that log reports something that actually happened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreState {
    /// The shared run lifecycle (step / progress / outcome / log).
    #[serde(flatten)]
    pub run: RunCore,
    /// `"s3" | "file"` (empty while idle). Rejoining via another member is
    /// not a restore way — that is the recovery ritual (`RecoverStart`).
    pub way: String,
    /// The way-specific target (workspace id / object key for `s3`, blob
    /// path for `file`).
    pub target: String,
}

/// The manual-export lifecycle (story 9, `backup_restore_design.md` §3):
/// one export at a time; the result is set ONLY by the off-actor task's
/// real outcome — an "ok" here means the blob is on disk, fsynced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportState {
    /// An export task is in flight.
    #[serde(default)]
    pub running: bool,
    /// The workspace the last/current export addresses (empty = none yet).
    #[serde(default)]
    pub workspace: WorkspaceId,
    /// The resolved destination path.
    #[serde(default)]
    pub dest: String,
    /// `""` while idle/running; `"ok"` after the blob was written and
    /// synced; `"error: …"` with the real reason otherwise.
    #[serde(default)]
    pub result: String,
    /// Total bytes written (only meaningful when `result == "ok"`).
    #[serde(default)]
    pub bytes: u64,
    /// Unknown files in the workspace dir the blob does NOT contain —
    /// named so the user sees what was left out.
    #[serde(default)]
    pub skipped: Vec<String>,
    /// Files written (the wiki export's document count; 0 for the
    /// single-blob workspace export, which writes one file by definition).
    #[serde(default)]
    pub files: u64,
}

/// Transport-health state surfaced on the header "chat" pill (transport
/// concept §4, T4 §P6). The engine sets it from the last dial/resolve
/// outcome; the UI reads `tone` for the pill colour and `reason` for the
/// tooltip. Additive and defaults to [`NetHealth::Ok`], so an older reader
/// meeting a view without it is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tone", rename_all = "snake_case")]
pub enum NetHealth {
    /// Transport is nominal (or unconfigured/clearnet) — green pill.
    #[default]
    Ok,
    /// Reachable but impaired (e.g. a Tor circuit timed out, retrying) — amber
    /// pill with the reason as tooltip.
    Degraded {
        /// Human-readable reason for the degraded state.
        reason: String,
    },
    /// Transport is down / fail-closed (e.g. Tor misconfigured, no dial
    /// attempted) — red pill with the reason as tooltip.
    Down {
        /// Human-readable reason for the down state.
        reason: String,
    },
}

/// The rung of the evidence ladder a Tor connectivity probe
/// ([`Command::NetTestTor`]) actually reached.
///
/// The whole point of this type is that a green light must never claim more
/// than was proven. Reaching the SOCKS address proves a socket is listening
/// there — it does **not** prove a working Tor circuit; only a real dial
/// THROUGH Tor to a relay the operator confirmed does. Each variant means
/// exactly one thing, and the surfaces are expected to word them that way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TorTestState {
    /// Never probed (or the verdict was invalidated by a settings change).
    #[default]
    Idle,
    /// A probe is running right now.
    Testing,
    /// **Refusal, not a verdict**: the anonymity network is not Tor, so
    /// there is nothing to test and no packet was sent.
    Off,
    /// **Nothing was probed**: Tor is selected but the fail-closed dialer
    /// refused to resolve the configuration (unknown mode, `embedded`
    /// without that build, …). A config failure, not a network result.
    Misconfigured,
    /// **No daemon**: nothing is listening at the configured SOCKS address.
    NoProxy,
    /// **Partial**: a socket answered at the SOCKS address, but nothing was
    /// routed through it — no relay was dialable, so no circuit is proven.
    ProxyOnly,
    /// **Nothing testable**: this Tor mode has no SOCKS address to probe
    /// (the embedded in-process client) and there was no relay to dial, so
    /// no rung could be reached at all.
    NoTarget,
    /// **Not working**: the proxy answered, but the dial to the relay
    /// through it failed — there is no usable circuit to that relay.
    CircuitFailed,
    /// The dial through Tor hit the deadline. NOT the same as a refusal: a
    /// first embedded-Tor start bootstraps the directory and legitimately
    /// takes minutes, so this says "no answer yet", never "not working"
    /// (review finding 2026-07-31).
    CircuitTimeout,
    /// **Working**: a relay from the operator's own confirmed pool was
    /// reached END TO END through Tor. The only state that means "Tor works".
    Circuit,
}

impl TorTestState {
    /// The stable wire/UI key (identical to the serde tag).
    pub fn as_str(self) -> &'static str {
        match self {
            TorTestState::Idle => "idle",
            TorTestState::Testing => "testing",
            TorTestState::Off => "off",
            TorTestState::Misconfigured => "misconfigured",
            TorTestState::NoProxy => "no_proxy",
            TorTestState::ProxyOnly => "proxy_only",
            TorTestState::NoTarget => "no_target",
            TorTestState::CircuitFailed => "circuit_failed",
            TorTestState::CircuitTimeout => "circuit_timeout",
            TorTestState::Circuit => "circuit",
        }
    }

}

/// The outcome of a Tor connectivity probe — the rung that was reached plus
/// the concrete, never-invented facts behind it.
///
/// `detail` is technical text (an error message, a reason); the surfaces own
/// the human copy for [`Self::state`] and show `detail` as the specifics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorTest {
    /// Which rung of the ladder was reached.
    #[serde(default)]
    pub state: TorTestState,
    /// The concrete reason/specifics behind the state (technical, English;
    /// empty when there is nothing to add). Never invented.
    #[serde(default)]
    pub detail: String,
    /// The SOCKS address that was (or would be) probed; empty when there is
    /// none (Tor off, misconfigured, or the embedded in-process client).
    #[serde(default)]
    pub proxy: String,
    /// The relay URL that was dialed through Tor; empty when none was
    /// dialable — the probe never picks a host of its own.
    #[serde(default)]
    pub target: String,
    /// How long the successful circuit dial took, in milliseconds. Only
    /// meaningful for [`TorTestState::Circuit`]; `0` otherwise.
    #[serde(default)]
    pub ms: u32,
}

/// The whole shared app/session state: which screen, which language, the last
/// wizard outcome, a transient notice (e.g. the settings-save toast) and the
/// settings. Both operators read and mutate this through the command set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    /// The screen currently shown.
    pub screen: Screen,
    /// The surface selected in the main view (shared, like the screen).
    pub surface: Surface,
    /// The selected surface's active sub-view (a key from
    /// [`Surface::views`]; selecting a surface resets it to the default).
    pub view: String,
    /// Active GUI language (`"en" | "de"`).
    pub language: String,
    /// Active GUI theme (`"classic" | "dark" | "brutalism"`).
    pub theme: String,
    /// A transient notice key for the GUI (e.g. `"saved"`); cleared on navigate.
    pub notice: String,
    /// Transient result of the backup panel's "Test connection" against the
    /// S3 endpoint: `""` (untested), `"testing"`, `"ok"`, or `"error: …"`.
    /// Never persisted; lives here (not in [`SessionSettings`]) so a test in
    /// flight does not look like an unsaved settings edit.
    #[serde(default)]
    pub s3_test: String,
    /// The same verdict for the MEDIA target's probe, in its own slot so a
    /// test of one bucket never overwrites the other's result.
    #[serde(default)]
    pub s3_media_test: String,
    /// Transient state of the last bucket listing ([`Command::NetListBackups`]),
    /// same rationale as [`Self::s3_test`]: `""` (never listed), `"listing"`,
    /// `"ok"`, or `"error: …"` (the honest failure class — including "no
    /// endpoint configured" when the backup target is not set up).
    #[serde(default)]
    pub s3_list: String,
    /// Transient result of the anonymity panel's "Test Tor" probe
    /// ([`Command::NetTestTor`]), same rationale as [`Self::s3_test`]: never
    /// persisted, and cleared whenever the anonymity settings change (the
    /// verdict describes ONE configuration). See [`TorTest`].
    #[serde(default)]
    pub tor_test: TorTest,
    /// Config keys (file names, e.g. `"mcp.port"`) whose current value
    /// differs from what the node booted with and which only take effect on
    /// restart. Set by the engine on every save/reload; NOT transient — it
    /// stays until the values return to the boot state or the node restarts.
    /// The GUI renders it as a persistent "restart required" warning.
    #[serde(default)]
    pub restart_required: Vec<String>,
    /// The OPEN workspace's transport kind: `"nostr"` for a relay republic,
    /// `""` otherwise. What the GUI keys transport-specific affordances on
    /// (§10.7 file gating, §6.5 coarse presence) — the engine enforces the
    /// same rules co-equally on its command surface.
    #[serde(default)]
    pub transport: String,
    /// The editable settings.
    pub settings: SessionSettings,
    /// The Nostr relay pool with its DERIVED state (kind, why a relay is or
    /// is not dialed), in priority order — see `docs_archive/transport/relay_pool.md`.
    /// Empty on a fresh install: nothing is pre-trusted, so the node connects
    /// to no relay until its operator adds and confirms one.
    #[serde(default)]
    pub relays: Vec<crate::relay::RelayStatus>,
    /// Whether dialing confirmed clearnet/local relays is switched on. A
    /// persisted decision since the ADR-0004 amendment: confirming such a
    /// relay with the acknowledgement switches it on, `RelayClearnetSession`
    /// switches it off, and both survive a restart.
    #[serde(default)]
    pub clearnet_session: bool,
    /// The locally known workspaces, from the real on-disk directory scan.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Backups in the S3 bucket without a local workspace, from the last
    /// real listing ([`Command::NetListBackups`]). Empty until a listing
    /// ran — the table never shows invented bucket contents.
    #[serde(default)]
    pub backup_orphans: Vec<BackupOrphan>,
    /// Id of the currently opened workspace (empty = none). The display
    /// name lives in the matching [`WorkspaceInfo`] entry.
    pub active_workspace: WorkspaceId,
    /// The restore lifecycle (real; see [`RestoreState`]).
    pub restore: RestoreState,
    /// The rejoiner's recovery-vote checklist (see [`RecoverState`]).
    /// Additive; empty outside a recovery.
    #[serde(default)]
    pub recover: RecoverState,
    /// The manual-export lifecycle (real; additive).
    #[serde(default)]
    pub export: ExportState,
    /// The wiki-export lifecycle (real; additive). Its OWN slot: a wiki
    /// export and a workspace export must never overwrite each other's
    /// outcome.
    #[serde(default)]
    pub wiki_export: ExportState,
    /// The founding lifecycle (real over SMP).
    pub create: CreateState,
    /// The join-via-invite lifecycle (real over SMP).
    pub join: JoinState,
    /// Transport-health state for the header pill (set by the engine from the
    /// last dial/resolve outcome). Additive; defaults to [`NetHealth::Ok`].
    #[serde(default)]
    pub net_health: NetHealth,
}

impl Default for SessionView {
    fn default() -> Self {
        SessionView {
            screen: Screen::Choice,
            surface: Surface::Chat,
            view: Surface::Chat.default_view().to_string(),
            language: "en".to_string(),
            theme: "classic".to_string(),
            notice: String::new(),
            s3_test: String::new(),
            s3_media_test: String::new(),
            s3_list: String::new(),
            tor_test: TorTest::default(),
            restart_required: Vec::new(),
            transport: String::new(),
            settings: SessionSettings::default(),
            relays: Vec::new(),
            clearnet_session: false,
            // EMPTY: every production constructor lists what is on disk;
            // the demo republics are a test fixture (`demo_set`), and a
            // default that shipped them hid the one real workspace (K6)
            workspaces: Vec::new(),
            backup_orphans: Vec::new(),
            active_workspace: String::new(),
            restore: RestoreState::default(),
            recover: RecoverState::default(),
            export: ExportState::default(),
            wiki_export: ExportState::default(),
            create: CreateState::default(),
            join: JoinState::default(),
            net_health: NetHealth::default(),
        }
    }
}

/// Identifier for a pending or applied proposal, assigned by the engine in
/// delivery order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProposalId(pub u64);

/// The one command set. This is the single source of truth for "what the
/// software can do"; the MCP tools and the GUI buttons are both thin shells
/// that construct these.
#[derive(Debug, Clone, Serialize, Deserialize, strum::VariantNames)]
#[serde(tag = "cmd", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Command {
    /// Post to the ungated chat surface.
    Chat {
        /// The message body.
        body: String,
        /// Quoted message (by stable id), if replying.
        #[serde(default)]
        quote: Option<MessageId>,
        /// The channel this message files under (`Group` when omitted).
        #[serde(default)]
        channel: ChannelRef,
    },
    /// An authenticated peer event arrived over the transport. Sent by the
    /// node's own `molt-net` supervisor (engine-internal, like the run
    /// tickers — never an MCP tool: a network peer must not be
    /// impersonatable through the MCP surface).
    NetDelivered {
        /// The peer whose queue delivered it (the transport's link
        /// identity; must match the envelope's `by`).
        from: MemberId,
        /// The peer's original envelope (their seq/ts stamps); the engine
        /// re-stamps it into the local log in arrival order.
        envelope: EventEnvelope,
        /// Which mesh incarnation sent this (`None` = the engine-lifetime
        /// transport). The engine drops commands from a torn-down mesh —
        /// a delivery already queued behind a workspace switch must not
        /// land in the new context's (possibly persisted!) log.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Passive presence: authenticated inbound traffic from a member was
    /// observed (engine-internal; no beacons, ever — concept §3.4).
    NetPeerSeen {
        /// The member that was heard from.
        member: MemberId,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A merged MLS re-key commit re-admitted a member as a NEW incarnation
    /// whose log seq space restarts (engine-internal, from the transport's
    /// ordered inbound path). The engine forgets its accept window for the
    /// member — BEFORE any of the new incarnation's envelopes arrive, which
    /// the ordering of the delivery path guarantees (live incident
    /// 2026-08-09 §2: a bystander otherwise swallows colliding seqs as the
    /// lost incarnation's duplicates and falsely acks them).
    NetPeerRekeyed {
        /// The re-admitted member.
        member: MemberId,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Periodic presence aging (engine-internal ticker): re-derive the
    /// member pills' 0/1/2 state from their real last-seen stamps so a
    /// silent member ages online → stale → offline without any traffic.
    NetPresenceTick,
    /// The delivery-guarantee beat (engine-internal 1 s ticker,
    /// `docs_archive/transport/delivery_guarantee.md` §4.3/§4.6): flush due delivery
    /// ACKs and run the debounced accept-window / live-ratchet persists.
    /// Fast on purpose — riding the 30 s presence tick alone made the
    /// "3 s" ACK debounce a 33 s latency, losing the race against the
    /// sender's 30 s resend timer (E7 review).
    NetDeliveryTick,
    /// Sending to a member's queue keeps failing; the outbox is backing
    /// off and retrying (engine-internal transport health).
    NetSendFailed {
        /// The unreachable member.
        member: MemberId,
        /// The transport's reason, for the log/pills.
        reason: String,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's inbound subscription is live (engine-internal transport
    /// health; the resubscribe watchdog confirms the leg, clearing its
    /// degraded state).
    NetLinkUp {
        /// The member whose leg came up.
        member: MemberId,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's inbound subscription ended or failed; the watchdog is
    /// backing off and re-subscribing (engine-internal transport health —
    /// surfaces as `NetHealth::Degraded`).
    NetLinkDown {
        /// The member whose leg died.
        member: MemberId,
        /// The transport's reason, for the health tooltip.
        reason: String,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A previously backing-off send to a member went through again
    /// (engine-internal transport health; clears the stuck-send state).
    NetSendOk {
        /// The member whose sends work again.
        member: MemberId,
        /// Mesh incarnation (see [`Command::NetDelivered`]).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Put an object forward for threshold approval on a gated surface.
    Propose {
        /// Target surface (must be gated; `Chat` is rejected).
        surface: Surface,
        /// Surface-specific transition, e.g. `{"op":"add_note","title":"…"}`.
        payload: Value,
    },
    /// Contribute one member's approval toward a pending proposal.
    Approve {
        /// The proposal to approve.
        proposal: ProposalId,
    },
    /// Decline a pending proposal.
    Decline {
        /// The proposal to decline.
        proposal: ProposalId,
    },
    /// Pull an OWN pending proposal back (proposer only): it turns
    /// terminal on every node without forging any vote.
    Withdraw {
        /// The proposal to pull back.
        proposal: ProposalId,
    },
    /// Delete a chat message: its text is wiped for everyone and replaced
    /// by a deletion notice naming who deleted it (reactions are dropped).
    DeleteChat {
        /// The message's stable id.
        id: MessageId,
    },
    /// Toggle the local member's emoji reaction on a chat message: reacting
    /// with the emoji you already picked un-reacts, picking another emoji
    /// switches — one reaction per member per message.
    ReactChat {
        /// The message's stable id.
        id: MessageId,
        /// The reaction emoji.
        emoji: String,
    },
    /// Confirm the local member has read these chat messages (read
    /// receipts). Co-equal: the GUI issues it when a channel is opened; an
    /// MCP agent may issue it explicitly. The engine binds the receipt to
    /// the local member, filters to messages not authored by them and not
    /// already read (so a repeat is a no-op), and — only when read receipts
    /// are enabled locally — records and broadcasts a batched
    /// [`WorkspaceEvent::ChatRead`]. Unknown or own-authored ids are ignored.
    MarkRead {
        /// The messages the local member has read.
        ids: Vec<MessageId>,
    },
    /// Share a local file into the ungated chat. The engine derives the
    /// metadata (name, size, date, kind) and streams the real sha256 off
    /// the actor; the share message posts when hashing completes. Only
    /// METADATA enters the chat — the path stays this node's business
    /// (prefs, never wire/log), and the bytes move per-download over a
    /// dedicated encrypted queue. A share IS a chat message, so it files
    /// under a channel view like any other (concept Q8).
    ShareFile {
        /// Absolute path of the local file to share.
        path: String,
        /// The channel view the share files under. `Command` is never
        /// persisted, so the field is a clean swap (no serde default) —
        /// every construction site states its channel.
        channel: ChannelRef,
    },
    /// [`Command::ShareFile`] for an MCP client: `name` is a bare file
    /// name inside the node's download directory — the EXCHANGE FOLDER an
    /// agent may read from and write into. `ShareFile` itself (any path
    /// the node's user can read) is the GUI's file dialog only: an agent
    /// that could name any path would publish the operator's private files
    /// to the whole republic (MCP audit 2026-08-26 H3).
    ShareFileFromExchange {
        /// A bare file name (no separators) in the download directory.
        name: String,
        /// The channel view the share files under.
        channel: ChannelRef,
    },
    /// Download a shared file: fetch the bytes peer-to-peer from the
    /// sharer's device (async kickoff — progress and the result arrive as
    /// `Event::FileTransfer` / `read_uploads`). Fails once the sharer
    /// deleted the local file; an offline sharer times out honestly.
    DownloadFile {
        /// The share message's stable id.
        id: MessageId,
        /// Destination: an existing directory (the file lands inside it,
        /// name collisions resolve as "name (1).ext") or a full target
        /// path. Defaults to the session's download directory.
        #[serde(default)]
        dest: Option<String>,
    },
    /// Sharer-only: the local file is gone (deleted from this disk) — the
    /// share becomes permanently unavailable for every participant.
    RemoveFile {
        /// The share message's stable id.
        id: MessageId,
    },
    /// B2 — advance the seat's OWN read cursor for one channel. Private to
    /// the seat (persisted in `prefs.toml`, never on the wire) — the shared
    /// read RECEIPTS ([`Command::MarkRead`]) are a different mechanism and
    /// stay as they are. A tool on both surfaces: the agent driving this
    /// seat reads too.
    MarkChannelRead {
        /// The channel whose cursor advances.
        #[serde(default, skip_serializing_if = "ChannelRef::is_group")]
        channel: ChannelRef,
        /// Read THROUGH this message id (hex). Empty = through the
        /// channel's newest visible message.
        #[serde(default)]
        up_to: String,
    },
    /// Read the projected state of one surface.
    ReadState {
        /// Which surface to read.
        surface: Surface,
        /// Chat only: return just the messages of this channel (`None` =
        /// the whole log). The snapshot's channel enumeration always lists
        /// every channel, filtered or not.
        #[serde(default)]
        channel: Option<ChannelRef>,
        /// Chat only: a [`Surface::views`] key (`"today"` = the General
        /// view; `"archive"` was a design mock and is refused until a real
        /// archive exists); `None` is the whole window (the additive
        /// default — older readers keep today's behavior). An unknown key
        /// is an error; other surfaces validate but ignore it. Orthogonal to
        /// `channel` — the two filters compose.
        #[serde(default)]
        view: Option<String>,
    },
    /// List every proposal the engine currently knows about.
    ListProposals,
    /// WP4b: put a chain CHECKPOINT forward for threshold approval — the
    /// compaction cut at the CURRENT head (`upto` = head height, B-F1 in
    /// `docs_archive/chain/log_compaction.md`). The engine computes the canonical
    /// state hash itself; every receiver recomputes it from its own chain
    /// before co-signing (sign-what-you-see), and the block seals at m.
    ProposeCheckpoint,
    /// Read a one-shot status summary of the group and surfaces.
    Status,
    /// Read the member table of the open workspace (Organization → Members):
    /// one [`MemberView`] per roster member.
    ReadMembers,
    /// Read every file shared into the chat (Shared Files → Uploads): one
    /// [`UploadView`] per share, newest last (log order).
    ReadUploads,
    /// Read who mirrors what (`docs_archive/files/mirroring.md` §3.4): this seat's
    /// switch and quota, every member's declaration, per share the holders.
    ReadMirror,
    /// Read the persistent chain as display data (the Chain-History view):
    /// one [`ChainBlockView`] per committed block of the open republic,
    /// newest first — checkpoint blocks included. A pruned holder appends
    /// summarized pre-cut entries rebuilt from its checkpoint blob.
    ReadChain,
    /// The GUI publishes what its window currently shows
    /// (`docs_archive/ui/gui_over_mcp.md`) — the read half of driving the GUI
    /// from MCP. ENGINE-INTERNAL in spirit: only the window may speak it
    /// (an agent must not forge what the window claims to show).
    UiPublish {
        /// The window's own rendering claim.
        snapshot: UiSnapshot,
    },
    /// Read the GUI's last published [`UiSnapshot`] (empty when no window
    /// runs, or none was published yet).
    ReadUiState,
    /// Request one GUI interaction (`docs_archive/ui/gui_over_mcp.md`): stored by
    /// the engine, announced as [`Event::UiActionRequested`], performed by
    /// the window's live mirror — which then publishes again, so the
    /// caller reads the effect back with [`Command::ReadUiState`].
    UiAction {
        /// The requested interaction.
        action: UiAction,
    },

    // --- session / app-level commands (co-equal with the GUI) ---
    /// Read the whole shared session state (screen, language, settings, …).
    ReadSession,
    /// Move the node to a different top-level screen.
    Navigate {
        /// The screen to show.
        screen: Screen,
    },
    /// Select the surface shown in the main view (shared, like the screen);
    /// its sub-view resets to the surface's default.
    SelectSurface {
        /// The surface to show.
        surface: Surface,
    },
    /// Select a surface *and* one of its sub-views (shared, like the screen).
    SelectView {
        /// The surface to show.
        surface: Surface,
        /// The sub-view key (validated against [`Surface::views`]).
        view: String,
    },
    /// Change the active GUI language (`"en" | "de"`).
    SetLanguage {
        /// The new language code.
        lang: String,
    },
    /// Change the active GUI theme (`"classic" | "dark" | "brutalism"`).
    SetTheme {
        /// The new theme name.
        theme: String,
    },
    /// Set the three GUI font sizes in px (app chrome / wiki navigator /
    /// editor+document) — a local preference, persisted to `config.toml`.
    SetFonts {
        /// App-chrome font size in px.
        app: u16,
        /// Wiki-navigator font size in px.
        nav: u16,
        /// Editor/document font size in px.
        editor: u16,
    },
    /// Turn this node's chat read receipts on or off (a local per-node
    /// preference, persisted to `config.toml` — never governance-gated, never
    /// on the wire). Symmetric: while off, the node sends no receipts of its
    /// own AND hides others' receipts from its chat view.
    SetReadReceipts {
        /// New read-receipts state.
        enabled: bool,
    },
    /// Add a relay to the pool — validated and normalized, appended at the
    /// LOWEST priority, and **unconfirmed**: adding never connects. See
    /// `docs_archive/transport/relay_pool.md`.
    RelayAdd {
        /// `wss://…` (or `ws://…` for a `.onion` host).
        url: String,
    },
    /// Remove a relay from the pool entirely.
    RelayRemove {
        /// The relay URL (any spelling that normalizes to a pool entry).
        url: String,
    },
    /// Move a relay one position up or down — the pool order IS the dial
    /// priority.
    RelayMove {
        /// The relay to move.
        url: String,
        /// `true` = towards position 0 (higher priority).
        up: bool,
    },
    /// Confirm a relay: the user's persisted "yes, use this one". A CLEARNET
    /// relay is refused unless `accept_clearnet` is set — the acknowledgement
    /// is enforced HERE, so an MCP agent faces the same gate as a human
    /// clicking through the GUI's warning.
    RelayConfirm {
        /// The relay to confirm.
        url: String,
        /// Explicit acknowledgement of the clearnet exposure (the relay
        /// operator sees this node's subscriptions, and its IP unless Tor is
        /// on). Ignored for onion relays.
        accept_clearnet: bool,
    },
    /// Withdraw a relay's confirmation — it stays in the pool but is no
    /// longer dialed.
    RelayRevoke {
        /// The relay to un-confirm.
        url: String,
    },
    /// B4: probe a relay — does it accept kind 445, can its auth demand be
    /// satisfied, does it retain events, does its frame cap fit the group?
    /// One verdict, one reason, on the notice channel. A human decision
    /// aid, so it is a TOOL on both surfaces; `RelayConfirm` runs the same
    /// probe implicitly and confirms only a usable relay.
    RelayProbe {
        /// The relay URL to vet.
        url: String,
    },
    /// The off-actor probe task reporting its verdict (INTERNAL — an agent
    /// must not be able to forge one and thereby confirm a dead relay).
    NetRelayProbed {
        /// The probed relay.
        url: String,
        /// The single reason; empty = usable.
        #[serde(default)]
        error: String,
        /// `true` = the relay could not be REACHED (no judgement — down, or
        /// onion while Tor is off); `false` with a non-empty `error` = it
        /// answered and disqualified itself. An unreachable relay may still
        /// be confirmed (the operator consented; it is honestly reported
        /// unverified); an unusable one never is.
        #[serde(default)]
        unreachable: bool,
        /// Whether this verdict completes a `RelayConfirm` (true) or a bare
        /// `RelayProbe` (false).
        #[serde(default)]
        confirm: bool,
    },
    /// Turn dialing of NON-onion relays (clearnet, LAN, loopback) on or off.
    /// **Persisted** since the ADR-0004 amendment (2026-07-31) — both the on
    /// and the off decision — so an operator states it once instead of after
    /// every restart. Confirming such a relay with its exposure
    /// acknowledgement already turns it on; this is the explicit switch (and
    /// the way to go dark again). Onion relays are unaffected.
    RelayClearnetSession {
        /// `true` = allow dialing confirmed clearnet/local relays, `false` =
        /// go dark; persisted across restarts (ADR-0004 amendment).
        unlock: bool,
    },
    /// Change SOME settings, keeping every field the payload does not
    /// mention.
    ///
    /// The reason this exists as its own verb: [`Command::SaveSettings`]
    /// takes a whole [`SessionSettings`], so a caller sending three fields
    /// necessarily sends defaults for the rest — and the defaults are not
    /// neutral. `anonymity` defaults to `"none"`, so adjusting a backup
    /// interval could take a Tor node onto clearnet; `mcp_token` defaults to
    /// empty, which disables authentication. A partial update has to be a
    /// different verb from a full replace, or every partial update is a
    /// silent reset.
    ///
    /// Unknown keys are refused rather than ignored: a typo'd `anonimity`
    /// that quietly changed nothing would be indistinguishable from a
    /// setting that did not take.
    PatchSettings {
        /// A JSON object of the settings to change, keyed exactly as
        /// [`SessionSettings`] serializes. `relays` and
        /// `clearnet_relays_enabled` are refused here for the same reason
        /// `SaveSettings` re-merges them: the relay pool has one door.
        patch: serde_json::Value,
    },
    /// Store the settings into the session and persist them to the node's
    /// `config.toml` (format-preserving, atomic). The reply does not wait for
    /// the disk; the write outcome lands in the session notice ("saved" /
    /// "save-failed: …") via [`Command::ConfigNotice`].
    ///
    /// This REPLACES every field. To change one, use
    /// [`Command::PatchSettings`] — a partial payload here would default the
    /// rest, which once meant an agent could turn Tor off by adjusting a
    /// backup interval.
    SaveSettings {
        /// The settings to store.
        settings: SessionSettings,
    },
    /// Mirror externally edited `config.toml` values into the shared session.
    /// Sent by the engine's own config watcher when the file changes on disk
    /// (engine-internal, like the run tickers — not an MCP tool: agents that
    /// want a reload edit via `save_settings`).
    ReloadSettings {
        /// The settings read from the file.
        settings: SessionSettings,
        /// GUI language from `[ui].lang`.
        language: String,
        /// GUI theme from `[ui].theme`.
        theme: String,
    },
    /// Report a config-persistence outcome into the session notice ("saved",
    /// "save-failed: …", "config-conflict"). Sent by the engine's own config
    /// store task after an asynchronous write or a rejected external edit
    /// (engine-internal, not an MCP tool).
    ConfigNotice {
        /// The notice key (plus optional detail) to show.
        notice: String,
    },
    // --- workspaces & restore (shared, co-equal) ---
    /// Open a locally known workspace: its state is loaded from disk
    /// (snapshot + event-log tail), it becomes active and the node moves
    /// to the main screen.
    OpenWorkspace {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
    },
    /// Close the active workspace (flush + closing snapshot, release the
    /// lock) and return to the choice screen.
    CloseWorkspace,
    /// Forget a locally known workspace: its directory moves to the
    /// recoverable `.trash` (entries older than 30 days are purged at
    /// startup) and the list entry disappears.
    DeleteWorkspace {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
    },
    /// Switch automatic S3 backup on or off for one workspace; persisted in
    /// the workspace's local `prefs.toml`. Enabling only persists the pref
    /// — the next [`Command::BackupTick`] pass runs the real first upload,
    /// and the last-backup stamp moves ONLY on a confirmed upload
    /// ([`Command::NetBackupDone`]); enabling never invents one.
    SetWorkspaceBackup {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// New auto-backup state.
        enabled: bool,
    },
    /// Seal a workspace at rest under its recovery phrase (S6): the phrase
    /// is verified against the encrypted genesis first (proof the caller
    /// holds the credential), then the device-sealed key material is
    /// removed from disk — the phrase becomes the only way back in. The
    /// workspace becomes inactive; opening requires
    /// [`Command::DecryptWorkspace`] first. The ACTIVE workspace cannot be
    /// encrypted from under itself. Durable: the state is derived from the
    /// directory and survives restarts.
    EncryptWorkspace {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// The workspace's recovery phrase (verified before any deletion).
        #[serde(default)]
        phrase: String,
    },
    /// Decrypt an at-rest-sealed workspace so it can be opened again. The
    /// phrase is REALLY verified (BIP-39 checksum, then an authenticated
    /// decrypt of the genesis frame with the derived key); a wrong phrase
    /// is a hard error and changes nothing on disk. On success the key
    /// material is re-sealed under the local device key.
    DecryptWorkspace {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// The workspace's recovery phrase.
        phrase: String,
    },
    /// Export a workspace as ONE encrypted `.molt.enc` blob file
    /// (`molt-export-v1`): manifest, encrypted history, the threshold-signed
    /// chain, the newest snapshot, the logo — and, when stored, the recovery
    /// seed (blob + passphrase then carries full seat capability, like the
    /// phrase itself). Live MLS/transport state is NEVER exported: the blob
    /// restores knowledge, the recovery ritual restores membership. Runs off
    /// the actor; the honest outcome lands in [`SessionView::export`].
    ExportWorkspace {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// Target file path (`~` is expanded; parents are created; an
        /// existing file is atomically replaced).
        dest: String,
        /// The export passphrase (Argon2id-stretched; minimum 10
        /// characters, engine-enforced).
        passphrase: String,
    },
    /// [`Command::ExportWorkspace`] for an MCP client: a KNOWLEDGE ARCHIVE
    /// — the history, chain and snapshot, but never the recovery seed, and
    /// marked phrase-sealed so an import needs the phrase to open it —
    /// written as `name` into the download directory (the exchange
    /// folder). The seed-carrying export to any path is the GUI's only:
    /// blob + passphrase would otherwise replace the phrase, which leaves
    /// the process on no surface (MCP audit 2026-08-26 H1).
    ExportWorkspaceArchive {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// A bare file name (no separators) in the download directory.
        name: String,
        /// The export passphrase (minimum 10 characters, engine-enforced).
        passphrase: String,
    },
    /// The export task confirmed the blob on disk (engine-internal, from
    /// the off-actor export task — an MCP agent must not be able to forge
    /// an export success).
    NetExportDone {
        /// The exported workspace.
        id: WorkspaceId,
        /// The written file.
        dest: String,
        /// Total bytes written.
        bytes: u64,
        /// Unknown files the blob does not contain (honesty).
        skipped: Vec<String>,
    },
    /// The export task failed (engine-internal); the real reason is
    /// surfaced verbatim — never a fake success.
    NetExportFailed {
        /// The workspace whose export failed.
        id: WorkspaceId,
        /// The failure, honestly.
        error: String,
    },
    /// Write the Shared-Memory wiki to a directory on disk as plain files
    /// (`docs_archive/memory/wiki_export_plan.md`): `<dest>/wiki/<path>` per folded
    /// document, and with `proof` the verification bundle
    /// (`<dest>/proof/bundle.json` + `README.md`) that lets an OUTSIDER check
    /// the tree is the fold of patches a real m-of-n of the sealed roster
    /// approved. The bundle carries the genesis, every membership block and
    /// every applied wiki patch — nothing else, and no local drafts. It
    /// necessarily reveals member names, identity and transport anchors, the
    /// relay pool, the charter and each patch's signer set. Refused on a
    /// workspace without chain governance (there are no signatures to prove
    /// anything with — a files-only export stays available). Runs off the
    /// actor; the honest outcome lands in [`SessionView::wiki_export`].
    WikiExport {
        /// Target directory (`~` is expanded; parents are created; existing
        /// files of the same name are overwritten).
        dest: String,
        /// Include the verification bundle.
        proof: bool,
    },
    /// [`Command::WikiExport`] for an MCP client: into `name`, a bare
    /// directory name inside the download directory — never an arbitrary
    /// path, which the tree would be scattered into and same-named files
    /// overwritten at (MCP audit 2026-08-26 M3).
    WikiExportArchive {
        /// A bare directory name (no separators) in the download directory.
        name: String,
        /// Include the verification bundle.
        proof: bool,
    },
    /// The wiki export task finished the write (engine-internal, from the
    /// off-actor task — an MCP agent must not be able to forge an export
    /// success).
    NetWikiExportDone {
        /// The directory written.
        dest: String,
        /// Wiki documents written.
        files: u64,
        /// Total bytes written, the proof bundle included.
        bytes: u64,
    },
    /// The wiki export task failed (engine-internal); the real reason is
    /// surfaced verbatim — never a fake success.
    NetWikiExportFailed {
        /// The failure, honestly.
        error: String,
    },
    /// Begin a REAL restore from a `molt-export-v1` backup blob
    /// (`backup_restore_design.md` §4/§6.6): the blob is read (from a file,
    /// or downloaded from the configured S3 bucket), decrypted and staged
    /// off the actor, then the engine **hard-verifies the threshold-signed
    /// chain before anything materializes** — a blob whose chain does not
    /// verify restores nothing. Progress and log lines report only what
    /// actually happened. The restored workspace opens *detached* (§4.4):
    /// knowledge is restored, membership is not — rejoining the live
    /// republic is the recovery ritual ([`Command::RecoverStart`]).
    RestoreStart {
        /// `"s3" | "file"` (rejoining via another member is [`Command::RecoverStart`]).
        way: String,
        /// The way-specific target: for `file` the `.molt.enc` path; for
        /// `s3` the workspace-id pseudonym from the backup table (the
        /// NEWEST object is used) or a full `molt/<id>/<ts>.molt.enc`
        /// object key.
        target: String,
        /// The secret unlocking the blob — its meaning follows the blob's
        /// own key mode: the RECOVERY PHRASE for automatic S3 backups
        /// (`workspace` mode), the export passphrase for manual file
        /// exports (`passphrase` mode). Additive.
        #[serde(default)]
        secret: String,
        /// Same-id collision policy (design P2): `false` (default) refuses
        /// when a workspace with this id already exists locally; `true`
        /// moves the existing directory to the recoverable `.trash` first.
        #[serde(default)]
        replace: bool,
    },
    /// Abandon the restore (idle again) and return to the choice screen:
    /// aborts the in-flight task and removes the staging — nothing partial
    /// stays behind.
    RestoreCancel,
    /// Finish a successful restore: open the restored workspace (detached —
    /// §4.4) and move straight to the main screen.
    RestoreFinish,
    /// Real progress from the off-actor restore task (engine-internal —
    /// the task speaking to the engine; every line reports something that
    /// actually happened, there is no simulated progress).
    NetRestoreProgress {
        /// Progress percent (0..=99; 100 is set by the verified finish).
        pct: u8,
        /// One honest live-log line.
        line: String,
        /// Restore incarnation (stale task output is dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The restore task staged the decrypted blob (engine-internal). The
    /// staging handle rides an engine-internal slot, never the wire; the
    /// HANDLER runs the mandatory chain verification (`verify_chain` /
    /// `verify_suffix_chain`, hard-reject) and only then commits — an MCP
    /// agent must not be able to inject an unverified workspace.
    NetRestoreStaged {
        /// Restore incarnation (stale task output is dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The restore task failed (engine-internal); the real reason lands
    /// verbatim in the run log — never a fake success, never an invented
    /// failure rule.
    NetRestoreFailed {
        /// The failure, honestly.
        error: String,
        /// Restore incarnation (stale task output is dropped).
        #[serde(default)]
        generation: Option<u64>,
    },

    // --- founding a republic (shared, co-equal) ---
    /// Begin the founding ritual: validates the configuration, derives the
    /// founder's recovery phrase and identity, mints the n−1 one-time
    /// invite links and opens their invite queues. The workspace is
    /// created only when every member activated their link AND signed the
    /// final roster (transport concept §3.3) — until then nothing exists
    /// on disk, and closing the wizard voids the links. The transport is
    /// NOT a parameter: the ritual always routes through the global
    /// anonymity settings (`SessionSettings.anonymity`, Settings → Anonymity network).
    CreateStart {
        /// The new republic's name.
        name: String,
        /// The founder's handle.
        member: String,
        /// The approval threshold (m), `2..=members` (the engine refuses 1).
        threshold: u8,
        /// The member count (n), `2..=13`.
        members: u8,
        /// The relays this republic will run on — the founder's deliberate
        /// pick, which the invites and the Welcome carry verbatim.
        ///
        /// Empty = "whatever this node can dial", the pre-pick behaviour.
        /// A relay named here that this node cannot dial is refused rather
        /// than dropped: a republic founded on a relay its own founder
        /// cannot reach is a republic nobody can join.
        ///
        /// This is a deliberate set, not an accident of the settings page,
        /// because it becomes constitutional — every member signs it into
        /// the genesis (`relay_topology_plan.md` R2c → R3), and a joiner's
        /// refusal quotes it as a fact about the REPUBLIC.
        #[serde(default)]
        relays: Vec<String>,
    },
    /// Propose the deliberated charter — the final DAO name and a free-text
    /// agenda — once every seat has joined (`create.can_propose`). This seals
    /// the roster for ratification: every member (founder included) signs the
    /// canonical bytes that bind exactly this name + agenda, and only when all
    /// have ratified does the workspace open. Co-equal: an operator or the GUI.
    CreatePropose {
        /// The final republic name.
        name: String,
        /// The free-text charter/agenda to ratify.
        agenda: String,
        /// The feature selection to ratify with the charter
        /// (`charter_features.md`): optional-surface keys, any order —
        /// the engine canonicalizes (sort + dedup) and refuses unknown
        /// keys. Every post-v5 founding carries a selection; empty = the
        /// founder deselected everything optional.
        #[serde(default)]
        features: Vec<String>,
    },
    /// Abandon the founding ritual: distributed links become worthless,
    /// the disk stays untouched (unless the ritual already sealed — then
    /// the created workspace stays listed, just not entered).
    CreateCancel,
    /// Enter the sealed republic: refused until every member is green
    /// (key received and roster signature verified — the engine enforces
    /// this for every operator, not just the GUI). The founding deliberately
    /// does not auto-enter (2026-08-08): the wizard's last step makes the
    /// founder back its recovery phrase up first — entering is that
    /// confirmation (the founder twin of [`Command::JoinFinish`]).
    CreateFinish,
    /// Confirm the operator's recovery-phrase BACKUP during a running
    /// founding or join ritual (`seed_backup_confirmation.md` ❻½): the
    /// engine matches the re-typed phrase against the ritual's minted one;
    /// on the member side the signed `BackupConfirmed` attestation then
    /// goes to the founder, on the founder side the own seat is marked.
    /// The ritual finalizes — and touches disk — only at n-of-n
    /// confirmations. A human decision: an MCP tool AND a GUI action.
    ConfirmSeedBackup {
        /// The re-typed recovery phrase (whitespace-normalized compare).
        phrase: String,
    },
    /// Persist the operator's LOCAL wiki draft (unvoted working copy +
    /// changeset stack) for the open workspace
    /// (`shared_memory_real.md` WP-D): survives a restart, sealed at
    /// rest with the directory, never part of the backup export. The
    /// draft is an opaque frontend blob — the engine stores bytes.
    WikiDraftSave {
        /// The serialized draft ("" removes it).
        draft: String,
    },
    /// Read the open workspace's stored wiki draft
    /// (`Reply::WikiDraft`; "" = none).
    WikiDraftLoad,

    // --- founding-ritual transport events (engine-internal) ---
    /// A member activated their invite link: their JoinRequest arrived on
    /// the invite queue. Sent by the node's own ritual transport tasks
    /// (engine-internal, like `NetDelivered` — never an MCP tool: it
    /// would allow forging members). The engine verifies the ticket MAC.
    NetJoinRequested {
        /// Which invite (0-based index into the ritual's seat list).
        seat: u32,
        /// The member's self-chosen display name.
        member: MemberId,
        /// The member's identity public key, lowercase hex.
        identity_pk: String,
        /// The member's Nostr transport anchor (x-only BIP-340 key,
        /// lowercase hex) — the third anchor the founder seals into the
        /// roster. Empty only from a pre-N1 joiner (whose MAC then fails v2).
        #[serde(default)]
        nostr_pk: String,
        /// `HMAC(KDF(ticket), 0x02 ‖ name ‖ 0 ‖ pk ‖ 0 ‖ nostr_pk)`,
        /// lowercase hex (invite MAC v2 — binds the third anchor too).
        proof: String,
        /// The member's reply-queue handover (JSON of the transport's
        /// `ReplyHandover`) so the founder can send the canonical table
        /// back. Opaque here — core has no transport dependency. Empty on
        /// the legacy path where the founder pre-created the reply queue.
        #[serde(default)]
        reply: String,
        /// On Nostr: the PROVEN sender of the gift wrap this request arrived
        /// in (x-only hex — NIP-59 verifies the seal signature and that the
        /// rumor's author is the sealer). The founder requires it to equal
        /// the claimed `nostr_pk`, which is what makes the third anchor
        /// proof-of-POSSESSED rather than merely chosen. Empty on the
        /// loopback path, where the ritual has no wrap to prove anything.
        #[serde(default)]
        sender_npub: String,
        /// The member's MLS KeyPackage (hex of the wire bytes) so the founder
        /// can add it to the group. Empty on a pre-MLS path.
        #[serde(default)]
        key_package: String,
        /// The relays the joiner actually dials (its confirmed subset of the
        /// invite pool) — display-grade: lets the founder's ritual log name a
        /// relay this member cannot reach. Empty = no declaration.
        #[serde(default)]
        relays: Vec<String>,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A surviving coordinator **mints a recovery link** for a member who lost
    /// its device (`recovery_ritual.md` §3): a human decision (an existing seat,
    /// manually granted), so it is a tool on both surfaces. The engine opens a
    /// dedicated recovery queue, mints a single-use ticket, renders a
    /// `molt://recover/…` link, and listens for the returning member's request.
    RecoverInviteStart {
        /// The returning member's seat handle (must be an anchored roster member).
        member: MemberId,
    },
    /// A member who **lost its device** rejoins from a coordinator-minted
    /// `molt://recover/…` link and its recovery phrase (`recovery_ritual.md`
    /// §4) — a human decision on a fresh device, so it is a tool on both
    /// surfaces. The engine runs the rejoin off the actor: re-derive the seat
    /// identity, prove it to the coordinator, re-enter the MLS group from the
    /// Welcome, verify the served chain from its genesis — the outcome arrives
    /// as `NetRecoverSealed` / `NetRecoverFailed`.
    RecoverStart {
        /// The `molt://recover/…` link (must carry the transport handover).
        link: String,
        /// The seat's recovery phrase (the identity re-derives from it).
        phrase: String,
    },
    /// A returning member's **recovery request** arrived on the coordinator's
    /// recovery queue (engine-internal — the recovery-ritual transport speaks to
    /// the engine; a member must not be able to forge one). The coordinator
    /// verifies the seat proof against the anchored roster key and proposes the
    /// threshold `Membership{Restored}` re-admission.
    NetRecoverRequested {
        /// The seat's member handle.
        member: MemberId,
        /// The member's re-derived identity pk (must equal the anchored one).
        identity_pk: String,
        /// The member's fresh MLS KeyPackage (hex) to re-key its leaf.
        key_package: String,
        /// The recovery ticket the seat proof is bound to.
        ticket: String,
        /// The seat proof: `sign(identity, ticket ‖ key_package ‖ republic_id
        /// ‖ new_nostr_pk)` under `molt-seat-proof-v2`.
        seat_proof: String,
        /// The rejoiner's NEW transport anchor (N4b §8.3), bound by the seat
        /// proof. Empty on the loopback path. Validated at the ingest choke
        /// point before it can reach a chain block.
        #[serde(default)]
        new_nostr_pk: String,
        /// The rejoiner's relay declaration (R5), bound by the seat proof.
        /// Empty = no declaration.
        #[serde(default)]
        relays: Vec<String>,
        /// The rejoiner's own co-signature over
        /// [`chain::restore_consent_bytes`] — its automatic approval of its
        /// re-admission (recovery approval design, 2026-08-08). Validated at
        /// the ingest choke point; rides the `Restored` change and counts as
        /// one distinct signer. Empty on pre-consent requests.
        #[serde(default)]
        consent: String,
        /// The member's reply-queue handover, for the coordinator to send the
        /// Welcome back once re-admission commits. Opaque here.
        #[serde(default)]
        reply: String,
        /// The gift wrap's PROVEN author, filled by the transport task that
        /// peeled it — never by the sender's own claim (the founding twin
        /// `NetJoinRequested.sender_npub` works the same way).
        ///
        /// It is the proof-of-possession for `new_nostr_pk`: a secp256k1 key
        /// that appears in a request signs nothing by itself, so without this
        /// a relay-level attacker could redirect the coordinator's Welcome to
        /// a key it does not hold. Empty on the loopback path, which has no
        /// wrap author.
        #[serde(default)]
        sender_npub: String,
        /// Ritual incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member returned their seal signature over the final roster table
    /// (engine-internal; the engine verifies it against the anchored key).
    NetSealSigned {
        /// Which invite (0-based index into the ritual's seat list).
        seat: u32,
        /// The Ed25519 signature over the canonical table, lowercase hex.
        sig: String,
        /// On Nostr: the MLS-authenticated author of the 445 that carried
        /// this signature. The signature is verified against the seat's
        /// anchored key anyway, so this is defence in depth — it refuses a
        /// signature attributed to a seat its author does not hold. Empty
        /// on the loopback path (a private queue authenticated it instead).
        #[serde(default)]
        from: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's seed-backup attestation arrived on the founder
    /// (engine-internal; verified against the seat's anchored key over
    /// `molt_storage::backup_confirm_bytes` of the ratified table —
    /// `seed_backup_confirmation.md` ❻½).
    NetBackupConfirmed {
        /// Which invite (0-based index into the ritual's seat list).
        seat: u32,
        /// The Ed25519 signature over the attestation bytes, lowercase hex.
        sig: String,
        /// The authenticated wire author (defence in depth, like
        /// `NetSealSigned.from`); empty on the loopback path.
        #[serde(default)]
        from: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Test the S3 backup target (the backup settings panel's Test button):
    /// a real SigV4-signed `HEAD /bucket` probe over the configured dialer
    /// (Tor when enabled — fail-closed, like every dial), run off the actor;
    /// the result lands in `session.s3_test`. Safe to expose — it only reads
    /// whether the bucket answers, with the configured credentials.
    NetTestS3 {
        /// Which target to probe; absent = the workspace-backup bucket, so
        /// a caller that predates the media bucket keeps its behaviour.
        #[serde(default)]
        target: S3Target,
        /// Endpoint URL to test (`https://…` / `http://…`; MinIO and onion
        /// endpoints supported, path-style). Empty tests the saved
        /// `settings.s3_endpoint`.
        #[serde(default)]
        endpoint: String,
        /// Access key id; empty falls back to the saved settings.
        #[serde(default)]
        access_key: String,
        /// Secret key; empty falls back to the saved settings.
        #[serde(default)]
        secret_key: String,
        /// Bucket to probe; empty falls back to the saved settings.
        #[serde(default)]
        bucket: String,
    },
    /// The outcome of a [`Command::NetTestS3`] probe, reported back from the
    /// off-actor probe task (engine-internal, never an MCP tool).
    NetTestS3Result {
        /// Which target this verdict is about - it decides which session
        /// slot it lands in.
        #[serde(default)]
        target: S3Target,
        /// `"ok"` or `"error: …"`; written verbatim into `session.s3_test`
        /// (or `session.s3_media_test` for [`S3Target::Media`]).
        result: String,
    },
    /// Test whether Tor is actually there and working (the anonymity settings
    /// panel's "Test Tor" button), run off the actor; the verdict lands in
    /// `session.tor_test`.
    ///
    /// The probe climbs an HONEST ladder and reports which rung it reached
    /// (see [`TorTestState`]): a socket answering at the SOCKS address is
    /// **not** a working Tor, and only a dial THROUGH Tor to a relay the
    /// operator already confirmed proves a circuit. It never invents a host
    /// to dial (ADR-0004 — nothing is pre-configured), so with an empty or
    /// fully blocked relay pool it stops at the partial rung and says so.
    ///
    /// Safe to expose: it opens at most one connection, to the operator's
    /// own proxy and their own confirmed relay.
    NetTestTor {
        /// Anonymity network to test (`"tor"`; anything else is refused as
        /// [`TorTestState::Off`]). Empty tests the saved
        /// `settings.anonymity`.
        #[serde(default)]
        network: String,
        /// Tor mode (`"local" | "embedded" | "whonix"`); empty falls back to
        /// the saved `settings.tor_mode`.
        #[serde(default)]
        mode: String,
        /// Local Tor SOCKS port; `0` falls back to the saved
        /// `settings.tor_port` (nothing can listen on port 0, so it is a
        /// safe "not given" marker).
        #[serde(default)]
        port: u16,
    },
    /// The outcome of a [`Command::NetTestTor`] probe, reported back from the
    /// off-actor probe task (engine-internal, never an MCP tool — an agent
    /// must not be able to forge a "Tor works" verdict).
    NetTestTorResult {
        /// The honest verdict; written into `session.tor_test`.
        result: TorTest,
        /// Which probe request this answers (the engine's test generation).
        /// A stale result — an older probe resolving after the anonymity
        /// settings changed — is dropped instead of claiming the new
        /// configuration was tested.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// List the configured S3 bucket's backup objects (the settings backup
    /// table's refresh): a SigV4-signed ListObjectsV2 under the `molt/`
    /// prefix over the configured dialer (Tor when enabled — fail-closed),
    /// run off the actor. The outcome classifies into real
    /// `session.backup_orphans` (objects with no matching local workspace)
    /// and `session.s3_list`; with no backup target configured it fails
    /// fast with an honest note and an empty table. Always driven by the
    /// SAVED settings. Safe to expose — it only reads the bucket.
    NetListBackups,
    /// The outcome of a [`Command::NetListBackups`] listing, reported back
    /// from the off-actor listing task (engine-internal, never an MCP tool
    /// — an agent must not be able to forge bucket contents).
    NetListBackupsResult {
        /// `"ok"` or `"error: …"`; written verbatim into `session.s3_list`.
        result: String,
        /// The listed objects (empty on error); the engine classifies them
        /// against the locally known workspaces at arrival time.
        #[serde(default)]
        objects: Vec<BackupObject>,
        /// Which listing request this answers (the engine's listing
        /// generation). A stale result — an older request resolving after a
        /// newer one, possibly against previously saved settings — is
        /// dropped instead of overwriting the newer table.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Run one workspace's automatic backup NOW (the manual "backup now to
    /// S3" trigger — a human decision, so a tool on both surfaces): builds
    /// the crash-consistent `molt-export-v1` blob in `workspace` key mode
    /// (restorable from recovery phrase + workspace id, no prompt) and PUTs
    /// it to the configured bucket over the configured dialer (Tor when
    /// enabled — fail-closed), then prunes old copies beyond
    /// `s3_keep_copies`. Same off-actor task as the ticker; the honest
    /// outcome lands in the workspace entry (stamp only on a confirmed
    /// upload, `backup_error` otherwise).
    BackupNow {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
    },
    /// Fetch the NEWEST bucket backup of a workspace onto this device — a
    /// human decision (the backup table's restore button), so a tool on both
    /// surfaces (S7, `backup_restore_design.md` §10). Downloads the blob
    /// VERBATIM into a [`SEALED_RESTORED`] stub the Open list shows; **no
    /// secret is asked and nothing is decrypted** — opening the entry with
    /// its recovery phrase later drives the verified restore pipeline
    /// ([`Command::DecryptWorkspace`]). Refused while a workspace with this
    /// id already exists locally.
    BackupFetch {
        /// The workspace id ([`WorkspaceInfo::id`]) — the bucket pseudonym.
        id: WorkspaceId,
    },
    /// The fetch task's outcome (engine-internal, from the off-actor fetch
    /// task — an agent must not be able to forge a fetched artifact).
    NetBackupFetched {
        /// The fetched workspace id.
        id: WorkspaceId,
        /// Empty on success; the honest reason otherwise.
        #[serde(default)]
        error: String,
    },
    /// The backup ticker's heartbeat (engine-internal, sent by the engine's
    /// own clock): the synchronous handler only DECIDES — workspaces whose
    /// auto-backup pref is on, whose interval elapsed, and whose key is
    /// accessible spawn an off-actor upload task; sealed-at-rest workspaces
    /// are skipped with an honest status (design P6).
    BackupTick,
    /// The backup task confirmed the upload (engine-internal, from the
    /// off-actor backup task — an MCP agent must not be able to forge a
    /// backup stamp). ONLY this moves `prefs.last_backup`.
    NetBackupDone {
        /// The backed-up workspace.
        id: WorkspaceId,
        /// Unix seconds the blob was built (the object key's timestamp).
        ts: u64,
        /// The confirmed bucket object key.
        object: String,
        /// Blob size in bytes.
        bytes: u64,
        /// Retention-pruning failure, honestly (empty = pruned fine). A
        /// prune failure never blocks the backup — the next successful
        /// backup re-prunes.
        #[serde(default)]
        prune_error: String,
        /// Byte-quota outcome, honestly (empty = the bucket fits its limit,
        /// or no limit is set). Its own field because it is a different
        /// story from a failed prune: the quota may be unreachable without
        /// deleting a workspace's last copy, which is reported, never done.
        #[serde(default)]
        quota_error: String,
    },
    /// The backup task failed (engine-internal); the stamp stays untouched
    /// and the real reason is surfaced verbatim — never a fake success.
    NetBackupFailed {
        /// The workspace whose backup failed.
        id: WorkspaceId,
        /// The failure, honestly.
        error: String,
    },
    /// A founding seat's real, joinable invite link became available once its
    /// queue was provisioned on the SMP server (engine-internal, from the
    /// off-actor provisioning task). Carries the transport handover, so it is
    /// never an MCP tool.
    NetRitualLinkReady {
        /// Which seat (0-based).
        seat: u32,
        /// The full `molt://invite/…` link (with the transport handover).
        link: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A minted recovery link became available once its dedicated queue was
    /// provisioned (engine-internal, from the off-actor recovery-mint task).
    /// Carries the transport handover, so it is never an MCP tool.
    NetRecoverLinkReady {
        /// The returning member the link re-admits.
        member: MemberId,
        /// The full `molt://recover/…` link (with the transport handover).
        link: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A recovery link's off-actor queue provisioning failed (e.g. the SMP
    /// server is unreachable), so no link will ever arrive (engine-internal,
    /// from the recovery-mint task). The engine surfaces the calm
    /// `recovery-link-failed:` notice — the flip side of
    /// [`Command::NetRecoverLinkReady`] — and unregisters the dead mint's
    /// ticket. Never an MCP tool.
    NetRecoverLinkFailed {
        /// The returning member the failed link was minted for.
        member: MemberId,
        /// A reason for the operator (`mesh-not-running`, or transport text).
        reason: String,
        /// The failed mint's single-use ticket, so the spend-once guard can
        /// unregister it (it never left this node).
        ticket: String,
        /// Workspace scope (stale reports for a closed workspace are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The founder's off-actor SMP provisioning failed (e.g. the server is
    /// unreachable), so the founding can never seal (engine-internal). The
    /// engine fails the create run rather than leaving it stuck. Not a tool.
    NetRitualFailed {
        /// A human-readable reason.
        error: String,
        /// Ritual incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The founder's own ritual task reporting a NON-FATAL transport
    /// condition (engine-internal) — e.g. the group channel cannot be heard
    /// after a window roll. Appends to the founding log and never fails the
    /// run: a one-shot `CreatePropose` must not die on a relay blip. Never a
    /// tool — an MCP agent must not be able to write lines into a founding
    /// log.
    NetRitualNote {
        /// The line to append, already worded by the task.
        note: String,
        /// Ritual incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The joiner's twin of [`Command::NetRitualNote`], scoped to the join
    /// wizard's run log. Never a tool, same reason.
    NetJoinNote {
        /// The line to append.
        note: String,
        /// Join incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The REJOINER's twin of [`Command::NetJoinNote`]: the recovery task
    /// reporting where its bounded wait stands (the join task surfaces a
    /// deaf relay live; recovery is the one flow whose user has already
    /// lost their device, so it must not be silent until the absolute
    /// deadline). Never a tool, same reason.
    NetRecoverNote {
        /// The status line (compact, one fact).
        note: String,
        /// Recovery incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The coordinator's live report of the re-admission vote (engine-
    /// internal, `recovery_auto_approval.md` §4): who has approved, out of
    /// which roster, toward which threshold. Display data for the rejoiner's
    /// checklist — carries no authority (the Welcome finishes a rejoin).
    NetRecoverProgress {
        /// The returning seat.
        member: String,
        /// Approvals needed (m).
        need: u32,
        /// The full roster, in roster order.
        roster: Vec<String>,
        /// Members whose voice is counted (approvals + the consent).
        approved: Vec<String>,
        /// Recovery incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A ritual publish task reporting its REAL per-relay outcome
    /// (engine-internal). Sent for every outcome — clean, partial and total
    /// failure — so "landed on 1 of 5 relays" and "landed nowhere" stop being
    /// indistinguishable from success. An empty `accepted` means nobody has
    /// the frame. Never a tool: an MCP agent must not be able to forge a
    /// relay outcome and thereby fail (or fake) a founding.
    NetRitualPublished {
        /// Which leg published — "seal", "genesis", …
        what: String,
        /// Relay URLs that accepted the event.
        #[serde(default)]
        accepted: Vec<String>,
        /// Pre-formatted `"url: reason"` per relay that refused, so the
        /// actor owns the wording.
        #[serde(default)]
        failed: Vec<String>,
        /// Ritual incarnation, or `None` for a leg published after the
        /// ritual was already taken (the genesis).
        #[serde(default)]
        generation: Option<u64>,
        /// Which workspace this leg belongs to, for legs published AFTER the
        /// ritual was taken. Without it a genesis report landing ~45 s later
        /// is attributed to whatever founding is on screen by then.
        #[serde(default)]
        workspace: String,
    },
    /// A real SMP join completed: the off-actor join task verified the sealed
    /// roster the founder distributed (engine-internal). The engine writes the
    /// joiner's own workspace from it. Never an MCP tool.
    NetJoinSealed {
        /// JSON of the verified `SealedRoster`.
        sealed: String,
        /// The joiner's own MLS group snapshot (hex of the `MlsMember` blob),
        /// produced by processing the founder's Welcome — node-local, sealed
        /// into the joiner's `transport.state`. Empty on a pre-MLS path.
        #[serde(default)]
        mls: String,
        /// The joiner's assembled direct-mesh handovers from the (best-effort)
        /// post-founding bootstrap; sealed into `transport.state.mesh`. Empty
        /// when the bootstrap did not run or did not complete.
        #[serde(default)]
        mesh: Vec<MeshLink>,
        /// The joiner's derived Nostr transport secret (32-byte secp256k1
        /// scalar, hex) — ticket-salted in the ritual, so it cannot be
        /// re-derived once the ritual is over; sealed into the joiner's
        /// `transport.state.nostr_sk` (like the MLS blob above, it rides an
        /// engine-internal command only). Empty on a legacy path.
        #[serde(default)]
        nostr_sk: String,
        /// The group's relay list as delivered inside the authenticated
        /// Welcome (N4) — sealed into `transport.state.relays`. Empty on a
        /// loopback/test join.
        #[serde(default)]
        relays: Vec<String>,
        /// The group's h-tag rotation seed (32 bytes, hex) from the same
        /// Welcome — sealed into `transport.state.rotation_seed`. Empty on a
        /// loopback/test join.
        #[serde(default)]
        rotation_seed: String,
        /// Join incarnation (a cancelled/restarted join drops stale results).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A real SMP join failed (engine-internal): surfaced into the join run.
    NetJoinFailed {
        /// A human-readable reason.
        error: String,
        /// Join incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A total-loss **recovery completed** (engine-internal): the off-actor
    /// rejoin task re-entered the MLS group and verified the coordinator-served
    /// chain from its genesis. The engine re-verifies and materializes the
    /// recovered workspace from the chain. Never an MCP tool.
    NetRecoverSealed {
        /// The recovered seat's member handle.
        member: MemberId,
        /// JSON of the full, verified `Vec<ChainBlock>` — the genesis carries
        /// the constitution the workspace materializes from.
        chain: String,
        /// The rejoiner's own post-Welcome MLS group snapshot (hex of the
        /// `MlsMember` blob), sealed into the recovered `transport.state`.
        #[serde(default)]
        mls: String,
        /// The re-established full-mesh handovers to the survivors (dynamic
        /// mesh membership); sealed into `transport.state.mesh` and — when the
        /// rejoin transport is available — the runtime supervisor stands up
        /// over them. Empty when the mesh re-join was skipped or timed out.
        #[serde(default)]
        mesh: Vec<MeshLink>,
        /// The returning seat's NEW Nostr transport secret (32-byte secp256k1
        /// scalar, hex): salted with the RECOVERY ticket, so it is a different
        /// key from the founding anchor that died with the lost device.
        ///
        /// The actor re-derives the same key from `(phrase, ticket)` and
        /// refuses a mismatch — the value here proves the rejoin task signed
        /// its request with the key the chain then anchored. Empty on a
        /// loopback/test recovery.
        #[serde(default)]
        nostr_sk: String,
        /// The group's h-tag rotation seed (32 bytes, hex) from the Welcome —
        /// sealed into `transport.state.rotation_seed`. The one piece of the
        /// Nostr shape the CHAIN cannot supply (the ratified relay pool comes
        /// from the verified chain, not from here). Empty on a loopback/test
        /// recovery.
        #[serde(default)]
        rotation_seed: String,
        /// Recovery incarnation (a superseded recovery drops stale results).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A total-loss recovery failed (engine-internal): surfaced to the
    /// operator. Never an MCP tool.
    NetRecoverFailed {
        /// A human-readable reason.
        error: String,
        /// Recovery incarnation.
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A rejoiner's **mesh announce** arrived on the coordinator's recovery
    /// queue (engine-internal, from the recovery recv loop). The coordinator
    /// authenticates it against the just-re-keyed member, relays the ciphertext
    /// over the runtime mesh, and extends its own mesh toward the rejoiner.
    /// Never an MCP tool.
    NetRecoverAnnounced {
        /// Hex of the announcer's MLS-encrypted `MeshAnnounce`.
        ct: String,
        /// Ritual incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A node's off-actor **mesh-extension task** finished (engine-internal):
    /// its fresh per-pair link to a (re)announced member is ready to fold into
    /// the running mesh. The engine rebuilds its supervisor with the link
    /// (replacing any stale link to the same member) and persists the grown
    /// mesh. Never an MCP tool.
    NetMeshExtended {
        /// The ready-to-run link to the (re)announced member.
        link: MeshLink,
        /// Mesh incarnation (a torn-down mesh drops the extension).
        #[serde(default)]
        generation: Option<u64>,
    },

    // --- joining via invite (shared, co-equal) ---
    /// Begin joining a republic from its `molt://invite/…` link. The link must
    /// carry a v2 transport handover — a bare preview link, or a pre-N4
    /// queue-shaped one, is rejected. Since N4a the join runs over Nostr: the
    /// engine spawns the off-actor member task (gated on the operator's
    /// confirmed relay pool), shows the joiner's own recovery phrase, and
    /// seals the workspace once the founder seals (entering then waits for
    /// [`Command::JoinFinish`]) — the outcome arrives as `NetJoinSealed` /
    /// `NetJoinFailed`. The loopback seams still serve the state-level tests.
    JoinStart {
        /// The `molt://invite/…` link.
        invite: String,
        /// The joiner's handle.
        member: String,
    },
    /// Ratify the founder's proposed charter (name + agenda), surfaced when the
    /// join reaches the ratification step (`join.awaiting_ratify`). This is the
    /// joiner's confirmation — it releases the seal signature and the join
    /// proceeds to open the workspace. Co-equal: an operator or the GUI.
    JoinConfirmCharter,
    /// Decline the founder's proposed charter (the other choice at the
    /// ratification step). The joiner's node tells the founder it declined (so
    /// the founder can re-mint) and the join ends as failed. Co-equal.
    JoinDeclineCharter,
    /// Enter the republic a completed join sealed (`join.sealed_id`). The
    /// join deliberately does not auto-enter (2026-08-08): the wizard's last
    /// step makes the joiner back its recovery phrase up first — entering is
    /// the human's confirmation, so it is a tool on both surfaces (the
    /// joiner twin of [`Command::CreateFinish`]).
    JoinFinish,
    /// A member explicitly declined the proposed charter (engine-internal;
    /// raised by the founder's ritual recv loop). The founder marks the seat and
    /// logs it. Never an MCP tool.
    NetJoinDeclined {
        /// Which seat (0-based invite index) declined.
        seat: u32,
        /// On Nostr: the MLS-authenticated author of the decline. A decline
        /// carries NO signature, so this is its ONLY authentication —
        /// without it any group member could abort the founding and frame
        /// another seat for it. Empty on the loopback path.
        #[serde(default)]
        from: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The off-actor share hash finished: metadata + real sha256 of a file
    /// being shared — the actor posts the share message and remembers the
    /// source path (engine-internal; raised by the share-hash task). Never
    /// an MCP tool.
    NetFileShared {
        /// File name (no path).
        name: String,
        /// Size in bytes.
        size: u64,
        /// Display type, e.g. `"PDF"`.
        kind: String,
        /// The file's own mtime, unix seconds.
        modified: u64,
        /// sha256 over the bytes, lowercase hex.
        checksum: String,
        /// The local source path (stays node-local: prefs, never wire/log).
        path: String,
        /// The channel view the share files under.
        channel: ChannelRef,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
        /// Series-v2 material ([`FileMeta::key_b64`]).
        #[serde(default)]
        key_b64: String,
        /// [`FileMeta::pieces`].
        #[serde(default)]
        pieces: u32,
        /// [`FileMeta::root`].
        #[serde(default)]
        root: String,
    },
    /// The off-actor share hash failed (unreadable file) — surface the
    /// honest error (engine-internal). Never an MCP tool.
    NetFileShareFailed {
        /// The file name that failed.
        name: String,
        /// The honest reason.
        reason: String,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The fetch task minted its reply queue and MLS-encrypted its request:
    /// the actor records the `FileRequested { ct }` event so the outbox
    /// carries it to the sharer (engine-internal). Never an MCP tool.
    NetFileRequestReady {
        /// The share message's stable id.
        id: MessageId,
        /// Hex of the MLS-encrypted `FetchRequest`.
        ct: String,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Download progress (engine-internal; raised by the fetch task,
    /// throttled). Never an MCP tool.
    NetFileProgress {
        /// The share message's stable id.
        id: MessageId,
        /// Bytes received so far.
        transferred: u64,
        /// Total bytes expected.
        total: u64,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A download completed and verified (engine-internal). Never an MCP tool.
    NetFileDone {
        /// The share message's stable id.
        id: MessageId,
        /// The final local path the file landed at.
        path: String,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A download failed (engine-internal; the honest reason). Never an
    /// MCP tool.
    NetFileFailed {
        /// The share message's stable id.
        id: MessageId,
        /// The honest reason.
        reason: String,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A parked relay download's watchdog fired (engine-internal): if the
    /// sharer's `FileServed` never came, the download fails honestly
    /// instead of parking forever. Never an MCP tool.
    NetFileWantedTimeout {
        /// The share message's stable id.
        id: MessageId,
        /// Workspace-net incarnation (stale watchdogs are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The sharer's lazy chunk-series publish finished (engine-internal;
    /// the off-actor publish task reports back, the actor records the
    /// `FileServed` announcement). Never an MCP tool.
    NetFileSeriesPublished {
        /// The share message's stable id.
        id: MessageId,
        /// The series' publish stamp (0 = the publish FAILED — the sharer
        /// clears its in-flight mark and the requester's fetch stays on
        /// its honest miss).
        at: u64,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// An authenticated `PieceWanted` control frame landed (engine-internal:
    /// the transport speaks to the engine). A member asks the holders of a
    /// v2 series for the pieces in `ranges`. Never an MCP tool.
    NetPieceWanted {
        /// The MLS-authenticated requester.
        from: MemberId,
        /// The share message's stable id.
        id: MessageId,
        /// Inclusive piece index ranges.
        ranges: Vec<(u32, u32)>,
        /// Transport incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The running piece fetch asks the actor to send a `PieceWanted` for
    /// the pieces still missing (engine-internal; the off-actor fetch job
    /// speaks to the actor, which holds the group runtime). Never an MCP
    /// tool.
    NetPieceWantSend {
        /// The share message's stable id.
        id: MessageId,
        /// Inclusive piece index ranges.
        ranges: Vec<(u32, u32)>,
        /// Workspace-net incarnation (a stale job's ask is dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's authenticated mirror declaration landed (engine-internal:
    /// the transport speaks). Never an MCP tool.
    NetMirrorDecl {
        /// The MLS-authenticated declarer.
        from: MemberId,
        /// Consent.
        on: bool,
        /// Budget, bytes.
        quota: u64,
        /// Revision; last wins.
        rev: u64,
        /// Transport incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's authenticated hold status landed (engine-internal).
    /// Never an MCP tool.
    NetMirrorStatus {
        /// The MLS-authenticated holder.
        from: MemberId,
        /// What it holds (this page of it).
        holds: Vec<MirrorHold>,
        /// The send this page belongs to; pages of one generation replace
        /// the stored status together.
        #[serde(default)]
        gen: u64,
        /// This page's index within the generation.
        #[serde(default)]
        page: u16,
        /// The generation's page count.
        #[serde(default = "one_page")]
        pages: u16,
        /// Transport incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member asks who holds what (engine-internal). Never an MCP tool.
    NetMirrorWho {
        /// The MLS-authenticated asker.
        from: MemberId,
        /// Transport incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The mirror worker's fetch verified more pieces (engine-internal).
    /// Never an MCP tool.
    NetMirrorProgress {
        /// The share message's stable id.
        id: MessageId,
        /// Verified data pieces.
        held: u32,
        /// Plaintext bytes held.
        bytes: u64,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The mirror worker's fetch ended (engine-internal). Never an MCP tool.
    NetMirrorDone {
        /// The share message's stable id.
        id: MessageId,
        /// The series is complete and stored; `false` = the honest reason.
        ok: bool,
        /// Why it failed ("" on success).
        reason: String,
        /// What the folder holds for it on success (sealed bytes).
        #[serde(default)]
        bytes: u64,
        /// Workspace-net incarnation (stale task results are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The founder **accepted** the join request (engine-internal; surfaced by
    /// the off-actor join task on the founder's advisory `JoinAccepted` ack). The
    /// joiner's wizard confirms the join landed while it waits for the
    /// deliberation. Never an MCP tool.
    NetJoinAccepted {
        /// Join incarnation (a cancelled/restarted join drops stale results).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The join reached the ratification step: the founder proposed this final
    /// name + agenda for the joiner to review before signing (engine-internal;
    /// surfaced by the off-actor join task). Never an MCP tool.
    NetJoinCharterProposed {
        /// The proposed final DAO name.
        name: String,
        /// The proposed free-text charter/agenda.
        agenda: String,
        /// The proposed feature selection (`None` from a pre-v5 founder).
        #[serde(default)]
        features: Option<Vec<String>>,
        /// Join incarnation (a cancelled/restarted join drops stale results).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// A member's post-founding **mesh announcement** reached the founder over
    /// the founding star (engine-internal; raised by the founder's ritual recv
    /// loop). The founder forwards the MLS ciphertext into its running mesh
    /// bootstrap (and relays it to the other members). Never an MCP tool.
    NetMeshAnnounced {
        /// Which seat (0-based invite index) announced.
        seat: u32,
        /// Hex of the member's MLS-encrypted `molt_net::mesh::MeshAnnounce`.
        ct: String,
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// The founder's post-founding **mesh bootstrap completed** (engine-internal;
    /// raised by the off-actor bootstrap task). Carries the assembled direct-mesh
    /// handovers and the founder's post-bootstrap MLS snapshot, which the actor
    /// persists into the founded workspace's transport state. Never an MCP tool.
    NetMeshReady {
        /// The founder's assembled full-mesh peer handovers.
        mesh: Vec<MeshLink>,
        /// The founder's MLS group snapshot taken **after** the bootstrap
        /// (its ratchet advanced through the announcements).
        mls_snapshot: Vec<u8>,
        /// Ritual incarnation (a superseded founding drops stale results).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Abandon the join (idle again) and return to the choice screen.
    JoinCancel,
    /// **Acknowledge the transient session notice** — the operator has seen
    /// it, so it stops being state. Without this a one-shot notice (a minted
    /// recovery link above all) lingers in the session for good and re-opens
    /// its dialog on every fresh window. A human "I have read this", so it is
    /// a tool on both surfaces.
    ClearNotice,
    /// **Poke a member** — a directed, ephemeral nudge with no governance
    /// meaning: the target's node (if it enabled poking) shows who poked,
    /// plays its configured sound and runs its wake command. A human
    /// decision, so it is a tool on both surfaces.
    Poke {
        /// The roster member to poke (not this seat itself).
        member: MemberId,
    },
    /// This seat's mirror consent and budget for the open republic
    /// (`docs_archive/files/mirroring.md` §3.5): declared to the members at once.
    /// A human decision on both surfaces.
    SetMirror {
        /// Mirror persistent files.
        on: bool,
        /// Total mirror budget for this republic, bytes.
        quota_bytes: u64,
    },
    /// Where this seat keeps the mirrored pieces of the open republic
    /// (`prefs.mirror_dir`; "" = the default folder). Any-path on the
    /// host, so GUI/config-only - never an MCP tool (`mcp-security.md`).
    SetMirrorDir {
        /// The folder, `~` allowed; "" restores the default.
        path: String,
    },
    /// An authenticated poke arrived over the transport (engine-internal —
    /// the transport speaks to the engine; an MCP agent must not be able to
    /// forge a nudge from another member). `from` is the MLS-authenticated
    /// sender, `to` the member the frame names.
    NetPoked {
        /// The authenticated poker.
        from: MemberId,
        /// The member the poke is addressed at.
        to: MemberId,
        /// Transport incarnation (stale commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Set (or clear) this node's **wake command** — the local shell hook a
    /// poke or pending vote runs. Deliberately NOT an MCP tool: the string is
    /// executed by the node, so an agent that could set it would grant itself
    /// code execution on the operator's machine (the same reasoning that
    /// keeps the clearnet switch off the settings surface). The GUI and
    /// `config.toml` are its only doors.
    SetWakeCommand {
        /// The command line, run via `sh -c`. Empty clears it.
        command: String,
    },
    /// Set the node's HOST POSTURE — the settings that decide who can
    /// reach this node, whether its traffic is anonymized, where it keeps
    /// and exchanges files, and the two secrets. Deliberately NOT an MCP
    /// tool (MCP audit 2026-08-26 M1/H4): an agent operates its human's
    /// SEAT (the "agents are seats" decision), not the human's machine or
    /// anonymity — `anonymity = "none"` over MCP would deanonymize the
    /// operator, `mcp_allow = "0.0.0.0"` or a token rotation would let it
    /// widen or take over its own access. `SaveSettings` and
    /// `PatchSettings` keep the stored values for these keys; the GUI and
    /// `config.toml` are the doors.
    SetNodePosture {
        /// The posture to store.
        posture: NodePosture,
    },
}

/// The host-posture settings ([`Command::SetNodePosture`]). `None` for a
/// secret keeps the stored value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodePosture {
    /// Run without a window.
    pub headless: bool,
    /// Where workspaces live.
    pub workspace_dir: String,
    /// The download / exchange directory.
    pub download_dir: String,
    /// The MCP TCP port.
    pub mcp_port: u16,
    /// The MCP client allowlist.
    pub mcp_allow: String,
    /// `"tor" | "none"`.
    pub anonymity: String,
    /// `"local" | "embedded" | "whonix"`.
    pub tor_mode: String,
    /// The local Tor SOCKS port.
    pub tor_port: u16,
    /// The MCP token; `None` keeps the stored one.
    #[serde(default)]
    pub mcp_token: Option<String>,
    /// The S3 secret; `None` keeps the stored one.
    #[serde(default)]
    pub s3_secret_key: Option<String>,
}

impl NodePosture {
    /// The posture `settings` describe, both secrets included (what the
    /// GUI's settings draft sends).
    #[must_use]
    pub fn of(settings: &SessionSettings) -> NodePosture {
        NodePosture {
            headless: settings.headless,
            workspace_dir: settings.workspace_dir.clone(),
            download_dir: settings.download_dir.clone(),
            mcp_port: settings.mcp_port,
            mcp_allow: settings.mcp_allow.clone(),
            anonymity: settings.anonymity.clone(),
            tor_mode: settings.tor_mode.clone(),
            tor_port: settings.tor_port,
            mcp_token: Some(settings.mcp_token.clone()),
            s3_secret_key: Some(settings.s3_secret_key.clone()),
        }
    }
}

/// The verbs the window performs for [`Command::UiAction`]; anything else
/// is refused by the engine instead of landing as a silent no-op that
/// still bumps the snapshot generation (review F2).
pub const UI_ACTION_VERBS: [&str; 5] = [
    "select_view",
    "select_channel",
    "open_workspace",
    "close_workspace",
    "chat_send",
];

/// The settings keys [`Command::SetNodePosture`] owns — refused by
/// `PatchSettings`, re-merged from the stored values by `SaveSettings`.
pub const NODE_POSTURE_KEYS: [&str; 10] = [
    "headless",
    "workspace_dir",
    "download_dir",
    "mcp_port",
    "mcp_allow",
    "mcp_token",
    "anonymity",
    "tor_mode",
    "tor_port",
    "poke_wake_command",
];

impl Command {
    /// The snake_case names of every command variant — the co-equality
    /// audit in `molt-mcp` checks the tool catalogue against this list.
    pub fn variant_names() -> &'static [&'static str] {
        <Command as strum::VariantNames>::VARIANTS
    }
}

/// The synchronous answer to a [`Command`]. Streaming changes arrive separately
/// as [`Event`]s on the broadcast channel.
/// What the WINDOW claims to show (`docs_archive/ui/gui_over_mcp.md`): published
/// by the GUI at the end of every live-mirror pass — so what it publishes
/// is what it rendered — and read back over MCP so an agent can test the
/// window the way it already tests the engine. Counts and a few strings,
/// deliberately: the question is "does the pane hold what the engine
/// holds", never a widget tree. Additive-only (serde defaults).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiSnapshot {
    /// The screen the window is on (`choice`/`main`/`create`/…).
    #[serde(default)]
    pub screen: String,
    /// Selected surface key.
    #[serde(default)]
    pub surface: String,
    /// Selected sub-view key.
    #[serde(default)]
    pub view: String,
    /// The chat pane's selected channel (storage key form).
    #[serde(default)]
    pub channel: String,
    /// Chat rows the pane's model holds.
    #[serde(default)]
    pub chat_rows: u32,
    /// The last few rendered chat bodies (newest last).
    #[serde(default)]
    pub chat_last: Vec<String>,
    /// The compose row is visible.
    #[serde(default)]
    pub compose_visible: bool,
    /// The chat log is scrolled into view (the empty-chat class was
    /// invisible without exactly this fact — a 0-height re-pin scrolled
    /// the whole log out of sight while every model test passed).
    #[serde(default)]
    pub chat_in_view: bool,
    /// Nav rows the window offers, as `surface` keys in order.
    #[serde(default)]
    pub nav: Vec<String>,
    /// Open decisions the window shows.
    #[serde(default)]
    pub pending_count: u32,
    /// Which wizard step/phase is active ("" outside a wizard).
    #[serde(default)]
    pub wizard: String,
    /// The topmost toast, if any.
    #[serde(default)]
    pub toast: String,
    /// The GUI's own monotonically increasing publish counter — lets an
    /// agent await "the click landed" (`ui_action` bumps it on perform).
    #[serde(default)]
    pub generation: u64,
}

/// One requested GUI interaction (`docs_archive/ui/gui_over_mcp.md`): a domain
/// VERB naming an affordance a human can reach (never a widget
/// coordinate), performed by the window's live mirror. `args` shape is
/// per-verb; unknown verbs are performed as no-ops and reported via the
/// next snapshot's generation (the request is a REQUEST — the GUI
/// performs it, MCP never writes into another surface's state).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiAction {
    /// The verb, one of [`UI_ACTION_VERBS`].
    #[serde(default)]
    pub verb: String,
    /// Per-verb parameters (JSON object).
    #[serde(default)]
    pub args: serde_json::Value,
}

/// What a [`Command`] answers with. One enum, so every frontend renders
/// the same reply surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// The command was accepted with no further data.
    Ack,
    /// A proposal was created.
    Proposed {
        /// The new proposal's id.
        id: ProposalId,
    },
    /// A surface snapshot.
    State(SurfaceSnapshot),
    /// The list of known proposals.
    Proposals {
        /// Every known proposal.
        proposals: Vec<ProposalView>,
    },
    /// A status summary.
    Status(StatusView),
    /// The open workspace's stored local wiki draft ("" = none).
    WikiDraft {
        /// The serialized draft blob, exactly as saved.
        draft: String,
    },
    /// The member table (Organization → Members). A struct variant on
    /// purpose: the internally-tagged `reply` repr cannot serialize a bare
    /// sequence (the MCP surface renders replies as JSON).
    Members {
        /// One row per roster member.
        members: Vec<MemberView>,
    },
    /// The GUI's last published snapshot (`Command::ReadUiState`).
    UiState {
        /// `None` while no window published anything.
        snapshot: Option<UiSnapshot>,
    },
    /// Every file shared into the chat (Shared Files → Uploads). Struct
    /// variant for the same reason as [`Reply::Members`].
    Uploads {
        /// One row per share, log order.
        uploads: Vec<UploadView>,
    },
    /// Who mirrors what ([`Command::ReadMirror`]).
    Mirror(Box<MirrorView>),
    /// The whole shared session state (boxed: it is by far the largest reply).
    Session(Box<SessionView>),
    /// The persistent chain as display views (the Chain-History read),
    /// newest first. Struct variant for the same reason as
    /// [`Reply::Members`]: the internally-tagged `reply` repr cannot
    /// serialize a bare sequence.
    Chain {
        /// One view per committed block — plus, on a pruned holder, the
        /// synthetic pre-cut entries from the checkpoint blob — newest first.
        blocks: Vec<ChainBlockView>,
    },
}

/// Lifecycle state of a proposal (a deliberately collapsed subset of the full
/// R0 lifecycle for the scaffold).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    /// Awaiting approvals.
    Proposed,
    /// Reached the threshold and was applied to the surface.
    Applied,
    /// Declined / can no longer reach the threshold.
    Rejected,
}

/// One member's stance on a pending proposal — what the pending cards'
/// voting row renders, one entry per roster member in roster order.
/// Display data, never consensus input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberVote {
    /// The member.
    pub member: MemberId,
    /// The member's stance.
    pub vote: VoteState,
}

/// A member's stance on a pending proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteState {
    /// Has not voted yet.
    Open,
    /// Approved (chain governance: a collected co-signature).
    Approved,
    /// Declined. A decline closes the proposal (it leaves the Proposed-only
    /// pending read); the snapshot's `declined` list marks the decliner's
    /// roster row with this value.
    Declined,
}

/// A read-only view of a proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalView {
    /// The proposal id.
    pub id: ProposalId,
    /// The surface it targets.
    pub surface: Surface,
    /// The proposed transition.
    pub payload: Value,
    /// Approvals collected so far.
    pub approvals: usize,
    /// Approvals required (the group threshold, m).
    pub threshold: usize,
    /// Current lifecycle state.
    pub state: ProposalState,
    /// Whether the READING node already approved (reader-relative: the same
    /// proposal reads differently on different nodes). Additive with a
    /// default, so an older writer's view stays deserializable.
    #[serde(default)]
    pub approved_by_me: bool,
    /// Whether the READING node already declined — with `approved_by_me`
    /// this is the frontends' "my stance is cast" signal: gray the vote
    /// buttons instead of letting a click run into the engine's refusal
    /// (one member, one stance). Additive with a default.
    #[serde(default)]
    pub declined_by_me: bool,
    /// Ist-Stand: what the targeted state is NOW (engine-derived for the
    /// Organization edit ops, e.g. the ratified charter before a
    /// `set_charter`). Display data, never consensus input; "" = unknown.
    #[serde(default)]
    pub current: String,
    /// Soll-Stand: what the change would make it (the payload's `value`).
    /// Display data; "" = the payload carries no value.
    #[serde(default)]
    pub proposed: String,
    /// Per-member stance, one entry per roster member in roster order (the
    /// pending cards' voting pills). Chain governance reports the collected
    /// signatures; the single-operator path claims only what it knows —
    /// the local member's own vote, every peer honestly open. (A legacy
    /// log whose counter once simulated peers keeps its `approvals` count,
    /// but the anonymous extras are attributed to nobody.)
    #[serde(default)]
    pub votes: Vec<MemberVote>,
    /// When the proposal was declined (unix seconds; 0 = not declined) —
    /// what the GUI's display-retention window filters the Declined view on.
    #[serde(default)]
    pub declined_at: u64,
    /// Who declined it ("" = not declined).
    #[serde(default)]
    pub declined_by: MemberId,
    /// Who proposed it ("" = unknown — see [`ProposalRecord::by`]).
    #[serde(default)]
    pub by: MemberId,
    /// Whether the READING node proposed it — the frontends' "pull back"
    /// visibility gate (reader-relative like `approved_by_me`). Display
    /// only: a withdraw execution must re-check the authenticated sender.
    #[serde(default)]
    pub mine: bool,
    /// Whether the base moved past this wiki patch and the supersede walk
    /// retired it (see [`ProposalRecord::superseded`]) — the Denied view
    /// labels it "superseded", never "declined by".
    #[serde(default)]
    pub superseded: bool,
    /// Whether the proposer pulled it back (see
    /// [`ProposalRecord::withdrawn`]) — labelled "pulled back".
    #[serde(default)]
    pub withdrawn: bool,
}

/// One chat channel as the engine enumerates it for the read contract
/// (chat-bus concept Q5): every distinct [`ChannelRef`] in the log, with
/// its message count and last activity — the sidebar's (and an agent's)
/// orientation data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// The channel.
    pub channel: ChannelRef,
    /// Messages filed under it.
    pub count: usize,
    /// Timestamp of its newest message (unix seconds; 0 when empty).
    pub last_ts: u64,
    /// For a `Patch` channel whose proposal this engine knows: the vote's
    /// lifecycle state — a terminal state means the discussion is closed
    /// (read-only). `None` for Group/Topic channels and for patch refs
    /// whose proposal is unknown here (chat-bus Q4: those stay writable).
    /// Additive (`#[serde(default)]`), so older snapshots read as `None`.
    #[serde(default)]
    pub state: Option<ProposalState>,
    /// B2: messages in this channel AFTER the seat's read cursor. A channel
    /// without a cursor counts fully unread — a cursor whose message was
    /// compacted away means the seat has not read the channel since before
    /// the retention horizon, so that is honest too. Additive.
    #[serde(default)]
    pub unread: usize,
}

/// **What the GUI window is showing right now** (`docs_archive/ui/gui_over_mcp.md`).
///
/// Published by the GUI at the end of every live-mirror pass — the same
/// place it applies a bundle, so this describes what was RENDERED, not what
/// the engine holds. That difference is the whole point: three chat bugs in
/// a row were the pane disagreeing with the engine, and nothing could see
/// it.
///
/// Row counts and a few strings, deliberately. A widget tree would make
/// every test brittle against layout; what a test needs to ask is "does the
/// pane hold what the engine holds".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    /// Whether a GUI is attached at all. `false` on a headless node — and
    /// then every field below is meaningless rather than merely empty.
    #[serde(default)]
    pub attached: bool,
    /// Unix seconds of the publish, so a reader can tell a live snapshot
    /// from one left behind by a window that stopped updating.
    #[serde(default)]
    pub at: u64,
    /// The screen the window is on (`molt_core::Screen::as_str`).
    #[serde(default)]
    pub screen: String,
    /// The selected surface key and sub-view key.
    #[serde(default)]
    pub surface: String,
    /// The sub-view key.
    #[serde(default)]
    pub view: String,
    /// The chat pane, as rendered.
    #[serde(default)]
    pub chat: UiChat,
    /// Per-surface row counts the nav and panes show.
    #[serde(default)]
    pub surfaces: Vec<UiSurface>,
    /// The toast currently up ("" = none).
    #[serde(default)]
    pub toast: String,
}

/// The chat pane as the window renders it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiChat {
    /// The channel key the pane is filtered to (`group`, `topic:…`, `patch:…`).
    #[serde(default)]
    pub channel: String,
    /// How many message rows the pane holds.
    #[serde(default)]
    pub rows: usize,
    /// The last few bodies, newest last — enough to tell "the right log"
    /// from "a log".
    #[serde(default)]
    pub tail: Vec<String>,
    /// Whether the compose row is on screen (a read-only discussion hides it).
    #[serde(default)]
    pub can_write: bool,
    /// The channel rows the sidebar offers, by key.
    #[serde(default)]
    pub channels: Vec<String>,
}

/// One surface as the window renders it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSurface {
    /// The surface key.
    #[serde(default)]
    pub key: String,
    /// Rows in its applied/log pane.
    #[serde(default)]
    pub log_rows: usize,
    /// Pending decisions it shows.
    #[serde(default)]
    pub pending: usize,
    /// The sub-view keys its nav offers.
    #[serde(default)]
    pub views: Vec<String>,
}

/// A projected snapshot of one surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceSnapshot {
    /// Which surface.
    pub surface: Surface,
    /// Whether it is threshold-gated.
    pub gated: bool,
    /// The ordered log of applied transitions (for chat, the messages —
    /// possibly filtered by [`Command::ReadState`]'s `channel`).
    pub applied: Vec<Value>,
    /// The parallel id track: positionally matched to `applied`, each entry
    /// names the proposal that produced it. `None` = origin unknown (chat
    /// rows, entries restored from a pre-id dump). The payloads in `applied`
    /// stay untouched — readers that compare payloads keep working; this
    /// track only ADDS the back-link a frontend needs to reopen the vote's
    /// discussion (its `patch:<id>` channel). Additive with a default, so an
    /// older writer's snapshot stays deserializable.
    #[serde(default)]
    pub applied_ids: Vec<Option<u64>>,
    /// Proposals still pending against this surface.
    pub pending: Vec<ProposalView>,
    /// Number of declined (denied) proposals against this surface.
    #[serde(default)]
    pub denied: usize,
    /// The declined proposals themselves, newest decline first (the
    /// Organization → Declined view). Additive with a default, so an older
    /// writer's snapshot stays deserializable.
    #[serde(default)]
    pub declined: Vec<ProposalView>,
    /// The APPLIED proposals (highest id first): the Accepted table's row
    /// data — each view carries the voters a sealed block proves, which the
    /// raw `applied` payloads cannot. Join to the log via `applied_ids`.
    /// Additive with a default, so an older writer's snapshot stays
    /// deserializable.
    #[serde(default)]
    pub accepted: Vec<ProposalView>,
    /// Chat only: every channel in the log (always the full list, even on
    /// a filtered read; `Group` is always present). Empty on other surfaces.
    #[serde(default)]
    pub channels: Vec<ChannelInfo>,
    /// Chat only: the archive half of the retention window currently holds
    /// at least one visible message. Engine-computed with the same view
    /// predicate an `"archive"` read filters by, on EVERY chat read (so a
    /// frontend needs no extra archive read to offer/hide that view).
    /// Additive with a default, so an older writer's snapshot stays
    /// deserializable. `false` on other surfaces.
    #[serde(default)]
    pub has_archive: bool,
    /// Memory only: the shared wiki BASE — the deterministic fold of the
    /// applied wiki patches in chain order (`shared_memory_real.md`). The
    /// SAME projection the GUI renders and an MCP `read_state` serves
    /// (co-equality). Empty on other surfaces; additive with a default.
    #[serde(default)]
    pub wiki_tree: Vec<WikiDoc>,
    /// Memory only: the base REVISION (count of patches that actually
    /// applied) — the display-only staleness currency a pending card's
    /// `base_rev` compares against. Additive with a default.
    #[serde(default)]
    pub wiki_rev: u64,
}

/// One document of the folded Shared-Memory base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiDoc {
    /// Normalized relative path (`file.md` or `folder/file.md`).
    pub path: String,
    /// The full markdown content.
    pub content: String,
}

/// One block of the persistent chain as display data — the row a
/// Chain-History view renders ([`Reply::Chain`]). Display data, never
/// consensus input: the verified [`chain::ChainBlock`]s stay the truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainBlockView {
    /// The block's chain height. Synthetic pre-cut entries a pruned holder
    /// rebuilds from its checkpoint blob carry 0 (their real heights are
    /// not reconstructible — the blob folds them away).
    pub height: u64,
    /// "genesis" | "applied" | "membership" | "checkpoint"
    pub kind: String,
    /// The gated surface an applied block targets ("" otherwise).
    pub surface: String,
    /// Display payload: for applied blocks the payload JSON (so frontends
    /// render titles via their op-placeholder lexicon — language-neutral,
    /// like [`SurfaceSnapshot::applied`]); for membership "op member"; for
    /// checkpoint the upto; for genesis the republic name.
    pub payload: Value,
    /// The proposal id an applied block consumed (0 = none).
    pub proposal_id: u64,
    /// The m signers, roster order as on the block.
    pub signers: Vec<String>,
}

/// Per-surface counters for the status summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceStat {
    /// Which surface.
    pub surface: Surface,
    /// Whether it is threshold-gated.
    pub gated: bool,
    /// Number of applied transitions.
    pub applied: usize,
    /// Number of pending proposals.
    pub pending: usize,
}

/// One row of the Organization → Members table. The identity anchor
/// (`id` / `identity_pk`) is real on a ritual-founded workspace and empty
/// on demo/legacy ones; presence is real too — `last_seen` is the last time
/// this member was actually observed on the wire, and `presence` is aged
/// live from it ([`presence_state`]; a send-failure pins offline).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberView {
    /// Display handle (the member id).
    pub member: MemberId,
    /// Short fingerprint of the anchored identity key, for humans
    /// ("" when unanchored).
    pub id: String,
    /// The anchored Ed25519 identity public key, lowercase hex
    /// ("" when unanchored).
    pub identity_pk: String,
    /// Unix seconds this member was last actually observed on the wire
    /// ([`MemberInfo::NEVER`] = never seen by this install); prose is
    /// rendered client-side.
    #[serde(default)]
    pub last_seen: u64,
    /// 0 = online, 1 = stale, 2 = offline/unreachable — aged live from
    /// `last_seen` ([`presence_state`]; a send-failure pins 2).
    pub presence: u8,
    /// Pending proposals still awaiting THIS member's approval.
    pub open_proposals: usize,
    /// Files this member shared into the chat.
    pub uploads: usize,
    /// R4 split marker: non-empty when this member shares NO relay with
    /// some other member — the compact fault line, naming the counterpart
    /// and the relay that would bridge. Empty = no split.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub split: String,
    /// This member's vote-gated profile picture: the LOCAL file path the
    /// applied `set_member_image` materialized ("" = none, or cleared by an
    /// applied `remove_member_image`). Like the org logo, the reference
    /// every view shows is that file (`member_profiles_plan.md` §4).
    #[serde(default)]
    pub image: String,
    /// This member's vote-gated description ("" = none). Set by the seat
    /// itself through an applied `set_member_desc`, capped engine-side.
    #[serde(default)]
    pub description: String,
}

/// A live download's progress, per share (requester side): what the
/// Uploads table and an MCP `read_uploads` render while bytes move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadView {
    /// `"requested" | "transferring" | "done" | "failed"`.
    pub phase: String,
    /// 0..=100 while transferring (piece counts are known up front).
    pub percent: u8,
    /// The final local path, set when done.
    pub path: String,
    /// The honest reason, set when failed.
    pub error: String,
}

/// One file shared into the chat (Shared Files → Uploads). Only metadata
/// travels in the chat — the bytes stay on the sharer's disk and move
/// user-to-user over a dedicated encrypted queue when a member downloads
/// ([`FileMeta`]), which is why a download needs the sharer online.
/// Uploads are ephemeral exactly like chat: past `expires_ts` (the chat
/// retention window after the share) the row leaves every read surface
/// and a download is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadView {
    /// The carrying chat message — the address `download_file` takes.
    pub id: MessageId,
    /// Who shared it.
    pub member: MemberId,
    /// When it was shared (unix seconds).
    pub ts: u64,
    /// File name.
    pub name: String,
    /// Display type, e.g. `"PDF"`.
    pub kind: String,
    /// Size in bytes.
    pub size: u64,
    /// Still present on the sharer's disk.
    pub available: bool,
    /// When the share ages out of the read contract (unix seconds): `ts` +
    /// the org's effective chat retention window — the same knob chat
    /// filters on. 0 = unknown age (`ts` 0), no deadline.
    pub expires_ts: u64,
    /// Whether the sharer is reachable right now — a user-to-user transfer
    /// needs the sharer online. Derived from the same real presence as the
    /// Members table (`presence != offline`); the own node is always online.
    /// Additive with a default.
    #[serde(default)]
    pub online: bool,
    /// Content checksum, lowercase sha256 hex — the sharer's log-anchored
    /// [`FileMeta::checksum`] a download must reproduce. "" on legacy
    /// shares (honestly unknown). Additive with a default.
    #[serde(default)]
    pub checksum: String,
    /// This node's live download of the share, if any (requester side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<DownloadView>,
    /// ONE status word for the relay plane (`file_transfer_nostr.md` §5.5):
    /// `"relay-held"` (a live series stamp is known — downloads need no
    /// live sharer), `"sharer-only"` (the first download wakes the
    /// sharer), `"gone"` (withdrawn and not on the relays). Additive.
    #[serde(default)]
    pub availability: String,
    /// A vote pinned this share (`persistent_uploads.md` D1): it never
    /// expires and lists under Persistent Uploads, not Temporary.
    #[serde(default)]
    pub persistent: bool,
    /// Members holding the whole series, the sharer included
    /// (`docs_archive/files/mirroring.md` §3.4).
    #[serde(default)]
    pub mirrors: u32,
    /// Data pieces THIS seat holds verified.
    #[serde(default)]
    pub mirror_held: u32,
    /// The series' data piece count (0 = a legacy share).
    #[serde(default)]
    pub mirror_of: u32,
}

/// Who mirrors what ([`Command::ReadMirror`], `docs_archive/files/mirroring.md`
/// §3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorView {
    /// This seat mirrors persistent files.
    pub on: bool,
    /// This seat's budget, bytes.
    pub quota: u64,
    /// Bytes this seat's mirror folder holds.
    pub used: u64,
    /// The mirror folder in use (`prefs.mirror_dir`, else the default).
    #[serde(default)]
    pub dir: String,
    /// Every roster member's declaration.
    pub members: Vec<MirrorMemberView>,
    /// Every share and its holders.
    pub files: Vec<MirrorFileView>,
}

/// One member's declaration as this seat knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorMemberView {
    /// The member.
    pub member: MemberId,
    /// Whether a declaration was ever heard (false = unknown).
    pub known: bool,
    /// Consent.
    pub on: bool,
    /// Budget, bytes.
    pub quota: u64,
    /// Revision of the declaration.
    pub rev: u64,
}

/// One share's holders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorFileView {
    /// The share message's stable id.
    pub id: MessageId,
    /// The file name.
    pub name: String,
    /// Members holding the whole series, the sharer included.
    pub holders: Vec<MemberId>,
    /// Data pieces THIS seat holds verified.
    pub held: u32,
    /// The series' data piece count.
    pub of: u32,
}

/// A one-shot status summary of the running group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    /// This node's member handle.
    pub member: MemberId,
    /// The full member set.
    pub members: Vec<MemberId>,
    /// The approval threshold (m of n).
    pub threshold: usize,
    /// Per-surface counters.
    pub surfaces: Vec<SurfaceStat>,
    /// Unix seconds of the founding — the genesis envelope's timestamp
    /// (real on replayed workspaces; 0 on pre-ritual/demo groups).
    #[serde(default)]
    pub founded_ts: u64,
    /// Members actually seen on the wire within the last hour (real
    /// last-seen stamps; the local member always counts — it is reading
    /// this). Never-seen members count in no window.
    #[serde(default)]
    pub active_1h: usize,
    /// Members seen within the last 24 h (real stamps).
    #[serde(default)]
    pub active_24h: usize,
    /// Members seen within the last 7 days (real stamps).
    #[serde(default)]
    pub active_7d: usize,
    /// The republic's current image: the file reference of the last applied
    /// `set_image` Organization change ("" = none, or cleared by an applied
    /// `remove_image`). Like a chat file share, only the reference travels —
    /// the bytes stay on the proposer's disk until the transfer story.
    #[serde(default)]
    pub image: String,
    /// The republic's EFFECTIVE display name: the last applied `set_name`
    /// Organization change, the ratified founding name until one applies
    /// ("" on pre-ritual/demo groups). The `republic_id` stays the
    /// content-derived founding value — a rename never changes identity.
    #[serde(default)]
    pub name: String,
    /// The EFFECTIVE charter: the last applied `set_charter` over the
    /// ratified founding agenda. The genesis block keeps the founding
    /// charter immutably; this is the fold on top.
    #[serde(default)]
    pub agenda: String,
    /// The EFFECTIVE "delete chat after" window in days: the last applied
    /// `set_chat_retention`, default 7. The read contract (`ReadState`)
    /// hides chat messages and declined proposals older than this.
    #[serde(default = "default_chat_retention_days")]
    pub chat_retention_days: u64,
    /// The GROUP's relay pool — the relays every member must be able to
    /// reach (`nostr_transport_marmot.md` §10.15). Empty on a legacy
    /// queue-shaped republic, which has no relays.
    ///
    /// A group setting like the name or the retention window, and shown
    /// beside them: relays do not federate, so a member who cannot reach one
    /// of these is partitioned, and the pool is the first thing to look at
    /// when that happens.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Whether the open workspace is a chain-governed republic. Recovery
    /// (link mint + rejoin) exists only here — a frontend offers the
    /// per-member "recovery link" action exactly when this is true (demo
    /// and pre-chain workspaces have no chain for a rejoiner to verify).
    #[serde(default)]
    pub chain_governed: bool,
    /// The EFFECTIVE feature set (`charter_features.md`): the ratified
    /// founding selection unioned with every applied `set_features` edit,
    /// sorted. Drives which optional surfaces the nav shows — and the
    /// engine refuses selecting/proposing on a surface not in it, so an
    /// MCP agent meets the same gate the nav renders (co-equality).
    #[serde(default)]
    pub features: Vec<String>,
    /// Decoded image bytes a `set_member_image` proposal can still carry in
    /// THIS republic (the derived transport headroom, which shrinks as the
    /// roster grows). The honest downscale target for a frontend fitting a
    /// picture before proposing — the engine stays the authority.
    #[serde(default)]
    pub image_budget: u64,
}

/// The chat-retention default: 7 days, the window every republic starts
/// with until a gated `set_chat_retention` change applies.
pub fn default_chat_retention_days() -> u64 {
    7
}

/// The group configuration the engine runs under.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupConfig {
    /// This node's member handle.
    pub member: MemberId,
    /// The member set (n).
    pub members: Vec<MemberId>,
    /// The approval threshold (m of n).
    pub threshold: usize,
    /// If true, a proposer's own proposal counts as their approval up front.
    pub self_cosign: bool,
}

impl GroupConfig {
    /// A demo 2-of-3 group — **test fixture only**. The product boots with
    /// [`GroupConfig::solo`]; a real group's roster comes from its founded
    /// workspace, never from this constructor.
    pub fn demo() -> Self {
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string(), "peer-1".to_string(), "peer-2".to_string()],
            threshold: 2,
            self_cosign: true,
        }
    }

    /// The honest boot group of a node with no workspace open: just this
    /// operator, no peers. Nothing in this context can pretend other
    /// members exist — an open workspace replaces it with the real roster
    /// from its genesis.
    pub fn solo() -> Self {
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: true,
        }
    }
}

/// Where a file download stands ([`Event::FileTransfer`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TransferPhase {
    /// The fetch request went out to the sharer.
    Requested,
    /// Bytes are moving.
    Progress {
        /// 0..=100.
        percent: u8,
    },
    /// Landed and verified.
    Done {
        /// The final local path.
        path: String,
    },
    /// Failed — the honest reason.
    Failed {
        /// Human-readable reason.
        reason: String,
    },
}

/// Events broadcast to every attached operator (GUI live-mirror, MCP stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The engine stored a GUI interaction request
    /// (`Command::UiAction`) — the window's live mirror performs it and
    /// publishes a fresh [`UiSnapshot`].
    UiActionRequested {
        /// The requested interaction.
        action: UiAction,
    },
    /// WP4b: a checkpoint block sealed — the summarized history below
    /// `upto` was dropped locally; frontends refresh their chain/status
    /// views and a waiting proposer gets closure.
    CheckpointSealed {
        /// The checkpoint block's height.
        height: u64,
        /// The cut it attests (last folded-in block).
        upto: u64,
    },
    /// WP4b: a pending checkpoint cut went STALE — another block sealed
    /// first, the cut cannot commit anymore; the proposer re-proposes at
    /// the new head.
    CheckpointStale {
        /// The stale checkpoint proposal's id.
        id: ProposalId,
    },
    /// A file download's lifecycle (requester side; [`TransferPhase`]):
    /// kicked off, moving, landed, or failed — what a GUI toasts and
    /// re-reads uploads on.
    FileTransfer {
        /// The share message's stable id.
        id: MessageId,
        /// Where the transfer stands.
        phase: TransferPhase,
    },
    /// A chat message was posted.
    Chat {
        /// The message's stable id.
        id: MessageId,
        /// Sender.
        from: MemberId,
        /// Body.
        body: String,
        /// The channel it files under.
        channel: ChannelRef,
    },
    /// A chat message was deleted (wiped and tombstoned).
    Deleted {
        /// The message's stable id.
        id: MessageId,
        /// Who deleted it.
        by: MemberId,
    },
    /// A member's reaction on a chat message was toggled.
    Reacted {
        /// The message's stable id.
        id: MessageId,
        /// The reaction emoji.
        emoji: String,
        /// Who toggled it.
        by: MemberId,
    },
    /// A member confirmed reading chat messages (read receipts) — the newly
    /// recorded ids only.
    Read {
        /// The messages now marked read by `by`.
        ids: Vec<MessageId>,
        /// Who read them.
        by: MemberId,
    },
    /// A shared file became unavailable (its sharer deleted it locally).
    FileRemoved {
        /// The share message's stable id.
        id: MessageId,
        /// The sharer.
        by: MemberId,
    },
    /// A proposal was created.
    Proposed {
        /// The proposal id.
        id: ProposalId,
        /// The surface.
        surface: Surface,
        /// Who this node first learned the proposal from: the local member
        /// on an own proposal, the authenticated wire sender on a peer's
        /// (normally the proposer; on a WP2 catch-up re-serve, the serving
        /// peer). Lets a frontend keep its alert sound for votes somebody
        /// ELSE initiated; duplicates never re-emit.
        by: MemberId,
    },
    /// A proposal gained an approval.
    Approved {
        /// The proposal id.
        id: ProposalId,
        /// Approvals collected.
        have: usize,
        /// Approvals required.
        need: usize,
    },
    /// A proposal reached the threshold and was applied.
    Applied {
        /// The proposal id.
        id: ProposalId,
        /// The surface that changed.
        surface: Surface,
    },
    /// One member declined a proposal that is STILL pending (their decline
    /// is a voice against, not a veto — the threshold stays reachable).
    /// Terminal rejection is [`Event::Rejected`].
    Declined {
        /// The proposal id.
        id: ProposalId,
        /// Who declined.
        by: MemberId,
    },
    /// The proposer pulled the proposal back — terminal, no vote forged.
    Withdrawn {
        /// The proposal id.
        id: ProposalId,
    },
    /// A proposal was rejected for good: enough members declined that
    /// approval can no longer reach the threshold (declines > n − m).
    Rejected {
        /// The proposal id.
        id: ProposalId,
    },
    /// The shared session state changed. Both operators re-read it via
    /// [`Command::ReadSession`]; `scope` tells a mirror how much to
    /// re-render (the run tickers fire every 90 ms — repainting everything
    /// on each tick is the kind of churn that costs focus and scroll state).
    SessionChanged {
        /// How much of the session changed.
        scope: SessionScope,
    },
    /// A poke — emitted on the poker's node (`by` = this seat) as the send
    /// confirmation and on the target's node (`to` = this seat) as the
    /// nudge a GUI toasts and sounds. Never re-emitted on replay.
    Poked {
        /// The poking member.
        by: MemberId,
        /// The poked member.
        to: MemberId,
    },
}

/// The reach of a [`Event::SessionChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionScope {
    /// Anything could have changed — re-render every mirror.
    Full,
    /// Only the restore run advanced.
    Restore,
    /// Only the founding run advanced.
    Create,
    /// Only the join run advanced.
    Join,
}

/// Errors returned to an operator.
#[derive(Debug, thiserror::Error)]
pub enum MoltError {
    /// The named proposal does not exist.
    #[error("unknown proposal {0:?}")]
    UnknownProposal(ProposalId),
    /// A proposal was attempted on an ungated surface (chat, files).
    #[error("{} is ungated - nothing to propose", .0.as_str())]
    NotGated(Surface),
    /// The proposal payload was malformed.
    #[error("bad payload: {0}")]
    BadPayload(String),
    /// An optional surface the charter has not enabled
    /// (`charter_features.md` D7) — selecting or proposing on it is
    /// refused engine-side, the co-equal twin of the nav hiding it.
    #[error("{0}: not enabled")]
    FeatureDisabled(&'static str),
    /// The proposal is already in a terminal state.
    #[error("proposal {0:?} is already {1:?}")]
    AlreadyTerminal(ProposalId, ProposalState),
    /// A withdraw by someone other than the recorded proposer.
    #[error("proposal {0:?}: only the proposer pulls back")]
    NotTheProposer(ProposalId),
    /// A repeated `Approve` in a context without chain governance. This
    /// node contributes exactly ONE real approval — its own; it never
    /// counts invented approvals on behalf of other members. The missing
    /// approvals must come from the members themselves, which takes a
    /// chain-governed republic (real signed m-of-n over the mesh).
    #[error("proposal {0:?} already carries this node's approval - the others must approve themselves")]
    AlreadyApproved(ProposalId),
    /// A repeated `Decline` by the same member — one voice per member; the
    /// proposal rejects only when enough DISTINCT members decline.
    #[error("proposal {0:?} already carries this member's decline")]
    AlreadyDeclined(ProposalId),
    /// A write into the discussion channel of a decided vote (the
    /// discussion stays readable, linked from the vote's card — but the
    /// deliberation ended with the vote).
    #[error("discussion of proposal {0:?} is read-only - the vote is {1:?}")]
    DiscussionClosed(ProposalId, ProposalState),
    /// A settings value failed validation (nothing was stored or written).
    #[error("settings: {0}")]
    Settings(String),
    /// The named workspace is not in the local list.
    #[error("unknown workspace `{0}`")]
    UnknownWorkspace(String),
    /// The workspace is already open (locally or by another process).
    #[error("workspace is busy: {0}")]
    WorkspaceBusy(String),
    /// The workspace is encrypted at rest — decrypt it first.
    #[error("workspace `{0}` is encrypted - decrypt it first")]
    WorkspaceEncrypted(String),
    /// A storage operation failed (I/O, corruption, wrong key, …).
    #[error("storage: {0}")]
    Storage(String),
    /// The named sub-view does not exist on the given surface.
    #[error("surface {0:?} has no view `{1}`")]
    UnknownView(Surface, String),
    /// The chat log has no message with this id.
    #[error("unknown chat message {0}")]
    UnknownMessage(MessageId),
    /// The chat message with this id carries no shared file.
    #[error("message {0} has no shared file")]
    NoFile(MessageId),
    /// The shared file's owner deleted it locally; nothing to download.
    #[error("the shared file at message {0} is no longer available")]
    FileUnavailable(MessageId),
    /// The share aged out of the chat retention window — uploads are
    /// ephemeral exactly like chat, so an expired share is not downloadable.
    #[error("the shared file at message {0} aged out of the chat retention window")]
    FileExpired(MessageId),
    /// Only the member who shared a file can remove it.
    #[error("only the member who shared the file at message {0} can remove it")]
    NotYourFile(MessageId),
    /// Only the author of a chat message can delete it (there is no
    /// moderation concept — the same rule every peer enforces on the wire).
    #[error("only the author of message {0} can delete it")]
    NotYourMessage(MessageId),
    /// A restore action arrived in the wrong lifecycle state.
    #[error("restore: {0}")]
    Restore(String),
    /// A founding action was invalid or arrived in the wrong lifecycle state.
    #[error("create: {0}")]
    Create(String),
    /// A join action was invalid or arrived in the wrong lifecycle state.
    #[error("join: {0}")]
    Join(String),
    /// A recovery action was invalid or arrived in the wrong lifecycle state.
    #[error("recover: {0}")]
    Recover(String),
    /// A poke was refused (feature off, unknown target, or self-poke).
    #[error("poke: {0}")]
    Poke(&'static str),
    /// A wiki export was refused (empty wiki, no target, no chain behind a
    /// proof, or one already running).
    #[error("wiki export: {0}")]
    WikiExport(&'static str),
    /// The engine task is gone or did not answer.
    #[error("engine: {0}")]
    Engine(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TransportState v4 (N4: the Nostr transport shape) ----------------

    /// A v3-era `transport.state` (no `kind`, no relay fields) must load with
    /// every N4 field at its default — the additive-evolution rule that lets
    /// every pre-N4 workspace open unchanged.
    #[test]
    fn a_v3_transport_state_loads_with_the_n4_fields_defaulted() {
        let v3 = serde_json::json!({
            "version": 3,
            "outbound": {},
            "inbound": {},
            "mls": null,
            "mesh": [],
            "smp_queues": null,
            "identity_sk": [1, 2, 3],
            "nostr_sk": null
        });
        let st: TransportState = serde_json::from_value(v3).expect("v3 state loads");
        assert_eq!(st.kind, None, "no discriminator on a legacy file");
        assert!(st.relays.is_empty());
        assert_eq!(st.rotation_seed, None);
        assert!(st.relay_cursors.is_empty());
        assert_eq!(st.identity_sk, Some(vec![1, 2, 3]));
    }

    /// The v4 fields round-trip, and a default (legacy-shaped) state
    /// serializes WITHOUT the new keys — old-shaped output stays old-shaped.
    #[test]
    fn transport_state_v4_round_trips_and_defaults_stay_invisible() {
        let mut st = TransportState {
            version: TRANSPORT_STATE_VERSION,
            kind: Some(TransportKind::Nostr),
            relays: vec!["wss://relay.example".to_string()],
            rotation_seed: Some(vec![7u8; 32]),
            ..TransportState::default()
        };
        st.relay_cursors.insert("wss://relay.example".to_string(), 1_753_900_000);
        let json = serde_json::to_value(&st).expect("encode");
        let back: TransportState = serde_json::from_value(json).expect("decode");
        assert_eq!(back, st, "v4 fields survive the round trip");

        let legacy = serde_json::to_value(TransportState::default()).expect("encode default");
        let obj = legacy.as_object().expect("object");
        for key in ["kind", "relays", "rotation_seed", "relay_cursors"] {
            assert!(
                !obj.contains_key(key),
                "default state must not serialize `{key}` — legacy shape preserved"
            );
        }
    }

    /// Version 4 is the current marker (v4 added `kind`/`relays`/
    /// `rotation_seed`/`relay_cursors`); the storage read gate accepts
    /// `<=` current, so this pin catches an unbumped field addition.
    #[test]
    fn transport_state_version_is_4() {
        assert_eq!(TRANSPORT_STATE_VERSION, 4);
    }

    // ---- Tor connectivity test (the honest ladder) ------------------------

    /// The rung vocabulary is a CONTRACT: the GUI's copy and the MCP tool
    /// description are both keyed on these exact strings, and `as_str` must
    /// never drift from the serde tag (a mismatch would show one surface a
    /// state the other cannot name).
    #[test]
    fn tor_test_state_keys_match_the_wire_tags() {
        for state in [
            TorTestState::Idle,
            TorTestState::Testing,
            TorTestState::Off,
            TorTestState::Misconfigured,
            TorTestState::NoProxy,
            TorTestState::ProxyOnly,
            TorTestState::NoTarget,
            TorTestState::CircuitFailed,
            TorTestState::Circuit,
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(
                json,
                format!("\"{}\"", state.as_str()),
                "as_str drifted from the serde tag for {state:?}"
            );
        }
        // exactly ONE state means "Tor works" — the whole point of the ladder
        assert!(TorTestState::ProxyOnly != TorTestState::Circuit);
        assert!(TorTestState::CircuitFailed != TorTestState::Circuit);
    }

    /// A fresh session has never probed Tor — and an older reader meeting a
    /// view without the field lands in exactly that state (additive-only).
    #[test]
    fn a_fresh_session_has_no_tor_verdict() {
        let sv = SessionView::default();
        assert_eq!(sv.tor_test, TorTest::default());
        assert_eq!(sv.tor_test.state, TorTestState::Idle);
        let old: SessionView = serde_json::from_value(
            serde_json::to_value(&sv)
                .expect("to value")
                .as_object_mut()
                .map(|m| {
                    m.remove("tor_test");
                    serde_json::Value::Object(m.clone())
                })
                .expect("object"),
        )
        .expect("a view without tor_test still reads");
        assert_eq!(old.tor_test.state, TorTestState::Idle);
    }

    // ---- AcceptedWindow (delivery guarantee §4.2) -------------------------

    /// Basics: nothing accepted at birth; a fresh seq accepts once and dups
    /// after; a forward jump marks the previous high as accepted below it.
    #[test]
    fn accepted_window_accepts_once_and_marks_the_old_high() {
        let mut w = AcceptedWindow::default();
        assert!(!w.is_accepted(1), "nothing accepted at birth");
        assert!(w.accept(5), "first sight is fresh");
        assert!(!w.accept(5), "the high itself dups");
        assert!(w.accept(9), "forward jump is fresh");
        assert!(w.is_accepted(5), "the old high stays accepted after the jump");
        assert!(!w.is_accepted(7), "an unseen seq between marks is not accepted");
        assert!(w.accept(7), "an in-window fill is fresh");
        assert!(!w.accept(7), "and dups after");
        assert!(!w.is_accepted(6), "its neighbors stay unaccepted");
        assert!(!w.is_accepted(10), "above the high is never accepted");
    }

    /// Aging: seqs pushed below the window read as accepted (conservative —
    /// W is large against the resend cadence), and `accept` refuses them.
    #[test]
    fn accepted_window_ages_out_below_the_window() {
        let mut w = AcceptedWindow::default();
        assert!(w.accept(1));
        assert!(w.accept(2 + ACCEPT_WINDOW_BITS), "jump the window forward");
        assert!(w.is_accepted(1), "aged out reads as accepted");
        assert!(!w.accept(1), "and cannot be re-accepted");
        // seq 2 sits exactly at the window's edge (offset = W - 1): its mark
        // survived the shift as a real bit — but it was never accepted
        assert!(!w.is_accepted(2), "the oldest in-window seq is honest");
        assert!(w.accept(2), "and still fillable");
    }

    /// Shifts across word boundaries (1, 63, 64, 65) keep every mark exact.
    #[test]
    fn accepted_window_shifts_keep_marks_across_word_boundaries() {
        for step in [1u64, 63, 64, 65, 127, 128] {
            let mut w = AcceptedWindow::default();
            let mut expected = Vec::new();
            let mut seq = 1;
            for _ in 0..6 {
                assert!(w.accept(seq), "fresh at step {step}");
                expected.push(seq);
                seq += step;
            }
            for &s in &expected {
                assert!(w.is_accepted(s), "seq {s} survives shifting by {step}");
            }
            // spot-check unseen neighbors stay unaccepted (in-window only)
            if step > 1 {
                assert!(
                    !w.is_accepted(expected[4] + 1),
                    "unseen neighbor at step {step} stays unaccepted"
                );
            }
        }
    }

    /// A jump wider than the whole window wipes every mark (all below read
    /// as aged-accepted) — no stale bit may survive under a new offset.
    #[test]
    fn accepted_window_survives_a_jump_wider_than_itself() {
        let mut w = AcceptedWindow::default();
        for s in 1..=10 {
            assert!(w.accept(s));
        }
        let far = 10 + 3 * ACCEPT_WINDOW_BITS;
        assert!(w.accept(far));
        assert_eq!(w.high, far);
        assert!(w.is_accepted(10), "below the window: aged-accepted");
        // everything inside the fresh window is honestly unaccepted
        assert!(!w.is_accepted(far - 1));
        assert!(!w.is_accepted(far - ACCEPT_WINDOW_BITS + 1));
    }

    /// G7: `prev_seq` is wire/log-ADDITIVE — zero (legacy, chain start, or a
    /// pre-G7 writer) serializes away so every existing byte fixture, log
    /// frame and hash stays identical; a chained value round-trips; and a
    /// pre-G7 envelope without the field parses to the unordered default.
    #[test]
    fn prev_seq_is_byte_invisible_at_zero_and_roundtrips_otherwise() {
        let env = |prev| EventEnvelope {
            seq: 7,
            ts: 1,
            by: "a".to_string(),
            body: WorkspaceEvent::MemberJoined { member: "b".to_string() },
            prev_seq: prev,
        };
        let legacy = serde_json::to_string(&env(0)).expect("json");
        assert!(
            !legacy.contains("prev_seq"),
            "zero serializes away — pre-G7 bytes stay identical: {legacy}"
        );
        let chained = serde_json::to_string(&env(5)).expect("json");
        assert!(chained.contains("\"prev_seq\":5"), "a chain value travels: {chained}");
        let back: EventEnvelope = serde_json::from_str(&chained).expect("parse");
        assert_eq!(back.prev_seq, 5);
        let old: EventEnvelope = serde_json::from_str(&legacy).expect("parse legacy");
        assert_eq!(old.prev_seq, 0, "a pre-G7 envelope reads as unordered");
    }

    /// E7 review finding 2: `seq 0` is not a valid log seq (logs start at 1),
    /// and a crafted envelope carrying it must be a plain duplicate-reject —
    /// never the `high - 1 - seq` underflow panic that would kill the engine
    /// actor (workspace profiles set overflow-checks).
    #[test]
    fn accepted_window_rejects_seq_zero_without_panicking() {
        let mut w = AcceptedWindow::default();
        assert!(!w.accept(0), "seq 0 on a fresh window is rejected, not a panic");
        assert!(w.accept(1), "real seqs still work");
        assert!(!w.accept(0), "seq 0 with a high set is still rejected");
        assert!(!w.is_accepted(0), "and never reads as accepted");
    }

    /// Serde-additivity: an old `transport.state` (no `accepted`, empty
    /// `bits`) deserializes to a working window.
    #[test]
    fn accepted_window_tolerates_missing_fields() {
        let w: AcceptedWindow =
            serde_json::from_str("{\"high\":7}").expect("short form parses");
        assert!(w.is_accepted(7));
        assert!(!w.is_accepted(6), "no bits = nothing marked below the high");
        let ts: TransportState = serde_json::from_str("{\"version\":2}")
            .expect("an old transport.state parses");
        assert!(ts.accepted.is_empty(), "additive default");
    }

    /// The single source of truth for the "which network am I on" display
    /// label: only a configured "tor" reads as tor; "none", the retired
    /// "nym", and any unknown value read as "none" (they never dial — an
    /// unknown network fails the dialer resolution closed). The retired
    /// value cannot reach a running node any more, but the LABEL keeps
    /// answering for it: a display that panics on old data is worse than
    /// one that tells the truth about it.
    #[test]
    fn effective_net_label_maps_only_tor_to_tor() {
        assert_eq!(effective_net_label("tor"), "tor");
        assert_eq!(effective_net_label("none"), "none");
        assert_eq!(effective_net_label("nym"), "none");
        assert_eq!(effective_net_label("garbage"), "none");
        assert_eq!(effective_net_label(""), "none");
    }

    /// Every Reply variant must serialize (the MCP surface renders replies
    /// as JSON text): the internally-tagged repr cannot hold a bare
    /// sequence, so the list replies must stay struct variants.
    #[test]
    fn every_reply_variant_serializes_to_json() {
        let replies = [
            Reply::Ack,
            Reply::Proposed { id: ProposalId(1) },
            Reply::Proposals { proposals: vec![] },
            Reply::Members { members: vec![] },
            Reply::Uploads { uploads: vec![] },
            Reply::Session(Box::default()),
            Reply::Chain { blocks: vec![] },
        ];
        for r in replies {
            let json = serde_json::to_string(&r);
            assert!(json.is_ok(), "reply failed to serialize: {r:?} → {json:?}");
        }
    }

    #[test]
    fn invite_roundtrip_and_rejects() {
        let inv = InviteInfo {
            republic: "Chess Club".to_string(),
            threshold: 2,
            members: 3,
            inviter: "walter".to_string(),
            ticket: "k9x2m4q7aa".to_string(),
        };
        let link = inv.render();
        assert_eq!(link, "molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa");
        assert_eq!(InviteInfo::parse(&link), Some(inv.clone()));

        // a real founding link appends a transport-handover segment after the
        // ticket (molt_engine::FoundingInvite); the preview ignores it
        assert_eq!(
            InviteInfo::parse("molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa/deadbeef"),
            Some(inv)
        );

        for bad in [
            "",
            "molt://invite/",
            "smp://not-an-invite",
            "molt://invite/X/3of2/w/abcdefgh", // threshold > members
            "molt://invite/X/2of3/w/ab",       // ticket too short
        ] {
            assert_eq!(InviteInfo::parse(bad), None, "should reject `{bad}`");
        }
    }

    /// BYTE-IDENTITY PIN — `molt-roster-v4`: the third anchor (N1), the
    /// relay pool and a length-prefixed `ws_id` (R3).
    /// Layout: tag ‖ ws_id ‖ m ‖ n ‖ per member (le32-len name ‖ le32-len
    /// identity_pk ‖ le32-len nostr_pk) ‖ le32-len agenda. The digest was
    /// computed INDEPENDENTLY of this codebase (python hashlib over the
    /// specified layout) — if this test disagrees with the implementation,
    /// the implementation is wrong, not the fixture. Changing the layout
    /// means a NEW version tag and a new independently-computed fixture.
    #[test]
    fn roster_canonical_bytes_v4_binds_anchor_pool_and_id_byte_exactly() {
        use sha2::Digest;
        let table = vec![
            MemberIdentity {
                member: "ada".to_string(),
                identity_pk: "aa".repeat(32),
                nostr_pk: "cc".repeat(32),
            },
            MemberIdentity {
                member: "bob".to_string(),
                identity_pk: "bb".repeat(32),
                nostr_pk: "dd".repeat(32),
            },
        ];
        let bytes = roster_canonical_bytes("f00", 2, 3, &table, "charter", &[], None);
        assert!(bytes.starts_with(b"molt-roster-v4\0"), "version tag bumped");
        assert_eq!(bytes.len(), 329, "fixture length");
        assert_eq!(
            hex::encode(sha2::Sha256::digest(&bytes)),
            "471faf57994f951dbbb7c57811122b296cdff8885ad41d0323997c5ea795116e",
            "independently computed byte-identity fixture"
        );
        // v4: the RELAY POOL is inside the signed bytes. A founder cannot seal
        // a pool different from the one everybody ratified.
        let pooled = roster_canonical_bytes(
            "f00",
            2,
            3,
            &table,
            "charter",
            &["wss://a.example".to_string(), "wss://b.example".to_string()],
            None,
        );
        assert_ne!(pooled, bytes, "the pool changes what every member signs");
        assert_eq!(pooled.len(), 367, "fixture length with two relays");
        assert_eq!(
            hex::encode(sha2::Sha256::digest(&pooled)),
            "035242b703cbeaaf5907f5bf7788fc919887b8cb43d9478ab3e9394a5847a625",
            "independently computed byte-identity fixture (pool)"
        );
        // …and the pool ORDER is part of the agreement: the order IS the dial
        // priority, so two orderings are two different pools, not one sorted
        assert_ne!(
            pooled,
            roster_canonical_bytes(
                "f00",
                2,
                3,
                &table,
                "charter",
                &["wss://b.example".to_string(), "wss://a.example".to_string()],
                None,
            ),
            "relay order is signed, not normalized away"
        );
        // INJECTIVITY: an uncounted member run collides once a field follows
        // the agenda. These two tables differ in every meaningful way and must
        // never share a byte form — they DID before the member count.
        assert_ne!(
            roster_canonical_bytes(
                "f00",
                2,
                3,
                &[MemberIdentity {
                    member: "ada".to_string(),
                    identity_pk: "\u{9}".to_string(),
                    nostr_pk: String::new(),
                }],
                "",
                &[],
                None,
            ),
            roster_canonical_bytes("f00", 2, 3, &[], "ada", &["\0\0\0\0\0\0\0\0\0".to_string()], None),
        );
        // v4 also length-prefixes ws_id, so the id/rule boundary is
        // self-delimiting: these two differ, and under v3 their prefixes
        // overlapped
        assert_ne!(
            roster_canonical_bytes("ab", 1, 2, &table, "charter", &[], None),
            roster_canonical_bytes("a", b'b', 1, &table, "charter", &[], None),
        );
        // the third anchor is inside the signed bytes: changing ONLY a
        // nostr_pk changes what every member signs
        let mut changed = table.clone();
        changed[0].nostr_pk = "dd".repeat(32);
        assert_ne!(bytes, roster_canonical_bytes("f00", 2, 3, &changed, "charter", &[], None));
        // a legacy (empty) nostr_pk still length-prefixes as 0 — no special
        // casing that could collide two different rosters onto one byte form
        let mut legacy = table.clone();
        legacy[0].nostr_pk = String::new();
        let legacy_bytes = roster_canonical_bytes("f00", 2, 3, &legacy, "charter", &[], None);
        assert_eq!(legacy_bytes.len(), 329 - 64);
        assert_ne!(legacy_bytes, bytes);
    }

    /// Shared Files is the seventh surface and a CORE one: never a feature
    /// key, gated (persist/unpersist are votes), its temporary and
    /// persistent tables plus the three outcome views — and the uploads
    /// table that used to live under Organization.
    #[test]
    fn shared_files_is_a_core_gated_surface_with_the_two_upload_views() {
        assert_eq!(Surface::ALL.last(), Some(&Surface::Files));
        assert_eq!(Surface::Files.as_str(), "files");
        assert_eq!(Surface::parse("files"), Some(Surface::Files));
        assert!(!Surface::Files.is_charter_feature());
        assert!(Surface::Files.is_gated());
        assert_eq!(
            Surface::Files.views().iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["uploads", "persistent", "pending", "accepted", "declined"]
        );
        assert_eq!(Surface::Files.default_view(), "uploads");
        assert!(
            !Surface::Organization.views().iter().any(|(k, _)| *k == "uploads"),
            "the uploads view moved out of Organization"
        );
        assert_eq!(
            canonical_features(&["files".to_string()]),
            Err("files is always on".to_string())
        );
        assert!(!Surface::LEGACY_FEATURES.contains(&"files"));
    }

    /// BYTE-IDENTITY PIN — `molt-roster-v5`: the ratified FEATURE SET
    /// (`charter_features.md` D2). The expected bytes are recomputed
    /// INDEPENDENTLY, field by field — when this goes red the diff says
    /// which field moved. The conditional rule is load-bearing: `None`
    /// must stay byte-identical to v4 (the v4 pin above proves that), and
    /// `Some([])` must be a DIFFERENT table than `None`.
    #[test]
    fn roster_canonical_bytes_v5_binds_the_feature_set() {
        let table = vec![MemberIdentity {
            member: "ada".to_string(),
            identity_pk: "aa".repeat(32),
            nostr_pk: "cc".repeat(32),
        }];
        let features = vec!["memory".to_string(), "wallet".to_string()];
        let bytes =
            roster_canonical_bytes("f00", 1, 1, &table, "charter", &[], Some(&features));
        assert!(bytes.starts_with(b"molt-roster-v5\0"), "presence bumps the tag");

        // the independent recomputation
        let mut want = Vec::new();
        let put = |out: &mut Vec<u8>, b: &[u8]| {
            out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(b);
        };
        want.extend_from_slice(b"molt-roster-v5\0");
        put(&mut want, b"f00");
        want.push(1);
        want.push(1);
        want.extend_from_slice(&1u32.to_le_bytes());
        put(&mut want, b"ada");
        put(&mut want, "aa".repeat(32).as_bytes());
        put(&mut want, "cc".repeat(32).as_bytes());
        put(&mut want, b"charter");
        want.extend_from_slice(&0u32.to_le_bytes()); // relay run, empty
        want.extend_from_slice(&2u32.to_le_bytes()); // feature run
        put(&mut want, b"memory");
        put(&mut want, b"wallet");
        assert_eq!(bytes, want, "independently recomputed v5 layout");

        // the feature set changes what every member signs
        let other = vec!["memory".to_string()];
        assert_ne!(
            bytes,
            roster_canonical_bytes("f00", 1, 1, &table, "charter", &[], Some(&other)),
        );
        // an explicitly empty selection is NOT the legacy absence
        assert_ne!(
            roster_canonical_bytes("f00", 1, 1, &table, "charter", &[], Some(&[])),
            roster_canonical_bytes("f00", 1, 1, &table, "charter", &[], None),
            "Some([]) and None must never share a byte form"
        );
        // and the feature run cannot be absorbed by the relay run (injectivity)
        assert_ne!(
            roster_canonical_bytes(
                "f00",
                1,
                1,
                &table,
                "charter",
                &["memory".to_string()],
                Some(&[]),
            ),
            roster_canonical_bytes(
                "f00",
                1,
                1,
                &table,
                "charter",
                &[],
                Some(&["memory".to_string()]),
            ),
        );
    }

    #[test]
    fn roster_canonical_bytes_are_stable_and_field_separated() {
        let table = vec![
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: "aa".repeat(32),
                nostr_pk: "cc".repeat(32),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: "bb".repeat(32),
                nostr_pk: "dd".repeat(32),
            },
        ];
        let a = roster_canonical_bytes("f00", 2, 3, &table, "charter", &[], None);
        assert_eq!(a, roster_canonical_bytes("f00", 2, 3, &table, "charter", &[], None));
        // any changed field changes the bytes
        assert_ne!(a, roster_canonical_bytes("f01", 2, 3, &table, "charter", &[], None));
        assert_ne!(a, roster_canonical_bytes("f00", 3, 3, &table, "charter", &[], None));
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table[..1], "charter", &[], None));
        // the ratified agenda is bound: a changed charter changes the bytes
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table, "other", &[], None));
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table, "", &[], None));
        // length prefixes prevent name/pk boundary games
        let shifted = vec![MemberIdentity {
            member: "petraa".to_string(),
            identity_pk: format!("a{}", "a".repeat(63)),
            nostr_pk: String::new(),
        }];
        let plain = vec![MemberIdentity {
            member: "petra".to_string(),
            identity_pk: format!("aa{}", "a".repeat(62)),
            nostr_pk: String::new(),
        }];
        assert_ne!(
            roster_canonical_bytes("f00", 1, 1, &shifted, "", &[], None),
            roster_canonical_bytes("f00", 1, 1, &plain, "", &[], None)
        );
    }

    #[test]
    fn event_envelope_roundtrips_and_unknown_variants_fall_back_raw() {
        let env = EventEnvelope { prev_seq: 0,
            seq: 7,
            ts: 1_751_700_000,
            by: "mithra".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 3,
                id: None,
                emoji: "🔥".to_string(),
                by: "mithra".to_string(),
                op: None,
            },
        };
        let wire = serde_json::to_string(&env).expect("encode");
        let back: EventEnvelope = serde_json::from_str(&wire).expect("decode");
        assert_eq!(back, env);

        // a frame written by a newer node: the typed decode fails, the raw
        // fallback preserves the envelope for re-emission
        let newer = r#"{"seq":8,"ts":1,"by":"x","body":{"type":"hologram","q":1}}"#;
        assert!(serde_json::from_str::<EventEnvelope>(newer).is_err());
        let raw: RawEnvelope = serde_json::from_str(newer).expect("raw fallback");
        assert_eq!(raw.seq, 8);
        assert_eq!(raw.body["type"], serde_json::json!("hologram"));
    }

    /// The boot group of a production node is honest: one member, no
    /// invented peers — nothing outside an open workspace may suggest
    /// other members exist.
    #[test]
    fn the_solo_boot_group_names_no_peers() {
        let solo = GroupConfig::solo();
        assert_eq!(solo.members, vec![solo.member.clone()]);
        assert_eq!(solo.threshold, 1);
    }

    // --- real presence: numeric last-seen stamps ---------------------------

    /// The presence classification is a pure function of (now, stamp):
    /// never-seen is offline, a fresh sighting is online, silence ages the
    /// state through stale to offline at the documented thresholds.
    #[test]
    fn presence_ages_by_real_time_and_never_seen_is_offline() {
        let t = 1_000_000u64;
        assert_eq!(presence_state(t, MemberInfo::NEVER), 2);
        assert_eq!(presence_state(t, t), 0);
        assert_eq!(presence_state(t + MemberInfo::ONLINE_SECS, t), 0);
        assert_eq!(presence_state(t + MemberInfo::ONLINE_SECS + 1, t), 1);
        assert_eq!(presence_state(t + MemberInfo::STALE_SECS, t), 1);
        assert_eq!(presence_state(t + MemberInfo::STALE_SECS + 1, t), 2);
        // a stamp slightly ahead of our clock (peer clock skew) is online
        assert_eq!(presence_state(t, t + 50), 0);
    }

    /// The one roster projection carries the REAL stamp per member and
    /// derives the pill state from it — a member without a stamp starts
    /// honestly as never-seen/offline, no placeholder presence.
    #[test]
    fn roster_members_projects_real_stamps() {
        let roster = vec!["ada".to_string(), "bob".to_string()];
        let now = 2_000_000u64;
        let members = roster_members(&roster, now, |m| {
            if m == "ada" {
                now - 10
            } else {
                MemberInfo::NEVER
            }
        });
        assert_eq!(
            members,
            vec![
                MemberInfo {
                    name: "ada".to_string(),
                    last_seen: now - 10,
                    state: 0,
                },
                MemberInfo {
                    name: "bob".to_string(),
                    last_seen: MemberInfo::NEVER,
                    state: 2,
                },
            ]
        );
    }

    #[test]
    fn demo_workspace_ids_are_stable_and_distinct() {
        let a = demo_workspace_id("Family Office");
        assert_eq!(a.len(), 64);
        assert_eq!(a, demo_workspace_id("Family Office"));
        assert_ne!(a, demo_workspace_id("Savings-DAO"));
    }

    // --- backup listing (mock_todo story 8) --------------------------------

    /// The production session must never invent bucket contents: orphans
    /// appear only from a real listing (`NetListBackups`), so the default is
    /// EMPTY. This is the regression fence for the removed demo fixture.
    /// K6: the default view lists NO republic — the demo set is a fixture,
    /// and a default that carried it hid the one real workspace on disk.
    #[test]
    fn session_default_lists_no_workspaces() {
        assert!(SessionView::default().workspaces.is_empty());
    }

    #[test]
    fn session_default_has_no_backup_orphans() {
        assert!(
            SessionView::default().backup_orphans.is_empty(),
            "demo orphans must not leak into the production session state"
        );
        assert!(SessionView::default().s3_list.is_empty(), "no listing ran yet");
    }

    /// The bucket object-naming scheme (`backup_restore_design.md` §6.2):
    /// `molt/<workspace_id>/<ts>.molt.enc` with a 64-hex id and a numeric
    /// timestamp. Anything else is a foreign key, not a backup.
    #[test]
    fn backup_key_parses_the_naming_scheme_and_rejects_foreign_keys() {
        let id = "ab".repeat(32);
        let key = format!("molt/{id}/001752800000.molt.enc");
        assert_eq!(
            parse_backup_key(&key),
            Some((id.clone(), 1_752_800_000)),
            "the canonical scheme parses"
        );
        for foreign in [
            "molt/backup.tar".to_string(),                     // no id level
            format!("molt/{id}/notes.txt"),                    // wrong suffix
            format!("molt/{id}/xx.molt.enc"),                  // non-numeric ts
            format!("molt/{id}/+123.molt.enc"),                // sign is not a digit
            format!("molt/{}/001.molt.enc", "zz".repeat(32)),  // not hex
            format!("molt/{}/001.molt.enc", "ab".repeat(16)),  // wrong id length
            format!("other/{id}/001.molt.enc"),                // outside molt/
            format!("molt/{id}/deep/001.molt.enc"),            // extra level
            String::new(),
        ] {
            assert_eq!(parse_backup_key(&foreign), None, "foreign: {foreign:?}");
        }
    }

    /// The writer half ([`backup_key`]) round-trips through the parser, and
    /// its zero-padding keeps lexicographic key order equal to age order —
    /// the invariant the retention pruner's key sort relies on (§6.2).
    #[test]
    fn backup_key_builder_round_trips_and_sorts_by_age() {
        let id = "cd".repeat(32);
        for ts in [0u64, 1, 1_752_800_000, 999_999_999_999] {
            let key = backup_key(&id, ts);
            assert_eq!(parse_backup_key(&key), Some((id.clone(), ts)), "{key}");
        }
        // lexicographic order == age order across magnitude boundaries
        let mut keys: Vec<String> = [999u64, 1_000, 99_999, 1_752_800_000, 1]
            .iter()
            .map(|ts| backup_key(&id, *ts))
            .collect();
        keys.sort_unstable();
        let ts_order: Vec<u64> = keys
            .iter()
            .map(|k| parse_backup_key(k).expect("own keys parse").1)
            .collect();
        assert_eq!(ts_order, vec![1, 999, 1_000, 99_999, 1_752_800_000]);
    }

    /// Canonical-width enforcement (§6.2): the writer always emits a fixed
    /// 12-wide zero-padded stem so lexicographic key order equals age order.
    /// A stem of any other width is a FOREIGN key, never a backup — otherwise
    /// a planted `molt/<id>/9.molt.enc` (age ts 9) would sort lexicographically
    /// AFTER a real `molt/<id>/000000000010.molt.enc` (age ts 10) and invert
    /// the retention pruner's oldest/newest pick.
    #[test]
    fn backup_key_requires_canonical_12_wide_stem() {
        let id = "ef".repeat(32);
        let make = |stem: &str| format!("molt/{id}/{stem}.molt.enc");
        // the real writer's fixed 12-digit width IS a backup
        assert_eq!(
            parse_backup_key(&make("000000000009")),
            Some((id.clone(), 9)),
            "the canonical 12-wide stem parses"
        );
        // every other width is a foreign key — never counted, pruned or picked
        for stem in ["9", "09", "00000000009", "0000000000009", "999999999999999"] {
            assert_ne!(stem.len(), 12, "negative fixture {stem:?} must be off-width");
            assert_eq!(parse_backup_key(&make(stem)), None, "off-width {stem:?} is foreign");
        }
        // the concrete corruption the fix prevents: a planted short key (age 9)
        // sorts lexicographically AFTER a real 12-wide key (age 10) — so it
        // must not parse at all, leaving the real key the sole age-ordered one.
        let planted = make("9");
        let real = backup_key(&id, 10);
        assert!(planted > real, "the short planted key sorts later than the newer real one");
        assert_eq!(parse_backup_key(&planted), None, "yet the planted key is not a backup");
        assert_eq!(parse_backup_key(&real), Some((id, 10)));
    }

    /// Classification of a real listing: objects of a locally known
    /// workspace are NOT orphans (their row exists locally); parseable
    /// prefixes without a local workspace aggregate into one orphan entry
    /// (newest object dates it, sizes sum); unparseable keys survive
    /// honestly as unknown per-key entries instead of being hidden.
    #[test]
    fn backup_listing_classifies_local_orphan_and_foreign() {
        let local = "11".repeat(32);
        let orphan = "22".repeat(32);
        let now = 1_752_800_000u64;
        let obj = |key: &str, size: u64, modified: u64| BackupObject {
            key: key.to_string(),
            size,
            modified,
        };
        let objects = vec![
            // two generations of a local workspace's backup → no orphan
            obj(&format!("molt/{local}/001752700000.molt.enc"), 4096, now - 100_000),
            obj(&format!("molt/{local}/001752790000.molt.enc"), 4096, now - 10_000),
            // two generations of an unknown workspace → ONE orphan entry
            obj(&format!("molt/{orphan}/001752600000.molt.enc"), 1024, now - 200_000),
            obj(&format!("molt/{orphan}/001752796400.molt.enc"), 2048, now - 3_600),
            // a foreign key → a per-key unknown entry
            obj("molt/leftover.bin", 512, now - 60),
        ];
        let got = backup_orphans_from_listing(&objects, std::slice::from_ref(&local), now);
        assert_eq!(got.len(), 2, "one orphan + one unknown: {got:?}");
        let o = &got[0];
        assert_eq!(o.id, orphan, "the orphan carries the workspace-id pseudonym");
        assert_eq!(o.name, "", "no display name is known for an orphan");
        assert_eq!(o.size_kib, 3, "sizes sum, KiB rounded up");
        assert_eq!(o.last_backup_min, 60, "dated by the NEWEST object");
        let f = &got[1];
        assert_eq!(f.id, "", "a foreign key has no workspace id");
        assert_eq!(f.name, "molt/leftover.bin", "shown by its raw key");
        assert_eq!(f.size_kib, 1);
        assert_eq!(f.last_backup_min, 1);
        // a listing with only local backups yields no orphans at all
        assert!(
            backup_orphans_from_listing(&objects[..2], &[local], now).is_empty()
        );
        // a future-dated object (clock skew) clamps to "just now", and an
        // empty listing stays empty (canonical 12-wide key so it is a real
        // orphan, not a foreign off-width key)
        let skew = backup_orphans_from_listing(
            &[obj(&format!("molt/{orphan}/000000000009.molt.enc"), 10, now + 500)],
            &[],
            now,
        );
        assert_eq!(skew[0].id, orphan, "a canonical-width object is a real orphan");
        assert_eq!(skew[0].last_backup_min, 0);
        assert!(backup_orphans_from_listing(&[], &[], now).is_empty());
    }

    // --- chat bus Stage A: message identity + channel tags -----------------

    #[test]
    fn message_id_round_trips_as_hex_and_rejects_bad_input() {
        let id = MessageId([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        // Display / FromStr: 32-char lowercase hex
        let s = id.to_string();
        assert_eq!(s, "00112233445566778899aabbccddeeff");
        assert_eq!(s.parse::<MessageId>().expect("parse back"), id);
        // serde: the same hex string on the wire
        assert_eq!(
            serde_json::to_string(&id).expect("encode"),
            "\"00112233445566778899aabbccddeeff\""
        );
        let back: MessageId =
            serde_json::from_str("\"00112233445566778899aabbccddeeff\"").expect("decode");
        assert_eq!(back, id);
        // NIL is all-zero and the only nil value
        assert!(MessageId::NIL.is_nil());
        assert_eq!(MessageId::NIL.to_string(), "0".repeat(32));
        assert!(!id.is_nil());
        // bad input: wrong length, non-hex, uppercase (canonical form only)
        for bad in [
            "",
            "0011",
            "00112233445566778899aabbccddeeff00",
            "zz112233445566778899aabbccddeeff",
            "00112233445566778899AABBCCDDEEFF",
        ] {
            assert!(bad.parse::<MessageId>().is_err(), "accepted {bad:?}");
            assert!(
                serde_json::from_str::<MessageId>(&format!("\"{bad}\"")).is_err(),
                "decoded {bad:?}"
            );
        }
        // and a non-string JSON value is rejected too
        assert!(serde_json::from_str::<MessageId>("42").is_err());
    }

    #[test]
    fn channel_ref_serdes_by_kind_tag_and_group_serializes_to_nothing() {
        // the enum itself: internally tagged by `kind`, snake_case
        assert_eq!(
            serde_json::to_string(&ChannelRef::Group).expect("encode"),
            r#"{"kind":"group"}"#
        );
        assert_eq!(
            serde_json::to_string(&ChannelRef::Patch { id: ProposalId(7) }).expect("encode"),
            r#"{"kind":"patch","id":7}"#
        );
        assert_eq!(
            serde_json::to_string(&ChannelRef::Topic {
                name: "budget".to_string()
            })
            .expect("encode"),
            r#"{"kind":"topic","name":"budget"}"#
        );
        for wire in [
            r#"{"kind":"group"}"#,
            r#"{"kind":"patch","id":7}"#,
            r#"{"kind":"topic","name":"budget"}"#,
        ] {
            let c: ChannelRef = serde_json::from_str(wire).expect("decode");
            assert_eq!(serde_json::to_string(&c).expect("re-encode"), wire);
        }
        assert!(ChannelRef::Group.is_group());
        assert!(!ChannelRef::Topic {
            name: "budget".to_string()
        }
        .is_group());
        assert_eq!(ChannelRef::default(), ChannelRef::Group);

        // THE compatibility pin: a Group / nil-id / no-quote_id message
        // serializes byte-identical to the pre-chat-bus fixture (captured
        // from the tree before this change).
        let plain = ChatMessage::text(MessageId::NIL, "petra", "gm", 102);
        assert_eq!(
            serde_json::to_string(&plain).expect("encode"),
            r#"{"from":"petra","body":"gm","ts":102}"#
        );
        // and a decoded legacy message re-serializes byte-identical, numeric
        // `quote` included (skip-if-nil / skip-if-group / skip-if-none)
        let legacy = r#"{"from":"walter","body":"re: gm","ts":103,"quote":0}"#;
        let msg: ChatMessage = serde_json::from_str(legacy).expect("decode");
        assert_eq!(serde_json::to_string(&msg).expect("re-encode"), legacy);
    }

    #[test]
    fn legacy_chat_json_without_id_or_channel_still_decodes() {
        // fixtures in the pre-chat-bus wire shape, incl. a numeric quote and
        // a file share (captured before this change)
        let quoted = r#"{"from":"walter","body":"re: gm","ts":103,"quote":0}"#;
        let msg: ChatMessage = serde_json::from_str(quoted).expect("decode");
        assert!(msg.id.is_nil());
        assert_eq!(msg.channel, ChannelRef::Group);
        assert_eq!(msg.quote, Some(0), "the legacy index quote is preserved");
        assert_eq!(msg.quote_id, None);
        assert_eq!(msg.from, "walter");
        assert_eq!(msg.ts, 103);

        let full = r#"{"from":"petra","body":"","ts":104,"reactions":{"👍":["walter"]},"file":{"name":"charter.pdf","size":48000,"kind":"PDF","modified":100,"available":true}}"#;
        let msg: ChatMessage = serde_json::from_str(full).expect("decode");
        assert!(msg.id.is_nil());
        assert_eq!(msg.channel, ChannelRef::Group);
        assert_eq!(msg.reactions["👍"], vec!["walter".to_string()]);
        assert_eq!(msg.file.as_ref().map(|f| f.size), Some(48_000));
    }

    #[test]
    fn chat_kind_is_additive_user_is_invisible_system_roundtrips() {
        // (a) legacy JSON without `kind` decodes as the User default
        let legacy = r#"{"from":"walter","body":"re: gm","ts":103}"#;
        let msg: ChatMessage = serde_json::from_str(legacy).expect("decode");
        assert_eq!(msg.kind, ChatKind::User);
        assert!(msg.kind.is_user());

        // (b) a User-kind message emits NO "kind" key — byte-identical to
        // the pre-kind wire shape (the skip_serializing_if is load-bearing)
        let plain = ChatMessage::text(MessageId::NIL, "petra", "gm", 102);
        assert_eq!(plain.kind, ChatKind::User);
        let wire = serde_json::to_string(&plain).expect("encode");
        assert!(!wire.contains("kind"), "User kind must stay invisible: {wire}");
        assert_eq!(wire, r#"{"from":"petra","body":"gm","ts":102}"#);

        // (c) a System message round-trips through JSON, snake_case tag
        let sys =
            ChatMessage::text(MessageId::NIL, "petra", "rejoined", 104).with_kind(ChatKind::System);
        assert!(!sys.kind.is_user());
        let wire = serde_json::to_string(&sys).expect("encode");
        assert!(wire.contains(r#""kind":"system""#), "system tag on the wire: {wire}");
        let back: ChatMessage = serde_json::from_str(&wire).expect("decode");
        assert_eq!(back.kind, ChatKind::System);
        assert_eq!(back, sys);
    }

    #[test]
    fn read_by_is_additive_empty_invisible_populated_roundtrips() {
        // (a) an empty read_by emits NO "read_by" key — byte-identical to the
        // pre-read-receipts wire shape (the skip_serializing_if is load-bearing)
        let plain = ChatMessage::text(MessageId::NIL, "petra", "gm", 102);
        assert!(plain.read_by.is_empty());
        let wire = serde_json::to_string(&plain).expect("encode");
        assert!(!wire.contains("read_by"), "empty read_by must stay invisible: {wire}");
        assert_eq!(wire, r#"{"from":"petra","body":"gm","ts":102}"#);

        // (b) legacy JSON without read_by decodes to the empty-set default
        let legacy = r#"{"from":"walter","body":"re: gm","ts":103}"#;
        let msg: ChatMessage = serde_json::from_str(legacy).expect("decode");
        assert!(msg.read_by.is_empty());

        // (c) a populated read_by round-trips (sorted set, stable order)
        let mut m = ChatMessage::text(MessageId::NIL, "petra", "hi", 104);
        m.read_by.insert("walter".to_string());
        m.read_by.insert("clara".to_string());
        let wire = serde_json::to_string(&m).expect("encode");
        assert!(wire.contains(r#""read_by":["clara","walter"]"#), "sorted set on the wire: {wire}");
        let back: ChatMessage = serde_json::from_str(&wire).expect("decode");
        assert_eq!(back.read_by, m.read_by);
        assert_eq!(back, m);
    }

    /// Mesh self-heal Stage 3: the additive `nonce` on `MeshAnnounced` must be
    /// invisible when absent (recovery/bootstrap announces stay byte-identical
    /// to the pre-self-heal wire shape), decode to `None` from legacy JSON, and
    /// round-trip when present (the relay loop-prevention token).
    #[test]
    fn mesh_announced_nonce_is_additive_and_invisible_when_absent() {
        // (a) no nonce → no "nonce" key on the wire (skip_serializing_if)
        let none = WorkspaceEvent::MeshAnnounced {
            ct: "deadbeef".to_string(),
            nonce: None,
        };
        let wire = serde_json::to_string(&none).expect("encode");
        assert!(!wire.contains("nonce"), "absent nonce must stay invisible: {wire}");
        assert_eq!(wire, r#"{"type":"mesh_announced","ct":"deadbeef"}"#);

        // (b) legacy JSON without nonce decodes to None
        let legacy = r#"{"type":"mesh_announced","ct":"cafe"}"#;
        let back: WorkspaceEvent = serde_json::from_str(legacy).expect("decode");
        assert_eq!(back, WorkspaceEvent::MeshAnnounced { ct: "cafe".to_string(), nonce: None });

        // (c) a self-heal re-announce carries and round-trips its nonce
        let rot = WorkspaceEvent::MeshAnnounced {
            ct: "f00d".to_string(),
            nonce: Some(0x1234_5678_9abc_def0),
        };
        let wire = serde_json::to_string(&rot).expect("encode");
        assert!(wire.contains(r#""nonce":1311768467463790320"#), "nonce on the wire: {wire}");
        assert_eq!(serde_json::from_str::<WorkspaceEvent>(&wire).expect("decode"), rot);
    }

    #[test]
    fn topic_names_normalize_on_send_but_stay_case_preserving() {
        let ok = ChannelRef::Topic {
            name: "  Budget 2027  ".to_string(),
        }
        .normalized()
        .expect("valid topic");
        assert_eq!(
            ok,
            ChannelRef::Topic {
                name: "Budget 2027".to_string()
            },
            "trimmed, case preserved"
        );
        assert!(ChannelRef::Topic {
            name: "   ".to_string()
        }
        .normalized()
        .is_err());
        assert!(ChannelRef::Topic {
            name: "x".repeat(65)
        }
        .normalized()
        .is_err());
        assert_eq!(
            ChannelRef::Group.normalized().expect("group passes"),
            ChannelRef::Group
        );
        let patch = ChannelRef::Patch { id: ProposalId(3) };
        assert_eq!(patch.clone().normalized().expect("patch passes"), patch);
    }
}
