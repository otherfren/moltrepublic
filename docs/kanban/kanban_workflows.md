# Kanban — planning, scheduling and the gated board

**Status: CONCEPT for discussion (2026-08-16). The §6 mock rework is BUILT
(landed 2026-08-16); the state model and ops (§2-§5) are a proposal awaiting
ratification — the §8 questions gate any backend build (§7).**

The ask: the Quests surface (GUI label **"Kanban"**, wire key `quests` —
`docs_archive/ritual/charter_features.md` §5.1; there is no `kanban` feature
key) grows from a bounty-board design mock into a real planning surface:
epics → stories → tasks, scheduled into sprints, operable by humans over the
GUI and by agents over MCP, with **every** change a threshold-approved
proposal. This document is the concept for that product — the architectural
template is the Shared Memory build
(`docs_archive/memory/shared_memory_real.md`): a deterministic fold over
applied payloads on the persistent chain, drafts local, one co-equal
command surface.

Read first: `docs_archive/ui/mock_todo.md` story 14 (Quests is one of its four
surfaces), `docs_archive/memory/shared_memory_real.md` (the template this
follows), `docs_archive/chain/persistent_chain.md` (the state model),
`docs_archive/ritual/charter_features.md` (how the surface is gated).

## 1. Where we stand

Real today (2026-08-16, master):

- `Surface::Quests` exists with sub-views `board / create / proposals /
  my-quests / archive` (`crates/molt-core/src/lib.rs`, `Surface::views`).
  It is an optional charter feature: off by default, locked off in the
  founding wizard ("not built yet", `crates/molt-ui-window/ui/app.slint`),
  but a running republic can already vote it on via `set_features` (the
  Organization features panel enables the checkbox when the feature is off).
- The governance loop is real and generic: `Command::Propose {surface:
  Quests, payload}` runs the same m-of-n threshold as every gated surface
  (approve / decline / withdraw → `ChainChange::Applied` block), reachable
  co-equally via the MCP `propose`/`approve`/`decline` tools. The
  `proposals` sub-view already routes to the REAL pending/decided tables in
  `app.slint` (only the other views render the mock pane).
- But there is **no surface state**: nothing folds, an Applied changes
  nothing, the pane shows sample data.

Mock today — the current `QuestsPane`
(`crates/molt-ui-window/ui/surfaces.slint`), quoted in structure:

```
struct MockQuest { title, detail, reward /* "5 XMR" */, who, when, done }
QuestsPane(view):
  board    → 3 QuestColumns: Open (accent) / Claimed (warn) / Done (good),
             each a header (DotLabel + count) over scrolling QuestCards
             (title, detail, reward TagChip 🪙, GlyphCaption 👤 who · when)
  create   → PanePanel form: title, description, reward, deadline;
             disabled "Propose" (not-implemented-yet tooltip)
  my-quests→ claimed rows with a disabled "Mark done"
  archive  → 44px rows: ✅ title, "reward → who · when"
```

That is a **bounty board** (put forward, claim, reward in XMR). This concept
replaces the model: planning Kanban with mandatory ownership and dates;
rewards are cut (→ §8 Q2). The shared chrome it uses — `MockBadge`,
`PaneHeader`, `PanePanel`, `TagChip`, `SigMeter`, `GlyphCaption`,
`DotLabel`, `ToolStrip`/`ToolBtn` — is the design vocabulary §6 keeps.

## 2. Target model

### 2.1 Items

Three kinds, one item table: **epic → story → task**.

- `parent` is optional but kind-checked when set: a task's parent is a
  story, a story's parent an epic, an epic has none. A standalone task or
  story is legal (a small republic's one-off chore must not require an epic
  of one).
- Item ids are random 128-bit lowercase hex, minted engine-side when the
  proposal is assembled (the chat `MessageId` precedent — `molt-core` stays
  RNG-free). Display form: `#` + first 8 hex chars. Ids are what wiki pages
  and dependencies cite, which is why items are never resurrected under a
  new id (→ `reopen`, §3).

**Every item of every kind always carries** (the non-negotiable trio):

- `responsible` — exactly ONE roster seat (`MemberId`, the roster name).
  The roster is fixed from founding (seat-adding is won't-do), so the
  validation set is stable and deterministic. No unassigned items, no
  multi-assignment: shared responsibility is a story with tasks.
- `start` — planned start, ISO `YYYY-MM-DD`.
- `due` — expected end, ISO `YYYY-MM-DD`, `start <= due`.

Dates are **planning data, display-only**: nothing executes on a date, and
"overdue" is a local-clock rendering (warn/bad tone), never consensus state.

### 2.2 The template (per kind)

One field set, Scrum/SAFe-standard; per-kind differences are minimal and
listed:

| field | meaning | epic | story | task |
|---|---|---|---|---|
| `title` | one line, <= 200 chars | required | required | required |
| `responsible` / `start` / `due` | §2.1 trio | required | required | required |
| `details` | description, markdown (wiki links live here, §4) | required | required | required |
| `ready` | precondition — Definition of Ready | optional | optional | optional |
| `done` | postcondition — acceptance criteria / Definition of Done | optional | recommended | recommended |
| `scope` | delimitation — explicitly out of scope | optional | optional | optional |
| `deps` | item ids this depends on (finish-to-start hint, display-only) | opt | opt | opt |
| `priority` | `low \| normal \| high \| critical` | required | required | required |
| `points` | estimate (story points, integer 1..100) | — (roll-up) | required | required |
| `status` | board column, §2.3 | required | required | required |
| `sprint` | sprint id (§2.4), for scheduling | — | optional | optional |
| `pi` | program-increment label, free string | optional | — | — |

An epic stores no points; the board and drill-in display the roll-up of its
descendants' points (computed on read, never stored). `deps` are opaque id
strings — rendered with an existence check at display, never enforced by a
scheduler (cross-ref integrity stays best-effort, the wiki non-goal).

### 2.3 Status columns and closing

`status ∈ backlog | ready | doing | review | done` — the five board
columns, all kinds. A `create` always lands in `backlog` (a changeset may
include the `move` in the same vote). Closing is separate from the `done`
column: `close {outcome: done | dropped}` removes the item from the board
into the Archive; ops on a closed item are void except `reopen`.

### 2.4 Sprints

Scheduling needs a shared sprint vocabulary, so sprints are governed rows
of the same fold: `{id, name, start, end, pi}` — upserted by the `sprint`
act (§3), never deleted (a sprint ends by its `end` date). `pi` groups
sprints into a SAFe program increment as a plain label; PIs get no registry
of their own until someone needs one (§8 Q7).

### 2.5 Invariants (the shared-memory template, restated)

- **The board is a deterministic fold.** `fold(empty board, applied
  kanban payloads in chain order) → BoardState`. Same chain → byte-identical
  board on every node, live, after replay, after a checkpoint cut. Same
  keystone class as the wiki fold.
- **Ephemeral vs persistent boundary.** The plan basket (§5.1) and form
  drafts are local and flüchtig; only the threshold-approved changeset
  becomes durable republic knowledge.
- **Sign-what-you-see.** Members ratify the exact acts the proposal card
  and decision chat render. Unlike wiki patches, kanban acts carry
  **absolute values** (never diffs/hunks), so there is no context-mismatch
  machinery: chain order arbitrates concurrent edits, the later applied
  absolute value wins.
- **Additive evolution.** New acts/fields follow the `WorkspaceEvent` rule;
  an unknown act voids the whole changeset (§3.4) — ship fold changes to
  all nodes together (the wiki fold-rule stance).

## 3. Ops — one payload, ordered acts

### 3.1 The payload

One op, mirroring `wiki_patch`: a proposal's payload is a **changeset** —
an ordered list of acts, voted as ONE m-of-n decision, applied
all-or-nothing by the fold. Batching is deliberate: sprint planning is one
vote, not thirty.

```json
{
  "op": "kanban_ops",
  "summary": "S9 planning: 1 story, 3 tasks scheduled",
  "base_rev": 17,
  "ops": [ ... 1..128 acts ... ]
}
```

`base_rev` is the fold revision the proposer saw — a **display-only**
staleness hint (the wiki §9.1 decision verbatim): the card warns "board
moved since proposed", the fold never reads it.

### 3.2 The acts

Dedicated act names instead of one generic patch, so proposal cards and the
decision chat render honest one-line summaries ("move #aa07c9d3 to review"):

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
  done, scope, deps`. The governed trio and status move ONLY through their
  dedicated acts (`assign`, `schedule`, `move`) — an edit can never bury a
  responsibility change in a text tweak.
- `schedule` on an epic sets `start`/`due`/`pi` (no sprint on epics).
- `reopen` is in the vocabulary on purpose: without it a resurrected item
  needs a new id, and a new id rots every `quest:` citation in the wiki and
  every `deps` entry — id stability is what makes §4 references durable.
- `sprint` is an upsert by id (create or amend); chain order arbitrates.

### 3.3 Validation, split exactly like the wiki

**Ingest-reject** (at `propose` AND at wire ingest — the `set_image`
pattern; never recorded): payload not object-shaped, unknown `op`, empty or
oversized `ops` (>128 acts), missing required fields per §2.2, malformed
date, `start > due`, unknown enum value (`kind`, `priority`, `to`,
`outcome`), title empty or > 200 chars, `edit.set` naming a reserved field,
points out of 1..100. The transport budget stays where it is:
`payload_fits` bounds the whole proposal at propose/ingest, never in the
fold.

**Fold-VOID** (deterministic, whole changeset, all-or-nothing — a
half-applied plan is a plan nobody proposed): `create` with an id that
already exists (in-changeset duplicates included), any act naming an
unknown item/sprint id, parent kind mismatch, `responsible` not a roster
seat, any act except `reopen` on a closed item, `reopen` on a non-closed
item, unknown `act`. Void is a fold verdict, never chain data: the block
stays applied, the Accepted row carries the quiet superseded marker — the
wiki rule unchanged.

**The supersede walk applies unchanged** (shared_memory_real §4): after
every Applied and at late proposal registration, every node re-checks
pending `kanban_ops` changesets with the fold's own `applies_cleanly`; a
changeset that would now void (e.g. it edits an item another vote just
closed) transitions to the terminal, unattributed SUPERSEDED outcome —
Denied view, "superseded - board moved (#id)", rescuable by anyone into
the local basket (§5.1). The fold stays the final arbiter for race-sealed
blocks.

### 3.4 The fold result

```
BoardState {
  rev: u64,                                  // applied changesets folded
  items: BTreeMap<ItemId, Item>,             // open AND closed
  sprints: BTreeMap<SprintId, Sprint>,
}
```

- Served through the existing `ReadState {surface: quests}` — ONE read for
  GUI and MCP (the shared-memory WP-G outcome: no extra read tool needed).
- Column ordering is deterministic and derived: `(priority desc, due asc,
  id asc)`. **No manual card ranking** — a vote-gated drag-reorder is
  absurd and an ungated one breaks the model; revisit only if the sort
  demonstrably hurts (§8 Q6).
- Checkpoints: `kanban_ops` entries **accumulate** (`applied_lww_slot`
  already returns `None` for non-Organization surfaces — the conservative
  default is correct here). Known debt, same as the wiki's: the changeset
  history inside checkpoints grows forever; a future summarization could
  carry the folded board at the cut. Deferred until growth hurts.

## 4. Cross-references: Kanban ↔ Multisig-Wiki

**One syntax — the markdown inline link — in both directions.** The wiki
already renders `[label](target)` runs and keeps `.md` targets as clickable
preview spans (`crates/molt-ui/src/wiki.rs`: the pulldown walk in
`parse_blocks`/`push_run`, `parse_links`, and `open_link`'s
exact-path-then-unique-basename resolution). No `[[…]]` dialect, no second
syntax — the target grammar gains one scheme:

- **Item → wiki page.** Template text fields (`details`, `ready`, `done`,
  `scope`) are markdown; `[recovery runbook](runbooks/node-recovery.md)`
  renders as a preview link in the drill-in and opens the Memory surface at
  that doc, through the same `open_link` resolution.
- **Wiki page → item** (and item → item): `[the drill task](quest:0b6d42f7…)`
  — the span walk keeps `quest:<id>` targets as clickable spans alongside
  `.md` targets; click routes to the Kanban board drill-in. Resolution:
  exact id first, then unique id-prefix (the basename-fallback idiom, so a
  hand-written `quest:0b6d42f7` short form resolves).
- **Backlinks both ways are computed on read** (scan the folded wiki tree
  for `quest:` targets, scan `BoardState` text fields for `.md` targets):
  the drill-in shows "referenced by <pages>", a wiki doc's info strip shows
  the items citing it. Display-only; link integrity stays best-effort
  navigation (the wiki §7 non-goal, extended to `quest:` targets — a dead
  id renders muted, nothing blocks).

## 5. Workflows — humans over GUI, agents over MCP, co-equally

### 5.1 The plan basket (the local draft layer)

The wiki's changeset stack, transplanted: acts are **staged locally** into
a plan basket (create forms, drill-in "propose change" actions, board
move intents all append acts), reviewed as a list, then proposed as ONE
`kanban_ops` vote. The basket is per-workspace local state, persisted
sealed-at-rest next to the wiki drafts (the WP-D idiom), OUT of the backup
export. Rescue (§3.3) reloads a superseded/declined changeset's acts into
the basket.

### 5.2 Planning (epic → stories → tasks)

GUI: Create view, kind Epic → fill template → "Add to basket"; repeat for
stories (parent = the epic's id, still local — ids are minted at staging
time, so intra-basket parents work); tasks likewise; review basket; Propose.
Then the normal decision flow: decision chat deliberates, members approve,
at *m* the engine seals the Applied block, every node folds, the board
shows the plan.

MCP (an agent driving its human's seat — same commands, same threshold):

```
propose {surface:"quests", payload:{op:"kanban_ops", summary:"Q3 epic + 2 stories",
         base_rev:…, ops:[{act:"create",kind:"epic",…}, {act:"create",kind:"story",…}]}}
  → returns the proposal id
approve {proposal:<id>}          # each consenting seat, human or agent
read_state {surface:"quests"}    # the folded BoardState, same read the GUI uses
```

### 5.3 Scheduling (sprint planning)

One changeset: the `sprint` upsert minting the window, then `schedule` acts
assigning stories/tasks into it (dates adjusted in the same breath), plus
the `move` acts pulling them `backlog → ready`. The Planning view (§6.3)
renders the result; committed points per sprint are the read-side sum.

### 5.4 Execution (move / review / close)

The responsible seat proposes `move` acts as work progresses (`doing` →
`review` when it believes `done` criteria are met); review IS the vote —
the decision chat on the `move …, to:"done"` proposal is where acceptance
criteria are checked, and the threshold approving it is the acceptance. At
sprint end, one changeset closes finished items (`close`, outcome `done`)
and reschedules the spillover.

**Vote volume is a feature, not an accident**: every state change is m-of-n
by requirement (agents-are-seats — the threshold is the ONLY authority; no
roles, no owner fast-lane). The mitigation is batching (§3.1), not a
permission system. Whether that holds up in daily use is §8 Q1 — the first
open question, on purpose.

## 6. Mock rework (this wave)

This wave rebuilds the **design mock** only: `QuestsPane` in
`crates/molt-ui-window/ui/surfaces.slint`, sample data .slint-side, never
engine state, every header keeps the `MockBadge` (DESIGN MOCK) and every
state-changing button renders disabled with `Strings.not-implemented-yet`
(the pane idiom). No drag-and-drop (an ungated drag would fake a vote, and
Slint 1.17's DragArea is unusable anyway) — cards click into the drill-in.
No em dash and no plain-text emoji in any string (Twemoji font via
glyph/emoji props). Iterate under `scripts/dev-ui.sh`; the authoritative
check is one `cargo build -p molt-ui-window -p molt-ui`.

### 6.0 Views vocabulary (the one non-.slint touch)

`Surface::Quests.views()` becomes:

```
("board","Board") ("plan","Planning") ("create","Create")
("proposals","Proposals") ("my-quests","Mine") ("archive","Archive")
```

New key `plan`; `my-quests` keeps its wire key (select_view vocabulary) and
gets the label "Mine" — "My Quests" is bounty vocabulary on a
Kanban-labelled surface. Ripple sites: `view_glyph` (`crates/molt-ui/src/lib.rs`,
add `"plan" => "🗓️"`), `view_label` German map (add `"plan" => "Planung"`,
change `"my-quests" => "Meine"`), the MCP `select_view` tool description
listing quests' views (`crates/molt-mcp/src/lib.rs`). The wizard checkbox
stays locked off (backend unbuilt); the pane stays reachable in a republic
that voted `quests` on, and under dev-ui.

### 6.1 Sample data (one coherent dataset)

Cast: petra (the local member), walter, mara, jonas. "Today" is 2026-08-16.
Sprints (all `pi: "PI-3"`): S8 `2026-08-05..2026-08-16` (current), S9
`2026-08-17..2026-08-28`, S10 `2026-08-31..2026-09-11`.

| id | kind | title | parent | resp. | start → due | pts | prio | sprint | status |
|---|---|---|---|---|---|---|---|---|---|
| 3fa1c802 | epic | Public onboarding path | — | petra | 08-05 → 10-09 | (16) | high | PI-3 | doing |
| 9b04d1ee | epic | Treasury operations | — | walter | 08-17 → 11-20 | (11) | normal | PI-3 | ready |
| 51c2ab07 | story | Onboarding guide v1 | 3fa1c802 | mara | 08-17 → 08-28 | 8 | high | S9 | ready |
| 7e99f0c4 | story | Invite flow dry-run | 3fa1c802 | jonas | 08-05 → 08-16 | 5 | normal | S8 | review |
| 20d5b3a1 | story | Reading list curation | 3fa1c802 | petra | 08-05 → 08-14 | 3 | normal | S8 | done |
| c4188e5b | story | Multisig runbook | 9b04d1ee | walter | 08-31 → 09-11 | 8 | normal | S10 | backlog |
| aa07c9d3 | task | Draft chapter: first signed proposal | 51c2ab07 | mara | 08-17 → 08-21 | 3 | high | S9 | ready |
| 5f31e6b8 | task | Screenshots per surface | 51c2ab07 | jonas | 08-17 → 08-26 | 2 | normal | S9 | backlog |
| 0b6d42f7 | task | Recovery drill notes | 7e99f0c4 | jonas | 08-11 → 08-16 | 3 | normal | S8 | doing |
| d17f30c5 | task | Charter FAQ draft | 51c2ab07 | petra | 08-12 → 08-16 | 2 | normal | S8 | doing |
| 6c25a984 | task | Glossary pass | — | petra | 09-01 → 09-05 | 2 | low | — | backlog |
| e3b8071d | task | Second relay checklist | c4188e5b | walter | 08-31 → 09-04 | 3 | high | S10 | ready |
| 4a90cc16 | task | Emblem vector cleanup | — | mara | 08-10 → 08-15 | 1 | low | S8 | review |

`5f31e6b8` carries `deps: [aa07c9d3]` (the one dependency demo).
`4a90cc16` is overdue (due < today → date in bad tone). Epic points are
the precomputed roll-up, shown as "16 pt".

Archive (closed): `77b2f4d9` task "Treasury multisig setup", walter, done,
closed 2026-07-14, 5 pt · `1e8ca6f3` story "Old reading list", petra,
dropped, closed 2026-07-02, 3 pt · `b5d30f28` epic "Found the republic",
petra, done, closed 2026-04-12.

Structs replace `MockQuest`/`QuestCard`/`QuestColumn`:

```
struct MockItem { id, kind /*0 epic 1 story 2 task*/, title, parent,
  responsible, start, due, overdue(bool), points, rollup(bool),
  prio /*0..3*/, sprint, pi, status /*0..4*/, details, ready, done-crit,
  scope, deps([string]), refs([string]) /* wiki backlink paths */ }
struct MockSprintRow { id, name, start, end, pi, points, current(bool),
  off(int), len(int) /* day offsets for the Planning scale */ }
```

Kind glyphs: epic 🏔️ · story 📘 · task 🔧. Priority tones: critical =
`Theme.bad`, high = `Theme.warn`, normal = `Theme.accent`, low =
`Theme.faint`.

### 6.2 Board

`PaneHeader` 📋 "Board" + MockBadge, hint: "The shared plan - every change
on it is a threshold vote." Under it a `ToolStrip` with pure view-state
filters (they really filter, mock-legal): three kind ToolBtns 🏔️📘🔧
(toggle each kind on/off) and a 🎯 "mine" toggle (responsible == petra).

Five `KanbanColumn`s (the `QuestColumn` layout: DotLabel header + count,
scrolling card list): Backlog (`Theme.faint`) · Ready (`Theme.accent`) ·
Doing (`Theme.warn`) · Review (`Theme.moved`) · Done (`Theme.good`).
Cards sort by (priority, due, id) within a column — precompute the order in
the sample arrays.

`KanbanCard` (from `QuestCard`): row 1 = kind glyph (fixed 18px clip box) +
title bold, wrap; row 2 = chips: `TagChip` 👤 responsible · `TagChip`
"3 pt" (roll-up renders "16 pt" muted) · priority `TagChip` label in the
priority tone (muted style for low); a muted sprint chip ("S9") when set;
row 3 = `GlyphCaption` 🗓️ "2026-08-17 → 2026-08-21" (label color
`Theme.bad` when overdue, else `Theme.faint`). Whole card is a TouchArea →
sets the pane-local `selected-item`; done-column cards keep the 0.62
opacity idiom.

**Drill-in** (selected-item != -1 replaces the columns, in-pane): a top
`ToolStrip` with a ← "Back" ToolBtn (enabled, clears selection); then a
`PanePanel` with: title row (kind glyph + title + "#51c2ab07" faint caption
+ status DotLabel in its column tone); a meta grid of caption/value pairs —
Responsible, Start, Due, Points (or roll-up), Priority, Sprint/PI, Parent
(clickable caption → selects the parent), Depends on (id chips, muted when
unknown); children list for epic/story (rows: status dot + title +
responsible, clickable); then the template sections, each `Theme.fs-cap`
dim heading + body text: Details, Definition of Ready, Acceptance criteria,
Out of scope. Markdown links in the sample bodies render as accent-colored
spans (static color in the mock — no navigation); a "Referenced by" caption
row lists `refs` paths. Footer: primary AppButton 📋 "Propose change",
disabled, `Strings.not-implemented-yet`.

Sample drill-in texts for `51c2ab07` (write them into the sample):
details: "Turn the reading list into a guided path - see
[onboarding](guides/onboarding.md)."; ready: "Reading list curated
(#20d5b3a1 done); guide skeleton agreed in chat."; done-crit: "A new member
reaches their first signed proposal with no help beyond the guide.";
scope: "No video material; no translation."

### 6.3 Planning

`PaneHeader` 🗓️ "Planning" + MockBadge, hint: "Sprints and dates - where
the work is scheduled." Two-pane `HorizontalLayout`:

- Left (fixed ~240px), sprint list: one `Theme.panel-2` card per
  `MockSprintRow` — name bold + `DotLabel` good "current" on S8, window
  caption "2026-08-05 → 2026-08-16", "13 pt committed" caption (precomputed
  sum), muted PI chip.
- Right, the timeline `PanePanel`: axis 2026-08-01..2026-09-30 (60 days,
  `px-per-day = inner-width / 60`); sprint boundaries as hairline
  `Theme.line` verticals with S8/S9/S10 `fs-cap` labels on top. One row per
  scheduled item, epics first then stories then tasks (indent 12px per
  level): elided title (fixed 160px) + a bar Rectangle (`x = off *
  px-per-day`, `width = len * px-per-day` from the precomputed day offsets)
  — epics as accent-soft fill with accent border, stories/tasks as thinner
  bars in their status-column tone; a due date past the axis clips with a →
  glyph at the edge (E2). Bottom caption row: "not scheduled: 1 item"
  (items with no sprint — `6c25a984`).

### 6.4 Create

`PaneHeader` ✨ "Create" + MockBadge, hint: "Draft an epic, story or task -
proposing starts a vote." `PanePanel` form (the qb-create idiom: fs-cap
caption over `AppField`/`AppArea`):

- Kind selector: three toggle AppButtons Epic/Story/Task (view-state,
  working). Switching hides Points for Epic and swaps Sprint ↔ PI.
- Row: Title (stretch). Row: Parent ("#id - optional") · Responsible
  ("member"). Row: Start ("2026-08-17") · Due ("2026-08-28"). Row: Points
  ("3") · Priority (four `TagChip`-styled toggles Low/Normal/High/Critical,
  view-state) · Sprint ("S9") or PI ("PI-3").
- Four `AppArea`s (height 90px each), captions Details / Definition of
  Ready / Acceptance criteria / Out of scope, placeholders (kind-neutral):
  "What and why - markdown, [links](notes/page.md) allowed" · "Ready when: what
  must exist before work starts" · "Done when: verifiable acceptance
  criteria" · "Explicitly not part of this item".
- Row: Depends on ("#id, #id").
- Footer: primary AppButton 📋 "Propose", disabled,
  `Strings.not-implemented-yet`.

### 6.5 Proposals

Unchanged this wave: the sub-view keeps routing to the REAL governance
tables in `app.slint` (pending cards with `SigMeter` + signer `TagChip`s,
decided tables). The per-act summary rendering in proposal cards and the
decision chat is backend work (§7), not mock work.

### 6.6 Mine

`PaneHeader` 🎯 "Mine" + MockBadge, hint: "Items you are responsible for."
`PanePanel`, petra's open items grouped by status (Doing, Review, Ready,
Backlog — sections as `DotLabel` headers in the column tone, done/closed
omitted): rows reuse the my-quests card layout — title bold, dates
`GlyphCaption` 🗓️ (+ points 🔢? no: a muted "2 pt" `TagChip`), and a
right-side disabled AppButton per row naming the honest next act:
"Propose: move to review" (doing rows) / "Propose: move to done" (review
rows), `Strings.not-implemented-yet`. Sample rows: `d17f30c5`, `6c25a984`.

### 6.7 Archive

`PaneHeader` 🗄️ "Archive" + MockBadge, hint: "Closed items - done or
dropped." The existing 44px-row idiom: outcome glyph (✅ done · 🚫
dropped, Twemoji clip box), kind glyph, title (dim, elide, stretch), right
caption "responsible · closed YYYY-MM-DD · N pt".

### 6.8 Files and strings

Touched: `crates/molt-ui-window/ui/surfaces.slint` (QuestsPane rework, new
structs/components, sample data), `crates/molt-ui-window/ui/theme.slint`
(Strings: the `qb-*` block is replaced by `kb-*`), `crates/molt-ui/src/lib.rs`
(i18n macro entries EN/DE, `view_glyph`, `view_label`),
`crates/molt-core/src/lib.rs` (`Surface::views` per §6.0),
`crates/molt-mcp/src/lib.rs` (`select_view` description). Remove the dead
`qb-*` strings in the same change.

New strings (EN / DE — every string states one thing and stops):

```
kb-title-board    Board / Board          kb-hint-board  see §6.2
kb-col-backlog    Backlog / Backlog      kb-col-ready   Ready / Bereit
kb-col-doing      Doing / In Arbeit      kb-col-review  Review / Review
kb-col-done       Done / Fertig
kb-title-plan     Planning / Planung     kb-hint-plan   see §6.3
kb-title-create   Create / Erstellen     kb-hint-create see §6.4
kb-title-mine     Mine / Meine           kb-hint-mine   see §6.6
kb-title-archive  Archive / Archiv       kb-hint-archive see §6.7
kb-kind-epic      Epic / Epic            kb-kind-story  Story / Story
kb-kind-task      Task / Task
kb-f-parent  Parent / Übergeordnet       kb-f-resp   Responsible / Verantwortlich
kb-f-start   Start / Start               kb-f-due    Due / Fällig
kb-f-points  Points / Punkte             kb-f-prio   Priority / Priorität
kb-f-sprint  Sprint / Sprint             kb-f-pi     PI / PI
kb-f-deps    Depends on / Hängt ab von   kb-f-refs   Referenced by / Verwiesen von
kb-sec-details Details / Details         kb-sec-ready Definition of Ready / Definition of Ready
kb-sec-done  Acceptance criteria / Akzeptanzkriterien
kb-sec-scope Out of scope / Nicht enthalten
kb-prio-low  Low / Niedrig               kb-prio-normal Normal / Normal
kb-prio-high High / Hoch                 kb-prio-critical Critical / Kritisch
kb-ph-title / kb-ph-parent / kb-ph-resp / kb-ph-date / kb-ph-points /
kb-ph-deps / kb-ph-details / kb-ph-ready / kb-ph-done / kb-ph-scope  (§6.4)
kb-propose   Propose / Vorschlagen       kb-propose-change Propose change / Änderung vorschlagen
kb-back      Back / Zurück               kb-mv-review Propose: move to review / Vorschlagen: nach Review
kb-mv-done   Propose: move to done / Vorschlagen: nach Fertig
kb-rollup    rolled up / aufsummiert     kb-committed  committed / eingeplant
kb-current   current / aktuell           kb-unscheduled not scheduled / nicht eingeplant
kb-closed-done done / fertig             kb-closed-dropped dropped / verworfen
```

## 7. Backend build order (once §2-§5 are ratified — NOT this wave)

TDD, red first, the shared-memory work-package shape:

- **K1 — core fold** (`crates/molt-core/src/kanban_fold.rs`):
  `kanban_fold(applied) -> BoardState` + `applies_cleanly`. Keystones: fold
  determinism (live == replay == snapshot), void-on-invalid all-or-nothing,
  unknown-act void, a byte-pin fixture board.
- **K2 — engine**: BoardState projection on the applied concat, ingest
  validation (propose + wire), the supersede walk reusing `applies_cleanly`,
  `ReadState{quests}` serves the board. Co-equality test extends by itself
  (`propose` is already a tool; no new Command).
- **K3 — UI real**: sample data out, basket + rescue in, proposal-card /
  decision-chat act summaries, MockBadge off, wizard checkbox unlock
  (charter_features D1 re-check).
- **K4 — drafts**: basket persisted sealed-at-rest (WP-D idiom, out of
  backup export). **K5 — cross-refs**: `quest:` spans in the wiki walk +
  backlink computation. **K6 — verification**: two-instance loopback
  (propose/approve/fold-identical), void + supersede + rescue paths,
  checkpoint cut keeps the fold, clippy 0, doc-refs clean.

Non-goals (deliberate): manual card ranking, WIP-limit enforcement,
burndown/velocity charts, notifications or date-driven automation, time
tracking, rewards/bounties, per-member permissions of any kind
(agents-are-seats), cross-reference integrity enforcement, recurring items.

## 8. Open questions (this doc starts the discussion)

1. **Vote fatigue.** Every `move` is m-of-n; batching is the only
   mitigation offered. Does daily use bear that, or do we want a
   deliberate exception (e.g. the responsible seat's own `doing → review`
   as an ungated signal)? Counterargument to any exception: the threshold
   is the republic's ONLY authority — the first ungated write to a gated
   surface is a precedent.
2. **Rewards/bounties.** The old quest model (claim + XMR reward) is cut
   here. Does it return later as a Wallet coupling (a bounty field paying
   out on `close done`), or is it dead?
3. **WIP limits.** Classic kanban caps per-column WIP. A governed limit
   would be a natural `sprint`-like registry value — worth it, or noise?
4. **Points scale.** Free integer 1..100, or enforce the Fibonacci ladder
   at ingest?
5. **Story/epic auto-close.** When all children are closed, should the
   parent close mechanically (a chain-derived lifecycle verdict, so
   automation would be legal by the shared-memory rule) — or does closing
   stay a human vote? Proposal: human vote; the drill-in shows "all
   children closed" as a hint.
6. **Manual ranking.** §3.4 fixes column order to (priority, due, id). Is
   a rank field (vote-gated reorder) ever wanted?
7. **PI registry.** `pi` is a plain label on sprints/epics. Does SAFe-style
   PI planning need first-class PI rows (windows, objectives)?
8. **View labels.** "Mine" for `my-quests`, "Planning" for `plan` (§6.0) —
   veto here if the wording is wrong, it lands with the mock wave.
