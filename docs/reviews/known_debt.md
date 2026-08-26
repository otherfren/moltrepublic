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



## Review 2026-08-25 — the deferred findings

`docs/reviews/code_review_2026-08-25.md` holds the full review (every crate,
eight passes); its CRITICAL/HIGH items were fixed the same night. The items
still OPEN there, by id (each carries its fix direction in the review):

- Chain: C3 `ChainRequest` amplifier · C4 bucket-wide quota prunes other
  nodes' backups · C5-C8 LOW · C9 style · C10 `chain.rs` split.
- Engine: E3 residual (insider system line) · E4 `acquire_lock` PID
  heuristic → `flock` · E5-E6 LOW · E7 style · E8 `lib.rs`/`net.rs` split.
- Ritual/recovery: R1 open half (write-before-verify in
  `materialize_workspace`) · R2 founder accepts attestations before the
  table is frozen · R3 ticketed-lane re-admission gate · R4-R9 LOW · R10
  style (with `known_log_shapes()`) · R11 duplicated member ladder.
- MLS/delivery: M1 residual (`ChainOracle` unwired) · M2 open half
  (reverse-order evicted commit → chain-backed commits outrank) · M3 prior
  slot not persisted · M5 hold accounting · M7 `TransportState` write race
  · M8 §7 commit resends on Nostr · M9-M10 LOW · M11 style · M12 refactor.
- Transport: T2 `PublishPool` breaker · T4-T8 LOW · T9 style · T10 refactor.
- Storage: S2 segment resurrection · S3 quadratic torn-tail scan · S4
  version floor · S5 unbounded segment numbers · S6-S9 LOW · S10 style ·
  S11 refactor.
- Core: K2-K3 LOW · K4 residuals (S3 secret / token in `read_session`,
  product call; a local `--reveal-seed` for headless nodes) · K5 style and
  stale contract docs · K6 refactor.
- Frontends: F2 `ui_action` verbs · F3 wake-command save paths · F4 open
  half (`molt_config::write` dead) · F6-F9 LOW · F10 style · F11 `molt-ui`
  split.
