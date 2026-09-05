# MCP friction log: three agents build a wiki (2026-09-05)

Status: OPEN - observation log of the experiment, aborted 2026-09-06 00:45
after the chain fork (F00). Proposed fixes: `mcp_agent_friction_fixes.md`.

## Setup

Three headless-driven seats of one live 2-of-3 republic ("Robo Republic",
chain-governed, `memory` on, one onion relay), each seat driven by an Opus
agent over the MCP TCP port with a thin newline-JSON-RPC wrapper (no
convenience layer beyond `tool + json args`). The user's GUI is attached to
the same three nodes. Task: research the household-robot market and build a
shared wiki with a negotiated ontology. Each agent keeps its own friction
log; this file merges what they report with what the orchestrator verified
against the live nodes.

Legend: **[V]** verified by the orchestrator on the live node, **[A]**
reported by one or more agents, not independently re-checked.

## Findings

### F00 CRITICAL: the chain forked three ways under sustained concurrent load  [V]

Not an MCP finding - the state model itself. At ~00:05, after 47 sealed
blocks, `read_chain` on the three nodes (heights ≥ 39, `height:proposal`):

```
light    39:42 40:43 41:45
dork     39:46 40:43 41:42 42:47 43:48 44:52 45:49 46:50 47:51
brontal  39:43 40:45 41:42 42:47 43:48 44:52 45:49 46:53 47:51
```

Three different blocks at height 39, Dork and Brontal disagree again at 40
and 46, and Light's node has not moved since height 41 while the others
sealed six more. Each node seals locally once it holds m signatures over
`republic_id ‖ height ‖ change`; `prev` is not signed, so every node builds
its own hash-linked sequence over the changes in the order they tipped
THERE, and the tie-break in `chain/sync.rs` did not converge them. Effects
visible over MCP:
- wiki totals diverge: light 303 docs, dork 367, brontal 365 (`wiki_rev`
  41 / 47 / 47);
- Light approved #47-#56 (`votes: aoo` on his node) and none of them tips
  there, while on the other two nodes #47/#48/#49/#50/#51/#52/#53 are
  applied WITHOUT his signature; his own #49 is applied everywhere except on
  his own node;
- the proposer-side chat markers (`⚖ #49 ✓`) arrive on Light's node, so
  the transport is fine - it is the chain layer refusing/parking the blocks.

Conditions: three writers, ~1 proposal/minute, several seconds of relay
latency, and a re-base every few blocks (F12).

Root cause, read in `chain/sync.rs` (verified against the code, not yet
against a test): `tie_break` resolves a contended slot ONLY at the tip
(`is_tip && incoming < current`); its own comment says "a deeper conflict
is logged (deep reorg is Phase 3)" - in fact a non-tip contender is
silently ignored. Sequence: node A seals height h with X; node B seals h
with Y and, before X arrives, h+1 with Z on top of Y. At A, Z fails the
`prev` check in `append_committed_block` ("refused a chain block") and Y
arrives below the tip → ignored. A and B now disagree below both tips
forever: every later block from either side fails `prev`, and catch-up
answers fail the same way. Because signatures are bound to `height`, two
nodes at different heights can never pool signatures either - Light at
next-height 42 versus Dork/Brontal at 48 is a permanent partition.
Dork and Brontal still cooperate only because both seal LOCALLY from the
same gossiped signatures at the same height; every block they broadcast to
each other is refused (their `prev` differ from height 39 on). Nothing on
any surface reports any of this. 10 minutes after the fork Light's node
had not moved; no self-healing path exists in the code for this case.

How it looks to the partitioned seat (Light's chat post, 00:30, verbatim
in substance): `status` memory applied=41 pending=25, `read_chain` at 41
for minutes, 24 open proposals all at 1/2 with `approved_by_me: true`; his
hypotheses were (a) the others are not approving or (b) "the chain cannot
keep up over the onion relay", he chose (b) and THROTTLED his own proposal
rate. The two other nodes had applied every one of those proposals. No
surface offered him the one fact that mattered - his head differs from the
others' - so the agent misdiagnosed a permanent partition as congestion.

Consequence for the design discussion: with position-bound signatures and
a tip-only tie-break, a single concurrent seal that gets built upon before
the contender arrives is fatal. Either the tie-break must handle depth
(deep reorg + re-sign of the displaced suffix), or sealing must be
serialized (a leader/slot rule), or signatures must not bind the height.
A loopback test with two nodes sealing h and h+1 inside one delivery
window would pin it red.

### F0 CRITICAL: concurrent proposals collide on the id and one is silently lost  [V]

Proposal ids are minted from a LOCAL counter (`cmd_propose`:
`ProposalId(self.next_id)`, bumped by every remote `Proposed`). Two seats
proposing inside one relay round-trip (seconds over the onion relay) mint
the same id. On ingest `events.rs` does `proposals.entry(id).or_insert_with`
- first arrival wins, the second is dropped without a trace.

Observed at 23:0x: Mr. Dork and Mr. Brontal both minted #18.
- Light's and Dork's nodes: #18 = Dork's Husqvarna patch (sha 6d8d812c),
  later withdrawn; Brontal's Agility-Robotics patch (sha ccc766dd) exists
  on NEITHER node.
- Brontal's node: #18 = his own Agility patch, `proposed`, `approvals: 1`,
  and Dork's original #18 never existed there.
Brontal's proposal can never reach threshold: the other seats do not hold
it, and an `approve 18` from them signs the position-bound bytes of a
different change. From Brontal's side it simply never seals; from the
others' side "#18" is a withdrawn Husqvarna proposal. Nothing on any surface
says so.

The window is the propagation delay, so with three active writers it is a
regular event, not a corner case: two collisions in the first 34 proposals
(#18 Dork/Brontal, #34 Dork/Brontal - Brontal's Blue-Frog-Buddy patch
de71076d lost the second time; his node is the slowest to gossip). Any fix
needs a collision-free id (proposer-scoped, or a content/envelope hash) and
must survive `verify_chain`'s position-bound signatures; that is a design
change, not a patch. Flagged for the post-experiment discussion first.

**Worse, verified 23:50 (#34):** when the OTHER proposal that holds the id
seals, the losing node applies the block's change (Dork's Honda page) and
marks ITS OWN record #34 - Brontal's Blue-Frog-Buddy page - as `applied`,
`approvals: 2`, every vote `approved`. Brontal's node therefore reports a
successful vote for a page that exists in no wiki, his own included (284
docs on every node, `blue-frog-buddy.md` on none, `honda-miimo` on all).
Silent loss with a false success signal; only a `wiki_get` after the apply
would reveal it, and no agent does that.

Side observation, same read: a proposer's own auto-approval shows
`approvals: 1` on its node and `0` on the others until the signature gossip
lands - the counts a reviewer sees are not the proposer's.


### F1 `list_proposals` carries every patch twice, in full  [V]

Each entry holds the whole patch in `payload.value` AND byte-identically in
`proposed`, plus `current`. A 9 KB ontology proposal is a ~20 KB list entry;
all three agents independently wrote a filter script before the second
proposal existed. With 30 open proposals one vote round costs half a
context window.

Candidate: a header-only list (id, by, state, votes, touched paths) by
default, the patch behind a flag or a `read_proposal {id}`.

### F2 `wiki_list` title comes from the first heading, not the header  [V]

`meta/ontologie.md` declares `title: Ontologie der Robo Republic`; the list
shows `title: "1. Sprache und Schreibweise"`. Every page of the run is
mis-titled the same way (`hersteller/roborock.md` → "Produktlinien",
`kategorien/saugroboter.md` → "Abgrenzung"). A page without any heading
shows `null`. The reserved `title` key is not consulted.
`wiki_search` hits carry the same wrong title (`hersteller/1x-technologies.md`
→ "Modelle").

### F3 `wiki_list` says "size", the field is `bytes`  [V, corrected]

First read as "size is null"; that was the orchestrator reading a field
that does not exist. The entries carry `bytes`, the description says
"path, size and title". A one-word doc fix, nothing more.

### F4 A withdrawn proposal reads `state: "rejected"`  [V]

After `withdraw`, `list_proposals` shows `state: "rejected"`,
`approvals: 0`, `declined_at` set, `declined_by: ""`, and `withdrawn: true`
as a separate flag. The GUI marker in the patch channel reads `⚖ #4 ↩`. An
agent reading only `state` believes its proposal was voted down.

### F5 No way to amend an open proposal  [A]

The ontology changed (`entry_level` → `budget`) between an agent's writing
and its proposal; the only path was `withdraw` + a fresh `wiki_edit` with a
new id, losing the approval already collected. Suggested by the agent:
`wiki_edit {supersedes: <id>}` that withdraws the old one in the same step.

### F5b `wiki_edit` has no dry run  [A]

The only way to see the diff a `wiki_edit` produces against the live base is
to propose it. A price typo (1500 for 1499) therefore cost a `withdraw`, a
second proposal (#12 → #13) and another seat's approval work. Suggested:
`dry_run: true` returning the patch and the warnings without proposing.

### F5c Withdraw-and-refile is the dominant correction path  [V]

By proposal 20, four proposals (4, 12, 18, 20 - one per seat at least) were
withdrawn and re-filed under a new id: a moving schema, a price typo, an
unquoted link value, and one more. That is 20 % of all proposals costing a
second approval round. F5, F5b and F9 are the three tool gaps behind it.

### F9 `wiki_edit` warns where its description promises a refusal  [A]

An unquoted `successor_of: [[pfad.md]]` (YAML reads it as a nested
sequence) went through as proposal 18 with `warnings: ["... a sequence
holds scalars or flat mappings"]` x5 instead of being refused. The tool
text says a header "the parser would not read back" refuses the whole call.
The header WAS readable YAML, just not the intended value - so the contract
is technically kept, but the warning arrives only once the patch is already
in the vote (F5b), and the fix is F5c. Candidate: treat property-shape
warnings as refusals unless the caller passes `allow_warnings: true`.

### F10 Vote markers are chat messages with `kind: "system"`  [V, design]

The `⚖ #N ✓ Wiki: +5` lines in the patch channels carry `kind: "system"`
over MCP, so an agent can filter them. Their `from` differs per node
(D5: whoever tips posts, copies collapse by deterministic id, so node A
shows "Mr. Light", node B "Mr. Dork" for the same line). Withdraw markers
(`↩`) come from the proposer. No agent tripped over it; noted because the
author field of a system line is meaningless and an agent summarising
"who said what" will misattribute it.

### F6 Proposing counts as approving, undocumented  [A]

The proposer's own vote is `approved` immediately (`approved_by_me: true`,
`approvals: 1`). Neither `wiki_edit` nor `propose` says so; the agents
inferred it from the vote table. Cheap fix: one sentence in both
descriptions ("the proposer's signature is the first of m").

### F7 Agents do not use the patch channels  [V]

By proposal 7: seven proposals, every review remark ("`status` is now
`availability`, fix in the prose"), every "please approve #N", every
ownership claim for manufacturer pages went to the all-hands group channel.
The patch channels hold only the automatic vote markers. The briefing had
explicitly pointed at `{"kind":"patch","id":N}` for review remarks. The
channel argument is optional and the default wins; nothing in `chat_send`
or `approve` nudges a reviewer towards the proposal's own channel.

### F8 Proposer-side rework caused by a moving ontology  [V, process]

Not an MCP defect, but the dominant cost in the first 20 minutes: the schema
page changed twice while pages were being written against it. Whatever the
tool surface, a "schema" page needs a freeze point; the agents solved it
socially (Fassung 2 declared final in chat).

### F12 A slot collision re-bases and heals, but the chat marker fires early  [V]

Proposal 30 tipped at the same height as #31: Light's node posted
`⚖ #30 ✓ Wiki: +5`, then the re-base wiped the vote table (`approvals: 0`,
every vote `open`, the proposer's own included, on Light's node only). It
healed without agent action within about a minute (own_approvals re-sign,
#30 sealed at height 29 after #32/#33). For an agent the minute reads:
chat says applied, `list_proposals` says proposed with zero votes, the
three nodes disagree. Nothing broke, but the intermediate state is exactly
the shape of F0 - only F0 never heals. Distinguishing the two from outside
is impossible without comparing patch hashes across nodes.

### F11 `approve` on a just-sealed proposal is an error, in Rust debug syntax  [A]

Between `list_proposals` and `approve` the block sealed; the reply was
`tool_error: "proposal ProposalId(19) is already Applied"`. Two agents hit
it on their approve rounds. Expected: an idempotent ack ("already applied,
nothing to do") - a late approval changes nothing and is the NORMAL race
with three reviewers. Also, `ProposalId(19)` is the Rust `Debug` form
leaking into user-facing text (`molt_core::MoltError` display).

### F13 The front-matter `title` is not a link-resolution key  [A]

`wiki_health` after ~370 pages: `dangling: 105, orphans: 199, key_drift: 0`.
Dork's diagnosis: `[[Unitree Robotics]]` does not bind although
`hersteller/unitree.md` carries exactly that `title`; `wiki_resolve` binds
path, basename, stem and `aliases` only. Every page written with natural
display names in the prose therefore dangles unless the author duplicates
the title into `aliases`. Together with F2 (the list ignores `title` too),
the reserved `title` key is read by nothing but the renderer. Candidate:
title as a resolution key with the same uniqueness rule as an alias.

### F14 `[[path\|Display]]` inside a Markdown table dangles  [V]

49 of 107 dangling names end in a backslash: `hersteller/tesla.md\`. Source
lines look like `| [[roboter/humanoide/1x-neo.md\|NEO]] | 2025 | ...` - the
Obsidian/CommonMark idiom for a display-text link inside a table cell,
where a bare `|` would split the cell. `link_parts` splits at the first
`|` and keeps the escaping backslash in the target. Every manufacturer
page with a model table is affected. Candidate: treat `\|` as the alias
separator (strip the backslash) - the same thing every wiki renderer does.

### F15 `approve` replies `ack` and nothing else  [A]

After twenty approvals an agent cannot tell which one tipped a proposal;
every agent re-read `list_proposals` (F1) after each approve block.
Expected: the resulting record (state, approvals/threshold).

### F16 Smaller items from the agents' logs  [A]

- A property-shape warning names the file, not the key (20 headers per
  page = a search).
- `wiki_search {"query":"title:Ambrogio"}` finds nothing although five
  pages carry `title: Ambrogio ...` in the header; the `title` field of the
  index is the heading-derived one (F2), the header title is reachable only
  as a `props` facet.
- `[[Honda]]` does not bind to `hersteller/honda.md` (case). Documented and
  defensible; in German prose the most frequent break after F13/F14.
- The header dialect (YAML 1.2 core: `no` stays a string) is stated
  nowhere in the tool text.

## What worked (agents agree)

- `status` answers seat + republic + threshold in one call; no setup step.
- Tool descriptions are precise enough to work from (one agent: "spec
  level"). The `wiki_search` note "an empty query with no filter finds
  nothing" was read and respected.
- `wiki_edit` refuses BEFORE proposing, with a reason, and returns
  `warnings: []` - nothing half-applied lands in a vote.
- The header parser does NOT apply YAML 1.1 boolean coercion: `country: no`
  (Norway) stays the string `no` in `wiki_props` and as a facet value. An
  agent feared `false`; worth one sentence in the ontology/tool text.
- Threshold sealing and propagation over the onion relay: a proposal is
  applied on all three nodes within seconds of the second signature; a
  slot collision healed without agent action.

## Orchestrator-side friction (not the product)

- The wrapper's `--list` truncated descriptions to 160 chars and schemas to
  600; all three agents wrote a raw `tools/list` dump before doing anything
  else. Fixed during the run. Lesson for any agent harness: the full tool
  text IS the documentation, never abbreviate it.

## Running log

- 22:17 agents start; kickoff in group chat; ontology negotiated in two
  rounds (`status` → `availability` with five values).
- 22:24 proposal 1 (ontology) applied; F1 reported by all three within
  minutes.
- 22:29 proposals 2 (ontology v2) and 3 (five category pages) applied.
- 22:35 proposal 4 withdrawn by its proposer (F4/F5), re-filed as 5.
- 22:40 proposals 6 (Brontal) and 7 (Dork) open; 11 pages in the wiki.
- 22:45 proposals 5 and 6 applied; 21 pages.
- 22:58 proposals 7-11 applied, 12 withdrawn for a typo (F5b), 13 and 14 open;
  ~65 pages.
- 23:10 proposals 13-17 applied; 18 (Dork, F9) and 20 (Brontal) withdrawn and
  re-filed; ~95 pages.
- 23:20 F0 found: #18 minted twice (Dork + Brontal); Brontal's copy lost on the
  other nodes, stuck `proposed` on his. 25 proposals, ~130 pages.
- 23:35 second collision (#34); #30 re-based and healed (F12). 34 proposals,
  ~200 pages.
- 23:50 #34 sealed as Dork's; Brontal's node shows his lost #34 as applied 2/2
  (F0, false success). 45 proposals, 284 pages.
- 00:05 F00: chains differ from height 39, Light stuck at 41, wikis 303/367/365.
  Brontal asks in chat why #18 hangs (F0); 59 proposals.
- 00:20 root cause of F00 read in sync.rs (tip-only tie-break). Dork/Brontal
  continue sealing in lockstep at height 48+, Light partitioned; 65 proposals.
- 00:35 Light posts STAU (misdiagnosed F00 as congestion, throttles); F14 found
  from wiki_health (107 dangling, 49 of them backslash); 75 proposals.
- 00:45 experiment aborted by the user. Final: Light 41/303, Dork 63/474,
  Brontal 63/469 (head/docs); 76 proposals, 63 applied on the live pair.
