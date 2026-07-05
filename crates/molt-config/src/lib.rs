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
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            workspace_dir: default_workspace_dir(),
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
    /// Route over Tor (the documented default).
    #[default]
    Tor,
    /// Route over the Nym mixnet.
    Nym,
    /// No anonymity network (clearnet).
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

/// Default per-group workspace root.
pub fn default_workspace_dir() -> String {
    "~/.moltrepublic/workspaces".to_string()
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
    /// Anonymity network: `"tor" | "nym" | "none"`.
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
            anonymity: "tor".to_string(),
            tor_mode: "local".to_string(),
            tor_port: default_tor_port(),
            mcp_port: default_mcp_port(),
            mcp_allow: default_mcp_allow(),
            mcp_token: String::new(),
            lang: default_lang(),
            theme: default_theme(),
        }
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
            anonymity: c.transport.anonymity.network.as_str().to_string(),
            tor_mode: c.transport.anonymity.tor.mode.as_str().to_string(),
            tor_port: c.transport.anonymity.tor.port,
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
# network = "tor" | "nym" | "none". Validated + logged; transport not wired yet.
network = {anonymity}

[transport.anonymity.tor]
# mode = how Tor is reached (only when network = "tor"):
#   "local"    = external tor daemon SOCKS proxy on `port`
#   "embedded" = in-process tor proxy
#   "whonix"   = transparent torification by env (Whonix/Tails); `port` ignored
mode = {tor_mode}
# Local tor SOCKS port. Used only when mode = "local".
port = {tor_port}

[ui]
# GUI language: "en" | "de".
lang = {lang}
# GUI theme: "classic" | "dark" | "brutalism".
theme = {theme}
"#,
        headless = settings.headless,
        workspace_dir = toml_str(&settings.workspace_dir),
        mcp_port = settings.mcp_port,
        mcp_allow = toml_str(&settings.mcp_allow),
        mcp_token = toml_str(&settings.mcp_token),
        anonymity = toml_str(&settings.anonymity),
        tor_mode = toml_str(&settings.tor_mode),
        tor_port = settings.tor_port,
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
    if let Some(dir) = value
        .get("storage")
        .and_then(|st| st.get("workspace_dir"))
        .and_then(toml::Value::as_str)
    {
        s.workspace_dir = dir.to_string();
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
        assert_eq!(config.transport.anonymity.network, AnonymityNetwork::Tor);
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
        assert_eq!(s.anonymity, "tor");
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
            s.anonymity, "tor",
            "an invalid anonymity network falls back to default"
        );
    }

    #[test]
    fn settings_round_trip_through_render_and_salvage() {
        // A non-default Settings survives a render -> salvage round-trip
        // unchanged: this is the property the GUI relies on when it writes a
        // runtime edit and the file is later read back.
        let original = Settings {
            headless: true,
            workspace_dir: "/srv/molt/ws".to_string(),
            anonymity: "nym".to_string(),
            tor_mode: "whonix".to_string(),
            tor_port: 9150,
            mcp_port: 5151,
            mcp_allow: "127.0.0.1, 192.168.1.10".to_string(),
            mcp_token: "deadbeefcafef00d".to_string(),
            lang: "de".to_string(),
            theme: "brutalism".to_string(),
        };
        let salvaged = salvage(&render(&original));
        assert_eq!(original, salvaged);
        // And the rendered text is accepted by the strict parser too.
        assert!(parse(&render(&original)).is_ok());
    }

    #[test]
    fn config_flattens_to_settings() {
        // Config (strict) -> Settings (flat) preserves every field, matching the
        // defaults that render/salvage produce.
        let config = parse(&render(&Settings::default())).expect("parse default");
        assert_eq!(Settings::from(&config), Settings::default());
    }
}
