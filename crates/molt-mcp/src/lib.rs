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
    ChannelRef, Command, MessageId, ProposalId, Reply, Screen, SessionSettings, Surface,
    TOPIC_NAME_MAX_CHARS,
};
use molt_engine::WalletHandle;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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
        // the LIVE token, per connection. `mcp-security.md` promises a
        // rotation "takes effect immediately"; a copy captured at startup
        // kept the leaked value working until the next restart and refused
        // the new one. The running session is the one source of truth, so
        // rotating through either surface applies at once. A failed read
        // falls back to the boot token — never to none.
        let tok = live_token(&handle).await.unwrap_or_else(|| token.clone());
        tokio::spawn(async move {
            let (r, w) = sock.into_split();
            if let Err(e) = serve_conn(h, BufReader::new(r), w, Some(tok)).await {
                tracing::warn!(%peer, error = %e, "MCP connection ended");
            }
        });
    }
}

/// The MCP token as the RUNNING session holds it.
///
/// `None` only when the engine cannot be reached; the caller then keeps the
/// value it booted with, because the alternative (an empty token) would
/// disable authentication exactly when something is already wrong.
async fn live_token(handle: &WalletHandle) -> Option<String> {
    match handle.execute(Command::ReadSession).await {
        Ok(Reply::Session(s)) => Some(s.settings.mcp_token.clone()),
        _ => None,
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
        // BOUNDED, and it has to be bounded BEFORE the request is
        // understood: on a node whose `[mcp].allow` opens the port beyond
        // loopback, anyone past the IP filter could stream gigabytes with no
        // newline in them and `read_line` would grow the buffer to match —
        // an out-of-memory kill from a peer that never authenticated. A
        // frame this large is not a request anybody makes; the connection
        // that sent it is done.
        let n = (&mut reader)
            .take(u64::try_from(MAX_RPC_LINE).unwrap_or(u64::MAX))
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        if n >= MAX_RPC_LINE && !line.ends_with('\n') {
            // …and it is NOT skipped: the rest of that line would parse as
            // a request of its own, so the only sound answer is to stop
            // reading this connection.
            tracing::warn!(bytes = n, "MCP request line past the bound — connection dropped");
            let mut out = serde_json::to_string(&error_response(
                Value::Null,
                -32600,
                "request line too large",
            ))
            .unwrap_or_else(|_| "{}".to_string());
            out.push('\n');
            let _ = writer.write_all(out.as_bytes()).await;
            let _ = writer.flush().await;
            return Ok(());
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

/// Largest single JSON-RPC line this server will read. Generous next to any
/// real request (the biggest is a chat message under the transport's 128 KiB
/// publish budget) and small next to the memory a hostile peer could
/// otherwise make the process allocate before it has authenticated at all.
const MAX_RPC_LINE: usize = 1024 * 1024;

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
                                 the GUI (when present) drives the same commands. Chat is ungated; \
                                 organization, memory, quests, vault and wallet change only via \
                                 propose + threshold approve."
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

/// A required array of chat-message ids (each 32-char lowercase hex). An
/// empty array is allowed (the engine treats it as a no-op); a non-array, or
/// any element of the wrong type/shape, is an error.
fn ids_arg(args: &Value, key: &str) -> Result<Vec<MessageId>, String> {
    let arr = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array argument `{key}` (message ids from read_state)"))?;
    arr.iter()
        .map(|v| match v {
            Value::String(s) => s.parse().map_err(|e| format!("argument `{key}`: {e}")),
            other => Err(format!(
                "argument `{key}` must be an array of string message ids, got element {other}"
            )),
        })
        .collect()
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

/// The optional `view` argument of `read_state`. Absent/`null` means the
/// whole retention window, which is also what the nav view `"today"`
/// returns; the one narrowing key is `"unread"` — the messages after this
/// seat's read cursor. A PRESENT argument of the wrong type is an error,
/// never ignored. The KEY itself is validated engine-side against the
/// surface's view list plus [`molt_core::CHAT_READ_SLICES`], so MCP and GUI
/// reads share one vocabulary.
fn view_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("view") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "argument `view` must be a string view key (e.g. \"today\" or \"unread\"), got {other}"
        )),
    }
}

fn screen_arg(args: &Value) -> Result<Screen, String> {
    let s = str_arg(args, "screen")?;
    Screen::parse(&s).ok_or_else(|| format!("unknown screen `{s}`"))
}

/// Build a [`SessionSettings`] from tool arguments, defaulting any omitted field.
/// `save_settings` replaces the session settings wholesale, so an agent reads the
/// current session first and passes back the fields it wants changed.
///
/// The relay pool is deliberately NOT one of those fields: it has its own
/// validated, gated commands (`relay_add`/`relay_confirm`/…), and the engine
/// ignores whatever pool a `save_settings` payload carries.
/// The FULL settings payload of `save_settings`.
///
/// Every field is required, and that is the fix for a real hazard: it used
/// to fall back to `SessionSettings::default()` per missing key, so a caller
/// sending three fields silently reset the rest — `anonymity` to `"none"`
/// (a Tor node onto clearnet) and `mcp_token` to empty (authentication off).
/// A partial update is `patch_settings`, which merges against the running
/// settings inside the engine, where the current values actually are.
fn settings_arg(args: &Value) -> Result<SessionSettings, String> {
    let d = SessionSettings::default();
    let missing = |key: &str| format!("`{key}` is required — to change one setting use patch_settings");
    let port = |key: &str| -> Result<u16, String> {
        args.get(key)
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| missing(key))
    };
    let text = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| missing(key))
    };
    let flag = |key: &str| -> Result<bool, String> {
        args.get(key).and_then(Value::as_bool).ok_or_else(|| missing(key))
    };
    Ok(SessionSettings {
        headless: flag("headless")?,
        // NOT settable through save_settings — the relay pool and the
        // clearnet decision have exactly one door each (the Relay* tools),
        // so an agent cannot grant itself non-onion dialing here. Carried
        // through unchanged; the engine re-merges the stored value.
        clearnet_relays_enabled: d.clearnet_relays_enabled,
        workspace_dir: text("workspace_dir")?,
        // the one optional field, and only because it postdates the tool's
        // schema: an agent that never set it keeps the default rather than
        // being refused for a key it has never heard of
        download_dir: args
            .get("download_dir")
            .and_then(Value::as_str)
            .map_or(d.download_dir, str::to_string),
        s3_backup: flag("s3_backup")?,
        s3_endpoint: text("s3_endpoint")?,
        s3_access_key: text("s3_access_key")?,
        s3_secret_key: text("s3_secret_key")?,
        s3_bucket: text("s3_bucket")?,
        s3_interval_min: port("s3_interval_min")?,
        s3_keep_copies: port("s3_keep_copies")?,
        sound_message: text("sound_message")?,
        sound_vote: text("sound_vote")?,
        read_receipts: flag("read_receipts")?,
        mcp_port: port("mcp_port")?,
        mcp_allow: text("mcp_allow")?,
        mcp_token: text("mcp_token")?,
        anonymity: text("anonymity")?,
        tor_mode: text("tor_mode")?,
        tor_port: port("tor_port")?,
        // never taken from the payload — the engine keeps the live pool
        relays: Vec::new(),
    })
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
            name: "mark_channel_read",
            command: "mark_channel_read",
            description: "Advance this seat's OWN read cursor for one channel (B2): what read_state's per-channel `unread` counts and the chat `view:\"unread\"` slice are measured against. Private to the seat (persisted locally, never on the wire) - the shared read receipts are a different mechanism. Omit `up_to` to mark the channel read through its newest visible message; pass a message id (32-char hex, from read_state) to stop at that message. The cursor only ever advances.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "channel": channel_schema("the channel to mark read (omit for the all-hands group)"),
                    "up_to": { "type": "string", "description": "optional: read THROUGH this message id (32-char lowercase hex)" }
                }
            }),
            build: |args| Ok(Command::MarkChannelRead {
                channel: channel_arg(args)?.unwrap_or_default(),
                up_to: args
                    .get("up_to")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
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
            name: "mark_read",
            command: "mark_read",
            description: "Confirm this member has read chat messages (read receipts), addressed by their stable ids (the 32-char lowercase hex `id`s from read_state). Records the local member's receipt and broadcasts it so peers can show a green dot; a message you authored, already-read, or unknown id is ignored, and while this node's read receipts are disabled it is a silent no-op. Honest: only call it for messages actually seen.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "message ids to mark read (each 32-char lowercase hex, from read_state)"
                    }
                },
                "required": ["ids"]
            }),
            build: |args| Ok(Command::MarkRead {
                ids: ids_arg(args, "ids")?,
            }),
        },
        ToolDef {
            name: "share_file",
            command: "share_file",
            description: "Share a local file into the ungated chat: the engine derives the metadata and streams the real sha256 off the actor, then posts the share message (async — it appears in read_state once hashing completes). Only metadata enters the chat; the path stays this node's business and the bytes move per-download over a dedicated encrypted queue. A share is a chat message, so `channel` files it under a view of the one stream exactly like chat_send (omit for the all-hands group).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute path of the local file to share" },
                    "channel": channel_schema("optional: the channel view this share files under (omit for the all-hands group)")
                },
                "required": ["path"]
            }),
            build: |args| Ok(Command::ShareFile {
                path: str_arg(args, "path")?,
                channel: channel_arg(args)?.unwrap_or_default(),
            }),
        },
        ToolDef {
            name: "download_file",
            command: "download_file",
            description: "Download a shared file: fetches the BYTES peer-to-peer from the sharer's device over a dedicated encrypted queue (the sharer must be online), verifies size + sha256 against the share, and writes the file to `dest` (an existing directory, or a full target path; omitted = the configured download directory). Async kickoff — poll read_uploads for the download's phase/percent/path/error. Addressed by the share message's stable id (32-char lowercase hex, from read_state). Fails honestly once the sharer deleted the file or stays offline.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "share message id (32-char lowercase hex, from read_state)" },
                    "dest": { "type": "string", "description": "optional destination: an existing directory or a full target path (omit = the configured download directory)" }
                },
                "required": ["id"]
            }),
            build: |args| Ok(Command::DownloadFile {
                id: id_arg(args, "id")?,
                dest: args.get("dest").and_then(Value::as_str).map(str::to_string),
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
            description: "Put an object forward for threshold approval on a gated surface. An Organization set_image payload must embed the actual image as base64 `bytes_b64` and the bytes must DECODE as a picture (png/jpeg/webp/gif/bmp/svg, ≤8192x8192) — sign-what-you-see: members vote on the image, so undecodable bytes are refused here and dropped by every peer. Payload size is capped at what one relay message can carry (about 64 KiB of image for a small roster); an over-size proposal is refused with the exact figure that fits.",
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
            description: "Contribute THIS node's approval toward a pending proposal. On a chain-governed republic it is a real signature gossiped to the mesh (the block seals once m distinct members signed); elsewhere the node records at most its own single approval — it can never approve on behalf of other members.",
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
            description: "Read the projected state of one surface. Chat messages each carry their stable 32-char hex `id` — the handle for react_chat, delete_chat, download_file, remove_file and chat_send's `quote` — plus the channel they file under, and the snapshot enumerates every channel seen in the log (`channels`). Each enumerated patch channel carries the vote's lifecycle in `state` (\"proposed\"/\"applied\"/\"rejected\"; absent for group/topic channels and unknown referents): a decided vote's discussion is READ-ONLY — chat_send/share_file into it are refused — but stays readable here. Pass `channel` to get only the messages of that view; channels are tags on the one shared stream, not boundaries, and the enumeration still lists all of them. Pass `view` (chat only) to narrow the read: \"unread\" keeps only the messages after this seat's read cursor; \"today\" and omitting it both give the whole retention window. The filters compose. On gated surfaces, `applied_ids` runs positionally parallel to `applied` and names the proposal each applied entry came from (null = origin unknown: legacy data) — the back-link from an accepted change to its `{\"kind\":\"patch\",\"id\":N}` discussion channel.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": surface_enum() },
                    "channel": channel_schema("optional, chat only: return just this channel's messages (the channel enumeration still lists every channel)"),
                    "view": { "type": "string", "enum": ["today", "unread"], "description": "optional, chat only: \"today\" = the whole retention window (same as omitting it), \"unread\" = only the messages after this seat's read cursor" }
                },
                "required": ["surface"]
            }),
            build: |args| Ok(Command::ReadState {
                surface: surface_arg(args)?,
                channel: channel_arg(args)?,
                view: view_arg(args)?,
            }),
        },
        ToolDef {
            name: "propose_checkpoint",
            command: "propose_checkpoint",
            description: "Propose a chain CHECKPOINT: a threshold-signed compaction cut at the current head. The engine computes the canonical state hash; every member recomputes it from its own chain and co-signs only on an exact match (m confirm the compaction's correctness). Once the block seals, history below the cut may be dropped locally and newcomers bootstrap from checkpoint + suffix.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ProposeCheckpoint),
        },
        ToolDef {
            name: "read_chain",
            command: "read_chain",
            description: "The persistent chain as display data (Chain-History): every committed block of the open republic, newest first — genesis, applied changes, membership transitions, and checkpoint compaction cuts — each with its height, kind, target surface, display payload, consumed proposal id, and the m signers. On a pruned holder the history below the last checkpoint cut appears as summarized entries rebuilt from the checkpoint blob (height 0 — the per-block positions and signatures were dropped with the history).",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ReadChain),
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
            name: "read_members",
            command: "read_members",
            description: "The Organization → Members table: one row per roster member with its anchored identity key (+ fingerprint; empty on demo workspaces), real presence (`last_seen` = unix seconds of the last authenticated traffic from that member, 0 = never seen by this install; `presence` aged from it: 0 online ≤5 min, 1 stale ≤30 min, 2 offline/unreachable), how many pending proposals still await that member's vote, and how many files it shared into the chat.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ReadMembers),
        },
        ToolDef {
            name: "read_uploads",
            command: "read_uploads",
            description: "The Organization → Uploads table: every file shared into the chat (metadata only — bytes move user-to-user via the share link), with sharer, timestamp, availability, and the retention deadline (`expires_ts`) — uploads are ephemeral like chat and age out of the read (and become undownloadable) after the org's chat retention window. The `id` is the chat message id `download_file` takes.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::ReadUploads),
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
            description: "Select a surface and one of its sub-views (organization: status/members/uploads/pending/declined · chat: today · memory: brain/proposals/accepted/denied/archive · quests: board/create/proposals/my-quests/archive · vault: secrets/disclose/proposals/exposed · wallet: balance/history/send/receive/status/settings).",
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
            name: "set_read_receipts",
            command: "set_read_receipts",
            description: "Turn this node's chat read receipts on or off (a local privacy switch, persisted to config.toml — never a governance vote). While off, this node sends no read confirmations of its own AND hides other members' receipts from its chat view (symmetric).",
            schema: || json!({
                "type": "object",
                "properties": { "enabled": { "type": "boolean" } },
                "required": ["enabled"]
            }),
            build: |args| Ok(Command::SetReadReceipts {
                enabled: bool_arg(args, "enabled")?,
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
                    "s3_keep_copies": { "type": "integer" },
                    "sound_message": { "type": "string", "enum": ["none", "bell", "chime", "pop"] },
                    "sound_vote": { "type": "string", "enum": ["none", "bell", "chime", "pop"] },
                    "read_receipts": { "type": "boolean", "description": "send/show per-message chat read receipts (local privacy switch)" },
                    "mcp_port": { "type": "integer" },
                    "mcp_allow": { "type": "string", "description": "client IP allowlist: \"127.0.0.1\" | \"0.0.0.0\" | comma-separated" },
                    "mcp_token": { "type": "string", "description": "rotate the MCP API token (what the GUI's Rotate button does)" },
                    "anonymity": { "type": "string", "enum": ["tor", "none"] },
                    "tor_mode": { "type": "string", "enum": ["local", "embedded", "whonix"] },
                    "tor_port": { "type": "integer" }
                },
                "required": [
                    "headless", "workspace_dir", "s3_backup", "s3_endpoint",
                    "s3_access_key", "s3_secret_key", "s3_bucket",
                    "s3_interval_min", "s3_keep_copies", "sound_message",
                    "sound_vote", "read_receipts", "mcp_port", "mcp_allow",
                    "mcp_token", "anonymity", "tor_mode", "tor_port"
                ]
            }),
            build: |args| Ok(Command::SaveSettings {
                settings: settings_arg(args)?,
            }),
        },
        ToolDef {
            name: "patch_settings",
            command: "patch_settings",
            description: "Change SOME settings, keeping every field you do not mention. This is the tool for adjusting one thing: save_settings REPLACES everything, and its defaults are not neutral - anonymity defaults to \"none\" (a Tor node onto clearnet) and mcp_token to empty (authentication off), so a partial save_settings would silently reset them. Unknown keys are refused rather than ignored, and the relay pool keeps its own door (the relay_* tools).",
            schema: || json!({
                "type": "object",
                "description": "the settings to change, keyed as in read_session.settings",
                "properties": {
                    "headless": { "type": "boolean" },
                    "workspace_dir": { "type": "string" },
                    "s3_backup": { "type": "boolean" },
                    "s3_endpoint": { "type": "string" },
                    "s3_access_key": { "type": "string" },
                    "s3_secret_key": { "type": "string" },
                    "s3_bucket": { "type": "string" },
                    "s3_interval_min": { "type": "integer" },
                    "s3_keep_copies": { "type": "integer" },
                    "download_dir": { "type": "string" },
                    "sound_message": { "type": "string", "enum": ["none", "bell", "chime", "pop"] },
                    "sound_vote": { "type": "string", "enum": ["none", "bell", "chime", "pop"] },
                    "read_receipts": { "type": "boolean" },
                    "mcp_port": { "type": "integer" },
                    "mcp_allow": { "type": "string" },
                    "mcp_token": { "type": "string" },
                    "anonymity": { "type": "string", "enum": ["tor", "none"] },
                    "tor_mode": { "type": "string", "enum": ["local", "embedded", "whonix"] },
                    "tor_port": { "type": "integer" }
                }
            }),
            build: |args| Ok(Command::PatchSettings { patch: args.clone() }),
        },
        ToolDef {
            name: "relay_probe",
            command: "relay_probe",
            description: "Vet a Nostr relay before trusting it (B4): does it accept kind 445, can its auth demand be satisfied, does it retain events, does its frame cap fit the group? One verdict with ONE reason, on the notice channel (relay-ok:/relay-refused:). relay_confirm runs the same probe implicitly - an unusable relay never becomes a confirmed one.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "wss://relay.example.org - or ws://…onion for an onion service" }
                },
                "required": ["url"]
            }),
            build: |args| Ok(Command::RelayProbe { url: str_arg(args, "url")? }),
        },
        ToolDef {
            name: "relay_add",
            command: "relay_add",
            description: "Add a Nostr relay to this node's pool. NOTHING SHIPS PRE-TRUSTED: the node connects to no relay until one is added AND confirmed, so adding is safe — the entry lands unconfirmed, at the lowest priority, and nothing is dialed. The URL is validated and normalized (wss://…; ws://… only for a .onion or local/private host — plaintext to the clearnet is refused). Read the pool back from read_session.relays, which carries each entry's derived kind (onion|clearnet|local) and why it is or is not dialed.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "wss://relay.example.org — or ws://…onion for an onion service" }
                },
                "required": ["url"]
            }),
            build: |args| Ok(Command::RelayAdd { url: str_arg(args, "url")? }),
        },
        ToolDef {
            name: "relay_remove",
            command: "relay_remove",
            description: "Remove a relay from the pool entirely (to merely stop using it while keeping it listed, use relay_revoke).",
            schema: || json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
            build: |args| Ok(Command::RelayRemove { url: str_arg(args, "url")? }),
        },
        ToolDef {
            name: "relay_move",
            command: "relay_move",
            description: "Move a relay one position in the pool. The pool ORDER is the dial priority (position 0 is tried first). Moving past either end is a no-op, not an error.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "up": { "type": "boolean", "description": "true = towards position 0 (higher priority)" }
                },
                "required": ["url", "up"]
            }),
            build: |args| Ok(Command::RelayMove {
                url: str_arg(args, "url")?,
                up: bool_arg(args, "up")?,
            }),
        },
        ToolDef {
            name: "relay_confirm",
            command: "relay_confirm",
            description: "Confirm a relay — the operator's persisted \"yes, use this one\". An ONION relay needs nothing more and becomes dialable immediately. A CLEARNET or LOCAL relay is REFUSED unless accept_clearnet is true: a clearnet operator sees this node's subscriptions (and its IP address unless Tor is on); a local relay is reached directly on this machine or network, never over Tor. Confirming a non-onion relay WITH accept_clearnet also switches non-onion dialing on and remembers that (ADR-0004 amendment) — relay_clearnet_session stays as the deliberate off switch.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "accept_clearnet": { "type": "boolean", "description": "explicit acknowledgement of the non-Tor exposure (clearnet or local); ignored for .onion relays" }
                },
                "required": ["url"]
            }),
            build: |args| Ok(Command::RelayConfirm {
                url: str_arg(args, "url")?,
                accept_clearnet: args
                    .get("accept_clearnet")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
        },
        ToolDef {
            name: "relay_revoke",
            command: "relay_revoke",
            description: "Withdraw a relay's confirmation: it stays in the pool (and keeps its priority) but is no longer dialed.",
            schema: || json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
            build: |args| Ok(Command::RelayRevoke { url: str_arg(args, "url")? }),
        },
        ToolDef {
            name: "relay_clearnet_session",
            command: "relay_clearnet_session",
            description: "Switch dialing of non-Tor relays — CLEARNET and LOCAL — on or off. BOTH decisions are persisted and survive a restart (ADR-0004 amendment): confirming such a relay with accept_clearnet already switches it on, and switching it off stays off. Onion relays are unaffected and always connect on their own. While it is off, confirmed clearnet/local relays are blocked (read_session.relays shows \"clearnet_session_locked\") and a join over one is refused with that reason.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "unlock": { "type": "boolean", "description": "true = dial confirmed clearnet and local relays; false = go dark. Remembered across restarts." }
                },
                "required": ["unlock"]
            }),
            build: |args| Ok(Command::RelayClearnetSession { unlock: bool_arg(args, "unlock")? }),
        },
        ToolDef {
            name: "net_test_s3",
            command: "net_test_s3",
            description: "Test the S3 backup target (the backup settings panel's Test button): a real SigV4-signed HEAD /bucket probe over the configured transport (Tor when enabled, fail-closed). The result lands in session.s3_test (\"ok\" or \"error: …\" with the honest failure class — connect vs TLS vs 403 bad credentials vs 404 missing bucket). Omit fields to test the saved settings; pass them to test a draft.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "endpoint": { "type": "string", "description": "https://… or http://… endpoint (MinIO/onion supported, path-style); omit to test settings.s3_endpoint" },
                    "access_key": { "type": "string", "description": "access key id; omit to use the saved one" },
                    "secret_key": { "type": "string", "description": "secret key; omit to use the saved one" },
                    "bucket": { "type": "string", "description": "bucket to probe; omit to use the saved one" }
                }
            }),
            build: |args| {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                Ok(Command::NetTestS3 {
                    endpoint: s("endpoint"),
                    access_key: s("access_key"),
                    secret_key: s("secret_key"),
                    bucket: s("bucket"),
                })
            },
        },
        ToolDef {
            name: "net_test_tor",
            command: "net_test_tor",
            description: "Test whether Tor is actually there and working (the anonymity settings panel's Test button). Reports the RUNG of evidence it reached in session.tor_test.state — never a bare yes/no: \"off\" (Tor is not enabled; nothing was sent), \"misconfigured\" (the fail-closed dialer refused the config; nothing was probed), \"no_proxy\" (nothing is listening at the SOCKS address — no Tor daemon there), \"proxy_only\" (a socket answered there, but NO traffic was routed through it, so no circuit is proven), \"circuit_failed\" (no connection to the relay through Tor — this does NOT single out Tor: the relay itself may be down or firewalled, see detail), \"circuit_timeout\" (no answer within the deadline — a first embedded-Tor start bootstraps the directory and can take minutes, so this is not a failure verdict), \"circuit\" (a relay from the confirmed pool completed a WebSocket handshake end to end through Tor — the only state that means Tor works, and it proves the RELAY answered, not merely that a SOCKS server said ok), \"no_target\" (nothing could be tested at all). session.tor_test also carries detail/proxy/target/ms. The probe never invents a host: with no confirmed, Tor-routable relay it stops at \"proxy_only\" — which also happens when the relays ARE confirmed but non-onion dialing is switched off (relay_clearnet_session / [transport.nostr] clearnet_enabled). THE CALL RETURNS BEFORE THE PROBE FINISHES: poll read_session until tor_test.state leaves \"testing\". Omit fields to test the saved settings; pass them to test a draft.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "network": { "type": "string", "description": "anonymity network to test (\"tor\"; anything else is refused as \"off\"); omit to use settings.anonymity" },
                    "mode": { "type": "string", "enum": ["local", "embedded", "whonix"], "description": "Tor mode; omit to use settings.tor_mode" },
                    "port": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "local Tor SOCKS port; omit or 0 to use settings.tor_port" }
                }
            }),
            build: |args| {
                let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                let port = args
                    .get("port")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Ok(Command::NetTestTor {
                    network: s("network"),
                    mode: s("mode"),
                    port: u16::try_from(port)
                        .map_err(|_| format!("port {port} is out of range (0..=65535)"))?,
                })
            },
        },
        ToolDef {
            name: "net_list_backups",
            command: "net_list_backups",
            description: "List the configured S3 bucket's backup objects (the settings backup table's refresh): a real SigV4-signed ListObjectsV2 under the molt/ prefix over the configured transport (Tor when enabled, fail-closed), driven by the SAVED settings. Objects with no matching local workspace land as real orphans in session.backup_orphans (foreign keys as unknown entries); the honest status lands in session.s3_list (\"ok\" or \"error: …\" — including \"no endpoint configured\" when no backup target is set up). Read-only against the bucket.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::NetListBackups),
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
            description: "Switch automatic S3 backup on or off for one workspace by its id (persisted in the workspace's prefs.toml). Enabling only persists the pref — the backup ticker runs the real first upload on its next pass, and the last-backup stamp moves ONLY on a confirmed upload (never on enable). Failures land honestly in the workspace entry's backup_error.",
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
            name: "backup_now",
            command: "backup_now",
            description: "Run one workspace's S3 backup NOW (same task as the automatic ticker, interval ignored): builds the crash-consistent encrypted molt-export-v1 blob in workspace key mode (restorable from the recovery phrase + workspace id — no passphrase involved) and uploads it to the configured bucket over the configured transport (Tor when enabled, fail-closed), then prunes copies beyond s3_keep_copies. Async kickoff — the honest outcome lands in the workspace entry (last_backup stamp only on a confirmed upload; backup_error otherwise). Refused for sealed-at-rest workspaces (no key material is accessible).",
            schema: || json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "the workspace id from read_session" } },
                "required": ["id"]
            }),
            build: |args| Ok(Command::BackupNow {
                id: str_arg(args, "id")?,
            }),
        },
        ToolDef {
            name: "encrypt_workspace",
            command: "encrypt_workspace",
            description: "Seal a closed workspace at rest under its recovery phrase: the phrase is verified against the workspace first, then the device-sealed key material is removed from disk — the phrase becomes the only way back in. The workspace becomes inactive and open_workspace refuses until decrypt_workspace; the state survives restarts. The active workspace cannot be encrypted.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "phrase": { "type": "string", "description": "the workspace's recovery phrase (verified before any key is removed)" }
                },
                "required": ["id", "phrase"]
            }),
            build: |args| Ok(Command::EncryptWorkspace {
                id: str_arg(args, "id")?,
                phrase: str_arg(args, "phrase")?,
            }),
        },
        ToolDef {
            name: "decrypt_workspace",
            command: "decrypt_workspace",
            description: "Decrypt an at-rest-sealed workspace so it can be opened again. The recovery phrase is really verified (an authenticated decrypt of the workspace's genesis with the derived key); a wrong phrase is a hard error and changes nothing on disk.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "phrase": { "type": "string", "description": "the workspace's recovery phrase" }
                },
                "required": ["id", "phrase"]
            }),
            build: |args| Ok(Command::DecryptWorkspace {
                id: str_arg(args, "id")?,
                phrase: str_arg(args, "phrase")?,
            }),
        },
        ToolDef {
            name: "export_workspace",
            command: "export_workspace",
            description: "Export a workspace as ONE encrypted blob file (*.molt.enc, format molt-export-v1): manifest, the encrypted history, the threshold-signed chain, the newest snapshot, the logo — and, when stored on this device, the recovery seed. CAUTION: with the seed inside, blob + passphrase replaces the recovery phrase (full seat capability) — guard both like the phrase. Live MLS/transport state is NEVER exported: the blob restores knowledge; rejoining the live republic goes through the recovery ritual. Protection: Argon2id-stretched passphrase (minimum 10 characters) + XChaCha20-Poly1305. Async kickoff — the honest outcome (ok with byte count and skipped files, or the real error) lands in read_session's `export` state; there is no fake success.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "the workspace id from read_session" },
                    "dest": { "type": "string", "description": "target file path (~ is expanded, parents are created, an existing file is atomically replaced)" },
                    "passphrase": { "type": "string", "description": "export passphrase, minimum 10 characters" }
                },
                "required": ["id", "dest", "passphrase"]
            }),
            build: |args| Ok(Command::ExportWorkspace {
                id: str_arg(args, "id")?,
                dest: str_arg(args, "dest")?,
                passphrase: str_arg(args, "passphrase")?,
            }),
        },
        ToolDef {
            name: "restore_start",
            command: "restore_start",
            description: "Begin a REAL restore from an encrypted molt-export-v1 backup blob. way=file reads a *.molt.enc file; way=s3 downloads from the CONFIGURED bucket (saved settings, Tor-capable fail-closed transport) — target is then the workspace-id pseudonym from the backup table (the newest object is used) or a full molt/<id>/<ts>.molt.enc object key. The blob is decrypted and staged off the actor, then the engine HARD-VERIFIES the threshold-signed chain before anything materializes (an unverifiable chain restores nothing). Progress and log lines in read_session's restore state report only what actually happened. The restored workspace opens DETACHED: knowledge (history, verified chain, prefs) is restored, live membership (MLS group, mesh) is NOT — rejoining the live republic is the recovery ritual (recover_start). Same-id collision refuses unless replace=true (which moves the existing dir to the recoverable trash first). NOTE: way=file has no GUI panel - this tool is the only surface offering it (the GUI's Restore wizard carries the link and S3 ways).",
            schema: || json!({
                "type": "object",
                "properties": {
                    "way": { "type": "string", "enum": ["s3", "file"] },
                    "target": { "type": "string", "description": "file way: the *.molt.enc path; s3 way: the 64-hex workspace id (newest backup) or a full molt/<id>/<ts>.molt.enc object key" },
                    "secret": { "type": "string", "description": "the blob's secret — the recovery phrase (24 words) for automatic S3 backups, the export passphrase for manual file exports" },
                    "replace": { "type": "boolean", "description": "same-id collision policy: true trashes the existing local workspace first (recoverable 30 days); default false refuses" }
                },
                "required": ["way", "target", "secret"]
            }),
            build: |args| Ok(Command::RestoreStart {
                way: str_arg(args, "way")?,
                target: str_arg(args, "target")?,
                secret: str_arg(args, "secret")?,
                replace: args.get("replace").and_then(Value::as_bool).unwrap_or(false),
            }),
        },
        ToolDef {
            name: "restore_cancel",
            command: "restore_cancel",
            description: "Abandon the restore and return to the choice screen: the in-flight download/staging task is aborted and the staging removed — nothing partial stays behind.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::RestoreCancel),
        },
        ToolDef {
            name: "restore_finish",
            command: "restore_finish",
            description: "Finish a successful restore: open the restored workspace — DETACHED (knowledge restored, membership not; the session notice says so) — straight to the main screen.",
            schema: || json!({ "type": "object", "properties": {} }),
            build: |_| Ok(Command::RestoreFinish),
        },
        ToolDef {
            name: "create_start",
            command: "create_start",
            description: "Begin founding a new republic: the engine derives the founder's identity, mints one-time invite links per member, and runs the real founding ritual with a live log; read_session shows the seed, the joinable links, and each seat filling in. Once every member has joined, propose the charter with create_propose. NOTE: this build has no network transport yet (the Nostr transport lands with N4), so create_start currently fails honestly with exactly that message instead of starting a run.",
            schema: || json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "the new republic's name (must be unique locally)" },
                    "member": { "type": "string", "description": "the founder's handle" },
                    "threshold": { "type": "integer", "description": "approvals required (m), 1..=members" },
                    "members": { "type": "integer", "description": "member count (n), 2..=13" }
                },
                "required": ["name", "member", "threshold", "members"]
            }),
            build: |args| Ok(Command::CreateStart {
                name: str_arg(args, "name")?,
                member: str_arg(args, "member")?,
                threshold: u8_arg(args, "threshold")?,
                members: u8_arg(args, "members")?,
                relays: Vec::new(),
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
            description: "As a surviving member, mint a single-use recovery link for a fellow member who lost their device (a manually-granted re-admission for an existing seat). The returning member does NOT need to be online. The engine listens for their request — on Nostr over the republic's relays, on the legacy shape over a fresh mesh queue — and the outcome arrives on the session notice (read_session): 'recovery-link:<link>' with the molt://recover/… link to share off-band, or 'recovery-link-failed:<reason>' naming what this node is missing. The returning member proves its seat with a re-derived-identity signature, then the group re-admits it by threshold.",
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
            description: "As a member who lost their device, rejoin a republic from a coordinator-minted molt://recover/… link using your recovery phrase (a fresh device with only the phrase). The engine re-derives the seat identity, proves it to the coordinator, waits for the group's threshold re-admission, re-enters the encrypted group from the Welcome, verifies the served chain from its genesis, and materializes the recovered workspace locally. NOTE: this build has no network transport yet (the Nostr transport lands with N4) — the link parses and the context arms, then the run fails honestly with exactly that reason.",
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
            description: "Begin joining a republic from a real molt://invite/… link (must carry the transport handover — a bare preview link is rejected). The engine shows the joiner's own recovery phrase and runs the join off the actor; when the founder proposes the charter it is surfaced for join_confirm_charter. NOTE: this build has no network transport yet (the Nostr transport lands with N4), so the join run currently fails honestly with exactly that message in its log.",
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
    /// on the documented internal list (see docs/security/mcp-security.md).
    #[test]
    fn co_equality_every_command_is_a_tool_or_documented_internal() {
        // engine-internal: the run tickers are the engine's own clock;
        // net_delivered / net_peer_seen / net_send_failed and the founding
        // ritual's net_join_requested / net_seal_signed are the node's own
        // transport/ritual tasks speaking (exposing them would let an agent
        // forge network peers or ritual members);
        // net_test_s3_result is the node's own S3 probe reporting back
        // (net_test_s3 is the tool); net_list_backups_result is the node's
        // own bucket-listing task reporting back (net_list_backups is the
        // tool — an agent must not be able to forge bucket contents);
        // net_ritual_link_ready / net_ritual_failed are the off-actor
        // provisioning task reporting a seat's real link or a provisioning
        // failure; net_recover_link_failed is the recovery-mint provisioning
        // task reporting its failure (recover_invite_start is the tool; the
        // outcome — link or failure — rides the session notice);
        // net_join_sealed / net_join_failed are the off-actor join
        // task reporting back; net_recover_sealed / net_recover_failed are the
        // off-actor rejoin task reporting back (recover_start is the tool);
        // net_recover_announced is the recovery recv loop delivering a
        // rejoiner's mesh announce and net_mesh_extended is the node's own
        // off-actor mesh-extension task reporting its assembled link — both
        // the node's own transport tasks speaking, not agent-forgeable;
        // reload_settings / config_notice are the config
        // watcher's mirror path — an agent that wants a reload edits via
        // save_settings (see docs/security/mcp-security.md)
        // net_mesh_announced is a member's post-founding mesh handover reaching
        // the founder over the star; net_mesh_ready is the founder's off-actor
        // bootstrap task reporting the assembled mesh — both are the node's own
        // transport tasks speaking, not agent-forgeable.
        // net_export_done / net_export_failed are the off-actor export task
        // reporting its real outcome (export_workspace is the tool; an agent
        // must not be able to forge an export success or failure).
        // backup_tick is the backup ticker's own heartbeat; net_backup_done /
        // net_backup_failed are the off-actor backup task reporting its real
        // outcome (backup_now / set_workspace_backup are the tools; an agent
        // must not be able to forge a backup stamp or failure).
        // net_restore_progress / net_restore_staged / net_restore_failed are
        // the off-actor restore task reporting real progress and its staged/
        // failed outcome (restore_start is the tool; the staged blob itself
        // rides an engine-internal slot and the HANDLER re-verifies the
        // chain, so even a forged internal command cannot materialize an
        // unverified workspace). RestoreTick is gone: there is no simulated
        // restore progress anymore.
        const INTERNAL: [&str; 50] = [
            "net_test_s3_result",
            // net_test_tor_result is the off-actor Tor probe reporting its
            // real verdict (net_test_tor is the tool; an agent must not be
            // able to forge a "Tor works" answer for the operator).
            "net_test_tor_result",
            "net_presence_tick",
            "net_delivery_tick",
            "net_list_backups_result",
            "backup_tick",
            "net_backup_done",
            "net_backup_failed",
            "net_restore_progress",
            "net_restore_staged",
            "net_restore_failed",
            "net_export_done",
            "net_export_failed",
            "net_file_shared",
            "net_file_share_failed",
            "net_file_request_ready",
            "net_file_progress",
            "net_file_done",
            "net_file_failed",
            "net_delivered",
            "net_peer_seen",
            "net_send_failed",
            "net_link_up",
            "net_link_down",
            "net_send_ok",
            "net_join_requested",
            "net_seal_signed",
            "net_recover_requested",
            "net_recover_link_ready",
            "net_recover_link_failed",
            "net_ritual_link_ready",
            "net_ritual_failed",
            // the publish task reporting its real per-relay outcome: an MCP
            // agent must not be able to forge a relay result and thereby fail
            // (or fake) a founding leg
            "net_ritual_published",
            // a task reporting a transport CONDITION (not an operator
            // decision): an MCP agent must not be able to write lines into a
            // founding or join log
            "net_ritual_note",
            "net_join_note",
            "net_recover_note",
            // the relay probe's verdict: an agent must not be able to forge
            // one and thereby confirm a dead relay
            "net_relay_probed",
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

    /// Concept Q8: a file share is a chat message, so it takes the same
    /// optional channel argument as `chat_send` (absent = the group view).
    #[test]
    fn share_file_accepts_channel() {
        // The schema exposes the same channel object as chat_send.
        let schema = (tool_named("share_file").schema)();
        assert_eq!(
            schema["properties"]["channel"]["properties"]["kind"]["enum"],
            json!(["group", "patch", "topic"])
        );
        assert_eq!(schema["required"], json!(["path"]));

        // Omitted channel → the all-hands group (the default view).
        match build("share_file", &json!({ "path": "/tmp/a.pdf" })).expect("plain share builds") {
            Command::ShareFile { channel, .. } => assert_eq!(channel, ChannelRef::Group),
            other => panic!("wrong command: {other:?}"),
        }
        // Patch channel by proposal id.
        match build(
            "share_file",
            &json!({ "path": "/tmp/a.pdf", "channel": { "kind": "patch", "id": 7 } }),
        )
        .expect("patch share builds")
        {
            Command::ShareFile { channel, .. } => {
                assert_eq!(channel, ChannelRef::Patch { id: ProposalId(7) });
            }
            other => panic!("wrong command: {other:?}"),
        }
        // Topic channel — normalized exactly like chat_send.
        match build(
            "share_file",
            &json!({ "path": "/tmp/a.pdf", "channel": { "kind": "topic", "name": "  Budget " } }),
        )
        .expect("topic share builds")
        {
            Command::ShareFile { channel, .. } => {
                assert_eq!(
                    channel,
                    ChannelRef::Topic {
                        name: "Budget".to_string()
                    }
                );
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

    /// `read_state` exposes the retention time axis exactly like the GUI's
    /// General/Archive sub-views (co-equality): an optional `view` string
    /// that rides the same `Command::ReadState`. Absent/null = the whole
    /// window; a present non-string is an error, never ignored.
    #[test]
    fn read_state_takes_the_retention_view_axis() {
        let schema = (tool_named("read_state").schema)();
        assert_eq!(
            schema["properties"]["view"]["enum"],
            json!(["today", "unread"])
        );

        match build("read_state", &json!({ "surface": "chat" })).expect("no view builds") {
            Command::ReadState { view, .. } => assert_eq!(view, None),
            other => panic!("wrong command: {other:?}"),
        }
        match build("read_state", &json!({ "surface": "chat", "view": null }))
            .expect("null view builds")
        {
            Command::ReadState { view, .. } => assert_eq!(view, None),
            other => panic!("wrong command: {other:?}"),
        }
        match build("read_state", &json!({ "surface": "chat", "view": "unread" }))
            .expect("unread slice builds")
        {
            Command::ReadState { view, .. } => assert_eq!(view, Some("unread".to_string())),
            other => panic!("wrong command: {other:?}"),
        }
        // both filters compose on the one command
        match build(
            "read_state",
            &json!({ "surface": "chat", "channel": { "kind": "group" }, "view": "today" }),
        )
        .expect("channel + view build")
        {
            Command::ReadState { channel, view, .. } => {
                assert_eq!(channel, Some(ChannelRef::Group));
                assert_eq!(view, Some("today".to_string()));
            }
            other => panic!("wrong command: {other:?}"),
        }
        // a present view of the wrong type is an error, never ignored
        assert!(build("read_state", &json!({ "surface": "chat", "view": 5 })).is_err());
        assert!(build("read_state", &json!({ "surface": "chat", "view": ["today"] })).is_err());
    }

    #[test]
    fn net_test_tor_maps_its_draft_arguments() {
        // Nothing passed = "test whatever is saved" — the engine falls back.
        match build("net_test_tor", &json!({})).expect("empty builds") {
            Command::NetTestTor {
                network,
                mode,
                port,
            } => {
                assert!(network.is_empty() && mode.is_empty());
                assert_eq!(port, 0, "0 is the 'not given' marker");
            }
            other => panic!("wrong command: {other:?}"),
        }
        // A draft from the settings panel (not yet saved).
        match build(
            "net_test_tor",
            &json!({ "network": "tor", "mode": "local", "port": 9150 }),
        )
        .expect("draft builds")
        {
            Command::NetTestTor {
                network,
                mode,
                port,
            } => {
                assert_eq!(network, "tor");
                assert_eq!(mode, "local");
                assert_eq!(port, 9150);
            }
            other => panic!("wrong command: {other:?}"),
        }
        // An out-of-range port is a clean error, never a silent truncation
        // onto some OTHER port the agent did not ask for.
        assert!(build("net_test_tor", &json!({ "port": 70000 })).is_err());
        // The description must not promise more than the ladder delivers.
        let tool = tool_named("net_test_tor");
        assert!(
            tool.description.contains("proxy_only") && tool.description.contains("circuit"),
            "the tool has to name the rungs an agent will see"
        );
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
            Command::DownloadFile { id: got, dest } => {
                assert_eq!(got, id);
                assert_eq!(dest, None);
            }
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

    /// **A peer that never authenticates cannot make us allocate without
    /// bound.** On a node whose `[mcp].allow` opens the port past loopback,
    /// anyone through the IP filter used to be able to stream gigabytes with
    /// no newline in them: `read_line` grew the buffer to match, and the
    /// process died of it before a token was ever checked.
    ///
    /// The connection ENDS on such a line rather than skipping it — the tail
    /// of an over-long line would otherwise parse as a request of its own.
    #[tokio::test]
    async fn a_giant_pre_auth_line_ends_the_connection_instead_of_growing() {
        let h = wallet();
        // no newline, comfortably past the bound
        let flood = vec![b'x'; MAX_RPC_LINE + 4096];
        let mut out: Vec<u8> = Vec::new();
        serve_conn(h, BufReader::new(&flood[..]), &mut out, Some("secret".to_string()))
            .await
            .expect("the server ends the connection, it does not error out");
        let answer = String::from_utf8_lossy(&out);
        assert!(
            answer.contains("too large"),
            "the peer is told why the connection ended: {answer}"
        );
        // …and nothing past the bound was ever parsed as a request
        assert_eq!(answer.lines().count(), 1, "one refusal, then silence: {answer}");
    }

    /// **A partial settings call cannot reset what it does not mention.**
    ///
    /// `save_settings` used to fill every missing key from
    /// `SessionSettings::default()`, and those defaults are not neutral:
    /// `anonymity` defaults to `"none"` and `mcp_token` to empty. An agent
    /// adjusting a backup interval could take a Tor node onto clearnet and
    /// switch MCP authentication off in the same call, and the reply said
    /// "ack".
    ///
    /// Now the full-replace verb REFUSES a partial payload by name, and
    /// there is a partial verb that merges in the engine, where the current
    /// values are.
    #[test]
    fn a_partial_settings_payload_is_refused_not_defaulted() {
        let partial = json!({ "s3_interval_min": 15 });
        let err = build("save_settings", &partial)
            .expect_err("a partial full-replace must not be accepted");
        assert!(
            err.contains("required") && err.contains("patch_settings"),
            "the refusal names the missing field AND the right tool: {err}"
        );
        // …and the partial verb takes exactly that payload, untouched
        match build("patch_settings", &partial).expect("patch builds") {
            Command::PatchSettings { patch } => {
                assert_eq!(patch, partial, "the patch travels verbatim - the engine merges");
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// **A rotated token applies to the next connection, not the next
    /// restart.** `mcp-security.md` promises rotation "takes effect
    /// immediately"; `serve_tcp` captured its copy at startup, so a leaked
    /// token kept working until the process was restarted and the new one
    /// was refused — the exact inverse of what an operator rotating under
    /// suspicion needs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_accept_loop_reads_the_token_that_is_current_now() {
        let h = wallet();
        let with_token = |t: &str| molt_core::SessionSettings {
            mcp_token: t.to_string(),
            ..molt_core::SessionSettings::default()
        };
        h.execute(Command::SaveSettings { settings: with_token("first") })
            .await
            .expect("save");
        assert_eq!(live_token(&h).await.as_deref(), Some("first"));

        // …rotate, exactly as the GUI button and save_settings.mcp_token do
        h.execute(Command::PatchSettings {
            patch: json!({ "mcp_token": "second" }),
        })
        .await
        .expect("rotate");
        assert_eq!(
            live_token(&h).await.as_deref(),
            Some("second"),
            "the next connection is gated on the NEW value"
        );
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
