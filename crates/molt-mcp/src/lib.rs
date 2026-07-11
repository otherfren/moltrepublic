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

use molt_core::{
    ChannelRef, Command, MessageId, ProposalId, Screen, SessionSettings, Surface,
    TOPIC_NAME_MAX_CHARS,
};
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

/// A required chat-message id argument (32-char lowercase hex): the
/// optional parse ([`opt_id_arg`]) plus a missing-argument error, so the
/// malformed cases read identically on both paths by construction.
fn id_arg(args: &Value, key: &str) -> Result<MessageId, String> {
    opt_id_arg(args, key)?.ok_or_else(|| {
        format!("missing argument `{key}` (a message id: 32 lowercase hex chars, from read_state)")
    })
}

/// An optional chat-message id argument (32-char lowercase hex). Only a
/// truly absent (or `null`) argument is `None`; a PRESENT argument of the
/// wrong type or shape is an error — it is never silently treated as absent.
fn opt_id_arg(args: &Value, key: &str) -> Result<Option<MessageId>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => s
            .parse()
            .map(Some)
            .map_err(|e| format!("argument `{key}`: {e}")),
        Some(other) => Err(format!(
            "argument `{key}` must be a string message id (32 lowercase hex chars), got {other}"
        )),
    }
}

/// The wire schema of the `channel` argument — one shape shared by
/// `chat_send` and `read_state`, mirroring [`ChannelRef`]'s tagged form.
fn channel_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "kind": { "type": "string", "enum": ["group", "patch", "topic"] },
            "id": { "type": "integer", "description": "the proposal id — required for kind \"patch\"" },
            "name": { "type": "string", "description": format!("the topic name (trimmed, at most {TOPIC_NAME_MAX_CHARS} chars, case-sensitive) — required for kind \"topic\"") }
        },
        "required": ["kind"]
    })
}

/// The optional `channel` argument: `{kind: "group"|"patch"|"topic", id?,
/// name?}`. Absent/`null` means "no channel given" (the caller picks its
/// default); a PRESENT argument of the wrong shape is always an error,
/// never ignored. Topic names go through the core normalization
/// ([`ChannelRef::normalized`]) so MCP and engine agree on the channel key.
fn channel_arg(args: &Value) -> Result<Option<ChannelRef>, String> {
    let obj = match args.get("channel") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(other) => {
            return Err(format!(
                "argument `channel` must be an object {{kind, id?, name?}}, got {other}"
            ))
        }
    };
    let kind = match obj.get("kind") {
        Some(Value::String(s)) => s.as_str(),
        Some(other) => return Err(format!("`channel.kind` must be a string, got {other}")),
        None => {
            return Err(
                "`channel.kind` is required: \"group\", \"patch\" or \"topic\"".to_string(),
            )
        }
    };
    let channel = match kind {
        "group" => ChannelRef::Group,
        "patch" => {
            let id = match obj.get("id") {
                Some(v) => v.as_u64().ok_or_else(|| {
                    format!("`channel.id` must be a non-negative integer proposal id, got {v}")
                })?,
                None => {
                    return Err(
                        "kind \"patch\" requires `channel.id` (the proposal id, an integer)"
                            .to_string(),
                    )
                }
            };
            ChannelRef::Patch { id: ProposalId(id) }
        }
        "topic" => match obj.get("name") {
            Some(Value::String(name)) => ChannelRef::Topic { name: name.clone() }
                .normalized()
                .map_err(|e| format!("`channel.name`: {e}"))?,
            Some(other) => return Err(format!("`channel.name` must be a string, got {other}")),
            None => {
                return Err("kind \"topic\" requires `channel.name` (the topic name)".to_string())
            }
        },
        other => {
            return Err(format!(
                "unknown `channel.kind` `{other}` (expected \"group\", \"patch\" or \"topic\")"
            ))
        }
    };
    Ok(Some(channel))
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
        smp_server: text("smp_server", d.smp_server),
        smp_url: text("smp_url", d.smp_url),
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
///
/// Public so the cross-frontend integration tests can drive the very same
/// argument→[`Command`] mapping an MCP agent gets (co-equality is proven
/// end to end, not assumed) — the servers in this crate remain the only
/// production callers.
pub struct ToolDef {
    /// The MCP-visible tool name.
    pub name: &'static str,
    /// snake_case name of the [`Command`] variant this tool drives; only
    /// the `co_equality` test reads it — it is the audit trail.
    #[cfg_attr(not(test), allow(dead_code))]
    pub command: &'static str,
    /// The operator-facing tool description.
    pub description: &'static str,
    /// Builds the JSON-schema advertised for the tool's arguments.
    pub schema: fn() -> Value,
    /// Maps validated JSON arguments onto the one command set.
    pub build: fn(&Value) -> Result<Command, String>,
}

/// The tool catalogue. Each entry is one verb of the command set.
pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "chat_send",
            command: "chat",
            description: "Post a message to the ungated chat. Every message rides the republic's ONE broadcast stream and every member receives it; `channel` merely files it under a view of that stream — a tag, never a boundary or a room (it hides nothing and grants nothing). Kinds: {\"kind\":\"group\"} the all-hands default; {\"kind\":\"patch\",\"id\":N} discussion attached to proposal N; {\"kind\":\"topic\",\"name\":\"…\"} a free named topic, created by simply posting to it. Pass `quote` (the quoted message's 32-char hex id, from read_state) to reply — and quoting a message that lives in another channel is the cross-post idiom: the original stays where it is, the quote carries it across.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "body": { "type": "string" },
                    "quote": { "type": "string", "description": "optional: the quoted message's id (32-char lowercase hex, from read_state)" },
                    "channel": channel_schema("optional: the channel view this message files under (omit for the all-hands group)")
                },
                "required": ["body"]
            }),
            build: |args| Ok(Command::Chat {
                body: str_arg(args, "body")?,
                quote: opt_id_arg(args, "quote")?,
                channel: channel_arg(args)?.unwrap_or_default(),
            }),
        },
        ToolDef {
            name: "react_chat",
            command: "react_chat",
            description: "Toggle this member's emoji reaction on a chat message, addressed by its stable id (the 32-char lowercase hex `id` every message carries in read_state). Reacting with the emoji you already picked un-reacts; picking another switches — one reaction per member per message.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "message id (32-char lowercase hex, from read_state)" },
                    "emoji": { "type": "string", "description": "a short emoji, e.g. 👍" }
                },
                "required": ["id", "emoji"]
            }),
            build: |args| Ok(Command::ReactChat {
                id: id_arg(args, "id")?,
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
            description: "Download a shared file from its sharer's disk, addressed by the share message's stable id (32-char lowercase hex, from read_state). Fails once the sharer deleted the local file. Mock: validates availability, moves no bytes.",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "share message id (32-char lowercase hex, from read_state)" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::DownloadFile {
                id: id_arg(args, "id")?,
            }),
        },
        ToolDef {
            name: "remove_file",
            command: "remove_file",
            description: "Sharer-only: mark a shared file as deleted from this disk, addressed by the share message's stable id (32-char lowercase hex, from read_state) — the share becomes permanently unavailable for every participant.",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "share message id (32-char lowercase hex, from read_state)" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::RemoveFile {
                id: id_arg(args, "id")?,
            }),
        },
        ToolDef {
            name: "delete_chat",
            command: "delete_chat",
            description: "Delete one of YOUR OWN chat messages, addressed by its stable id (32-char lowercase hex, from read_state): the text is wiped for everyone and replaced by a deletion notice naming the deleter. Author-only — there is no moderation; the engine rejects the id of another member's message.",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "message id (32-char lowercase hex, from read_state)" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::DeleteChat {
                id: id_arg(args, "id")?,
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
            description: "Read the projected state of one surface. Chat messages each carry their stable 32-char hex `id` — the handle for react_chat, delete_chat, download_file, remove_file and chat_send's `quote` — plus the channel they file under, and the snapshot enumerates every channel seen in the log (`channels`). Pass `channel` to get only the messages of that view; channels are tags on the one shared stream, not boundaries, and the enumeration still lists all of them.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": surface_enum() },
                    "channel": channel_schema("optional, chat only: return just this channel's messages (the channel enumeration still lists every channel)")
                },
                "required": ["surface"]
            }),
            build: |args| Ok(Command::ReadState {
                surface: surface_arg(args)?,
                channel: channel_arg(args)?,
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
                    "tor_port": { "type": "integer" },
                    "smp_server": { "type": "string", "enum": ["public", "custom"], "description": "SMP messaging server: bundled public default, or the custom smp_url" },
                    "smp_url": { "type": "string", "description": "custom SMP server URL (smp://<fingerprint>@host), used when smp_server = custom" }
                }
            }),
            build: |args| Ok(Command::SaveSettings {
                settings: settings_arg(args),
            }),
        },
        ToolDef {
            name: "test_smp_server",
            command: "net_test_server",
            description: "Test connectivity to an SMP messaging server (a live TLS handshake, the settings panel's Test button). The result lands in session.smp_test (\"ok\" or \"error: …\"). Pass an explicit url, or omit it to test the configured server.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "smp://<fingerprint>@host to test; omit to test the configured server" }
                }
            }),
            build: |args| Ok(Command::NetTestServer {
                url: args.get("url").and_then(Value::as_str).unwrap_or_default().to_string(),
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
            description: "Begin founding a new republic over the configured transport (SMP). The engine derives the founder's identity, mints one-time invite links per member, and runs the real ritual with a live log; read_session shows the seed, the joinable links, and each seat filling in. Once every member has joined, propose the charter with create_propose.",
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
            name: "recover_invite_start",
            command: "recover_invite_start",
            description: "As a surviving member, mint a single-use recovery link for a fellow member who lost their device (a manually-granted re-admission for an existing seat). The engine opens a dedicated recovery queue on the running mesh transport and listens; read_session shows the resulting molt://recover/… link to share off-band. The returning member proves its seat with a re-derived-identity signature, then the group re-admits it by threshold.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "member": { "type": "string", "description": "the returning member's seat handle (an anchored roster member)" }
                },
                "required": ["member"]
            }),
            build: |args| Ok(Command::RecoverInviteStart {
                member: str_arg(args, "member")?,
            }),
        },
        ToolDef {
            name: "recover_start",
            command: "recover_start",
            description: "As a member who lost their device, rejoin a republic from a coordinator-minted molt://recover/… link using your recovery phrase (a fresh device with only the phrase). The engine re-derives the seat identity, proves it to the coordinator, waits for the group's threshold re-admission, re-enters the encrypted group from the Welcome, verifies the served chain from its genesis, and materializes the recovered workspace locally.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "link": { "type": "string", "description": "the molt://recover/… link (must carry the transport handover)" },
                    "phrase": { "type": "string", "description": "the seat's recovery phrase" }
                },
                "required": ["link", "phrase"]
            }),
            build: |args| Ok(Command::RecoverStart {
                link: str_arg(args, "link")?,
                phrase: str_arg(args, "phrase")?,
            }),
        },
        ToolDef {
            name: "create_propose",
            command: "create_propose",
            description: "Propose the deliberated charter — the final republic name and a free-text agenda — once every member has joined (read_session shows create.can_propose). This seals the roster: every member ratifies the exact name+agenda with their signature, and only then does the workspace open.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "the final republic name to ratify" },
                    "agenda": { "type": "string", "description": "the free-text charter/agenda to ratify" }
                },
                "required": ["name"]
            }),
            build: |args| Ok(Command::CreatePropose {
                name: str_arg(args, "name")?,
                agenda: args.get("agenda").and_then(Value::as_str).unwrap_or_default().to_string(),
            }),
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
            description: "Begin joining a republic from a real molt://invite/… link (must carry the SMP transport handover — a bare preview link is rejected). The engine shows the joiner's own recovery phrase, runs the real SMP join off the actor, and — when the founder proposes the charter — surfaces it for join_confirm_charter; on success it enters the republic on its own.",
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
            name: "join_confirm_charter",
            command: "join_confirm_charter",
            description: "Ratify the founder's proposed charter, surfaced when the join reaches the ratification step (read_session shows join.awaiting_ratify with proposed_name / proposed_agenda). This is the joiner's confirmation — it releases the seal signature and the workspace opens.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::JoinConfirmCharter),
        },
        ToolDef {
            name: "join_decline_charter",
            command: "join_decline_charter",
            description: "Decline the founder's proposed charter at the ratification step (the other choice besides join_confirm_charter). Tells the founder the charter was declined (its seat shows declined so it can re-mint) and ends the join as failed.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::JoinDeclineCharter),
        },
        ToolDef {
            name: "join_cancel",
            command: "join_cancel",
            description: "Abandon the join run and return to the choice screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::JoinCancel),
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
        // forge network peers or ritual members); net_test_result is the
        // node's own SMP probe reporting back (net_test_server is the tool);
        // net_ritual_link_ready / net_ritual_failed are the off-actor
        // provisioning task reporting a seat's real link or a provisioning
        // failure; net_join_sealed / net_join_failed are the off-actor join
        // task reporting back; net_recover_sealed / net_recover_failed are the
        // off-actor rejoin task reporting back (recover_start is the tool);
        // net_recover_announced is the recovery recv loop delivering a
        // rejoiner's mesh announce and net_mesh_extended is the node's own
        // off-actor mesh-extension task reporting its assembled link — both
        // the node's own transport tasks speaking, not agent-forgeable;
        // reload_settings / config_notice are the config
        // watcher's mirror path — an agent that wants a reload edits via
        // save_settings (see documents/mcp-security.md)
        // net_mesh_announced is a member's post-founding mesh handover reaching
        // the founder over the star; net_mesh_ready is the founder's off-actor
        // bootstrap task reporting the assembled mesh — both are the node's own
        // transport tasks speaking, not agent-forgeable.
        const INTERNAL: [&str; 24] = [
            "restore_tick",
            "net_delivered",
            "net_peer_seen",
            "net_send_failed",
            "net_join_requested",
            "net_seal_signed",
            "net_recover_requested",
            "net_recover_link_ready",
            "net_test_result",
            "net_ritual_link_ready",
            "net_ritual_failed",
            "net_join_sealed",
            "net_join_failed",
            "net_recover_sealed",
            "net_recover_failed",
            "net_recover_announced",
            "net_mesh_extended",
            "net_join_accepted",
            "net_join_charter_proposed",
            "net_join_declined",
            "net_mesh_announced",
            "net_mesh_ready",
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

    /// Find one tool in the catalogue by name.
    fn tool_named(name: &str) -> ToolDef {
        tools()
            .into_iter()
            .find(|t| t.name == name)
            .expect("tool exists in the catalogue")
    }

    /// Run a tool's build closure against literal JSON arguments.
    fn build(name: &str, args: &Value) -> Result<Command, String> {
        (tool_named(name).build)(args)
    }

    /// A well-formed message id for the argument-mapping tests.
    const HEX_ID: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn chat_send_accepts_channel_and_quote_id() {
        // The schema exposes the channel object and the quote id.
        let schema = (tool_named("chat_send").schema)();
        let channel = &schema["properties"]["channel"];
        assert_eq!(channel["type"], "object");
        assert_eq!(
            channel["properties"]["kind"]["enum"],
            json!(["group", "patch", "topic"])
        );
        assert!(channel["properties"]["id"].is_object());
        assert!(channel["properties"]["name"].is_object());
        assert_eq!(schema["properties"]["quote"]["type"], "string");
        assert_eq!(schema["required"], json!(["body"]));

        // Omitted channel → the all-hands group (the default view).
        match build("chat_send", &json!({ "body": "hi" })).expect("plain send builds") {
            Command::Chat {
                body,
                quote,
                channel,
            } => {
                assert_eq!(body, "hi");
                assert_eq!(quote, None);
                assert_eq!(channel, ChannelRef::Group);
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Explicit group.
        match build(
            "chat_send",
            &json!({ "body": "hi", "channel": { "kind": "group" } }),
        )
        .expect("group send builds")
        {
            Command::Chat { channel, .. } => assert_eq!(channel, ChannelRef::Group),
            other => panic!("wrong command: {other:?}"),
        }
        // Patch channel by proposal id.
        match build(
            "chat_send",
            &json!({ "body": "hi", "channel": { "kind": "patch", "id": 7 } }),
        )
        .expect("patch send builds")
        {
            Command::Chat { channel, .. } => {
                assert_eq!(channel, ChannelRef::Patch { id: ProposalId(7) });
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Topic channel — normalized exactly like the engine (trimmed).
        match build(
            "chat_send",
            &json!({ "body": "hi", "channel": { "kind": "topic", "name": "  Budget " } }),
        )
        .expect("topic send builds")
        {
            Command::Chat { channel, .. } => {
                assert_eq!(
                    channel,
                    ChannelRef::Topic {
                        name: "Budget".to_string()
                    }
                );
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Quote by hex id.
        match build("chat_send", &json!({ "body": "hi", "quote": HEX_ID }))
            .expect("quoted send builds")
        {
            Command::Chat { quote, .. } => {
                assert_eq!(quote, Some(HEX_ID.parse().expect("valid id")));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn read_state_accepts_a_channel_filter() {
        // The schema exposes the same channel object, optional.
        let schema = (tool_named("read_state").schema)();
        assert_eq!(
            schema["properties"]["channel"]["properties"]["kind"]["enum"],
            json!(["group", "patch", "topic"])
        );
        assert_eq!(schema["required"], json!(["surface"]));

        // No filter → the whole log.
        match build("read_state", &json!({ "surface": "chat" })).expect("unfiltered read builds") {
            Command::ReadState { channel, .. } => assert_eq!(channel, None),
            other => panic!("wrong command: {other:?}"),
        }
        // An explicit null filter is the same as no filter.
        match build("read_state", &json!({ "surface": "chat", "channel": null }))
            .expect("null filter builds")
        {
            Command::ReadState { channel, .. } => assert_eq!(channel, None),
            other => panic!("wrong command: {other:?}"),
        }
        // Filter by patch channel.
        match build(
            "read_state",
            &json!({ "surface": "chat", "channel": { "kind": "patch", "id": 3 } }),
        )
        .expect("patch filter builds")
        {
            Command::ReadState { channel, .. } => {
                assert_eq!(channel, Some(ChannelRef::Patch { id: ProposalId(3) }));
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Filter by topic — normalized like the engine, so both sides agree.
        match build(
            "read_state",
            &json!({ "surface": "chat", "channel": { "kind": "topic", "name": " ops " } }),
        )
        .expect("topic filter builds")
        {
            Command::ReadState { channel, .. } => {
                assert_eq!(
                    channel,
                    Some(ChannelRef::Topic {
                        name: "ops".to_string()
                    })
                );
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Explicit group filter.
        match build(
            "read_state",
            &json!({ "surface": "chat", "channel": { "kind": "group" } }),
        )
        .expect("group filter builds")
        {
            Command::ReadState { channel, .. } => assert_eq!(channel, Some(ChannelRef::Group)),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn react_delete_download_remove_take_hex_ids() {
        // The happy path: each id-addressed tool builds its command.
        let id: MessageId = HEX_ID.parse().expect("valid id");
        match build("react_chat", &json!({ "id": HEX_ID, "emoji": "👍" })).expect("react builds") {
            Command::ReactChat { id: got, emoji } => {
                assert_eq!(got, id);
                assert_eq!(emoji, "👍");
            }
            other => panic!("wrong command: {other:?}"),
        }
        match build("delete_chat", &json!({ "id": HEX_ID })).expect("delete builds") {
            Command::DeleteChat { id: got } => assert_eq!(got, id),
            other => panic!("wrong command: {other:?}"),
        }
        match build("download_file", &json!({ "id": HEX_ID })).expect("download builds") {
            Command::DownloadFile { id: got } => assert_eq!(got, id),
            other => panic!("wrong command: {other:?}"),
        }
        match build("remove_file", &json!({ "id": HEX_ID })).expect("remove builds") {
            Command::RemoveFile { id: got } => assert_eq!(got, id),
            other => panic!("wrong command: {other:?}"),
        }

        // Malformed ids are clean errors on every id tool — never a panic,
        // never silently treated as absent.
        let bad_ids = [
            json!("0011"),                             // wrong length
            json!("zz112233445566778899aabbccddeeff"), // non-hex
            json!("00112233445566778899AABBCCDDEEFF"), // uppercase
            json!(5),                                  // present but not a string
            json!(null),                               // required, so null is missing
        ];
        for bad in &bad_ids {
            for tool in ["react_chat", "delete_chat", "download_file", "remove_file"] {
                let args = json!({ "id": bad, "emoji": "👍" });
                assert!(
                    build(tool, &args).is_err(),
                    "{tool} accepted the bad id {bad}"
                );
            }
        }

        // The required and optional id paths emit the SAME error for the
        // same malformed input — only the missing case may differ (required
        // errors, optional yields None).
        for bad in [json!(5), json!(true), json!([1])] {
            let args = json!({ "id": bad });
            let required = id_arg(&args, "id").expect_err("wrong type errors on required");
            let optional = opt_id_arg(&args, "id").expect_err("wrong type errors on optional");
            assert_eq!(
                required, optional,
                "wrong-type message diverged between required and optional for {bad}"
            );
        }
        for bad in ["0011", "zz112233445566778899aabbccddeeff"] {
            let args = json!({ "id": bad });
            let required = id_arg(&args, "id").expect_err("malformed id errors on required");
            let optional = opt_id_arg(&args, "id").expect_err("malformed id errors on optional");
            assert_eq!(
                required, optional,
                "malformed-id message diverged between required and optional for {bad}"
            );
        }

        // A PRESENT quote of the wrong type or shape is an error — not
        // silently treated as absent; only absent/null means "no quote".
        assert!(build("chat_send", &json!({ "body": "x", "quote": 5 })).is_err());
        assert!(build("chat_send", &json!({ "body": "x", "quote": true })).is_err());
        assert!(build("chat_send", &json!({ "body": "x", "quote": "ABC" })).is_err());
        match build("chat_send", &json!({ "body": "x", "quote": null })).expect("null = no quote") {
            Command::Chat { quote, .. } => assert_eq!(quote, None),
            other => panic!("wrong command: {other:?}"),
        }

        // The channel object is equally strict on both tools that take it.
        let long_name = "x".repeat(65);
        let bad_channels = [
            json!({ "kind": "dm" }),                    // unknown kind
            json!({ "kind": 3 }),                       // kind of the wrong type
            json!({}),                                  // no kind at all
            json!({ "kind": "patch" }),                 // missing id
            json!({ "kind": "patch", "id": "7" }),      // id of the wrong type
            json!({ "kind": "patch", "id": -1 }),       // negative id
            json!({ "kind": "topic" }),                 // missing name
            json!({ "kind": "topic", "name": "   " }),  // empty after trim
            json!({ "kind": "topic", "name": 3 }),      // name of the wrong type
            json!({ "kind": "topic", "name": long_name }), // over the 64-char cap
            json!("group"),                             // not an object
            json!(7),                                   // not an object
        ];
        for bad in &bad_channels {
            assert!(
                build("chat_send", &json!({ "body": "x", "channel": bad })).is_err(),
                "chat_send accepted the bad channel {bad}"
            );
            assert!(
                build("read_state", &json!({ "surface": "chat", "channel": bad })).is_err(),
                "read_state accepted the bad channel {bad}"
            );
        }
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
