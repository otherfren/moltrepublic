# Rejoin and divergence fixes: build plan

**Status: IN PROGRESS (2026-11-21, evening).** B1-B3, B5, B6 and B7a
are on master; B4 and B7b are being built; B8 (acceptance on the field
copies) follows. Evidence: `docs/chain/rejoin_divergence_findings.md`.

## In short

A 2-of-3 republic could not be saved by rejoin and was re-founded. Read
from the three seats' copies and reproduced in isolation:

1. Two seats sealed different proposals at one height (a fork; it had
   happened and healed 139 times before).
2. Seats then "restored" each other, each alone on its own branch. Every
   restore re-keys the group and locks the other seat's running node
   out, silently. Three seats, three branches.
3. The locked-out nodes never got an acknowledgement again and kept
   re-publishing their whole outbox: tens of thousands of frames.
4. Every later rejoiner stalled on that backlog (more than 5 000 stored
   frames) and gave up after a fixed 15 minutes; the coordinator had
   already sealed a block for it.

| stage | what it changes | state |
|---|---|---|
| B1 | a rejoiner (and a founding, a join) reads a large relay backlog instead of stalling on it | on master |
| B2 | a locked-out node notices, says so, and stops publishing | on master |
| B3 | no restore of a seat that is still active or while the chain is forked; a node never asks for a restore by itself | on master |
| B4 | the rejoiner gets the chain anchor BEFORE anything is sealed; no fixed deadlines | in progress |
| B5 | a relay-list change waits until a fork has healed | on master |
| B6 | one sealer per height (measured: fork decisions 11-19 -> 0 in a lossy three-seat test) | on master |
| B7 | fork-free sealing (a Flexible-Paxos lock): design and proven state machine on master (B7a); wiring in progress (B7b) | partly |
| B8 | acceptance: the three field copies with the fixed build | open |
| later | rescue: a rejoin that cuts back to the last common block and restarts a split republic | own design, after B8 |

No signed format changes; old and new builds run mixed. The fixes
protect a seat once it runs the new build; B1 in particular acts on the
rejoiner's side. Nothing is released yet.

Decisions (user, 2026-11-21): build B1-B8 including the fork-free lock;
a locked-out node only reports, it never asks for a restore by itself; a
restore keeps sealing with one survivor plus the consent, guarded; no
fixed deadlines.

## Rules for every stage

- Red test first, with invented fixtures; then code; clippy 0 per crate
  (`molt-core`, `molt-storage`, `molt-net`, `molt-engine`, `molt-mcp`),
  never the window build. A changed `molt-core` type is checked against
  the GUI with the live-preview chain only.
- No signed layout changes: `approval_bytes`, block link, roster,
  checkpoint and consent bytes keep their tags and byte pins.
- Wire additions are additive: new fields `#[serde(default)]`, a new
  variant must be a dropped frame on an older build (pinned with the
  old shape), never owed by the delivery guarantee unless every seat
  can read it.
- Mixed versions keep working: each stage states what an older peer
  sees.
- User-facing strings: one fact, no prose, no em dash.

## B1. The rejoiner reads while it waits (findings R11)

`recovery_rejoin` waits for the end of the group replay without reading;
above 5 064 stored events the subscription blocks for good.

- Drain the subscription during the replay: frames are processed as they
  arrive, "readable" means a relay answered (first event or end of
  replay), not "the replay ended".
- Audit every `live_state` caller in `nostr_ritual.rs` (seat inbox,
  recovery inbox, join, reattach) for the same shape; fix or pin each.
- Tests (`nostr_recovery.rs`, `MockRelay`): a rejoin against a relay
  holding 12 000 foreign group events in the read window completes; red
  before the fix. A join with the same backlog completes.
- Older peers: nothing changes for them.

## B2. A locked-out node notices and goes quiet (R12, N1)

A commit that removes the own leaf is merged as an ordinary re-key; the
node keeps resending its whole tail.

- `molt-net`: detect "own leaf removed" at commit processing and report
  it as its own event (`Evicted`), never as `EpochAdvanced`.
- Engine: on `Evicted` stop the group outbox for this workspace (no
  resends), raise a session notice `seat-evicted`, `status` gains
  `evicted: bool`, and `tracing::warn!(by=, "seat_evicted")`. A later
  successful rejoin or reopen with a valid group clears it.
- The unreadable-frames counter gets a WARN once per episode with its
  count (`group_unreadable count=`), so a split is visible without
  debug logs.
- Tests: A is restored by C while A-old runs: A-old raises the notice
  and publishes nothing afterwards (count on the relay stays flat); a
  plain re-key of another seat raises nothing.
- Older peers: unaffected.

## B3. No restore of a live seat, none by itself (N1, self-heal)

- A node never sends a recovery request by itself: the self-heal path
  (`presence.rs` -> `spawn_reattach`) and the detached-open path
  (`session.rs`) end in the notice `seat-locked-out` and a WARN; the
  operator rejoins with a link. `recover_start` stays the only sender.
- Coordinator gates, at mint, at ingest and at auto-approval, each with
  a refusal the requester and the operator can read:
  - `seat still active`: the seat's current anchor was heard within
    `SILENT_AFTER_SECS`;
  - `chain diverged`: `chain.diverged` or fork candidates are non-empty;
  - unsolicited (link-less) requests from older builds are refused with
    `rejoin needs a link`.
- `try_commit` does not seal a `Restored` card while either gate holds
  (the card stays open).
- Tests: a self-heal trigger sends no `Recover`; a link for a seat heard
  10 s ago is refused at mint; a coordinator with a known fork refuses;
  an old-shaped unsolicited request is refused and answered.
- Older peers: an older coordinator still accepts everything; an older
  node still self-heals, and now gets a refusal instead of a block.

## B4. Anchor first, then seal; no fixed deadlines (R13, R2, R7)

B4 built 2026-11-21.

Today: request -> seal -> re-key -> Welcome -> anchor over the group,
one 15 min deadline over all of it. A failed attempt leaves a block and
a key nobody holds.

New order, negotiated by a version field in the request (additive; a
request without it runs the old order):

1. Rejoiner sends `Recover` with `anchor_first: true` and its version.
2. Coordinator verifies seat proof and gates (B3), then sends the anchor
   (checkpoint blob in parts of at most 24 KiB + anchor block) as gift
   wraps to the rejoiner's new key. Nothing is proposed yet.
3. Rejoiner verifies the anchor standalone (`verify_served`) and
   answers `RecoverReady`; missing parts are asked for by index
   (`RecoverWant`), repeated with backoff.
4. Only then the coordinator proposes, seals and re-keys; the Welcome is
   published until a relay accepts it (backoff, no attempt cap).
5. The rejoiner materializes from the anchor it already holds and
   catches up with an ordinary `ChainRequest`.

Deadlines: `RECOVERY_WELCOME_TIMEOUT`, the re-key and Welcome attempt
caps, the anchor re-push cap and window, and the end of the genesis
re-publish go. Every wait runs until `recover_cancel` or a definite
refusal, with a step line at least every 30 s (`rejoin_step`) and the
catch-up bar. The rejoiner's "since the request minus 10 min" read
window goes with the old order's anchor wait. Kept, as bounds and not
deadlines: the consent window (1 024 blocks), the cooldown on
unsolicited requests, the 3-day frame expiry.

- Tests: a refused or abandoned attempt leaves the coordinator's head
  and MLS epoch unchanged; an anchor that needs 20 min (paused clock)
  still completes; a lost part is re-fetched; the Welcome survives a
  relay that refuses the first five publishes; an old-order rejoiner
  against a new coordinator and a new rejoiner against an old
  coordinator both complete.
- Byte pins for `RecoverReady`, `RecoverWant` and the new request
  fields.

## B5. A relay change waits for a healed chain (N6)

- `sign_blocker` refuses to sign a `set_relays` approval while a peer is
  diverged or fork candidates exist (`relays wait: chain diverged`); a
  held approval is signed once the chain heals.
- Test: a fork, a `set_relays` proposal: no signature until the fork is
  decided, then it seals on the one chain.
- Older peers: sign as before; the guard lowers the odds, the lock (B7)
  closes the hole.

## B6. One sealing line per height (D1-D3)

- The line is built per HEIGHT from the seats heard, not per proposal
  from its signers; the turn-holder seals the smallest proposal id that
  holds the needed signatures at head+1.
- A node records its own `Committed` before the approvals it re-signs
  for the next height.
- An approval for head+2 is parked until the head moves, not refused.
- No cut is proposed or co-signed while a peer is diverged or a variant
  is unhealed.
- Tests: the convergence test grows to three seats with bursts and 10 %
  dropped frames: one head hash, fork count recorded and below the
  current value; the D2 ordering as a unit test.
- Older peers: seal freely as today.

## B7. Fork-free sealing: the lock (S6)

Its own design doc under `docs/chain/` comes first, as part of this
stage. Shape: Flexible Paxos over the chain heights, phase-2
quorum = m (the block's signatures, unchanged bytes), phase-1 quorum =
n - m + 1.

- B7a: the design doc, and the lock as a pure state machine
  (`chain/lock.rs`, no I/O): acceptor (promised ballot, accepted
  (ballot, change) per open height) and leader (announce, collect,
  takeover with the highest accepted value). A deterministic simulation
  test drives n seats through random delivery orders, drops, duplicates,
  crashes with restart from persisted state and leader changes, and
  asserts: never two different changes with m signatures at one height.
  The same simulation without the promise check must find a fork.
- B7b: wiring. Approvals stay intents; position signatures are given
  only to the announced (height, ballot, change) and only at or above
  the promise; promise and acceptance are persisted before the frame
  leaves; ballots travel as additive fields; takeover replaces the
  sealing-line fallback. Active only while every roster seat advertises
  the capability (presence), per height: one seat without it means the
  B6 rules for that height.
- Tests: three seats over `MockRelay` with bursts and drops: zero fork
  decisions; a leader killed mid-height: the next leader completes the
  SAME change; a seat restarted mid-height keeps its promise; mixed
  with one old-shaped seat: B6 behaviour, no stall.

## B8. Acceptance on the field copies

The isolated sandbox of the findings (three seats, local relay), with a
live-preview build of `moltd` from master:

- three old nodes: each reports `seat-evicted` or the split, none floods
  the relay;
- a rejoin with all three old nodes running completes;
- no automatic restore appears within 45 min;
- one fixed seat mixed with two v0.0.20 seats neither stops nor forks
  off.

## Order and hand-over

Wave 1 in parallel (disjoint files): B1; B2+B3; B5+B6; B7a.
Wave 2: B4 (on B1-B3); B7b (on B6, B7a). Then review, B8, and the
status lines of this doc, the findings and the previous plan.
