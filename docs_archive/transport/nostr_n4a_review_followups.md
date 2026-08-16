# N4a review follow-ups — the fix plan

Status: **EXECUTED (2026-08-16) — archive material.** Every cluster ✅; the section-B debts landed 2026-08-16 (TargetGap threaded into the Tor verdict; the duplicated run-log blocks were unified by the wizard rework; `TargetGap` has real callers now). Two independent adversarial review passes ran over
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

**Debt CLOSED (2026-08-16):** `TargetGap` is threaded through the probe
into the verdict (four named causes, pairwise-distinct, pinned); the two
run-log blocks became one `RitualLog` component with the wizard rework.

## C. The inert publish-failure seam — ✅ DONE

`spawn_publish_frame_with`'s `fail` argument had ZERO `Some(...)` callers, so
the reporting path was dead code and a `Seal` no relay accepted hung BOTH
sides: the founder sat on "charter proposed" while every member waited for a
frame that was never accepted, and the `NetRitualFailed` sink that exists for
exactly this was unreachable. Reproduced before the fix — the red run times
out waiting for the founding to fail, which is the bug in one line.

**Fixed by deleting the optional seam, not by wiring it.** One
`spawn_publish_frame(chan, payload, what, retry, tx, generation)` with a
NON-optional sink: there is no longer an `Option` a future call site can
forget to pass. It encrypts ONCE and retries only the publish (a re-encrypt
would advance the ratchet past the snapshot `finalize_founding` takes, and
every member would meet `SecretReuseError` on reopen), and it ALWAYS reports
via the new engine-internal `Command::NetRitualPublished` — success, partial
and total failure alike.

`RitualNet::publish` / `send_ritual` / `send_welcome` / `publish_frame` now
return the `PublishReport` they were discarding, so "landed on 1 of 5 relays"
is no longer indistinguishable from full delivery.

**Four outcomes, three of them previously invisible:** nothing accepted on a
pre-seal leg → the founding fails honestly; nothing accepted on the genesis →
the founder HAS materialized, so the run is not failed, but
`genesis-undelivered:` is surfaced (and toasted) because the members were
never told; partial → a ⚠ line naming who refused; clean → debug only.

**Two structural traps the fix had to respect.** The genesis report must NOT
be generation-gated — `maybe_finalize` already `take()`n the ritual, so a
gated report would vanish and recreate the exact inertness being removed; and
`cmd_net_ritual_failed` early-returns once the run has an outcome, so the
genesis could not reuse that sink. A `seal_published` once-guard was also
needed: `maybe_seal` is reachable from two call sites, and a second Seal would
double-report and advance the ratchet after the snapshot.

**Two sub-findings RETIRED as misdiagnosed:** the Welcome fan-out was never
inert (it already routed a failure into `NetRitualFailed` by hand), and the
member's own `Signed` publish already failed loudly. Both only lacked the
partial-landing report. The claim that "the member's own wait surfaces a
relays-down condition" was FALSE — that wait is unbounded — and is struck
here and in the two code comments that carried it.

Keystones: `a_seal_that_no_relay_accepts_fails_the_founding_instead_of_hanging`
(two real engines against a relay whose write policy refuses kind-445, so the
failure is a real wire outcome, not an injected one; verified red-without) and
`publish_frame_reports_the_relay_that_refused` in molt-net.

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

## E. `GroupSub::recv` failure handling — ✅ DONE

On a failed window-roll resubscribe `recv` returned `None` — which every
caller reads as "idle tick". A node therefore went **permanently deaf at a UTC
midnight boundary while looking perfectly healthy**, and because the caller
loops straight back it burned CPU doing it.

`GroupRecv { Frame, Idle, Deaf(reason) }` separates the honest quiet from the
lie. The re-placement is now backoff-gated (1 s, ×2, cap 30 s) instead of
retried on every caller iteration, and while deaf the stale subscription keeps
being read — inside the ±1 h skew margin the previous window's tag is still
legitimate traffic, so `Deaf` and a delivered `Frame` can interleave. Deaf is
advisory, never terminal.

**Decided: loud forever, never fatal.** `CreatePropose` is one-shot, so a
founding aborted on a transient relay blip would lose every collected
signature and force a re-mint. (This is the same question cluster G answers
the other way at PLACEMENT time, where nothing has started yet and refusing is
cheap — the two rules are deliberate, not inconsistent.)

Two engine-internal commands carry it to the surfaces, `NetRitualNote` and
`NetJoinNote`, both deduped against the last line so a note repeating every
poll cannot stack, and neither ever sets `outcome = 2`. A `✓ the group channel
is back` line closes the spell.

**The anti-inertness guard is the point.** The compiler forces every caller to
handle the new variant, but it cannot stop `Deaf(_) => continue` — which would
restore the exact silence. Verified: making that edit compiles cleanly and the
engine keystone fails on it.

Keystones: `nostr_window_roll.rs` in BOTH crates (own test binaries — the
window-clock seam is process-global). The molt-net one pins all three
properties against a cuttable proxy (deaf not idle · ≤12 connection attempts
in 6 s · heals); the molt-engine one drives a real founding+join and pins that
both wizards SEE it, that neither run dies, and that the founding still
completes afterwards.

## F. Honesty gaps — ✅ DONE (one sub-item deferred, named below)

**F1 — the green pill that lied for a whole session.** `net_health` was
written on the OPEN path only, so a freshly founded/joined Nostr workspace
kept the serde default `Ok` and promised a runtime that does not exist until
N5; only a REOPEN was honest. Now set in the shared `materialize_workspace`,
from ONE shared literal (`session::NOSTR_RUNTIME_PENDING`) so the
first-session and reopen answers cannot drift into two different promises.
Pinned inside the main keystone, for both engines.

**F2 — a founding could die in silence.** `cmd_create_cancel` only tore the
ritual down locally; every member sat in an unbounded `loop { recv() }` with
no deadline, unable to tell a dead founding from a slow one, forever. New
additive wire variant `RitualMsg::Aborted { reason }`, and `abandon_ritual`
tells the members on BOTH paths — a gift-wrap per anchored seat before group
birth, a 445 group frame after — because a member listens on exactly one of
them depending on how far the ritual got. Routed in at all abandon sites
(cancel, ritual-failed, a new founding, a join).

SECURITY: the 445 abort arm is gated on the MLS-authenticated author
(`frame_is_from_founder`, shared with the Seal arm). Ungated it would be a
one-frame kill switch any welcomed seat could pull on every other seat — the
impersonation class fixed as CRITICAL in `63555dc`, re-entering by a new door.

**F3 — the backlog's diagnosis was WRONG, and the real defect was worse.** It
claimed a retry "re-derives the same identity" and that the `(member,
identity_pk)` comparison was the bug. It is not: `cmd_join_start` mints a
FRESH seed phrase on every start, so a retry genuinely presents a different
identity and the comparison is correct. The actual defect was that no
re-activation path existed at all — so any transport hiccup burned the seat to
a dead identity and wedged the founding permanently.

Now a same-handle retry BEFORE group birth re-anchors: the seat is cleared and
the request falls THROUGH the full ingest ladder (PoP → MAC → canonical anchor
→ cross-seat uniqueness → KeyPackage binding), never fast-pathed, and the
DISPLACED anchor gets the `LinkSpent`. After birth it is refused with the true
reason (the group formed around the first activation — cancel and re-mint),
not the misleading "ask for your own link". An unverifiable re-activation is
now logged instead of silently ignored.

Keystones, all verified red-without: the first-session health assert;
`a_founder_cancel_reaches_the_members_inside_the_born_group`;
`a_retry_of_the_same_link_by_the_same_joiner_keeps_the_seat`; and the unit pin
`only_the_founder_can_abort_a_founding`. The pre-existing carol test (a
DIFFERENT handle stays refused) is the guard on F3's blast radius and stays
green.

**Deferred, deliberately:** the elapsed-wait surfacing (`waiting_since` on
`JoinState` + a rewritten "still waiting — N min" line on the presence tick).
The abort frame closes the case that mattered — a member could not tell a DEAD
founding from a slow one. What remains is only making a genuinely slow one
legible, and it needs the `clock_override` seam and an in-crate harness. Worth
doing; not worth holding this change-set for.

## G. NIP-42 inert on ritual subscriptions — ✅ DONE

`with_auth_keys` had ZERO production callers, so every ritual subscription was
built unauthenticated. Against an auth-required relay the supervisor drops the
challenge and keeps a live, SILENT session — no EOSE, no events — and the
ritual simply times out with no error anywhere.

**Two identities, decided deliberately (2026-08-01):**
- the **1059 inbox authenticates with the roster anchor**. Its filter is
  `#p = our anchor`, so the relay already learns that key from the REQ —
  authenticating with the same key discloses nothing the subscription did not.
- the **445 group channel uses a FRESH ephemeral key per placement** (and per
  window-roll re-placement). That filter names only an h tag, so it is
  anonymous; the anchor would hand every relay operator the anchor→group-id
  link for the life of the republic, and it would survive into the N5 runtime
  subscriptions. The cost — a relay that WHITELISTS known pubkeys refuses us —
  fails loudly and visibly, which is the right trade against a silent,
  irreversible deanonymization.
- the **publish path stays unauthenticated** (§7.5): an authed publish channel
  links every ephemeral-key event to the member behind it. Guarded by a test
  that goes red if `with_auth_keys` is ever added to it.

**The second half — proceeding blind.** `subscribe()` succeeds as soon as a
relay accepts the REQ; it says nothing about whether that relay will ever
REPLAY. Three `let _ = live(...)` sites discarded exactly that answer, so
subscribe-before-advertise degraded to advertise-blind, and the founder's own
445 recv had no gate at all. New `SyncState { synced, connected }` carries the
counts, and the **≥1 rule** applies: `synced == 0` is a provisioning failure
(refuse, name the unreadable subscription), `0 < synced < connected` is a
warning — failing on any unsynced relay would let one lagging relay in a
healthy pool kill every founding.

One correction to the backlog's wording: a HARD subscribe failure was never
ignored (`subscribe()` returns Err and both call sites reported it). The gap
was the relay that ACCEPTS and never becomes readable.

Keystones: `ritual_endpoints_sync_and_deliver_on_an_auth_required_relay` and
`the_publish_path_refuses_to_authenticate` (molt-net, fast), plus
`a_founding_refuses_when_its_inbox_never_becomes_readable` driven through
`Command::CreateStart` against a relay whose query policy refuses every REQ —
verified red-without, and it additionally pins that NO seat link is advertised
over an inbox nothing replayed.

## H. Unpinned security checks — PARTIAL (headline ✅, two items open)

The inert-keystone class: checks that EXIST but no test would notice losing.

**✅ The headline finding, which was not even in the original list.** While
verifying H, the investigation found `SealedRoster.roster` is a
CONSTITUTIONAL field covered by no signature — see `1defd69`. Fixed with a
cross-check in both `verify_sealed_roster` and `verify_seal_proposal`.

**✅ H2, and it was worse than reported.** The keystone that supposedly pinned
the genesis sign-what-you-see byte comparison
(`a_sealed_roster_differing_from_the_ratified_proposal_is_rejected`) was
SEMI-INERT: its evil-identity swap trips `verify_seal_proposal`'s "does not
anchor our own (name, key)" check one gate EARLIER, and the test only asserted
`is_err()` — so the byte comparison could be deleted and it stayed green. It
was pinning a different gate than its name claims.

Repaired by swapping the CHARTER instead: the republic id does not commit to
the agenda, the member's three anchors are untouched, and the roster still
matches the identities, so every other gate on that path passes and only the
ratified-bytes comparison can fire. The assertion is now coupled to the
reason, not just to failure. **Verified: deleting the check now turns it
red**, where before it stayed green.

**✅ H4 — the window-roll resubscribe** is pinned by cluster E's
`nostr_window_roll.rs` in both crates (that work needed the same test seam).

**✅ H1 and H3 — DONE (2026-08-04).** The harness this cluster was waiting on
is `crates/molt-engine/tests/nostr_ritual_adversarial.rs`: a hostile
counterparty built only from public API (a `RitualNet` under a key of the
test's choosing) against a REAL engine driven through the Command surface.

- **H1** — `a_request_claiming_a_transport_key_it_did_not_sign_with_is_refused`.
  The attacker mints a genuine v2 MAC over the VICTIM's anchor (the ticket is
  printed in the link, so the MAC proves nothing about possession) and seals
  the wrap under its own key. Asserts the refusal line, the unanchored seat
  and `!can_propose`, then a CONTROL request with its own anchor to prove the
  refusal did not spend the ticket. Verified red with the `if is_nostr` block
  deleted: the impersonation anchors the seat and `can_propose` flips true.
- **H3** — `a_1059_frame_from_anyone_but_the_link_founder_cannot_kill_a_join`.
  An unrelated key gift-wraps `LinkSpent` and a `WelcomePayload` carrying the
  invite's exact relay list with unusable MLS bytes, continuously from BEFORE
  the founder accepts. Verified red with either guard pair deleted.

**A timing finding worth keeping.** The first version of H3 shot only after
the genuine acceptance and stayed GREEN with the Welcome guards deleted — the
honest Welcome had already been consumed, so the garbage never raced anything.
Shooting from before the acceptance is what makes both pairs load-bearing.
Same inert-keystone class as H2, found the same way: by running the deletion
experiment instead of trusting a green test.

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
