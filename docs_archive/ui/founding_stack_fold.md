# Founding wizard: the finished steps fold away

**Status: BUILT (2026-09-13), pending the user's local check. The §6
questions were answered by the user the same day; the answers are folded
into §2-§5. Pinned by `molt-ui/src/tests/gui/founding_fold.rs`.**

Known limit: the setup fold reads the founder's relay picks from the live
pick model, which the session refresh may widen while the wizard is open;
the invites carry the picks as they were at "Found republic".

## 1. Today

The founder's lobby (`app.slint`, `cw-step == 1`) is a stack: the phase's
current interaction on top, the standing artefacts of earlier phases below
it at 55 % opacity (`input-active`). The pile stays fully expanded, so the
member list (up to 340 px) and the read-only charter sit under the form the
user must act on, and the column scrolls past them.

Founder phases and what is current / standing in each:

| phase | current (top) | standing (below) |
|---|---|---|
| P1 invites | member list, seats turning green | - |
| P2 deliberation | charter form + features | members |
| P3 charter out | wait note (spinner) | charter (read-only), members |
| P4 phrase proof | `SeedConfirmStep` | charter, members |
| P5 backups out | wait note | charter, members |
| P6 sealed | `SealedPanel` | - (members already hidden) |
| failed | failure banner | whatever the phase had |

The setup step (`cw-step == 0`: name, m-of-n, relays, transport) vanishes
entirely once "Found republic" runs.

## 2. Target

The current interaction renders as today. Every finished step folds to a
one-line header at the bottom of the column, newest above oldest, with a
`+` to expand it and a `-` to fold it back. At most one folded step is open
at a time (accordion, like the surface nav's `nav-expanded`). The folded
header carries a live one-line summary so the pile still informs without
being opened (members: `3 / 4 sealed`; charter: the name and the char
count; setup: `m-of-n · k relays · tor`).

Not folded, ever: the failure banner, the wait notes, `SealedPanel` and
`SeedConfirmStep` - each is the phase's current state, not a record. The
recovery phrase is not a record either: once confirmed it is gone from the
screen (`seed_backup_confirmation.md`, seeds are cleared at the seal).

## 3. Design

### 3.1 `FoldedStep` (parts.slint, new)

```
export component FoldedStep inherits Rectangle {
    in property <string> title;     // wiz-step-* string
    in property <string> summary;   // live one-liner, may be ""
    in property <bool> open;
    callback toggle();
    // header: [+|-] title ........ summary   (whole row is a TouchArea)
    // body: Rectangle { clip: true; height: open ? body.preferred-height : 0;
    //                   animate height { 200ms ease-in-out } ; @children }
}
```

The body is height-clipped rather than `if`-gated: `@children` stays
unconditional, the fold animates, and a live model inside (the seat list)
keeps updating while folded so the summary and the seat rows agree the
moment it opens. Element ids for the tests: `fs-toggle`, `fs-body`.

Tones: header text `Theme.faint`, border `Theme.line`; an OPEN body renders
at full opacity - the 55 % dim disappears with the pile it was dimming. The
current step keeps its accent frame, so open-old vs. current is told by the
frame, not by opacity.

### 3.2 Accordion state

`property <int> cw-fold: -1` on `AppWindow` - UI-local like `cw-step`
(`gui_over_mcp.md` §"UI-local"). Keys: 0 setup, 1 members, 2 charter.
`toggle()` sets `cw-fold = (cw-fold == k) ? -1 : k`. Reset in the create
reset block (`app.slint` ~2555) and on every phase advance via a
`changed` handler on the derived phase int, so a freshly folded step never
opens on top of a new current step.

### 3.3 The lobby column

```
VerticalLayout {                     // cw-step == 1
    [failure banner]                 // unchanged
    [SealedPanel]                    // unchanged
    [wait note]                      // unchanged
    [SeedConfirmStep]                // unchanged
    [deliberation panel]             // unchanged (P2 current)
    [member list, unfolded]          // P1 only: it IS the current step
    // ---- folded pile, newest first ----
    if P3+:      FoldedStep { title: charter; summary; CharterView }
    if P2+:      FoldedStep { title: invites; summary: "3 / 4 sealed"; seat list }
    if always:   FoldedStep { title: setup;  summary; read-only setup facts }
}
```

The seat list and `CharterView` move into the folded bodies unchanged;
the `opacity: input-active ? 0.55 : 1` lines go. The setup body is a new
read-only rendering of `cw-name`, `cw-member`, `cw-threshold`/`cw-members`,
the picked relays and `cw-net` (facts the founder can no longer change -
no fields, no buttons). The relays come from `cw-relay-picks` filtered on
`picked`; a count in the header, the urls in the body.

In-flow, not docked: the pile sits at the end of the scroll column, not
pinned above the footer (§6 Q1 argues this).

### 3.4 Strings

New `Strings`: `fold-open` / `fold-close` (tooltips for + / -),
`cw-fold-setup-sum` if the summary needs a word beyond the numbers. Titles
reuse `wiz-step-setup`, `wiz-step-invites`, `wiz-step-charter`. Both
lexica in `molt-ui/src/i18n.rs`.

## 4. Tests (headless, `molt-ui/src/tests/gui/founding_fold.rs`)

1. P2 (`cw-can-propose`, not proposed): the deliberation panel is visible,
   the `FoldedStep` titled invites exists and its `fs-body` height is 0.
2. Click the invites `fs-toggle`: body height > 0 and the seat rows lie
   inside it. Click the setup toggle: invites body back to 0, setup open.
   Click setup again: nothing open.
3. Advance to P3 (`cw-proposed = true`) with a fold open: `cw-fold == -1`,
   a charter `FoldedStep` exists, the wait note is on top.
4. Mutate a seat to state 4 while folded: the invites header summary reads
   the new count.
5. P1: the member list renders unfolded (no invites `FoldedStep`).
6. i18n: every new string present in both lexica (existing pattern).

Then the component, then the lobby rewire, then a visual pass in the live
preview. Clippy for `molt-ui` in the live-preview flavour per change-set.

## 5. The join wizard, same shape

`jw-step == 1` gets the identical treatment: the current step on top,
never folded; every finished step a `FoldedStep` with a live summary,
read-only when opened. Folded there: the join facts (republic, rule,
inviter, own name - never the invite link) always, and the charter once
it is ratified (`jw-proposed-name != ""` and not `jw-awaiting-ratify`).
Never folded: `RunView` (the run's status), `SeedConfirmStep`,
`SealedPanel`. Accordion state `jw-fold`, closed on every phase change.

`read_ui_state` / `ui_action` exposure of the fold: none, it is
presentation state.

## 6. Questions, answered (2026-09-13)

1. Pile in flow, not docked - a dock would need a third WizardFrame band
   and steal viewport from a tall current step.
2. The setup step folds in as the oldest entry; every folded header
   carries its short summary so the pile reads without opening.
3. An open fold closes on every phase advance.
4. The join wizard gets the same fold in this change-set (§5).
5. In P1 the member list is the current step, unfolded.
