# Knowledge base at scale: the shared wiki for tens of thousands of entries

**Status: EXECUTED - designed 2026-09-03, decisions ratified by the user
the same evening (§1), built 2026-09-04/05. K0-K5 and K7 landed on
2026-09-04; K6 (the folded cut and the base's own file-plane series) on
2026-09-05, after its review gate rejected the original design and the
user chose option (b) - §4.9 is that design as built. The two forks it
left, the tree's key and the fetch's pace, are decided in §8.** Extends
the executed fold design `docs_archive/memory/shared_memory_real.md` and
the export `docs_archive/memory/wiki_export_plan.md`; K6 changes what a
checkpoint carries (`docs_archive/chain/log_compaction.md` §B.6a).

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
- Build (BUILT 2026-09-04, two deviations, both deliberate):
  - **Resolution runs as a whole pass, always.** The plan's incremental
    edge surgery (drop out-edges, re-add, re-resolve dangling, turn a
    deleted document's in-edges dangling) has four ways to be subtly
    wrong. Instead the graph keeps each document's RAW edges as written,
    and an update re-parses only the touched documents and then runs the
    SAME resolution pass a full build runs. "Incremental == full" is then
    true by construction rather than by care - the keystone still pins it,
    over edit, add, delete and a name that becomes ambiguous. The pass is
    O(edges), which is the cheap half; parsing the tree is the expensive
    half and that stays incremental.
  - **The full build runs OFF the actor** (built 2026-09-04, second pass).
    It is kicked off EAGERLY at workspace open, so a human who searches
    usually finds it ready. The per-patch update stays ON the actor, where
    it is a handful of documents. `Command::NetWikiIndexReady { epoch }`
    carries only the epoch - neither index type is serializable, and an
    agent must not be able to hand this node an index - while the built
    artefacts ride a shared slot (the `restore_staging` idiom).
    Reconciliation: the result is INSTALLED only when the epoch still
    matches (a wholesale re-projection has no delta from the tree the task
    read), and the dirty sets are taken at COMPLETION, never at kick-off -
    taking them early and then discarding the result would lose the paths.
    A read that arrives with no index answers `MoltError::IndexBuilding`,
    which is a different claim from an empty page; `wiki_list` and
    `wiki_get` never refuse for index reasons, because they need only the
    fold, and `wiki_get`'s link counts became `Option<u32>` so "not known
    yet" stops being spelled `0`.
  - Name resolution is CASE-EXACT, like the path resolution it extends:
    `[[Acme]]` does not find `people/acme.md`. Deterministic and consistent with
    today's GUI rule; a case-folding rule would need its own ambiguity
    story.
  - `body_links` masks code spans and fenced blocks, so a link in an
    example is not a claim about the graph. It is the ONE parser now:
    `molt-ui`'s `parse_links` calls it.
- Tools (Read): `wiki_links { path, direction: out|in|both, predicate:
  Option<String>, limit, cursor }` → edges; `wiki_neighbors { path, depth:
  1|2, limit, predicate: Option<String>, direction: out|in|both,
  transitive }` → documents with distance (BFS, cap 500). Both carry
  `index_rev` (the fold revision the graph reflects) and `wiki_rev`.
- **Traversal that says WHY (BUILT 2026-09-05).** Every neighbour carries
  the edge that FIRST reached it - `predicate`, `direction` (out|in, as
  seen from the document it was reached from) and `via` (the documents in
  between, empty at distance 1; the route, which is why it is not called
  `path`). BFS, so that edge lies on a shortest route. `transitive` drops
  the depth bound and closes ONE predicate to a fixpoint - the CALLER's
  assumption about that predicate, since §1.2 leaves the vocabulary
  descriptive, so it is refused without one. The 500 cap stays the bound
  and the walk reports `capped` when it hit it (one document past the cap
  is fetched, so a full last page is not reported as a cut). A cycle
  terminates on the `seen` set the walk already keeps. The predicate
  filter matches the EDGE's predicate, never `header`, so an inline typed
  link (§6) walks with the same call.

### 4.6 Full-text search (K5)

- `tantivy 0.26.1` with `default-features = false, features = ["stopwords",
  "lz4-compression", "stemmer"]`: the DEFAULT feature set pulls `zstd` (C,
  via `columnar-zstd-compression`) and would silently break the default
  build's pure-Rust posture. **Corrected 2026-09-04, measured:** the spec
  this section first named (`stopwords` + `lz4-compression` alone) does not
  compile - `stop_word_filter` imports `Language`, which is gated behind
  `stemmer` (E0432; still mis-gated on tantivy `main`). `stemmer` is
  therefore mandatory with `stopwords`, and it is `rust-stemmers 1.2.0`,
  not `frostem` - `frostem` appears in no tantivy release. Both are pure
  Rust, no build.rs, and the resulting graph carries ZERO `-sys` crates
  (100 deps, `cargo tree -i` empty for zstd-sys, libsqlite3-sys, ring, cc,
  cmake, pkg-config). Two further API corrections for the implementation:
  `TopDocs` is no longer a `Collector` (needs `.order_by_score()`), and
  `stopwords` registers no filter by itself - a `TextAnalyzer` has to.
  Guard: `crates/molt-engine/tests/c_free_guard.rs` mirrors
  `crates/molt-net/tests/ring_free_guard.rs` with `-i zstd-sys` (and
  `-i libsqlite3-sys`) on `-p molt-engine -e no-dev`.
- Schema: `path` STRING|STORED (the delete key), `title` TEXT|STORED,
  `body` TEXT, `header` TEXT, `alias` TEXT, `folder` STRING, `facet` Facet
  (`/tag/<t>`, `/type/<t>`, `/prop/<key>/<value>`).
- **Recall over the header (BUILT 2026-09-05).** `header` carries every
  scalar the front matter says, `alias` the names the page declares, both
  rendered by the SAME rule as the inventory (`graph::scalar_strings`:
  link braces stripped, a number as its decimal text). Aliases keep their
  own field rather than joining `title`, because `title` is STORED and IS
  the title a hit displays; a short field ranks a name match high by
  itself. The facets follow the same rendering, one per `(key, value)`
  pair the inventory reports (≤ 64 chars, or it is not faceted), so every
  pair `wiki_props` shows is queryable - the two reserved keys under their
  own roots, everything else under `/prop/`.
- Ownership (BUILT 2026-09-04, same deviation as the graph): the index is
  owned by the actor, built on the FIRST search and updated per applied
  patch (`delete_term` + `add_document` + `commit`, then an explicit
  `reader.reload()` — `ReloadPolicy::Manual` shows nothing without it).
  There is therefore no `index building` state and no `IndexDelta`
  channel: a search always answers over a current index. FOLLOW-UP, same
  as §4.5: at §3's target the first search after an open pays for indexing
  the whole tree on the actor, and that is when the off-actor
  `WikiIndexer` this section sketches earns its complexity.
- Tool (Read): `wiki_search { query, tags: Vec<String>, type:
  Option<String>, folder: Option<String>, props: Vec<(String, String)>,
  limit, cursor }` → hits `{ path, title, score, snippet }`, `next_cursor`
  (an offset), `index_rev`, `wiki_rev`. Query syntax is tantivy's
  (`+must -not "phrase" title:term`) over `title`, `body`, `header` and
  `alias`; `props` (an OBJECT on the MCP surface, pairs in the command)
  becomes facet clauses like `tags`/`type`, all of them `Must`, so an
  unknown key narrows to nothing rather than to everything; snippets from
  `SnippetGenerator` over `body`.
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
- **The Read set, pinned by a test** — NARROWED by the user 2026-09-04 to
  the WIKI and the SHARED FILES: `wiki_list`, `wiki_get`, `wiki_search`,
  `wiki_links`, `wiki_neighbors`, `read_uploads`. `read_state` is out, and
  that also closes the hole the first build carried: a chat read SENDS
  this seat's read receipts, so a read key that could call it made the
  seat-scoping of `mark_read` decoration. Also `Seat` now: `read_chain`,
  `list_proposals`, `status`, `read_members`, `read_session`. Deliberately
  `Seat` from the start:
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

### 4.9 A cut FOLDS the wiki; the tree is fetched (K6)

**Decided by the user 2026-09-04: option (b).** The earlier design - the
folded tree rides INSIDE the checkpoint blob - was reviewed and rejected:
the blob is the trust root a rejoiner is handed once the genesis is gone,
and §3's own target would have made it 100 MiB against a 65408 B
gift-wrap cap. It would have bounded the blob against HISTORY and
unbounded it against CONTENT, which is not what a bound is.

So the cut still FOLDS (history stops accumulating), but the blob carries
a content COMMITMENT to the tree, and the tree itself rides the file
plane that landed 2026-09-04 (`docs_archive/files/mirroring.md`).

#### 4.9.1 What the blob carries

`summarize_memory(group) -> group'` folds the group's `wiki_patch`
entries in order into ONE synthetic first entry and keeps every non-wiki
entry; `consumed_ids` stays untouched (every patch id stays consumed):

```json
(0, {"op": "wiki_base", "hash": "<64 hex>", "size": 12345})
```

- `hash` is the CONSENSUS commitment: sha256 over the canonical tree
  bytes (§4.9.2). Every signer recomputes it from its own fold, so a
  proposer cannot name a tree it did not fold.
- `size` bounds the fetch. Content-derived like `hash`, so it costs
  nothing in consensus terms.
- **No `root`** (corrected while building, 2026-09-05). The earlier draft
  carried the file plane's `Manifest::root()` here "beside" the hash, on
  the argument that a transport constant must never become a consensus
  input - but an entry inside the hashed state IS a consensus input, so
  that draft made `PIECE_PAYLOAD_LEN` one: two builds with different piece
  sizes could not sign the same cut. The root now rides the holder gossip
  instead. It still gates every fetched piece during the transfer; the
  chain-anchored check is `hash` over the assembled bytes, which is what
  makes a wrong root harmless rather than unverifiable.
- **No `rev`.** `wiki_fold.rs` states that the revision is display-only
  and never consensus input; the earlier draft would have broken that.

#### 4.9.2 The canonical tree bytes

Its own versioned, length-prefixed layout, NOT `serde_json` of the map:

```
"molt-wiki-base-v1\0" ‖ le64 entry-count ‖ ( put_bytes(path) put_bytes(content) )*
```

over the `BTreeMap` in path order, `put_bytes` = `le32 len ‖ bytes` (the
`molt-republic-id-v2` rule: length-prefixed, never separators). A byte-pin
test goes with it, like every other layout in this repo.

#### 4.9.3 Versioning: a v9 tag AND a new variant, both

- **`molt-chain-checkpoint-v9`**, content-selected on the presence of a
  `wiki_base` entry in the Memory group - the same conditional discipline
  v7 and v8 use, so every existing cut keeps its bytes and its recorded
  signatures. Memory is inside `CHECKPOINT_V7_SURFACES`, so without this
  the same republic would emit v6/v7 bytes for a materially different
  state: exactly what §B.6a's versioning invariant forbids.
- **`ChainChange::CheckpointFolded { upto, state_hash }`**, variant byte 4
  in `approval_bytes` (byte 3 is the legacy `Checkpoint`). The variant is
  what tells the verifier which fold to run, and it is what gives an older
  build a STOP rather than a forgery verdict.
- `CheckpointProposed` carries the variant too, or
  `receive_checkpoint_proposal` cannot know which hash to recompute.
- **Once folded, always folded**: a legacy `Checkpoint` block after a
  `CheckpointFolded` anchor is refused. Otherwise the state a node hashes
  would depend on whether it happened to prune.

#### 4.9.4 The blocker that must be fixed in the same change

`walk_suffix_chain` (`chain/verify.rs:~1005`) destructures the anchor as
`ChainChange::Checkpoint` and errors otherwise. Every node prunes at the
cut, so after the first folded cut `blocks[0]` IS the folded anchor and
the next open fails with "this workspace's chain is unreadable" - on the
UPGRADED nodes. Both `walk_suffix_chain` and `verify_suffix_chain` accept
either variant.

#### 4.9.5 The summary runs at THREE hash sites or the republic cannot cut

`hash_walk_state` (the running walk), `own_checkpoint_state` (the propose
hash AND the verify-before-sign hash) and the blob actually persisted at
`governance.rs:~597` must reach identical bytes. `own_checkpoint_state`
takes the variant. A keystone drives all three over one chain and asserts
one hash.

#### 4.9.6 Base-pending: adoption and readability decouple

This is the structural change, and the dangerous one. Today
`try_adopt_from_blob` -> `apply_chain_to_state` -> `wiki_base()` always
produces a tree. With a commitment, a node can hold a VERIFIED chain and
not yet hold the tree. It must then be honest, not empty:

- `State` carries a base-pending state (the commitment, plus fetch
  progress). `wiki_base()` returns it instead of a tree.
- **`supersede_stale_wiki` is a NO-OP while base-pending.** Run against an
  empty base every pending `wiki_patch` fails to apply and would be
  retired as superseded - silent data loss on a rejoiner. This is the
  single most important line in this section.
- Every wiki read (`WikiList`, `WikiGet`, `WikiSearch`, `WikiLinks`,
  `WikiNeighbors`, `wiki_export`, the Memory snapshot) answers a typed
  refusal naming the state and the progress, never an empty page.
- The GUI shows the Memory surface with a progress line, the way the
  mirror row already does.

#### 4.9.7 Transport

The file plane's primitives are share-agnostic and take exactly what a
tree can supply (`publish_series_v2`, `fetch_series_v2_with`,
`enqueue_publish`, `spawn_trickle`, `SeriesExpect`, `impl PieceSink for
Vec<u8>`). The engine WRAPPERS are not: they are keyed on
`share_identity(&MessageId)` and land files in the human's download
directory. So the tree gets its own job family beside the share family -
a `kind` on the publish/fetch jobs so `resume_file_jobs` routes it, a
second answer path for `PieceWanted` keyed by the tree hash, and a sink
that writes beside `chain.state` rather than into `download_dir`.

- **Holders.** A node that FOLDED the base itself is a primary holder and
  answers from its own store; a node that FETCHED it answers once
  complete. This is the sharer/complete-mirror split the mirror election
  already makes.
- **The key.** OPEN QUESTION for the user, see §8.4.
- **Pace.** 100 MiB is ~2 387 pieces; at the shipped 15 s trickle
  interval that is ~10 hours per fetching node, on a plane deliberately
  paced behind chat and governance. See §8.5.

#### 4.9.8 The wedge this also closes

`CheckpointServed` is an ordinary logged `WorkspaceEvent` published over
the 445 outbox. An over-budget frame is a `PublishStall::Permanent` and
the node writes nothing more until it can go out - ACROSS RESTARTS. So a
pruned holder whose blob crosses the frame budget bricks its own outbox
the first time a peer asks for catch-up below the anchor. Option (b)
removes the cause; this change also adds the guard, because nothing today
measures a `WorkspaceEvent` against the transport budget (`payload_fits`
covers proposals only).

#### 4.9.9 Failure modes, and what the human sees

| Case | Answer |
|---|---|
| Chain adopted, tree not fetched yet | base-pending, progress shown, no wiki reads answered empty |
| Fetch never completes | never fails, never times out: a quiet persistent state, plus the one line that unblocks it - another member holding the tree must be online |
| No dialable relay | a named republic-level condition, not silence |
| Two treeless nodes fetching from each other | "no member online holds the shared memory base" - the holder gossip already carries what is needed |
| Local tree fails its hash | delete the store, re-enter base-pending, notice. Deliberately NOT a refused workspace open: the chain is the trust root and a damaged one is evidence, the tree is a re-fetchable cache of threshold-signed content |

#### 4.9.10 Unchanged

Genesis; the legacy `Checkpoint` variant and every existing cut's bytes;
`consumed_ids`; the wiki export's provenance (`bundle_from_chain` already
returns `None` for a pruned holder - the base tree is not a substitute for
per-patch provenance).

### 4.10 The GUI (K7)

- **Infobox** — BUILT 2026-09-04. The viewer hides the header block and
  renders the parsed properties as a key/value table above the body, a
  link-valued property clickable exactly like a body link; the editor
  keeps the raw text. The header is dropped from BOTH sides of the
  preview diff, or every document carrying one would read as fully
  changed. `tags` is split off the table since 2026-09-05 and renders as
  coloured pills, the hue derived from the LOWERCASED tag, and a
  `[[path|Name]]` value shows the name half rather than the path.
- **Authoring the header** — BUILT 2026-09-05
  (`wiki_tags_and_semantic_links.md`). A document without a header offers
  `+ Tag` and nothing else: a modal takes several free-text tags and
  writes `tags: [...]`, leaving the pane in the viewer. Typed relations
  are authored by "Create semantic link" (toolbar, editor context menu,
  navigator file menu - the navigator opens the file first, because the
  relation is written into ITS header): name, target out of the folded
  base, and any number of relations switched on, each becoming one header
  key with a quoted `"[[path|Name]]"` value. Existing keys grow into a
  list; the header is edited LINE-WISE, never re-emitted.
- **`[[Name]]` in the body** — BUILT. Two traps found in the building:
  pulldown-cmark splits an unmatched `[[` across several text events, so
  the scan has to run over a block's FINISHED runs (a per-event scan
  never sees the pair); and `open_link` compared `d.name()`, which keeps
  the `.md`, so a bare name resolved to nothing. Inline code is excluded,
  the same rule the index follows.
- **Search and backlinks** — BUILT. A field over the navigator issues
  `wiki_search` and REPLACES the tree with its ranked hits (an empty
  field brings the tree back); the open document shows its in-edges from
  `wiki_links { direction: "in" }`, header relations included. Both are
  reads that fill only their own face, and a reply that outlived its tab
  is dropped rather than shown against the wrong document. The in-edge
  request rides the DOCUMENT change, not every model mutation.
- **Lazy tree — BUILT 2026-09-05.** `SurfaceSnapshot.wiki_tree` is gone
  and `wiki_docs: u64` took its place, so a Memory read no longer ships
  every page's content: it says how many there are, and the tree is read
  through `wiki_list` (metadata, paged) and `wiki_get` (one document).
  The mirror tick now carries a SIGNAL rather than a payload - the tree
  used to be copied about seven times per engine event, twice of them on
  the UI thread.

  The load-bearing distinction is inside `Doc.base`: `Option<BaseDoc>` is
  "has a ratified counterpart", and `BaseDoc.raw: Option<String>` is "I
  hold its bytes". Conflating them would paint every unfetched page as a
  local addition and propose the whole wiki back to the republic, so:
  an unfetched document is `Unchanged` by construction (the member cannot
  have edited what was never shown), it colours nothing in the preview or
  the infobox, and `build_patch` REFUSES while any changed document is
  unfetched rather than emitting a patch over bytes this node does not
  hold. `wants_content()` asks for the open document first and then for
  anything changed without being opened - a delete from the navigator,
  which needs the ratified text before it can become a deletion hunk.
  Content is cached per REVISION: a base that moved may have moved this
  document too, so held bytes are dropped and re-fetched.

- Validation: the headless GUI tests in `crates/molt-ui/src/tests/gui/`
  and ONE `cargo build -j 1 -p molt-ui-window -p molt-ui` per change-set.

## 5. Work packages (build order, each red-first, each green on master)

- **K0 Fold cache + cheap superseding** — BUILT 2026-09-04. §4.1-4.2.
  Two deviations from the design: the cache carries `(tree, rev)` together
  (the snapshot path needs the revision, so a tree-only cache could drift
  from `wiki_tree`), and it tracks the two applied logs SEPARATELY, since
  the fold order is legacy-then-chain and an append to the first half is
  not at the end of that order.
- **K1 Paged reads** (`WikiList`, `WikiGet`, tools) — BUILT 2026-09-04.
  §4.3. `kind` and `props` stay `None`/null until K4 parses the header.
- **K2 Read-only key** (config, core, engine, mcp, GUI, docs). §4.7. Gate:
  `a_read_token_reads_but_cannot_propose`,
  `an_empty_read_token_admits_nobody`, `tools_list_shows_only_the_scope`,
  scope completeness inside the co-equality test, `patch_settings` refuses
  `mcp_read_token`, one headless panel test modelled on
  `gui/layout.rs:730`, one window build.
- **K3 Wake hook pinned** — BUILT 2026-09-04. §4.8.
- **K4 Front matter + link graph** — BUILT 2026-09-04. §4.4-4.5. The YAML
  subset is driven through `yaml-rust2`'s EVENT parser as designed (the
  `no` → false class cannot happen); `Parser` is not an `Iterator` in
  0.12, so the loop pulls `next_token()`.
- **K5 Search** — BUILT 2026-09-04. §4.6. `crates/molt-engine/tests/c_free_guard.rs`
  keeps the graph C-free; the index-building answer does not exist (see
  §4.6). An empty query with no filter finds NOTHING, never everything.
- **K6 A cut FOLDS the wiki** (§4.9), in four stages - the order is a
  safety property, not a preference: a folded cut drops the patches, so
  nothing may PROPOSE one before a holder keeps the tree locally and can
  fetch a missing one. `wiki_base::FOLD_CUTS` is that switch.
  - **K6a Layouts + verify** - BUILT 2026-09-05. `molt-wiki-base-v1` and
    its reader (`wiki_fold.rs`), `ChainChange::CheckpointFolded` (variant
    tag 4), the `molt-chain-checkpoint-v9` tag, `chain/wiki_base.rs` (the
    summary), the walk (`walk_suffix_chain` anchors on either variant -
    §4.9.4), `own_checkpoint_state(upto, folded)`, the wire's `folded`
    flag, and the keystone that pins all four hash readings against each
    other. Accepting a folded cut works from here; proposing one does not.
  - **K6b The base store** - BUILT 2026-09-05. `wiki_base.bin` beside
    `chain.state`, written BEFORE the prune and checked against the
    commitment on open; base-pending (§4.9.6) where it is missing:
    `wiki_base()` returns `MoltError::WikiBasePending`,
    `supersede_stale_wiki` is a NO-OP (pinned by a keystone that goes
    Rejected without it), every wiki read refuses by name, and the Memory
    snapshot carries the progress instead of an empty tree.
  - **K6c The transport** - BUILT 2026-09-05 (§4.9.7). The base is a
    content-addressed series on the file plane: id and key both derived
    from the commitment, so every holder publishes into ONE series and a
    fetcher takes pieces from whoever is online. No holder gossip and no
    election - a holder answers what it holds. `wiki_base.bin` is framed
    in PIECE-sized frames, so the sender serves piece k with one seek and
    one decrypt and the wiki is never written out in plaintext; the fetch
    lands in memory for the same reason. `SeriesExpect.root` became
    optional: the chain commits to CONTENT, so the assembled bytes are
    checked against the threshold-signed hash rather than a root learned
    from gossip.
  - **K6d Flip `FOLD_CUTS`** - BUILT 2026-09-05, with the
    `CheckpointServed` size guard (§4.9.8): a blob that does not fit one
    transport frame is not served, because an over-budget WorkspaceEvent
    is a permanent publish stall.
  - Keystones: `chain/checkpoint_tests.rs` (the four hash readings, the
    legacy-after-folded refusal, the missing base, the supersede no-op)
    and `tests/wiki_base_plane.rs` (a real 2-of-2 republic over a relay:
    ratify, cut, lose one seat's base, fetch it back).
- **K7 GUI** — BUILT 2026-09-04 except the lazy tree / snapshot diet,
  which is deferred with its reason in §4.10.

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

1. K6's review gate RAN (2026-09-04), the user chose option (b), and §4.9
   was BUILT on 2026-09-05. The two forks it left are decided:

   **8.4 The tree's encryption key - DECIDED while building (2026-09-05):
   `HKDF(rotation seed, "molt-wiki-base-v1" ‖ commitment)`.** Not because
   it is simpler: because the series is CONTENT-ADDRESSED, and that forces
   it. Every holder must seal the same pieces or a fetcher could not take
   them from whoever is online, and an MLS-exporter key is epoch-bound -
   holders would become unusable to each other after any re-key, and each
   rejoin would cost a full re-publish. The rotation seed is also the one
   shared secret a rejoiner holds before it holds any chain. The cost is
   stated in the code: the base has the rotation seed's lifetime, so no
   forward secrecy against a relay that recorded the ciphertext and later
   obtains the seed - a member-compromise scenario in which the wiki is
   already in the attacker's hands. Reversible: any holder can re-publish
   under a new derivation, and only in-flight fetches would notice.

   **8.5 The pace - DECIDED while building: the base asks on its own
   clock.** A share's requester is missing one file; a base-pending node
   has no wiki at all. So the base fetch asks after 5 s and repeats every
   60 s (a share waits 10 minutes), and the beat retries a finished fetch
   every 15 s. The SENDER keeps the file plane's ordinary pace and daily
   budget - the ask is one small control frame, and it is the ask, not the
   publish rate, that was the wrong knob to leave at ten minutes.
2. `frostem` (tantivy's stemmer) purity and the exact `yaml-rust2` event
   shape (`Event::Scalar` style + raw text) are locked against `cargo tree`
   and the compiler in K5 / K4, not assumed here.
3. Whether `wiki_neighbors` needs a predicate filter (K4 ships without;
   `wiki_links` has one).
