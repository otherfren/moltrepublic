// SPDX-License-Identifier: GPL-3.0-or-later

//! `molt-mcp`: the MCP interface to MoltRepublic.
//!
//! This is the **headless operator**. It speaks the Model Context Protocol
//! (JSON-RPC 2.0, newline-delimited) and exposes one MCP *tool* per thing the
//! software can do — and every tool is just a thin wrapper that builds a
//! [`molt_core::Command`] and sends it through the shared [`WalletHandle`]. The
//! GUI builds the *same* commands through the *same* handle, which is what makes
//! the two operators co-equal: there is no reduced MCP command set and no
//! GUI-only action.
//!
//! Each tool is declared ONCE, as a [`ToolDef`] carrying its wire schema and
//! its command builder side by side — the schema cannot drift from the parser,
//! and the `co_equality` test checks the catalogue against the command set.
//!
//! Two transports are provided. [`serve_stdio`] is the standard MCP server
//! transport an agent host launches (used in headless mode). [`serve_tcp`] runs
//! the identical protocol over a socket so an agent can attach while a GUI is
//! also running (UI mode).

use std::net::IpAddr;

use molt_core::{Command, ProposalId, Screen, SessionSettings, Surface};
use molt_engine::WalletHandle;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// MCP protocol version this server advertises.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Serve the MCP protocol over stdin/stdout (the standard headless transport).
/// stdio is inherently local — the agent host spawns the process — so it needs
/// no token.
pub async fn serve_stdio(handle: WalletHandle) -> std::io::Result<()> {
    tracing::info!("MCP server on stdio");
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_conn(handle, reader, writer, None).await
}

/// Serve the MCP protocol over a TCP listener. A connection is refused unless the
/// peer IP is on `allowlist` (or `allow_all` is set), and every client must
/// present `token` in its `initialize` request. Each connection runs on its own
/// task.
pub async fn serve_tcp(
    handle: WalletHandle,
    addr: &str,
    allow_all: bool,
    allowlist: Vec<IpAddr>,
    token: String,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, allow_all, allowed = allowlist.len(), "MCP server on tcp");
    loop {
        let (sock, peer) = listener.accept().await?;
        if !allow_all && !allowlist.contains(&peer.ip()) {
            tracing::warn!(%peer, "MCP connection refused: peer IP not on the allowlist");
            continue; // sock dropped here -> connection closed
        }
        tracing::info!(%peer, "MCP client connected");
        let h = handle.clone();
        let tok = token.clone();
        tokio::spawn(async move {
            let (r, w) = sock.into_split();
            if let Err(e) = serve_conn(h, BufReader::new(r), w, Some(tok)).await {
                tracing::warn!(%peer, error = %e, "MCP connection ended");
            }
        });
    }
}

/// The newline-delimited JSON-RPC loop, generic over any reader/writer. When
/// `auth` is `Some(token)` the client must call `initialize` with a matching
/// `token` before any other method works; `None` (stdio) skips the gate.
async fn serve_conn<R, W>(
    handle: WalletHandle,
    mut reader: R,
    mut writer: W,
    auth: Option<String>,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // stdio (auth == None) is trusted from the start; TCP starts unauthenticated.
    let mut authed = auth.is_none();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(req) => handle_rpc(&handle, req, auth.as_deref(), &mut authed).await,
            Err(e) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
            out.push('\n');
            writer.write_all(out.as_bytes()).await?;
            writer.flush().await?;
        }
    }
}

/// Dispatch one JSON-RPC message. Returns `None` for notifications (no reply).
/// `auth` is the required token (or `None` for stdio); `authed` tracks whether
/// this connection has presented it.
async fn handle_rpc(
    handle: &WalletHandle,
    req: Value,
    auth: Option<&str>,
    authed: &mut bool,
) -> Option<Value> {
    let has_id = req.get("id").is_some();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // `initialize` carries the token; authenticate here.
    if method == "initialize" {
        if let Some(required) = auth {
            let given = params.get("token").and_then(Value::as_str).unwrap_or("");
            if given != required {
                return Some(error_response(
                    id,
                    -32001,
                    "unauthorized: missing or invalid MCP token (send it as params.token)",
                ));
            }
            *authed = true;
        }
        return Some(ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "moltrepublic", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "MoltRepublic node. Every tool maps to one Command on the shared engine; \
                                 the GUI (when present) drives the same commands. Chat is ungated; memory, \
                                 quests, vault and wallet change only via propose + threshold approve."
            }),
        ));
    }

    // Everything else requires an authenticated connection.
    if !*authed {
        return has_id.then(|| {
            error_response(
                id,
                -32001,
                "unauthorized: call initialize with a valid token first",
            )
        });
    }

    match method {
        "notifications/initialized" => None,
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tool_defs() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(handle, name, &args).await {
                Ok(text) => Some(ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                )),
                Err(msg) => Some(ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
                )),
            }
        }
        _ if has_id => Some(error_response(id, -32601, "method not found")),
        _ => None,
    }
}

/// Look the tool up in the catalogue, build its command, execute it on the
/// shared handle, and return the reply as pretty JSON text.
async fn call_tool(handle: &WalletHandle, name: &str, args: &Value) -> Result<String, String> {
    let def = tools()
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("unknown tool: {name}"))?;
    let cmd = (def.build)(args)?;
    let reply = handle.execute(cmd).await.map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&reply).map_err(|e| e.to_string())
}

/// The wire-visible tool list, rendered from the same catalogue the
/// dispatcher uses.
fn tool_defs() -> Vec<Value> {
    tools()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)()
            })
        })
        .collect()
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

fn bool_arg(args: &Value, key: &str) -> Result<bool, String> {
    args.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("missing boolean argument `{key}`"))
}

fn u64_arg(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing integer argument `{key}`"))
}

fn u8_arg(args: &Value, key: &str) -> Result<u8, String> {
    u8::try_from(u64_arg(args, key)?).map_err(|_| format!("argument `{key}` out of range"))
}

fn surface_arg(args: &Value) -> Result<Surface, String> {
    let s = str_arg(args, "surface")?;
    Surface::parse(&s).ok_or_else(|| format!("unknown surface `{s}`"))
}

fn screen_arg(args: &Value) -> Result<Screen, String> {
    let s = str_arg(args, "screen")?;
    Screen::parse(&s).ok_or_else(|| format!("unknown screen `{s}`"))
}

/// Build a [`SessionSettings`] from tool arguments, defaulting any omitted field.
/// `save_settings` replaces the session settings wholesale, so an agent reads the
/// current session first and passes back the fields it wants changed.
fn settings_arg(args: &Value) -> SessionSettings {
    let d = SessionSettings::default();
    let port = |key: &str, fallback: u16| {
        args.get(key)
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(fallback)
    };
    let text = |key: &str, fallback: String| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(fallback)
    };
    SessionSettings {
        headless: args
            .get("headless")
            .and_then(Value::as_bool)
            .unwrap_or(d.headless),
        workspace_dir: text("workspace_dir", d.workspace_dir),
        s3_backup: args
            .get("s3_backup")
            .and_then(Value::as_bool)
            .unwrap_or(d.s3_backup),
        s3_endpoint: text("s3_endpoint", d.s3_endpoint),
        s3_access_key: text("s3_access_key", d.s3_access_key),
        s3_secret_key: text("s3_secret_key", d.s3_secret_key),
        s3_bucket: text("s3_bucket", d.s3_bucket),
        s3_interval_min: port("s3_interval_min", d.s3_interval_min),
        mcp_port: port("mcp_port", d.mcp_port),
        mcp_allow: text("mcp_allow", d.mcp_allow),
        mcp_token: text("mcp_token", d.mcp_token),
        anonymity: text("anonymity", d.anonymity),
        tor_mode: text("tor_mode", d.tor_mode),
        tor_port: port("tor_port", d.tor_port),
    }
}

fn surface_enum() -> Value {
    json!([
        "organization",
        "chat",
        "memory",
        "quests",
        "vault",
        "wallet"
    ])
}

fn gated_enum() -> Value {
    json!(["memory", "quests", "vault", "wallet"])
}

/// One MCP tool: name, wire schema and command builder side by side — a
/// single source of truth, so the schema can never drift from the parser
/// (that drift class is exactly how `save_settings` once hid `mcp_token`).
struct ToolDef {
    name: &'static str,
    /// snake_case name of the [`Command`] variant this tool drives; only
    /// the `co_equality` test reads it — it is the audit trail.
    #[cfg_attr(not(test), allow(dead_code))]
    command: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    build: fn(&Value) -> Result<Command, String>,
}

/// The tool catalogue. Each entry is one verb of the command set.
fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "chat_send",
            command: "chat",
            description: "Post a message to the ungated chat surface; pass `quote` (0-based log index) to reply to an earlier message.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "body": { "type": "string" },
                    "quote": { "type": "integer", "description": "optional: quoted message position in the chat log" }
                },
                "required": ["body"]
            }),
            build: |args| Ok(Command::Chat {
                body: str_arg(args, "body")?,
                quote: args.get("quote").and_then(Value::as_u64),
            }),
        },
        ToolDef {
            name: "react_chat",
            command: "react_chat",
            description: "Toggle this member's emoji reaction on a chat message (0-based log index). Reacting with the emoji you already picked un-reacts; picking another switches — one reaction per member per message.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "description": "message position in the chat log (0-based)" },
                    "emoji": { "type": "string", "description": "a short emoji, e.g. 👍" }
                },
                "required": ["index", "emoji"]
            }),
            build: |args| Ok(Command::ReactChat {
                index: u64_arg(args, "index")?,
                emoji: str_arg(args, "emoji")?,
            }),
        },
        ToolDef {
            name: "share_file",
            command: "share_file",
            description: "Share a file into the ungated chat: only the METADATA (name, size, type, date) is posted — the bytes stay on this node's disk, participants download from there while the file exists (mocked until the transport lands).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "file name, no path" },
                    "size": { "type": "integer", "description": "size in bytes" },
                    "kind": { "type": "string", "description": "display type, e.g. PDF" },
                    "modified": { "type": "integer", "description": "file date, unix seconds (omit = now)" }
                },
                "required": ["name"]
            }),
            build: |args| Ok(Command::ShareFile {
                name: str_arg(args, "name")?,
                size: args.get("size").and_then(Value::as_u64).unwrap_or(0),
                kind: args.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
                modified: args.get("modified").and_then(Value::as_u64).unwrap_or(0),
            }),
        },
        ToolDef {
            name: "download_file",
            command: "download_file",
            description: "Download a shared file from its sharer's disk (0-based chat log index). Fails once the sharer deleted the local file. Mock: validates availability, moves no bytes.",
            schema: || json!({
                "type": "object",
                "properties": { "index": { "type": "integer" } },
                "required": ["index"]
            }),
            build: |args| Ok(Command::DownloadFile {
                index: u64_arg(args, "index")?,
            }),
        },
        ToolDef {
            name: "remove_file",
            command: "remove_file",
            description: "Sharer-only: mark a shared file as deleted from this disk (0-based chat log index) — the share becomes permanently unavailable for every participant.",
            schema: || json!({
                "type": "object",
                "properties": { "index": { "type": "integer" } },
                "required": ["index"]
            }),
            build: |args| Ok(Command::RemoveFile {
                index: u64_arg(args, "index")?,
            }),
        },
        ToolDef {
            name: "delete_chat",
            command: "delete_chat",
            description: "Delete a chat message (0-based log index): the text is wiped for everyone and replaced by a deletion notice naming the deleter.",
            schema: || json!({
                "type": "object",
                "properties": { "index": { "type": "integer" } },
                "required": ["index"]
            }),
            build: |args| Ok(Command::DeleteChat {
                index: u64_arg(args, "index")?,
            }),
        },
        ToolDef {
            name: "propose",
            command: "propose",
            description: "Put an object forward for threshold approval on a gated surface.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": gated_enum() },
                    "payload": { "type": "object", "description": "surface-specific transition, e.g. {\"op\":\"add_note\",\"title\":\"…\"}" }
                },
                "required": ["surface", "payload"]
            }),
            build: |args| Ok(Command::Propose {
                surface: surface_arg(args)?,
                payload: args.get("payload").cloned().unwrap_or_else(|| json!({})),
            }),
        },
        ToolDef {
            name: "approve",
            command: "approve",
            description: "Contribute one member's approval toward a pending proposal.",
            schema: || json!({
                "type": "object",
                "properties": { "proposal_id": { "type": "integer" } },
                "required": ["proposal_id"]
            }),
            build: |args| Ok(Command::Approve {
                proposal: ProposalId(u64_arg(args, "proposal_id")?),
            }),
        },
        ToolDef {
            name: "decline",
            command: "decline",
            description: "Decline a pending proposal.",
            schema: || json!({
                "type": "object",
                "properties": { "proposal_id": { "type": "integer" } },
                "required": ["proposal_id"]
            }),
            build: |args| Ok(Command::Decline {
                proposal: ProposalId(u64_arg(args, "proposal_id")?),
            }),
        },
        ToolDef {
            name: "read_state",
            command: "read_state",
            description: "Read the projected state of one surface.",
            schema: || json!({
                "type": "object",
                "properties": { "surface": { "type": "string", "enum": surface_enum() } },
                "required": ["surface"]
            }),
            build: |args| Ok(Command::ReadState {
                surface: surface_arg(args)?,
            }),
        },
        ToolDef {
            name: "list_proposals",
            command: "list_proposals",
            description: "List every proposal the engine currently knows about.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ListProposals),
        },
        ToolDef {
            name: "status",
            command: "status",
            description: "Read a one-shot status summary of the group and surfaces.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::Status),
        },
        ToolDef {
            name: "read_session",
            command: "read_session",
            description: "Read the shared app/session state the GUI mirrors: current screen, surface + sub-view, language, workspaces, run lifecycles, and settings.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ReadSession),
        },
        ToolDef {
            name: "navigate",
            command: "navigate",
            description: "Move the node (and any attached GUI) to a top-level screen.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "screen": { "type": "string", "enum": ["choice", "create", "open", "join", "restore", "settings", "main"] }
                },
                "required": ["screen"]
            }),
            build: |args| Ok(Command::Navigate {
                screen: screen_arg(args)?,
            }),
        },
        ToolDef {
            name: "select_surface",
            command: "select_surface",
            description: "Select the surface shown in the main view (its sub-view resets to the default). The GUI switches live (shared session state, like navigate).",
            schema: || json!({
                "type": "object",
                "properties": { "surface": { "type": "string", "enum": surface_enum() } },
                "required": ["surface"]
            }),
            build: |args| Ok(Command::SelectSurface {
                surface: surface_arg(args)?,
            }),
        },
        ToolDef {
            name: "select_view",
            command: "select_view",
            description: "Select a surface and one of its sub-views (organization: status/members/statistics · chat: today/archive · memory: brain/proposals/accepted/denied/archive · quests: board/create/proposals/my-quests/archive · vault: secrets/disclose/proposals/exposed · wallet: balance/history/send/receive/status/settings).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": surface_enum() },
                    "view": { "type": "string" }
                },
                "required": ["surface", "view"]
            }),
            build: |args| Ok(Command::SelectView {
                surface: surface_arg(args)?,
                view: str_arg(args, "view")?,
            }),
        },
        ToolDef {
            name: "set_language",
            command: "set_language",
            description: "Set the active GUI language. The GUI re-renders in the new language.",
            schema: || json!({
                "type": "object",
                "properties": { "lang": { "type": "string", "enum": ["en", "de"] } },
                "required": ["lang"]
            }),
            build: |args| Ok(Command::SetLanguage {
                lang: str_arg(args, "lang")?,
            }),
        },
        ToolDef {
            name: "set_theme",
            command: "set_theme",
            description: "Set the active GUI theme. The GUI restyles live.",
            schema: || json!({
                "type": "object",
                "properties": { "theme": { "type": "string", "enum": ["classic", "dark", "brutalism"] } },
                "required": ["theme"]
            }),
            build: |args| Ok(Command::SetTheme {
                theme: str_arg(args, "theme")?,
            }),
        },
        ToolDef {
            name: "save_settings",
            command: "save_settings",
            description: "Store the node settings and persist them to the node's config.toml (format-preserving, atomic; the write outcome lands in the session notice, restart-required keys in session.restart_required). Replaces the settings wholesale; read_session first, then pass back the changed fields.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "headless": { "type": "boolean" },
                    "workspace_dir": { "type": "string" },
                    "s3_backup": { "type": "boolean" },
                    "s3_endpoint": { "type": "string" },
                    "s3_access_key": { "type": "string" },
                    "s3_secret_key": { "type": "string" },
                    "s3_bucket": { "type": "string" },
                    "s3_interval_min": { "type": "integer" },
                    "mcp_port": { "type": "integer" },
                    "mcp_allow": { "type": "string", "description": "client IP allowlist: \"127.0.0.1\" | \"0.0.0.0\" | comma-separated" },
                    "mcp_token": { "type": "string", "description": "rotate the MCP API token (what the GUI's Rotate button does)" },
                    "anonymity": { "type": "string", "enum": ["tor", "nym", "none"] },
                    "tor_mode": { "type": "string", "enum": ["local", "embedded", "whonix"] },
                    "tor_port": { "type": "integer" }
                }
            }),
            build: |args| Ok(Command::SaveSettings {
                settings: settings_arg(args),
            }),
        },
        ToolDef {
            name: "open_workspace",
            command: "open_workspace",
            description: "Open a locally known workspace by its id (see read_session → workspaces[].id): its state loads from disk, it becomes active, and the node moves to the main screen.",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "the workspace id from read_session" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::OpenWorkspace {
                id: str_arg(args, "id")?,
            }),
        },
        ToolDef {
            name: "close_workspace",
            command: "close_workspace",
            description: "Close the active workspace and return to the choice screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::CloseWorkspace),
        },
        ToolDef {
            name: "delete_workspace",
            command: "delete_workspace",
            description: "Forget a locally known workspace by its id: its directory moves to the recoverable .trash and the list entry disappears.",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "the workspace id from read_session" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::DeleteWorkspace {
                id: str_arg(args, "id")?,
            }),
        },
        ToolDef {
            name: "set_workspace_backup",
            command: "set_workspace_backup",
            description: "Switch automatic S3 backup on or off for one workspace by its id (persisted in the workspace's prefs.toml; enabling stamps a first backup).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "the workspace id from read_session" },
                    "enabled": { "type": "boolean" }
                },
                "required": ["id", "enabled"]
            }),
            build: |args| Ok(Command::SetWorkspaceBackup {
                id: str_arg(args, "id")?,
                enabled: bool_arg(args, "enabled")?,
            }),
        },
        ToolDef {
            name: "restore_start",
            command: "restore_start",
            description: "Begin the (mock) restore. The engine ticks progress and a live log by itself; read_session shows both. Implausible targets fail (~45%).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "way": { "type": "string", "enum": ["peer", "s3", "file"] },
                    "target": { "type": "string", "description": "smp:// endpoint, http(s) S3 URL, or a *.molt.enc path" }
                },
                "required": ["way", "target"]
            }),
            build: |args| Ok(Command::RestoreStart {
                way: str_arg(args, "way")?,
                target: str_arg(args, "target")?,
            }),
        },
        ToolDef {
            name: "restore_cancel",
            command: "restore_cancel",
            description: "Abandon the restore and return to the choice screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::RestoreCancel),
        },
        ToolDef {
            name: "restore_finish",
            command: "restore_finish",
            description: "Finish a successful restore: the restored workspace becomes active, straight to the main screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::RestoreFinish),
        },
        ToolDef {
            name: "create_start",
            command: "create_start",
            description: "Begin the (mock) founding of a new republic. The engine ticks progress and a live log by itself; on success the session holds the recovery seed and one-time invite links for the other members (read_session shows all of it).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "the new republic's name (must be unique locally)" },
                    "member": { "type": "string", "description": "the founder's handle" },
                    "threshold": { "type": "integer", "description": "approvals required (m), 1..=members" },
                    "members": { "type": "integer", "description": "member count (n), 2..=13" },
                    "net": { "type": "string", "enum": ["tor", "nym", "none"] }
                },
                "required": ["name", "member", "threshold", "members"]
            }),
            build: |args| Ok(Command::CreateStart {
                name: str_arg(args, "name")?,
                member: str_arg(args, "member")?,
                threshold: u8_arg(args, "threshold")?,
                members: u8_arg(args, "members")?,
                net: args
                    .get("net")
                    .and_then(Value::as_str)
                    .unwrap_or("tor")
                    .to_string(),
            }),
        },
        ToolDef {
            name: "create_cancel",
            command: "create_cancel",
            description: "Abandon the founding run and return to the choice screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::CreateCancel),
        },
        ToolDef {
            name: "create_finish",
            command: "create_finish",
            description: "Finish a successful founding: the new republic joins the local list, becomes active, straight to the main screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::CreateFinish),
        },
        ToolDef {
            name: "join_start",
            command: "join_start",
            description: "Begin the (mock) join for an invite link. The engine ticks progress and a live log by itself; any non-empty invite is accepted for now (a well-formed molt://invite/… link contributes the republic's details).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "invite": { "type": "string", "description": "molt://invite/<republic>/<m>of<n>/<inviter>/<ticket>" },
                    "member": { "type": "string", "description": "the joiner's handle" }
                },
                "required": ["invite", "member"]
            }),
            build: |args| Ok(Command::JoinStart {
                invite: str_arg(args, "invite")?,
                member: str_arg(args, "member")?,
            }),
        },
        ToolDef {
            name: "join_cancel",
            command: "join_cancel",
            description: "Abandon the join run and return to the choice screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::JoinCancel),
        },
        ToolDef {
            name: "join_finish",
            command: "join_finish",
            description: "Finish a successful join: the joined republic appears in the local list, becomes active, straight to the main screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::JoinFinish),
        },
    ]
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::{GroupConfig, SessionView};

    fn wallet() -> WalletHandle {
        molt_engine::spawn(GroupConfig::demo(), SessionView::default())
    }

    fn init_req(token: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "token": token } })
    }

    fn tools_list() -> Value {
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })
    }

    /// The co-equality guard: every command variant is either an MCP tool or
    /// on the documented internal list (see documents/mcp-security.md).
    #[test]
    fn co_equality_every_command_is_a_tool_or_documented_internal() {
        // engine-internal: the run tickers are the engine's own clock;
        // net_delivered / net_peer_seen / net_send_failed and the founding
        // ritual's net_join_requested / net_seal_signed are the node's own
        // transport/ritual tasks speaking (exposing them would let an agent
        // forge network peers or ritual members); reload_settings /
        // config_notice are the config watcher's mirror path — an agent
        // that wants a reload edits via save_settings
        // (see documents/mcp-security.md)
        const INTERNAL: [&str; 9] = [
            "restore_tick",
            "join_tick",
            "net_delivered",
            "net_peer_seen",
            "net_send_failed",
            "net_join_requested",
            "net_seal_signed",
            "reload_settings",
            "config_notice",
        ];
        let mut covered: Vec<&str> = tools().iter().map(|t| t.command).collect();
        covered.extend(INTERNAL);
        covered.sort_unstable();
        covered.dedup();
        let mut expected: Vec<&str> = Command::variant_names().to_vec();
        expected.sort_unstable();
        assert_eq!(
            covered, expected,
            "the MCP tool catalogue drifted from the command set"
        );
    }

    /// Every tool builds a command from its own minimal example arguments —
    /// catches schema/builder drift inside a single ToolDef.
    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate tool names");
    }

    #[tokio::test]
    async fn tcp_requires_matching_token() {
        let h = wallet();
        let mut authed = false;
        // Wrong token: rejected, connection stays unauthenticated.
        let resp = handle_rpc(&h, init_req("nope"), Some("secret"), &mut authed)
            .await
            .expect("initialize always replies");
        assert_eq!(resp["error"]["code"], -32001);
        assert!(!authed);
        // Correct token: authenticated, handshake returns a result.
        let resp = handle_rpc(&h, init_req("secret"), Some("secret"), &mut authed)
            .await
            .expect("initialize always replies");
        assert!(resp.get("result").is_some());
        assert!(authed);
    }

    #[tokio::test]
    async fn methods_refused_until_authenticated() {
        let h = wallet();
        let mut authed = false;
        let resp = handle_rpc(&h, tools_list(), Some("secret"), &mut authed)
            .await
            .expect("tools/list replies");
        assert_eq!(resp["error"]["code"], -32001);
        // After a valid handshake, the same call succeeds.
        let _ = handle_rpc(&h, init_req("secret"), Some("secret"), &mut authed).await;
        let resp = handle_rpc(&h, tools_list(), Some("secret"), &mut authed)
            .await
            .expect("tools/list replies");
        assert!(resp["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn stdio_needs_no_token() {
        let h = wallet();
        // stdio: serve_conn seeds `authed = auth.is_none()`, i.e. already trusted.
        let mut authed = true;
        let resp = handle_rpc(&h, init_req(""), None, &mut authed)
            .await
            .expect("initialize always replies");
        assert!(resp.get("result").is_some());
        let resp = handle_rpc(&h, tools_list(), None, &mut authed)
            .await
            .expect("tools/list replies");
        assert!(resp["result"]["tools"].is_array());
    }
}
