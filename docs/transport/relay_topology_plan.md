# Relay topology — the group shares its relays, and says so

**Status: PLANNED, nothing built.** Rules ratified by the user 2026-08-02;
recorded in `nostr_transport_marmot.md` §10.15. This document is the execution
plan.

Read first: `relay_pool.md` (relays do not federate; the confirmation gate),
`nostr_transport_marmot.md` §10.15, `nostr_n4_plan.md` §8.8 (N4b, in flight).

## 1. The rules

1. Everyone must be able to reach the same relay. Either all members join over
   one common relay, or — when a member runs their own — that relay must
   already be in every other member's pool **before** they join.
2. The founder has **no special standing**. Coordinating the opening ritual is
   the whole of it; afterwards every member is equal.
3. Relays are **exchangeable**. A member moving to a new relay **re-joins**;
   the others must have added it first, or the join fails.
4. The members **keep a ledger** of who knows which relay.
5. A **split is communicated** — run log and error messages, naming the
   missing relay.
6. The **Create-DAO dialog states rule 1** before invites are minted.

## 2. Why this is not just a UI change

The current gate proves the condition needed to JOIN, not the one needed to
OPERATE. That is the failure class this project keeps meeting: a condition
that holds when it is checked and quietly stops holding later. Two members can
both satisfy "share a relay with the founder" and still share none with each
other.

The founder's pool is also, today, the group's relay list: it is captured at
founding (`founding.rs`, the `dialable` ∩ cap-8 set), baked into every invite
and Welcome, and sealed into `TransportState.relays`. Nothing can change it
afterwards. That is rule 2 violated by construction — not by intent, but
because no other mechanism exists.

## 3. What exists today (verified 2026-08-02)

| piece | where | state |
|---|---|---|
| per-node relay pool + confirmation | `molt_core::relay`, `RelayEntry` | real |
| dialability verdicts per relay | `relay::diagnose_invite_relays` | real |
| whole-pool verdict | `relay::pool_gap` | real |
| join-time gate vs the INVITE's relays | `lifecycles.rs::cmd_join_start` | real |
| operator-facing refusal text | `relay_msg::join_relay_refusal` | real |
| group relay list, sealed | `TransportState.relays` | written at founding/join |
| …read by | the N4b recovery mint only | since step 5 |
| per-member relay knowledge | — | **does not exist** |
| relay change → re-join | — | **does not exist** |
| split detection | — | **does not exist** |

`State.nostr.relays` (N4b step 5a) is the group list in the live actor.

## 4. THE OPEN FORK — where the ledger lives

Rule 4 says the members keep a ledger of who knows which relay. Two homes,
and the choice changes everything downstream:

**(a) Chain-borne.** A member's relay set rides the membership machinery that
already exists — `ChainChange::Membership` carries `nostr_pk` since N4b step 2,
and a re-join is already a membership event. Threshold-approved, converged,
authenticated, survives a reopen; every member computes the same ledger from
the same blocks.
*Cost:* a relay change needs m-of-n approval, so a member cannot fix their own
connectivity alone. Blocks for an operational setting.

**(b) Ephemeral projection.** Each member announces its relay set; others
record it in a runtime projection (like `chat_pos` / `chain_anchors`).
*Cost:* not authenticated state — a peer's claimed relay set is hearsay, and
the ledger diverges per node. But it costs no governance round-trip, and the
announce is exactly the traffic that proves reachability anyway.

**Recommendation: (a), because rule 3 already routes a relay change through a
re-join**, and a re-join is a membership event either way. The ledger is then
a projection over the chain — the same shape as `working_nostr_pk` (step 3),
computed, never persisted separately. (b) would make "who can reach whom"
depend on unauthenticated gossip, which is the thing §10.15 was opened about.

**This fork must be settled before step R3.** R1/R2 below do not depend on it.

## 5. Steps (one commit each, red test first)

### R1 — The Create dialog states the rule
The founder is the one person who can still choose the relay cheaply, and the
only one who sees this screen before invites exist.

- Copy, compact (`CLAUDE.md`): the group needs ONE relay everyone can reach; a
  self-hosted relay must be in every member's pool first.
- Shown in the create wizard next to the relay/threshold inputs, not as a
  modal.
- **Red:** a lexicon test pinning the string is short and names both branches.

### R2 — A refused join names the missing relay
Largely built (`join_relay_refusal`) — this is the audit that it says the right
thing under rule 1, plus the headline.

- The refusal must name what to ADD, not merely what is wrong.
- `headline_for` already maps this to "No shared relay".
- **Red:** the joiner's log names the republic's relay it does not have.

### R3 — The ledger *(blocked on §4)*
`State::member_relays(member) -> &[String]`, a projection like
`working_nostr_pk`. Filled from whatever §4 decides.
- **Red:** after a member joins over relay X, every OTHER member's ledger
  reports X for that member.

### R4 — Split detection
With the ledger, a split is computable: any pair of members with an empty
relay intersection.
- Surfaced as a named state, not a silence: the run log gets a line, and the
  members surface gets a per-member marker.
- **Red:** two members with disjoint relay sets produce a split verdict naming
  both and the missing relay.

### R5 — Relay change → re-join
A member switching relays re-joins; the gate refuses until the others carry
the new relay.
- The refusal names the relay the others must add — that message is the whole
  feature (rule 5).
- **Red:** a re-join whose new relay is in nobody else's pool fails with that
  relay named; adding it on the others makes the same re-join succeed.

### R6 — The founder loses its standing
The group relay list stops being "whatever the founder's pool was" and becomes
group state changeable by the same rule as any other membership change.
- **Red:** a non-founder can carry a relay change to completion.

## 6. Ordering against N4b

N4b steps 6–12 are in flight and touch the same files (`net.rs`,
`nostr_ritual.rs`, `recovery.rs`). R1/R2 are copy-and-test only and can land
beside it. R3–R6 must wait for N4b to finish, or they will collide in
`ChainChange::Membership` (step 2's layout) and in the Welcome payload
(step 10's open size decision).
