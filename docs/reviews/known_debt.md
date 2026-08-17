# Known debt — the living list (started 2026-08-16)

Status: **OPEN WORK.** The surviving deferred items whose home documents
were executed and archived. One entry per item, with its fix direction;
an item leaves in the change that closes it.

## H3 second half — the governance broadcast does not wait for durable persist

From `docs_archive/reviews/total_review.md` (H3, fixed 2026-08-07 except
this half). A threshold-signed block is broadcast independently of the
persist outcome; re-ordering Append → Persist → Broadcast in `chain.rs`
is a state-model change and needs its own session with the chain design
beside it. The failure is loud today (storage-failed notice), not
silent.

## Story 14 remainder — real backends for Kanban and Vault

From `docs_archive/ui/mock_todo.md` §14. Memory is REAL
(`docs_archive/memory/shared_memory_real.md`); the Wallet is ratified
and in development (`docs/chain/wallet_treasury_design.md`, next stop
the dep-lock spike); Kanban and Vault have fresh design-mock rounds and
their concept docs (`docs/kanban/kanban_workflows.md` §2–§5+§7,
`docs/vault/vault_threshold_disclosure.md`) — both docs carry open
questions that gate any real build.

## Vault mock quality follow-ups (KANN list, review 2026-08-16)

From the vault round's handover: state as a Slint enum instead of
strings; one `done` counter instead of `signed`+`resealed`; derived mark
glyph; extract CardHead/InsetWell (copied 2–3×); TagChip wrapper via
ternary; `vt_reveal`/`vt_hide` duplicates `set_token_show/hide` — name
neutrally and share; align the reveal UX with the established peek idiom
(no auto-remask).

## Auto-checkpoint gate can still be pinned inside the buffer window

`maybe_auto_checkpoint` refuses while any future block is buffered; L3's
window cap bounds the freeze to head+4096 heights, but an insider
claiming a plausible near-future height still delays compaction until
the drain or a re-serve clears it. Refine the gate to "a buffered block
ADJACENT to head" if it ever bites.

## Window-build spike: slint-build `experimental-module-builds`

The ~400k-line generated module is one compilation unit because
slint-build compiles ONE root — but slint-build 1.17 ships an
experimental `as_library()`/`rust_module()` pair ("components and types
accessible from other modules"). A spike could split the design-mock
panes (`surfaces.slint`, the heaviest repeaters) into their own Slint
library crate so their codegen is paid only when THEY change.
Experimental upstream — pin the behavior before relying on it. Cheap
companion win either way: dedup the mock components (the vault KANN
list) — codegen scales with element count.

## Field verification of the 2026-08-15/16 fixes

`docs/transport/live_incident_2026-08-09.md` — the live three-node rerun
with a fresh binary is the one item that needs the user's hardware.
