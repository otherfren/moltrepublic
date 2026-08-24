// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! `moltd` — the MoltRepublic node binary.
//!
//! The node **requires a `config.toml`** to start. It is found like this:
//!
//! * `--config <PATH>` — use exactly this file (abort if it does not exist).
//! * otherwise auto-discover, first match wins:
//!     1. `./config.toml`
//!     2. `~/config.toml`
//!     3. `~/.moltrepublic/config.toml`
//! * if none is found — abort with a hint to `--generate-config`.
//!
//! Config maintenance (each writes and exits):
//! * `--generate-config [PATH]` — write a fresh default config. Without a path
//!   it targets `~/.moltrepublic/config.toml`. Aborts if the file already
//!   exists or the path is not writable.
//! * `--repair-config <PATH>` — fix an existing config: salvage the valid
//!   fields, fill the rest with defaults, back the original up to `<PATH>.bak`.
//!
//! The config carries: whether to start headless (`[node].headless`), where
//! workspaces are stored (`[storage].workspace_dir`), and how/whether an
//! anonymity network is used (`[transport.anonymity]`). It is parsed strictly:
//! `deny_unknown_fields` makes typos and unknown fields hard errors. The schema,
//! rendering and lenient salvage all live in the `molt-config` crate, shared
//! with the GUI so a runtime settings change round-trips through the same
//! renderer used here.
//!
//! Once started there are two modes, both driving the *same* `WalletHandle`. A
//! co-equal MCP server is **always** available over TCP on `[mcp].port`. UI mode
//! (the default) additionally runs the GUI; headless mode (`[node].headless`,
//! or automatic when the GUI cannot start) additionally serves MCP over stdio
//! (or `--mcp-tcp`).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use molt_config::{backup_path, is_well_formed, parse, render, salvage, Config, Settings};
use molt_engine::WalletHandle;
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "moltd",
    version,
    about = "MoltRepublic node — one command set, two co-equal operators (GUI + MCP)."
)]
struct Cli {
    /// Use exactly this config.toml (skips auto-discovery; aborts if missing).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Write a default config.toml and exit. Without a path: ~/.moltrepublic/config.toml.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    generate_config: Option<Option<PathBuf>>,

    /// Repair an existing config.toml (salvage valid fields, back up the original) and exit.
    #[arg(long, value_name = "PATH")]
    repair_config: Option<PathBuf>,

    /// Headless: expose MCP over this TCP address instead of stdio.
    #[arg(long, value_name = "ADDR")]
    mcp_tcp: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing();

    // Maintenance subcommands short-circuit everything else.
    if let Some(maybe_path) = cli.generate_config {
        let path = match maybe_path {
            Some(p) => p,
            None => default_generate_path()?,
        };
        return generate_config(&path);
    }
    if let Some(path) = cli.repair_config {
        return repair_config(&path);
    }

    let config_path = resolve_config_path(cli.config.as_deref())?;
    let config = load_config(&config_path)?;
    tracing::info!(path = %config_path.display(), "loaded config");

    let workspace_dir = provision_workspace_dir(&config.storage.workspace_dir)?;
    // recoverable deletes expire after 30 days
    molt_storage::purge_trash(&workspace_dir, molt_storage::TRASH_MAX_AGE_SECS);
    // the Open screen's list: every manifest under the root, plus what the
    // device-sealed key opens for the details panel of an at-rest-unencrypted
    // dir — the stored recovery phrase and the genesis' roster/charter
    let workspaces: Vec<molt_core::WorkspaceInfo> = molt_storage::scan_workspaces(&workspace_dir)
        .iter()
        .map(|e| {
            let mut w = e.info();
            // the manifest has no network label; every entry runs over the
            // one global anonymity setting, so stamp its effective label
            w.net =
                molt_core::effective_net_label(config.transport.anonymity.network.as_str())
                    .to_string();
            // the at-rest-sealed marker ("phrase") means "no device-sealed
            // key material on disk". ONLY an unsealed entry may surface those
            // details (the recovery phrase, the genesis roster/charter) — a
            // sealed marker must never hand them to the panel. A sealed
            // marker found next to still-present key material is a
            // half-unsealed dir: an inconsistency to flag and repair
            // (decrypt), not a clean encrypted=true that quietly leaks.
            if w.encrypted {
                if e.dir.join(&e.manifest.crypto.key_file).exists() {
                    tracing::warn!(
                        id = %w.id,
                        dir = %e.dir.display(),
                        "workspace marked sealed-at-rest but its device-sealed key \
                         material is still present — half-unsealed; decrypt to repair"
                    );
                }
            } else {
                if let Some(phrase) = molt_storage::read_sealed_seed(&workspace_dir, &e.dir, &w.id)
                {
                    w.seed = phrase;
                }
                if let Some(genesis) = molt_storage::peek_genesis(&workspace_dir, &e.dir, &w.id) {
                    if let molt_core::WorkspaceEvent::Founded { roster, agenda, .. } = genesis.body {
                        // a closed workspace has no presence knowledge —
                        // every member is honestly never-seen until traffic
                        // stamps it
                        w.members = molt_core::roster_members(&roster, 0, |_| {
                            molt_core::MemberInfo::NEVER
                        });
                        w.agenda = agenda;
                    }
                }
            }
            w
        })
        .collect();
    tracing::info!(
        dir = %workspace_dir.display(),
        found = workspaces.len(),
        "workspace directory ready"
    );
    let anonymity = &config.transport.anonymity;
    tracing::info!(
        network = ?anonymity.network,
        tor_mode = ?anonymity.tor.mode,
        tor_port = anonymity.tor.port,
        "transport anonymity configured (loopback transport active; the Nostr transport lands with N4)"
    );

    // a hand-written pool may contain entries ingest would refuse; they are
    // dropped rather than shown as relays, but never silently — the operator
    // must be able to find out why their line vanished
    {
        let raw: Vec<molt_core::relay::RelayEntry> = config
            .transport
            .nostr
            .relay
            .iter()
            .map(|r| molt_core::relay::RelayEntry {
                url: r.url.clone(),
                confirmed: r.confirmed,
            })
            .collect();
        let kept = molt_core::relay::sanitize_pool(&raw);
        for r in &raw {
            if !kept.iter().any(|k| k.url == r.url) {
                match molt_core::relay::normalize_relay_url(&r.url) {
                    Err(e) => tracing::warn!(url = %r.url, reason = %e, "relay dropped from config"),
                    Ok(_) => tracing::warn!(url = %r.url, "duplicate relay dropped from config"),
                }
            }
        }
        // The state an operator cannot read off their own file: entries that
        // say `confirmed = true` while the node-level switch is off are never
        // dialed. Say it at LOAD time, not only when a founding or join later
        // fails on it (2026-08-01 report).
        if molt_core::relay::pool_gap(&kept, config.transport.nostr.clearnet_enabled)
            == Some(molt_core::relay::PoolGap::NonOnionOff)
        {
            tracing::warn!(
                "every confirmed relay is clearnet or local and [transport.nostr] \
                 clearnet_enabled is false — this node will dial NO relay; founding \
                 and joining will be refused"
            );
        }
        // The transport POSTURE, once, at startup: the route this config
        // actually resolves to, and one line per relay saying whether it will
        // be dialed. The relay runtime reports the same `via=` — but only
        // once something constructs it, which is after a workspace or a
        // ritual opens. "My config.toml says X and nothing connects" is
        // unanswerable at the moment the operator asks it without this.
        let anonymity = &config.transport.anonymity;
        let mode = anonymity.tor.mode.as_str();
        match molt_net::dial::Dialer::resolve(anonymity.network.as_str(), mode, anonymity.tor.port)
        {
            Ok(d) => tracing::info!(
                network = anonymity.network.as_str(),
                via = %d.route(),
                clearnet_enabled = config.transport.nostr.clearnet_enabled,
                configured = kept.len(),
                dialable =
                    molt_core::relay::dialable(&kept, config.transport.nostr.clearnet_enabled).len(),
                "transport posture"
            ),
            // fail-closed: nothing will connect until the settings are fixed,
            // and the operator should not have to infer that from silence
            Err(e) => tracing::error!(
                network = anonymity.network.as_str(),
                tor_mode = mode,
                error = %e,
                "the anonymity settings do not resolve to a usable dialer — NOTHING will connect"
            ),
        }
        for e in &kept {
            let kind = molt_core::relay::relay_kind(&e.url);
            match molt_core::relay::relay_block(e, config.transport.nostr.clearnet_enabled) {
                None => tracing::info!(relay = %e.url, ?kind, "relay: will dial"),
                Some(b) => {
                    tracing::warn!(relay = %e.url, ?kind, blocked = ?b, "relay: will NOT dial")
                }
            }
        }
        tracing::info!(
            "for the full transport trace: RUST_LOG=molt_net=debug (dial, TLS, websocket, \
             subscription and NIP-42 stages)"
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // The initial shared session mirrors config.toml. It lives in the engine, so
    // the GUI and an MCP agent see and change the same screen / language / settings.
    let session = molt_core::SessionView {
        language: config.ui.lang.clone(),
        theme: config.ui.theme.clone(),
        settings: molt_core::SessionSettings {
            headless: config.node.headless,
            workspace_dir: config.storage.workspace_dir.clone(),
            s3_backup: config.storage.s3_backup,
            s3_endpoint: config.storage.s3_endpoint.clone(),
            s3_access_key: config.storage.s3_access_key.clone(),
            s3_secret_key: config.storage.s3_secret_key.clone(),
            s3_bucket: config.storage.s3_bucket.clone(),
            s3_interval_min: config.storage.s3_interval_min,
            s3_keep_copies: config.storage.s3_keep_copies,
            s3_max_bytes: config.storage.s3_max_bytes,
            media_s3_bucket: config.storage.media_s3_bucket.clone(),
            media_s3_max_bytes: config.storage.media_s3_max_bytes,
            sound_message: config.storage.sound_message.clone(),
            sound_vote: config.storage.sound_vote.clone(),
            sound_poke: config.storage.sound_poke.clone(),
            poke_enabled: config.node.poke_enabled,
            poke_wake_command: config.node.poke_wake_command.clone(),
            read_receipts: config.storage.read_receipts,
            font_app: config.ui.font_app,
            font_nav: config.ui.font_nav,
            font_editor: config.ui.font_editor,
            mcp_port: config.mcp.port,
            mcp_allow: config.mcp.allow.clone(),
            mcp_token: config.mcp.token.clone(),
            anonymity: config.transport.anonymity.network.as_str().to_string(),
            tor_mode: config.transport.anonymity.tor.mode.as_str().to_string(),
            tor_port: config.transport.anonymity.tor.port,
            download_dir: config.storage.download_dir.clone(),
            // 0 became "sharing off" on 2026-08-16 (FP4) — say so once at
            // boot, since an old hand-written 0 used to mean "default"
            file_cap_bytes: {
                if config.storage.file_cap_bytes == 0 {
                    tracing::warn!(file_cap_bytes = 0, "file sharing off");
                }
                config.storage.file_cap_bytes
            },
            // the operator's PERSISTED clearnet decision (ADR-0004
            // amendment): a node that was told "yes, use non-onion relays"
            // starts that way instead of asking again after every restart.
            // Onion relays never depended on it.
            clearnet_relays_enabled: config.transport.nostr.clearnet_enabled,
            // the pool as configured; nothing is dialed until an entry is
            // confirmed, and a non-onion entry additionally needs the
            // decision above.
            // hand-written entries never ran ingest validation: normalize,
            // drop the unusable, collapse duplicates (relay_pool.md)
            relays: molt_core::relay::sanitize_pool(
                &config
                    .transport
                    .nostr
                    .relay
                    .iter()
                    .map(|r| molt_core::relay::RelayEntry {
                        url: r.url.clone(),
                        confirmed: r.confirmed,
                    })
                    .collect::<Vec<_>>(),
            ),
        },
        // the scanned on-disk workspaces replace the demo list
        workspaces,
        // active workspace, restore lifecycle; backup orphans start EMPTY
        // and fill only from a real bucket listing (NetListBackups)
        ..molt_core::SessionView::default()
    };
    // Group is workspace-specific: outside an open workspace the node runs
    // the honest solo boot group (just this operator — no simulated peers;
    // opening a workspace swaps in its real genesis roster). The engine is
    // bound to the config file: settings changes persist to it
    // (format-preserving, atomic) and external edits of it are watched,
    // validated and mirrored into the shared session.
    let (wallet, config_store) = {
        let path = config_path.clone();
        rt.block_on(async move {
            molt_engine::spawn_with_config(molt_core::GroupConfig::solo(), session, path)
        })
        .with_context(|| format!("binding the engine to {}", config_path.display()))?
    };

    // Always-on MCP over TCP, UI + headless. The bind address, peer-IP allowlist
    // and required token all come from [mcp] in the config.
    let (bind_ip, allow_all, allowlist) = parse_mcp_allow(&config.mcp.allow);
    let mcp_addr = format!("{bind_ip}:{}", config.mcp.port);
    if config.mcp.token.is_empty() {
        tracing::warn!(
            "MCP token is empty — the TCP endpoint is unauthenticated; \
             run `moltd --generate-config` or set [mcp].token"
        );
    }
    let mcp_token = config.mcp.token.clone();
    {
        let mcp_wallet = wallet.clone();
        let listen_addr = mcp_addr.clone();
        let allowlist = allowlist.clone();
        let token = mcp_token.clone();
        rt.spawn(async move {
            if let Err(e) =
                molt_mcp::serve_tcp(mcp_wallet, &listen_addr, allow_all, allowlist, token).await
            {
                tracing::warn!(error = %e, "MCP TCP server stopped");
            }
        });
    }
    tracing::info!(mcp = %mcp_addr, allow = %config.mcp.allow, "MCP server listening (co-equal operator, token-gated)");

    let shutdown_wallet = wallet.clone();
    let result = if config.node.headless {
        tracing::info!("mode: headless (config: node.headless = true)");
        run_headless(
            &rt,
            wallet,
            cli.mcp_tcp.as_deref(),
            allow_all,
            allowlist,
            mcp_token,
        )
    } else {
        // UI mode (default): GUI on main thread; fall back to headless stdio
        // MCP if it can't start.
        #[cfg(feature = "ui-testing")]
        if std::env::var_os("MOLT_UI_TESTING").is_some_and(|v| v == "1") {
            // docs_archive/ui/gui_over_mcp.md step 2: the real window on the Slint
            // testing backend — no display, but the full event loop, so the
            // live mirror publishes and `ui_action` drives it over MCP.
            // Must run on this (main) thread, before the window is created.
            i_slint_backend_testing::init_integration_test_with_system_time();
            tracing::info!("mode: UI (testing backend, no display)");
        }
        tracing::info!("mode: UI (GUI on main thread)");
        // The GUI greys the embedded tor-mode row unless this binary was built
        // with the in-process arti dialer (the `embedded-tor` feature, P3).
        let embedded_tor_available = cfg!(feature = "embedded-tor");
        match molt_ui::run_app(
            wallet.clone(),
            rt.handle().clone(),
            config_path.clone(),
            embedded_tor_available,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "GUI unavailable; falling back to headless MCP");
                run_headless(
                    &rt,
                    wallet,
                    cli.mcp_tcp.as_deref(),
                    allow_all,
                    allowlist,
                    mcp_token,
                )
            }
        }
    };

    // Close any open workspace durably (flush, closing snapshot, LOCK
    // release) — quitting must be as safe as the in-app close button.
    let _ = rt.block_on(shutdown_wallet.execute(molt_core::Command::CloseWorkspace));
    // Flush any pending (debounced) config write and release the config lock.
    rt.block_on(config_store.shutdown());
    result
}

// ---------------------------------------------------------------------------
// Config discovery, loading, validation
// ---------------------------------------------------------------------------

/// Resolve which config file to load. An explicit `--config` that points at a
/// missing file is a hard error; otherwise auto-discover in three locations.
fn resolve_config_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        anyhow::bail!(
            "--config points to a file that does not exist:\n  {}\n\n\
             Generate one with:  moltd --generate-config {}",
            p.display(),
            p.display(),
        );
    }

    let candidates = discovery_candidates();
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }

    let looked = candidates
        .iter()
        .map(|c| format!("  - {}", c.display()))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::bail!(
        "no config.toml found. Looked in:\n{looked}\n\n\
         Generate one with:  moltd --generate-config [PATH]\n\
         \x20 (PATH is optional; without it, ~/.moltrepublic/config.toml is used)\n\
         Then start with:    moltd --config <path-to-config.toml>"
    );
}

/// The auto-discovery search path, in priority order.
fn discovery_candidates() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("config.toml")];
    if let Some(home) = home_dir() {
        v.push(home.join("config.toml"));
        v.push(home.join(".moltrepublic").join("config.toml"));
    }
    v
}

/// Read and strictly parse a config file, wrapping errors with its path.
///
/// One legacy exception rides in front of the strict failure: files managed
/// before the SMP transport was removed all carry a `[transport.smp]` section
/// the strict parse now rejects — `heal_legacy` drops exactly that section
/// (and nothing else) so existing installations keep booting. The heal is
/// written back best-effort; a read-only file still boots from the healed
/// in-memory text.
fn load_config(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    // The retired mixnet, refused BY NAME rather than healed: normalizing it
    // to "none" would turn the operator's "anonymize me" into clearnet
    // behind their back, and leaving it would start a node whose every dial
    // fails with "nym not implemented" — silently, at the first connection.
    if molt_config::selects_retired_nym(&text) {
        anyhow::bail!(
            "config {}: transport.anonymity.network = \"nym\" was removed \
             (the mixnet was never implemented and every dial refuses). Set \
             it to \"tor\" or \"none\" - not changed for you, because the \
             two mean opposite things for your privacy.",
            path.display()
        );
    }
    match parse(&text) {
        Ok(config) => Ok(config),
        Err(err) => {
            if let Some(healed) = molt_config::heal_legacy(&text) {
                eprintln!(
                    "config {}: healed the legacy [transport.smp] section \
                     (the SMP transport was removed)",
                    path.display()
                );
                if let Err(write_err) = std::fs::write(path, &healed) {
                    eprintln!(
                        "config {}: could not persist the heal ({write_err}); \
                         booting from the healed copy in memory",
                        path.display()
                    );
                }
                return parse(&healed)
                    .with_context(|| format!("parsing healed config {}", path.display()));
            }
            Err(err).with_context(|| {
                format!(
                    "parsing config {} (try: moltd --repair-config {})",
                    path.display(),
                    path.display()
                )
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Config generation / repair
// ---------------------------------------------------------------------------

/// Default target for `--generate-config` with no path: `~/.moltrepublic/config.toml`.
fn default_generate_path() -> anyhow::Result<PathBuf> {
    let home = home_dir().context(
        "cannot determine the home directory ($HOME); \
         pass an explicit path: moltd --generate-config <path>",
    )?;
    Ok(home.join(".moltrepublic").join("config.toml"))
}

/// Write a fresh default config. Fails if the file exists or the path is not
/// writable (e.g. an unreachable directory). Creates parent directories.
/// Write the config owner-only: it carries the MCP token (and, once set,
/// the S3 secret). The runtime writer keeps whatever mode it finds, so the
/// FIRST write decides — the umask default would leave it world-readable.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

fn generate_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::bail!(
            "refusing to overwrite an existing file: {}\n\
             To fix a broken config instead, use:  moltd --repair-config {}",
            path.display(),
            path.display()
        );
    }
    ensure_parent_dir(path)?;
    // Mint a fresh MCP API token and write it into the config. A failing
    // CSPRNG ABORTS: an empty token disables MCP authentication, and a
    // config written with one would report success while handing out an
    // unauthenticated node.
    let settings = Settings {
        mcp_token: molt_config::random_token()
            .with_context(|| "cannot mint an MCP token, so no config is written")?,
        ..Settings::default()
    };
    write_private(path, &render(&settings))
        .with_context(|| format!("writing {} (path not reachable?)", path.display()))?;
    let shown = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    println!("Wrote default config to {}", shown.display());
    println!();
    println!("MCP API token (shown once — clients send it as `initialize` params.token):");
    println!("    {}", settings.mcp_token);
    println!("It is stored in the config; rotate it anytime from the GUI settings (MCP tab).");
    println!();
    println!("Start with:  moltd --config {}", shown.display());
    Ok(())
}

/// Repair an existing config: salvage valid fields, default the rest, and back
/// up the original to `<path>.bak`. Requires the file to exist.
fn repair_config(path: &Path) -> anyhow::Result<()> {
    if !path.is_file() {
        anyhow::bail!(
            "nothing to repair — no file at {}\n\
             Create one with:  moltd --generate-config {}",
            path.display(),
            path.display()
        );
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parseable = is_well_formed(&text);
    let was_valid = parse(&text).is_ok();
    let settings = salvage(&text);

    let backup = backup_path(path);
    // through write_private, not fs::copy: a copy keeps the source's mode,
    // and the old world-readable file would live on as the backup
    write_private(&backup, &text)
        .with_context(|| format!("writing backup {} (path not reachable?)", backup.display()))?;
    write_private(path, &render(&settings))
        .with_context(|| format!("writing {}", path.display()))?;

    if was_valid {
        println!(
            "Config at {} was already valid; rewrote it in normalized form.",
            path.display()
        );
    } else if parseable {
        println!(
            "Repaired {} — salvaged the valid fields, filled the rest with defaults.",
            path.display()
        );
    } else {
        println!(
            "Could not parse {} as TOML; wrote a fresh default config. \
             Copy any values you need back from the backup.",
            path.display()
        );
    }
    println!("Backup of the original: {}", backup.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths, workspace, runtime
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Parse the `[mcp].allow` string into `(bind_ip, allow_all, allowlist)`.
///
/// `"0.0.0.0"` anywhere means "any client" (`allow_all`). Otherwise every valid
/// IP is an allowlist entry. We bind loopback only when loopback is the sole
/// entry; any other case binds all interfaces and filters connections per peer
/// IP inside the MCP server.
fn parse_mcp_allow(allow: &str) -> (String, bool, Vec<IpAddr>) {
    let entries: Vec<&str> = allow
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let allow_all = entries.contains(&"0.0.0.0");
    let ips: Vec<IpAddr> = entries
        .iter()
        .filter_map(|e| e.parse::<IpAddr>().ok())
        .collect();
    // A LIST WE COULD NOT READ binds loopback, never the world. `allow`
    // used to reach the wide bind by falling through: an entry like
    // "localhost" parses as no IP at all, so `only_loopback` was false and
    // the socket opened on every interface. Connections were still refused
    // (the allowlist was empty), but the port was there to find — a typo is
    // not consent to expose the node.
    let understood = allow_all || !ips.is_empty();
    if !understood && !entries.is_empty() {
        eprintln!(
            "config: [mcp].allow = {allow:?} names no usable address \
             (an entry must be an IP literal, or 0.0.0.0 for any) — \
             binding loopback only"
        );
    }
    let wide = allow_all || (understood && !(ips.len() == 1 && ips[0].is_loopback()));
    let bind_ip = if wide {
        "0.0.0.0".to_string()
    } else {
        "127.0.0.1".to_string()
    };
    (bind_ip, allow_all, ips)
}

/// Expand and create the workspace directory, returning its resolved path.
/// Tilde expansion is `molt_storage::expand_tilde` — the same resolution the
/// engine uses at open time, so the scanned root and the opened root can
/// never diverge.
fn provision_workspace_dir(configured: &str) -> anyhow::Result<PathBuf> {
    let dir = molt_storage::expand_tilde(configured);
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "creating workspace dir {} (path not reachable?)",
            dir.display()
        )
    })?;
    Ok(dir)
}

fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {} (path not reachable?)", parent.display()))?;
        }
    }
    Ok(())
}

/// Run the MCP server in the foreground (headless / fallback path). A `--mcp-tcp`
/// override reuses the same peer-IP allowlist and token as the always-on endpoint.
fn run_headless(
    rt: &Runtime,
    wallet: WalletHandle,
    mcp_tcp: Option<&str>,
    allow_all: bool,
    allowlist: Vec<IpAddr>,
    token: String,
) -> anyhow::Result<()> {
    match mcp_tcp {
        Some(addr) => {
            tracing::info!(%addr, "MCP transport: tcp");
            rt.block_on(molt_mcp::serve_tcp(
                wallet, addr, allow_all, allowlist, token,
            ))?;
        }
        None => {
            tracing::info!("MCP transport: stdio");
            rt.block_on(molt_mcp::serve_stdio(wallet))?;
        }
    }
    Ok(())
}

/// Logs go to stderr — in headless/stdio mode, stdout is the MCP channel.
///
/// zbus is capped at ERROR by default: the XDG-portal request pattern
/// (client creates the Request proxy before the portal creates the object)
/// makes zbus emit a scary-but-harmless "Failed to populate properties
/// cache via GetAll" WARN on every portal interaction — Slint's
/// color-scheme query at startup, the file picker. A user cannot act on
/// it and nothing is broken, so it does not belong in their terminal.
/// `RUST_LOG` overrides the whole filter for debugging.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,zbus=error"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("valid ip literal")
    }

    #[test]
    fn allow_default_binds_loopback_only() {
        let (bind, all, list) = parse_mcp_allow("127.0.0.1");
        assert_eq!(bind, "127.0.0.1");
        assert!(!all);
        assert_eq!(list, vec![ip("127.0.0.1")]);
    }

    #[test]
    fn allow_zero_means_any() {
        let (bind, all, list) = parse_mcp_allow("0.0.0.0");
        assert_eq!(bind, "0.0.0.0");
        assert!(all);
        // 0.0.0.0 is a valid IP, so it lands in the list too — harmless when allow_all.
        assert_eq!(list, vec![ip("0.0.0.0")]);
    }

    #[test]
    fn allow_comma_list_binds_all_and_collects_ips() {
        let (bind, all, list) = parse_mcp_allow("127.0.0.1, 192.168.1.10 , 10.0.0.5");
        assert_eq!(bind, "0.0.0.0"); // more than loopback -> bind all, filter per peer
        assert!(!all);
        assert_eq!(
            list,
            vec![ip("127.0.0.1"), ip("192.168.1.10"), ip("10.0.0.5")]
        );
    }

    #[test]
    fn allow_ignores_blank_and_garbage_entries() {
        let (_, all, list) = parse_mcp_allow(" , not-an-ip, 192.168.0.2 ,");
        assert!(!all);
        assert_eq!(list, vec![ip("192.168.0.2")]);
    }

    /// **L5: a list we cannot read binds loopback, not the world.**
    ///
    /// `allow = "localhost"` is the obvious way to write the intended
    /// setting and parses as no IP at all. That fell through to the wide
    /// bind: connections were refused (the allowlist was empty) but the
    /// port stood open on every interface. A typo is not consent to expose
    /// the node.
    #[test]
    fn an_unreadable_allowlist_binds_loopback() {
        for typo in ["localhost", "loopback", "any", "::1 or so", "0.0.0.0.0"] {
            let (bind, all, list) = parse_mcp_allow(typo);
            assert_eq!(bind, "127.0.0.1", "{typo:?} must not open every interface");
            assert!(!all, "{typo:?} is not an explicit any");
            assert!(list.is_empty(), "{typo:?} names no peer");
        }
        // an empty setting is the shipped default's shape, and stays loopback
        assert_eq!(parse_mcp_allow("").0, "127.0.0.1");
        // …while the deliberate wide binds are untouched
        assert_eq!(parse_mcp_allow("0.0.0.0").0, "0.0.0.0");
        assert_eq!(parse_mcp_allow("192.168.1.10").0, "0.0.0.0");
    }
}
