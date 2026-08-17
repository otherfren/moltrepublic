# Known debt — the living list (started 2026-08-16)

Status: **OPEN WORK.** The surviving deferred items whose home documents
were executed and archived. One entry per item, with its fix direction;
an item leaves in the change that closes it.

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

