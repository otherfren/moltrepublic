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

/// The republic's persistent-change chain: a single-branch, threshold-signed
/// sequence of commit blocks (the founding is block 0). See [`chain`].
pub mod chain;
pub use chain::{
    approval_bytes, block_link_bytes, ChainBlock, ChainChange, MembershipOp, GENESIS_PREV,
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

/// The shared surfaces. [`Surface::Organization`] is a read-only info area (it
/// carries the ratified charter + roster) and [`Surface::Chat`] is ungated; the
/// other four change the shared state only through a threshold-approved proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// Who this republic is: status (with the ratified charter), roster,
    /// statistics. Read-only.
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
        !matches!(
            self,
            Surface::Chat | Surface::Organization
        )
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

    /// The view a surface opens on (the first of [`Surface::views`], or `""`
    /// for a surface with no sub-views — e.g. Constitution).
    pub fn default_view(self) -> &'static str {
        self.views().first().map(|v| v.0).unwrap_or("")
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
    /// SMP server selection: `"public"` (bundled default) or `"custom"`.
    #[serde(default)]
    pub smp_server: String,
    /// Custom SMP server URL, used when `smp_server = "custom"`.
    #[serde(default)]
    pub smp_url: String,
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
            smp_server: "public".to_string(),
            smp_url: String::new(),
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
                agenda: String::new(),
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

/// Metadata of a file shared into the chat. Only metadata travels — the
/// bytes stay on the sharer's disk; participants download from there as
/// long as the file exists (the fetch itself is the transport's job, next
/// story; today it is mocked). When the sharer deletes the local file the
/// share flips to unavailable for everyone, permanently.
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
}

fn file_available_default() -> bool {
    true
}

impl ChatMessage {
    /// A plain text message — the one constructor chat posts and test
    /// builders share, so the default-field shape cannot drift.
    pub fn text(from: impl Into<MemberId>, body: impl Into<String>, ts: u64) -> ChatMessage {
        ChatMessage {
            from: from.into(),
            body: body.into(),
            ts,
            quote: None,
            reactions: BTreeMap::new(),
            deleted_by: None,
            file: None,
        }
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
    /// This republic's other members are in-process simulations (founded
    /// before the real network exists, T3): the node runs their loopback
    /// peer engines while the workspace is open. Never set on workspaces
    /// whose members joined over a real transport.
    #[serde(default)]
    pub simulated_members: bool,
}

impl Default for WorkspacePrefs {
    fn default() -> Self {
        WorkspacePrefs {
            format: PREFS_FORMAT.to_string(),
            version: STORAGE_VERSION,
            s3_backup: false,
            last_backup: None,
            simulated_members: false,
        }
    }
}

/// Format marker of `transport.state` (the node-local encrypted transport
/// bookkeeping file — concept-transport-simplex-tor.md §6). v2 added the
/// `identity_sk` field (additive; a v1 file loads with it defaulting to `None`).
pub const TRANSPORT_STATE_VERSION: u32 = 2;

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
}

/// One peer's runtime **full-mesh handover** (concept §3.2/§3.3): the per-pair
/// queues a node uses to reach and hear one peer. All fields are strings so
/// `molt-core` keeps no transport dependency — `molt-net` parses them into a
/// `PeerLink`. `snd_*` is the peer's inbound queue this node SENDS to; `rcv_*`
/// is this node's own inbound queue it RECEIVES on from that peer (each party
/// owns the queue it receives on). Persisted so a reopened workspace rebuilds
/// its mesh without re-bootstrapping (real SMP queues live on their servers;
/// the ephemeral loopback hub rebuilds fresh).
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
}

/// `transport.state` — node-local transport bookkeeping (concept §6):
/// delivery cursors today; per-queue wrapping keys, MLS ratchets and the
/// dedup windows join in later milestones. It must **not** live in the
/// shared log: two nodes' cursors legitimately differ, and the log stays
/// replayable shared history. Losing this file loses transport progress
/// (peers absorb the resulting resends via their dedup), never history.
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
    /// persisted MLS signer (one identity, two anchors). `None` before a
    /// chain-aware founding.
    #[serde(default)]
    pub identity_sk: Option<Vec<u8>>,
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
    /// The deliberated free-text charter every member ratified (concept §3.3).
    #[serde(default)]
    pub agenda: String,
}

impl SealedRoster {
    /// Build the local `Founded` genesis envelope for a member. The single
    /// place a `Founded` body is constructed for a real founding, so a new
    /// genesis field cannot be forgotten at one of the call sites (founder
    /// finalize, GUI join, standalone join). `member` is this node's own local
    /// handle; `ts` the founding timestamp.
    pub fn into_genesis(&self, member: &str, ts: u64) -> EventEnvelope {
        EventEnvelope {
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
            },
        }
    }
}

/// The one canonical serialization of a roster table — what every member
/// signs during the founding ritual's seal round and what every verifier
/// reconstructs. Length-prefixed fields, entries in the given order (the
/// ritual fixes the order: founder first, then invite order).
pub fn roster_canonical_bytes(
    ws_id: &str,
    rule_m: u8,
    rule_n: u8,
    members: &[MemberIdentity],
    agenda: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"molt-roster-v2\0");
    out.extend_from_slice(ws_id.as_bytes());
    out.push(rule_m);
    out.push(rule_n);
    for m in members {
        let name = m.member.as_bytes();
        out.extend_from_slice(&u32::try_from(name.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(name);
        let pk = m.identity_pk.as_bytes();
        out.extend_from_slice(&u32::try_from(pk.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(pk);
    }
    // the deliberated charter (DAO name is already folded into the republic id
    // that salts ws_id; the free-text agenda is bound here) — every member's
    // seal signature is its ratification of exactly these bytes
    let ag = agenda.as_bytes();
    out.extend_from_slice(&u32::try_from(ag.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(ag);
    out
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
    /// The sharer deleted a shared file from their disk — the share at
    /// this chat position is unavailable from now on.
    FileRemoved {
        /// The share message's position in the chat log (0-based).
        index: u64,
        /// The sharer.
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
    /// committer bundles into the block); both default empty on the legacy
    /// counted-simulation path.
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
    /// The anchored name → identity-key table (empty on pre-ritual
    /// workspaces).
    #[serde(default)]
    pub identities: Vec<MemberIdentity>,
    /// The ratified founding charter (free-text agenda), so a snapshot-restored
    /// workspace keeps it (the genesis is before the snapshot and not replayed).
    #[serde(default)]
    pub agenda: String,
    /// The neutral, content-derived republic id (the genesis' value). Kept at
    /// runtime so the persistent-chain path can recompute `approval_bytes`
    /// without re-deriving it; a snapshot-restored open keeps it too (the
    /// genesis is before the snapshot and not replayed). Empty on a pre-republic
    /// genesis.
    #[serde(default)]
    pub republic_id: String,
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
    /// 2 = sealed (signature verified).
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
    /// Transport: `"tor" | "nym" | "none"`.
    pub net: String,
    /// The founder's recovery phrase (shown during the ritual, then gone).
    pub seed: String,
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
    #[serde(default)]
    pub seed: String,
    /// The founder's proposed final DAO name, surfaced when the ritual reaches
    /// the ratification step (empty until then).
    #[serde(default)]
    pub proposed_name: String,
    /// The founder's proposed free-text charter/agenda, for the joiner to read
    /// before ratifying (empty until the ratification step).
    #[serde(default)]
    pub proposed_agenda: String,
    /// Whether the join is paused awaiting the joiner's ratification of the
    /// charter above — the wizard shows the charter + a confirm/decline choice,
    /// and the workspace opens only once confirmed.
    #[serde(default)]
    pub awaiting_ratify: bool,
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
    /// Transient result of the settings panel's "Test connection" against
    /// the SMP server: `""` (untested), `"testing"`, `"ok"`, or `"error: …"`.
    /// Never persisted; lives here (not in [`SessionSettings`]) so a test in
    /// flight does not look like an unsaved settings edit.
    #[serde(default)]
    pub smp_test: String,
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
    /// The founding lifecycle (real over SMP).
    pub create: CreateState,
    /// The join-via-invite lifecycle (real over SMP).
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
            smp_test: String::new(),
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
    /// Share a file into the ungated chat. Only the METADATA is posted —
    /// the bytes never leave this node's disk; participants download from
    /// there while the file exists (the fetch is the transport's job, next
    /// story; mocked today).
    ShareFile {
        /// File name (no path).
        name: String,
        /// Size in bytes.
        size: u64,
        /// Display type, e.g. `"PDF"`.
        kind: String,
        /// The file's own date, unix seconds (0 = stamp now).
        modified: u64,
    },
    /// Download a shared file from the sharer's disk (mock: validates
    /// availability, moves no bytes). Fails once the sharer deleted the
    /// local file.
    DownloadFile {
        /// The share message's position in the chat log (0-based).
        index: u64,
    },
    /// Sharer-only: the local file is gone (deleted from this disk) — the
    /// share becomes permanently unavailable for every participant.
    RemoveFile {
        /// The share message's position in the chat log (0-based).
        index: u64,
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
    /// Begin the founding ritual: validates the configuration, derives the
    /// founder's recovery phrase and identity, mints the n−1 one-time
    /// invite links and opens their invite queues. The workspace is
    /// created only when every member activated their link AND signed the
    /// final roster (transport concept §3.3) — until then nothing exists
    /// on disk, and closing the wizard voids the links.
    CreateStart {
        /// The new republic's name.
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
    },
    /// Abandon the founding ritual: distributed links become worthless,
    /// the disk stays untouched (unless the ritual already sealed — then
    /// the created workspace stays listed, just not entered).
    CreateCancel,
    /// Enter the sealed republic: refused until every member is green
    /// (key received and roster signature verified — the engine enforces
    /// this for every operator, not just the GUI).
    CreateFinish,

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
        /// `HMAC(KDF(ticket), name ‖ pk)`, lowercase hex.
        proof: String,
        /// The member's reply-queue handover (JSON of the transport's
        /// `ReplyHandover`) so the founder can send the canonical table
        /// back. Opaque here — core has no transport dependency. Empty on
        /// the legacy path where the founder pre-created the reply queue.
        #[serde(default)]
        reply: String,
        /// The member's MLS KeyPackage (hex of the wire bytes) so the founder
        /// can add it to the group. Empty on a pre-MLS path.
        #[serde(default)]
        key_package: String,
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
        /// The seat proof: `sign(identity, ticket ‖ key_package ‖ republic_id)`.
        seat_proof: String,
        /// The member's reply-queue handover, for the coordinator to send the
        /// Welcome back once re-admission commits. Opaque here.
        #[serde(default)]
        reply: String,
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
        /// Ritual incarnation (stale ritual commands are dropped).
        #[serde(default)]
        generation: Option<u64>,
    },
    /// Test connectivity to an SMP server (the settings panel's Test
    /// button): a live TLS handshake, run off the actor, whose result lands
    /// in `settings.smp_test`. Safe to expose — it only dials a server.
    NetTestServer {
        /// The `smp://<fingerprint>@host[:port]` URL to test. Empty tests
        /// the currently-configured server (public default or custom URL).
        #[serde(default)]
        url: String,
    },
    /// The outcome of a [`Command::NetTestServer`] handshake, reported back
    /// from the off-actor probe task (engine-internal, never an MCP tool).
    NetTestResult {
        /// `"ok"` or `"error: …"`; written verbatim into `settings.smp_test`.
        result: String,
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

    // --- joining via invite (shared, co-equal) ---
    /// Begin joining a republic from its `molt://invite/…` link. The link must
    /// carry the SMP transport handover (a bare preview link is rejected); the
    /// engine runs the real join over SMP off the actor, shows the joiner's own
    /// recovery phrase, and enters the republic on its own once the founder
    /// seals — its outcome arrives as `NetJoinSealed` / `NetJoinFailed`.
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
    /// A member explicitly declined the proposed charter (engine-internal;
    /// raised by the founder's ritual recv loop). The founder marks the seat and
    /// logs it. Never an MCP tool.
    NetJoinDeclined {
        /// Which seat (0-based invite index) declined.
        seat: u32,
        /// Ritual incarnation (stale ritual commands are dropped).
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
    /// A shared file became unavailable (its sharer deleted it locally).
    FileRemoved {
        /// The share message's position in the chat log.
        index: u64,
        /// The sharer.
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
    /// The chat message at this position carries no shared file.
    #[error("message {0} has no shared file")]
    NoFile(u64),
    /// The shared file's owner deleted it locally; nothing to download.
    #[error("the shared file at message {0} is no longer available")]
    FileUnavailable(u64),
    /// Only the member who shared a file can remove it.
    #[error("only the member who shared the file at message {0} can remove it")]
    NotYourFile(u64),
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

    #[test]
    fn roster_canonical_bytes_are_stable_and_field_separated() {
        let table = vec![
            MemberIdentity {
                member: "petra".to_string(),
                identity_pk: "aa".repeat(32),
            },
            MemberIdentity {
                member: "walter".to_string(),
                identity_pk: "bb".repeat(32),
            },
        ];
        let a = roster_canonical_bytes("f00", 2, 3, &table, "charter");
        assert_eq!(a, roster_canonical_bytes("f00", 2, 3, &table, "charter"));
        // any changed field changes the bytes
        assert_ne!(a, roster_canonical_bytes("f01", 2, 3, &table, "charter"));
        assert_ne!(a, roster_canonical_bytes("f00", 3, 3, &table, "charter"));
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table[..1], "charter"));
        // the ratified agenda is bound: a changed charter changes the bytes
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table, "other"));
        assert_ne!(a, roster_canonical_bytes("f00", 2, 3, &table, ""));
        // length prefixes prevent name/pk boundary games
        let shifted = vec![MemberIdentity {
            member: "petraa".to_string(),
            identity_pk: format!("a{}", "a".repeat(63)),
        }];
        let plain = vec![MemberIdentity {
            member: "petra".to_string(),
            identity_pk: format!("aa{}", "a".repeat(62)),
        }];
        assert_ne!(
            roster_canonical_bytes("f00", 1, 1, &shifted, ""),
            roster_canonical_bytes("f00", 1, 1, &plain, "")
        );
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
