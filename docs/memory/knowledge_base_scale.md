# Knowledge base at scale: the shared wiki for tens of thousands of entries

**Status: OPEN - design of 2026-09-03, decisions ratified by the user the
same evening (§1), work packages K0-K7 not built.** Extends the executed
fold design `docs_archive/memory/shared_memory_real.md` and the export
`docs_archive/memory/wiki_export_plan.md`; K6 changes what a checkpoint
carries (`docs_archive/chain/log_compaction.md` §B.6a) and is gated on a
review of §4.9 before it is built.

The goal: a republic's shared wiki becomes a general, decentralised
knowledge base - tens of thousands of entries, dense cross-references, a
per-republic ontology - that a member's agents read over MCP with a
read-only key, while other agents fill it under instruction.

## 1. Decisions (user, 2026-09-03)

1. **Governance stays per patch.** Every change is one threshold-signed
   block of at most one publish budget (~70 KiB). Agents are seats: the
   member's agent proposes and approves on instruction, so 1,500 votes are
   not a throughput problem. No bundle approval, no chunked changesets.
2. **A fold cache, and superseding only on collision.** An accepted patch
   re-checks the pending ones; a pending patch is dropped only when it no
   longer applies (already the rule today, §2) - what changes is the cost.
3. **The wake hook on new proposals** - it already exists (§4.8); K3 pins
   and documents it.
4. **Front matter, YAML subset, open keys.** No mandatory field. `tags` and
   `aliases` are the only reserved keys (the Obsidian conventions). A value
   that is a link (`[[Name]]` or a `.md` path) is a typed relation whose
   predicate is the key: the Semantic-MediaWiki triple, expressed in the
   header instead of the sentence. The ontology is content (pages), never
   code. The GUI renders the header as an infobox, never as raw YAML.
   Typed inline links in prose are deferred (§6).
5. **A second MCP key with read-only scope**, issued in a second block of
   the Settings › MCP tab exactly like the seat key (show once, copy,
   rotate, revoke). Empty means OFF, never "unauthenticated". Scope is
   host-local: the republic sees no roles, the human restricts their own
   tool (`docs_archive/security/mcp-security.md` §"The host boundary").
6. **Every index is derived**: a cache over the fold, RAM-first, rebuilt
   from the fold, never consensus input, never persisted outside the
   at-rest-sealed workspace. Three of them: paths (K1), the link graph
   (K4), full text (K5).

## 2. What exists (verified 2026-09-03, file:line)

- **The fold is recomputed on every read, and the whole tree ships with
  every Memory snapshot.** `wiki_tree()`
  (`crates/molt-engine/src/proposals.rs:1684`) clones every applied payload
  (`applied_values`, `:1745-1762`) and folds from scratch; the snapshot
  branch (`:1861-1874`) does the same fold a second time and materialises
  every page's full content into `SurfaceSnapshot.wiki_tree`
  (`crates/molt-core/src/lib.rs:5484`). No paging exists in any read
  command (`Command::ReadState`, `molt-core/src/lib.rs:3622`).
- **Superseding is already collision-only, but clones the tree per pending
  patch.** `supersede_stale_wiki` (`proposals.rs:1720`) folds once, then
  `wiki_patch_applies` (`:1696`) clones the WHOLE tree (`:1706`) for every
  pending `wiki_patch` and runs the strict apply. Pending patches are
  checked against the base, not against each other. Five call sites:
  `chain/governance.rs:851`, `events.rs:316`, `events.rs:374`,
  `chain/projection.rs:343`, `chain/projection.rs:546`.
- **The fold itself** (`crates/molt-core/src/wiki_fold.rs`): a strict
  git-format parser + apply of its own (`diffy` was rejected, `:11-14`);
  `PatchFile` (`:23`) exposes `old_path`/`new_path` per file, so a patch's
  touched paths are available without a tree; `apply_patch` (`:310`) works
  on a clone and reads/writes only the paths named in the patch; no size
  cap lives in the fold (the cap is `validate_payload_fits` at propose,
  `proposals.rs:616`); depth cap 8 segments (`valid_path`, `:217`).
- **Every wiki patch lives forever in the checkpoint.** `applied_lww_slot`
  returns `None` for `Surface::Memory` (`crates/molt-core/src/chain.rs:432`),
  so every entry accumulates; the blob IS `CheckpointState` (`chain.rs:321`)
  and a pruned holder re-seeds its projection from `blob.applied`
  (`crates/molt-engine/src/chain/projection.rs:512-521`) and re-folds.
  Summaries are applied at fold time in the engine's `fold_one`
  (`crates/molt-engine/src/chain/verify.rs:585-590`), not at the cut.
- **Links** are markdown links whose target ends in `.md`, parsed in the
  GUI (`crates/molt-ui/src/wiki.rs:1944 parse_links`, pulldown-cmark),
  resolved exact-path-then-unique-basename (`open_link`, `:906-915`). No
  link graph exists anywhere; `links()` (`:1524`) re-parses one document.
- **The GUI holds every page twice** (`Doc.raw` + `Doc.base`,
  `wiki.rs:93-104`), reconciles the base quadratically on every mirror
  tick (`set_base`, `:355-437`), and copies every page's content through
  `surfaces.rs:1220-1225` → `mirror.rs:1216-1231` → `wiki_bridge.rs:355-372`.
- **MCP has one token and one scope.** `serve_conn`'s auth state is a
  single `bool` (`crates/molt-mcp/src/lib.rs:125`); `initialize` compares
  constant-time (`:200-216`); `tools/call` maps name → `Command` in
  `call_tool` (`:289`); `ToolDef` (`:641-655`) carries no scope; stdio is
  authenticated by construction (`:39-43`). The accept loop reads the
  CURRENT token from the engine per connection (`live_token`, `:104-110`),
  so a rotation applies to the next connection - the panel note
  (`crates/molt-ui/src/i18n.rs:749`) and `mcp-security.md:198-200` still
  say otherwise. Config: `McpConfig` is `deny_unknown_fields`
  (`crates/molt-config/src/lib.rs:277-291`); `NODE_POSTURE_KEYS` is a fixed
  `[&str; 10]` (`crates/molt-core/src/lib.rs:5073`); `INTERNAL` is a fixed
  `[&str; 65]` inside the co-equality test (`molt-mcp/src/lib.rs:1770`);
  `SessionSettings.mcp_token` is `skip_serializing`
  (`molt-core/src/lib.rs:390-391`). The Settings › MCP tab is the
  `set-tab == 6` branch (`crates/molt-ui-window/ui/app.slint:8126-8195`),
  its token row `tok-row` (`:8145-8188`), rotate handler
  `crates/molt-ui/src/actions/settings.rs:72`, the door
  `Command::SetNodePosture` (`settings.rs:162-166`). No headless test
  drives the MCP tab.
- **The proposal wake hook exists.** `maybe_wake_pending`
  (`crates/molt-engine/src/chat.rs:894`) runs `poke_wake_command` with
  `MOLT_WAKE_REASON=vote_pending` when a genuinely new foreign proposal
  waits on this seat (`net/ingest.rs:318`) and on open (`session.rs:1389`),
  debounced by `WAKE_HOLDOFF_SECS = 300` (`chat.rs:37`) and serialised by
  `WAKE_RUNNING` (`:42`). Env: `MOLT_WAKE_REASON`, `MOLT_WAKE_BY`,
  `MOLT_WAKE_WORKSPACE` (`:946-948`). The command is host posture
  (`Command::SetWakeCommand` only; `PatchSettings` refuses it,
  `session.rs:462-466`).
- **Caps a builder agent meets:** `OPEN_CARDS_PER_PROPOSER_MAX = 64` open
  proposals per proposer (`chain/governance.rs:17`); one publish budget per
  patch.
- **Dependencies:** `pulldown-cmark` (parser only) is a workspace dep used
  by molt-ui; molt-core has no markdown/yaml dep and states a lean-deps
  posture; `tantivy`, `yaml-rust2`, `petgraph` are absent from the lock
  file. Off-actor work follows `cmd_wiki_export`
  (`session.rs:2131-2194`): `cmd_tx.upgrade()` → `tokio::spawn` →
  `spawn_blocking` → an engine-internal `Net*` command carries the result
  back (`Command::NetWikiExportDone`, `molt-core/src/lib.rs:4002`).

## 3. The scale model

| Quantity | Assumption | Value |
|---|---|---|
| Pages | 50,000 × 2 KiB average | 100 MiB folded tree |
| Patches | 1,500 minimum (cap 70 KiB), 5,000-20,000 with edits | 100-500 MiB history |
| `read_state(memory)` today | fold all + clone all + serialise the tree | ~100 MiB per call, on the actor |
| `supersede_stale_wiki` today | fold + pending × tree clone | 50 pending = 5 GiB copied per applied block |
| Checkpoint blob today | the full patch history | ≥ the tree, forever |

Targets: a list page ≤ 100 KiB; one page per `wiki_get`; a search or graph
query in milliseconds on the actor; superseding in O(touched paths); a
cut that carries the tree once. The history bound is the only part that
touches consensus (K6); everything else is a cache.

## 4. Design

### 4.1 The fold cache (K0)

`State.wiki_cache: Option<WikiCache>` next to `chain.applied`
(`crates/molt-engine/src/lib.rs:646`):

```rust
struct WikiCache { tree: BTreeMap<String, String>, rev: u64,
                   folded: usize, epoch: u64 }
```

- `State.applied_epoch: u64` bumps on every NON-append mutation of the
  Memory projection: `apply_chain_to_state` (`projection.rs:506`),
  `set_checkpoint_blob` (`:112`), restore/replay. Appends
  (`projection.rs:343`, `events.rs:374`) do not bump.
- `wiki_tree_cached(&mut self) -> &BTreeMap<..>`: if the cache's `epoch`
  matches, fold only the entries after `folded` (iterating by reference - a
  new `applied_iter(Surface)` replaces the cloning `applied_values` on this
  path); else refold everything. `wiki_tree()`, the snapshot branch and
  `cmd_wiki_export` (`session.rs:2145`) all go through it; `ReadState`
  refreshes before `snapshot(&self)`.
- Keystones (red first): `the_fold_cache_equals_a_fresh_fold_after_every_block`
  (two-instance loopback; after each applied block, rebase and cut:
  `cached == wiki_fold(all)`); the existing
  `a_checkpoint_cut_keeps_the_wiki_fold_identical`
  (`chain/checkpoint_tests.rs:630`) stays green.

### 4.2 Superseding in O(touched paths) (K0)

- `wiki_fold::touched_paths(&[PatchFile]) -> BTreeSet<String>` (old and
  new side of every file).
- `State.wiki_pending: HashMap<u64, PendingPatch { files: Vec<PatchFile>,
  paths: BTreeSet<String> }>` filled when a `wiki_patch` proposal registers
  (`governance.rs:832-846`, `events.rs:293-311`), dropped when it leaves
  `Proposed`. Runtime-only, never persisted.
- `supersede_stale_wiki(&mut self, moved: Option<&BTreeSet<String>>)`: the
  append paths pass the applied patch's paths; rebuild paths pass `None`.
  For each pending patch: if `moved` is `Some` and disjoint from its paths,
  it still applies - skip. Otherwise the strict apply runs on a RESTRICTED
  tree holding only that patch's paths. Equivalence holds because
  `apply_patch` reads and writes only the paths it names
  (`wiki_fold.rs:310-361`); a keystone pins restricted == full verdict over
  every fold fixture, and one pins that a patch on a foreign path survives
  an applied block untouched while an overlapping one is superseded
  (extending `a_sealed_wiki_patch_supersedes_overlapping_pending_patches`,
  `chain/projection_tests.rs:499`).

### 4.3 Paged reads (K1)

New commands in `molt_core::Command`, tools in `molt-mcp`, scope Read:

- `WikiList { prefix: Option<String>, cursor: Option<String>, limit: u32 }`
  → `Reply::WikiList { docs: Vec<WikiDocMeta>, next_cursor, total: u64,
  wiki_rev: u64 }`. `WikiDocMeta { path, bytes: u64, title: Option<String>,
  kind: Option<String> }` (`title` = first heading, `kind` = the header's
  `type`). Limit clamped to 1..=500 (default 100); the cursor is the last
  path (`BTreeMap::range`); `prefix` is a folder prefix.
- `WikiGet { path }` → `Reply::WikiDoc { path, content, wiki_rev, props:
  Value, links_out: u32, links_in: u32 }` (`props` = the parsed header, an
  object or null; the link counts come from K4 and are 0 before it).
  Unknown path → an honest `MoltError`.
- `read_state`'s tool description points at them. The snapshot keeps
  carrying `wiki_tree` until K7 removes it together with the GUI's lazy
  loading (§4.10) - one contract change, not two.

### 4.4 Front matter: the YAML subset (K4)

Where: `crates/molt-engine/src/wiki_index/front_matter.rs`, pure
functions, no I/O; molt-ui reuses them. Parser: `yaml-rust2` (0.12, YAML
1.2, pure Rust) driven through its `parser::Parser` EVENTS, never through
`YamlLoader`, so a plain scalar keeps its raw text - the subset is defined
over events and kills the `no` → `false` class of surprises by rule.

The block: the document's first line is exactly `---`; it ends at the next
line that is exactly `---` or `...`; anything else means no header (the
whole document is body). Header longer than 64 KiB → invalid.

The subset (anything outside it → the document has NO properties, plus an
error string; deterministic on every node):

- one document, top level a mapping; keys are plain scalars matching
  `[A-Za-z_][A-Za-z0-9_-]{0,63}`, unique;
- a value is a scalar, a sequence of scalars, a mapping of scalars, or a
  sequence of mappings of scalars - one level of structure below the key,
  never deeper;
- a plain scalar matching `-?[0-9]{1,18}` is an Integer; every other
  scalar is a String (quoted scalars always are). No booleans, nulls,
  floats, dates as types;
- anchors, aliases, tags (`!`), merge keys, multiple documents → invalid;
- a String is a LINK iff it is `[[Name]]` / `[[Name|display]]` (display
  stripped) or ends with `.md`;
- reserved: `tags` and `aliases` (sequence of strings, or one string);
  conventional, not enforced: `type`, `title`.

The fold never reads the header: a malformed header never voids a patch.
The propose path parses touched documents and returns warnings on the
`Reply` (`Reply::Proposed` gains `#[serde(default)] warnings: Vec<String>`),
the card shows them, members may decline. Ontology as content: a
convention, e.g. one page per type under `_ontology/`, listing the keys in
use; the tool does not read it.

Worked example (a person page):

```yaml
---
type: person
aliases: [P. Müller, Müller]
tags: [gruender, berlin]
works_at: "[[Acme GmbH]]"
knows: ["[[Anna Schmidt]]", "[[Bob Meier]]"]
born: 1975
---
```

A qualified relation is a mapping of scalars under the predicate:

```yaml
works_at:
  to: "[[Acme GmbH]]"
  since: 2019
  role: CTO
```

`to` is the conventional object key of a qualified relation; any other
link-valued key inside the mapping is ALSO an edge with the outer key as
predicate.

### 4.5 The link graph (K4)

`crates/molt-engine/src/wiki_index/graph.rs`:

- `body_links(markdown) -> Vec<String>`: pulldown-cmark link destinations
  ending in `.md` (moved from `molt-ui/src/wiki.rs:1944`, which then calls
  it) PLUS `[[Name]]` / `[[Name|display]]` in text events - the readable
  form in prose, rendered as a link by the GUI (K7).
- Resolution, in order: exact path → unique basename (today's rule) →
  unique alias (from `aliases`) → ambiguous / missing (kept as a dangling
  edge by target string).
- `WikiGraph { docs: BTreeMap<path, DocMeta>, out: HashMap<path, Vec<Edge>>,
  inn: HashMap<path, Vec<Edge>>, dangling: HashMap<target, Vec<(path, Edge)>>,
  aliases: HashMap<String, Vec<path>> }`,
  `Edge { to, predicate: Option<String>, header: bool }`.
- Build: full from the folded tree off-actor at workspace open (the wiki
  export pattern; the result comes back as an internal
  `Command::NetWikiIndexReady { rev, graph }`); incremental on-actor per
  applied patch for the touched paths (drop their out-edges and reverse
  entries, re-parse, re-add, re-resolve dangling edges naming the new
  document, turn a deleted document's in-edges dangling). A pure function
  of the tree, so every node computes the same graph.
- Tools (Read): `wiki_links { path, direction: out|in|both, predicate:
  Option<String>, limit, cursor }` → edges; `wiki_neighbors { path, depth:
  1|2, limit }` → paths with distance (BFS, cap 500). Both carry
  `index_rev` (the fold revision the graph reflects) and `wiki_rev`.

### 4.6 Full-text search (K5)

- `tantivy` with `default-features = false, features = ["stopwords",
  "lz4-compression"]`: the DEFAULT feature set pulls `zstd` (C, via
  `columnar-zstd-compression`, verified against the crate's manifest
  2026-09-03) and would silently break the default build's pure-Rust
  posture. `stemmer` only if `frostem` proves pure Rust under `cargo tree`.
  Guard: `crates/molt-engine/tests/c_free_guard.rs` mirrors
  `crates/molt-net/tests/ring_free_guard.rs` with `-i zstd-sys` (and
  `-i libsqlite3-sys`) on `-p molt-engine -e no-dev`.
- Schema: `path` STRING|STORED (the delete key), `title` TEXT|STORED,
  `body` TEXT, `folder` STRING, `facet` Facet (`/tag/<t>`, `/type/<t>`,
  `/prop/<key>/<value>` for scalar String properties ≤ 64 chars).
- Ownership: an off-actor `WikiIndexer` task owns the RAM index
  (`Index::create_in_ram`) and its `IndexWriter`; the actor sends it
  `IndexDelta { rev, upserts, deletes }` per applied patch and the full
  tree at open; the task does `delete_term` + `add_document` + `commit`.
  The actor holds the `IndexReader` (`ReloadPolicy::Manual`, Send + Sync)
  and runs the QUERY itself - milliseconds of CPU, no await, so the
  handler stays synchronous. Before the first commit a search answers
  `index building`, never an empty result dressed as "no hits".
- Tool (Read): `wiki_search { query, tags: Vec<String>, type:
  Option<String>, folder: Option<String>, limit, cursor }` → hits
  `{ path, title, score, snippet }`, `next_cursor` (an offset),
  `index_rev`, `wiki_rev`. Query syntax is tantivy's
  (`+must -not "phrase" title:term`); facets become `TermQuery` clauses in
  a `BooleanQuery`; snippets from `SnippetGenerator` over `body`.
- Memory: the index is roughly the size of the text; it lives only while
  the workspace is open and is rebuilt from the fold at every open. Nothing
  touches disk.

### 4.7 The read-only key (K2)

- **Config / settings:** `McpConfig.read_token: String`
  (`#[serde(default)]`; template `molt-config/src/lib.rs:671-682`, salvage
  `:929-942`, writer `:1120-1123`); `Settings.mcp_read_token`; the
  session twin `SessionSettings.mcp_read_token` with `#[serde(skip_serializing,
  default)]` exactly like `mcp_token`; `NODE_POSTURE_KEYS` → 11 entries;
  `NodePosture.mcp_read_token: Option<String>` (`None` = keep);
  `cmd_set_node_posture`, `apply_stored_posture` and the configstore
  round-trips carry it. `--generate-config` leaves it EMPTY: the key is
  issued in the panel. No restart flag: the accept loop already reads live.
- **Scope in molt-mcp:** `enum Scope { Read, Seat }`; `ToolDef.scope:
  Scope` is a REQUIRED field with no default; `serve_conn` keeps
  `Option<Scope>` instead of `bool`; `initialize` compares the seat key
  first, then the read key, both constant-time, and an EMPTY read key
  matches nothing; stdio is `Seat`; an empty seat key keeps today's
  meaning (unauthenticated → `Seat`). `tools/list` returns only the
  session's scope; `tools/call` on a `Seat` tool under `Read` answers
  `-32001 unauthorized: read-only token`.
- **The Read set, pinned by a test:** `read_state`, `read_chain`,
  `list_proposals`, `status`, `read_members`, `read_uploads`,
  `read_session`, and the wiki tools `wiki_list`, `wiki_get`,
  `wiki_search`, `wiki_links`, `wiki_neighbors`. Deliberately `Seat`:
  `mark_read` / `mark_channel_read` (read receipts cross the wire),
  `download_file` (writes the host's disk), `read_ui_state` / `navigate` /
  `ui_action` (the human's screen), `wiki_draft_load` (the human's draft),
  `wiki_export`, `net_list_backups`, and everything that proposes, votes,
  chats or configures.
- **GUI:** a second block in the MCP tab (assumption: a block below the
  seat key, not a tenth Settings tab - the tab-count test at
  `crates/molt-ui/src/tests/gui/layout.rs:674` stays untouched) with the
  same `InsetWell` + peek + copy + Rotate, plus Issue (when empty) and
  Revoke (empties it) and one line of scope: "Reads only: state, chain,
  members, files list, wiki". Root property `cfg-mcp-read-token`,
  callbacks `rotate-read-token()` / `revoke-read-token()`; handlers next to
  `actions/settings.rs:72`, fields in `settings.rs:135-137` / `:207-209`;
  i18n keys in both arms (`tests/i18n.rs:227` guard). The stale "takes
  effect on restart" note (`i18n.rs:749`) is corrected in the same change.
- **Docs:** `docs_archive/security/mcp-security.md` gains the read key
  (table, errors, host-boundary list) and loses the stale
  `save_settings.mcp_token` rotation claim (`:198-200`).

### 4.8 The wake hook (K3)

Nothing new to build - K3 pins what exists and makes it usable for a
proposal stream: a keystone that a peer's `wiki_patch` proposal wakes the
opted-in seat with `MOLT_WAKE_REASON=vote_pending` (the poke E2E at
`crates/molt-engine/tests/poke.rs:22` is the model); a new
`MOLT_WAKE_PENDING=<n>` env var (proposals waiting on this seat) so the
woken agent knows the size; and the agent contract in the config template
comment (`molt-config/src/lib.rs:626-632`): loop `list_proposals` until
nothing waits, because the 300 s holdoff swallows a burst on purpose. The
holdoff stays - a running agent drains the stream itself.

### 4.9 A cut carries the tree (K6 - review before build)

Today a cut summarises Organization and Files slots but archives every
wiki patch (§2), so a pruned holder and every rejoiner re-fold the whole
history, and the blob only grows. Target: the Memory group of a checkpoint
carries the FOLDED tree once, plus the non-wiki Memory entries.

- **Summarise at the cut, from the accumulated group:**
  `summarize_memory(group) -> group'` folds the group's `wiki_patch` entries
  (in order, over a `wiki_base` entry if the group starts with one) into
  ONE synthetic entry `(0, {"op": "wiki_base", "rev": N, "tree": {path:
  content, ...}})` placed first, keeps every non-wiki entry, and leaves
  `consumed_ids` untouched (every patch id stays consumed, `verify.rs:595`).
  `molt_core::wiki_fold::fold_one` learns `wiki_base`: the tree becomes the
  base, `rev` becomes `base.rev`; honoured only as the FIRST Memory entry
  of a projection - a later one is void.
- **A new variant, not a new tag:** `ChainChange::CheckpointFolded { upto,
  state_hash }`. The layout tag stays `molt-chain-checkpoint-v8` - the
  bytes differ by content, the VARIANT tells a verifier which fold to run
  (`hash_walk_state` summarises the running state's Memory group before
  hashing at a `CheckpointFolded` block). A build that predates this plan
  meets an unknown variant and STRANDS (additive-only rule: stop extending,
  tell the human to upgrade) instead of hard-rejecting the chain as forged
  - the reason for a variant over a field.
- **Humans start the new mode:** `maybe_auto_checkpoint`
  (`chain/checkpoint.rs:46`) keeps proposing legacy `Checkpoint` until the
  first `CheckpointFolded` was sealed by a human's `propose_checkpoint
  { folded: true }`; from then on auto cuts fold. Same stranding posture as
  checkpoint-v8 (`charter_features.md` D1), but the moment is a decision,
  not an accident.
- **Unchanged:** genesis, the v8 byte layout and its pins, the legacy
  `Checkpoint` variant, the wiki export's provenance statement (a pruned
  holder already lacks per-patch blocks; the blob's patches were never
  individually signed). Blob transport size is not this plan's problem:
  the folded blob is never larger than today's history blob.
- **Keystones:** `a_folded_cut_hashes_the_base_not_the_history` (byte pin);
  `a_pruned_holder_folds_from_the_base_and_keeps_superseding`
  (blob-seeded projection + a pending patch that collides with a post-cut
  block); `adding_the_variant_keeps_every_existing_approval_byte`
  (`molt-chain-change-v2` pin unchanged); `a_second_wiki_base_is_void`;
  the existing `a_checkpoint_cut_keeps_the_wiki_fold_identical` extended
  to the folded variant.
- **Review gate:** this section is checked against
  `docs_archive/chain/log_compaction.md` §B.6a and
  `docs_archive/chain/persistent_chain.md` §10 with the user before K6
  starts; the doc statuses of both move in the same change that lands it.

### 4.10 The GUI (K7)

- **Infobox:** the wiki viewer hides the header block and renders the
  parsed properties as a key/value table above the body, links clickable,
  unresolved links marked; the editor shows the raw text. `[[Name]]` in
  the body renders as a link (§4.5).
- **Lazy tree:** `Wiki.docs` holds metadata for every page and content
  only for open tabs and drafts; the base arrives via `WikiList` (paged)
  and `WikiGet` on open instead of the snapshot; `set_base` reconciles by
  path through a `HashMap` (the quadratic scans at `wiki.rs:357-410` go);
  `to_draft` serialises edited documents only. `SurfaceSnapshot.wiki_tree`
  is removed and `wiki_docs: u64` added (`#[serde(default)]`).
- **Search and backlinks:** a search field over the nav issues
  `wiki_search`; an open page shows its in-edges from `wiki_links`.
- Validation: the headless GUI tests in `crates/molt-ui/src/tests/gui/`
  (infobox rows by element id, a search result click opens the page), and
  ONE `cargo build -j 1 -p molt-ui-window -p molt-ui` per change-set.

## 5. Work packages (build order, each red-first, each green on master)

- **K0 Fold cache + cheap superseding** (engine, core helper). §4.1-4.2.
  Gate: the two keystones plus the whole existing wiki suite.
- **K1 Paged reads** (`WikiList`, `WikiGet`, tools). §4.3. Gate:
  `crates/molt-mcp/tests/tool_reads.rs` extended (tool == engine-direct),
  limit clamp and cursor round-trip tests.
- **K2 Read-only key** (config, core, engine, mcp, GUI, docs). §4.7. Gate:
  `a_read_token_reads_but_cannot_propose`,
  `an_empty_read_token_admits_nobody`, `tools_list_shows_only_the_scope`,
  scope completeness inside the co-equality test, `patch_settings` refuses
  `mcp_read_token`, one headless panel test modelled on
  `gui/layout.rs:730`, one window build.
- **K3 Wake hook pinned** (engine test, env var, template comment). §4.8.
- **K4 Front matter + link graph** (engine `wiki_index`, molt-ui reuse,
  `wiki_links` / `wiki_neighbors`, propose warnings). §4.4-4.5. Gate: the
  subset's accept/reject table as one test per rule, resolution order,
  incremental == full graph after every fixture patch, dangling
  re-resolution.
- **K5 Search** (`tantivy`, indexer task, `wiki_search`, C-free guard).
  §4.6. Gate: guard green, index-building answer, delete + re-add on edit,
  facet filter, snippet.
- **K6 A cut carries the tree** (core fold, engine verify/checkpoint,
  ChainChange variant). §4.9, AFTER the review gate.
- **K7 GUI** (infobox, lazy tree, search, backlinks, snapshot diet).
  §4.10.

K0-K3 are independent of K4-K7 and small; K5 depends on K4 (properties
feed the facets); K7 depends on K1 and K4.

## 6. Non-goals (deliberate)

- Bundle approval or chunked changesets (decision 1).
- Vector / embedding search, and any external embedding API (the wiki's
  text would leave the republic).
- Typed inline links in prose (`[[works_at::Acme]]`): a later stage on top
  of K4 if named relations in sentences are wanted.
- MCP resources (`resources/list` / `resources/read`): optional later; the
  tools carry the same reads.
- Republic-level roles or rights: the read scope is host-local
  (`docs_archive/security/mcp-security.md`, agents are seats).
- Blob transport for a 100 MiB checkpoint state (pre-existing, unchanged
  by K6, which only shrinks the blob).

## 7. Implementation map

- `crates/molt-core/src/wiki_fold.rs` - `touched_paths`, `wiki_base` in
  `fold_one` (K6).
- `crates/molt-core/src/lib.rs` - `Command::{WikiList, WikiGet, WikiLinks,
  WikiNeighbors, WikiSearch, NetWikiIndexReady}`, the replies,
  `WikiDocMeta`, `SessionSettings.mcp_read_token`, `NODE_POSTURE_KEYS`,
  `NodePosture.mcp_read_token`, `Reply::Proposed.warnings`.
- `crates/molt-core/src/chain.rs` - `ChainChange::CheckpointFolded` (K6).
- `crates/molt-engine/src/lib.rs` - `State.wiki_cache`, `applied_epoch`,
  `wiki_pending`, `wiki_graph`, `wiki_reader`, dispatch arms.
- `crates/molt-engine/src/proposals.rs` - `wiki_tree_cached`,
  `supersede_stale_wiki(moved)`, `applied_iter`, propose warnings.
- `crates/molt-engine/src/wiki_index/{mod,front_matter,graph,search}.rs`
  - NEW (K4, K5).
- `crates/molt-engine/src/chain/{verify,checkpoint,governance,projection}.rs`
  - `summarize_memory`, the folded variant's walk and proposal (K6),
  epoch bumps (K0).
- `crates/molt-engine/src/chat.rs`, `net/ingest.rs` - `MOLT_WAKE_PENDING`
  (K3).
- `crates/molt-engine/tests/c_free_guard.rs` - NEW (K5).
- `crates/molt-mcp/src/lib.rs` - `Scope`, `ToolDef.scope`, the two-key
  `initialize`, scoped `tools/list` and `tools/call`, the new tools, the
  Read-set test, `INTERNAL` + `NetWikiIndexReady`.
- `crates/molt-config/src/lib.rs` - `McpConfig.read_token`,
  `Settings.mcp_read_token`, template / salvage / writer.
- `crates/molt-ui/src/{settings.rs,actions/settings.rs,i18n.rs,wiki.rs,
  wiki_bridge.rs,surfaces.rs,mirror.rs}` and
  `crates/molt-ui-window/ui/{app.slint,theme.slint}` - K2 panel, K7.
- `docs_archive/security/mcp-security.md`,
  `docs_archive/chain/log_compaction.md` (§B.6a, K6),
  `docs_archive/memory/shared_memory_real.md` (status note: the fold cache
  "if a real history ever makes this measurable" is now K0) - updated in
  the change that lands each package; this document moves to
  `docs_archive/memory/` with K7.

## 8. Open points

1. K6 review gate (§4.9) - a conversation, not a code task.
2. `frostem` (tantivy's stemmer) purity and the exact `yaml-rust2` event
   shape (`Event::Scalar` style + raw text) are locked against `cargo tree`
   and the compiler in K5 / K4, not assumed here.
3. Whether `wiki_neighbors` needs a predicate filter (K4 ships without;
   `wiki_links` has one).
