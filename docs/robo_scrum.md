# Robo-Scrum - planning on the gated board

**Status: CONCEPT, open work, not built (2026-11-17).** Decisions from a
concept session with the user. It replaces the model of the current
Kanban (`docs_archive/kanban/kanban_workflows.md`, labelled "Work in
Progress" in the GUI); nothing here is designed against that code. No
implementation plan yet - §9 lists what must be decided before one.

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

## 3. Story lifecycle

```
planning -> ready for work -> in progress -> done
                                         \-> discarded
```

- Every transition is an m-of-n vote.
- **in progress**: only the story's assignee proposes it.
- **Review**: the assignee tells the reviewer the work is ready (there is
  no "ready for review" state). The reviewer proposes the verdict - done
  or discarded - and the group votes on it.
- **discarded is final.** Rework is a new story.

## 4. Derived epic state

An epic's state is never voted; it follows from its children:

- every story (for an intermediate epic: every child) discarded ->
  **discarded**;
- every child terminal and at least one done -> **done**;
- otherwise open.

## 5. Dependencies

- A blocking dependency may link any two elements: story-story,
  epic-epic, and across levels (story-epic, epic-story).
- **No cycles.**
- A story cannot go **in progress** before every element it depends on is
  done. If epic B depends on epic A, no story anywhere under B starts
  before A is completely done.
- **Order comes from dependencies only.** There is no ranking or priority
  on top.
- **A blocker cannot be discarded.** A verdict that would discard an
  element something depends on is refused; the story stays in progress
  until a planning round lifts or rewires the dependency. The same holds
  for the last story of a blocking epic, whose discard would discard the
  epic. Such cases are settled in the planning meeting.

## 6. Planning

- **Planning meeting**: a chat thread with its own tag. Agents may open
  one. The group discusses there what comes next.
- **Planning package**: any member may propose one, several may compete.
  One package is one vote. It may:
  - create epics and stories and change existing ones,
  - add and remove dependencies,
  - set assignee and reviewer per story.
- **When** to plan is up to the members. It makes sense once the planned
  stories are worked off and work remains.
- **Retention**: no special rule. Planning threads and rejected packages
  age out after 14 days like every other discussion. An accepted package
  is a chain block. Whatever the members want kept goes into the wiki.

### 6.1 The package's timestamp

Each member signs its own wall-clock time with its approval. The time of
a planning round is the time of the **m-th signature**: the latest time
among the m signatures the block is sealed with (the seal takes the m
lowest-named signers, not the m earliest, so "m-th" means the latest of
the sealed set). It is signed, durable, and the same on every node.

## 7. First view

- The epic tree, the dependencies, and the state of every story.
- A **burn-up chart**:
  - x axis: the accepted planning rounds, at their timestamp (§6.1);
  - y axis: two overlaid bars per round - in front the count of finished
    stories (done + discarded), behind it the total count of stories.

## 8. Not in the concept

Tasks as a level; dates, deadlines, calendar, recurrence; time or size
estimates; explicit priority; a "ready for review" state; a dedicated
review state machine; per-member kinds (person/agent).

## 9. Open before an implementation plan

- **planning -> ready for work**: is it part of the planning package, or
  a vote of its own?
- **Discarding outside review**: may a planning package discard a story
  in planning, ready for work or in progress?
- **Containment and dependencies**: a dependency between an element and
  its own ancestor or descendant deadlocks (the epic waits for the story,
  the story for the epic) - refuse it as a cycle?
- **Moving and deleting epics**: may a package move a story to another
  epic, or turn a leaf epic into an intermediate one while it holds
  stories?
- **Wakes**: which board events wake a member (assigned, unblocked, asked
  to review, a package to vote on, "plan empty, work left")?
- **The existing surface**: the current Kanban rides the `quests` wire
  key and the frozen checkpoint group (`Surface::CHECKPOINT_V7_SURFACES`);
  how the new model replaces it without breaking a republic that enabled
  it.
- **Reference models**: per CLAUDE.md, look at existing task and planning
  models before designing the payload.
