# Driving and reading the GUI from MCP

**Status: steps 1, 3 and 4 are BUILT (2026-08-16); steps 2 and 5 are
open.** Step 1: `Command::UiPublish` (INTERNAL) + `ReadUiState` + the
`read_ui_state` tool; the GUI publishes at the end of every live-mirror
pass, pinned by `the_ui_snapshot_claims_what_the_window_holds` (headless)
and `the_ui_snapshot_roundtrips_and_actions_are_announced` (engine).
Step 3 resolved itself during the wizard work: the cold-open bug was the
layout-timing re-pin described below, fixed in the chat Flickable's
`changed height` handler (`app.slint`, with the explanation in place).
Step 4: `Command::UiAction` + the `ui_action` tool with the first verb
set (`select_view` · `select_channel` · `open_workspace` ·
`close_workspace` · `chat_send`) performed by the live mirror through
the SAME Slint callbacks a click takes; the snapshot's `generation`
answers "did it land". Open: step 2 (a dev-only headless `moltd` run on
the testing backend) and step 5 (the walk script), which depends on 2.

## Why, concretely

Three chat bugs in a row were diagnosed by reading code, and two of those
diagnoses were wrong. Each time the engine could be measured — a real `moltd`
over MCP answers `read_state` and is provably right — and the GUI could not.
The fault was in the GUI every time.

`molt-ui` now has a headless harness (`gui_tests`, `i-slint-backend-testing`)
that drives the real `AppWindow` against a real engine. It found the
reported cold-open sequence PASSING, which means the remaining difference is
what that harness does not reproduce: the real event loop, the real
concurrency of pushes around an open, and the hop onto the UI thread.

To see that, the running program has to be observable. And once it is
observable it should be drivable, or every test is half a test.

## What the current bug turned out to be, and what it teaches this plan

The empty chat on a first open was **layout timing**, not data: the chat
Flickable re-pins itself to the newest message imperatively
(`changed viewport-height`), and an imperative write REPLACES a Slint
binding. The content arrives while the box is still 0 high, the re-pin
computes `0 - content` and scrolls the whole log out of sight, and nothing
corrects it afterwards because the binding is gone. Leaving the surface and
coming back rebuilds the box with its height already known.

Every model-level test passed through all of this, because the rows WERE in
the model — which is exactly why the snapshot below must carry more than row
counts. It needs at least one fact about what is actually VISIBLE (is the
log in view?), or this class stays invisible to automation a second time.

## The shape

The engine is already the meeting point of both surfaces. It stays that way:
the GUI does not grow an RPC port, and MCP does not learn about widgets.

| direction | command | who speaks it |
|---|---|---|
| GUI → engine | `UiPublish { snapshot }` | the GUI only (**INTERNAL** — an agent must not be able to forge what the window claims to show) |
| read | `ReadUiState` → `Reply::UiState` | both (`read_ui_state`) |
| MCP → GUI | `UiAction { action }` | both (`ui_action`) |

The GUI publishes its snapshot at the end of every live-mirror pass — the
same place it applies a bundle, so what it publishes is what it rendered.
An action is stored by the engine and announced as an event; the GUI's
mirror performs it and publishes again. An agent that wants to know whether
its click landed reads the snapshot back.

## What the snapshot carries

What a test needs to assert, not a widget tree:

- the screen, surface and sub-view the window is on;
- the chat pane: selected channel, row count, the last few bodies, whether
  the compose row is visible, **and whether the log is scrolled into view**
  (the empty-chat bug was invisible without that last one);
- the nav: the rows it offers per surface, their badges;
- pending decisions: count and quorum text;
- the wizards: which step/phase, which fields are armed;
- the topmost toast, if any.

Row COUNTS and a few strings, deliberately: the point is "does the pane hold
what the engine holds", which is exactly what the three bugs got wrong.

## What an action names

Domain verbs, not coordinates. Every one maps to a Slint callback the GUI
already has, so the action surface cannot drift from what a human can do:

- `select_channel { channel }`, `select_view { surface, view }`
- `open_workspace { id }`, `close_workspace`
- `chat_send { body }`, `set_draft { field, value }`
- `press { key }` (Escape, Enter, arrows — the keyboard paths)
- `click { target }` for the named affordances (`new_topic`, `propose`,
  `approve { id }`, `rotate_token`, …)

A widget-coordinate protocol would let a test click something a user cannot
reach, and would break on every layout change. These break when a
CAPABILITY disappears, which is the change a test should notice.

## What this costs, and the decision behind it

The channel filter and the wizard steps are UI-LOCAL today, deliberately:
`ChatUiState`'s doc says the selection is presentation, not shared state, so
one operator cannot move another's view. `ui_action` breaks that on purpose
— the user asked for it explicitly, to make the window testable. The
mitigation is that it stays what it is: an operator-level door, gated by the
same MCP token as everything else, and an action is a REQUEST the GUI
performs, not a write into another surface's state.

## Order of work

1. **Read half.** `UiPublish` (INTERNAL) + `ReadUiState` + the `read_ui_state`
   tool; the GUI publishes at the end of every mirror pass. Pin: a headless
   test asserts the snapshot matches what the window's models hold.
2. **A headless RUN.** `moltd` gains a dev-only way to bring the GUI up
   without a display (the testing backend), so the whole program — live
   mirror, event loop, concurrency — can be driven from MCP. This is what
   the current bug needs.
3. **Find the bug** with 1+2, and fix it with evidence this time.
4. **Drive half.** `UiAction` + the `ui_action` tool, verb by verb, each
   with a test that performs it and reads the result back.
5. **A developer-test script** that walks the whole GUI: open, chat, topic,
   propose, approve, the wizards. This is the artefact the user asked for.

Each step lands green on master on its own.
