# Recovery: survivors approve automatically, the rejoiner sees a checklist

**Status: OPEN — plan agreed 2026-08-23 (user decision), execution in this
session.** Supersedes product decision (1) of
`docs_archive/ritual/recovery_approval_design.md` (membership proposals as
human-approved cards): for a **consented** re-admission the approval is now
automatic; the card remains as a visible record.

## 1. The field defect

A rejoiner on a m≥3 republic hangs at "Waiting for the surviving members to
approve" until timeout: survivors hold a proposal card on the Organization
surface nobody looks at during a recovery, and nothing tells them to. The
rejoiner cannot see who has approved or how many approvals are missing.
Meanwhile the human vote adds no security (§2) — the minting survivor already
made the only human decision there is.

## 2. Security argument for auto-approval

What a `Membership{Restored}` proposal proves, verifiable on EVERY node:

- The **consent** signature covers `restore_consent_bytes(republic_id, member,
  identity_pk, nostr_pk)` and verifies against the seat's ANCHORED identity
  key — only the holder of the seat's recovery phrase can produce it.
  `verify_chain`/`try_commit` already enforce it block-level, fail-closed.
- `apply_membership` hard-rejects a Restored block that changes the anchored
  `identity_pk` — a re-admission re-keys the MLS leaf, never the roster.
- The single-use ticket + seat proof gate the request at the coordinator; the
  coordinator's human MINT of the recovery link is the human decision.

So for a consented restore, a survivor's approval click attests nothing the
node cannot check itself — the checkpoint precedent applies ("correctness
attestation, not a product decision", `receive_checkpoint_proposal`). The
threshold model is untouched: the block still carries m real position-bound
signatures; a seat = its phrase (product decision 2026-08-01, agents-are-seats).
A stolen phrase defeats manual approval equally (survivors cannot distinguish
the thief), so the click was never a defense.

**Hard rule: auto-sign ONLY what this node verified itself.** A consent-less
(legacy) restore and every `Joined` proposal keep the human card — there a
human voice IS the content. Never trust the proposing coordinator.

## 3. WP1 — engine: auto-approve a verified consented restore

`crates/molt-engine/src/chain.rs`, at the end of
`receive_membership_proposal` (so every wire receipt gets it), a new
`auto_approve_restore(id)`:

1. change is `Membership{op: Restored, member, identity_pk, nostr_pk,
   consent: Some(c)}`; proposal record (if any) still `Proposed`;
2. seat is anchored and `identity_pk` equals the anchored key;
3. `nostr_pk` (when `Some`) is canonical (`molt_net::canonical_nostr_pk`
   round-trips) and **no other seat holds it**;
4. `c` verifies over `restore_consent_bytes` against the anchored key;
5. replay guard (checkpoint pattern): not already signed at head+1;
6. then `chain_sign_and_gossip_approval(id)` + one structured log line.

Coordinator side unchanged (`self_cosign` already signs). Decline stays
possible in the race window; D2 semantics untouched.

Tests (red first, `chain.rs` mod tests, harness of
`a_membership_proposal_is_a_visible_approvable_record`):
- `a_consented_restore_is_approved_without_a_human` — receipt alone puts the
  receiver's verified signature in `pending_sigs`; with the coordinator's
  gossiped sig the block seals at m=3 with no `Command::Approve`.
- `a_consentless_restore_still_waits_for_the_human`.
- `a_forged_consent_never_auto_signs` (wrong key).
- `a_restore_claiming_a_living_seats_anchor_never_auto_signs`.

E2E keystone (`tests/nostr_recovery.rs`): 3-of-3 republic, all survivors
online, petra recovers — completes with **no** `Command::Approve` anywhere.
Red today (hangs), green with WP1.

## 4. WP2 — the rejoiner's checklist (engine + wire)

The rejoiner sits OUTSIDE the group; only the coordinator can tell it where
the vote stands. New ritual frame (additive, old peers ignore it):

- `molt-net invite.rs`: `RitualMsg::RecoverProgress { member, need: u32,
  roster: Vec<String>, approved: Vec<String> }` — display data, carries no
  authority (the Welcome stays the only thing that finishes a rejoin).
- Coordinator (`molt-engine`): `recover_progress_for(id)` builds the frame
  from `proposal_changes` + `pending_sigs[id].verified` (∪ the consenting
  member) + `rule_m` + the head roster, for a Restored proposal whose member
  has a `pending_recovery` entry. Sent (a) right after
  `verify_and_propose_restore` proposes, (b) on every verified approval
  arriving for that id (`Approved` wire arm), via gift wrap to the change's
  `nostr_pk` (the `coordinator_rekey_nostr` transport pieces). Loopback path:
  not wired (test transport; engine fn is unit-tested directly).
- Rejoiner: `RitualDelivery::Msg(RecoverProgress, sender == coordinator)` in
  the `recovery_rejoin` wait loop → new INTERNAL `Command::NetRecoverProgress`
  → `SessionView.recover: RecoverState` (new, `#[serde(default)]`):
  `{ member, need: u32, seats: Vec<RecoverSeat { member, approved: bool }> }`,
  roster order; reset on `RecoverStart`, kept on Done (shows the full list).
  MCP: `NetRecoverProgress` on the documented INTERNAL list; the session
  surface carries the state co-equally.

Tests: unit — `recover_progress_for` counts consent + verified sigs, roster
order; command — `NetRecoverProgress` respects the generation guard (stale
incarnation dropped, like `NetRecoverNote`); E2E — in the WP1 keystone the
rejoiner's `session.recover` names every survivor approved before Done.

## 5. WP3 — rejoiner UI: the checklist replaces the prose

`ui/app.slint` (the `rv-running` block in the restore wizard) + `molt-ui`:
- one compact header line: `Strings.rv-approvals` = "Approvals {have} of
  {need} - members approve automatically when online." (DE: "Zustimmungen
  {have} von {need} - Mitglieder stimmen automatisch zu, sobald sie online
  sind.") — replaces the "human step / can take a while" prose;
- under it one row per seat (RitualSeat-list pattern, small): dot
  green=approved / gray=pending + name; own seat pre-checked (consent);
- `rv-note` (the minutes ticker) stays as the last quiet line.
- molt-ui: map `SessionView.recover` → `rv-have`, `rv-need`,
  `rv-seats: [RecoverSeatRow]`; clear with the existing rv reset sites.

Tests: molt-ui lib tests over the live-preview stub build (mapping,
localization, reset), per the stub-test recipe.

## 6. WP4 — "A recovery is already running" banner fixes

`app.slint` run-banner row (link step): the goto `AppButton` is top-stuck —
center it (`y: (parent.height - self.height) / 2`, the codebase's own
pattern). Label `rw_join_goto` "Go to it"/"Dorthin" → "Show"/"Anzeigen"
(used by all three banner variants; verb, compact).

## 7. WP5 — follow-up story: restore-from-backup entry in Settings › Backup

The pipeline exists (`Command::RestoreStart { way: "s3", target, secret }`,
hard-verified chain, detached open — `backup_restore_design.md` §4). What is
missing is the entry point the user expects:

- Settings › Backup table: every ORPHAN row (bucket-only workspace) gets a
  compact "Restore" button;
- click → modal asking for the recovery phrase (masked field, paste, cancel/
  restore);
- confirm → `RestoreStart { way: "s3", target: <orphan id>, secret: phrase }`
  and navigate to the Restore screen's run step (`rw-step = 1`), where
  progress/outcome already render; `RestoreFinish` opens the workspace.
- No engine change expected; molt-ui wires the callback + modal state.

Tests: molt-ui mapping test (orphan rows expose the restore affordance,
locals do not), modal state machine test; the engine pipeline is already
pinned by `tests/restore_real.rs`.

## 8. Execution order & landing

WP1 → WP2 → WP3+4 (one window build at the end) → WP5. TDD per WP; commit +
push per WP on master; `cargo clippy --all-targets` clean; final
`cargo build -j 1 -p molt-ui-window -p molt-ui`; then archive this doc with a
status flip and correct `recovery_approval_design.md`'s status note
(decision (1) superseded), `python3 scripts/check-doc-refs.py` after moves.
