# docs_archive — everything that is not open work

Nothing here describes open work — that is the whole rule (since 2026-08-16):
`docs/` holds ONLY unfinished plans; this directory holds everything else.
That is two distinct kinds of document, told apart by the status line at the
top of each:

- **Current specifications of shipping behaviour** — the "read first"
  authorities CLAUDE.md points at (founding ritual, persistent chain, chat
  bus, delivery guarantee, Nostr transport, relay pool, MCP security,
  reproducible builds) and the ADRs. These are LIVE: consult them as the
  authority for how the shipped thing works, and keep them current when the
  behaviour changes.
- **Historical records** — superseded designs, executed plans, and analysis a
  decision consumed. Kept for the REASONING: what was tried, what was
  rejected, and what the tradeoff was. Deleting them would leave the code
  with no answer to "why is it like this".

## Why each of these left `docs/`

**Superseded by the Nostr transport (etappe N-demo, 2026-07-30).** The SMP
transport and its mesh machinery were deleted; these documents describe
machinery that no longer exists. They record why the mesh was built the way it
was, and — in `mesh_reliability.md` — the live measurement that eventually
justified abandoning SMP entirely.

- `transport/concept-transport-simplex-tor.md`
- `transport/dynamic_mesh.md`
- `transport/mesh/mesh_probe.md`
- `transport/mesh/mesh_redundancy_stage2.md`
- `transport/mesh/mesh_reliability.md`
- `transport/mesh/mesh_rotation_trackc.md`
- `transport/mesh/mesh_selfheal.md`
- `transport/mesh/mesh_verify_at_open.md`
- `transport/mesh/stage_b.md`
- `security/haertung.md`

**Executed plans.** The work landed; the code and its tests are now the
authority. Kept for the design reasoning, not as instructions.

- `transport/tor_transport_implementation.md` — T4; the dialer is
  `crates/molt-net/src/dial.rs`
- `transport/nostr_n05_engine_inventory.md` — the pre-N1 engine inventory,
  consumed by the N-etappen it sized
- `chat/chat_read_receipts.md` — built 2026-07-19
- `build/concept-config-bidirection.md` — C1–C3 built

**Analysis consumed by a decision.** The verdict lives on in the follow-up
plan; this is the argument that produced it.

- `reviews/buzz_comparison.md` → decided in `docs_archive/reviews/buzz_followups.md`

**Moved 2026-08-16 (the "docs/ = open work only" cut).** Everything without
an unbuilt remainder left `docs/` in one sweep. The specifications among
them stay LIVE (see the top of this file); the executed plans are records.

- Live specifications: `adr/0001`–`0006`, `build/reproducible-builds.md`,
  `chain/persistent_chain.md`, `chat/chat_bus.md`,
  `ritual/founding_ritual.md`, `ritual/recovery_ritual.md`,
  `security/mcp-security.md`, `transport/delivery_guarantee.md`,
  `transport/nostr_transport_marmot.md`, `transport/relay_pool.md`
- Executed plans/designs: `chain/log_compaction.md` (WP4a+WP4b built;
  leftovers are declared v1 limits), `memory/shared_memory_real.md` (built
  2026-08-15), `ritual/charter_features.md` (built 2026-08-12),
  `ritual/recovery_approval_design.md` (built 2026-08-08),
  `storage/backup_restore_design.md` (stories 9/10/12/13 shipped)

## Rules

- **Do not implement NEW work from a historical record in here.** If
  something in a superseded/executed doc still looks undone, it was either
  dropped deliberately or superseded — check `docs/` before acting on it.
  The live specifications are the exception: they ARE the authority for how
  shipped behaviour works.
- **Status lines were corrected on archiving**, so a historical record does
  not claim to be a plan. Several said "not yet built" for work that had
  shipped months earlier.
- Cross-references into `docs_archive/` are written with the full path, and a
  reference checker keeps them honest — see the commit that created this
  directory.
