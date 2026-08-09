# Review follow-ups: decline convergence (2026-08-09) + file plane (2026-08-10)

## File-plane review, deferred (2026-08-10)

The correctness findings of the file-plane review were fixed in-session
(FileServed trust gates, share-time cap, park watchdog, fetch deadline +
claimed-count bound, current-epoch-only sealing, stamp invalidation on
failure, fsync, reassembler size clamp). Deferred with fix directions:

- **FP1 — publish-budget metering (§5.4, ratified).** Chunk publishes ride
  no budget today; wire them through the same hourly machinery the resends
  use (`group_runtime`'s consumed-budget persistence) so a spent budget
  holds the upload and says so.
- **FP2 — transport-vs-miss signal.** `CatchupSub::recv` returns `Idle`
  both on quiet and on a closed subscription and never `Deaf`; a fetch
  cannot tell "not stored" from "no relay reachable" (the miss message
  names both). Fix in `ritual_net.rs`: surface the closed state, then let
  `fetch_series` report a transport error distinctly.
- **FP3 — fetch-task cancellation.** The fetch loop is bounded by the
  overall deadline but holds its subscription across a workspace close
  until then; a cancellation token (or a generation check inside the loop)
  would end it at close.
- **FP4 — cap semantics.** `file_cap_bytes = 0` is "keep current" on the
  settings surface and "default" in `effective_file_cap`, so a cap of 0
  (sharing off) is unrepresentable and settings cannot RESET to default —
  config.toml stays the one door for both. Decide whether that needs a
  distinct sentinel.

# Decline-convergence review — deferred findings (2026-08-09)

Source: the high-effort review over the defect-6/7 fix wave
(`695e023..44b9d21`). The correctness findings of that review were fixed in
the same session (applier or-insert, mint-counter bound + per-member park
cap, cross-workspace park leak, membership applied-card voters, membership
filter + retention gate on the decline re-serve, approve-refuses-decliner).
What remains here is DEFERRED work, each with its fix direction — none of it
regresses the shipped behaviour, all of it hardens or completes it.

## D1 — declines are bound to a bare id, not to content (design)

A parked/registered decline references `ProposalId` only. Two proposers
minting the same id in one gossip round-trip means a decline can register
against a DIFFERENT proposal than the decliner saw. Approvals are immune
(signatures verify against `approval_bytes`).

**Fix direction:** carry a payload hash in `WorkspaceEvent::Declined`
(additive `#[serde(default)]` field), match it in `register_decline` and the
park drain; an empty hash (older sender) keeps today's id-only semantics.

## D2 — approve/decline are not mutually exclusive end-to-end (design)

`cmd_approve` now refuses a standing OWN decliner, but a collected
signature still stands after a later decline, and `try_commit` counts a
decliner's earlier signature toward the threshold — a majority-declined
proposal can still seal if the sealing node has not seen the declines, and
`after_block_applied` flips even a locally-Rejected card to Applied (the
chain wins by design).

**Fix direction:** decide the retraction semantics (a decline retracts the
own collected signature and vice versa), exclude current decliners in
`try_commit`, and re-sign-proof the decision in the design doc — this
touches vote semantics, so it needs the user's call, not a drive-by patch.

## D3 — wire `MembershipProposed` creates no card on receivers (pre-existing)

`deliver_gated` calls only `receive_membership_proposal` (chain-side
registration); the human-facing `ProposalRecord` is created by the log
APPLIER, which never runs for a wire receiver. An m ≥ 3 recovery therefore
stalls: survivors hold no card to approve. Masked in tests (unit tests
hand-apply; the capstone runs 2-of-2 where coordinator cosign + rejoiner
consent already meet m).

**Fix direction:** record/apply the membership gossip on ingest (the Chat
arm's re-author pattern), plus an m ≥ 3 recovery keystone test.

## D4 — park-drain emits one event for N voices

`register_parked_declines` collapses all drained voices into one
`Event::Declined` naming `decliners.last()` (empty-string fallback). An
event-stream consumer undercounts declines until a full re-read.

**Fix direction:** return the registered voices and emit one event per
voice; collapse `DeclineOutcome` to `Option<tipping member>` while at it.

## D5 — the decision summary is arrival-order dependent

"Minted exactly once on the node whose LOCAL decline tips" yields zero or
two summaries under concurrent declines (wire tips stay silent by design).

**Fix direction:** make the poster deterministic — e.g. only the
lowest-named decliner posts on the Rejected transition, wherever the tip
came from.

## D6 — an over-subscribed vote renders only the m sealed signers

`try_commit` seals the m lowest-named signatures; the applied card's pills
now read from the block, so a third approver shows Open on a vote they
approved.

**Fix direction:** keep the m-of-n proof sigs as the chain truth, but
preserve the full voter set as display data at seal time (record-side, not
block-side — the block layout must not change for a display concern).

## D7 — a full decline park sheds AFTER the accept point

The per-member cap (64 ids) makes honest loss practically unreachable, but
structurally a shed voice was already ACKed and will never be resent. The
sibling G7 `ordered_park` sheds before accepting.

**Fix direction:** move the park admission check ahead of
`accept_envelope` for `Declined` frames, so a shed frame stays unacked and
rides the resend machinery.
