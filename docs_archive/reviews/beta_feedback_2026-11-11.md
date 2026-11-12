# Beta feedback 2026-11-11: action plan

**Status: EXECUTED (2026-11-12). WP1-WP7 are built; WP1b was dropped
(R1). Do not build new work from this document.**

Left as known debt: the GUI's own avatar fitter duplicates the engine's
`image_fit`; a wake line and a dropped-proposal notice announce with a
full session event; a wake command with shell syntax is not `exec`ed, so
its timeout ends the shell only.

Source: first founding by beta testers on v0.0.9, one GUI seat and one
headless seat. Eleven raw findings, grouped into seven work packages.
Code facts below come from reading the tree at `2360fd84`; none was
reproduced live unless it says so.

## 1. Findings against the code

| # | Finding | What the code does today | Class |
|---|---|---|---|
| F1 | Founding must survive hours or days of waiting | See 1.1 | design gap |
| F2 | A tester built the debug profile for 40 minutes | README's first build command is plain `cargo build`; `scripts/build-release.sh` is not mentioned in the build section | docs trap |
| F3 | Wake command is hard to find and understand | Group "Agent wake" is the last group on Settings/General and the first one clipped (`app.slint:8044-8125`) | UX |
| F4 | Settings shows both an X and "Close" | The only full screen with both (`app.slint:7764`, `:9055`); every other full screen has the header X only | UX |
| F5 | Proposal view: navigator and details use half the height with no discussion shown | `DiffViewer` gets a fixed 35 % of the surface (max 60 %) in a decision channel even when the message list is empty (`app.slint:5630-5665`); 320 px fixed in the proposals list (`parts.slint:2758`) | UX |
| F6 | A guided wake skill, shown where the command is entered | A skill modal exists ("Show agent skill...", MCP `wake_skill`), but the text (`WAKE_SKILL`, `molt-mcp/src/lib.rs:90-154`) is a reference, not a step-by-step guide, and renders as raw monospace | content + UX |
| F7 | Bot proposed a logo, the others could not download it | See 1.2 | bug (likely) |
| F8 | Script runs locally but not from moltd; need a button that checks visibility and executability | "Test wake" exists but: runs the SAVED command while enabled by the unsaved draft; stderr goes to `/dev/null`; only `exit N` is shown; no path or exec check. Real wakes drop a non-zero exit silently and have no timeout (`wake.rs:264-326`) | bug + gap |
| F9 | Logo stretched to a stripe | Org banner: 56 px high, stretches to full width, `image-fit: cover` (`app.slint:6147-6183`). No crop or downscale at ingest | bug |
| F10 | Title must be centred | Title sits right of the image in the same row (`app.slint:6200-6210`) | UX |
| F11 | Image left of the title when near-square and title short, else below | Not implemented | UX |

### 1.1 What ends a founding today (F1)

The spec already says "no timeout" for the human waits, and the waits for
ratification, backup confirmation, signatures and genesis are unbounded.
What actually kills a slow founding:

1. `ACCEPT_TIMEOUT` = 90 s (`ritual_member.rs:37`): a joiner whose founder
   is not running at that moment fails. The only timer that needs the
   founder present.
2. One-shot network gates abort instead of retrying: `LIVE_WAIT` 10 s and
   `REPLAY_WAIT` 120 s (`nostr_ritual.rs:33,36`), the Welcome publish
   (1 try), every member publish (1 try), the Seal (3 tries).
3. The Seal is published once and the genesis four times, never again. A
   member that misses either waits forever with no error.
4. The group subscription follows the UTC-day `h` tag and re-subscribes
   with the new tags only (`ritual_net.rs:792-822`). A frame published
   under yesterday's tag is unreachable for a client that was not live.
5. Any restart of founder or member ends the ritual: all state is in
   memory by design (`founding_ritual.md:30-34`). A founder quit sends no
   `Aborted`; accepted members wait forever.
6. Leaving the screen ends it: the founder navigating away from Create
   calls `abandon_ritual` (`session.rs:52-58`); a member opening another
   workspace calls `invalidate_join`.
7. KeyPackage lifetime: about 84 days (OpenMLS default). Not a practical
   limit.

Items 1-4 and 6 are fixable inside the current design. Item 5 is not: a
ritual that survives a restart needs ritual state on disk, which
contradicts the load-bearing invariant "ephemeral, no disk write before
the final seal".

### 1.2 Why the logo did not arrive (F7)

A logo is not a download. `set_image` carries the bytes inline as base64
in the proposal; peers get them with the proposal. Candidate causes, by
likelihood:

1. Empty `value` (display name). The engine does not check it; the
   proposal card renders the preview and the save button only when it is
   non-empty (`parts.slint:1663`).
2. The bot used `share_file` (a temporary chat share served by the
   sharer while online) instead of, or next to, the proposal.
3. The receiver dropped the proposal at ingest (too large or not
   decodable) with a log line only (`net/ingest.rs:271-301`). The cap is
   about 64 KiB of image and shrinks with the roster.

## 2. Work packages

Each package starts with failing tests. GUI packages are checked through
the live-preview chain; no full window build per package. Decision ids
(R1, W5, ...) refer to sections 4-6.

### WP1 Founding without time pressure

No ritual state on disk (R1): the founder's process must stay up for the
whole founding; "unreachable" below means no connectivity.

1. Joiner accept wait (R2, R6, R7): remove `ACCEPT_TIMEOUT` on the Nostr
   leg. The joiner waits without bound with progress notes and Cancel,
   and re-publishes its JoinRequest on a backoff. The founder's spent-seat
   check must answer a repeated request from the same seat and key
   idempotently.
2. Start gates and publishes retry with backoff instead of aborting
   (`LIVE_WAIT`, `REPLAY_WAIT`, Welcome, member frames, Seal); loud after
   N rounds, never a silent stop.
3. Seal and genesis are re-deliverable: the founder re-publishes until
   every seat has confirmed receipt.
4. Subscriptions reach back to the ritual's start: every `h` window
   since then for the group channel, and the founder's inbox replay (R8).
5. Leaving the screen keeps the ritual (R3); it ends by Cancel only. The
   start screen shows one line for a running ritual, a click returns to
   the wizard (R9).
6. Clean quit: a founder publishes `Aborted` (R5), a member declines
   (R10).
7. No second workspace beside a running ritual (R4). Pin the
   per-workspace flock with a two-process test if none exists.
8. Spec: `docs_archive/ritual/founding_ritual.md` updated in the same
   change.

Tests: accept wait outlives a long founder absence (mocked clock);
repeated JoinRequest is idempotent; ritual crosses a UTC-day rollover;
lost Seal and lost genesis are re-delivered; navigate-away keeps the
ritual; clean quit notifies the other side.

### WP2 Release build is the default path

- README minimal, same text for humans and agents: what it is, hero
  image, "What you can build", feature table, technologies (B3, B7),
  then one copyable block per goal (B2): get the signed release, build
  from source (release command only, B1), run, connect over MCP.
- Developer build notes leave the README; the window-build memory
  figure is deleted (B4, B8).
- A debug `moltd` says so: one start log line, "debug" beside the GUI
  version (B5). No hint on plain `cargo build` (B6).
- Test: unit test on the version string.

### WP3 Wake command: findable, explained, testable

- Own Settings tab for the agent hooks (W1).
- Skill: replace `WAKE_SKILL` with the step-by-step guide (source: the
  operator's draft). English in the repo, German through i18n in the
  GUI (W2); raw text with Copy and Save in the modal (W3), saved as a
  markdown file named `moltrepublic-wake-skill`; port and config path filled from the
  live config (W4). `wake_skill` serves the same text.
- One button "Test wake" (W6, W14): takes the text in the field, checks
  it as moltd's user (every path component traversable, file readable
  and executable, shebang interpreter present, program found on moltd's
  PATH), then runs it. Shows the failed check, or exit code plus the
  last lines of stderr. Same behaviour on the MCP tool.
- Timeout: configurable, default 10 minutes (W7). On expiry only the
  script is killed (W13); the skill's script therefore carries its own
  shorter timeout around the agent.
- A failed real wake is logged as structured fields and noted in the
  GUI (W8).
- Chat lines (W5, W10-W12): poke and test wakes write local system
  lines into the general chat of the woken node, one at start and one
  with the result. One switch hides both kinds, past lines included; it
  sits beside the line and in Settings.
- Tests: each check outcome; draft is what runs; stderr tail returned;
  non-zero exit logged and noted; timeout kills the script; chat lines
  appear and the switch hides them.

### WP4 Settings: one way to close

- The footer Close goes, the header X and Escape stay (S1).
- Test: headless GUI test pins exactly one close control.

### WP5 Proposal view uses the full height

- Decision channel: with no messages the diff viewer takes the whole
  body; with the first message the 35 % split and its handle return
  (S2, S3).
- Proposals list: the inline viewer follows the available height
  instead of 320 px (S2).
- Test: geometry test for both states in both views.

### WP6 Logo proposal reaches everyone

Cause unknown (L1), so all three candidates are closed.

- Empty name gets a default; card preview and save depend on the bytes
  only (L2).
- A proposal dropped at ingest shows one notice line on the receiver.
- MCP: `path` argument for the org logo and member avatars, read on the
  node (L3, L10); the `propose` guide states the shape and the cap and
  says that `share_file` is not the logo path.
- An oversized image is downscaled at propose: pixels only, the format
  never changes (L4, L9). Members sign the downscaled bytes.
- Tests: empty name; save with any name; ingest drop surfaces; path
  argument; downscale lands under the cap and keeps the format.

### WP7 Org header: logo and title

Shape is chosen at display from the aspect ratio; the signed bytes are
untouched (L7).

- Square (ratio 0.8 to 1.25, and every portrait image, centre-cropped
  to 1:1): logo left of the title when the title fits one line beside
  it, the pair centred (L5, L6, L11).
- Banner (everything wider, centre-cropped to 3:1), and a square logo
  with a long title: title centred on top, logo below.
- Never distorted. Same fit rule in the logo modal and the proposal
  card.
- Sidebar shows a square logo, else the generic icon (L8).
- Tests: geometry for square+short, square+long, wide, 2:1, portrait;
  sidebar both cases.

## 3. Order

Decided (P1, P2): ritual first, one release at the end.

1. WP1a
2. WP6, WP7
3. WP3, WP4, WP5 (one live-preview pass)
4. WP2
5. Release

WP1b is dropped (R1): a founding does not survive a restart, and the
"ephemeral" invariant stays as it is.

## 4. Decisions (operator, 2026-11-11)

**R: ritual**
- R1 WP1a is enough. No ritual state on disk.
- R2 A joiner may activate a link while the founder is unreachable and
  waits without bound, with Cancel. Under R1 "unreachable" means no
  connectivity; a founder whose process ended has no ritual left.
- R3 Leaving the screen keeps the ritual running; it ends by Cancel only.
- R4 No second workspace beside a running ritual in one client; a second
  moltd instance does that. One workspace is open in one instance at a
  time: the per-workspace flock exists (`molt-storage/src/lib.rs:1195`);
  pin it with a two-process test if none exists.
- R5 A founder quitting cleanly publishes `Aborted` to every seat.
- R6 The 90 s accept error hurt most: first item built.

**B: build and README**
- B1 README leads with the signed release; building from source is the
  offered alternative (release command only).
- B2 One copyable command block per goal; no `AGENTS.md`.
- B3 The hero image stays.
- B4 Developer build notes leave the README; the remaining figures are
  re-checked, they are reported stale.
- B5 Debug marker: start log line plus "debug" beside the GUI version.
- B6 No hint on plain `cargo build`.

**W: wake**
- W1 Own Settings tab for the agent hooks.
- W2 Skill in English in the repo, German through i18n in the GUI.
- W3 Modal shows raw text with Copy and Save.
- W4 Port and config path are filled from the live config.
- W5 Wakes and pokes appear in the general chat until switched off: a
  "do not show again" control beside the post and a switch in Settings.
- W6 One button: check, then run. The check covers what moltd's user can
  see: every path component traversable, file readable and executable,
  interpreter of the shebang present.
- W7 Wake timeout is configurable, default 10 minutes.
- W8 A failed real wake is logged and noted in the GUI.
- W9 The affected tester ran moltd from a terminal (NixOS VM). The
  service file is not the cause; no change to it from this finding.

**S: settings and layout**
- S1 The header X stays, the footer Close goes.
- S2 Both views are fixed.
- S3 With the first message the 35 % split and its handle return.

**L: logo**
- L1 Unknown what the bot called: all three candidate causes get fixed.
- L2 Empty name gets a default; the card depends on the bytes only.
- L3 `path` argument for images on the MCP surface.
- L4 An oversized image is downscaled at propose; members sign the
  downscaled bytes.
- L5 Shapes: square 1:1 (logo left of the title) and banner 3:1 (logo
  below the title).
- L6 Near-square = ratio 0.8 to 1.25; short title = fits one line beside
  the image.
- L7 Crop at display only.
- L8 The sidebar shows the logo when it is square, else the generic icon.

## 5. Decisions, round 2 (operator, 2026-11-11)

- R7 A waiting joiner re-publishes its JoinRequest on a backoff.
- R8 The founder's inbox replay reaches back to the ritual's start.
- R9 A backgrounded ritual shows as one line on the start screen; a
  click returns to the wizard.
- R10 A member quitting cleanly declines, so the founder does not wait
  on a dead seat.
- B7 "What you can build", the feature table and technologies stay.
- B8 The window-build memory figure is deleted, not re-measured.
- W10 Wake and poke lines are local system lines of the woken node.
- W11 Lines for poke and test wakes only: one at start, one with the
  result.
- W12 One switch for both kinds; switching off hides past lines too.
- L9 Downscaling reduces pixels only; the format never changes.
- L11 A portrait image is treated as square (centre crop, left of the
  title).

## 6. Decisions, round 3 (operator, 2026-11-11)

- W13 On timeout only the script is killed, not what it started.
- W14 The test runs the text in the field.
- L10 The `path` argument applies to member avatars too.
