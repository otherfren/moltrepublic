# What the wiki still lacks to be a semantic knowledge base an agent can use

Status: ANALYSIS + three decisions (user, 2026-09-05); the rest still
open. Every "today" claim below was read out of the tree at that date
(file:line given); the comparisons to other systems carry their sources.

**Decided 2026-09-05** (§7 is the build order that follows; §5 is empty -
everything is either decided against or scheduled):
0. **Relations live in the PROSE** as typed inline links, with the
   predicate shown as a tooltip in the preview; the header keeps identity,
   scalars and qualified relations. Both forms stay valid - edges are a
   set. See §6.
1. **Build the structured write path (§1.1).** A REGULAR (seat) agent may
   write through it; a read-only agent keeps reading only - which is
   exactly the existing `Scope::Seat` / `Scope::Read` split, so the tool
   is a Seat tool and needs no new privilege concept.
2. **No reserved vocabulary (§1.2).** Keep it generic: the ontology stays
   purely descriptive, derived from usage. §1.2 is rewritten below to say
   what that costs and what is left.
3. **The wiki stays page-shaped (§2.3).** No section reads, no
   `path#heading` citation unit. §2.3 is struck; its cost is noted there.

The question this answers: an AI client with a read-only MCP key opens a
republic's wiki. What can it NOT do that a semantic knowledge base is
supposed to let it do - understand what a page IS, how it relates to the
others, and find the right handful of pages without reading the tree?

## 0. What already works (so the gaps are honest)

- **Triples exist.** A header key whose value is a link is a typed edge,
  the key being the predicate (`wiki_index/graph.rs::collect_typed`) -
  the Semantic-MediaWiki idea expressed in front matter. Qualified
  relations (a flat mapping under the key) carry the edge too.
- **The graph is real and current.** Full and incremental builds run the
  SAME resolution pass; `wiki_links` answers in- and out-edges filtered
  by predicate, which already answers "who works_at Acme" as an in-edge
  query. `wiki_neighbors` walks 1-2 hops.
- **Full text is real.** tantivy over title and body, facets for
  `tags` / `type` / `prop/<k>/<v>`, snippets, paging, an honest
  `index building` state that is not an empty answer.
- **The ontology is observable as USAGE.** `wiki_props` returns every
  header key with its values and counts - "what is usual here" without a
  schema. That is a genuinely good answer to metadata drift, which is the
  failure that empties Dataview queries in practice (a `Status` and a
  `status` key are two fields, and nothing tells the author).
- **Reads are paged and revision-stamped** (`wiki_rev` / `index_rev`), so
  an agent can tell a stale answer from a current one.

That is a solid base. The gaps below are what sits on top of it.

## 1. Tier 1 - what actually blocks an agent

### 1.1 The only way an agent can WRITE is a hand-written positional diff

`propose { surface: memory, payload: { op: wiki_patch, value: <git patch> } }`
is the entire write surface (`molt-mcp/src/lib.rs:981`). The applier is
position-exact: right context at the wrong line number is VOID, and a
hunk header whose counts disagree with its lines is VOID
(`molt-core/src/wiki_fold.rs:300`, test
`strictness_position_and_context_must_match_exactly`). There is no offset
search, deliberately.

And the feedback loop is open: `propose` does not refuse a patch that
does not apply - `wiki_header_warnings` returns nothing for one
(`proposals.rs:749`), so a wrong diff becomes a proposal members can vote
on and that then dies as VOID or superseded.

Why this is first: line-number diffs are the format LLMs are worst at.
The agent-scaffold survey finds exact-string replacement (`old_str` /
`new_str`) in 5 of 13 coding agents and states plainly that line-number
indexing "is inherently fragile for LLM generation since probabilistic
models struggle with precise numerical offsets" [KNOWN, source below].
Our format is the fragile one, our applier is stricter than git's, and
our error arrives after a vote.

Fix direction, cheap and layer-correct: `molt-ui/src/wiki.rs:2352-2460`
already builds a git patch from a working tree with `similar` (a pure-Rust
workspace dependency). Move that emitter down to `molt-core` next to
`wiki_fold::apply_patch` - its exact inverse belongs beside it - and add a
Seat tool `wiki_edit` that takes STRUCTURED edits against the current
base (`set_props`, `replace {old,new}`, `content`, `rename`, `delete`),
builds the patch engine-side and proposes it. Governance is unchanged:
members still see and vote on the same diff. The agent simply stops
writing hunk headers. A refusal then happens where it belongs - at the
call, with a reason.

### 1.2 The ontology stays DESCRIPTIVE - decided, with its cost stated

The gap is real: an agent sees that `betreut: "[[a person page]]"` occurs
40 times and cannot learn what `betreut` means. English-looking keys
(`works_at`) it guesses; a republic's own vocabulary it cannot.

**Decision (user, 2026-09-05): no reserved keys, keep it generic.** The
program declares no vocabulary of its own - not `inverse_of`, not
`subclass_of`, not `transitive`. What a predicate means stays prose that
a human wrote and a member ratified, and the machine-readable half stays
what it is today: USAGE.

What that costs, plainly, so nobody rediscovers it as a bug:
- No inverse-aware reads. `wiki_links` on Acme reports in-edges under
  `works_at`; nothing turns that into `employs`.
- No transitive closure and no subclass widening (§2.2 keeps only the
  bounded predicate-filtered walk).
- An agent asked "who reports to whom" must be TOLD the predicate, or
  infer it from the key's spelling and its usage.

What is left, and worth strengthening instead:
- `wiki_props` is the ontology-as-it-is (keys, values, counts). It should
  cover the inline predicates too once those exist (see §6), so one call
  answers "what relations does this republic actually use".
- A page may of course DESCRIBE `betreut` in prose; an agent reaches it
  the same way a human does, by reading the page. Nothing stops a
  republic from keeping an `_ontology/` folder - the tool simply gives it
  no special status.

### 1.3 The front matter is invisible to full-text search

`search.rs::write` indexes `path`, `title` (the first heading), `body`
(= `split_front_matter(content).1`) and facets. The header's own text is
NOT in the searchable body, and `aliases` are indexed nowhere.

So: a person page whose header says `works_at: "[[Acme]]"` and
`aliases: [P. Müller, Müller]` is not found by searching `Acme`, and not
found by searching `Müller`. The graph knows both facts; the search does
not. An agent that starts with a name - the normal entry point - misses
the page.

Fix: index the header's string values into a searchable field (and
aliases into the title field, or their own). Small, local, no new
dependency. This is the cheapest large win in the list.

## 2. Tier 2 - what makes the difference between "possible" and "efficient"

### 2.1 No property-value query, although the facets are already indexed

`facets_of` writes `/prop/<key>/<value>` for every scalar string property
(`search.rs:240`), but `wiki_search` exposes only `tags`, `type` and
`folder` (`Command::WikiSearch`, `molt-core/src/lib.rs:4031`). "Every
document with `status: draft`" has no query path at all; the agent must
page the whole tree and read each header.

Fix: one parameter, `props: Vec<(key, value)>`, mapped onto the facet
clauses the search already builds. The index needs no change.

### 2.2 Traversal loses the relation and stops at two hops

`wiki_neighbors` returns `(path, distance)` and nothing else
(`WikiNeighbor`, `molt-core/src/lib.rs:6196`): no predicate, no
direction, no path, no predicate filter, depth capped at 2. An agent
asking "how is Anna connected to Acme" gets a distance and has to
reconstruct the why with N further `wiki_links` calls; "what is Anna part
of, transitively" is not expressible at all.

Fix: carry the edge that reached each node (predicate + direction + the
path taken), and accept `predicate` + `transitive` so a declared
transitive relation (§1.2) can be closed in one call. Note that SMW
deliberately does NOT close transitives in queries [KNOWN] - it makes the
author materialise them. For an agent the opposite is right: it cannot
maintain the closure, and a bounded closure over one declared-transitive
predicate is cheap.

### 2.3 STRUCK - the wiki stays page-shaped (decided)

Proposed: section reads (`wiki_get { path, section }`) and a
`path#heading` citation unit. **Decided against (user, 2026-09-05): the
page stays the unit.**

The cost, stated once: at 10^4-10^5 pages a `wiki_get` on a long page is
the agent's token bottleneck, and a quote has no anchor finer than the
path. The mitigation that stays open is keeping pages small - which is a
writing convention, not a feature - and the snippet the search already
returns.

### 2.4 No "what changed since"

An agent that maintains the KB re-reads everything or nothing. Every read
carries `wiki_rev`, so the information exists; there is no query for the
delta.

Fix: `wiki_changes { since_rev, limit, cursor }` → touched paths with
their change kind. The fold already computes `touched_paths` per patch
(`molt-core/src/wiki_fold.rs`).

## 3. Tier 3 - hygiene the agent can act on

- **Dangling edges are computed and never exposed.** `WikiGraph.dangling`
  (`graph.rs:69`) holds exactly "what this republic references but does
  not have" - the single best "what should I write next" signal for an
  agent - and no tool returns it. Same for orphans (no in-edges) and hubs.
- **Name resolution is case-exact and silent.** `[[Acme]]` does not find
  `people/acme.md`; the edge simply dangles (documented deviation,
  §4.5). An agent has no way to check a name before writing it. A
  `wiki_resolve { name }` → candidates + ambiguity would close it.
- **Key drift is visible but unflagged.** `wiki_props` shows `status` and
  `Status` as two keys; nothing says they are probably one. A near-duplicate
  hint (case-folded / separator-folded collision) costs little and
  prevents the failure that silently empties property queries in
  Dataview practice [KNOWN].

## 4. What I do NOT recommend, and why

- **A query language (SMW `#ask`, Dataview DQL).** Tempting and wrong
  here: it is a parser, a planner and a surface to secure, for an agent
  that is perfectly happy composing three typed calls. Add the missing
  PARAMETERS (§2.1, §2.2) first; revisit only if real transcripts show
  agents fighting the call count.
- **An OWL-style reasoner.** Nothing above needs entailment beyond
  inverse, transitive and subclass - three closures, each a dozen lines
  over the existing graph. A description-logic layer would be the
  hand-rolled machinery this repo's own rule warns about.
- **Vector / embedding search.** Already a ratified non-goal, and the
  reason given (the text would leave the republic) holds for any external
  API. A LOCAL model is a different question, but it is a heavy
  dependency against the pure-Rust posture, and the cheaper win is
  §1.3 + aliases + declared synonyms - lexical recall the current index
  is simply not being given.
- **Republic-level schema enforcement.** Rejecting a patch whose header
  violates a declared range would make the ontology CODE and give one
  member's page authority over another's patch. The current shape -
  warnings on the card, members decide - is right. A `wiki_lint` read
  tool that REPORTS violations is the agent-facing half, and it governs
  nothing.

## 5. Nothing open

Every finding above is either decided against (§2.3, and the four
non-goals in §4) or scheduled in §7. §6 carries the decision that shapes
the rest.

## 6. DECIDED (user, 2026-09-05): typed links live in the PROSE

Relations belong in the sentence that asserts them, not in a metadata
block beside it. A link may carry its relation keyword -
`[[works_at::Acme]]`, `[[works_at::Acme|Acme GmbH]]` - and the preview
shows that keyword as a tooltip over the link. This is Semantic
MediaWiki's inline form, named as deferred in `knowledge_base_scale.md`
§6 and now taken up.

**Why this way round**, in one line: the binding constraint on a
knowledge base is not query power but how many true statements get
written at all, and metadata that is a separate chore is kept worse than
a statement made where the author is already thinking. The header form
also duplicates what the sentence says, and two copies drift.

**The cost, stated once so nobody rediscovers it as a bug:** an inline
annotation attaches to the PAGE, never to the sentence's grammatical
subject. `[[works_at::Beta]]` inside a sentence about someone's brother
writes a wrong edge, and no notation fixes that without subobjects, which
we do not have. The preview tooltip is therefore not decoration but the
safety mechanism: it renders the claim ("this page - works_at -> Beta")
where a reader will see it.

What the decision carries with it:

1. **Both forms stay valid; no migration, no precedence rule.** A header
   `works_at: "[[Acme]]"` and a sentence `[[works_at::Acme]]` produce the
   same edge, and edges are a SET - two sources can only ADD, never
   contradict the way two values of one field would. What changes is
   which form the UI and the write tools lead an author to. Live
   republics keep verifying, unchanged.
2. **The header keeps identity and qualifiers.** `type`, `tags`,
   `aliases`, plain scalars (`born: 1975`) and the qualified form
   `works_at: { to, since, role }` have no sensible inline shape and stay
   where they are. The split reads cleanly: the header says what a page
   IS, the prose says what it RELATES to.
3. **Parsing is one place.** `wiki_index/graph.rs::body_links` already
   parses `[[Name]]` and `[[Name|display]]` over finished runs with code
   spans masked. `[[pred::Name]]` and `[[pred::Name|display]]` extend the
   same function; `RawEdge.predicate` already exists and is already what
   `wiki_links { predicate }` filters on, so the graph, the tools and the
   MCP surface need NO new shape.
4. **The tooltip needs the predicate on the span.** `WikiSpan` (theme.slint)
   carries `text` + `link`; it gains `rel`, and the preview's `LinkRun`
   shows it through the existing `HintTip` overlay (the generation-owner
   idiom, `slint-hinttip-generation-and-change-handlers`).
5. **It is not standard markdown.** `[[pred::Name]]` renders as literal
   text in a plain renderer, exactly as today's `[[Name]]` already does -
   the export (`wiki_export`) keeps working, it just reads as text
   elsewhere. No new class of problem.
6. **`wiki_props` must inventory the inline predicates too.** More
   predicates will exist and all of them stay uninterpreted strings
   (§1.2). If the one call that answers "what relations does this
   republic use" saw only header keys, it would miss exactly the form the
   republic writes.
7. **The semantics moves into the QUERY, not the schema.** §1.2 leaves
   predicates uninterpreted, which costs transitive closure and inverse
   unification. Both come back without any declared vocabulary by letting
   the CALLER carry the assumption for one query:
   `wiki_neighbors { predicate: "part_of", transitive: true }`. The
   inverse needs nothing at all - `wiki_links { direction: "in",
   predicate }` IS the inverse view, the agent only has to know the name,
   and it learns names from `wiki_props`. Subclass widening is the one
   that cannot be rescued this way; it is dropped, and it is the least
   valuable of the three.

## 7. Build order

Six steps. Each is red-first, each lands green on master, each carries
its own documentation - the rules below hold for all of them and are not
repeated per step:

- **Every new `Command` is an MCP tool or on the documented INTERNAL
  list.** `co_equality_every_command_is_a_tool_or_documented_internal`
  fails otherwise. All the reads here are `Scope::Read`; the one write is
  `Scope::Seat` (the decision above).
- **The shipping spec is `docs_archive/memory/knowledge_base_scale.md`.**
  Its §4.4 / §4.5 / §4.6 describe the header subset, the graph and the
  search as BUILT; whichever step changes one corrects it in the same
  change. Its §6 lists typed inline links as deferred - step 3 removes
  that line. A spec that still claims the old behaviour costs a planning
  session.
- **Tool descriptions are the agent's only manual.** Every new parameter
  gets one clause in the tool's `description`
  (`crates/molt-mcp/src/lib.rs`), in the register the existing ones use:
  what it does, not how it feels.
- **This document moves to `docs_archive/memory/` in the change that
  finishes step 6.**

### Step 1 - Recall. ~2 days. No dependencies.

Today a page is not findable under the name it declares, and a scalar
property has no query path at all. Largest gap per hour in the list.

- `crates/molt-engine/src/wiki_index/search.rs`: the schema gains a
  searchable field for the header's own text (its string values, joined)
  and one for `aliases`; `write()` fills them from the properties it
  ALREADY parses for the facets. Decide once whether aliases join `title`
  (so a name match ranks like a title match) or get their own field, and
  write the reason in the code, not here.
- Same file: `Filters` gains `props: Vec<(String, String)>`, mapped onto
  the `/prop/<key>/<value>` facet clauses `facets_of` already writes. The
  index needs no change for this half.
- `crates/molt-core/src/lib.rs`: `Command::WikiSearch` gains `props`.
- `crates/molt-mcp/src/lib.rs`: the `wiki_search` schema and description.
- Keystones: a document whose header says `aliases: [Müller]` is found by
  `Müller`; one whose header says `works_at: "[[Acme]]"` is found by
  `Acme`; `props: [("status","draft")]` returns exactly the drafts, and
  an unknown key returns nothing rather than everything.

### Step 2 - Traversal that says WHY. ~1 day. No dependencies.

`WikiNeighbor` is `(path, distance)`: an agent learns THAT two documents
are related and never HOW. And with no declared vocabulary, the
transitive closure has to be a query option or it does not exist.

- `crates/molt-engine/src/wiki_index/graph.rs::neighbors`: carry the edge
  that first reached each node - predicate, direction, and the path it
  came through - and accept a predicate filter plus `transitive`, which
  drops the depth cap and walks that ONE predicate to fixpoint. The 500
  cap stays the bound; a cycle terminates on the `seen` set the walk
  already maintains.
- `crates/molt-core/src/lib.rs`: `WikiNeighbor` gains the three fields;
  `Command::WikiNeighbors` gains `predicate` and `transitive`.
- `crates/molt-mcp/src/lib.rs`: schema + description. Say plainly that
  `transitive` is the CALLER's assumption about that predicate, because
  the republic declares none.
- Keystones: a `part_of` chain of four closes under `transitive` and
  stops at depth without it; a predicate filter excludes the other edges;
  a cycle terminates; the cap holds and the reply says it was reached.

### Step 3 - Inline typed predicates. ~2 days. Steps 4 and 5 depend on it.

- `crates/molt-engine/src/wiki_index/graph.rs::body_links`: learn
  `[[pred::Name]]` and `[[pred::Name|display]]`. ONE parser - `molt-ui`'s
  `parse_links` already calls this function, so the GUI follows for free.
  Keep the code-span and fenced-block masking: a predicate in an example
  is not a claim about the graph. `pred` must satisfy the subset's key
  rule (`molt_engine::header_key_ok`); if it does not, the whole thing
  stays an ordinary `[[Name]]` link - never a third syntax.
- The callers already carry `RawEdge.predicate`, so `WikiGraph`,
  `wiki_links { predicate }` and the MCP surface need NO new shape.
  Verify that rather than assume it.
- `crates/molt-engine/src/proposals.rs::cmd_wiki_props`: the inventory
  must count inline predicates too. It reads `graph.inventory` today,
  which is built from header properties only - decide whether to extend
  the inventory or to merge the graph's predicates into the reply, and
  say why in the code.
- The tooltip: `WikiSpan` (`crates/molt-ui-window/ui/theme.slint`) gains
  `rel`; `molt-ui/src/wiki.rs`'s block parser fills it; the preview's
  `LinkRun` / `SpanFlow` in `surfaces.slint` shows it through the
  existing `HintTip` overlay (the generation-owner idiom - a shared
  overlay needs an owner mark, not an anchor comparison).
- `docs_archive/memory/knowledge_base_scale.md`: §4.5 gains the inline
  form, §6 loses the deferral line.
- Keystones: the sentence `[[works_at::Acme]]` and the header
  `works_at: "[[Acme]]"` produce the SAME edge; a predicate inside a code
  span produces none; a predicate failing the key rule degrades to a
  plain link; `wiki_props` lists an inline-only predicate; the tooltip
  renders headless over the link run and carries the predicate.

### Step 4 - The authoring surface follows the decision. ~1 day. Depends on 3.

The "Create semantic link" modal shipped 2026-09-05 writes a HEADER key
(`molt-ui/src/wiki.rs::add_relations`). Under the decision above that is
no longer the form a member should be led to, and a UI that contradicts
the decision is worse than no UI.

- The modal writes an inline annotation into the open document's prose.
  Decide - and write down - WHERE: the honest default is the cursor
  position in the editor, and a new line at the end of the body in the
  viewer, which has no cursor.
- The header path stays reachable for the QUALIFIED form
  (`{to, since, role}`), which has no inline shape - that is the modal's
  remaining reason to write a header at all.
- `add_relations`, `with_relation` and the canonical-emitter fallback
  stay as they are: they are the qualified path, and their
  parser-verified write (`grew_by`) is what keeps a header readable.
- Keystones: the modal's write lands in the body and produces the edge;
  the qualified form still lands in the header; one Undo takes back
  either.

### Step 5 - The structured write path + `wiki_resolve`. ~1 week. Depends on 3.

The one item that decides whether an agent can CONTRIBUTE rather than
only read. Today its only write is a hand-written positional diff that
`propose` does not even refuse when it cannot apply.

- Move the unified-diff emitter from `crates/molt-ui/src/wiki.rs`
  (~2352-2460, `similar`, a pure-Rust workspace dependency) down to
  `crates/molt-core` beside `wiki_fold::apply_patch` - its exact inverse
  belongs next to it - and have `molt-ui` call it. A pure move plus its
  own tests; land it as its own commit so a regression here is bisectable.
- New Seat-scoped `wiki_edit`, one or more edits against the CURRENT
  base: `content` (whole document), `replace { old, new }` (exact string,
  the format agent scaffolds converged on), `set_props`, `add_relation`
  (a sentence, after step 3), `rename`, `delete`. The engine builds the
  patch and proposes it, so members still see and vote on the same diff.
- It REFUSES at the call, with a reason: an `old` that is absent or
  ambiguous, a path that does not exist, a header the parser would not
  read back. That is the feedback loop the current path lacks.
- `wiki_resolve { name }` (Read): the candidates for a `[[Name]]`, with
  the ambiguity said out loud. Name resolution is case-exact and silent
  today, so an agent cannot check a link target before writing one. It
  belongs here because it is what makes `add_relation` land on a real page.
- `crates/molt-mcp/src/lib.rs`: the two tools, their scopes, the
  `INTERNAL` list if anything internal is added, and the server
  `instructions` block - which today teaches `wiki_patch` as THE way to
  write.
- Keystones: every edit kind round-trips
  (`apply_patch(build_patch(x)) == x`); an ambiguous `replace` is refused
  and writes nothing; a read-only key cannot call `wiki_edit` (the
  scoped-tools test already has the shape); `wiki_resolve` names both
  candidates for an ambiguous basename.

### Step 6 - Maintenance affordances. ~2 days. Depends on 5.

These exist for an agent that MAINTAINS the wiki, which is only true once
it can write. Earlier would be building for nobody.

- `wiki_changes { since_rev, limit, cursor }` (Read): the paths touched
  since a revision, with their change kind. The fold already computes
  `touched_paths` per patch (`crates/molt-core/src/wiki_fold.rs`); the
  work is keeping enough per-revision history to answer without a refold.
- The graph's hygiene as ONE read (`wiki_health`, or parameters on one
  tool - decide, do not ship three): `WikiGraph.dangling` (`graph.rs:69`,
  computed today and exposed nowhere) = what this republic references but
  does not have; orphans = documents with no in-edges; and the key-drift
  hint - header keys differing only by case or separator (`status` vs
  `Status`), the failure that silently empties property queries elsewhere.
- Keystones: a deleted target turns its in-edges dangling and the report
  says so; a rename moves a path out of the orphan list; `since_rev`
  answers the same set the fold touched.

## Sources

- Semantic MediaWiki, inverse properties (`-Property`) and the fact that
  `#ask` does not close transitives:
  https://www.semantic-mediawiki.org/wiki/Help:Inverse_properties ,
  https://www.semantic-mediawiki.org/wiki/Help:Inline_queries
- Coding-agent scaffolds converging on exact-string edits, and why
  line-number diffs are fragile for LLMs: "Inside the Scaffold: A
  Source-Code Taxonomy of Coding Agent Architectures",
  https://arxiv.org/pdf/2604.03515 ; "To Diff or Not to Diff?
  Structure-Aware and Adaptive Output Formats for Efficient LLM-based
  Code Editing", https://arxiv.org/html/2604.27296
- Metadata drift emptying property queries in practice (Dataview):
  https://www.obsibrain.com/blog/obsidian-dataview-complete-guide
- Agent knowledge-graph servers combining BM25, graph traversal and
  entity/relation tools: https://github.com/getzep/graphiti (MCP server
  surface: search_facts / search_nodes / add_episode)
