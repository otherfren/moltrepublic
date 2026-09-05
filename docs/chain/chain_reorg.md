# Deep tie-break: a bounded reorg for a forked chain (A2.3)

Status: BUILT 2026-09-06 (`chain/sync.rs`: `try_reorg`, `fork_candidates`,
`known_heads`, `serve_from_for`; tests in `chain/sync_tests.rs`). Open:
the loopback E2E listed last, and R6 (signing `prev`), which stays a design
question. Companion pieces: the divergence detector (A2.1) and the seal
pacing (A2.2), both in `docs/reviews/mcp_agent_friction_fixes.md`.

## The defect

`chain/sync.rs::tie_break` settles a contended slot ONLY at the tip. Nodes
seal locally the moment they hold m position-bound signatures, and they
seal in bursts, so a contender for height h routinely arrives when the
local tip is already h+3. The contender is ignored, the sender's later
blocks fail the `prev` check, and the two nodes refuse each other's blocks
forever. Observed 2026-09-05 with three writers: a three-way fork at
height 39, one seat partitioned for good, the wikis at 303/474/469 docs.

## Rules

R1 **The winner at a fork is the branch whose block at the first divergent
height has the smaller [`block_hash`].** This is the rule the tip
tie-break already applies; it is symmetric, so both sides compute the same
winner once both hold both branches. Length plays no role: a shorter
winning branch still wins, and the loser's extra blocks return to pending.

R2 **A reorg never crosses a checkpoint cut.** The candidate chain starts
at this holder's anchor (the genesis, or the blob's anchor block). A fork
point below the anchor cannot be verified here and is left flagged
(`chain.diverged` stays, the operator sees it); recovery is the existing
rejoin path.

R3 **Adopting is all-or-nothing and verified first.** The candidate chain
(our prefix below the fork point + the foreign suffix) is walked with
`walk_own` BEFORE anything moves; a candidate that does not verify (bad
signature, gap, double-apply) is dropped and the peer stays flagged. Then
`adopt_chain` replaces blocks, head, walk and projection in one step -
that path exists and already re-projects, bumps the fold epoch (the wiki
cache rebuilds) and settles the cards.

R4 **Displaced changes return to the vote.** Every `Applied { proposal_id }`
of our dropped suffix that the adopted suffix does not contain goes back to
`Proposed` (materialized records with an empty `by` are removed, exactly
as the tip tie-break does); `rebase_pending_approvals` then re-signs what
this node had approved at the new heights. Membership and checkpoint
changes of the dropped suffix are re-proposed by their proposers (they are
cut/height-bound; today's re-base already drops them).

R5 **Both sides must be able to see both branches.** A block at a height
this node already holds, from a peer, on a foreign `prev`, is a fork
candidate, not garbage: it is kept in `chain.fork_candidates` (bounded, by
height, evicted when it links into our own chain or after the reorg). The
requester asks for the rest with `ChainRequest`, and to let the server
find the fork point in one round the request carries the requester's
`known` heads - `(height, hash)` samples going back geometrically from
its head (an ADDITIVE field; an old server ignores it and serves from
`from_height` as today, which the requester then steps back).

R6 **Signatures stay position-bound.** `prev` is not added to the signed
bytes in this step (open question 4 of the fixes plan); the reorg works
without it, and adding it would be a `molt-chain-change-v3` layout bump
with its own tests. Recorded as the next design question, not built here.

## Wire

- `WorkspaceEvent::ChainRequest { from_height, #[serde(default)] known:
  Vec<HeightHash> }` - `known` is at most 16 entries: the head, then
  head-1, head-2, head-4, head-8, ... down to the anchor.
- No new variant. The served answer is the existing `Committed` fan-out
  from the fork point the server finds: the highest `known` entry whose
  hash matches its own block at that height, plus one.

## Engine

1. `sync.rs::receive_block_from`: a block at height ≤ head whose `prev`
   differs from ours, or a non-tip contender (both are the A2.1 divergence
   cases), goes into `fork_candidates`; then `try_reorg()`.
2. `try_reorg()`: from the lowest candidate upward, find the first height f
   where `candidates[f].prev == block_hash(ours[f-1])` and, for every height
   above f up to the highest candidate, the candidates link. If the run does
   not reach a height ≥ our head, request the missing heights (R5) and
   return. Compare `block_hash(candidates[f])` with `block_hash(ours[f])`
   (R1); if ours is smaller, drop the candidates (the peer will reorg) and
   return. Otherwise build the candidate chain, `walk_own` (R3), then
   `adopt_chain`, then R4, then clear `diverged` for the peers whose blocks
   were adopted and persist.
3. `serve_chain_from` learns the fork point from `known` (R5) and serves
   from there; the debounce stays.
4. An operator read: `read_chain.diverged` already names the peer; the reorg
   is automatic, so no new command. (A "resync from peer" command is the
   same code path, deferred until someone needs it by hand.)

## Tests (red first, `chain/sync_tests.rs`)

- `a_deeper_fork_reorgs_onto_the_smaller_branch`: two Builders fork at
  height 1 and both grow to 3; the holder of the larger-hash branch receives
  the other's blocks 3, 2, 1 (any order) and ends on the smaller branch
  with its displaced proposals back to `Proposed`; the holder of the
  smaller branch receives the larger one and keeps its chain.
- `a_reorg_never_crosses_the_cut`: a pruned holder with an anchor at 5 and
  a fork at 3 stays flagged and unchanged.
- `an_unverifiable_candidate_is_dropped`: a forged suffix with one bad
  signature never adopts.
- `a_chain_request_names_the_fork_point`: the server picks the highest
  matching `known` entry and serves from the block above it.
- Loopback E2E in `tests/`: two nodes seal h and h+1 inside one delivery
  window (the pacing off) and converge within a few ticks; the wiki on both
  is identical afterwards.
