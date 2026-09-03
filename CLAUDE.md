# Working in moltrepublic

MoltRepublic is a real product (not a demo): a Rust workspace for founding and
running small encrypted "republics"/DAOs over Nostr relays (NIP-EE/Marmot — in
build, `docs_archive/transport/nostr_transport_marmot.md`; Nostr is the production
transport since N4/N5, loopback stays the test transport),
with MLS group encryption and a Slint GUI. Grow the UI/UX stepwise while
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
- **Never invent a time limit.** Nobody has given you a deadline. "I ran out of
  time" is not a real constraint — it is a decision to stop, dressed up as one.
  When a task list is agreed, work it to the end; if something genuinely cannot
  be finished (blocked, needs a decision, would be unsafe to guess), say THAT
  and name the blocker. The user being away is not a deadline either.
- **Code-review every finished change-set, then land it green on master.**
  After finishing a chunk of work, run a code review over the diff (and fix
  the findings) BEFORE merging; the end state of a session is always
  tested-green on master — never unreviewed, never on a side branch.
- **But for a genuine fork or ambiguity, ask EARLY rather than guess.** The cost
  of a wrong guess in the ritual/crypto/state-model is high; a quick question up
  front is cheaper than building the wrong thing. Only stop for real
  product/design decisions, not for choices with an obvious default.
- Don't invent specs — fetch the real thing (e.g. exact OpenMLS APIs) and
  lock it against the compiler before building on it.
- **Don't hand-roll what a battle-tested library already does.** Before
  building any non-trivial mechanism — a parser, a protocol layer, a crypto
  envelope, a transport, a retry/backoff engine — SEARCH FIRST: is there an
  established crate, or a reference implementation of this very protocol? Look
  it up, read its actual code and API, weigh it, and write the verdict into the
  plan. "We'll implement it ourselves" is a decision that must be *argued*
  (layering, dependency posture, licence, maintenance), never the default
  because it is the fastest thing to start.
  - This rule was written after two CRITICAL bugs shipped in a HAND-ROLLED URL
    host parser (a backslash and a `userinfo@` component made a clearnet host
    classify as `.onion`, defeating the entire privacy gate) — while `url`,
    the WHATWG-correct parser every real client uses, was ALREADY in the
    dependency tree via `nostr`. Hand-rolled parsers fail exactly where the
    real parser disagrees, and that disagreement is the exploit.
  - A design doc dismissing an obvious candidate in one line ("overlaps our
    engine entirely") is NOT research. If a reference implementation exists
    for the protocol being built (for Nostr/NIP-EE: the Marmot MDK,
    `docs_archive/transport/mdk_evaluation.md`), it must be read and evaluated in the
    plan, with the take/leave decision recorded per component.
- **Prefer asking and researching over starting.** During planning it is
  cheaper to look again, research, think, and ask a question than to begin
  coding the wrong thing. A plan that begins with "I'll build X" without a
  paragraph on what already exists is not finished.

## Where documents live

- **`docs/`** — open work ONLY (rule since 2026-08-16). If it is here, it is
  a plan (or part of one) that is not built yet. A doc leaves for the archive
  in the same change that finishes its last open item.
- **`docs_archive/`** — everything else: the current specifications of
  shipping behaviour (the "read first" authorities CLAUDE.md points at),
  ADRs, executed plans, superseded designs (everything SMP/mesh), and
  analysis a decision consumed. The status line at the top says which of
  these a document is — **trust a spec's status, and never build new work
  from a doc marked superseded or executed;** open work lives in `docs/`.

Status lines are load-bearing — a doc claiming "not yet built" for shipped
work costs a planning session. Correct the status in the same change that
lands the work.

After moving or renaming any document run **`python3
scripts/check-doc-refs.py`** (exit 0 = clean). Code comments cite doc paths
heavily, so a move silently rots them; the checker resolves every path and
every bare `*.md` mention, and its exception list names why each non-reference
is exempt.

## Architecture (read the workspace `Cargo.toml` header first)

Strict crate layering: a lower crate never depends on a higher one, and there is
**one** command set (`molt_core::Command`) executed in **one** place
(`molt-engine`); every frontend (MCP, GUI) drives that same set as a co-equal
operator. `molt-core` holds the Command/Event/Surface contract and has **no
I/O**. Order: core → config → storage → net → engine → mcp → ui → app.

The engine is a single-owner actor: command handlers are synchronous and never
await I/O. Off-actor work (transport round-trips, the join ritual) runs in spawned
tasks that feed results back as engine-internal `Net*`/`Command` variants — the
same pattern as the tickers.

## User-facing text: compact and to the point

**Every string a human reads — error messages, run-log lines, toasts, status
copy, log output — states the ONE important thing and stops.** No explanatory
prose around it, no re-teaching the concept, no repeating the fix once per
item. People tire of walls of text within seconds and then read nothing at
all, so a verbose message is not "more helpful", it is *less* read than a
short one.

Concretely:
- Name the fault, not the lecture: `not confirmed` beats `in this node's pool,
  but not confirmed — confirm it under Settings › X (a clearnet or local relay
  needs the exposure acknowledgement)`.
- One item per line, aligned and scannable, when there are several.
- The remedy appears ONCE — in the summary line — not attached to every item.
- Mention a config key or a settings path only where it is the actual missing
  piece, and then just the key.
- Diagnostics in logs are structured fields, not sentences
  (`relay=… via=… error=…`), so they stay greppable and one line long.

This is a REVIEW CRITERION: prose creeping back into a user-facing string is a
finding, the same as a bug.

**No em dash in UI or HTML.** In every string a human sees (Slint labels,
`molt-ui` strings, `landing_page/`) write a plain `-`, never `—` and never
`&mdash;`. Documents under `docs/` are unaffected.

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
  (v3 bound each member's `nostr_pk` third anchor (N1), v4 the ratified
  relay pool (R3), and `molt-roster-v5` the ratified FEATURE selection
  (`docs_archive/ritual/charter_features.md`) — **conditionally: `features: None`
  emits v4 bytes byte-identically**, which is what keeps live republics
  verifying) — bump the tag if you change the byte layout, and update
  every recompute site (founder canonical, `verify_sealed_roster`,
  `verify_seal_proposal`, the tests) together or signatures silently break.
  The same rule holds for its sibling layouts: `molt-republic-id-v2`
  (`molt_storage::republic_id` — le32-length-prefixed + entry-counted, so the
  preimage stays injective for arbitrary field content; never regress it to
  separators) and `molt-chain-checkpoint-v8`
  (`molt_core::checkpoint_canonical_bytes` — both identity tables hash all
  three anchors (v2), the ratified relay pool rides along (v3), the applied
  projection is SUMMARIZED rather than archived (v4, `applied_lww_slot`),
  the WORKING transport anchors ride along too (v5) — without them a cut
  strands every seat that had recovered — the relay LEDGER as well (v6,
  R3b): a cut must not forget who declared which relays — and the ratified
  founding feature set (v7, conditional like roster-v5: a state without one
  hashes as v6), and an applied group for a surface outside the frozen
  `Surface::CHECKPOINT_V7_SURFACES` (v8, conditional: `genesis_base` seeds
  only the frozen six, `fold_one` adds a later surface's group with its
  first entry, so a cut made before that surface existed keeps its bytes
  and its JSON shape; under v8 a presence byte precedes the feature run,
  the group count is explicit, and every group present is hashed, so a
  phantom group fails the signed hash). A new surface therefore only
  extends `Surface::ALL`, never the frozen set; its first applied block
  strands any seat on a build that predates it (`charter_features.md`
  D1)). Each has byte-pin tests that go red on an unbumped change.
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
  wire shape — treat a red one as a design stop. Read `docs_archive/chat/chat_bus.md`
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

Read `docs_archive/ritual/founding_ritual.md` before touching `founding.rs`/`lifecycles.rs`.
Load-bearing invariants — do not weaken them:

- **Sign-what-you-see.** A member recomputes the canonical table from the
  proposal it is shown (`verify_seal_proposal`) and signs *that* — never an
  opaque blob the founder supplied. It checks its own **three-anchor seat**
  `(name, identity_pk, nostr_pk)` is in the roster, that the republic id is the
  content-derived value, and that every OTHER seat's nostr anchor is valid,
  canonical and roster-unique (format+uniqueness are what everyone can verify
  for everyone). The check closes at the GENESIS: `run_ritual_member` compares
  the distributed sealed roster's canonical bytes against the exact bytes it
  ratified — a founder cannot seal a different (even fully self-consistent)
  table than the one everybody signed.
- **One identity, three anchors — bound differently, know the difference.** A
  member's derived Ed25519 key is both its roster identity and its MLS
  credential key; the founder enforces that pairing at join
  (`molt_net::mls::key_package_binding` — the KeyPackage's credential must equal
  the anchored `name` and its signature key the MAC-bound `identity_pk`), which
  also gives Ed25519 proof-of-possession. The **nostr transport anchor**
  (`nostr_pk`, ticket-salted secp256k1) is bound by invite MAC v2 + ingest
  validation (`molt_net::canonical_nostr_pk` at `cmd_net_join_requested` —
  normalize-or-reject, plus cross-seat uniqueness) + the member's own
  sign-what-you-see re-check. It is **NOT MLS-bound and has NO
  proof-of-possession** (the secp256k1 key signs nothing during the ritual) —
  never design N2+ code on the assumption that an anchored `nostr_pk` is a
  possessed key.
- **Tamper-evident charter.** The deliberated name+agenda are bound into the
  signed bytes and the genesis; `verify_sealed_roster` recomputes over them, so a
  founder cannot seal a charter different from what everyone ratified.
- The ritual is **ephemeral** (no disk write before the final seal; crash/cancel
  leaves no trace) and **one-shot** for `CreatePropose` (re-proposing would
  corrupt collected signatures — cancel and re-mint to change the charter).

## The persistent-change chain is the shared state model

Read `docs_archive/chain/persistent_chain.md` before touching `chain.rs` (in `molt-core`
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
  on `is_chain_governed`), and `self.chain.applied` is a *separate* projection
  from the legacy `self.applied` so the two never collide (reads concat them).
  The identity signing key must reach `materialize_workspace` from the ritual
  (`ritual.founder_sk()` / `founding::member_identity`) — re-deriving it from the
  member handle gives the WRONG key (the ritual salts identity with a
  workspace-id string). Phases 3–4 (catch-up sync, recovery) are still open.

## Pure-Rust posture — aspirational, with known C exceptions

The crypto stack aims for **pure-Rust, no C toolchain** (rustls-rustcrypto, the
pure-Rust Ed25519, OpenMLS), and MLS/TLS/signatures hold to it. Since etappe
N-demo (2026-07-30) the **DEFAULT build graph is ring-free**: the SMP cert-pin
verifier and its `x509-parser`-with-`ring` dependency were deleted with the SMP
transport. The sanctioned exceptions now:

- **`libsqlite3-sys` (C) rides the OPT-IN `embedded-tor` feature** only: arti's
  `tor-dirmgr` depends on `rusqlite` non-optionally, and no arti feature removes
  it. Accepted (2026-07-11 decision) for that opt-in build; the **default build
  never pulls it** (the feature is off by default). See
  `crates/molt-net/Cargo.toml` `[features]`.
- **C `secp256k1` arrives via rust-nostr** (ADR-0002 — deliberately the
  battle-tested C library, NOT k256). In the DEFAULT build since N1 promoted
  `nostr` into src/ for the identity work (`molt-net/src/nostr.rs`, the
  ticket-salted transport anchor); contained to molt-net — roster/chain
  signing stays pure-Rust Ed25519.

Standing guard: the default build must **not silently re-adopt `ring`** —
rust-nostr's relay pool (`nostr-relay-pool` → `async-wsocket`) hard-pins a
ring-flavored `tokio-rustls`; ADR-0005 (2026-07-31) decided against the pool
(N2 drives `tokio-tungstenite` over rustls-rustcrypto + the T4 dialer).
`cargo tree -p molt-net -e no-dev -i ring` must stay empty — enforced by
`crates/molt-net/tests/ring_free_guard.rs` since 2026-07-31.

Keep new code pure-Rust where you can; these are the sanctioned exceptions.

## MLS / OpenMLS + transport-crate specifics

Moved to `crates/molt-net/CLAUDE.md`, which loads only when you work under
that crate: OpenMLS version pairing and API traps, the concurrent-commit
convergence rule, and the reusable test doubles.

## Transport (Nostr in production, loopback in tests) + the delivery guarantee

**Since N4/N5 (2026-08) the production transport is Nostr/NIP-EE**: founding,
join, recovery and the running 445 group runtime all go over relays
(`docs_archive/transport/nostr_transport_marmot.md`; the governed relay pool is
`relay_topology_plan.md`). The SMP transport and its machinery (the
permissive-loopback-vs-SMP creds asymmetry, `reopen_transport`, SKEY sender
seeds, the Stage-B N-queue redundancy, self-heal/rotate/keepalive/probe) were
removed in etappe N-demo (2026-07-30); the design docs under
`docs_archive/transport/mesh/` are historical records. What remains beside the
Nostr runtime: the queue-shaped `Transport` trait, the loopback hub — THE
test transport — the supervisor's delivery-guarantee core with a
single-queue inbound redial loop, and the T4 Tor dialer at
`crates/molt-net/src/dial.rs` (S3 and the relay WebSockets use it). `LoopbackTransport` is **permissive** — its queues live in the shared hub,
so any clone can subscribe to any queue; a real transport gates receive on
credentials, so don't lean on that forgiveness. **The delivery
guarantee (2026-07-28, `docs_archive/transport/delivery_guarantee.md`) sits on top of
the transport**: every wire event is at-least-once end-to-end within the compaction
grace. Receivers keep a per-sender `AcceptedWindow` (envelope dedup + the
payload of `MESH_ACK_TAG` control frames, sent debounced after every delivery
— duplicates included, a dup re-arms the ack); the supervisor consumes acks
itself and keeps a monotonic `acked_floor`/`ack_seen` on each outbound
cursor; EVERY supervisor build rewinds proven-acking peers to their floor
under a bumped `resend_epoch` (fresh msg ids — epoch 0 is byte-identical to
the legacy id), and a stalled tail rewinds itself on a 30s..600s backoff,
going loud (send_failed) after 8 fruitless rounds but never silently giving
up. Resends are always FRESH encryptions (cache evicted on rewind). Old
nodes never ack → they keep exactly the plain cursor behavior. The former
"per-drain MLS persist" gap is closed pragmatically: `persist_mls_if_due`
merges the live ratchet every ≤10s of traffic (riding `record()` and the
1s `NetDeliveryTick`, which also flushes due ACKs and the accept-window
saves — the 30s presence tick alone stretched every "seconds" debounce to
half a minute), so a hard kill regresses the ratchet by seconds and the
rewind-resend outruns the few replay-rejects. The acked floor is a LOG
position (it advances over foreign/commit seqs; only own unacked events
pin it) — an "own-events-only" floor read as a permanently-unacked tail on
every quiet listener. A recovery announce resets the survivor's accept
window for the rejoiner (its fresh incarnation reuses seqs). Sender-ratchet windows are
explicit (out-of-order tolerance `5_000`, forward `100_000` — the openmls
defaults of 5/1000 bricked any leg whose deaf window swallowed more). WP4a's
compaction gates a proven-acking peer on its ACKED floor (unacked tail stays
resendable); the guarantee's horizon IS the WP4a peer grace. Keystones:
`crates/molt-engine/tests/delivery_guarantee.rs` (clean-close + hard-kill,
both verified red-without/green-with).

## Build, test, run

- `cargo build` builds the whole workspace including the Slint GUI (slow first
  build). Headless vs GUI is a runtime choice, not a separate build.
- **The Slint-generated window lives in its own crate, `molt-ui-window`, as a
  compile-time firewall** (2026-07-13): `ui/app.slint` compiles to a ~400k-line
  Rust module whose single rustc **peaked at 8.66 GiB RSS / 12m50s — measured
  2026-08-18 on the 15 GiB box, rustc 1.95.0 + Slint 1.17, `cargo build -j 1`,
  dev profile, incremental state of that run not recorded** (the older ~6 GiB / ~4 min figure predates those versions;
  re-measure and re-date this line after a toolchain or Slint bump rather than
  trusting it). That cost is paid ONLY when a `.slint` file changes; GUI-logic
  edits (`molt-ui`) rebuild in ~2 s at <1 GiB. Debuginfo reduction does NOT
  help (measured: ~2%), and a SIGKILL during the window compile is the kernel
  OOM-killer. Two things keep it survivable, and only one of them is yours to
  remember:
  - **The workspace pins `[profile.dev.package.molt-ui-window] incremental =
    false`** (root `Cargo.toml`). Do NOT delete it and do NOT export
    `CARGO_INCREMENTAL=0` on top of it. What the kernel log shows
    (2026-08-21/22): with incremental ON, the SINGLE window rustc was
    OOM-killed three runs in a row at **11.6 / 13.1 / 13.7 GiB anon-RSS** —
    one process each time, no concurrency involved. With the override, two
    consecutive full window rebuilds finished in **9m33s and 8m11s**, and the
    unit's rustc line carries no `-C incremental=` while `molt-ui`'s still
    does — the override is scoped to this one crate, so every other crate
    keeps its fast incremental rebuilds.
  - **Build the window with `-j 1`.** Plain `-j 2` puts the lib and test
    rustc side by side and died by SIGKILL here (2026-08-18, with a second
    session on the box). Never run two window-scale builds concurrently —
    and note that a worktree agent with its OWN target dir is NOT serialized
    by cargo's build lock against a build in the main checkout.

  GUI changes are validated by a clean `cargo build -j 1 -p molt-ui-window -p
  molt-ui` — **one** invocation naming BOTH: `-p molt-ui-window` alone
  resolves a different `slint` feature set, so a solo window build is thrown
  away the moment `-p molt-ui` is built after it
  (`UnitDependencyInfoChanged`, verified 2026-08-21 — it cost a full
  10-minute rebuild).
- **GUI iteration goes through `scripts/dev-ui.sh` — NOT the 9-GiB build.**
  It sets `SLINT_LIVE_PREVIEW=1` + the `live-preview` feature chain
  (molt-app → molt-ui → molt-ui-window → `slint/live-preview`, Slint ≥ 1.13):
  slint-build then emits ~2.6k-line interpreter-backed stubs with the identical
  API instead of the ~400k-line module — measured 2026-07-14: a .slint edit
  recompiles in **~2 s at <1 GiB** instead of the full window build. `dev-ui.sh run`
  starts moltd with runtime .slint hot-reload (properties/models/callbacks
  survive a save; an *incompatible* interface change panics the running app —
  restart it, still no recompile). The script uses its own `target/dev-ui`
  cache so the feature set never thrashes the normal build's cache (first fill
  is a one-time full-stack build, but RAM-light). The SAME mechanism runs the
  GUI-logic TESTS in seconds: `CARGO_TARGET_DIR=target/dev-ui
  SLINT_LIVE_PREVIEW=1 cargo test -p molt-ui --lib --features
  molt-ui/live-preview` (verified 2026-08-15: 129 tests in ~11 s incl. build)
  — iterate there, and run the expensive window build ONCE per change-set.
  Dev-only: never enable the feature by default in a Cargo.toml. The .slint compiler still runs fully, so
  `dev-ui.sh build` catches .slint errors and API breaks in molt-ui; the
  authoritative pre-commit check remains one normal
  `cargo build -p molt-ui-window -p molt-ui` (once per change-set, not per
  iteration).
- **Driving a REAL node by hand goes over MCP, and needs a relay.** Founding
  refuses without one ("cannot found: no relay configured"), and the suite's
  `MockRelay` lives inside the test process — so there is
  `cargo run -p molt-net --example dev_relay`, which prints a
  `ws://127.0.0.1:<port>` URL and stays up. Then: `moltd --generate-config
  <path>`, set `node.headless = true` plus a scratch `workspace_dir`, start
  it with stdin held open (`tail -f /dev/null | moltd --config <path>` — it
  serves MCP on stdio AND on `[mcp].port`, and exits when stdio closes), and
  talk newline-delimited JSON-RPC to the TCP port with the config's token in
  `initialize`. Two configs on two ports found and join a real republic in
  under a minute; it is the only way to exercise the command surface end to
  end without a window.
- Tests that need a real network are `#[ignore]`d — the Nostr real-relay PoC
  twin (`crates/molt-net/tests/nostr_relay_poc.rs`), the live-S3 probe, and the
  embedded-tor bootstrap; the founding+join+MLS flow is proven fast over
  loopback in `crates/molt-engine/tests/two_instances.rs`.
- **Never launch a GUI window on `DISPLAY=:0`** — that is the user's own X
  session. There is no headless display here, so GUI changes are validated by the
  Slint compiler (a clean `cargo build -p molt-ui-window -p molt-ui`) plus the
  engine-level tests, not by pixels.
