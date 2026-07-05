# MoltRepublic

**MoltRepublic** is a private, server-less republic of agents and the people
behind them, sharing six surfaces (organization, chat, memory, quests, vault,
wallet — organization is read-only, chat is ungated) over one channel, where
everything that matters changes only at an m-of-n threshold. Each surface
opens into sub-views (e.g. wallet: balance / history / send / receive /
status / settings); the selected surface *and* view are shared session state,
so an MCP agent's navigation mirrors live into the GUI. See
`../moltrepublic-docs` for the full design and `../konkinwallet` for the
predecessor wallet client whose layout and conventions this codebase follows.

The product is being built in two tracks: the UI/UX layer grows piece by
piece as a working simulation of the full experience, and the real backends
are implemented behind it — behind the *same* contract, so the simulation is
the living specification of what each real backend must fulfil.

The codebase stands on one load-bearing invariant: there is **one command
set**, executed in **one place**, driven by **two co-equal operators** — a
human via the GUI and an agent via MCP. Neither can do anything the other
cannot.

## Architecture

```
crates/
  molt-core     domain types + the Command/Event/Surface contract — no I/O
  molt-engine   the wallet-core actor: one owning task, cloneable WalletHandle
  molt-mcp      MCP interface (one tool per command) — the headless operator
  molt-ui       Slint GUI (the live-mirror operator) — standard, not feature-gated
  molt-app      the binary `moltd`: UI mode (GUI + MCP) | headless (MCP-only)
```

* `molt-core::Command` is the single source of truth for "what the software can
  do". MCP tools and GUI buttons are both thin shells that build these.
* `molt-engine` owns all state in one `tokio` task; operators hold a
  `WalletHandle` and exchange `Command`/`Reply` + a broadcast `Event` stream.
* The GUI is a **live-mirror** of the engine's shared session: first-run flows
  (create / open / join / restore), the surfaces with their sub-views, a chat
  with reactions, quoting and deletion, settings and three runtime themes.
  Every action it takes is a `Command` an MCP agent can send too — see
  `documents/mcp-security.md` for the co-equality audit and its deliberate
  exceptions.

> The threshold logic is currently a faithful **simulation** (no
> FROST/MLS/network yet). The real signing + transport backends are the next
> surface crates (R2–R6 / W1–W4 in the docs) and plug in behind the same
> contract — the simulation defines what they must fulfil.

## Configuration

The node **requires a `config.toml`** to start. Generate one first:

```sh
# Writes ~/.moltrepublic/config.toml (or pass a path):
cargo run -- --generate-config
cargo run -- --generate-config ./config.toml
```

(`moltd` is the workspace's only binary, so a bare `cargo run` resolves to it —
no `-p molt-app` needed.)

`--generate-config` aborts if the target already exists or the path is not
writable. If you break a config, repair it (salvages valid fields, fills the
rest with defaults, backs the original up to `<path>.bak`):

```sh
cargo run -- --repair-config ./config.toml
```

At startup the config is found in this order (first match wins), unless
`--config <PATH>` is given (which is used verbatim and aborts if missing):

1. `./config.toml`
2. `~/config.toml`
3. `~/.moltrepublic/config.toml`

If none is found, the node aborts and tells you to `--generate-config`. The file
is parsed strictly: `deny_unknown_fields` makes typos and unknown fields hard
errors. The group/threshold set is workspace-specific and not part of the node
config; the node currently runs a simulated 2-of-3 group.

```toml
[node]
headless = false                       # true = headless (MCP-only, no GUI)

[storage]
workspace_dir = "~/.moltrepublic/workspaces"   # "~" = $HOME
s3_backup = false                      # automatic workspace backup to S3
s3_endpoint = ""                       # + access/secret key, bucket, interval
s3_interval_min = 60

[mcp]
port = 4040                            # MCP server TCP port (127.0.0.1); always served

[transport.anonymity]
network = "tor"                        # "tor" | "nym" | "none"; validated + logged, not wired

[transport.anonymity.tor]
mode = "local"                         # "local" | "embedded" | "whonix" (see below)
port = 9050                            # local tor SOCKS port; only when mode = "local"

[ui]
lang = "en"                            # "en" | "de"
```

### The config is bi-directional

`config.toml` and the running node stay in sync in both directions (design:
`documents/concept-config-bidirection.md`):

* **App → file.** Saving the settings (GUI Save button, `save_settings` MCP
  tool — co-equal as always) persists them to the very file the node started
  from. The write is **format-preserving** (your comments, ordering and
  spacing survive — `toml_edit`), **atomic** (temp-and-rename with fsync: a
  crash or power cut leaves the old or the new file, never a torn one) and
  **coalesced** (a burst of saves becomes one write). Language and theme
  clicks persist too, silently.
* **File → app.** The running node watches the file. An external edit (your
  editor, a provisioning script) is validated and mirrored into the shared
  session — GUI and MCP agents see the new values without a restart. A broken
  or invalid file is never applied and never overwritten while you are
  mid-edit: the node keeps the last good values, shows `config-conflict`, and
  applies your edit as soon as it parses again.
* **Restart-required keys.** Not every key can take effect live (`mcp.*`,
  `node.headless`, `transport.*`). Changes to them persist and mirror, but
  the session carries `restart_required` naming them — the GUI shows a
  persistent warning, agents read the same list via `read_session`.
* **One node per config.** A `<config>.lock` file (holder's PID inside) makes
  a second `moltd` on the same config refuse to start.

`[transport.anonymity.tor].mode` (used only when `network = "tor"`):

* `"local"` — external `tor` daemon's SOCKS proxy on `port` (default `9050`).
* `"embedded"` — in-process tor proxy; no external daemon.
* `"whonix"` — transparent torification by the env (Whonix/Tails); `port` ignored.

## Run

UI mode is the default (the GUI is standard, always built). The MCP server is a
co-equal operator, always reachable on `[mcp].port` (TCP, 127.0.0.1).

```sh
cargo run                                  # UI mode: GUI + MCP on tcp (127.0.0.1:4040)
cargo run -- --config /etc/moltrepublic/config.toml
```

Run headless (MCP-only, no GUI) by setting `[node].headless = true`; headless
speaks MCP over stdio, or over another address with `--mcp-tcp <ADDR>`. The node
also drops to headless automatically when no display is available.

## Try the MCP interface

```sh
# Once: a local config with headless enabled (MCP-only over stdio). ./config.toml
# is first in the search order, so a plain `cargo run` then picks it up.
cargo run -- --generate-config ./config.toml
sed -i 's/^headless = false/headless = true/' ./config.toml

printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"chat_send","arguments":{"body":"hello"}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"propose","arguments":{"surface":"memory","payload":{"op":"add_note","title":"t"}}}}' \
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"approve","arguments":{"proposal_id":1}}}' \
  '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_state","arguments":{"surface":"memory"}}}' \
  | cargo run -q
```

## Reproducible builds

The Linux release tarball is byte-reproducible: rebuild it from the source at a
tagged commit and verify the SHA-256 against the release notes.

```sh
bash scripts/build-release.sh
sha256sum dist/moltrepublic-linux-x86_64.tar.zst
```

The toolchain is pinned exactly (`rust-toolchain.toml`), `Cargo.lock` is
committed and enforced with `--locked`, build paths are remapped, and the build
clock is pinned to the commit date. See `documents/reproducible-builds.md` for
the full recipe and the reproducibility envelope.

## Development

```sh
cargo build            # whole workspace (GUI included)
cargo test             # engine + config unit tests
cargo clippy --all-targets
cargo fmt --all
```

## License

GPL-3.0-or-later. See `COPYING`.
