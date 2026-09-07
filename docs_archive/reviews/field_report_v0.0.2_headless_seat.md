# Field report v0.0.2: a headless agent seat over MCP (2026-09-07)

Status: EXECUTED 2026-09-08 - the verdicts below are on master; F5's
allowlist is in `docs/reviews/known_debt.md`. Source: an LLM agent that
installed release v0.0.2 as a service (Debian trixie, `headless = true`,
MCP over TCP, one onion relay, `network = "tor"`), ran the transport
tests and joined a 2-of-3 republic end to end, GUI-free.

| # | Reported | Verdict |
|---|---|---|
| F1 | `tor.mode = "embedded"` aborts the process on the first dial (rustls has no process-level provider) | Already fixed in v0.0.3 (`43782d6e`): `ArtiShared::new` installs the rustcrypto provider; pinned by `crates/molt-app/tests/embedded_tor.rs`. The report's "two rustls majors" is a misread of `futures-rustls 0.26`; its `ring` fix would re-adopt ring, which the ring-free guard forbids. |
| F2 | The phrase arrives unrequested in every `read_session` poll (`workspaces[].seed`) | Built: `reveal_seed {id}` is the pull (SEAT scope, GUI hold-to-peek uses the same command); the list flags `has_seed`; `create.seed`/`join.seed` are cleared at the seal. The `confirm_seed_backup {path}` idea is declined: the engine writing the phrase in clear to the disk whose loss the phrase covers is not a backup. |
| F3 | `create_start` says the phrase is GUI-only; the join disproves it | Stale text since ADR-0007; three tool descriptions corrected (`create_start`, `create_finish`, `join_finish`). |
| F4 | No done-signal; `notice` stayed `saved` for a whole join | `run.outcome` (1 ok / 2 failed) IS the signal, now named in `read_session`; every run start clears `notice`. A sequence number is declined: the client can hash the reply. |
| F5 | Read-only key undiscoverable headless; write key all-or-nothing | The generated config now names `patch_settings {mcp_read_token}`. No read token by default (a second secret that grants nothing until handed out). Allowlist: known debt. |
| F6 | `last_backup_min = 4294967295`; `join.step` stays 1, `headline` stays "" | The sentinel reads `null` on the MCP surface. `step` 1 = the run view and an empty `headline` = nothing went wrong are by design. |

Keystones: `the_recovery_phrase_is_revealed_on_request_only`,
`a_run_start_clears_the_stale_notice` (molt-engine),
`no_recovery_phrase_reaches_the_read_scope`,
`a_never_backed_up_workspace_reads_null_not_the_sentinel` (molt-mcp),
`the_details_panel_pulls_the_phrase_and_drops_it_on_hide` (molt-ui,
live-preview).
