# Working in moltrepublic

MoltRepublic is a real product (not a demo): a Rust workspace for founding and
running small encrypted "republics"/DAOs over the SimpleX Messaging Protocol
(SMP), with MLS group encryption and a Slint GUI. Grow the UI/UX stepwise while
implementing the real thing behind the same contract — never fake behavior a
user could mistake for real.

## Working style

- **Never point at a secret in a tracked artifact.** Naming or describing a
  secret in anything committed — a `.gitignore` comment, a commit message, a
  code comment, a doc — leaks it: it advertises that the secret exists and what
  it is, even when the value itself is elsewhere. Ignore secrets via a **generic
  path with no explanatory comment** (e.g. a bare `/.secrets/` line); keep any
  commit that touches secret handling **generic** (never name the file or its
  contents). When removing a secret from history, the rewrite's own commits and
  messages must not describe what was removed.
- **Work directly on master.** The user relies on the session's result being on
  master — other sessions build on it there. Branches/worktrees are short-lived
  tooling only (e.g. isolating parallel agents); merge back to master and delete
  them BEFORE reporting done, and never leave the deliverable on a side branch.
  If a plan document prescribes a branch workflow, this rule wins — raise the
  contradiction with the user instead of silently following the document.
- **Plan first, then act.** For anything non-trivial, lay out the plan (and for
  a security-critical flow, the design doc) before writing code — don't code your
  way into the design.
- **Test-first (TDD).** Write the failing test(s) that pin the behavior, watch
  them fail for the right reason, then implement until they pass. New behavior
  starts as a test, not as code-then-a-test-after. (Applies especially to the
  ritual/crypto/chain — the invariants belong in a red test first.)
- **Proceed on greenlit multi-step work — don't keep asking "weiter?".** Once a
  task is agreed, carry it through; commit at meaningful checkpoints.
- **Code-review every finished change-set, then land it green on master.**
  After finishing a chunk of work, run a code review over the diff (and fix
  the findings) BEFORE merging; the end state of a session is always
  tested-green on master — never unreviewed, never on a side branch.
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
- **Chat addressing is by `MessageId` — never reintroduce indices.** Every chat
  message carries a random 128-bit id minted by the sender's engine
  (`chat::mint_message_id`; core stays RNG-free); reactions/deletes/quotes/file
  ops address by id, which is why they can cross the wire and converge. Legacy
  (pre-id) log entries get deterministic synthetic ids at the two ingest choke
  points (`events.rs`: `apply`'s Chat arm + `restore_dump` — the
  `molt-chat-legacy-id` sha256 formula; both must stay identical or the
  determinism keystones break). The id→position map is `State.chat_pos`
  (runtime-only, never persisted). Channels (`ChannelRef`) are *views, never
  boundaries*: exactly one per message, filtering is engine-side on `ReadState`
  (never client-side — co-equality), tags carry no governance meaning, and the
  chat surface's byte-identity fixtures in `molt-core` mod tests pin the legacy
  wire shape — treat a red one as a design stop. Read `documents/chat_bus.md`
  before touching chat/channel code.
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

## The persistent-change chain is the shared state model

Read `documents/persistent_chain.md` before touching `chain.rs` (in `molt-core`
and `molt-engine`). The republic's persistent state is a **single-branch,
threshold-signed commit-block chain** ("git patches"); the founding is block 0.
It is the state-model twin of the founding ritual — load-bearing invariants:

- **Chain persistent changes only; chat + deliberation stay ephemeral.** Blocks
  are the founding, gated `Applied` transitions, and `Membership` changes. Chat,
  reactions, and the propose/approve gossip are *flüchtig* — never blocks. The
  boundary: ephemeral by default; content becomes a block only when a gated,
  threshold-approved change makes it durable republic knowledge.
- **The genesis reuses the roster bytes.** Block 0's `sigs` *are* the founding
  attestations and its `approval_bytes` *is* `roster_canonical_bytes` — do not
  fork a second signing path for it. Genesis is n-of-n; later blocks are m-of-n.
- **Signatures are position-bound.** Members sign over `republic_id ‖ height ‖
  change`, so a block cannot move/reorder/splice without re-signing. A "re-base"
  onto a contended slot means a **new height → the members re-sign**. `prev` is a
  structural hash-link `verify_chain` checks, not part of the member signature.
- **`verify_chain` is hard-reject, all-or-nothing.** Bad sig, broken `prev`,
  height gap, below-threshold, repeated/unknown signer, double-applied proposal,
  forged genesis id → the whole chain is rejected. A partially-trusted prefix
  could fork state, so there is no soft path. Deterministic convergence demands it.
- **Versioned byte layouts, like the roster.** `approval_bytes` /
  `block_link_bytes` carry `molt-chain-change-v1` / `molt-chain-block-v1` — bump
  the tag and update `verify_chain` together if you change a layout, or
  signatures silently break. `ChainChange` is additive-only (`WorkspaceEvent`
  rule): an unknown variant must stop a reader from extending the chain.
- **Additive, not an `EventEnvelope` change.** The chain is a *separate*
  structure; the local ephemeral event log (chat + materialized blocks) is
  untouched. Phase 2 is wired: a chain-governed republic runs **real threshold
  governance over the mesh** (`cmd_approve` signs; the engine collects distinct
  signatures and seals a block at *m*, deterministically — the m lowest-named
  signers — then broadcasts it; `crosses_wire` carries `Proposed`/`Approved`/
  `Committed`). The legacy counted simulation is OFF for chain workspaces (guard
  on `is_chain_governed`), and `self.chain_applied` is a *separate* projection
  from the legacy `self.applied` so the two never collide (reads concat them).
  The identity signing key must reach `materialize_workspace` from the ritual
  (`ritual.founder_sk()` / `founding::member_identity`) — re-deriving it from the
  member handle gives the WRONG key (the ritual salts identity with a
  workspace-id string). Phases 3–4 (catch-up sync, recovery) are still open.

## Pure-Rust posture — aspirational, with two known C exceptions

The crypto stack aims for **pure-Rust, no C toolchain** (rustls-rustcrypto, the
pure-Rust Ed448/Ed25519, OpenMLS), and MLS/TLS/signatures hold to it. But the
claim is **not literally true of the current build** — two C dependencies exist:

- **`ring` (C + assembly, pulls `cc`) is in the DEFAULT build** via
  `x509-parser`'s `verify = ["ring"]` feature, used by the SMP cert-pin
  verifier (`crates/molt-net/src/smp/tls.rs`). So a C compiler is already
  required to build `molt-net` today. (Open follow-up: swap the leaf-cert verify
  for a rustcrypto path to remove it — not yet done.)
- **`libsqlite3-sys` (C) rides the OPT-IN `embedded-tor` feature** only: arti's
  `tor-dirmgr` depends on `rusqlite` non-optionally, and no arti feature removes
  it. Accepted (2026-07-11 decision) for that opt-in build; the **default build
  never pulls it** (the feature is off by default). See
  `crates/molt-net/Cargo.toml` `[features]`.

Keep new code pure-Rust where you can; these two are the sanctioned exceptions.

## MLS / OpenMLS reference (crates/molt-net/src/mls.rs)

Facts that cost time to (re)discover:

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

## Transport gotchas the loopback tests can't catch

`LoopbackTransport` is **permissive** — its queues live in the shared hub, so any
clone can subscribe to any queue. **SMP is not**: a queue's *receive* credential
(recipient key) lives in the creating `SmpTransport`'s `Arc<Mutex<SmpState>>`, so
only that instance (or a clone sharing the Arc) can `subscribe`. A *fresh*
`SmpTransport::new(server)` can **send** to a queue by id but never receive on
one it didn't create (`"subscribe to a queue this node did not create"`). So the
runtime supervisor must **reuse the ritual transport** that created the mesh
queues (founder: `runtime_transport`; joiner: the transport handed back through
`join_transport`) — a fresh transport from the mesh handover is wrong, and
loopback won't expose it. **Cross-session resume** works via a *clean-close
persist*: on close, the engine writes the advanced MLS snapshot + the transport's
serialized queue credentials (`Transport::export_creds`) into `transport.state`
(`persist_crypto_blocking` — a read-modify-write that preserves the delivery
cursors); on reopen, `reopen_transport` re-adopts the creds into a fresh
`SmpTransport` and `cmd_open_workspace` rebuilds the real mesh. Since the
2026-07-19 hardening the queue creds + MLS snapshot are ALSO merged live at
every mesh-up (founder mesh-ready, join-sealed, recover-sealed,
mesh-extension — the live `persist_mesh_crypto_blocking`, never the sealing
close-persist), so a hard kill after the mesh came up is survivable: reopen
resumes from the last-persisted ratchet, and a few in-flight messages may be
replay-rejected by the peer (MLS's per-message `reuse_guard` prevents the
worse nonce-reuse). A workspace with mesh evidence on disk that cannot
resume opens HONESTLY offline (`notice = "detached"`, `net_health = Down`),
never silently transport-less. **Sender keys are deterministic since the
2026-07-19 fix**: each queue's `SKEY` key is derived from a persisted
per-transport `sender_seed` (`derive_sender_key`, in `transport.state` from
mesh-up on; additive V2 creds with a V1 read-fallback), so a reopened
transport re-asserts the SAME key — and on an SKEY reject it still attempts
the signed SEND (the server's send verdict is authoritative). Stage B is in
(`documents/stage_b.md`): a died subscription resubscribes itself (per-peer
watchdog, capped backoff; a server `END` ends the stream instead of a zombie
wait), and `net_health` is honest — `Ok` means every mesh leg's subscription
confirmed, a dead inbound leg or a stuck outbox shows as `Degraded` naming
peer + reason. Per-drain MLS persist (full crash-safety of the ratchet) is
the remaining hardening: a hard-killed node's regressed-window messages are
replay-rejected (loudly) at the peers; the leg heals from the next message.

## Build, test, run

- `cargo build` builds the whole workspace including the Slint GUI (slow first
  build). Headless vs GUI is a runtime choice, not a separate build.
- **The Slint-generated window lives in its own crate, `molt-ui-window`, as a
  compile-time firewall** (2026-07-13): `ui/app.slint` compiles to a ~400k-line
  Rust module whose single rustc peaks at ~6 GiB / ~4 min — that cost is now
  paid ONLY when a `.slint` file changes. GUI-logic edits (`molt-ui`) rebuild
  in ~2 s at <1 GiB. Debuginfo reduction does NOT help (measured: ~2%), and a
  SIGKILL during the window compile is the kernel OOM-killer — never run two
  window-scale builds concurrently (e.g. an agent's test run next to the
  user's `cargo build`); when RAM is tight build with `-j 1`. GUI changes are
  validated by a clean `cargo build -p molt-ui-window` (Slint compiler) plus
  `-p molt-ui` (logic against the generated API).
- **GUI iteration goes through `scripts/dev-ui.sh` — NOT the 6-GiB build.**
  It sets `SLINT_LIVE_PREVIEW=1` + the `live-preview` feature chain
  (molt-app → molt-ui → molt-ui-window → `slint/live-preview`, Slint ≥ 1.13):
  slint-build then emits ~2.6k-line interpreter-backed stubs with the identical
  API instead of the ~400k-line module — measured 2026-07-14: a .slint edit
  recompiles in **~2 s at <1 GiB** instead of ~4 min / ~6 GiB. `dev-ui.sh run`
  starts moltd with runtime .slint hot-reload (properties/models/callbacks
  survive a save; an *incompatible* interface change panics the running app —
  restart it, still no recompile). The script uses its own `target/dev-ui`
  cache so the feature set never thrashes the normal build's cache (first fill
  is a one-time full-stack build, but RAM-light). Dev-only: never enable the
  feature by default in a Cargo.toml. The .slint compiler still runs fully, so
  `dev-ui.sh build` catches .slint errors and API breaks in molt-ui; the
  authoritative pre-commit check remains one normal
  `cargo build -p molt-ui-window -p molt-ui` (once per change-set, not per
  iteration).
- Network tests (real SMP server) are `#[ignore]`d; the founding+join+MLS flow is
  proven fast over loopback in `crates/molt-engine/tests/two_instances.rs` and
  end-to-end over real SMP in `ritual_engine_over_smp.rs` (`-- --ignored`).
- **Never launch a GUI window on `DISPLAY=:0`** — that is the user's own X
  session. There is no headless display here, so GUI changes are validated by the
  Slint compiler (a clean `cargo build -p molt-ui-window -p molt-ui`) plus the
  engine-level tests, not by pixels.
