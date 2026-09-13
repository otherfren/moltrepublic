# `list_proposals` answers every proposal since the founding, unbounded

Status: **OPEN (2026-09-13)** - fix plan, execution-ready. Found driving
a live seat of a production republic (dev build of `1356e034`) over MCP.
Sibling issues from the same session: `supersede_verdict_survives_apply.md`,
`wiki_changes_below_the_cut.md`. Leaves for `docs_archive/reviews/` with the
change that closes §4.

## 1. Symptom

`list_proposals {}` on a republic with NOTHING open answered 339 proposals
(338 applied, 1 withdrawn): 467 KB pretty-printed, 335 KB compact. The
caller's question was "does anything need my vote"; the answer was the
whole governance history. Three of the first ten calls of the session
overflowed the client's context this way.

Where the bytes are (measured on the dump):

| part | bytes | why |
|---|---|---|
| `paths` | ~150 KB | 3 565 path strings; the 2026-09-12 cleanup chunks touch up to 218 paths each |
| `votes` | ~45 KB | one row per roster member per proposal |
| headers | ~140 KB | 339 × id/state/by/stamps/channel |

`read_proposal {id}` is worse in a way the caller cannot see: it builds
`Command::ListProposals` and picks one record in `present`
(`crates/molt-mcp/src/lib.rs:498-517`), so the engine renders EVERY record
with its `current`/`proposed` texts (a cleanup chunk's `proposed` is 53 KB)
for one header. The agent toolkit polls it every 15 s while waiting for
the second voice.

## 2. Cause

- `Command::ListProposals` is a unit variant (`crates/molt-core/src/lib.rs:4020`);
  the handler (`crates/molt-engine/src/lib.rs:1770`) views the whole map
  and sorts by id. No state filter, no page.
- Round 1's B1 (`docs_archive/reviews/mcp_agent_friction_fixes.md`) only
  stripped the texts and added `paths`; the tool text still says "every
  proposal the engine currently knows about" and nothing says that this is
  the history since the founding.
- `State::view` (`crates/molt-engine/src/proposals.rs:1500`) computes
  `current`/`proposed` via `change_summary` for every record, and
  `present` throws them away again for the list.

## 3. Design

Filtering is ENGINE-side (co-equality: the GUI must be able to ask the
same question), the presentation stays in molt-mcp.

- `Command::ListProposals { filter: ProposalFilter, limit: u32, cursor: u64, with_texts: bool }`
  - `ProposalFilter { Open, Decided, All }`, serde default `Open`.
    `Open` = `state == Proposed` (a held seal and a re-based patch are
    open); `Decided` = everything else (applied, rejected, withdrawn,
    superseded); `All` = both.
  - `limit` 0 = default 100, clamped 1..=500 (the `wiki_list` page rule);
    `cursor` = "after this id" (ids are ascending and stable, so a page
    survives a seal between two calls; an offset would not).
  - `with_texts` = build `current`/`proposed` (default false): the list
    stops rendering what the list never shows.
  - `pub const LIST_ALL_PROPOSALS: Command` for the eleven test sites that
    want the old shape.
- `Reply::Proposals { proposals, total: u64, next_cursor: Option<u64> }` -
  additive fields with defaults; every `Reply::Proposals { proposals }`
  pattern gains `..`.
- `Command::ReadProposal { id: u64 }` → `Reply::Proposals` with exactly one
  full view (texts included), `MoltError::Engine("unknown proposal N")`
  otherwise. The MCP `read_proposal` tool builds it; `present` only adds
  `reply: "proposal"`.
- MCP `list_proposals` schema: `state` ("open" | "decided" | "all",
  default open), `limit`, `cursor`, `with_patch` (unchanged; implies
  `with_texts`). An unknown `state` is a tool error naming the three
  values. Tool text rewritten to say what the default is and that
  `total`/`next_cursor` page the rest.
- GUI: `crates/molt-ui/src/surfaces.rs:1094` (the decided card behind a
  patch channel) uses `ReadProposal { id }`.

Not changed: `paths` and `votes` stay in the header (the `open_path` check
and the vote pills need them; bounded by the page now); id order stays
ascending (a caller wanting the newest decided reads `total` and pages
from `total - limit`).

## 4. Work, red first

1. `crates/molt-engine/tests/proposal_listing.rs` (solo threshold-1
   harness as in `read_filter.rs`): propose three, apply two →
   `Open` = 1 with `total` 1; `Decided` = 2; `All` = 3; `limit: 1` pages
   by `cursor` and the last page has `next_cursor: None`; `with_texts:
   false` leaves `current`/`proposed` empty; `ReadProposal` returns the
   texts, an unknown id is an error.
2. `crates/molt-mcp/src/lib.rs` tests: the default build carries
   `Open`; `state: "decided"` and `"all"` map; `"later"` is refused with
   the three values; `read_proposal` builds `ReadProposal` and presents
   the one record; `proposals_present_as_headers_and_one_full_record`
   keeps passing on the compaction it already pins; the co-equality test
   lists `ReadProposal` as a tool.
3. Core: the variants, `ProposalFilter`, `LIST_ALL_PROPOSALS`, the two
   reply fields.
4. Engine: the handler filters, sorts, pages, counts; `view` takes
   `texts: bool` (`view(id, p)` stays as the `true` twin so the GUI's
   card paths do not change); `cmd_read_proposal`.
5. MCP: schema, build, text; `read_proposal` build.
6. GUI: the one call site; live-preview clippy + the GUI test shards.
7. The eleven engine test sites and `crates/molt-ui/src/surfaces.rs`
   compile on `LIST_ALL_PROPOSALS` / `..`.
8. Docs: this file's status; the tool texts are the spec. The agent
   toolkit outside the repo reads `read_proposal` only - its reply shape is unchanged.

## 5. Decided here (object if wrong)

- Default is `open`. The two real callers (a voter, a script waiting on
  its own proposal) never want the history; the round-2 briefing already
  told agents to loop "until `list_proposals` is empty".
- No newest-first order: one order, one cursor. `total` makes "the last
  page" one call.
