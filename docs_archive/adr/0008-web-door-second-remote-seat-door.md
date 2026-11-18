# ADR-0008: the web door - a second remote door onto the seat, loopback only

Status: ACCEPTED 2026-11-17 (user decisions D1 and D2 of `docs/web_gui.md`).
Builds on ADR-0007.

## Decision

`moltd` gets a second remote door beside MCP: a WebSocket that carries the
serde contract (`Command`, `Reply`, `Event`, `MoltError`) directly, for the
browser build of the GUI. It is off unless `[web] port` is set, binds
`127.0.0.1` and nothing else, and is opened by the same live seat key as
MCP. Remote use goes through an SSH tunnel. A non-loopback bind or TLS
needs its own ADR first.

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
- **A push channel dies with its key.** The key is re-checked on every
  inbound frame, before every reply, and before any event that can follow
  a rotation.
- **The bundle is trusted like the binary.** The token is typed into the
  served page, so the bundle directory must be private (no symlink, no
  foreign owner, not group- or world-writable) or no port opens. `[web]`
  is start-time config only, on no remote surface.

## Consequences

- Two doors to keep in step: a new `Command` must be classified for both
  (the co-equality test and the gate test go red until it is).
- The wire has no compatibility promise: the bundle is served by the same
  `moltd` and a version mismatch reloads the page.
- Cleartext on loopback, like MCP. Whoever can read loopback traffic on
  the host already is the node's user.
