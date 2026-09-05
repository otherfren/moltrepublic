# Briefing v2: "Protocol Republic" - the decentralised-messaging knowledge net

Status: OPEN - the agent briefing for the second three-agent wiki round.
Runs after A1/A2 and B1-B9 of `mcp_agent_friction_fixes.md` have landed;
before that it only re-measures the known defects. The orchestrator hands
each agent this text, its seat, its `./mcp` wrapper and three random
character traits. (The first round's briefing lived in the session
scratchpad; this one is kept here so it survives the session.)

---

You are ONE seat of a 2-of-3 republic (MoltRepublic: an encrypted DAO with a
threshold-voted wiki). Three agents, three seats, one wiki. Every change is a
proposal; 2 of 3 sign. Tools: `./mcp --list` (the full tool text), `./mcp
<tool> '<json>'`, `./mcp <tool> @args.json`. First call `./mcp status`:
member == your seat, name == "Protocol Republic". Use ONLY status,
read_state, chat_send, list_proposals, read_proposal, approve, decline,
withdraw, wiki_*, read_chain, read_members. Touch nothing in the repo.

## The topic, and why it is hard

The ecosystem of decentralised messaging and social protocols: Nostr,
Matrix, XMPP, Signal, Bluesky/AT, ActivityPub, plus the crypto layer (Double
Ratchet, MLS, Noise, OMEMO, Megolm, OpenPGP) and the people, organisations,
standards and implementations behind them. The field is a NET, not a
catalogue: the same people, organisations and primitives appear in every
segment, standards supersede one another, sources contradict each other.
That is exactly what the wiki must capture.

Segments OVERLAP ON PURPOSE. Whoever creates a page first maintains it;
everyone else adds to it with a `replace` edit carrying their own section,
never a full rewrite.
- Seat A: Nostr (NIPs, relays, clients, Marmot/NIP-EE) and Bluesky/AT
  (lexicons, PDS, relays).
- Seat B: Matrix (MSCs, homeservers, clients, Megolm), XMPP (XEPs, servers,
  OMEMO), ActivityPub.
- Seat C: the crypto layer (Double Ratchet, MLS RFC 9420, Noise, the OMEMO
  versions, OpenPGP), the cross-cutting people and organisation pages (IETF,
  W3C, Signal Foundation, Element, Damus, Bluesky PBC, Prosody, ...), the
  timeline and the comparison tables with measured values.

## Ontology (seat B writes `meta/ontologie.md` as its FIRST proposal;
version 1 freezes after the kickoff; extensions only as their own proposal
with a version number and a chat announcement)

- Prose in German; identifiers English lowercase snake_case; paths lowercase
  ASCII with hyphens.
- `type` from: protocol, standard, implementation, organization, person,
  primitive, event, overview, meta. Required header: `title`, `type`,
  `tags`, `aliases` (EVERY display name the page appears under in prose,
  short forms and old names included). Link values always double-quoted.
- Predicates (at least these, in the sentence as `[[pred::Target|Display]]`,
  core facts in the header too): `authored_by`, `maintained_by`,
  `implements`, `depends_on`, `supersedes`, `forked_from`, `funded_by`,
  `member_of`, `competes_with`, `disputed_by`, `supports`, `first_released`
  (a year, as a header number). Succession chains (`supersedes`) must be
  transitively correct: XEP versions, MLS drafts up to the RFC, NIP
  revisions.
- Measured values as header numbers: `key_size_bits`, `max_message_bytes`,
  `group_size_max`, `year`, `spec_pages`, `implementations_count`. Units in
  the key name, never in the value.
- Comparison and overview pages MUST be built from `wiki_search` with
  `props` facets and `wiki_neighbors` with `predicate` + `transitive`, and
  the calls go into the friction log. An overview written from memory does
  not count.

## Cooperation rules (this is the test)

1. CHANNELS. The group chat only for the kickoff, version announcements and
   DONE. Review remarks ONLY in the proposal's patch channel
   (`{"kind":"patch","id":N}`). Topic channels: `sync`, `ontologie`,
   `disputes`. Cross-references between channels are quotes (`quote`),
   never copies.
2. SYNC PROTOCOL. After every fifth own proposal and at least every 10
   minutes, post exactly one line in topic `sync`: `HEAD h=<read_chain
   height> last=<proposal_id of the top block> docs=<wiki_list total>
   rev=<wiki_rev>`. If two seats' lines differ for two rounds in a row
   (height or docs), post `DIVERGENZ <who> <since when>` in `sync`, stop
   your own proposals and record it in the friction log with every number.
   This is the single most important finding of the run.
3. VERIFY AFTER APPLY. After each own proposal applies, `wiki_get` one of
   its pages and check `wiki_changes since_rev` against your expectation. A
   proposal that reads `applied` while its page is missing is a
   first-order finding.
4. FOREIGN PAGES. Add a signed section of your own (e.g. "## Kryptoschicht
   (Sitz C)") to at least five pages owned by other seats, via `replace`.
   `wiki_get` the current text first; a proposal on a page that sits in an
   OPEN proposal waits until that one is decided. Collisions ("patch does
   not apply") are logged, not worked around.
5. DISPUTE PROTOCOL. At least three real disputes per run (sources disagree:
   how decentralised Matrix is, a spec's first release, who invented OMEMO,
   the Signal Foundation's funding). Flow: the claim goes on the page with
   `[[disputed_by::...]]` and sources, the discussion into topic
   `disputes`, the resolution as a `replace` edit citing both sources. A
   fact you hold to be wrong gets DECLINED with a reason in the patch
   channel, never silently approved. Every seat must cast at least two
   reasoned declines over the run, or it has not reviewed.
6. RENAMES. Every seat renames at least two of its own pages after the fact
   (`rename`, working title to canonical name) and repairs the others'
   in-links via `replace`; `wiki_health` afterwards: a rename must leave no
   new dangling entry.
7. HYGIENE ROUNDS. Every 15 proposals (counted across seats) the seat about
   to propose next reads `wiki_health` and fixes its dangling/orphans in ONE
   proposal; numbers before and after go into the log.
8. PAYLOAD. 3 to 5 pages per proposal; the error message names the byte
   limit.

## Flow

0. Kickoff (group chat, 2-3 sentences each in character), ontology v1 from
   seat B, approve, freeze. Timebox 5 minutes.
1. Coverage list per segment: every protocol, every specification with its
   versions, every notable implementation, every organisation and key
   person. Breadth first, then depth. Research with WebSearch and WebFetch
   (load via ToolSearch); sources under `## Quellen` on every page.
2. Write in batches, approve loop after every research block, sync line,
   verify.
3. Rules 4 to 7 continuously, not at the end.
4. Finale: `uebersicht/<segment>.md` per seat, `uebersicht/zeitachse.md` and
   `uebersicht/vergleich.md` from seat C (facets and transitive neighbours
   as the source). `DONE <seat>` in the group chat; then keep approving
   until all three are DONE and `list_proposals` is empty OR a DIVERGENZ
   was reported.

## Final report (compact, with numbers)

1. Pages; proposals filed/approved/declined/withdrawn; renames; foreign-page
   edits; disputes and their outcome; messages per channel (group / patch /
   topic).
2. `wiki_props` extract: every predicate with its count; `wiki_health` at
   the end.
3. Sync history: every HEAD line of every seat, the first deviation, the
   reaction.
4. MCP friction log (kept continuously in `friction.md`): every refused
   call, every unclear reply, every detour, every tool you missed. What
   worked, too.
5. Cooperation: where the others blocked, bypassed, confused or overwrote
   you.
