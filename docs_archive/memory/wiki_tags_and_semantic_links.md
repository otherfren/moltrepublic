# Wiki: tags instead of free properties, and semantic links

Status: EXECUTED 2026-09-05. Two UI rounds on Shared Memory /
Multisig-Wiki, requested the same day. Nothing here changes the engine: the header subset
(`docs_archive/memory/knowledge_base_scale.md` §4.4) already reserves
`tags` and already reads any header key that carries a link as a TYPED
RELATION (`wiki_index/graph.rs::collect_typed` - the key is the
predicate). Both rounds are `molt-ui` + `.slint` only.

## Why

An agent reading this wiki has no real-world experience. What a document
IS, and how it relates to the others, has to be written down rather than
inferred. Two affordances carry that: tags (what this is about) and typed
links (how it relates), both authored from the viewer, both landing in
the header the index and the graph already read.

## A. `+ Property` becomes `+ Tag`

Today the headerless document offers the republic's header KEYS, one
click each, and drops the member into the raw editor with `key: ` typed
for them. That teaches the YAML, not the ontology.

- A1 The chip reads `+ Tag`. It keeps its current visibility rule
  (`WikiState.can-add-property`: the base bytes are here and the document
  has no header) - so it disappears once a header exists, in view mode
  and everywhere else.
- A2 Clicking it opens a modal that takes SEVERAL free-text tags: one
  input row per tag, `+` adds a row, `×` drops one. The republic's own
  tags (from `Command::WikiProps`, key `tags`) are offered as chips that
  fill a row - the ontology stays content, never a schema.
- A3 OK writes exactly those tags as the document's header
  (`---\ntags: [a, b]\n---`) and leaves the pane in the VIEWER - the
  member asked for tags, not for a YAML lesson. Cancel writes nothing.
- A4 In the viewer the `tags` key renders as PILLS, one per value, with a
  colour derived deterministically from the lowercased tag. Every other
  key keeps the current infobox row.
  - The hue is computed Rust-side (`wiki::tag_hue`, FNV-1a over the
    lowercased tag → 0..359) and carried on `WikiProp`; `-1` means "not a
    tag". Slint tints the pill from that hue and keeps `Theme.text` for
    the label, so one palette works in both themes.
- A5 The key-chip row (`prop-keys`) goes: it offered arbitrary keys, and
  arbitrary keys are what the semantic-link modal owns now.

## B. "Create semantic link"

One modal, three entry points, always writing into the OPEN document:

- B1 A toolbar button right of the ✏️/👁️ toggle.
- B2 An item in the editor's right-click menu (today cut/copy/paste). The
  menu lives in the shared `AppArea`, so the item is an opt-in property -
  no other field grows one.
- B3 An item in the navigator's file menu. It OPENS the file first and
  then raises the modal, so the subject is the open document in all three
  cases and the write never needs bytes that are not fetched.

The modal:

- B4 **Name** (free text) - the link's display half. Prefilled with the
  target's title, editable.
- B5 **Target** - a wiki file, picked from the folded base. A filter
  field narrows the list; the pick is a path, never free text.
- B6 **Relations** - toggle chips the member switches on, several per
  link. The list is the built-in vocabulary plus every header key this
  republic already uses as a relation (`WikiProps`), plus one free-text
  row for a key the republic has not used yet. Keys are validated against
  the subset (`[A-Za-z_][A-Za-z0-9_-]{0,63}`) before OK arms.
  - built-in: `is_a`, `part_of`, `has_part`, `depends_on`, `causes`,
    `defined_by`, `example_of`, `opposite_of`, `related_to`, `see_also`,
    `supersedes`, `authored_by`.
- B7 OK writes one header entry per selected relation, value
  `[[<target path>|<name>]]` (the form `front_matter::link_target`
  strips). A key that is already there grows into a list rather than
  being replaced; a header that is not there is created. The document
  stays in the viewer, and the write lands on the changeset stack like
  every other edit.
- B8 The header is edited LINE-WISE, never re-emitted: a canonical
  re-emit would rewrite a member's own header (comments, order, style) on
  every link.

## Polish (both rounds)

Same gap everywhere (`Theme.pane-gap`), inputs centred like the chat
composer, every button with a speaking icon, no wall of text in any
string.

## Tests

Rust (`wiki.rs`): the tag hue is stable and case-insensitive; writing
tags creates the header and leaves the editor closed; a semantic link
creates / extends / grows a key into a list; a bad relation key is
refused. Headless GUI: the `+ Tag` chip appears only without a header,
the tag pills render one per tag, the link modal opens from all three
entry points and writes what it showed.
