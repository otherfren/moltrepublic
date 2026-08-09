# Error codes: making every engine-composed string localizable

Status: **PLAN — ratified 2026-08-09 (full scope).** Execution order below;
each etappe lands green on master on its own. The audit that motivates this
is `i18n_error_audit.md` — engine-composed English reaches users through
~35 toast paths, the wizard headline layer, notices and the run-log, and
the GUI lexicon structurally cannot reach it.

## Architecture

The engine stops being the author of user-facing prose. Every surface that
today carries composed English carries a **stable machine key + typed
parameters**; the GUI renders it through the lexicon (English + German),
and MCP keeps serving the English prose (rendered engine-side from the
same keys — agents read English; co-equality is capability, not language).

Review criterion from then on: new engine prose reaching a frontend
without a key is a finding.

## Etappen

- **E1 — `MoltError` becomes key-bearing.** Every variant already is a
  key; the holes are the free-text carriers (`BadPayload(String)`,
  `Engine(String)`): split their high-traffic uses into typed variants
  (params, not prose), keep the string variant as the tail fallback.
  Engine-side `Display` stays English (MCP unchanged). Test: a
  variant-coverage test that fails when a new variant lacks a lexicon
  mapping GUI-side.
- **E2 — the toast funnels localize.** `issue` / `issue_then_toast` and
  the ~13 named toast sites in `molt-ui` render `MoltError` through a
  `localize_error(lang, &MoltError)` that maps variants + params to
  lexicon entries, falling back to the English `Display` for unmapped
  tails. This alone covers most of what a user ever sees.
- **E3 — the wizard headline layer.** `relay_msg.rs`'s
  `network_headline` / `ritual_headline` / `restore_only_headline` (23
  phrases) and their detail sentences return keys; the run views render
  them through the lexicon. The headline is the most prominent text in
  the product — this etappe is user-visible payoff.
- **E4 — notices become typed.** `session.notice`'s `kind:{detail}`
  strings get typed kinds with parameters (the GUI already switches on
  the prefixes; the detail tails stop being prose). Includes
  `net_health` reasons and the S3 test verdicts.
- **E5 — run-log lines.** The ~28 ritual/restore log producers emit keys
  + params; the log pane renders localized. Lowest urgency — logs are
  read rarely and diagnostically.
- **E6 — lexicon fill.** ~150 entries, German arms written with the
  compact-text rule (name the fault, no lectures).

## Bounds

- Wire/log compatibility: none of this touches `WorkspaceEvent` or the
  chain — keys live in `Reply`/`MoltError`/notice surfaces only.
- The design-mock prose in `surfaces.slint` and placeholder examples are
  OUT of scope (sample data, replaced when those surfaces get real).
