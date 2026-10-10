# Known debt — the living list (started 2026-08-16)

Status: **OPEN WORK.** The surviving deferred items whose home documents
were executed and archived. One entry per item, with its fix direction;
an item leaves in the change that closes it.

## Story 14 remainder - the Wallet's Stage 2

From `docs_archive/ui/mock_todo.md` §14. Memory, Vault, Kanban and the
Wallet's Stage 1 (`docs_archive/chain/wallet_treasury_design.md`, built
2026-10-10) are REAL. Spending (Stage 2, design §8) is unplanned and waits
for all three: `SalLegacyAlgorithm` over `ThresholdKeys<Ed25519>` in a
released monero-wallet, a proof or audit of threshold SA+L, fixed FCMP++
fork heights. Kanban's vote load is still to be measured on a real
republic (`kanban_workflows.md` §11 Q1).

## Wallet Stage 1 - open items (2026-10-10)

- **Emergency exit** (design §5): m holders can rebuild the spend key and
  sweep. Recorded, not built; needs a user decision whether it gets a
  governed path.
- **Manual three-instance walk** (plan §16) not run: founding with the
  purse stage, same address, warning, mining, close/reopen, export/import,
  phrase recovery watch-only, a later enable via the panel.
- **Retention pin**: while a `.lost` keys file exists, copies older than
  it are kept for good; only a restore or removing the file releases
  them. The dialog offers no restore. Decide on a restore-from-S3 action.
- **Output cap**: `molt_treasury::scan::MAX_OUTPUTS` (50k) never shrinks
  (no spend detection in Stage 1), so a purse past it (dust spam) stops
  scanning for good as `daemon fault: too many outputs`.
- **Release note, backup version skew** (design §6): an older build
  exports without the keys file and refuses to import a blob carrying it.
  Say so in the first release notes that ship the purse.

## A restore consent binds neither its relays nor a position (audit 2026-10-10)

`restore_consent_bytes` (`molt-restore-consent-v1`) signs republic id,
seat, identity key and nostr anchor only, yet `block_signers` counts it
as a full voice. Two consequences, both from one member:

- **Relays.** Only the coordinator checks the seat proof that binds the
  declared relays; survivors auto-sign any `relays` beside a valid
  consent, so a hostile coordinator (or any member re-gossiping the
  consent under a fresh id) writes the seat's ledger entry. That freezes
  R6 pool votes and fails R5 for every other seat's recovery.
- **Replay.** On m = 2 one member plus a published consent seals a
  `Restored` block at any height: rolling the seat back to an earlier
  anchor, or rewriting its relays. Neither the verifier nor the receive
  path checks anchor freshness on blocks.

Fix direction (user decision, signed layout): a `molt-restore-consent-v2`
over the relays and a single-use value (the KeyPackage hash or ticket),
conditional like roster-v5 so sealed v1 blocks still verify; plus a
verifier anchor-freshness rule for v2 blocks only (pre-C8 chains may hold
replayed-anchor blocks, field storm 2026-08-24).

## `SaveTransport` overwrites `vault_status` wholesale

`molt-storage` writer, `WriterMsg::SaveTransport`: the supervisor's clone
replaces `vault_status`, unlike `vault_seed` (kept unless set) and
`vault_displaced` (union). A clone older than a receipt update writes the
old receipts back. Fix direction: give the receipts their own writer
message, as `SaveWalletSeat` does for `wallet_status`.

## Vault share refresh (V6)

From `docs_archive/vault/vault_threshold_disclosure.md` §9.6 and §14 step
6: shares hang off seeds that never rotate, so m phrases collected over
the years read every earlier deposit. Fix direction: proactive refresh
(each holder adds a share of a random zero-polynomial) under a refresh
epoch in the deposit record; needs its own plan.

## A `rebase` verdict is not reproduced after a hard-kill reopen

From `docs_archive/reviews/supersede_verdict_survives_apply.md` §3: the
tail replay of a reopen walks nothing until the chain is adopted, and the
walk after the adoption is `supersede_stale_wiki(None)`, which writes no
`Rebase` (that needs the block's `moved` paths). An open patch a live seat
showed as `superseded: "rebase"` reads `null` on the seat that was
hard-killed after its last snapshot; the card itself is intact and
votable. Fix direction: walk the suffix blocks' `moved` paths in the
adoption walk, and pin it with a propose-then-ratify ordering in
`checkpoint_under_load.rs`.

## An export can pair a pre-cut chain with a post-cut wiki base

From the 2026-10-05 review of the wiki-base backup fix. The export reads
`chain.state` and `wiki_base.bin` one after the other while the node runs;
a folded cut writes the new base first and the new chain second
(`governance.rs`). A cut landing between the two reads yields a blob whose
base matches no commitment in its chain, and the open drops the base
(`adopt_wiki_base`) - the one it will need once it catches up past the
cut. Rare (a cut is a voted event) and never silent data loss while a
holder lives, but it defeats the every-seat-restores case. Fix direction:
read both under the workspace's writer, or keep a base that authenticates
but does not match the current chain aside until the chain reaches its
commitment instead of deleting it.

## A wiki base near 512 MiB fails the whole backup

`READ_CAP_WIKI_BASE` and the restore cap (`RESTORE_MAX_BYTES`) are both
512 MiB, and the S3 ticker refuses an over-cap blob as a whole
(`backup.rs`). Since 2026-10-05 the base rides every export, so a
knowledge base near the cap stops ALL backups rather than just its own.
Far beyond the K6 target (100 MiB), hence debt, not a defect. Fix
direction: raise the restore cap with the base's own bound, or ship the
base as its own S3 object beside the blob.

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

## `wiki_draft.json` is plaintext (2026-10-09)

Its doc comments and `shared_memory_real.md` §9.2 say "sealed at rest",
but `write_wiki_draft` writes the raw draft, mode 0644. The kanban basket
next to it is sealed since 2026-10-09 (`Workspace::write_kanban_draft`,
its own sub-key and segment marker, held by the engine actor). Fix: the
same move for the wiki draft; a legacy plaintext file reads as no draft.
