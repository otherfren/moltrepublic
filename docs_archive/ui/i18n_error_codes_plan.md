# Error codes: making every engine-composed string localizable

Status: **EXECUTED — E1–E6 BUILT (E1–E3 2026-08-16, E4–E6 2026-08-17).**
E5 landed as a GENERALIZED phrase-as-key, not per-producer keys: the
real inventory was 62 lines (not ~28), ~40 of them carrying dynamic
slots, some tail-leading — so the engine exports SHAPES
(`molt_engine::known_log_shapes`: constant parts with slots between),
the GUI matches a line and re-renders it from German constants with the
slots carried verbatim, and set-equality both ways plus a per-shape
synthesis round-trip pin it (`every_log_shape_has_a_german_rendering`;
engine-side well-formedness in `relay_msg.rs`). The wire and the engine
tests stay English. E6 delivered: the net-health reason (part-wise), S3
test/list verdicts (shells + HTTP hints), the Tor probe detail incl.
the four TargetGap clauses, the rejoiner's recovery note/failure lines,
the save-failed footer, the wiki pane's own refusals (source-scan
pinned), and the two propose-toast funnel bypasses. E1 landed in compiler
form: `molt-ui::localize_error` matches every `MoltError` variant with NO
wildcard, so a new variant fails compilation until it gets a German arm
(EN stays the engine `Display` verbatim — MCP parity; free-text tails ride
through untouched). E2: both toast funnels and the five direct toast
sites render through it, in the window's active language. E3 landed
phrase-as-key instead of a parallel key field: the engine exports its
headline inventory (`molt_engine::known_headlines`, pinned PRODUCIBLE by
`every_known_headline_is_producible`), the GUI localizes by phrase with
an honest English fallback, and
`every_engine_headline_has_a_german_rendering` keeps the map complete —
no wire change, agents keep reading English. E4 was largely OVERTAKEN
by the code since the plan was written: the recovery family is a typed
GUI-side enum (`RecoverNotice`) and every toast prefix renders through a
lexicon word with the diagnostic tail carried verbatim — the E4 remainder
is vocabulary (net_health reasons, S3 verdicts) and folds into E6. Open:
E5 (run-log lines, lowest urgency by design) and the E6 remainder.
The audit that motivates this is `i18n_error_audit.md`.

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
