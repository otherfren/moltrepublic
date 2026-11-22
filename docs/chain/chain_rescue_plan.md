# Chain rescue: a rejoin that restarts a split republic

**Status: PLAN, not started; follows v0.1.1.** Extracted from F-X1 of
`docs_archive/chain/rejoin_divergence_analysis_plan.md` §7 (user,
2026-11-21: built after the rejoin and divergence fixes). Evidence:
`docs_archive/chain/rejoin_divergence_findings.md`. Its own design doc
comes first.

## Candidate

- A rejoin that can save a republic whose chains diverged and whose MLS
  group split: rejoiner and coordinator find the last block both hold
  (the `known` exchange), the rejoiner's consent names it, and the
  coordinator seals a rescue block right above it that discards
  everything higher. Sealed like `Restored` (one survivor plus the
  consent); further seats follow the same way.
- The split group cannot be rolled back (MLS deletes old epoch secrets),
  so the coordinator forms a FRESH group instead of re-keying.
- The cut is a block, never a silent truncation: signatures do not cover
  `prev`, so a seat that missed the rescue could re-offer its old
  branch; the rescue block takes precedence at its height.
- Bounds: never below the anchor; an explicit operator decision, never
  automatic. Displaced `Applied` decisions return to the vote, displaced
  `Restored` blocks are dropped.
- Compatibility: one additive `ChainChange` variant (an older build
  stops extending); every seat needs the new build.
- Size: about that of the recovery ritual. The fresh-group path is the
  riskiest part.
