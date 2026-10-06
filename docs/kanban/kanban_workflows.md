# Kanban - planning, scheduling and the gated board

**Status: CONCEPT for discussion, revision 2 (2026-10-06), every anchor
re-verified against master `0f0cb30d`. The §6 design mock is BUILT
(2026-08-16). The state model, ops, engine and MCP design (§2-§5, §7) are a
proposal awaiting ratification; the §8 questions gate any backend build.
§9 lists what changed against revision 1.**

The ask: the Quests surface (GUI label **"Kanban"**, wire key `quests` -
`docs_archive/ritual/charter_features.md` §5.1; there is no `kanban` feature
key) grows from its design mock into a real planning surface: epics →
stories → tasks, scheduled into sprints, operable by humans over the GUI
and by agents over MCP, with **every** change a threshold-approved
proposal. The architectural template is the shared wiki
(`docs_archive/memory/shared_memory_real.md`, scaled by
`docs_archive/memory/knowledge_base_scale.md`): a deterministic fold over
applied payloads on the persistent chain, drafts local, one co-equal
command surface.

Read first: `docs_archive/memory/shared_memory_real.md` §4 (conflict and
staleness), `docs_archive/memory/knowledge_base_scale.md` §4.1-§4.2 (fold
cache, O(touched) supersede), §4.7 (read key), §4.9 (folded cut), §4.12
(the structured write path), `docs_archive/reviews/supersede_verdict_survives_apply.md`
(two bugs the wiki's supersede walk shipped), `docs_archive/chain/persistent_chain.md`,
`docs_archive/ritual/charter_features.md`.

## 1. Where we stand (verified 2026-10-06)

Real:

- `Surface::Quests` (`crates/molt-core/src/lib.rs:66`), key `"quests"`
  (:112), a charter feature (:142), `is_implemented() == false` (:129).
  Views `board / plan / create / proposals / my-quests / archive` (:204).
- Quests is in the frozen `Surface::CHECKPOINT_V7_SURFACES` (:97): every
  cut already carries an (empty) quests group, so applied `kanban_ops`
  never trigger the checkpoint-v8 conditional-group branch.
  `applied_lww_slot` (`molt-core/src/chain.rs:466`) returns `None` for
  Quests: applied entries accumulate.
- The governance loop is generic: `Command::Propose {surface: Quests,
  payload}` runs the m-of-n threshold, MCP `propose`/`approve`/`decline`
  reach it co-equally. The `proposals` sub-view routes to the real tables
  (`app.slint:7415`, `:7463`). The `vote_pending` wake hook fires for any
  proposal, so agent seats are woken for kanban votes with no extra work.
- The engine accepts `quests` in `set_features` (`check_feature_key`,
  lib.rs:157), but **both GUI paths are locked**: the founding wizard
  (`app.slint:3957`) and the Organization features panel (`:9815`). Only
  an MCP `propose` can switch it on today - and would then show a mock.
- The design mock (§6) is built; no engine state behind it.

Not there:

- No fold, no board state: `ReadState {surface: quests}`
  (`snapshot`, `molt-engine/src/proposals.rs:4014`) returns the generic
  card lists plus raw applied payloads.
- No payload validation for quests at either door
  (`propose_payload`, proposals.rs:577; wire ingest,
  `molt-engine/src/net/ingest.rs:239`).
- No kanban read for the read-only key: `Scope::Read`
  (`molt-mcp/src/lib.rs:181`) is the wiki and shared-files reads only;
  `read_state` is Seat on purpose (a chat read sends read receipts).

## 2. Target model

### 2.1 Items

Three kinds, one item table: **epic → story → task**.

- `kind` is immutable after `create`.
- `parent` is optional but kind-checked when set: a task's parent is a
  story, a story's parent an epic, an epic has none. Because the kind
  ladder is strict, parent links cannot form a cycle. A standalone task
  or story is legal. A parent may be closed (a late task under a closed
  story is history, not an error).
- Item ids are random 128-bit lowercase hex, minted ENGINE-side
  (`molt-core` stays RNG-free - the chat `MessageId` precedent). Display
  form: `#` + first 8 hex chars. Ids are what wiki pages and `deps` cite,
  so an item is never resurrected under a new id (→ `reopen`, §3).

**Every item carries the trio:**

- `responsible` - exactly ONE roster seat (the roster name). The roster is
  fixed from founding (seat-adding is won't-do) and a recovery keeps the
  name, so the validation set is stable. No unassigned items, no
  multi-assignment: shared responsibility is a story with tasks.
- `start` - planned start, `YYYY-MM-DD`.
- `due` - expected end, `YYYY-MM-DD`, `start <= due`.

Dates are **planning data, display-only**: nothing executes on a date;
"overdue" is a local-clock rendering, never consensus state.

**Dates are not hand-parsed.** Validation uses
`chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")` (chrono is already a
workspace dependency, `Cargo.toml:73`; molt-core uses only `NaiveDate`,
a pure parser - no I/O, no clock read) AND requires
the round-trip `date.format("%Y-%m-%d") == s`, so `2026-8-1` and other
non-canonical spellings are refused instead of stored in two forms.
Comparison is on the parsed `NaiveDate`.

### 2.2 The template (per kind)

| field | meaning | epic | story | task |
|---|---|---|---|---|
| `title` | one line, 1..=200 chars, no newline | required | required | required |
| `responsible` / `start` / `due` | §2.1 trio | required | required | required |
| `details` | markdown (links, §4), <= 8 KiB | required | required | required |
| `ready` | Definition of Ready, <= 4 KiB | optional | optional | optional |
| `done` | acceptance criteria / DoD, <= 4 KiB | optional | recommended | recommended |
| `scope` | explicitly out of scope, <= 4 KiB | optional | optional | optional |
| `deps` | item ids, <= 32, no self, no duplicates | opt | opt | opt |
| `priority` | `low \| normal \| high \| critical` | required | required | required |
| `points` | integer 1..=100 | - (roll-up) | required | required |
| `status` | board column, §2.3 | derived | derived | derived |
| `sprint` | sprint id (§2.4) | - | optional | optional |
| `pi` | program-increment label, <= 40 chars | optional | - | - |

The per-field caps are new in revision 2: `payload_fits` bounds one
proposal, but nothing bounded the BOARD, and the board rides every
`read_state`. With the caps a 500-item board stays in the low MiB.

An epic stores no points; the roll-up of its descendants is computed on
read. `deps` are opaque id strings, rendered with an existence check,
never enforced (cross-ref integrity stays best-effort, the wiki non-goal).
"recommended" is GUI guidance (an empty `done` on a story/task gets a
muted hint), never a validation rule.

### 2.3 Status columns and closing

`status ∈ backlog | ready | doing | review | done`, all kinds. A `create`
always lands in `backlog` (the same changeset may carry the `move`).
Closing is separate from the `done` column: `close {outcome: done |
dropped}` removes the item from the board into the Archive; any act on a
closed item voids except `reopen`. Closing a parent with open children is
legal; the drill-in shows the open count.

### 2.4 Sprints

Sprints are governed rows of the same fold: `{id, name, start, end, pi}`,
upserted by the `sprint` act, never deleted (a sprint ends by its `end`
date). `name` 1..=40 chars, `start <= end`. `pi` groups sprints as a plain
label (§8 Q7). Sprint windows may overlap (two teams, one republic);
nothing checks it.

### 2.5 Invariants (the template, restated)

- **The board is a deterministic fold.** `kanban_fold(empty, applied
  kanban payloads in chain order) → BoardState`. Same chain → byte-identical
  board on every node: live, after replay, after a checkpoint cut.
- **Ephemeral vs persistent.** The plan basket (§5.1) and form drafts are
  local; only the threshold-approved changeset becomes durable.
- **Sign-what-you-see.** Members ratify the exact acts the card and the
  decision chat render. The payload that is recorded and signed is the
  CANONICAL one (§3.5): every id minted, every local ref resolved -
  nobody signs a placeholder. Kanban acts carry **absolute values**, never
  diffs, so there is no context-mismatch machinery: chain order
  arbitrates, the later applied value wins.
- **Additive evolution.** New acts/fields follow the `WorkspaceEvent`
  rule; an unknown act voids the whole changeset; fold changes ship to all
  nodes together (the wiki fold-rule stance).

## 3. Ops - one payload, ordered acts

### 3.1 The payload

A proposal's payload is a **changeset**: an ordered list of acts, voted as
ONE m-of-n decision, applied all-or-nothing. Sprint planning is one vote,
not thirty.

```json
{
  "op": "kanban_ops",
  "summary": "S9 planning: 1 story, 3 tasks scheduled",
  "base_rev": 17,
  "ops": [ ... 1..=128 acts ... ]
}
```

`base_rev` is the fold revision the proposer saw - display-only (§3.6).
`summary` 1..=200 chars, the card's headline.

### 3.2 The acts

```json
{"act":"create","id":"<hex32>","kind":"task","title":"…","parent":"<id>|null",
 "responsible":"mara","start":"2026-08-17","due":"2026-08-21","points":3,
 "priority":"high","sprint":"<id>|null","details":"…","ready":"…","done":"…",
 "scope":"…","deps":["<id>", "…"]}
{"act":"edit","id":"<id>","set":{"title":"…","points":5,"details":"…"}}
{"act":"move","id":"<id>","to":"review"}
{"act":"assign","id":"<id>","responsible":"walter"}
{"act":"schedule","id":"<id>","start":"…","due":"…","sprint":"<id>|null"}
{"act":"close","id":"<id>","outcome":"done"}
{"act":"reopen","id":"<id>","to":"doing"}
{"act":"sprint","id":"<hex32>","name":"S9","start":"2026-08-17",
 "end":"2026-08-28","pi":"PI-3"}
```

- `edit.set` may name: `title, parent, points, priority, details, ready,
  done, scope, deps` (and `pi` on an epic). The trio and the status move
  ONLY through `assign`, `schedule`, `move` - an edit can never bury a
  responsibility change in a text tweak. An empty `set` is refused.
- `schedule` on an epic carries `pi` instead of `sprint`.
- `reopen` exists so a resurrected item keeps its id - a new id would rot
  every `quest:` citation and every `deps` entry.
- `sprint` is an upsert by id; chain order arbitrates.

### 3.3 Validation - two doors, then the fold

**Shape-reject at BOTH doors** - one stateless function
`validate_kanban_payload(&Value) -> Result<(), MoltError>` in molt-core,
called from `propose_payload` and from wire ingest. The precedent is
Files (`validate_files_payload`, `molt-engine/src/files_state.rs:139`,
called at both doors), not the wiki (whose `wiki_patch_check` runs at
propose only). Refused: not an object, unknown `op`, empty or > 128 acts,
missing required field per §2.2, a field outside the act's vocabulary,
non-canonical date, `start > due`, unknown enum value, a cap from §2.2
exceeded, `edit.set` naming a governed field or empty, points out of
range, self-dependency, an unresolved local ref (§3.5). Never recorded.
`payload_fits` keeps bounding the whole proposal.

**State-precheck at propose only** - `kanban_precheck(&BoardState,
&changeset)`: the fold's own apply over the current board. A changeset
that would void RIGHT NOW is refused at the call with the first reason
(`#aa07c9d3 is closed`), the `wiki_patch_check` stance: an agent learns
its mistake before the vote, not after. Not run at wire ingest - a peer's
board may legitimately be behind.

**Fold-VOID** (deterministic, whole changeset): `create` with an existing
id (in-changeset duplicates included), any act naming an unknown
item/sprint, parent kind mismatch, `responsible` not a roster seat, any
act except `reopen` on a closed item, `reopen` on an open item, unknown
`act`. Void is a fold verdict, never chain data: the block stays applied,
the Accepted row carries the superseded marker - the wiki rule unchanged.

### 3.4 The fold result

```
BoardState {
  rev: u64,                                  // applied changesets folded (void ones too)
  items: BTreeMap<ItemId, Item>,             // open AND closed
  sprints: BTreeMap<SprintId, Sprint>,
}
Item { …§2.2 fields…, closed: Option<Outcome>, touched_rev: u64 }
```

- `touched_rev` (the `rev` of the changeset that last set any field) is
  what makes the overwrite warning of §3.6 computable; consensus state,
  because it is a pure function of the chain.
- Column order is derived: `(priority desc, due asc, id asc)`. No manual
  ranking (§8 Q6).
- Fold location: `crates/molt-core/src/kanban_fold.rs`, beside
  `wiki_fold.rs`. The engine keeps a `kanban_cache` exactly like
  `WikiCache` (`knowledge_base_scale.md` §4.1: epoch-keyed, folds only
  the appended suffix).

### 3.5 Ids, local refs and canonicalization (new)

Revision 1 had the GUI mint ids at staging time but said nothing about an
agent. A model asked to invent 128-bit ids invents `aaaa…` or reuses one -
the friction `wiki_edit` was built to remove. So:

- `create.id` and `sprint.id` are OPTIONAL at the door. Omitted →
  `propose_payload` mints one (engine-side RNG, the `mint_message_id`
  precedent), in its canonicalize-at-propose block (the `set_relays` /
  `set_features` precedent, proposals.rs:594-626).
- A `create`/`sprint` may carry `"ref":"<label>"` (1..=32 chars of
  `[a-z0-9_-]`); later acts in the SAME changeset may cite it as `"@<label>"`
  wherever an item or sprint id goes (`id`, `parent`, `sprint`, `deps`).
  Canonicalization replaces every `@label` by the minted id and drops the
  `ref` keys. A duplicate label or a dangling `@label` is a shape-reject.
- The RECORDED payload carries ids only. Wire ingest therefore refuses any
  `ref` key or `@` value - a peer cannot smuggle unresolved refs past the
  canonicalization that only `propose` runs.
- The GUI basket uses the same path (it may still mint client-side for the
  drill-in preview; the engine keeps a supplied well-formed id).
- `Reply::Proposed` gains the minted ids in act order, so the agent can
  cite what it just created without a second read.

### 3.6 Staleness and supersede (new: generalize, do not copy)

Kanban acts are absolute, so a pending changeset never needs a rebase: if
it still applies, it applies as written. Two consequences:

- **Only one supersede kind.** A pending changeset that would now void
  becomes `SupersededKind::Conflict` (terminal, `state = Rejected`,
  rescuable). `Rebase` is never used for kanban.
- **The silent overwrite is the real staleness risk.** Changeset A sets
  `#aa07` title X at `base_rev 17`; B, applied at rev 18, set it to Y;
  A still applies and overwrites Y. Not a fold error - chain order
  arbitrates - but the voters must see it: the card renders "changed since
  proposed: #aa07 title" for every act whose item has `touched_rev >
  base_rev`. Display-only, the wiki §9.1 `base_rev` stance.

**The walk is generalized, not duplicated.** `supersede_stale_wiki`
(proposals.rs:3808) and `supersede_stale_vault_cards`
(`vault/deposit.rs:339`) are two copies already; a third copy would
inherit both bugs `supersede_verdict_survives_apply.md` found (the walk
ran before the consumed card left `Proposed`; a hard-kill reopen did not
reproduce the verdict - the latter still open in `known_debt.md`). K2
introduces ONE per-surface hook (`touched keys` + `still applies`) that
the existing call sites drive (`chain/governance.rs:666, 1108`,
`events.rs:337, 416`, `chain/projection.rs:582`); the wiki and vault walks
move onto it in the same change, pinned by their existing keystones.
Touched keys for kanban: item and sprint ids (O(touched), the
`knowledge_base_scale.md` §4.2 argument).

### 3.7 Checkpoints (revised)

Revision 1 called accumulation "the conservative default, deferred debt".
Since K6 the wiki FOLDS at a cut (`chain/wiki_base.rs`, checkpoint-v9)
and the vault likewise (`chain/vault_base.rs`, v10), so accumulation is no
longer the house style, only the cheapest start:

- **K1-K6 accumulate.** Legal with no tag change: the quests group is in
  the frozen set and is hashed whatever it holds.
- **Folding is K8, after use proves the need.** It would be a v11 tag,
  content-selected on a `kanban_base` entry, with the board's canonical
  bytes. Unlike the wiki, a capped board (§2.2) FITS in the blob far
  longer - so the vault pattern (commitment inside the blob), not the
  wiki's file-plane fetch, is the likely shape. Decided only when K8 is
  planned (§8 Q9).

## 4. Cross-references: Kanban ↔ wiki

The wiki now speaks two link forms: markdown `[label](target)` (targets
`.md` and `upload:<hex>`, `crates/molt-ui/src/wiki.rs:3071`) and
`[[Name]]` / `[[Name|shown]]` / `[[pred::Name]]` (`expand_wiki_links`,
wiki.rs:3283; split by `molt_engine::link_parts`). Kanban adds one TARGET
scheme, not a dialect:

- **`quest:<id>`**, parsed by `molt_core::wiki_refs::quest_id_of` beside
  `checksum_of` (`wiki_refs.rs:54`) - one parser for the GUI walk and the
  engine's link index. Full 32-hex id, or a unique prefix of >= 8 hex
  (the basename-fallback idiom). In a page: `[the drill
  task](quest:0b6d42f7)`. `[[quest:…]]` is NOT added - `[[…]]` names
  pages.
- **Item → page:** item text fields are markdown; `.md` and `[[Name]]`
  targets resolve through `open_link` (wiki.rs:1239) and open the Memory
  surface.
- **Backlinks are computed on read**: the drill-in shows "referenced by
  <pages>" (a scan of the link index for `quest:` targets), a wiki doc's
  info strip shows the items citing it. Display-only.
- **The wiki's maintenance reads ignore `quest:` targets**: `wiki_health`
  does not count them as dead links, `wiki_links`/`wiki_neighbors` do not
  return them as page edges. A dead id renders muted in the GUI, nothing
  else.

## 5. Workflows - humans over GUI, agents over MCP, co-equally

### 5.1 The plan basket

Acts are **staged locally** into a plan basket (create forms, drill-in
"propose change", move intents), reviewed as a list, then proposed as ONE
`kanban_ops` vote. Persisted next to the wiki draft as
`kanban_draft.json` via a `write_kanban_draft`/`read_kanban_draft` pair
beside `write_wiki_draft` (`molt-storage/src/lib.rs:1083`), sealed at rest
with the directory, and out of the backup by construction (the export
include table is an allowlist, `export.rs:570`). Rescue reloads a
superseded or declined changeset's acts into the basket, the
`Wiki::rescue_patch` idiom (wiki.rs:1141).

### 5.2 Planning, scheduling, execution

- **Planning:** Create view → kind → template → "Add to basket"; stories
  cite the epic by its (basket) id; review; Propose. Decision chat
  deliberates, *m* approvals seal the Applied block, every node folds.
- **Scheduling:** one changeset: the `sprint` upsert, the `schedule` acts,
  the `move`s `backlog → ready`. Committed points per sprint are a read-side
  sum.
- **Execution:** the responsible seat proposes `move`s; the vote on
  `to:"done"` IS the review - the decision chat checks the acceptance
  criteria, the threshold is the acceptance. At sprint end one changeset
  closes and reschedules.

Every state change is m-of-n (agents-are-seats: the threshold is the only
authority, no roles, no owner fast-lane). The mitigation is batching, not
permissions; §8 Q1 asks whether that holds up.

### 5.3 MCP

```
propose {surface:"quests", payload:{op:"kanban_ops", summary:"Q3 epic + 2 stories",
         base_rev:17, ops:[
           {act:"create", ref:"q3", kind:"epic", …},
           {act:"create", kind:"story", parent:"@q3", …}]}}
  → Reply::Proposed {id, minted:["<epic id>","<story id>"]}
approve {proposal:<id>}
read_state {surface:"quests"}            # carries `board` (§5.4)
```

No new Command and no new write tool: `propose` already exists, so the
co-equality test needs no change. A typed `kanban_propose` tool (JSON
schema per act, the `wiki_edit` lesson) is §8 Q11 - the precheck plus
canonicalization already give the refuse-at-the-call behaviour that
mattered there; the remaining gain is schema discoverability.

The `read_state` tool description gets the act vocabulary in one compact
block, and its stale "On memory the whole folded wiki rides along" line
(`molt-mcp/src/lib.rs:1467`) is corrected in the same change.

### 5.4 Reads

- **Seat:** `snapshot` (proposals.rs:4014) gains `board: Option<BoardView>`
  for `surface == Quests` (sorted columns, sprints, closed items, roll-ups,
  per-act "changed since proposed" flags for pending cards). One read for
  GUI and MCP. A board of the §2.2 size fits a `read_state` reply; if it
  ever does not, paging follows the `wiki_list` shape (cursor = id).
- **Read-only key: none in this plan.** The user narrowed `Scope::Read` to
  the wiki and the shared files on 2026-09-04. Exposing the board to that
  key would be a dedicated Read tool (`kanban_get`), never `read_state` -
  §8 Q10.

## 6. Design mock (BUILT 2026-08-16)

As built, `crates/molt-ui-window/ui/surfaces.slint`: `MockItem` (:2433),
`MockSprintRow` (:2458), `KanbanCard` (:2508), `KanbanColumn` (:2586),
`QuestsPane` (:2808) with board :2934, drill-in :3063, planning :3346,
create :3497, mine :3677, archive :3709; 57 `kb-*` strings in
`theme.slint`, EN/DE in `crates/molt-ui/src/i18n.rs`; `view_icon` /
`view_label` in `crates/molt-ui/src/labels.rs:259, 443`; the `select_view`
description lists the six views (`molt-mcp/src/lib.rs:1943`). The design
spec it was built from (sample cast petra/walter/mara/jonas, sprints
S8-S10) lives in revision 1 in git history (`6998c741`).

What K3 changes in it: the `MockItem` sample arrays are replaced by a
model fed from `BoardView`; `MockBadge` goes off; the disabled "Propose" /
"Propose change" / "Propose: move to …" buttons stage into the basket; a
basket view is added (a seventh view key, `basket`, or a drawer on the
board - decided in K3 by the layout, no wire impact beyond `views()`).

## 7. Backend build order (once §2-§5 are ratified)

TDD, red first, each package green on master before the next.

- **K0 - generalize the supersede walk** (§3.6), no behaviour change:
  one per-surface hook; wiki and vault onto it; their keystones
  (`a_sealed_wiki_patch_supersedes_overlapping_pending_patches`,
  the `supersede_verdict_survives_apply` tests) stay green.
- **K1 - core** (`molt-core/src/kanban_fold.rs`, `validate_kanban_payload`,
  `wiki_refs::quest_id_of`, chrono in molt-core): fold + precheck.
  Keystones: fold determinism (one-by-one == all-at-once == from a cached
  prefix), void all-or-nothing incl. an in-changeset duplicate id, unknown
  act voids, the date round-trip refusal, a byte-pinned fixture board
  (canonical JSON of `BoardState`).
- **K2 - engine:** canonicalization (§3.5) in `propose_payload`; the
  shape check at both doors (`propose_payload`, `net/ingest.rs:239`); the
  precheck at propose; `kanban_cache`; supersede on the K0 hook;
  `BoardView` in `snapshot`; `Reply::Proposed.minted`; `read_state`
  description. Keystones: a `ref`/`@` payload arriving over the wire is
  dropped; a pending changeset on an item another vote closed goes
  Conflict; `touched_rev` flags the overwrite.
- **K3 - UI real:** sample data out, basket + rescue in, act summaries on
  cards and in the decision chat ("move #aa07c9d3 to review"), MockBadge
  off, `is_implemented()` true, wizard checkbox AND Organization panel
  unlocked (`app.slint:3957`, `:9815`; charter_features D1 re-check).
  Headless GUI tests under live-preview.
- **K4 - drafts:** `kanban_draft.json` (§5.1).
- **K5 - cross-refs:** `quest:` spans in the wiki walk (wiki.rs:3071),
  backlinks, the maintenance-read exclusions (§4).
- **K6 - verification:** two-instance loopback over the real governance
  path (propose/approve → identical board on both), void + supersede +
  rescue, a checkpoint cut keeps the fold, a hard-kill reopen reproduces a
  Conflict verdict; clippy 0 per crate; `scripts/check-doc-refs.py` clean;
  the doc moves to `docs_archive/` with its status line corrected and the
  `known_debt.md` Story-14 entry updated.
- **K7 - MCP polish** only if §8 Q10/Q11 say so. **K8 - folded cut** only
  if §8 Q9 says so.

Non-goals (deliberate): manual card ranking, WIP-limit enforcement,
burndown/velocity charts, notifications or date-driven automation, time
tracking, rewards/bounties, per-member permissions (agents-are-seats),
cross-reference integrity enforcement, recurring items.

## 8. Open questions

Q1-Q7 are unchanged from revision 1 and still unanswered; Q8 is closed by
the landed mock; Q9-Q11 are new. Each carries a recommendation, the
counterargument first.

1. **Vote fatigue.** Every `move` is m-of-n. Against the strict rule: a
   five-seat team with a 3-of-5 threshold doing ten moves a day casts
   thirty approvals a day, and the `vote_pending` hook wakes every agent
   seat for each. For it: the first ungated write to a gated surface is a
   precedent that erodes the only authority there is. *Recommendation:*
   keep it strict, ship K1-K6, measure on a real republic before
   designing an exception.
2. **Rewards/bounties.** Dead for now. Wallet Stage 1 cannot spend
   (`docs/chain/wallet_treasury_design.md` rev 2; spending waits on SA+L,
   Stage 2), so a bounty field could not pay out anyway.
   *Recommendation:* no field now; revisit after Wallet Stage 2.
3. **WIP limits.** *Recommendation:* no - enforcement is a non-goal and a
   display-only limit is a local view setting, not governance.
4. **Points scale.** Free 1..=100 or Fibonacci at ingest? *Recommendation:*
   free; a ladder is team convention, and ingest rules are forever.
5. **Parent auto-close.** *Recommendation:* human vote; the drill-in shows
   "all children closed".
6. **Manual ranking.** *Recommendation:* no, until the derived order
   demonstrably hurts.
7. **PI registry.** *Recommendation:* plain label until someone needs PI
   objectives.
8. ~~View labels~~ - landed as "Planning" / "Mine" with the mock; closed.
9. **Folded cut (new).** Accumulate through K6 and decide K8 on measured
   growth, or fold from day one? *Recommendation:* accumulate; a kanban
   changeset is ~1-50 KiB, so years of daily votes stay below what the
   wiki carried before K6.
10. **Read-only key (new).** Should a member's read-only agents see the
    board (`kanban_get`, Read scope)? Against: the user narrowed that key
    deliberately; a plan reveals who works on what. For: an agent that
    reads the wiki for context will want the plan beside it.
    *Your decision; default: no.*
11. **Typed write tool (new).** Generic `propose` with canonicalization
    (§3.5), or a dedicated `kanban_propose` with a per-act JSON schema?
    *Recommendation:* generic first (no new surface area); add the typed
    tool only if agent sessions show malformed changesets in practice.

## 9. Changes against revision 1 (2026-08-16)

- §1 re-verified; corrected: the Organization panel is locked too,
  `view_glyph` is `view_icon` in `labels.rs`, Quests sits in the frozen
  checkpoint set.
- New: canonical dates via chrono (§2.1), per-field caps (§2.2), ids
  minted at propose + local refs (§3.5), the propose-time precheck
  (§3.3), `touched_rev` and the overwrite warning (§3.6), the generalized
  supersede walk as K0, `Reply::Proposed.minted`, `BoardView` in
  `snapshot`, the read-key stance.
- Corrected: wire ingest does not check wiki patches (the both-doors
  precedent is Files); there is no `applies_cleanly` (the wiki's is
  `wiki_patch_applies`); the wiki has a `[[…]]` dialect, so §4 adds a
  target scheme only; checkpoints fold since K6 (§3.7); the read key
  cannot call `read_state` (§5.4).
- §6 reduced to the as-built inventory; the mock spec is history.
