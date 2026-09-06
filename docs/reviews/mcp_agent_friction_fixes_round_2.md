# Fixes from the second three-agent wiki round (2026-09-06)

Status: EXECUTED 2026-09-06 - Parts A (A1-A3), B, C and D are on master,
built by three agents in parallel worktrees and merged in that order.
Open: A4 (a decline delays the seal - a governance question for the
republic) and a latent index race found on the way (`refresh_wiki_graph` takes `wiki_graph_dirty` before it
checks that a graph exists; a build installed over a moved tree stays
stale - one line plus a pinning test, own change-set). Source:
`mcp_agent_friction_2026-09-06.md` (findings G1-G20). Round 1's fixes (`mcp_agent_friction_fixes.md`) held:
over 100 proposals from three writers, no fork, no lost proposal, one tip
tie healed in four minutes, chains and wikis identical on every check. The
friction that remains is VISIBILITY, not correctness: the engine is right
and its state arrives in a shape an agent misreads.

## Landed during the run (in the round-2 commit)

- `superseded: true` reads as `state: "superseded"` at the MCP edge (G9);
  round 1 did the same for `withdrawn`.
- The last Rust `Debug` leak in a user-facing error (`ProposalId(1)` in
  the read-only-channel refusal) (G2).
- A link whose destination carries a scheme is never a wiki path - NIPs on
  GitHub end in `.md` (G15).
- The tool texts say that ids are minted per seat and gaps are normal (G5).

## Part A - the discussion of a proposal must outlive its decision (G2, G6, G17)

The single most expensive class of the run: six review remarks lost by one
seat alone, the third reviewer's real findings homeless, the post-mortem of
a rejected proposal impossible, and the vote table wiped on every negative
end state. All from one decision - a decided patch channel is read-only.

A1 **A decided patch channel stays writable.** Read-only was chosen to stop
chat on dead cards; a review of an applied or rejected change is not dead
chat. Built: the local send path refuses NO patch channel any more -
including one whose proposal this node has not seen yet, per chat_bus.md
Q4 (a tagged message may arrive before the Proposed it references, so an
unknown id was never an error; the briefing's "keep that refusal" collided
with Q4 and the cross-instance keystone and was dropped). `chat_bus.md`
carries the amendment, dated.

A2 **`note` on a vote.** `approve {proposal_id, note?}` and `decline
{proposal_id, note?}` post the note into the patch channel BEFORE the vote
lands, in the same command. A reviewer's reason then never races the
decision (the second reviewer's approval is the tipping one; his note goes
with it). Engine: `cmd_approve`/`cmd_decline` take `note: Option<String>`,
call the chat post first; molt-core `Command::Approve { proposal, note }`
(additive field with serde default); MCP schema + text; GUI unchanged
(passes `None`).

A3 **The vote table survives the decision.** `voted` (the display list D6
already keeps for applied cards) is stashed for rejected, withdrawn and
superseded cards too, as it stood at the decision; `ProposalView.votes`
reads from it once the card is terminal. Engine: `stash_voted` on every
terminal transition, not only on a seal.

A4 **A reasoned decline delays the seal (design question, not decided).**
#96 was declined by Left with a source and a date; the other two sealed it
minutes later, and the corrected re-file #111 became stale - "the
provably wrong version went live, the provably right one was gone". At
m=2 of n=3 a decline is powerless the moment two others sign. Right's
wish: a proposal carrying at least one decline waits one round (the A2.2
pacing machinery already holds seals) before the tipping signature seals
it, so the decline can be read. It changes governance semantics - a
decline gains a delaying power the charter never gave it - so it is the
republic's decision, recorded here for the discussion, not built.

Tests: a `chat_send` into an applied and a rejected patch channel succeeds;
`approve {note}` lands the note then the vote, on a 2-of-2 where the note
is the tipping seat's; a withdrawn card's votes still show the proposer.

## Part B - what `wiki_edit` should know before it proposes (G7, G10, G13, G19)

The agents wrote three checkers the engine could have been: alias
collisions against `wiki_props`, open proposals' `paths` before writing,
in-link completeness before a rename. Each is a warning under B5's rule
(refuses by default, `allow_warnings: true` proposes anyway).

B1 **A distinct `create` op.** `{op: "create", path, content}` refuses an
existing path; `content` keeps its create-or-overwrite meaning for callers
that mean it, and its text says so. Sharper than any warning: a create can
never silently become a rewrite of a page that appeared while the batch
was being written (#48 → G13).

B2 **Open-proposal awareness.** For every touched path, if an OPEN
proposal's patch also touches it: warning `path X is in open proposal N by
Mr. Center`. Covers G10 (two creators of one page: the loser learned it as
a phantom rejection) and G19 (a rename while an open proposal still links
the old path). The engine holds every open proposal's payload; `paths`
come from the same diff-header scan `list_proposals` uses.

B3 **Name collisions.** A new `title` or alias that equals an existing
basename, stem, alias or title: warning `alias "Signal" already names
organisationen/signal-foundation.md - both pages lose it`. Reuses
`NameIndex`; the check runs on the working copy after the edits.

B4 **Rewrite of a foreign page.** `content` on an existing page whose last
applied proposal came from another seat: warning `rewrites a page last
written by Mr. Center (#28)`. The applied projection carries proposal id
and proposer per path.

Tests: each warning red-then-green in `tests/wiki_edit.rs`; `dry_run`
returns them without proposing.

## Part C - states and counts that say what they are (G9, G12, G14)

C1 **A held seal is visible.** While A2.2 pacing holds a seal, the card
reads `state: "sealing"` (MCP edge, like `withdrawn`/`superseded`) and the
vote reply carries `held_for_secs`. Engine exposes `chain.seal_held` in the
view; the GUI card gets a "sealing" tone later.

C2 **The vote reply says what it counts.** `Reply::Vote` gains `head:
u64` (this node's chain height) and its text says "approvals are the
signatures THIS node holds; the block may already be sealed on a peer".
Cheap, and it turns "1/2 then applied two seconds later" from a mystery
into a sentence.

C3 **The propose text stops promising "patch does not apply".** A path
collision surfaces as `superseded`, never as that refusal; say so.

## Part D - text and naming (G1, G3, G8, G11, G16, G18)

- G1 `read_state` for chat: name the `applied` key, or alias it as
  `messages` on the chat surface.
- G3 `dry_run` answers summary + warnings + paths by default; the patch
  behind `with_patch: true` (the `list_proposals` flag).
- G8 the count string: "`+` `-` `→` count files, `~` counts changed lines"
  in the `propose` and `wiki_edit` texts.
- G11 `replace` text names the substring rule and the `\n## Quellen\n`
  anchor idiom; or `old` without a newline matches whole lines only.
- G16 `set_props` text: a relation also asserted in the prose stays until
  that link goes; `wiki_health` could list relations asserted twice on one
  page.
- G18 each `wiki_*` text names its list key (`hits`, `docs`, `edges`, ...).

## Part E - not a tool defect, but worth a line in the briefing

- 2-of-3 is fast and thin: a quarter of the decisions fell without the
  third seat, and two of its real findings became follow-up damage (#22,
  #60). Part A gives that seat a place to speak; the threshold itself is
  the republic's choice.
- The inline-relation direction trap (G4) was closed by the agents in the
  ontology ("the page is always the subject"); a `wiki_health` two-cycle
  check per predicate is the tool-side complement.

## Order

A1-A3 first (one change-set, the largest cost), then B1+B2 (one
change-set, both need the open-proposal path scan), then C1-C3, then B3,
B4 and Part D. Each item red-test first; Part A touches the chat contract,
so `docs_archive/chat/chat_bus.md` gets its status line updated in the
same change.
