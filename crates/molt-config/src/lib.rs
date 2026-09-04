// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-config`: the node's on-disk configuration (`config.toml`).
//!
//! This crate is the **one source of truth** for the config schema, its
//! rendering, and its lenient salvage. Both operators of the file use it:
//!
//! * the `moltd` binary (CLI: discover / load / `--generate-config` /
//!   `--repair-config`), and
//! * the GUI settings panel, which edits values at runtime and writes them back.
//!
//! Because both go through the same [`render`], a config written by one is read
//! by the other, and a runtime change round-trips through the exact renderer the
//! CLI uses — so a hand edit (app off) and an in-app edit (app running) produce
//! the same normalized file and neither silently loses the other's fields.
//!
//! The schema is parsed strictly ([`Config`], `deny_unknown_fields`); the
//! flat [`Settings`] view is the lenient salvage/render target.

use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Strict on-disk schema (config.toml), parsed with deny_unknown_fields.
// ---------------------------------------------------------------------------

/// On-disk node configuration (`config.toml`), parsed strictly at startup.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Node-level runtime settings.
    #[serde(default)]
    pub node: NodeConfig,
    /// Where local state is stored.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Network anonymity settings.
    #[serde(default)]
    pub transport: TransportConfig,
    /// MCP server settings.
    #[serde(default)]
    pub mcp: McpConfig,
    /// GUI settings.
    #[serde(default)]
    pub ui: UiConfig,
    /// The file plane (`[files]`).
    #[serde(default)]
    pub files: FilesConfig,
}

/// Node-level runtime settings (`[node]`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Start without a GUI (MCP-only).
    #[serde(default)]
    pub headless: bool,
    /// React to pokes (toast, sound, wake command) and offer poking in the
    /// UI. Off by default — an explicit opt-in.
    #[serde(default)]
    pub poke_enabled: bool,
    /// Command run via `sh -c` when this seat is poked or new work awaits
    /// its vote (context in `MOLT_WAKE_*` env vars, `MOLT_WAKE_PENDING`
    /// carrying the queue size). Empty = off.
    #[serde(default)]
    pub poke_wake_command: String,
}

/// The file plane's pacing (`[files]`, `docs_archive/files/mirroring.md` §3.2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    /// Seconds between two piece publishes of the trickle sender (at least 1).
    #[serde(default = "default_mirror_publish_interval_secs")]
    pub mirror_publish_interval_secs: u64,
    /// Piece bytes the trickle sender may publish per UTC day.
    #[serde(default = "default_mirror_daily_bytes")]
    pub mirror_daily_bytes: u64,
}

/// The trickle sender's default pace. Mirrors `molt_core::SessionSettings`.
pub fn default_mirror_publish_interval_secs() -> u64 {
    15
}

/// The trickle sender's default daily budget (512 MiB).
pub fn default_mirror_daily_bytes() -> u64 {
    512 * 1024 * 1024
}

impl Default for FilesConfig {
    fn default() -> Self {
        FilesConfig {
            mirror_publish_interval_secs: default_mirror_publish_interval_secs(),
            mirror_daily_bytes: default_mirror_daily_bytes(),
        }
    }
}

/// Local storage settings (`[storage]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory holding this node's per-group workspaces. `~` expands to $HOME.
    #[serde(default = "default_workspace_dir")]
    pub workspace_dir: String,
    /// Automatically back workspaces up to an S3-compatible store.
    #[serde(default)]
    pub s3_backup: bool,
    /// S3 endpoint / bucket URL the automatic backup targets.
    #[serde(default)]
    pub s3_endpoint: String,
    /// S3 access key id.
    #[serde(default)]
    pub s3_access_key: String,
    /// S3 secret key.
    #[serde(default)]
    pub s3_secret_key: String,
    /// S3 bucket name.
    #[serde(default = "default_s3_bucket")]
    pub s3_bucket: String,
    /// Automatic-backup interval in minutes.
    #[serde(default = "default_s3_interval_min")]
    pub s3_interval_min: u16,
    /// How many automatic-backup copies to keep per workspace.
    #[serde(default = "default_s3_keep_copies")]
    pub s3_keep_copies: u16,
    /// Byte quota for the backup bucket (only what this node wrote counts).
    /// 0 = no limit. Mirrors `molt_core::SessionSettings::s3_max_bytes`.
    #[serde(default)]
    pub s3_max_bytes: u64,
    /// Media bucket at the SAME endpoint and credentials (empty = not
    /// configured). Mirrors `molt_core::SessionSettings::media_s3_bucket`.
    #[serde(default)]
    pub media_s3_bucket: String,
    /// Byte quota for the media bucket, 0 = no limit.
    #[serde(default)]
    pub media_s3_max_bytes: u64,
    /// Where downloaded chat files land when no explicit destination is
    /// given. `~` expands to $HOME.
    #[serde(default = "default_download_dir")]
    pub download_dir: String,
    /// Per-file byte cap for sharing: absent = no cap, 0 = sharing off,
    /// n = a deliberate cap.
    #[serde(default)]
    pub file_cap_bytes: Option<u64>,
    /// Alert sound for an incoming chat message ("none"|"bell"|"chime"|"pop").
    #[serde(default = "default_sound")]
    pub sound_message: String,
    /// Alert sound for a new incoming vote, same vocabulary.
    #[serde(default = "default_sound")]
    pub sound_vote: String,
    /// Alert sound for an incoming poke, same vocabulary.
    #[serde(default = "default_sound")]
    pub sound_poke: String,
    /// Send (and show) per-message chat read receipts (local privacy switch,
    /// on by default). Mirrors `molt_core::SessionSettings::read_receipts`.
    #[serde(default = "default_true")]
    pub read_receipts: bool,
}

/// Default alert sound — silent. Mirrors `molt_core::SessionSettings`.
pub fn default_sound() -> String {
    "none".to_string()
}

/// Default for an opt-out boolean — on unless the operator disables it (and
/// present-by-absence in an older `config.toml`).
pub fn default_true() -> bool {
    true
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            workspace_dir: default_workspace_dir(),
            s3_backup: false,
            s3_endpoint: String::new(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_bucket: default_s3_bucket(),
            s3_interval_min: default_s3_interval_min(),
            s3_keep_copies: default_s3_keep_copies(),
            s3_max_bytes: 0,
            media_s3_bucket: String::new(),
            media_s3_max_bytes: 0,
            download_dir: default_download_dir(),
            file_cap_bytes: None,
            sound_message: default_sound(),
            sound_vote: default_sound(),
            sound_poke: default_sound(),
            read_receipts: true,
        }
    }
}

/// Transport settings (`[transport]`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    /// Anonymity settings for node traffic.
    #[serde(default)]
    pub anonymity: AnonymityConfig,
    /// The Nostr relay pool.
    #[serde(default)]
    pub nostr: NostrConfig,
}

/// Nostr transport settings (`[transport.nostr]`) — currently just the relay
/// pool. **No relay ships with the app**: the node connects to nothing until
/// its operator adds one and confirms it (`docs_archive/transport/relay_pool.md`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NostrConfig {
    /// The relay pool as `[[transport.nostr.relay]]` tables — **the file
    /// order is the dial priority** (the first entry is tried first).
    #[serde(default)]
    pub relay: Vec<RelayConfig>,
    /// Whether this node may dial NON-onion relays (clearnet, LAN,
    /// loopback — anything reached outside Tor). Off by default. Set by
    /// acknowledging the exposure when confirming such a relay, and cleared
    /// by switching clearnet off; persisted so the decision survives a
    /// restart (ADR-0004 amendment 2026-07-31 — it used to reset on every
    /// start, which made the operator re-perform the same consent forever).
    #[serde(default)]
    pub clearnet_enabled: bool,
}

/// One relay in the pool. The onion/clearnet kind is deliberately NOT a field:
/// it is derived from the URL wherever it is needed, so a hand-edited file
/// cannot label a clearnet relay as onion and walk through the clearnet gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    /// `wss://…` (or `ws://…` for a `.onion` host).
    pub url: String,
    /// The operator's persisted "yes, use this relay". Unconfirmed relays are
    /// never dialed, so a relay pasted into the file still connects to
    /// nothing until it is confirmed.
    #[serde(default)]
    pub confirmed: bool,
}

/// How node traffic is anonymized: which network, plus that network's settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnonymityConfig {
    /// Which anonymity network to route node traffic over.
    #[serde(default)]
    pub network: AnonymityNetwork,
    /// Local Tor SOCKS proxy settings (used when `network = "tor"`).
    #[serde(default)]
    pub tor: TorConfig,
}

/// Anonymity network selection. Unknown values are rejected by the parser.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnonymityNetwork {
    /// Route over Tor. Explicit opt-in — a fail-closed dialer refuses every
    /// direct dial once this is selected (transport concept §6).
    Tor,
    /// No anonymity network (clearnet) — the shipped default; the user opts
    /// into Tor.
    #[default]
    None,
}

/// Tor settings used when `network = "tor"`: how Tor is provided, plus the
/// parameters that mode needs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TorConfig {
    /// How Tor is provided (local daemon, embedded proxy, or transparent env).
    #[serde(default)]
    pub mode: TorMode,
    /// SOCKS port of the local Tor daemon (only used when `mode = "local"`).
    #[serde(default = "default_tor_port")]
    pub port: u16,
}

impl Default for TorConfig {
    fn default() -> Self {
        TorConfig {
            mode: TorMode::default(),
            port: default_tor_port(),
        }
    }
}

/// How the node reaches the Tor network. Unknown values are rejected by the parser.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TorMode {
    /// Dial an external Tor daemon's local SOCKS proxy (on `port`).
    #[default]
    Local,
    /// Run an integrated Tor proxy inside the node.
    Embedded,
    /// Rely on transparent torification by the environment (Whonix/Tails);
    /// Tor works out of the box, so `port` is ignored.
    Whonix,
}

/// MCP server settings (`[mcp]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// TCP port the MCP server listens on (always available, every mode).
    #[serde(default = "default_mcp_port")]
    pub port: u16,
    /// Which client IP(s) may connect over TCP (allowlist). `"127.0.0.1"` =
    /// loopback only, `"0.0.0.0"` = any, or a comma-separated list.
    #[serde(default = "default_mcp_allow")]
    pub allow: String,
    /// API key every MCP client must present in its `initialize` request.
    #[serde(default)]
    pub token: String,
    /// A SECOND key admitting only the read tools
    /// (`docs/memory/knowledge_base_scale.md` §4.7). Empty or absent = off,
    /// never "unauthenticated". Written here only once one is issued, so a
    /// config an older build still opens stays that way.
    #[serde(default)]
    pub read_token: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            port: default_mcp_port(),
            allow: default_mcp_allow(),
            token: String::new(),
            read_token: String::new(),
        }
    }
}

/// GUI settings (`[ui]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// Initial GUI language (en / de).
    #[serde(default = "default_lang")]
    pub lang: String,
    /// Initial GUI theme (classic / dark / brutalism).
    #[serde(default = "default_theme")]
    pub theme: String,
    /// App-chrome font size in px.
    #[serde(default = "default_font_app")]
    pub font_app: u16,
    /// Wiki-navigator font size in px.
    #[serde(default = "default_font_nav")]
    pub font_nav: u16,
    /// Editor/document font size in px.
    #[serde(default = "default_font_editor")]
    pub font_editor: u16,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            lang: default_lang(),
            theme: default_theme(),
            font_app: default_font_app(),
            font_nav: default_font_nav(),
            font_editor: default_font_editor(),
        }
    }
}

/// The default download destination.
pub fn default_download_dir() -> String {
    "~/Downloads".to_string()
}

/// Default per-group workspace root.
pub fn default_workspace_dir() -> String {
    "~/.moltrepublic/workspaces".to_string()
}

/// Default S3 bucket name. Deliberately inconspicuous — the bucket listing
/// should not advertise what it holds. Mirrors `molt_core::SessionSettings`.
pub fn default_s3_bucket() -> String {
    "media-archive".to_string()
}

/// Default automatic-backup interval (minutes). Mirrors `molt_core::SessionSettings`.
pub fn default_s3_interval_min() -> u16 {
    60
}

/// Default number of automatic-backup copies kept per workspace. Mirrors
/// `molt_core::SessionSettings`.
pub fn default_s3_keep_copies() -> u16 {
    5
}

/// Default MCP server TCP port.
pub fn default_mcp_port() -> u16 {
    4040
}

/// Default local Tor SOCKS port.
pub fn default_tor_port() -> u16 {
    9050
}

/// Default GUI language.
pub fn default_lang() -> String {
    "en".to_string()
}

/// Default MCP client allowlist: loopback only.
pub fn default_mcp_allow() -> String {
    "127.0.0.1".to_string()
}

/// The default app-chrome font size in px (the historical `fs-body`).
pub fn default_font_app() -> u16 {
    14
}

/// The default wiki-navigator font size in px (the historical row size).
pub fn default_font_nav() -> u16 {
    13
}

/// The default editor/document font size in px.
pub fn default_font_editor() -> u16 {
    14
}

/// Default GUI theme.
pub fn default_theme() -> String {
    "classic".to_string()
}

/// A fresh random MCP API token: 24 bytes from the OS CSPRNG, hex-encoded.
///
/// `Err` rather than a fallback, and that is the whole point of the
/// signature: it used to open `/dev/urandom` by path and return `""` when
/// that failed — in a sandbox without the device node, or on any non-Unix
/// target, `--generate-config` then wrote `token = ""` and reported success.
/// An empty token disables MCP authentication (the check is a string
/// compare), so the quiet failure handed out an unauthenticated node.
pub fn random_token() -> Result<String, TokenError> {
    let mut buf = [0u8; 24];
    getrandom::getrandom(&mut buf).map_err(|e| TokenError(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The OS CSPRNG was unavailable. There is no safe fallback for a token that
/// gates a network surface, so this is reported rather than papered over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError(String);

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the OS random source is unavailable: {}", self.0)
    }
}

impl std::error::Error for TokenError {}

// ---------------------------------------------------------------------------
// Flat settings: the single source of defaults, used to render and salvage.
// ---------------------------------------------------------------------------

/// A flat view of every configurable value. It is the one place the written
/// defaults live, the target of lenient salvage during `--repair-config`, and
/// the shape the GUI settings panel edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Start without a GUI (MCP-only).
    pub headless: bool,
    /// Per-group workspace root (`~` allowed).
    pub workspace_dir: String,
    /// Automatically back workspaces up to an S3-compatible store.
    pub s3_backup: bool,
    /// S3 endpoint / bucket URL the automatic backup targets.
    pub s3_endpoint: String,
    /// S3 access key id.
    pub s3_access_key: String,
    /// S3 secret key.
    pub s3_secret_key: String,
    /// S3 bucket name.
    pub s3_bucket: String,
    /// Automatic-backup interval in minutes.
    pub s3_interval_min: u16,
    /// How many automatic-backup copies to keep per workspace.
    pub s3_keep_copies: u16,
    /// Byte quota for the backup bucket, 0 = no limit.
    pub s3_max_bytes: u64,
    /// Media bucket at the same endpoint/credentials (empty = unconfigured).
    pub media_s3_bucket: String,
    /// Byte quota for the media bucket, 0 = no limit.
    pub media_s3_max_bytes: u64,
    /// Where downloaded chat files land when no explicit destination is given.
    pub download_dir: String,
    /// Per-file byte cap for sharing: absent = no cap, 0 = off.
    pub file_cap_bytes: Option<u64>,
    /// Seconds between two piece publishes of the trickle sender.
    pub mirror_publish_interval_secs: u64,
    /// Piece bytes the trickle sender may publish per UTC day.
    pub mirror_daily_bytes: u64,
    /// Alert sound for an incoming chat message.
    pub sound_message: String,
    /// Alert sound for a new incoming vote.
    pub sound_vote: String,
    /// Alert sound for an incoming poke.
    pub sound_poke: String,
    /// React to pokes and offer poking in the UI (explicit opt-in).
    pub poke_enabled: bool,
    /// Wake command run when this seat is poked or new work awaits its vote.
    pub poke_wake_command: String,
    /// Send (and show) per-message chat read receipts (local privacy switch).
    pub read_receipts: bool,
    /// Anonymity network: `"tor" | "none"`.
    pub anonymity: String,
    /// Tor mode: `"local" | "embedded" | "whonix"`.
    pub tor_mode: String,
    /// Local Tor SOCKS port.
    pub tor_port: u16,
    /// MCP server TCP port.
    pub mcp_port: u16,
    /// MCP client allowlist (`"127.0.0.1" | "0.0.0.0" | comma-separated list`).
    pub mcp_allow: String,
    /// MCP API token clients must present.
    pub mcp_token: String,
    /// The read-only MCP token ("" = no read-only access).
    pub mcp_read_token: String,
    /// GUI language: `"en" | "de"`.
    pub lang: String,
    /// GUI theme: `"classic" | "dark" | "brutalism"`.
    pub theme: String,
    /// App-chrome font size in px.
    pub font_app: u16,
    /// Wiki-navigator font size in px.
    pub font_nav: u16,
    /// Editor/document font size in px.
    pub font_editor: u16,
    /// The Nostr relay pool in dial-priority order. Empty by default — the
    /// node connects to no relay until its operator adds and confirms one.
    pub relays: Vec<RelayConfig>,
    /// Whether non-onion (clearnet/LAN/loopback) relays may be dialed.
    pub clearnet_relays_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            headless: false,
            workspace_dir: default_workspace_dir(),
            s3_backup: false,
            s3_endpoint: String::new(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_bucket: default_s3_bucket(),
            s3_interval_min: default_s3_interval_min(),
            s3_keep_copies: default_s3_keep_copies(),
            s3_max_bytes: 0,
            media_s3_bucket: String::new(),
            media_s3_max_bytes: 0,
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
            anonymity: "none".to_string(),
            tor_mode: "local".to_string(),
            tor_port: default_tor_port(),
            mcp_port: default_mcp_port(),
            mcp_allow: default_mcp_allow(),
            mcp_token: String::new(),
            mcp_read_token: String::new(),
            lang: default_lang(),
            theme: default_theme(),
            font_app: default_font_app(),
            font_nav: default_font_nav(),
            font_editor: default_font_editor(),
            relays: Vec::new(),
            clearnet_relays_enabled: false,
        }
    }
}

impl AnonymityNetwork {
    /// The lowercase wire/config name (`"tor" | "none"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AnonymityNetwork::Tor => "tor",
            AnonymityNetwork::None => "none",
        }
    }
}

impl TorMode {
    /// The lowercase wire/config name (`"local" | "embedded" | "whonix"`).
    pub fn as_str(self) -> &'static str {
        match self {
            TorMode::Local => "local",
            TorMode::Embedded => "embedded",
            TorMode::Whonix => "whonix",
        }
    }
}

impl From<&Config> for Settings {
    /// Flatten a strictly-parsed [`Config`] into the editable [`Settings`] view
    /// the GUI binds to. Lossless for every field the schema carries.
    fn from(c: &Config) -> Self {
        Settings {
            headless: c.node.headless,
            workspace_dir: c.storage.workspace_dir.clone(),
            s3_backup: c.storage.s3_backup,
            s3_endpoint: c.storage.s3_endpoint.clone(),
            s3_access_key: c.storage.s3_access_key.clone(),
            s3_secret_key: c.storage.s3_secret_key.clone(),
            s3_bucket: c.storage.s3_bucket.clone(),
            s3_interval_min: c.storage.s3_interval_min,
            s3_keep_copies: c.storage.s3_keep_copies,
            s3_max_bytes: c.storage.s3_max_bytes,
            media_s3_bucket: c.storage.media_s3_bucket.clone(),
            media_s3_max_bytes: c.storage.media_s3_max_bytes,
            download_dir: c.storage.download_dir.clone(),
            file_cap_bytes: c.storage.file_cap_bytes,
            mirror_publish_interval_secs: c.files.mirror_publish_interval_secs,
            mirror_daily_bytes: c.files.mirror_daily_bytes,
            sound_message: c.storage.sound_message.clone(),
            sound_vote: c.storage.sound_vote.clone(),
            sound_poke: c.storage.sound_poke.clone(),
            poke_enabled: c.node.poke_enabled,
            poke_wake_command: c.node.poke_wake_command.clone(),
            read_receipts: c.storage.read_receipts,
            anonymity: c.transport.anonymity.network.as_str().to_string(),
            tor_mode: c.transport.anonymity.tor.mode.as_str().to_string(),
            tor_port: c.transport.anonymity.tor.port,
            relays: c.transport.nostr.relay.clone(),
            clearnet_relays_enabled: c.transport.nostr.clearnet_enabled,
            mcp_port: c.mcp.port,
            mcp_allow: c.mcp.allow.clone(),
            mcp_token: c.mcp.token.clone(),
            mcp_read_token: c.mcp.read_token.clone(),
            lang: c.ui.lang.clone(),
            theme: c.ui.theme.clone(),
            font_app: c.ui.font_app,
            font_nav: c.ui.font_nav,
            font_editor: c.ui.font_editor,
        }
    }
}

/// Quote and escape a string as a TOML basic string.
fn toml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render a fully-commented, valid `config.toml` from `settings`.
pub fn render(settings: &Settings) -> String {
    format!(
        r#"# MoltRepublic.ai node configuration.

[node]
# true = headless (MCP-only, no GUI).
headless = {headless}
# React to pokes (toast, sound, wake command) and offer poking in the UI.
poke_enabled = {poke_enabled}
# Command run via `sh -c` when this seat is poked or new work awaits its
# vote - wakes a sleeping agent harness. "" = off. Context arrives as the
# env vars MOLT_WAKE_REASON (poked|vote_pending), MOLT_WAKE_BY,
# MOLT_WAKE_WORKSPACE and MOLT_WAKE_PENDING (cards awaiting this seat);
# always QUOTE them ("$MOLT_WAKE_BY"). One wake runs at a time and a burst
# nudges once: loop list_proposals until nothing waits. Only this file and
# the GUI set it: it is executed here, so no MCP client may plant one.
poke_wake_command = {poke_wake_command}

[storage]
# Per-group workspace root. "~" = $HOME.
workspace_dir = {workspace_dir}
# ONE S3 account - endpoint and credentials - shared by every bucket below.
s3_endpoint = {s3_endpoint}
s3_access_key = {s3_access_key}
s3_secret_key = {s3_secret_key}
# Bucket 1: automatic backup of workspaces.
s3_backup = {s3_backup}
s3_bucket = {s3_bucket}
# Automatic-backup interval in minutes.
s3_interval_min = {s3_interval_min}
# Keep at most this many backup copies per workspace.
s3_keep_copies = {s3_keep_copies}
# Byte quota for the backup bucket. 0 = no limit. Counts only the backups this
# node wrote; over the limit the oldest copies go first, never a workspace's
# newest one.
s3_max_bytes = {s3_max_bytes}
# Bucket 2: media, at the same endpoint and credentials.
# Configured here, but nothing writes media to S3 yet.
media_s3_bucket = {media_s3_bucket}
# Byte quota for the media bucket. 0 = no limit.
media_s3_max_bytes = {media_s3_max_bytes}
# Where downloaded chat files land ("~" = $HOME).
download_dir = {download_dir}
# Per-file byte cap for sharing. 0 = file sharing off; absent = no cap.
# 4194304 is the old default and reads as no cap; a deliberate cap near
# 4 MiB uses another value.
{file_cap_line}
# Alert sounds: "none" | "bell" | "chime" | "pop".
sound_message = {sound_message}
sound_vote = {sound_vote}
sound_poke = {sound_poke}
# Send (and show) per-message chat read receipts. false = this node reveals no
# read confirmations and hides others' from its chat view (symmetric).
read_receipts = {read_receipts}

[files]
# The trickle sender: seconds between two piece publishes (at least 1) and
# the piece bytes it may publish per UTC day.
mirror_publish_interval_secs = {mirror_publish_interval_secs}
mirror_daily_bytes = {mirror_daily_bytes}

[mcp]
# MCP server TCP port. Always served (UI + headless).
port = {mcp_port}
# allow = which client IPs may connect over TCP:
#   "127.0.0.1" = loopback only (default)
#   "0.0.0.0"   = any address (careful - this exposes the node)
#   or a comma-separated allowlist, e.g. "127.0.0.1, 192.168.1.10"
# Connections from IPs not on the list are refused.
allow = {mcp_allow}
# API key every MCP client must send in its initialize request. Keep it secret;
# rotate it from the GUI settings. A fresh token is written on --generate-config.
token = {mcp_token}
# A second key admitting only the READ tools can be issued in the GUI
# (Settings > MCP); it is written here as read_token. Absent = off.

[transport.anonymity]
# network = "tor" | "none" (default "none" = clearnet). "tor" routes
# every network dial through Tor, fail-closed (no silent clearnet fallback).
network = {anonymity}

[transport.anonymity.tor]
# mode = how Tor is reached (only when network = "tor"):
#   "local"    = external tor daemon SOCKS proxy on `port`
#   "embedded" = in-process tor proxy
#   "whonix"   = transparent torification by env (Whonix/Tails); `port` ignored
mode = {tor_mode}
# Local tor SOCKS port. Used only when mode = "local".
port = {tor_port}

{relay_doc}[transport.nostr]
# May this node dial relays that are NOT onion services (clearnet, LAN,
# loopback)? Off by default. Confirming such a relay with its explicit
# exposure acknowledgement sets this to true and KEEPS it - the decision is
# remembered instead of being asked again after every restart.
clearnet_enabled = {clearnet_enabled}
{relays}
[ui]
# GUI language: "en" | "de".
lang = {lang}
# GUI theme: "classic" | "dark" | "brutalism".
theme = {theme}
# Font sizes in px: app chrome, wiki navigator, editor/document.
font_app = {font_app}
font_nav = {font_nav}
font_editor = {font_editor}
"#,
        headless = settings.headless,
        poke_enabled = settings.poke_enabled,
        poke_wake_command = toml_str(&settings.poke_wake_command),
        workspace_dir = toml_str(&settings.workspace_dir),
        s3_backup = settings.s3_backup,
        s3_endpoint = toml_str(&settings.s3_endpoint),
        s3_access_key = toml_str(&settings.s3_access_key),
        s3_secret_key = toml_str(&settings.s3_secret_key),
        s3_bucket = toml_str(&settings.s3_bucket),
        s3_interval_min = settings.s3_interval_min,
        s3_keep_copies = settings.s3_keep_copies,
        s3_max_bytes = settings.s3_max_bytes,
        media_s3_bucket = toml_str(&settings.media_s3_bucket),
        media_s3_max_bytes = settings.media_s3_max_bytes,
        download_dir = toml_str(&settings.download_dir),
        file_cap_line = match settings.file_cap_bytes {
            Some(n) => format!("file_cap_bytes = {n}"),
            None => "# file_cap_bytes = 4194304".to_string(),
        },
        sound_message = toml_str(&settings.sound_message),
        sound_vote = toml_str(&settings.sound_vote),
        sound_poke = toml_str(&settings.sound_poke),
        read_receipts = settings.read_receipts,
        mirror_publish_interval_secs = settings.mirror_publish_interval_secs,
        mirror_daily_bytes = settings.mirror_daily_bytes,
        mcp_port = settings.mcp_port,
        mcp_allow = toml_str(&settings.mcp_allow),
        mcp_token = toml_str(&settings.mcp_token),
        anonymity = toml_str(&settings.anonymity),
        tor_mode = toml_str(&settings.tor_mode),
        tor_port = settings.tor_port,
        relay_doc = RELAY_SECTION_DOC,
        relays = render_relays(&settings.relays),
        clearnet_enabled = settings.clearnet_relays_enabled,
        lang = toml_str(&settings.lang),
        theme = toml_str(&settings.theme),
        font_app = settings.font_app,
        font_nav = settings.font_nav,
        font_editor = settings.font_editor,
    )
}

/// The explanatory header of the `[transport.nostr]` section. ONE source for
/// both writers: the generated template ([`render`]) and the retrofit that
/// [`apply`] performs when an older config gains the section for the first
/// time — so a hand-editing operator always finds the same instructions.
const RELAY_SECTION_DOC: &str = "# The Nostr relays this node may use. NO RELAY SHIPS WITH THE APP: the node\n# connects to nothing until you add one below (or in Settings > Nostr relays) AND\n# confirm it. A default relay list would be a default surveillance point.\n#\n# The ORDER of the entries is the dial priority - the first one is tried\n# first. Onion relays connect automatically. A clearnet or local (LAN/\n# loopback) relay must be confirmed WITH its exposure acknowledgement; that\n# also sets clearnet_enabled below and is remembered, so you confirm once\n# rather than after every restart. Switch it off again in Settings > Nostr\n# relays or with the relay_clearnet_session MCP tool.\n#\n# EDITING THIS FILE BY HAND: confirmed and clearnet_enabled are two\n# separate decisions. Writing confirmed = true on a clearnet or local\n# relay does NOT grant dialing outside Tor - set clearnet_enabled = true\n# above as well, or the relay stays in the pool and is never dialed.\n#\n# Add one block per relay (the host below is a placeholder, not a real\n# relay), or let the app manage the list for you:\n#\n#   [[transport.nostr.relay]]\n#   url = \"wss://your-relay.onion\"   # ws:// only for .onion and local addresses\n#   confirmed = false                # nothing is dialed until this is true\n#                                    # (non-onion also needs clearnet_enabled)\n";

/// The `[[transport.nostr.relay]]` tables for [`render`], in pool order —
/// empty (not even a table header) when the pool is empty, so a generated
/// config never suggests a relay.
fn render_relays(relays: &[RelayConfig]) -> String {
    let mut out = String::new();
    for r in relays {
        out.push_str("\n[[transport.nostr.relay]]\n");
        out.push_str(&format!("url = {}\n", toml_str(&r.url)));
        out.push_str(&format!("confirmed = {}\n", r.confirmed));
    }
    out
}

/// Lenient parse: keep every field that is present and well-typed, default the
/// rest. A totally unparseable file yields all defaults.
pub fn salvage(text: &str) -> Settings {
    let mut s = Settings::default();
    let Ok(value) = text.parse::<toml::Value>() else {
        return s;
    };

    if let Some(headless) = value
        .get("node")
        .and_then(|n| n.get("headless"))
        .and_then(toml::Value::as_bool)
    {
        s.headless = headless;
    }
    if let Some(node) = value.get("node") {
        if let Some(b) = node.get("poke_enabled").and_then(toml::Value::as_bool) {
            s.poke_enabled = b;
        }
        if let Some(v) = node.get("poke_wake_command").and_then(toml::Value::as_str) {
            s.poke_wake_command = v.to_string();
        }
    }
    if let Some(files) = value.get("files") {
        if let Some(v) = files.get("mirror_publish_interval_secs").and_then(toml::Value::as_integer) {
            s.mirror_publish_interval_secs = u64::try_from(v).unwrap_or(1).max(1);
        }
        if let Some(v) = files.get("mirror_daily_bytes").and_then(toml::Value::as_integer) {
            s.mirror_daily_bytes = u64::try_from(v).unwrap_or(0);
        }
    }
    if let Some(storage) = value.get("storage") {
        if let Some(dir) = storage.get("workspace_dir").and_then(toml::Value::as_str) {
            s.workspace_dir = dir.to_string();
        }
        if let Some(b) = storage.get("s3_backup").and_then(toml::Value::as_bool) {
            s.s3_backup = b;
        }
        if let Some(v) = storage.get("download_dir").and_then(toml::Value::as_str) {
            s.download_dir = v.to_string();
        }
        if let Some(v) = storage.get("file_cap_bytes").and_then(toml::Value::as_integer) {
            s.file_cap_bytes = u64::try_from(v).ok().filter(|n| *n != molt_core::LEGACY_FILE_CAP_BYTES);
        }
        if let Some(v) = storage.get("s3_endpoint").and_then(toml::Value::as_str) {
            s.s3_endpoint = v.to_string();
        }
        if let Some(v) = storage.get("s3_access_key").and_then(toml::Value::as_str) {
            s.s3_access_key = v.to_string();
        }
        if let Some(v) = storage.get("s3_secret_key").and_then(toml::Value::as_str) {
            s.s3_secret_key = v.to_string();
        }
        if let Some(v) = storage.get("s3_bucket").and_then(toml::Value::as_str) {
            s.s3_bucket = v.to_string();
        }
        if let Some(v) = storage
            .get("s3_interval_min")
            .and_then(toml::Value::as_integer)
            .and_then(|p| u16::try_from(p).ok())
        {
            s.s3_interval_min = v;
        }
        if let Some(v) = storage
            .get("s3_keep_copies")
            .and_then(toml::Value::as_integer)
            .and_then(|p| u16::try_from(p).ok())
        {
            s.s3_keep_copies = v;
        }
        if let Some(v) = storage
            .get("s3_max_bytes")
            .and_then(toml::Value::as_integer)
            .and_then(|p| u64::try_from(p).ok())
        {
            s.s3_max_bytes = v;
        }
        if let Some(v) = storage.get("media_s3_bucket").and_then(toml::Value::as_str) {
            s.media_s3_bucket = v.to_string();
        }
        if let Some(v) = storage
            .get("media_s3_max_bytes")
            .and_then(toml::Value::as_integer)
            .and_then(|p| u64::try_from(p).ok())
        {
            s.media_s3_max_bytes = v;
        }
        let valid_sound = |v: &str| matches!(v, "none" | "bell" | "chime" | "pop");
        if let Some(v) = storage.get("sound_message").and_then(toml::Value::as_str) {
            if valid_sound(v) {
                s.sound_message = v.to_string();
            }
        }
        if let Some(v) = storage.get("sound_vote").and_then(toml::Value::as_str) {
            if valid_sound(v) {
                s.sound_vote = v.to_string();
            }
        }
        if let Some(v) = storage.get("sound_poke").and_then(toml::Value::as_str) {
            if valid_sound(v) {
                s.sound_poke = v.to_string();
            }
        }
        if let Some(b) = storage.get("read_receipts").and_then(toml::Value::as_bool) {
            s.read_receipts = b;
        }
    }
    if let Some(anonymity) = value.get("transport").and_then(|t| t.get("anonymity")) {
        if let Some(net) = anonymity.get("network").and_then(toml::Value::as_str) {
            if matches!(net, "tor" | "none") {
                s.anonymity = net.to_string();
            }
        }
        if let Some(tor) = anonymity.get("tor") {
            if let Some(mode) = tor.get("mode").and_then(toml::Value::as_str) {
                if matches!(mode, "local" | "embedded" | "whonix") {
                    s.tor_mode = mode.to_string();
                }
            }
            if let Some(port) = tor
                .get("port")
                .and_then(toml::Value::as_integer)
                .and_then(|p| u16::try_from(p).ok())
            {
                s.tor_port = port;
            }
        }
    }
    // The persisted clearnet decision (ADR-0004 amendment). Absent or
    // malformed salvages as FALSE — a damaged file must never grant
    // non-onion dialing the operator cannot be shown to have chosen.
    if let Some(enabled) = value
        .get("transport")
        .and_then(|t| t.get("nostr"))
        .and_then(|n| n.get("clearnet_enabled"))
        .and_then(toml::Value::as_bool)
    {
        s.clearnet_relays_enabled = enabled;
    }
    // The relay pool salvages entry by entry: one malformed table must not
    // cost the operator the rest of the pool. A relay with no `url` is
    // dropped; `confirmed` defaults to false, so anything salvaged from a
    // damaged file is inert until the operator confirms it again.
    if let Some(relays) = value
        .get("transport")
        .and_then(|t| t.get("nostr"))
        .and_then(|n| n.get("relay"))
        .and_then(toml::Value::as_array)
    {
        s.relays = relays
            .iter()
            .filter_map(|entry| {
                let url = entry.get("url").and_then(toml::Value::as_str)?;
                if url.trim().is_empty() {
                    return None;
                }
                Some(RelayConfig {
                    url: url.trim().to_string(),
                    confirmed: entry
                        .get("confirmed")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect();
    }
    if let Some(mcp) = value.get("mcp") {
        if let Some(port) = mcp
            .get("port")
            .and_then(toml::Value::as_integer)
            .and_then(|p| u16::try_from(p).ok())
        {
            s.mcp_port = port;
        }
        if let Some(allow) = mcp.get("allow").and_then(toml::Value::as_str) {
            s.mcp_allow = allow.to_string();
        }
        if let Some(token) = mcp.get("token").and_then(toml::Value::as_str) {
            s.mcp_token = token.to_string();
        }
        if let Some(token) = mcp.get("read_token").and_then(toml::Value::as_str) {
            s.mcp_read_token = token.to_string();
        }
    }
    if let Some(ui) = value.get("ui") {
        if let Some(lang) = ui.get("lang").and_then(toml::Value::as_str) {
            s.lang = lang.to_string();
        }
        if let Some(theme) = ui.get("theme").and_then(toml::Value::as_str) {
            if matches!(theme, "classic" | "dark" | "brutalism") {
                s.theme = theme.to_string();
            }
        }
        if let Some(v) = ui.get("font_app").and_then(toml::Value::as_integer) {
            s.font_app = u16::try_from(v).unwrap_or_else(|_| default_font_app());
        }
        if let Some(v) = ui.get("font_nav").and_then(toml::Value::as_integer) {
            s.font_nav = u16::try_from(v).unwrap_or_else(|_| default_font_nav());
        }
        if let Some(v) = ui.get("font_editor").and_then(toml::Value::as_integer) {
            s.font_editor = u16::try_from(v).unwrap_or_else(|_| default_font_editor());
        }
    }
    s
}

// ---------------------------------------------------------------------------
// File-level helpers (strict parse, lenient read, round-trip write).
// ---------------------------------------------------------------------------

/// Strictly parse `config.toml` text into a [`Config`] (`deny_unknown_fields`).
/// Callers wrap the error with their own file-path context.
pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
    let mut c: Config = toml::from_str(text)?;
    // the old unconditional default means "no cap" - read so, never
    // rewritten for it (a rewrite would break rollback to an older build)
    if c.storage.file_cap_bytes == Some(molt_core::LEGACY_FILE_CAP_BYTES) {
        c.storage.file_cap_bytes = None;
    }
    Ok(c)
}

/// The retired `network = "nym"`, if this text still selects it.
///
/// The mixnet was never implemented: `Dialer::resolve` answers "nym not
/// implemented" and refuses every dial, so a node configured this way starts
/// fine and then fails silently at its first connection. Worse, the GUI had
/// no dropdown entry for it — the settings panel read it back as "none",
/// which made the draft differ from the stored value forever (an
/// unsaved-changes modal on every exit) and would have written the
/// difference out as a silent downgrade to CLEARNET.
///
/// So it is refused at load, by name, rather than normalized: turning an
/// operator's "anonymize me" into "no anonymity" behind their back is the
/// one outcome worse than refusing to start.
#[must_use]
pub fn selects_retired_nym(text: &str) -> bool {
    text.parse::<toml::Value>()
        .ok()
        .as_ref()
        .and_then(|v| v.get("transport"))
        .and_then(|t| t.get("anonymity"))
        .and_then(|a| a.get("network"))
        .and_then(toml::Value::as_str)
        == Some("nym")
}

/// Whether `text` is syntactically valid TOML (ignores schema validity).
/// Used to distinguish "broken TOML" from "valid TOML, wrong schema".
pub fn is_well_formed(text: &str) -> bool {
    text.parse::<toml::Value>().is_ok()
}

/// Read a file and lenient-salvage it into [`Settings`]. The file must exist;
/// any salvageable fields are kept, the rest defaulted.
pub fn read_settings(path: &Path) -> std::io::Result<Settings> {
    let text = std::fs::read_to_string(path)?;
    Ok(salvage(&text))
}

/// `config.toml` -> `config.toml.bak` (keeps the original file name intact).
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}


// ---------------------------------------------------------------------------
// Format-preserving runtime rewrite (toml_edit).
// ---------------------------------------------------------------------------

/// Load-time heal for pre-N-demo config files: every old `render()`/`apply()`
/// wrote a `[transport.smp]` section, which the strict boot parse
/// (`deny_unknown_fields`) now rejects — and the save-time heal in [`apply`]
/// can never run for an app that fails to boot. Returns the healed text iff
/// (a) `text` does NOT strict-parse today, (b) dropping `transport.smp` is
/// format-preserving possible, and (c) the result DOES strict-parse — i.e.
/// exactly the legacy-file class, never a general repair (that stays
/// `--repair-config`'s job).
pub fn heal_legacy(text: &str) -> Option<String> {
    heal_legacy_notes(text).map(|(healed, _)| healed)
}

/// [`heal_legacy`] with one note per heal for the operator's terminal -
/// ONE class: the `[transport.smp]` section (fails the strict parse). The
/// old `file_cap_bytes = 4194304` is not a heal: it parses and reads as
/// "no cap" (`LEGACY_FILE_CAP_BYTES`), the file stays as it is.
pub fn heal_legacy_notes(text: &str) -> Option<(String, Vec<String>)> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    let removed_smp = doc
        .as_table_mut()
        .get_mut("transport")
        .and_then(toml_edit::Item::as_table_like_mut)
        .map(|transport| transport.remove("smp").is_some())
        .unwrap_or(false);
    if !removed_smp {
        return None;
    }
    let healed = doc.to_string();
    parse(&healed).ok()?;
    Some((
        healed,
        vec!["healed the legacy [transport.smp] section (the SMP transport was removed)".to_string()],
    ))
}

/// Rewrite `text` so it carries exactly the values in `settings`, preserving
/// everything the user hand-wrote into the file: comments, key order, spacing.
///
/// This is the write path of the bi-directional config (see
/// `docs_archive/build/concept-config-bidirection.md`): [`render`] produces our
/// canonical file for `--generate-config`, but a runtime save must not
/// clobber a user-maintained file, so it edits the existing document instead.
/// Fails only when `text` is not parseable TOML — the caller must not guess
/// on a broken file (the user may be mid-edit).
pub fn update(text: &str, settings: &Settings) -> Result<String, toml_edit::TomlError> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    apply(settings, &mut doc);
    Ok(doc.to_string())
}

/// Set exactly the keys that map from [`Settings`] on `doc` — the inverse of
/// `Config → Settings`. Keys keep their position and comments; a key whose
/// value is already correct is left untouched (so even its same-line comment
/// survives); missing tables/keys are created.
pub fn apply(settings: &Settings, doc: &mut toml_edit::DocumentMut) {
    let node = table_at(doc.as_table_mut(), &["node"]);
    set_bool(node, "headless", settings.headless);
    set_bool(node, "poke_enabled", settings.poke_enabled);
    set_str(node, "poke_wake_command", &settings.poke_wake_command);

    let storage = table_at(doc.as_table_mut(), &["storage"]);
    set_str(storage, "workspace_dir", &settings.workspace_dir);
    set_bool(storage, "s3_backup", settings.s3_backup);
    set_str(storage, "s3_endpoint", &settings.s3_endpoint);
    set_str(storage, "s3_access_key", &settings.s3_access_key);
    set_str(storage, "s3_secret_key", &settings.s3_secret_key);
    set_str(storage, "s3_bucket", &settings.s3_bucket);
    set_int(
        storage,
        "s3_interval_min",
        i64::from(settings.s3_interval_min),
    );
    set_int(
        storage,
        "s3_keep_copies",
        i64::from(settings.s3_keep_copies),
    );
    set_int(
        storage,
        "s3_max_bytes",
        i64::try_from(settings.s3_max_bytes).unwrap_or(i64::MAX),
    );
    set_str(storage, "media_s3_bucket", &settings.media_s3_bucket);
    set_int(
        storage,
        "media_s3_max_bytes",
        i64::try_from(settings.media_s3_max_bytes).unwrap_or(i64::MAX),
    );
    set_str(storage, "download_dir", &settings.download_dir);
    match settings.file_cap_bytes {
        Some(n) => set_int(storage, "file_cap_bytes", i64::try_from(n).unwrap_or(i64::MAX)),
        // absent IS the value (no cap): the key must not linger
        None => {
            storage.remove("file_cap_bytes");
        }
    }
    set_str(storage, "sound_message", &settings.sound_message);
    set_str(storage, "sound_vote", &settings.sound_vote);
    set_str(storage, "sound_poke", &settings.sound_poke);
    set_bool(storage, "read_receipts", settings.read_receipts);

    // written only off their defaults: a config an older build (which
    // denies unknown tables) still opens stays that way until the operator
    // actually changes the pace
    let files = table_at(doc.as_table_mut(), &["files"]);
    if settings.mirror_publish_interval_secs == default_mirror_publish_interval_secs() {
        files.remove("mirror_publish_interval_secs");
    } else {
        set_int(
            files,
            "mirror_publish_interval_secs",
            i64::try_from(settings.mirror_publish_interval_secs).unwrap_or(i64::MAX),
        );
    }
    if settings.mirror_daily_bytes == default_mirror_daily_bytes() {
        files.remove("mirror_daily_bytes");
    } else {
        set_int(
            files,
            "mirror_daily_bytes",
            i64::try_from(settings.mirror_daily_bytes).unwrap_or(i64::MAX),
        );
    }
    if files.is_empty() {
        doc.as_table_mut().remove("files");
    }

    let mcp = table_at(doc.as_table_mut(), &["mcp"]);
    set_int(mcp, "port", i64::from(settings.mcp_port));
    set_str(mcp, "allow", &settings.mcp_allow);
    set_str(mcp, "token", &settings.mcp_token);
    // written only once one is issued: an absent key is the value (off),
    // and a config without it still opens on a build that predates it
    if settings.mcp_read_token.is_empty() {
        mcp.remove("read_token");
    } else {
        set_str(mcp, "read_token", &settings.mcp_read_token);
    }

    // Heal-once: configs written before the SMP transport was removed carry
    // a [transport.smp] section that the deny_unknown_fields strict parse
    // rejects — dropping it here is the ONE exception to the leave-unknown-
    // keys-alone guarantee, or every save would re-persist a file the
    // hot-reload watcher can never accept.
    if let Some(transport) = doc
        .as_table_mut()
        .get_mut("transport")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        transport.remove("smp");
    }

    let anon = table_at(doc.as_table_mut(), &["transport", "anonymity"]);
    set_str(anon, "network", &settings.anonymity);

    let tor = table_at(doc.as_table_mut(), &["transport", "anonymity", "tor"]);
    set_str(tor, "mode", &settings.tor_mode);
    set_int(tor, "port", i64::from(settings.tor_port));

    apply_relays(settings, doc);

    let ui = table_at(doc.as_table_mut(), &["ui"]);
    set_str(ui, "lang", &settings.lang);
    set_str(ui, "theme", &settings.theme);
    set_int(ui, "font_app", i64::from(settings.font_app));
    set_int(ui, "font_nav", i64::from(settings.font_nav));
    set_int(ui, "font_editor", i64::from(settings.font_editor));
}

/// Write the relay pool as `[[transport.nostr.relay]]` tables in pool order.
///
/// The array is rewritten WHOLESALE — unlike the scalar settings, these
/// entries are app-managed structured data whose order carries meaning (the
/// dial priority), and a merge would have to guess how a hand-reordered file
/// relates to the in-app order. Comments elsewhere in the file keep their
/// format-preserving guarantee; a comment written INSIDE the relay array does
/// not survive a save. An empty pool removes the array entirely, so a config
/// whose relays were all deleted does not keep stale entries.
fn apply_relays(settings: &Settings, doc: &mut toml_edit::DocumentMut) {
    // A config written before the relay pool existed has no [transport.nostr]
    // section at all, so hand-editing operators would never learn the syntax
    // from their own file. Creating it carries the same explanation the
    // generated template has — written ONCE, when the section first appears.
    let fresh_section = doc
        .as_table()
        .get("transport")
        .and_then(toml_edit::Item::as_table_like)
        .map_or(true, |t| t.get("nostr").is_none());
    {
        let nostr = table_at(doc.as_table_mut(), &["transport", "nostr"]);
        set_bool(nostr, "clearnet_enabled", settings.clearnet_relays_enabled);
        if settings.relays.is_empty() {
            nostr.remove("relay");
        } else {
            let mut array = toml_edit::ArrayOfTables::new();
            for r in &settings.relays {
                let mut t = toml_edit::Table::new();
                t.insert("url", toml_edit::value(r.url.clone()));
                t.insert("confirmed", toml_edit::value(r.confirmed));
                array.push(t);
            }
            nostr.insert("relay", toml_edit::Item::ArrayOfTables(array));
        }
    }
    // the header decor is set after the table exists (a second navigation, so
    // the mutable borrow above is already released); an inline-table config
    // (`transport = { nostr = { … } }`) has no header to comment — skipped.
    if let Some(table) = doc
        .get_mut("transport")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|t| t.get_mut("nostr"))
        .and_then(toml_edit::Item::as_table_mut)
    {
        // A section that already exists but carries NO comment of its own
        // gets the explanation too: it was written by a version whose
        // template predates it, and its operator is exactly the one who would
        // otherwise hand-write `confirmed = true` and never learn that
        // `clearnet_enabled` is a separate decision (2026-08-01 report).
        // A prefix the OPERATOR wrote is never touched — the file's
        // comment-preserving guarantee outranks our explanation.
        let uncommented = table
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::as_str)
            .map_or(true, |p| p.trim().is_empty());
        if fresh_section || uncommented {
            table.decor_mut().set_prefix(format!("\n{RELAY_SECTION_DOC}"));
        }
    }
}

/// Walk (and create where missing) the table at `path`. Inline tables the
/// user wrote (`transport = { anonymity = { … } }`) are edited in place;
/// only a non-table value standing where a table belongs is replaced.
fn table_at<'a>(
    mut t: &'a mut dyn toml_edit::TableLike,
    path: &[&str],
) -> &'a mut dyn toml_edit::TableLike {
    for seg in path {
        let missing = !t.get(seg).is_some_and(|i| i.as_table_like().is_some());
        if missing {
            t.insert(seg, toml_edit::table());
        }
        t = t
            .get_mut(seg)
            .and_then(toml_edit::Item::as_table_like_mut)
            .expect("segment was just ensured to be a table");
    }
    t
}

/// Set `key` to `item`. An existing key is updated in place (its Key — and
/// with it the comment block above it — keeps its decor; `insert` would mint
/// a fresh Key and drop that); a missing key is appended.
fn set_item(t: &mut dyn toml_edit::TableLike, key: &str, item: toml_edit::Item) {
    match t.get_mut(key) {
        Some(existing) => *existing = item,
        None => {
            t.insert(key, item);
        }
    }
}

fn set_str(t: &mut dyn toml_edit::TableLike, key: &str, v: &str) {
    if t.get(key).and_then(toml_edit::Item::as_str) != Some(v) {
        set_item(t, key, toml_edit::value(v));
    }
}

fn set_bool(t: &mut dyn toml_edit::TableLike, key: &str, v: bool) {
    if t.get(key).and_then(toml_edit::Item::as_bool) != Some(v) {
        set_item(t, key, toml_edit::value(v));
    }
}

fn set_int(t: &mut dyn toml_edit::TableLike, key: &str, v: i64) {
    if t.get(key).and_then(toml_edit::Item::as_integer) != Some(v) {
        set_item(t, key, toml_edit::value(v));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_and_parses() {
        let text = render(&Settings::default());
        let config: Config = parse(&text).expect("default config must parse");
        assert!(!config.node.headless);
        assert_eq!(config.transport.anonymity.network, AnonymityNetwork::None);
        assert_eq!(config.transport.anonymity.tor.mode, TorMode::Local);
        assert_eq!(config.transport.anonymity.tor.port, default_tor_port());
        assert_eq!(config.mcp.port, default_mcp_port());
        assert_eq!(config.storage.workspace_dir, default_workspace_dir());
    }

    /// Item 11 (wiki UX round): a PRE-FONTS config.toml keeps parsing with
    /// the shipped defaults, a runtime save carries changed sizes into the
    /// `[ui]` table without touching hand-written keys, and render/salvage
    /// round-trip them.
    #[test]
    fn font_sizes_default_and_round_trip() {
        let old = "[ui]\nlang = \"de\"\n";
        let config = parse(old).expect("a pre-fonts config still parses");
        assert_eq!(config.ui.font_app, default_font_app());
        assert_eq!(config.ui.font_nav, default_font_nav());
        assert_eq!(config.ui.font_editor, default_font_editor());
        let s = Settings {
            font_app: 16,
            font_nav: 12,
            font_editor: 15,
            // update() writes the WHOLE live session state — the file's
            // hand-written language is in it, not clobbered by a default
            lang: "de".to_string(),
            ..Settings::default()
        };
        let updated = update(old, &s).expect("runtime save");
        let back = parse(&updated).expect("updated file parses");
        assert_eq!(back.ui.font_app, 16);
        assert_eq!(back.ui.font_nav, 12);
        assert_eq!(back.ui.font_editor, 15);
        assert_eq!(back.ui.lang, "de", "the session's language survives");
        let salvaged = salvage(&render(&s));
        assert_eq!(salvaged.font_app, 16);
        assert_eq!(salvaged.font_nav, 12);
        assert_eq!(salvaged.font_editor, 15);
    }

    #[test]
    fn a_config_written_before_the_media_bucket_still_parses() {
        // the five media keys and the backup quota are all serde-default:
        // a config.toml in the field must keep booting untouched, with the
        // media target simply unconfigured and no quota.
        let text = render(&Settings::default());
        let older: String = text
            .lines()
            .filter(|l| !l.starts_with("media_s3_") && !l.starts_with("s3_max_bytes"))
            .collect::<Vec<_>>()
            .join("\n");
        let c = parse(&older).expect("a pre-media config still parses");
        assert_eq!(c.storage.s3_max_bytes, 0);
        assert_eq!(c.storage.media_s3_bucket, "");
        assert_eq!(c.storage.media_s3_max_bytes, 0);
        // …and the backup target it did configure is untouched
        assert_eq!(c.storage.s3_bucket, default_s3_bucket());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = format!("{}\nbogus_key = 1\n", render(&Settings::default()));
        assert!(
            parse(&text).is_err(),
            "deny_unknown_fields must reject typos"
        );
        assert!(is_well_formed(&text), "but it is still well-formed TOML");
    }

    #[test]
    fn salvage_fills_defaults_for_garbage() {
        let s = salvage("this is not::: toml");
        assert_eq!(s.anonymity, "none");
        assert_eq!(s.tor_mode, "local");
        assert_eq!(s.tor_port, default_tor_port());
        assert_eq!(s.mcp_port, default_mcp_port());
        assert!(!is_well_formed("this is not::: toml"));
    }

    #[test]
    fn salvage_keeps_valid_fields_and_drops_bad_ones() {
        let s = salvage(
            "[storage]\nworkspace_dir = \"/data/molt\"\n\
             [transport.anonymity]\nnetwork = \"bogus\"\n\
             [transport.anonymity.tor]\nmode = \"embedded\"\nport = 9150\n\
             [mcp]\nport = 5050\n",
        );
        assert_eq!(s.workspace_dir, "/data/molt");
        assert_eq!(s.mcp_port, 5050);
        assert_eq!(s.tor_mode, "embedded");
        assert_eq!(s.tor_port, 9150);
        assert_eq!(
            s.anonymity, "none",
            "an invalid anonymity network falls back to the shipped default"
        );
    }

    #[test]
    fn default_anonymity_is_none() {
        // The shipped default is clearnet (network = "none"): a fresh install
        // preserves today's behaviour and the user opts into Tor explicitly.
        assert_eq!(AnonymityNetwork::default(), AnonymityNetwork::None);
        assert_eq!(Settings::default().anonymity, "none");
        let config = parse(&render(&Settings::default())).expect("parse default");
        assert_eq!(config.transport.anonymity.network, AnonymityNetwork::None);
    }

    #[test]
    fn settings_round_trip_through_render_and_salvage() {
        // A non-default Settings survives a render -> salvage round-trip
        // unchanged: this is the property the GUI relies on when it writes a
        // runtime edit and the file is later read back.
        let original = non_default_settings();
        let salvaged = salvage(&render(&original));
        // …with ONE deliberate exception: the read-only MCP key is not in
        // the template, so a config generated by this build still opens on
        // one that predates the key (`deny_unknown_fields`). It reaches the
        // file only through `apply`, once the operator issues one.
        assert_eq!(
            salvaged.mcp_read_token, "",
            "the template must not carry the read-only key"
        );
        assert_eq!(
            Settings {
                mcp_read_token: original.mcp_read_token.clone(),
                ..salvaged
            },
            original
        );
        // And the rendered text is accepted by the strict parser too.
        assert!(parse(&render(&original)).is_ok());
    }

    /// …but `apply` does carry it, and REMOVES it again on revoke, so an
    /// emptied key cannot linger in the file as a live credential.
    #[test]
    fn the_read_only_key_is_written_on_issue_and_removed_on_revoke() {
        let issued = non_default_settings();
        let text = update(&render(&issued), &issued).expect("update");
        assert!(text.contains("read_token = \"0ddba11feed1eaf5\""));
        assert_eq!(salvage(&text).mcp_read_token, issued.mcp_read_token);

        let revoked = Settings {
            mcp_read_token: String::new(),
            ..issued
        };
        let text = update(&text, &revoked).expect("update");
        assert!(
            !text.lines().any(|l| l.trim_start().starts_with("read_token")),
            "a revoked key must not linger"
        );
        assert!(parse(&text).is_ok());
    }

    /// A settings value with every field off its default.
    fn non_default_settings() -> Settings {
        Settings {
            headless: true,
            workspace_dir: "/srv/molt/ws".to_string(),
            s3_backup: true,
            s3_endpoint: "https://s3.example.org".to_string(),
            s3_access_key: "AK".to_string(),
            s3_secret_key: "SK".to_string(),
            s3_bucket: "holiday-pics".to_string(),
            s3_interval_min: 15,
            s3_keep_copies: 9,
            s3_max_bytes: 20 * 1024 * 1024 * 1024,
            // a SECOND bucket at the same endpoint and credentials
            media_s3_bucket: "vacation-clips".to_string(),
            media_s3_max_bytes: 5 * 1024 * 1024 * 1024,
            sound_message: "chime".to_string(),
            sound_vote: "pop".to_string(),
            sound_poke: "bell".to_string(),
            poke_enabled: true,
            poke_wake_command: "claude -p 'check your seat'".to_string(),
            read_receipts: false,
            anonymity: "tor".to_string(),
            tor_mode: "whonix".to_string(),
            tor_port: 9150,
            download_dir: "/srv/molt/downloads".to_string(),
            file_cap_bytes: Some(8 * 1024 * 1024),
            mirror_publish_interval_secs: 20,
            mirror_daily_bytes: 1024,
            mcp_port: 5151,
            mcp_allow: "127.0.0.1, 192.168.1.10".to_string(),
            mcp_token: "deadbeefcafef00d".to_string(),
            mcp_read_token: "0ddba11feed1eaf5".to_string(),
            lang: "de".to_string(),
            theme: "brutalism".to_string(),
            font_app: 16,
            font_nav: 12,
            font_editor: 15,
            // two relays in a deliberate order: the round-trip must preserve
            // BOTH the order (it is the dial priority) and each confirmation
            relays: vec![
                RelayConfig {
                    url: "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion".to_string(),
                    confirmed: true,
                },
                RelayConfig {
                    url: "wss://relay.example.org".to_string(),
                    confirmed: false,
                },
            ],
            // the persisted clearnet decision round-trips like any scalar
            clearnet_relays_enabled: true,
        }
    }

    /// The pool is app-managed data whose ORDER means something, so a save
    /// rewrites the array wholesale — and an emptied pool must leave no stale
    /// entry behind.
    #[test]
    fn the_relay_pool_round_trips_and_an_empty_pool_clears_the_array() {
        let with_relays = non_default_settings();
        let text = update("", &with_relays).expect("write into an empty file");
        assert_eq!(
            salvage(&text).relays,
            with_relays.relays,
            "order and confirmations survive the round-trip"
        );
        parse(&text).expect("the written file parses strictly");

        // reordering is a plain vec swap and must land in the file
        let mut swapped = with_relays.clone();
        swapped.relays.swap(0, 1);
        let text = update(&text, &swapped).expect("rewrite");
        assert_eq!(salvage(&text).relays, swapped.relays, "the new priority order");

        // deleting every relay removes the array — no stale entry survives
        let mut empty = swapped.clone();
        empty.relays.clear();
        let text = update(&text, &empty).expect("rewrite empty");
        assert!(salvage(&text).relays.is_empty());
        assert!(
            !text
                .lines()
                .any(|l| l.trim_start().starts_with("[[transport.nostr.relay]]")),
            "an emptied pool leaves no ACTIVE relay table behind:\n{text}"
        );
        parse(&text).expect("still strict-parses");
    }

    /// A config written before the relay pool existed has no `[transport.nostr]`
    /// section, so its operator would never learn the syntax from their own
    /// file. The first save must retrofit the section WITH the same
    /// explanation the generated template carries — once, without disturbing
    /// anything the operator wrote.
    #[test]
    fn an_older_config_gains_the_documented_relay_section_on_save() {
        let legacy = "# my own notes\n[node]\nheadless = true\n\n[ui]\nlang = \"de\"\n";
        assert!(!legacy.contains("transport.nostr"), "the fixture predates relays");
        let mut settings = salvage(legacy);
        settings.headless = true;

        let once = update(legacy, &settings).expect("first save");
        assert!(once.contains("[transport.nostr]"), "the section appears");
        assert!(
            once.contains("NO RELAY SHIPS WITH THE APP"),
            "…and carries the same explanation as the template:\n{once}"
        );
        assert!(once.contains("# my own notes"), "the operator's own text survives");
        parse(&once).expect("strict-parses");
        assert!(salvage(&once).relays.is_empty(), "no relay was invented");

        // saving again must NOT stack a second copy of the explanation
        let twice = update(&once, &settings).expect("second save");
        assert_eq!(
            twice.matches("NO RELAY SHIPS WITH THE APP").count(),
            1,
            "the explanation is written exactly once:\n{twice}"
        );
    }

    /// The operator this explanation is FOR is the one hand-writing
    /// `confirmed = true` — who by definition already has `[transport.nostr]`
    /// and so never got the "fresh section" text. A section that carries no
    /// comment of its own gains it; a comment the OPERATOR wrote is never
    /// overwritten (the file's comment-preserving guarantee outranks ours).
    #[test]
    fn an_uncommented_relay_section_gains_the_explanation_but_never_clobbers_one() {
        // hand-written, no comment above the section — the reported shape
        let bare = "[transport.nostr]\nclearnet_enabled = false\n\n\
                    [[transport.nostr.relay]]\nurl = \"wss://relay.example.org\"\n\
                    confirmed = true\n";
        let settings = salvage(bare);
        assert_eq!(settings.relays.len(), 1, "the hand-written relay survives salvage");
        let out = update(bare, &settings).expect("save");
        assert!(
            out.contains("EDITING THIS FILE BY HAND"),
            "the two-flag warning reaches the operator who needs it:\n{out}"
        );
        assert!(out.contains("confirmed = true"), "their relay is untouched");
        parse(&out).expect("strict-parses");
        // and it is not stacked on the next save
        assert_eq!(
            update(&out, &settings).expect("second save").matches("EDITING THIS FILE BY HAND").count(),
            1,
            "written exactly once"
        );

        // an operator's OWN comment on the section is left alone
        let mine = "# I wrote this myself, do not touch\n[transport.nostr]\n\
                    clearnet_enabled = false\n";
        let out = update(mine, &salvage(mine)).expect("save");
        assert!(out.contains("# I wrote this myself, do not touch"), "kept:\n{out}");
        assert!(
            !out.contains("EDITING THIS FILE BY HAND"),
            "our text does not displace theirs:\n{out}"
        );
    }

    /// A generated config must not suggest, let alone activate, any relay.
    #[test]
    fn a_generated_config_ships_no_relay() {
        let text = render(&Settings::default());
        // the commented example block is documentation; what must not exist
        // is an ACTIVE relay table
        assert!(
            !text
                .lines()
                .any(|l| l.trim_start().starts_with("[[transport.nostr.relay]]")),
            "the default config activates no relay:\n{text}"
        );
        assert!(salvage(&text).relays.is_empty());
        assert!(parse(&text).is_ok(), "and it strict-parses");
        // the host in the explanatory comment is a placeholder, not a relay
        // anyone could actually reach
        assert!(text.contains("wss://your-relay.onion"));
    }

    /// One damaged relay table must not cost the operator the whole pool, and
    /// nothing salvaged from a damaged file is silently active.
    #[test]
    fn salvage_keeps_the_good_relays_and_drops_the_broken_one() {
        let s = salvage(
            "[[transport.nostr.relay]]\nurl = \"wss://good.onion\"\nconfirmed = true\n\
             [[transport.nostr.relay]]\nconfirmed = true\n\
             [[transport.nostr.relay]]\nurl = \"\"\n\
             [[transport.nostr.relay]]\nurl = \"wss://second.example.org\"\n",
        );
        assert_eq!(s.relays.len(), 2, "the two url-less entries are dropped");
        assert_eq!(s.relays[0].url, "wss://good.onion");
        assert!(s.relays[0].confirmed);
        assert_eq!(s.relays[1].url, "wss://second.example.org");
        assert!(!s.relays[1].confirmed, "confirmation defaults to false");
    }

    #[test]
    fn update_round_trips_every_field() {
        // Settings -> apply(toml_edit) -> strict parse -> Settings equality:
        // the property the runtime save path relies on.
        let original = non_default_settings();
        let updated = update(&render(&Settings::default()), &original).expect("update");
        let config = parse(&updated).expect("updated text stays strictly parseable");
        assert_eq!(Settings::from(&config), original);
    }

    #[test]
    fn update_preserves_comments_order_and_unknown_formatting() {
        // A user-maintained file: odd ordering, hand-written comments, extra
        // blank lines. A runtime save must keep all of it.
        let fixture = "\
# my precious node config -- do not touch!

[ui]
lang = \"de\"   # weil deutsch

[mcp]
# my private port
port = 4041
allow = \"127.0.0.1\"
token = \"abc\"

[node]
headless = false
";
        let mut settings = salvage(fixture);
        settings.mcp_port = 5555;
        let updated = update(fixture, &settings).expect("update");
        // comments survive
        assert!(updated.contains("# my precious node config -- do not touch!"));
        assert!(updated.contains("# my private port"));
        assert!(
            updated.contains("# weil deutsch"),
            "same-line comment of an unchanged key survives"
        );
        // order survives: [ui] still first, [node] still last of the three
        let ui = updated.find("[ui]").expect("[ui]");
        let mcp = updated.find("[mcp]").expect("[mcp]");
        let node = updated.find("[node]").expect("[node]");
        assert!(ui < mcp && mcp < node, "table order changed:\n{updated}");
        // the changed value landed, missing tables were created, and the
        // result parses strictly
        assert!(updated.contains("port = 5555"));
        let config = parse(&updated).expect("updated fixture parses strictly");
        assert_eq!(Settings::from(&config), settings);
    }

    #[test]
    fn update_rejects_broken_toml() {
        assert!(update("this is not::: toml", &Settings::default()).is_err());
    }

    #[test]
    fn heal_legacy_fixes_a_render_era_file_so_boot_can_strict_parse() {
        // Every pre-demolition render()/apply() wrote [transport.smp] into the
        // managed file, and BOOT strict-parses (deny_unknown_fields) — so
        // without a load-time heal every existing installation fails to start
        // and the save-time heal can never run. heal_legacy is that load-time
        // heal: it must fix exactly this file class…
        let legacy = "\
[node]
headless = true

[transport.smp]
server = \"public\"
url = \"\"

[transport.anonymity]
network = \"none\"
";
        assert!(parse(legacy).is_err(), "the legacy file must fail strict parse");
        let healed = heal_legacy(legacy).expect("healable");
        parse(&healed).expect("healed file parses strictly");
        assert!(healed.contains("headless = true"), "values survive");
        // …and ONLY this file class - a current file is left alone
        assert!(
            heal_legacy("[node]\nheadless = true\n").is_none(),
            "a current file is not 'healed'"
        );
        assert!(
            heal_legacy("[node]\nmystery = 1\n").is_none(),
            "an unrelated strict-parse failure is not silently rewritten"
        );
        assert!(heal_legacy("not::: toml").is_none(), "broken TOML is untouched");
    }

    /// The old unconditional `file_cap_bytes = 4194304` reads as "no cap"
    /// at parse and at salvage alike, and the file is never touched for
    /// it: no heal, no rewrite, so an older build still opens the same file.
    #[test]
    fn the_old_default_cap_reads_as_no_cap_without_touching_the_file() {
        let old = "[node]\nheadless = true\n\n[storage]\nfile_cap_bytes = 4194304\n";
        assert_eq!(parse(old).expect("parses").storage.file_cap_bytes, None);
        assert_eq!(salvage(old).file_cap_bytes, None);
        assert!(heal_legacy_notes(old).is_none(), "nothing to heal, nothing rewritten");
        let near = "[storage]\nfile_cap_bytes = 4194305\n";
        assert_eq!(parse(near).expect("parses").storage.file_cap_bytes, Some(4_194_305));
        assert_eq!(salvage(near).file_cap_bytes, Some(4_194_305));
        let off = "[storage]\nfile_cap_bytes = 0\n";
        assert_eq!(parse(off).expect("parses").storage.file_cap_bytes, Some(0), "sharing off stays off");
        let raised = "[storage]\nfile_cap_bytes = 52428800\n";
        assert_eq!(parse(raised).expect("parses").storage.file_cap_bytes, Some(52_428_800));
    }

    #[test]
    fn update_heals_a_stale_transport_smp_section() {
        // Configs written before the SMP transport was removed carry a
        // [transport.smp] section. `TransportConfig` is deny_unknown_fields,
        // so the section would fail every strict parse (hot-reload watcher,
        // post-save verify) forever — a save must drop it once, while every
        // OTHER unknown key keeps its leave-it-alone guarantee.
        let fixture = "\
# keep me
[node]
headless = false

# dies with its section
[transport.smp]
server = \"public\"
url = \"smp://AAAA@host.example\"
urls = [\"smp://BBBB@other.example\"]

[transport.anonymity]
network = \"none\"
";
        let settings = salvage(fixture);
        let updated = update(fixture, &settings).expect("update");
        assert!(
            !updated.contains("[transport.smp]") && !updated.contains("smp://"),
            "stale [transport.smp] must be healed away:\n{updated}"
        );
        // the section's own header comment goes with it; everything else stays
        assert!(!updated.contains("# dies with its section"));
        assert!(updated.contains("# keep me"), "unrelated comments survive");
        parse(&updated).expect("healed file parses strictly");
    }

    #[test]
    fn config_flattens_to_settings() {
        // Config (strict) -> Settings (flat) preserves every field, matching the
        // defaults that render/salvage produce.
        let config = parse(&render(&Settings::default())).expect("parse default");
        assert_eq!(Settings::from(&config), Settings::default());
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    /// **A token is never empty, and never predictable.**
    ///
    /// It used to come from opening `/dev/urandom` by path, with `""` as the
    /// failure value — so a sandbox without the device node, or any non-Unix
    /// target, produced a config that reported success and disabled MCP
    /// authentication (the check is a string compare against the token).
    /// `Err` is the only honest answer when the CSPRNG is unavailable, and
    /// the type is what forces both callers to say so.
    #[test]
    fn a_minted_token_is_full_length_hex_and_never_repeats() {
        let a = random_token().expect("the OS CSPRNG is available here");
        let b = random_token().expect("…twice");
        assert_eq!(a.len(), 48, "24 bytes, hex-encoded");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {a}"
        );
        assert_ne!(a, b, "two mints must not collide");
        assert!(!a.is_empty(), "an empty token would disable authentication");
    }
}

#[cfg(test)]
mod nym_tests {
    use super::*;

    /// **The retired mixnet is refused, never quietly re-labelled.**
    ///
    /// `network = "nym"` was never implemented: the dialer answers "nym not
    /// implemented" and refuses every connection, so such a node starts
    /// happily and then fails at its first dial. The GUI, meanwhile, had no
    /// entry for it and read it back as "none" — a draft that differed from
    /// the stored value forever, and a save that would have written the
    /// difference out as CLEARNET.
    ///
    /// Both halves are pinned here, because the tempting fix (normalize it
    /// to "none") is the one outcome worse than refusing: it turns
    /// "anonymize me" into "do not" without telling anybody.
    #[test]
    fn the_retired_nym_is_named_and_refused_not_normalized() {
        let cfg = "[transport.anonymity]\nnetwork = \"nym\"\n";
        assert!(selects_retired_nym(cfg), "the loader can name it");
        assert!(parse(cfg).is_err(), "…and the strict parse refuses it");
        // the two live values are unaffected
        for good in ["tor", "none"] {
            let cfg = format!("[transport.anonymity]\nnetwork = \"{good}\"\n");
            assert!(!selects_retired_nym(&cfg));
            assert!(parse(&cfg).is_ok(), "{good} still parses");
        }
        // …and salvage does not carry it forward into a Settings the GUI
        // would then render back out as a live setting
        assert_eq!(
            salvage("[transport.anonymity]\nnetwork = \"nym\"\n").anonymity,
            "none",
            "salvage falls back to the default rather than keeping a dead value"
        );
    }
}
