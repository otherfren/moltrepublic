# Wiki preview: pipe tables

Status: **EXECUTED 2026-09-08.** Built as written here.

## Why

`parse_blocks` drove pulldown-cmark without `ENABLE_TABLES`, so a pipe
table reached the pane as one paragraph of bars. An agent-written wiki
(Deka Knowledge) is full of them.

## Model (`molt-ui/src/wiki.rs`)

- A table is ONE block of kind 6 with `rows` → `cells`. One block per
  table, not per row: the pane's block list has a gap between blocks, and
  a grid must read as one frame. Cost: the preview diff marks the whole
  table changed, never the row.
- A cell carries its own spans, so `[[Name]]` in a cell stays navigation
  (`expand_wiki_links` runs per cell). An `upload:` image inside a cell
  reads as its alt text - a grid holds no picture block.
- `Block.text` is the rows flattened (`a | b` per row, rows joined by a
  newline), so the diff sees a changed cell; `Block.spans` stays empty for
  a table - the one exception to "spans concatenate to text".
- Column share (`Cell.frac`, permille, the same on every row of a
  column): the column's longest cell in characters - a header cell counts
  1.2x for its bold glyphs - clamped to 4..48, plus 2 for the cell
  padding, over the sum. The rounding remainder goes to the last column
  so the shares fill the row. Seen 2026-09-08 without the two weights: a
  `Verfügbarkeit` header over `100 %` cells wrapped mid-word. Alignment (`Cell.align`) is the delimiter row's
  (0 none · 1 left · 2 center · 3 right). Short rows are padded to the
  grid.

## Render (`surfaces.slint`, `BrainBlockView` kind 6)

One framed rectangle (the code block's frame), the header row bold on
`Theme.bg`, a `Theme.line` rule between rows and between columns, cell
text wrapping at the column width, a link-bearing cell through `SpanFlow`.

The column share is a `horizontal-stretch` factor over `min-width` and
`preferred-width` of zero: a cell width derived from the layout's own
width is a binding loop (`layout-cache → width → layoutinfo-h`), and the
cell's inner layout has explicit geometry so the text's natural width
stays out of the cell's constraints.

## Tests

`wiki.rs`: kinds, rows, cells, link in a cell, alignment, shares sum to
1000, a short row padded. `tests/gui/wiki.rs`: the block reaches the pane
with its rows, cells and spans.
