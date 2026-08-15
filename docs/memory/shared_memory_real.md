# Shared Memory leaves design-mock — the plan

**Status: BUILT 2026-08-15** — WP-A through WP-F landed the same day the
forks were decided (WP-G folded into `read_state`: the Memory snapshot
carries the tree, so GUI and MCP share ONE read and no extra tool was
needed; drafts additionally got `wiki_draft_save`/`load` tools). Deltas
against the plan, all recorded in place: the transport size caps stay at
propose/ingest (never in the fold), the fold recomputes on read (cache
deferred until measurable), superseded entries carry no timestamp and thus
do not age out of the Denied view (they are the rescue's entry point).
The ask:
the Shared Memory surface (Multisig-Wiki) stops being a design mock and
becomes the real product. This document is the exhaustive, execution-ready
plan; the §9 forks were discussed and decided with the user on 2026-08-15.
Build order relative to other open work: the ritual seed-backup round
(`docs_archive/ritual/seed_backup_confirmation.md`) lands first.

Read first: `docs/ui/mock_todo.md` (story 14 + the 2026-08-12/14 rounds),
`docs/chain/persistent_chain.md` (the state model this builds on),
`docs/ritual/charter_features.md` (how the surface is gated).

## 1. Where we stand — what is already real, what is mock

Real today (2026-08-14 state, on master):

- The wiki UI state machine (`crates/molt-ui/src/wiki.rs`): navigator,
  tabs, editor, per-file change tracking, changeset stack with undo,
  `build_patch` (net unified diff + rename/new/delete headers via
  `similar`), all unit-tested.
- The GOVERNANCE loop end to end: "start vote" mints a REAL gated proposal
  `{op:"wiki_patch", summary, value}` on `Surface::Memory`
  (`wire_wiki_vote`), runs the same threshold machinery as every surface
  (approve/decline/seal → `Applied` chain block), is reachable co-equally
  via MCP `propose`, and its decision chat carries the diff viewer
  (`patchview.rs`). Applied payloads are ACCUMULATING at a checkpoint cut
  (`applied_lww_slot` returns None for Memory), so the full patch history
  survives compaction already.
- Proposals / Accepted / Denied views: real decided-vote tables.
- The transport-derived payload cap (`payload_fits`) already bounds a
  changeset's size at propose AND at wire ingest.

Mock today:

- **The base.** `Wiki::sample()` seeds hardcoded sample docs; the ratified
  base never changes: an APPLIED wiki_patch changes nothing anywhere
  ("Patch-APPLY bei Threshold" is story 14's named gap). No convergence:
  two members' wikis share nothing but the proposals.
- **No persistence.** The whole wiki model is in-memory; local drafts die
  with the process.
- **Archive view**: hardcoded `MockNote` rows.
- **Co-equality gap**: no MCP read serves the wiki tree or a doc's
  content — an agent cannot see the base it would patch.
- **Shell**: the brain pane carries the DESIGN-MOCK badge; app routing
  sends brain/archive to the mock `MemoryPane`.
- On decline, the proposer's changes are gone (working copy was reset at
  vote start) — "Draft-Rettung bei Decline" is the second named story-14
  gap.

## 2. Target model

**The base is a deterministic fold.** A republic's shared wiki is
`fold(empty tree, applied wiki_patch payloads in chain order)` — no new
wire events, no new signed bytes: the persistent chain already carries and
orders the `Applied` transitions, and the checkpoint already preserves
them. The working copy stays a per-node LOCAL draft layer on top; content
reaches the base only through the existing threshold vote. Invariants:

- **Determinism.** Same log/chain → byte-identical tree on every node,
  live, after replay, and after a checkpoint cut. This is the same class
  of invariant as the chain projections; it gets the same keystone tests.
- **Ephemeral vs persistent boundary** (persistent_chain.md): drafts and
  the changeset stack are flüchtig and local; only the threshold-approved
  patch becomes durable republic knowledge.
- **Sign-what-you-see.** Members vote on the exact patch bytes the diff
  viewer renders; the fold applies exactly those bytes.
- **Additive evolution.** No checkpoint/roster layout bump needed (§WP-B);
  older nodes simply keep not folding (they never displayed a base).

## 3. Work packages, in build order

### WP-A — patch parse + apply + fold in molt-core

- Move the tolerant git-patch PARSER out of `molt-ui/src/patchview.rs`
  into molt-core (pure, no I/O; the UI keeps its rendering half and
  re-uses the moved parser). Layering demands it: the engine folds, and
  engine must not depend on ui.
- **Search-first (dont-handroll) — VERDICT 2026-08-15:** `diffy`
  (MIT/Apache, pure Rust) was evaluated against the real API: its `apply`
  deliberately mirrors GNU patch — "will attempt to find the correct
  place to apply each hunk by iterating forward and backward from the
  given position until all context lines match". That offset tolerance is
  exactly the disqualifier from the decided criterion: with repeated
  markdown patterns a hunk could bind deterministically but SEMANTICALLY
  WRONG to a duplicate region — and members ratified a diff shown at a
  specific location. Decision: the STRICT own hunk-apply over our parsed
  hunks (exact position AND exact context, else the whole patch is void),
  keystone-pinned. The multi-file git-header parser was ours already
  (`patchview.rs`) and moves down to molt-core.
- **Refinement (found at build time):** the transport size caps stay at
  propose/wire-ingest ONLY and are NOT part of the fold — a fold that
  re-checked a version-dependent budget would void honest chain-recorded
  patches differently across versions, breaking determinism.
- `molt_core::wiki_fold(applied: &[Value]) -> BTreeMap<String, String>`:
  parse each wiki_patch payload, apply ALL-OR-NOTHING per patch (a half
  applied patch would fork trees), path-normalize, enforce the same size
  caps as propose. Any parse failure or context mismatch → the WHOLE
  patch is VOID (skipped) — deterministic, because every node folds the
  same bytes over the same predecessor tree (§4).
- Keystones (red first): fold determinism (live == replay == snapshot),
  void-on-mismatch, malformed-payload void, all-or-nothing, a byte-pin
  fixture tree.

### WP-B — engine integration

- `State` gains the folded-tree projection for `Surface::Memory`
  (recomputed from the applied concat — legacy `applied` + `chain_applied`
  — on demand; cache invalidated when an Applied lands. Content is small
  text; recompute cost is a non-issue at wiki scale).
- **Read side (co-equality):** `ReadState{surface: memory, view: brain}`
  serves the folded tree (paths + per-doc content + the fold revision
  counter). This is the SAME projection GUI and MCP read.
- **Supersede walk (§4):** runs inside the deterministic apply path after
  every Applied AND at proposal registration (catch-up), using the fold's
  `applies_cleanly`; transitions incompatible pending wiki_patches to the
  terminal superseded outcome (Rejected state + additive `superseded`
  marker, no vote forged); `cmd_approve` refuses a superseded wiki_patch
  with an honest error (race narrowing — the fold stays the arbiter).
- **Ingest validation** (node-independent, the set_image pattern): a
  wiki_patch payload that does not parse as a patch is dropped at propose
  and at wire ingest — never recorded.
- Checkpoint: verify-with-a-keystone that a cut keeps the fold identical
  (accumulating entries survive by design). NO v8 bump. Record the known
  debt: the patch history inside checkpoints grows forever; a future
  conditional v8 could carry the folded tree and drop the history at the
  cut — deliberately deferred until growth hurts.
- Determinism keystone extension: the existing dump-equality tests cover
  the projection automatically once it derives purely from applied data —
  add one asserting the folded tree explicitly.

### WP-C — molt-ui: the wiki sits on the real base

- `Wiki` loses `sample()`: the base loads from the engine read; a fresh
  republic starts with the honest empty state ("Nothing here yet").
- Base refresh when an Applied lands (the surfaces sync already ticks):
  swap `BaseDoc`s in place; the existing status derivation (raw vs base)
  recolors automatically; local drafts are KEPT (they are the member's
  work); the undo stack keeps its existing collision-honest refusals.
- **Draft rescue:** on a Rejected — and on a VOID fold verdict — the
  proposal payload still carries the patch: one click re-applies it to
  the CURRENT base into the working copy (honest toast when parts no
  longer apply). Closes story 14's "Draft-Rettung bei Decline".
- **Superseded cards (§4):** they leave Pending and list in the Denied
  view as "superseded - base moved (#id)", distinct from "declined by
  <member>"; the rescue affordance sits on the Denied row AND in the
  patch's read-only decision discussion (outliving the view's display
  retention). Rescue applies onto the CURRENT working copy, best-effort,
  with an honest toast for what no longer fits — any member may rescue.
- The proposer's post-vote reset stays (the proposal carries the changes);
  rescue is the way back.

### WP-D — local draft persistence

Drafts survive a restart: serialize the working copy + changeset stack
per workspace into the workspace dir (at-rest sealed like everything
else there; never part of backup export? — see §9 Q2). Load on open,
write-behind on mutation (debounced). Small, UX-critical.

### WP-E — the archive view

Recommendation: REMOVE "archive" from `Surface::Memory.views()` until a
real design exists (the Accepted table already IS the decision history;
a per-doc version history is a later, separate design). Removing a mock
beats keeping it badged. Delete `MockNote`.

### WP-F — de-mock the shell

- app.slint routing: the brain view renders the real pane
  unconditionally; `MemoryPane`'s memory-mock paths and the DESIGN-MOCK
  badge on the wiki title go away (the badge component stays for the
  other staged surfaces).
- Strings: empty-state/i18n audit for the new lines; compact-text rule.
- Status lines: `docs/ui/mock_todo.md` story 14 (memory part) flips to
  done; this doc's status flips to BUILT with the landing commit.

### WP-G — MCP surface

- New read tool (`read_wiki` — tree + one doc's content), thin over the
  WP-B projection; `propose` stays THE write path (the payload format is
  the contract, now validated at ingest).
- Co-equality test extends automatically (`tools()` list); GUI drives the
  same reads through the same ReadState.

### WP-H — verification (the gate for "done")

- Two-instance loopback: A edits, votes; both approve; BOTH trees fold
  identically; B's unrelated local draft survives the base move.
- Conflict path: two proposals over the same base; the second is VOID on
  every node; rescue restores it as a draft against the new base.
- Supersede walk: after an Applied, the overlapping pending patch turns
  superseded on every node (live == replay == rejoiner catch-up — the
  determinism keystone), a fresh approve on it is refused, and a
  NON-overlapping pending patch stays approvable and still applies
  (blast-radius pin).
- Walk/fold agreement: one `applies_cleanly` — a patch the walk keeps is
  a patch the fold applies (pinned).
- Coexistence: a superseded patch race-sealed by a behind node processes
  cleanly (block applied, fold voids, card stays superseded, no wedge).
- Rescue from the Denied row and from the read-only discussion restores
  the changeset into the local working copy (both entry points pinned).
- Decline path: rescue restores the declined changeset.
- Checkpoint cut: fold before == fold after.
- Oversized changeset: vote refuses honestly (cap message).
- Legacy migration (§6) behaves as specified.
- clippy 0, i18n complete, doc-refs checker clean.

## 4. Conflict & staleness rules (the determinism core)

- Patches apply with EXACT context. A patch whose context no longer
  matches (the base moved since propose) is VOID in the fold — whole
  patch, all-or-nothing, deterministically on every node (same bytes,
  same predecessor tree). Void is a FOLD verdict, never chain data — the
  chain honestly records that the vote passed; the Accepted row shows a
  quiet "superseded" marker (display projection, like `<moved>`).
- STALENESS is display, not consensus: the proposal card can warn "base
  moved since proposed" (compare the payload's base revision hint, §9.1),
  so members can decline instead of approving a doomed patch. The fold's
  verdict never depends on the hint.
- Path rules: normalized relative paths, single-level folders (the
  existing tree model), collisions within one patch → void.

**The pending pool after every Applied — the SUPERSEDE WALK** (decided
with the user, 2026-08-15):

- **Never automatic CONTENT changes.** No auto-rebase, ever: members
  deliberated and signed THESE patch bytes; an engine-rewritten patch
  under old approvals breaks sign-what-you-see. Automatic is allowed
  ONLY for lifecycle verdicts derivable from chain-ordered data — never
  for content, never for attributed votes.
- **The walk.** After every Applied — inside the ONE deterministic apply
  path, so live, replay and snapshot+tail all reach the same states —
  every node walks the pending wiki_patches and re-checks each against
  the new base with THE fold's own `applies_cleanly` (same parser, same
  exact-context rule — one function, keystone-pinned). An incompatible
  patch transitions to the terminal outcome **SUPERSEDED**: mechanical
  and unattributed — `declined_by` stays empty, the decliners list is
  untouched (a "decline" is a member's voice; nobody's voice is forged).
  Snapshot compatibility: reuse `ProposalState::Rejected` + an additive
  `superseded` marker field, so older readers keep parsing.
- **Registration-time check too.** A proposal learned late (WP2 catch-up
  re-serve, recovery) lands against the CURRENT base: if already
  incompatible it registers straight as superseded — no zombie pending
  cards on rejoiners.
- **Display.** Superseded patches list in the DENIED view next to human
  declines, visibly distinct: "superseded - base moved (#<id>)" naming
  the patch that overtook it, vs "declined by <member>".
- **Rescue, for everyone.** Any member can pull a superseded (or
  declined) patch into their OWN local working set: best-effort apply
  onto the current working copy (it becomes part of their changeset),
  honest report of what no longer fits; rework → new vote. That IS the
  re-base, as a human act. Reachable from the Denied view AND from the
  patch's read-only decision discussion (the Denied view's display
  retention ages entries out; the record and the discussion remain).
- **The fold stays the final arbiter (backstop, never removed).** A node
  behind on the chain may still deliver the m-th signature and SEAL a
  superseded patch; a mixed-version node without the walk may too. Both
  truths must coexist idempotently: the block is processed, the fold
  voids it identically everywhere, the card stays superseded, nothing
  wedges. Approving a locally-superseded card is refused (clickable OR
  grayed + engine refusal) — that only narrows the race, the fold closes
  it.
- **Blast radius is real overlaps only.** Context-exact apply works per
  hunk: pending patches on other files or other regions stay applicable
  through an Applied. Only genuine same-region conflicts supersede — and
  all-or-nothing then takes the WHOLE patch (a half-applied changeset is
  a state nobody proposed).

## 5. Size & transport

`payload_fits` already bounds proposals by the publish budget (70 KiB
headroom rule). Add a UI pre-flight: the vote button refuses an oversized
changeset with the byte count instead of letting the engine bounce it.
Chunked large changesets are OUT of scope (recorded as future work).

## 6. Migration (live republics)

Existing logs already hold test-era wiki_patch payloads that were diffed
against the SAMPLE docs. Under the fold they apply against the EMPTY
founding tree: most will be void (context mismatch), a few (pure adds)
will apply. Both outcomes are deterministic and honest — no data is
invented, nothing crashes, and the Accepted history keeps naming what was
decided. No migration tooling needed. The surface stays charter-gated
exactly as today (`charter_features.md`).

## 7. Non-goals (deliberate)

Three-way merge / rebase tooling; per-doc permissions (agents-are-seats:
none, threshold is the only authority); the archive view's real design;
chunked large files; checkpoint v8 fold-at-cut (deferred debt, §WP-B);
cross-doc link integrity enforcement (links stay best-effort navigation).

## 8. Implementation map

- Parser/apply/fold: NEW `crates/molt-core/src/wiki_fold.rs` (parser moved
  from `crates/molt-ui/src/patchview.rs`, which keeps rendering).
- Engine projection + read + ingest gate:
  `crates/molt-engine/src/proposals.rs` (snapshot/view),
  `crates/molt-engine/src/net.rs` (ingest gate, the set_image pattern),
  `crates/molt-engine/src/chain.rs` (applied concat is already there).
- UI: `crates/molt-ui/src/wiki.rs` (base load/refresh, rescue, drop
  `sample()`), `crates/molt-ui/src/lib.rs` (`wire_wiki`, `sync_wiki`,
  surfaces routing), `crates/molt-ui-window/ui/surfaces.slint` +
  `app.slint` (badge, routing, archive removal).
- Drafts: `crates/molt-storage` (sealed local file next to the workspace).
- MCP: `crates/molt-mcp/src/lib.rs` (`read_wiki` tool + co-equality list).
- Views vocabulary: `crates/molt-core/src/lib.rs` (`Surface::views`).

## 9. Decided (with the user, 2026-08-15)

1. **Staleness hint: YES, display-only.** `wiki_patch` gains an additive
   base-revision field (fold counter or tree hash); the card warns "base
   moved since proposed" so members can decline a doomed patch. The fold
   verdict NEVER reads it — consensus stays context-exact apply.
2. **Drafts persist locally, sealed at rest — and stay OUT of the backup
   export.** Backups keep carrying republic state only; drafts are
   personal scratch. The export contract is unchanged.
3. **Archive view is REMOVED** from `Surface::Memory.views()` until a
   real design exists; `MockNote` is deleted. The Accepted table is the
   decision history.
4. **Founding baseline is the EMPTY tree.** No seeded charter document —
   the ratified agenda lives in Organization, not duplicated as content.
5. **Apply library: evaluate `diffy` first** (WP-A), strictness before
   convenience — any fuzz/offset tolerance disqualifies its apply and the
   strict own hunk-apply wins; the verdict is recorded here either way.
6. **Fold strategy: recompute-on-Applied with cache.** One code path for
   live/replay/checkpoint; incremental only if a real history ever makes
   recompute measurable.
7. **Supersede walk (follow-up decision, same day).** After every Applied
   (and at late proposal registration) every node deterministically
   transitions incompatible pending wiki_patches to the terminal
   SUPERSEDED outcome — shown in the Denied view, rescuable into the
   local working set by any member, reworkable, resubmittable. Not a
   forged decline: unattributed, `declined_by` empty, decliners
   untouched. Automatic applies to chain-derived lifecycle verdicts
   only — never to content, never to votes. Full rules in §4.
