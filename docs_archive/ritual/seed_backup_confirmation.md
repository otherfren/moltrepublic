# Seed-backup confirmation round (founding ritual)

> **ARCHIVED 2026-08-15 — executed.** The shipping ritual specification
> (including this round, ❻½) lives in `docs_archive/ritual/founding_ritual.md`;
> this document is the design + decisions record.

**Status: BUILT 2026-08-15** (engine round ❻½ + both wizards + MCP tool
`confirm_seed_backup`; keystones in `molt-engine/src/lib.rs`
`founding_waits_for_the_backup_confirmation_before_writing` and the
two-instances gate test). Ask of
2026-08-15: the founding wizard needs a step that **waits until every member
has backed up their recovery phrase and confirmed it**; only then is the
ritual complete and the workspace written to disk. Without that last step
the ritual FAILS and nothing is created anywhere. This document is the
execution plan; the §7 forks were discussed and decided with the user on
2026-08-15.

Read `founding_ritual.md` first — phase numbers below refer to its §4.

## 1. What changes, in one sentence

A new **confirmation round between ratify (❺–❻) and finalize (❼)**: after
every seat has ratified the charter, each participant (founder included)
proves locally that its recovery phrase is backed up and sends a
`BackupConfirmed` attestation; the founder gates ❼ — the FIRST disk write of
the whole ritual — on **n-of-n** confirmations.

Today the re-type proof (`SeedConfirmStep`) exists but runs **after** the
seal, locally, per wizard — the workspace already exists when it appears. A
member who closes the window there holds a seat whose only recovery path was
never saved. Moving the proof before the seal makes "everyone can come back"
a founding invariant, like "everyone ratified".

## 2. Protocol change

New phase between ❻ and ❼ (`F` = founder, `Mᵢ` = member):

```
  ❻½ CONFIRM BACKUP (all n seats, founder included)
                                                    Mᵢ: re-type own phrase
                                                        (local proof, arms send)
                          ◀── BackupConfirmed{ attᵢ } on Qinv ──
  F: verify attᵢ against anchored pkᵢ
     own re-type proof arms own confirmation
     …wait until every seat (incl. self) confirmed…
  → only then run ❼ (write own genesis, distribute Genesis{sealed, welcome})
```

- `attᵢ = sign(skᵢ, "molt-backup-confirmed-v1" ‖ sha256(T))` — signed with
  the identity key over the ratified table's hash, so a confirmation is
  self-authenticating (like `Signed{sig}`), bound to exactly this ritual,
  and cannot be forged by someone who merely knows the queue. It proves a
  second deliberate human act, not key possession (ratify already proved
  that). It cannot prove the phrase is truly on paper — that stays the
  member's honest act, same as today's local re-type.
- Idempotent per seat like the ratify handler: a duplicate confirmation is
  ignored.
- Wire vocabulary (§10) gains one row: `BackupConfirmed{ att }`,
  member → founder, on `Qinv`.

## 3. State machines

- **Founder** (`founding.rs::start_ritual` runtime): new per-seat flag
  `backup_confirmed`, new wait state after all-ratified; ❼ fires only at
  n-of-n (founder's own flag set by its local re-type). Cancel in this state
  behaves exactly like every earlier cancel: void links, tear down, no disk.
- **Member** (`run_ritual_member`): after sending `Signed{sig}`, the member
  does NOT go straight to "wait for Genesis": it enters `confirm-backup`,
  sends `BackupConfirmed` only once the local re-type matches, then waits
  for `Genesis`. A `Genesis` arriving BEFORE the own confirmation was sent
  is a protocol violation by the founder (it sealed without us) — treat as
  ritual failure (defensive; an honest founder cannot reach ❼ early).
- **Ephemerality unchanged and strengthened**: the first disk write of the
  ritual (founder's own genesis at ❼) now sits behind the last human act of
  every participant. Crash/cancel anywhere before that leaves no trace on
  any machine — exactly the ask's failure rule.

## 4. UI (both wizards)

- **Create wizard**: today's post-seal `SeedConfirmStep` (cw-outcome 1)
  moves BEFORE finalize: phrase + re-type, then a live seat list "backup
  confirmed n/m" (the anchor/ratify list idiom); the workspace opens only
  when the ritual completes. Cancel keeps its current semantics.
- **Join wizard**: same step after ratifying (`jw-sealed` today): re-type,
  send, then honest waiting ("waiting for the others to confirm"), then the
  existing Genesis verification and entry.
- Copy stays compact: the existing `cw_seed_confirm_*` strings carry over;
  one new waiting line and one per-seat state, no prose walls.

## 5. Command surface (co-equality)

Confirming the backup is a HUMAN decision → a `Command` that is an MCP tool
AND a GUI action (`co_equality_every_command_is_a_tool_or_documented_internal`
demands one of the lists — this one is a tool on both surfaces, like
approve/confirm). The founder-side ingest of a member's `BackupConfirmed` is
ritual plumbing → INTERNAL, like the other `Net*`/ritual variants. The MCP
tool takes the re-typed phrase (the engine matches, refuses on mismatch) so
an agent-driven seat has the same proof obligation as a human.

## 6. Tests (TDD keystones, red first)

1. Founder does not distribute `Genesis` while any seat is unconfirmed
   (loopback two_instances-style; the member that never confirms pins the
   founder in the wait state, nothing on disk on either side).
2. Cancel during the confirm round leaves both disks untouched.
3. A forged/unsigned `BackupConfirmed` (wrong key, wrong T-hash) is ignored
   — the seat stays unconfirmed.
4. Duplicate confirmation is idempotent.
5. A `Genesis` before the own confirmation aborts the member ritual.
6. The full happy path stays green (`two_instances.rs` extended: both sides
   confirm, then seal, workspace opens).

## 7. Decided (with the user, 2026-08-15)

1. **No auto-timeout.** The confirm round waits honestly like the rest of
   the founding; the founder sees per-seat confirm state and can cancel,
   every member can cancel too, and nothing is on disk before the seal.
   (The recovery ritual's 15-min welcome timeout guards an unattended
   machine — a different situation; no analogy here.)
2. **Strict order: ratify, then confirm.** The attestation signs the
   ratified table's hash, so a `BackupConfirmed` from a seat that has not
   ratified means nothing. The MEMBER always sends Signed before
   BackupConfirmed; the human cannot confirm before ratifying.
   Implementation nuance (found by the two-instances suite): the
   transports do not order separate messages (the loopback hub reorders
   under load, relays reorder 445s), so an attestation that OUTRAN its
   own seat's seal signature on the wire parks in the seat's one bounded
   slot and applies when the seal signature lands (the parked-decline
   idiom). A seat that never ratifies never applies it — the semantic
   rule is untouched; only honest wire reorder is tolerated.
3. **n-of-n includes the founder.** The founder's own re-type gates the
   seal exactly like every other seat's; sealing is NOT an implicit
   confirmation.
4. **Mixed-version founding: incompatibility accepted.** An old client
   never sends `BackupConfirmed`; its seat stays visibly unconfirmed and
   the founder cancels. No invite-format/version change — the ritual is
   ephemeral and both sides ship in one binary.
