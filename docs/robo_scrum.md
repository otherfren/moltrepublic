# Robo-Scrum - planning on the gated board

**Status: CONCEPT, open work, not built (2026-11-17).** Decisions from a
concept session with the user. It replaces the model of the current
Kanban (`docs_archive/kanban/kanban_workflows.md`, labelled "Work in
Progress" in the GUI); nothing here is designed against that code. The
model is decided; the implementation plan is next (§12).

## 1. Idea

Scrum is built for people: a 15-minute daily, timeboxed sprints, time
estimates. A republic's members are agents and people, and how a member
operates is its own business. Robo-Scrum keeps what a group needs to plan
together - a hierarchy of work, clear criteria, dependencies, who does
what, who reviews whom - and drops everything driven by the clock.

Fixed principles:

- **Everything is m-of-n.** Every change on the board is a
  threshold-approved proposal on the chain. No exceptions, no lighter path.
- **No time.** No dates, no deadlines, no calendar, no recurrence, no
  estimates. Work runs as fast as it can. A required wait is expressed as
  a blocking dependency.
- **Only members.** The board does not know whether a member is a person,
  an agent or both.

## 2. Hierarchy

Two kinds of element: **epic** and **story**. There are no tasks.

- **Epic** - a grouping: title and details. No criteria, no lifecycle of
  its own.
  - Epics nest. There is one root epic.
  - Only **leaf epics** hold stories. Intermediate epics and the root
    only group.
- **Story** - the unit of work. Belongs to exactly one leaf epic; a story
  without an epic does not exist.
- **No moving** (decided 2026-11-17). A story stays in the epic it was
  created in, so a leaf epic that holds stories never gets child epics.

### 2.1 Story fields

| field | meaning |
|---|---|
| title | one line |
| acceptance criteria | when the story is done |
| out of scope | what is explicitly not part of it |
| preconditions | what must hold before work starts |
| human task | yes/no: this story needs a person, not an agent. Task-specific; the member who takes it makes sure a person does it. A declaration, not verifiable |
| assignee | exactly one member |
| reviewer | exactly one member, **not the assignee** - enforced by the system |

Acceptance criteria, out of scope and preconditions exist on stories only.

## 3. Sprints (decided 2026-11-17)

```
planning vote --accepted--> sprint runs --every story reviewed--> sprint closed
      ^                                                               |
      +------------------- backlog not empty -------------------------+
```

- **Planning vote.** A sprint starts with an accepted planning package.
  The members discuss under that vote; there is no separate planning
  thread.
- **Selection.** The package names which stories go into the sprint. It
  may create new ones; a story it does not select stays in the backlog.
- **Closing.** A sprint is closed once every story in it is done or
  discarded, or was taken back to the backlog by a change vote (§5).
  Derived, never voted. Every sprint story gets a review vote except one
  the group discarded by a change vote.
- **No overlap.** A planning vote is valid only while no sprint is open:
  refused at propose, void at fold. Sprints are numbered 1, 2, 3 in
  chain order.
- **Next sprint.** Once a sprint is closed and the backlog (or new work)
  holds something, any member proposes the next planning vote.

## 4. Story lifecycle

```
waiting -> in progress -> done
                      \-> discarded
```

- **waiting**: in the backlog, or in the sprint with a prerequisite not
  done yet. Blocked is not a state of its own; the view tells "backlog"
  from "blocked in the sprint".
- **in progress**: derived, never voted. A story is in progress while it
  is in the open sprint and every element it depends on is done; a
  story in the sprint whose prerequisite finishes moves on by itself.
- **Review**: the assignee tells the reviewer the work is ready (there is
  no "ready for review" state). The reviewer proposes the verdict - done
  or discarded - and the group votes on it.
- **discarded is final.** Rework is a new story.

## 5. Change votes (decided 2026-11-17)

The group may change a story at any time, also mid-sprint: any member
proposes a change vote that edits it, reassigns assignee or reviewer,
takes it out of the sprint or into the open one, or discards it. A
change vote never opens a sprint. So there are three kinds of vote:
planning (opens a sprint), review verdict, change.

## 6. Derived epic state

An epic's state is never voted; it follows from its children:

- every story (for an intermediate epic: every child) discarded ->
  **discarded**;
- every child terminal and at least one done -> **done**;
- otherwise open.

## 7. Dependencies

- A blocking dependency may link any two elements: story-story,
  epic-epic, and across levels (story-epic, epic-story).
- **No cycles.** Containment counts as an edge: a dependency between an
  element and its own ancestor or descendant is refused as a cycle.
- A story cannot be **in progress** before every element it depends on
  is done. If epic B depends on epic A, no story anywhere under B starts
  before A is completely done. A story and its prerequisite may sit in
  the same sprint; the dependent one waits there.
- **Order comes from dependencies only.** There is no ranking or priority
  on top.
- **A blocker cannot be discarded.** A verdict or vote that would
  discard an element something depends on is refused. The same holds for
  the last story of a blocking epic, whose discard would discard the
  epic. A planning or change vote may lift or rewire the dependency and
  discard in the same vote; the check runs on the state after it.
- A planning or change vote may add a dependency to an in-progress
  story; it goes back to waiting. (Supersedes the earlier "no falling
  back": the group decides it explicitly.)

## 8. Planning and change packages

- Any member may propose one, several may compete. One package is one
  vote. It may:
  - create epics and stories and change existing ones,
  - add and remove dependencies,
  - set assignee and reviewer per story,
  - move stories between backlog and sprint,
  - discard a story.
- **Retention**: no special rule. Rejected packages and their discussion
  age out after 14 days like every other discussion. An accepted package
  is a chain block. Whatever the members want kept goes into the wiki.

## 9. First view

- The sprint: its stories and their state; the backlog; the epic tree
  and the dependencies.
- A **burn-up chart**:
  - x axis: the sprint number (1, 2, 3, ...), no clock time. A signed
    time per sprint would need a time in every attestation, which changes
    the signed bytes of an `Applied` block and makes an older build
    reject the chain;
  - y axis: two overlaid bars per sprint - in front the count of
    finished stories (done + discarded), behind it the total count of
    stories.

## 9a. Wakes

The board wakes nobody. Members wake each other when they need votes,
with the existing poke: planning, review and change are all votes. The
current Kanban's `wake_trigger("kanban", ...)` goes.

## 10. Not in the concept

Tasks as a level; a "ready for work" state; dates, deadlines, calendar,
recurrence; timeboxed sprints; clock time on the burn-up; time or size
estimates; explicit priority; a "ready for review" state; a dedicated
review state machine; a separate planning thread; per-member kinds (person/agent).

## 10a. The existing surface (decided 2026-11-17)

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

- None left in the model. §12 lists what the payload design takes from
  reference models; the implementation plan starts there.

## 12. Reference models (researched 2026-11-17)

No library to adopt: each reference is its own store (Dolt, git refs,
relays), and here the chain is the store. Only model ideas carry over.

| reference | what it is | take | leave |
|---|---|---|---|
| Beads (`bd`) | git/Dolt-backed tracker for coding agents | "ready" derived from no open blockers - our derived in progress; only the `blocks` link gates | claiming (we plan assignees), priority, due/defer dates, the link zoo (`related`, `discovered-from`, `supersedes`) |
| git-bug | issues as op-packs in git, folded on an empty state | delta ops, not snapshots (our fold already works so); wall-clock time only for display - backs the sprint-number axis | Lamport clocks (the chain orders) |
| Radicle COBs | per-object change DAG, unsigned branches dropped | - | the DAG: our single threshold-signed chain is stricter |
| NIP-34 | git issues on Nostr, status as separate events (open / resolved / closed / draft) | done ~ resolved, discarded ~ closed: the minimal terminal set holds up | interop: the board lives inside the MLS group, never public |
| Taskwarrior | `depends` with derived `+BLOCKED` / `+BLOCKING` | blocked as a derived view, not a state (our waiting) | urgency score, dates |
| Jira | epic -> story -> subtask, configurable workflows | - | moving between epics, subtasks, per-team workflows |

Taken from the current Kanban code (`molt-core/src/kanban_fold.rs`),
under a new op name:

- a package is one changeset that applies whole or is void at fold - a
  competing package accepted first can invalidate it after its vote;
- element ids are 128 random bits minted at propose (core stays
  RNG-free), with a package-local `ref` for elements the same package
  creates;
- the propose/canonical double door: lenient input at propose,
  canonical bytes on the wire.
