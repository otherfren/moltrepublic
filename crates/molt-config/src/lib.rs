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
}

/// Node-level runtime settings (`[node]`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Start without a GUI (MCP-only).
    #[serde(default)]
    pub headless: bool,
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
    /// Where downloaded chat files land when no explicit destination is
    /// given. `~` expands to $HOME.
    #[serde(default = "default_download_dir")]
    pub download_dir: String,
    /// Alert sound for an incoming chat message ("none"|"bell"|"chime"|"pop").
    #[serde(default = "default_sound")]
    pub sound_message: String,
    /// Alert sound for a new incoming vote, same vocabulary.
    #[serde(default = "default_sound")]
    pub sound_vote: String,
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
            download_dir: default_download_dir(),
            sound_message: default_sound(),
            sound_vote: default_sound(),
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
    /// SMP messaging server selection.
    #[serde(default)]
    pub smp: SmpConfig,
}

/// SMP messaging server selection (`[transport.smp]`): which SimpleX
/// messaging server the founding ritual (and, from T3 on, all group
/// traffic) routes over.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmpConfig {
    /// `"public"` = the bundled default server ([`default_public_smp`]);
    /// `"custom"` = use `url`. Anything else is salvaged back to `"public"`.
    #[serde(default = "default_smp_server")]
    pub server: String,
    /// A custom SMP server URL (`smp://<base64-fingerprint>@host[:port]`),
    /// used when `server = "custom"`. Not validated here (molt-config has no
    /// transport dependency) — the GUI's Test button validates it live.
    #[serde(default)]
    pub url: String,
    /// OPTIONAL redundant SMP server list (Track B Stage 2 redundancy). When
    /// non-empty it OVERRIDES `server`/`url`: the node spreads its inbound
    /// queues across these servers (N = min(count, 2)), so one server's outage
    /// leaves each leg alive on another. Empty (the default) = the single-server
    /// behaviour above, unchanged. Additive; the GUI settings screen edits this
    /// list (each entry Test-buttoned).
    #[serde(default)]
    pub urls: Vec<String>,
}

impl Default for SmpConfig {
    fn default() -> Self {
        SmpConfig {
            server: default_smp_server(),
            url: String::new(),
            urls: Vec::new(),
        }
    }
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
    /// direct SMP dial once this is selected (transport concept §6).
    Tor,
    /// Route over the Nym mixnet.
    Nym,
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
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            port: default_mcp_port(),
            allow: default_mcp_allow(),
            token: String::new(),
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
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            lang: default_lang(),
            theme: default_theme(),
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

/// Default SMP server selection: the bundled public server.
pub fn default_smp_server() -> String {
    "public".to_string()
}

/// The bundled public SMP server, used when `[transport.smp].server =
/// "public"`. An official SimpleX server (Ed448 CA), so users who cannot
/// run their own server still have a working default. Verified reachable by
/// the `smp_*` live tests.
pub fn default_public_smp() -> String {
    "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im".to_string()
}

/// Default MCP client allowlist: loopback only.
pub fn default_mcp_allow() -> String {
    "127.0.0.1".to_string()
}

/// Default GUI theme.
pub fn default_theme() -> String {
    "classic".to_string()
}

/// A fresh random MCP API token: 24 bytes from the OS CSPRNG, hex-encoded.
pub fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 24];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => String::new(),
    }
}

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
    /// Where downloaded chat files land when no explicit destination is given.
    pub download_dir: String,
    /// Alert sound for an incoming chat message.
    pub sound_message: String,
    /// Alert sound for a new incoming vote.
    pub sound_vote: String,
    /// Send (and show) per-message chat read receipts (local privacy switch).
    pub read_receipts: bool,
    /// Anonymity network: `"tor" | "nym" | "none"`.
    pub anonymity: String,
    /// Tor mode: `"local" | "embedded" | "whonix"`.
    pub tor_mode: String,
    /// Local Tor SOCKS port.
    pub tor_port: u16,
    /// SMP server selection: `"public"` (bundled default) or `"custom"`.
    pub smp_server: String,
    /// Custom SMP server URL (`smp://<fp>@host[:port]`), used when
    /// `smp_server = "custom"`.
    pub smp_url: String,
    /// Redundant SMP server list (Track B Stage 2). When non-empty it overrides
    /// `smp_server`/`smp_url`; the node spreads inbound queues across these
    /// servers for redundancy. Empty = single-server (unchanged). See
    /// [`SessionSettings::smp_server_list`].
    pub smp_urls: Vec<String>,
    /// MCP server TCP port.
    pub mcp_port: u16,
    /// MCP client allowlist (`"127.0.0.1" | "0.0.0.0" | comma-separated list`).
    pub mcp_allow: String,
    /// MCP API token clients must present.
    pub mcp_token: String,
    /// GUI language: `"en" | "de"`.
    pub lang: String,
    /// GUI theme: `"classic" | "dark" | "brutalism"`.
    pub theme: String,
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
            download_dir: default_download_dir(),
            sound_message: default_sound(),
            sound_vote: default_sound(),
            read_receipts: true,
            anonymity: "none".to_string(),
            tor_mode: "local".to_string(),
            tor_port: default_tor_port(),
            smp_server: default_smp_server(),
            smp_url: String::new(),
            smp_urls: Vec::new(),
            mcp_port: default_mcp_port(),
            mcp_allow: default_mcp_allow(),
            mcp_token: String::new(),
            lang: default_lang(),
            theme: default_theme(),
        }
    }
}

impl Settings {
    /// The effective SMP server URL list the transport builds over (Track B
    /// Stage 2). When `smp_urls` is non-empty it wins (redundancy across those
    /// servers, de-duplicated, order preserved); otherwise the single-server
    /// resolution: the custom `smp_url` when `smp_server == "custom"` and it is
    /// set, else the bundled public server. Always returns ≥1 entry. The engine
    /// parses each into an `SmpServer` and builds a (multi-server) transport.
    pub fn smp_server_list(&self) -> Vec<String> {
        if !self.smp_urls.is_empty() {
            let mut out: Vec<String> = Vec::with_capacity(self.smp_urls.len());
            for u in &self.smp_urls {
                let u = u.trim();
                if !u.is_empty() && !out.iter().any(|e| e == u) {
                    out.push(u.to_string());
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        let single = if self.smp_server == "custom" && !self.smp_url.trim().is_empty() {
            self.smp_url.trim().to_string()
        } else {
            default_public_smp()
        };
        vec![single]
    }
}

impl AnonymityNetwork {
    /// The lowercase wire/config name (`"tor" | "nym" | "none"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AnonymityNetwork::Tor => "tor",
            AnonymityNetwork::Nym => "nym",
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
            download_dir: c.storage.download_dir.clone(),
            sound_message: c.storage.sound_message.clone(),
            sound_vote: c.storage.sound_vote.clone(),
            read_receipts: c.storage.read_receipts,
            anonymity: c.transport.anonymity.network.as_str().to_string(),
            tor_mode: c.transport.anonymity.tor.mode.as_str().to_string(),
            tor_port: c.transport.anonymity.tor.port,
            smp_server: c.transport.smp.server.clone(),
            smp_url: c.transport.smp.url.clone(),
            smp_urls: c.transport.smp.urls.clone(),
            mcp_port: c.mcp.port,
            mcp_allow: c.mcp.allow.clone(),
            mcp_token: c.mcp.token.clone(),
            lang: c.ui.lang.clone(),
            theme: c.ui.theme.clone(),
        }
    }
}

/// Quote and escape a string as a TOML basic string.
fn toml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render a string slice as a TOML inline array (`["a", "b"]`), each element
/// escaped via [`toml_str`]. An empty slice renders `[]`.
fn toml_arr(items: &[String]) -> String {
    let inner = items.iter().map(|s| toml_str(s)).collect::<Vec<_>>().join(", ");
    format!("[{inner}]")
}

/// Render a fully-commented, valid `config.toml` from `settings`.
pub fn render(settings: &Settings) -> String {
    format!(
        r#"# MoltRepublic.ai node configuration.

[node]
# true = headless (MCP-only, no GUI).
headless = {headless}

[storage]
# Per-group workspace root. "~" = $HOME.
workspace_dir = {workspace_dir}
# Automatic backup of workspaces to an S3-compatible store.
s3_backup = {s3_backup}
s3_endpoint = {s3_endpoint}
s3_access_key = {s3_access_key}
s3_secret_key = {s3_secret_key}
s3_bucket = {s3_bucket}
# Automatic-backup interval in minutes.
s3_interval_min = {s3_interval_min}
# Keep at most this many backup copies per workspace.
s3_keep_copies = {s3_keep_copies}
# Where downloaded chat files land ("~" = $HOME).
download_dir = {download_dir}
# Alert sounds: "none" | "bell" | "chime" | "pop".
sound_message = {sound_message}
sound_vote = {sound_vote}
# Send (and show) per-message chat read receipts. false = this node reveals no
# read confirmations and hides others' from its chat view (symmetric).
read_receipts = {read_receipts}

[mcp]
# MCP server TCP port. Always served (UI + headless).
port = {mcp_port}
# allow = which client IPs may connect over TCP:
#   "127.0.0.1" = loopback only (default)
#   "0.0.0.0"   = any address (careful — this exposes the node)
#   or a comma-separated allowlist, e.g. "127.0.0.1, 192.168.1.10"
# Connections from IPs not on the list are refused.
allow = {mcp_allow}
# API key every MCP client must send in its initialize request. Keep it secret;
# rotate it from the GUI settings. A fresh token is written on --generate-config.
token = {mcp_token}

[transport.anonymity]
# network = "tor" | "nym" | "none" (default "none" = clearnet). "tor" routes
# every SMP dial through Tor, fail-closed (no silent clearnet fallback).
network = {anonymity}

[transport.anonymity.tor]
# mode = how Tor is reached (only when network = "tor"):
#   "local"    = external tor daemon SOCKS proxy on `port`
#   "embedded" = in-process tor proxy
#   "whonix"   = transparent torification by env (Whonix/Tails); `port` ignored
mode = {tor_mode}
# Local tor SOCKS port. Used only when mode = "local".
port = {tor_port}

[transport.smp]
# Which SimpleX messaging server the founding ritual routes over:
#   "public" = a bundled official SimpleX server (no server to host yourself)
#   "custom" = the `url` below (e.g. your own server)
server = {smp_server}
# Custom SMP server URL: smp://<base64-fingerprint>@host[:port].
# Used only when server = "custom". Test it from the GUI settings.
url = {smp_url}
# Optional redundant server list. When non-empty it OVERRIDES server/url: the
# node spreads its inbound queues across these servers (2 = full redundancy —
# one server can go down and each connection stays alive on the other). Edit
# this from the GUI settings (add/remove/Test each). Empty = single server.
urls = {smp_urls}

[ui]
# GUI language: "en" | "de".
lang = {lang}
# GUI theme: "classic" | "dark" | "brutalism".
theme = {theme}
"#,
        headless = settings.headless,
        workspace_dir = toml_str(&settings.workspace_dir),
        s3_backup = settings.s3_backup,
        s3_endpoint = toml_str(&settings.s3_endpoint),
        s3_access_key = toml_str(&settings.s3_access_key),
        s3_secret_key = toml_str(&settings.s3_secret_key),
        s3_bucket = toml_str(&settings.s3_bucket),
        s3_interval_min = settings.s3_interval_min,
        s3_keep_copies = settings.s3_keep_copies,
        download_dir = toml_str(&settings.download_dir),
        sound_message = toml_str(&settings.sound_message),
        sound_vote = toml_str(&settings.sound_vote),
        read_receipts = settings.read_receipts,
        mcp_port = settings.mcp_port,
        mcp_allow = toml_str(&settings.mcp_allow),
        mcp_token = toml_str(&settings.mcp_token),
        anonymity = toml_str(&settings.anonymity),
        tor_mode = toml_str(&settings.tor_mode),
        tor_port = settings.tor_port,
        smp_server = toml_str(&settings.smp_server),
        smp_url = toml_str(&settings.smp_url),
        smp_urls = toml_arr(&settings.smp_urls),
        lang = toml_str(&settings.lang),
        theme = toml_str(&settings.theme),
    )
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
        if let Some(b) = storage.get("read_receipts").and_then(toml::Value::as_bool) {
            s.read_receipts = b;
        }
    }
    if let Some(anonymity) = value.get("transport").and_then(|t| t.get("anonymity")) {
        if let Some(net) = anonymity.get("network").and_then(toml::Value::as_str) {
            if matches!(net, "tor" | "nym" | "none") {
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
    if let Some(smp) = value.get("transport").and_then(|t| t.get("smp")) {
        if let Some(v) = smp.get("server").and_then(toml::Value::as_str) {
            if matches!(v, "public" | "custom") {
                s.smp_server = v.to_string();
            }
        }
        if let Some(v) = smp.get("url").and_then(toml::Value::as_str) {
            s.smp_url = v.to_string();
        }
        if let Some(arr) = smp.get("urls").and_then(toml::Value::as_array) {
            s.smp_urls = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|u| !u.is_empty())
                .collect();
        }
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
    }
    s
}

// ---------------------------------------------------------------------------
// File-level helpers (strict parse, lenient read, round-trip write).
// ---------------------------------------------------------------------------

/// Strictly parse `config.toml` text into a [`Config`] (`deny_unknown_fields`).
/// Callers wrap the error with their own file-path context.
pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
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

/// Render `settings` and write them to `path`, creating parent directories.
///
/// When `make_backup` is true and `path` already exists, the current file is
/// first copied to `<path>.bak`. This is the call the GUI settings panel uses to
/// persist a runtime change without losing the previous on-disk version.
pub fn write(path: &Path, settings: &Settings, make_backup: bool) -> std::io::Result<()> {
    if make_backup && path.exists() {
        std::fs::copy(path, backup_path(path))?;
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, render(settings))
}

// ---------------------------------------------------------------------------
// Format-preserving runtime rewrite (toml_edit).
// ---------------------------------------------------------------------------

/// Rewrite `text` so it carries exactly the values in `settings`, preserving
/// everything the user hand-wrote into the file: comments, key order, spacing.
///
/// This is the write path of the bi-directional config (see
/// `documents/concept-config-bidirection.md`): [`render`] produces our
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
    set_str(storage, "download_dir", &settings.download_dir);
    set_str(storage, "sound_message", &settings.sound_message);
    set_str(storage, "sound_vote", &settings.sound_vote);
    set_bool(storage, "read_receipts", settings.read_receipts);

    let mcp = table_at(doc.as_table_mut(), &["mcp"]);
    set_int(mcp, "port", i64::from(settings.mcp_port));
    set_str(mcp, "allow", &settings.mcp_allow);
    set_str(mcp, "token", &settings.mcp_token);

    let anon = table_at(doc.as_table_mut(), &["transport", "anonymity"]);
    set_str(anon, "network", &settings.anonymity);

    let tor = table_at(doc.as_table_mut(), &["transport", "anonymity", "tor"]);
    set_str(tor, "mode", &settings.tor_mode);
    set_int(tor, "port", i64::from(settings.tor_port));

    let smp = table_at(doc.as_table_mut(), &["transport", "smp"]);
    set_str(smp, "server", &settings.smp_server);
    set_str(smp, "url", &settings.smp_url);
    set_arr(smp, "urls", &settings.smp_urls);

    let ui = table_at(doc.as_table_mut(), &["ui"]);
    set_str(ui, "lang", &settings.lang);
    set_str(ui, "theme", &settings.theme);
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

/// Set `key` to a string array, replacing any existing value. Rebuilds the
/// array unconditionally (comparing element-wise against a toml_edit array is
/// not worth it for this small, rarely-edited list).
fn set_arr(t: &mut dyn toml_edit::TableLike, key: &str, items: &[String]) {
    let mut arr = toml_edit::Array::new();
    for s in items {
        arr.push(s.as_str());
    }
    set_item(t, key, toml_edit::value(arr));
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
    fn config_round_trips_onion_and_tor_mode() {
        // A comma-host onion smp_url and every tor_mode survive the full
        // render -> salvage -> update round-trip (molt-config keeps `url`
        // opaque, so the onion slot rides through unchanged).
        for mode in ["local", "embedded", "whonix"] {
            let original = Settings {
                anonymity: "tor".to_string(),
                tor_mode: mode.to_string(),
                smp_server: "custom".to_string(),
                smp_url: "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@\
                     smp8.simplex.im,beccx4yfxxbvyhqypaavemqurytl6hozr47wfc7uuecacjqdvwpw2xid.onion"
                    .to_string(),
                ..Settings::default()
            };
            let salvaged = salvage(&render(&original));
            assert_eq!(original, salvaged, "mode {mode} round-trips through salvage");
            let updated = update(&render(&Settings::default()), &original).expect("update");
            let config = parse(&updated).expect("updated text stays strictly parseable");
            assert_eq!(Settings::from(&config), original, "mode {mode} round-trips through update");
        }
    }

    #[test]
    fn smp_server_list_resolves_redundancy_then_single() {
        // default (public, no list) → the one bundled public server
        let def = Settings::default();
        assert_eq!(def.smp_server_list(), vec![default_public_smp()]);

        // custom single server, no list → that one server
        let custom = Settings {
            smp_server: "custom".to_string(),
            smp_url: "smp://AAAA@one".to_string(),
            ..Settings::default()
        };
        assert_eq!(custom.smp_server_list(), vec!["smp://AAAA@one".to_string()]);

        // a non-empty urls list WINS over server/url, de-duped, order kept
        let redundant = Settings {
            smp_server: "public".to_string(),
            smp_url: String::new(),
            smp_urls: vec![
                "smp://AAAA@one".to_string(),
                "smp://BBBB@two".to_string(),
                "smp://AAAA@one".to_string(), // dup dropped
                "   ".to_string(),            // blank dropped
            ],
            ..Settings::default()
        };
        assert_eq!(
            redundant.smp_server_list(),
            vec!["smp://AAAA@one".to_string(), "smp://BBBB@two".to_string()],
            "the list wins, de-duplicated, order preserved"
        );

        // a list of only blanks falls back to the single-server path
        let blanks = Settings {
            smp_server: "custom".to_string(),
            smp_url: "smp://CCCC@three".to_string(),
            smp_urls: vec!["".to_string(), "  ".to_string()],
            ..Settings::default()
        };
        assert_eq!(blanks.smp_server_list(), vec!["smp://CCCC@three".to_string()]);
    }

    #[test]
    fn settings_round_trip_through_render_and_salvage() {
        // A non-default Settings survives a render -> salvage round-trip
        // unchanged: this is the property the GUI relies on when it writes a
        // runtime edit and the file is later read back.
        let original = non_default_settings();
        let salvaged = salvage(&render(&original));
        assert_eq!(original, salvaged);
        // And the rendered text is accepted by the strict parser too.
        assert!(parse(&render(&original)).is_ok());
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
            sound_message: "chime".to_string(),
            sound_vote: "pop".to_string(),
            read_receipts: false,
            anonymity: "nym".to_string(),
            tor_mode: "whonix".to_string(),
            tor_port: 9150,
            smp_server: "custom".to_string(),
            smp_url: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io".to_string(),
            smp_urls: vec![
                "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io".to_string(),
                "smp://0YuTwO05YJWS8rkjn9eLJDjQhFKvIYd8d4xG8X1blIU=@smp8.simplex.im".to_string(),
            ],
            download_dir: "/srv/molt/downloads".to_string(),
            mcp_port: 5151,
            mcp_allow: "127.0.0.1, 192.168.1.10".to_string(),
            mcp_token: "deadbeefcafef00d".to_string(),
            lang: "de".to_string(),
            theme: "brutalism".to_string(),
        }
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
    fn config_flattens_to_settings() {
        // Config (strict) -> Settings (flat) preserves every field, matching the
        // defaults that render/salvage produce.
        let config = parse(&render(&Settings::default())).expect("parse default");
        assert_eq!(Settings::from(&config), Settings::default());
    }
}
