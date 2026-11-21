# Chain divergence and failed rejoin: findings

**Status: Phase O and the isolated live runs done on two of three
seats, Phase O on all three (2026-11-21); the tracing logs are missing.** Plan: `docs/chain/rejoin_divergence_analysis_plan.md`. Counts
and shapes only; seats are A, B, C. Times are UTC. "Own envelopes" are
the `Committed` events a node wrote itself: its seals and its re-serves.

## 1. What was read

| seat | chain | event log |
|---|---|---|
| A | anchor 573, head 936, 364 blocks | last 2 days only (compacted), 9 035 envelopes |
| C | anchor 573, head 939, 367 blocks | complete since founding, 43 410 envelopes |
| B | anchor 573, head 933, 361 blocks | complete since founding, 78 193 envelopes |

Both seats hold the SAME anchor block and a byte-identical checkpoint
blob (149 KB). H1-H3 of the plan (divergent anchors) did not happen
between A and C.

## 2. The final state

- Heights 573-927: identical on A and C (355 blocks).
- **Height 928: a real fork on a shared predecessor.** A holds one
  proposal signed A+B, C a different one signed B+C. The seat both
  quorums share signed two blocks at one height (plan D1/D5).
- 929-932: each branch applied nearly the same proposals in another
  order; a block signed B+C sits on A's branch (signatures do not cover
  `prev`).
- From 933/934 on both branches carry only `Restored` blocks, each
  signed by ONE seat plus the returning seat's consent:

| branch | `Restored` blocks | for |
|---|---|---|
| A (934-936) | 3 | C, B, C |
| C (933-939) | 7 | A, B, A, A, A, A, A |
| B (933) | 1 | A |

**B (read later the same day):** same anchor and blob; identical to C
through 932, so B sits on C's side of the fork at 928. At 933 B holds a
`Restored(A)` block of its own, sealed by B alone on 11-20 08:47, while
C had sealed a different `Restored(A)` at 933 the evening before. Three
seats, three heads (936, 939, 933), three branches: every seat
coordinated a restore alone. From then on B sent only catch-up requests
(about ten an hour) and a few approvals nobody could count.

## 3. Timeline

Forks per day, from C's own envelopes (a height counts once it was
written with more than one predecessor/proposal/signer set):

| day | proposals | heights written | newly forked heights |
|---|---|---|---|
| 11-11 .. 11-12 | 37 | 23 | 0 |
| 11-13 | 395 | 79 | 1 |
| 11-14 | 127 | 80 | 0 |
| 11-15 | 193 | 177 | 4 |
| 11-16 | 279 | 239 | 1 |
| 11-17 | 1 296 | 74 | 57 |
| 11-18 | 762 | 155 | 19 |
| 11-19 | 1 491 | 122 | 57 |

139 heights were written in more than one variant, up to six. The
first fork is on day 3; the rate follows the proposal load, not a
release.

The end:

| time | event |
|---|---|
| 11-19 16:20-20:32 | heights 923-933 re-sealed and re-served every few minutes on both seats |
| 11-19 20:29 / 20:50 | last ordinary block on C (932) / on A (933) |
| 11-19 21:37 | A seals `Restored(C)` alone; serves the bare anchor, no blob |
| 11-19 21:41 | C seals `Restored(A)` alone; bare anchor, no blob |
| 11-20 06:54, 08:20 | C, then A, each seal `Restored(B)` alone |
| 11-20 10:45 | C seals `Restored(A)`; blob in 3 parts of ~95 k characters; re-pushed every 10 min |
| 11-21 | further restores on both; blob in 5 parts of ~48.6 k characters |

After 11-19 20:50 no ordinary block sealed anywhere; both seats sent
only catch-up requests (150-180 a day each).

## 4. Verdicts so far

| row | verdict |
|---|---|
| D1/D5 (two sealers, shared seat signs twice) | CONFIRMED as the shape of the last fork and of the 139 forked heights |
| H1-H3 (divergent anchors) | REFUTED for A and C |
| Healing | worked up to 927 (every earlier fork converged), failed for 928; cause open |
| R (rejoin) | none of 10 sealed restores completed; cause per attempt open |

New, not in the plan:

- **N1 Mutual restore.** Each survivor restored the OTHER, four minutes
  apart, each on its own branch. A restore seals with one survivor plus
  the consent, so two diverged survivors never need each other: the
  restores cannot converge, and each re-keys the MLS group against the
  other. Nothing checked that the restored seat was lost; both were
  running. Whether the two re-keys locked the old nodes out of each
  other is the question for the code reading and Phase L.
- **N2 A restore deepens the fork.** Every attempt added a block only
  its own branch has (10 in total).
- **N3 The first two restores (v0.0.17) served no blob at all**; the
  149 KB blob is above the single-frame cap (O3 of the previous plan).
- **N4 One consent names height 49** where the others name head - 1;
  to explain.
- **N5 The test relay publishes no size limit**; whether a part frame
  passes is a measurement for Phase L.

- **N6 The relay list changed on one branch only.** The republic went
  from 3 relays to 2 (one kept dropping out). The `set_relays` block is
  A's last ordinary block, above the fork: sealed A+B on A's branch,
  never sealed on C, where the proposal stayed open. A ran on 2 ratified
  relays, C on 3; the lists still overlapped in 2, so this did not
  separate them, but a change that drops a shared relay on one branch
  would. Recovery links carry the coordinator's list (2 from A, 3 from
  C, the dead relay included).

## 5. Code reading after the dump (not yet reproduced)

- **A live seat can be restored, and can ask for it by itself.** No
  gate anywhere checks that the restored seat is lost or that the
  coordinator's chain is diverged. A running node whose outbox is stuck
  and whose inbound frames are unreadable self-heals by sending a
  recovery request to the other seats (up to 3 per session, 10 min
  apart), each with a fresh anchor, each able to seal a `Restored`
  block. This fits N1 and the repeated restores without any operator
  action; the tracing logs decide between operator and self-heal.
- **The re-key removes the seat's old leaf and adds the new one in one
  commit.** The still-running old node merges the commit that evicts it,
  logs it as an ordinary re-key, and keeps running: its own frames fail
  to encrypt and are skipped while the cursor advances, inbound frames
  are dropped as undecryptable. Nothing tells the operator.
- **Two re-keys at one epoch:** the lower commit digest wins, the loser
  rewinds onto the winner and is thereby evicted; its already sent
  Welcome and its sealed `Restored` block stay. The seat that joined
  from the loser's Welcome is alone and can decrypt nobody; its rejoin
  ends at the deadline. The engine does not handle the rewind at all.
- **N4 explained (inferred):** the consent's number is the minting
  node's own head. A seat opened from an old copy (head 49) reattached
  by itself.
- **Fixed give-ups found** (work list for F-R2 of the plan): the 15 min
  rejoin deadline (bounds four steps), 3 publish attempts for the
  re-key commit and ONE for the Welcome, 12 anchor re-pushes within
  24 h, the genesis re-publish ending after ~7 days. Security bounds
  that need a decision instead of removal: the consent window (1024
  blocks), the 30 min cooldown for unsolicited requests, the clock-skew
  margins, the 3-day expiry on group frames.

## 6. Live reproduction (field binary v0.0.20, isolated)

Copies of A and C, one throwaway local relay, the nodes' SOCKS port
redirected to it; nothing left the machine (the gates refused nothing).

| run | setup | result |
|---|---|---|
| E1 | old A and old C together, 14 min | neither reads the other; unreadable counter 3 388 -> 6 327 on A, 17 656 -> 31 286 on C; no WARN or ERROR on either; no block, no fork decision, `chain_diverged` empty |
| E2 | C alone coordinates, fresh node restores A | SUCCESS: Welcome after 0.04 s, 5 of 5 parts, anchor verified, 367 blocks caught up in ~1 s, one head for 5 min |
| E3 | old A and old C both up, fresh node restores A via C | FAILURE after exactly 15 min, nothing received |
| E4 | A alone coordinates, fresh node restores C | SUCCESS, as E2 |

**The rejoin failure, reproduced (E3) and read in code:**

1. An evicted old node does not know it is evicted and never gets an
   ack, so the delivery guarantee re-offers its whole unacked tail,
   freshly encrypted and freshly stamped, with backoff: ~4 600 events a
   round on A. In one run A published 45 470 group frames, C 10 457.
2. The rejoiner subscribes to the group channel and waits for the end
   of the stored replay WITHOUT reading; the subscription's buffer holds
   5 064 events. More than that in its read window, and the relay task
   blocks on the full buffer, the end-of-replay signal never comes, and
   the run sits silent (no frame line, no heartbeat, megabytes unread on
   the socket) until the fixed deadline.
3. It ends as `the group channel is not readable on any relay`; the
   coordinator hears nothing. The coordinator had already sealed the
   `Restored` block and re-keyed, so every failed attempt left a block
   and one more key the chain trusts and nobody holds.

So the rejoin works on v0.0.20; it fails when a relay holds more than
5 064 group events in the rejoiner's window, and the republic's own
locked-out nodes produce exactly that. Reading "only since the request"
(v0.0.20) cannot help against freshly stamped resends.

**Three seats (E5-E8, same day):**

| run | stored group events at the request | result |
|---|---|---|
| E2 | 981 | success, no paging |
| E4 | 4 544 | success, no paging |
| early E6 | 4 741 | success, paged only after anchor and catch-up |
| E7 (old B and C up) | 5 492 | failure |
| E3 | 22 100 | failure |
| E6 (old A, B, C up) | 23 668 | failure |
| E8 (old A stopped before, only its stored frames) | ~28 200 | failure |

Every failure pages (`got=5001`) before the first anchor frame; the
boundary is the replay bound of 5 000, not the number of old nodes, and
stored frames alone suffice. With all three old nodes up (16 min):
nobody reads anybody, B and C included although their chains agree to
932; zero WARN or ERROR on any seat; unreadable counters 4 955 ->
9 074 (A), 21 555 -> 38 127 (B), 19 223 -> 34 034 (C); frames published
31 814 (A), 2 747 (B), 6 882 (C). B never follows C's new `Restored`
block or re-key: no two seats find back to a 2-of-3 majority by
themselves, every restore only adds one more single-seat branch.

**Self-heal, measured (E3):** both old nodes ran `self-healing via
reattach` by themselves 30.5 min after start, at the eighth fruitless
resend round. The requests went to the working anchors their own chains
name, which are keys of abandoned attempts, so nobody answered. In the
field the anchors were still the original ones: last ordinary block
20:50, mutual restores 21:37 and 21:41. The first two restores were
most likely automatic; the tracing logs can prove it.

**Verdicts, revised:**

| plan row | verdict |
|---|---|
| R1 poisoned run | NOT OBSERVED (the rejoiner cannot read the other branch at all) |
| R2 fixed deadline | CONFIRMED as what ends the run, not as its cause |
| R7/R8 parts | REFUTED as cause on a local relay (5 of 5 in ms); open over Tor and real relays |
| R4, R5 | NOT OBSERVED |
| new R11 | rejoiner deadlocks on a replay above its buffer: CONFIRMED |
| new R12 | evicted nodes flood the relay with resends: CONFIRMED |
| new R13 | a failed rejoin still seals its block and re-keys: CONFIRMED |
| N1 mutual restore splits the group | CONFIRMED (E1) |
| self-heal restores a live seat | trigger CONFIRMED, field link inferred |

Also seen: after every seal the coordinator logs `recovery request
reuses an anchored transport key - dropped` for the same request; a
freshly restored node auto-approved an old open restore proposal for
another seat.

## 7. Open

- The tracing logs: the fork decision at 928 and the rejoiner's final
  reason per attempt.
- The same runs through Tor against the real relay (latency, size
  limits); what first stalled the two outboxes before 21:37 on 11-19.
