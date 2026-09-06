# Wiki pane performance - analysis and fix plan

Status: **OPEN - analysis measured 2026-09-06, fix plan partly executed.**
A step marked BUILT below is done; the rest is a proposal to discuss first.

Reported live (2026-09-06, three `moltd` nodes on the "Second Wiki Test"
republic, 125 pages / 9 folders / 293 KB): the wiki pane is sluggish
throughout, "reveal in navigator" takes seconds and sometimes does nothing,
and the file navigator should start collapsed.

## 1. How it was measured

Two `#[ignore]`d probes in `crates/molt-ui/src/tests/gui/wiki.rs`, fed the
real corpus (pulled from the running node over MCP, `wiki_list` +
`wiki_get`; not committed - regenerate with the same two tools):

- `wiki_face_sync_cost_on_a_real_corpus` - the model side of ONE face
  sync (`sync_wiki`) per interaction, headless (`i-slint-backend-testing`,
  no layout, no paint).
- `wiki_pane_paint_cost_offscreen` - the pane rendered OFFSCREEN through
  Slint's software renderer (`MinimalSoftwareWindow`, 1600x1000): layout +
  rasterization per frame, and the dirty region the renderer repainted.

```
MOLT_WIKI_BENCH_CORPUS=<json [{path,content}]> [MOLT_WIKI_BENCH_LAZY=1] \
  [MOLT_PAINT_MODE=full|partial] CARGO_TARGET_DIR=target/dev-ui \
  SLINT_LIVE_PREVIEW=1 cargo test [--release] -p molt-ui --lib \
  --features molt-ui/live-preview wiki_face_sync_cost -- --ignored --nocapture
```

`LAZY=1` is the production shape (K7: metadata first, bytes per opened
document); without it every document's bytes are held, which is where a
long session drifts to (the draft persists fetched bytes).

Both probes run the live-preview INTERPRETER stubs, which is exactly what
the user's three nodes run (`target/dev-ui/debug/moltd`,
`SLINT_LIVE_PREVIEW=1`). Neither measures the femtovg/GL path the nodes
paint through (see finding F1); the software renderer is the closest
CPU-only proxy and is itself one of the proposed remedies.

## 2. Numbers

### 2.1 Model side (`sync_wiki`, debug profile, interpreter stubs)

| step | lazy base (production) | every byte held |
|---|---|---|
| base arrives (125 docs, 134 rows) | 2.2 ms | 2.5 ms |
| `nav_rows()` | 0.17 ms | 0.20 ms |
| `to_draft()` with a tab open | 0.8 ms | **21.3 ms** |
| `build_patch()` (clean / dirty) | 0.006 / 0.008 ms | 0.03 / 0.7 ms |
| `preview()` of the largest doc (8.6 KB, 48 blocks) | - | 1.35 ms |
| nav_mark, no document open | 0.8 ms | 0.8 ms |
| nav_mark, document open | 1.7 ms | **25.8 ms** |
| fold_all / reveal | 1.4 ms | 25 ms |
| one keystroke in the editor | 4.8 ms | **27.4 ms** |
| `content_wanted` fired during the flow | **45 times** | 0 |

Reading: on a held base, 85 % of every sync is `to_draft()` - the WHOLE
model (every document, its base bytes included) serialized to JSON to
compare a string that is thrown away 2 s out of 2 s. On the lazy base the
model side is a few milliseconds - it is not where the seconds come from.

### 2.2 Paint side (software renderer, lazy base, debug profile, interpreter)

| frame after… | dirty region (partial mode) | cost |
|---|---|---|
| first frame (cold) | 1600x1000 | 189 ms |
| nothing changed | none | 0 |
| nav_mark (first) | 412x216 | 35 ms |
| nav_mark (second, un-marks the first) | **1600x1000** | 121 ms |
| open_link (largest doc) | 1600x1000 | 134 ms |
| nav_mark with a document open | 1600x1000 | 124 ms |
| fold_all(false) → 9 rows | 1600x1000 | 124 ms |
| reveal (one folder opens, 14 rows) | 1255x436 | 68 ms |
| fold_all(true) → 134 rows | 1600x1000 | **687 ms** |
| edit_toggle | 1600x1000 | 137 ms |
| keystroke | 1332x927 | 149 ms |
| **the surfaces model rewritten (= any engine event)** | 1600x1000 | **992 ms** |

In `full` mode (a fresh buffer every frame - what a GL renderer does)
every one of those is a 1600x1000 repaint at 105-135 ms; fold_all(true)
stays at 688 ms (1.02 s in a second run under load).

### 2.3 Release profile (`--release`, still the interpreter stubs)

Model side, every byte held: nav_mark with a document open 1.4 ms,
keystroke 1.4 ms, `to_draft()` 0.69 ms, `preview()` 0.25 ms - the debug
profile inflates the model side about 18x; lazy base: 0.1-0.5 ms per
interaction, `content_wanted` still 45x.

| frame after… | partial mode | full mode |
|---|---|---|
| first frame (cold) | 17.6 ms | 12.6 ms |
| nav_mark (second) | 5.4 ms (1600x1000) | 4.3 ms |
| open_link (largest doc) | 5.7 ms | 5.2 ms |
| fold_all(false) → 9 rows | 6.3 ms | 6.1 ms |
| reveal (14 rows) | 2.4 ms (412x166) | 4.9 ms |
| fold_all(true) → 134 rows | **33.7 ms** | 32.9 ms |
| the surfaces model rewritten (= any engine event) | **42.1 ms** | 43.1 ms |
| keystroke | 8.7 ms | 7.0 ms |

Debug-to-release ratio on the paint side: 20-25x, uniformly. The
generated (compiled) module would take a further, unmeasured share off
the interpreter's binding evaluation.

## 3. Findings, ranked by what they cost the user

**F1 - The daily driver is the slowest possible build flavour, and the
machine has no GPU.** The three live nodes are `target/dev-ui/debug/moltd`
with `SLINT_LIVE_PREVIEW=1`: the dev profile (no optimisation), the Slint
INTERPRETER instead of the generated module, and femtovg over OpenGL - which
on this Qubes VM is Mesa llvmpipe (`LIBGL_ALWAYS_SOFTWARE=1` from
`/etc/profile.d/qubes-gui.sh`; the node runs four `llvmpipe-*` threads).
Each factor multiplies: the probes show 110-135 ms per frame and 5.5 ms per
navigator row in this flavour, before any GL rasterization - and the same
probes in the release profile run 20-25x faster (§2.3). The product build
(release, generated module) is a different machine; nobody has run the
wiki on it here (§5 Q1).

**F2 - Almost every interaction repaints the whole window.** Even with a
renderer that CAN repaint a region, a mark that moves from one row to
another dirties 1600x1000 (§2.2) - the `font-weight: marked ? 600 : 400`
and colour changes invalidate the row's text metrics, the layout above it,
and with it the pane. femtovg/GL never repaints partially anyway. Hover over
navigator rows, in contrast, cost nothing offscreen.

**F3 - The navigator is eager, heavy, and starts at its worst.** All
folders open by default, so the pane starts with 134 rows; every row
instantiates a `ContextMenuArea` with a seven-item `Menu`, a `TouchArea`
with drag logic, two variant rectangles, the emoji cell, the label with its
tooltip `changed` handler, and the rename variant - 687 ms to instantiate
125 rows, i.e. what a "reveal" into a 30-file folder costs on top of its
repaint. `ScrollBody` is a `Flickable` over a `VerticalLayout`: nothing is
virtualised, off-screen rows are laid out and painted.

**F4 - `sync_wiki` rebuilds the whole face on every callback.** Every
click, keystroke and reply runs: `to_draft()` (F4a: full JSON of all docs +
base bytes, 21 ms held), `build_patch()` + `cs_patch` (the whole patch
string pushed into a modal that is not open), `preview()` (markdown parse
of working AND base text plus a Myers diff - per KEYSTROKE, while the
editor is up and the preview is not even on screen), `infobox`, `links`,
`link_targets`, the tag/link modal drafts, `nav_rows`/`tab_rows`/
`stack_rows` and a fresh `VecModel` per block's spans. Each is cheap alone
on the lazy base; together they are 5-27 ms of model work per keystroke
before Slint sees anything.

**F5 - `content_wanted` storms.** While a document's bytes are in flight,
every sync asks again (`wants_content()` is stateless): 45 `WikiGet`
round-trips in a 60-interaction flow, each reply re-syncing the face. On a
busy engine (three nodes gossiping, the graph build) these queue.

**F6 - Reveal does not scroll, and is disabled when it is needed most.**
`Wiki::reveal()` opens the folder chain and marks the row - nothing moves
the navigator's `Flickable`, so a row below the fold is "revealed"
invisibly (the "does nothing" report). `can-reveal` is `active != marked`,
so once the row IS marked but scrolled away the button is dead.

**F7 - Every engine event rewrites the surfaces model, and that is the
single most expensive frame there is.** `push_surfaces` runs `sync_rows`
(no equality) with fresh `ModelRc`s per row for every chat, proposal,
presence (every 30 s per peer) and session event; the memory pane sits
inside that `for s in root.surfaces` repeater, so every `s`-bound binding
re-evaluates and the nested repeaters rebuild. Measured: **992 ms** per
event in the live flavour, 42 ms in release - the largest frame in both
tables, and it fires without the user touching anything. With three
nodes gossiping, this alone reads as "the pane is sluggish".

## 4. Fix plan

Ordered by payoff over effort. Steps 1-3 need no discussion (pure waste
removal, no visible change); 4-6 change behaviour or structure.

1. **BUILT 2026-09-06 (requested, landed with this doc).** Base folders start
   CLOSED (a folder the member creates opens; the draft keeps whatever
   state they left - `Wiki::ensure_folder_chain`); reveal returns the
   marked row's index, the bridge hands it to `WikiState.nav-scroll-to`,
   and `ScrollBody` centres that row (`scroll-to-row`, index x
   `row-stride`); the button is enabled whenever a document is active.
   Trap met on the way: when the rows arrive WITH the request, the
   `Flickable`'s `viewport-height` (bound to the layout's
   preferred-height) is still the old value inside the `changed` handler -
   the repeater updates in the next layout pass - and a viewport clamped
   against it scrolls nowhere. `ScrollBody` therefore derives the content
   height from `row-count` (the model's length) whenever it is given one.
   Keystone: `reveal_scrolls_the_navigator_to_the_marked_row`.
2. **Surfaces mirror (F7) - BUILT 2026-09-06, molt-ui only.** `sync_rows`
   now carries the row type's own equality, and every surface row's five
   nested models (`log`, `pending`, `declined`, `accepted`, `views`) are
   PATCHED in place against the row already on screen, keyed by surface -
   so a chat line grows the chat log and touches nothing else. Same
   discipline applied to the other per-event rewrites: `chain_rows`,
   `selected_decision`, the charter columns, the wizard's relay picks and
   the workspace cards (nested member chips compared by content).
   Keystones in `crates/molt-ui/src/tests/gui/mirror.rs`:
   `an_unchanged_surfaces_push_keeps_every_model_instance`,
   `a_new_chat_line_grows_the_log_model_in_place`,
   `an_unchanged_surfaces_push_paints_nothing`. Measured on the same
   corpus, same flavour, in one run (`surfaces_push_frame_cost_offscreen`,
   `#[ignore]`d): the wholesale rewrite 397/782 ms over two runs,
   `apply_surfaces` with an unchanged bundle **drew=false**.
3. **`sync_wiki` diet (F4, F5) - molt-ui only, no .slint change.**
   - `to_draft()` only when the 2 s guard is due AND a generation counter
     (bumped by every mutating verb) moved - never on the keystroke echo.
   - `build_patch()`/`cs_patch` cached by that generation; the modal reads
     it when it opens.
   - `preview()` only when `!editing` and the (id, raw, base) triple
     changed; the block/spans models patched in place as today.
   - `wants_content()` remembers what it asked for (`in_flight: BTreeSet`,
     cleared by `load_base`/`content_failed`).
   - Modal drafts (tags/link) synced only while their modal is open.
4. **Navigator rows (F3) - .slint.** One `ContextMenuArea` per navigator,
   its `Menu` built from the marked row (the row's right-click marks then
   shows it - the click already does the marking); the tooltip `changed`
   handler only on the hovered row; and the navigator on a virtualising
   `ListView` (std-widgets) so off-screen rows do not exist. The drag/drop
   gesture reads row indices from `mouse-y / stride`, which a `ListView`
   keeps.
5. **Renderer choice on a GPU-less box (F1, F2).** The choice is BUILT
   (2026-09-06): `[ui] renderer = "auto" | "software" | "gl"`, default
   `auto`, applied by `molt-app` through `slint::BackendSelector` before
   the window exists - a runtime choice, like headless. A set
   `SLINT_BACKEND` still wins (the selection is then skipped), so the
   one-off trial needs no file edit. STILL OPEN: the measurement itself -
   run the live nodes with `SLINT_BACKEND=winit-software` and
   `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console,overlay`, then decide
   what a GPU-less box should carry in its config. Partial repaint only
   pays with step 6; without it the software renderer still repaints the
   window on every mark (§2.2), just without the GL round trip.
6. **Marks without relayout (F2).** Keep row text metrics constant (weight
   via colour only, or a fixed-width label) so a mark dirties two rows, not
   the window - only pays off with a partially-repainting renderer (step 4).

## 5. Open questions

- **Q1 - Which flavour is the daily driver?** The interpreter build exists
  for .slint iteration; if the nodes are driven all day, a compiled RELEASE
  `moltd` (one 13 GiB window build per .slint change-set) is the honest
  baseline - and the product numbers are unmeasured until then.
- **Q2 - Software renderer as the default on Qubes?** Partial repaint is
  its edge; it needs step 6 to matter for marks, and the text rendering
  differs slightly (no subpixel AA). Config key or env var? *Answered: a
  config key* (`[ui] renderer`, built 2026-09-06), default `auto` - whether
  a GPU-less box should ship `software` stays open until step 6.
- **Q3 - `ListView` for the navigator** changes the drag/drop and inline
  rename code paths (row elements are recycled). Worth it at 125 pages; at
  a 1000-page wiki it is the only thing that keeps the pane usable.
- **Q4 - The 5.8 GiB stray `target/` under `crates/molt-ui/src/tests/gui/`**
  (ignored, from a mis-rooted `CARGO_TARGET_DIR`) is not performance, but
  it is disk the window build will want; delete it?
