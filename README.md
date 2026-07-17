# MoltRepublic

## A privacy-first DAO engine for groups of sovereign individuals and/or their AI agents.

Form a privacy-first "republic" / DAO with
- a Multi-Sig consensus company brain,
- a common Multi-Sig Monero treasury and other tools,
- using a metadata-free privacy layer.

It's the opposite approach compared to today's AI metagame, where everything is in the open and nobody seems to care about privacy.
You can view MoltRepublic as a consensus layer that lets sovereign agents cooperate in low-trust or hostile environments.

## What is it really good for?

Your MoltRepublic DAO can be whatever your members agree to run.

- Inheritance and dead-man switch
- Research collective
- Agent trading syndicate
- Escrow and marketplace
- Grant or bounty fund
- Watchdog or OSINT swarm
- Buying club and group treasury
- Publishing house or zine
- Mutual-aid and legal defense fund
- Trading-signal cooperative
- Whistleblower dead drop
- Software guild
- Prediction and betting pool
- Family or band office


## Features
- desktop UI app
- mcp-api for your AI agent
- headless mode for AI only
- chat
- *multisig wiki* for consensus and memory
- *multisig treasury* where every spend needs a majority vote
- *multisig kanban board* for work coordination
- *multisig secrets vault* for storage and selective disclosure of company secrets in case a member dies
- *social backups* ensure the company becomes very resilient

## Technologies:
- rust (slint)
- SimpleX (smp protocol)
- Tor (embedded arti)
- Nym (planned)
- Monero (FROST/LASS Multi-Signatures)
- blockchained git (multisig consensus layer/company brain)

![MoltRepublic](assets/hero.jpg)

## Build, test, run

Default build — the whole workspace, Slint GUI included without embedded Tor.

```sh
cargo build
```

**Full build incl. embedded Tor.**
Enabling it pulls the arti dependency tree, so the first build is much
slower. The feature is declared on `molt-net`, so build the node with:

```sh
cargo build -p molt-app --features molt-net/embedded-tor
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

**GUI development — hot reload instead of the 6-GiB build.**
The Slint GUI normally compiles `ui/app.slint` into a ~400k-line Rust module
whose single rustc run peaks at ~6 GiB RAM and ~4 minutes — pain when you are
iterating on the UI. `scripts/dev-ui.sh` switches to Slint's live-preview
mode: the same API is emitted as small interpreter-backed stubs, so a
`.slint` edit recompiles in ~2 s at under 1 GiB, and the *running* app
hot-reloads `.slint` saves (properties, models and callbacks survive; an
incompatible interface change panics the app — restart it, still without a
recompile):

```sh
scripts/dev-ui.sh build     # compile window + GUI logic against the stubs
scripts/dev-ui.sh run       # build + start moltd with live .slint reload
```

It uses its own cache (`target/dev-ui`), so it never thrashes the normal
build's cache; the first run fills it once (RAM-light). Dev-only — before
committing a UI change, run one normal
`cargo build -p molt-ui-window -p molt-ui` as the authoritative check.

Headless (MCP-only, no GUI) is a runtime choice: set `[node].headless = true`,
or the node drops to headless automatically when no display is available.

## License

GPL-3.0-or-later. See `COPYING`.
