# i18n audit: error messages and other non-lexicon user-facing text

Status: audit complete (2026-08-09), cheap fixes landed. The structural
fix is DECIDED (2026-08-09): error codes, full scope — the execution plan
is `i18n_error_codes_plan.md`.

The GUI's localization runs through one lexicon (`molt-ui` `lexicon!` →
the Slint `Strings` global). This audit swept everything a user reads that
BYPASSES it. Full site-by-site inventory lives in the session that produced
this doc; the categories and the load-bearing conclusions are recorded here.

## 1. What the sweep found

- **Engine error prose reaches toasts raw (~35 paths).** `MoltError`'s
  Display strings are English; the generic command-error funnels
  (`issue`, `issue_then_toast`) plus ~13 named toast sites pipe them to the
  user verbatim. German users get English for every failure.
- **The wizard headline layer is engine-side English (23 phrases).**
  `molt-engine/src/relay_msg.rs` composes the large signal-colored
  founding/join/restore headlines ("No shared relay", "Invite already
  used", …) inside the engine crate — structurally unreachable by the
  GUI lexicon. Same for the run-log lines (~28 producers) and the
  recovery notes.
- **Notices surface verbatim (9 kinds).** `session.notice` carries
  `kind:{detail}` strings; the GUI localizes some prefixes, but several
  details (and `net_health` reasons, S3 test verdicts) render raw.
- **Slint-side literals are minor.** Theme names, `"Republic"` fallback
  title, placeholder examples; the big block is the DESIGN-MOCK prose in
  `surfaces.slint` (explicitly labeled sample data; the Memory pane is the
  one reachable mock).

## 2. Fixed in this pass (the unambiguous items)

- `Invite {n}` seat label got its German arm (it sat between two
  correctly bilingual siblings).
- The logo-pick read failure no longer renders a Rust `{:?}` debug dump —
  it names the unreadable file, localized prefix.
- `relay-refused:` / `relay-unverified:` toasts carry a localized prefix
  (the body stays the probe's one-line verdict).

## 3. OPEN — the structural decision: error codes vs. string mapping

The remaining ~90% cannot be fixed at the toast sites, because the English
is COMPOSED in the engine crate. Two architectures:

- **A. Error codes (recommended):** engine surfaces carry a stable machine
  key + parameters next to (or instead of) prose — `MoltError` variants
  already are that key; `relay_msg.rs` headlines and notices would each
  become a key + params. Frontends localize via the lexicon; MCP keeps the
  English prose (agents read English; co-equality is about capability, not
  language). Cost: a sweep over `MoltError`'s string-carrying variants
  (`BadPayload(String)` must split into typed variants or key+detail),
  `relay_msg.rs`, the notice kinds, and the run-log producers — plus ~150
  lexicon entries. This is a multi-session project.
- **B. GUI-side mapping of known strings:** a lookup table from English
  prose to lexicon keys. Cheap to start, silently rots on every engine
  wording change — rejected unless A is refused.

Decision needed from the user: greenlight A (and its scope: errors only,
or headlines + run-log too), or accept English error prose as a known
limitation for now.
