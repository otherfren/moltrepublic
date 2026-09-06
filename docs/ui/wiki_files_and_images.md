# Wiki: images and references to persisted files

Status: **RATIFIED 2026-09-06, not built.** The user decided Q1-Q5 (§6)
the same day: the grammar as proposed, PERSISTENT files only, a click on a
ready image opens it large, downloads are explicit in the wiki unless the
mirror brings the file by itself, the registry lives in the workspace
prefs. Execution starts AFTER the wiki pane performance wave
(`wiki_pane_performance.md` §4 steps 2-6) has merged: this plan builds on
its preview cache and block equality (§3.4).

## 1. The ask (user, 2026-09-06)

A wiki page can show images and point at files the republic keeps. An
image renders as a PLACEHOLDER until its bytes are on this device, then as
the picture. Any other file renders as a link; a click jumps to Shared
Files with the table pre-filtered to that file's checksum. User experience
first.

## 2. What exists (read before designing)

- **Shared Files** (`Surface::Files`, `docs_archive/files/persistent_uploads.md`,
  `docs_archive/files/mirroring.md`): every share is a chat message with a
  stable `MessageId`; `Reply::Uploads` carries one `UploadView` per share:
  `id`, `member`, `name`, `kind` (`file_kind_label`: "Image", "PDF", …),
  `size`, `checksum` (full sha256), `available`, `online`, `availability`
  (`relay-held` · `sharer-only` · `gone`), `expires_ts`, `persistent`,
  `mirrors` / `mirror_held` / `mirror_of`, and a live `download`
  (`DownloadView { phase, percent, path, error }`). MCP `read_uploads`
  serves the same projection; `download_file { id, dest? }` fetches into
  the exchange folder (`download_dir`) and verifies the checksum
  (`status-kind` 2 done / 3 failed in the GUI row).
- **Bytes on this device** come three ways today: the SHARER's own file
  (`files.share_paths[id]`), a completed DOWNLOAD (the path in
  `DownloadView.path`, live only - not remembered across a restart), and a
  complete MIRROR of a persistent file (`MirrorJob.complete`, sealed pieces
  under `<mirror_dir>/<id>/<i>`, opened with the series key
  `ShareIdentity.key_b64` via `molt_net::file_plane::open_piece`). Nothing
  assembles a mirrored file into plaintext yet: `piece_source_if_elected`
  only serves pieces onward.
- **The uploads table already has the jump the ask wants**: a filter
  needle (`filter_uploads`: user, name, checksum substring;
  `ou-filter` in app.slint) and a precedent for pre-filling it from
  elsewhere - `jump-member-uploads` (`actions/org.rs`): set the needle in
  `ChatUiState`, `Command::SelectView { Files, "uploads" }`, refresh.
- **Images in the GUI**: `crates/molt-ui/src/images.rs` decodes with the
  `image` crate under hard limits (8192x8192, no SVG), and `FittedImage`
  downscales; the pending-logo viewer (`on_view_proposal_image`) shows a
  decoded image inline. All decoding runs on the UI thread today - fine
  for an avatar, not for a 12 MB photo.
- **The wiki pipeline**: `parse_blocks` (pulldown-cmark → `Block { kind
  0..4, text, spans }`) drops `Tag::Image` (the alt text flattens into the
  paragraph); `Span { text, link, rel }` carries `.md` links and
  `[[Name]]` forms; `BrainBlockView`/`SpanFlow`/`LinkRun` render them. The
  engine's graph (`wiki_index::graph::body_links`) counts only `.md`
  destinations without a scheme and `[[…]]` - an `upload:` destination is
  invisible to the graph by construction, which is what we want.
- **Agents** write pages through `wiki_edit` (free markdown) and read
  them through `wiki_get`; the friction rounds added a warning channel
  (`mcp_agent_friction_fixes.md` B5) that an unresolvable reference can
  ride.

Verdict: no library question here - the parser, the transfer plane, the
mirror store and the image decoder all exist; the work is one reference
grammar, one resolve/read pair in the engine, and the pane's rendering.

## 3. Design

### 3.1 The reference grammar

A file is named by its content, never by a path or a message id:

```
![Netzdiagramm](upload:3f9a2c1b7e04)          image, renders inline
[Bericht Q3 (PDF)](upload:3f9a2c1b7e04)       any file, renders as a link
```

`upload:` + the sha256 in hex, full (64) or a prefix of at least 12 hex
digits, resolved like a git short hash against the PERSISTENT files:
exactly one with that prefix, else "ambiguous" (shown, never guessed). A
temporary share is not a valid target (Q2): the picker never offers one,
and a reference that resolves only to a temporary share renders as the
`temporary` card (§3.4) - the page waits for the persist vote, it does not
embed something that expires. Plain
markdown - pulldown-cmark passes the destination through, agents can write
it with `wiki_edit`, and the graph ignores it (no `.md`). The parser lives
in `molt_core` (`wiki_refs::file_refs(markdown) -> Vec<FileRef { hex, alt,
image: bool, span }>`), masking code spans and fences like `body_links`.

Rejected: `[[file:…]]` (the double-bracket form is the SEMANTIC link
grammar, a file is not a claim), `molt://file/…` (a URL shape suggests a
fetchable location; the reference is content-addressed), paths under a
`files/` pseudo-folder (renames would rot every page).

### 3.2 Resolution: one engine question, both surfaces

`Command::ResolveUpload { checksum: String }` → `Reply::UploadResolved {
match: Option<UploadView>, ambiguous: bool, temporary: bool, local:
LocalCopy }` with `LocalCopy = None | Own { path } | Downloaded { path } |
Mirrored | Partial { held, of }`; `temporary` is set when the only match
is not persistent (the pane's `temporary` state), `match` then still
carries it so the jump can show it. MCP tool `resolve_upload` (Read scope) - an agent that meets
`upload:…` in a page gets the same answer the pane does (co-equality is
the seat, so the paths are the exchange folder's or elided for an agent,
like `download_dir`).

The engine keeps a small persisted **local-copy registry**
(`files.local_copies: checksum → path`, in the workspace prefs beside
`read_cursors`): fed by every VERIFIED download (`DownloadView.phase` done
with the checksum matched), by own shares, cleared when the file is gone
at read time. A restart no longer forgets what was downloaded.

### 3.3 Bytes: `ReadUploadBytes`, off the actor

`Command::ReadUploadBytes { checksum, cap: u64 }` (INTERNAL, GUI-only: an
agent has `download_file` into its folder) reads the bytes from the best
local source - own path, registry path, or the mirror pieces opened with
the series key and concatenated - inside `spawn_blocking` (the actor
never awaits I/O; the pattern of the export in `session.rs`), re-hashes
them, and answers `Reply::UploadBytes { bytes }` only when the sha256
equals the reference; a mismatch answers an error, never bytes. `cap`
refuses anything larger before reading (images: 32 MiB).

### 3.4 The pane

**Model.** `parse_blocks` learns `Tag::Image` → a block of kind 5 with
`text` = alt and `spans[0].link` = the `upload:` destination; a markdown
link with an `upload:` destination stays a span with `link` = the
destination. `WikiBlock` grows `file-name`, `file-size`, `file-state`
(0 none · 1 unknown · 2 ambiguous · 3 remote · 4 fetching · 5 decoding ·
6 ready · 7 failed · 8 temporary) and `image` (`slint::Image`, empty until
ready); `WikiSpan` grows `file-state` and `file-name` for link runs. Both
are additive fields, so the running nodes' hot-reload survives.

**Bridge.** After a preview lands, `sync_wiki` collects the page's
references (`file_refs`), asks `ResolveUpload` once per distinct hex
(cached per `(checksum, wiki_rev)`; the perf wave's in-flight set idiom),
patches the rows in place as answers arrive (`set_row_data`, never a
model swap), and for a resolved IMAGE with a local copy issues
`ReadUploadBytes`, decodes on a worker (`spawn_blocking`:
`image_from_bytes` limits + a downscale to at most 2048 px on the long
edge - one decode serves the column and the large view), and hands an
RGBA buffer to the UI thread, where the `slint::Image`
is built and cached (`HashMap<checksum, Image>`, LRU by decoded bytes, 64
MiB). `Event::FileTransfer` for a referenced checksum re-resolves that
reference; a mirror completing does the same via the surfaces push
(`mirror_held == mirror_of`).

**Rendering (UX).** An image block:
- `remote` (known, not here): a bordered card in the text column: kind
  glyph, `name · size · von <member>`, the availability word, and ONE
  verb: `Herunterladen` (enabled by `available`; tooltip says why not -
  sharer offline, gone) or, when this seat mirrors and the fetch runs,
  the progress `spiegelt 40 %` with no verb (it will arrive by itself).
  Persistent files only reach this card (§3.1).
- `fetching` / `decoding`: the same card, the verb replaced by the phase
  word - never a spinner, never a modal.
- `ready`: the picture, width = the text column, height by aspect, the
  alt text as caption in the dim tone; a click opens it LARGE (Q3): a
  window-wide overlay in the modal idiom (`ConfirmModal`'s dimmed ground,
  Escape / click outside closes), the image `image-fit: contain`, one
  caption line `name · size · von <member>` and one verb `In Shared Files
  zeigen` (the jump, §3.5). The tooltip on the inline picture carries
  name · size · sharer; its context menu offers the jump too.
- `unknown` / `ambiguous` / `failed` / `temporary`: the card with the one
  word and the reference prefix, in the bad tone (`temporary` in the warn
  tone with the verb `In Shared Files zeigen`, where the persist vote is
  proposed); a `failed` card offers `Erneut laden`.
A file link run (`FileRun`, sibling of `LinkRun`): `📎 name` with
`(size)` in the dim tone; state colours as above; a click jumps (§3.5).
Ghost blocks (removed in a diff) render the card flat, never fetch.

### 3.5 The jump

`WikiState.jump-upload(checksum)` → the `jump-member-uploads` shape: the
needle is the FULL checksum (unique), `SelectView { Files, "persistent" }`
(`"uploads"` for a `temporary` match - the row lives in that table),
surfaces refreshed. The filter field shows the needle, so the
member sees why the table is short and clears it with the existing
control. No second highlighting mechanism.

### 3.6 Authoring

The editor's toolbar gets `+ Datei` beside `+ Tag` / `Verknüpfen`: a
modal listing the republic's PERSISTENT files (a search field over
name/sharer; kind glyphs; an empty list says that a file has to be
persisted first, with the jump to Shared Files), one click inserts `![name](upload:<12-hex>)` for an Image kind or
`[name](upload:<12-hex>)` otherwise at the caret (`cursor-byte-offset`,
the link modal's insertion path incl. its fence/front-matter refusals).
The preview refreshes on leaving the editor as today. Drag from the
Files pane is NOT part of this (Slint drag across panes is the
pane-local gesture we already avoid).

### 3.7 Agents

- `wiki_get` answers `files: [{ hex, name, state }]` per page (resolved
  references), so an agent knows what a page depends on without a second
  call.
- `wiki_edit` warns (B5 channel) on an `upload:` reference that resolves
  to nothing or is ambiguous - it writes anyway (a page may reference a
  file that is shared next).
- `wiki_health` gains `files: { dangling, temporary, ambiguous }` so an
  agent's hygiene pass finds rotting references (a file unpersisted after
  the page was written turns `temporary`).

## 4. Security and limits

- Bytes render only after the sha256 matched the reference (§3.3); the
  transfer plane's own verification is not trusted twice, it is repeated
  on read - a swapped file in the exchange folder is a placeholder, not a
  picture.
- Decoding under `image::Limits` (8192x8192), a 32 MiB read cap, a 2048 px
  downscale, and off the UI thread; SVG is never decoded (the engine
  refuses SVG proposals for the same reason) - it renders as a file link.
- A reference is content-only: no path, no host, nothing an agent could
  point outside the republic's files. `ReadUploadBytes` is INTERNAL; the
  registry holds paths but no surface serializes them for an agent.
- Nothing here writes to the chain: references live in page text; the
  file's persistence stays the `persist` vote's business.

## 5. Stages (each red-first, each green on master)

1. **Grammar + resolve.** `molt_core::wiki_refs` (tests: full/prefix,
   masking, image vs link, alt); `ResolveUpload` + the registry (engine
   tests: own share, verified download survives a reopen, mirror complete,
   ambiguous prefix); MCP `resolve_upload` (co-equality test updated).
2. **Bytes.** `ReadUploadBytes` off-actor with the three sources and the
   re-hash; keystone over loopback: share on A, persist, mirror on B
   completes, B reads the bytes without a download; a tampered exchange
   file answers an error.
3. **Pane.** Parser kinds, additive model fields, bridge resolution +
   decode worker + cache, the cards, `FileRun`, the jump, transfer-event
   refresh, the large view; headless GUI tests: every state renders its
   word, a file link's click sets the needle and the view, a ready
   image's click opens the overlay and Escape closes it, an arriving
   image patches the row in place (a 2x2 PNG fixture), the perf probe's
   frame after a resolve stays partial. i18n DE/EN for every new string.
4. **Authoring.** The `+ Datei` modal and caret insertion; test: the
   markup lands at the caret and previews as a card.
5. **Agents + docs.** `wiki_get.files`, the `wiki_edit` warning,
   `wiki_health.files`; `docs_archive/memory/knowledge_base_scale.md`
   gets a §4.13 "file references"; this document moves to
   `docs_archive/ui/` in the change that lands stage 5.

## 6. Decisions (user, 2026-09-06)

- **Q1 Grammar.** `upload:<hex>` with a 12-hex minimum prefix - as
  proposed.
- **Q2 Temporary shares.** NOT allowed: persistent files only, in the
  picker and in the renderer (the `temporary` card, §3.4).
- **Q3 A click on a READY image** opens it large (the overlay, §3.4);
  the jump to Shared Files is the card's verb and the overlay's.
- **Q4 Auto-fetch.** Explicit: the card's `Herunterladen` verb, unless
  the mirror already brings the file by itself.
- **Q5 The registry** lives in the workspace prefs beside `read_cursors`.
