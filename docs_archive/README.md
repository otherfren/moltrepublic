# docs_archive — the record of what was decided, and why

Nothing here describes open work. These documents are kept because they hold
the REASONING behind decisions that shaped the code: what was tried, what was
rejected, and what the tradeoff was. Deleting them would leave the code with
no answer to "why is it like this".

`docs/` is the opposite: everything there either describes open work or is the
current specification of shipping behaviour.

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

- `reviews/buzz_comparison.md` → decided in `docs/reviews/buzz_followups.md`

## Rules

- **Do not implement from a document in here.** If something in an archived
  doc still looks undone, it was either dropped deliberately or superseded —
  check `docs/` before acting on it.
- **Status lines were corrected on archiving**, so a historical record does
  not claim to be a plan. Several said "not yet built" for work that had
  shipped months earlier.
- Cross-references into `docs_archive/` are written with the full path, and a
  reference checker keeps them honest — see the commit that created this
  directory.
