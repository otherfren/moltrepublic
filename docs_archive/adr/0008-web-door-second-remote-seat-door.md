# ADR-0008: the web door - a second remote door onto the seat, loopback only

Status: ACCEPTED 2026-11-17 (user decisions D1 and D2 of `docs/web_gui.md`).
Builds on ADR-0007.

## Decision

`moltd` gets a second remote door beside MCP: a WebSocket that carries the
serde contract (`Command`, `Reply`, `Event`, `MoltError`) directly, for the
browser build of the GUI. It is off unless `[web] port` is set, binds
loopback and nothing else, and is opened by the same live seat key as
MCP. Remote use goes through an SSH tunnel. A non-loopback bind or TLS
needs its own ADR first.

Loopback is both addresses, `127.0.0.1` and `[::1]` (2026-11-19, review
of PR #9): the door answers `Host: localhost`, and a browser asked for
`localhost` may try either, so a door on one of them leaves the other to
whoever binds it - a page that is not the node's asking for the seat
key. A `[::1]` port already held by another process opens no door at
all; a host without IPv6 has one loopback and binds one.

## Why not MCP

MCP is poll-only, its replies are reshaped text and not `Reply`, and three
commands the window issues are not tools. Making it the GUI's wire would
mean a second hand-kept mapping of ~94 commands and an invented push
channel. The contract already serializes; the window's whole engine seam
is `execute` + `subscribe`.

## What the door is allowed to be

- **Scope is ADR-0007.** A browser tab holding the seat token operates
  the machine: host posture, any path, the recovery phrase, the wake
  command. The door adds no reach over MCP and removes none. The seat
  token is the boundary; the loopback bind and the Origin/Host check only
  limit who can present it.
- **Fail-closed gate.** A command executes only if it is an MCP tool's
  command or on the named `WEB_DOOR` list (`molt-remote/src/gate.rs`).
  Everything that speaks AS someone else - `net_*`, `as_operator`,
  `ui_publish` - stays shut, pinned by test.
- **The browser is the hostile neighbour.** WebSockets are outside CORS,
  so `Host` must be a canonical loopback name (the DNS-rebinding guard)
  and `Origin` must be the page this listener served (the cross-site
  hijack guard). The token rides the first frame, never a URL, header or
  cookie, and the page keeps it in memory only.
- **Bytes, never paths (2026-11-18).** File content crosses as chunked
  bytes through its own commands - an upload into an engine-owned
  per-workspace store, reads by share id, by a view's picture key, or of
  the last export. No byte door takes a path, so none adds reach over
  the path commands; each is bounded (256 KiB a chunk, 64 MiB a transfer,
  1 GiB a store) and an upload belongs to the session that began it
  (`mcp-security.md`, "The byte doors"). The store deletes only a
  directory it minted itself: ownership is never read off a client's
  path, and `share_file` refuses a path under the store.
- **A push channel dies with its key.** The key is re-checked on every
  inbound frame, before every reply, before any event that can follow
  a rotation, and between the chunks of a transfer already on its way.
- **A slow reader is served, a dead one dropped (2026-11-18).** No
  write has a total: a connection is dropped when it takes nothing for
  the stall time, so a tunnel at a few KB/s gets its files and replies
  whole (the door's own small send buffer keeps the timer that fine). The bundle goes out compressed and large frames packed, both
  by request only (`docs/web_gui.md` §5.6).
- **The bundle is trusted like the binary.** The token is typed into the
  served page, so the bundle directory must be private (no symlink, no
  foreign owner, not group- or world-writable) or no port opens. `[web]`
  is start-time config only, on no remote surface.

## Consequences

- The engine holds uploaded files for a share's life: disk the node's
  user did not place there, bounded by the store's quota and removed
  with the share.
- Two doors to keep in step: a new `Command` must be classified for both
  (the co-equality test and the gate test go red until it is).
- The wire has no compatibility promise: the bundle is served by the same
  `moltd` and a version mismatch reloads the page.
- Cleartext on loopback, like MCP. Whoever can read loopback traffic on
  the host already is the node's user.
