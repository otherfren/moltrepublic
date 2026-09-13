# MCP over Streamable HTTP with a Bearer token

**Status: BUILT 2026-09-13** (`molt-mcp/src/http.rs`; the plan below is the
executed one). Verified against Claude Code 2.1.270 with
`claude mcp add --transport http` - connected, catalogue fetched. One
finding beyond the plan: **the served protocol versions are capped at
2025-11-25** on both transports. Claude Code opens with `server/discover`
under 2026-07-28 and then requires `ttlMs`/`cacheScope` on a `tools/list`
result, which rmcp 3.3 does not emit; offering that version made the client
refuse the catalogue. rmcp answers the newer probe with a version-refusal
naming what it serves, and the client continues on 2025-11-25. Lift the cap
when rmcp emits the 2026-07-28 result shapes.

## Why

A field report (2026-09-13): a client "cannot read the MCP interface, it is
not real MCP but TCP with wrong line breaks". Measured against a live node:
the framing is correct (one JSON-RPC object per `\n`, no `\r`, no embedded
newline - byte-identical to what the stdio transport mandates), but the
TCP port is a **custom transport**. The spec knows stdio and Streamable
HTTP only; no stock client speaks newline-JSON over a raw socket, and none
can put our token into `initialize` `params.token`. A Streamable-HTTP
client pointed at the port sends `POST /mcp HTTP/1.1` and receives one
`-32700 parse error` per header line - which is exactly what the report
describes.

The fix is a Streamable HTTP endpoint that takes the two existing config
tokens as `Authorization: Bearer`. The token comparison and the scope
model (`Scope::Seat` / `Scope::Read`, `docs_archive/security/mcp-security.md`)
exist; only the HTTP frame around them is missing.

## What exists (search first)

| Candidate | Verdict |
|---|---|
| **rmcp 3.3.0** (official Rust MCP SDK, `modelcontextprotocol/rust-sdk`) | **TAKE** for the HTTP layer. `transport-streamable-http-server` is a `tower_service::Service<http::Request>`; stateless mode (`legacy_session_mode=false`, `json_response=true`) answers a POST with one `application/json` body, GET/DELETE with 405; `Host` validation defaults to loopback (DNS-rebinding), `Origin` allowlist available, `MCP-Protocol-Version` header checked, `initialize` negotiation implemented for 2024-11-05 … 2026-07-28. It injects `http::request::Parts` into the request extensions, so a Bearer check outside rmcp can hand the handler a `Scope`. Pure Rust; new crates in the app graph: rmcp, sse-stream, tower-service, http-body, http-body-util, tokio-util, tokio-stream, rand 0.10 (third rand). No rustls, no ring. MSRV 1.88. |
| **hyper 1.11 + hyper-util 0.1** | TAKE to serve the tower service on our accepted socket (`http1::Builder::serve_connection`). Both already in the registry cache; pure Rust. |
| hand-rolled HTTP/1.1 over the existing TcpListener | LEAVE. It is the URL-parser mistake again: the spec's HTTP layer has a dozen edge rules (Accept, 202 for notifications, session/version headers, Host/Origin) that rmcp maintains and we would drift on. |
| rmcp for stdio/TCP as well | NOT NOW. The raw-TCP handshake carries the token in `params.token`; rmcp's typed `InitializeRequestParams` drops the field, so the existing TCP clients (gui_walk.py, the wake hooks, the docs recipe) would break. A later migration is a separate decision (§Follow-ups). |
| rmcp's own HTTP client as the test client | LEAVE. It needs `reqwest`; a dev-dep that widens the graph hides provider/ring regressions (the `nostr-relay-pool` trap in CLAUDE.md). Tests speak raw HTTP/1.1 over a `TcpStream`; the real-client check is the `claude` CLI (§Verification). |

## Design

1. **One port, two protocols.** The accept loop in `molt_mcp::serve_tcp`
   keeps the peer-IP allowlist and the connection bound, then peeks the
   first byte: an ASCII uppercase letter is an HTTP method
   (`GET`/`POST`/`DELETE`/`OPTIONS`), `{` or whitespace is a JSON-RPC line.
   HTTP connections go to hyper serving the rmcp service; everything else
   runs the existing line loop unchanged. URL: `http://127.0.0.1:<port>/mcp`.
   (Alternative, see Q1: a second `[mcp].http_port`.)
2. **Bearer gate outside rmcp.** A small hyper service wraps rmcp: it reads
   `Authorization: Bearer <token>`, compares with `secret_eq` against the
   LIVE tokens (`live_tokens`, same as TCP - rotation applies at once), and
   inserts the resulting `Scope` into the request extensions. Missing or
   wrong token → `401` with `WWW-Authenticate: Bearer realm="moltrepublic"`,
   empty body. An empty configured seat token admits without a header
   (parity with TCP today, same startup warning). The read token maps to
   `Scope::Read`.
3. **Handler adapter.** `impl rmcp::ServerHandler for HttpSeat`:
   `get_info` → the same `serverInfo`/`instructions` the line loop sends;
   `list_tools` → `tool_defs(scope)` converted to `rmcp::model::Tool`;
   `call_tool` → the existing `call_tool(handle, name, args, scope)`,
   `Ok(text)` → `CallToolResult::success`, `Err(msg)` → `::error`; a seat
   tool under a read scope → JSON-RPC error `-32001` (the current contract:
   a protocol error, not prose). The scope comes from
   `ctx.extensions.get::<http::request::Parts>()` → `parts.extensions`.
   `tools()` stays the single catalogue; the co-equality test is untouched.
4. **rmcp config.** `legacy_session_mode=false`, `json_response=true`,
   `allowed_hosts` default (loopback) - when `[mcp].allow` is not loopback
   the configured bind host is added, `allowed_origins` empty (missing
   Origin passes; a browser origin is not a supported client), body bound
   = `MAX_RPC_LINE` via `http_body_util::Limited` in the wrapper (413).
5. **Version negotiation in the line loop.** `initialize` echoes the
   client's `protocolVersion` when it is one we serve, else the newest.
   Today it always answers `2025-06-18`; a strict client disconnects.
6. **Logging.** One `info` line per HTTP connection like TCP
   (`peer=… via=http`); a refused Bearer is a `warn` with `peer=` only.

## Tests (red first, `cargo test -p molt-mcp`)

- `http_initialize_with_bearer` → 200, `application/json`, result carries
  `serverInfo` and echoes the client's protocol version.
- `http_missing_bearer_is_401` / `http_wrong_bearer_is_401` with the
  `WWW-Authenticate` header; the body is not JSON-RPC.
- `http_read_token_narrows_tools` → `tools/list` is the read catalogue; a
  seat tool → `-32001`.
- `http_notification_is_202` (`notifications/initialized`, empty body).
- `http_get_and_delete_are_405`.
- `http_body_over_bound_is_413`.
- `http_foreign_host_is_403` (`Host: evil.example`, loopback allowlist).
- `http_peer_off_the_allowlist_gets_nothing` (empty allowlist: the socket
  closes before a byte is read).
- `same_port_still_serves_line_json` - the existing TCP tests keep
  passing on the sniffed listener (they are the regression).
- `line_loop_echoes_supported_protocol_version`.
- `crates/molt-app`: none (a molt-app test is the window build).
- Graph guard: `cargo tree -p molt-app -e no-dev -i ring` stays empty;
  recorded in the commit message.

## Verification with a real client

```
claude mcp add --transport http molt http://127.0.0.1:<port>/mcp \
  --header "Authorization: Bearer <token from config.toml>"
claude mcp list          # must show the server connected
```

against a headless node from the dev-ui build (`scripts/dev-ui.sh build`,
own scratch config, own port - never the user's running node).

## Docs to touch in the same change

- `docs_archive/security/mcp-security.md`: transport table gains the HTTP
  row (token via Bearer, same allowlist), the "when you need TCP" recipe
  gets the `claude mcp add` line first.
- `moltd --generate-config` `[mcp]` comment: one line naming the URL.

## Decisions (2026-09-13)

- **Q1 port:** same port, protocol sniffed. No `[mcp].http_port`.
- **Q2 rmcp:** yes. Note `server` pulls `schemars`; accepted.
- **Q3 browser clients:** no Origin validation. rmcp's `Host` check stays
  at its loopback default while `[mcp].allow` is loopback-only and is
  disabled otherwise - the peer-IP allowlist is the gate, for HTTP as for
  the line protocol, and a test pins that a peer off the list gets no
  HTTP answer at all.

## Follow-ups (not in this change)

- Migrate stdio and raw TCP onto rmcp's transports once the `params.token`
  handshake can be retired (would collapse the two dispatch paths).
- `structuredContent` on tool results (2025-06-18) - optional, the text is
  already JSON.
