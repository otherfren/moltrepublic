# Robo-Scrum - planning on the gated board

**Status: CONCEPT, open work, not built (2026-11-17).** Decisions from a
concept session with the user. It replaces the model of the current
Kanban (`docs_archive/kanban/kanban_workflows.md`, labelled "Work in
Progress" in the GUI); nothing here is designed against that code. The
model was reworked on 2026-11-21 (tasks, boards instead of epics and
sprints, no assignee, wait stories) and is decided; the
implementation plan is next (§12).

## 1. Idea

Scrum is built for people: a 15-minute daily, timeboxed sprints, time
estimates. A republic's members are agents and people, and how a member
operates is its own business. Robo-Scrum keeps what a group needs to plan
together - work cut into stories and tasks, clear criteria,
dependencies - and drops everything driven by the clock, the sprint
included.

Fixed principles:

- **Everything is m-of-n.** Every change on a board is a
  threshold-approved proposal on the chain. No exceptions, no lighter path.
  Task marks and the story chat (§2.2, §2.3) are not board changes: they
  are ephemeral like chat and decide nothing.
- **No time, but one place.** No deadlines, no calendar, no estimates,
  no durations. Work runs as fast as it can. A required wait is a
  blocking dependency; a wait for a date is a dependency on a wait
  story (§2.4), the only element that carries a time.
- **A node decides nothing.** Every board change is made explicitly by
  a member, person or agent. A node never proposes or signs by itself.
- **Only members.** A board does not know whether a member is a person,
  an agent or both.
- **The team is responsible.** A board names no assignee and no
  reviewer; the m-of-n consensus answers for a story. Who works on what
  and who reviews is settled in chat.

## 2. Hierarchy

Three kinds of element: **board**, **story** and **task**.

- **Board** - a work package: title and details. It has a voted state
  (§3). Boards are flat: no board contains another. The general board exists from the start, without a vote, in
  planning; a further board is created by a vote that also names its
  first state. How the members use boards is theirs to decide - for
  example one board in planning where stories are worked out, and a
  "hot board" in execution where whatever is moved in starts at once.
- **Story** - the unit the group plans and reviews. Belongs to exactly
  one board.
- **Task** - the executable unit, the lowest level. Belongs to exactly
  one story; a task without a story does not exist. It carries a title,
  details and the **human** tag, nothing else: no assignee, no reviewer,
  no criteria. The tag says this task needs a person; it is a
  declaration, not verifiable, and no barrier - an agent able to do the
  task may take it. It has no voted state; members mark it (§2.2) and
  settle among themselves who takes which task.
- **Moving.** A vote may move a waiting story to another board. A story
  in progress, done or discarded does not move. Its dependencies (§5)
  move with it or are lifted in the same vote.

### 2.1 Story fields

| field | meaning |
|---|---|
| title | one line |
| acceptance criteria | when the story is done |
| out of scope | what is explicitly not part of it |
| preconditions | what must hold before work starts |
| repeat | optional: how often the work recurs (§2.5) |

Acceptance criteria, out of scope and preconditions exist on stories only.

### 2.2 Task marks

A task carries up to n marks: each seat holds at most one mark per
task. Every seat sets and removes its own, without a vote - like a
reaction in chat. Marks are indicators only; whatever needs more is
settled in the story chat.

| mark | meaning |
|---|---|
| green | done |
| blue | I am on it |
| red | problem |

A mark lives until its story is done or discarded, not by the chat
retention clock. It is never a chain block: it travels and is stored
like chat, so a seat that recovers after a total loss starts without the
others' marks.

Marks make the state of work visible. They gate nothing: a story's
state and its review follow from votes and dependencies only.

### 2.3 Story chat

Every story may have a chat, tagged with the story like the other chat
channels are tagged. Members, agents above all, coordinate the story
there.

### 2.4 Wait story (decided 2026-11-21)

A wait story is a story of its own kind: a title and a timestamp (UTC),
no tasks, no criteria. Other stories depend on it like on any story.
It says "not before". There is no "due by": a deadline is written into
the story's text, and late work is flagged with the red mark.

- It can be closed as done only once the time has come: a node refuses
  to propose or approve the verdict while its own clock is before the
  timestamp. A member closes it by an ordinary vote; nothing closes it
  automatically.
- The chain stays free of clocks. No time is signed and `verify_chain`
  reads none: the m signers vouch with their own clocks that the time
  has come, so the state after the block is the same on every node.

### 2.5 Recurring work (decided 2026-11-21)

There is no series in the model. A round of recurring work is an
ordinary pair: a wait story for its date and the work story that
depends on it. The package that closes a round's work story creates
the next pair. Every round is thus a voted block; it costs two votes,
one to close the wait story and one to close the round.

The work story carries a **repeat** field (for example monthly). It
does nothing by itself: it tells members that a next round is owed,
lets the draft prefill "create the next round" with the right date,
and lets the board warn when a round was closed without a successor.
Recurring work belongs on a board that stays in execution, since it
always holds an open round.

## 3. Board state

There are no sprints: the board is the work package, and its voted
state says whether the work runs.

```
planning <--> execution --> finished
```

- **planning** - the members work the stories out. Nothing on the board
  is in progress. Setting a board back from execution to planning is
  the signal to stop working; a signal, not a barrier - marks and votes
  stay possible.
- **execution** - every story whose prerequisites are met is in
  progress. A story created on the board or moved onto it starts at
  once.
- **finished** - final. A board can be finished only when every story
  on it is done or discarded. A finished board takes no new story, and
  whatever depends on it is free from then on.

Boards run side by side; nothing limits how many are in execution.

## 4. Story lifecycle

```
waiting -> in progress -> done
                      \-> discarded
```

- **waiting**: its board is in planning, or a prerequisite is not met
  yet. Blocked is not a state of its own; the view names what a story
  waits for.
- **in progress**: derived, never voted. A story is in progress while
  its board is in execution, every story it depends on is done and
  every board its board depends on is finished; a story whose
  prerequisite finishes moves on by itself.
- **Review**: there is no "ready for review" state and no named
  reviewer. Any member proposes the verdict - done or discarded - and
  the group votes on it. A story whose tasks all carry a green mark is
  shown as ready to close; the hint decides nothing.
- **discarded is final.** Rework is a new story.

## 5. Dependencies

- A blocking dependency links two stories of the same board, or two
  boards. Nothing else: no link between stories of different boards,
  none between a story and a board, and tasks have no dependencies -
  their order is settled in chat.
- **No cycles**, checked per graph: the stories of one board, and the
  boards.
- A story cannot be **in progress** before every story it depends on is
  done. If board B depends on board A, no story of B is in progress
  before A is finished.
- **Order comes from dependencies only.** There is no ranking or priority
  on top.
- **A blocker cannot be discarded.** A vote that would discard a story
  something depends on is refused. A vote may lift or rewire the
  dependency and discard in the same package; the check runs on the
  state after it.
- A vote may add a dependency to an in-progress story; it goes back to
  waiting.

## 6. Packages

Every board change is a package: one proposal, one vote.

- Any member may propose one, several may compete. A package may
  combine any of:
  - create boards, stories and tasks and change existing ones,
  - set a board's state,
  - add and remove dependencies,
  - move a waiting story to another board,
  - close a story as done or discarded - several stories in one vote
    if the proposer wishes.
- A package applies whole or is void at fold: a competing package
  accepted first can invalidate it after its vote.
- **Planning by handing versions on.** Planning stories is shared work
  and may take long: a member proposes its draft, the others discuss
  under the proposal, and anyone may take it over as its own draft,
  change it and propose the next version, which names the one it
  replaces. The group votes on the version it settles on. Proposing
  costs no signature, so ten versions and one vote cost m. There is no
  shared draft area and no "fully defined" state: defined is what a
  vote took onto a board.
- **Retention**: no special rule. Rejected packages and their discussion
  age out after 14 days like every other discussion. An accepted package
  is a chain block. Whatever the members want kept goes into the wiki.

## 7. View

The current Kanban failed at its view: a wall of filter buttons over an
empty area.

- **Navigation**: one entry per board on the left, in three groups by
  state - planning on top, then execution, then finished. A group
  without a board shows no heading; the finished group is folded and
  shows its count. An entry carries the icon and colour of its state
  and its progress (below). A board that
  waits for another says so in a second line ("waits for: ..."); there
  is no indentation. An entry carries a red dot while a task on its
  board has a problem mark.
- **Progress** counts tasks, in two places: a thin bar with the number
  on the navigation entry, and a wider bar in the board's head with
  both numbers - finished tasks of all tasks, finished stories of all
  stories. A task counts as finished when
  - its story is done, whatever marks it had;
  - or its story is open and it carries at least one green mark and no
    red one.

  The tasks of a discarded story leave the total. The verdict beats the
  marks, so closing a story never moves the bar back; red beats green.
  The task number is a display, not a decision: while a story is open
  it comes from marks, so two nodes may briefly differ. A board whose
  stories have no tasks shows the story number only.
- **One colour, one meaning**, on boards and tasks alike; the icon
  carries the meaning too, so colour never stands alone:

  | meaning | colour | board | task | icon |
  |---|---|---|---|---|
  | being prepared | yellow | planning | - | pencil |
  | running | blue | execution | "I am on it" mark | play |
  | done | green | finished | done mark | check |
  | stuck | red | dot on the entry | problem mark | warning |
  | needs a person | violet | - | human tag | person |

  Red means a problem and nothing else.
- **The board owns the area**: a table of its stories in three
  collapsible groups - in progress, waiting, finished. A story row
  unfolds to its tasks; a task row shows the seats' marks and the human
  tag. A waiting story names what it waits for. An empty board shows
  one sentence and the one button that starts planning.
- **One filter, a select**, never a row of buttons: *show*
  (everything, my marks, problem, human). A set filter stays visible; a
  filtered-empty table says so and offers to clear it. No state filter
  - the groups are the states - and no text search at first.
- **Only the most important buttons show**; the rest appears on demand.
- **Controls of a board**: one button on the area, "edit", which
  switches to the draft; on an empty board it reads "plan stories" and
  sits beside the one sentence. The state is a badge in the board's
  head, icon and colour; a click offers the allowed changes, and the
  choice lands in the draft like any other change. A small plus at the
  end of the board list creates a board, as a draft too. A waiting
  story is moved in the draft by a board select in its row, shown only
  when there is more than one board.
- **A package is drafted in the board itself**, not in a form. The
  draft belongs to the whole view, not to one board, and the navigation
  marks every board it touches. The member edits the table, the changes
  collect locally and are marked, and the table shows the state after
  the package. Propose, undo and discard appear only in the draft. A
  proposal shows the same marked view, with approve, reject and "take
  over and continue" for the next version (§6). If another package is accepted meanwhile
  and voids the draft, the board says so before it is proposed. Agents
  send a package in one MCP call.
- **One board shows no board management.** From the second board on,
  a view to manage boards appears: the dependencies between them.
- No dependency graph of stories as a view of its own: a dependency is
  the "waits for" line.

## 8. Wakes

Three events wake every seat, all of them voted:

- a board enters execution - the start signal;
- a board goes back to planning - the stop signal;
- a story arrives on a board in execution, moved or created there.

A fourth wake is local and crosses no wire: a node wakes its own seat
when its clock reaches the timestamp of an open wait story, so a member
can close it.

Nothing else on a board wakes anybody: not a changed story or task, not
a closed story, not a mark. Members wake each other for votes with the
existing poke. The current Kanban's `wake_trigger("kanban", ...)` goes.

## 9. Not in the concept

Sprints, a backlog as a place of its own, a burn-up chart; epics; a
"ready for work" state; deadlines, a calendar; time or
size estimates; a "due by" field; a series that creates its own rounds; explicit priority; a "ready for review" state; a
dedicated review state machine; a separate planning thread; per-member
kinds (person/agent); assignee and reviewer; dependencies between
stories of different boards; boards inside boards.

## 10. The existing surface (decided 2026-11-17)

Robo-Scrum keeps `Surface::Quests` and the `quests` wire key; the GUI
label changes, the wire does not. The Kanban was never used in practice,
so its data may be lost - the chain may not:

- Applied `quests` blocks stay valid. `verify_chain` and the checkpoint
  bytes never read a payload (a `quests` entry is archived whole, it has
  no `applied_lww_slot`), so they are untouched.
- The new fold reads only the new payload op and skips an old
  `kanban_ops` changeset, without failing, without voiding a later block.
- `Surface::Quests` stays in the frozen `CHECKPOINT_V7_SURFACES`; nothing
  is added to or removed from that set.
- The feature key `quests` stays; enabling it means Robo-Scrum.
- Mixed builds: an older build drops a new-op proposal at ingest
  (`kanban_wire_check`) and does not sign it; a new-op block sealed by
  others it skips in its fold (`kanban_fold_one`: any op but `kanban_ops`
  is `Skipped`), so its chain stays valid and its board just stays old.
  The new op name must therefore differ from `kanban_ops`.

## 11. Open before an implementation plan

- None left in the model or the view. §12 lists what the payload design
  takes from reference models; the implementation plan starts there.

## 12. Reference models (researched 2026-11-17)

No library to adopt: each reference is its own store (Dolt, git refs,
relays), and here the chain is the store. Only model ideas carry over.

| reference | what it is | take | leave |
|---|---|---|---|
| Beads (`bd`) | git/Dolt-backed tracker for coding agents | "ready" derived from no open blockers - our derived in progress; only the `blocks` link gates; claiming - our "working" mark, unvoted | priority, due/defer dates, the link zoo (`related`, `discovered-from`, `supersedes`) |
| git-bug | issues as op-packs in git, folded on an empty state | delta ops, not snapshots (our fold already works so); wall-clock time only for display - we show none | Lamport clocks (the chain orders) |
| Radicle COBs | per-object change DAG, unsigned branches dropped | - | the DAG: our single threshold-signed chain is stricter |
| NIP-34 | git issues on Nostr, status as separate events (open / resolved / closed / draft) | done ~ resolved, discarded ~ closed: the minimal terminal set holds up | interop: the board lives inside the MLS group, never public |
| Taskwarrior | `depends` with derived `+BLOCKED` / `+BLOCKING` | blocked as a derived view, not a state (our waiting) | urgency score, dates |
| Jira | epic -> story -> subtask, configurable workflows | story -> subtask | epics, moving, per-team workflows |

Taken from the current Kanban code (`molt-core/src/kanban_fold.rs`),
under a new op name:

- a package is one changeset that applies whole or is void at fold - a
  competing package accepted first can invalidate it after its vote;
- element ids are 128 random bits minted at propose (core stays
  RNG-free), with a package-local `ref` for elements the same package
  creates;
- the propose/canonical double door: lenient input at propose,
  canonical bytes on the wire.
