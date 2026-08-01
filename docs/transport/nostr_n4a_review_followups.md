# N4a review follow-ups — the fix plan

Status: **IN PROGRESS.** Two independent adversarial review passes ran over
the N4a change-set on 2026-08-01 (round 1: six code-dimension lenses +
per-finding refutation; round 2: inert-keystone hunt, loopback differential,
malicious-relay model, regression sweep + a 3-lens judge panel). 25 + 16 raw
findings, **21 + 14 survived** refutation, with heavy overlap.

**Companion:** `nostr_n4a_followup_plans.md` carries the per-cluster
EXECUTION plans — verified anchors, the red test to write first, ordered
steps, risks — plus the file-conflict matrix, the two-session ownership split,
and the five open decisions that must be made before the affected cluster
starts. This document says what is broken; that one says how to fix it.

This document is the execution plan for the survivors, clustered by root
cause — several findings are the same defect seen from different angles, and
fixing the cause once closes all of them. Work top-down: the order is by
"what breaks the product or its security", not by reported severity.

**Method note worth keeping:** both rounds found the CRITICAL independently
(2 lenses in round 1, 3/3 judges in round 2). Round 2's framing — "what did
the loopback path guarantee that the Nostr fork silently dropped?" — is what
made the mechanism obvious, and is the question to ask on every future
transport fork.

---

## A. 445 sender binding — CRITICAL — ✅ DONE (`63555dc`)

The defect: on loopback, `Seal`/`Genesis` arrived on the member's own reply
queue under a key only the founder held — **the channel was the proposer
authentication**, so no code ever checked it. The shared kind-445 channel has
no such property, and `open_group_frame` discarded the MLS-authenticated
author, so any welcomed seat could impersonate the founder (hijack a peer
into an attacker-governed 1-of-2 republic) or kill any peer's join with one
garbage frame.

Fixed: `open_group_frame` returns the author; `check_proposal_provenance` is
three-valued (`FromFounder` / `NotTheFounder` = ignore, never fatal /
`Refused` = the founder itself is inconsistent) and additionally binds the
link's promise (founder seated with the link's npub, m/n as advertised).
Declines and seal signatures are author-bound too.

Closes: R1 CRITICAL ×2, R1 MEDIUM (Declined ×2, forged-Genesis strand, m/n
vs link), R2 CRITICAL, R2 MEDIUM (Declined seat).

## B. The joiner's relay gate — HIGH — ✅ DONE (`9fe600f` + the diagnosis)

The gate demanded EVERY relay in the invite be locally dialable, so any node
whose pool merely *overlapped* the founder's was refused. Fixed: the join
runs over the intersection, refused only when empty.

**The operator-visible gap (reported from real use, config2 vs config3) — ✅
FIXED.** A relay hand-written into `config.toml` as `confirmed = true` but
WITHOUT `clearnet_enabled = true` is undialable, and the refusal ("no relay
in common … this node can dial [nothing (no confirmed relay on this node)]")
told the operator to confirm what they had already confirmed.

`molt_core::relay::diagnose_invite_relays` / `invite_relay_refusal` now judge
every invite relay individually against the pool — not in the pool / in the
pool but unconfirmed / confirmed but non-onion dialing off — each with the
one action that fixes THAT relay, one log line each (the run log elides per
line, so a paragraph would have reached the operator as its first clause).
"No relay in common" survives only for the case where it is true: every named
relay unknown here. Keystone
`the_relay_refusal_diagnoses_every_invite_relay_individually` drives all
three branches through `Command::JoinStart`; two `molt-core` unit tests pin
the classification and the wording.

**Decided (2026-08-01): a hand-written `confirmed = true` does NOT imply the
clearnet decision.** Confirming an entry says "use this one"; the non-onion
switch says "this node may leave Tor at all" — a property of the NODE, which
a file edit must not grant as a side effect. Recorded in
`relay_pool.md` §2.5.

**Same root, also fixed:** the surfaces still narrated the *removed*
session-only model — the GUI badge said "confirmed — needs activation this
session", the clearnet panel promised the activation "ends with the app", and
THREE MCP tool descriptions said it is "never persisted". That is the same
defect as the join message: the operator is told to repeat an act that no
longer exists, and never told about the switch that is actually off.

**Found by the review pass over this change-set, and fixed with it:**
- The refusal was *invisible* even when correct. Both run logs used
  `overflow: elide` with no horizontal scroll, so a failure's tail — the part
  that says what to do — was unrecoverable. Worse, the founding wizard has its
  OWN copy of the log block (`app.slint`) that the first fix missed. Both wrap
  now; the duplication is noted in both files as debt.
- The founding refusal reaches the GUI as a **toast**, which was a fixed 38px
  single-line bubble sized to its text: a 200+ character message rendered
  wider than the window and clipped. Long toasts now wrap inside a capped
  bubble.
- "Why can this node dial nothing" existed **three times** (`tor_probe::
  target_gap` — which had zero callers, an inline predicate in the GUI's Tor
  panel, and the new one). One `relay::pool_gap` in molt-core now; the others
  delegate. See `relay_pool.md` §3a.
- Operator-facing prose had landed in `molt-core`, naming a GUI tab and a
  config key from the no-I/O contract crate. Classification stayed in core
  (`PoolGap`, `InviteRelayBlock`, both `Serialize`); the words moved to
  `molt-engine::relay_msg`, where every other run-log line is authored.
- The join gate judged each relay **twice** (once for the dial set, once for
  the message) — two readings that could disagree, making the refusal
  contradict its own detail lines. One `diagnose_invite_relays` pass feeds
  both now.
- The new hand-edit warning in the config template shipped only on a *fresh*
  `[transport.nostr]` section — never reaching the operator who hand-edited an
  existing one. It now also fills in a section that carries no comment of its
  own (never overwriting one the operator wrote), and both pool-load paths
  `tracing::warn!` when the confirmed-but-switched-off state is loaded.

**Left as debt (not this change-set):** `tor_probe::verdict` still cannot name
the real cause for `ProxyOnly` — it has no pool to inspect, so it now claims
only what it observed; threading `TargetGap` in is the real fix. The two
run-log blocks should become one component. `TargetGap` itself is still
uncalled.

## C. The inert publish-failure seam — HIGH ×3 + MEDIUM ×3

`spawn_publish_frame_with`'s `fail` argument is **never once passed
`Some(...)`** — the reporting path I wrote is dead code. A failed `Seal`
publish therefore hangs BOTH sides forever: the founder shows "charter
proposed", every member waits for a frame that was never accepted, and the
`NetRitualFailed` sink that exists for exactly this is never reached. This is
the project's signature failure mode (a seam that exists but is not wired),
committed by me in the same change-set that documents the lesson.

Related, same cluster:
- `RitualNet::send_ritual` / `send_welcome` / `publish_frame` discard
  `PublishReport`, so landing on 1 of N relays is indistinguishable from full
  delivery — the per-relay outcomes N2 built are thrown away.
- The Genesis 445 is a single fire-and-forget publish with no ack and an
  unbounded member wait: one dropped frame strands every member while the
  founder has already materialized (the "the member's own wait surfaces it"
  claim in the N4a plan §8b is **false** — there is no such wait).

Fix: pass the failure sink for every pre-seal leg (Seal, Welcome fan-out);
surface partial-relay landings (at minimum log the report, and treat "landed
on fewer relays than configured" as a warning the founding log shows); give
the Genesis leg a real story — either an ack round or an explicit,
surfaced-to-the-user retry, decided deliberately rather than by omission.
Keystone: a founding whose Seal publish fails must reach `NetRitualFailed`,
not hang.

## D. Join-task lifecycle — ✅ DONE

A late `NetJoinSealed` from an abandoned join hijacked the session: the ONLY
gate on `cmd_net_join_sealed` is the join generation, and neither
`cmd_create_start` nor `cmd_recover_start` moved it or aborted the task — so
the report materialized a republic the user never created, re-pointed
`active_workspace` at it and flipped the screen out from under the founding
wizard. Reproduced end-to-end before the fix.

Fixed with one shared `invalidate_join()` (generation bump + close the
ratification gate + abort the task + clear the wizard + fresh transport slot);
`cmd_join_cancel` now delegates to it, so there is ONE definition of
"abandon a join".

**Two further holes of the same root cause, found by the investigation and
closed here:**
- `cmd_open_workspace` invalidated no join either — entering a workspace is a
  context switch like any other, and a late seal would materialize a SECOND
  republic beside the one just opened. (This also fires on restore-finish,
  which is intended.)
- The SYMMETRIC hole: `cmd_recover_start` never called `teardown_ritual`, so
  an in-flight FOUNDING could still seal into the recovery session via
  `maybe_finalize`. A recovery now abandons both.

Keystones `founding_invalidates_an_in_flight_join` /
`recovery_invalidates_an_in_flight_join` drive a REAL live join (two engines
over an in-process relay) and then feed the abandoned task's own
`NetJoinSealed`; both verified red-without / green-with.

## E. `GroupSub::recv` failure handling — MEDIUM ×3

On a failed window-roll resubscribe the receiver returns `None` — which every
caller reads as "idle tick" — so a node goes **permanently deaf** at a UTC
midnight boundary while looking healthy, and the caller loops without backoff
(busy-spin).

Fix: distinguish "idle" from "resubscribe failed"; retry with backoff, and
report loudly (G4) rather than returning a lie. Keystone with injected time
across a boundary plus a refusing relay.

## F. Honesty gaps — MEDIUM ×3

1. A freshly founded/joined Nostr workspace still reports `NetHealth::Ok` —
   the honest "no runtime until N5" state is only applied on REOPEN
   (`session.rs`), not at founding/join time (`lifecycles.rs:1334`). The
   green pill is a lie for the whole first session.
2. A founder-side abort (`CreateCancel`) is never told to the group, so
   members already inside the born MLS group wait forever. This is the same
   root as my own `F-SELF-1` note (the unbounded post-accept waits): the
   member has no way to learn a ritual died. Fix both together — publish an
   abort frame AND make the member's wait legible (elapsed time surfaced, so
   "still waiting" is distinguishable from "stuck").
3. A legitimate retry of the same invite link is misdiagnosed as a second
   activation and permanently refused (`founding.rs:1926`) — the same-member
   idempotency arm compares `(member, identity_pk)` but a retry after a
   transport hiccup re-derives the same identity, so this should be the
   idempotent path, not the `LinkSpent` path. Re-check the comparison.

## G. NIP-42 is inert on ritual subscriptions — MEDIUM ×2

`with_auth_keys` is never called on the ritual runtimes, so an auth-required
relay silently delivers nothing — the ritual just times out with no
explanation. Also the subscribe-before-advertise gate ignores `live()`'s
result, so links are published even when no relay accepted the REQ.

Fix: wire the anchor keys into the ritual runtimes (noting the N2 §3.5
correlation caveat: NIP-42 with the roster anchor is a correlation handle;
prefer ephemeral-per-relay when that lands), and treat a failed `live()` as a
provisioning failure instead of proceeding blind.

## H. Unpinned security checks — the inert-keystone class — HIGH ×2 + MEDIUM ×2

Round 2's most valuable output: checks that EXIST but no test would notice
losing. Deleting the check leaves the suite green.

- The §2.1 **proof-of-possession** gate on the founder's ingest is pinned by
  no test. (It has since MOVED to the actor in `63555dc`, which makes it
  state-level testable — do it.)
- The **genesis sign-what-you-see byte comparison** is unpinned on the
  shipping path (the N1 keystone covers the loopback twin only).
- The joiner's founder-identity guards on the 1059 inbox (`sender == h.npub`)
  are pinned by no test.
- The §4.4 **window-roll resubscribe** is pinned by no test — a founding
  spanning UTC midnight is untested.

Fix: one keystone per item, each driving the PRODUCTION entry point, each
verified red by deleting the check first. This is the etappe's real coverage
debt and should not be deferred again.

## I. Leftovers — ✅ DONE

The invite's untrusted-input relay cap (8) was enforced against the FOUNDER's
own pool, so an operator with >8 confirmed relays could not render a link at
all — and it was worse than reported: `spawn_founder_inbox` turned the render
refusal into a FATAL `NetRitualFailed`, so the founding aborted outright.

Fixed at the single source (`founding.rs::start_ritual`): the founder takes
the first `MAX_PAYLOAD_RELAYS` of `relay::dialable` in pool (= priority)
order, and the SAME capped list feeds the invite, the Welcome payload, the
group channel and the persisted `TransportState.relays`. Capping ONCE is
load-bearing — the joiner requires the invite's relay set and the Welcome's to
be identical, so a link-only fix would have moved the failure to group birth.
`invite.rs`/`welcome.rs` keep their build-side refusal as the fail-loud
backstop.

Truncation is never silent: the founding log names how many of the pool the
invite carries and points at the pool order. **Behaviour change worth
knowing:** the founder now also subscribes/publishes on only those first
eight — an operator whose top 8 are dead and whose 9th is live must reorder.

Keystone `a_founder_pool_over_the_link_cap_still_founds_over_its_first_eight`
drives a 9-relay founder through the whole choreography to a sealed join, so
it pins the Welcome leg too, not just the rendered link.

---

## Suggested order

1. ~~**B-open** (blocks real use today) — the diagnosing relay message.~~ ✅
2. **C** (silent hangs + the inert seam I wrote).
3. **D** (session hijack by a stale join).
4. **H** (the coverage debt — do it before more behavior lands on top).
5. **F**, **E**, **G**, **I**.

Each step: red test first over the production path, one commit, green on
master before the next.
