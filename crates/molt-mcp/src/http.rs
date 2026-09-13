// SPDX-License-Identifier: GPL-3.0-or-later

//! MCP over Streamable HTTP on the same port as the line protocol
//! (`docs_archive/security/streamable_http.md`): the official SDK's tower service, served
//! by hyper on a connection whose first byte is an HTTP method. The token
//! arrives as `Authorization: Bearer`; the peer-IP allowlist was applied
//! before the byte was read.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::{header, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Empty};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use molt_engine::WalletHandle;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
    Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tower_service::Service;

use crate::{
    call_tool, supported_versions, tool_defs, tools, Credentials, Scope, INSTRUCTIONS,
    MAX_RPC_LINE,
};

/// The HTTP face of one listener; cloned per connection.
#[derive(Clone)]
pub(crate) struct Http {
    svc: StreamableHttpService<Seat, LocalSessionManager>,
}

impl Http {
    /// `loopback_only` keeps rmcp's `Host` check (DNS rebinding); a wider
    /// `[mcp].allow` disables it - the peer-IP allowlist is the gate there.
    pub(crate) fn new(handle: WalletHandle, loopback_only: bool) -> Self {
        let mut config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_max_request_body_bytes(MAX_RPC_LINE);
        if !loopback_only {
            config = config.disable_allowed_hosts();
        }
        let svc = StreamableHttpService::new(
            move || Ok(Seat { handle: handle.clone() }),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        Self { svc }
    }

    pub(crate) async fn serve(
        self,
        sock: TcpStream,
        creds: Credentials,
        peer: SocketAddr,
    ) -> std::io::Result<()> {
        let creds = Arc::new(creds);
        let svc = service_fn(move |req: Request<Incoming>| {
            let mut inner = self.svc.clone();
            let creds = creds.clone();
            async move { Ok::<_, Infallible>(gate(&creds, peer, req, &mut inner).await) }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(sock), svc)
            .await
            .map_err(std::io::Error::other)
    }
}

type Body = BoxBody<Bytes, Infallible>;

fn empty(status: StatusCode) -> http::response::Builder {
    Response::builder().status(status)
}

/// Bearer → scope, body bound, then rmcp. The scope rides the request
/// extensions; rmcp hands them to the handler as `http::request::Parts`.
async fn gate(
    creds: &Credentials,
    peer: SocketAddr,
    mut req: Request<Incoming>,
    inner: &mut StreamableHttpService<Seat, LocalSessionManager>,
) -> Response<Body> {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
        })
        .unwrap_or("");
    let Some(scope) = creds.scope_for(given) else {
        tracing::warn!(%peer, "MCP HTTP refused: missing or invalid bearer token");
        return empty(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Bearer realm=\"moltrepublic\"")
            .body(Empty::new().boxed())
            .unwrap_or_default();
    };
    // declared up front so an oversized body is refused before it is read
    // (rmcp bounds the stream as well, for a chunked body)
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if declared > MAX_RPC_LINE {
        tracing::warn!(%peer, bytes = declared, "MCP HTTP request past the bound");
        return empty(StatusCode::PAYLOAD_TOO_LARGE)
            .body(Empty::new().boxed())
            .unwrap_or_default();
    }
    req.extensions_mut().insert(scope);
    match inner.call(req).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

/// The seat behind the HTTP face: the same catalogue and the same
/// `call_tool` as the line protocol, scoped by the bearer of each request.
#[derive(Clone)]
struct Seat {
    handle: WalletHandle,
}

fn scope_of(ctx: &RequestContext<RoleServer>) -> Result<Scope, McpError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|p| p.extensions.get::<Scope>().copied())
        .ok_or_else(|| McpError::internal_error("request carries no scope", None))
}

impl ServerHandler for Seat {
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(supported_versions())
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        info.server_info = Implementation::new("moltrepublic", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let scope = scope_of(&ctx)?;
        let tools = serde_json::from_value(Value::Array(tool_defs(scope)))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let scope = scope_of(&ctx)?;
        let name = request.name.as_ref();
        // a protocol error, not a tool result: the line loop's contract
        if scope == Scope::Read
            && tools()
                .iter()
                .any(|t| t.name == name && t.scope == Scope::Seat)
        {
            return Err(McpError::new(
                ErrorCode(-32001),
                "unauthorized: read-only token",
                None,
            ));
        }
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        let result = match call_tool(&self.handle, name, &args, scope).await {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(msg) => CallToolResult::error(vec![ContentBlock::text(msg)]),
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::wallet;
    use crate::{serve_listener, tool_defs, Scope, MAX_RPC_LINE};
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const SEAT: &str = "seat-secret";
    const READ: &str = "read-secret";

    async fn spawn(allowlist: Vec<IpAddr>, seat: &str, read: &str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (seat, read) = (seat.to_string(), read.to_string());
        // the listener reads the LIVE keys from the session, not its args
        let handle = wallet();
        handle
            .execute(molt_core::Command::PatchSettings {
                patch: json!({ "mcp_token": seat, "mcp_read_token": read }),
            })
            .await
            .expect("seed the live keys");
        tokio::spawn(async move {
            let _ = serve_listener(handle, listener, false, allowlist, seat, read).await;
        });
        addr
    }

    async fn loopback() -> SocketAddr {
        spawn(vec!["127.0.0.1".parse().expect("ip")], SEAT, READ).await
    }

    /// One HTTP/1.1 exchange, hand-written on purpose: a spec client in the
    /// dev graph would widen it past what the binary carries.
    async fn http(
        addr: SocketAddr,
        method: &str,
        extra_headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, HashMap<String, String>, String) {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let mut req = format!(
            "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
             Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n"
        );
        if !extra_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).await.expect("write");
        let mut raw = Vec::new();
        // a line-protocol answer never closes the socket: bound the wait
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut raw))
            .await
            .unwrap_or_else(|_| panic!("no HTTP reply within 5s, got {:?}", String::from_utf8_lossy(&raw)));
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no HTTP status line in {raw:?}"));
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        (status, headers, body.to_string())
    }

    fn bearer(token: &str) -> (&'static str, String) {
        ("Authorization", format!("Bearer {token}"))
    }

    async fn post(addr: SocketAddr, token: &str, msg: Value) -> (u16, HashMap<String, String>, Value) {
        let auth = bearer(token);
        let (status, headers, body) = http(addr, "POST", &[(auth.0, &auth.1)], &msg.to_string()).await;
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, headers, json)
    }

    fn initialize(version: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": version, "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" } } })
    }

    fn call(name: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": name, "arguments": {} } })
    }

    #[tokio::test]
    async fn http_initialize_with_bearer() {
        let addr = loopback().await;
        let (status, headers, body) = post(addr, SEAT, initialize("2025-03-26")).await;
        assert_eq!(status, 200, "{body}");
        assert!(headers["content-type"].starts_with("application/json"), "{headers:?}");
        assert_eq!(body["result"]["serverInfo"]["name"], "moltrepublic");
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert!(body["result"]["instructions"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn http_missing_bearer_is_401() {
        let addr = loopback().await;
        let (status, headers, body) = http(addr, "POST", &[], &initialize("2025-06-18").to_string()).await;
        assert_eq!(status, 401, "{body}");
        assert!(headers["www-authenticate"].starts_with("Bearer"), "{headers:?}");
        assert!(body.is_empty(), "{body}");
    }

    #[tokio::test]
    async fn http_wrong_bearer_is_401() {
        let addr = loopback().await;
        let (status, headers, _) = post(addr, "nope", initialize("2025-06-18")).await;
        assert_eq!(status, 401);
        assert!(headers["www-authenticate"].starts_with("Bearer"));
    }

    #[tokio::test]
    async fn http_empty_seat_token_admits_without_header() {
        let addr = spawn(vec!["127.0.0.1".parse().expect("ip")], "", "").await;
        let (status, _, body) = http(addr, "POST", &[], &initialize("2025-06-18").to_string()).await;
        assert_eq!(status, 200, "{body}");
    }

    #[tokio::test]
    async fn http_seat_token_calls_a_tool() {
        let addr = loopback().await;
        let (status, _, body) = post(addr, SEAT, call("read_session")).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["isError"], false, "{body}");
        assert_eq!(body["result"]["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn http_read_token_narrows_tools() {
        let addr = loopback().await;
        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let (status, _, body) = post(addr, READ, list).await;
        assert_eq!(status, 200, "{body}");
        let n = body["result"]["tools"].as_array().map(Vec::len).expect("tools");
        assert_eq!(n, tool_defs(Scope::Read).len());
        let (status, _, body) = post(addr, READ, call("chat_send")).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["error"]["code"], -32001, "{body}");
    }

    #[tokio::test]
    async fn http_notification_is_202() {
        let addr = loopback().await;
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let (status, _, body) = post(addr, SEAT, note).await;
        assert_eq!(status, 202);
        assert!(body.is_null(), "{body}");
    }

    #[tokio::test]
    async fn http_get_and_delete_are_405() {
        let addr = loopback().await;
        let auth = bearer(SEAT);
        for method in ["GET", "DELETE"] {
            let (status, _, _) = http(addr, method, &[(auth.0, &auth.1)], "").await;
            assert_eq!(status, 405, "{method}");
        }
    }

    #[tokio::test]
    async fn http_body_over_bound_is_413() {
        let addr = loopback().await;
        let auth = bearer(SEAT);
        let too_long = (MAX_RPC_LINE + 1).to_string();
        let (status, _, _) = http(
            addr,
            "POST",
            &[(auth.0, &auth.1), ("Content-Length", &too_long)],
            "{",
        )
        .await;
        assert_eq!(status, 413);
    }

    #[tokio::test]
    async fn http_foreign_host_is_403() {
        let addr = loopback().await;
        let auth = bearer(SEAT);
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let body = initialize("2025-06-18").to_string();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\
             Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n{}: {}\r\n\r\n{body}",
            body.len(),
            auth.0,
            auth.1
        );
        s.write_all(req.as_bytes()).await.expect("write");
        let mut raw = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_string(&mut raw)).await;
        assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");
    }

    #[tokio::test]
    async fn http_peer_off_the_allowlist_gets_nothing() {
        let addr = spawn(vec![], SEAT, READ).await;
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let body = initialize("2025-06-18").to_string();
        let _ = s
            .write_all(format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await;
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw).await;
        assert!(raw.is_empty(), "{}", String::from_utf8_lossy(&raw));
    }

    #[tokio::test]
    async fn same_port_still_serves_line_json() {
        let addr = loopback().await;
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                           "params": { "token": SEAT, "protocolVersion": "2025-03-26" } });
        s.write_all(format!("{line}\n").as_bytes()).await.expect("write");
        let mut buf = vec![0u8; 65536];
        let n = s.read(&mut buf).await.expect("read");
        let reply: Value = serde_json::from_slice(&buf[..n]).expect("one JSON line");
        assert_eq!(reply["result"]["serverInfo"]["name"], "moltrepublic");
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
    }

    /// Claude Code 2.1.270 opens with `server/discover` and takes the newest
    /// version listed; under 2026-07-28 it then refuses a `tools/list`
    /// without `ttlMs`/`cacheScope`, which rmcp 3.3 does not emit.
    #[tokio::test]
    async fn http_discover_offers_nothing_newer_than_the_served_shape() {
        let addr = loopback().await;
        let discover = json!({ "jsonrpc": "2.0", "id": "probe", "method": "server/discover",
            "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } } });
        let auth = bearer(SEAT);
        let (status, _, body) = http(
            addr,
            "POST",
            &[
                (auth.0, &auth.1),
                ("MCP-Protocol-Version", "2026-07-28"),
                ("Mcp-Method", "server/discover"),
            ],
            &discover.to_string(),
        )
        .await;
        // rmcp refuses the newer version outright and names what it serves
        assert_eq!(status, 400, "{body}");
        let body: Value = serde_json::from_str(&body).expect("json");
        let offered = body["error"]["data"]["supported"].as_array().expect("versions");
        assert_eq!(offered.last().and_then(Value::as_str), Some("2025-11-25"), "{body}");
        let (_, _, body) = post(addr, SEAT, initialize("2026-07-28")).await;
        assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");
    }

    #[tokio::test]
    async fn line_loop_answers_the_newest_version_to_an_unknown_one() {
        let addr = loopback().await;
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                           "params": { "token": SEAT, "protocolVersion": "1999-01-01" } });
        s.write_all(format!("{line}\n").as_bytes()).await.expect("write");
        let mut buf = vec![0u8; 65536];
        let n = s.read(&mut buf).await.expect("read");
        let reply: Value = serde_json::from_slice(&buf[..n]).expect("one JSON line");
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    }
}
