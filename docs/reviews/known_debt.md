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
questions that gate any real build. Shared Files (2026-08-28) is the
fifth mock: its real build waits on `docs/storage/s3_buckets.md` §7, and
the engine refuses proposals on it until a vote view exists.

## Charter features are a hand-rolled column per feature

Every feature key is wired by hand at ~19 sites (wizard grid, both
`CharterView`s, the Organization list, the enable modal's arming
expression + payload string, `mirror.rs` setters) — the `files` rollout
(2026-08-28) was the fifth copy. Fix direction: one `FeatureRow
{key,label,checked,enabled}` model built in Rust from
`Surface::ALL.filter(is_charter_feature)`, rendered by ONE component at
all five sites.



## Review 2026-08-25 — the deferred findings

`docs/reviews/code_review_2026-08-25.md` holds the full review (every crate,
eight passes); its CRITICAL/HIGH items were fixed the same night. The items
still OPEN there, by id (each carries its fix direction in the review):

- Chain: C3 residual (a non-logged direct serve).
- Engine: E3 residual (insider system line).
- Ritual/recovery: R4 (design: a survivor capturing a reattaching seat) ·
  R9 residual (`nostr_sk` in the two `Net*Sealed` commands).
- MLS/delivery: M1 residual (`ChainOracle` allowance design) · M2 open half
  (chain-backed commits outrank at the tiebreak, design).
- Transport: T10 residual (test-only cursor API).
- Storage: S1 residual (`openat2` beneath the workspace).
- Core: K4 residual (a local `--reveal-seed` for headless nodes).
- Frontends: F7 residual (token read per accepted connection).
- MCP privileges (section 9): P8 ritual abandon on context switch
  (product) · P10 send-side rate limits.
