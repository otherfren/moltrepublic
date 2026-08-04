# The Welcome screen, and the one way back in

**Status: BUILT (2026-08-05), all six steps.** Product decision by the user;
this document was the execution order and is now the record of what shipped.
The polish pass ran last, as asked.

## Why

The Welcome screen offers four ways in — Open, Restore, Create, Join — and two
of them are the same thing to the person standing in front of it. "Join" and
"Restore" both mean *I have a link and I want to be in that republic*; which
command runs behind it (a founding join vs. the recovery ritual) is a
distinction the SOFTWARE can make from the link itself, and it has been making
the user make it instead. Four doors, two of which lead to the same room.

The subtitle ("Choose how to begin.") says nothing the four cards do not.

And the Restore wizard offers a "Manual restore" file picker, which is a button
for something the user can do with their file manager.

## The end state

**Welcome: three cards.**

| card | key | goes to |
|---|---|---|
| Open | `O` / `Ö` | `AppScreen.open` — the local workspace table |
| Create | `C` / `G` | `AppScreen.create` — found a new republic |
| Restore | `R` / `W` | `AppScreen.restore` — the way back in, below |

Create sits directly under Open (both are "I already know what I want"); the
"New republic" group header and its indent rail go away with the grouping. No
subtitle. `choice-focus` wraps over 3, not 4.

**Restore step 1: two panels, not three, plus the phrase.**

1. **Recovery phrase** — unchanged, but no longer unconditionally required
   (see the dispatch rule below).
2. **Link** — ONE field for a `molt://…` link, plus a name field. This is the
   panel that merges Join and social peer-restore.
3. **Online restore via S3** — unchanged.
4. ~~Manual restore (file)~~ — the GUI panel goes. The COMMAND stays
   (`RestoreStart { way: "file" }`) and so does its MCP tool: dropping a
   capability is not what was asked for, and a `.molt.enc` blob is not
   something a file manager can install — only the button goes.

## The dispatch rule (the whole design)

The two link shapes are already unambiguous and already parsed in `molt-engine`:

| prefix | parser | needs | runs |
|---|---|---|---|
| `molt://invite/…` | `molt_engine::FoundingInvite::parse` | a NAME | `Command::JoinStart { invite, member }` |
| `molt://recover/…` | `molt_engine::RecoveryInvite::parse` | the PHRASE | `Command::RecoverStart { link, phrase }` |

So the panel is dynamic in exactly one way: **what the link is decides which
field is required.**

- A recovery link names its own seat — the name field is inert (it shows the
  seat the link carries, read-only) and the phrase is required.
- An invite link cannot know who the person is — the name is required and the
  phrase is NOT: a join mints its own recovery phrase and shows it once. The
  phrase panel dims for an invite link rather than disappearing (a field that
  vanishes as you type reads as a bug).
- Junk, or a preview-only link with no handover, arms nothing and says which
  of the two shapes it expected.

**No new `Command`.** Both flows are already co-equal commands with MCP tools
(`join_start`, `recover_start`), so nothing about the engine's surface changes
— which is also why the co-equality test does not move. The dispatch is
presentation: which existing command a click issues. `molt-ui` already depends
on `molt-engine`, so both parsers are in reach.

**MCP parity check** (the standing rule, verified rather than assumed): the
tools this touches are `join_start`, `recover_start`, `restore_start` — all
three keep their arguments and semantics. What changes is the GUI's routing.
The one thing worth adding is a note in `restore_start`'s description that
`way: "file"` has no GUI panel any more, so an agent knows it is the only
surface offering it.

## Order of work — done except the polish

Each step ends green; the GUI is validated by `scripts/dev-ui.sh build` per
iteration and ONE `cargo build -p molt-ui-window -p molt-ui` per change-set
(never next to another window-scale build).

1. ✅ **The pure function first, with tests.** `molt-ui`: `link_kind(&str) ->
   LinkKind` (`Invite { republic, inviter }` / `Recovery { republic, member }`
   / `Unrecognized`). Unit tests: both real shapes (rendered by the engine's
   own `render()`, never hand-written strings), a preview-only invite link
   with no handover, a recovery link with a damaged handover, empty, and plain
   junk. This is the whole decision, and it is testable without a window.
2. ✅ **Welcome.** Three cards, no subtitle, no group rail; `choice-go` and the
   hotkey/arrow handling collapse from 4 to 3. Retire the `choice_join_*` and
   `choice_group_republic` strings, and `choice_subtitle`.
3. ✅ **Restore step 1.** Replace the peer panel with the Link panel; delete the
   file panel; wire the two Continue paths through `link_kind`. The phrase
   panel gains its armed/dimmed state.
4. ✅ **Keyboard.** Enter in the link field continues when the panel is armed;
   the name field only takes focus when it is required. Welcome keys stay
   single letters (localized), matching the card titles.
5. ✅ **Docs + MCP note**, then the full suites (`molt-core`, `molt-engine`,
   `molt-mcp`, `molt-ui`), clippy at zero, and the authoritative GUI build.
6. ✅ **Polish last** — the user asked for it in that order: get the behaviour
   right and tested, then the spacing/typography pass over both screens.

## What this does not change

The recovery ritual, the founding ritual, the restore verification ladder, and
every string a human reads about them. This is a routing and layout change: the
same three flows, reached through one fewer decision.

## What shipped, beyond the plan

- The **orphaned file-picker modal** went with the panel (nothing could open
  it any more), and with it `rw-file`, `rw-file-draft` and
  `restore-file-modal-open`. `rw-file-pick` stayed: the settings folder picker
  uses the same "Select" label.
- `rw-via-file` stayed as a **RunView title**: an MCP-started file restore
  still reports its progress in the GUI, and a run that cannot name itself is
  worse than an unused string. `rw-via-peer` did NOT — the polish pass found
  its RunView branch was already dead (`RestoreStart.way` is `"s3" | "file"`,
  never `"peer"`), so the ternary's first arm could not fire.
- Polish: the panel took the **link** icon rather than the peer panel's
  `users.svg`, the title gained room now that no subtitle sits under it, and
  the three new hint strings were cut to one line each (the `RestoreWay` hint
  elides rather than wrapping, so a long one is simply lost).
- The phrase panel **dims** (opacity, plus a hint that says why) for an
  invite link rather than vanishing.
- The S3 branch inherited `last: true` — the tree rail's L now closes there.
- The recovery link's seat is named by the panel's **verdict line**
  ("Recovery for <republic> · <member>") rather than by an inert, disabled
  name field. Same information, one control fewer: a greyed-out field the
  user cannot act on is chrome, and the line has to be there anyway to say
  which of the two shapes the link turned out to be.
