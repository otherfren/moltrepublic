# What the wiki still lacks to be a semantic knowledge base an agent can use

Status: ANALYSIS + three decisions (user, 2026-09-05); the rest still
open. Every "today" claim below was read out of the tree at that date
(file:line given); the comparisons to other systems carry their sources.

**Decided 2026-09-05** (§7 is the build order that follows from them):
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

## 5. Still open

- §2.4 (`wiki_changes { since_rev }`) and §3 (dangling / orphan report,
  `wiki_resolve`, the key-drift hint) are not scheduled. They are
  maintenance affordances: worth building once agents actually maintain
  this wiki, not before.
- Nothing else. §6 is decided and §7 is the build order.

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

Each step red-first, each landing green on master. Costs are working
estimates, not commitments.

1. **Recall first (~2 days).** Index the header's string values and
   `aliases` into searchable fields (§1.3), and add a `props: [(key,
   value)]` filter to `wiki_search` mapped onto the facets that are
   already written (§2.1). Today a page is not findable under the name it
   declares, and `status: draft` has no query path at all. Nothing else
   in this list buys as much per hour.
2. **Traversal that says WHY (~1 day).** `WikiNeighbor` carries the edge
   that reached it (predicate + direction), `wiki_neighbors` accepts
   `predicate` and `transitive` (§2.2, §6.7). Bounded closure, capped
   like the current walk.
3. **Inline typed links (~2 days).** `body_links` learns
   `[[pred::Name]]` and `[[pred::Name|display]]` - one parser, the same
   code-span masking, `RawEdge.predicate` already exists, so the graph,
   `wiki_links` and the MCP surface need no new shape. `WikiSpan` gains
   `rel`; the preview's `LinkRun` shows it through the existing HintTip
   overlay. `wiki_props` counts inline predicates (§6.6). The GUI's
   editor gets no new syntax helper in this step.
4. **The structured write path (~several days, §1.1).** Move the
   unified-diff emitter from `molt-ui/src/wiki.rs` down to `molt-core`
   beside `wiki_fold::apply_patch` - its exact inverse - and add a Seat-scoped
   `wiki_edit` taking structured edits against the current base
   (`content`, `replace {old,new}`, `set_props`, `add_relation`, `rename`,
   `delete`), building the patch engine-side and proposing it. After step
   3, `add_relation` annotates a SENTENCE rather than setting a header
   key. Read-only agents keep reading only: the scope split already
   exists and needs no new privilege concept. A refusal then happens at
   the call, with a reason, instead of after a vote.

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
