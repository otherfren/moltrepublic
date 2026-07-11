# MoltRepublic

MoltRepublic is a real product, not a demo: a Rust workspace for founding and
running small, encrypted "republics" / DAOs over the SimpleX Messaging Protocol
(SMP), with MLS group encryption and a Slint GUI. Persistent state changes only
at an m-of-n threshold. There is **one command set**, executed in **one place**,
driven by **two co-equal operators** — a human at the GUI and an agent over MCP.

## The load-bearing invariant

One command set (`molt_core::Command`) is executed in one place (`molt-engine`,
a single-owner actor). Every frontend — the Slint GUI and the MCP interface — is
a thin shell that builds those same commands and observes the same event stream;
neither operator can do anything the other cannot. Co-equality is enforced by a
test in `molt-mcp` (every `Command` is either an MCP tool or a documented
internal). `molt-core` holds the Command/Event/Surface contract and has no I/O.

## Crates

Strict layering — a lower crate never depends on a higher one (order matches the
workspace `Cargo.toml`):

- `molt-core` — domain types, the Command/Event/Surface contract, errors. No I/O.
- `molt-config` — `config.toml` schema, render, salvage, format-preserving write.
- `molt-storage` — encrypted append-only workspace storage: event log, snapshots, sealed keys.
- `molt-net` — the transport: SMP-style queues behind a `Transport` trait, with MLS group encryption of the live traffic.
- `molt-engine` — the single-owner engine actor: one owning task; operators hold a cloneable handle and exchange Commands/Events.
- `molt-mcp` — the MCP frontend (headless operator; stdio or TCP), one tool per command.
- `molt-ui` — the Slint GUI frontend (the live-mirror operator).
- `molt-app` — the node binary `moltd`: UI mode (GUI + MCP) or headless (MCP-only).

## What's real

The founding ritual, threshold chain governance, member recovery, the chat bus
(channels as filters over one broadcast stream), and the SMP + MLS transport are
implemented and tested. Tor routing is **in progress (T4)**: the config surface
and UI controls exist, with the embedded (arti) mode behind an opt-in Cargo
feature — see the build notes below and `documents/tor_transport_implementation.md`.

## Build, test, run

Default build — the whole workspace, Slint GUI included. The first build is slow
(Slint + OpenMLS + rustls compile from source):

```sh
cargo build
```

**Full build incl. embedded Tor.** The in-process, pure-Rust arti Tor proxy is
opt-in behind the `embedded-tor` Cargo feature, so the default build stays lean
and reproducible without it. You only need the feature for the **embedded** Tor
mode; the `off`, system-Tor (`local`, SOCKS on `:9050`), and `whonix` modes do
not. Enabling it pulls the arti dependency tree, so the first build is much
slower. The feature is declared on `molt-net`, so build the node with:

```sh
cargo build -p molt-app --features molt-net/embedded-tor
```

Note: on this branch the `embedded-tor` feature is not yet wired — T4 is planned
(`documents/tor_transport_implementation.md`) and it lands with Stage A. Until
then `cargo build` is the full build.

Clippy is kept at 0, tests included (`unwrap_used = "warn"` across all targets —
use `.expect("…")` in tests, never `.unwrap()`):

```sh
cargo clippy --all-targets
```

Test — the network / Tor tiers hit a real SMP server and are `#[ignore]`d; the
fast suite proves founding + join + MLS over loopback:

```sh
cargo test                  # fast suite (loopback)
cargo test -- --ignored     # live-SMP / Tor tiers (need a reachable SMP server)
```

Run — `moltd` is the only binary, so a bare `cargo run` resolves to it. It needs
a `config.toml`; generate one first:

```sh
cargo run -- --generate-config ./config.toml
cargo run                   # UI mode: GUI + MCP over TCP (127.0.0.1)
```

Headless (MCP-only, no GUI) is a runtime choice: set `[node].headless = true`,
or the node drops to headless automatically when no display is available. Dev
caveat: never launch a GUI on the user's own `DISPLAY` — GUI changes are
validated by a clean `cargo build -p molt-ui` plus the engine tests, not by
pixels.

## Design docs

The real design lives in `documents/` — start with `founding_ritual.md`,
`persistent_chain.md`, `recovery_ritual.md`, `chat_bus.md`, and
`tor_transport_implementation.md`.

## License

GPL-3.0-or-later. See `COPYING`.
