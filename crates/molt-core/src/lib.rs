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
//! (the generalized approval engine, R0, and the five surfaces).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A member of the republic (the holder of one threshold share). In this
/// scaffold it is just a display handle; the real per-group MLS identity is a
/// future `molt-identity` concern.
pub type MemberId = String;

/// The sole address of a workspace across the command set: 32 bytes, lowercase
/// hex (64 chars), derived from the recovery seed and the member identity
/// (`HKDF(seed, "molt-ws-id", member)` — see `molt-storage`). Display names
/// are presentation only and may repeat; the id never does.
pub type WorkspaceId = String;

/// The shared surfaces. [`Surface::Organization`] is a read-only info area
/// and [`Surface::Chat`] is ungated; the other four change the shared state
/// only through a threshold-approved proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// Who this republic is: status, roster, statistics. Read-only.
    Organization,
    /// Free conversation. Ungated: a message changes no shared state.
    Chat,
    /// The shared brain: versioned, cross-linked notes. Gated.
    Memory,
    /// The quest board: tasks put forward, taken and completed. Gated.
    Quests,
    /// Sealed secrets, released only at the threshold. Gated.
    Vault,
    /// Shared funds (Monero multisig in production). Gated.
    Wallet,
}

impl Surface {
    /// Every surface, in display (= navigation) order.
    pub const ALL: [Surface; 6] = [
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
        }
    }

    /// Whether changes to this surface require a threshold of approvals.
    /// Chat is ungated; Organization is read-only (nothing to propose).
    pub fn is_gated(self) -> bool {
        !matches!(self, Surface::Chat | Surface::Organization)
    }

    /// Parse a surface from its lowercase name.
    pub fn parse(s: &str) -> Option<Surface> {
        Surface::ALL.into_iter().find(|x| x.as_str() == s)
    }

    /// The surface's sub-views as `(key, display label)` pairs, in navigation
    /// order. The first entry is the default view. Shared vocabulary: the GUI
    /// nav and the `select_view` command validate against this same list.
    pub fn views(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Surface::Organization => &[
                ("status", "Status"),
                ("members", "Members"),
                ("statistics", "Statistics"),
            ],
            Surface::Chat => &[("today", "Today"), ("archive", "Archive")],
            Surface::Memory => &[
                ("brain", "Brain"),
                ("proposals", "Proposals"),
                ("accepted", "Accepted"),
                ("denied", "Denied"),
                ("archive", "Archive"),
            ],
            Surface::Quests => &[
                ("board", "Board"),
                ("create", "Create"),
                ("proposals", "Proposals"),
                ("my-quests", "My Quests"),
                ("archive", "Archive"),
            ],
            Surface::Vault => &[
                ("secrets", "Secrets"),
                ("disclose", "Disclose"),
                ("proposals", "Proposals"),
                ("exposed", "Exposed"),
            ],
            Surface::Wallet => &[
                ("balance", "Balance"),
                ("history", "History"),
                ("send", "Send"),
                ("receive", "Receive"),
                ("status", "Status"),
                ("settings", "Settings"),
            ],
        }
    }

    /// The view a surface opens on (the first of [`Surface::views`]).
    pub fn default_view(self) -> &'static str {
        self.views()[0].0
    }
}

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
    /// S3 secret key.
    #[serde(default)]
    pub s3_secret_key: String,
    /// S3 bucket name. Defaults to something inconspicuous — the bucket
    /// listing should not advertise what it holds.
    #[serde(default = "default_s3_bucket")]
    pub s3_bucket: String,
    /// Automatic-backup interval in minutes.
    #[serde(default = "default_s3_interval")]
    pub s3_interval_min: u16,
    /// MCP server TCP port.
    pub mcp_port: u16,
    /// MCP client allowlist (`"127.0.0.1" | "0.0.0.0" | comma-separated`).
    pub mcp_allow: String,
    /// MCP API token clients must present.
    pub mcp_token: String,
    /// Anonymity network: `"tor" | "nym" | "none"`.
    pub anonymity: String,
    /// Tor mode: `"local" | "embedded" | "whonix"`.
    pub tor_mode: String,
    /// Local Tor SOCKS port.
    pub tor_port: u16,
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
            mcp_port: 4040,
            mcp_allow: "127.0.0.1".to_string(),
            mcp_token: String::new(),
            anonymity: "tor".to_string(),
            tor_mode: "local".to_string(),
            tor_port: 9050,
        }
    }
}

/// The inconspicuous default S3 bucket name.
fn default_s3_bucket() -> String {
    "media-archive".to_string()
}

/// Default automatic-backup interval (minutes).
fn default_s3_interval() -> u16 {
    60
}

/// One member of a workspace with its (mock) last-sync info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Display handle.
    pub name: String,
    /// Human "last synced" label, e.g. `"2 min ago"`.
    pub last: String,
    /// 0 = synced, 1 = syncing, 2 = offline.
    pub state: u8,
}

/// Project a member roster into the session's [`MemberInfo`] shape: members
/// for whom `synced` holds are "just now"/online, everyone else gets
/// `absent_label` and shows offline. The one projection every flow uses —
/// presence is the transport's runtime state, so until that exists these
/// labels are the honest defaults.
pub fn roster_members(
    roster: &[MemberId],
    synced: impl Fn(&str) -> bool,
    absent_label: &str,
) -> Vec<MemberInfo> {
    roster
        .iter()
        .map(|m| {
            let is_synced = synced(m);
            MemberInfo {
                name: m.clone(),
                last: if is_synced {
                    "just now".to_string()
                } else {
                    absent_label.to_string()
                },
                state: if is_synced { 0 } else { 2 },
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
    /// On-disk size in KiB (mock).
    #[serde(default)]
    pub size_kib: u32,
    /// Minutes since the last completed backup; [`WorkspaceInfo::NEVER`]
    /// = never backed up. Prose is rendered UI-side.
    #[serde(default = "WorkspaceInfo::never")]
    pub last_backup_min: u32,
    /// The (mock) recovery seed all of its secret keys derive from.
    pub seed: String,
    /// Transport: `"tor" | "nym" | "clearnet"`.
    pub net: String,
    /// Members and when each of them last synced.
    pub members: Vec<MemberInfo>,
}

/// A stable fake [`WorkspaceId`] for demo entries: an FNV-1a hash of the
/// name, expanded to 32 bytes with splitmix-style mixing — distinct names
/// yield distinct ids (no cyclic-name collisions, unlike naive byte
/// repetition). Real ids come from the seed derivation in `molt-storage`;
/// this exists so session-only demo lists are addressable by id too.
pub fn demo_workspace_id(name: &str) -> WorkspaceId {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
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

    /// The demo set of local republics the scaffold ships with.
    pub fn demo_set() -> Vec<WorkspaceInfo> {
        fn m(name: &str, last: &str, state: u8) -> MemberInfo {
            MemberInfo {
                name: name.to_string(),
                last: last.to_string(),
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
                seed: seed.to_string(),
                net: net.to_string(),
                members,
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

/// A backup found in the S3 bucket with no matching local workspace
/// (mock). Shows up in the settings backup table with an empty "local"
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupOrphan {
    /// Workspace name recorded in the backup.
    pub name: String,
    /// Backup size in KiB.
    pub size_kib: u32,
    /// Minutes since this backup was written.
    pub last_backup_min: u32,
}

impl BackupOrphan {
    /// The demo bucket contents that have no local counterpart.
    pub fn demo_set() -> Vec<BackupOrphan> {
        fn o(name: &str, size_kib: u32, last_backup_min: u32) -> BackupOrphan {
            BackupOrphan {
                name: name.to_string(),
                size_kib,
                last_backup_min,
            }
        }
        vec![
            o("Chess Club", 480, 129_600), // 90 days
            o("Book Money", 75, 43_200),   // 30 days
        ]
    }
}

/// One chat message — THE schema of the chat log. The engine mutates and
/// the GUI reads this one type; on the wire (`read_state.applied`) it
/// serializes to the same JSON object as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Sender handle.
    pub from: MemberId,
    /// Message body (empty once deleted).
    pub body: String,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub ts: u64,
    /// Quoted message (0-based position in the chat log), if replying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<u64>,
    /// Emoji → the members who picked it (one reaction per member; a
    /// BTreeMap keeps the pill order stable across re-renders).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reactions: BTreeMap<String, Vec<MemberId>>,
    /// Who deleted the message (`None` = live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<MemberId>,
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
}

impl Default for CryptoParams {
    fn default() -> Self {
        CryptoParams {
            kdf: default_kdf(),
            cipher: default_cipher(),
            key_file: default_key_file(),
        }
    }
}

fn default_kdf() -> String {
    "argon2id".to_string()
}
fn default_cipher() -> String {
    "xchacha20poly1305".to_string()
}
fn default_key_file() -> String {
    "keys/workspace.key".to_string()
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
}

impl Default for WorkspacePrefs {
    fn default() -> Self {
        WorkspacePrefs {
            format: PREFS_FORMAT.to_string(),
            version: STORAGE_VERSION,
            s3_backup: false,
            last_backup: None,
        }
    }
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
}

/// What can happen in a workspace. **Additive-only evolution**: new kinds
/// append variants; an older reader that meets an unknown variant must not
/// write to that workspace (applying a partial history would fork state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    /// seq 1, exactly once: who this republic is. Rule, roster and the
    /// acting member never exist outside the event stream.
    Founded {
        /// Display name at founding.
        name: String,
        /// Approval threshold (m).
        rule_m: u8,
        /// Member count (n).
        rule_n: u8,
        /// The acting member on this node.
        member: MemberId,
        /// The full member roster (filled seats + open invites).
        roster: Vec<MemberId>,
    },
    /// A seat filled via invite.
    MemberJoined {
        /// The joining member.
        member: MemberId,
    },
    /// A chat message was posted (the existing typed schema).
    Chat(ChatMessage),
    /// A member's emoji reaction on a chat message was toggled.
    ChatReacted {
        /// Message position in the chat log (0-based).
        index: u64,
        /// The reaction emoji.
        emoji: String,
        /// Who toggled it.
        by: MemberId,
    },
    /// A chat message was wiped; only the deletion notice remains.
    ChatDeleted {
        /// Message position in the chat log (0-based).
        index: u64,
        /// Who deleted it.
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
    /// One member's approval landed on a pending proposal.
    Approved {
        /// The proposal.
        id: ProposalId,
        /// The approving member.
        by: MemberId,
    },
    /// A pending proposal was declined.
    Declined {
        /// The proposal.
        id: ProposalId,
        /// The declining member.
        by: MemberId,
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
    /// The chat log.
    pub chat: Vec<ChatMessage>,
    /// Applied transition log per gated surface (keyed by surface name).
    pub applied: BTreeMap<String, Vec<Value>>,
    /// Every known proposal by id.
    pub proposals: BTreeMap<u64, ProposalRecord>,
    /// The next proposal id to assign.
    pub next_proposal_id: u64,
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

/// A parsed (mock) invite link. The one wire format both the GUI preview and
/// the engine's join run use: `molt://invite/<republic>/<m>of<n>/<inviter>/<ticket>`
/// (spaces in the republic name travel as dashes).
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
        if parts.next().is_some() {
            return None;
        }
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

/// Tiny demo PRNGs behind every mock generator in the workspace. This is
/// NOT cryptography — it feeds simulations (seeds, tickets, reply timing).
pub mod mockrand {
    /// One LCG step (Knuth's MMIX constants).
    pub fn lcg(x: u64) -> u64 {
        x.wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407)
    }

    /// One xorshift64 step, advancing the seed in place.
    pub fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }
}

use mockrand::lcg;

// NOTE: the old `mock_seed` (12 words off a 48-word LCG list) is gone on
// purpose: founded workspaces derive their key hierarchy from a real
// OS-CSPRNG seed rendered as a BIP-39 phrase (`molt-storage::keys`). A key
// hierarchy hanging off ~30 bits of hashed wall-clock is decorative
// encryption. `mock_ticket` below still feeds the simulated invite flow.

/// Derive a mock one-time invite ticket (10 base32 characters) from `entropy`.
pub fn mock_ticket(entropy: u64) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut x = lcg(entropy ^ 0x9e37_79b9_7f4a_7c15);
    let mut out = String::with_capacity(10);
    for _ in 0..10 {
        x = lcg(x);
        let idx = usize::try_from((x >> 33) % 32).unwrap_or_default();
        out.push(char::from(ALPHABET[idx]));
    }
    out
}

/// The shared core of every engine-run mock lifecycle (restore / create /
/// join): step, progress, outcome and the live log. `#[serde(flatten)]`
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

/// The (mock) founding lifecycle of a new republic. Shared session state like
/// the restore: any operator can start it, both watch the same progress and
/// live log, and the founding result (seed + invites) is readable by both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateState {
    /// The shared run lifecycle (step / progress / outcome / log).
    #[serde(flatten)]
    pub run: RunCore,
    /// The republic's name.
    pub name: String,
    /// The founder's handle.
    pub member: String,
    /// The approval threshold (m).
    pub threshold: u8,
    /// The member count (n).
    pub members: u8,
    /// Transport: `"tor" | "nym" | "none"`.
    pub net: String,
    /// The freshly derived recovery phrase (shown once, on success).
    pub seed: String,
    /// One-time invite links for the other n−1 members.
    pub invites: Vec<String>,
}

/// The (mock) join-via-invite lifecycle. Shared session state like the restore.
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
}

/// The (mock) restore lifecycle. Shared session state: any operator can start
/// it, both watch the same progress and live log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreState {
    /// The shared run lifecycle (step / progress / outcome / log).
    #[serde(flatten)]
    pub run: RunCore,
    /// `"peer" | "s3" | "file"` (empty while idle).
    pub way: String,
    /// The way-specific target (endpoint / bucket URL / file path).
    pub target: String,
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
    /// Config keys (file names, e.g. `"mcp.port"`) whose current value
    /// differs from what the node booted with and which only take effect on
    /// restart. Set by the engine on every save/reload; NOT transient — it
    /// stays until the values return to the boot state or the node restarts.
    /// The GUI renders it as a persistent "restart required" warning.
    #[serde(default)]
    pub restart_required: Vec<String>,
    /// The editable settings.
    pub settings: SessionSettings,
    /// The locally known workspaces (mock list, shared).
    pub workspaces: Vec<WorkspaceInfo>,
    /// Backups in the S3 bucket without a local workspace (mock, static).
    #[serde(default)]
    pub backup_orphans: Vec<BackupOrphan>,
    /// Id of the currently opened workspace (empty = none). The display
    /// name lives in the matching [`WorkspaceInfo`] entry.
    pub active_workspace: WorkspaceId,
    /// The (mock) restore lifecycle.
    pub restore: RestoreState,
    /// The (mock) founding lifecycle.
    pub create: CreateState,
    /// The (mock) join-via-invite lifecycle.
    pub join: JoinState,
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
            restart_required: Vec::new(),
            settings: SessionSettings::default(),
            workspaces: WorkspaceInfo::demo_set(),
            backup_orphans: BackupOrphan::demo_set(),
            active_workspace: String::new(),
            restore: RestoreState::default(),
            create: CreateState::default(),
            join: JoinState::default(),
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
        /// Quoted message (0-based position in the chat log), if replying.
        #[serde(default)]
        quote: Option<u64>,
    },
    /// Post a chat message as another member. Sent by the engine's own
    /// demo reply-simulator (like the run tickers, this is engine-internal
    /// and not exposed as an MCP tool).
    ChatFrom {
        /// The (simulated) sender.
        from: MemberId,
        /// The message body.
        body: String,
        /// Quoted message (0-based position in the chat log), if replying.
        #[serde(default)]
        quote: Option<u64>,
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
    /// Delete a chat message: its text is wiped for everyone and replaced
    /// by a deletion notice naming who deleted it (reactions are dropped).
    DeleteChat {
        /// The message's position in the chat log (0-based).
        index: u64,
    },
    /// Toggle the local member's emoji reaction on a chat message: reacting
    /// with the emoji you already picked un-reacts, picking another emoji
    /// switches — one reaction per member per message.
    ReactChat {
        /// The message's position in the chat log (0-based).
        index: u64,
        /// The reaction emoji.
        emoji: String,
    },
    /// Read the projected state of one surface.
    ReadState {
        /// Which surface to read.
        surface: Surface,
    },
    /// List every proposal the engine currently knows about.
    ListProposals,
    /// Read a one-shot status summary of the group and surfaces.
    Status,

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
    /// Store the settings into the session and persist them to the node's
    /// `config.toml` (format-preserving, atomic). The reply does not wait for
    /// the disk; the write outcome lands in the session notice ("saved" /
    /// "save-failed: …") via [`Command::ConfigNotice`].
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
    /// the workspace's local `prefs.toml`. Enabling runs a first backup
    /// right away (the uploader itself is not wired yet).
    SetWorkspaceBackup {
        /// The workspace id ([`WorkspaceInfo::id`]).
        id: WorkspaceId,
        /// New auto-backup state.
        enabled: bool,
    },
    /// Begin the (mock) restore: moves its lifecycle to the run view; the
    /// engine ticks the progress and the live log by itself.
    RestoreStart {
        /// `"peer" | "s3" | "file"`.
        way: String,
        /// The way-specific target (endpoint / bucket URL / file path).
        target: String,
    },
    /// Advance the (mock) restore one step. Sent by the engine's own ticker;
    /// answered with an error once the run is over (which stops the ticker).
    RestoreTick,
    /// Abandon the restore (idle again) and return to the choice screen.
    RestoreCancel,
    /// Finish a successful restore: the restored workspace becomes active and
    /// the node moves straight to the main screen.
    RestoreFinish,

    // --- founding a republic (shared, co-equal) ---
    /// Begin the (mock) founding run: validates the configuration, derives the
    /// recovery phrase and the n−1 invite links, and moves the lifecycle to
    /// the run view; the engine ticks the progress and the live log by itself.
    CreateStart {
        /// The new republic's name (must be unique locally).
        name: String,
        /// The founder's handle.
        member: String,
        /// The approval threshold (m), `1..=members`.
        threshold: u8,
        /// The member count (n), `2..=13`.
        members: u8,
        /// Transport: `"tor" | "nym" | "none"`.
        net: String,
    },
    /// Advance the (mock) founding one step. Sent by the engine's own ticker;
    /// answered with an error once the run is over (which stops the ticker).
    CreateTick,
    /// Abandon the founding (idle again) and return to the choice screen.
    CreateCancel,
    /// Finish a successful founding: the new republic joins the local list,
    /// becomes active, and the node moves straight to the main screen.
    CreateFinish,

    // --- joining via invite (shared, co-equal) ---
    /// Begin the (mock) join run for an invite link; the engine ticks the
    /// progress and the live log by itself. Any non-empty invite is accepted
    /// for now — real validation comes with the network implementation.
    JoinStart {
        /// The `molt://invite/…` link.
        invite: String,
        /// The joiner's handle.
        member: String,
    },
    /// Advance the (mock) join one step. Sent by the engine's own ticker;
    /// answered with an error once the run is over (which stops the ticker).
    JoinTick,
    /// Abandon the join (idle again) and return to the choice screen.
    JoinCancel,
    /// Finish a successful join: the joined republic appears in the local
    /// list, becomes active, and the node moves straight to the main screen.
    JoinFinish,
}

impl Command {
    /// The snake_case names of every command variant — the co-equality
    /// audit in `molt-mcp` checks the tool catalogue against this list.
    pub fn variant_names() -> &'static [&'static str] {
        <Command as strum::VariantNames>::VARIANTS
    }
}

/// The synchronous answer to a [`Command`]. Streaming changes arrive separately
/// as [`Event`]s on the broadcast channel.
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
    /// The whole shared session state (boxed: it is by far the largest reply).
    Session(Box<SessionView>),
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
}

/// A projected snapshot of one surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceSnapshot {
    /// Which surface.
    pub surface: Surface,
    /// Whether it is threshold-gated.
    pub gated: bool,
    /// The ordered log of applied transitions (for chat, the messages).
    pub applied: Vec<Value>,
    /// Proposals still pending against this surface.
    pub pending: Vec<ProposalView>,
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
    /// A demo 2-of-3 group used by the scaffold when nothing else is supplied.
    pub fn demo() -> Self {
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string(), "peer-1".to_string(), "peer-2".to_string()],
            threshold: 2,
            self_cosign: true,
        }
    }
}

/// Events broadcast to every attached operator (GUI live-mirror, MCP stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A chat message was posted.
    Chat {
        /// Sender.
        from: MemberId,
        /// Body.
        body: String,
    },
    /// A chat message was deleted (wiped and tombstoned).
    Deleted {
        /// The message's position in the chat log.
        index: u64,
        /// Who deleted it.
        by: MemberId,
    },
    /// A member's reaction on a chat message was toggled.
    Reacted {
        /// The message's position in the chat log.
        index: u64,
        /// The reaction emoji.
        emoji: String,
        /// Who toggled it.
        by: MemberId,
    },
    /// A proposal was created.
    Proposed {
        /// The proposal id.
        id: ProposalId,
        /// The surface.
        surface: Surface,
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
    /// A proposal was declined / rejected.
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
    /// A proposal was attempted on the ungated chat surface.
    #[error("chat is ungated — use the chat command, not propose")]
    ChatNotGated,
    /// The proposal payload was malformed.
    #[error("bad payload: {0}")]
    BadPayload(String),
    /// The proposal is already in a terminal state.
    #[error("proposal {0:?} is already {1:?}")]
    AlreadyTerminal(ProposalId, ProposalState),
    /// A settings value failed validation (nothing was stored or written).
    #[error("settings: {0}")]
    Settings(String),
    /// The named workspace is not in the local list.
    #[error("unknown workspace `{0}`")]
    UnknownWorkspace(String),
    /// The workspace is already open (locally or by another process).
    #[error("workspace is busy: {0}")]
    WorkspaceBusy(String),
    /// A storage operation failed (I/O, corruption, wrong key, …).
    #[error("storage: {0}")]
    Storage(String),
    /// The named sub-view does not exist on the given surface.
    #[error("surface {0:?} has no view `{1}`")]
    UnknownView(Surface, String),
    /// The chat log has no message at this position.
    #[error("unknown chat message {0}")]
    UnknownMessage(u64),
    /// A restore action arrived in the wrong lifecycle state.
    #[error("restore: {0}")]
    Restore(String),
    /// A founding action was invalid or arrived in the wrong lifecycle state.
    #[error("create: {0}")]
    Create(String),
    /// A join action was invalid or arrived in the wrong lifecycle state.
    #[error("join: {0}")]
    Join(String),
    /// The engine task is gone or did not answer.
    #[error("engine: {0}")]
    Engine(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(InviteInfo::parse(&link), Some(inv));

        for bad in [
            "",
            "molt://invite/",
            "smp://not-an-invite",
            "molt://invite/X/3of2/w/abcdefgh", // threshold > members
            "molt://invite/X/2of3/w/ab",       // ticket too short
            "molt://invite/X/2of3/w/abcdefgh/x", // trailing segment
        ] {
            assert_eq!(InviteInfo::parse(bad), None, "should reject `{bad}`");
        }
    }

    #[test]
    fn mock_generators_are_deterministic() {
        let ticket = mock_ticket(42);
        assert_eq!(ticket.len(), 10);
        assert_eq!(ticket, mock_ticket(42));
        assert_ne!(ticket, mock_ticket(43));
    }

    #[test]
    fn event_envelope_roundtrips_and_unknown_variants_fall_back_raw() {
        let env = EventEnvelope {
            seq: 7,
            ts: 1_751_700_000,
            by: "mithra".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 3,
                emoji: "🔥".to_string(),
                by: "mithra".to_string(),
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

    #[test]
    fn demo_workspace_ids_are_stable_and_distinct() {
        let a = demo_workspace_id("Family Office");
        assert_eq!(a.len(), 64);
        assert_eq!(a, demo_workspace_id("Family Office"));
        assert_ne!(a, demo_workspace_id("Savings-DAO"));
    }
}
