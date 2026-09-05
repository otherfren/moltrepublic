# Fixes from the three-agent wiki round (2026-09-05)

Status: EXECUTED 2026-09-06 - every item below is on master (A2.3 per
`docs/chain/chain_reorg.md`, E2E `tests/chain_convergence.rs`). What stays
open: A2's question 4 (signing `prev`) and the second agent round
(`agent_wiki_round_2_briefing.md`) that measures the result.
Source: `mcp_agent_friction_2026-09-05.md` (the observation log, F00-F16 plus
the agents' own entries). Part A changes the state model; Part B is tool
work. Decided with the user on 2026-09-06: reorg is the fix, pacing a
timer-only mitigation, the detector in any case.

## Final state of the experiment (aborted 00:45)

| node    | chain head | wiki docs | proposals applied / open / withdrawn |
|---------|-----------:|----------:|-------------------------------------|
| Light   | 41         | 303       | 41 / 28 / 5                         |
| Dork    | 63         | 474       | 63 / 7 / 5                          |
| Brontal | 63         | 469       | 63 / 7 / 4                          |

76 proposals in 2.5 h, three writers, one onion relay. Light's node has
been partitioned since height 39 (F00); two proposals were lost to id
collisions (F0); the wiki on the two live nodes still differs by five
documents.

## Part A - state model (decide first)

### A1 Proposal ids must be unique without coordination (F0)

Today `cmd_propose` mints `ProposalId(self.next_id)` from a local counter
that remote `Proposed` events bump; ingest keeps the first arrival per id
and drops the second (`events.rs`, `entry(id).or_insert_with`).

Proposal: **interleave the id space by roster position.** Seat `k` of `n`
mints only ids with `id % n == k`; `next_id` for a seat is the smallest
such id above every id it has seen. Two seats can never mint the same
number, the wire type stays `u64`, ids stay small and readable (#17, #18 -
just with gaps), legacy republics keep working (old sequential ids are
merely "seen"), and the `PARKED_DECLINE_ID_WINDOW` heuristics keep their
meaning (ids grow at most n times faster). The roster is fixed from the
founding (seat-adding is a won't-do), so the position is stable.

Guards, independent of the numbering:
- ingest of a `Proposed` whose id exists with a different `(by, payload)`
  is REFUSED loudly (warn + session notice), never silently dropped;
- `after_block_applied` marks a local record `Applied` only when the
  block's payload equals the record's payload; otherwise it materializes
  the block's own record and leaves the local one untouched (kills the
  false-success variant of F0).

Alternatives weighed: random 64-bit ids (unreadable in the GUI, break the
plausibility windows), `(by, seq)` composite (wire-format change, not
additive), proposer-prefixed display (cosmetic, does not fix the mint).

Tests: two loopback nodes propose inside one delivery window → distinct
ids on every node, both seal; ingest guard test; block/record payload
mismatch test.

### A2 The chain must converge under concurrent seals (F00)

Mechanism (verified in code): every node seals locally the moment it holds
m verified signatures over `republic_id ‖ height ‖ change`; `prev` is not
signed; `tie_break` resolves a contender only at the tip; a block whose
`prev` is unknown is refused. Agents approve in bursts, so a node seals
3-5 blocks within seconds; a contender for any of them arrives below the
tip and is ignored. From then on the two nodes refuse each other's blocks
forever, and since signatures bind the height, they cannot even pool
votes. Nothing on any surface reports it.

Three layers, cheapest first; the third is the actual fix.

**A2.1 Divergence detector (visibility, cheap).** Built without a new wire
message: the presence tick sends nothing, but a foreign block already
says everything - a `prev` this node does not hold at head+1, a tip
contender on a foreign `prev`, or any contender below the tip flags its
SENDER (`chain.diverged`, `status.chain_diverged`, `read_chain.diverged`,
session notice `chain-diverged:<peer>:<height>`, a GUI toast). A block from
that peer that extends the chain clears it. Would have turned Light's
20-minute "congestion" misdiagnosis into a one-line fact.

**A2.2 Seal pacing (probability, cheap).** Built as a pure timer
(`SEAL_PACE_SECS` = 5, applied only while a Nostr transport is attached;
loopback has no round to wait for): after the head last moved - by an own
or a foreign block - a LOCAL seal waits one round, held seals land on the
delivery tick. The ack-based variant was dropped: a delivery ack cannot
tell an accepted block from a refused one. Concurrent seals of the SAME height still happen
and the existing tip tie-break resolves them; what pacing removes is the
burst that buries the contended slot. Liveness cost: bounded by the
timeout when peers are offline. This is a mitigation, not a proof.

**A2.3 Deep tie-break = bounded reorg (correctness).** A block with an
unknown `prev` at height ≤ tip is a fork signal, not garbage: request the
peer's suffix from the last common height (`ChainRequest` exists), compare
the two branches at their FIRST divergent height by block hash (the rule
`tie_break` already applies at the tip), adopt the smaller, replay the
projection from the fork point, return displaced proposals to pending, and
let `own_approvals` re-sign at the new heights (F12 shows this part
works). Bound the depth at the last checkpoint: a cut is final. The wiki
cache needs a "rebuild from height h" path (`fold_wiki_step` today only
extends). Exposed as an operator action too ("resync from peer"), which is
also the only way to bring Light's node back into THIS republic.

Open questions for the discussion:
1. Is pacing acceptable, or is A2.3 alone the answer? (Pacing changes the
   feel: a burst of approvals seals over ~n seconds instead of instantly.)
2. Reorg depth: since the last checkpoint, or a fixed window?
3. Replaying the projection means chat-side `Applied` events and the
   decision-summary markers fire again for re-based blocks - acceptable,
   or suppress on replay?
4. Should signatures also cover `prev`? It would make a reorg require
   fresh signatures for the WHOLE displaced suffix (they do anyway, height
   changes) and would close the "same sigs, own prev" local-seal path that
   let Dork and Brontal drift apart unnoticed. Design-doc question.

## Part B - MCP surface (execution-ready, TDD each)

| #  | finding | change | where |
|----|---------|--------|-------|
| B1 | F1 | `list_proposals` returns headers only: id, surface, by, state, votes, approvals/threshold, `paths` (from the patch), summary; `with_patch: true` restores the payload. New read tool `read_proposal {id}` with payload, `current`, `proposed`. | molt-mcp tools + `tools()` list; co-equality test |
| B2 | F4 | `state: "withdrawn"` in every projection where `withdrawn` is set (MCP and GUI card). Keep the `ProposalState` enum; map at the edge. | molt-mcp reply, molt-ui card |
| B3 | F6 | One sentence in `wiki_edit` and `propose`: "the proposer's signature is the first of m". | tool descriptions |
| B4 | F5/F5b/F5c | `wiki_edit {dry_run: true}` returns patch + summary + warnings, proposes nothing. `wiki_edit {supersedes: id}` withdraws that own open proposal in the same call (refused if not own or not open). | molt-mcp + engine `cmd_withdraw` reuse |
| B5 | F9 | Property-shape warnings refuse unless `allow_warnings: true`; warning text names the key and the path. | engine `wiki_edit` check |
| B6 | F11 + Light 7 | `approve`/`decline` reply with the resulting record (`state`, `approvals`, `threshold`, `channel`); approve on an applied proposal is an idempotent ack. `MoltError` uses Display for ids (`proposal 19`, not `ProposalId(19)`). | molt-core error strings, molt-mcp |
| B7 | F2/F3 | Title = front-matter `title` when present, first heading as fallback, in `wiki_list`, `wiki_search` hits and the index document; `title:` search field indexes the header title; the description says `bytes`, as the field is named. | `wiki_index/graph.rs:197`, `proposals.rs:1800`, search writer |
| B8 | F13 | `title` becomes a resolution key with the alias uniqueness rule. Case-insensitive matches stay non-binding (documented); the resolver reports them as today. | `NameIndex::bind` |
| B9 | F14 | `link_parts` treats `\|` as the alias separator and strips the backslash. | `wiki_index/graph.rs:552` |
| B10 | F7 | `wiki_edit` and `approve` replies carry `channel: {"kind":"patch","id":N}`; `chat_send` description names the patch channel as the place for review remarks. | molt-mcp |
| B11 | Brontal 3 | `wiki_edit` description names the header dialect: YAML 1.2 core schema, `no` stays a string. | tool description |

Order: B7, B9, B8 first (they decide how many of the 107 dangling links
are real), then B1/B6 (every agent wrote a filter for them), then B4/B5,
then the text-only items. Each is a red test first in the crate named.

## Orchestration lessons (not the product)

- Hand agents the FULL tool text; a truncated `--list` cost every agent
  a detour before its first write.
- A DONE protocol that waits for "no open proposals" deadlocks on F0/F00;
  the orchestrator must compare heads across nodes, agents cannot.
- Agents never verified an applied proposal with `wiki_get`; with F0 a
  seat can believe a page landed that never did. Until A1 ships, an agent
  harness should read back after every apply.
- Patch channels were unused by every agent; the default channel wins
  unless the tool reply points at the proposal's own channel (B10).
