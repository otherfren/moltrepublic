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
* a seat-scope tool called with the read-only key → `unauthorized: read-only token`.

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
  keep working, but the next `initialize` must use the new value. The accept loop
  reads the running session's token per connection, so no restart is involved.
* An **empty** `token = ""` disables token auth (the node logs a warning on start).
  The peer-IP allowlist still applies, so loopback-only + empty token is a common,
  reasonable local setup.

## The read-only key

A SECOND key, `[mcp].read_token`, admits the READ tools only
(`docs/memory/knowledge_base_scale.md` §4.7). It is issued, rotated and revoked
in the same panel as the seat key, and it is host posture like every other
setting there — an agent can neither issue itself one nor read one back.

* The read set is the WIKI and the SHARED FILES, nothing else (product
  decision 2026-09-04): `wiki_list`, `wiki_get`, `wiki_search`, `wiki_links`,
  `wiki_neighbors`, `read_uploads`. A tool is seat-scope unless that list
  names it, and a test pins the list, so a new tool cannot drift into the
  read scope by omission.
* **`read_state` is deliberately NOT in it.** A chat read sends this seat's
  read receipts to the republic (retrieval is the reading), so admitting it
  would have made the seat-scoping of `mark_read` decoration - the boundary
  would have been bypassable by reading. The wiki tools serve the same
  content without that side effect, which is what they exist for.
* `tools/list` shows a read-only client only its own scope; a seat tool called
  with the read key answers `-32001 unauthorized: read-only token`.
* **Empty means OFF**, never "unauthenticated": an empty `read_token` matches
  no credential at all. The seat key keeps its own meaning — an empty one still
  admits everybody as the seat.
* The key is absent from a generated config and is written only once one is
  issued, so a config this build writes still opens on a build that predates it.
* The scope is **host-local**. The republic knows no roles and no rights; the
  human narrows their own tool (see the host boundary below).

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

## The host boundary: what an agent may NOT do (audit 2026-08-26)

"Agents are seats" licenses REPUBLIC actions — propose, approve, chat,
share, restore — not the operator's MACHINE, identity or secrets. The
tool catalogue enforces that line at the builder and the engine keeps the
stored values behind it:

* **Host posture is the GUI's / `config.toml`'s door** (`SetNodePosture`,
  INTERNAL): `headless`, `workspace_dir`, `download_dir`, `mcp_port`,
  `mcp_allow`, `mcp_token`, `mcp_read_token`, `anonymity`, `tor_mode`, `tor_port` and
  `poke_wake_command`. `patch_settings` refuses them, `save_settings`
  never carries them (the engine re-merges the stored values). An agent
  therefore cannot switch the operator's Tor off, bind the endpoint to
  `0.0.0.0`, rotate itself a token the human never learns, or repoint the
  workspace root.
* **Secrets never read back.** `settings.s3_secret_key`,
  `settings.mcp_token` and `settings.mcp_read_token` are not serialized
  (like the recovery phrase); the S3 secret is settable write-only through
  `patch_settings`.
* **The exchange folder is the agent's whole filesystem.** `download_file`
  writes into `download_dir` only (a bare name), `share_file` shares a
  bare name FROM it (`share_file_from_exchange`), `export_workspace` and
  `wiki_export` write INTO it (`export_workspace_archive`,
  `wiki_export_archive`). Any-path access — the file dialog — is the GUI's
  (`ShareFile`, `ExportWorkspace`, `WikiExport` are INTERNAL).
* **The MCP export is a knowledge archive.** It carries no recovery seed and
  is marked phrase-sealed: an import commits it sealed, so reading it needs
  the phrase the human holds. Blob + passphrase never becomes a seat.
* **Clearnet consent is a human decision.** `relay_confirm` refuses
  `accept_clearnet: true` and `relay_clearnet_session` refuses
  `unlock: true` over MCP; switching non-onion dialing OFF and confirming
  onion relays stay available.
* **Founding and joining complete on a GUI node.** The recovery phrase is
  shown in the wizard only and `confirm_seed_backup` needs it re-typed, so
  a headless MCP client cannot attest the backup on the human's behalf.

Still open (product): a ritual in flight is abandoned as a side effect of
`create_start` / `join_start` / `recover_start` / `open_workspace` from
any surface — a griefing primitive against the human's own in-flight
work; a `force` argument or an "in flight" refusal is the fix direction.

## Co-equality audit: what the GUI can do vs. what a bot can do

The rule is: **every state-changing action the GUI offers maps to one MCP
tool**, because both frontends build the same `molt_core::Command` on the same
engine handle. As of this audit the mapping is complete — chat (send incl.
quotes, react, delete), proposals (propose / approve / decline), navigation
(screen, surface, sub-view), language, theme, settings (the MCP tokens are
NOT among them: they are host posture, minted by the GUI and refused by both
settings verbs), workspaces (open / close /
delete), and the three engine-run lifecycles (restore / create / join with
their start / cancel / finish verbs). Reading is co-equal too: what the GUI
live-mirrors, an agent reads via `read_session`, `read_state`,
`list_proposals` and `status`.

**Two settings verbs, deliberately.** `save_settings` REPLACES every field
and requires every field; `patch_settings` changes only what it names and
merges against the running settings inside the engine. The split is a
security boundary, not ergonomics: a partial payload used to be filled from
`SessionSettings::default()`, and those defaults are not neutral —
`anonymity` defaults to `"none"` and `mcp_token` to empty, so an agent
adjusting a backup interval could take a Tor node onto clearnet and switch
this authentication off in the same call, and be told "ack". The merge has
to happen where the current values are, which is the engine; the frontend
cannot tell "omitted" from "set to the default" (H5, fixed 2026-08-07).

**Deliberate exceptions** — GUI affordances with no MCP tool, and why:

* **Quit** (the window's quit confirm). Ends the local *process*, not shared
  republic state. A network-reachable shutdown verb would let any tokened
  client kill the node under the person at the keyboard — that is a
  denial-of-service primitive, not an operator capability. Agents that own
  their node process (stdio mode) stop it by closing the transport.
* **Clipboard** (copy seed / invite / message / log, paste). The clipboard is
  a device of the GUI machine, not shared state. An agent already holds the
  same bytes from `read_session` / `read_state` — EXCEPT the recovery
  phrase, which no surface ever serializes (below); a clipboard tool would
  only let a remote client snoop on or overwrite the local user's clipboard.
* **View-local state**: list sort order on the Open screen, the collapsed
  sidebar, open modals, form drafts, hold-to-peek seed reveal. None of it
  exists in the engine; the data behind it is readable over MCP (e.g.
  `workspaces[].last_sync_min` for sorting) and the *effects* (e.g. actually
  starting a restore) go through the shared commands.
* **The recovery phrase** (`workspaces[].seed`, the wizards' `create.seed` /
  `join.seed`) is the one exception to "the same bytes": it is private and
  leaves the process on NO surface. Since 2026-08-26 the fields never
  serialize (`#[serde(skip_serializing)]`), so `read_session` — and every
  other wire form of the session — carries none of it; the GUI reads it
  in-process for its hold-to-peek and the wizard's write-it-down step. A
  headless operator gets the phrase from the wizard on a GUI node, or from
  the device (`keys/seed.sealed` + the device key), never from the MCP
  endpoint. Note that the endpoint is cleartext TCP: loopback or an SSH
  tunnel only.
* **Preview helpers**: the GUI's live invite parse and duplicate-name check
  are conveniences over the same rules the engine enforces — `join_start` /
  `create_start` re-validate and return the authoritative error to MCP
  clients.
* **Manual backup note** (Open screen). A pure display mock: confirming the
  modal records nothing in the engine and writes nothing anywhere, so there
  is nothing to expose. If it ever gains a real effect it becomes a command
  first.
* **Engine-internal commands** (`RestoreTick` / `CreateTick` / `JoinTick`,
  `NetDelivered` / `NetPeerSeen` / `NetSendFailed`, `ReloadSettings`,
  `ConfigNotice`). These exist in the command enum but are deliberately not
  MCP tools: ticks are the engine's own clock, and the `Net*` commands are
  the node's own transport supervisor speaking (concept-transport §2) —
  `NetDelivered` records events under *other members'* names, so exposing
  it would let any client impersonate a network peer, and the two health
  signals would let a client forge presence. (They replaced `ChatFrom`,
  the retired reply simulator, which was internal for the same reason.)
  `ReloadSettings` and `ConfigNotice` are the config watcher's mirror path
  (file → session): `ReloadSettings` as a tool would let a client bypass
  `save_settings` (and with it the persist path and its validation
  semantics), and `ConfigNotice` would let a client forge "saved" /
  "save-failed" toasts in the user's GUI. An agent that wants a reload edits
  via `save_settings`; an agent that wants the file re-read edits the file —
  the watcher picks it up.
