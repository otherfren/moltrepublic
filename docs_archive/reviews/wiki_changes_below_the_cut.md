# `wiki_changes` answers nothing below a cut, although a full holder still has the patches

Status: **EXECUTED 2026-09-13** - the user chose option A (§5.1). Built
red-first; what shipped differs from §3/§4 in these points: the stamp is
written by the fold cache's refresh, which the apply path runs after
every Memory block (`after_block_applied`) and after every re-projection,
so it is a function of the chain, not of reads; a branch change (a blob
re-anchor, a re-base, a tip displacement) forgets every stamp; the floor
walk is strict - a repeated revision is a hole like a missing one, and
nothing under the floor is listed; the history below the cut is parsed
once per fold into a memo on the cache; `rev_at_cut` is `Option` - a
pre-A3 cut (absent) keeps a floor of 1, an explicit 0 folded nothing
away. Not built: §4.2's suffix-holder integration assertion (no seat of
`checkpoint_under_load` becomes a suffix holder; the unit test's
"no stamps" branch is the only pin of that path). §5.2 stands as stated:
the live seat's current cut is not back-filled. Found on a live
seat of a production republic (dev build of `1356e034`). Siblings:
`list_proposals_unbounded.md`, `supersede_verdict_survives_apply.md`.

## 1. Symptom

The republic cut on 2026-09-12 (block 339, `rev_at_cut` 327 = the current
`wiki_rev`). Since then every `wiki_changes {since_rev}` with
`since_rev < 327` answers `changes: [], truncated: true`; `since_rev:
327` answers empty and not truncated. The tool exists so a maintaining
agent re-reads only what moved (§4.11 of
`docs_archive/memory/knowledge_base_scale.md`); right after every cut it
has no answer for ANY copy taken before the cut - which is every copy.
The agent toolkit leans on it (`wikipush` checks `wiki_changes since_rev`
before pushing over a local dump); the fallback is a full `wiki_list` and
re-reading everything.

## 2. Cause

By design, and the design's premise is half true:

- The per-revision history rides the fold cache
  (`WikiCache.history`, `crates/molt-engine/src/lib.rs:938-951`) and
  `fold_wiki_from_base` CLEARS it at the base entry
  (`crates/molt-engine/src/proposals.rs:3465`) because the patches below
  the cut left the chain (`summarize`,
  `crates/molt-engine/src/chain/wiki_base.rs:78-123`). `cmd_wiki_changes`
  (`proposals.rs:2415`) flags `truncated` for `since_rev < rev_at_cut`.
  The spec says "below the cut there is a tree and no history" (§4.9 A3,
  §4.11).
- But on a full holder the `ProposalRecord`s survive the cut with their
  patches: the live seat still answers a cleanup chunk's 53 KB patch in
  full, and `EngineStateDump.proposals` persists every record
  (`crates/molt-core/src/lib.rs:3056`). The TOUCHES are all there; what
  is missing is the revision each patch produced, because the fold order
  (chain order) is gone with the blocks and id order is not chain order.
- `proposals.rs:2415` also flags `since_rev == 0` under a base, which is
  correct today and stays correct for a suffix holder.

## 3. Options

**A. Stamp the fold revision on the record (recommended).**
`ProposalRecord.wiki_rev: Option<u64>` (additive,
`skip_serializing_if = None`). The fold step learns the entry's proposal
id (`applied_payloads` iterates `(Option<u64>, Value)`; `fold_wiki_step`
takes the id, `WikiRevChanges` gains `proposal: Option<u64>`) and after
every fold/extension the caller stamps `proposals[id].wiki_rev = rev`
(idempotent - a refold writes the same number). Below `rev_at_cut`,
`cmd_wiki_changes` derives the touches from the applied wiki records
with `wiki_rev > since_rev` (`parse_patch` → `wiki_touch_of`, the same
helper the cache uses), prepends them to the cache history and coalesces
as today. `truncated` becomes: `since_rev < floor`, where `floor` is the
lowest revision reachable by a CONTIGUOUS run of stamps downward from
`rev_at_cut` (0 when the run reaches the first patch), or `rev_at_cut`
when no stamp is held; or `since_rev > wiki_rev`. A gap (a record
dropped by `chain/sync.rs:317`/`:532` on a re-base, a void patch's
`None`) ends the run - honest, never a short answer that reads complete.
Cost: a patch parse per record in the window, paid only when a caller
asks below the cut. Durable: the record store is already persisted and
nothing prunes decided records. Limits: a holder that materialized a
record from the blob (`ensure_applied_record`, no patch) or fetched its
base as a suffix holder has no stamps → truncated as today; the CURRENT
cut of that republic stays truncated (its 320 records were folded before the field
existed and chain order below the cut is unrecoverable) - the fix works
from the next cut on.

**B. A sealed sidecar `wiki_history.bin`.** The fold step appends
`(rev, touches)` frames next to `wiki_base.bin` (`molt-storage`
`write_wiki_base`/`read_wiki_base` as the template: AAD segment, chunked
frames, torn-file check, read cap); a cut leaves it alone; a cap (e.g.
50 000 touches) raises the floor. Independent of record retention, but
a second durable format for data the record store already holds, plus
its own corruption and cap stories. Fallback if A's premise (records
survive) is ever weakened.

**C. Keep the history in memory across the refold.** Lost on restart;
rejected.

**D. Change nothing, document it.** The agent re-lists on `truncated`
(813 rows, cheap) and re-reads everything or diffs by `bytes`. Rejected:
it is exactly the cost §4.11 was built to remove.

## 4. Work for A, red first (all done)

1. `proposals.rs::wiki_maintenance_tests`: (a) three stamped records
   (page `a` added @1, page `b` modified @2, page `c` added @3), a cut at 2,
   `since_rev 1` → `b modified@2, c added@3`, `truncated false`;
   `since_rev 0` → a, b, c and not truncated. (b) the record of rev 1
   removed → `since_rev 0` truncated, `since_rev 1` complete. (c) no
   stamps at all → today's answer. (d) a rename below the cut still
   carries `from` through the coalescing. (e) fresh fold == cache,
   across the cut.
2. `crates/molt-engine/tests/checkpoint_under_load.rs`: after the first
   cut, `wiki_changes since 0` on a full holder lists a, b, c and is not
   truncated; on the suffix holder (`vera` after her fetch) it is.
3. Core field; engine stamping in `refresh_wiki_cache` (after the full
   fold and after an extension, also one a cut ends), driven from the
   apply path; `wiki_history_below_the_cut`, the floor, the memo; reply
   shape unchanged.
4. MCP tool text: "a full holder answers below its cuts from its own
   records; `truncated` says where it cannot". Spec: §4.9 A3 and §4.11
   in `knowledge_base_scale.md` get the corrected sentence.

## 5. Decided by the user (2026-09-13: A; the current cut stays truncated)

Residuals after the review: a bulk re-projection over a cut (rejoin,
catch-up that lands the cut together with its patches) stamps nothing
below it - such a holder reads truncated like a suffix holder; a
pre-A3 cut co-signed by an old build after stamps were written would
mix two counter eras below a later cut [SPECULATION on the precondition:
every live build writes `rev_at_cut`, and a cut is n-of-n].

1. A or B - A relies on decided records never being pruned from the
   store. Is that a guarantee the product wants to keep (the archive view
   already depends on it)?
2. The current cut cannot be back-filled. Acceptable, or should the
   agent side (`wikipush`) get a "re-list on truncated" fallback
   now, before the build?
