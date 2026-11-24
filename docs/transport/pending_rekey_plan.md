# A durable pending re-key

**Status: PLAN, not started, not for v0.1.3.** Replaces the shelf built and
reverted for v0.1.3 (review 2026-11-24). Shipping behaviour stays v0.1.2's:
`docs_archive/transport/delivery_guarantee.md` §11 - a logged own commit is
re-offered at the reopen.

## 1. The hole (B1)

Coordinator C makes re-key K_C at epoch N, persists commit and ratchet in
one writer step, and dies before any relay took K_C. The device gets a new
link from seat S, which makes K_S at N. C reopens and re-offers K_C.

| during C's outage | K_C re-offered (v0.1.2) |
|---|---|
| S makes K_S at N | lower digest wins: K_C below K_S undoes the second restore, the device is locked out again |
| S makes K_S at N, K_S2 at N+1 | the group ignores K_C; C stays at its own N+1 for good |

The Welcome of the first restore is never re-sent either (§11).

## 2. Why the v0.1.3 shelf was reverted

| finding | cause |
|---|---|
| a seat whose newest merge is its own restore re-key, written by v0.1.0-v0.1.2, is shelved at the first open of the new build | absence of a relay-OK record taken as evidence; v0.1.2 records the prior slot with committer = this seat |
| a shelved re-key made on an earlier UTC day never comes back | the running inbox subscribes the current day's `h` tags only (`window_tags`, no `since`) |
| the seat then sits at N, the group at N+1: deaf for good, the shelf never dropped | no past epochs; nothing else ends the shelf |
| the rejoin note of a restore that succeeded is deleted | the note is withdrawn at the shelving, not at its end |
| a rollback to a released build loses every ratchet | snapshot v6 written unconditionally; v0.1.2 refuses it, `build_group_net` drops the error |
| a downgrade and back loses the record | the older build rewrites `transport.state` without the new field |

## 3. The design

**Shelve only on positive evidence the same build wrote.**

| element | rule |
|---|---|
| marker | `commit_pending = digest`, written in the SAME writer step as the own commit and its ratchet (`append_with_mls_blocking`) |
| clearing | the first relay OK of that commit (outbox or re-key delivery task) clears it through the transport-state gate |
| open | marker present and equal to the prior slot's own key: shelve; marker absent: never touch the state (every older build's file, a downgrade's file) |
| snapshot version | v5 while no shelf exists; the newer version only while a shelf exists, so a rollback breaks only a seat caught mid-shelf |
| replay | while a shelf exists the inbox also subscribes the commit's own day tag (`h` of its stamp), so a stored K_C comes back after a day boundary |
| re-adoption | a commit with the shelf's digest at N re-adopts; at N+1 by a foreign F of epoch N the bystander contest decides |
| end of the shelf | a merge past N+1, a re-adoption, or the day tag's relay expiry (3 days) passed without K_C |
| rejoin note | withdrawn only when the shelf is dropped without re-adoption |
| Welcome | re-sent at the open while the marker stands and the shelf was re-adopted |

## 4. Schedules (tests, red first)

| schedule | expected |
|---|---|
| B1, one commit by S during the outage | C shelves, merges K_S from the replay; the second restore stands |
| B1, two commits by S | C merges both; nobody split |
| no competitor | C shelves, stays at N; a new link re-keys from the old leaf |
| lost OK, same UTC day | relay holds K_C, marker not cleared; C shelves, re-adopts on the replay |
| lost OK across a day boundary | the day-tag subscription replays K_C; C re-adopts |
| upgrade from v0.1.0, v0.1.1, v0.1.2 with an own re-key as newest merge | no marker: untouched, re-offered as before |
| downgrade to v0.1.2 and back, no shelf | v5 snapshot, v0.1.2 runs; marker dropped by v0.1.2, back: untouched |
| downgrade while shelved | v0.1.2 cannot read the shelf snapshot: named in the release notes, operator restores the seat |
| OK recorded, a relay loses K_C before anyone read it | re-offer; the B1 race remains (known limit) |

## 5. An operator signal: behind the group's key

| signal | source |
|---|---|
| `status.mls_behind = true`, `mls_behind epoch= frames=` WARN once | own frames opaque to every peer's acks and peers' frames held as FutureEpoch for longer than one resend round |
| GUI | one line: `behind the group's key - restore this seat` (EN + DE) |

## 6. Alternative: a receiver-side rule

A re-key commit carries the `Restored` block it executes (MLS authenticated
data = the block hash). Receivers decide two commits re-keying the SAME seat
at the same epoch by the chain: the commit whose block is the seat's later
`Restored` block wins, before eviction and digest.

| for | against |
|---|---|
| closes B1 at every receiver, deterministic (the chain is totally ordered) | wire change: older builds send no binding and must fall back to the digest |
| no shelf, no snapshot version, no day-tag subscription | covers re-keys only; C still sits at its own N+1 after two commits and needs the shelf or a restore |
| the Welcome of the superseded restore is moot | a receiver must hold both blocks; a seat behind on the chain decides late |

Recommendation: the receiver rule for the first schedule (one commit), the
marker-based shelf for C's own state after two commits; decide before
building either.
