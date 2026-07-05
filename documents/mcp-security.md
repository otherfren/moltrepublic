# MCP endpoint security

MoltRepublic's MCP interface is a **co-equal operator**: anything an agent can do
over MCP, it does through the same command set the GUI drives. That makes the MCP
endpoint as powerful as the person at the keyboard, so the network-facing TCP
transport is gated two ways — a **peer-IP allowlist** and a **shared token**.

Both are configured in the `[mcp]` section of `config.toml`:

```toml
[mcp]
# MCP server TCP port. Always served (UI + headless).
port = 4040
# allow = which client IPs may connect over TCP:
#   "127.0.0.1" = loopback only (default)
#   "0.0.0.0"   = any address (careful — this exposes the node)
#   or a comma-separated allowlist, e.g. "127.0.0.1, 192.168.1.10"
# Connections from IPs not on the list are refused.
allow = "127.0.0.1"
# API key every MCP client must send in its initialize request.
token = "2d6f1183510ee92cb59ba355a3f1b502274df9bf708f1bce"
```

## What is enforced, and where

| Transport | Peer-IP allowlist | Token |
|-----------|-------------------|-------|
| **TCP** (`[mcp].port`, always on; also `--mcp-tcp`) | yes | yes |
| **stdio** (`moltd` headless, no `--mcp-tcp`) | n/a | no |

* **stdio is trusted.** The agent host spawns the process and owns its stdin/stdout,
  so there is nothing to authenticate — stdio skips both checks. This is the path
  to prefer for local development (see below).
* **TCP is gated.** On every incoming connection the server first checks the peer
  IP; if `allow` is not `0.0.0.0` and the peer is not on the list, the socket is
  closed immediately with no reply. A surviving connection must then call
  `initialize` with the correct `token` before any other method works.

### Bind address

`allow` also decides what the server binds:

* sole entry is loopback (`127.0.0.1`) → bind `127.0.0.1` (unreachable off-box).
* anything else (a real IP, a list, or `0.0.0.0`) → bind `0.0.0.0` and filter each
  connection by peer IP.

### Errors

Auth failures come back as JSON-RPC error `-32001`:

* wrong/missing token in `initialize` → `unauthorized: missing or invalid MCP token`.
* any method before a successful `initialize` → `unauthorized: call initialize with a valid token first`.

## The token

* A fresh random token is written into the config by `moltd --generate-config`,
  and the clear value is printed **once** to the terminal:

  ```
  MCP API token (shown once — clients send it as `initialize` params.token):
      2d6f1183510ee92cb59ba355a3f1b502274df9bf708f1bce
  ```

  After that it lives only in `config.toml`.
* **Rotate it** anytime from the GUI: *Settings → MCP → Rotieren*. That mints a new
  token, writes it to the config, and takes effect immediately — existing sessions
  keep working, but the next `initialize` must use the new value.
* An **empty** `token = ""` disables token auth (the node logs a warning on start).
  The peer-IP allowlist still applies, so loopback-only + empty token is a common,
  reasonable local setup.

## A TCP handshake, end to end

Newline-delimited JSON-RPC 2.0. Send the token in `initialize`, then use tools:

```jsonc
--> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"token":"<token from config>"}}
<-- {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"...","serverInfo":{"name":"moltrepublic",...}}}
--> {"jsonrpc":"2.0","id":2,"method":"tools/list"}
<-- {"jsonrpc":"2.0","id":2,"result":{"tools":[ ... ]}}
--> {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"chat_send","arguments":{"body":"hi"}}}
```

A 20-line client:

```python
import json, socket, tomllib

cfg = tomllib.load(open("config.toml", "rb"))
tok = cfg["mcp"]["token"]
port = cfg["mcp"]["port"]

s = socket.create_connection(("127.0.0.1", port))

def rpc(obj):
    s.sendall((json.dumps(obj) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        buf += s.recv(4096)
    return json.loads(buf)

print(rpc({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"token":tok}}))
print(rpc({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
```

## Developing and debugging against MCP

**Prefer stdio.** For iterating on an agent or tooling, run the node headless and
talk to it over stdio — no token, no ports, and the process exits cleanly on EOF:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | moltd --config ./config.toml   # with [node].headless = true
```

Most MCP client libraries can also *spawn* `moltd` themselves and speak stdio —
that is the least-friction integration and needs no secret.

**When you need TCP** (attaching to a running UI node, or a client that only speaks
TCP):

* Read the token straight from `config.toml` — don't retype it. The 20-line client
  above does exactly this.
* Keep `allow = "127.0.0.1"`. Loopback-only means only processes on your machine can
  reach it, so a leaked token off-box is still useless.
* For a throwaway local session you can set `token = ""` to skip the handshake
  entirely. Do this only on a loopback-bound dev node; never on anything that binds
  `0.0.0.0`.
* Debugging from another machine (e.g. an agent on a LAN host): add that host's IP
  to `allow` **and** hand it the token. Reach for `0.0.0.0` only when you truly
  cannot enumerate the clients, and understand it exposes the full command set to
  anyone who reaches the port with the token.
* After **Rotieren**, update whatever you pasted the old token into — old value stops
  working on the next `initialize`.

**Reading the logs.** On start the node prints what it is enforcing, which is the
fastest way to spot a misconfiguration:

```
INFO moltd: MCP server listening (co-equal operator, token-gated) mcp=127.0.0.1:4040 allow=127.0.0.1
WARN moltd: MCP token is empty — the TCP endpoint is unauthenticated; run `moltd --generate-config` or set [mcp].token
WARN molt_mcp: MCP connection refused: peer IP not on the allowlist peer=192.168.1.50:41022
```

If a client hangs or gets a connection reset with no JSON reply, check that
`WARN ... peer IP not on the allowlist` line first — the allowlist drops the socket
before the protocol ever starts.

## Co-equality audit: what the GUI can do vs. what a bot can do

The rule is: **every state-changing action the GUI offers maps to one MCP
tool**, because both frontends build the same `molt_core::Command` on the same
engine handle. As of this audit the mapping is complete — chat (send incl.
quotes, react, delete), proposals (propose / approve / decline), navigation
(screen, surface, sub-view), language, theme, settings (including rotating
the MCP token via `save_settings.mcp_token`; the GUI's Rotate button does
exactly that with a locally generated value), workspaces (open / close /
delete), and the three engine-run lifecycles (restore / create / join with
their start / cancel / finish verbs). Reading is co-equal too: what the GUI
live-mirrors, an agent reads via `read_session`, `read_state`,
`list_proposals` and `status`.

**Deliberate exceptions** — GUI affordances with no MCP tool, and why:

* **Quit** (the window's quit confirm). Ends the local *process*, not shared
  republic state. A network-reachable shutdown verb would let any tokened
  client kill the node under the person at the keyboard — that is a
  denial-of-service primitive, not an operator capability. Agents that own
  their node process (stdio mode) stop it by closing the transport.
* **Clipboard** (copy seed / invite / message / log, paste). The clipboard is
  a device of the GUI machine, not shared state. An agent already holds the
  same bytes from `read_session` / `read_state`; a clipboard tool would only
  let a remote client snoop on or overwrite the local user's clipboard.
* **View-local state**: list sort order on the Open screen, the collapsed
  sidebar, open modals, form drafts, hold-to-peek seed reveal. None of it
  exists in the engine; the data behind it is fully readable over MCP (e.g.
  `workspaces[].last_sync_min` for sorting) and the *effects* (e.g. actually
  starting a restore) go through the shared commands.
* **Preview helpers**: the GUI's live invite parse and duplicate-name check
  are conveniences over the same rules the engine enforces — `join_start` /
  `create_start` re-validate and return the authoritative error to MCP
  clients.
* **Manual backup note** (Open screen). A pure display mock: confirming the
  modal records nothing in the engine and writes nothing anywhere, so there
  is nothing to expose. If it ever gains a real effect it becomes a command
  first.
* **Engine-internal commands** (`RestoreTick` / `CreateTick` / `JoinTick`,
  `ChatFrom`). These exist in the command enum but are deliberately not MCP
  tools: ticks are the engine's own clock, and `ChatFrom` (the demo reply
  simulator) posts under *other members' names* — exposing it would let any
  client impersonate members.
