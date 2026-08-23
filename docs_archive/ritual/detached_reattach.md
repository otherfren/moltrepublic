# Detached reattach: a restored seat reconnects without a fresh ritual

**Status: EXECUTED 2026-08-24 — built in the same session as the decision.**
Keystone: `nostr_recovery.rs::a_restored_workspace_reattaches_without_a_ritual`
(export → device dies → fresh device restores the blob → OPEN alone
reattaches; walter only hears the rejoin notice; verified red before the
build). Builds on `recovery_auto_approval.md` (auto-approval,
WP6/WP7, the local-copy replace at the recovery seal).

## 1. Product decision and security posture

After an S3 restore the workspace opens detached and today's only way back
into the LIVE republic is a survivor-minted recovery link. The user removed
that last manual act for the restore case: **possession of the seat's seed
(phrase) alone re-attaches the seat automatically once m survivors are
online.** The survivors' nodes verify everything cryptographically (the
existing consent + seat-proof + threshold machinery); the humans are only
NOTIFIED (the existing "🔑 X rejoined" system line).

Explicitly accepted: a stolen phrase now re-enters without any survivor's
manual act. That is consistent with the standing model — the phrase always
WAS the seat (it derives identity, storage keys, backups); the ticket-mint
gated only the request channel, not the key material. The **ticketed
recovery link stays** as the fallback for total loss (no local backup) and
as the manual override.

## 2. The three new pieces

### 2.1 A standing seat inbox (survivor side)

An open chain-governed Nostr workspace subscribes its OWN kind-1059 inbox
(`RitualNet::inbox` on the seat's transport anchor, over the dialable group
relays) for the workspace's lifetime — torn down with the net teardown,
scoped by `net_scope` like every recovery loop. It feeds `RecoverRequest`
frames into the existing `NetRecoverRequested` ingest. (Until now the 1059
inbox existed only while a mint was outstanding.)

### 2.2 The unsolicited request (ingest gate)

`cmd_net_recover_requested` today drops any unknown ticket silently. New
second lane, fail-closed at every rung: a request whose ticket is unknown
is treated as SELF-SERVICE iff

1. this node holds the open group of that republic (it must be able to
   coordinate);
2. the request carries a **consent** (mandatory here — it is the
   authorization) and the full existing ladder passes: canonical,
   collision-free anchor; PoP (`sender_npub == claimed anchor`); seat proof
   and consent against the ANCHORED identity key
   (`verify_and_propose_restore`, unchanged);
3. no Restored proposal for this member is already pending (first receiver
   coordinates; later receivers of the same broadcast skip — their vote
   arrives through the normal auto-approval of the gossiped proposal);
4. a cooldown map `(member, new_anchor) → stamp` (30 min) swallows relay
   replays of the same request after the seal (the accept-window does not
   cover 1059 wraps).

A FAILED unsolicited request is dropped **silently** — no `RecoverRefused`
answer (that frame stays gated behind a live ticket; an unauthenticated
prober must not get an oracle). Two survivors racing the same broadcast can
still double-propose; the concurrent-commit rule and the double-recovery
convergence keystone already cover that (accepted churn, not divergence).

The self-service ticket in the request is the REJOINER's own random salt
(the founder's self-ticket pattern) — it anchors the fresh transport key
derivation and is never registered anywhere.

### 2.3 The reattach task (rejoiner side)

Opening a workspace that lands in the honest DETACHED state (verified
chain, no live crypto) with a resolvable identity and a ratified relay
pool spawns the reattach task instead of leaving a dead end:

- material from disk: seed entropy (sealed beside the keys) → the phrase
  reconstructs via bip39; the identity resolves against the seat's OWN
  anchored pk (`seat_identity` — founder seats included, WP7);
- self-ticket → fresh anchor `nostr_identity(entropy, self_ticket)`,
  fresh KeyPackage, seat proof + consent exactly like the ticketed path;
- the request goes to EVERY other seat's WORKING transport anchor (the
  restored chain's fold — possibly stale; whoever still holds their anchor
  answers), over ratified ∩ locally-confirmed relays (ADR-0004);
- then the ticketed rejoiner's own wait: Welcome (accepted from ANY roster
  anchor, not one coordinator) → group join → chain anchor → the existing
  `NetRecoverSealed`, whose local-copy replace retires the detached dir to
  the trash and materializes the live state.

Status rides the existing recovery notices (`recover-note:` /
`recover-failed:`); the detached toast becomes the one-line "reconnecting"
state. If no survivor answers within the bounded wait the workspace simply
stays detached (readable), with the honest note — and the ticketed link
remains the manual path.

## 3. What is deliberately NOT built

- No new command surface for agents: the reattach is engine-internal
  (spawned on open); MCP sees the same session states as the GUI.
- No auto-retry loop: one attempt per OPEN of the detached workspace
  (reopening retries). A background retry ticker can come later if the
  field wants it.
- No coordinator election beyond first-receiver + pending-dedup (see 2.2).

## 4. Test plan (red first)

1. Ingest unit: an unknown-ticket request WITH valid consent proposes
   (self-service); without consent it stays a silent drop; the cooldown
   swallows an immediate replay; a pending Restored for the member skips.
2. E2E keystone (`nostr_recovery.rs`): found 2-of-2 → petra's device dies →
   petra restores from the S3 blob on a fresh node (the real restore
   pipeline) → OPENING the detached workspace reattaches with **no
   RecoverInviteStart anywhere** → both ends converge (chat both ways).
3. Probe silence: an unsolicited request with a broken consent leaves no
   notice and no refusal frame (the ticket path's WP6 answer stays
   ticket-gated).

## 5. Landing

Engine + net changes, one window build not needed (no .slint change beyond
the toast string), full engine suite + clippy green, archive this doc with
the round's plan on completion.
