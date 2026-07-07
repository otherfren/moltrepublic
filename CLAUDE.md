# Working in moltrepublic

MoltRepublic is a real product (not a demo): a Rust workspace for founding and
running small encrypted "republics"/DAOs over the SimpleX Messaging Protocol
(SMP), with MLS group encryption and a Slint GUI. Grow the UI/UX stepwise while
implementing the real thing behind the same contract — never fake behavior a
user could mistake for real.

## Working style

- **Proceed on greenlit multi-step work — don't keep asking "weiter?".** Once a
  task is agreed, carry it through; commit at meaningful checkpoints.
- **But for a genuine fork or ambiguity, ask EARLY rather than guess.** The cost
  of a wrong guess in the ritual/crypto/state-model is high; a quick question up
  front is cheaper than building the wrong thing. Only stop for real
  product/design decisions, not for choices with an obvious default.
- Don't invent specs — fetch the real thing (e.g. exact OpenMLS/SMP APIs) and
  lock it against the compiler before building on it.

## Architecture (read the workspace `Cargo.toml` header first)

Strict crate layering: a lower crate never depends on a higher one, and there is
**one** command set (`molt_core::Command`) executed in **one** place
(`molt-engine`); every frontend (MCP, GUI) drives that same set as a co-equal
operator. `molt-core` holds the Command/Event/Surface contract and has **no
I/O**. Order: core → config → storage → net → engine → mcp → ui → app.

The engine is a single-owner actor: command handlers are synchronous and never
await I/O. Off-actor work (SMP round-trips, the join ritual) runs in spawned
tasks that feed results back as engine-internal `Net*`/`Command` variants — the
same pattern as the tickers.

## Conventions that will trip you up

- **clippy is kept at 0, including tests.** `unwrap_used = "warn"` applies to all
  targets — use `.expect("…")` in tests, never `.unwrap()`. `cargo clippy
  --all-targets` must be clean before you commit.
- **Co-equality is enforced by a test.** Every `Command` variant must be either
  an MCP tool (`crates/molt-mcp/src/lib.rs::tools()`) or on the documented
  `INTERNAL` list in that file — `co_equality_every_command_is_a_tool_or_documented_internal`
  fails otherwise. Adding a `Command` means updating one of those. Network/ritual
  tasks speaking *to* the engine are INTERNAL (an MCP agent must not be able to
  forge a peer/ritual member); human decisions (approve, propose, confirm) are
  tools on **both** surfaces.
- **`WorkspaceEvent::Founded`, `SealedRoster`, and `roster_canonical_bytes`
  ripple widely.** Adding a field touches ~15 sites, many of them test harnesses
  that recompute the signed table. `roster_canonical_bytes` is versioned
  (`molt-roster-v2`) — bump the tag if you change the byte layout, and update
  every recompute site (founder canonical, `verify_sealed_roster`,
  `verify_seal_proposal`, the tests) together or signatures silently break.
- **Additive-only event evolution.** New `WorkspaceEvent` fields get
  `#[serde(default)]`; an older reader meeting an unknown variant must not write.
- **Drain the outbound path, don't `abort()` it.** In the mesh/bootstrap async
  plumbing a node finishes as soon as its *inbound* work is done, but its own
  last outbound frame may still be in the `channel → encrypt task → send task →
  wire` pipeline. Aborting those tasks on completion silently drops that frame
  and the peer waits forever (an intermittent, load-dependent deadlock). Let the
  upstream drop its sender so the task ends by itself, then `.await` it (only
  `abort()` the inbound reader). See `bootstrap_over_mls` / `member_bootstrap` /
  `founder_bootstrap`.

## The founding ritual is the security-critical core

Read `documents/founding_ritual.md` before touching `founding.rs`/`lifecycles.rs`.
Load-bearing invariants — do not weaken them:

- **Sign-what-you-see.** A member recomputes the canonical table from the
  proposal it is shown (`verify_seal_proposal`) and signs *that* — never an
  opaque blob the founder supplied. It also checks its own `(name, key)` is in
  the roster and that the republic id is the content-derived value.
- **One identity, two anchors.** A member's derived Ed25519 key is both its
  roster identity and its MLS credential key. The founder enforces this at join
  (`molt_net::mls::key_package_binding` — the KeyPackage's credential must equal
  the anchored `name` and its signature key the MAC-bound `identity_pk`).
- **Tamper-evident charter.** The deliberated name+agenda are bound into the
  signed bytes and the genesis; `verify_sealed_roster` recomputes over them, so a
  founder cannot seal a charter different from what everyone ratified.
- The ritual is **ephemeral** (no disk write before the final seal; crash/cancel
  leaves no trace) and **one-shot** for `CreatePropose` (re-proposing would
  corrupt collected signatures — cancel and re-mint to change the charter).

## MLS / OpenMLS reference (crates/molt-net/src/mls.rs)

Pure-Rust only (no C toolchain — same posture as the pure-Rust TLS/Ed448). Facts
that cost time to (re)discover:

- Version pairing (they version independently): `openmls 0.8.1`,
  `openmls_traits 0.5.0`, `openmls_rust_crypto 0.5.1`,
  `openmls_basic_credential 0.5.0`, `tls_codec 0.4.2`. Ciphersuite
  `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (matches our Ed25519 + X25519).
- `SignatureKeyPair::from_raw(ED25519, seed, pub)` wants the **32-byte Ed25519
  seed** (what `ed25519_dalek::SigningKey::to_bytes()` returns) — NOT a 64-byte
  expanded key, NOT seed‖pub.
- Persist the provider's storage by bincode-serializing its public byte-keyed
  `values` map — **not** JSON (JSON object keys can't be `Vec<u8>`). Reload with
  `MlsGroup::load(storage, &group_id)`; the signer round-trips via
  `SignatureKeyPair::read`.

## Build, test, run

- `cargo build` builds the whole workspace including the Slint GUI (slow first
  build). Headless vs GUI is a runtime choice, not a separate build.
- Network tests (real SMP server) are `#[ignore]`d; the founding+join+MLS flow is
  proven fast over loopback in `crates/molt-engine/tests/two_instances.rs` and
  end-to-end over real SMP in `ritual_engine_over_smp.rs` (`-- --ignored`).
- **Never launch a GUI window on `DISPLAY=:0`** — that is the user's own X
  session. There is no headless display here, so GUI changes are validated by the
  Slint compiler (a clean `cargo build -p molt-ui`) plus the engine-level tests,
  not by pixels.
