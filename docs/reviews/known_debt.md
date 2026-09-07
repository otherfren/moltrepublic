# Known debt — the living list (started 2026-08-16)

Status: **OPEN WORK.** The surviving deferred items whose home documents
were executed and archived. One entry per item, with its fix direction;
an item leaves in the change that closes it.

## Story 14 remainder — real backends for Kanban and Vault

From `docs_archive/ui/mock_todo.md` §14. Memory is REAL
(`docs_archive/memory/shared_memory_real.md`); the Wallet is ratified
and in development (`docs/chain/wallet_treasury_design.md`, next stop
the dep-lock spike); Kanban and Vault have fresh design-mock rounds and
their concept docs (`docs/kanban/kanban_workflows.md` §2–§5+§7,
`docs/vault/vault_threshold_disclosure.md`) — both docs carry open
questions that gate any real build.

## The live-preview GUI suite keeps every headless window until the process ends

Measured 2026-09-06 (`cargo test -p molt-ui --lib --features
molt-ui/live-preview`, single-threaded, RSS sampled per test): the test
process grows by roughly 50-100 MB per headless `AppWindow` and never
shrinks - 25 GUI tests in, 0.7 GiB; the whole 283-test suite in one
process reached 12.4 GiB and was OOM-killed on the 15.9 GiB box. Each
test drops its window, so something outside the test retains the
interpreter's compiled component or the testing backend's window
(suspects: the live-preview stub's hot-reload registry,
`i-slint-backend-testing`'s window list). Until the retainer is found the
suite runs in SHARDS, one process per GUI module (CLAUDE.md, build
section). Fix direction: reproduce with two windows in one test and
`Rc::strong_count` on the adapter, then look at what the stub generated
under `target/dev-ui/.../out/app.rs` holds after `AppWindow` drops.

## Which renderer a GPU-less box should ship

From `docs_archive/ui/wiki_pane_performance.md` §4 step 5 / §5 Q1-Q2: the
`[ui] renderer` key exists (`auto` | `software` | `gl`), but nobody has
measured the software renderer against femtovg-on-llvmpipe on the live
Qubes nodes (`SLINT_BACKEND=winit-software` plus
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console,overlay`), nor run the wiki
on the compiled release build at all. Decide the default from that
measurement; until then `auto`.

## The seat token is all-or-nothing (field report v0.0.2, F5)

One write key admits all seat tools, `delete_workspace` and
`export_workspace` included; the only narrower key is the read-only one.
A per-token tool allowlist ("may chat and propose, not exfiltrate or
destroy") is a third scope beside Read and Seat and a design question
against ADR-0007's "one seat, one operator". Not built. The follow-up
report's `[mcp] allow_seed_reveal` knob is the same question in a smaller
coat: refusing `reveal_seed` alone is theatre while `export_workspace`
still carries the seed - a per-token allowlist covers both, a lone knob
covers neither.
Source: `docs_archive/reviews/field_report_v0.0.2_headless_seat.md`.

## A reasoned decline delays the seal (agent round 2, A4)

From `docs_archive/reviews/mcp_agent_friction_fixes_round_2.md` §A4: a
decline with a reason should hold the seal for a moment so the other
voters see it before the threshold closes - a governance question the
republic has to decide (how long, and whether a decline can hold at all).
Not built; everything else of that round is on master.

## Charter features are a hand-rolled column per feature

Every feature key is wired by hand at ~19 sites (wizard grid, both
`CharterView`s, the Organization list, the enable modal's arming
expression + payload string, `mirror.rs` setters) — a fifth key rolled
out on 2026-08-28 and was removed again on 2026-09-03, each direction
touching every site. Fix direction: one `FeatureRow
{key,label,checked,enabled}` model built in Rust from
`Surface::ALL.filter(is_charter_feature)`, rendered by ONE component at
all five sites.



## The consumed-frame ring is in-memory: every start re-decrypts the day window

The 445 subscription names the day's `h` tag with no `since` (`place_req`
clamps a reconnect to cursor − 48 h anyway), so every subscribe replays
the window; `SeenCiphertexts` starts empty per runtime, and each consumed
frame costs one outer AEAD plus a ratchet lookup ending in
`MlsError::Stale` (debug since 2026-09-03). Own echoes and frames sealed
outside the 8-deep exporter ring replay the same way and no ring catches
them. Cost: bounded by `MAX_STORED_EVENTS_PER_REQ` per start, no loss.
Fix direction, if ever: a ring INSIDE `MlsMember` (noted under the member
lock, snapshotted with the ratchet, so it can never run ahead of it) - a
separate file cannot hold that invariant. Open: a v3 snapshot blob strands
a downgraded seat.

## Review 2026-08-25 — the deferred findings

`docs_archive/reviews/code_review_2026-08-25.md` holds the full review (every crate,
eight passes); its CRITICAL/HIGH items were fixed the same night. The items
still OPEN there, by id (each carries its fix direction in the review):

- Chain: C3 residual (a non-logged direct serve).
- Engine: E3 residual (insider system line).
- Ritual/recovery: R4 (design: a survivor capturing a reattaching seat) ·
  R9 residual (`nostr_sk` in the two `Net*Sealed` commands).
- MLS/delivery: M1 residual (`ChainOracle` allowance design) · M2 open half
  (chain-backed commits outrank at the tiebreak, design).
- Transport: T10 residual (test-only cursor API).
- Storage: S1 residual (`openat2` beneath the workspace).
- Frontends: F7 residual (token read per accepted connection).
- MCP privileges (section 9): P8 ritual abandon on context switch
  (product) · P10 send-side rate limits.

## The compiled window is invisible to `ElementHandle` (2026-09-06)

`i-slint-backend-testing`'s element queries answer NOTHING against the
generated window: ids, type names, accessible labels and the descendant
walk all read as empty (measured with a diagnostic test in the normal
profile, round-3 fix G1), because the compiled flavour emits element
info only under `SLINT_EMIT_DEBUG_INFO`. So the 73 GUI tests that look
an element up are `#[cfg(feature = "live-preview")]` and prove the
INTERPRETED window, never the shipped one. Fix direction:
`slint_build::CompilerConfiguration::with_debug_info(true)` in
`crates/molt-ui-window/build.rs` for the dev profile only, then drop the
gates; unmeasured cost on a window build that has no RSS headroom left
(CLAUDE.md, build section) - measure once, alone, before adopting.

## Flaky: `a_broadcast_ack_moves_the_senders_proven_floor` (2026-09-05)

Failed once on `cursor.ack_seen` during a fully parallel `cargo test
--workspace`, and passed 6/6 alone plus once under two concurrent test
binaries. Load-dependent: the debounced `MESH_ACK_TAG` frame has to land
and be consumed before the close seals the sheet, and a starved runtime
misses that window. Fix direction: the test should wait on the sheet
(poll `read_transport_state` until `ack_seen`, with a deadline) instead of
on the close - the guarantee is eventual, so a one-shot read of it is the
test's bug, not the engine's.
