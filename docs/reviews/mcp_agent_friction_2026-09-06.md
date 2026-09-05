# MCP friction log, round 2: three agents build a protocol knowledge net (2026-09-06)

Status: OPEN - observation log of the second run, stopped by the user at
04:4x after Left's and Center's DONE. Proposed fixes:
`mcp_agent_friction_fixes_round_2.md`. Round 1 and its fixes:
`mcp_agent_friction_2026-09-05.md`, `mcp_agent_friction_fixes.md`.
Briefing: `agent_wiki_round_2_briefing.md`.

## Setup

Three seats of one live 2-of-3 republic ("Second Wiki Test", chain-governed,
`memory` on, one onion relay), nodes on the build that carries A1-A2.3 and
B1-B11 (`a5bc8880`..`2cc85698`), each seat driven by an Opus agent over the
MCP TCP port with the same thin wrapper as round 1 (full `--list` this
time). The user's GUI is attached to the three nodes; the workspaces were
opened over MCP (`open_workspace`) because all three sat on the choice
screen. Seats: A = Mr. Left (4040, Nostr + Bluesky), B = Mr. Center (4041,
Matrix + XMPP + ActivityPub, writes the ontology), C = Mr. Right (4042,
crypto layer, people/orgs, overviews). Traits rolled: Left
data-obsessed/contrary/structure-loving, Center ironic/impatient/contrarian,
Right structure-loving/data-obsessed/enthusiastic.

What this round measures, beyond round 1: the patch channels and topic
channels (rule 1), the HEAD sync protocol and `read_chain.diverged`
(rule 2), verify-after-apply (rule 3), `replace` edits on foreign pages
(rule 4), disputes and declines (rule 5), renames (rule 6), hygiene rounds
(rule 7), and whether A1/A2 hold under the same load that forked round 1.

Legend: **[V]** verified by the orchestrator on the live nodes, **[A]**
reported by an agent, not independently re-checked.

## Findings

### G1 A chat read files its messages under `applied`  [A]

`read_state {surface: "chat"}` returns the messages under `applied`, the key
a GATED surface uses for its applied transitions; the tool text talks
about "the messages it returns" and never names the field, so an agent
looked for `messages` first. One wasted call and a wrong parser.
Candidate: name the key in the description, or alias it as `messages` for
the chat surface.

### G2 The patch channel closes the instant the vote decides  [V]

Mr. Right approved proposal 1 (the tipping vote) and posted his review
reasoning into its patch channel right after: refused with "discussion of
proposal ProposalId(1) is read-only - the vote is Applied". The rule
"comment first, then approve" is learnable, but the natural order for a
reviewer is approve-with-reason, and the reason is lost. Candidates: a
grace window after the decision, or an optional `note` on `approve` /
`decline` that lands in the channel with the vote. Also one more Rust
`Debug` leak (`ProposalId(1)`), fixed during the run.
Second incident 40 minutes later, the other way round: Mr. Right was
WRITING his reasoning for #14 when the third seat's approval sealed it;
the channel shut under his hands and the text went to topic `ontologie`
instead. At 2-of-3 the window between "one vote" and "decided" is exactly
one foreign action wide, so "comment first" does not help the second
reviewer either. The `note`-on-vote candidate is the one that survives
this; a grace window only narrows it.

Positive, verified: the HEAD sync lines land in topic `sync` with
`diverged=[]` from all three seats; the ontology announcement went to
topic `ontologie`; `dry_run` was used before the first batch (Center:
"exactly the right tool before a batch"); `chain_diverged` in `status`
made `read_chain` unnecessary for the divergence question.

### G3 `dry_run` always returns the whole patch  [A]

A dry run for five pages answers 13 KB of patch when the caller only wants
the header verdict. Candidate: `dry_run` returns summary + warnings + paths
by default, the patch behind `with_patch: true` (the same flag
`list_proposals` uses). Praise in the same breath: five pages in one
`wiki_edit`, the patch built server-side, `warnings: []` explicit - "the
reason the round runs at all".

### G4 An inline relation binds to the PAGE, and prose inverts it  [V]

`primitive/x3dh.md` says "X3DH ist durch [[supersedes::primitive/pqxdh.md|PQXDH]]
abgelöst" - grammatically PQXDH supersedes X3DH, but the edge the index
records is x3dh → pqxdh, the opposite of what pqxdh.md's header states.
Mr. Center caught the cycle in review and declined. This is the documented
price of relations-in-the-sentence (`wiki_semantic_decisions`: "an inline
annotation hangs on the PAGE, never on the grammatical subject"); the GUI
tooltip is the safeguard, and an agent writing prose has no tooltip.
Candidates: `wiki_health` reports two-cycles on one predicate (`a supersedes
b` and `b supersedes a`), and the ontology page states the direction rule
("the page is always the subject") where the predicates are listed.
The agents fixed the second half themselves within 20 minutes: ontology
v2 (#16) adds "the subject of an inline edge is always the page it stands
on" and a one-edge rule (assert a relation once, read the inverse with
`wiki_links direction: in`). The two-cycle check in `wiki_health` remains
the tool-side candidate.

Positive, verified: two reasoned declines in the patch channels turned #3
`rejected`; #5 and #7 were corrected via `supersedes` and read `withdrawn`
in the list (B2, B4 working as designed). A dispute opened in topic
`disputes` with a sourced table (rule 5).

### G5 The interleaved id space reads as "holes"  [V]

Ids 1, 3, 5, 7, 8; `read_proposal` on 2, 4, 6 answers "unknown proposal".
Mr. Left spent a research block ruling out lost proposals (round 1's
trauma) before concluding the numbering is per seat. A1's cost, unpaid in
the tool text: nothing says that seat k of n mints every n-th number and
gaps are normal. Fixed during the run in the `list_proposals` and
`propose`/`wiki_edit` descriptions.

### G6 At 2-of-3 the third reviewer's finding has nowhere to land  [V]

Mr. Right reviewed #22 after the other two had sealed it, found a real
error, and got two refusals: the decline ("proposal 22 is already
applied") and the channel ("read-only - the vote is Applied"). Governance
is right - m was reached - but the finding is now homeless: the only path
is a corrective proposal on the applied page, and the review remark that
explains it can only go to a topic. He posted the process note in `sync`,
the wrong topic, for lack of a better one. Same root as G2: a decided
proposal's discussion closes on decision. Candidate: keep a decided patch
channel writable (read-only was chosen to stop chat on dead cards; a
review of an applied change is not dead chat), or let `note` on a vote
land even when the vote itself is late.
Third shape, 02:5x: Center asked a question in #26's channel, #26 turned
`rejected`, and Left had to answer in `sync` - the post-mortem of a
REJECTED proposal has no room either. Whatever the decision, the
discussion of a proposal should outlive it.

### G7 `wiki_edit` accepts an alias that collides  [V]

#25 declared `aliases: ["MSC3575", ...]` and "Signal" already lived on two
pages; `wiki_resolve {"name":"Signal"}` answers `exact: null` with two
alias candidates - both pages lost the name. Nothing in `wiki_edit`
warns; Mr. Left built a checker that crosses every foreign patch's
aliases against `wiki_props`, and two reviewers declined #25 on its
output. Candidate: the header check that already refuses an unreadable
header also reports an alias or title that would collide with an existing
basename, alias or title (a warning under B5's rule, refusing by default).

Positive: the dispute about the Signal Foundation's funding became its own
event page (`ereignisse/signal-foundation-darlehen.md`) with both readings
and sources - "the dispute now has an address instead of a footnote" -
and rule 4 produced five signed sections on foreign pages in one proposal
(#34), each inserted before `## Quellen` with its own source block.

### G8 The `summary` count string is not explained  [A]

A pure-replace proposal answered `~151`; the tool text's example is
"+2 -1 →1 ~34" and nowhere says that `~` counts changed LINES while `+`,
`-` and `→` count files. Candidate: one sentence in the `propose` and
`wiki_edit` texts, or a self-describing summary object beside the string.

### G9 A tip tie resolved, but the loser read `rejected` with nobody declining  [V]

02:2x, height 11: Left and Right sealed #34 (Center's five foreign-page
sections), Center sealed #32 (Left's three pages) - both touch
`protokolle/nostr.md`. `diverged` stayed empty on all three (correct: a tie
at the tip, not a fork), and after about four minutes Center adopted #34;
heads and doc counts agree again. In those minutes Center's list showed his
own #34 as `state: rejected, approvals: 0, votes: open/open/open` -
"NOBODY declined, not even me, and I am the proposer" (his words in
`sync`, for lack of a writable channel). That is the `superseded` flag
(the patch no longer applied to his moved base) rendered as `rejected`,
the same shape as round 1's F4 for `withdrawn`. Meanwhile he re-filed the
content as #43, which the tie-break then made stale in turn. Fixed during
the run at the MCP edge: `superseded: true` reads as `state: "superseded"`.
Open: why the tie took minutes rather than one relay round - the pacing
delays seals, not the tie-break, so the contender's block must have
arrived late or been re-served.
R4 in action, verified: Center's displaced #32 (Left's pages) went back to
the vote and sealed again at a later height - by h14 it is `applied` on
all three nodes. The same `rejected`-with-open-votes shape hit #45 (Right)
when #37 (Center) created `ereignisse/omemo-urheberschaft.md` first: two
seats creating the same page is a path collision, and the loser learns it
as a phantom rejection. Right's count: of 24 decided proposals, six applied
without his vote (10, 14, 22, 23, 28, 34) - "at m=2 of 3 that is not a
slip, it is the structure" (G6).

### G10 `wiki_edit` does not see the OPEN proposals' paths  [A]

#45 created `standards/xep-0384.md` and `ereignisse/omemo-urheberschaft.md`
while both sat in Center's open #37; the engine checks a patch against
the BASE only, so the call went through and the loser learned the
collision as a phantom rejection once #37 sealed (G9). Rule 4 asked the
agents to check open proposals by hand; the engine holds that list.
Candidate: `wiki_edit` (dry run included) reports every touched path that
an open proposal already touches - `path X is in open proposal N` - as a
warning under B5's rule, and `list_proposals` already carries `paths` to
make the manual check cheap.

G2 tally so far, one seat: four review remarks lost to a channel that
closed while the text was being written (#1, #14, #22, #32).

### G11 `replace` matches substrings, so a section anchor breaks itself  [A]

The rule-4 idiom inserts a foreign section before `## Quellen`; each seat
adds its own `### Quellen (Sitz B)` source block. That heading CONTAINS
`## Quellen`, so the next seat's `replace {old: "## Quellen"}` finds two
occurrences and is refused. Workaround: anchor on `\n## Quellen\n`.
Candidate: a `replace` that matches whole lines when `old` has no
newline, or the tool text naming the substring rule and the anchor idiom.

Positive: #47 renamed a page and repaired the in-links in the same
proposal, keeping the old name as an alias (rule 6, no dangling edge).

### G12 The vote reply counts what THIS node holds, and that lags the chain  [A]

`approve` on #10 answered `state: proposed, approvals: 1/2`; `read_proposal`
right after showed the PROPOSER's own vote as `open` - although the tool
text says the proposer's signature is the first of m. The proposer's
signature travels as gossip and had not landed on Left's node, while
Center's node (holding Left's signature) had already sealed the block; a
moment later the block arrived and the card flipped to applied. Left
nearly reported a DIVERGENZ; `read_chain` settled it. Same on #30, #39,
#46. Candidate: the vote reply says what its counts mean ("signatures
held here; the block may already be sealed elsewhere") and carries the
head height, so a reader sees the chain move before the votes catch up.
Center reports the same independently and rates it CRITICAL against the
tool text ("the reply is the record after the vote"): `approve {47}` said
`approvals: 1`, two seconds later `read_proposal` said applied 2/2.

Right adds to G9: a path collision surfaces as a silent `rejected`, not as
the "patch does not apply" the `propose` text announces - "from outside
indistinguishable from two seats declining". The `superseded` state fixes
the reading; the propose text needs its sentence adjusted.

### G13 A full rewrite of a foreign page is indistinguishable from a create  [V]

#48 (Right) carried `organisationen/xsf.md` as a `content` edit - a full
rewrite of Center's page from #28. Center declined on method (rule 4:
foreign pages take a signed section via `replace`, never a rewrite). The
tool cannot tell a rewrite from a create: `content` on an existing path is
accepted like any other edit, and the reviewer has to notice from the patch
that a `---` header line was replaced rather than a section added.
Candidate: a warning (B5 rule) when `content` targets an EXISTING page
whose last applied proposal came from another seat - "rewrites a page last
written by Mr. Center (#28)"; the applied projection knows both facts.
Right, the proposer, reports it as first-order from his side: he did not
know the page existed; `content` "creates OR overwrites, with no
difference and no warning", and the tool text only says it "also CREATES
a document when the path is free". His wish: a distinct `create` op that
refuses an existing path, so a create can never silently become a rewrite.
That is the cheaper and sharper fix; the authorship warning stays the
complement for deliberate rewrites.

### G14 A held seal reads as "2 of 2, still proposed"  [V]

`approve 50` answered `approvals: 2, threshold: 2, state: proposed`. That is
A2.2's pacing: the head had moved within the last round, so the seal was
held for the delivery tick - but nothing in the reply says so, and an
agent reads "threshold reached, not applied" as a contradiction. Candidate:
the vote reply (and the card) carry a `held` flag or a `sealing` state
while a seal waits for its round, with the seconds left.

G2 tally, one seat, at 03:00: six review remarks lost (#1, #14, #22, #32,
#37, #52).

### G15 A source URL ending in `.md` is indexed as a wiki link  [V]

`wiki_health.dangling` lists five entries of the form
`https://github.com/nostr-protocol/nips/blob/master/01.md` - the NIPs are
Markdown files on GitHub, every Nostr source line cites one, and
`body_links` takes any link destination ending in `.md` as a wiki path.
Fixed during the run: a destination with a scheme (`://`) is never a wiki
link.

### G16 Removing a header relation leaves its inline twin  [A]

#54 dropped `supersedes: "[[primitive/olm.md]]"` from vodozemac's header;
`wiki_props` still counted the edge, because the same relation stood in a
sentence as `[[supersedes::...]]`. Edges are a SET with two sources
(decided 2026-09-05: two sources only add, no precedence), so removing one
leaves the other - the tool is right, the author was surprised. `wiki_links`
already marks each edge `header: true/false`. Candidate: the `set_props`
text says "a relation also asserted in the prose stays until that link
goes", and `wiki_health` could list relations asserted twice on one page.

### G17 A negative end state erases the proposer's own vote  [A, measured]

Left counted all 28 proposals he could see: every `applied` card shows
the proposer's vote `approved`, every `rejected` (declined, superseded or
withdrawn) card shows it `open` - zero exceptions. The engine clears the
signature set on a terminal state and the vote table is rebuilt from it,
so the history of who voted how is gone exactly when someone wants to
read it (a post-mortem, G6). Candidate: keep the `voted` display list on
a terminal card as it stood at the decision.

### G18 The wiki_* replies name their list differently each time  [A]

`wiki_search` → `hits`, `wiki_neighbors` → `docs`, `wiki_links` → `edges`,
`wiki_list` → `docs`, `wiki_health` → `dangling`/`orphans`/`key_drift`. A
detour per tool for the first parser. Cheap fix: name the key in each
tool text; a uniform `items` would be the tidy one but breaks readers.

Positive (F14 fix verified live): `[[pfad.md\|Anzeige]]` inside a table
resolves as a link now; a `replace` on such a cell must carry the
backslash in `old`, which Center notes as expected.

### G19 A rename cannot promise "no new dangling edge" while proposals are queued  [A]

Rule 6 asked that a rename leave no new dangling entry. Right measured the
in-links before renaming and repaired every one in the same patch - and an
OPEN proposal of another seat still linked the old path, landing after the
rename. "In a system with a queue, no-new-dangling-edge is a property of a
state, not of one proposal." Candidate: `wiki_edit` warns on a rename when
an open proposal's patch still names the old path (the same open-paths
awareness as G10), and the renamed page keeps the old name as an alias
(the agents already do this by convention, so the late link binds).

### G20 A reasoned decline is powerless once two others sign  [V]

#96: Left declined with source and date (a statement false since
2026-02-12); Center and Right sealed it minutes later; the corrected
re-file #111 went stale under it. Nothing misbehaved - m was reached - but
the wiki carried the provably wrong sentence for an hour until #120
repaired it. Right's proposal (a decline holds the seal for one round) is
in the fixes doc as a design question (A4).

Positive: `wiki_neighbors {predicate: competes_with, transitive: true}`
returned nonsense (IETF among Matrix's competitors) exactly as the tool
text warns - "transitive is the caller's assumption"; the agent read it
as such. Hygiene over the run: dangling 27 → 4, orphans 6 → 6, key_drift
0 throughout; the largest single improvement was the G15 URL cleanup.

## Final state (04:4x, all three nodes)

| measure | value |
|---|---|
| chain head / wiki docs | 59 / 125 on every node, `diverged` empty |
| `wiki_health` | dangling 0, orphans 0, key_drift 0 (from 27 / 6 / 0 at the first hygiene round) |
| proposals | 75: 59 applied, 11 rejected (declines and superseded), 5 withdrawn |
| messages | group 7, patch channels 101, topic `sync` 28, `ontologie` 7, `disputes` 4 |
| relations | 11 predicates, 221 edges (`authored_by` 50, `implements` 41, `maintained_by` 32, `depends_on` 30, `competes_with` 20, `disputed_by` 14, `member_of` 14, `supersedes` 10, `forked_from` 4, `supports` 4, `funded_by` 2) |
| ontology | four versions, each from a measured error |

Against round 1 (three-way fork after 40 minutes, two proposals lost, 20 %
withdraw rate, patch channels unused): no fork in four hours, nothing
lost, reviews where the briefing put them, a wiki with zero dangling
names at the end.

## Running log

- 01:0x workspaces opened over MCP on all three nodes; chain head 0, wiki
  empty; agents start.
- 01:1x ontology v1 applied (2/2 within two minutes; Left did not get to
  read it first); HEAD lines from all three; heads identical.
- 01:3x first batches: #3 rejected after two declines (a supersedes cycle,
  a self-contradicting header), #5/#7 withdrawn and re-filed as #8/#10 via
  supersedes; dispute 1 (Signal Foundation funding) opened.
- 01:5x heads identical at every check so far (h7, 26 docs); A1/A2 hold under
  the same three-writer load that forked round 1 within 40 minutes.
- 02:1x h10, 40 docs, identical; rule 4 (foreign-page sections) and rule 5
  (disputes, declines) both in use; the alias checker Left built catches
  a real collision (G7).
- 02:2x first tip tie at height 11 (#32 vs #34); converged after ~4 min; the
  displaced proposer saw `rejected` with no decliner (G9).
- 02:4x h14, 51 docs, identical; the displaced #32 re-sealed (R4).
- 02:5x h18, 56 docs, identical.
- 03:0x h24, 80 docs, identical; 58 proposals so far, every head check equal
  except the one tip tie (G9).
- 03:3x h30, 91 docs, identical; renames (rule 6), hygiene rounds (rule 7)
  and the first overview page from facet/transitive queries are in.
- 04:3x Left DONE; h57, 125 docs, identical.
- 04:4x Center DONE; the user stops the run; h59, 125 docs, identical.
