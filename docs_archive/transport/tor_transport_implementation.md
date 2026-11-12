# T4 (Tor) — implementation plan (multi-agent, parallel)

> **Scope note (2026-07-30, etappe N-demo):** the SMP transport this plan
> targeted was removed in the Nostr transport replacement
> (`docs_archive/transport/nostr_transport_marmot.md`). The dialer machinery it built
> — the `Dialer` modes, socks5h, fail-closed resolve, session isolation,
> circuit prebuild, opt-in embedded arti — is KEPT and now lives at
> `crates/molt-net/src/dial.rs` (S3 backup dials through it today; N2 reuses
> it to dial WebSocket relays, onion-preferred). The SMP-target sections
> (`smp://` addressing, `SmpServer`/`SmpTransport` wiring, the SMP no-leak
> harness) are historical.

Status: **BUILT** (T4 complete 2026-07-11; the dialer lives in
`crates/molt-net/src/dial.rs`). ARCHIVED — the plan was executed. Was: plan,
not started. Executes T4 of
`concept-transport-simplex-tor.md` (§3.1 addressing, §4 modes, §5 circuit
prebuild, §6 fail-closed). Written to be handed to several coding agents in
parallel worktrees: it pins the design calls the code inventory forced, cuts
the work into packages with disjoint file ownership, and defines the stage
gates. Test-first throughout, per house rules.

Anchors below are from the inventory of master at commit `7263619`
(post nym-greying). Re-verify anchors before starting if the tree has moved.

The four user-facing modes (fixed):

| Mode | config | dial |
|------|--------|------|
| **off** | `anonymity.network = none` | direct TCP (as today) |
| **system Tor** | `network = tor`, `tor.mode = local`, `tor.port = 9050` | SOCKS5h → `127.0.0.1:9050` |
| **embedded** | `network = tor`, `tor.mode = embedded` | in-process **arti** (opt-in build) |
| **whonix** | `network = tor`, `tor.mode = whonix` | SOCKS5h → `10.152.152.10:9050` |

The four product decisions taken up front (2026-07-11, user):

1. **embedded-arti ships behind a Cargo feature** `embedded-tor`, default **off**
   — the reproducible core build stays lean; the UI greys the embedded row when
   built without the feature (same affordance as nym).
2. **Connection pooling is in scope** — persistent per-server connection reuse +
   circuit prebuild at workspace-open (concept §5). Fresh-connect-per-send over
   Tor is pathological and must go.
3. **`.onion` is a first-class alternate host, onion-preferred** — the address
   format gains an onion slot; when Tor is on and an onion host exists, dial it
   (no exit node, reaches onion-only servers).
4. **SMP is fail-closed; the Test button is fixed** — `network = tor` blocks
   every direct SMP dial (never a silent clearnet fallback); the Settings "Test
   connection" button routes through the resolved dialer. The two non-SMP egress
   paths (S3 backup HTTP, the MCP TCP listener) are a **documented follow-up**
   (§7 below), not in this pass.

---

## 0. The load-bearing finding

**T4 is mostly wiring, not a build-from-scratch.** The SOCKS5h dialer already
exists and is unit-tested — it is wired to nothing:

- `Dialer` enum + `dial()` — `crates/molt-net/src/smp/tls.rs:234-277` (the ONE
  place a TCP socket to an SMP server opens; single chokepoint).
- `socks5h_connect` (RFC 1928 + 1929, `ATYP_DOMAIN` = proxy-side DNS) —
  `crates/molt-net/src/socks5.rs:128-171`, per-host isolation token.
- `Dialer::from_config` — `tls.rs:252-262` — maps `local`/`whonix` to SOCKS,
  **everything else (incl. `embedded`, `""`) silently to `Direct`**.
- `SmpTransport::with_dialer` — `crates/molt-net/src/smp/transport.rs`.

The gap: **every transport is built with `SmpTransport::new` = `Dialer::Direct`**
(founding.rs:363, founding.rs:938, founding.rs:48, recovery.rs:724, plus
transport.rs:52-54), and the `anonymity.network`/`tor.mode` settings **never
reach `molt-net`**. Enabling Tor = (a) make config reach the dialer, (b) flip
those construction sites, (c) fail closed instead of the permissive `_ =>
Direct`, (d) add pooling, onion addressing, timeouts, arti, and the health pill.

---

## 1. Design pins (fixed — an agent that disagrees stops and escalates)

### P1 — one routing decision, collapsed into `Dialer::resolve`, fail-closed

Replace the permissive `Dialer::from_config` with:

```rust
pub fn resolve(network: &str, mode: &str, port: u16) -> Result<Dialer, NetError>
```

- `network == "none"` → `Ok(Dialer::Direct)`.
- `network == "tor"`, `mode == "local"`  → `Ok(Socks5{ "127.0.0.1:{port}" })`.
- `network == "tor"`, `mode == "whonix"` → `Ok(Socks5{ "10.152.152.10:9050" })`.
- `network == "tor"`, `mode == "embedded"`:
  - with feature `embedded-tor` → `Ok(Dialer::Arti(handle))`.
  - without the feature → `Err(NetError::TorMisconfigured("embedded Tor not built …"))`.
- `network == "tor"`, unknown mode → `Err(TorMisconfigured)`.
- `network == "nym"` → `Err(TorMisconfigured("nym not implemented"))` (defence
  in depth; the UI already greys it).

There is **no code path where `network == "tor"` yields `Direct`.** The old
permissive `_ => Direct` is deleted. This is the whole fail-closed guarantee,
concentrated in one function with an exhaustive unit test.

### P2 — the address gains an onion slot; onion-preferred when Tor is on

SimpleX advertises the same server as clearnet **and** `.onion` under one
fingerprint (`documents/SimpleX.txt`: identical fp, two hosts). The wire syntax
is comma-separated hosts after `@`:

```
smp://<fp>@<host>[:port][,<host2>[:port2]…]
```

- `SmpServer` gains `alt_hosts: Vec<HostPort>` (additive; primary stays `host`/
  `port`). Parse splits on `,` — first is primary (clearnet by convention), any
  `.onion` among them is the onion alt. `server.rs:30-67`.
- `SmpServer::dial_target(tor_on: bool) -> (&str host, u16 port)`: when `tor_on`
  and an `.onion` host exists → that host; else the primary. The **SNI/pin is
  unchanged** — cert verification ignores the hostname (fingerprint pin,
  `tls.rs:100`), so onion and clearnet share one fingerprint cleanly.
- `Direct` never dials `.onion` (no resolver) — `dial_target(false)` returns the
  primary. An **onion-only** server (no clearnet host) with `network=none` is a
  clean config error, not a hang.
- `SmpServer::addr()`/display must round-trip every host so config save/load and
  the genesis-embedded server survive (`server.rs:79`, config `smp_url`).

### P3 — embedded-arti behind `embedded-tor`, and the UI knows at compile time

- New Cargo feature `embedded-tor` in `molt-net` (optional deps
  `arti-client`, `tor-rtcompat`), default off. Default workspace build and the
  reproducible-build envelope are **unchanged** unless the feature is on.
- A `Dialer::Arti(ArtiHandle)` variant exists **only** under
  `#[cfg(feature = "embedded-tor")]`; `resolve` errors without it (P1).
- The UI must grey the embedded row when built without the feature. Surface the
  compile-time truth as a bound bool `embedded_tor_available` (set from
  `cfg!(feature = "embedded-tor")` at the molt-app→molt-ui seam) and feed it into
  the tor-mode dropdown's `enabled` array (the `AppDropdown.enabled` affordance
  landed with nym-greying, `components.slint`). nym stays greyed regardless.

### P4 — connection pooling + circuit prebuild; per-server isolation token

- `SmpTransport` keeps a pool: **one live `SmpConn` per server**, reused across
  `create_queue`/`send`/`subscribe`/`delete_queue` instead of fresh-connect-per-op
  (`transport.rs:140,158,181,218`). On a broken connection, reconnect lazily.
  Subscribe keeps its own long-lived connection as today.
- **Circuit prebuild** at workspace-open: dial (and TLS-handshake) all member
  servers in parallel, bounded by a semaphore of 4 (concept §5), so first-send is
  one round-trip, not a cold circuit build. The hook is the supervisor
  spawn/mesh-open path (`supervisor.rs`).
- **Isolation token** becomes per-server with a random component minted once per
  session (`molt-<random>-<host>`, concept §4/§5) — replaces the deterministic
  `molt-{host}` at `tls.rs:272`. Same server host reuses one circuit (pooling);
  different servers get different circuits. With arti, an `IsolationToken` per
  server.

### P5 — deadlines make "Tor down" a clean state, not a hang

The SMP path has **no** connect/handshake/read/write timeout today
(`conn.rs:378-413`, `tls.rs:267,292`). Add `tokio::time::timeout` wrappers,
sized for Tor:

- connect (incl. SOCKS negotiation / circuit build): **30 s**.
- TLS handshake: **20 s**.
- per read/write block: **30 s**.

A timeout → `NetError::Unreachable`/`TorUnavailable`, surfaced as the pill (P6),
never an infinite await. Keep the existing generous ritual deadlines
(`MESH_BOOTSTRAP_TIMEOUT = 20 s` founding.rs:36; `RECOVERY_WELCOME_TIMEOUT`
recovery.rs:26) — with pooling (P4) the per-send cost drops, so 20 s stays
adequate; note it as a watch item for large n on cold circuits.

### P6 — the "chat" status pill is the real transport-health surface

Concept §4: "tor unreachable is a first-class state — the header's chat pill
goes amber/red with the reason." Today the pill tone is hardcoded from
`active-state` (`app.slint:2477`). Add:

- `SessionView` gains `net_health: NetHealth { tone, reason }` (molt-core;
  additive `#[serde(default)]`). Values e.g. `Ok | Degraded(reason) |
  Down(reason)`.
- The engine sets it from the last dial outcome per server (a `TorUnavailable`
  or `TorMisconfigured` from `resolve`/`dial` → `Down`/`Degraded` with the
  reason string). `net.rs` / the supervisor's error path.
- The pill reads `net_health` for tone + tooltip (`app.slint`, `parts.slint`
  `StatusPill`).

### P7 — the Test button routes through the resolved dialer

`test_connection` hardcodes `Dialer::Direct` (`tls.rs:309-313`) — over Tor it
both leaks (clearnet dial) and misreports (an onion target shows as failed).
Fix: `test_connection` takes the resolved `Dialer` (and `dial_target`), so the
probe uses the same routing the app will. For an onion-only target with
`network=none`, report "requires Tor", don't dial.

### P8 — the per-workspace `net` choice is GONE (cleaned up 2026-07-18)

Routing comes from the **global** settings, not per-workspace — and since the
mock_todo §2 cleanup the surfaces say so. `CreateStart` no longer carries a
`net` field (command + MCP tool param removed); the create screen's cosmetic
dropdown/tor-mode/port inputs were replaced by a read-only display of the
effective global network plus a "change it under Settings → Anonymity network" hint.
`CreateState.net` / `WorkspaceInfo.net` remain as pure display labels, now
derived from the live global settings at founding/join/recovery time and at
the startup scan (one shared normalizer, `molt_core::effective_net_label`:
`"tor"` or `"none"`, anything unknown reads as `"none"` because
`Dialer::resolve` fails it closed) — never from a client-supplied string.
The hardcoded `"tor"` labels on the join/recovery/restore paths and in
`ScanEntry::info` are gone with it, which also retires the `none`/`clearnet`
vocabulary drift for real entries (demo seed data may still carry legacy
values). If
per-workspace enforcement is ever wanted it is a separate design.

### P9 — DNS never resolves locally when Tor is on

SOCKS5**h** (`ATYP_DOMAIN`, `socks5.rs:59-63`) and arti resolve in-circuit. The
only local-resolution path is `Dialer::Direct` (`tls.rs:267`), reachable only
when `network=none` (P1). Audit: no `to_socket_addrs`/`lookup_host` on the SMP
path. (The S3 and MCP egress paths are §7 follow-up.)

---

## 2. Dependency truth & where the parallelism is

Everything depends on the **contract**: `SmpServer` shape (onion slot),
`Dialer::resolve` + `Dialer::Arti` hook, `NetError::{TorUnavailable,
TorMisconfigured}`, `SessionView.net_health`, and the config→dialer bridge.
Adding the onion field + flipping the 5 construction sites breaks/touches every
SMP dial site at once — splitting "contract" from "wiring" across agents just
serializes them with merge pain. So: **one serial Stage A** lands the contract
*and* the fail-closed wiring, leaving the workspace green (off = direct as
today; tor+local = actually SOCKS; embedded/unknown = clean config error). Then
four packages run in parallel on disjoint files, then a serial integration.

```
A  contract + fail-closed wiring + timeouts       (serial, 1 agent)
├── B1 pooling + circuit prebuild + isolation      (molt-net transport/conn/supervisor)
├── B2 arti embedded dialer (feature-gated)         (molt-net new module + Cargo features)
├── B3 UI: grey embedded, health pill, test button  (molt-ui only)
└── B4 real-Tor test tier + egress no-leak harness   (tests + CI feature)
C  integration (merge B1→B2→B3→B4, gates, docs)     (serial, 1 agent)
```

### File-ownership matrix (Stage B — no file appears twice)

| File | A | B1 | B2 | B3 | B4 |
|------|---|----|----|----|----|
| `molt-net/src/smp/server.rs` (onion slot, dial_target) | ✍ | — | — | — | — |
| `molt-net/src/smp/tls.rs` (`resolve`, Arti hook, timeouts) | ✍ | — | — | — | — |
| `molt-net/src/socks5.rs` (isolation token param) | ✍ signature | — | — | — | — |
| `molt-net/src/errors` (`TorUnavailable`/`TorMisconfigured`) | ✍ | — | — | — | — |
| `molt-net/src/smp/transport.rs` (pooling) | — | ✍ | — | — | — |
| `molt-net/src/smp/conn.rs` (timeouts land in A; pool reuse) | ✍ timeouts | ✍ pool reuse | — | — | — |
| `molt-net/src/supervisor.rs` (prebuild hook) | — | ✍ | — | — | — |
| `molt-net/src/tor_embedded.rs` (NEW, feature-gated) | — | — | ✍ | — | — |
| `molt-net/Cargo.toml` (feature skeleton) | ✍ skeleton | — | ✍ deps | — | — |
| `molt-config/src/lib.rs` (onion round-trip) | ✍ | — | — | — | — |
| `molt-engine/src/session.rs` (`dialer_for`, net_health set) | ✍ | — | — | — | — |
| `molt-engine/src/{founding,recovery}.rs` (flip 5 sites) | ✍ | — | — | — | — |
| `molt-engine/src/net.rs` (health from dial outcome) | ✍ hook | — | — | — | — |
| `molt-core/src/lib.rs` (`net_health`, additive) | ✍ | — | — | — | — |
| `molt-ui/**` (grey embedded, pill, test button) | — | — | — | ✍ | — |
| `molt-*/tests`, CI (real-Tor + egress no-leak) | ✍ fix ignored | — | — | — | ✍ new tier |

`conn.rs` is the one shared file: **A lands the timeout wrappers**, B1 adds pool
reuse in a different region; if that seam feels risky, fold B1's `conn.rs` edits
forward into A and let B1 own only `transport.rs` + `supervisor.rs`.

---

## 3. Stage A — contract + fail-closed wiring (serial, 1 agent)

> **Status: LANDED 2026-07-11** (master). Default flipped to `none`;
> fail-closed `resolve`; onion slot; timeouts; `net_health`; `embedded-tor`
> feature skeleton (dep-less until B2). Adversarial review found one **critical**
> (verified, fixed): the 30 s block deadline was also killing the subscription
> idle long-poll — a node went deaf after 30 s of quiet (recovery / runtime
> delivery / late joins, clearnet included). Fixed: `recv_next`'s idle read is
> deadline-free (`read_block_waiting`); request/response reads keep the 30 s
> deadline. Pinned by the live-SMP `subscription_survives_idle_ed25519`.

**Goal:** the full new contract exists; the workspace builds (feature off),
clippy 0, all existing tests pass; **off** behaves exactly as today, **tor+local**
now actually dials SOCKS `127.0.0.1:9050`, **embedded/unknown** is a clean
`TorMisconfigured`, onion addresses parse and are onion-preferred under Tor.

**Failing tests first** (molt-net + molt-config + engine unit tests):
1. `resolve_maps_every_mode_and_fails_closed` — none→Direct; tor+local→socks 9050;
   tor+whonix→socks gateway; tor+embedded (no feature)→Err; tor+unknown→Err;
   nym→Err. **No input yields Direct under network=tor.**
2. `smp_server_parses_comma_hosts_and_prefers_onion_under_tor` — `smp://fp@clear,onion`
   parses; `dial_target(true)` = onion, `dial_target(false)` = clearnet; round-trips
   through display + config `smp_url`.
3. `direct_never_targets_onion` — onion-only server + `dial_target(false)` selects
   nothing dialable → the caller surfaces a clean error, not a hang.
4. `dial_timeout_is_bounded` — a black-holed connect returns `Unreachable` within
   the deadline (use a non-routable addr / a proxy that never answers).
5. `config_round_trips_onion_and_tor_mode` (molt-config) — onion host in `smp_url`
   and every `tor_mode` survive render→parse→salvage.

**Work list** (anchors):
- `molt-net/src/smp/server.rs`: `alt_hosts: Vec<HostPort>` + comma parse
  (`:30-67`); `dial_target(tor_on)`; `addr()`/display round-trip (`:79`).
- `molt-net/src/smp/tls.rs`: delete `from_config` permissive body, add
  `resolve` (P1); `dial()` uses `dial_target` + the random isolation token
  (`:265-277`); `connect_tls`/`dial` wrapped in `tokio::time::timeout` (P5);
  declare the `Dialer::Arti` variant behind `cfg(feature="embedded-tor")` with a
  `#[cfg(not)]` `resolve` arm returning `TorMisconfigured` (B2 fills the impl).
- `molt-net/src/socks5.rs`: isolation-token parameter already there; take the
  session-random token (signature stable).
- `molt-net/src/errors`: `TorUnavailable(String)`, `TorMisconfigured(String)`.
- `molt-net/Cargo.toml`: `[features] embedded-tor = ["dep:arti-client",
  "dep:tor-rtcompat"]`, deps `optional = true` (B2 pins versions).
- `molt-net/src/smp/conn.rs`: timeout wrappers on `read_block`/`write_block`/
  handshake (`:378-413`).
- `molt-config/src/lib.rs`: `smp_url` onion round-trip (`:114-135`, render `:438`,
  salvage `:522`, apply `:694`).
- `molt-core/src/lib.rs`: `SessionView.net_health: NetHealth` additive
  (`#[serde(default)]`); `NetHealth` enum/struct documented.
- `molt-engine/src/session.rs`: `fn dialer_for(&self) -> Result<Dialer, NetError>`
  from `settings.{anonymity, tor_mode, tor_port}` (the bridge, P1); set
  `net_health` from `resolve` errors at boot (restart-required already marks the
  three keys, `session.rs:222-230`).
- `molt-engine/src/{founding.rs:363,938,48, recovery.rs:724}` + `transport.rs`
  construction: `SmpTransport::new` → `with_dialer(server, dialer_for()?)`. A
  `TorMisconfigured` here aborts the flow with the reason (fail-closed).
- `molt-engine/src/net.rs`: a hook that turns a dial error into `net_health`
  (B4-independent minimal version; the pill rendering is B3).
- **Ignored real-SMP tests**: `ritual_engine_over_smp.rs`, `ritual_over_smp.rs`,
  `smp_transport.rs`, `smp_live.rs` set `anonymity = "none"` explicitly (they
  target clearnet servers), or they now fail closed. Do this in A so the suite
  stays green.

**Gate:** `cargo clippy --all-targets` 0; `cargo test` green (feature off);
`cargo build -p molt-ui` green. Commit; tag for Stage-B branch-off.

**Migration risk to flag at the Stage-A review:** the **default config is
`tor + local`** (`molt-config` `AnonymityNetwork::Tor`, `TorMode::Local`). After
this flip, a fresh install with **no Tor running fails closed** — correct for a
privacy product, but a behavior change from today's silent clearnet. Decide at
review whether the shipped default should be `none` (preserve today's UX) or
`tor+local` (privacy-by-default) with a first-run Tor check. **This is a product
call — surface it, don't silently pick.**

---

## 4. Stage B — four parallel packages

> **Status: LANDED 2026-07-11** (master). B1 pooling (one connection/circuit per
> server, reused across ops; prebuild under a Semaphore of 4), B3 UI (health pill
> ← `net_health`; embedded greyed unless built `--features embedded-tor`), B4
> tests (deterministic `tor_no_leak.rs` proving zero direct egress via a
> blackhole SOCKS proxy + `#[ignore]`d `tor_e2e.rs`). B2 arti: FIRST attempt
> correctly blocked (arti forces `libsqlite3-sys`, a C dep); after the user
> accepted that C dep for the **opt-in** `embedded-tor` feature only (2026-07-11),
> B2 landed — `DialStream` enum unifies `TcpStream`/arti `DataStream` through one
> TLS handshake, lazy bootstrap, one `TorClient` reused, per-server
> `IsolationToken`. Default build stays pure-of-arti (feature off). Review found
> one minor (fixed): B1's blanket retry could double-allocate a queue — non-
> idempotent ops now retry only to heal a stale reused connection. All gates
> green both feature ways; `resolve` is fail-closed (verified by `tor_no_leak`
> both ways). Remaining before "T4 done": Stage C polish (this section's items are
> all in), and the §7 follow-ups (S3-over-Tor, MCP hardening, the ring/x509
> cleanup) stay deferred.

### B1 — pooling + circuit prebuild + isolation (molt-net)
Failing tests first: a loopback/mock-proxy test that a second `send` reuses the
first connection (count dials); prebuild opens N connections under the
semaphore; a broken connection reconnects transparently. Work per P4. Files:
`transport.rs`, `supervisor.rs` (+ `conn.rs` pool region if not folded into A).

### B2 — arti embedded dialer, feature-gated (molt-net)
Failing tests first (only compiled under `--features embedded-tor`): arti
bootstraps to a state dir; `Dialer::Arti` dials an SMP server host; a per-server
`IsolationToken` yields distinct circuits. Work: new `tor_embedded.rs`
(`arti-client` bootstrap, `~/.moltrepublic/arti` state, `TorClient` reused,
isolation per server), the `#[cfg(feature)]` `Dialer::Arti` impl + the `resolve`
arm. Pin `arti-client`/`tor-rtcompat` versions in `Cargo.toml`; **confirm the
pure-Rust build with no C toolchain** before wiring (CLAUDE.md posture — if arti
pulls a C dep, STOP and report). Files: `tor_embedded.rs` (new), `Cargo.toml`
deps.

### B3 — UI (molt-ui only)
Compile gate `cargo build -p molt-ui` (no GUI) + Rust unit tests. Work per
P3/P6/P7: grey the embedded tor-mode row via `AppDropdown.enabled` when
`embedded_tor_available` is false (bind the cfg bool at the app→ui seam); the
"chat" pill tone+tooltip from `SessionView.net_health`; the Test button through
the resolved dialer with an onion-aware message. New strings via `lexicon!`
EN/DE. Files: `molt-ui/src/lib.rs`, `ui/{app,parts,components,theme}.slint`.

### B4 — real-Tor test tier + egress no-leak harness (tests + CI)
Concept §7. Work: an `#[ignore]`d real-Tor E2E reusing `ritual_engine_over_smp`
with `Dialer::resolve("tor","local",9050)` (a Tor daemon on 9050); a `net-tests`
feature gate; the **egress-firewall no-leak assertion** — run the SMP flow with
all direct (non-loopback) egress blocked and assert it still completes over Tor
and makes **zero** direct connections (the strongest T4 guarantee). Files: new
test files + CI wiring only.

---

## 5. Stage C — integration (serial, 1 agent)

1. Merge B1→B2→B3→B4 (rebase, `cargo clippy --all-targets` + `cargo test` each;
   fix seams, don't re-architect).
2. Build twice: default (feature off) and `--features embedded-tor` — both clippy
   0 + tests green. `cargo build -p molt-ui` both ways.
3. Manual-ish validation via the ignored tiers: system-Tor E2E (Tor on 9050),
   and — if a feature build is available — an embedded smoke test.
4. Docs: flip `concept-transport-simplex-tor.md` T4 status (in-progress →
   implemented, minus the §7 follow-ups); note the default-network decision from
   the Stage-A review; add a CLAUDE.md transport line if a new invariant emerged
   (e.g. "SMP is fail-closed under `network=tor`; the only local-DNS path is
   `Dialer::Direct`, reachable only when `network=none`").
5. `/code-review` on the full diff; fix findings; land on master.

---

## 6. Orchestration mechanics

- Stages are separate runs with a review between them (Stage A is the contract
  four packages amplify — it deserves a look, esp. the fail-closed default
  decision). Same pattern as `chat_bus_implementation.md`.
- Stage-B agents: one worktree each off the Stage-A commit, brief = this doc §4 +
  the file-ownership row + pins §1. Warm a shared `CARGO_TARGET_DIR` before
  fan-out (Slint + OpenMLS + rustls first build is slow; arti adds more).
- Every agent runs `cargo clippy --all-targets` + relevant `cargo test -p …`
  before done; C runs the full suite both feature ways.
- **Work on master** per CLAUDE.md — branches/worktrees are short-lived agent
  tooling, merged back before reporting done.

## 7. Explicit out-of-scope (documented follow-up, NOT this pass)

- **S3 backup + restore over Tor.** The backup/restore HTTP path
  (`molt-storage` `s3_*`) dials clearnet directly — a leak when Tor is on.
  Follow-up: let the user set an **onion endpoint** and toggle Tor for backups
  *and* restores, routing S3 through the dialer (or blocking it under
  `network=tor`). **The UI controls already exist, greyed** (2026-07-11): a
  `DisabledTorRow` placeholder (dimmed "route over Tor (onion endpoint)" + a
  dead onion field, tagged "not yet") sits in both the Settings→Backup S3
  section and the Restore→S3 way (`components.slint DisabledTorRow`,
  `app.slint`). Wiring it = a storage-side dialer + those controls going live.
  Until then, **backups/restores bypass Tor** — a real deanonymization surface;
  the greyed control signals it is deliberately not yet available.
- **MCP listener hardening.** `TcpListener::bind` (`molt-mcp/src/lib.rs:57`) is
  inbound; ensure it binds loopback-only under `network=tor` (or always). Its
  own concept is `mcp-security.md`.
- **nym** — greyed; a separate transport.
- **Per-workspace `net` enforcement** (P8) — the cosmetic per-workspace choice
  was removed 2026-07-18 (see §P8); only a real per-workspace transport design
  remains open, if ever wanted.
- **arti circuit tuning** (guard/bridge config, pluggable transports).

## 8. Risks & watch items

- **Fail-closed default** (§3) — the one product decision that changes existing
  behavior; must be decided at the Stage-A review, not silently.
- **arti = pure-Rust?** — verify no C toolchain creeps in via `arti-client`
  (CLAUDE.md). If it does, embedded stays greyed and B2 is deferred.
- **`conn.rs` shared seam** between A (timeouts) and B1 (pooling) — fold forward
  if risky.
- **Reproducible-build envelope** — the `embedded-tor` feature must not perturb
  the default build's dependency graph (verify `Cargo.lock` for the default
  feature set is unchanged but for the added optional entries).
- **20 s mesh-bootstrap over cold circuits at large n** — watch; pooling +
  prebuild should keep it inside the deadline, but a real-Tor run at n≈13 is the
  proof.
- **Onion SNI** — cert pin ignores the hostname, so onion+clearnet share one
  fingerprint; confirm rustls accepts an `.onion` `ServerName` (it is a valid DNS
  name syntactically).

## 9. Embedded client recovery: rebuild and reset (BUILT 2026-11-12)

Field report: a node on the embedded client ran for a day, then no dial
succeeded, and a restart did not help. The client used to be created once
per process and its state under `~/.moltrepublic/arti` never left the disk.
Two remedies, deliberately different in what they keep.

### 9.0 The bootstrap outlives the dial that started it (2026-11-12)

The likely field failure. The bootstrap used to run INSIDE the first dial,
and every caller that can reach the embedded client puts a deadline around
that dial:

| Caller (`molt-net`) | Deadline |
|---|---|
| relay subscription connect and reconnect (`relay_runtime.rs`, `CONNECT_TIMEOUT`) | 20 s |
| publish, single and pooled (`PUBLISH_TIMEOUT`) | 10 s |
| relay probe (`PROBE_PHASE_TIMEOUT`; NIP-11 `PROBE_NIP11_TIMEOUT`) | 10 s; 3 s |
| Tor test (`tor_probe.rs`, `CIRCUIT_TIMEOUT`) | 45 s |
| S3, purse daemon | none around the dial |

A deadline dropped the dial, the dial dropped `create_bootstrapped`, and
the next dial began again. A warm directory bootstraps well inside 20 s; a
stale one (the node had run for a day) has to be reloaded and does not, so
no bootstrap ever finished - and a restart changed nothing, because the
stale cache is on disk. A SOCKS daemon bootstraps on its own clock, which
is why switching to it worked.

Now the bootstrap runs in a task of its own. The slot holds it while it
runs; every dial waits on that one task, and a dial that gives up leaves
it running (`event=waiter_left`, debug). One bootstrap at a time; a failed
one empties the slot, so the next dial starts a fresh one; a reset
interrupts it. The rebuild below is the same task with the old client's
drain in front.

### 9.1 Automatic rebuild - keeps everything on disk

`crates/molt-net/src/tor_health.rs` (pure, in the default build) decides,
`tor_embedded.rs` acts. The evidence is one classification per dial:

| Outcome | Reads as |
|---|---|
| stream opened; or arti's `ErrorKind` says the far end answered (`RemoteConnectionRefused`, `RemoteHostNotFound`, `OnionServiceNotFound`, `OnionServiceNotRunning`, ...) | **alive** - clears the record and the backoff |
| `TorNetworkTimeout`, `TransientFailure`, `CircuitCollapse`, `CircuitRefused`, `RelayTooBusy`, `NoExit`, a dial its caller dropped | **ambiguous** - one dead onion service looks the same |
| every other kind (`TorAccessFailed`, `TorDirectoryUnusable`, `DirectoryExpired`, `NoPath`, `LocalNetworkError`, state and cache errors, ...), a failed bootstrap | **client** |

An address arti refuses to dial (`InvalidStreamTarget`,
`ForbiddenStreamTarget`, ...) is no evidence either way.

The client is unusable when, for 10 minutes without one alive outcome,
either

- arti's own `bootstrap_status()` is not `ready_for_traffic()` or reports a
  `blocked()` reason (sampled at each failed dial), or
- at least 3 dials failed and they are not all ambiguous failures to ONE
  host.

One dead relay therefore never triggers it: a dead destination is either
"alive" by its error kind, or ambiguous on a single host, or interleaved
with successes elsewhere. A TLS refusal happens above the dialer, on an
opened stream.

Then the client is dropped and a new one bootstraps from the SAME state and
cache directories: entry guards, circuit timeouts and the directory cache
stay. The first rebuild comes after 10 minutes; the next ones no sooner
than 20, 40, then 60 minutes after the previous. Any alive outcome resets
that. One rebuild at a time; dials arriving meanwhile wait for it. A dial in flight keeps the client it started
on (the swap waits up to 30 s for them, so two clients do not contend for
the state lock); its outcome is not counted against the new client.
Isolation tokens survive a rebuild.

### 9.2 Manual reset - removes state and cache

`Command::NetResetTor` (MCP `net_reset_tor`, seat scope; Settings ›
Anonymity network, shown for the embedded mode) drops the client and
deletes

- `~/.moltrepublic/arti/state` - `state/guards.json`,
  `state/circuit_timeouts.json`, `state/state.lock`, the (unused) `keystore`
- `~/.moltrepublic/arti/cache` - `dir.sqlite3`, `dir_blobs/`, `dir.lock`

and forgets the isolation tokens. The next dial bootstraps from scratch. A
bootstrap in flight is interrupted. Refused, with nothing removed, unless
the saved route is `tor` / `embedded`, and on a build without the feature.

The directory is shared by every `moltd` of the user, so the reset removes
it for all of them; a second running process carries on with the files it
holds open.

### 9.3 Why only the manual path removes the guards

A Tor client keeps a small, long-lived guard set so that an adversary who
runs some relays is picked as the first hop rarely, not eventually. A
client that picks fresh guards whenever something looks stuck re-rolls that
dice on every hiccup - and an adversary able to cause hiccups could force
the re-roll. So the automatic path never deletes `guards.json`; giving the
guards up is a decision a human (or the seat's operator) takes knowingly.

### 9.4 Log lines

One line each, `tor=embedded event=...`: `bootstrap_start`, `bootstrap_ok`
/ `bootstrap_failed` (`ms`, `error`), `waiter_left` (debug), `health_bad`
(`reason=not_ready | all_dials_failing`, once per bad spell),
`rebuild_start` / `rebuild_ok` / `rebuild_failed` (`count`, `reason`, `ms`),
`reset` (`removed`).
