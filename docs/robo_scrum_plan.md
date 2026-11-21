# Robo-Scrum - implementation plan

**Status: PLAN, open work, not built (2026-11-21).** Implements the
concept in `docs/robo_scrum.md` (cited below as C§n). Nothing here is
built. The decisions in §2 were settled with the user on 2026-11-21;
the first step is S1.

## 1. What exists (inventory 2026-11-21)

The Kanban is built end to end and goes; sizes are `wc -l`.

| layer | files | lines | fate |
|---|---|---|---|
| core | `molt-core/src/kanban_fold.rs` (+tests) | 1235 + 884 | replaced by `board_fold.rs`; structure reused |
| core | `kanban_calendar.rs` | 1138 | deleted except `parse_datetime` / `fmt_datetime` |
| core | `kanban_dates.rs`, `kanban_review.rs`, `kanban_view.rs`, `kanban_wake.rs` (+tests) | 332 + 450 + 586 + 223 (+1149) | deleted; view and wake rewritten small |
| engine | `molt-engine/src/kanban.rs` (+tests) | 286 + 360 | replaced by `board.rs` |
| engine | `wake.rs` (+tests) | 560 + 748 | kept; reasons and the timer scan change |
| mcp | `molt-mcp/src/quests.rs`, `wake_skill.md` | 278 + 397 | rewritten |
| ui (Rust) | `kanban.rs`, `kanban_views.rs`, `actions/kanban.rs` | 1052 + 754 + 1047 | replaced |
| ui (Slint) | `surfaces.slint` 2392-4271, `theme.slint` 1052-1339 + 2125-2244 | ~2290 | replaced |
| tests | core ~81, engine ~40, mcp ~14, ui 71 | | rewritten per step |

Hooks the new code keeps using, verified in the code:

- **Fold shape.** `kanban_fold_one` returns `Applied | Void | Skipped`;
  any op but `kanban_ops` is `Skipped`, neither `rev` nor the board
  moves. A changeset applies on a clone and is void on the first fault.
  The engine holds the folded board in `State.kanban_cache`, refolds
  from empty when the applied epoch moves, and exposes it as
  `SurfaceSnapshot.board`.
- **Both doors.** `kanban_canonicalize` (lenient input, mints ids,
  resolves `@ref`) at propose, `validate_kanban_wire` plus the roster
  check at ingest (`net/ingest.rs`, engine `kanban_wire_check`).
- **Checkpoints.** `applied_lww_slot` is `None` for Quests: entries are
  archived whole, and the fold re-runs over archive plus new blocks
  after a cut (`tests/kanban_two_nodes.rs` pins it).
- **Signing.** `sign_blocker` (`chain/governance.rs:195`) is consulted
  by `cmd_approve`, by the proposer's own co-signature and by the
  re-sign after a rebase - the one place a node refuses to sign. There
  is no Quests arm today.
- **Clock.** `presence_now()` (`molt-engine/src/lib.rs:1745`), with the
  test-only `clock_override`. Nothing on propose or approve takes a
  `now` parameter.
- **Wakes.** `wake_trigger(reason, by)`; reasons `poked`,
  `vote_pending`, `kanban`, `task_start`; `task_timer` on the 1 s
  `NetDeliveryTick` is a stored-timestamp wake with a persisted
  fired-set (`kanban_wakes.json`).
- **Drafts.** The Kanban `Basket` (`molt-ui/src/kanban.rs:552`) stages
  acts locally, survives in `Command::KanbanDraftSave`/`Load`, and
  `rescue(id)` copies a decided changeset back into the draft.
- **Navigation.** The chat-channel block in `app.slint` (a heading
  `Text` plus `for c in model: ViewRow`) is the only dynamic nav list.

Three findings that shape the plan:

1. **A new wire event kind is not free.** An older build cannot decode
   an unknown `WorkspaceEvent` or `ChannelRef` variant: it discards the
   frame, never acks it, parks that sender's later events for up to
   900 s, and refuses to open a log that contains one. Pokes are a
   control frame for exactly this reason (`chat.rs` `send_poke`).
2. **Nothing references another proposal today**, and `Proposed` cards
   do not age; only rejected ones do.
3. **The GUI has no tree table, no progress bar, no blue token and no
   nav row with a second line.** Each is new, small work.

## 2. Decisions (settled 2026-11-21)

| # | question | decided |
|---|---|---|
| D1 | How do task marks travel? | A new log event, emitted only once every roster seat has announced the capability (`seat_caps`, the mechanism of `recovery_auto_approval.md` §7e). It rides the delivery guarantee and the dump. The alternative, a control frame, is safe for old builds but has no resend and no persistence. |
| D2 | What is the story chat on the wire? | A `Topic` channel with the reserved name `story:<id>`. No wire change; an old build shows an ordinary topic. A fourth `ChannelRef` variant would hit finding 1 and touch ~40 match sites. Cost: the topic thread rules (expiry, close line) apply to it. |
| D3 | Is a package void when the board moved since it was drafted? | No. The Kanban voids on a stale `base_rev`; with versions handed on over days that would void almost every package. A package is void only if it no longer applies to the state it meets. `base_rev` stays as a display hint. |
| D4 | What may a package remove? | Everything: any task, story or board, in any state (C§2). Only the general board stays. The blocker rule holds for what is still waited for. |
| D5 | Tool names | `board_view`, `board_propose`, `board_mark`; `read_actions`, `wake_skill`, `test_wake` keep their names. The `quests_*` names go with the Kanban. |
| D6 | Wake settings | Two reasons replace `kanban` and `task_start`: `board` (the three voted events of C§8) and `wait_due`. The config reader maps the old names onto them. |

## 3. Library verdict

C§12 already records it for the model: every reference tracker is its
own store, and here the chain is the store. For the build:

- **Dates:** `chrono`, already in the workspace, behind the two parse
  and format functions kept from `kanban_calendar.rs`. The RRULE
  expansion goes: there is no series (C§2.5).
- **Shared editing:** not needed, by decision (C§6, versions handed on).
  No CRDT.
- **Graphs:** cycle detection over two small graphs; `find_cycle` in
  the Kanban fold is 30 lines and is carried over. `petgraph` would be
  a dependency for one function.

## 4. Design

### 4.1 Payload

Wire key `quests`, op **`board_ops`** (must differ from `kanban_ops`,
C§10).

```json
{"op":"board_ops","summary":"one line","base_rev":12,"replaces":"<proposal id>",
 "acts":[
  {"act":"board_add","id":"<hex32>","title":"…","details":"…","state":"planning","waits_for":[]},
  {"act":"board_set","id":"…","fields":{"title":"…","details":"…","state":"execution","waits_for":["…"]}},
  {"act":"story_add","id":"…","board":"…","title":"…","acceptance":"…","out_of_scope":"…",
   "preconditions":"…","repeat":"monthly","waits_for":["…"],
   "tasks":[{"id":"…","title":"…","details":"…","human":true}]},
  {"act":"wait_add","id":"…","board":"…","title":"…","at":"2026-12-01T00:00"},
  {"act":"story_set","id":"…","fields":{"title":"…","waits_for":["…"]}},
  {"act":"story_move","id":"…","board":"…"},
  {"act":"story_close","id":"…","verdict":"done"},
  {"act":"task_add","id":"…","story":"…","title":"…","details":"…","human":false},
  {"act":"task_set","id":"…","fields":{"title":"…","human":true}},
  {"act":"task_remove","id":"…"},
  {"act":"story_remove","id":"…"},
  {"act":"board_remove","id":"…"}
 ]}
```

- Ids are 32 lowercase hex chars, minted by the engine at propose; the
  propose door also accepts `@ref` for elements of the same package.
- `at` is UTC, minute precision, the format `parse_datetime` reads.
- `replaces` is optional and names the proposal this version supersedes
  (C§6). It lives in the payload, so `ProposalRecord` and the proposal
  wire shape do not change. It decides nothing in the fold.
- The general board is not created by an act: the empty fold state
  holds it under the fixed id of 32 zeros, in planning, with an empty
  title that the view localizes.

### 4.2 Fold

`molt-core/src/board_fold.rs`, pure, no clock, no RNG.

- State: `BoardsState { rev, boards, stories, tasks }`, all `BTreeMap`
  by id. A story is `Work { … }` or `Wait { at }`.
- `board_fold_one(state, payload, seats) -> FoldStep`: any op but
  `board_ops` is `Skipped` - an applied `kanban_ops` entry included.
- A package applies on a clone and is void on the first fault:

| fault | rule |
|---|---|
| unknown id, duplicate id | every referenced element exists, every new id is free |
| closed target | no act but a removal touches a finished board, a done or discarded story, or their tasks |
| remove | the general board is never removed; a removed story takes its tasks, a removed board its stories; links to a removed done story or finished board go with it |
| move | only a waiting story moves (C§2); its `waits_for` must lie inside the target board after the package |
| finish | a board is finished only when every story on it is terminal (C§3) |
| blocker | a discarded story, and a removed story or board that is not done or finished, has no open dependent after the package (C§5) |
| cross-board link | `waits_for` of a story names stories of its own board only |
| cycle | none among the stories of a board, none among the boards |
| wait story | no tasks, no criteria; `story_close` of a wait story is `done` only |
| state step | planning <-> execution, execution -> finished; nothing leaves finished |

- `derive(state) -> Derived`: per story `waiting | in_progress | done |
  discarded` by C§4, and what each waiting story and board waits for.
  A pure function of the fold state - no clock anywhere.
- The time rule of the wait story is **not** in the fold (C§2.4).

### 4.3 The clock gate

The one security-relevant addition. A node refuses, by its own clock,
to help close a wait story early; the chain never checks.

- **Propose:** `prepare_board_proposal` refuses a package that closes a
  wait story whose `at` is after `presence_now()`.
- **Sign:** a Quests arm in `sign_blocker` refuses the same. This
  covers approve, the proposer's co-signature and the re-sign after a
  rebase.
- **Ingest:** no check. A proposal from a seat with a faster clock stays
  pending and is signed when this node's clock arrives.
- **Verify:** unchanged. A block that closes a wait story early is
  valid if m seats signed it - the security of any vote.
- Nothing proposes or signs by itself (C§1).

### 4.4 Wakes

- Reasons `board` and `wait_due` (D6).
- `board` fires from `after_block_applied` and the blob-adoption path,
  by comparing the board before and after the folded block: a board
  entered execution, a board went back to planning, a story arrived on
  a board in execution. A full refold fires nothing.
- `wait_due` replaces the `task_start` scan in `task_timer`: the next
  open wait story whose board is in execution and whose `at` is
  reached. The fired-set and its file stay.

### 4.5 Marks

- `Command::MarkTask { task, mark }`, mark `none | working | done |
  problem`; a tool on both surfaces (`board_mark`).
- Event `TaskMarked { task, mark, n }`. `by` is the MLS-authenticated
  sender, as for reactions. `n` is a per-seat counter carried in the
  body; the higher `n` wins per `(task, seat)`. The envelope order is
  not enough: the receiver re-records under its own seq, and in-order
  delivery gives up after 900 s.
- State: `board_marks` in `EngineStateDump`, `#[serde(default)]`.
- A mark is dropped when its story becomes terminal, when its task,
  story or board is removed, and at compaction when its task is
  unknown. It is not in
  the checkpoint and not in the recovery bootstrap.
- Emitted only when every roster seat announced the capability (D1);
  until then the command answers with one short fault.
- Progress (C§7) is computed on read from the fold state and the marks.

### 4.6 Reads and tools

- `SurfaceSnapshot.board` carries boards, stories, tasks, the derived
  states, the marks and the progress numbers.
- `board_view` (filters: board, `show` = everything | mine | problem |
  human), `board_propose`, `board_mark`; `read_actions` lists what a
  woken seat can do: votes waiting, wait stories due, stories in
  progress. `wake_skill.md` is rewritten for this loop.
- `ProposalView` keeps `rendered`, `advisories` and `void` for a
  `board_ops` proposal: the act lines, and "would be void: …" computed
  against the current board.
- Co-equality: `MarkTask` is a tool; the draft commands keep their
  variants and carry the new draft shape.

### 4.7 GUI

- **Navigation.** A block like the chat channels: three headings, one
  `BoardRow` per board (a fork of `ViewRow` with a state icon, a second
  line, a progress bar and the red dot). The finished group is folded.
- **Board pane.** A flat row model built in Rust - group heading, story
  row, task row - with the fold state per row, as `KDeps` and the wiki
  tree do. Rows are windowed from the start: the wiki pane measured
  5.5 ms per unwindowed row.
- **Draft.** The `Basket` pattern: staged acts, drawn marked in the
  table, saved through the draft commands, proposed as one package.
  The draft belongs to the view, not to one board.
- **Proposal.** The board with the package applied and marked, the
  existing approve / decline wiring, and "take over and continue",
  which is `rescue` on a pending proposal plus `replaces`.
- **Board management.** Appears from the second board: `waits_for`
  between boards.
- **Tokens.** One blue token is new; yellow, green, red and violet
  exist as `warn`, `good`, `bad`, `moved`. A small progress-bar
  component is new.
- Every string goes through `lexicon!` and `Strings`, compact, no em
  dash.

## 5. Steps

Each step is test-first, reviewed, and lands green on master. clippy
runs per crate. GUI steps are checked with the live-preview chain
(`scripts/dev-ui.sh build`, the GUI tests under `--features
molt-ui/live-preview`); no full window build per step.

**S1 - Core model and fold.** `board_fold.rs` beside the Kanban code.
Tests first: one per fault row of §4.2; a removed finished board
leaves no trace in the state and frees nothing it had not freed; the derived states of C§4; a
`kanban_ops` entry is `Skipped`; a package of ten acts applies whole or
not at all; the general board exists in the empty state; canonical
bytes of a package are pinned.

**S2 - Engine, tools, and the Kanban goes.** `board.rs` replaces
`kanban.rs`: cache, both doors, reads, advisories. MCP tools per D5.
The Kanban's core, engine, MCP and GUI code is deleted in this step;
the pane shows the read-only board table (enough of §4.7 to be honest:
navigation, table, progress without marks). Tests: a two-node
round-trip over loopback (propose, approve, fold on both); the fold
survives a checkpoint cut; a republic with applied `kanban_ops` blocks
opens, verifies and shows an empty board; the co-equality test.

**S3 - Clock gate.** Tests first, with `clock_override`: propose
refused before `at`, accepted at `at`; approve refused before, signed
after; the proposer's co-signature and the rebase re-sign refuse
alike; a pending early proposal is signed once the clock passes; a
chain holding an early close verifies. This step gets its own review
pass.

**S4 - Wakes.** Tests: each of the three voted events fires `board`
once; a refold fires nothing; `wait_due` fires at `at` and once only
across a restart; the old `wake_on` names still work.

**S5 - Marks.** Tests: last writer wins per `(task, seat)` under
reordered and duplicated delivery; a mark from seat A cannot set seat
B's mark; marks drop when the story closes; marks survive a restart
and a compaction while the story is open; the command refuses while a
seat lacks the capability; progress follows the table in C§7.

**S6 - Story chat.** The reserved topic name, its title from the
board, its entry point on the story row. Tests: the chat opens from a
story on both surfaces; an ordinary topic cannot take a `story:` name
by accident.

**S7 - Draft and propose in the GUI.** Staging, marks in the table,
undo, discard, propose; the state badge; the plus; the board select
for a move. GUI tests under live-preview: a story with two tasks goes
through the draft into one vote; the draft survives a restart; a draft
voided by another package says so.

**S8 - Proposal view and versions.** The marked board for a pending
package, "take over and continue", the `replaces` line. Test: two
versions, the second names the first, one vote applies the second.

**S9 - Board management, filter, empty state, mark clicks.** The rest
of C§7. GUI tests: the red dot appears with a problem mark; the filter
select shows "nothing matches" with the clear action; one board shows
no management view.

**S10 - Documents.** `docs/robo_scrum.md` moves to
`docs_archive/kanban/` as the spec and replaces
`kanban_workflows.md` in the read-first list of `CLAUDE.md`; this plan
is archived as executed; `python3 scripts/check-doc-refs.py`. The
window module is re-measured and the figure in `CLAUDE.md` re-dated.

S3 to S6 are independent of each other once S2 is in; S7 to S9 need S2
and, for the marks, S5.

## 6. Risks

- **The clock gate is honest-node only.** m seats with wrong or
  dishonest clocks close a wait story early, and the block is valid.
  That is the stated model (C§2.4), and the tests in S3 pin it so
  nobody later "fixes" it into `verify_chain`.
- **Mixed builds.** An old build ignores `board_ops` safely (dropped at
  ingest, skipped in the fold). It does not survive a `TaskMarked`
  event, hence D1. S5 must prove the capability gate with a seat that
  never announced.
- **Vote load is untested** (C, risks). Recurring work costs two votes
  per round. Nothing in this plan measures it; the first real republic
  will.
- **Refold cost.** Quests entries are archived whole, so the fold
  re-runs over every package after each cut. Unmeasured; the Kanban
  spec deferred a folded cut "until growth is measured", and so does
  this plan.
- **Window build.** Roughly 2,300 Slint lines go and fewer come back,
  so the generated module should shrink. It is a hope until S10
  measures it.
